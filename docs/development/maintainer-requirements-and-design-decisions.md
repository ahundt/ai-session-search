# Maintainer requirements and design decisions

This is the durable, public requirements reference for maintainers of AI Session Search. It records
normalized product and engineering contracts, their rationale, intentional surface differences,
and verification locations. Repository-internal agents should use
`.agents/skills/maintain-ai-session-search`; that skill is developer guidance and is not an
end-user package installed by `aise`.

Requirement identifiers are stable review anchors. Revise an existing requirement when its contract
changes and add the next identifier only for a genuinely new contract.

## Priority map

| Priority | Meaning |
| --- | --- |
| P0 | Correctness, data safety, and externally observable search semantics |
| P1 | Product, configuration, installation, capability, and provider contracts |
| P2 | Maintainer workflow, evidence quality, performance, and release discipline |

## P0 — correctness and data safety

### REQ001-preserve-user-data

Installation, migration, repair, update, and uninstall operations must preserve source transcripts,
indexes, configuration, user edits, unrelated client settings, and uncommitted repository work.
Destructive cleanup requires an exact target and explicit authorization.

### REQ002-share-typed-contract

Rust, Python, CLI, and MCP must route message search and deterministic capabilities through shared
typed service requests and responses. Adapters translate surface syntax and serialization; they
must not independently redefine matching, ordering, filtering, paging, or completeness.

### REQ003-preserve-surface-semantics

Intentional defaults are part of the API:

| Surface | Omitted literal, regex, or queryless message-search limit | Rationale |
| --- | --- | --- |
| Rust | All results when no purpose or operation default applies | Programmatic callers control consumption |
| Python | All results when no purpose or operation default applies | Programmatic callers control consumption |
| CLI | All results when no purpose or operation default applies | Output can be redirected, piped, filtered, or explicitly limited |
| MCP | Configured finite page; currently 20 by default | Tool results enter an agent context directly |

Every surface accepts an explicit positive page size. `all_results` states a complete-corpus request
explicitly for literal, regex, or queryless search. Fuzzy search always requires finite retention
and rejects `all_results`. A purpose bundle or `[search.message-search].default_limit` may
deliberately supply a finite operation-level default before the surface default is considered.

This distinction applies to result membership. Separate presentation windows may shorten displayed
content without removing a result.

### REQ004-separate-retrieval-presentation

Line windows, preview-character limits, match-evidence budgets, response formats, and whitespace
presentation must never change matching, ranking, hit membership, context membership, result count,
offsets, or next-page identity.

`lines_per_message` uses one sign convention:

- positive selects the first N lines of each returned message;
- negative selects the last N lines;
- zero returns complete message content.

`show_transcript_lines` and MCP `get_session_transcript_lines` window a whole transcript and are
distinct from per-message presentation.

### REQ005-return-match-evidence

Every queried hit must expose visible match-centered evidence even when the boundary content window
does not contain the match. Literal mode also returns the exact matched source spelling and absolute
character coordinates. Regex and fuzzy evidence remain independently bounded because their spans
may be data-dependent, discontiguous, or numerous.

The evidence budget changes only the excerpt. It must never change whether a row matches.

### REQ006-report-extent-honestly

Structured responses must state enough metadata to prevent a caller from mistaking a window for the
whole result:

- schema version and query mode;
- returned hit count;
- finite limit or explicit all-results extent;
- offset, ordering, `has_more`, and `next_offset`;
- line and character presentation policy;
- per-value completeness and omitted-start/omitted-end flags;
- original size only when it is known without an extra unbounded scan;
- parameter origins when full receipts are requested.

CLI JSONL begins with metadata and ends with a terminal record. A consumer that never receives the
terminal record must not infer that an interrupted stream was complete.

### REQ007-preserve-page-identity

Evidence generation, context enrichment, reference extraction, formatting, and presenter failures
must not alter the retrieval plan or selected page. Pagination must be derived from one extra
retained row, not from post-presentation behavior.

### REQ008-reject-hidden-cutoffs

Do not introduce silent aggregate byte ceilings, row ceilings, content truncation, time limits, or
fallback pages. Intentional bounds must be named, typed, documented, reflected in receipts or
metadata, and rejected when they conflict.

`max_hits_per_page` and `max_context_neighbors_per_hit` are policy ceilings: an oversized resolved
request errors. They do not silently clamp the request. Explicit `all_results` is not a finite page
and therefore does not masquerade as one.

### REQ009-bound-fuzzy-search

Fuzzy search scores the complete structurally eligible corpus, applies deterministic relevance
ordering, and retains only the finite top-K page window. A missing finite page is an error on Rust,
Python, and CLI; MCP may provide its configured finite default.

