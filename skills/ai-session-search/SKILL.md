---
name: ai-session-search
description: Search, recover, inspect, export, and analyze local AI session history with AI Session Search (`aise`) across Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI. Use when asked to "find prior AI work", "recover context after compaction", "inspect tool calls or corrections", "reconstruct a file", "export a session", "analyze repeated mistakes", or turn session evidence into durable agent guidance.
metadata:
  version: 1.0.0-rc.1
---
<!-- ai-session-search-managed-skill v1 -->

# AI Session Search (`aise`)

Use `aise` instead of scanning raw provider files. It normalizes eight providers into one index.

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

## Prefer MCP when available

The MCP server key and protocol identity are `ai-session-search`, with display title
**AI Session Search**. Route requests by evidence type:

1. Use `query_session_index` for read-only SQL, schema inspection, counts, grouping, time-series,
   and cross-provider questions that the higher-level tools cannot express. It is the priority tool
   for structured index analysis, but not for full-text content search.
   *(~0.01 s indexed aggregate; >1 s full scan; `O(R)` rows scanned.)*
2. Use `search_messages` for exact, regex, or fuzzy search over message content, canonical tool
   names, or one tool-argument JSON pointer. Start with the shortest discriminating fragment; add
   words only when results are ambiguous.
   *(~1 s selective indexed search; `O(P + H + C)` bounded candidates, hits, context.)*
3. Use `search_sessions` for broad topics, titles, repositories, or remembered phrases.
   *(~0.05 s; `O(P + K log K)` postings and ranked results.)*
4. Pass a returned session ID and optional sequence to `get_session` for bounded evidence.
   *(~0.01 s focused; `O(log M + C)` message lookup and context.)*

The remaining tools are `list_sessions` *(~0.01 s; `O(log S + K)`)*,
`get_resume_command` *(indexed lookup; `O(log S)`)*, and `get_index_status`
*(~0.2 s warm; `O(F)` discovered source files)*. MCP pages are bounded by default; follow
`next_offset` for non-overlapping pages. Use the CLI for export, file recovery, or when MCP is
unavailable. Timings are one-significant-figure warm-cache measurements on this machine; MCP
transport and result size add overhead.

Choose the first MCP call by what is known, then narrow:

- Unknown session, remembered topic: `search_sessions(query, path_prefix, when, limit)`.
- Exact phrase, identifier, correction, or tool activity: `search_messages(query, path_prefix,
  role/kind/tool, context, limit, lines_per_message)`; skip session discovery when the string is
  already distinctive.
- Returned hit: pass its `session_id` and `seq` as `get_session(session_id, message_seq, context)`.
- Aggregate or relationship question: inspect the schema, then use bounded
  `query_session_index(sql, limit, timeout_ms, max_cell_chars)`; do not use raw SQL for content
  search.

## Search, focus, then act

1. Bound the source with `--path`, `--provider`, and `--when` when those facts are known.
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
- Analyze recurrence: `corrections` or `planning` -> record session IDs and sequences -> search
  later sessions for the same behavior.
- Require newly indexed source files: add `--index-refresh before-query`; use `existing-only` for a
  reproducible read of the current index.

Run the relevant `--help` before adding options not shown below.

Time periods and relative days are UTC. For an exact event boundary, pass an RFC 3339 timestamp
with `Z` or an explicit offset; fractional seconds are preserved for same-second event ordering.

## Commands by task

### Find sessions by topic

```sh
aise search "database migration" --path ~/source/project --when 30d --limit 10
aise list --provider codex --when 7d --limit 20
```

Use `aise search` when the topic, title, repository, or remembered phrase is enough. Provider names
are:

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

Literal matching is the default. Use `--regex` for Rust regex syntax and `--fuzzy` for bounded
approximate wording. Fuzzy is sequence-based retrieval, not exhaustive edit distance: use at least
3 characters, a positive `--limit`, and `offset + limit <= 10,000`. A hit is identified by
`(session_id, seq)`.

Add `--explain` when a search is unexpectedly broad or slow. Exact and regex modes still verify the
requested predicate after indexed candidate retrieval. Fuzzy mode is bounded approximate retrieval;
if the receipt reports `candidate_source_saturated`, add provider, path, session, role, kind, tool,
or date filters for better recall. This is distinct from `prefilter_skipped`, which means structured
filters already made a direct scan cheaper. CLI explanations use stderr; MCP returns the receipt as
structured output.

Search canonical tool fields without scanning rendered output conventions:

```sh
aise messages search exec --field tool-name --fuzzy --limit 20
aise messages search "cargo test" --field tool-argument --argument-path /cmd --limit 20
```

Prefer a short fragment such as `foreign key` or `wrong repo` over a remembered sentence. Short
queries tolerate wording differences and expose more candidate turns; use `--path`, `--role`,
`--when`, and `--context` to narrow results before lengthening the text query.

Literal mode has no Boolean `OR`: a query such as `stdout OR printf` searches those exact words.
Use `--regex 'stdout|printf'` for alternatives. Put boundaries around short identifiers
(`--regex '\baise\b'`) so `aise` does not also match `raise`. Recent sessions contained stale
`--project` and `--type` calls; use `--path` and `--role`, and check `--help` before reusing an old
command.

Searching for a string that starts with `-` (a flag name, a diff line, `--path`) needs an escape,
because a bare positional query is parsed as a flag. Put every other flag first, then `--`, then
the query: `aise search --limit 5 -- --path`. `messages search` also accepts `-e`:
`aise messages search -e --path --limit 5`. This applies to `search`, `messages search`, and
`repeats`. The MCP tools take the query as a JSON string and need no escaping.

`--context N` adds neighboring turns even when they have other roles; in plain output, `*` marks
the actual hit. `--lines-per-message N` changes presentation only: positive keeps the first N
lines, negative keeps the last N, and zero keeps complete content. It does not change matches,
ranking, result count, pagination, context membership, or reference extraction.

For a search expected to return several hits, set `--lines-per-message` before increasing
`--limit`: the latter bounds hit count, not the size of each hit or its context. To inspect one
known session without a content query, use bounded `messages evidence`, `messages get`, or `show`
rather than an empty message search.

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
aise corrections --path ~/source/project --when 30d --limit 50
aise planning --path ~/source/project --when 30d --limit 50
aise stats --path ~/source/project --when 30d
aise repeats --path ~/source/project --when 30d
aise analyze --provider codex --when 7d --limit 50 --output /absolute/new/analysis
```

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
the database and may need temporary space. Refresh is separate from query cost: a forced refresh on
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

`--restore` writes a collision-safe `.recovered` sibling and never overwrites the original. Use
`--output-dir` for an explicit destination and `--all` only when every reconstructable version is
required.
