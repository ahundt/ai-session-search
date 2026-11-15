# Search costs and receipts

What each `aise` operation costs, in work and memory, and how to read a search
receipt. `SKILL.md` carries the plain cost order; this file holds the detail.

## Symbols

`N` eligible message rows, `T` their selected-field characters, `W` the page window
(offset + limit), `C` candidates admitted by a prefilter, `P_Q` prefilter work for query `Q`,
`S` indexed sessions, `M` messages in one session, `K` a positive result limit, `A` query
tokens, `F` discovered source files, `E` filtered file edits, `B` source bytes, `R` SQL rows, `D_W`/`D_K` returned context bytes for a page/all matches.

For file history and extraction, `E` counts selected edit rows, including replay prerequisites
outside the output date window; `H` is their retained payload bytes, `V` is peak live content
bytes, and `J` is accumulated version-metadata bytes before output paging.
`L` counts bytes scanned or copied during replay, line counting, and output cloning.

## Per operation

| Operation | Time | Memory |
| --- | --- | --- |
| `list_sessions` / `aise list` | `O(log S + offset + K)` with favorable indexes; up to `O(S log S + offset + K)` with a temporary sort, plus `O(S)` span checks; paging uses SQL `OFFSET` | `O(K)` records plus their text and SQLite cache/sort memory |
| `get_session` / `messages get`, `show`, `messages evidence` | `O(log M + C)` after an `O(S)` id resolve | returned window |
| `get_resume_command` / `aise resume` | `O(S)` id scan | `O(1)` |
| `get_index_status` / `aise doctor` | `O(F)` discovered files | `O(F)` |
| Literal or regex `search_messages` | `O(P_Q + C + verified bytes)`: word and trigram indexes admit candidates, then the exact predicate is verified; a short or unsafe anchor falls back to the filtered corpus, `O(B)` worst case | finite page `O(W + D_W)`; explicit all-results `O(N + D_K)` |
| Fuzzy `search_messages` | `O(T + N + W log W)`; every structurally eligible row is read and scored in bounded parallel batches | one batch plus its largest row, retained `W` text, per-worker matcher scratch |
| `search_sessions` / `aise search` | `O(S + T*(A+1) + N + K log K)`; every eligible session's title, summary, paths, preview, and transcript are read | retained `K` records plus the batch budget and largest session |
| `query_session_index` / `aise db query` | a predicate without a usable index scans `O(R)` rows | bounded by `limit`/`max_cell_chars` |
| `files search` / `cross-ref` | SQL scans, groups, and sorts eligible edit metadata; a result limit does not bound rows examined | returned metadata bytes plus SQLite grouping, sort, and cache memory |
| `files history` | after SQL selection, `O(E + L)` for replay and line counting | `O(E + H + V + J)` plus SQLite memory |
| `files extract` / reconstruction iterator | after SQL selection, `O(E + L)` for snapshot lookup and consumed replay, including copies of yielded content | `O(E + H + V)` plus SQLite memory and content retained by the caller |
| Refresh | incremental source reads `O(B_new + overlap)`; whole-source fallbacks `O(B_all)` | streaming reads; full parses retain one source |

Repeated edits can scan or copy the whole content repeatedly, so `L` can grow with edit count
times content size. SQL selection can scan the edit table and sort matching rows; history and
extraction load selected edit payloads before applying the output limit.
File queries default to `limit=0`, which returns all matching results.

`B_new` is appended source bytes, `B_all` is all bytes in a source, and the backward overlap is
bounded at 1 MiB. Growing transcripts need a full parse for an older build's checkpoint without
an overlap digest, a parser-version change, changed overlap bytes, or a newly appended record
that changes an indexed row. Queued Claude delivery through a queued-command attachment or
directly after dequeue needs at most one full parse per delivery; queue records that only add
rows stay incremental. Linked Claude interactive answers, Codex `request_user_input` responses,
and Codex `thread_settings_applied` records closing fork replay can also require earlier context.
Refresh does not detect rewrites between the head fingerprint and overlap; use `aise reindex --full`.
Codex fork reconciliation checks each fork's own edits; a parent edit reparses a fork only when
that fork holds a matching edit. Codex, Pi, and Prime Agent fork reconciliation runs while the
index writer lock is held. Every parser version changes in 1.1.0, so the first refresh after
upgrading reparses every source. Compressed sources and providers without incremental parsing
read the whole source. Index persistence additionally counts stored session messages and copies
stored transcript bytes.

Structural filters (`--workspace-path`/`workspace_path_prefix`, `--session-id`, `--role`,
`--when`, `--kinds`, `--tool-name-contains`) shrink `N`, `T`, and `C` before the query runs.
`--all-results` makes output `O(matches)`; `--context N` multiplies returned rows per hit;
presentation budgets (`--lines-per-message`, `field_view`, `match_view`) change bytes returned
and never which rows match.

## Receipts

`--receipt-level summary` (MCP `receipt_level=summary`) explains how the search was planned;
`full` adds the origin of every resolved parameter (explicit call, purpose bundle, operation
config, surface config, or typed default). Fields:

- `prefilter` and `prefilter_skipped`: which index admitted candidates, or the reason none did
  (for example a query too short for a safe anchor, or a tool-name search verified directly in
  SQLite).
- `corpus`: every row the structural filters admit. It is counted from indexes:
  one pass over the smallest message-table index when no structural filter is set, or the
  filtered rows' index entries with role, session, workspace, date, or tool filters.
  An explicit `--kinds` set beyond `harness-notice`/`compaction` counts by reading rows.
- `candidates`: rows the prefilter admitted before verification (absent for a verified scan or a
  fuzzy search). `candidates / corpus` is the selectivity; improve it by anchoring the query on a
  rarer literal.
- Exact and regex modes verify the requested predicate after candidate retrieval, so a prefilter
  changes cost, never membership. Fuzzy scores the complete eligible corpus and retains at most
  `min(offset + limit, corpus)` ranked rows. An `offset` at or past the last eligible row is
  answered from the corpus count without scoring anything, which is what keeps a huge `offset`
  cheap; a deep `offset` that does land inside the corpus still retains up to every matching row.

The CLI prints the receipt with `--format json` (and a one-line `[explain]` summary on stderr);
MCP returns it as `receipt` in the structured output.
