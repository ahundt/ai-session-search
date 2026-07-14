# Rust and Python API architecture

Related plans: [major migration](ai-session-search-major-migration.md) and
[capability parity](capability-parity.md).

## Current workspace and long-term split

```text
rust/ai-session-search-core/      public Rust services plus the one `aise` executable
rust/ai-session-search-python/    PyO3 conversion/module crate only
ai_session_search/                typed Python facade and stubs
```

The core already exposes catalog/message/file/export/source/index/analysis service
boundaries. Split it into narrower crates only after a measured compile-time, ownership,
or independent-versioning need; do not move code and redesign behavior in the same commit.
Every workspace uses one root `Cargo.lock` and root `target`.
Executable consolidation is complete: Cargo and Python distributions expose only
`aise`; the Rust CLI and PyO3-backed Python entry point both serve MCP at
`aise mcp serve`, and generated client entries use the same argument contract.

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
- Filesystem effects are explicit. Single-file restore uses collision-safe create-new
  publication; multi-version recovery requires an absolute caller destination and one
  same-parent atomic no-replace directory transaction with a typed receipt.
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
- Lazy reconstruction owns selected edit rows and releases the application mutex before
  Python iteration or explicit publication; Python never replays or renames versions itself.
- Initial API is synchronous because legacy aise is synchronous. Async wrappers are
  added only for measured consumers and must not duplicate service logic.
- Ship `cp312-abi3` wheels for standard GIL-enabled CPython 3.12 through 3.14. The
  exact same wheel must install, import, and execute native calls on all three versions.
  Free-threaded CPython remains unsupported until separate `abi3t` or version-specific
  wheels pass dedicated runtime tests.
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
- Python type checking, compiled-runtime/stub parity (`mypy.stubtest`), import/API
  differential tests, exact `cp312-abi3` wheel execution on CPython 3.12 through 3.14,
  wheel/abi inspection, and GIL progress tests during long native operations.
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
