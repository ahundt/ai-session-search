# AI Session Search agent guidance

Read by every tool following the `AGENTS.md` convention. `CLAUDE.md` imports it,
so change it here only.

Rust first: logic, checks, and tooling belong in the crate, and Python carries only
the binding surface and the tests that must exercise it.

## Setup

Rust 1.88 or newer, CPython 3.12 through 3.14 with the standard GIL (3.12 is the floor;
3.12–3.14 are what CI tests), and uv.

```bash
uv sync --locked --all-extras
uv run maturin develop --uv    # abi3 extension into the venv; pytest fails without it
```

## Build

```bash
./run_ci_local.sh                                       # host-platform gate, before proposing a commit
cargo test -p ai-session-search <test name>             # focused Rust
uv run pytest tests/<file>.py -k <test name>            # focused Python
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
uv run ruff check . && uv run mypy ai_session_search tests
```

The gate runs on one machine, so a green run proves the platform you ran it on. macOS, Windows,
and Linux, on x86_64 and arm64 across CPython 3.12 to 3.14, are exercised only by the required CI
matrix, which has caught what a green local gate did not: a test building a `file://` URL by
string interpolation passed everywhere paths start with a separator and failed on all three
Windows jobs, where the drive letter parses as the URL host instead.

Run the gate with no environment prefix. Never prefix
`AI_SESSION_SEARCH_RUSTC_WRAPPER=`: it exports an empty `RUSTC_WRAPPER` and
disables sccache. Use it only when an inherited wrapper is broken. Before step one
the gate prints the wrapper it resolved and whether incremental compilation is on;
read that line before concluding a build is slow for another reason.

Measured: clean full gate ≈ 4 min compiling, ≈ 2.5 GB `target/`; workspace
`cargo check` after one edit ≈ 9s. Raising `-j` does not help — one 3.5 MB crate
dominates and its type and borrow checking are serial. Cache reuse is what helps.
[sccache](https://github.com/mozilla/sccache/blob/main/docs/Rust.md) cannot cache
incrementally compiled crates, which is why `.cargo/config.toml` sets
`incremental = false`, and it never caches crates that invoke the system linker,
so the `aise` binary and the `_native` cdylib relink locally every time.

## Disk

Cargo never garbage-collects `target/`. It reached 78 GB here against a 2.5 GB
working set, mostly artifacts left by old toolchains and feature sets.
`incremental = false` in `.cargo/config.toml` stops the largest single
contributor from returning; old toolchains need sweeping. sccache holds a second
cache outside `target/` under its own ceiling, 10 GiB by default.

```bash
cargo sweep --installed    # drop artifacts from uninstalled toolchains
cargo sweep --time 30      # drop artifacts untouched for 30 days
cargo clean                # ≈ 4 min to rebuild; cheap, not a last resort
```

## Verification

Reproduce with the smallest failing test at the shared typed layer, then cover
every adapter reached: Rust, PyO3, Python, CLI, MCP, schemas, docs, examples,
provider fixtures, packaging. State what you measured and the command that
measured it. Mark inferences as inferences and name the evidence they rest on.
Benchmarks: use `scripts/benchmark_release.py` with `--fixture generated` and a `smoke`, `subsystem`, or `release` tier, then compare baseline/candidate JSONL with `scripts/render_benchmark_report.py`.

Before altering public behavior, read the entries that touch your change in the
[maintainer requirements](docs/development/maintainer-requirements-and-design-decisions.md).
Setup and review: [CONTRIBUTING.md](CONTRIBUTING.md).

## Commits

One concern per commit, so a single change can be reverted on its own. A subject
names the file or component and the behavior, and the body says what changed, why,
and how you verified it:

```
mcp_server.rs: return next_offset when evidence is truncated
```

## Search semantics

Message-search limits are surface-specific: Rust, CLI, and Python preserve all
literal/regex/no-text matches when no operation, purpose, or call limit applies;
MCP alone supplies a bounded default, fuzzy always requires a finite page, and
presentation bounds never change hit membership.

## Client limits

MCP clients cap what the server emits, mostly silently — Codex deletes every
schema description past 5,000 sanitized bytes with no marker — so after touching
MCP schemas or descriptions run `aise mcp schema-budget`, re-pin
`EMITTED_ARTIFACT_CEILINGS` in the same commit that moves a measurement, and
track a client's own change to its cap via `[mcp.client_limits]`; the caps and
their evidence live in
[MCP client limits](docs/development/mcp-client-limits-and-measured-evidence.md).

## Attribution

Source files carry REUSE 3.3 SPDX headers. New files get
`SPDX-FileCopyrightText: 2026 Andrew Hundt` and
`SPDX-License-Identifier: Apache-2.0`, after any shebang. Removing a holder needs
that contributor's agreement; see [provenance](docs/migration/provenance.md).
