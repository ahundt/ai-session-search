---
name: ai-session-search
description: Search, recover, inspect, export, and analyze local AI coding-agent session history with AI Session Search (`aise`) across Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Prime Agent, Google AI Studio, and Gemini CLI. Use when asked to "find prior AI work", "recover context after compaction", "inspect tool calls or corrections", "reconstruct a file", "export a session", "analyze repeated mistakes", "search past conversations", "what did we decide last session", "resume a previous session", or turn session evidence into durable agent guidance.
metadata:
  version: 1.1.0
---
<!-- ai-session-search-managed-skill v1 -->

# AI Session Search (`aise`)

`aise` normalizes nine providers' local session files into one index and answers from that index,
so use it instead of scanning raw provider files. Provider names are `claude, claude-desktop,
codex, cursor, antigravity, pi, prime-agent, aistudio, gemini-cli`: the Codex provider covers
ChatGPT Codex desktop plus Codex CLI/IDE through their shared `~/.codex` host; Claude Code CLI and
Desktop share `claude`, while Claude Desktop local-agent sessions are `claude-desktop`; Antigravity
App/IDE/CLI share `antigravity`. Cloud-only account history is outside this local index.

## 1. Start

For “my earlier messages/corrections”, use `role="user"` on `search_messages`
or CLI `--role user`: answers can have tool role. It finds
recorded prompts, queued submissions and linked answers. For content queries, matching stays in
recorded input regions; known parent-agent prompts sent to spawned sessions are excluded from this
selector. Omitting `role` searches complete content; matching remains on the selected field.
Views and context can include generated text. Provenance distinguishes source and sender/delivery
uncertainty; a queued submission need not have reached the model. If exact searches are empty,
check effective role/date/path filters, try a bounded all-content search, locate the
session from the note's references, and inspect surrounding events. Preserve source distinctions.

```sh
aise --version
aise doctor
aise config paths
```

Take option spellings from `aise <command> --help`; every command below is current. Session IDs
are canonical, such as `codex:<id>` or `claude:<id>`, and a message is located by
`(session_id, seq)`; where two messages share a seq, reading it returns both, in a fixed order.
Configuration precedence is CLI option, then `AI_SESSION_SEARCH_*`
environment variable, then `config.toml`, then the embedded default; `aise config file`,
`aise config show`, and `aise config origins` show it.

## 2. Pick the tool by what you know

The MCP server key and protocol identity are `ai-session-search` (title **AI Session Search**).
The same operations exist on the CLI, so choose by what is known, then narrow:

| You know | CLI | MCP |
| --- | --- | --- |
| Metadata (directory, provider, period) | `aise list --path DIR --when 7d --limit 10` | `list_sessions(path_prefix, when, limit)` |
| A topic or remembered phrase | `aise search "topic"` | `search_sessions(query, path_prefix, when, limit)` |
| An exact string or tool activity | `aise messages search "text"` | `search_messages(query, workspace_path_prefix, role, kinds, context, limit)` |
| A session id | `aise messages evidence ID`, `aise show ID` | `get_session(session_id, summary=true)` or `get_session(session_id, transcript_lines=N)` |
| A `(session_id, seq)` hit | `aise messages get ID --seq N --context 2` | `get_session(session_id, seq, context)` |
| Counts, grouping, or a structural question | `aise db query "SQL"` | `query_session_index(sql)` or `query_session_index(schema_table)` |
| A session to reopen | `aise resume ID` | `get_resume_command(session_id)` |
| Index health | `aise doctor` | `get_index_status()` |

In an MCP-capable harness prefer MCP. File recovery, export, analysis, and explicit reindexing use
`aise files extract`, `aise export`, `aise analyze`, and `aise reindex`, or the library APIs.
When the string is distinctive, start with message search. Session search reads eligible session
text; fuzzy message search scores every eligible row. Narrow the scope before either.

Result extent differs by surface, deliberately: MCP pages are bounded by default (follow
`next_offset` for non-overlapping pages; pass `all_results=true` instead of `limit` for every
match of a literal or regex query and expect a bounded-ceiling refusal with retry guidance when
that is too large), while CLI and Python return every literal, regex, or query-less match
when no request, purpose, or operation limit applies. CLI `--all-results` requests that explicitly. Fuzzy search always needs a
positive limit on every surface. Presentation options never change which rows match.

