---
name: ai-session-search
description: Search, recover, inspect, export, and analyze local AI session history with AI Session Search (`aise`) across Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi coding agent, Google AI Studio, and Gemini CLI. Use when asked to "find prior AI work", "recover context after compaction", "inspect tool calls or corrections", "reconstruct a file", "export a session", "analyze repeated mistakes", or turn session evidence into durable agent guidance.
metadata:
  version: 1.0.0-rc.1
---
<!-- ai-session-search-managed-skill v1 -->

# AI Session Search (`aise`)

Use `aise` instead of scanning raw provider files. It normalizes eight providers into one index.
The local Codex provider covers ChatGPT/Codex App plus Codex CLI/IDE through
their shared `~/.codex` host. Claude Code CLI/Desktop share the Claude Code
provider, while Claude Desktop local-agent sessions use `claude-desktop`.
Antigravity App/IDE/CLI share the `antigravity` provider. Cloud-only account
history is outside this local index.

If this workflow reveals a needed implementation change, inspect the project's manifests,
lockfiles, imports, and existing abstractions before writing infrastructure. Reuse a fitting
dependency first; otherwise adopt a mature, widely used library only after verifying contract,
lifecycle, platform, performance, dependency, and release fit and confirming that it removes more
custom machinery than it adds.

## Start safely

```sh
aise --version
aise doctor
aise config paths
```

If a command or option is uncertain, run `aise <command> --help`. Do not guess old aliases.
Use canonical session IDs from results, such as `codex:<id>` or `claude:<id>`.

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
   and cross-provider questions that the higher-level tools cannot express. It is the priority tool
   for structured index analysis, but not for full-text content search.
   *(~0.01 s indexed aggregate; >1 s full scan; `O(R)` rows scanned.)*
2. Use `search_messages` for exact, regex, or fuzzy search over message content, canonical tool
   names, or one tool-argument JSON pointer. Start with the shortest discriminating fragment; add
   words only when results are ambiguous.
   *(Exact/regex can use selective indexes. For `N` eligible rows with `T` total selected-field
   characters and page window `W`, fuzzy uses `O(T + N + W log W)` time. Peak memory includes the
   full text bytes in one 512-row scoring batch, the retained `W` rows, and per-worker
   UTF-32/lowercase scratch for the largest row. Literals under 3 characters and exact/regex
   tool-argument fields scan the filtered corpus.)*
3. Use `search_sessions` for broad topics, titles, repositories, or remembered phrases.
   *(For `N` eligible sessions with `T` total field/transcript characters and positive result limit
   `K`, scoring uses `O(T + N + K log K)` time. Peak memory is the retained `K` records and their
   text plus the largest streamed transcript and its transient lowercase copy.)*
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
- Analyze recurrence: `skills corrections` or `planning` -> record session IDs and sequences -> search
  later sessions for the same behavior.
- Require newly indexed source files: add `--index-refresh before-query`; use `existing-only` for a
  reproducible read of the current index.

For broad, ambiguous, multi-provider, or long-range research that would consume substantial main
context, delegate the search to a smaller harness subagent when the harness supports delegation.
Read
[references/recover-prior-work-with-evidence.md](references/recover-prior-work-with-evidence.md)
before writing the assignment. Keep a distinctive exact lookup in the current agent because
delegation would add overhead without increasing recall.

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

### Find what a subagent did

```sh
# Only delegated work, across every provider that records it.
aise search "flaky test" --session-kinds subagent --when 7d --limit 20
# Only sessions you started, so a listing is not dominated by the runs beneath them.
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
aise messages search "foreign key" --path ~/source/project --limit 20 --context 2
aise messages search 'timeout|lock|busy' --regex --limit 20 --lines-per-message 4
aise messages search "approximate remembered wording" --fuzzy --limit 20
aise messages search misunderstood --role user --when 14d --limit 20 --context 2
aise messages search "CANNOT STOP" --kinds harness-notice --when 2d --limit 20
```

To learn why an agent stopped, looped, or was blocked, search `--kinds harness-notice`:
Stop-hook feedback, PreToolUse blocks, local-command caveats, and task notifications are what
the harness told the agent rather than what the user wrote. They are indexed but excluded from
ordinary results, so they never skew `corrections`, `repeats`, or a user-role search. `--kinds`
is the single class filter and accepts several values; `--kind` selects one.

Literal matching is the default. Use `--regex` for Rust regex syntax and `--fuzzy` for
sequence-based approximate wording. Fuzzy is not edit distance: use at least 3 characters and a
positive `--limit`. Every structurally eligible row is scored before the deterministic offset and
limit slice is selected. A hit is identified by
`(session_id, seq)`.

### Read one session: newest/oldest N, and page without re-reading

```sh
# The 75 most recent user turns. Direction is --order (MCP: order=newest), never a negative --limit.
aise messages get SESSION_ID --role user --limit 75 --order newest
# A long session in non-overlapping chunks: advance --seq-from, do NOT grow --limit.
aise messages get SESSION_ID --seq-from 0 --seq-to 499
aise messages get SESSION_ID --seq-from 500 --seq-to 999
```

`--limit` selects oldest-first unless `--order newest`; order picks WHICH N, so newest is the last
N, not the first N shown backwards. To read further, continue from the next seq range
(`seq_from = last seq + 1`) rather than re-requesting a larger `--limit`/`transcript_lines`, which
re-sends what you already read. MCP mirrors this: `search_messages(order=…)` (newest requires
`session_id`) and `get_session(seq_from, seq_to)`.

Add `--explain` when a search is unexpectedly broad or slow. Exact and regex modes still verify the
requested predicate after indexed candidate retrieval. Fuzzy mode scores the complete structurally
eligible corpus and retains bounded top-K state for the requested page. `prefilter_skipped` explains
why an exact or regex index prefilter was not used. CLI explanations use stderr; MCP returns the
receipt as structured output.

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
aise skills corrections --path ~/source/project --when 30d --limit 50
aise planning --path ~/source/project --when 30d --limit 50
aise stats --path ~/source/project --when 30d
aise repeats --path ~/source/project --when 30d
aise analyze --provider codex --when 7d --limit 50 --output /absolute/new/analysis
```

`aise skills corrections` scans only what a PERSON wrote, in user-started sessions; pass
`--session-kinds user subagent` to include delegation prompts. `--format json` returns
a tagged skill-run receipt plus the classification report. The separate managed `corrections`
skill owns the deterministic categories and their `capability.toml`.

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
