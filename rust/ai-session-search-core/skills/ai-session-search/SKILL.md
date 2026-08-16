---
name: ai-session-search
description: Search, recover, inspect, export, and analyze local AI session history with AI Session Search (`aise`) across Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Prime Agent, Google AI Studio, and Gemini CLI. Use when asked to "find prior AI work", "recover context after compaction", "inspect tool calls or corrections", "reconstruct a file", "export a session", "analyze repeated mistakes", or turn session evidence into durable agent guidance.
metadata:
  version: 1.0.0-rc.2
---
<!-- ai-session-search-managed-skill v1 -->

# AI Session Search (`aise`)

Use `aise` instead of scanning raw provider files. It normalizes nine providers into one index.
The local Codex provider covers ChatGPT Codex desktop plus Codex CLI/IDE through
their shared `~/.codex` host. Claude Code CLI/Desktop share the Claude Code
provider, while Claude Desktop local-agent sessions use `claude-desktop`.
Antigravity App/IDE/CLI share the `antigravity` provider. Cloud-only account
history is outside this local index.

If this workflow reveals a needed implementation change, inspect the project's manifests, lockfiles, imports, and existing abstractions before writing infrastructure.
Reuse a fitting dependency first; otherwise adopt a mature, widely used library only after verifying contract, lifecycle, platform, performance, dependency, and release
fit and confirming that it removes more custom machinery than it adds.

## Start safely

```sh
aise --version
aise doctor
aise config paths
```

If a command or option is uncertain, run `aise <command> --help` and take the spelling from its
output. Use canonical session IDs from results, such as `codex:<id>` or `claude:<id>`.

The configuration hierarchy is:

1. CLI option
2. `AI_SESSION_SEARCH_*` environment variable
3. `config.toml`
4. embedded default

Inspect it with `aise config file`, `aise config show`, and `aise config origins`.

## Prefer MCP when available

The MCP server key and protocol identity are `ai-session-search`, with display title
**AI Session Search**. Route requests by evidence type:

1. Use `query_session_index` for read-only SQL, schema inspection, counts, grouping, time-series,
   and cross-provider questions beyond the higher-level tools. It is the priority tool for
   structured index analysis; `search_messages` owns full-text content search. Read the `note` each
   column carries in `query_session_index(schema_table=...)` before writing a predicate over it:
   several answer a different question than their name suggests, notably `messages.tool_name`,
   which holds the provider's own spelling, namespaced on Claude and a bare leaf name on Codex.
   *(~0.01 s indexed aggregate; >1 s full scan; `O(R)` rows scanned.)*
2. Use `search_messages` for exact, regex, or fuzzy search over message content, canonical tool
   names, or one tool-argument JSON pointer. Start with the shortest discriminating fragment; add
   words only when results are ambiguous.
   *(Exact/regex can use selective indexes, including JSON-safe tool-argument anchors. For `N`
   eligible rows with `T` total selected-field characters and page window `W`, fuzzy uses
   `O(T + N + W log W)` aggregate work in bounded parallel batches. Peak memory includes one
   bounded batch plus its largest row, retained `W` text, and per-worker matcher/query scratch.
   Short or unsafe anchors fall back to the filtered corpus; fuzzy tool arguments remain exhaustive.)*
3. Use `search_sessions` for broad topics, titles, repositories, or remembered phrases.
   *(For `N` eligible sessions, `T` total field/transcript characters, `A` query tokens, and positive
   result limit `K`, scoring uses `O(T*(A+1) + N + K log K)` aggregate work in bounded parallel
   batches. Peak memory is retained `K` records/text plus the batch budget and largest session.)*
4. Pass a returned session ID and optional sequence to `get_session` for bounded evidence.
   *(~0.01 s focused; `O(log M + C)` message lookup and context after an `O(S)` id resolve.)*

The remaining tools are `list_sessions`
*(~0.01 s; favorable indexed `O(log S + offset + K)` because paging uses SQL `OFFSET`)*,
`get_resume_command` *(`O(S)` id scan, milliseconds in practice)*, and `get_index_status`
*(~0.2 s warm; `O(F)` discovered source files)*. MCP pages are bounded by default; follow
`next_offset` for non-overlapping pages. Use the CLI for export, file recovery, or when MCP is
unavailable. Timings are one-significant-figure warm-cache measurements on this machine; MCP
transport and result size add overhead.

