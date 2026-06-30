# sessiongrep

[![CI](https://github.com/braincompany/sessiongrep/actions/workflows/ci.yml/badge.svg)](https://github.com/braincompany/sessiongrep/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

**You solved that bug last week. Your next agent session has no idea.**

A local-first memory layer for CLI agents. `sessiongrep` indexes your Claude Code, Claude Desktop local agent, Codex CLI, Cursor, Antigravity, and Pi session histories into a single SQLite + FTS5 database, then gives you one CLI/TUI to find old work by topic, repo, provider, or recency. It also ships an MCP server so your agent can search its own history.

The real payoff is portable context: your session history isn't trapped in one tool. Work you started in Claude Code can continue in Codex, and an agent can recover — and even critique — its own prior reasoning across every tool you use.

![sessiongrep demo](docs/demo.gif)
<!-- Demo GIF is generated from sanitized sample data (generation scripts kept outside the repo). -->

Read the announcement: [Sessiongrep: a local-first memory layer for CLI agents](https://brain.co/blog/sessiongrep-a-local-first-memory-layer-for-cli-agents).

## Why

Session transcripts already live on your machine — scattered across `~/.claude/projects`, Claude Desktop local agent storage, `~/.codex/sessions`, and `~/.cursor/projects` as noisy JSONL with opaque filenames. The information is not missing, it's stranded. Humans don't want to read it; agents don't know how to retrieve it. Grep over JSONL drowns in tool payloads. Shell history captures commands but not reasoning. Cloud-synced or vector-backed alternatives bring secrets and URLs into systems that aren't yours.

`sessiongrep` keeps recall local. Two small binaries (`sessiongrep` and `sessiongrep-mcp`), one SQLite file, no daemon. The index is a disposable cache; delete it and rebuild it whenever you want.

## How it works

Provider adapters normalize Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, and Pi transcripts into a single `Session` model and write them into SQLite (WAL mode) with an FTS5 virtual table over transcript text, title, summary, and preview. Claude Code sessions use provider `claude`; Claude Desktop local agent sessions use provider `claude-desktop`. Each session is also broken into per-message rows (user / assistant / tool / slash / compaction) with their own FTS index, which powers `messages`, `corrections`, `planning`, `stats`, and `files`. Every read command runs an incremental reindex first — files whose mtime and size haven't changed are skipped, so search and list stay fast even as your history grows. When the index schema changes between releases, the next run reindexes once automatically (no manual `reindex --full` needed).

## Installation

### Prerequisites

- [Rust toolchain](https://rustup.rs/) (1.70+)
- Claude Code, Claude Desktop local agent mode, Codex CLI, and/or Cursor installed (for session data)

### Build and install

```bash
git clone git@github.com:braincompany/sessiongrep.git
cd sessiongrep

# Install both binaries
cargo install --path . --locked

# Or install only one binary
cargo install --path . --bin sessiongrep --locked
cargo install --path . --bin sessiongrep-mcp --locked
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

The substring/regex search index (a custom, parallel-built trigram prefilter) builds **lazily on your
first `--regex`/substring search** — a one-time "building search index…" notice prints while it runs, and
later searches are warm. A full rebuild fragments the FTS5 word index into many segments; `reindex --full`
merges them automatically (FTS5 `optimize`), and `sessiongrep compact` reclaims the freed space on demand
(`optimize` + `VACUUM`; it needs roughly the database's size in free disk while it runs).

## Quick start

```bash
sessiongrep list --limit 20        # recent sessions (auto-indexes on first run)
sessiongrep search "auth bug"      # keyword search
sessiongrep search "redis" --provider codex
sessiongrep search "datadog" --provider cursor
sessiongrep search "temporal" --provider pi
sessiongrep show claude:79accec8-5bf5-415b-a4a5-fe370eb2c998
sessiongrep resume 79accec8 --dry-run
sessiongrep export 79accec8 --format markdown
sessiongrep doctor                 # health check
sessiongrep compact                # reclaim disk space (FTS5 optimize + VACUUM)
sessiongrep tui                    # interactive browser
```

## Messages, analytics, and file recovery

Beyond session-level search, `sessiongrep` indexes every message — user, assistant, tool
output, slash commands, and compaction summaries — so you can search and analyze across all
your history:

```bash
# Per-message search across sessions (filter by role, date, regex, session)
sessiongrep messages search "race condition" --type assistant --since 2026-01
sessiongrep messages search "TODO" --regex --type user
sessiongrep messages search "ls -la" --type tool      # tool output across supported providers
sessiongrep messages get <session-id>                 # all messages in one session
sessiongrep messages timeline <session-id> --type user

# Analytics
sessiongrep corrections --since 7d                    # where you corrected the agent
sessiongrep planning --commands '^/(ar:)?plan'        # slash-command usage frequency
sessiongrep stats --when 2026-01                      # message counts by role

# File recovery (from recorded Write/Edit/MultiEdit/ApplyPatch tool calls)
sessiongrep files search '*.rs'                       # files edited, with counts
sessiongrep files history src/db.rs                   # ordered versions of one file
sessiongrep files extract src/db.rs --version 3 --output-dir /tmp/recovered

sessiongrep dates                                     # list every supported date/EDTF form
```

Every command takes `--format table|json|jsonl|csv|plain` for scripting, and the date flags
(`--since`/`--until`/`--when`) accept ISO dates, EDTF (`2026-01`, `202X`, `2026-01-1X`,
intervals like `2026-01/2026-03`), durations (`7d`, `2w`, `24h`), and natural language
(`yesterday`, `3 days ago`). Analytics/message limits default to unlimited (`--limit 0`).

## MCP server setup

The MCP server lets AI agents search and retrieve your past sessions programmatically — no copy-pasting context from old conversations.

### Install MCP client config

After installing the binaries, register `sessiongrep-mcp` with your MCP clients:

```bash
sessiongrep mcp install
```

The installer is idempotent and preserves existing config. By default it updates every detected client config it can find:

| Client | Config location | Shape | Instruction guidance |
| --- | --- | --- | --- |
| Claude Code | `~/.claude.json`, `~/.claude/.mcp.json` | `mcpServers.sessiongrep` | `CLAUDE.md` imports `SESSIONGREP.md` |
| Claude Desktop | `claude_desktop_config.json` | `mcpServers.sessiongrep` | MCP config only |
| Codex CLI / Codex desktop config | `~/.codex/config.toml` | `[mcp_servers.sessiongrep]` | managed `AGENTS.md` block |
| Gemini CLI | `~/.gemini/settings.json` | `mcpServers.sessiongrep` | MCP config only |
| Antigravity | `~/.gemini/antigravity/mcp_config.json` | `mcpServers.sessiongrep` | MCP config only |
| Cursor | `~/.cursor/mcp.json` | `mcpServers.sessiongrep` | MCP config only |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | `mcpServers.sessiongrep` | MCP config only |
| VS Code | `Code/User/mcp.json` | `servers.sessiongrep` with `type = "stdio"` | MCP config only |
| Zed | `Zed/settings.json` | `context_servers.sessiongrep` | MCP config only |
| OpenCode | `~/.config/opencode/opencode.json` | `mcp.sessiongrep.command[]` | managed `AGENTS.md` block |
| OpenClaw | `~/.openclaw/openclaw.json` | `mcpServers.sessiongrep` | MCP config only |
| KiloCode | `Code/User/globalStorage/.../mcp_settings.json` | `mcpServers.sessiongrep` | MCP config only |

Platform-native config roots are used: macOS `~/Library/Application Support/...`, Linux `~/.config/...`, and Windows roaming config directories where applicable.

For Claude Code, install also writes `SESSIONGREP.md` next to `CLAUDE.md` and adds `@SESSIONGREP.md`, using Claude Code's file-import support. For Codex and OpenCode, install adds a short managed block directly to `AGENTS.md` because those harnesses read `AGENTS.md` as literal instructions. Pass `--no-instructions` to skip instruction files. Uninstall removes only the managed `sessiongrep` reference or block.

Use `--client` to create or update one client, `--dry-run` to preview writes, and custom flags for compatible config and instruction files:

```bash
sessiongrep mcp install --client codex --dry-run
sessiongrep mcp install --client claude
sessiongrep mcp install --client claude --no-instructions
sessiongrep mcp install --json-mcp-config ~/my-agent/mcp.json
sessiongrep mcp install --vscode-config ~/Library/Application\ Support/Code/User/mcp.json
sessiongrep mcp install --codex-config ~/.codex/config.toml
sessiongrep mcp install --agents-md ~/my-agent/AGENTS.md
sessiongrep mcp status
sessiongrep mcp uninstall --client codex --dry-run
```

The MCP binary can perform the same registration without opening the index:

```bash
sessiongrep-mcp install
sessiongrep-mcp status
sessiongrep-mcp uninstall --client codex
```

Restart the client after install or uninstall.

### Manual setup fallback

If you prefer to manage MCP config manually, use these commands.

#### Claude Code

```bash
claude mcp add --scope user --transport stdio sessiongrep -- sessiongrep-mcp
```

#### Codex CLI

```bash
codex mcp add sessiongrep -- sessiongrep-mcp
```

### Verify

Start a new session and try a prompt like:

> "Find my previous session where I was setting up Datadog metrics"

The agent will call `search_sessions` to find matches and `get_session` to pull in relevant context. For finer-grained recall it can call `search_messages` (individual messages, with surrounding context) — e.g. *"find where I corrected you about the retry logic in this repo last week"*.

### MCP tools

Two layers: **session-level** (find/open whole sessions) and **message-level** (find individual turns and their neighbors). Message search returns structured JSON, and every hit carries `session_id`+`seq` so the agent can chain into `get_session` for a focused message window or `get_resume_command` to reopen the session.

| Tool | Description |
|------|-------------|
| `search_sessions` | Search sessions by keyword; optional `provider`, `path_prefix` (cwd/repo), `since`/`until`/`when` date bounds, `limit` |
| `list_sessions` | List recent sessions; filter by `provider`, `path_prefix`, `since`/`until`/`when`, `limit` |
| `get_session` | Get transcript and metadata by session ID (`max_lines` defaults to `-40`, i.e. tail; positive=head, negative=tail, `0`=entire transcript and may be very large), or pass `seq` + `context` to read a focused message window around a `search_messages` hit |
| `get_resume_command` | Get the CLI command to resume a session in its native tool |
| `search_messages` | Search individual messages by `query` or `regex`; filter by `role`, `provider`, `tool`, `path_prefix`, `since`/`until`/`when`, `session`; include surrounding turns with `context`; `limit`/`offset` pagination; `response_format` concise/detailed |

Date bounds accept the same EDTF/ISO/duration/natural-language strings as the CLI (e.g. `2026-01`, `7d`, `yesterday`). Use `since` or `until` alone for an open-ended window, or `when` for one complete span; do not combine `when` with `since` or `until`. For `path_prefix`, prefer an **absolute path** (or `~/...`, which the server expands) — a relative path resolves against the MCP server's working directory, which the client controls and may differ from yours. The CLI's `--path` resolves relative paths against your current directory and canonicalizes `.`/`..`/symlinks to match the absolute paths stored in the index.

## Config

Optional config file: `~/.config/sessiongrep/config.toml`. If it is absent, sessiongrep uses built-in defaults. Use `sessiongrep config path` to print the config location, `sessiongrep config show` to print the effective merged TOML, and `sessiongrep paths` to see active data paths.

```toml
[index]
busy_timeout_ms = 5000
auto_reindex_busy_timeout_ms = 10000

[providers.claude]
enabled = true
paths = ["~/.claude/projects"]

[providers.claude-desktop]
enabled = true
paths = [
  "~/Library/Application Support/Claude/local-agent-mode-sessions",
]

[providers.codex]
enabled = true
paths = ["~/.codex/sessions"]

[providers.cursor]
enabled = true
paths = ["~/.cursor/projects"]

[providers.antigravity]
enabled = true
paths = ["~/.gemini/antigravity/brain"]

[providers.pi]
enabled = true
paths = ["~/.pi/agent/sessions"]
```

`busy_timeout_ms` controls normal SQLite reads and writes. `auto_reindex_busy_timeout_ms` controls only the automatic refresh that runs before read commands and MCP tool calls; if another process is still writing after this timeout, sessiongrep serves the existing valid index. Set it to `0` only when you explicitly prefer immediate stale-read fallback under writer contention.

Filter Claude Code with `--provider claude` and Claude Desktop local agent sessions with `--provider claude-desktop`. Claude Desktop defaults use the platform config/data directories when available; on Windows that is expected to resolve under `%APPDATA%\Claude`, but use `sessiongrep paths` or an absolute custom path to confirm your machine.

## Privacy & data

- Everything stays on your machine. No network calls, no telemetry, no cloud sync.
- The tool is read-only — it never modifies your session files.
- The SQLite index is a derived cache. Delete it anytime and `reindex --full` rebuilds it from your transcripts.
- All paths (database, cache, config) are user-local under `~/.local/share`, `~/.cache`, and `~/.config`.

## Limitations

- Resume delegates to the native provider CLI (`claude --resume <id>`, `codex resume <id>`, or `pi --session <id>`). Cursor and Antigravity resume are not currently supported.
- Claude Desktop support covers local agent mode `audit.jsonl` sessions plus the sibling `local_*.json` metadata sidecar. General cloud chat history stored behind Claude Desktop's Electron/IndexedDB cache is not indexed.
- Claude, Cursor, and Pi subagent transcripts are excluded from indexing to avoid duplicate records.
- Tool output (`messages search --type tool`) is indexed for supported providers (Claude Code, Claude Desktop local agent, Codex, Cursor, Pi, Antigravity).
- File-version recovery (`files`) covers supported providers, with per-provider fidelity:
  - **Claude Code / Claude Desktop local agent / Pi** — `Write`/`Edit`/`MultiEdit` (Pi: `write`/`edit`) with full content and `old`→`new` deltas; reconstructable via `files extract`.
  - **Codex** — `apply_patch` payloads: `Add File` carries full content (replayable); `Update`/`Delete` are path-only.
  - **Cursor** — `ApplyPatch` unified diffs, path-only (a diff is not a replayable Write/Edit delta).
  - **Antigravity** — edit tool calls (`write_to_file`/`replace_file_content`/`multi_replace_file_content`), path-only; the transcript's edit-arg content shape is unverified upstream, so only the file path is recorded.

## Status

Early but usable — pre-release, built from source (no tagged release yet). The CLI surface and MCP tool names are likely to stay stable; the on-disk index schema may still change between releases, but the next run detects the bump and reindexes once automatically — you should not need to delete the database (you always can: remove `~/.local/share/sessiongrep/index.db` and it rebuilds from your transcripts).

## Contributing

Issues and pull requests are welcome. For bugs, please include your provider versions and a `sessiongrep doctor` output. For features, a quick issue to discuss scope before sending a PR keeps things moving.

## License

Apache-2.0. See [LICENSE](LICENSE).
