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

Keep all seven declarations aligned. The consumer crate stays unpublished at
`0.0.0`; only its requirement on the released core crate carries the release
version:

| Location | Field | Required RC1 value |
| --- | --- | --- |
| `pyproject.toml` | `project.version` | `1.0.0rc1` |
| `rust/ai-session-search-core/Cargo.toml` | `package.version` | `1.0.0-rc.1` |
| `rust/ai-session-search-python/Cargo.toml` | `package.version` | `1.0.0-rc.1` |
| `rust/ai-session-search-python/Cargo.toml` | `dependencies.ai-session-search.version` | `1.0.0-rc.1` |
| `tests/rust-api-consumer/Cargo.toml` | `dependencies.ai-session-search.version` | `1.0.0-rc.1` |
| `skills/ai-session-search/SKILL.md` | `metadata.version` | `1.0.0-rc.1` |
| `rust/ai-session-search-core/skills/ai-session-search/SKILL.md` | `metadata.version` | `1.0.0-rc.1` |

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

- The crates.io account exists and its email is verified. The crate name itself
  is claimed after the tag rather than before it, and its trusted publisher is
  registered after that; see the name bootstrap below.
- The pending PyPI trusted publisher matches project `ai-session-search`,
  repository `ahundt/ai-session-search`, workflow `publish.yml`, and
  environment `pypi`.
- GitHub environments `crates-io`, `pypi`, and `release` have the intended
  maintainers and approval rules.
- The `release-tags` ruleset is active over `refs/tags/v*`:
  `gh api repos/ahundt/ai-session-search/rulesets`.
- The exact RC1 commit passes the local gate and package preparation below.
- The message-search response shape is the one you intend to publish. Until this
  tag exists, `MESSAGE_SEARCH_RESPONSE_SCHEMA_VERSION` stays at 1 because an
  increment signals a consumer that does not exist; afterwards, removing a field,
  renaming one, changing a type, or changing what a value means requires an
  increment across the serializer, the closed MCP `outputSchema`, the Python
  stubs, and every fixture. See `REQ006-report-extent-honestly` in
  `docs/development/maintainer-requirements-and-design-decisions.md`. Reshaping
  is cheap before this line and expensive after it.

These are release blocking because the workflow publishes in the order
crates.io, PyPI, then GitHub Release. Registry versions are immutable.

## One-time account and publisher setup

### crates.io

1. Sign in to crates.io with GitHub, provide and verify the account email,
   and create a short-lived API token.
2. Run `cargo login` and enter that token. Cargo stores it in
   `~/.cargo/credentials.toml`.
3. Use the token only for the name bootstrap below.
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

## One-time crates.io name bootstrap

crates.io requires the crate to exist before its trusted publisher can be
registered, and it has no pending-publisher equivalent to PyPI's, so exactly one
manual publish is unavoidable; see
[RFC 3691](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html).

Publish the release version itself. A placeholder such as `1.0.0-rc.0` would
consume a version number nobody installs and invite a yank that is not
warranted, and it is unnecessary: the `crate` job builds the crate twice and
requires `cmp` to pass, so `cargo package` output is deterministic, and a local
`cargo package --locked --no-verify` at the tagged commit reproduces the
workflow's attested crate byte for byte. Verify that rather than assume it.

The tag workflow still runs end to end afterwards. Its `publish-crate` job
compares the registry's recorded sha256 against the attested crate: a matching
checksum sets `published=true` and skips both the credential request and
`cargo publish`, so `publish` and `release` still run. Differing bytes fail the
job instead of replacing an immutable version.

Because the comparison needs the attested artifact, this happens after the tag,
between the workflow parking at `crates-io` and approving that environment:

1. From the clean tagged checkout, reproduce the crate and confirm it matches
   what the workflow attested:

   ```bash
   git checkout --detach v1.0.0rc1
   gh run download <run-id> --name verified-crate-distribution --dir attested
   cargo package --locked --no-verify -p ai-session-search
   cmp attested/*.crate target/package/ai-session-search-1.0.0-rc.1.crate
   ```

   The detached checkout is load-bearing, not tidiness. `cargo package` writes
   `.cargo_vcs_info.json` into the crate recording the commit it was built from,
   so building from `main` after even a docs-only commit produces a different
   sha256 while every packaged file is identical. Publishing that crate consumes
   the version with bytes the attestation does not cover, `publish-crate` then
   fails the checksum comparison on every retry, and because registry versions
   are immutable and `cargo yank` does not free the number, the only exits are a
   new version or shipping unattested bytes. `cmp` catches this; run it.

2. Only if `cmp` is silent, explicitly authorize and run:

   ```bash
   cargo publish --locked -p ai-session-search
   ```

