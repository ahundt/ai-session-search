---
name: maintain-ai-session-search
description: Maintain, audit, test, install, and release the AI Session Search repository. Use for provider parsing, Rust/CLI/MCP/Python API changes, search limits or match evidence, configuration and package ownership, harness MCP/skill/instruction integration, performance bounds, dogfooding, release readiness, or cumulative project-requirement work in this checkout. This is repository-internal developer guidance, not an end-user skill shipped by aise.
---

# Maintain AI Session Search

Work from measured repository and installed-state evidence. Preserve the product contracts recorded
in [the maintainer requirements and design decisions](../../../docs/development/maintainer-requirements-and-design-decisions.md).
Read that document before changing public behavior, installation, configuration, provider parsing,
budgets, capabilities, or release machinery.

When the task affects match evidence, typed skill capabilities, ownership lifecycle, updates, or
installed dogfood, also read the applicable detailed requirement and tracked sources in the
document's **Verification map**. If the checkout has a focused maintainer note for that topic, use
it as supplemental historical evidence. Preserve verified mechanism comparisons, invariants, state
taxonomy, deferrals, test matrices, and complexity analysis; do not replace them with this skill's
summary.

## Establish evidence

1. Read repository guidance, the relevant development docs, source, tests, and git state.
2. Before guessing about earlier work, use the `ai-session-search` MCP:
   `search_sessions` or `search_messages`, then `get_session`. Keep requirements, measured
   evidence, and previous agent claims distinct.
3. Use the codebase knowledge graph for symbols and call paths. Check index coverage after finding
   material paths; read exact source or use literal search for any missed ranges and non-code files.
4. Classify important statements as requirement, measured evidence, or hypothesis. Reproduce
   external AI reports before adopting them.

## Prioritized requirement catalog

Treat these identifiers as stable review anchors. The detailed rationale and verification map live
in the maintainer requirements document.

### P0 — correctness and data safety

- `REQ001-preserve-user-data` — Never lose source sessions, indexes, configuration, edits, or
  unrelated work while installing, migrating, repairing, or uninstalling.
- `REQ002-share-typed-contract` — Route Rust, Python, CLI, and MCP through shared typed requests and
  responses; adapters translate syntax, not product semantics.
- `REQ003-preserve-surface-semantics` — Keep deliberate surface differences explicit: when no
  purpose or operation default applies, omitted literal/regex/queryless limits mean all results in
  Rust, Python, and CLI, while MCP supplies a finite context-safe page; fuzzy search is always
  finite.
- `REQ004-separate-retrieval-presentation` — Presentation windows and character budgets may shorten
  displayed values but never alter matching, rank, membership, context, or pagination.
- `REQ005-return-match-evidence` — Every queried hit exposes visible, match-centered evidence;
  literal mode also preserves the exact source occurrence and coordinates.
- `REQ006-report-extent-honestly` — Structured output states returned count, paging, ordering,
  presentation policy, completeness, and omitted sides so shortened output cannot look complete.
- `REQ007-preserve-page-identity` — Evidence, context, formatting, or presenter failures must not
  change offsets, next-page identity, or the selected result set.
- `REQ008-reject-hidden-cutoffs` — Do not add silent row, byte, content, or elapsed-time cutoffs;
  expose intentional bounds as named parameters with origins.
- `REQ009-bound-fuzzy-search` — Score the complete eligible fuzzy corpus while retaining a finite,
  deterministic top-K page; reject unbounded fuzzy requests.
- `REQ010-protect-complexity-bounds` — Enrich retained rows only, avoid N+1 reads and pre-page
  content copies, and test the stated time and memory bounds.
- `REQ011-validate-language-boundaries` — Keep PyO3 conversions, Rust types, Python stubs,
  exceptions, nullability, enums, and serialized output lossless and aligned.
- `REQ012-reject-invalid-combinations` — Reject conflicting or unsatisfiable parameter sets with
  actionable errors instead of returning misleading empty or partial results.

### P1 — product and integration contracts

