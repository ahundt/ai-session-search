# sessiongrep

Local-first search, inspection, export, and resume for Claude Code and Codex CLI sessions.

`sessiongrep` scans local session artifacts, normalizes them into a single SQLite index, and gives you one CLI/TUI to find old work by topic, repo, provider, or recency. It also ships an MCP server so your AI agent can search its own history.

## Installation

### Prerequisites

- [Rust toolchain](https://rustup.rs/) (1.70+)
- Claude Code and/or Codex CLI installed (for session data)

### Build and install

```bash
git clone git@github.com:braincompany/sessiongrep.git
cd sessiongrep

# Install both binaries
cargo install --path .
```

This installs two binaries to `~/.cargo/bin/`:
- `sessiongrep` — CLI and TUI
- `sessiongrep-mcp` — MCP server

Make sure `~/.cargo/bin` is in your PATH. Add to your `~/.bashrc` or `~/.zshrc` if not already present:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

### Index your sessions

The index updates automatically — every command (search, list, tui, etc.) runs an incremental reindex before executing. No cron jobs or manual steps needed.

To force a full rebuild from scratch:

```bash
sessiongrep reindex --full
```

## Quick start

```bash
sessiongrep list --limit 20        # recent sessions (auto-indexes on first run)
sessiongrep search "auth bug"      # keyword search
sessiongrep search "redis" --provider codex
sessiongrep show claude:79accec8-5bf5-415b-a4a5-fe370eb2c998
sessiongrep resume 79accec8 --dry-run
sessiongrep export 79accec8 --format markdown
sessiongrep doctor                 # health check
sessiongrep tui                    # interactive browser
```

## MCP server setup

The MCP server lets AI agents search and retrieve your past sessions programmatically — no copy-pasting context from old conversations.

### Claude Code

```bash
claude mcp add --scope user --transport stdio sessiongrep -- sessiongrep-mcp
```

### Codex CLI

```bash
codex mcp add sessiongrep -- sessiongrep-mcp
```

### Verify

Start a new session and try a prompt like:

> "Find my previous session where I was setting up Datadog metrics"

The agent will call `search_sessions` to find matches and `get_session` to pull in relevant context.

### MCP tools

| Tool | Description |
|------|-------------|
| `search_sessions` | Search sessions by keyword, with optional provider filter and limit |
| `get_session` | Get full transcript and metadata by session ID (supports `max_lines` to limit context) |
| `list_sessions` | List recent sessions, filterable by provider and path prefix |
| `get_resume_command` | Get the CLI command to resume a session in its native tool |

## Config

Optional config file at `~/.config/sessiongrep/config.toml`:

```toml
[providers.claude]
enabled = true
paths = ["~/.claude/projects"]

[providers.codex]
enabled = true
paths = ["~/.codex/sessions"]

[index]
db_path = "~/.local/share/sessiongrep/index.db"
cache_dir = "~/.cache/sessiongrep"

[ui]
preview_lines = 30

[search]
default_limit = 50
prefer_current_repo = true
```

## Notes

- The tool is read-only — it never modifies your session files.
- Resume delegates to the native provider CLI (`claude --resume <id>` or `codex resume <id>`).
- Claude subagent transcripts are excluded from indexing to avoid duplicate records.
- The SQLite index is a derived cache — delete it anytime and `reindex --full` rebuilds it.
