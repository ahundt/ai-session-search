# Maintainer requirements and design decisions

This is the durable, public requirements reference for maintainers of AI Session Search. It records
normalized product and engineering contracts, their rationale, intentional surface differences,
and verification locations. Repository-internal agents should use
`.agents/skills/maintain-ai-session-search`; that skill is developer guidance and is not an
end-user package installed by `aise`.

Requirement identifiers are stable review anchors. Revise an existing requirement when its contract
changes and add the next identifier only for a genuinely new contract. Document order expresses
priority; identifier numbers preserve identity and do not determine priority.

## Priority map

| Priority | Meaning |
| --- | --- |
| P0 | Pre-change discovery, architecture, correctness, data safety, and externally observable semantics |
| P1 | Product, configuration, installation, capability, and provider contracts |
| P2 | Maintainer evidence, validation, documentation, and release discipline |

## P0 — discovery, architecture, correctness, and data safety

### REQ037-explore-before-change

Before judging a defect or proposing a change, search and inspect:

- repository guidance, design documents, source, tests, TODOs, history, and current git state;
- prior local AI sessions when the task depends on earlier requirements or experiments;
- current installed/configured behavior when the claim concerns packaging or integrations;
- authoritative upstream sources when a harness, platform, protocol, or dependency contract is
  material.

Use the code knowledge graph for structural discovery and exact source for verification. If graph
coverage is partial or unavailable, state that limitation and inspect the reported ranges or use
targeted source search. Do not turn a previous agent statement or historical measurement into a
current fact without reproducing it.

### REQ038-map-semantic-ownership

Map what the system conceptually does before editing it. The map must identify the authoritative
owners of data normalization, defaults, parameter precedence, validation, matching, ordering,
paging, context expansion, presentation, serialization, configuration, lifecycle state, and
resource cleanup across every affected surface.

Search for semantic duplication: two implementations can duplicate a contract even when their
names and text differ. Conversely, superficially similar code may intentionally represent distinct
surface or lifecycle semantics. Preserve deliberate differences and consolidate accidental ones.

### REQ039-reuse-or-improve-architecture

Start at the strongest existing shared seam. Reuse the typed service, parser, transaction,
configuration, ownership, or presentation abstraction that already owns the behavior. Improve that
abstraction when evidence shows it is insufficient; do not route around it with a second facade,
parallel parser, duplicate default, or surface-only patch.

Architecture changes must leave one obvious authority per contract, thin adapters, composable
extension points, and a migration path for existing callers. A replacement must preserve verified
strengths and demonstrate why it is better than improving the current seam.

### REQ048-adopt-proven-libraries

Before implementing protocol, serialization, concurrency, lifecycle, parsing, configuration, or
other infrastructure, inventory current manifests, lockfiles, imports, and repository abstractions.
Reuse a fitting dependency or existing wrapper when it already satisfies the contract.

When a gap remains, evaluate mature, widely used, actively maintained libraries against the exact
requirements: API and protocol conformance, ownership and cancellation behavior, error quality,
security posture, MSRV and supported platforms, asymptotic/runtime cost, dependency and binary
cost, release cadence, and migration risk. Adopt a library only when it robustly removes more
custom machinery than it adds. When custom code remains, record the concrete unmet requirement and
keep it behind the narrowest shared seam. Marketing claims, copied version pins, and one model's
recommendation are hypotheses until verified against primary sources and this repository.

### REQ040-eliminate-semantic-duplication

Apply DRY to meaning and policy, not mechanically to every repeated line. Shared product semantics
belong in one typed implementation and shared tests. Surface adapters may retain syntax, help,
serialization, and deliberately different defaults, but they must not independently redefine the
underlying contract.

Before adding a type, parameter, config key, helper, parser, cache, lifecycle state, or capability,
search for equivalent ownership. Prefer extending or consolidating the existing mechanism. Remove
duplication only after the durable source retains all required behavior, evidence, and compatibility.

### REQ041-optimize-multi-objective-outcomes

Design for the user's real workflow, not one isolated metric. Prefer Pareto-improving changes that
improve or preserve correctness, task completion, discoverability, ease of use, latency, throughput,
peak and retained memory, allocation/copy volume, I/O, output size, cost, token use, compatibility,
maintainability, recovery, and user time together.

The product should be easy to use correctly, hard to use incorrectly, automatic when the safe intent
is unambiguous, explicit when a choice changes semantics, and actionable when it cannot complete the
task. Do not reduce output or context at the expense of match correctness, evidence, completeness,
or task success. Document any material tradeoff that is not Pareto-improving.

