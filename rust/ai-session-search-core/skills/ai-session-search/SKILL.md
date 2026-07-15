---
name: ai-session-search
description: Search, recover, inspect, export, and analyze local AI session history with AI Session Search (`aise`) across Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI. Use when asked to "find prior AI work", "recover context after compaction", "inspect tool calls or corrections", "reconstruct a file", "export a session", "analyze repeated mistakes", or turn session evidence into durable agent guidance.
---
<!-- ai-session-search-managed-skill v1 -->

# AI Session Search

Use `aise` instead of scanning raw provider files. It normalizes eight providers into one indexed
CLI, Rust API, Python API, and MCP service.

## Start safely

```sh
aise --version
aise doctor
aise paths
```

If a command or option is uncertain, run `aise <command> --help`. Do not guess old aliases.
Use canonical session IDs from results, such as `codex:<id>` or `claude:<id>`.

The configuration hierarchy is:

1. CLI option
2. `AI_SESSION_SEARCH_*` environment variable
3. `config.toml`
4. embedded default

Inspect it with `aise config path`, `aise config show`, and `aise config explain`.

## How It Works

1. Search whole sessions when the remembered evidence is broad; search messages when exact turns,
   roles, tool calls, or surrounding context matter.
2. Carry the returned canonical session ID and message sequence into focused evidence commands.
   Start bounded, then expand only the relevant page, window, or turn.
3. Export, recover, or analyze only after the evidence identifies the required scope. Publication
   commands use new non-replacing destinations; recovery never overwrites the original file.
4. Convert repeated, independently supported corrections into the narrowest durable guidance, then
   search later sessions to verify whether the failure recurs.

## Detailed Workflow

Select the smallest workflow that answers the request, then use the examples below as canonical
command shapes. Run the relevant `--help` before adding options not shown here.

## Examples

### Find sessions by topic

```sh
aise search "database migration" --path ~/source/project --when 30d --limit 10
aise list --provider codex --when 7d --limit 20
```

`aise search` ranks whole sessions. Use it when the topic, title, repository, or remembered phrase
is enough. Use `--provider` only for one of:

```text
claude, claude-desktop, codex, cursor, antigravity, pi, aistudio, gemini-cli
```

### Find exact turns and their context

```sh
aise messages search "foreign key" --path ~/source/project --limit 20 --context 2
aise messages search 'timeout|lock|busy' --regex --limit 20 --lines-per-message 4
aise messages search "approximate remembered wording" --fuzzy --limit 20
aise messages search misunderstood --role user --when 14d --limit 20 --context 2
```

Exact literal matching is the default. Use `--regex` for Rust regex syntax and `--fuzzy` for
remembered wording or typos. A hit is identified by `(session_id, seq)`.

`--context N` adds neighboring turns even when they have other roles; in plain output, `*` marks
the actual hit. `--lines-per-message N` changes presentation only: positive keeps the first N
lines, negative keeps the last N, and zero keeps complete content. It does not change matches,
ranking, result count, pagination, context membership, or reference extraction.

Expand one hit without reading the full transcript:

```sh
aise messages get SESSION_ID --seq 42 --context 3 --refs --lines-per-message 8
```

### Inspect one session

```sh
aise messages evidence SESSION_ID --summary-items -12 --include time-profile
aise show SESSION_ID --transcript-lines -40
aise show SESSION_ID --transcript-lines 0
aise resume SESSION_ID
```

Use compact evidence first. Signed windows are explicit:

- Positive: first N records or transcript lines.
- Negative: last N records or transcript lines.
- Zero: all records or lines; this may be large.

Use `aise messages get --seq` for one turn instead of increasing a transcript window.

### Export sessions

```sh
aise export SESSION_ID --format markdown --output session.md
aise export --path ~/source/project --when 7d --limit 20 --output-dir /absolute/new/directory
```

A single export may write one file or stdout. A filtered bundle publishes a new immutable directory
and requires an explicit bound unless all matching sessions are genuinely required.

### Recover edited files

```sh
aise files search '*.rs' --path ~/source/project --limit 50
aise files history src/db.rs --path ~/source/project
aise files cross-ref src/db.rs --path ~/source/project
aise files extract src/db.rs --session-id SESSION_ID --dry-run
aise files extract src/db.rs --session-id SESSION_ID --restore
```

`files extract --restore` writes a collision-safe `.recovered` sibling and never overwrites the
original. Use `--output-dir` for an explicit destination. Use `--all` only when every reconstructable
version is required.

### Analyze repeated behavior

```sh
aise corrections --path ~/source/project --when 30d --limit 50
aise planning --path ~/source/project --when 30d --limit 50
aise stats --path ~/source/project --when 30d
aise repeats --path ~/source/project --when 30d
aise analyze --provider codex --when 7d --limit 50 --output /absolute/new/analysis
```

Use precise correction searches such as `misunderstood`, `wrong repo`, `you forgot`, and
`should have`, then add `--context 2`. Broad terms such as `mistake` have higher recall but often
match general discussion rather than a concrete correction. Treat mirrored provider records as
correlated evidence unless their content proves they are independent conversations.

Publish analysis only to a new absolute directory. Add `--policy` only for a validated JSON
`AnalysisPolicySpec`; omit it for structural graph/taxonomy analysis.

## Turn evidence into durable guidance

1. Search for a repeated correction with bounded results and context.
2. Record exact session IDs and sequence numbers.
3. Distinguish measured evidence from inference.
4. Identify the narrowest durable target: repository guidance, an existing skill, a hook, or code.
5. Add one concrete rule that names the action, scope, and exception.
6. Search later sessions for the same correction to verify whether recurrence decreased.

Do not turn one incidental match into a global rule. Do not count mirrored sessions as independent
failures. Keep sandbox and approval policy in agent/autorun guidance, not AI Session Search product
documentation.

## Control index freshness deliberately

The default `--index-refresh auto` keeps a compatible existing index available and performs needed
incremental maintenance automatically. Use:

- `--index-refresh before-query` when this query must wait for current source files to be indexed.
- `--index-refresh existing-only` for a reproducible read of the current compatible index with no
  implicit refresh.
- `aise reindex` for an explicit incremental rebuild.
- `aise reindex --full` only when a complete reparse is required.

Do not run `compact` automatically. Check `aise doctor` and available disk first; compaction mutates
the database and may need temporary space.

## Use MCP when available

The MCP server key and protocol identity are `ai_session_search`. It exposes seven tools:

- `search_sessions`: ranked whole-session search.
- `get_session`: compact evidence, transcript window, or focused message context.
- `list_sessions`: recent sessions newest first.
- `get_resume_command`: native resume arguments or exact fallback guidance.
- `search_messages`: exact, regex, or fuzzy turn search with deterministic pages.
- `get_index_status`: database, parser, provider, and automatic-update health.
- `query_session_index`: expert read-only SQL and schema inspection.

Prefer `search_messages` over raw SQL for content search because it uses the FTS/trigram planner and
returns context. MCP pages are bounded by default. Use returned `next_offset` values for
non-overlapping pages; request zero/unlimited only when the complete response is safe for the client.
