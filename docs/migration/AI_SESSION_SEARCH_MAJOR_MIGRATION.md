# AI Session Search major migration

Related contracts: [capability parity](CAPABILITY_PARITY.md) and
[Rust/Python API architecture](RUST_PYTHON_API_ARCHITECTURE.md).

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
- [x] Keep adapters thin, generate CLI/MCP/Python contract tests, and remove the
  temporary MCP executable at the major boundary.
- [x] Eliminate the non-cancellable background refresh worker; keep initialize
  index-independent and refresh synchronously before each `tools/call` read.
- [x] Failure-inject EOF and SIGTERM against the Python-distributed `aise mcp serve`
  subprocess and prove bounded termination independently of destructor execution.
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
- [x] As the final CLI/MCP architectural step, move stdio serving to `aise mcp serve`,
  update every installer/config contract, rerun startup/shutdown/parity/install gates,
  and remove the temporary second executable after those gates pass.
- [x] After executable consolidation and its complete regression gate, rework the
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
- Rust analysis results now support deterministic, immutable v1 artifact bundles through
  `AnalysisPublicationPlan`, with versioned JSON/Markdown filenames, a checksum manifest,
  same-parent staging, file and directory sync, one atomic directory rename, no-overwrite
  semantics, and RAII cleanup (`0a5ca05`). The public PyO3 facade uses the same plan and
  retains one canonical Rust result while caching graph derivation. The bundle dashboard now
  ranks every session by validated policy score with canonical-ID tie breaking and no utility
  threshold or fabricated fallback path (`46a66fe`). Symlink taxonomy and mutable incremental
  analysis state remain only until differential tests prove outcomes not supplied by immutable
  bundles; they are not accepted merely for implementation parity.
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
- Database snapshots, receipts, and immutable analysis directories now share a
  symlink-aware durability layer and atomic no-replace rename (`36b71af`). The macOS
  implementation uses `renamex_np(RENAME_EXCL)`, Linux uses
  `renameat2(RENAME_NOREPLACE)`, and Windows uses `MoveFileW` without replacement.
  Tests prove raced file and empty-directory destinations, broken symlinks, held locks,
  corruption, and config-link replacement fail without overwriting the winner. Both
  Apple architectures compile and the macOS runtime suite passes; Linux/Windows runtime
  execution remains a hosted release gate.
- The earlier refresh-worker containment (`9814ace`) was superseded because provider
  scans could not observe cancellation while RAII teardown joined the worker. The final
  transport has no background thread: initialize is index-independent, `tools/call`
  refreshes before reading, and Cargo/PyO3 EOF and SIGTERM behavior is regression-tested.
- The Python-distributed console now invokes the same process-safe Rust Clap dispatcher
  as the Cargo binary (`fb14e17`). Clap help and usage failures return their native exit
  codes without terminating the embedding interpreter; runtime failures use the same
  single-line `error:` contract without Python tracebacks. Only `mcp serve` remains a
  PyO3 stdio shim so Python and Rust never compete for buffered stdin ownership.
- The sanitized demo now writes an explicit temporary database path, enables only its
  fixture Claude root, and disables all seven other providers (`fb14e17`). This closed a
  regression where a temporary cache still selected the real platform database and a
  nominal fixture search consumed more than four minutes of CPU. All 20 demo tests now
  exercise canonical Rust commands, exact session scopes, and typed output contracts in
  under ten seconds on the local validation host.
- Native wheel/sdist content checks, locked dependency graphs, portable CycloneDX
  SBOMs, and compatible dependency-license policy are implemented (`12f17fc`,
  `eb73629`, `163b45e`). Cross-platform hosted artifact execution and signing remain.
- Matching native x86_64/ARM64 Linux and macOS plus x86_64 Windows release jobs now
  install-test exact wheels, build and smoke-test the single `aise` CLI/MCP executable,
  create deterministic no-overwrite native archives, keep native files out of the PyPI
  upload set, and checksum/attest distributions, SBOMs, and license inventories
  (`0e85b13`). The local ARM64 macOS release executable and archive pass end-to-end;
  the five-runner workflow, PyPI PEP 740 attestations, and GitHub provenance/release
  publication remain unexecuted until a future explicitly authorized tag operation.
- Cargo packaging and install contracts now independently verify the publishable crate,
  exact generated package directory, source path, and committed Git revision, then run
  the same isolated CLI/MCP smoke contract against each installed executable (`0693026`).
  Local package and Git installs pass; crates.io publication remains intentionally
  unexecuted and requires a future explicit authorization and registry identity setup.
- Native archives now contain platform installers that derive destinations from standard
  environment/configuration, refuse overwrite and symbolic links by default, and require
  an explicit absent rollback path for replacement (`6bd01a8`). Archive verification
  requires the installer, and every release runner extracts, installs, and smoke-tests the
  exact archived executable. The ARM64 macOS path passes locally; Windows execution remains
  part of the unexecuted hosted matrix.
- The legacy sessiongrep database was migrated with SQLite online backup, verified
  receipt/checksum/counts, preserved rollback files, and an atomic local rename. A full
  reindex then exposed 213 parser-stale rows that the old diagnosis incorrectly treated
  as repairable.
- Rust index lifecycle now separates discoverable repairable stale sessions from
  unavailable retained archives, canonicalizes provider-root aliases, and transactionally
  replaces superseded session IDs for the same physical source (`ea4c9f8`). The measured
  local postcondition is 49 current Claude Desktop sessions for 49 discovered files,
  212 unavailable Claude archives, zero duplicate `(provider, source_path)` groups, and
  no ineffective repair command. CLI, MCP, PyO3, and the public Rust consumer share
  `IndexService::status`.
- Final post-dispatch lifecycle validation passed 368 Rust library tests, 52 integration
  tests, Rust and downstream API doc tests, warning-free rustdoc, workspace all-feature
  Clippy, rustfmt, and the downstream Rust API consumer. The uninterrupted selected
  Python run passed 1,369 tests with 44 integration-marked tests deselected; its only two
  warnings are assertions of the legacy multi-source warning path. Focused CLI/MCP and
  demo tests pass 28/28.
- Fresh ARM64 and x86_64 wheels plus the sdist built from `fb14e17` pass archive
  verification. The exact x86_64 wheel and exact sdist install into separate isolated uv
  environments, resolve outside the checkout, expose the Rust-backed canonical entry
  point, advertise all MCP lifecycle commands, and complete initialize/EOF. ARM64
  execution remains a hosted/native-runner gate.
- Repository-wide legacy Python quality remains a removal gate: historical whole-package
  mypy reports 93 errors across the transitional scanner/CLI surface; scoped native,
  release, and entrypoint Ruff/mypy checks pass.

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