### REQ044-automate-safe-problem-solving

When user intent, authority, and a safe deterministic action are unambiguous, solve the problem in
the owning service instead of returning a manual checklist. Resolve known defaults, create required
app-owned directories, select the verified package manager, normalize supported inputs, close
resources, and perform safe idempotent recovery automatically. Preserve explicit choices whenever
they change semantics, ownership, data, external state, or cost materially.

Design APIs and workflows so the easiest path is correct: use typed builders, closed enums,
validated combinations, generated descriptions, transactional writes, and deliberate defaults.
Accepted-but-ignored parameters, hidden precedence, duplicated semantic owners, and cleanup that
depends on callers remembering an undocumented step are contract failures.

### REQ045-own-and-clean-resources

Every resource has one explicit lifecycle owner. Use Rust RAII for database connections,
transactions, snapshots, iterators, buffers, subprocesses, temporary artifacts, and locks. Expose
equivalent deterministic cleanup at foreign boundaries: Python iterators implement `close()` and
context management, and CLI/MCP adapters release resources on natural completion, early break,
drop, cancellation, error, broken pipe, and process shutdown.

Closing an iterator or failed operation must not drain unread work. Cleanup cost must be bounded by
active owned state rather than remaining corpus size. Tests cover lock/snapshot release, idempotent
close, exception paths, garbage collection/drop fallback, rerun safety, and preservation of
diagnostic evidence. Temporary test artifacts are removed by default through the existing
retention/debug mechanism rather than ad hoc cleanup flags.

### REQ046-preserve-boundary-results

Rust, PyO3, Python, CLI, MCP, and serialized public interfaces must preserve the same semantic
values, nullability, ordering, identities, coordinate spaces, terminal state, and error causes.
Conversion layers may translate syntax and presentation; they may not flatten away typed states,
coerce invalid input, replace an error with an empty result, silently drop optional data, or change
ownership obligations.

Return types distinguish natural completion, partial/interrupted delivery, caller close,
cancellation, validation failure, retrieval failure, serialization failure, and cleanup failure
where those states require different caller action. PyO3 releases the GIL during blocking database
batch work, maps Rust failures to stable actionable Python exceptions, and never holds the shared
application mutex for a user-controlled iterator lifetime.

### REQ047-return-actionable-recovery

Only conditions that cannot be solved automatically and safely are handed back to the caller. Such
errors must name:

- the failed operation and exact parameter, resource, path, provider, harness, or lifecycle state;
- the observed versus required value and why automatic repair was unsafe, ambiguous, or outside
  authority;
- what was preserved, rolled back, or already cleaned up;
- the smallest exact next command, API call, config location, candidate choice, or restart action;
- how to verify success and where to obtain authoritative help when external action is required.

Do not return generic “retry,” “restart,” “remove when done,” “invalid input,” or “contact an
administrator” text when the system has more specific context. Errors remain bounded and redact
secrets, but preserving actionability takes priority over shaving a few diagnostic bytes.

### REQ010-protect-complexity-bounds

For every performance-sensitive path, name the relevant input symbols, state present and worst-case
time and peak/retained-memory growth, and distinguish algorithmic bounds from measured latency.
Also inspect allocations and copies, I/O amplification, concurrency and lock behavior, output growth,
startup cost, and failure-path cleanup. A finite result page is not proof of bounded work.

Symbols used below: `F` source files; `B` eligible input bytes; `N` eligible rows or sessions; `M`
messages in one session; `R` SQL rows scanned; `C` authoritative candidates; `K` returned/retained
rows; `O` page offset; `W = O + K`; `A = 512` fuzzy scoring rows; `D_A` selected text bytes in that
batch; `D_W` selected text bytes retained in the page window; `D_K` text bytes retained in returned
records; `D_max` the largest one-row or one-session selected text; `J` scoring workers; `Q`
rules/policies; `E` edits or graph edges; `L` emitted or reconstructed bytes; `P` trigram postings
bytes; `P_Q` postings read for one query; and `K_s`/`N_s` selected/catalog skill packages. Bounds
describe current source behavior, not a timeless latency guarantee.

