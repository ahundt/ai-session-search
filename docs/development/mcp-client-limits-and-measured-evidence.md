<!-- SPDX-FileCopyrightText: 2026 Andrew Hundt -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# MCP client limits and measured evidence

Last measured 2026-08-03 against the executable built from this tree.

What this build emits, measured against every client limit it can breach, and which of
those clients were actually available to check. A limit that could not be observed is
recorded as unverified. None is recorded as a pass.

Reproduce the measurements with `aise mcp schema-budget --ledger` and
`aise mcp schema-budget`, which run against the executable's own catalogue.

## Clients on this machine

| Client | Version | Schema limits checkable | Result limits checkable |
|---|---|---|---|
| Codex | codex-cli 0.146.0 | yes, normalization reimplemented from its source | yes, from a response fixture |
| Claude Code | 2.1.220 (Claude Code) | yes, description caps | yes, from a response fixture |
| OpenCode | not installed | no | no |
| VS Code | not installed | no | no |

Codex and Claude Code are installed and their limits are measured below. OpenCode and
VS Code are not installed here, so their rows are unverified rather than passing: the
checker still measures the artifact against their published caps, but nothing on this
machine confirms how those clients treat it.

## Emitted catalogue, per tool

| Tool | inputSchema wire | as Codex counts it | descriptions reach the model | outputSchema | depth |
|---|---:|---:|---|---:|---:|
| `search_messages` | 5,489 B | 4,995 B | all | 20,116 B | 18 |
| `run_skill_capability` | 5,274 B | 4,983 B | all | 8,350 B | 21 |
| `get_session` | 3,775 B | 3,567 B | all | 11,227 B | 12 |
| `search_sessions` | 3,459 B | 3,368 B | all | 5,361 B | 8 |
| `list_sessions` | 3,101 B | 3,010 B | all | 4,212 B | 8 |
| `query_session_index` | 1,858 B | 1,724 B | all | 2,996 B | 8 |
| `get_resume_command` | 365 B | 348 B | all | 2,811 B | 8 |
| `get_index_status` | 255 B | 238 B | all | 5,729 B | 9 |

Catalogue `tools[]` 92,017 B; the whole `tools/list` JSON-RPC
message 92,061 B. Those are two stages, not a
discrepancy, and a figure quoted for either must say which it measured.

At the baseline this work started from, `search_messages` measured 9,954 bytes as Codex
counts it and `run_skill_capability` 5,945, both over the 5,000-byte budget, so Codex
deleted every parameter description on both before any model saw them. Every tool is now
under, and every description survives.

## Limits, and what was actually observed

```
PENDING mcp-output-schema-depth — MCP specification, outputSchema nesting depth
        outputSchema nesting depth measured 21 levels against the 10 levels limit of MCP specification. The client rejects the artifact outright. The specification tells clients to apply a maximum schema depth to prevent a denial-of-service vector but prescribes no number. Measured here: 21, 18, 12, 9, 9 and three at 8. A blanket $defs extraction over the emitted documents was written and measured: it reaches 15, 12, 9 and five at 8, takes the catalogue's output schemas from 59,931 to 55,699 bytes, and brings the subschema count from 256 to 198. It was not shipped. Those bytes reach no model on any client read in source, no measured client enforces a depth bound at all, and the cost is real: every consumer that navigates the document by path has to resolve pointers, and the pass names two dozen shapes mechanically, without the descriptions S3-every-named-type-has-a-description requires. Extraction by hand on the three tools that pay for it, with names a reader recognises, is the shape worth shipping.
        run_skill_capability: 21 levels against the 10 limit (deepest nested container, root at 1)
        search_messages: 18 levels against the 10 limit (deepest nested container, root at 1)
        get_session: 12 levels against the 10 limit (deepest nested container, root at 1)
        Not enforced yet; WP-GQ-deduplicate-and-correct-output-schemas sets enforced: true in the same change that makes it pass.
        Raise when: A legitimate schema needs more nesting than extraction can flatten. Name a repeated type first; a $ref is a leaf at its point of use.
        Lower when: A client is found that enforces a tighter bound.
WARN    codex-input-schema-margin — Codex 0.146.0, Codex-normalized inputSchema
        search_messages: 4995 bytes against the 4750 limit (as Codex counts it)
        run_skill_capability: 4983 bytes against the 4750 limit (as Codex counts it)
        Raise when: The achievable size drops far enough that a tighter line stays actionable.
        Lower when: Codex raises its budget and the extra headroom is genuinely available.
NOTE    3 response-artifact rows need a tools/call fixture and are not measured here;
        they land with WP-F-bound-mcp-results-without-silent-reduction:
        codex-tool-result-chars: 48000 characters
        claude-code-tool-result-tokens: 25000 tokens
        gemini-cli-tool-result-chars: 40000 characters
104 measurements: 99 pass, 2 warn, 1 rules pending, 0 fail
Pending rules (measured, not yet enforced): mcp-output-schema-depth
```

## Not verified here, and why

The five cold-agent decision tasks are not recorded. They need a model driving an
installed client end to end -- choose the right session among several carrying the same
text, tell a spawned run from its root, continue a second page without duplicates, expand
the exact message -- and the observation that matters is what the agent did, not what the
server emitted. That is a separate run against live clients, not something this session
can honestly self-report.

What is recorded instead is every server-side fact those tasks depend on: the schema
reaches each client under its budget with descriptions intact, a default result fits every
published result cap, and a plain query returns the repo, title, recency and parent-run
evidence needed to choose. Those are asserted by
`a_default_search_result_fits_every_client_result_cap` and
`a_plain_query_returns_enough_to_choose_between_sessions` against a synthetic corpus.