### REQ010-protect-complexity-bounds

Maintain these algorithmic properties:

- exact and regex search use indexed candidates when safe, followed by authoritative verification;
- regex without a safe literal may scan the filtered corpus;
- fuzzy scoring is `O(T + N + W log W)` time with bounded retained state for eligible text `T`,
  rows `N`, and requested page window `W`;
- presentation enrichment is proportional to retained rows and configured evidence/window sizes;
- literal source proof is proportional to the returned literal occurrences;
- extent metadata does not trigger a second full-source scan;
- avoid pre-page full-content copies, N+1 lookups, and metadata proportional to omitted source text.

Performance-sensitive changes require a comparable baseline and a repeated benchmark.

### REQ011-validate-language-boundaries

PyO3 inputs and outputs, Rust request/response types, Python stubs, enums, optional fields,
exceptions, serialized schemas, and docstrings must agree. Wrong types must raise type errors;
invalid values and conflicting combinations must raise actionable value or contract errors rather
than being silently coerced.

### REQ012-reject-invalid-combinations

Validate the resolved request, not parameters in isolation. Examples include:

- sequence bounds require one session;
- latest-match selection requires one session;
- fuzzy search cannot use `all_results` or match-window selection;
- tool-argument search requires a compatible message kind and RFC 6901 pointer;
- mutually exclusive kind selectors cannot silently overwrite one another;
- a resolved message-kind set that can match nothing is an error.

## P1 — product and integration contracts

### REQ013-resolve-parameters-by-origin

Message-search values resolve in this order:

1. explicit call, CLI flag, Python argument, or MCP argument;
2. selected versioned purpose bundle;
3. operation configuration such as `[search.message-search]`;
4. surface configuration such as `[mcp]` or `[cli]`;
5. typed embedded default.

General configuration location and runtime overrides follow explicit option, applicable
`AI_SESSION_SEARCH_*` environment variable, platform config, then embedded default. Full receipts
must expose the selected origin rather than only the final value.

### REQ014-use-platform-app-paths

Config, database, cache, transaction receipts, update state, and canonical app-owned skills must
live under platform-appropriate AI Session Search locations. On macOS and Linux, the primary user
configuration root is `~/.ai-session-search`; cache data uses the platform cache location. Legacy
paths are migration inputs, not competing canonical roots.

### REQ015-separate-app-harness-roots

The canonical end-user packages `ai-session-search` and `corrections` live under the resolved AI
Session Search application root and contain real `SKILL.md` files. Harness-native skill directories
contain links to those canonical packages where the harness supports links. App ownership and
harness discovery are separate concepts.

The repository-only `maintain-ai-session-search` skill is canonical under `.agents/skills`.
Repository-local Claude discovery uses `.claude/skills/maintain-ai-session-search` as a link to that
single editable copy. This developer skill must not enter Python artifacts, the Rust crate, embedded
managed skills, user integration manifests, or global end-user installation.

### REQ016-support-multiple-skill-roots

Repeated custom `--skill-root` values and multiple selected harnesses must fan out deterministically
without collapsing distinct paths, overwriting one destination with another, or assuming that a
user has only one harness.

### REQ017-preserve-install-ownership

Package-manager ownership, integration registration, managed instruction blocks, executable
aliases, and skill trees are distinct ownership domains. Record enough evidence to determine what
may be changed, restored, or removed. Unreadable or inconsistent ownership metadata must fail
preflight rather than grant destructive authority.

### REQ018-preserve-unmanaged-content

Integration writes are transactional and preserve unknown JSON/TOML keys and unmanaged Markdown.
A skill directory is removable only when every owned byte matches the recorded manifest. If a file
was changed or an unknown file exists, preserve the entire directory and report why. Force cleanup
requires one exact skill root and explicit destructive intent.

### REQ019-verify-each-harness

Treat these as separate verification layers:

1. native config shape and path;
2. executable resolution;
3. skill discovery;
4. MCP process startup and initialize/tools-list;
5. harness permission policy;
6. real tool invocation and result;
7. provider transcript discovery and parsing.

Do not claim end-to-end support from a configured JSON key or an MCP handshake alone.