| Key surface | Current time bound | Current peak/retained memory | Latency, output, and regression guard |
| --- | --- | --- | --- |
| Provider discovery and indexing | Discovery `O(F)`; incremental parse/index `O(B_new)`; full rebuild `O(B_all)` | Discovery retains `O(F)` source metadata. A full provider parse currently retains normalized messages, transcript lines, file edits, and the joined transcript for one source, so peak growth is `O(B_session)`; custom trigram rebuild is `O(B + P)`. | Benchmark cold and incremental refresh separately. The existing 40 MiB fixture guards full versus incremental time. Measure peak RSS and cancellation latency before claiming a lower parser bound; trigram rebuild remains another known peak-memory cliff. |
| Session list, resolve, and focused read | Session-span filtering is worst-case `O(R)` constant-time comparisons; favorable indexed list is `O(log N + O + K)` because current paging uses SQL `OFFSET`, while a planner-selected temporary sort is conservatively `O(R log R + O + K)`; zero-limit list is `O(R)` plus any sort; focused read is `O(log M + K)` after resolution; current partial-ID resolution is `O(N)` | `O(K + D_K)` returned rows plus SQLite cache/temp-sort memory; overlap itself adds `O(1)` application memory | Date bounds use closed overlap: known start <= query end and known end >= query start. Do not call list paging keyset-based. Track offset skipping, query plans, and the known case-insensitive `LIKE` session-resolution scan; never hide work behind a small output page. |
| Exact and regex message search | Indexed safe candidates plus authoritative verification `O(P_Q + C + verified bytes)`; unsafe/no-literal regex worst case `O(B)` | Finite page `O(W + D_W + evidence bytes)`; explicit all-results `O(N + D_K + evidence bytes)` | Row-count bounds alone do not bound retained text bytes. Content and JSON-stable tool-argument anchors reuse schema-v4's raw-content trigram superset; RFC 6901 projection and matching remain authoritative. Context is fetched for retained hits in one batched statement, not N+1 queries. |
| Fuzzy message search | `O(B + N + W log W)` aggregate work | `O(D_A + D_W + D_max + J × Q + evidence bytes)`; retained row count is corpus-independent for finite `W`, but text bytes are not | Score the complete eligible corpus in bounded parallel batches, compact retained top-K deterministically, and reject unbounded fuzzy requests. Tool-argument projection is exhaustive because subsequence matches have no required contiguous trigram. Benchmark exact/regex/fuzzy across content, tool name, and tool arguments, including giant rows. |
| Session search | `O(R + B × (Q + 1) + N + K log K)` aggregate work for positive `K`; session-span overlap rejects rows before transcript scoring | `O(K + D_K + batch_budget + D_max)`; overlap adds `O(1)` application memory; explicit all-results makes `K` the number of matches | Bounded batches score on the application worker runtime, but SQLite traversal and global top-K remain serial. A result limit bounds retained hits, not corpus scoring. Preserve deterministic order and compare Rust, Python, CLI, and MCP eligible-ID and ordered-result digests as appropriate. |
| Context and presentation | Context DB/output work `O(K × (before + after + 1))`; evidence/window formatting proportional to retained text | Same order as returned context/evidence | Batch enrichment after page selection. Presentation budgets must not trigger a second corpus scan or change membership/page identity. |
| Skill capabilities and analysis | Skill name resolution `O(K_s × N_s)`; classification/analysis scales with selected source-attributable human bytes × applicable `Q`; repeat mining additionally enumerates phrases from those bytes; MCP presentation adds `O(D + V)` per returned match; relationship graph `O(N log N + E log E)` | Catalog metadata excludes instruction bodies; classification and repeats may materialize the eligible human slice; MCP retains bounded view bytes `O(V)` per returned match while Rust/Python/CLI programmatic results intentionally retain complete selected messages | Bound capability documents by declared bytes, classify only source-authoritative original human text (including only human fragments of mixed messages), preserve exact message/match coordinates, and budget only MCP delivery unless a caller explicitly requests `detail=full`. Benchmark classification separately from catalog resolution. Never introduce all-pairs graph comparison. |
| File recovery and export | File reconstruction `O(E + L)`; full export/publish proportional to selected transcript bytes | One-pass reconstruction `O(L)`; full document export can retain `O(L)` | Preserve the one-pass reconstruction invariant. Use streaming formats for large exports when possible and report whether output is complete. |
| Read-only SQL | Query work is determined by the SQL plan, worst case `O(R)` scanned rows; response offset/limit is applied after execution | Returned rows and cell payloads are bounded only by explicit `limit`/`max_cell_chars`; SQLite cache is separate | Prefer SQL `LIMIT` for expensive queries. Native and MCP elapsed timeout defaults are zero/disabled in current source; a configured timeout is an explicit availability guard, not a search default. |

