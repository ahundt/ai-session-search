# Contributing to AI Session Search

Thanks for your interest. This document covers what the project expects from a
change, how to run the same gates CI runs, and where the deeper design records
live.

Report a suspected vulnerability privately instead of opening an issue. See
[SECURITY.md](SECURITY.md).

## Before you start

Search [existing issues](https://github.com/ahundt/ai-session-search/issues)
first. For anything beyond a small fix, open an issue describing the observed
behavior and the outcome you want before writing code; changes that alter public
behavior are reviewed against recorded product contracts and are easier to land
when the contract question is settled first.

State what you measured. A concrete report names the exact command, the exact
output, the provider and version involved, and the file and line you are talking
about. Never paste a real transcript into an issue: it is someone's session
content. `tests/aise-demo/` holds synthetic fixtures with obviously fake session
identifiers and paths, and the Rust tests build corpora in temporary
directories.

## Architecture in one paragraph

The Rust workspace is canonical. `rust/ai-session-search-core` owns parsing,
indexing, querying, migration, and filesystem publication, and produces the
`aise` executable including `aise mcp serve`.
`rust/ai-session-search-python` is a typed PyO3 adapter, and the Python package
is the compatibility API over it. The CLI, MCP server, Rust library, and Python
API are adapters over the same typed requests and responses: they translate
syntax, not product semantics. A behavior change belongs in the shared service,
not in one surface.

## Development setup

Requires Rust 1.88 or newer, CPython 3.12 through 3.14 with the standard GIL,
and [uv](https://docs.astral.sh/uv/).

```bash
git clone https://github.com/ahundt/ai-session-search
cd ai-session-search
uv sync --locked --all-extras
uv run maturin develop --uv        # build the native extension into the venv
```

Focused checks while iterating:

```bash
cargo test -p ai-session-search <test name>
uv run pytest tests/<file>.py -k <test name>
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
uv run ruff check . && uv run mypy ai_session_search tests
```

## The gate your change must pass

```bash
./run_ci_local.sh
```

This is the locally reproducible subset of `.github/workflows/ci.yml`. It
creates isolated configuration, cache, and database state with every provider
disabled, so it never reads or writes your real session index. It quarantines
any native extension already in the source tree and restores it by checksum on
exit.

It checks both lockfiles, builds the current abi3 extension, then runs Ruff,
mypy, native/stub parity, the Python tests, Rust formatting, check, Clippy,
tests, and public-API doctests, the release executable and its MCP schema, the
exact wheel and source distribution install pathways, and workflow syntax when
`actionlint` is installed.

The hosted operating-system and Python matrices stay CI-owned: CI additionally
runs the Rust portability suite on macOS and Windows, the MSRV check, the Cargo
registry/path/Git install pathways, `cargo deny check advisories licenses
sources bans`, and an offline `zizmor` workflow-security audit.

## What a good change looks like

- **Reproduce first.** Add the smallest failing test at the shared layer before
  implementing, then cover every adapter the change reaches: Rust, PyO3, Python,
  CLI, MCP, schemas, docs, examples, provider fixtures, and packaging.
- **One implementation per contract.** Look for duplicated meaning, not just
  duplicated text. Prefer improving an existing seam over adding a parallel one.
- **Keep surfaces honest.** Presentation windows and character budgets may
  shorten what is displayed; they must never change which results match, their
  order, their paging, or their next-page identity.
- **No silent cutoffs.** Do not add unannounced row, byte, or time limits.
  Intentional bounds are named parameters whose origin is reportable.
- **Change docs with code.** Source comments, CLI help, MCP schemas, Python
  docstrings, type stubs, examples, and the guides under `docs/` move together.
- **State bounds you rely on.** For work that touches scale, latency, memory,
  concurrency, indexing, or output volume, record a comparable baseline and
  rerun it.

The complete, numbered contract catalogue with its verification map is
[docs/development/maintainer-requirements-and-design-decisions.md](docs/development/maintainer-requirements-and-design-decisions.md).
Read the entries that touch your change before altering public behavior.

## Commits and pull requests

Write commit subjects that name the files or components and the behavior, for
example `mcp_server.rs: return next_offset when evidence is truncated`. Say what
changed, why, and how you verified it. Avoid vague verbs and internal shorthand;
someone reading the log a year from now should not need this conversation.

Open the pull request against `main`. All required CI checks must pass, and the
branch must be current with `main` before merge. Keep unrelated changes in
separate commits so a single concern can be reverted on its own.

## Documentation map

| Topic | Document |
| --- | --- |
| Product capabilities and quick start | [README.md](README.md) |
| Install, verify, update, uninstall | [docs/development/installation.md](docs/development/installation.md) |
| Settings resolution across CLI, env, TOML, MCP | [docs/development/configuration.md](docs/development/configuration.md) |
| Building and publishing artifacts | [docs/development/releasing.md](docs/development/releasing.md) |
| Release operator checklist | [RELEASING.md](RELEASING.md) |
| Cumulative product contracts | [docs/development/maintainer-requirements-and-design-decisions.md](docs/development/maintainer-requirements-and-design-decisions.md) |

## License

Contributions are accepted under the [Apache License 2.0](LICENSE), matching the
project license. Existing commit authorship is preserved in Git history; see
[docs/migration/provenance.md](docs/migration/provenance.md).
