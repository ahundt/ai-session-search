# Releasing AI Session Search

This is the release operator checklist. The detailed design and recovery
contract is in [docs/development/releasing.md](docs/development/releasing.md).
Nothing in this file authorizes a tag, push, registry publication, trusted
publisher registration, or GitHub release.

Run every command from the monorepo root. The Rust workspace is canonical;
the Python distribution contains the typed PyO3 adapter and compatibility API.
Never rebuild between verification and publication.

## Current release identity

The release being prepared is `1.0.0rc1` for Python and `1.0.0-rc.1` for
Cargo. Its tag is `v1.0.0rc1`.

Keep all five declarations aligned. The consumer crate stays unpublished at
`0.0.0`; only its requirement on the released core crate carries the release
version:

| Location | Field | Required RC1 value |
| --- | --- | --- |
| `pyproject.toml` | `project.version` | `1.0.0rc1` |
| `rust/ai-session-search-core/Cargo.toml` | `package.version` | `1.0.0-rc.1` |
| `rust/ai-session-search-python/Cargo.toml` | `package.version` | `1.0.0-rc.1` |
| `rust/ai-session-search-python/Cargo.toml` | `dependencies.ai-session-search.version` | `1.0.0-rc.1` |
| `tests/rust-api-consumer/Cargo.toml` | `dependencies.ai-session-search.version` | `1.0.0-rc.1` |

Cargo resolves a stale `1.0.0-rc.1` requirement against a `1.0.0` core crate
without complaint, so `cargo check --locked` cannot report either dependency
drifting. The metadata gate is the only check that does.

Run the metadata gate before creating a tag:

```bash
uv run python -m scripts.verify_release_metadata --tag v1.0.0rc1
```

The release requires Rust 1.88 or newer and CPython 3.12 through 3.14 with
the standard GIL enabled. Wheels use `cp312-abi3`. Free-threaded CPython is not supported.
The distribution exposes one executable, `aise`, and MCP clients run
`aise mcp serve`.

This is the first public compatibility baseline at 1.0.0. The former private,
single-user package does not define the public compatibility contract.

Pinned release tools are uv 0.11.28, cargo-cyclonedx 0.5.9, and cargo-deny
0.20.2. Change them only in a separate reviewed toolchain change.

## Release blockers

Do not create the RC1 tag until all of these are complete:

- The crates.io account exists, its email is verified, and the
  `ai-session-search` crate name has been bootstrapped with RC0.
- The crates.io trusted publisher matches repository
  `ahundt/ai-session-search`, workflow `publish.yml`, and environment
  `crates-io`.
- The pending PyPI trusted publisher matches project `ai-session-search`,
  repository `ahundt/ai-session-search`, workflow `publish.yml`, and
  environment `pypi`.
- GitHub environments `crates-io`, `pypi`, and `release` have the intended
  maintainers and approval rules.
- The exact RC1 commit passes the local gate and package preparation below.

These are release blocking because the workflow publishes in the order
crates.io, PyPI, then GitHub Release. Registry versions are immutable.

## One-time account and publisher setup

### crates.io

1. Sign in to crates.io with GitHub, provide and verify the account email,
   and create a short-lived API token.
2. Run `cargo login` and enter that token. Cargo stores it in
   `~/.cargo/credentials.toml`.
3. Use the token only for the RC0 bootstrap below.
4. After the trusted publisher is registered, revoke the token and run
   `cargo logout`.