Every materially changed executable API or adapter must either state its non-trivial time, peak and retained memory, allocation/copy, I/O, output, concurrency/lock, and cleanup bounds beside its owning implementation, or explicitly delegate to the shared owner and state only its adapter overhead. Derive this inventory from the final diff and freeze it in repository contracts; do not duplicate full formulas in constant-size translators or hide synchronous callee work behind an `O(1)` adapter claim.

Performance work requires a comparable baseline and post-change run with the same dataset,
environment, build, and workload. Record latency distribution, throughput, peak RSS, CPU, threads,
process count, output bytes, and correctness digests where applicable. Reject regressions hidden by
small fixtures, page limits, warm caches, changed result sets, or omitted failure paths.

### REQ027-use-tdd

For defects and contract changes, reproduce the problem, add the smallest failing test at the lowest
shared layer, and confirm that it fails for the intended reason. Implement one coherent change, then
cover every affected Rust, Python/PyO3, CLI, MCP, schema, documentation, provider, package, and
installed boundary. Parameterize shared fixtures and sweep meaningful edge combinations without
turning tests into copies of the implementation.

### REQ042-plan-fine-grained-work

Maintain a dependency-ordered task plan for nontrivial work. Each item should name a concrete
outcome, evidence or failing test, implementation scope, verification, and status. Separate
completed work, current work, justified deferrals, and external maintainer actions. Update the plan
as evidence changes; do not leave stale tasks or replace the core blocker with peripheral cleanup.

### REQ043-reread-active-plans-after-compaction

After context compaction or session resumption, reread every active execution plan sequentially
from start to finish before editing. Then reconcile all work performed in the current session
against the plan's requirements, dependency order, intentional differences, non-goals, TDD gates,
complexity bounds, and completion criteria. A summary or targeted excerpt can restore orientation,
but it cannot replace this end-to-end regression audit.

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
| MCP | Configured finite page; currently 15 by default | Tool results enter an agent context directly, and the page is sized so an ordinary call lands near half of `[mcp].max_tool_result_chars` |

Every surface accepts an explicit positive page size. `all_results` states a complete-corpus request
explicitly for literal, regex, or queryless search. Fuzzy search always requires finite retention
and rejects `all_results`. A purpose bundle or `[search.message-search].default_limit` may
deliberately supply a finite operation-level default before the surface default is considered.

This distinction applies to result membership. Separate presentation windows may shorten displayed
content without removing a result.

### REQ004-separate-retrieval-presentation

Line windows, field-view limits, match-view budgets, response formats, and whitespace
presentation must never change matching, ranking, hit membership, context membership, result count,
offsets, next-page identity, classification categories, or policy digests.

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
- each returned field view's absolute `field_start_char` and
  `field_end_char_exclusive`;
- `additional_field_text=none|before|after|before_and_after`;
- nullable `field_total_chars`, populated only when known without an extra full-field scan solely
  for metadata;
- view-relative match marker ranges named `view_start_char` and
  `view_end_char_exclusive`;
- parameter origins when full receipts are requested.

CLI JSONL begins with metadata and ends with a terminal record. A consumer that never receives the
terminal record must not infer that an interrupted stream was complete.

**When the schema version moves.** `MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION`
(`rust/ai-session-search-core/src/message_search.rs`) is the signal to a consumer that the response
shape it was written against has changed. Increment it, in one commit that also moves the canonical
serializer, the closed MCP `outputSchema`, the Python stubs, and every fixture, when a published
response **removes a field, renames one, changes a field's type, or changes the meaning of a value
already emitted under that name**. Adding an optional field does not increment it: a consumer
reading by name is unaffected, and spending an increment on an addition teaches consumers to ignore
the signal.

Before the first published release the version does not move at all, and the reason is that the
signal has no recipient: an increment tells an existing consumer to re-read the contract, and until
a release exists there is none. Reshaping the response is therefore cheap now and expensive later,
which is an argument for taking deliberate shape changes before publishing rather than after. What
a pre-release reshape still owes is the lockstep edit above, because the `outputSchema` is closed
and two tests validate it against live responses. This paragraph becomes binding at the first
published release; the rule itself does not change at that point, only the set of consumers it
protects.

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

