# AI Session Search major migration

Related contracts: [capability parity](capability-parity.md) and
[Rust/Python API architecture](rust-python-api-architecture.md).

Historical execution records below preserve the command names and the advertised
surface counts of their referenced commits, so an entry naming `aise mcp
serve|recover` or seven MCP tools records what that commit shipped rather than what
this one does. The current pre-release contract is `aise package
status|check|update`, `aise integrations install|status|uninstall|recover`,
`aise config file|example|init|show|origins|paths`, and `aise mcp serve`. For the
current advertised tool set, read the emitted catalogue with
`aise mcp schema-budget --catalogue`; no count is restated here that could drift
from it.

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

- [x] Baseline legacy aise, imported sessiongrep, CLI, MCP, Python, index lifecycle,
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
- [x] Failure-inject lock permissions/types/contention, schema backfill, SQLite
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
- [ ] **Postponed until an explicitly authorized public release:** Sign and attest release artifacts;
  local work must not manufacture or publish release identity.
- [x] Build immutable local release candidates once, install-test the exact ARM64 macOS
  artifacts, migrate the local installation with rollback ready, and retire duplicate
  local runtime paths only after acceptance.
- [ ] **Postponed while public/GitHub-facing actions are prohibited:** Execute the immutable artifact matrix on hosted Linux, macOS x86_64, and Windows
  runners during an explicitly authorized release operation.
- [x] As the final CLI/MCP architectural step, move stdio serving to `aise mcp serve`,
  update every installer/config contract, rerun startup/shutdown/parity/install gates,
  and remove the temporary second executable after those gates pass.
- [x] After executable consolidation and its complete regression gate, rework the
  sanitized fixture-driven demo workflow for the final capabilities. Treat its script
  as an end-to-end test; publish generated GIF/video externally and never commit media.

## Post-migration audit ledger

- [x] Preserve explicit `limit = 0` as unlimited across CLI, Rust, MCP, Python, FTS
  candidate selection, grouping, and pagination; omission retains each surface's
  documented bounded default (`f8fb1ce`).
- [x] Add canonical provider display names and native-resume capability metadata;
  generate MCP descriptions/enums from the registry and name all eight session sources
  in root CLI help (`f8fb1ce`).
- [x] Add one immutable Python `QueryExclusions` value and apply it before limits across
  session, message, analysis, and file queries (`f8fb1ce`).
- [x] Reuse one RAII same-parent staged-file writer for migration and config initialization;
  reject symlinks/nonregular files and clean unpublished stages on drop (`f8fb1ce`).
- [x] Make MCP installer reads strict, preflight every transformation before writes, use
  durable publication, and report partial multi-file outcomes without claiming cross-directory
  atomicity (`fd57956`).
- [x] Anchor TOML-relative paths to the selected config file's parent; keep CLI/environment
  relative paths cwd-relative and fail explicit missing config files (`fd57956`).
- [x] Finish the recoverable MCP multi-file transaction checkpoint: exact preimages, portable
  absolute receipt paths, one advisory lock, reverse rollback, published-state finalization,
  concurrent-edit preservation, explicit recovery, authoritative status, and crash-window tests.
- [x] Replace process-global Rayon configuration with an application-owned execution runtime;
  each database lifecycle owns a fixed-size pool and every parallel region enters it explicitly.
- [x] Remove the MCP `get_session` aliases `view`, `seq`, and `max_lines`; retain only `summary`,
  `message_seq`, and `transcript_lines`, with the configured default on the canonical schema field
  (`0edc5f9`).
- [x] Route CLI, TUI, MCP, and inspection command rendering through one POSIX-shell renderer with
  structured argument vectors and NUL/C0/DEL rejection (`8ca0dd7`).
- [x] Rename the CLI whole-session selector to `aise show --transcript-lines` with
  `[cli].show_transcript_lines`, matching MCP `transcript_lines`, and add tail/head/entire-transcript
  usage guidance across help, schema, and config docs (`8e7a3f9`).
- [x] Add the scope-explicit per-message cap `lines_per_message` (positive=head, negative=tail,
  0=full content, default 0 everywhere) to CLI `messages search/get/timeline`, MCP `search_messages`
  and `get_session` focused `message_seq` output, Python `search_messages`/`message_context`, and
  `[mcp]`/`[cli]` config defaults; refs always come from full content (`7554741`).
- [x] Classify string/path conversions with evidence: remaining `to_string_lossy` sites in
  `service.rs`/`indexer.rs`/`source.rs` are test fixtures; `text_file_transaction.rs` preserves
  exact platform path bytes; `util::normalize_path` and `db.rs` provider-root canonicalization sit
  on the deliberate UTF-8 TEXT index boundary; config path fields are TOML-facing; the Python FFI
  uses native `PathBuf`/`OsString`. No allocation-oriented string change is justified until a
  benchmark shows pressure, and no new string/path crate is warranted.
