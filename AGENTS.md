# AI Session Search agent guidance

Read by every tool following the `AGENTS.md` convention. `CLAUDE.md` imports this
file, so this is the one place to change.

## Running builds and the gate

Run the gate as written, with no environment prefix:

```bash
./run_ci_local.sh
```

It inherits whatever compiler wrapper Cargo is configured to use. An installed
`sccache` reuses its cache across runs and checkouts, which is the difference
between minutes and tens of minutes here. The gate prints the wrapper it resolved
and whether incremental compilation is on before the first step; read that line
before concluding a build is slow for some other reason.

Do **not** prefix `AI_SESSION_SEARCH_RUSTC_WRAPPER=` by habit. That exports an
empty `RUSTC_WRAPPER` and disables the wrapper. Combined with the gate's
`CARGO_INCREMENTAL=0`, every run then becomes a cold full rebuild. Use it only
when an inherited wrapper is genuinely broken in the current environment.

For a focused change, run the narrow check rather than the whole gate:

```bash
cargo test -p ai-session-search <test name>
uv run pytest tests/<file>.py -k <test name>
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
uv run ruff check . && uv run mypy ai_session_search tests
```

Reserve the full gate for a clean commit you are about to propose or release.

`ai-session-search-core` is one large crate. Cargo parallelizes across crates and
codegen units, but a single crate's type checking and borrow checking are largely
serial, so raising `-j` does not shorten a cold build of it. Cache reuse does.

The wrapper and `CARGO_INCREMENTAL=0` work together rather than competing.
[sccache cannot cache incrementally compiled crates](https://github.com/mozilla/sccache/blob/main/docs/Rust.md),
and Cargo enables incremental compilation for workspace members in the dev
profile by default, so turning incremental off is what lets the wrapper cache
this workspace at all. Disabling the wrapper while leaving incremental off is the
worst of both. The same document notes it can never cache crates that invoke the
system linker: `bin`, `cdylib`, `dylib`, and `proc-macro`. The `aise` executable
and the `_native` extension are always relinked locally.

## Disk

`target/` reaches tens of gigabytes and the largest part of it,
`target/debug/deps`, *is* the reuse. Never reach for a blanket `cargo clean` to
free space; that deletes the compiled dependency graph every later build depends
on and guarantees the slow cold rebuild you were trying to avoid.

Reclaim in this order, checking free space first:

1. `target/debug/incremental` — pure incremental state, regenerated on demand.
   The gate sets `CARGO_INCREMENTAL=0` so it never writes this, but any ad-hoc
   `cargo build`/`cargo test` in the same target directory repopulates it, and it
   grows into the tens of gigabytes. Prefix ad-hoc commands with
   `CARGO_INCREMENTAL=0` to keep it from coming back.
2. `target/<triple>/` directories for targets you are not currently building, and
   `target/llvm-cov-target`. These are cross-target and coverage build graphs that
   go stale for weeks between releases.
3. `target/debug/deps` only when you accept a full cold rebuild.

A compiler wrapper trades disk for time: sccache's own cache defaults to a 10 GiB
ceiling on top of `target/`. On a full volume, reclaim stale build graphs before
enabling more caching.

## Verification expectations

Reproduce a defect with the smallest failing test at the shared typed layer
before implementing, then cover every adapter the change reaches: Rust, PyO3,
Python, CLI, MCP, schemas, docs, examples, provider fixtures, and packaging.

State what you measured, with the command and its output. Mark anything inferred
rather than observed as an inference, and say which evidence it rests on.

The cumulative product contracts live in
[docs/development/maintainer-requirements-and-design-decisions.md](docs/development/maintainer-requirements-and-design-decisions.md).
Read the entries that touch your change before altering public behavior.
[CONTRIBUTING.md](CONTRIBUTING.md) covers setup and review expectations.

## Search surface semantics

Message-search limits are surface-specific: Rust, CLI, and Python preserve all
literal/regex/no-text matches when no operation, purpose, or call limit applies;
MCP alone supplies a bounded default, fuzzy always requires a finite page, and
presentation bounds never change hit membership.

## Attribution

Source files carry REUSE 3.3 SPDX headers. New files get
`SPDX-FileCopyrightText: 2026 Andrew Hundt` and
`SPDX-License-Identifier: Apache-2.0`, after any shebang. See
[docs/migration/provenance.md](docs/migration/provenance.md) for when a file
names an additional copyright holder; removing one needs that contributor's
agreement.