- `REQ013-resolve-parameters-by-origin` — Resolve explicit call values before purpose bundles,
  operation config, surface config, and typed defaults; expose origins when requested.
- `REQ014-use-platform-app-paths` — Keep config, database, cache, receipts, and app-owned skills in
  platform-appropriate AI Session Search locations.
- `REQ015-separate-app-harness-roots` — Keep one canonical app-owned skill package and link it into
  each harness-native discovery root instead of conflating those locations.
- `REQ016-support-multiple-skill-roots` — Accept repeated custom skill roots and multiple selected
  harnesses without collapsing, overwriting, or silently skipping destinations.
- `REQ017-preserve-install-ownership` — Track package, integration, instruction, alias, and skill
  ownership separately; mutate or remove only bytes proven owned.
- `REQ018-preserve-unmanaged-content` — Preflight transactional changes and preserve unknown keys,
  files, directories, aliases, and user-modified managed trees.
- `REQ019-verify-each-harness` — Validate configuration, skill discovery, MCP startup, permission,
  and tool invocation separately for every supported app, IDE, and CLI.
- `REQ020-normalize-provider-records` — Parse supported local provider formats into one canonical
  session/message model and exercise every public read surface against it.
- `REQ021-state-local-data-boundary` — Never imply that cloud-only history without a local
  transcript is searchable.
- `REQ022-separate-guidance-capabilities` — Let harnesses interpret `SKILL.md`; let aise execute only
  adjacent, deterministic, closed-schema capability declarations.
- `REQ023-accept-capability-parameters` — Expose typed runtime scope, selector, paging, and
  composition parameters through Rust, Python, CLI, and MCP in addition to selecting a
  `capability.toml` package.
- `REQ024-delegate-package-updates` — Detect the verified package owner and delegate updates to it;
  never silently replace source, URL, path, Git, or unknown installations.
- `REQ025-justify-timeouts` — Default search and native-query time limits to disabled; add a finite
  timeout only for a measured safety or availability need and document zero/omission semantics.

### P2 — maintainer execution

- `REQ026-reproduce-material-claims` — Reproduce external reports and distinguish discovery,
  permission, process, protocol, parser, and presentation failures.
- `REQ027-use-tdd` — Add the smallest failing shared-layer test before fixing a defect, then cover
  every affected adapter.
- `REQ028-test-cross-surface-contracts` — Freeze intentional parity and intentional differences
  across Rust, Python, CLI, MCP, schemas, docs, examples, and installed behavior.
- `REQ029-dogfood-installed-artifacts` — Exercise the installed executable, MCP server, skills,
  capabilities, provider parsing, and lifecycle from clean or minimally inherited environments.
- `REQ030-benchmark-risky-paths` — Record comparable baselines and rerun them for changes affecting
  scale, latency, memory, concurrency, indexing, or output volume.
- `REQ031-align-docs-and-schemas` — Change source comments, CLI help, MCP schemas, Python docstrings,
  stubs, examples, and public docs together.
- `REQ032-record-design-tradeoffs` — Record selected mechanisms, rejected alternatives, UX
  consequences, complexity, evidence, and unresolved external actions.
- `REQ033-commit-coherent-progress` — Preserve recoverability with focused commits that name exact
  files, behavior, rationale, and verification.
- `REQ034-gate-release-artifacts` — Run the full release gate from a clean commit, build once,
  verify exact artifacts, and require protected-environment approval to publish.
- `REQ035-critically-review-ai-output` — Treat agent reviews as hypotheses until checked against
  current code, tests, installed behavior, and primary sources.
- `REQ036-preserve-existing-strengths` — Retain verified mechanisms and useful design evidence when
  revising plans, skills, docs, or implementations; do not replace additions with lossy rewrites.

## Preserve cross-surface contracts

- Route Rust, CLI, MCP, and Python through shared typed service requests. Keep PyO3 inputs,
  outputs, stubs, docstrings, schemas, CLI help, and examples aligned.