Choose the first MCP call by what is known, then narrow:

- Recent sessions by metadata alone (a directory, a provider, a period):
  `list_sessions(path_prefix, when, limit)`, newest first; then `get_session(session_id,
  transcript_lines=-40)` or `get_session(session_id, summary=true)` reads one.
- Unknown session, remembered topic: `search_sessions(query, path_prefix, when, limit)`.
- Exact phrase, identifier, correction, or tool activity: `search_messages(query,
  workspace_path_prefix, role/kinds/tool_name_contains, context, limit, lines_per_message)`; skip
  session discovery when the string is already distinctive. Omit `query` to list every message
  the filters select; pass `all_results=true` instead of `limit` for every match of a literal or
  regex query, and expect a bounded-ceiling refusal with retry guidance when that is too large.
- Returned hit: pass its `session_id` and `seq` as `get_session(session_id, message_seq, context)`.
- Aggregate or relationship question: inspect the schema, read each column's `note`, then use
  bounded `query_session_index(sql, limit, timeout_ms, max_cell_chars)`; content search stays with
  `search_messages`. Counts differ by surface: `aise stats` omits harness notices while `aise vocab`
  and a raw `group by role` count them, and one `tool_call_id` can appear on several rows, so
  count distinct ids rather than rows.

## Search, focus, then act

1. Use the scope name owned by each command: message search uses `--workspace-path`; broad search and analytics use `--path`; MCP message search uses
   `workspace_path_prefix`. Also use `--provider` and `--when` when those facts are known.
2. Search one concept with a short distinctive fragment. Add filters before making it longer.
3. Use `aise search` for broad topics and `aise messages search` for exact turns or tool calls.
4. Carry the returned `(session_id, seq)` into `messages get`, `messages evidence`, or `show`;
   expand only the relevant turn or transcript edge.
5. Once a hit supplies an exact path, identifier, or sequence, inspect that target directly instead
   of broadening the search.
6. Export, recover, or analyze the identified scope. Use a new destination, and promote only
   repeated independent corrections into durable guidance.

## Choose the evidence path

- Recover prior context: `search` -> `messages evidence` -> `messages get` for the decisive turn.
- Find an exact correction or tool call: `messages search --context 2` -> `messages get --seq`.
- Analyze recurrence: `skills corrections` or `planning` -> record session IDs and sequences -> search
  later sessions for the same behavior.
- Require newly indexed source files: add `--index-refresh before-query`; use `existing-only` for a
  reproducible read of the current index.

For broad, ambiguous, multi-provider, or long-range research that would consume substantial main
context, delegate the search to a smaller harness subagent when the harness supports delegation.
Read
[references/recover-prior-work-with-evidence.md](references/recover-prior-work-with-evidence.md)
before writing the assignment. A distinctive exact lookup completes fastest in the current agent.

Run the relevant `--help` before adding an option; every option below is current.

Time periods and relative days are UTC. Session list/search/export/analysis date filters intersect
the inclusive query period with each known indexed span from `created_at` through `updated_at`.
`since` tests the span end, `until` tests the span start, and `when` tests both. An exact RFC 3339
value is a zero-width period. A span records first and last known activity and may contain gaps.
Message, file, and event-analytics dates still test each event timestamp. For an exact event
boundary, pass a timestamp with `Z` or an explicit offset; fractional seconds are preserved for
same-second event ordering.

## Commands by task

### List recent sessions by metadata, then read them

```sh
# The 10 most recent sessions whose working directory is this project, this week.
aise list --path ~/source/project --when 7d --limit 10
# The same rows as JSON: id, provider, cwd, created/updated timestamps, message count, preview.
aise list --path ~/source/project --when 7d --limit 10 --format json
# Newest session in this directory or a component-boundary descendant of it.
aise list --path ~/source/project --limit 1
aise list --provider codex --when 7d --limit 20
# Then read one: bounded evidence, the last 40 transcript lines, or its newest user turns.
aise messages evidence SESSION_ID --summary-items -12
aise show SESSION_ID --transcript-lines -40
aise messages get SESSION_ID --role user --limit 20 --order newest
```

