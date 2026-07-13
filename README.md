# AI Session Tools - aise

Search, analyze, and organize AI sessions from Claude Code, AI Studio, and Gemini CLI.

![demo](https://github.com/user-attachments/assets/1b3e12cc-68c0-4643-9d5c-4cf4f5cd8992)


## The problem

AI tools save conversations in raw files — Claude Code uses JSONL with UUID names, AI Studio uses
unnamed JSON exports, Gemini CLI stores sessions under hashed temp directories. If you want to
find "that Python file Claude wrote last week," search across hundreds of sessions, or build a
structured knowledge base from thousands of AI Studio conversations, you'd have to manually parse
JSON and dig through dozens of folders.

## What this tool does

`aise` reads directly from your session files and gives you these capabilities:

| Capability | Command |
|-----------|---------|
| List sessions with metadata (project, branch, date, message count) | `aise list` |
| Filter by provider: Claude Code, AI Studio, or Gemini CLI | `aise list --provider claude` |
| Find source files Claude wrote or edited (Write + Edit) | `aise files search "*.py"` |
| Inspect file version history (Write + Edit timeline) | `aise files history cli.py` |
| Extract or reconstruct a file (Write snapshots + Edit replay) | `aise files extract cli.py` |
| Cross-reference session edits against a current file | `aise files cross-ref ./cli.py` |
| Search conversation messages full-text | `aise messages search "auth bug"` |
| Search tool invocations (Bash, Edit, Write, Read, …) | `aise tools search Write "login"` |
| Read all messages from one session | `aise get ab841016` |
| Find user correction messages | `aise messages corrections` |
| Count slash command usage | `aise messages planning` |
| List slash command invocations with metadata | `aise commands list --since 14d` |
| See context after a slash command | `aise commands context /ar:plannew` |
| Pipe session IDs for composable workflows | `aise list --ids-only \| xargs ...` |
| Export one session to markdown | `aise export session ab841016` |
| Export recent sessions to markdown | `aise export recent 7 --output week.md` |
| Manage session source directories | `aise source list` / `aise source add <path>` |
| Run full analysis pipeline | `aise analyze` |
| View / create the config file | `aise config show` / `aise config init` |
| Inspect a session (message counts, tool usage, files touched) | `aise messages inspect ab841016` |
| Chronological timeline of a session | `aise messages timeline ab841016` |
| Extract content from a session (e.g. pbcopy content) | `aise messages extract ab841016 pbcopy` |
| Show supported date/time format reference | `aise dates` |
| Summary statistics across all sources | `aise stats` |

Works as a CLI tool (`aise`) and as an importable Python library (`from ai_session_search import ...`).

### Claude Code integration via autorun

[autorun](https://github.com/ahundt/autorun) is a Claude Code plugin that adds
slash commands, hooks, and autonomous task workflows to your editor.  It ships
with a built-in `/ar:ai-session-tools` skill (also available as
`/ar:claude-session-tools` for backward compatibility) that exposes `aise` as a
first-class Claude Code command, so you can search and recover session history
without leaving the editor or switching to a terminal.

**What the integration adds:**

- `/ar:ai-session-tools` (or `/ar:claude-session-tools`): natural-language skill.
  Describe what you want (`"find the auth bug I fixed last week"`, `"show recent
  Python files"`) and Claude runs the appropriate `aise` commands and surfaces
  the results inline
- Full access to all `aise` capabilities (search, file history, corrections,
  stats, export) from within a Claude Code conversation
- Useful after context compaction: ask Claude to recover the previous session
  context directly inside the new conversation

**Install:**

```bash
# 1. Install autorun (one-time)
git clone https://github.com/ahundt/autorun ~/.claude/plugins/autorun
# Follow autorun's README for Claude Code plugin activation

# 2. Install ai-session-tools (already done if you followed Install above)
uv tool install git+https://github.com/ahundt/ai-session-search

# 3. Use inside Claude Code
# /ar:ai-session-tools find files I edited yesterday
```

See [autorun's README](https://github.com/ahundt/autorun) for full setup and
the complete list of available slash commands.

## Install

```bash
uv tool install git+https://github.com/ahundt/ai-session-search
```

This gives two equivalent commands: `aise` (short) and `ai_session_search` (long).

To use as a library instead:

```bash
uv add git+https://github.com/ahundt/ai-session-search
```

## Quick start

```bash
# 1. List sessions from all auto-detected sources (Claude Code, AI Studio, Gemini CLI)
aise list

# 2. Filter to one provider
aise list --provider claude      # Claude Code sessions only
aise list --provider aistudio    # AI Studio sessions only
aise list --provider gemini      # Gemini CLI sessions only

# 3. Search messages across all sources
aise messages search "authentication"

# 4. Run the full analysis pipeline (AI Studio / Gemini sessions)
aise analyze

# 5. Show statistics per source
aise stats
```

### Claude Code file recovery

```bash
# See all files Claude ever wrote or edited
aise files search

# Find a specific file
aise files search --pattern "*session*"

# Check its version history
aise files history session_manager.py

# Print the latest version to stdout
aise files extract session_manager.py

# Redirect to a file
aise files extract session_manager.py > session_manager.py
```

## Source management

`aise` auto-detects session sources from standard install locations:

| Source | Auto-detected path |
|--------|-------------------|
| Claude Code | `~/.claude/projects/` (always included) |
| Gemini CLI | `~/.gemini/tmp/` (if dir exists and non-empty) |
| AI Studio | `~/Downloads/Google AI Studio/` (if exists) |
| AI Studio | `~/Downloads/drive-download-*/Google AI Studio/` (glob match) |

For non-standard locations, add them explicitly:

```bash
# See what's currently active
aise source list

# Scan standard locations for new sources
aise source scan

# Add a custom directory
aise source add ~/Documents/aistudio_exports
aise source add ~/.gemini/tmp --type gemini

# Remove a directory
aise source remove ~/Documents/old_sessions

# Disable/enable auto-discovery for a source type
aise source disable gemini_cli
aise source enable gemini_cli
```

Source directories are saved to `config.json` and persist across runs.

## Use as a Python Library

```python
import ai_session_search as aise

search = aise.SessionSearch()
search.refresh()

scope = aise.QueryScope(
    provider="codex",
    path_prefix="/path/to/repo",
    dates=aise.DateRangeQuery(when="7d"),
)
sessions = search.list_sessions(aise.SessionQuery(provider="codex", limit=20))
messages = search.search_messages(
    "authentication",
    aise.MessageQuery(
        scope=scope,
        selector=aise.MessageSelector(role="user", no_compaction=True),
        limit=50,
    ),
)
files = search.search_files(
    "*.py",
    aise.FileQueryRequest(scope=scope, min_edits=3, limit=50),
)

if sessions:
    inspection = search.inspect_session(sessions[0].id, include_time_profile=True)
    document = search.export_session(sessions[0].id, "markdown")
status = search.index_status()
diagnostics = search.diagnostics()
```

`SessionSearch` is the canonical Python API. It delegates to the same public Rust
services used by the native CLI and MCP server; Python does not rescan provider
files or reimplement SQL/filter policy. Detailed result types are importable from
`ai_session_search.native`.

## CLI reference

### List sessions

```bash
# All sessions from all sources, newest first
aise list

# Filter by provider
aise list --provider claude
aise list --provider aistudio
aise list --provider gemini
aise list --provider all       # explicit all (default)

# Filter by project directory substring
aise list --project myproject

# Sessions since a date
aise list --since 2026-01-15

# JSON output
aise list --format json

# Limit to 10
aise list --limit 10
```

### Statistics

```bash
aise stats
```

```
Recovery Statistics
  Sessions:      1360
    aistudio       1167
    gemini_cli      193
  Files:          318
  Versions:      1205
  Largest File:  engine.py (47 edits)
```

### Find source files (Claude Code)

```bash
# All files (shows edits, sessions, last modified — includes Edit tool calls)
aise files search

# Only Python files (positional arg or --pattern flag)
aise files search "*.py"

# Files edited 5+ times
aise files search --min-edits 5

# Python and Markdown, after a date
aise files search -i py,md --since 2026-01-15

# Exclude compiled files
aise files search -x pyc,tmp,o

# JSON output
aise files search --format json

# 'find' is an alias for 'search'
aise files find --pattern "*.py"
```

### Inspect file version history (Claude Code)

```bash
# Show a read-only version table (version#, lines, Δlines, tool, +add/-del, timestamp, session)
aise files history cli.py

# Narrow to one session
aise files history cli.py --session ab841016

# Export all versions to disk as cli_v1.py, cli_v2.py, …
aise files history cli.py --export

# Preview without touching disk
aise files history cli.py --export --dry-run

# Dump all versions to stdout (for piping to AI tools)
aise files history cli.py --stdout
```

`files history` is **read-only by default** — no disk writes unless you pass `--export`.

### Extract file content (Claude Code)

```bash
# Print latest version to stdout (Write snapshots or Edit reconstruction)
aise files extract cli.py

# Specific version
aise files extract cli.py --version 2

# Pipe to clipboard
aise files extract cli.py | pbcopy

# Limit to one session
aise files extract cli.py --session ab841016

# Write to a directory
aise files extract cli.py --output-dir ./backup

# Preview without I/O
aise files extract cli.py --output-dir ./backup --dry-run
```

If no Write snapshot exists, `extract` automatically reconstructs the file by replaying Edit
tool calls from the session JSONL. File content goes to stdout; status messages go to stderr.

### Cross-reference session edits (Claude Code)

```bash
# Show which edits Claude made to cli.py are present in the current file
aise files cross-ref ./cli.py

# Limit to one session
aise files cross-ref ./cli.py --session ab841016

# JSON output
aise files cross-ref ./cli.py --format json
```

### Search conversation messages

```bash
# Full-text search across all sources (literal match, case-insensitive)
aise messages search "authentication"

# Regex search (use --regex for | OR, .* wildcards, etc.)
aise messages search "forgot|missed|deleted" --regex

# Narrow to one provider
aise messages search "error" --provider claude
aise messages search "error" --provider aistudio

# Only user messages (not assistant)
aise messages search "error" --type user

# Only slash command messages
aise messages search "" --type slash --since 14d

# Exclude compaction summaries
aise messages search "error" --no-compaction

# Asymmetric context windows
aise messages search "bug" --context-before 2 --context-after 5

# Filter by tool type (show only Write/Edit/Bash calls)
aise messages search "" --tool Write --since 7d

# More results with truncation
aise messages search "TODO" --limit 50 --max-chars 200

# JSON output
aise messages search "refactor" --format json

# 'find' is an alias for 'search'
aise messages find "authentication"
```

### Search tool invocations (Claude Code)

Find the actual tool calls Claude made — what files it wrote, what commands it ran:

```bash
# All tool calls (no --tool required)
aise tools search

# All Write tool calls
aise tools search Write

# Bash calls containing "git commit"
aise tools search Bash "git commit"

# Edit calls to cli.py
aise tools search Edit "cli.py"

# JSON or JSONL output
aise tools search Write --format json
aise tools search Write --format jsonl

# 'find' is an alias for 'search'
aise tools find Write

# Also works via messages search with --tool flag
aise messages search "*" --tool Write
```

### Read messages from a session

```bash
# Read all messages (positional session ID)
aise get ab841016

# Read only your messages
aise get ab841016 --type user --limit 50

# JSON output
aise get ab841016 --format json

# Same via messages subcommand
aise messages get ab841016
```

Find session IDs with `aise list`.

### Find user corrections (Claude Code)

Detects messages where you corrected Claude's behavior:

```bash
# Show all correction messages (categorized as regression/skip_step/misunderstanding/incomplete)
aise messages corrections

# Filter by project or session
aise messages corrections --project myproject
aise messages corrections --session ab841016

# Session IDs only (for piping to other commands)
aise messages corrections --since 14d --ids-only

# More results
aise messages corrections --limit 50

# JSON output
aise messages corrections --format json
```

### Slash command usage (Claude Code)

Count slash commands you've invoked across all sessions:

```bash
# Auto-discover ALL slash commands (no config needed)
aise messages planning

# Filter by project or date range
aise messages planning --project myproject --since 2026-01-01

# JSON output
aise messages planning --format json

# Use specific regex patterns instead of auto-discovery
aise messages planning --commands "/ar:plannew,/ar:pn"
```

The default **discovery mode** finds every message that starts with `/command` — `/ar:plannew`,
`/commit`, `/help`, whatever you've used. No configuration required.

`slash` is an alias for `commands` — `aise slash list` = `aise commands list`.

### Slash command invocations (Claude Code)

List every individual slash command invocation with timestamp, session, and arguments:

```bash
# List all slash command invocations
aise commands list --since 14d

# Filter to a specific command
aise commands list --command /ar:plannew --since 14d

# Session IDs only (for piping)
aise commands list --command /ar:plannew --ids-only

# JSON output
aise commands list --format json --since 14d
```

### Slash command context (Claude Code)

See what Claude did after each invocation of a command:

```bash
# Show 5 messages after each /ar:plannew invocation
aise commands context /ar:plannew --context-after 5

# JSON output
aise commands context /ar:plannew --format json

# Limit content length
aise commands context /ar:plannew --max-chars 500
```

### Export to markdown

```bash
# Export one session to stdout
aise export session ab841016

# Redirect to a file
aise export session ab841016 > session.md

# Write to a file explicitly
aise export session ab841016 --output session.md

# Preview without writing
aise export session ab841016 --dry-run

# Export all sessions from the last 7 days to a single file
aise export recent --output week.md

# Last 14 days, filtered by project
aise export recent 14 --project myproject --output week.md
```

System messages (`[Request interrupted`, `<task-notification>`, `<system-reminder>`) are filtered
out automatically.

### Session inspection and timeline (Claude Code)

```bash
# Per-session breakdown: message counts, tool usage, files touched
aise messages inspect ab841016

# Chronological timeline with type/tool filtering
aise messages timeline ab841016
aise messages timeline ab841016 --type user --grep "error"

# Extract specific content types from a session
aise messages extract ab841016 pbcopy
```

### Date format reference

```bash
# Show all supported date/time formats for --since, --until, --when
aise dates
```

Accepted formats: ISO dates (`2026-01-15`), relative (`7d`, `2w`, `3m`), EDTF periods
(`2026-01/2026-03`, `202X` for decades), and natural-language shortcuts.

### Root-level aliases

These are shortcuts — identical to their subcommand equivalents:

```bash
aise extract cli.py          # = aise files extract cli.py
aise history cli.py          # = aise files history cli.py
aise get ab841016            # = aise messages get ab841016
aise search files ...        # = aise files search ...
aise search messages ...     # = aise messages search ...
```

### Search across files and messages at once

```bash
# Find Python files AND messages mentioning "error" in one command
aise search --pattern "*.py" --query "error"

# Scope any search to one session
aise search --session ab841016 --query "error"

# Tool calls with a query (auto-routes to messages)
aise search --tool Write --query "login"
aise search tools --tool Bash --query "git"

# 'find' is an alias for 'search'
aise find files --pattern "*.py"
aise find --tool Write --query "login"
```

---

## Analysis pipeline

After weeks of AI-assisted development you have hundreds of sessions — which ones solved hard
problems? Which techniques worked best? `aise analyze` turns that pile of raw sessions into a
searchable, ranked knowledge base you can browse by project, technique, or role.

Most `aise` commands (`list`, `search`, `stats`, `files`, etc.) work immediately with no config.

```bash
# 1. Run the full pipeline — works out of the box
aise analyze

# 3. Browse results in your org_dir
#    - INDEX.md          → ranked table of all sessions with utility scores
#    - SESSIONS_FULL.md  → detailed per-session breakdowns
#    - 01_by_project/    → symlink taxonomy organized by project
#    - 02_by_technique/  → sessions grouped by technique (CoT, few-shot, etc.)
#    - SESSION_GRAPH.json → provenance graph (which sessions branched from which)
#    - VOCABULARY_ANALYSIS.md → recurring prompt patterns across all sessions

# 4. Drill into interesting sessions using aise commands
aise list --since 7d                          # find recent sessions
aise messages search "authentication"         # search across all sessions
aise get ab841016                             # read a specific session
aise files search "*.py"                      # find files Claude wrote/edited
aise files extract cli.py --session ab841016  # recover a file version
aise stats --provider aistudio                # per-source summary
```

Output goes to `org_dir` — by default inside the app config directory
(`~/Library/Application Support/ai_session_search/organized/` on macOS,
`~/.config/ai_session_search/organized/` on Linux). Override with `--org-dir` or set `org_dir`
in `config.json`. After completion, `aise analyze` prints the output directory and key files.

Re-running is idempotent — stages whose inputs haven't changed are skipped.

```bash
# Show which stages are stale vs current
aise analyze --status

# Force re-run all stages
aise analyze --force

# Narrow to one provider
aise analyze --provider aistudio

# Override output directory
aise analyze --org-dir ~/my_org_dir

# Choose organize output format(s): symlinks (default), json, markdown
aise analyze --format symlinks,json

# Set marker-matching window (chars of user text to scan, 0 = use config default)
aise analyze --window 5000
```

`aise analyze` runs all stages in the correct order (`analyze` → `graph` → `organize`, plus
`instruction-history` if configured). Each stage's output feeds the next. Individual stage
commands exist (`--step`, `aise graph`, etc.) but stages have ordering dependencies so
`aise analyze` is the right way to run the pipeline.

### Pipeline stages

| Stage | Input | Output |
|-------|-------|--------|
| `analyze` | All session files | `session_db.json`, `VOCABULARY_ANALYSIS.md` |
| `graph` | `session_db.json` | `SESSION_GRAPH.json` |
| `organize` | `session_db.json` + `SESSION_GRAPH.json` | Symlink taxonomy, `INDEX.md`, `SESSIONS_FULL.md` |
| `instruction-history` | Gemini CLI session (if configured) | `USER_INSTRUCTIONS_CLEAN.md` |

`org_dir` defaults to the app config directory (see above). Override with `--org-dir` or set
`org_dir` in `config.json` (run `aise config init` to create a starter config).

### Analysis methodology

- **Content analysis** (inspired by [Directed Content Analysis, Hsieh & Shannon 2005](https://journals.sagepub.com/doi/10.1177/1049732305276687)):
  pattern-matching sessions against `CODEBOOK.md` markers for technique, role, and task categories
- **Scoring** (custom weighted system; detects [Chain-of-Thought prompting](https://arxiv.org/abs/2201.11903) patterns per Wei et al. 2022):
  utility scores from technique markers, role signals, and session complexity
- **Provenance graphing**: session lineage detection via filename patterns (`Branch of X`,
  `Copy of X`, `Name vN`) and TF-IDF similarity
- **Vocabulary mining**: n-gram analysis of recurring prompt patterns across all user turns

---

## Configuration

`aise` works out of the box for Claude Code — no config file required. The config lets you add AI
Studio and Gemini CLI sources, customize the analysis pipeline, and override detection patterns.

### Config file location

| Priority | Source |
|----------|--------|
| 1 | `--config /path/to/config.json` CLI flag |
| 2 | `AI_SESSION_SEARCH_CONFIG` environment variable |
| 3 | macOS default: `~/Library/Application Support/ai_session_search/config.json` |
| 3 | Linux default: `~/.config/ai_session_search/config.json` |

### Config commands

```bash
# Show the config file path (whether or not it exists)
aise config path

# Show current configuration (file contents + source)
aise config show
aise config show --format json

# Create a starter config.json with documented defaults
aise config init

# Overwrite an existing config file
aise config init --force
```

### Config file format

```json
{
  "source_dirs": {
    "aistudio": [
      "~/Downloads/Google AI Studio",
      "~/Downloads/drive-download-20260220T174026Z/Google AI Studio"
    ],
    "gemini_cli": "~/.gemini/tmp"
  },
  "org_dir": "~/Downloads/aistudio_sessions/organized",
  "vocab_output_filename": "VOCABULARY_ANALYSIS.md",
  "gemini_org_task_session": "session-2026-02-23T04-07-bd7e3697",
  "scoring_weights": {
    "technique": 20,
    "role": 15,
    "thinking_budget": 30,
    "anti_ai": 35,
    "version_multiplier": 10,
    "corrected_bonus": 5,
    "descendant_boost": 15,
    "tfidf_similarity_threshold": 0.70,
    "min_session_text_len": 50,
    "min_ngram_freq": 3
  },
  "taxonomy_dimensions": [
    {
      "name": "01_by_project",
      "match": "keyword_map",
      "keyword_map": "project_map",
      "prefer_for_links": true
    }
  ],
  "correction_patterns": [
    "regression:you deleted",
    "regression:you removed",
    "skip_step:you forgot",
    "misunderstanding:that's wrong",
    "incomplete:also need"
  ],
  "planning_commands": [
    "/ar:plannew",
    "/ar:planrefine"
  ]
}
```

**`source_dirs.aistudio`** — list of paths to AI Studio session directories. Strings or list of
strings.

**`source_dirs.gemini_cli`** — path to the Gemini CLI tmp directory (usually `~/.gemini/tmp`).

**`org_dir`** — output directory for the analysis pipeline. Defaults to `organized/` inside
the app config directory if not set.

**`scoring_weights`** — configurable weights for the empirical scoring stage. All keys are
optional; missing keys fall back to the built-in defaults shown above.

**`taxonomy_dimensions`** — list of taxonomy dimension configs controlling how sessions are
organized into symlink subdirectories by `aise analyze --step organize`. Each dimension has:
- `name`: directory name under `org_dir`
- `match`: `"keyword_map"`, `"field"`, or `"list_field"`
- `keyword_map`: name of JSON keyword map file in `org_dir` (for `match: keyword_map`)
- `field`: field name on `SessionRecord` (for `match: field`)
- `scalar`: `true` if the field is a single string (not a list)
- `exclude`: list of values to skip (e.g. `[""]` to skip empty strings)
- `fallback`: category name for unmatched sessions (omit to skip unmatched)
- `prefer_for_links`: `true` to use this dimension for INDEX.md links

**`correction_patterns`** — list of `"CATEGORY:KEYWORD"` strings for correction detection.
Overrides built-in patterns when set.

**`gemini_org_task_session`** — session filename stem for the Gemini CLI instruction-history
extraction stage (e.g. `"session-2026-02-23T04-07-bd7e3697"`). Only needed for the
`instruction-history` pipeline step.

### Priority chain

Every setting follows: **CLI flag > environment variable > config file > built-in default**

---

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CLAUDE_CONFIG_DIR` | `~/.claude` | Base Claude config directory |
| `AI_SESSION_SEARCH_PROJECTS` | `~/.claude/projects` | Path to Claude session folders |
| `AI_SESSION_SEARCH_RECOVERY` | `~/.claude/recovery` | Recovery output path |
| `AI_SESSION_SEARCH_CONFIG` | OS config dir | Config file path |

```bash
# Use an external drive
aise --claude-dir /Volumes/External/.claude files search

# Point at a non-default projects directory
AI_SESSION_SEARCH_PROJECTS=/data/claude-sessions aise list
```

---

## Command ordering

Both orderings are equivalent — use whichever reads more naturally:

```bash
aise search files --pattern "*.py"     # "search" then narrow to "files"
aise files search --pattern "*.py"     # start in "files" then "search"

aise search messages --query "error"   # "search" then "messages"
aise messages search "error"           # "messages" then "search" (positional query)

aise find files --pattern "*.py"       # "find" is a root alias for "search"
aise files find --pattern "*.py"       # "find" in files subapp

aise tools search Write                # tools subapp with positional tool name
aise search --tool Write               # root search with --tool flag (equivalent)
```

The `--provider` flag works at the root level or per-command:

```bash
aise --provider claude list            # global flag before subcommand
aise list --provider claude            # per-command flag (same result)
```

---

## Legacy Python scanner API (transitional)

The classes in this section remain available while the legacy Typer CLI is being
migrated. They parse provider files in Python and are not recommended for new
integrations. Use `SessionSearch` above for the shared Rust implementation.

```python
from pathlib import Path
from ai_session_search import SessionRecoveryEngine, FilterSpec

engine = SessionRecoveryEngine(
    Path.home() / ".claude" / "projects",
    Path.home() / ".claude" / "recovery",
)

# List sessions (since= is the canonical date flag; --after is a hidden alias)
sessions = engine.get_sessions(project_filter="myproject", since="2026-01-01")
for s in sessions:
    print(f"{s.session_id[:16]}  {s.git_branch}  {s.message_count} messages")

# List all files Claude ever wrote or edited
all_files = engine.search("*")
for f in all_files:
    print(f"{f.name}  ({f.edits} edits, last modified {f.last_modified})")

# Filter to heavily-edited Python files
filters = FilterSpec(min_edits=5)
filters.with_extensions(include={"py"})
results = engine.search("*", filters)

# Search conversation text
matches = engine.search_messages("authentication")
for msg in matches:
    print(f"[{msg.type.value}] {msg.session_id}: {msg.content[:100]}")

# Search tool invocations
write_calls = engine.search_messages("", tool="Write")
bash_git = engine.search_messages("git commit", tool="Bash")

# Find user corrections
corrections = engine.find_corrections(project_filter="myproject", limit=20)
for c in corrections:
    print(f"{c.category}: {c.matched_pattern} — {c.content[:80]}")

# Count slash command usage (auto-discovery)
planning = engine.analyze_planning_usage()  # discovers all /command patterns
for p in planning:
    print(f"{p.command}: {p.count} uses across {len(p.session_ids)} sessions")

# Cross-reference session edits against current file
current = Path("cli.py").read_text()
refs = engine.cross_reference_session("cli.py", current)
for r in refs:
    mark = "+" if r["found_in_current"] else "-"
    print(f"{mark} {r['tool']} {r['timestamp'][:10]}: {r['content_snippet'][:60]}")

# Export a session to markdown
md = engine.export_session_markdown("ab841016")
Path("session.md").write_text(md)

# Get all recorded versions of a file
versions = engine.get_versions("cli.py")
for v in versions:
    print(f"v{v.version_num}: {v.line_count} lines  {v.timestamp}  session {v.session_id[:16]}")

# Summary statistics
stats = engine.get_statistics()
print(f"{stats.total_sessions} sessions, {stats.total_files} files, {stats.total_versions} versions")
```

### Multi-source usage

```python
from ai_session_search import get_session_backend, AiStudioSource, GeminiCliSource

# Auto-detect all configured sources (Claude Code, AI Studio, Gemini CLI)
backend = get_session_backend()

# List sessions from all sources
sessions = backend.get_sessions()

# Date filtering — same flags as CLI (since/until/EDTF patterns supported)
recent = backend.get_sessions(since="7d")           # last 7 days
decade = backend.get_sessions(since="202X")         # EDTF: whole 2020s decade

# Narrow to one source
aistudio_backend = get_session_backend(source="aistudio")
sessions = aistudio_backend.get_sessions()

# Search messages across all sources
results = backend.search_messages("transcription")

# Use AiStudioSource and GeminiCliSource directly
ai_source = AiStudioSource([Path("~/Downloads/Google AI Studio").expanduser()])
for session_info in ai_source.stream_sessions():
    messages = ai_source.read_session(session_info)
    print(f"{session_info.session_id}: {len(messages)} messages")
```

### Date parsing utility

```python
from ai_session_search import parse_date_input

# All the same formats the CLI accepts
parse_date_input("2026-01-15")        # → "2026-01-15T00:00:00" (start mode)
parse_date_input("7d")                # → ISO datetime 7 days ago
parse_date_input("202X", mode="end")  # → "2029-12-31T23:59:59" (end of decade)
parse_date_input("2026-01/2026-03")   # → ("2026-01-01T00:00:00", "2026-03-31T23:59:59")
```

### Configuration API

```python
from ai_session_search import load_config, write_config, get_config_path

# Read current config (respects --config flag > AI_SESSION_SEARCH_CONFIG env > OS default)
cfg = load_config()

# Add an AI Studio source directory
cfg.setdefault("source_dirs", {})["aistudio"] = ["/path/to/Google AI Studio"]
write_config(cfg)

# Check where the config file lives
print(get_config_path())  # e.g. ~/Library/Application Support/ai_session_search/config.json
```

### Key classes

| Class | Description |
|-------|-------------|
| `SessionSearch` | Canonical Rust application root for catalog, message, file, analysis, export, and lifecycle services |
| `QueryScope` | Shared provider, session, path, and date constraints |
| `SessionQuery` | Bounded session list/search request |
| `MessageQuery` | Bounded message request combining scope and selector |
| `MessageSelector` | Role, kind, sequence, tool, compaction, and search-target constraints |
| `FileQueryRequest` | Bounded file history/reconstruction request |
| `AnalysisQuery` | Bounded corrections, planning, and role-statistics request |
| `DateRangeQuery` | Shared `since`, `until`, or `when` date expression |
| `SessionRecoveryEngine` | Transitional Claude-only Python scanner used by the legacy CLI |
| `AiStudioSource` | Transitional AI Studio Python scanner |
| `GeminiCliSource` | Transitional Gemini CLI Python scanner |
| `SessionInfo` | One session: `session_id`, `project_dir`, `git_branch`, `cwd`, `message_count` |
| `RecoveredFile` | One source file: `name`, `edits`, `last_modified`, `sessions` |
| `FileVersion` | One file version: `version_num`, `line_count`, `timestamp`, `session_id`, `tool`, `lines_added`, `lines_deleted` |
| `SessionMessage` | One conversation message: `type` (user/assistant), `content`, `timestamp` |
| `CorrectionMatch` | One correction: `category`, `matched_pattern`, `content`, `session_id` |
| `PlanningCommandCount` | One slash command: `command`, `count`, `session_ids`, `project_dirs` |
| `RecoveryStatistics` | Aggregate counts: `total_sessions`, `total_files`, `total_versions` |

---

## Project structure

```
ai_session_search/
├── __init__.py          # Public API — all exports listed here
├── engine.py            # SessionRecoveryEngine, MultiSourceEngine, SessionBackend
├── config.py            # Canonical config loader (respects --config flag)
├── models.py            # Data classes: SessionInfo, SessionMessage, RecoveredFile, etc.
├── filters.py           # SearchFilter chain and filter predicates
├── formatters.py        # Output as table, JSON, CSV, or plain text
├── types.py             # Protocol interfaces (Storage, Searchable, etc.)
├── cli.py               # CLI commands (thin wrappers over the engine)
├── sources/
│   ├── __init__.py
│   ├── aistudio.py      # AiStudioSource: AI Studio JSON + legacy .md sessions
│   └── gemini_cli.py    # GeminiCliSource: Gemini CLI ~/.gemini/tmp sessions
└── analysis/
    ├── __init__.py
    ├── analyzer.py       # Content analysis (pattern-matching inspired by qualitative coding) + scoring
    ├── codebook.py       # CODEBOOK.md loader, n-gram helpers, scoring utilities
    ├── extract.py        # Gemini CLI instruction-history extraction
    ├── graph.py          # Session provenance graph builder
    ├── orchestrator.py   # Taxonomy symlinks, INDEX.md, SESSIONS_FULL.md
    ├── pipeline_state.py # Idempotent pipeline change detection
    └── vocab.py          # Standalone vocabulary mining
```

---

## Development

```bash
git clone https://github.com/ahundt/ai-session-search.git
cd ai_session_search

# Install with dev dependencies
uv sync --all-extras

# Run tests (integration tests require ~/.claude/projects/)
uv run pytest                    # unit tests only (fast)
uv run pytest -m ""             # all tests including integration

# Lint and format
uv run ruff check ai_session_search/
uv run ruff format ai_session_search/

# Type check
uv run mypy ai_session_search/
```

---

## License

Apache License 2.0 — Copyright (c) 2026 Andrew Hundt
