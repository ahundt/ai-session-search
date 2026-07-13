# AI Session Search major migration

## Objective

Make this repository the canonical Apache-2.0 AI Session Search monorepo. Rust is
the default end-to-end engine; Python remains a first-class in-process API through
PyO3. CLI, MCP, and Python adapters share one typed service layer and one index
lifecycle. The release is a deliberate major version, not a permanent compatibility
stack.

Sessiongrep supplies the canonical overlapping CLI, MCP, indexing, parameterization,
and performance behavior. The target is a superset of useful outcomes, not a union
of legacy complexity: port the rare valuable aise-only capabilities into the
sessiongrep-derived typed services while retaining sessiongrep's clearer and more
composable simplifications.

## Non-negotiable invariants

- Work locally on `feat/ai-session-search-rust-migration`; no public remote or push
  without a new explicit one-time authorization.
- Search for semantic duplication before each extraction or port.
- Use TDD for behavior, failure, lifecycle, and migration changes.
- Preserve session data and rollback capability; derived indexes may be rebuilt but
  never silently discarded.
- Use RAII for locks, transactions, worker pools, cancellation, temporary files, and
  MCP shutdown.
- Keep CLI, MCP, and Python operations, defaults, filters, errors, pagination, and
  result semantics generated from or tested against the same service contracts.
- No absolute user paths, uncommon-tool assumptions, magic timeouts, or duplicated
  policy in committed code or artifacts.

## Completed foundation

- [x] Audit both histories for large blobs, suspicious artifacts, credential markers,
  licenses, shallow ancestry, and repository integrity.
- [x] Extend the history helper with general literal follower exclusions and a
  binary-preserving selective-import policy using unit, CLI, and integration tests.
- [x] Validate 330 helper tests, 29 intentional skips, Ruff, and 27 focused tests.
- [x] Build a complete sessiongrep mirror and join local unpushed tip `a0f6cfd`.
- [x] Dry-run and execute the clone-first monorepo merge locally.
- [x] Preserve the complete selected leader/follower ancestries, exclude the legacy
  GIF from destination history, retain all other binaries, and verify source drift.
- [x] Create the single local migration branch with no remotes configured.
- [x] Establish the root Cargo workspace and tracked sanitized provenance.

## Ordered implementation ledger

- [ ] Baseline legacy aise, imported sessiongrep, CLI, MCP, Python, index lifecycle,
  correctness, latency, memory, and artifact size on fixed fixtures.
- [x] Inventory semantic overlap and produce a capability/provider/API parity matrix.
- [ ] Extract Rust library services for catalog, messages, tools, files, export,
  sources, maintenance, and optional analysis; adapters own no policy.
- [ ] Port AI Studio and Gemini providers plus every aise filter, recovery, export,
  analysis, graph, taxonomy, configuration, and public Python capability.
- [ ] Add a mixed Rust/Python maturin/PyO3 package with bounded typed conversions,
  stable synchronous Python API, GIL release for native work, and differential tests.
- [ ] Keep `aise` and temporary `aise-mcp` thin during parity work; generate
  CLI/MCP/Python parity tests and remove redundant implementations at the major boundary.
- [x] Implement owned background refresh cancellation and graceful MCP initialize,
  operation, EOF, and shutdown behavior.
- [ ] Failure-inject process signals and prove child/worker cleanup without relying on
  normal EOF or destructor execution.
- [ ] Failure-inject lock permissions/types/contention, schema backfill, SQLite
  BUSY/LOCKED/I/O/corruption/disk-full/WAL/checkpoints, process crashes, and signals.
- [x] Implement a configurable SQLite backup/migrate/validate/atomic-publish/rollback
  mechanism that never raw-copies a live WAL database. Stopping external writers and
  proving old/new exclusion remain part of the pending local cutover gate.
- [x] Finalize the major-version identity: repository/distribution
  `ai-session-search`, executable `aise`, Python import `ai_session_search`, and one
  platform-derived config/index identity.
