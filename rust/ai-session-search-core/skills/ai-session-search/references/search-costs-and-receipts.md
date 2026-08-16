# Search costs and receipts

What each `aise` operation costs, in symbols and in measured time, and how to read a search
receipt. `SKILL.md` carries the plain cost order; this file holds the detail.

## Symbols

`N` eligible message rows, `T` their selected-field characters, `W` the page window
(offset + limit), `C` candidates admitted by a prefilter, `P_Q` prefilter work for query `Q`,
`S` indexed sessions, `M` messages in one session, `K` a positive result limit, `A` query
tokens, `F` discovered source files, `E` filtered file edits, `B` source bytes.

## Per operation

| Operation | Time | Memory | Measured (warm cache, one machine, one significant figure) |
| --- | --- | --- | --- |
| `list_sessions` / `aise list` | `O(log S + offset + K)`; paging uses SQL `OFFSET` | `O(K)` | ~0.01 s |
| `get_session` / `messages get`, `show`, `messages evidence` | `O(log M + C)` after an `O(S)` id resolve | returned window | ~0.01 s |
| `get_resume_command` / `aise resume` | `O(S)` id scan | `O(1)` | ms |
| `get_index_status` / `aise doctor` | `O(F)` discovered files | `O(F)` | ~0.2 s |
| Literal or regex `search_messages` | `O(P_Q + C + verified bytes)`: word and trigram indexes admit candidates, then the exact predicate is verified; a short or unsafe anchor falls back to the filtered corpus, `O(B)` worst case | finite page `O(W + D_W)`; explicit all-results `O(N + D_K)` | 0.3 s for the rare `ECONNRESET\|socket hang` regex, 1.4 s for the common phrase `permission denied`, both `--limit 50` on ~2.5 million messages |
| Fuzzy `search_messages` | `O(T + N + W log W)`; every structurally eligible row is read and scored in bounded parallel batches | one batch plus its largest row, retained `W` text, per-worker matcher scratch | seconds on the same index; narrow first |
| `search_sessions` / `aise search` | `O(T*(A+1) + N + K log K)`; every eligible session's title, summary, paths, preview, and transcript are read | retained `K` records plus the batch budget and largest session | 1–2 s on this index (release build) |
| `query_session_index` / `aise db query` | indexed aggregate ~0.01 s; a predicate the planner cannot serve scans `O(R)` rows, >1 s | bounded by `limit`/`max_cell_chars` | as stated |
| `files search`/`history`/`cross-ref`/`extract` | `O(E + K log K)` filtered edits/results | `O(K)` | ~0.01 s search/history, ~0.06 s cross-reference |
| Refresh | incremental `O(B_new)`; full `O(B_all)` | streaming | a forced refresh exceeded 100 s before cancellation on this machine |

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
  one pass over the smallest message-table index when no structural filter is set (a few seconds
  cold on a 23 GB, 2.7-million-message index, well under a second warm), and only the filtered
  rows' index entries with `--role`, `--session-id`, `--workspace-path`, `--when`, or
  `--tool-name-contains`.
  An explicit `--kinds` set beyond `harness-notice`/`compaction` counts by reading rows. An index
  written by an aise before 1.0.0rc2 gains the supporting partial index at its next refresh.
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