Crate names are first-come-first-served and published versions cannot be
overwritten. See the
[Cargo publishing guide](https://doc.rust-lang.org/cargo/reference/publishing.html).

### PyPI

1. Create and verify the PyPI account and enable its required two-factor
   authentication.
2. Register a pending GitHub Actions trusted publisher with the exact project,
   repository, workflow, and environment values listed above.

A pending publisher creates the project on first publication and does not
reserve the name. No long-lived PyPI upload token is needed. See
[PyPI pending publishers](https://docs.pypi.org/trusted-publishers/creating-a-project-through-oidc/).

The existing `ai-session-tools` project and its history cannot be renamed or
merged into `ai-session-search`. Publish a final deprecation pointer there
only as a separate maintainer decision.

## One-time crates.io RC0 bootstrap

crates.io requires the crate to exist before its trusted publisher can be
registered. Do not manually publish RC1 because the tag workflow must publish
that unused version.

1. Create and review a dedicated bootstrap commit with all five declarations
   set to `1.0.0rc0` or `1.0.0-rc.0` as appropriate. Refresh both lockfiles.
2. Run `./run_ci_local.sh`, then package, inspect, and dry-run the exact crate:

   ```bash
   cargo package --locked -p ai-session-search
   uv run python -m scripts.verify_release_artifacts \
     target/package/ai-session-search-1.0.0-rc.0.crate
   cargo publish --dry-run --locked -p ai-session-search
   ```

3. Create the annotated provenance tag
   `crate-bootstrap-v1.0.0-rc.0`. It intentionally does not match the
   `publish.yml` `v*` trigger.
4. From that unchanged clean checkout, explicitly authorize and run:

   ```bash
   cargo publish --locked -p ai-session-search
   ```

5. Register the crates.io trusted publisher, revoke the bootstrap token, and
   run `cargo logout`.
6. Restore all five declarations to RC1, refresh the lockfiles, and rerun the
   complete RC1 gate. Do not create `v1.0.0rc1` before this is green.

## RC1 local gate

The authoritative local gate creates isolated config, cache, and database
state. It quarantines and checksum-restores any source-tree native extension,
so it does not use a real user database:

```bash
AI_SESSION_SEARCH_RUSTC_WRAPPER= ./run_ci_local.sh
```

Omit `AI_SESSION_SEARCH_RUSTC_WRAPPER=` when the configured compiler wrapper,
such as sccache, works in the current environment.

The gate checks both lockfiles, builds the current ABI3 extension, runs Ruff,
mypy, stub parity, Python tests, Rust formatting/check/Clippy/tests/doctests,
the release executable and MCP schema, exact wheel and sdist install pathways,
and workflow syntax when `actionlint` is installed.

Run the release policy check with the pinned cargo-deny version:

```bash
cargo deny --locked check licenses sources bans
```

Prepare a fresh, complete package directory. The destination must not exist:

```bash
uv run python -m scripts.prepare_packages
```

Use `--package rust` or `--package python` only for diagnosis. Never merge
package directories from different attempts or rebuild between verification
and publication.

Before tagging, confirm:

- The release branch started from a green `main` commit and contains only
  reviewed version or release corrections. Do not rewrite shared history or
  force-push.
- `git status --short` is clean.
- The staged release diff was inspected before its version commit.
- `python -m scripts.verify_release_metadata --tag v1.0.0rc1` passes.
- The local wheel, sdist, and crate prepared above pass artifact verification.
- The wheel contains the extension, typed stubs, `py.typed`, `LICENSE`, and
  `NOTICE`; sdists contain both lockfiles and build manifests.
- Archives contain no demo media, absolute or traversal paths, legacy Python
  package directories, symlinks, or hard links.

## Tag workflow

Create the annotated tag only after reviewing the exact commit. The tag must
be `v` plus the PEP 440 version from `pyproject.toml`.

`publish.yml` then:

1. reruns the reusable CI and metadata gates;
2. builds each wheel, native archive, sdist, and crate once;
3. installs and tests the exact artifacts on their target runners;
4. verifies the complete artifact set, writes `SHA256SUMS`, and creates GitHub
   build-provenance attestations;
5. reproduces the attested crate before requesting short-lived crates.io
   credentials;
6. pauses at `crates-io`, publishes through OIDC, then pauses at `pypi` and
   publishes the verified wheel/sdist set with PyPI attestations;
7. pauses at `release` and creates the GitHub prerelease from the same verified
   artifacts.

Approve protected environments only in that order. Do not rebuild or replace
an artifact between stages. Before the first approval, confirm five wheels, one
sdist, one crate, five native archives, checksums, three CycloneDX SBOMs,
separate Python/Rust license inventories, and attestations. Every third-party
Action must remain pinned to a reviewed commit SHA. Attestations supplement
artifact inspection; they do not prove an artifact safe.

## Post-release and recovery

Install the published version into clean Cargo and Python environments. Verify
`aise --version`, `aise package status`, a typed search, Python imports, MCP
startup/EOF/cancellation, and database compatibility.

If a stage fails:

- Before any registry publication, fix the cause, rerun the full gate, and
  create a new tag only if the immutable tag or artifacts changed.
- If crates.io succeeded and PyPI failed, rerun only the failed jobs from the
  same workflow when the verified artifacts are unchanged.
- If both registries succeeded and GitHub Release failed, rerun only the
  release job from the same workflow.
- If any published artifact must change, publish a new version. Never replace
  an immutable registry version.

Record the failing job, artifact hashes, registry state, affected targets,
user impact, and the regression test that prevents recurrence.