- [x] Complete the remaining public-surface audit (`bd89663`): `SessionSearch::database` narrowed
  from `pub` to `pub(crate)` (no external caller existed — verified across
  `rust/ai-session-search-python` and integration tests); cancellation/lifetime audit found no
  defects (read-only SQL interrupts via a `progress_handler` deadline reset after each query,
  index writes are per-session immediate transactions, interrupted migration and MCP-install
  transactions have explicit recovery commands, services are `Copy` borrows bound to the
  `SessionSearch` RAII lifetime); error vocabulary standardized
  (`unrecognised`→`unrecognized`, `session id`→`session ID`).
- [x] Replace the unbounded per-session concatenated user text with lossless per-message
  streaming (`177c98f`): `Db::visit_analysis_sessions` streams user-message rows;
  `AnalysisAccumulator::push_session_text_stream` + `StreamingPhraseAggregator` +
  shared `ProseLineFilter` reproduce the joined-text semantics exactly (junction n-grams,
  fence state across chunks, joiner-only-when-nonempty, `min_document_tokens`-gated
  `max_unique_phrases` errors). Differential fixtures prove byte-identical results/errors
  (`analysis_pipeline::tests::streaming_*`, service test
  `analysis_run_matches_paged_documents_reference_on_indexed_sessions`). The public paged
  `documents()` API intentionally still returns full joined text.
- [x] Refresh the installed `~/.local/bin/aise` (2026-07-14): `scripts/install-native.sh
  --replace --backup ~/.local/bin/aise.rollback.20260714005036` replaced
  `ce194675…` with `279b00a6…` (built from `54adf7e`). Installed-help canary shows
  `--transcript-lines`/`--lines-per-message`; isolated MCP canary (explicit `[index].db_path` +
  `AI_SESSION_SEARCH_CACHE_DIR`, all providers disabled) returned initialize + 8 tools at that
  historical checkpoint, before the MCP analysis adapter was removed, with
  `lines_per_message` on `search_messages` and `lines_per_message`+`transcript_lines` on
  `get_session`. Running `aise mcp serve` clients keep the old inode until their next restart.
- [x] Run the locally executable portability gates (`54adf7e` for the MSRV correction):
  exact-toolchain `cargo +1.88 check --workspace --locked` passes; the previously declared
  MSRV 1.85 was FALSE against the committed lockfile (darling 0.23.0 / instability 0.3.12
  require rustc 1.88; icu 2.2.0 requires 1.86) and was raised to 1.88 in
  `Cargo.toml`/CI/README/RELEASING/INSTALLATION. One `cp312-abi3` arm64 wheel installed and
  smoke-tested on CPython 3.12.9/3.13.13/3.14.5; the dev venv separately exercises
  x86_64-darwin under Rosetta. Empirical CLI canary: relative env paths containing spaces
  under a symlinked spaced directory for config/db/cache (reindex + list). Residual items are
  hosted-runner-only (non-UTF-8 paths are not constructible on APFS; Linux/Windows/x86_64
  matrices) and stay release-gated below.
- [x] Audit platform-sensitive paths after the skill-lifecycle cutover: injected `ClientLayout`
  tests cover macOS/Linux/Windows config roots; durable publication has native Apple
  `renamex_np`, Linux `renameat2`, and Windows `MoveFileW` branches; executable discovery handles
  Unix execute bits and Windows `PATHEXT`; package matrices cover Linux/macOS x86_64/ARM64 and
  Windows x86_64. No absolute developer path is embedded in generated MCP configuration.
- [x] Add a non-duplicative `rust-portability` pull-request matrix on native macOS and Windows.
  It runs `cargo test -p ai-session-search --all-targets --locked`; the existing Linux `rust` job
  remains the sole full-workspace/static-analysis job, and the existing Python and release matrices
  retain ABI, wheel, archive, and installer responsibility.
- [ ] Capture the first hosted `rust-portability` run before declaring Windows/macOS behavior
  proven. A workflow definition is policy, not execution evidence; record job URLs and exact
  failing test names if either runner exposes a platform defect.
- [ ] Add a Linux-only non-UTF-8 path fixture for config, database, cache, transaction receipt,
  export, and skill destinations. Assert byte-preserving diagnostics and cleanup; do not force
  these paths through TOML/SQLite UTF-8 text fields whose boundary is already documented.
- [ ] Add Windows hosted cases for Developer Mode unavailable, a running executable that cannot be
  replaced, mixed-case `PATHEXT`, drive-root/UNC paths, read-only destinations, CRLF instruction
  files, and interrupted PowerShell installer rollback. Preserve current APIs unless a failing
  native test proves an interface change is necessary.
- [x] Validate the portability batch locally: repository contracts passed `15/15`, `actionlint`
  accepted all three workflows, and `./run_ci_local.sh` passed `16/16` (152 Python tests; 514
  Rust unit tests plus integration suites; release executable/MCP schema; wheel and sdist install
  pathways). This is local macOS arm64 evidence, not evidence that hosted macOS or Windows jobs ran.
