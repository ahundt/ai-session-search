# Installation and source-build guide

This guide defines the supported installation pathways for the
`ai-session-search` Python distribution, Rust crate, and `aise` executable.
Prefer a package registry for normal use. Install from Git only when testing an
unreleased change or when a specific commit is required.

The canonical repository URL is
`https://github.com/ahundt/ai-session-search`. Maintainers must keep it equal to
`project.urls.Repository` in `pyproject.toml` and `workspace.package.repository`
in `Cargo.toml`; installation documentation must not introduce a second
repository identity.

## Install in five steps

1. Choose exactly one owner for the global `aise` executable: `uv tool`
   (recommended), Cargo, pip, or a verified native archive.
2. Run one command from [Registry installation](#registry-installation), or use
   [an immutable Git revision](#install-an-immutable-git-revision) when testing
   unreleased code.
3. Run `aise --version` and `aise paths` to confirm which executable and state
   directories are active.
4. Run `aise install` and `aise status` to create the `aisearch` and
   `ai_session_search` aliases, register the same executable with detected MCP clients,
   and install managed agent instructions. Use
   `aise install --dry-run` first when targeting existing custom files.
5. Run `aise reindex`, then `aise list` and `aise search "QUERY"` to verify the
   index and search path end to end.

To update or remove the product later, follow [Update and
uninstall](#update-and-uninstall) in order. Integration removal must happen
before removing the executable that performs it.

## Registry installation

Choose one global `aise` command owner. Installing both uv and Cargo/native
global commands can select different builds based on PATH order. Install the
standalone command in uv's isolated tool environment:

```bash
uv tool install ai-session-search
aise --help
```

Run it without a persistent installation:

```bash
uvx --from ai-session-search aise --help
```

Add the Python API to a uv-managed project:

```bash
uv add ai-session-search
```

Install the Python API and command with pip:

```bash
python -m pip install ai-session-search
```

Install the native Rust command from crates.io:

```bash
cargo install ai-session-search --locked
```

These package installation commands do not create aliases, register MCP servers, write
managed Markdown, or install client hooks. The common `aise install` step owns aliases
and integrations without copying the package-owned executable.

After any installation method, register the same `aise` executable with detected
MCP clients:

```bash
aise install
aise status
```

Users who want the recommended executable plus detected-client integration can
run one fail-fast shell command. Package ownership and MCP configuration remain
separate transactions, so a package-manager failure never edits client files:

```bash
uv tool install ai-session-search && aise install
cargo install ai-session-search --locked && aise install
```

This follows the same proven lifecycle as RTK (`rtk` installation followed by
`rtk init -g`) and autorun (uv tool installation followed by
`autorun --install`), while `aise install` additionally provides dry-run,
status, per-client selection, durable recovery, and ownership-safe uninstall.

The default stored command is portable `aise`. If a desktop client reports that
the executable is missing because it inherits a different PATH, rerun install for
that client with `--binary PATH`. Supported selectors are `claude`, `codex`,
`gemini`, `antigravity`, `cursor`, `windsurf`, `vscode`, `zed`, `opencode`,
`openclaw`, and `kilocode`; omission or `all` updates detected clients. Repeat
`--client CLIENT` to include several explicit clients, or repeat
`--exclude-client CLIENT` to remove clients from that set. Explicit custom
paths are always included and are not client aliases, so exclusions do not
discard them. The Kilo selector is
explicitly the legacy VS Code extension adapter. Current standalone Kilo uses
`~/.config/kilo/kilo.jsonc` and is not modified. The installer adds managed
instruction guidance for Claude, Codex, OpenCode, Gemini, and Antigravity;
Gemini and Antigravity share `~/.gemini/GEMINI.md`. It does not install hooks.
Generated guidance introduces the product as **AI Session Search (`aise`)** and
names the initial MCP tools (`search_sessions`, `search_messages`, and
`get_session`) rather than assuming that a new user or agent knows what `aise`
means. Claude's imported `AI_SESSION_SEARCH.md` has an explicit whole-file
ownership sentinel so upgrades can replace older aise-owned wording while
refusing to overwrite user-owned content.

Published wheels support GIL-enabled CPython 3.12 through 3.14 on
manylinux2014 x86_64/aarch64, macOS x86_64/arm64, and Windows x86_64; they do
not require a local Rust compiler. Git, sdist, and Cargo installations build
native code from source and require Git, Rust 1.88 or newer, and a C linker for
the target platform.

## Update and uninstall

Use the same owner for installation, update, and removal. Before removing a
global command, remove its MCP registrations while the command is still
available:

```bash
aise uninstall

# Choose only the commands matching the installation owner or project use.
uv tool upgrade ai-session-search && aise install
uv tool uninstall ai-session-search
uv remove ai-session-search
python -m pip uninstall ai-session-search
cargo install ai-session-search --locked --force && aise install
cargo uninstall ai-session-search
```

`aise uninstall --keep-instructions` removes MCP entries while preserving
managed guidance. The default removes only aise-owned MCP entries and guidance.
Neither MCP nor package-manager uninstall deletes the index, configuration, or
source session files.

Lifecycle operations are top-level commands: `aise install`, `aise status`, and
`aise uninstall`. The `aise mcp` namespace contains only `serve` and `recover`,
which are protocol/recovery operations rather than duplicate lifecycle aliases.
Top-level `aise install` does not bypass package
ownership: the running CLI must already be provided by uv, Cargo, pip, or a
verified native archive. It verifies that `aise` is on `PATH`, then installs or
refreshes MCP registrations and managed instructions. This prevents an
ephemeral `uvx`, source-tree, or Python interpreter process from being copied
and mislabeled as a package-managed installation.

`aise install` is an idempotent installation refresh: rerunning the same version
changes no bytes, while running it after a package-manager update refreshes
owned relative `aisearch -> aise` and `ai_session_search -> aise` symbolic links,
MCP entries, and instruction text. It refuses to replace either alias path when that
path is not an owned symbolic link. Use `--dry-run` before mutation,
repeat `--client CLIENT` for an explicit include set, repeat
`--exclude-client CLIENT` to subtract clients, use `--no-instructions` for MCP only,
use `--no-aliases` to skip executable aliases, or use the custom config/Markdown flags
shown by `aise install --help`. Use the
separate `aise uninstall` command with the same target selectors;
`--keep-instructions` retains managed Markdown and `--keep-aliases` retains executable
aliases while removing the other selected integration. Neither command installs or
removes the package-owned `aise` executable.

The links are relative so moving an intact bin directory keeps them valid. Unix supports
them directly. Windows requires symbolic-link permission (normally Developer Mode or an
elevated process); if the operating system rejects link creation, the command reports the
failed path and recommends `--no-aliases`. The installer never substitutes copied binaries,
hard links, `.cmd` wrappers, or extra Python console scripts because those create divergent
ownership and update behavior.

For a pip-owned global installation, update and refresh with:

```bash
python -m pip install --upgrade ai-session-search && aise install
```

For a verified native archive, run its rollback-preserving installer and then
`aise install`. Do not mix uv-, Cargo-, pip-, and native-owned executables on
one `PATH`; `aise paths` reports the active executable and every matching
candidate when ownership is unclear.

## Custom installation locations

Keep executable ownership separate from client configuration. Use the package
manager's supported destination controls, then pass the resulting executable to
`aise install` only when a graphical client cannot resolve it from `PATH`:

```bash
# uv tool environment and executable directory
UV_TOOL_DIR=/custom/uv/tools UV_TOOL_BIN_DIR=/custom/bin \
  uv tool install ai-session-search

# Cargo installation root; the executable is /custom/cargo/bin/aise
cargo install --root /custom/cargo ai-session-search --locked

# Extracted verified native archive (the packaged script is named install.sh)
sh install.sh --bin-dir /custom/bin

# Repository checkout used by maintainers
sh scripts/install-native.sh --bin-dir /custom/bin

# Register that executable with detected clients
/custom/bin/aise install --binary /custom/bin/aise
```

Use the same uv environment variables or Cargo `--root` when upgrading or
uninstalling so the package manager edits the installation it owns. Integration
removal remains `/custom/bin/aise uninstall` and must run before executable
removal. `aise install --help` exposes typed paths for common JSON, VS Code,
Zed, OpenCode, Codex TOML, Claude Markdown, Gemini/Antigravity Markdown,
`AGENTS.md`, and recovery receipt locations; those flags configure
integrations, not package ownership.

The integration acceptance matrix covers Claude Code/Desktop, Codex, Gemini
CLI, **Antigravity**, Cursor, Windsurf, VS Code, Zed, **OpenCode**, OpenClaw,
and the legacy KiloCode adapter. Antigravity's CLI and legacy MCP files are
separate targets but share managed `~/.gemini/GEMINI.md` instructions with
Gemini. OpenCode uses `mcp.aise` in `~/.config/opencode/opencode.json` and a
managed block in `~/.config/opencode/AGENTS.md`. Each target must pass install,
content-aware status, byte-idempotent reinstall, dry-run, and ownership-safe
uninstall tests.

## Install an immutable Git revision

Use a full commit object ID rather than `main`, another branch, a tag, or an
abbreviated hash. A full hash makes the selected source unambiguous and lets pip
avoid extra network work. Replace the example revision below with the commit to
test:

```bash
REPOSITORY_URL=https://github.com/ahundt/ai-session-search
REV=0123456789abcdef0123456789abcdef01234567
```

Install the standalone command with uv:

```bash
uv tool install "ai-session-search @ git+$REPOSITORY_URL@$REV"
```

Run the command ephemerally:

```bash
uvx --from "ai-session-search @ git+$REPOSITORY_URL@$REV" aise --help
```

Add the pinned source to a uv project:

```bash
uv add "ai-session-search @ git+$REPOSITORY_URL@$REV"
```

Install the Python API and command with pip:

```bash
python -m pip install "ai-session-search @ git+$REPOSITORY_URL@$REV"
```

Install the native Rust command:

```bash
cargo install ai-session-search \
  --git "$REPOSITORY_URL" \
  --rev "$REV" \
  --locked
```

## Maintainer acceptance contract

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

The harness deliberately accepts the repository and revision as parameters. It
rejects mutable or abbreviated revisions, insecure HTTP, embedded credentials,
and relative local paths. Temporary virtual environments, tool installations,
configuration, and application caches are scoped to one temporary root and removed
on success or failure. The harness deliberately inherits the caller's content-addressed
uv cache and `CARGO_TARGET_DIR`: normal standalone invocations reuse uv's platform cache,
while `run_ci_local.sh` selects the workspace `target` directory. Set either environment
variable explicitly to use a different shared cache; the harness never deletes it.

`run_ci_local.sh` defaults `CARGO_INCREMENTAL=0` because its full build/test/package gate
does not benefit from retaining a second multi-gigabyte incremental graph. It preserves an
explicit caller value and an inherited `RUSTC_WRAPPER` such as `sccache`; set
`AI_SESSION_SEARCH_RUSTC_WRAPPER=` to disable that wrapper for one gate. If Cargo reports
`No space left on device`, inspect `target` first. Remove only workspace-owned output with
`cargo clean` or a more selective `cargo clean -p <package>` after confirming no other build
is using it. Do not delete `$CARGO_HOME` or uv's shared cache as an automatic recovery step.

## Primary references

- uv defines isolated tool ownership, updates, and uninstall in
  [Tools](https://docs.astral.sh/uv/concepts/tools/).
- uv defines `UV_TOOL_DIR` and `UV_TOOL_BIN_DIR` in
  [The uv tool directory](https://docs.astral.sh/uv/concepts/tools/#the-uv-tool-directory).
- uv documents Git sources, commit revisions, and `uv add --rev` in
  [Managing dependencies](https://docs.astral.sh/uv/concepts/projects/dependencies/).
- pip documents supported VCS URL forms and recommends full commit hashes in
  [VCS Support](https://pip.pypa.io/en/stable/topics/vcs-support/).
- Cargo defines `cargo install --git` and `--rev` in the
  [`cargo install` reference](https://doc.rust-lang.org/cargo/commands/cargo-install.html).
- Cargo defines package-owned executable removal in
  [`cargo uninstall`](https://doc.rust-lang.org/cargo/commands/cargo-uninstall.html).
- PyPA specifies direct URL requirements such as `name @ URL` in
  [Dependency specifiers](https://packaging.python.org/en/latest/specifications/dependency-specifiers/).
- Maturin documents mixed Rust/Python layouts, wheel/sdist builds, and manylinux
  compatibility in the [Maturin user guide](https://www.maturin.rs/).
- PyO3 documents extension-module and stable-ABI distribution in
  [Building and distribution](https://pyo3.rs/latest/building-and-distribution.html).
- RTK documents binary installation followed by its explicit recommended
  integration command in the [RTK README](https://github.com/rtk-ai/rtk#installation).
- Claude Code documents persistent `CLAUDE.md` instructions and imports in
  [How Claude remembers your project](https://code.claude.com/docs/en/memory).
- Codex documents global `~/.codex/AGENTS.md` loading in
  [Custom instructions with AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md.md).
- Gemini CLI documents global and hierarchical context loading in
  [Provide context with GEMINI.md files](https://geminicli.com/docs/cli/gemini-md/).
- The MCP lifecycle defines initialization in
  [Lifecycle](https://modelcontextprotocol.io/specification/2024-11-05/basic/lifecycle),
  while Codex documents its use of returned server `instructions` in
  [Model Context Protocol](https://learn.chatgpt.com/docs/extend/mcp.md).
- OpenCode documents its cross-platform global configuration at
  [`~/.config/opencode/opencode.json`](https://dev.opencode.ai/docs/config).
- Kilo documents current standalone MCP configuration in
  [`~/.config/kilo/kilo.jsonc`](https://kilo.ai/docs/automate/mcp/using-in-kilo-code).

When these tools change syntax or security guidance, update this guide, the CI
workflow, and the installer acceptance tests in the same commit.
