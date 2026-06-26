# sessiongrep

[![CI](https://github.com/braincompany/sessiongrep/actions/workflows/ci.yml/badge.svg)](https://github.com/braincompany/sessiongrep/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

**You solved that bug last week. Your next agent session has no idea.**

A local-first memory layer for CLI agents. `sessiongrep` indexes your Claude Code, Codex CLI, Cursor, Antigravity, and Pi session histories into a single SQLite + FTS5 database, then gives you one CLI/TUI to find old work by topic, repo, provider, or recency. It also ships an MCP server so your agent can search its own history.

The real payoff is portable context: your session history isn't trapped in one tool. Work you started in Claude Code can continue in Codex, and an agent can recover — and even critique — its own prior reasoning across every tool you use.

![sessiongrep demo](docs/demo.gif)
<!-- Demo GIF is generated from sanitized sample data (generation scripts kept outside the repo). -->

Read the announcement: [Sessiongrep: a local-first memory layer for CLI agents](https://brain.co/blog/sessiongrep-a-local-first-memory-layer-for-cli-agents).

## Why

Session transcripts already live on your machine — scattered across `~/.claude/projects`, `~/.codex/sessions`, `~/.cursor/projects` as noisy JSONL with opaque filenames. The information is not missing, it's stranded. Humans don't want to read it; agents don't know how to retrieve it. Grep over JSONL drowns in tool payloads. Shell history captures commands but not reasoning. Cloud-synced or vector-backed alternatives bring secrets and URLs into systems that aren't yours.

`sessiongrep` keeps recall local. One static binary, one SQLite file, no daemon, no server. The index is a disposable cache — delete it and rebuild it whenever you want.

## How it works

Provider adapters normalize Claude, Codex, Cursor, Antigravity, and Pi transcripts into a single `Session` model and write them into SQLite (WAL mode) with an FTS5 virtual table over transcript text, title, summary, and preview. Each session is also broken into per-message rows (user / assistant / tool / slash / compaction) with their own FTS index, which powers `messages`, `corrections`, `planning`, `stats`, and `files`. Every read command runs an incremental reindex first — files whose mtime and size haven't changed are skipped, so search and list stay fast even as your history grows. When the index schema changes between releases, the next run reindexes once automatically (no manual `reindex --full` needed).

## Installation

### Prerequisites

- [Rust toolchain](https://rustup.rs/) (1.70+)
- Claude Code, Codex CLI, and/or Cursor installed (for session data)

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
sessiongrep search "datadog" --provider cursor
sessiongrep search "temporal" --provider pi
sessiongrep show claude:79accec8-5bf5-415b-a4a5-fe370eb2c998
sessiongrep resume 79accec8 --dry-run
sessiongrep export 79accec8 --format markdown
sessiongrep doctor                 # health check
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
sessiongrep messages search "ls -la" --type tool      # tool output (claude/codex/cursor/pi)
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

[providers.cursor]
enabled = true
paths = ["~/.cursor/projects"]

[providers.antigravity]
enabled = true
paths = ["~/.gemini/antigravity/brain"]

[providers.pi]
enabled = true
paths = ["~/.pi/agent/sessions"]

[index]
db_path = "~/.local/share/sessiongrep/index.db"
cache_dir = "~/.cache/sessiongrep"

[ui]
preview_lines = 30

[search]
default_limit = 50
prefer_current_repo = true

# Override the built-in correction/planning detection (optional). When non-empty,
# `correction_patterns` fully replaces the built-in correction categories; each entry is
# "CATEGORY:REGEX". `planning_commands` restricts `planning` to matching slash commands.
[analytics]
correction_patterns = []   # e.g. ["regression:you (broke|reverted) ", "incomplete:you forgot"]
planning_commands = []     # e.g. ["^/ar:", "^/plan"]

# Fuzzy-search ranking weights (optional). Defaults shown; you rarely need to change these.
# Any omitted key keeps its default, so set only what you want to retune.
[search.scoring]
title_score = 600
summary_score = 450
path_score = 350          # cwd / repo-root match
preview_score = 250
other_score = 100         # transcript and other fields
token_bonus = 40          # per query token found in a field
all_tokens_bonus = 150    # when every query token matched somewhere
recency_weight = 2        # added per day of recency, up to recency_max_days
recency_max_days = 90
current_repo_bonus = 200  # session repo == current repo
fts_candidate_multiplier = 5
fts_candidate_floor = 200
```

## Privacy & data

- Everything stays on your machine. No network calls, no telemetry, no cloud sync.
- The tool is read-only — it never modifies your session files.
- The SQLite index is a derived cache. Delete it anytime and `reindex --full` rebuilds it from your transcripts.
- All paths (database, cache, config) are user-local under `~/.local/share`, `~/.cache`, and `~/.config`.

## Limitations

- Resume delegates to the native provider CLI (`claude --resume <id>`, `codex resume <id>`, or `pi --session <id>`). Cursor and Antigravity resume are not currently supported.
- Claude, Cursor, and Pi subagent transcripts are excluded from indexing to avoid duplicate records.
- Tool output (`messages search --type tool`) is indexed for Claude, Codex, Cursor, and Pi. Antigravity tool output is not yet indexed — its transcript result-record shape is unverified and the CLI may store sessions outside `transcript.jsonl`.
- File-version recovery (`files`) covers Claude and Cursor, which record file-mutating tool calls (Write/Edit/MultiEdit/NotebookEdit, and Cursor's ApplyPatch as path-only — a unified diff is not reconstructable via `files extract`). Codex (patches are embedded in `exec_command` shell args), Pi, and Antigravity do not yet emit `file_edits`.

## Status

Early but usable — pre-release, built from source (no tagged release yet). The CLI surface and MCP tool names are likely to stay stable; the on-disk index schema may still change between releases, but the next run detects the bump and reindexes once automatically — you should not need to delete the database (you always can: remove `~/.local/share/sessiongrep/index.db` and it rebuilds from your transcripts).

## Contributing

Issues and pull requests are welcome. For bugs, please include your provider versions and a `sessiongrep doctor` output. For features, a quick issue to discuss scope before sending a PR keeps things moving.

## License

Apache-2.0. See [LICENSE](LICENSE).