`aise list` orders eligible sessions newest first before applying `--limit`; it is the query-less
lookup, and a query-less `aise search` prints the equivalent `aise list` command. Path matching
includes the exact directory and descendants and leaves lexical siblings such as `project-other`
out. MCP: `list_sessions(path_prefix, when, limit)` returns the same rows newest first and
`get_session` reads one.

### Find sessions by topic

```sh
aise search "database migration" --path ~/source/project --when 30d --limit 10
```

`aise search` ranks by the query across titles, previews, and transcripts; use it when the topic,
title, repository, or remembered phrase is known. Provider names are:

```text
claude, claude-desktop, codex, cursor, antigravity, pi, prime-agent, aistudio, gemini-cli
```

### Find what a subagent did

```sh
# Only delegated work, across every provider that records it.
aise search "flaky test" --session-kinds subagent --when 7d --limit 20
# Only sessions you started, so the listing shows the sessions a person began.
aise list --session-kinds user --when 7d --limit 20
# Everything one session delegated. Pass the parent's full id, as `aise list` prints it.
aise list --parent-session claude:7e745098-c299-4cf5-bdbe-5cdb1fb5a62d
```

A run spawned by another session is indexed as a session of its own, so what a subagent was
asked and what it found is searchable. Each carries `parent_session_id` (the spawning session's
whole id) and `agent_label` (`Explore`, `general-purpose`, a Codex agent nickname). Both classes
come back by default. `--session-kinds` is the single class filter and accepts several values;
`--session-kind` selects one. The values are the providers' own — Codex records this same
distinction as `thread_source: user | subagent`.

### Find exact turns and their context

```sh
aise messages search "foreign key" --workspace-path ~/source/project --limit 20 --context 2
aise messages search 'timeout|lock|busy' --query-mode regex --limit 20 --lines-per-message 4
aise messages search "approximate remembered wording" --query-mode fuzzy --limit 20
aise messages search misunderstood --role user --when 14d --limit 20 --context 2
aise messages search "CANNOT STOP" --kinds harness-notice --when 2d --limit 20
```

To learn why an agent stopped, looped, or was blocked, search `--kinds harness-notice`:
Stop-hook feedback, PreToolUse blocks, local-command caveats, and task notifications are what
the harness told the agent rather than what the user wrote. They are indexed and left out of
ordinary results, so `corrections`, `repeats`, and a user-role search see what people wrote.
`--kinds` is the single class filter and accepts several values; `--kind` selects one.

Literal matching is the default. `--query-mode regex` (MCP: `query_mode=regex`) uses Rust regex
syntax: classes, alternation, anchors, and repetition (look-around is absent). `--query-mode fuzzy`
matches the query's characters in order with gaps between them, so a 3-character-or-longer
fragment and a positive `--limit` are the inputs. Every structurally eligible row is scored before
the deterministic offset and limit slice is selected. A hit is identified by `(session_id, seq)`.

Cost order on one corpus, cheapest first: `list`/`messages get`/`show` (indexed lookups,
milliseconds) < literal or regex with a selective prefilter (word and trigram indexes admit
candidates, then the exact predicate is verified: sub-second on a multi-million-message index for
a rare fragment such as `ECONNRESET`; a common phrase such as `permission denied` costs more
because more candidates are verified) < fuzzy (every structurally eligible row's selected field
is read and scored, `O(T + N)` bytes and rows, so seconds on the same index). Structural filters
(`--workspace-path`, `--session-id`, `--role`, `--when`, `--kinds`, `--tool-name-contains`) shrink
every mode's work before the query runs, so add them before widening a query and before choosing
fuzzy. `--all-results` makes output grow with the match count; `--context N` multiplies rows
returned per hit; `--receipt-level summary|full` adds one index pass for the corpus count.

### Read one session: newest/oldest N, and page without re-reading

```sh
# The 75 most recent user turns. Direction is --order (MCP: order=newest); --limit is a positive count.
aise messages get SESSION_ID --role user --limit 75 --order newest
# A long session in non-overlapping chunks: advance --seq-from and keep --limit fixed.
aise messages get SESSION_ID --seq-from 0 --seq-to 499
aise messages get SESSION_ID --seq-from 500 --seq-to 999
```

