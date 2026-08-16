# Recover prior work with evidence

Use this workflow when broad AI-session research would consume substantial main-agent context.
The harness starts and supervises the subagent; AI Session Search answers its read-only MCP or
CLI queries.

## Decide whether to delegate

Delegate when at least one condition applies:

- the request spans several providers, repositories, or time periods;
- the remembered wording is uncertain enough to require literal, regex, and fuzzy passes;
- the answer needs both recent evidence and an explicit historical check;
- the work requires several result pages or comparison across many sessions.

Keep one distinctive phrase, known session ID, or known file lookup in the current agent. If the
harness cannot delegate, follow the same workflow locally with tighter pages.

## Give the subagent a typed assignment

Provide these named fields. Do not encode different meanings in positional text or Boolean flags.

```yaml
research_question: <question the evidence must answer>
authorized_search_roots:
  - <repository or directory the harness permits>
utc_time_range: <explicit range, recent-first with historical check, or all available history>
providers: <known providers or all indexed providers>
session_filters: <roles, message kinds, or session kinds that matter>
discovery_kind: topic-overview | exact-evidence | topic-then-verify
required_claims:
  - <claims that need direct support>
index_freshness: existing-only | before-query   # before-query is a CLI flag; MCP calls accept auto | existing-only
research_budget:
  max_elapsed_minutes: <optional positive integer>
  max_search_pages_per_query: <positive integer>
  max_query_ledger_entries: <positive integer>
  max_evidence_groups: <positive integer>
  max_excerpt_chars_per_group: <positive integer>
  max_report_chars: <positive integer>
prohibited_actions:
  - write repository files
  - modify or compact the session index
  - export or recover session files
```

Use `topic-overview` for purpose or trajectory, `exact-evidence` for a phrase, identifier, tool
call, or correction, and `topic-then-verify` when broad discovery must lead to message-level proof.
Every supplied budget must be finite and positive; omit `max_elapsed_minutes` unless the caller
explicitly wants a wall-clock deadline, and never use zero to mean both "none" and "unlimited."
Give the child larger page and evidence budgets than the main agent can absorb directly, scaled to
the named scope. Budgets control research effort and report size, not which matches qualify.

## Search without losing the best evidence

1. Confirm the CLI or MCP schema instead of guessing options.
2. Apply authorized roots, providers, and time scope before increasing result volume.
3. For `exact-evidence`, start with `search_messages`.
   Use literal matching first, regex for alternatives or boundaries, and fuzzy only for uncertain
   wording.
4. For `topic-overview`, start with `search_sessions`, then inspect the relevant start, end, or
   matched turn with `get_session`.
5. For `topic-then-verify`, use session discovery only to derive candidate sessions or terms, then
   support every returned claim with message-level evidence.
6. For counts or relationships, use `query_session_index`; do not use SQL for content search.
7. Follow `next_offset` in non-overlapping pages. Page size is a transport and context choice, not
   an algorithmic candidate cutoff. Never rank only a prefix of eligible matches.
8. Use larger pages than the main agent can comfortably absorb, but keep paging adaptive. Continue
   while a page adds relevant independent evidence, an explicit scope remains unchecked, or a
   required claim is unsupported. Stop on the first of: all required claims have enough independent
   evidence, the scoped corpus is exhausted, any explicitly supplied budget (including optional
   elapsed time) is reached, or further pages add no new evidence groups.
9. Expand only decisive hits. Record canonical `session_id` and `seq` before requesting more
   context.

Recent-first is the useful default for ordinary debugging and handoffs. When the question asks
about recurrence, long-range behavior, or historical origin, add older time strata deliberately;
do not mistake a recent sample for all available history.

Treat provider mirrors and imported copies as correlated evidence unless their content establishes
independent conversations. Do not inflate frequency counts with duplicates.

## Return a bounded evidence packet

Return synthesis, not a transcript dump:

```yaml
answer: <direct evidence-backed conclusion>
claim_findings:
  - claim: <required claim>
    status: evidence | inference | unresolved
    evidence_group_ids: [<deduplicated evidence groups>]
evidence_groups:
  - evidence_group_id: <stable report-local identifier>
    canonical_locations:
      - session_id: <canonical provider-prefixed ID>
        seq: <message sequence when applicable>
    timestamp: <timestamp or range when available>
    path: <indexed repository or source path when relevant>
    supports: <claim supported by this item>
coverage:
  authorized_search_roots: <roots actually searched>
  utc_time_range: <range actually searched>
  providers: <providers actually searched>
  query_modes: <literal, regex, fuzzy, session, or SQL modes used>
  pages_examined: <page and offset summary>
duplicate_handling: <mirrors or repeated records excluded>
query_ledger: <bounded list of tool, search semantics, resolved filters, and page offsets>
stop_reason: <claims-supported, scope-exhausted, budget-reached, or evidence-saturated>
has_more: <whether an unchecked continuation remains>
next_offset: <continuation offset when one remains>
gaps_and_uncertainty:
  - <unsearched scope, unavailable provider, ambiguous match, or unsupported claim>
recommended_next_step: <one action, or none>
```

Prefer the smallest set of decisive evidence that supports every material claim. Include exact
commands or MCP arguments when they are needed to reproduce a surprising result. State measured
evidence separately from inference. Never claim exhaustive coverage unless the assignment named a
bounded scope and every page in that scope was consumed. If a budget stops the search, say "not
found in reviewed pages" rather than claiming absence.