## 3. Narrow, then read, then act

1. Add the filters you already know before lengthening a query: scope (`--workspace-path` on
   message search, `--path` on session commands and analytics, `workspace_path_prefix` on MCP
   message search), `--provider`, `--when`, `--role`, `--session-kinds`, `--kinds`.
2. Search one concept with a short distinctive fragment (`foreign key`, `wrong repo`) rather than
   a remembered sentence; short queries tolerate wording drift and expose more candidate turns.
3. Carry the returned `(session_id, seq)` into `messages get`, `messages evidence`, or `show`
   and expand only that turn or transcript edge; once a hit supplies an exact path, identifier,
   or sequence, inspect that target directly.
4. Export, recover, or analyze the identified scope. Write to a new destination, and promote
   only repeated independent corrections into durable guidance (section 8).

For broad, ambiguous, multi-provider, or long-range research that would consume substantial main
context, delegate the search to a smaller harness subagent when the harness supports delegation;
read [references/recover-prior-work-with-evidence.md](references/recover-prior-work-with-evidence.md)
before writing the assignment. A distinctive exact lookup completes fastest in the current agent.

## 4. Filters that apply everywhere

- **Time.** Values without an offset use UTC; a trailing
  [RFC 9557](https://www.rfc-editor.org/rfc/rfc9557) zone, as in `2026-01-15[Europe/Berlin]`,
  reads one value in another zone. Session list/search/export/analysis date filters intersect
  the inclusive query period with each known indexed span from `created_at` through
  `updated_at`: `since` tests the span end, `until` tests the span start, and `when` tests both.
  An exact RFC 3339 value is a zero-width period. A span records first and last known activity
  and may contain gaps. Message, file, and event-analytics dates test each event timestamp; for
  an exact event boundary pass a timestamp with `Z` or an explicit offset, and fractional seconds
  are preserved for same-second ordering. Forms: `7d`, `yesterday`, `2026-01`, `2026-01/..`,
  RFC 3339; `aise dates` is the reference.
- **Path.** Session commands use `--path` (MCP `path_prefix`) for a directory and its
  component-boundary descendants, excluding siblings such as `project-other`. Message search
  separates workspace and transcript paths with `--workspace-path` and `--transcript-path`.
- **Session class.** A run spawned by another session is indexed as a session of its own with
  `parent_session_id` (the spawning session's whole id) and `agent_label` (`Explore`,
  `general-purpose`, a Codex agent nickname). Both classes come back by default;
  `--session-kinds` accepts several values and `--session-kind` selects one; the values are the
  providers' own (Codex records `thread_source: user | subagent`).
- **Message class.** `--kinds` accepts several values and `--kind` selects one. `harness-notice`
  rows (Stop-hook feedback, PreToolUse blocks, local-command caveats, task notifications) are what
  the harness told the agent; they are indexed and left out of ordinary results, so
  `corrections`, `repeats`, and a user-role search see what people wrote. Search them with
  `--kinds harness-notice` to learn why an agent stopped, looped, or was blocked. Claude Code's
  conversation and attachment records supply searchable text; saved API request copies do not add
  repeated turns or file edits. A queued submission delivered as a prompt counts as that prompt;
  independent same-text submissions remain distinct. Files a person mentions, memory and plan text,
  and invoked skill contents are searchable by default. Injected instructions, session context,
  and hook messages use `--kinds harness-notice`. `aise doctor` reports unrecognized record and
  attachment types; malformed attachments and skipped binary payloads produce parse warnings.
- **Leading dash.** A bare positional query starting with `-` is parsed as a flag. Put every
  other flag first, then `--`, then the query (`aise search --limit 5 -- --path`); `messages
  search` also accepts `-e`. This applies to `search`, `messages search`, and `repeats`; MCP
  tools take the query verbatim as a JSON string.

## 5. Query modes and their cost

Literal is the default: the query text as typed, a case-insensitive substring in which
punctuation is significant (`stdout OR printf` searches those exact words; `--query-mode regex
'stdout|printf'` searches the alternatives). `--query-mode regex` (MCP `query_mode=regex`) is
Rust regex syntax: classes, alternation, anchors, repetition; look-around is absent, and
`'\baise\b'` matches the word alone. `--query-mode fuzzy` matches the
query's characters in order with gaps between them; a 3-character-or-longer fragment and a
positive `--limit` are its inputs, and every structurally eligible row is scored before the
deterministic offset/limit slice is selected. `aise search` is plain text with no quote or
boolean operators: the whole query and each word match as case-insensitive substrings of title,
summary, cwd, repo, preview, and transcript, plus a fuzzy match on title and paths, and sessions
matching every word rank first.

Indexed session lists and focused reads avoid scanning message text. Literal and regex searches
use a prefilter when a safe anchor exists, then verify the exact predicate. Fuzzy search reads and
scores every eligible row. Structural filters reduce that work before matching. `--all-results`
grows output with the match count, `--context N` adds neighbors to each hit, and a summary or full
receipt adds a pass to count eligible rows.

To learn how a search was planned, add `--receipt-level summary` (MCP `receipt_level=summary`);
`full` adds the origin of every resolved parameter. The receipt names the prefilter used or
skipped, `corpus` (every row the structural filters admit) and `candidates`, whose ratio is the
selectivity to improve by anchoring on a rarer literal. Per-tool complexity bounds, the receipt
fields, and what the corpus count costs are in
[references/search-costs-and-receipts.md](references/search-costs-and-receipts.md).

After an MCP `search_messages` call that sets filters, compare `effective_request` in the result
with what you sent. A client can drop a misspelled or unsupported parameter before the server
receives it; the search then runs without that filter and the result shows no error.

## 6. Commands by task

### List recent sessions by metadata, then read them

```sh
aise list --path ~/source/project --when 7d --limit 10
aise list --path ~/source/project --when 7d --limit 10 --format json
aise list --path ~/source/project --limit 1
aise list --provider codex --when 7d --limit 20
aise messages evidence SESSION_ID --summary-items -12
aise show SESSION_ID --transcript-lines -40
aise messages get SESSION_ID --role user --limit 20 --order newest
```

`aise list` orders eligible sessions newest first before applying `--limit`; it is the
query-less lookup, and a query-less `aise search` prints the equivalent `aise list` command. The
JSON rows carry id, provider, cwd, created/updated timestamps, message count, and preview. MCP:
`list_sessions(path_prefix, when, limit)` returns the same rows newest first and `get_session`
reads one.

### Find sessions by topic, and what a subagent did

```sh
aise search "database migration" --path ~/source/project --when 30d --limit 10
aise search "flaky test" --session-kinds subagent --when 7d --limit 20
aise list --session-kinds user --when 7d --limit 20
aise list --parent-session claude:7e745098-c299-4cf5-bdbe-5cdb1fb5a62d
```

### Find exact turns and their context

```sh
aise messages search "foreign key" --workspace-path ~/source/project --limit 20 --context 2
aise messages search 'timeout|lock|busy' --query-mode regex --limit 20 --lines-per-message 4
aise messages search "approximate remembered wording" --query-mode fuzzy --limit 20
aise messages search misunderstood --role user --when 14d --limit 20 --context 2
aise messages search "CANNOT STOP" --kinds harness-notice --when 2d --limit 20
# What the harness denied in one session, then the tool call each denial answered:
aise messages search 'denied|blocked|not allowed' --query-mode regex --kinds harness-notice --session-id SESSION_ID --limit 50 --lines-per-message 3
aise messages get SESSION_ID --seq 42 --context 2 --kinds conversation,harness-notice
aise messages search exec --field tool-name --query-mode fuzzy --limit 20
aise messages search "cargo test" --field tool-argument --argument-path /cmd --limit 20
aise messages search "cargo test" --field tool-argument --argument-path /command --tool-name-contains Bash --limit 20
aise messages search -e --path --limit 5
```

`--field tool-name` and `--field tool-argument` (with `--argument-path`, an RFC 6901 pointer)
search canonical tool fields without scanning rendered output. Use `--workspace-path` for message
search and `--path` for session search. `--include` accepts `normalized_session_metadata`,
`parsed_references`, `raw-metadata`, `runtime_diagnostics`, or `none`.

### Read one session, page without re-reading, and bound each hit

```sh
aise messages get SESSION_ID --seq 42 --context 3 --refs --lines-per-message 8
aise messages get SESSION_ID --role user --limit 75 --order newest
aise messages get SESSION_ID --seq-from 0 --seq-to 499
aise messages get SESSION_ID --seq-from 500 --seq-to 999
aise messages get SESSION_ID --kinds conversation,harness-notice
aise messages evidence SESSION_ID --summary-items -12 --include time-profile
aise show SESSION_ID --transcript-lines -40
aise show SESSION_ID --transcript-lines 0
aise resume SESSION_ID
```

`aise resume ID` prints its command when stdin or stdout is redirected; `--dry-run` also
prints it in a terminal.

Range reads omit harness notices by default; an explicit `--kinds` list can include them.
MCP `get_session` ranges accept `kinds=["conversation", "harness_notice"]`. Focused context
already includes every class. Use MCP `seq` for a focused read. MCP focused/range reads use `detail="compact"|"full"`, like
message search; `response_format="concise"|"detailed"` remains a deprecated alias.

Signed transcript, per-message, and evidence windows share one convention: positive keeps the
first N lines or evidence records, negative the last N, and zero everything (which may be large). `--limit` on `messages get`
selects oldest-first unless `--order newest`, which selects the last N and prints them
oldest-first. To read further, continue from the next seq range (`seq_from = last seq + 1`)
rather than re-requesting a larger `--limit` or `transcript_lines`, which re-sends what you
already read; MCP mirrors this with `get_session(seq_from, seq_to)`. `search_messages` pages in
session/sequence order with `offset` (fuzzy pages by score); `match_window=latest` (requires
one `session_id`) keeps the last N matching messages of that session instead of the first N,
still returned oldest first. Prefer compact evidence (`messages evidence`) first and one turn
(`messages get --seq`) over a larger transcript window.

Presentation flags change display only; matches, ranking, result count, pagination, context
membership, and reference extraction stay the same: `--context N` adds neighboring turns of any
role (in table and plain output the `match` column is filled only on hit rows: the match
evidence when the query produced one, otherwise `*`); `--lines-per-message N` keeps the first N lines,
negative the last N, zero the complete content; MCP `field_view`/`match_view` are character
budgets. For a search expected to return several hits, set `--lines-per-message` before
increasing `--limit`: `--limit` bounds the hit count and `--lines-per-message` bounds each hit.

### Export sessions

```sh
aise export SESSION_ID --format markdown --output session.md
aise export --path ~/source/project --when 7d --limit 20 --output-dir /absolute/new/directory
```

A filtered bundle creates a new immutable directory; `--limit` defaults to
`[search].default_limit` (50) and `--limit 0` bundles every matching session.

### Analyze repeated behavior

```sh
aise skills corrections --path ~/source/project --when 30d --limit 50
aise slash-commands --path ~/source/project --when 30d --limit 50
aise stats --path ~/source/project --when 30d
aise repeats --path ~/source/project --when 30d --max-candidates 5000 --max-groups 20
aise analyze --provider codex --when 7d --output /absolute/new/analysis
```

`aise skills corrections` and default/user-role `aise repeats` scan only source-attributable
human text, including messages typed while the agent was working when the transcript records a
human sender. Harness notices and copied parent history in Codex forks remain searchable but do
not count as new human input. Queued messages with no recorded sender (older Claude Code) are left out and counted:
`unattributed_input_count` in the JSON report, a note in table output, a stderr note for
`repeats` (or the count inside `aise repeats --format json --report`). Pass `--include-unattributed-input` (MCP `include_unattributed_input: true`) to read
them; a program may have sent them. A queued message also delivered as its own prompt counts once.
Corrections default to user-started sessions; pass `--session-kinds user subagent` to include
attributable human text in both classes. `--format json` returns a tagged skill-run receipt plus
the classification report.

The shipped correction categories are a small starter set of fixed phrasings. They will miss
corrections worded for the situation. Use them as a quick first filter. To find where sessions
went wrong, list their recorded input, inspect sender provenance, and read the surrounding turns:

```sh
aise list --path ~/source/project --when 7d --limit 10
aise messages search "" --session-id SESSION_ID --role user --all-results --format json
aise messages get SESSION_ID --seq 42 --context 3
```

MCP: `list_sessions(path_prefix="/path/to/project", when="7d", limit=10)`, then
`search_messages(session_id="SESSION_ID", role="user", all_results=true)` and
`get_session(session_id="SESSION_ID", seq=42, context=3)`. For a large session, use a positive
`limit` and follow `page.next_offset` instead of `all_results`. Add categories for wording you
see repeatedly using [Create your own classification package](#create-your-own-classification-package).
A copied parent conversation is correlated evidence, not an independent correction.
`aise stats` omits harness notices; raw SQL role counts include the classes its predicate selects.
Count distinct `tool_call_id` values when measuring calls, since several rows can share an id.
Publish analysis only to a new absolute directory; add `--policy` only for a validated JSON
`AnalysisPolicySpec`.

The corrections categories are the `aise-capability.toml` embedded in the executable; the copy
beside this SKILL.md documents them, and editing an installed copy changes nothing (make your
own package below). Categories and selected skills are evaluated in declaration order, the
first match wins, and every run reports the exact capability digest. Point `--skill` at another
skill directory to add its categories after these:

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
# add "~/.claude/skills" to [skills].search_paths in the file `aise config file` prints (`aise config init` writes it if absent)
aise skills list
aise skills my-corrections --path ~/source/project --when 30d --format json
aise skills corrections --skill ~/.claude/skills/my-corrections --format json   # defaults first, yours after
```

`aise skills show my-corrections` prints where it resolved from and its categories in order; a
package named like a management verb (`list`, `show`, `validate`, `create`, `update`, `restore`)
runs as `aise skills run <name>`.
MCP runs the same package with `run_skill_capability(skill={"name":"my-corrections"})` or
`skill={"path":...}` under a `[skills].search_paths` root, `additional_skills` for more
packages, and `definition={"categories":[{"name":...,"patterns":[...]}]}` for one call's
inline rules. Read [references/message-classification.md](references/message-classification.md)
for the schema, precedence, and pattern guidance before changing categories or regular
expressions.

### Structural questions over the index

Use `query_session_index` (CLI `aise db query`) for read-only SQL, schema inspection, counts,
grouping, time series, and cross-provider questions beyond the higher-level tools; content and
regex search stay with `search_messages`. Read the `note` each column carries in
`query_session_index(schema_table=...)` before writing a predicate over it: several answer a
different question than their name suggests, notably `messages.tool_name`, which holds the
provider's own spelling (namespaced on Claude, a bare leaf name on Codex; match it with
`tool_name_contains`). Bound raw SQL with `limit`, `timeout_ms`, and `max_cell_chars`.

### Recover an edited file

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

## 7. Control index freshness deliberately

`--index-refresh auto` (the default) keeps a compatible existing index available and performs
needed incremental maintenance. Use `--index-refresh before-query` when this query must wait for
current source files to be indexed, `--index-refresh existing-only` to read with
no implicit refresh, `aise reindex` for an explicit incremental rebuild, and `aise reindex --full`
only when a complete reparse is required. Run `compact` only when asked, after `aise doctor` and
a disk check: compaction mutates the database and may need temporary space. Refresh is separate
from query cost: incremental refresh reads new source bytes; a full reparse reads all source bytes.

For library use, Python `search_sessions()`/`list_sessions()`, `search_messages()`,
`read_session_messages()`/`message_context()`, `reconstruct_file()`, `classify_messages()` and
`repeats()` cover discovery, search, reading, recovery, and analysis. Rust exposes those routes
through `SessionSearch::catalog()`, `messages()`, `files()`, and `analysis()`; `index()` controls
freshness. See the [library guide](https://github.com/ahundt/ai-session-search/blob/main/docs/development/library-api.md).

An empty search hint suggests other content classes or file history to try. It means the current
query found no matches, not that the suggested alternatives contain an answer.

## 8. Turn evidence into durable guidance

1. Search with bounded results and context; record session IDs and sequence numbers.
2. Separate measured evidence from inference and mirrored records from independent conversations.
3. Put one concrete rule in the narrowest target: repository guidance, an existing skill, a hook,
   or code.
4. Search later sessions for the same correction to check whether it recurs.

Promote a rule once the same correction appears in independent sessions; a mirrored session
counts as the one conversation it copies. Sandbox and approval policy belong in agent/autorun
guidance; AI Session Search documentation describes the product.

If this workflow reveals a needed implementation change, inspect the project's manifests,
lockfiles, imports, and existing abstractions before writing infrastructure; reuse a fitting
dependency first, and adopt a new library only after verifying contract, lifecycle, platform,
performance, dependency, and release fit and confirming that it removes more custom machinery
than it adds.
