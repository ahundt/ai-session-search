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

## Prioritized requirement catalog

Treat these identifiers as stable review anchors. The detailed rationale and verification map live
in the maintainer requirements document. Catalog order expresses priority; the number preserves
identity and does not determine priority.

### P0 — discovery, architecture, correctness, and data safety

- `REQ037-explore-before-change` — Search the repository, prior sessions, docs, tests, history, and
  current installed state before judging or changing behavior.
- `REQ038-map-semantic-ownership` — Map the conceptual behavior, data flow, defaults, validation,
  ordering, lifecycle, and owners across every affected surface before designing a fix.
- `REQ039-reuse-or-improve-architecture` — Use the strongest existing seam and shared abstraction,
  improving it when evidence justifies the change instead of creating a parallel mechanism.
- `REQ048-adopt-proven-libraries` — Search manifests, lockfiles, imports, and existing abstractions before building infrastructure. Reuse a fitting dependency first;
  otherwise evaluate mature, widely used, actively maintained libraries for contract fit, lifecycle safety, security, MSRV/platform support, performance, dependency
  cost, and release risk. Adopt one only when it robustly removes more custom machinery than it adds; record the concrete gap when custom code remains.
- `REQ040-eliminate-semantic-duplication` — Search for duplicated meaning, not only repeated text;
  keep one authoritative implementation for each contract and thin surface adapters.
- `REQ041-optimize-multi-objective-outcomes` — Improve or preserve correctness, task success,
  usability, latency, throughput, memory, output size, cost, maintainability, and user time together.
- `REQ044-automate-safe-problem-solving` — When intent, authority, and a safe deterministic action
  are clear, solve the problem in the owning service; make typed correct use the easiest path and
  keep semantic choices explicit.
- `REQ045-own-and-clean-resources` — Give connections, snapshots, iterators, locks, subprocesses,
  buffers, and temporary artifacts one RAII owner; close without draining unread work on completion,
  break, drop, cancellation, error, broken pipe, and foreign-language exit paths.
- `REQ046-preserve-boundary-results` — Preserve values, nullability, identities, ordering,
  coordinate spaces, terminal states, errors, and ownership across Rust, PyO3, Python, CLI, MCP,
  and serialization boundaries.
- `REQ047-return-actionable-recovery` — When automatic safe completion is impossible, return the
  exact failed state, why automation stopped, what was preserved or cleaned up, the smallest next
  action, and a verification step.
- `REQ010-protect-complexity-bounds` — State and verify time, retained-memory, allocation, I/O, latency,
  concurrency, and output-growth bounds; protect them with representative benchmarks.
- `REQ027-use-tdd` — Reproduce the defect and add the smallest failing shared-layer test
  before implementation, then cover every affected adapter and installed surface.
- `REQ042-plan-fine-grained-work` — Keep a current dependency-ordered task plan with explicit
  evidence, verification, completion, deferral, and external-action states.
- `REQ043-reread-active-plans-after-compaction` — After context compaction or session resumption,
  reread every active plan sequentially from start to finish, then reconcile all work performed in
  the current session against its requirements, ordering, non-goals, tests, and completion gates
  before resuming edits. Targeted excerpts and summaries are not substitutes.
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
  presentation policy, completeness, and whether the selected field has text before or after each
  returned view so shortened output cannot look complete.
- `REQ007-preserve-page-identity` — Evidence, context, formatting, or presenter failures must not
  change offsets, next-page identity, or the selected result set.
- `REQ008-reject-hidden-cutoffs` — Do not add silent row, byte, content, or elapsed-time cutoffs;
  expose intentional bounds as named parameters with origins.
- `REQ009-bound-fuzzy-search` — Score the complete eligible fuzzy corpus while retaining a finite,
  deterministic top-K page; reject unbounded fuzzy requests.
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
- `REQ020-normalize-provider-records` — Parse supported local provider formats, including Prime
  Agent through the shared Pi-family parser, into one canonical session/message model and exercise
  every public read surface against it.
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
- `REQ028-test-cross-surface-contracts` — Freeze intentional parity and intentional differences
  across Rust, Python, CLI, MCP, schemas, docs, examples, and installed behavior.
- `REQ029-dogfood-installed-artifacts` — Exercise the installed executable, MCP server, skills,
  capabilities, provider parsing, and lifecycle from clean or minimally inherited environments.
- `REQ030-benchmark-risky-paths` — Record comparable baselines and rerun them for changes affecting
  scale, latency, memory, concurrency, indexing, or output volume.
- `REQ031-align-docs-and-schemas` — Change source comments, CLI help, MCP schemas, Python docstrings,
  stubs, examples, and public docs together.
- `REQ032-record-design-tradeoffs` — Record selected mechanisms, serious alternatives, UX
  consequences, complexity, evidence, and unresolved external actions.