- [ ] Add hosted macOS x86_64 cases for spaces and Unicode in config/cache/database/skill roots,
  alias creation and removal, GUI-inherited PATH failure guidance, and transaction cleanup after
  termination. Reuse the existing typed path, transaction, and installer helpers rather than
  adding platform-specific parallel implementations.
- [x] Cache/incremental gate defaults verified: `run_ci_local.sh` sets `CARGO_TARGET_DIR` to the
  one workspace target and `CARGO_INCREMENTAL=0` only when unset, applies
  `AI_SESSION_SEARCH_RUSTC_WRAPPER` only when the caller provides it, never overrides
  `CARGO_HOME`/uv caches, and never creates or deletes a machine-wide cache (state lives under
  a `mktemp` root).
- [x] Cleanup verified on success/failure/signal (`54adf7e` adds the ENOSPC guidance): a
  successful gate removes the state root and lock and restores checksummed native modules; a
  SIGTERM mid-`cargo check` canary left no `ai-session-search-local-ci.*` state, no
  `*.local-ci-conflict.*` files, and intact native modules (SIGINT/HUP share the same
  trap→EXIT→`cleanup_local_ci` path). The failure summary now names only project-owned
  reclaimable paths for `ENOSPC` and states shared caches are never deleted.
- [x] Reconcile docs against executable contract tests (`54adf7e`): README shows both window
  flags with the shared sign convention; `configuration.md` gains "Output windowing defaults"
  naming all four keys; `capability-parity.md` marks the analysis-streaming requirement
  IMPLEMENTED with fixture pointers;
  `test_public_docs_match_native_abi_mcp_and_quality_gates` pins the new strings. No media
  published.

### Outstanding (fine-grained)

- [x] **Investigate the transient `FOREIGN KEY constraint failed` (SQLite error 787) observed
  once during live-index auto-refresh (2026-07-14 ~00:50).** Evidence: first
  `~/.local/bin/aise search` after the binary refresh failed while opportunistically indexing
  ~6h of new sessions; an interleaved run of the previous binary refreshed cleanly;
  `pragma foreign_key_check` returns zero rows; an explicit `aise reindex` then upserted
  6 sessions cleanly; `git diff 0edc5f9..HEAD` touches no write path; writers are serialized
  by `with_index_update_lock` (indexer.rs:73); all four concurrent `aise mcp serve` processes
  postdate the lock protocol. FK edges: `transcripts`/`messages`/`file_edits.session_id →
  sessions(id) ON DELETE CASCADE`. Landed diagnosability fix — reindex upsert errors now name
  the session and source file. Before:

  ```rust
  db.upsert_session_reconciling_sources(&parsed, source.mtime_ns, source.size_bytes, aliases, !full)?;
  ```

  After:

  ```rust
  db.upsert_session_reconciling_sources(&parsed, source.mtime_ns, source.size_bytes, aliases, !full)
      .with_context(|| format!(
          "failed to index session '{}' from {}",
          parsed.session.id, source.path.display()
      ))?;
  ```

  Investigation result: the proposed two-process race cannot occur through the supported writer
  lifecycle. `with_index_update_lock` holds one process-shared exclusive lock across the entire
  refresh; `upsert_session_with_mode` then uses one SQLite transaction, inserts/updates
  `sessions(id)` before `transcripts`, `messages`, or `file_edits`, and all child helpers bind the
  same `SessionRecord.id`. The live database passed `pragma foreign_key_check`, an explicit
  reindex succeeded, and the failure did not reproduce. No statement-order change is justified
  without a failing supported-path test. The landed context identifies the exact session and
  source if it recurs; capture that full error chain, the binary commit, `pragma foreign_key_check`,
  and the source file before retrying so a deterministic fixture can target the actual row.
- [x] **Document asymptotic costs of public operations:** service rustdoc now names caller-visible
  work and memory bounds for analysis (streamed bytes × applicable rules), keyset document pages,
  export bundles, session/message search (FTS candidates plus fuzzy/regex work), bounded message
  context, file aggregation/history, and replay reconstruction. Zero limits are explicitly
  documented as complete-corpus requests rather than hidden safety caps. The final local gate
  passed all 15 stages with 128 Python tests and 521 Rust tests (one intentional crash-helper
  test and one benchmark ignored).