`--limit` selects oldest-first unless `--order newest`; order picks WHICH N: newest selects the
last N and prints them oldest-first. To read further, continue from the next seq range
(`seq_from = last seq + 1`) rather than re-requesting a larger `--limit`/`transcript_lines`, which
re-sends what you already read. MCP mirrors this with `get_session(seq_from, seq_to)`;
`search_messages` pages newest-first with `offset`, and `match_window=latest` (one `session_id`)
selects the last occurrence inside each hit rather than the first.

To learn how a search was planned, add `--receipt-level summary` (MCP: `receipt_level=summary`);
`full` adds the origin of every resolved parameter. Exact and regex modes still verify the
requested predicate after indexed candidate retrieval. Fuzzy mode scores the complete
structurally eligible corpus and retains bounded top-K state for the requested page. In the
receipt, `prefilter_skipped` names the reason an exact or regex prefilter was skipped, `corpus` is
every row the structural filters admit, and `candidates` over `corpus` is the selectivity to
improve by anchoring the query on a rarer literal. The `corpus` count reads indexes: one pass over
the smallest index of the message table when no structural filter is set (a few seconds cold on a
23 GB, 2.7-million-message index, well under a second warm), and only the filtered rows' index
entries with `--role`, `--session-id`, `--workspace-path`, `--when`, or `--tool`. An explicit
`--kinds` set beyond `harness-notice`/`compaction` counts by reading rows. An index written by an
aise before 1.0.0rc2 gains the supporting index at its next refresh. The CLI prints the receipt
with `--format json`; MCP returns it as `receipt` in the structured output.

Search canonical tool fields without scanning rendered output conventions:

```sh
aise messages search exec --field tool-name --query-mode fuzzy --limit 20
aise messages search "cargo test" --field tool-argument --argument-path /cmd --limit 20
aise messages search "cargo test" --field tool-argument --argument-path /command --tool-name-contains Bash --limit 20
```

Prefer a short fragment such as `foreign key` or `wrong repo` over a remembered sentence. Short
queries tolerate wording differences and expose more candidate turns; use `--workspace-path`,
`--role`, `--when`, and `--context` to narrow message results before lengthening the text query.

Literal mode matches the query text as typed: `stdout OR printf` searches those exact words, and
`--query-mode regex 'stdout|printf'` searches the alternatives. Word boundaries around a short
identifier (`--query-mode regex '\baise\b'`) match the word `aise` alone. The current
message-search names are `--workspace-path`, `--role`, `--query-mode`, and `--receipt-level`
(earlier releases spelled them `--project`, `--type`, `--regex`/`--fuzzy`, and `--explain`);
`--help` confirms a saved command before it is reused. Output formats are `table`, `json`, `jsonl`,
`csv`, and `plain`; `--include` accepts `normalized_session_metadata`, `parsed_references`,
`raw_provider_metadata`, `runtime_diagnostics`, or `none`.

Searching for a string that starts with `-` (a flag name, a diff line, `--path`) needs an escape,
because a bare positional query is parsed as a flag. Put every other flag first, then `--`, then
the query: `aise search --limit 5 -- --path`. `messages search` also accepts `-e`:
`aise messages search -e --path --limit 5`. This applies to `search`, `messages search`, and
`repeats`. The MCP tools take the query verbatim as a JSON string.

`--context N` adds neighboring turns even when they have other roles; in plain output, `*` marks
the actual hit. `--lines-per-message N` changes presentation only: positive keeps the first N
lines, negative keeps the last N, and zero keeps complete content; matches, ranking, result count,
pagination, context membership, and reference extraction stay the same.

For a search expected to return several hits, set `--lines-per-message` before increasing
`--limit`: `--limit` bounds the hit count and `--lines-per-message` bounds each hit's size. To
inspect one known session by itself, use bounded `messages evidence`, `messages get`, or `show`.

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

Use compact evidence first. Signed windows mean:

- Positive: first N records or transcript lines.
- Negative: last N records or transcript lines.
- Zero: all records or lines; this may be large.

Use `aise messages get --seq` for one turn instead of increasing a transcript window.

### Export sessions

```sh
aise export SESSION_ID --format markdown --output session.md
aise export --path ~/source/project --when 7d --limit 20 --output-dir /absolute/new/directory
```

A filtered bundle creates a new immutable directory and requires an explicit bound unless every
matching session is required.

### Analyze repeated behavior