- `REQ033-commit-coherent-progress` — Preserve recoverability with focused commits that name exact
  files, behavior, rationale, and verification.
- `REQ034-gate-release-artifacts` — Run the full release gate from a clean commit, build once,
  verify exact artifacts, and require protected-environment approval to publish.
- `REQ035-critically-review-ai-output` — Treat agent reviews as hypotheses until checked against
  current code, tests, installed behavior, and primary sources.
- `REQ036-preserve-existing-strengths` — Retain verified mechanisms and useful design evidence when
  revising plans, skills, docs, or implementations; do not replace additions with lossy rewrites.

## Execution sequence

1. **Discover (`REQ037-explore-before-change`, `REQ038-map-semantic-ownership`,
   `REQ039-reuse-or-improve-architecture`, `REQ048-adopt-proven-libraries`,
   `REQ040-eliminate-semantic-duplication`,
   `REQ043-reread-active-plans-after-compaction`).** Read
   repository guidance, current docs, source, tests, git history/state, installed behavior, and
   prior sessions. Use AI Session Search before guessing about earlier work. Use the code graph for
   symbols and call paths, check coverage, then read exact source and non-code files. Map shared ownership, dependency manifests, imports, and existing abstractions before
   editing. When no current dependency fits, compare mature libraries against the full product and release constraints before choosing a dependency or custom code. After
   compaction or resumption, reread each active plan end-to-end and audit the current session's edits and decisions against it before continuing.
2. **Frame (`REQ041-optimize-multi-objective-outcomes`,
   `REQ044-automate-safe-problem-solving`, `REQ045-own-and-clean-resources`,
   `REQ046-preserve-boundary-results`, `REQ047-return-actionable-recovery`,
   `REQ010-protect-complexity-bounds`, `REQ042-plan-fine-grained-work`).** Classify claims as
   requirement, measurement, or hypothesis.
   Define correctness, UX, resource, compatibility, and lifecycle success criteria. State symbols
   and present/worst-case cost bounds, then make the task plan dependency-ordered and testable.
   Decide which problems the owning service solves automatically, which choices must stay explicit,
   who owns cleanup, what every boundary returns, and the exact recovery guidance for conditions
   that genuinely require user or maintainer action.
3. **Design (`REQ001-preserve-user-data` through `REQ025-justify-timeouts`).** Start at the shared
   typed service and preserve deliberate surface differences, provider normalization, platform
   paths, ownership states, and parameter precedence. Prefer one strong composable mechanism;
   record serious alternatives and why the selected design is easier to use correctly and harder
   to misuse.
4. **Test and change (`REQ027-use-tdd`, `REQ028-test-cross-surface-contracts`).** Reproduce first,
   add the smallest failing shared-layer test, implement one coherent change, and cover Rust,
   Python/PyO3, CLI, MCP, schemas, docs, examples, provider fixtures, packaging, and installed
   behavior wherever affected.
5. **Measure (`REQ010-protect-complexity-bounds`, `REQ030-benchmark-risky-paths`).** Record a
   comparable baseline for risky paths, rerun the same workload, and report dataset, environment,
   latency distribution, throughput, peak RSS, allocation/copy behavior, I/O, output bytes, and
   complexity interpretation. Treat historical timings as samples, not guarantees.
6. **Dogfood (`REQ026-reproduce-material-claims`, `REQ029-dogfood-installed-artifacts`,
   `REQ035-critically-review-ai-output`).** Exercise the installed executable, MCP server,
   skills, deterministic capabilities, provider parsing, and lifecycle from clean or minimally
   inherited environments. Separate configuration, discovery, executable resolution, process,
   protocol, permission, parser, presentation, and harness failures. Exercise package
   status/check, integration dry-run/install/status/uninstall, skill validation, MCP
   initialize/tools-list, and real provider parsing. Verify AI reports directly.
7. **Gate (`REQ033-commit-coherent-progress`, `REQ034-gate-release-artifacts`).** Run focused checks
   first, then `./run_ci_local.sh` from a clean commit. Test applicable uv tool/project, pip, Cargo
   registry/path/Git, and native-archive pathways. Exactly one manager should own the global `aise`;
   report every PATH candidate. Build once, verify exact artifacts, and never publish without
   protected-environment maintainer approval.
   Keep commit messages and other maintainer-facing prose cold readable: lead with stable behavior
   phrases and omit internal codes, transient task/session identifiers, and raw implementation
   values unless a public contract or exact verification step requires the identifier.
8. **Preserve (`REQ031-align-docs-and-schemas`, `REQ032-record-design-tradeoffs`,
   `REQ036-preserve-existing-strengths`).** Keep comments, help, schemas, stubs, examples,
   design evidence, test matrices, and verification maps aligned. Revise the narrowest existing
   requirement; add a new identifier only for a distinct contract.