- [ ] Support and clean-install-test uv add/pip/tool/uvx, pip, Cargo registry/Git/path,
  sdist fallback, platform wheels, signed native archives, and installers.
- [ ] Generate Apache-2.0 metadata, provenance, relevant NOTICE content, third-party
  license inventory, SBOM, checksums, signatures, and artifact-content tests.
- [ ] Build immutable release candidates once, install-test exact artifacts on every
  supported platform, migrate the local installation with rollback ready, and retire
  legacy paths only after acceptance.
- [ ] As the final CLI/MCP architectural step, move stdio serving to `aise mcp serve`,
  update every installer/config contract, rerun startup/shutdown/parity/install gates,
  and remove the temporary `aise-mcp` executable only after those gates pass.
- [ ] After executable consolidation and its complete regression gate, rework the
  sanitized fixture-driven demo workflow for the final capabilities. Treat its script
  as an end-to-end test; publish generated GIF/video externally and never commit media.

## Verified implementation checkpoints

- Rust AI Studio and Gemini CLI providers are indexed through the shared normalizer;
  provider, search, and integration tests pass (`aef331a`).
- `SessionSearch` owns configuration and database lifecycle; catalog, message, file,
  index, and maintenance services are shared by native adapters (`7f73dfb`, `5432290`).
- Typed PyO3 APIs release the GIL for native work and expose indexed catalog, message,
  file search/history/cross-reference, export, canonical provider inventory, and
  refresh operations (`8fe31dc`, `aee1f96`, `5e94162`, `440ba5e`, `6154ff3`).
  Remaining legacy Python analysis/scanner removal is not complete.
- Destination-independent `ExportService` and canonical `SourceService` keep rendering
  and provider discovery out of CLI, MCP, and Python adapters (`f701522`, `e8d24b5`).
- Public Rust catalog/message/file/export/source consumers compile in a downstream
  workspace fixture; current-toolchain strict rustdoc, doctest, and all-feature Clippy
  gates pass (`098a96c`). Exact Rust 1.85 execution remains CI-enforced because that
  toolchain was unavailable locally; do not record local MSRV proof until it runs.
- MCP provider schemas derive from the canonical registry, retain explicit agent-facing
  guidance, and dispatch every advertised provider through all provider-filtered tools
  (`64d83d6`, `f57c34f`).
- Config and cache overrides preserve non-UTF-8 paths. New installs use platform state
  directories, while an existing legacy database remains selected until transactional
  cutover writes an explicit destination (`ad6cc0a`, `7d7f580`).
- Typed `AnalysisService` centralizes correction classification, planning usage, and
  role statistics without changing CLI rendering. Role counts now reuse canonical
  message predicates instead of silently ignoring provider scope (`7978028`). Native
  Python exposes validated, bounded analysis records and shares provider/session/path
  conversion with message search (`ac7e82e`, `c90ca98`).
- Native Python session, message, and analysis requests compose one immutable
  `DateRangeQuery` and resolve it through the same Rust EDTF, ISO, duration, and
  natural-language parser used by CLI and MCP (`d2819c5`). Architecture-matched
  runtime tests cover indexed month filtering, empty ranges, exclusivity, and
  malformed input; the larger cross-language property corpus remains pending.
- Native message, analysis, and file requests share one immutable `QueryScope` for
  provider, exact/fuzzy session, normalized path, and date predicates (`2511a6b`).
  File queries now resolve abbreviated exact session IDs and date bounds through the
  live Rust catalog instead of bypassing validation with duplicate raw fields.
- Native Python exposes the indexed `MessageService` context window with abbreviated
  session resolution, asymmetric bounds, GIL release, and the existing typed message
  record (`f659573`). Immutable `MessageSelector`, `MessageSearchTarget`, and
  `MessageSequenceRange` requests expose the canonical Rust roles, semantic kinds,
  content/tool fields, RFC 6901 argument paths, tool filters, sequence ranges,
  compaction exclusion, and exact/regex/fuzzy modes (`178e073`). `MessageFilters`
  owns cross-surface validation while retaining the Rust API's useful cross-session
  sequence queries.