| Harness surface | MCP configuration | Skill discovery | Managed instruction |
| --- | --- | --- | --- |
| Claude Code | `~/.claude.json`; legacy `~/.claude/.mcp.json` | `~/.claude/skills` | `~/.claude/CLAUDE.md` |
| Claude Desktop local agent | platform Claude `claude_desktop_config.json` | shared Claude behavior where supported | Claude guidance |
| ChatGPT/Codex App, Codex CLI, IDE | shared `~/.codex/config.toml` | `~/.agents/skills` | `~/.codex/AGENTS.md` |
| Gemini CLI | `~/.gemini/settings.json` | `~/.gemini/skills` | `~/.gemini/GEMINI.md` |
| Antigravity App/IDE/current CLI | `~/.gemini/config/mcp_config.json` | App/IDE: `~/.gemini/config/skills`; CLI: `~/.gemini/antigravity-cli/skills` | shared `~/.gemini/GEMINI.md` |
| Antigravity compatibility | `~/.gemini/antigravity-cli/settings.json`; `~/.gemini/antigravity/mcp_config.json` | tested compatibility roots only | no duplicate instruction file |

Other supported clients retain their documented native MCP shapes. Do not fabricate every
instruction filename for every harness.

### REQ020-normalize-provider-records

Claude Code, Claude Desktop local agent, Codex App/CLI/IDE, Cursor, Antigravity App/IDE/CLI, Pi,
Google AI Studio, and Gemini CLI local transcripts normalize into the shared session/message model.
List, show, search, message reads, export, analysis, Python, Rust, CLI, and MCP must operate on those
canonical records rather than adapter-specific response models.

Do not add a second parser for an opaque or duplicative database without evidence that it contains
unique supported data.

### REQ021-state-local-data-boundary

The product searches locally discoverable transcripts. Cloud-only account history with no local
record is outside the index and must not be advertised as searchable.

### REQ022-separate-guidance-capabilities

Harnesses load and interpret `SKILL.md`. Aise does not execute that prose or invoke a model.
Deterministic runnable behavior belongs in an adjacent, closed-schema `capability.toml` parsed by
aise. Skill guidance and machine capability declarations are related package components with
different execution authorities.

### REQ023-accept-capability-parameters

Runnable capabilities must accept typed runtime parameters in addition to selecting a package by
name or authorized path. Scope, provider, session class, time range, paging, all-results selection,
and compatible additional packages must flow through the same Rust request model and be exposed by
Python, CLI, and MCP. Unknown fields and incompatible capability compositions must fail closed.

### REQ024-delegate-package-updates

Package installation and integration installation remain separate. `aise package check` is
read-only. `aise package update` detects uv tool, uv/pip, pip, pipx, Cargo, Homebrew, native archive,
direct source, or unknown ownership and delegates only to a verified owning manager after
confirmation.

Source checkout, direct URL, Cargo path/Git, and unknown installations receive guidance instead of
silent registry replacement. Refresh integrations after a manager update. Uninstall integrations
before removing the global executable.

### REQ025-justify-timeouts

Indexed search has no elapsed-time cutoff. Native read-only SQL and MCP SQL default to timeout zero,
meaning disabled. A finite timeout requires a measured availability or safety justification,
explicit scope, configurable value, and documented zero/omission behavior.

Network release notifications are a distinct bounded operation: they are TTY-only, cached,
disabled for MCP/library/noninteractive use, configurable, and use a finite request timeout so an
optional notification cannot stall ordinary CLI work.

## P2 — maintainer execution

### REQ026-reproduce-material-claims

Reproduce material external reports before adopting them. Distinguish permission denial, config
discovery failure, executable resolution, process startup, protocol negotiation, parser behavior,
and presentation behavior. Record commands, exact paths, versions, and environment differences.

### REQ027-use-tdd

For defects and contract changes, add the smallest failing test at the lowest shared layer, confirm
the failure, implement one coherent change, then add adapter and repository-contract coverage for
every affected surface.

### REQ028-test-cross-surface-contracts

Tests must freeze both parity and deliberate differences across:

- Rust service and public API;
- CLI parsing, help, human output, JSON, and JSONL;
- MCP schemas, defaults, errors, and receipts;
- Python/PyO3 constructors, return types, stubs, and docstrings;
- provider fixtures and incremental parsing;
- packaging and installed executable behavior.

General tests should use temporary roots and synthetic data and must not mutate real user
configuration, indexes, sessions, or manually edited files.

### REQ029-dogfood-installed-artifacts

Exercise the installed executable, MCP server, managed skills, deterministic capabilities, provider
parsing, and install/status/uninstall/recovery lifecycle from clean or minimally inherited
environments. Separate a harness permission denial from discovery or process failure.

### REQ030-benchmark-risky-paths

Changes affecting scale, latency, memory, concurrency, indexing, or output volume require a
comparable baseline, the same post-change measurement, dataset size, environment, and complexity
interpretation. Historical machine timings are evidence samples, not timeless guarantees.

