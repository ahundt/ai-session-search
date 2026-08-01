# Releasing Rust and Python packages

This document is the maintainer contract for building, testing, and releasing
`ai-session-search`. It covers the public Rust crate and native executable, the
PyO3 Python distribution, and the GitHub release archives. It does not grant
permission to publish a release; a maintainer must approve the protected
`crates-io`, `pypi`, and `release` environments.

## One release identity, ecosystem-native versions

Use one release identity with the canonical spelling required by each package
ecosystem. For release candidate 1 of version 1.0.0, declare:

| Surface | Version |
|---|---|
| `pyproject.toml`, Python artifacts, and Git tag | `1.0.0rc1` and `v1.0.0rc1` |
| Cargo manifests, Rust dependencies, and `.crate` artifact | `1.0.0-rc.1` |

Do not force one spelling into the other ecosystem. Python release candidates
use the normalized `X.Y.ZrcN` form defined by the
[PyPA version specifier specification](https://packaging.python.org/en/latest/specifications/version-specifiers/).
Cargo requires SemVer prerelease syntax after `-`, as documented by the
[Cargo manifest reference](https://doc.rust-lang.org/cargo/reference/manifest.html#the-version-field).
`scripts/release_versions.py` is the sole mapping implementation, and
`python -m scripts.verify_release_metadata --tag vX.Y.ZrcN` rejects mismatched
manifests, dependencies, or tags before packaging starts.

## Prepare and release in order

1. Confirm the intended version, supported targets, MSRV, and CPython range in
   the manifests and [distribution matrix](#supported-distribution-matrix).
2. Run the complete local gate from a clean commit:

   ```bash
   ./run_ci_local.sh
   ```

3. Prepare and verify the complete local package set. The destination must not
   already exist, which prevents stale artifacts from entering the result:

   ```bash
   uv run python -m scripts.prepare_packages
   ```

4. Use `--package rust` or `--package python` only for registry-specific
   diagnosis. Use `--output-dir PATH` when several immutable candidate sets
   must coexist; never merge their directories.
5. Optionally dispatch **Prepare package artifacts** in GitHub Actions with its
   default `all` scope. Confirm all five wheel targets, the sdist, and the Rust
   crate complete without publish credentials.
6. Create the release tag only after `scripts.verify_release_metadata`
   confirms the Cargo, Python, dependency, and tag versions represent the same
   release identity.
   The tag-triggered `publish.yml` reruns the full gate, builds each artifact
   once, and pauses at protected environments before irreversible publication.
7. Review artifact membership, hashes, SBOMs, attestations, and exact-artifact
   installation results in that workflow run. Approve `crates-io`, then `pypi`,
   then `release`; do not rebuild or substitute files between stages.
8. Verify the published crate, Python distributions, native archives, checksums,
   and release notes. If any stage fails, use [Partial release
   recovery](#partial-release-recovery) rather than deleting or replacing a
   published version.

No preparation command publishes, edits MCP clients, changes user data, or
purges shared Cargo, uv, or compiler caches.

## Supported distribution matrix

| Surface | Required targets |
|---|---|
| Python runtime | Standard GIL-enabled CPython 3.12, 3.13, and 3.14 |
| Python wheels | manylinux2014 x86_64/aarch64, macOS x86_64/arm64, Windows x86_64 |
| Native archives | Linux x86_64/aarch64, macOS x86_64/arm64, Windows x86_64 |
| Rust | The workspace MSRV plus current stable Rust |

Every pull request runs the complete Rust workspace and static-analysis gate on Linux, plus
`cargo test -p ai-session-search --all-targets --locked` on native macOS and Windows runners.
This exercises platform-specific filesystem, process, executable-discovery, installer, and path
branches before merge without repeating Linux Clippy, rustfmt, rustdoc, Ruff, or mypy work. The
Python matrix separately imports and tests CPython 3.12 through 3.14 on Linux, macOS, and Windows,
with Linux ARM64 and macOS x86_64 additions. Release workflows remain responsible for installing
and smoke-testing each exact wheel and native archive on its target runner.

The Linux ARM64 runner is currently a GitHub public-preview runner. Treat loss
of that hosted label as an infrastructure failure, not evidence that ARM64 may
be silently dropped. The current labels and architectures are defined in the
[GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

PyO3's stable ABI feature makes one wheel per OS/architecture usable on all
supported CPython versions. CI still imports and exercises the extension on
3.12, 3.13, and 3.14 because ABI compatibility does not prove behavioral or
typing compatibility. Free-threaded CPython is not implied by `abi3` and must
not be advertised until it has a separate build and concurrency test matrix.
See [PyO3 building and distribution](https://pyo3.rs/latest/building-and-distribution.html).

## One build, exact-artifact verification

The release workflow follows this sequence:

1. Run the complete reusable CI workflow at the tagged commit.
2. Require the Git tag, Python version, and Rust crate version to represent the
   same release identity using the ecosystem-native spellings above.
3. Build each wheel and native archive once on its matching native runner.
4. Build one sdist and generate locked Rust/Python SBOMs.
5. Install and exercise each exact artifact outside the source environment.
6. Aggregate the artifacts, reject missing or unexpected members, hash them,
   and attest the same files that will be published.
7. Publish the Rust crate, then the Python distributions, then create the
   GitHub release from the verified artifacts. Do not rebuild in publish jobs.

Maturin documents that portable Linux wheels require a manylinux container or
Zig and that `manylinux: 2014` makes `maturin-action` enforce the corresponding
policy: [Maturin build and manylinux guidance](https://www.maturin.rs/).
The uv packaging guide recommends testing both the wheel and sdist and supports
installing an exact local artifact in an isolated environment:
[uv package guide](https://docs.astral.sh/uv/guides/package/) and
[uv GitHub Actions guide](https://docs.astral.sh/uv/guides/integration/github/).

The preparation command used in the ordered workflow defaults to the complete
Cargo and Python set. Narrower scopes are explicit maintainer diagnostics:

```bash
# Registry-specific maintainer diagnostics
uv run python -m scripts.prepare_packages --package rust --output-dir dist/rust-packages
uv run python -m scripts.prepare_packages --package python --output-dir dist/python-packages
```

The output directory must not already exist, so artifacts from different
versions cannot mix. Builders stage beside the destination and publish the
verified directory with one atomic rename. Failures remove staging but never
delete Cargo, uv, or compiler-wrapper caches. The process inherits
`RUSTC_WRAPPER`, `CARGO_TARGET_DIR`, uv/Python selection, and platform settings;
override them in the environment only when intentional. It defaults
`CARGO_INCREMENTAL=0` only when unset because incremental Rust compilations are
not cacheable by sccache; an explicit caller value still wins. This preserves
`RUSTC_WRAPPER=sccache` and enables cache reuse without requiring sccache or
managing its global cache. A resolvable bare wrapper name is passed to Cargo as
its absolute executable path, avoiding child-process lookup ambiguity while
leaving explicit wrapper paths unchanged. An unset `CARGO_TARGET_DIR` resolves
to the repository's shared `target`; the copied `.crate` and its extracted
verification tree are removed after atomic publication so repeated preparation
does not accumulate duplicate package trees. See the
[official sccache Rust requirements](https://github.com/mozilla/sccache/blob/main/docs/Rust.md).
A Python preparation invokes maturin through the locked uv environment and
builds in the shared Cargo target rather than recompiling an extracted sdist in
a disposable target. The full quality gate separately installs and exercises
the sdist, preserving source-distribution completeness coverage without paying
that duplicate compilation cost for every local preparation.
An ABI3 local wheel matches an explicit Cargo/maturin target when configured,
otherwise the Rust host target; maturin does not require a build interpreter for
this ABI. The GitHub preparation workflow uses explicit native runners and
manylinux2014 for the five publishable wheel targets.

Run **Prepare package artifacts** manually in GitHub Actions to prepare `all`
(the default), `rust`, or `python` without acquiring registry credentials or
publishing. Tag-triggered `.github/workflows/publish.yml` remains the only full
release authority and still defaults to the coordinated crate, PyPI, native
archive, provenance, and GitHub release pathway.

This separation follows the official guidance to keep build artifacts outside
the minimal OIDC publishing job: [PyPI trusted-publisher security model](https://docs.pypi.org/trusted-publishers/security-model/),
[GitHub workflow artifacts](https://docs.github.com/en/actions/concepts/workflows-and-actions/workflow-artifacts),
[uv builds and publishing](https://docs.astral.sh/uv/guides/package/),
[maturin distribution and manylinux](https://www.maturin.rs/distribution.html),
and [Cargo package/publish dry runs](https://doc.rust-lang.org/cargo/reference/publishing.html).

`cargo package` is the non-publishing equivalent of `cargo publish --dry-run`.
Inspect `target/package/*.crate`; crates.io rejects crates larger than 10 MB.
See [Cargo publishing](https://doc.rust-lang.org/cargo/reference/publishing.html).

## Installation contracts

The following are distinct supported user pathways and must consume a built
artifact in CI rather than the source checkout:

```bash
uv tool install ai-session-search
uvx --from ai-session-search aise --help
uv add ai-session-search
python -m pip install ai-session-search
cargo install ai-session-search --locked
```

`uv` is the preferred Python project and tool manager. `pip` remains supported
because it is Python's baseline installer. Cargo path and Git installs are
tested separately from the packaged-crate install so missing manifest files or
accidental workspace dependencies fail before a release.

Exactly one package manager or native archive should own the global `aise`
command. A Python project dependency may coexist, but CI and documentation must
not recommend simultaneous Cargo and uv global commands because PATH order can
select different versions. Package installation must not mutate MCP configs,
managed Markdown, hooks, indexes, configuration, or session data. MCP setup and
removal remain explicit `aise integrations install`/`aise integrations uninstall`
operations. See
[uv tool ownership](https://docs.astral.sh/uv/concepts/tools/) and
[`cargo install`](https://doc.rust-lang.org/cargo/commands/cargo-install.html).

## Trusted publishing and provenance

Do not store long-lived PyPI or crates.io tokens in GitHub secrets.

1. Protect the `pypi`, `crates-io`, and `release` GitHub environments with
   required maintainer approval.
2. Register `.github/workflows/publish.yml` and environment `pypi` as the PyPI
   Trusted Publisher. PyPA explicitly recommends manual approval for this
   environment: [PyPA GitHub publishing guide](https://packaging.python.org/en/latest/guides/publishing-package-distribution-releases-using-github-actions-ci-cd-workflows/).
3. Publish the first Rust crate release manually, then register the repository,
   workflow, and `crates-io` environment as its trusted publisher. Subsequent
   jobs exchange GitHub OIDC identity for a short-lived token that is revoked
   after the job: [crates.io Trusted Publishing](https://crates.io/docs/trusted-publishing)
   and [official authentication action](https://github.com/rust-lang/crates-io-auth-action).
4. Keep every third-party action pinned to a full commit SHA. Review and update
   pins deliberately; never replace them with a mutable branch or major tag.
5. Keep `id-token: write` and `attestations: write` scoped to the individual job
   that needs them. Checkout credentials must not persist in build jobs.

The PyPA publisher creates PEP 740 attestations by default. GitHub provenance
attestations bind the remaining artifacts to the workflow and commit. See
[PyPI attestations](https://docs.pypi.org/attestations/producing-attestations/)
and [GitHub artifact attestations](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations).

## Partial release recovery

PyPI, crates.io, and GitHub do not provide a cross-registry transaction. The
workflow orders irreversible operations so a GitHub release is never announced
before both package registries accept the version.

| Failure | Maintainer action |
|---|---|
| Before either registry publishes | Fix the cause, delete the unpublished tag locally/remotely only with explicit approval, bump if artifacts changed, and restart. |
| Rust published, PyPI failed | Re-run failed jobs in the same workflow run. Do not re-run the successful crates.io job. If artifacts must change, bump the version; registry files are immutable. |
| Both registries published, GitHub release failed | Re-run only the failed release job; it downloads the already verified artifacts from the same run. |
| Published artifact is unsafe or unusable | Yank the affected crate/release files where supported, publish a corrected patch version, and add a regression test. Never replace an immutable artifact under the same version. |

Record a post-mortem with the failing job, artifact hashes, affected targets,
registry state, user impact, and the exact prevention test. A yanked release is
still part of history and must remain documented.

## Configuration lifecycle contract

All entry points must eventually use one typed Rust resolver with this strict
precedence:

```text
explicit CLI/API argument > environment variable > config file > built-in default
```

An omitted value differs from an empty or invalid value. Empty paths and invalid
numbers must produce actionable errors rather than silently selecting another
source. Runtime-derived paths and CPU/thread counts remain typed functions, not
literal paths or machine-specific values embedded in the binary. An embedded
example config is useful documentation; an embedded default config is acceptable
only if it replaces, rather than duplicates, Rust defaults and a test proves the
serialized example, parser, CLI help, MCP schemas, and API defaults cannot drift.

Any future `--config`, setup, or migration flag must be available through the
shared resolver where the surface supports it. Config writes must be atomic,
preserve unrelated keys, use scoped locks, clean temporary files on error, and
offer provenance diagnostics that identify the winning source without printing
secret values.

## Standard tooling decision record

Prefer maintained ecosystem tools when they remove project-specific code and
make failures more actionable. Do not layer two tools over the same responsibility.

| Tool | Decision | Reason and re-evaluation trigger |
|---|---|---|
| `uv` | Adopted | One lock, Python provisioning, isolated execution, builds, and uv/pip-compatible install tests. Dependabot has a native `uv` ecosystem: [uv Dependabot guidance](https://docs.astral.sh/uv/guides/integration/dependabot/). |
| `maturin-action` | Adopted | Direct PyO3/abi3 and manylinux support with one wheel per platform/architecture. |
| `cibuildwheel` | Not added | It solves broad per-interpreter wheel matrices, but would duplicate the current abi3 maturin matrix. Re-evaluate for free-threaded, PyPy, musllinux, or additional Python ABI wheels: [cibuildwheel](https://cibuildwheel.pypa.io/en/latest/). |
| `cargo-deny` | Adopted | Central license, source, and duplicate-dependency policy for the Rust graph. |
| `actionlint` | Adopted locally | Fast workflow syntax and expression validation. Keep it in the local release gate. |
| `zizmor` | Adopted | Adds GitHub Actions security diagnostics beyond syntax checking. Its exact version is pinned and it runs offline so CI does not require a broad GitHub token: [zizmor](https://docs.zizmor.sh/). |
| `cargo-semver-checks` | Planned after the first public baseline | Detects public Rust API changes. A first major release has no valid registry baseline, so do not manufacture one: [cargo-semver-checks](https://docs.rs/crate/cargo-semver-checks/latest). |
| `cargo-binstall` | Planned after repository URLs stabilize | Can install the existing native archives without compilation. Add manifest metadata only when release URLs and signature policy are final: [cargo-binstall](https://github.com/cargo-bins/cargo-binstall). |
| `cargo-dist` | Evaluation only | It may replace native archive, installer, checksum, and GitHub release code. Adopt only if an experiment preserves Python artifacts, SBOMs, exact-artifact tests, attestations, and protected registry ordering while deleting custom code: [cargo-dist](https://opensource.axo.dev/cargo-dist/). |
| `release-plz` | Not added | Its release PR/tag/crates.io authority overlaps the protected mixed-registry workflow and requires broader repository permissions. Re-evaluate only if release volume makes manual version preparation the dominant failure source: [release-plz](https://release-plz.dev/). |

`.github/dependabot.yml` requests weekly, reviewable updates for the `uv`, Cargo,
and GitHub Actions ecosystems. It does not auto-merge. Every update must pass the
same runtime, exact-artifact, license, and workflow gates as a human-authored
change. GitHub documents SHA-aware Actions updates when version comments are on
the same line: [Dependabot supported ecosystems](https://docs.github.com/en/code-security/reference/supply-chain-security/supported-ecosystems-and-repositories).

## Reference projects and review cadence

These mature projects are architectural references, not dependencies or exact
templates:

- [Polars](https://github.com/pola-rs/polars) demonstrates a Rust core with
  language bindings, platform wheel variants, and performance-focused APIs.
- [Pydantic](https://github.com/pydantic/pydantic) demonstrates keeping a Rust
  validation core and Python API in one lifecycle after merging `pydantic-core`.
- [PyCA cryptography](https://github.com/pyca/cryptography) demonstrates a
  long-running security-sensitive Rust/Python wheel and release matrix.

Before every release:

1. Check supported Python, Rust MSRV, uv, PyO3, maturin, runner labels, and action
   pins against their primary documentation.
2. Review dependency licenses, SBOMs, crate contents, wheel contents, executable
   startup behavior, MCP initialization, and exact install pathways.
3. Compare CI and release matrices so every published target has pre-merge or
   tag-gated native execution evidence.
4. Run failure injection for interrupted config/index writes, stale locks,
   read-only state directories, full disks, malformed config, and terminated
   subprocesses; verify scoped resources and temporary files are cleaned up.
5. Record measured latency and memory regressions against the prior release.

Source links in this document were reviewed on 2026-07-13. Re-check them when
tool versions or hosted runner labels change; do not treat a copied workflow as
permanent evidence of current best practice.