- The legacy message-filter audit found no capability worth preserving as a parallel
  scanner: its three-value `MessageType` converts every unknown record to `SYSTEM`,
  `by_content` is an in-memory substring already superseded by indexed exact search,
  and `long_messages_only` is an unused hardcoded `len(content) > 500` presentation
  heuristic. Do not port that magic threshold. If measured workflows later require
  length selection, add explicit parameterized character/byte bounds to Rust first.
- Rust recovery now exposes one collision-safe restore primitive that atomically
  claims destinations, syncs content, and removes partial files with an RAII guard;
  CLI extraction and `NativeReconstructedFile.restore` reuse it (`b9f8692`,
  `6cdd1ee`). Four concurrent restores produce distinct files without overwriting.
- CLI summary, CLI message evidence, MCP evidence, and Python inspection now share
  `CatalogService::inspect` (`2748ffe`, `bb03d40`). The typed bounded result combines
  provider-general user intent, tool activity, normalized references, changed files,
  optional indexed time profile, and actionable expansion commands; do not port the
  narrower scan-based legacy `SessionAnalysis` as a parallel model.
- The legacy timeline adds no missing data model: it drops tool rows and attaches a
  Claude-only `tool_count` to assistant rows, while Rust exposes each normalized tool
  call/result with kind, name, call ID, sequence, timestamp, and searchable canonical
  arguments. Python composes `QueryScope` and `MessageSelector` for the same timeline;
  a runtime fixture proves nested tool-argument and tool-name search. Do not add a
  second timeline result or preview policy. The `pbcopy` heredoc parser remains
  postponed as platform/shell interpretation; general RFC 6901 argument search can
  locate such commands without hardcoding clipboard tools in core.
- Scoped native-facade mypy, Ruff, PyO3 Clippy, and seven architecture-matched runtime
  tests pass. Whole-package mypy still reports 93 errors across nine legacy Python
  files; legacy scanner/CLI deletion and its replacement type gate remain incomplete.
- Online SQLite backup, integrity/count/checksum receipts, crash-window recovery, and
  legacy config import are implemented (`523e9f6`, `a97269b`). Local installation
  cutover and rollback acceptance are still pending.
- MCP refresh workers own cancellation and join on shutdown (`9814ace`). The temporary
  `aise-mcp` executable deliberately remains until the final consolidation gate.
- Native wheel/sdist content checks, locked dependency graphs, portable CycloneDX
  SBOMs, and compatible dependency-license policy are implemented (`12f17fc`,
  `eb73629`, `163b45e`). Cross-platform hosted artifact execution and signing remain.

## Database cutover state machine

1. Discover effective paths through configuration APIs and inventory every writer.
2. Stop old writers and acquire the exclusive update lock.
3. Record schema, SQLite runtime, journal, integrity, row/FTS/parser, and freshness
   manifests.
4. Create a consistent destination-filesystem snapshot with SQLite backup API or
   `VACUUM INTO`; never copy only a live `index.db`.
5. Sync, reopen, integrity-check, checksum, and preserve the original for rollback.
6. Apply idempotent transactional schema steps to the snapshot.
7. Differentially validate counts, sampled queries, FTS, file reconstruction, and
   CLI/MCP/Python results.
8. Atomically publish the database and atomically replace one config/MCP owner.
9. Start, canary, observe, and roll back both config and database on any failure.

## Strictly-better gate

Every phase must preserve or improve correctness, recoverability, API clarity,
latency, memory, portability, and user workflow together. A build is not proof.
Measured regression blocks the phase unless explicitly accepted. Each commit must
be independently reviewable and leave the branch testable or clearly marked as a
history-only/build-wiring boundary.