### REQ031-align-docs-and-schemas

Source comments, Rust docs, CLI help, MCP schemas, Python docstrings, stubs, examples, public docs,
and tests must describe the same contract. A surface-specific difference must be named and justified
at every relevant boundary.

### REQ032-record-design-tradeoffs

Maintainer-ready design work records the selected mechanism, serious alternatives, selection
criteria, UX consequences, error behavior, ownership states, complexity bounds, failure modes,
verification plan, and unresolved external actions. New summaries must retain verified mechanisms
instead of erasing useful prior analysis.

### REQ033-commit-coherent-progress

Commit coherent, recoverable progress points. Commit messages name exact files or components,
previous behavior, resulting behavior, rationale, and verification. Preserve unrelated user work
and never use destructive history operations as routine cleanup.

### REQ034-gate-release-artifacts

Run `./run_ci_local.sh` from a clean commit before release-candidate claims. Test uv project/tool,
wheel and source distribution, Cargo registry/path/Git or native archive pathways as applicable.
Build release artifacts once, verify the exact bytes, and publish only through maintainer-controlled
protected environments.

### REQ035-critically-review-ai-output

Agent and subagent reports are hypotheses until checked against current source, tests, installed
behavior, primary harness documentation, and complete scoped evidence. Accept or reject each
material finding with reasons; do not merge recommendations merely because several agents repeat
them.

### REQ036-preserve-existing-strengths

When revising a plan, skill, document, or implementation, retain verified mechanisms, state
taxonomies, test matrices, complexity analysis, and useful UX reasoning. Additions must not become
lossy replacements. Remove duplication only after the durable source of truth contains the
substance being preserved.

## Current measured installation evidence

Measured on 2026-07-27 from commit `41f52c3` and installed `aise 1.0.0-rc.1`:

- Current and compatibility MCP targets reported configured.
- Both managed end-user skills validated and linked into every selected harness root.
- `~/.gemini/config/mcp_config.json` contained `codebase-memory-mcp` and `ai-session-search`, with
  `command=/Users/athundt/.local/bin/aise` and `args=["mcp","serve"]`.
- The uv executable and `/Users/athundt/.cargo/bin/aise` both ran normally, including under an
  empty environment. The uv executable completed MCP initialize and tools/list verification.
- `ModuleNotFoundError: No module named 'encodings'` was reproducible only with a deliberately
  invalid `PYTHONHOME`. Running Antigravity processes did not expose `PYTHONHOME` or `PYTHONPATH`.
- An interactive Antigravity CLI discovered `ai-session-search/get_index_status` and displayed
  one-time, conversation, persistent, and deny permission choices. Only one-time permission was
  selected. This proves live discovery and permission routing; it does not substitute for the
  separate protocol verification or claim an unseen result payload.
- `aise package check --format json` classified the uv checkout as `direct-source`, refused
  automatic replacement, reported current `1.0.0-rc.1`, latest published `0.3.1`, and
  `current_build_is_newer_than_latest_release=true`.

This evidence is time-scoped. Rerun integration status, package ownership/check, MCP handshake,
provider parsing, and installed dogfood before a new release-readiness claim.

## Verification map

| Contract | Primary implementation | Representative verification |
| --- | --- | --- |
| REQ002–REQ013 message request, extent, evidence, origins | `rust/ai-session-search-core/src/message_search.rs`, `service.rs`, `messages.rs`, `mcp_server.rs` | `rust/ai-session-search-core/tests/message_search_contract.rs`, service/MCP unit tests, `tests/test_native_binding.py` |
| REQ011 Python/Rust boundary | `rust/ai-session-search-python/src/lib.rs`, `ai_session_search/_native.pyi` | native binding tests, stubtest, runtime/stub parity |
| REQ014–REQ019 config and integration lifecycle | `config.rs`, `integrations.rs`, `skills.rs`, `skill_manifest.rs`, `text_file_transaction.rs` | integration/config unit tests and repository contracts |
| REQ020–REQ021 provider normalization | provider modules under `rust/ai-session-search-core/src/providers/` | provider fixtures, incremental/full parse parity, session-id binding |
| REQ022–REQ023 deterministic capabilities | `skill_catalog.rs`, `skill_capability.rs`, `skills.rs`, `mcp_server.rs` | skill catalog, process lifecycle, Python, CLI, and MCP capability tests |
| REQ024 package ownership/update | `update.rs`, release configuration | package ownership/update tests and installed `aise package status/check` |
| REQ027–REQ034 quality and release | `tests/`, Rust test suites, `run_ci_local.sh`, release workflows | focused tests followed by all local release-gate stages |
