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
- [x] Extract Rust library services for catalog, messages, tools, files, export,
  sources, maintenance, and optional analysis; adapters own no policy.
- [x] Port AI Studio and Gemini providers plus every useful aise recovery, export,
  analysis, graph, taxonomy, configuration, and public Python capability.
- [x] Add a mixed Rust/Python maturin/PyO3 package with bounded typed conversions,
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
  mechanism that never raw-copies a live WAL database. The local cutover and rollback
  preservation passed; exhaustive external-writer/crash failure injection remains open.
- [x] Finalize the major-version identity: repository/distribution
  `ai-session-search`, executable `aise`, Python import `ai_session_search`, and one
  platform-derived config/index identity.
- [x] Implement clean-install gates for uv add/pip/tool/uvx, pip, Cargo
  registry/Git/path, sdist fallback, platform wheels, native archives, and installers.
- [x] Generate Apache-2.0 metadata, provenance, relevant NOTICE content, third-party
  license inventory, SBOM, checksums, and artifact-content tests.
- [ ] Sign and attest release artifacts during an explicitly authorized public release;
  local work must not manufacture or publish release identity.
- [x] Build immutable local release candidates once, install-test the exact ARM64 macOS
  artifacts, migrate the local installation with rollback ready, and retire duplicate
  local runtime paths only after acceptance.
- [ ] Execute the immutable artifact matrix on hosted Linux, macOS x86_64, and Windows
  runners during an explicitly authorized release operation.
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
  refresh and analysis operations (`8fe31dc`, `aee1f96`, `5e94162`, `440ba5e`,
  `6154ff3`, `3b48c68`). The major boundary removed the legacy Python scanner and
  orchestration graph after native API, package-content, and installed-artifact gates passed.
- Rust analysis results now support deterministic, immutable v1 artifact bundles through
  `AnalysisPublicationPlan`, with versioned JSON/Markdown filenames, a checksum manifest,
  same-parent staging, file and directory sync, one atomic directory rename, no-overwrite
  semantics, and RAII cleanup (`0a5ca05`). The public PyO3 facade uses the same plan and
  retains one canonical Rust result while caching graph derivation. The bundle dashboard now
  ranks every session by validated policy score with canonical-ID tie breaking and no utility
  threshold or fabricated fallback path (`46a66fe`). Symlink taxonomy and mutable incremental
  analysis state were then audited independently: the state helper was loaded but never read or
  persisted, while stage skipping trusted filename existence and could ignore changed inputs.
  That false freshness API and its self-contained tests were deleted (`b8d131c`). The later major
  Python boundary also removed symlink taxonomy because immutable bundles supply the supported
  browsing outcome without mutable filesystem policy.
- Provider-neutral `AnalysisPolicySpec` and `PhraseVocabularyPolicySpec` now provide one
  serializable validation boundary for Rust callers, the native `aise analyze` command, and
  typed PyO3 constructors (`325cb84`, `3b48c68`). CLI analysis reuses the canonical session
  filter model, treats omitted/zero limit as the full selected corpus, and uses private automatic
  keyset batches that differential tests prove cannot alter serialized results (`2067f8a`). No
  batch/page tuning appears in Rust, CLI, or Python public APIs. CLI preflights a non-replacing
  destination before scanning and prints the publication receipt. Read-only MCP analysis now
  accepts the same serialized policy, defaults to a configurable bounded canonical-ID-ordered
  corpus, preserves explicit `limit=0`, reports when a corpus may be partial, and warns that
  separate bounded graphs/vocabularies are not mergeable. It never accepts a publication path.
  The pre-existing seven MCP operation descriptions and schema fields remain available; the
  only additive wording change documents explicit `limit=0` behavior. The installed cutover
  advertises and executes the eighth `analyze_sessions` tool.
  Pre-mortem call-chain review found that keyset batching does not bound a single session's
  concatenated user text: `analysis_document_page` reads all user messages before policy
  evaluation. Do not truncate analysis input to control memory. Replace concatenation with
  lossless streaming phrase state and metadata-only reads where text is unnecessary; an explicit
  classification window remains analysis semantics, not a hidden resource limit. Preserve exact
  unbounded-policy results through an RAII spill/mapped representation or retain the current path
  until differential fixtures prove a lossless replacement. Expose analysis through MCP only after
  computation and response bounds cannot silently discard requested evidence.
