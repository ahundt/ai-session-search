# Releasing AI Session Search

Releases are built from the monorepo root. The Rust workspace is the canonical
implementation; the Python distribution contains the typed PyO3 adapter and the
Python compatibility API. Never rebuild between verification and publication.

## Version and compatibility contract

Keep `pyproject.toml`, `rust/ai-session-search-core/Cargo.toml`,
`rust/ai-session-search-python/Cargo.toml`, and the source-tree fallback version
in `ai_session_search/__init__.py` equal. A release requires Rust 1.85 or newer
and Python 3.12 or newer. This migration intentionally starts at version 1.0.0;
the former single-user package does not constrain its compatibility surface.
Release automation pins uv 0.11.28, cargo-cyclonedx 0.5.9, and cargo-deny
0.20.2; update those versions
only in a reviewed toolchain change with regenerated lock/SBOM evidence.

The temporary `aise-mcp` binary remains part of pre-consolidation builds. Remove
it only in the plan's final executable-consolidation change, after replacing all
generated client commands with `aise mcp serve` and rerunning every gate below.

## Local release candidate gate

Use isolated config/cache directories. Never point tests at a user's live index.

```bash
# Point this at a Python 3.12 interpreter matching the host/target architecture.
export PYO3_PYTHON=/path/to/python3.12
cargo fmt --all --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo deny --locked check licenses sources bans
uv sync --locked --all-extras
uv run ruff check .
uv run pytest -m 'not integration'
uv run maturin build --release --locked --out dist
uv run maturin sdist --out dist
uv run python scripts/verify_release_artifacts.py dist/*
wheel=$(find dist -maxdepth 1 -name '*.whl' -print -quit)
uv run --isolated --no-project --with "$wheel" python \
  scripts/python_license_inventory.py --output dist/python-runtime-licenses.md
uv export --locked --no-dev --format cyclonedx1.5 \
  --output-file dist/ai-session-search-python-runtime.cdx.json
cargo install cargo-cyclonedx --version 0.5.9 --locked
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) \
  cargo cyclonedx --manifest-path Cargo.toml --format json \
    --spec-version 1.5 --target all --all-features --all
uv run python scripts/sanitize_sboms.py --root "$PWD" \
  --source-date-epoch "$(git log -1 --format=%ct)" rust/*/*.cdx.json
```

Install the wheel into a new environment, run `aise --version`, import both
`ai_session_search` and `ai_session_search._native`, exercise one typed query,
and start/stop the MCP server through EOF and cancellation. Validate Cargo and
`uv tool install` paths separately because they exercise different launchers.

## Artifact invariants

- `uv.lock` and `Cargo.lock` are committed and every automated install is locked.
- Linux wheels use manylinux2014; macOS builds cover arm64 and x86_64; Windows
  builds cover x64. The source distribution is a fallback for supported systems.
- Wheels contain the native extension, typed stubs, `py.typed`, `LICENSE`, and
  `NOTICE`. Source distributions also contain both lock/build manifests.
- Archives contain no demo media, absolute/traversal paths, legacy Python package
  directories, or symbolic/hard links.
- CI uploads platform artifacts; a separate job verifies and combines them; the
  publish job downloads exactly that verified set. It does not rebuild.
- Every third-party action is pinned to a reviewed commit SHA. CycloneDX 1.5 SBOMs
  are generated independently from the locked runtime Python and Rust graphs. SBOM
  identity is not license approval: review the separate third-party license inventory.
  Before enabling a public release, add provenance attestations
  for the downloadable artifacts. Attestations supplement inspection; they do
  not establish that an artifact is safe.

## Release lifecycle

1. Create one release branch from a green `main` and make only version/release
   corrections on it. Do not rewrite shared history or force-push.
2. Run the local gate, inspect `git diff --staged`, and commit the version change.
3. Create an annotated `vX.Y.Z` tag only after the commit is reviewed.
4. The tag workflow reruns CI, builds the wheel matrix and sdist once, verifies
   archive contents, records checksums, and pauses at the protected `pypi`
   environment before publishing the same artifacts through trusted publishing.
5. Install the published version into clean Rust and Python environments, verify
   CLI/MCP startup and database compatibility, then record the result. If the
   post-release check fails, stop publication/rollout and issue a new patch;
   never replace an immutable version.

No tag, push, trusted-publisher registration, or public release is authorized by
this document. Those are explicit maintainer actions outside local migration work.