- [x] **Complete the streamlined CLI/MCP/Python redesign and local cutover** (`30826e6`,
  `8bda9ff`, 2026-07-14): lifecycle operations are top-level `aise install|status|uninstall`;
  `aise mcp` contains only `serve|recover`; MCP advertises exactly seven retrieval, inspection,
  status, and read-only SQL tools; message search uses one `query` plus `match_mode`; analysis
  remains on the CLI, Rust, and Python surfaces. Public Python requests are flat where fields
  compose independently, and all session-ID methods accept a canonical ID or unique prefix while
  rejecting ambiguity. The final local gate passed 15/15 with 138 Python tests, 488 Rust unit
  tests, 56 Rust integration tests, Clippy with warnings denied, strict mypy/stub parity, Rust API
  doctests, verified ABI3 wheel/sdist artifacts, pip/uv/uvx/Git install canaries, and workflow
  syntax. The installed user-local executable and validated release binary both hash to
  `d7e7799f7b5c82dc4b4aeff01e90132f90da04911220034e123b255f98ef9704`; rollback is
  the timestamped user-local rollback executable. Installed help, a bounded
  `--index-refresh existing-only` search, MCP initialize, and exact seven-tool advertisement pass.
  `aise status` reports every detected MCP target and managed instruction target configured. No
  inspected client config, executable on PATH, or live process references legacy sessiongrep. The
  retained 10,253,185,024-byte legacy database was not opened, copied, migrated, or removed.
- [x] **Make background refresh failures actionable without exposing worker mechanics and unify
  explicit reindex finalization** (`0579bcb`, `e5afcc0`, `e219085`, 2026-07-14): a bounded atomic
  sidecar retains process details privately; Rust, Python, CLI, and MCP expose only live
  `in_progress` or actionable failure status. Live state derives from the existing update lock, not
  a PID or timeout heuristic. Installed dogfood found that CLI `reindex` on a new empty database
  failed to stamp the schema while Rust/Python already promoted required backfill to a full pass;
  all three now share one private RAII transaction with unchanged public parameters/results. The
  complete local gate passed 16/16, and the installed archive passed plain reindex, immediate
  `existing-only` read, MCP initialize/seven-tool/status/EOF canaries, and 16 configured integration
  targets. Installed SHA-256 is
  `826b778086dec09fe08990e56059dfc97b5e878a86570406628adbccf490f11c` with rollback
  `~/.local/bin/aise.rollback.20260714203637`. No push or publication occurred.
- [ ] **Release-gated (requires fresh explicit user authorization; no push, no publication):**
  signing/attestation, hosted Linux/macOS-x86_64/Windows runner matrix (includes the
  non-UTF-8-path portability case and hosted exact-MSRV job), crates.io/PyPI/GitHub
  publication, exhaustive external-writer/crash injection for DB migration, and any demo
  media publication (never commit media).

## Verified implementation checkpoints

- MCP client install/uninstall now preflights all selected clients and commits their independent
  JSON/TOML files through one versioned receipt. Exact before/after images, tagged Unix-byte or
  Windows-wide absolute paths, a shared advisory lock, reverse rollback, explicit recovery, and
  subprocess crash tests prevent a partial update from being reported as authoritative. The local
  gate also serializes source-extension replacement, records original checksums before quarantine,
  verifies restoration, and retains incomplete recovery evidence. The complete local gate passed
  all 15 stages: 127 Python tests, 440 Rust unit tests plus integration suites, strict Clippy,
  rustfmt/check, public API doctests, ABI3 wheel/sdist and install verification, and workflow syntax.
  Both pre-gate native modules were restored with their exact recorded checksums.
- `ExecutionRuntime` now owns a fixed-size local Rayon pool for each `Db`/`SessionSearch` lifecycle.
  Fuzzy ranking, correction classification, and trigram posting construction explicitly enter that
  pool; CLI, MCP, and PyO3 contain no global initializer. A focused Rust test opens simultaneous
  one-worker and two-worker applications in one process, and strict all-target Clippy passes.
- MCP `get_session` now exposes only `summary`, `transcript_lines`, and `message_seq` as mutually
  exclusive output selectors. The former `view`, `max_lines`, and `seq` aliases are absent from the
  closed schema and rejected before index access. TOML uses `get_session_transcript_lines`, and the
  canonical schema field advertises the same bounded default used at runtime.
- CLI, TUI, MCP, and inspection command rendering share one `render_posix_shell_command` built on
  `shlex::try_join`: argument vectors stay structured until presentation, NUL/C0/DEL are rejected
  with errors naming the argument index and code point, and output is labeled POSIX-shell syntax
  (`8ca0dd7`). The CLI whole-session selector is now `aise show --transcript-lines` with
  `[cli].show_transcript_lines`, matching MCP `transcript_lines`; help and schema text explain the
  tail/head/entire-transcript modes and point turn-level lookup at `search_messages` plus
  `message_seq` (`8e7a3f9`).
- Message-level output has a scope-explicit per-message cap: `lines_per_message`
  (positive=head, negative=tail, 0=full content, default 0 everywhere) on
  `aise messages search/get/timeline`, MCP `search_messages` and `get_session` focused
  `message_seq` output, and Python `search_messages`/`message_context`, with
  `[mcp]`/`[cli].lines_per_message` defaults. References are extracted from full content before
  capping, one shared `MessagePresentation` shapes both MCP tools, and `get_session` summary and
  transcript paths reject the argument with errors naming the correct knob (`7554741`). The
  15-stage local gate passed 15/15 with 449 Rust and 128 Python tests.
