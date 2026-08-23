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
release means setting all eight declarations to the new version in one commit and tagging that
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
| `docs/development/library-api.md` | the `ai-session-search = { version = "…" }` snippet |

Cargo resolves a stale `X.Y.Z-rc.N` requirement against a newer core crate without complaint,
so `cargo check --locked` cannot report either dependency drifting. The metadata gate is the
only check that does; run it before creating a tag:

```bash
uv run python -m scripts.verify_release_metadata --tag vX.Y.ZrcN
```

What is published is recorded by the registries and by `git tag`, never by this file: `git tag
--list 'v*'`, `https://crates.io/crates/ai-session-search`, and
`https://pypi.org/project/ai-session-search/`. The working tree declares the next candidate in
the eight locations above.

## Toolchain and compatibility

Releases require Rust at or above the workspace `rust-version` (1.88 at the time of writing;
`Cargo.toml` is authoritative) and CPython 3.12 through 3.14 with the standard GIL enabled. Wheels use `cp312-abi3`.
Free-threaded CPython is not supported. The distribution exposes one executable, `aise`, and
MCP clients run `aise mcp serve`.

1.0.0 is the first public compatibility baseline; the former private single-user package does
not define the public compatibility contract. Release tools (uv, cargo-cyclonedx, cargo-deny)
are pinned by version in `.github/workflows/ci.yml`, `prepare-packages.yml`, and `publish.yml`;
change them only in a separate reviewed toolchain change.

Behavior changes that ship in a release are described in [CHANGELOG.md](CHANGELOG.md), not here.
Accumulate them under `## [Unreleased]` as they land, so the section is written by the people who
made the changes rather than reconstructed from history at tag time. The release notes for a
version are that version's changelog section: the metadata gate refuses a tag the changelog does
not describe, and publishes the section it validated as the GitHub Release body.

## Release blockers

Registration is done once and holds for every later release. Confirm rather than redo:

- The crates.io crate exists and its trusted publisher is registered for repository
  `ahundt/ai-session-search`, workflow `publish.yml`, environment `crates-io`:
  `https://crates.io/crates/ai-session-search/settings`.
- The PyPI trusted publisher is registered for the same repository, workflow, and environment
  `pypi` (a pending publisher until the first upload creates the project, an ordinary project
  publisher afterwards). The settings page re-prompts for the account password. Once a version
  exists, the registry serves the identity it accepted and reading that needs no password:
  `https://pypi.org/integrity/ai-session-search/<version>/<file>/provenance` returns the recorded
  `repository`, `workflow`, and `environment`.
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
| crates.io | Implicit in the SemVer spelling `X.Y.Z-rc.N` | the command below, reading `max_stable_version` |

