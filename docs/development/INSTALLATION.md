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

These package installation commands do not register MCP servers, write managed
Markdown, or install client hooks. Integration is a separate opt-in operation.

After any installation method, register the same `aise` executable with detected
MCP clients:

```bash
aise mcp install
aise mcp status
```

Users who want the recommended executable plus detected-client integration can
run one fail-fast shell command. Package ownership and MCP configuration remain
separate transactions, so a package-manager failure never edits client files:

```bash
uv tool install ai-session-search && aise mcp install
cargo install ai-session-search --locked && aise mcp install
```

This follows the same proven lifecycle as RTK (`rtk` installation followed by
`rtk init -g`) and autorun (uv tool installation followed by
`autorun --install`), while `aise mcp install` additionally provides dry-run,
status, per-client selection, durable recovery, and ownership-safe uninstall.

The default stored command is portable `aise`. If a desktop client reports that
the executable is missing because it inherits a different PATH, rerun install for
that client with `--binary PATH`. Supported selectors are `claude`, `codex`,
`gemini`, `antigravity`, `cursor`, `windsurf`, `vscode`, `zed`, `opencode`,
`openclaw`, and `kilocode`; `all` updates detected clients. The Kilo selector is
explicitly the legacy VS Code extension adapter. Current standalone Kilo uses
`~/.config/kilo/kilo.jsonc` and is not modified. The installer adds managed
instruction guidance for Claude, Codex, OpenCode, Gemini, and Antigravity;
Gemini and Antigravity share `~/.gemini/GEMINI.md`. It does not install hooks.

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
aise mcp uninstall

# Choose only the commands matching the installation owner or project use.
uv tool upgrade ai-session-search
uv tool uninstall ai-session-search
uv remove ai-session-search
python -m pip uninstall ai-session-search
cargo install ai-session-search --locked --force
cargo uninstall ai-session-search
```

`aise mcp uninstall --no-instructions` removes MCP entries while preserving
managed guidance. The default removes only aise-owned MCP entries and guidance.
Neither MCP nor package-manager uninstall deletes the index, configuration, or
source session files.

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
- uv documents Git sources, commit revisions, and `uv add --rev` in
  [Managing dependencies](https://docs.astral.sh/uv/concepts/projects/dependencies/).
- pip documents supported VCS URL forms and recommends full commit hashes in
  [VCS Support](https://pip.pypa.io/en/stable/topics/vcs-support/).
- Cargo defines `cargo install --git` and `--rev` in the
  [`cargo install` reference](https://doc.rust-lang.org/cargo/commands/cargo-install.html).
- PyPA specifies direct URL requirements such as `name @ URL` in
  [Dependency specifiers](https://packaging.python.org/en/latest/specifications/dependency-specifiers/).
- Maturin documents mixed Rust/Python layouts, wheel/sdist builds, and manylinux
  compatibility in the [Maturin user guide](https://www.maturin.rs/).
- PyO3 documents extension-module and stable-ABI distribution in
  [Building and distribution](https://pyo3.rs/latest/building-and-distribution.html).
- RTK documents binary installation followed by its explicit recommended
  integration command in the [RTK README](https://github.com/rtk-ai/rtk#installation).
- OpenCode documents its cross-platform global configuration at
  [`~/.config/opencode/opencode.json`](https://dev.opencode.ai/docs/config).
- Kilo documents current standalone MCP configuration in
  [`~/.config/kilo/kilo.jsonc`](https://kilo.ai/docs/automate/mcp/using-in-kilo-code).

When these tools change syntax or security guidance, update this guide, the CI
workflow, and the installer acceptance tests in the same commit.
