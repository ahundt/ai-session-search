<!--
SPDX-FileCopyrightText: 2026 Andrew Hundt
SPDX-License-Identifier: Apache-2.0
-->

# Installer and configuration maintainer contracts

Rules for people changing the integration installer, configuration resolution, or their
documentation. Nothing here is needed to install or configure `aise`; for that, read
[Installation](installation.md) and [Configuration](configuration.md).

## Installer acceptance

The integration acceptance matrix covers Claude Code/Desktop, ChatGPT Codex desktop and Codex
CLI/IDE, Gemini CLI, **Antigravity App/IDE/CLI**, **Pi**, **Prime Agent**, Cursor, Windsurf, VS
Code, Zed, **OpenCode**, OpenClaw, and KiloCode. Each target must pass
install, content-aware status, byte-idempotent reinstall, dry-run, and ownership-safe uninstall
tests. Pi and Prime Agent acceptance includes strict-JSON MCP registration, MCP-first guidance,
native skill discovery, managed `AGENTS.md`, explicit selection, status, idempotent reinstall,
dry-run, and ownership-safe uninstall.

Maintainers install from a repository checkout with
`sh scripts/install-native.sh --bin-dir DIR`; the packaged archive ships the same script as
`install.sh`.

The canonical repository URL is `https://github.com/ahundt/ai-session-search`. Keep it equal to
`project.urls.Repository` in `pyproject.toml` and `workspace.package.repository` in `Cargo.toml`;
installation documentation must not introduce a second repository identity. When a harness
changes its configuration syntax or security guidance, update the [installation guide](installation.md),
the CI workflow, and the installer acceptance tests in the same commit.

CI checks direct-Git installation without depending on GitHub availability. It
uses the checked-out repository as a `file://` remote and `$GITHUB_SHA` as the
immutable revision, then verifies pip, `uv add`, `uv tool install`, `uvx`, and
`cargo install --git`. The Python acceptance harness is reusable locally:

```bash
uv run --isolated --no-project python scripts/verify_python_install_methods.py \
  --git-url "file://$(git rev-parse --show-toplevel)" \
  --git-rev "$(git rev-parse HEAD)" \
  --source-root "$(git rev-parse --show-toplevel)" \
  --timeout-seconds 600
```

The harness accepts the repository and revision as parameters. It
rejects mutable or abbreviated revisions, insecure HTTP, embedded credentials,
and relative local paths. Temporary virtual environments, tool installations,
configuration, and application caches are scoped to one temporary root and removed
on success or failure. The harness inherits the caller's content-addressed
uv cache and `CARGO_TARGET_DIR`: normal standalone invocations reuse uv's platform cache,
while `run_ci_local.sh` selects the workspace `target` directory. Set either environment
variable explicitly to use a different shared cache; the harness never deletes it.

On `No space left on device`, reclaim with `cargo clean`, or `cargo clean -p <package>`. Never
delete `$CARGO_HOME` or uv's shared cache to recover space.

## Configuration file initialization

`aise config init` uses the shared `durable_fs` staged-file transaction; unpublished staging files
are removed on drop.

## MCP client configuration transactions

The user-visible guarantees (receipt permissions, rollback, recovery, comment preservation, and
rejection of non-UTF-8 executable paths) are listed in
[Configuration](configuration.md#mcp-client-configuration-transactions). They rest on this
mechanism.

The MCP installer uses an advisory lock and a versioned receipt containing exact UTF-8
before/after images and lossless platform path units. It validates every input before the
receipt, revalidates each preimage before publication, atomically publishes each file, and syncs
its parent.

Status holds an `fd_lock` shared guard across the receipt check and every selected
config/instruction read; install, uninstall, and recovery take the corresponding exclusive guard.
Codex TOML edits use `toml_edit::DocumentMut` to remove or replace the semantic
`mcp_servers.ai-session-search` item. Generated TOML is reparsed before the durable transaction
can publish it.

The implementation retains the shared `durable_fs` primitive rather than adding a second atomic-file
crate. Rust's [`OsStrExt`](https://doc.rust-lang.org/std/os/unix/ffi/trait.OsStrExt.html) defines the
lossless Unix byte representation used in receipts, and the formatting-preserving TOML API and
keyed removal contract come from
[`toml_edit::DocumentMut`](https://docs.rs/toml_edit/latest/toml_edit/struct.DocumentMut.html).
Receipts record explicit platform-native units, because the alternatives either change encoding
between releases or reject non-Unicode paths.

## Configuration changes

Configuration changes require focused tests for all four precedence levels, invalid canonical
environment values, canonical/legacy conflicts, omitted versus explicit-empty provider paths,
unknown TOML keys, CLI/MCP/Python parity, and effective-config provenance. Tests must inject
`ConfigEnvironment`; do not mutate process environment in parallel Rust tests.

The embedded `config.example.toml` is documentation, not a second runtime-default source. Typed
Rust defaults remain canonical, and the example contract test must compare each uncommented
tunable with those typed defaults. An embedded default config is acceptable only if it replaces
rather than duplicates those defaults, and only with a test proving the serialized example,
parser, CLI help, MCP schemas, and API defaults cannot drift.

Runtime-derived paths and CPU or thread counts stay typed functions, never literal paths or
machine-specific values embedded in the binary.
