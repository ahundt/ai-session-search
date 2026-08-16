# Releasing AI Session Search

This is the release operator checklist. The design and recovery contract behind it is
[docs/development/releasing.md](docs/development/releasing.md). Nothing in this file authorizes
a tag, push, registry publication, trusted-publisher registration, or GitHub release: those are
maintainer actions taken through the protected environments named below.

Run every command from the monorepo root. The Rust workspace is canonical; the Python
distribution contains the typed PyO3 adapter and compatibility API. Never rebuild between
verification and publication.

## Release identity

One release has one identity, spelled the way each ecosystem requires:

| Release | Python and Git tag (PEP 440) | Cargo (SemVer) |
| --- | --- | --- |
| Release candidate N of X.Y.Z | `X.Y.ZrcN`, tag `vX.Y.ZrcN` | `X.Y.Z-rc.N` |
| Final X.Y.Z (including patch releases) | `X.Y.Z`, tag `vX.Y.Z` | `X.Y.Z` |

`scripts/release_versions.py` is the sole mapping between the two spellings. Preparing a
release means setting all seven declarations to the new version in one commit and tagging that
commit. The consumer crate stays unpublished at `0.0.0`; only its requirement on the released
core crate carries the release version:

| Location | Field |
| --- | --- |
| `pyproject.toml` | `project.version` |
| `rust/ai-session-search-core/Cargo.toml` | `package.version` |
| `rust/ai-session-search-python/Cargo.toml` | `package.version` |
| `rust/ai-session-search-python/Cargo.toml` | `dependencies.ai-session-search.version` |
| `tests/rust-api-consumer/Cargo.toml` | `dependencies.ai-session-search.version` |
| `skills/ai-session-search/SKILL.md` | `metadata.version` |
| `rust/ai-session-search-core/skills/ai-session-search/SKILL.md` | `metadata.version` |

Cargo resolves a stale `X.Y.Z-rc.N` requirement against a newer core crate without complaint,
so `cargo check --locked` cannot report either dependency drifting. The metadata gate is the
only check that does; run it before creating a tag:

```bash
uv run python -m scripts.verify_release_metadata --tag vX.Y.ZrcN
```

What is published is recorded by the registries and by `git tag`, never by this file: `git tag
--list 'v*'`, `https://crates.io/crates/ai-session-search`, and
`https://pypi.org/project/ai-session-search/`. The working tree declares the next candidate in
the seven locations above.

## Toolchain and compatibility

Releases require Rust at or above the workspace `rust-version` (1.88 at the time of writing;
`Cargo.toml` is authoritative) and CPython 3.12 through 3.14 with the standard GIL enabled. Wheels use `cp312-abi3`.
Free-threaded CPython is not supported. The distribution exposes one executable, `aise`, and
MCP clients run `aise mcp serve`.

1.0.0 is the first public compatibility baseline; the former private single-user package does
not define the public compatibility contract. Release tools (uv, cargo-cyclonedx, cargo-deny)
are pinned by version in `.github/workflows/ci.yml`, `prepare-packages.yml`, and `publish.yml`;
change them only in a separate reviewed toolchain change.

Behavior changes that ship in a release are described in the GitHub release notes for that
version and in the requirements document, not here.

## Release blockers

Registration is done once and holds for every later release. Confirm rather than redo:

- The crates.io crate exists and its trusted publisher is registered for repository
  `ahundt/ai-session-search`, workflow `publish.yml`, environment `crates-io`:
  `https://crates.io/crates/ai-session-search/settings`.
- The PyPI trusted publisher is registered for the same repository, workflow, and environment
  `pypi` (a pending publisher until the first upload creates the project, an ordinary project
  publisher afterwards).
- GitHub environments `crates-io`, `pypi`, and `release` have the intended maintainers and
  approval rules.
- The `release-tags` ruleset is active over `refs/tags/v*`:
  `gh api repos/ahundt/ai-session-search/rulesets`.

Do not create a tag until these are also true of the specific release:

- The exact release commit passes the local gate and package preparation below.
- Every declaration in the identity table is the new version, and the metadata gate passes
  against the tag you are about to create.
- The message-search response shape is the one you intend to publish. Once any version is on a
  registry, removing a field, renaming one, changing a type, or changing what a value means
  requires incrementing `MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION` across the serializer, the
  closed MCP `outputSchema`, the Python stubs, and every fixture; see
  `REQ006-report-extent-honestly` in
  `docs/development/maintainer-requirements-and-design-decisions.md`.