- Treat provider discovery and parsing as shared index inputs. Validate Claude Code, Claude Desktop
  local agent, Codex App/CLI/IDE, Cursor, Antigravity App/IDE/CLI, Pi, AI Studio, and Gemini CLI.
  Never claim cloud-only history is locally searchable.
- Keep literal, regex, and queryless message search unbounded when Rust, CLI, or Python callers omit
  a limit. MCP alone supplies a finite default. Fuzzy search always requires finite retention.
  Explicit caller limits remain supported everywhere.
- Keep presentation budgets independent of retrieval. A shortened boundary view must not change
  matching, ranking, membership, context, pagination, or `all_results`.
- Return visible match-centered evidence for queried hits and structured extent/pagination metadata
  that cannot be mistaken for complete content or a complete result set. Preserve explicit
  first/last/all expansion.
- Reject hidden aggregate cutoffs and accidental elapsed-time limits. Add a timeout only when an
  operation requires one for safety, expose it explicitly, and document omission and zero.
- State and test asymptotic cost. Enrich only retained page rows; avoid pre-page full-content
  copying, N+1 lookups, and metadata proportional to omitted source text.

## Preserve configuration and integration ownership

1. Resolve parameters in this order: explicit call/CLI, `AI_SESSION_SEARCH_*`, platform config,
   embedded default. Keep config and index under platform-appropriate AI Session Search app
   directories.
2. Keep end-user skill packages canonical under the resolved AI Session Search config root and
   symlink them into every selected harness-native discovery directory. Support repeated custom
   skill roots.
3. Do not package or install this `maintain-ai-session-search` developer skill. Its canonical copy
   is the repository `.agents/skills` entry; repository-local harness links may point to it.
4. Keep harness surfaces distinct:
   - Claude: MCP configuration, `~/.claude/skills`, and managed `CLAUDE.md`.
   - Codex App/CLI/IDE: shared `~/.codex/config.toml`, `~/.agents/skills`, and `AGENTS.md`.
   - Gemini/Antigravity: current MCP configuration, each documented global skill root, and shared
     `GEMINI.md`; retain tested legacy paths only as compatibility targets.
5. Preflight lifecycle changes. Preserve unmanaged keys, Markdown, directories, aliases, and
   user-edited managed trees. Uninstall only bytes proven owned unless the user explicitly selects
   exact destructive cleanup.
6. Keep package ownership separate from integration ownership. `aise package update` delegates to
   a verified owning package manager; source/direct-URL installs receive guidance and are never
   silently replaced. Refresh integrations after a package update.

## Change with TDD

1. Add the smallest failing test at the lowest shared layer, then add adapter and
   repository-contract tests for each affected surface.
2. Implement one coherent change without unrelated cleanup.
3. Run focused tests first. For performance work, record a baseline and rerun the same benchmark.
4. Dogfood the installed command, MCP server, and relevant skill from clean or minimally inherited
   environments. Treat permission denial separately from discovery or process-start failure.
5. Run `./run_ci_local.sh` from a clean commit before declaring release-candidate readiness.
6. Read the commit guide, commit at coherent progress points, and report exact tests and unresolved
   external maintainer actions.

## Validate release and installation

- Test uv tool, uv project, pip, Cargo registry/path/Git, and native-archive pathways as applicable.
  Exactly one package manager should own the global `aise`; report every PATH candidate.
- Exercise `aise package status`, `aise package check`, integration dry-run/install/status/uninstall
  lifecycle fixtures, skill validation, MCP initialize/tools-list, and real provider parsing.
- Verify all current client files and discovery links from actual installed state. A configured JSON
  key is not sufficient proof of MCP startup; an MCP handshake is not sufficient proof that a
  harness permission policy allows invocation.
- Do not publish without protected-environment maintainer approval. Build artifacts once, verify the
  exact bytes, and preserve registry immutability.

## Keep durable requirements current

When a product or maintainer contract changes, revise the narrowest existing `REQ` item or add the
next stable identifier. Preserve priority order, reconcile conflicts explicitly, update the
detailed maintainer document and verification map, and avoid duplicating the same rule under
several names.