- Destination-independent `ExportService` and canonical `SourceService` keep rendering
  and provider discovery out of CLI, MCP, and Python adapters (`f701522`, `e8d24b5`).
- Filtered multi-session export now composes `CatalogService` with `ExportService` and a public
  Rust `ExportPublicationPlan`: omitted limits use the configured bounded session page, explicit
  `limit=0` selects all, each canonical ID maps to a deterministic portable hashed filename, and
  one RAII-staged no-replace directory publish prevents partial bundles. The direct
  `aise export ID` path remains the simpler single-session interface. Typed PyO3 bundle export
  calls the same service and returns the same receipt while releasing the GIL; it serializes on
  the binding's one SQLite connection rather than collecting every full transcript in memory.
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
- The top-level package, runtime native facade, and native type stub now place
  `SessionSearch` first in `__all__`, matching the documented canonical Rust-backed lifecycle
  entry point instead of letting later analysis exports reorder API discovery (`85152e6`).
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
  A typed fused iterator reconstructs all causally valid versions in one forward pass,
  preserves version gaps after path-only edits, owns its edit rows without retaining a
  database lock, and is shared by Rust and PyO3 (`7476c6a`). `files extract --all`
  streams lossless framed or JSONL output, or explicitly publishes a new complete
  directory through the same-parent no-replace RAII transaction (`61ead3a`). Empty,
  duplicate, raced, and pre-existing destinations leave no partial result. Parent-sync
  failure after rename leaves the complete visible destination intact and returns
  actionable durability uncertainty (`0dccea2`). Native Python releases the application
  mutex before publication; its runtime API and stubs now match, and the removed internal
  analysis batching parameter is rejected by mypy (`a67d033`).
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
- Before the major boundary, scoped native-facade mypy, Ruff, PyO3 Clippy, and seven
  architecture-matched runtime tests passed while the duplicate scanner graph still
  reported 93 mypy errors. The native-only boundary removed that graph; current mypy
  checks every retained Python source file successfully.
- Online SQLite backup, integrity/count/checksum receipts, crash-window recovery, and
  legacy config import are implemented (`523e9f6`, `a97269b`). Local installation
  cutover and rollback preservation passed; exhaustive external-writer and process-crash
  injection remains an explicit pre-release gate.
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
- Python artifact acceptance now runs one self-cleaning cross-platform harness through
  `pip install`, `uv add`, `uv tool install`, and `uvx`. Every mechanism uses the exact
  artifact-compatible interpreter, isolated config/cache/tool roots, and the shared
  installed-distribution or native-executable contract. The local ARM64 wheel passes all
  four paths, including execution from a Rosetta host process; hosted Linux, macOS x86_64,
  and Windows execution remains CI-owned. Rust date parsing also removed the obsolete
  `edtf`, `python-dateutil`, `pyparsing`, and `six` dependency chain.
- Native archives now contain platform installers that derive destinations from standard
  environment/configuration, refuse overwrite by default, and require an explicit absent
  rollback path for replacement (`6bd01a8`). A real uv-tool cutover exposed that unconditional
  symlink rejection made the documented replacement path unusable. Unix links and Windows
  reparse points can now be replaced only under the same explicit rollback contract; the link
  itself is preserved, its target is never mutated, and failure or Unix signals restore it
  before exit (`8f5a0f3`). Archive verification
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
- Current workspace validation passes `cargo test --workspace --all-features`, the Rust
  core and downstream API consumer doctests, warning-free rustdoc, workspace all-target
  all-feature Clippy, rustfmt, and the downstream Rust API consumer. The blanket workspace
  doctest command is intentionally not a valid macOS PyO3 extension gate because extension
  modules leave Python runtime symbols for the loader; exact wheel runtime tests cover that
  boundary instead. Repository Ruff, mypy, and all 77 retained Python tests pass.