The canonical end-user package `ai-session-search` lives under the resolved AI Session Search
application root and contains a real `SKILL.md`. Harness-native skill directories contain links to
that canonical package where the harness supports links. App ownership and harness discovery are
separate concepts.

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
| ChatGPT Codex desktop and Codex CLI/IDE | shared `~/.codex/config.toml` | `~/.agents/skills` | `~/.codex/AGENTS.md` |
| Gemini CLI | `~/.gemini/settings.json` | `~/.gemini/skills` | `~/.gemini/GEMINI.md` |
| Antigravity App/IDE/current CLI | `~/.gemini/config/mcp_config.json` | App/IDE: `~/.gemini/config/skills`; CLI: `~/.gemini/antigravity-cli/skills` | shared `~/.gemini/GEMINI.md` |
| Antigravity compatibility | `~/.gemini/antigravity-cli/settings.json`; `~/.gemini/antigravity/mcp_config.json` | tested compatibility roots only | no duplicate instruction file |
| Pi coding agent | none: Pi deliberately has no MCP client | `~/.pi/agent/skills` | `~/.pi/agent/AGENTS.md` |
| Prime Agent | no local stdio MCP: Prime's kernel integration currently accepts remote HTTP servers only | `~/.prime/agent/skills` | `~/.prime/agent/AGENTS.md` |

Pi and Prime Agent also discover `~/.agents/skills`, but each explicit selector installs one link in
its harness-native root. This avoids coupling their installation to Codex detection and keeps
ownership/status output attributable to the selected harness. Their repeatable `--skill PATH` and
`--no-skills` flags remain harness runtime controls: explicit paths are additive even when automatic
skill discovery is disabled. Other supported clients retain their documented native MCP shapes. Do
not fabricate every instruction filename or unsupported MCP transport for every harness.

### REQ020-normalize-provider-records

Claude Code, Claude Desktop local agent, ChatGPT Codex desktop and Codex CLI/IDE, Cursor, Antigravity App/IDE/CLI, Pi,
Prime Agent, Google AI Studio, and Gemini CLI local transcripts normalize into the shared session/message model.
List, show, search, message reads, export, analysis, Python, Rust, CLI, and MCP must operate on those
canonical records rather than adapter-specific response models.

Do not add a second parser for an opaque or duplicative database without evidence that it contains
unique supported data.

Adding a provider variant is an index-compatibility event even when the schema generation does not
move: Prime Agent shipped at generation 5, so a `1.0.0rc1` executable opening an index that a later
build has written cannot decode `sessions.provider` for those rows. Two aise executables on one
machine is the ordinary case (one per package manager, or a harness registration pinned to an
absolute path), and it produced exactly this on 2026-08-13 in a Pi session: every command of the
older build failed while reading a Prime Agent row with advice to run `aise reindex --full`, which
enumerates the same rows and failed identically. The open path therefore inspects
`select distinct provider from sessions` (covered by `idx_sessions_provider`, about 1.4 ms on a
6,946-session index) and refuses once with `SchemaState::UnknownProviders`, naming the provider,
this build's version, and the fix — upgrade aise — the same way `SchemaState::Newer` refuses a newer
generation. The per-row decode failure keeps the same upgrade advice for a row written after the
open. Bumping the schema generation for a provider addition was rejected because it forces every
user through a full reparse for a change that touches no existing row.

### REQ021-state-local-data-boundary

The product searches locally discoverable transcripts. Cloud-only account history with no local
record is outside the index and must not be advertised as searchable.

### REQ022-separate-guidance-capabilities

Harnesses load and interpret `SKILL.md`. Aise does not execute that prose or invoke a model.
Deterministic runnable behavior belongs in an adjacent, closed-schema `aise-capability.toml` parsed by
aise. Skill guidance and machine capability declarations are related package components with
different execution authorities.

### REQ023-accept-capability-parameters

Runnable capabilities must accept typed runtime parameters in addition to selecting a package by
name or authorized path. Scope, provider, session class, time range, paging, all-results selection,
and compatible additional packages must flow through the same Rust request model and be exposed by
Python, CLI, and MCP. A typed direct definition replaces only the primary package's executable
rules for one call; the selected `SKILL.md` still owns name, version, instructions, and path
authorization. Packaged and direct rules use one validator and deterministic digest. Unknown
fields, empty definitions, and incompatible capability compositions must fail closed.

### REQ024-delegate-package-updates

