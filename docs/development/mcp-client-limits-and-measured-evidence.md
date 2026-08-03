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

Two depth figures, because one can be satisfied without helping anyone. **Use path** is
what a reader walks: a `$ref` ends it and the `$defs` table is not on it, so only steps a
value takes are counted. **Document** counts every JSON container, which is roughly twice
the nesting a response has, because a `properties` map is itself a level.

| Tool | inputSchema wire | outputSchema | use-path depth | document depth |
|---|---:|---:|---:|---:|
| `search_messages` | 5,489 B | 16,537 B | 6 | 10 |
| `get_session` | 3,775 B | 10,905 B | 5 | 10 |
| `run_skill_capability` | 5,274 B | 8,227 B | 5 | 9 |
| `get_index_status` | 255 B | 5,729 B | 5 | 9 |
| `search_sessions` | 3,459 B | 5,361 B | 4 | 8 |
| `list_sessions` | 3,101 B | 4,212 B | 4 | 8 |
| `query_session_index` | 1,858 B | 2,996 B | 4 | 8 |
| `get_resume_command` | 365 B | 2,811 B | 4 | 8 |

Catalogue `tools[]` 87,993 B. Total `outputSchema` 56,778 B, from 60,802 B before the
shapes on the deepest chains were named. Every tool's descriptions reach the model.

At the baseline this work started from, `search_messages` measured 9,954 bytes as Codex
counts it and `run_skill_capability` 5,945, both over the 5,000-byte budget, so Codex
deleted every parameter description on both before any model saw them. Every tool is now
under, and every description survives.

Document depth was 21, 18 and 12 on the three deepest tools against a guard of 10. Naming
the repeated shapes on those chains brought every tool inside it. The use-path figures are
what answers whether the response was ever really that deep: `search_messages` counts 18
containers but 6 levels of data, so a reader expecting five or six levels was right about
the response and the older metric was measuring the schema's spelling.

**10 is this repository's guard, not an MCP limit.** The specification asks clients to
bound schema depth and deliberately prescribes no number.

## Limits, and what was actually observed

```
WARN    codex-input-schema-margin — Codex 0.146.0, Codex-normalized inputSchema
        search_messages: 4995 bytes against the 4750 limit (as Codex counts it)
        run_skill_capability: 4983 bytes against the 4750 limit (as Codex counts it)
        Raise when: The achievable size drops far enough that a tighter line stays actionable.
        Lower when: Codex raises its budget and the extra headroom is genuinely available.
WARN    mcp-output-schema-point-of-use-depth — this repository, outputSchema depth along a use path
        search_messages: 6 levels against the 6 limit
        Raise when: A response genuinely gains a level of structure. Name the path and the reason
          in the requirements document first; do not raise it to make a test green.
        Lower when: The response shape is flattened and the lower figure holds across a release.
NOTE    3 response-artifact rows need a tools/call fixture and are not measured here:
        codex-tool-result-chars: 48000 characters
        claude-code-tool-result-tokens: 25000 tokens
        gemini-cli-tool-result-chars: 40000 characters
120 measurements: 117 pass, 3 warn, 0 rules pending, 0 fail
```

No rule is pending. The three warnings are margin tripwires that are expected to fire: the
two input-schema figures sit about 1% under the budget that binds them, and no measured
route reaches the 4,750-byte warning line, so a warning here means "remeasure before
adding a field" rather than "something regressed".

## Observed against the installed server

Run through `aise mcp serve` as a client launches it, not only in tests.

| Layer | Observed |
|---|---|
| Registered binary | `/Users/athundt/.local/bin/aise`, 1.0.0-rc.1, from `uv tool install` |
| Resolved page | 20, origin `config file` -- an explicit user value, preserved |
| Resolved ceiling | 48,000, origin `typed default` |
| Registration override | `AI_SESSION_SEARCH_MAX_TOOL_RESULT_CHARS=6000` produced an error naming "ceiling of 6000", so the value a registration sets reaches the served result |
| Default result | `limit=2` returned 4,732 characters with `has_more`, `next_offset`, and exact `get_session` coordinates on every hit |
| Over-ceiling recovery | `limit=20` and `limit=3` both overflowed 6,000 and both named `limit=2`; `limit=2` then succeeded |

The recovery row is the one worth keeping. The first suggestion this server produced for
that call was `limit=3`, and `limit=3` measured 6,358 against the same 6,000 ceiling: the
advice failed on the caller's own next call.

It was found by running the installed server rather than by a test, and the reason is
about proportions rather than correctness. The synthetic fixture holds one session and
three short messages, so the two parts the estimator was misclassifying -- the per-session
metadata and the text rendering beside the structured content -- are close to nothing
there. On the real index the same two parts were 1,021 and 4,729 characters of a 30,518
character response, which is where a mistake about them decides the answer. A fixture can
prove the arithmetic; only a real corpus shows which term dominates.

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
