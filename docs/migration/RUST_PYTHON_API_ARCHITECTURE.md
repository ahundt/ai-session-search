# Rust and Python API architecture

Related plans: [major migration](AI_SESSION_SEARCH_MAJOR_MIGRATION.md) and
[capability parity](CAPABILITY_PARITY.md).

## Workspace target

```text
crates/session-search-core/       public provider/model/query primitives
crates/session-search-service/    public catalog/message/file/export/source/maintenance services
crates/session-search-analysis/   optional analysis, graph, taxonomy services
crates/aise-cli/                  one native executable (`aise`), including `aise mcp serve`
                                   as the thin MCP transport/lifecycle adapter
crates/aise-python/               PyO3 conversion/module crate only
python/ai_session_search/         typed Python facade, documentation, compatibility helpers
```

The imported `rust/ai-session-search-core` crate is split mechanically only after service
boundaries are proven in place. Do not move code and redesign behavior in the same
commit. Every intermediate workspace uses one root `Cargo.lock` and root `target`.
The temporary `aise-mcp` binary remains only while capability, installer, and
lifecycle parity are being validated. Remove it in the final consolidation step,
after all other CLI/MCP work is stable, and point every client at `aise mcp serve`.

## Public Rust API contract

- Rust is a first-class supported library with SemVer, rustdoc examples, changelog,
  MSRV policy, feature matrix, and downstream compile tests.
- Public request/result types live in core/service crates, have private fields where
  future evolution requires builders, and implement appropriate standard traits.
- Use newtypes/builders for paths, limits, cursors, time bounds, and operation modes
  when primitive values admit invalid states. Validate at construction boundaries.
- Public functions document errors, cancellation, blocking, allocation, ordering,
  pagination, and stale-index semantics. They never panic on session input.
- Expose iterators/pages and intermediate typed results so callers avoid duplicate
  work; callers choose output destinations and rendering.
- Storage, clap, MCP, and PyO3 types remain private adapter details.
- Feature flags stay capability-level: providers, analysis, Python. Do not create a
  feature per command, format, or configuration field.

## Python API contract

- PyO3 depends inward on stable Rust services; Rust core/service crates never depend
  on Python.
- Python classes are typed conversions/builders over Rust request/result types, not
  a second query engine.
- Parsing, indexing, SQLite, search, reconstruction, and serialization independent
  of Python objects run through `Python::detach` so other Python threads progress.
- Initial API is synchronous because legacy aise is synchronous. Async wrappers are
  added only for measured consumers and must not duplicate service logic.
- Target `abi3-py312` only after limited-API and lowest/highest-version tests pass.
  Test free-threaded CPython separately rather than assuming ordinary abi3 coverage.
- Python exceptions map from stable Rust error categories with actionable structured
  fields; never parse Rust error strings to recover semantics.

## Distribution contract

- Maturin builds the mixed Python/Rust package and platform wheels.
- `uv add`, `uv pip install`, and pip install the importable facade and extension.
- `uv tool install`/uvx expose declared Python-distributed executables.
- Cargo registry/Git/path installs expose native binaries and Rust libraries.
- Signed native archives/installers serve users without Python or Rust.
- Clean builds use published dependency graphs (`uv build --no-sources`) and test
  sdist fallback explicitly. Wheels exist for every supported interpreter/platform
  so ordinary users do not unexpectedly compile Rust.
- Release candidates include separate sanitized CycloneDX graphs for the Python
  runtime and both Rust packages. Machine-local path references are rejected.
- AI Session Search is Apache-2.0; compatible third-party licenses remain intact and
  are checked as policy plus emitted as runtime inventories rather than relabeled.

## Required gates

- `cargo test/check/clippy/doc --workspace --all-features` and rustdoc link checks.
- Downstream fixture crates compile common Rust API examples at MSRV and current Rust.
- Python type checking, import/API differential tests, wheel/abi inspection, and GIL
  progress tests during long native operations.
- CLI/MCP/Python generated parity matrix over operation names, request fields,
  defaults, mutual exclusions, error categories, pagination, and result schemas.
- SemVer review for every public Rust/Python type or behavior change.
- Demo generation runs only after the one-executable CLI/MCP contract is final; its
  sanitized script is tested, while generated GIF/video artifacts stay out of Git.

## Primary guidance

- Cargo workspaces share one lockfile and output directory and support inherited
  package metadata and dependencies.
- Rust API Guidelines require predictable naming/conversions, common traits,
  validated inputs, thorough examples, failure documentation, caller control, and
  stable permissively licensed dependencies.
- PyO3 models Python attachment with explicit lifetimes and provides
  `Python::detach` for Python-independent work.
- PyO3 stable ABI features reduce wheel count but constrain the API and require
  explicit interpreter/free-threaded compatibility testing.
