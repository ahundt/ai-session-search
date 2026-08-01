# AI Session Search agent guidance

`CLAUDE.md` imports this file. Change it here only.

## Build

```bash
./run_ci_local.sh                                      # full gate, before proposing a commit
cargo test -p ai-session-search <name>                 # focused Rust
uv run pytest tests/<file>.py -k <name>                # focused Python
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
uv run ruff check . && uv run mypy ai_session_search tests
```

Never prefix `AI_SESSION_SEARCH_RUSTC_WRAPPER=`. It exports an empty `RUSTC_WRAPPER`
and disables sccache; use it only when an inherited wrapper is broken. The gate
prints the wrapper it resolved before step one.

Measured: clean full gate ≈ 4 min compiling, ≈ 2.5 GB `target/`; workspace
`cargo check` after one edit ≈ 9s. Raising `-j` does not help — one 3.5 MB crate
dominates and its type/borrow checking is serial.

## Disk

Cargo never garbage-collects `target/`. It reached 78 GB here against a 2.5 GB
working set, mostly artifacts from old toolchains and feature sets.
`.cargo/config.toml` sets `incremental = false`, which removed the largest single
contributor and is also required for sccache to cache workspace crates.

```bash
cargo sweep --installed    # drop artifacts from uninstalled toolchains
cargo sweep --time 30      # drop artifacts untouched for 30 days
cargo clean                # ≈ 4 min to rebuild; cheap, not a last resort
```

## Verification

Reproduce with the smallest failing test at the shared typed layer, then cover
every adapter reached: Rust, PyO3, Python, CLI, MCP, schemas, docs, examples,
fixtures, packaging. State what you measured and the command that measured it.
Mark inferences as inferences.

Contracts: [maintainer requirements](docs/development/maintainer-requirements-and-design-decisions.md).
Setup and review: [CONTRIBUTING.md](CONTRIBUTING.md).

## Search semantics

Message-search limits are surface-specific: Rust, CLI, and Python preserve all
literal/regex/no-text matches when no operation, purpose, or call limit applies;
MCP alone supplies a bounded default, fuzzy always requires a finite page, and
presentation bounds never change hit membership.

## Attribution

Source files carry REUSE 3.3 SPDX headers. New files get
`SPDX-FileCopyrightText: 2026 Andrew Hundt` and
`SPDX-License-Identifier: Apache-2.0`, after any shebang. Removing a holder needs
that contributor's agreement; see [provenance](docs/migration/provenance.md).
