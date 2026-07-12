# AI Session Search major migration

## Objective

Make this repository the canonical Apache-2.0 AI Session Search monorepo. Rust is
the default end-to-end engine; Python remains a first-class in-process API through
PyO3. CLI, MCP, and Python adapters share one typed service layer and one index
lifecycle. The release is a deliberate major version, not a permanent compatibility
stack.

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
- [ ] Make `aise` and `aise-mcp` thin native adapters; generate CLI/MCP/Python parity
  tests and remove redundant implementations at the major boundary.
- [ ] Implement owned background refresh cancellation and graceful MCP initialize,
  operation, EOF/signal, and shutdown behavior.
- [ ] Failure-inject lock permissions/types/contention, schema backfill, SQLite
  BUSY/LOCKED/I/O/corruption/disk-full/WAL/checkpoints, process crashes, and signals.
- [ ] Implement a configurable SQLite backup/migrate/validate/atomic-publish/rollback
  state machine; never raw-copy a live WAL database or run old/new writers together.
- [ ] Finalize AI Session Search names only after parity: repository/distribution
  candidate `ai-session-search`, executable `aise`, MCP executable `aise-mcp`, Python
  import `ai_session_search`, and one platform-derived config/index identity.
- [ ] Support and clean-install-test uv add/pip/tool/uvx, pip, Cargo registry/Git/path,
  sdist fallback, platform wheels, signed native archives, and installers.
- [ ] Generate Apache-2.0 metadata, provenance, relevant NOTICE content, third-party
  license inventory, SBOM, checksums, signatures, and artifact-content tests.
- [ ] Build immutable release candidates once, install-test exact artifacts on every
  supported platform, migrate the local installation with rollback ready, and retire
  legacy paths only after acceptance.

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