Package installation and integration installation remain separate. `aise package check` is
read-only. `aise package update` detects uv tool, uv/pip, pip, pipx, Cargo, Homebrew, native archive,
direct source, or unknown ownership and delegates only to a verified owning manager after
confirmation.

Source checkout, direct URL, Cargo path/Git, and unknown installations receive guidance instead of
silent registry replacement. After a manager update, refresh only manifest-recorded owned skill
roots; stable executable paths, aliases, MCP registrations, and managed instructions do not need
rewriting, and an update must not discover new clients. Uninstall integrations before removing the
global executable.

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
  the user-local `aise` executable and `args=["mcp","serve"]`.
- The uv-managed and Cargo-managed executables both ran normally, including under an
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
| `REQ037-explore-before-change`; `REQ038-map-semantic-ownership`; `REQ039-reuse-or-improve-architecture`; `REQ048-adopt-proven-libraries`; `REQ040-eliminate-semantic-duplication`; `REQ043-reread-active-plans-after-compaction` | repository guidance, complete active plans, architecture seams, dependency manifests/locks/imports, upstream primary sources, git history, installed state, prior-session evidence, and current-session changes | end-to-end plan reread after compaction/resumption, plan-to-diff regression audit, graph plus coverage when available, exact-source review, dependency/library comparison, history diff, installed reproduction |
| `REQ041-optimize-multi-objective-outcomes`; `REQ044-automate-safe-problem-solving`; `REQ045-own-and-clean-resources`; `REQ046-preserve-boundary-results`; `REQ047-return-actionable-recovery` | typed service owners, iterator/transaction/subprocess lifecycles, Rust/PyO3/Python adapters, CLI/MCP termination, installers/updaters, public error types | automatic-recovery and invalid-use fixtures; completion/break/drop/cancel/error/broken-pipe cleanup; GIL/mutex tests; return/error parity; actionable-error snapshots with preserved/cleaned-state assertions |
| `REQ010-protect-complexity-bounds`; `REQ030-benchmark-risky-paths` | `db.rs`, `service.rs`, `trigram_index.rs`, `analysis_pipeline.rs`, `files.rs`, `scripts/benchmark_release.py` | complexity comments/tests, deterministic scale fixtures, latency/CPU/RSS/output benchmark reports |
| `REQ002-share-typed-contract`; `REQ003-preserve-surface-semantics`; `REQ004-separate-retrieval-presentation`; `REQ005-return-match-evidence`; `REQ006-report-extent-honestly`; `REQ007-preserve-page-identity`; `REQ008-reject-hidden-cutoffs`; `REQ009-bound-fuzzy-search`; `REQ012-reject-invalid-combinations`; `REQ013-resolve-parameters-by-origin` | `rust/ai-session-search-core/src/message_search.rs`, `service.rs`, `messages.rs`, `mcp_server.rs` | `rust/ai-session-search-core/tests/message_search_contract.rs`, service/MCP unit tests, `tests/test_native_binding.py` |
| `REQ011-validate-language-boundaries` | `rust/ai-session-search-python/src/lib.rs`, `ai_session_search/_native.pyi` | native binding tests, stubtest, runtime/stub parity |
| `REQ014-use-platform-app-paths`; `REQ015-separate-app-harness-roots`; `REQ016-support-multiple-skill-roots`; `REQ017-preserve-install-ownership`; `REQ018-preserve-unmanaged-content`; `REQ019-verify-each-harness` | `config.rs`, `integrations.rs`, `skills.rs`, `skill_manifest.rs`, `text_file_transaction.rs` | integration/config unit tests and repository contracts |
| `REQ020-normalize-provider-records`; `REQ021-state-local-data-boundary` | provider modules under `rust/ai-session-search-core/src/providers/` | provider fixtures, incremental/full parse parity, session-id binding |
| `REQ022-separate-guidance-capabilities`; `REQ023-accept-capability-parameters` | `skill_catalog.rs`, `skill_capability.rs`, `skills.rs`, `mcp_server.rs` | skill catalog, process lifecycle, Python, CLI, and MCP capability tests |
| `REQ024-delegate-package-updates` | `update.rs`, release configuration | package ownership/update tests and installed `aise package status/check` |
| `REQ027-use-tdd`; `REQ028-test-cross-surface-contracts`; `REQ029-dogfood-installed-artifacts`; `REQ033-commit-coherent-progress`; `REQ034-gate-release-artifacts` | `tests/`, Rust test suites, `run_ci_local.sh`, release workflows | focused tests followed by all local release-gate stages |