```sh
aise skills corrections --path ~/source/project --when 30d --limit 50
aise planning --path ~/source/project --when 30d --limit 50
aise stats --path ~/source/project --when 30d
aise repeats --path ~/source/project --when 30d
aise analyze --provider codex --when 7d --output /absolute/new/analysis
```

`aise skills corrections` and default/user-role `aise repeats` scan only source-attributable
human text; generated, harness, unknown-authorship, and mirror rows do not count as recurrence.
Corrections default to user-started sessions; pass `--session-kinds user subagent` to include
attributable human text in both session classes. `--format json` returns
a tagged skill-run receipt plus the classification report.

Its deterministic categories live in this skill's adjacent `aise-capability.toml`. Categories
and selected skills are evaluated in declaration order, the first match wins, and every run
reports the exact capability digest. Point `--skill` at another skill directory to add its
categories after these:

```sh
aise skills corrections --skill ./my-review --format json
```

### Create your own classification package

A skill package is a directory holding `SKILL.md` (what the harness reads) and, beside it,
`aise-capability.toml` (what `aise` runs: ordered `[[categories]]` of `name` + `patterns`).
Scaffold one from the shipped categories, validate it, register its parent, then run it:

```sh
aise skills create my-corrections --capability message-classification --output-dir ~/.claude/skills
aise skills validate ~/.claude/skills/my-corrections
# add "~/.claude/skills" to [skills].search_paths in the file `aise config file` prints
aise skills list
aise skills my-corrections --path ~/source/project --when 30d --format json
aise skills corrections --skill ~/.claude/skills/my-corrections --format json   # defaults first, yours after
```

`aise skills show my-corrections` prints where it resolved from and its categories in order.
MCP runs the same package with `run_skill_capability(skill={"name":"my-corrections"})` or
`skill={"path":...}` under a `[skills].search_paths` root, `additional_skills` for more
packages, and `definition={"categories":[{"name":...,"patterns":[...]}]}` for one call's
inline rules. Read [references/message-classification.md](references/message-classification.md)
for the schema, precedence, and pattern guidance before changing categories or regular
expressions.

Search precise correction phrases such as `misunderstood`, `wrong repo`, `you forgot`, and
`should have`, then add `--context 2`. Treat mirrored provider records as correlated unless their
content proves they are independent conversations.

Publish analysis only to a new absolute directory. Add `--policy` only for a validated JSON
`AnalysisPolicySpec`; omit it for structural graph/taxonomy analysis.

## Turn evidence into durable guidance

1. Search with bounded results and context; record session IDs and sequence numbers.
2. Separate measured evidence from inference and mirrored records from independent conversations.
3. Put one concrete rule in the narrowest target: repository guidance, an existing skill, a hook,
   or code.
4. Search later sessions for the same correction to check whether it recurs.

Promote a rule once the same correction appears in independent sessions; a mirrored session
counts as the one conversation it copies. Sandbox and approval policy belong in agent/autorun
guidance; AI Session Search documentation describes the product.

## Control index freshness deliberately

The default `--index-refresh auto` keeps a compatible existing index available and performs needed
incremental maintenance automatically. Use:

- `--index-refresh before-query` when this query must wait for current source files to be indexed.
- `--index-refresh existing-only` for a reproducible read of the current compatible index with no
  implicit refresh.
- `aise reindex` for an explicit incremental rebuild.
- `aise reindex --full` only when a complete reparse is required.

Run `compact` only when asked, after `aise doctor` and a disk check: compaction mutates the
database and may need temporary space. Refresh is separate from query cost: a forced refresh on
this machine exceeded 100 s before cancellation *(incremental `O(B_new)`; full `O(B_all)` bytes)*.

## Lower-priority file recovery

Use this path only when the request is to reconstruct an edited file:
*(~0.01 s search/history, ~0.06 s cross-reference; `O(E + K log K)` filtered edits/results.)*

```sh
aise files search '*.rs' --path ~/source/project --limit 50
aise files history src/db.rs --path ~/source/project --limit 50
aise files cross-ref src/db.rs --path ~/source/project
aise files extract src/db.rs --session-id SESSION_ID --dry-run
aise files extract src/db.rs --session-id SESSION_ID --restore
```

`--restore` writes a collision-safe `.recovered` sibling and leaves the original in place. Use
`--output-dir` for an explicit destination and `--all` only when every reconstructable version is
required.