- The locked ARM64 wheel and exact sdist pass archive verification and installation outside
  the checkout. One self-cleaning harness passes `pip install`, `uv add`, `uv tool install`,
  and `uvx` against the exact wheel while pinning an architecture-compatible interpreter.
  Other native runners remain hosted gates rather than unverified local claims.
- Local cutover now installs the ARM64 macOS `aise 1.0.0` executable at
  `~/.local/bin/aise`, with the former ai-session-tools `0.3.1` uv symlink preserved as a
  versioned rollback artifact. The installed executable passed CLI/MCP distribution smoke tests,
  then completed initialize, `tools/list`, and structured `get_index_status` against schema 2:
  1,348 current sessions, 212 unavailable retained archives, zero repairable stale sessions,
  and zero parse warnings. Ten detected client configs use portable `aise mcp serve` commands
  in their native JSON/TOML shapes, contain zero sessiongrep MCP entries, and have pre-retirement
  snapshots. Existing sessiongrep instruction references remain until documentation cutover;
  sleeping servers owned by live clients drain on restart rather than being killed cross-session.
  The installed binary was then refreshed through explicit rollback-preserving replacements after an
  installed-help canary caught and fixed `analyze --limit` incorrectly describing search-default
  semantics. The final installed executable SHA-256 is
  `ce194675873291301c6c8fbef45b1584eaa576f55e9b909823ccaa23c8a96dcd`; its immediate
  predecessor remains at `~/.local/bin/aise.rollback.20260713T174955Z`. The native executable
  verifier and an isolated live initialize/`tools/list`/`analyze_sessions` canary pass with
  eight advertised tools, a zero-session result, and `session_id_asc` selection. The canary
  explicitly sets both `[index].db_path` and `AI_SESSION_SEARCH_CACHE_DIR`: cache overrides do
  not relocate durable data, and an earlier fixture that omitted the database override correctly
  reached the real configured index rather than proving a directory-creation defect. Canonical
  TOML provider keys are hyphenated (`gemini-cli`, `ai-studio`); using `gemini_cli` in the first
  fixture left Gemini enabled and explained its unintended scan latency.
- The major Python boundary is now Rust-only: package import exposes the typed PyO3
  application/query facade, the console entry point dispatches the Rust CLI plus the one
  Python-owned MCP stdio shim, and the legacy scanner, Typer CLI, JSON configuration,
  formatters, source adapters, symlink taxonomy, and Python analysis pipeline are absent
  from both source and wheel. Typer, Click, Rich, and orjson are no longer runtime
  dependencies. Repository Ruff, mypy over every retained Python source file, 77 retained
  Python tests, the locked ARM64 wheel, and all four Python install mechanisms pass.
  Immutable demo recovery files are excluded from style mutation because their bytes are
  parser fixtures, not project source.

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

An interruption after the prepared receipt is synced is recovered with
`aise migrate recover --receipt <path>`. Recovery acquires the same source and destination
writer locks, verifies the checksum, integrity, and row manifest, and either publishes the
preserved same-directory staging database or finalizes an already-published destination. It is
idempotent after final receipt publication and never deletes conflicting or incomplete evidence.

## Strictly-better gate

Every phase must preserve or improve correctness, recoverability, API clarity,
latency, memory, portability, and user workflow together. A build is not proof.
Measured regression blocks the phase unless explicitly accepted. Each commit must
be independently reviewable and leave the branch testable or clearly marked as a
history-only/build-wiring boundary.