- History forensics on 2026-07-14 confirmed no line-window capability was removed:
  `8e7a3f9` renamed the whole-session CLI selector to `--transcript-lines`, and `7554741`
  added the distinct `lines_per_message` presentation window across CLI, MCP, and Python.
  Follow-up help/schema/doc contracts state why per-message windows are useful for large result
  pages and that they never change matches, ranking, result count, pagination, context membership,
  or reference extraction. The uncapped per-message default remains `0`; the old ambiguous
  `max_lines` alias remains intentionally absent.
- Analysis text is now streamed per message end to end (`bd89663`, `177c98f`, `54adf7e`,
  2026-07-14): raw `Db` access left the supported public surface, `AnalysisService::run` never
  materializes a session's joined user text, and memory is bounded by the policy's explicit
  bounds except for unbounded `user_text`/`any` classification rules (which retain exactly what
  the batch path retained). Byte-identical behavior is proven by differential fixtures at the
  accumulator level (junction-spanning phrases, fences opened/closed across chunks,
  junction-merged fence markers, CRLF, Unicode, empty chunks, bounded/unbounded classification,
  `min_document_tokens`-gated overflow errors, duplicate/poisoning parity) and at the service
  level against the paged `documents()` + `analyze()` reference over real SQLite. The declared
  MSRV was corrected from 1.85 to 1.88 after the first local exact-toolchain proof showed the
  lockfile requires it. The installed `~/.local/bin/aise` was replaced via the
  rollback-preserving installer (rollback: `aise.rollback.20260714005036`) and passed the
  installed-help and isolated MCP initialize/tools-list canaries. Each commit state was
  validated by the full 15-stage local gate (15/15; 458 Rust + 128 Python tests at `54adf7e`).

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
  destination before scanning and prints the publication receipt. Analysis remains available as
  `aise analyze`, Rust `AnalysisService`, and Python `SessionSearch.analyze`. The MCP
  adapter was removed after the surface audit found no observed calls and a poor fit for large,
  non-page-mergeable graph and vocabulary responses. MCP remains seven retrieval, inspection,
  status, and read-only SQL tools; stale `analyze_sessions` calls return an explicit unknown-tool
  error instead of selecting a different operation.
  Pre-mortem call-chain review found that keyset batching does not bound a single session's
  concatenated user text: `analysis_document_page` reads all user messages before policy
  evaluation. Do not truncate analysis input to control memory. Replace concatenation with
  lossless streaming phrase state and metadata-only reads where text is unnecessary; an explicit
  classification window remains analysis semantics, not a hidden resource limit. Preserve exact
  unbounded-policy results through an RAII spill/mapped representation or retain the current path
  until differential fixtures prove a lossless replacement. Do not expose analysis through MCP;
  reconsider that boundary only if measured MCP use cases justify it and computation plus response
  bounds can preserve every requested result without silent truncation.
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
  `DateRange` and resolve it through the same Rust EDTF, ISO, duration, and
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
  record (`f659573`). The initial nested selector objects (`178e073`) were consolidated into
  one immutable `MessageQuery` with canonical Rust roles, semantic kinds, content/tool fields,
  RFC 6901 argument paths, tool filters, sequence ranges, and compaction exclusion. Message
  matching has one
  pattern plus one typed mode on every programmatic surface: Rust `MessageFilters.match_mode`,
  Python `match_mode`, and MCP `query` + `match_mode`. Regex and fuzzy modes reject an empty query;
  exact mode keeps empty-query filter-only listing. The MCP schema rejects the removed `regex` and
  `fuzzy_query` fields rather than applying hidden precedence. `MessageFilters` owns cross-surface
  validation while retaining the Rust API's useful cross-session sequence queries.
- The legacy message-filter audit found no capability worth preserving as a parallel
  scanner: its three-value `MessageType` converts every unknown record to `SYSTEM`,
  `by_content` is an in-memory substring already superseded by indexed exact search,
  and `long_messages_only` is an unused hardcoded `len(content) > 500` presentation
  heuristic. Do not port that magic threshold. If measured workflows later require
  length selection, add explicit parameterized character/byte bounds to Rust first.