3. Register the crates.io trusted publisher for repository
   `ahundt/ai-session-search`, workflow `publish.yml`, and environment
   `crates-io`. This release no longer needs it, because `publish-crate` now
   skips, but every later release publishes through it.
4. Revoke the bootstrap token and run `cargo logout`. A token created for this
   bootstrap should not outlive it; a pre-existing general-purpose token is the
   maintainer's to keep, but while one with `publish-new` or `publish-update`
   survives, the crate can still be published outside the workflow and outside
   its approvals.
5. Approve `crates-io`, and confirm the job logs the skip rather than
   publishing a second time.

A release candidate is already a pre-release, so `cargo add ai-session-search`
reports no stable version until 1.0.0 ships. That is the intended behavior, not
a side effect of a placeholder.

## RC1 local gate

The authoritative local gate creates isolated config, cache, and database
state. It quarantines and checksum-restores any source-tree native extension,
so it does not use a real user database:

```bash
./run_ci_local.sh
```

Run it this way. The gate inherits whatever compiler wrapper Cargo is configured
to use, so an installed `sccache` reuses its cache across runs and checkouts.
The gate prints the wrapper it resolved before the first step.

Only when an inherited wrapper is broken in the current environment, override it:

```bash
AI_SESSION_SEARCH_RUSTC_WRAPPER= ./run_ci_local.sh
```

That form exports an empty `RUSTC_WRAPPER`, which turns the wrapper off. The gate
also sets `CARGO_INCREMENTAL=0`, so with no wrapper every run is a cold full
rebuild of a large workspace and takes far longer than a cached one.

The gate checks both lockfiles, builds the current ABI3 extension, runs Ruff,
mypy, stub parity, Python tests, Rust formatting/check/Clippy/tests/doctests,
the release executable and MCP schema, exact wheel and sdist install pathways,
and workflow syntax when `actionlint` is installed. CI runs `actionlint` in the
required `workflow-security` job, so a workflow syntax error blocks the merge
whether or not it was caught locally.

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

## TestPyPI rehearsal

crates.io has no test registry, so only the Python half can be rehearsed. Do it
before approving `pypi`, because a rejected wheel tag or unrenderable metadata
cannot be fixed in place once crates.io has published an immutable version.

Register a pending publisher on TestPyPI, which is a separate account from
PyPI, using the same project, owner, and workflow values as PyPI but
environment `testpypi`. TestPyPI re-prompts for the account password before
accepting publisher changes; a submission made after that window lapses is
discarded without an error, so confirm the publisher appears under **Pending
publishers** before continuing.

`gh workflow run publish.yml --ref v1.0.0rc1` then reuses the same build,
verification, and attestation pipeline and uploads to TestPyPI. Confirm it
installs:

```bash
uv run --isolated --no-project --default-index https://test.pypi.org/simple/ \
  --with ai-session-search==1.0.0rc1 aise --version
```

`--default-index` replaces PyPI rather than adding to it, so the command fails
if anything has to come from the production index. That is the stronger check
and it holds here because every `Requires-Dist` entry in the wheel is gated
behind `extra == 'dev'`, leaving no runtime dependency to resolve. A project
with runtime dependencies absent from TestPyPI needs `--index` and
`--index-strategy unsafe-best-match` instead. `--no-project` keeps this
checkout's own `pyproject.toml` out of the resolution.

`publish-crate`, `publish`, and `release` are gated on `github.event_name ==
'push'` and `publish-testpypi` on `workflow_dispatch`, and those are the only
triggers, so a dispatch can never reach a production registry and a tag push
can never reach TestPyPI. A rehearsal consumes the version on TestPyPI; a
second attempt at the same version needs a new one.

## Tag workflow

Create the annotated tag only after reviewing the exact commit. The tag must
be `v` plus the PEP 440 version from `pyproject.toml`.

The `release-tags` repository ruleset restricts creating, moving, and deleting
`refs/tags/v*` to the repository admin role, so write access alone cannot fire
this workflow, and a tag that named a published artifact cannot later be
repointed at a different commit. The maintainer holds that role and is unaffected.

`publish.yml` then:

1. reruns the reusable CI and metadata gates;
2. builds each wheel, native archive, sdist, and crate once;
3. installs and tests the exact artifacts on their target runners;
4. verifies the complete artifact set, writes `SHA256SUMS`, and creates GitHub
   build-provenance attestations;
5. reproduces the attested crate, then compares the registry's recorded sha256
   for this version before requesting short-lived crates.io credentials. An
   absent version publishes; a version already carrying the attested checksum
   is skipped so a retry reaches the remaining jobs; a version carrying
   different bytes fails, because the tag would otherwise try to replace an
   immutable release;
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