crates.io answers its API with HTTP 403 and a [data access policy](https://crates.io/data-access)
error unless the request identifies its caller, and curl's default `User-Agent` does not, so name
the caller:

```bash
curl -s -H 'User-Agent: ai-session-search release check (https://github.com/ahundt/ai-session-search)' \
  https://crates.io/api/v1/crates/ai-session-search
```

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

That division holds for platforms too: the gate runs on one machine and proves that machine.
macOS, Windows, and Linux, on x86_64 and arm64 across CPython 3.12 to 3.14, are exercised only by
the required CI matrix. A green local gate is the precondition for pushing; the green CI run on
the exact commit being released is the precondition for tagging.

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
- `CHANGELOG.md` carries this version: rename `## [Unreleased]` to `## [X.Y.ZrcN] - YYYY-MM-DD`
  with the tag's date, add a fresh empty `## [Unreleased]` above it, and point the link
  definitions at the new tag. Anything a user has to do when upgrading belongs first in that
  section, because the whole section is published as the release body. The metadata gate rejects
  a missing, duplicate, undated, impossible-date, or empty section, and `--notes-out FILE` writes
  the body it would publish.
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
- The wheel's embedded SBOM (`.dist-info/sboms/*.cyclonedx.json`, written by maturin) names
  the workspace crates as `workspace:<relative path>`. maturin records them as
  `path+file://<checkout>/...`, so an unrewritten wheel carries the directory it was built in
  (the runner's checkout in the first published release, the maintainer's home locally).
  `scripts/sanitize_sboms.py` rewrites the wheel in place, both from `scripts.prepare_packages`
  and in the `wheels` job of `publish.yml`, and refuses a path outside the checkout;
  `scripts.verify_release_artifacts` then rejects any wheel that still holds `path+file://`.

## TestPyPI rehearsal

crates.io has no test registry, so only the Python half can be rehearsed. The tag push rehearses
it automatically: `publish-testpypi` runs on both triggers and `publish-crate` waits for it, so a
rejected wheel tag or unrenderable metadata stops the release while every registry version is
still repairable. The 1.0.0rc2 tag push is why it works that way, because the job was
dispatch-only then and reported `skipped` while the crate went to crates.io.

The job uploads the verified wheels and sdist, then installs the release from TestPyPI alone and
runs `aise --version`, because an accepted upload is a weaker property than an installable wheel.
`skip-existing` keeps a tag push working after a dispatch already rehearsed that version.

TestPyPI is a separate account from PyPI and needs its own publisher, carrying the same project,
owner, and workflow values but environment `testpypi`. It follows the same lifecycle as PyPI's:
pending until the first upload creates the project, an ordinary project publisher afterwards.
Once a version exists, confirm it by reading the identity TestPyPI recorded, which needs no
password: `https://test.pypi.org/integrity/ai-session-search/<version>/<file>/provenance`.

To register one the first time: TestPyPI re-prompts for the account password before accepting
publisher changes, and a submission made after that window lapses is discarded without an error,
so confirm the publisher appears under **Pending publishers** before continuing.

To rehearse before tagging, or to rehearse a ref on its own,
`gh workflow run publish.yml --ref <ref>` reuses the same build and verification pipeline and
stops after TestPyPI. The same install check runs inside the job; to repeat it by hand:

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

`publish-crate`, `publish`, and `release` are gated on `github.event_name == 'push'`, and those
are the only triggers, so a dispatch stops at TestPyPI and can never reach a production registry.
`publish-testpypi` carries no event gate, which is what makes the rehearsal part of a release
rather than a step beside it.

The GitHub provenance attestation in the `verify` job stays gated on `push`, unlike the rehearsal.
`actions/attest-build-provenance` has no dry-run — `push-to-registry` only controls registry
push and `create-storage-record` only controls artifact metadata — so every invocation signs an
attestation into the repository's list, and there is no API to remove one. A rehearsal would
leave a permanent entry for a release that never happened, and because `SOURCE_DATE_EPOCH` pins
the build clock, it would name the same subject digests the real release attests later.

The rehearsal still covers what that step depends on. `verify_release_artifacts --release-set`
runs on every trigger against the same `dist/*` the attestation would take as its subject, so a
missing or unexpected artifact fails the dispatch. The TestPyPI publish keeps `attestations:
true`, so PEP 740 attestations are signed through the same OIDC identity and land on the
disposable test registry. A repository-contract test asserts the `verify` job still declares
`id-token: write` and `attestations: write`, which is the one precondition the gate would
otherwise hide until a real tag push.

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
   commit, rewriting each wheel's embedded SBOM to `workspace:` references, and then requiring
   that SBOM to record the exact pinned clock and no `path+file://` path, so a manylinux
   container that never received the pin, or a wheel that skipped the rewrite, fails the job
   instead of shipping;
3. installs and tests the exact artifacts on their target runners;
4. verifies the complete artifact set, writes `SHA256SUMS`, and creates GitHub build-provenance
   attestations;
5. uploads the verified wheels and sdist to TestPyPI and installs the release from that index
   alone, so an unusable distribution stops the release before any immutable version exists.
   The `testpypi` environment has no approval rule, so this adds no pause;
6. reproduces the attested crate, then compares the registry's recorded sha256 for this
   version before requesting short-lived crates.io credentials. An absent version publishes; a
   version already carrying the attested checksum is skipped so a retry reaches the remaining
   jobs; a version carrying different bytes fails, because the tag would otherwise try to
   replace an immutable release;
7. pauses at `crates-io`, publishes through OIDC, then pauses at `pypi` and publishes the
   verified wheel/sdist set with PyPI attestations;
8. pauses at `release` and creates the GitHub release (marked pre-release for an `rcN` tag)
   from the same verified artifacts.

Approve protected environments only in that order. The `needs` chain enforces it, so an
out-of-order approval is impossible, but the order still decides what has already been published
when you inspect a later pause. Do not rebuild or replace an artifact between stages. Every
third-party Action must remain pinned to a reviewed commit SHA. Attestations supplement artifact
inspection; they do not prove an artifact safe.

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