These block the tag because the workflow publishes in the order crates.io, PyPI, then GitHub
Release, and registry versions are immutable.

## One-time account and publisher setup

Both registries are set up; this section records how, for recovery and for anyone reproducing
the project elsewhere.

### crates.io

1. Sign in to crates.io with GitHub, provide and verify the account email, and create a
   short-lived API token.
2. Run `cargo login` and enter that token. Cargo stores it in `~/.cargo/credentials.toml`.
3. Use the token only for the manual publish that creates the crate.
4. After the trusted publisher is registered, revoke the token and run `cargo logout`.

Crate names are first-come-first-served and published versions cannot be overwritten. See the
[Cargo publishing guide](https://doc.rust-lang.org/cargo/reference/publishing.html).

### PyPI

1. Create and verify the PyPI account and enable its required two-factor authentication.
2. Register a pending GitHub Actions trusted publisher with the exact project, repository,
   workflow, and environment values listed above.

A pending publisher creates the project on first publication and does not reserve the name. No
long-lived PyPI upload token is needed. See
[PyPI pending publishers](https://docs.pypi.org/trusted-publishers/creating-a-project-through-oidc/).

The earlier `ai-session-tools` project and its history cannot be renamed or merged into
`ai-session-search`. Publish a final deprecation pointer there only as a separate maintainer
decision.

## Manual crate publish: bootstrap once, fallback afterwards

crates.io requires the crate to exist before its trusted publisher can be registered, and it
has no pending-publisher equivalent to PyPI's, so the very first version of a crate is published
by hand ([RFC 3691](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html)).
Once the trusted publisher is registered, `publish-crate` publishes through the workflow, and
this procedure remains only for a workflow that cannot obtain credentials.

The `publish-crate` job compares the registry's recorded sha256 against the attested crate: a
matching checksum sets `published=true` and skips both the credential request and `cargo
publish`, so `publish` and `release` still run; differing bytes fail the job instead of
replacing an immutable version. That skip means a manually published version never exercises
the workflow's own publish step; the first version the workflow publishes on its own is the
first proof that trusted publishing works. Enable "Require trusted publishing for all new
versions" in the crate settings only after such a publish has succeeded, because it also blocks
this manual procedure.

The version published by hand is the release version itself, never a placeholder such as
`X.Y.Z-rc.0`: a placeholder consumes a version number nobody installs and invites a yank that
is not warranted, and it is unnecessary because the `crate` job builds the crate twice and
requires `cmp` to pass, so `cargo package` output is deterministic and reproduces the
workflow's attested crate byte for byte. Verify that rather than assume it. Because the
comparison needs the attested artifact, this happens after the tag, between the workflow
parking at `crates-io` and approving that environment:

1. From the clean tagged checkout, reproduce the crate and confirm it matches what the
   workflow attested:

   ```bash
   git checkout --detach vX.Y.ZrcN
   gh run download <run-id> --name verified-crate-distribution --dir attested
   cargo package --locked --no-verify -p ai-session-search
   cmp attested/*.crate target/package/ai-session-search-X.Y.Z-rc.N.crate
   ```

   The detached checkout is load-bearing. `cargo package` writes `.cargo_vcs_info.json` into
   the crate recording the commit it was built from, so building from `main` after even a
   docs-only commit produces a different sha256 while every packaged file is identical.
   Publishing that crate consumes the version with bytes the attestation does not cover,
   `publish-crate` then fails the checksum comparison on every retry, and because registry
   versions are immutable and `cargo yank` does not free the number, the only exits are a new
   version or shipping unattested bytes. `cmp` catches this; run it.

2. Only if `cmp` is silent, explicitly authorize and run:

   ```bash
   cargo publish --locked -p ai-session-search
   ```

3. Confirm the crates.io trusted publisher still matches repository `ahundt/ai-session-search`,
   workflow `publish.yml`, and environment `crates-io`; this is a check, not a re-registration.
4. Revoke any token created for this publish and run `cargo logout`. A token minted for a
   one-off publish should not outlive it; while a token with `publish-new` or `publish-update`
   survives, the crate can be published outside the workflow and its approvals.
5. Approve `crates-io`, and confirm the job logs the skip rather than publishing a second time.

## Pre-release semantics

Three surfaces express pre-release status, two of them implicitly. Verify all three rather than
assuming the version string carried through:

| Surface | How it is expressed | Verify |
| --- | --- | --- |
| GitHub Release | Explicit `--prerelease`, chosen by the `case` on the tag in the `release` job | `gh release view vX.Y.ZrcN --json isPrerelease` |
| PyPI | Implicit in the PEP 440 spelling `X.Y.ZrcN` | `curl -s https://pypi.org/pypi/ai-session-search/json` and read `info.version` |
| crates.io | Implicit in the SemVer spelling `X.Y.Z-rc.N` | `curl -s https://crates.io/api/v1/crates/ai-session-search` and read `max_stable_version` |

The GitHub flag is the only one a release can get wrong on its own; the other two follow from
the version string the metadata gate already pins. crates.io reporting
`"max_stable_version": null` is the positive signal that it classified the version as a
pre-release. A final `X.Y.Z` tag takes the non-prerelease branch of the same `case`.

### While no stable version exists, plain installs resolve to the newest release candidate

This surprises people, so do not "fix" it. Measured against `1.0.0rc1` while it was the only
published version:

```
uv pip install ai-session-search              -> ai-session-search==1.0.0rc1
cargo add ai-session-search --dry-run         -> Adding ai-session-search v1.0.0-rc.1
```

Neither warns. This is specified behavior, not a marking failure.
[PEP 440](https://peps.python.org/pep-0440/#handling-of-pre-releases) excludes pre-releases
from version specifiers "unless they are already present on the system, explicitly requested by
the user, or if the only available version that satisfies the version specifier is a
pre-release." Cargo resolves the same way when a crate has no stable version. Once any stable
version exists, both resolvers prefer it and a release candidate is reachable only by an
explicit pin. Do not publish a stable version merely to change this, and do not yank a
candidate to hide it; if plain installs must not reach a pre-release, the only real options are
to keep release candidates off the public registries or to say so in the README.

## Local gate

The authoritative local gate creates isolated config, cache, and database state. It quarantines
and checksum-restores any source-tree native extension, so it does not use a real user database:

```bash
./run_ci_local.sh
```

Run it this way. The gate inherits whatever compiler wrapper Cargo is configured to use, so an
installed `sccache` reuses its cache across runs and checkouts, and it prints the wrapper it
resolved before the first step. Only when an inherited wrapper is broken in the current
environment, override it:

```bash
AI_SESSION_SEARCH_RUSTC_WRAPPER= ./run_ci_local.sh
```

That form exports an empty `RUSTC_WRAPPER`, which turns the wrapper off. The gate also sets
`CARGO_INCREMENTAL=0`, so with no wrapper every run is a cold full rebuild of a large workspace
and takes far longer than a cached one.

The gate checks both lockfiles, builds the current ABI3 extension, runs Ruff, mypy, stub parity,
Python tests, Rust formatting/check/Clippy/tests/doctests, the release executable and MCP
schema, exact wheel and sdist install pathways, and workflow syntax when `actionlint` is
installed. CI runs `actionlint` in the required `workflow-security` job, so a workflow syntax
error blocks the merge whether or not it was caught locally.

Run the release policy check with the pinned cargo-deny version, the same four checks CI runs:

```bash
cargo deny --locked check advisories licenses sources bans
```

Prepare a fresh, complete package directory. The destination must not exist:

```bash
uv run python -m scripts.prepare_packages
```

Use `--package rust` or `--package python` only for diagnosis. Never merge package directories
from different attempts or rebuild between verification and publication.

Before tagging, confirm:

- The release branch started from a green `main` commit and contains only reviewed version or
  release corrections. Do not rewrite shared history or force-push.
- `git status --short` is clean.
- The staged release diff was inspected before its version commit.
- `python -m scripts.verify_release_metadata --tag vX.Y.ZrcN` passes.
- The local wheel, sdist, and crate prepared above pass artifact verification.
- The wheel contains the extension, typed stubs, `py.typed`, `LICENSE`, and `NOTICE`; the sdist
  carries `Cargo.lock` and both build manifests. `uv.lock` is deliberately absent, because it
  locks the development extras and no install path from the sdist reads it. `python -m
  scripts.verify_release_artifacts` holds the enforced list.
- Archives contain no demo media, absolute or traversal paths, legacy Python package
  directories, symlinks, or hard links.

## TestPyPI rehearsal

crates.io has no test registry, so only the Python half can be rehearsed. Do it before approving
`pypi`, because a rejected wheel tag or unrenderable metadata cannot be fixed in place once
crates.io has published an immutable version.

Register a pending publisher on TestPyPI, which is a separate account from PyPI, using the same
project, owner, and workflow values as PyPI but environment `testpypi`. TestPyPI re-prompts for
the account password before accepting publisher changes; a submission made after that window
lapses is discarded without an error, so confirm the publisher appears under **Pending
publishers** before continuing.

`gh workflow run publish.yml --ref vX.Y.ZrcN` then reuses the same build, verification, and
attestation pipeline and uploads to TestPyPI. Confirm it installs:

```bash
uv run --isolated --no-project --default-index https://test.pypi.org/simple/ \
  --with ai-session-search==X.Y.ZrcN aise --version
```

`--default-index` replaces PyPI rather than adding to it, so the command fails if anything has
to come from the production index. That is the stronger check and it holds here because every
`Requires-Dist` entry in the wheel is gated behind `extra == 'dev'`, leaving no runtime
dependency to resolve. A project with runtime dependencies absent from TestPyPI needs `--index`
and `--index-strategy unsafe-best-match` instead. `--no-project` keeps this checkout's own
`pyproject.toml` out of the resolution.

`publish-crate`, `publish`, and `release` are gated on `github.event_name == 'push'` and
`publish-testpypi` on `workflow_dispatch`, and those are the only triggers, so a dispatch can
never reach a production registry and a tag push can never reach TestPyPI. A rehearsal consumes
the version on TestPyPI; a second attempt at the same version needs a new one.

## Tag workflow

Create the annotated tag only after reviewing the exact commit. The tag must be `v` plus the
PEP 440 version from `pyproject.toml`.

The `release-tags` repository ruleset restricts creating, moving, and deleting `refs/tags/v*`
to the repository admin role, so write access alone cannot fire this workflow, and a tag that
named a published artifact cannot later be repointed at a different commit. The maintainer
holds that role and is unaffected.

`publish.yml` then:

1. reruns the reusable CI and metadata gates;
2. builds each wheel, native archive, sdist, and crate once, pinning the build clock to the
   commit and then requiring each wheel's embedded SBOM to record that exact clock, so a
   manylinux container that never received the pin fails the job instead of shipping a wheel
   that cannot be rebuilt from its commit;
3. installs and tests the exact artifacts on their target runners;
4. verifies the complete artifact set, writes `SHA256SUMS`, and creates GitHub build-provenance
   attestations;
5. reproduces the attested crate, then compares the registry's recorded sha256 for this
   version before requesting short-lived crates.io credentials. An absent version publishes; a
   version already carrying the attested checksum is skipped so a retry reaches the remaining
   jobs; a version carrying different bytes fails, because the tag would otherwise try to
   replace an immutable release;
6. pauses at `crates-io`, publishes through OIDC, then pauses at `pypi` and publishes the
   verified wheel/sdist set with PyPI attestations;
7. pauses at `release` and creates the GitHub release (marked pre-release for an `rcN` tag)
   from the same verified artifacts.

Approve protected environments only in that order. Do not rebuild or replace an artifact
between stages. Before the first approval, confirm five wheels, one sdist, one crate, five
native archives, checksums, three CycloneDX SBOMs, separate Python/Rust license inventories,
and attestations. Every third-party Action must remain pinned to a reviewed commit SHA.
Attestations supplement artifact inspection; they do not prove an artifact safe. Demo media for
the release page is attached through the GitHub release UI by the maintainer and is never
committed to the repository or the archives.

## Post-release and recovery

Install the published version into clean Cargo and Python environments. Verify `aise --version`,
`aise package status`, a typed search, Python imports, MCP startup/EOF/cancellation, and database
compatibility against an index written by the previous release.

If a stage fails:

- Before any registry publication, fix the cause, rerun the full gate, and create a new tag only
  if the immutable tag or artifacts changed.
- If crates.io succeeded and PyPI failed, rerun only the failed jobs from the same workflow when
  the verified artifacts are unchanged.
- If both registries succeeded and GitHub Release failed, rerun only the release job from the
  same workflow.
- If any published artifact must change, publish a new version. Never replace an immutable
  registry version.

Record the failing job, artifact hashes, registry state, affected targets, user impact, and the
regression test that prevents recurrence.