- Rust recovery now exposes one collision-safe restore primitive that atomically
  claims destinations, syncs content, and removes partial files with an RAII guard;
  CLI extraction and `ReconstructedFile.restore` reuse it (`b9f8692`,
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
  arguments. Python composes `QueryScope` and flat `MessageQuery` fields for the same timeline;
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
  nominal fixture search consumed more than four minutes of CPU. The demo suite now
  exercises canonical Rust commands, exact session scopes, and typed output contracts;
  use the commit-keyed CI result instead of copying a test count that changes with coverage.
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
  boundary instead. Repository Ruff, mypy over all maintained Python/test modules,
  compiled-runtime/stub parity, and all 123 retained Python tests pass. The Rust gate
  passes 428 library tests plus all integration suites; the separate ignored 40.0 MB
  tail benchmark passed with a measured 24.1x incremental/full speedup.
- The locked ARM64 wheel and exact sdist pass archive verification and installation outside
  the checkout. One self-cleaning harness passes `pip install`, `uv add`, `uv tool install`,
  and `uvx` against the exact wheel while pinning an architecture-compatible interpreter.
  Other native runners remain hosted gates rather than unverified local claims.
- Local cutover now installs the ARM64 macOS `aise 1.0.0-rc.1` executable at
  `~/.local/bin/aise`, with the former ai-session-tools `0.3.1` uv symlink preserved as a
  versioned rollback artifact. The installed executable passed CLI/MCP distribution smoke tests,
  then completed initialize, `tools/list`, and structured `get_index_status` against schema 2.
  The latest `existing-only` doctor read reports 1,581 indexed sessions: 1,369 current and 212
  unavailable retained archives, with zero repairable stale sessions and zero parse warnings.
  Twelve detected client config files use portable `aise mcp serve`
  commands in their native JSON/TOML shapes, contain zero sessiongrep MCP entries, and have
  pre-retirement snapshots. Four managed guidance targets cover Claude, Codex, Gemini/Antigravity,
  and OpenCode. Existing sessiongrep instruction references remain until documentation cutover;
  sleeping servers owned by live clients drain on restart rather than being killed cross-session.
  The installed binary was then refreshed through explicit rollback-preserving replacements after an
  installed-help canary caught and fixed `analyze --limit` incorrectly describing search-default
  semantics. The final installed executable SHA-256 is
  `0a3e4fddd42ea025dd1f55e114330c998861d032663ed46f365672837529a065`; its immediate
  predecessor remains at `~/.local/bin/aise.rollback.20260714T203628Z`. The native executable
  verifier and an isolated live initialize/`tools/list`/`analyze_sessions` canary passed at that
  earlier eight-tool checkpoint with a zero-session result and `session_id_asc` selection. The
  current source contract removes that MCP-only adapter while retaining the same analysis service
  through CLI, Rust, and Python. An installed RC canary completed initialize and `tools/list`,
  advertised exactly seven schema-bearing tools, called all seven successfully with bounded
  read-only inputs, and reported server version `1.0.0-rc.1`.
  An `existing-only` dogfood search for `sccache` returned five real message matches in 0.62 seconds
  without lazy-index progress; the service regression test proves this policy never builds the
  optional trigram base (`6643e27`). These are local macOS ARM64 canaries, not proof that every
  configured harness has reloaded the server or that unexecuted hosted targets pass. The canary
  explicitly sets both `[index].db_path` and `AI_SESSION_SEARCH_CACHE_DIR`: cache overrides do
  not relocate durable data, and an earlier fixture that omitted the database override correctly
  reached the real configured index rather than proving a directory-creation defect. Canonical
  TOML provider keys use their public provider IDs (`aistudio`, `gemini-cli`); using
  `gemini_cli` in the first fixture left Gemini enabled and explained its unintended scan latency.
- The all-tool canary exposed and the follow-up fixes a compact-summary aggregate-size defect.
  Before: one `get_session(summary=true)` call for a 346-message session produced 41,613 JSON bytes
  / 22,082 text bytes because four independent 12-item section limits could retain 38 top-level
  items and 20 nested references. After: one internal fair allocator shares the existing 12-item
  budget across populated sections and separately bounds retained nested references; the identical
  call produces 16,390 JSON bytes / 8,567 text bytes (60.6% / 61.2% reductions), 12 top-level items,
  and five nested references. Typed `truncated_evidence` categories identify exactly which kinds
  have more indexed entries, and existing expansion commands retrieve them. No public limit, config key, or
  surface-specific policy was added (`74ac8a8`). The committed release binary was installed with
  rollback preservation, all 12 MCP files and four guidance targets report configured, and the
  installed-path canary reproduces the 16,390-byte result with no transient installer payload.
- Follow-up dogfood showed that `74ac8a8` retained the earliest qualifying evidence and therefore
  lost later corrections and outcomes in long sessions. In a 17,107-message session, the old
  compact response stopped user intent at sequence 68. The corrected default `summary_items=-12`
  retained user sequences 16764, 16772, 16802, and 17069 in a measured 11,647-character JSON
  response; `summary_items=12` deterministically reproduced sequences 1, 22, 63, and 68, while
  `summary_items=0` returned all 14 records in a small-session canary with no truncation flags.
  CLI, MCP, and Python accept the same signed boundary convention; Rust exposes typed
  `EvidenceWindow::First | Last | All`; database ordering remains internal, avoiding a Rust-only
  public flag or boolean trap. Existing unlimited timeline expansion remains available for pipelines, with
  bounded focused and offset-page commands added alongside it. The local annotated tag
  `pre-evidence-window-redesign-20260714` points to validated pre-change commit `d402a8d`.
- The major Python boundary is now Rust-only: package import exposes the typed PyO3
  application/query facade, the console entry point dispatches the Rust CLI plus the one
  PyO3 call into the Rust MCP stdio server, and the legacy scanner, Typer CLI, JSON configuration,
  formatters, source adapters, symlink taxonomy, and Python analysis pipeline are absent
  from both source and wheel. Typer, Click, Rich, and orjson are no longer runtime
  dependencies. Repository Ruff, mypy over every maintained Python/test module,
  `mypy.stubtest`, 86 retained Python tests, the locked ARM64 wheel, and all four Python
  install mechanisms pass. The install harness selects an interpreter whose architecture
  matches the wheel instead of inheriting an incompatible ambient Python.
  Immutable demo recovery files are excluded from style mutation because their bytes are
  parser fixtures, not project source.
- The Python distribution now ships one `cp312-abi3` extension wheel contract for standard
  GIL-enabled CPython 3.12 through 3.14; isolated tests installed and executed the exact wheel
  on all three versions. Free-threaded CPython is explicitly out of scope until separately tested.
  Runtime/stub parity is a mandatory `mypy.stubtest` gate: native classes are typed as final,
  PyO3 constructor signatures use `__new__`, and every readable public query field is declared.
- MCP argument validation is schema-driven and precedes application open/index refresh. All seven
  top-level argument objects reject additional properties, numeric domains distinguish explicit
  unlimited zero from strictly positive page sizes, and regression tests prove rejected calls do
  not create an index. Database initialization now uses existential FTS-population probes instead
  of four full counts; on the 1,491,223-message local index the old probes measured about 4.83
  seconds total and the replacements each completed below 0.01-second resolution.
- Cross-provider dogfooding found that `corrections --provider` was parsed but ignored by a
  duplicate SQL-filter path. `011a38b` replaces that partial query construction with the shared
  message-filter builder, preserving the intrinsic user-role scope while applying provider,
  session ID, path, date, exclusion, sequence, tool, and compaction filters consistently. A
  two-provider regression passes along with the complete 16-stage gate (150 Python tests and 505
  active Rust unit tests plus integration suites). The validated archive was installed atomically
  with rollback at `~/.local/bin/aise.rollback.20260714205019`; installed read-only Claude and
  Codex calls returned disjoint provider-correct results. `ea61cfa` then standardized CLI and Rust
  API filter wording on “indexed session source” while Clap continues to list all eight exact IDs.
- Tool-name search is general and shared across CLI, MCP, Rust, and Python, but its Unicode
  substring predicate is not B-tree-indexed. On the live index, 1,239,308 tool-tagged rows contain
  only 174 distinct names: bounded common-name searches measured 0.00-0.24 seconds, while a rare
  exact name and a missing name measured 1.54 and 0.84 seconds. Adding a million-entry duplicate
  index is deferred until an index-byte/reindex-cost benchmark demonstrates a Pareto improvement;
  the current implementation and docs must not claim indexed substring execution.

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

## Execution status update 2026-07-14 (Rust/Python names, aliases, and MCP identity)

- `24e6f9b` exposes the same semantic result type names in Rust and Python. Python
  users now receive `SessionRecord`, `SearchHit`, `MessageHit`, `RoleStat`,
  `IndexStatus`, and the other Rust domain names; `Native*` remains private PyO3
  implementation vocabulary. Runtime exports, extension/facade stubs, return
  annotations, and a non-vacuous no-`Native*` contract move together.
- `ff5dc1f` makes the common post-package `aise install` step own relative
  `aisearch -> aise` and `ai_session_search -> aise` links. It preflights both
  destinations, refuses non-owned paths, rolls newly created links back when the
  following MCP/instruction transaction fails, reports each link in `status`, and
  removes only exact owned links. `--no-aliases` and `--keep-aliases` are the
  explicit install/uninstall controls; no second binary, copied wrapper, Cargo bin,
  or Python console script exists.
- `c039a44` changed the managed MCP registration and protocol identity from the
  opaque `aise` key to the intermediate `ai_session_search` key, while retaining
  `aise` as the single executable.
- The pre-1.0 naming correction uses `ai-session-search` for the current registration
  and protocol identity. Install removes both historical keys in the same planned
  JSON/TOML change, status reports either historical registration as stale, and
  uninstall recognizes all three owned keys.
- The complete local gate passed 16/16 after each public-surface commit. The final
  state passed 151 Python tests, 511 active Rust unit tests plus integration tests,
  Ruff, mypy, runtime/stub parity, clippy/fmt, Rust API doctests, release/MCP schema,
  Python wheel/sdist/install checks, and GitHub workflow syntax.
- The final commit was packaged through the deterministic native archive path and
  installed at the user-local executable path; its timestamped rollback was retained. Both aliases report
  `1.0.0-rc.1`, 12 detected MCP client configs and four managed instruction targets
  report configured, and an isolated initialize canary returned
  `serverInfo.name = "ai_session_search"`. This records that installation checkpoint;
  the pre-1.0 naming correction now tests `serverInfo.name = "ai-session-search"` and
  `serverInfo.title = "AI Session Search"`. The existing database path and contents
  were not part of installation or integration mutation.
- Tool-call provenance remains DRY: provider normalization already stores canonical
  server-qualified tool names such as `mcp:{server}:{tool}`. A separate
  `tool_server` column would duplicate that identity and allow contradictory rows, so
  no schema, CLI, MCP, Rust, or Python parameter was added.
- `a4c19d5`, `1d230ce`, `0711400`, and `1daa7c1` remove pre-release CLI, TOML, MCP,
  and Python aliases instead of carrying compatibility debt before the first release.
  `2745ae5` removes a redundant session join from role statistics; the measured
  provider-scoped live query fell from 7,438.7 ms to 1,533.8 ms without changing
  results. `54aca78` adds explicit MCP component selection to the installer lifecycle.
- `3b65790` documents all 14 root and 61 advanced Python exports at runtime and in
  stubs, with a contract rejecting undocumented public exports. `0d0b7a0` removes
  duplicated inspection defaults from the external Rust consumer. `0788704` fixes the
  isolated gate fixture to use `[providers.aistudio]` and pins that spelling in a
  repository contract. The subsequent full gate passed 16/16.
- Autorun commit `6e55f34` replaces the installed `ai-session-tools` skill's removed
  pre-release commands and raw Claude-only JSONL workflow with the current eight-provider
  CLI, refresh, recovery, analysis, and seven-tool MCP surfaces. Its focused contract,
  Ruff, and four tests pass; both skill validators accept it, with the scored audit at
  90% and zero failures. Autorun's repository-owned Codex reinstall regenerated the
  durable source, global skill, and plugin cache with identical SHA-256
  `7839fa9f17b4a8b12c16ef3a6c1c4881c0e6e38b6a4a9fbfe1e1bf0d3bcbd2d9` while
  preserving the separate repository's eight unrelated uncommitted files.

## Execution status update 2026-07-14 (skill ownership consolidation)

- AI Session Search became the sole source and lifecycle owner of its harness skill. That
  prerelease consolidation initially nested correction rules under `skills/ai-session-search/`.
  The current layout preserves that ownership guarantee while splitting responsibilities into two
  embedded sibling packages: `skills/ai-session-search/SKILL.md` supplies general harness
  guidance, `SKILL.md`, adjacent `aise-capability.toml`, and
  `references/message-classification.md`. Cargo, uv, pip, native archive, CLI, and MCP
  distributions therefore cannot ship different instructions or deterministic rules.
- Default `aise install`, `aise status`, and `aise uninstall` include skills for detected Claude,
  Codex, and Gemini/Antigravity harnesses. `--no-skill`, `--keep-skill`, and repeatable exact
  `--skill-root DIR` provide component and custom-destination control without a second installer.
- Skill writes join the existing durable text-file transaction. Install upgrades only marked
  owned files, uninstall removes only marked owned files, and an unmanaged conflict aborts
  preflight before any selected MCP, instruction, or skill mutation is published.
- Autorun no longer bundles, installs, probes, versions, or removes AI Session Search. This
  eliminates the duplicate package fallback and skill copy that could drift from `aise`; autorun
  retains only factual cross-references and demo examples.
- The first full local gate exposed Clippy `type_complexity` in the internal four-vector uninstall
  tuple while every behavioral stage passed. A named `UninstallPlan` now carries the same mutation
  and change-report vectors without altering transaction order or any public surface. The focused
  installer suite passed 45/45 and the corrected full gate passed 16/16: 151 Python tests, 514
  active Rust unit tests plus integration tests, formatting, Clippy, mypy, runtime/stub parity,
  doctests, release executable/MCP schema, Python artifacts/install pathways, and workflow syntax.
- The rollback-preserving local install retained a timestamped prior executable. `aise install` configured 12 MCP files,
  four instruction targets, three skill targets, and two aliases. The installed MCP initialize
  canary returned `serverInfo.name = ai_session_search` and version `1.0.0-rc.1`; all installed
  skill SHA-256 values matched the canonical embedded file. The two prior autorun-owned legacy
  skill directories were moved to Trash, and no index, cache, config, or source-session data was
  removed.
- After the corrected 16/16 gate, the final binary replacement retained a timestamped rollback. Repository and installed binary
  SHA-256 are both `a25223c5e8364c81d18f020f4918c8dd9e34da21064c94fdcb4085cce2a871e6`;
  canonical and all three installed skill SHA-256 values are
  `cb1a1700bebf594f57c8cce273c360f99446cd8de3d6b1ba2027af80e0f51a44`.
