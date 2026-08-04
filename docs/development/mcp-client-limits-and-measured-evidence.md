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
| Claude Code | 2.1.221 (Claude Code) | yes, description caps | yes, from a response fixture |
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
WARN    mcp-output-schema-point-of-use-depth — this repository, outputSchema depth along a use
        path, with $ref as a leaf and $defs excluded
        search_messages: 6 levels against the 6 limit (deepest use path, $ref as a leaf, at
          /properties/effective_request/properties/presentation/properties/field_view/properties/kind/enum/0)
        Raise when: A response genuinely gains a level of structure. Name the path and the reason
          in the requirements document first; do not raise it to make a test green.
        Lower when: The response shape is flattened and the lower figure holds across a release.
123 measurements: 120 pass, 3 warn, 0 rules pending, 0 fail
```

No rule is pending. The three warnings are margin tripwires that are expected to fire: the
two input-schema figures sit about 1% under the budget that binds them, and no measured
route reaches the 4,750-byte warning line, so a warning here means "remeasure before
adding a field" rather than "something regressed".

### The input-schema margin is five bytes, and the schema is configuration-dependent

`search_messages` measures 4,995 bytes against the 5,000 at which Codex deletes every
description. That margin is not fixed, because the schema is not fixed: seven of its
descriptions interpolate a resolved number -- the page, both context counts, the line window,
and the two view budgets -- so each extra decimal digit an operator configures is an extra
byte on the wire.

Measured on the shipped binary, one key at a time:

| Configuration | `search_messages`, as Codex counts it |
|---|---:|
| default | 4,995 |
| `[mcp] search_messages_limit = 1000` | 4,997 |
| `[mcp] preview_chars = 10000` | 4,997 |
| `[mcp] lines_per_message = 100` | 4,997 |
| all three together | **5,001 — breach** |

Three ordinary settings, two bytes each, one byte past the limit. So the answer to whether the
margin is hit by accident is yes, and the release gate cannot see it: the gate measures the
catalogue built from the default configuration, and an operator serves the catalogue built from
theirs.

The breach is silent by construction, so nothing downstream can report it and the server is the
last component that still knows. It now measures its own emitted catalogue when it builds it,
once per connection, and writes any enforced breach to stderr where a client shows server
output. Verified against a live `aise mcp serve`: the default configuration emits nothing, and
the configuration above emits one line naming the tool, 5,001 against 5,000, and what Codex
does next. It warns rather than refuses, because the schema is degraded rather than invalid.

### Every declared row is measured

All ten declared rows are measured. The three that bound a `tools/call` result were reported
as needing a fixture for as long as the checker had none; it now builds the same synthetic
corpus the stage ledger measures, runs one real search through the production dispatcher, and
sweeps those rows against the serialized `CallToolResult`. A fixture that fails to build
leaves them unmeasured and says so on stderr, because a row scored without an artifact is the
same defect as an unobserved stage reported as zero bytes.

## Observed against the installed server

Run through `aise mcp serve` as a client launches it, not only in tests.

| Layer | Observed |
|---|---|
| Registered binary | `/Users/athundt/.local/bin/aise`, 1.0.0-rc.1, installed from the release wheel below; Codex and Claude Code both register that exact path |
| Release artifacts | wheel `6fa873a709791eb9dc0127096ac199ba3d088caf8ead84b63273136a182063ad`, executable `613d6cf5e7a35f0d45626b89af924d8a01ed97f5b663ad17018bb20444f815fb` |
| Artifact the cold-agent runs used | executable `1804bdec7997aebc35ddd4f1c3fea05b3d57256f71b584bcc219dac011245293`, which differs from the release candidate only by two CLI help strings and emits the identical MCP catalogue and results |
| Resolved page | 20, origin `config file` -- an explicit user value, preserved |
| Resolved ceiling | 48,000, origin `typed default` |
| Registration override | `AI_SESSION_SEARCH_MAX_TOOL_RESULT_CHARS=6000` produced an error naming "ceiling of 6000", so the value a registration sets reaches the served result |
| Over-ceiling recovery, `search_messages` | `limit=20` overflowed a 6,000 ceiling at 30,759 characters and named `limit=1`; `limit=1` then returned 3,095 |
| Over-ceiling recovery, `search_sessions` | `limit=0` overflowed the 48,000 ceiling at 154,174 characters and named `limit=23`; `limit=23` then returned 40,563 |

Both recovery rows were wrong before a cold agent ran the tool, and the second was wrong
in a way no fixture had reached. `search_sessions(limit=0)` was told **"there is no smaller
page to ask for"** -- and the same agent then answered the same question with `limit=20`.
`limit=0` means "every match" on the session tools, and that had been folded into the same
state as `get_session`, which advertises no `limit` at all. The advice denied the remedy
the caller went on to use.

Two more defects sat behind it, both invisible while only `search_messages` was exercised.
The count and the items were read from `page.returned` and `results`, which the session
tools do not have; they report `returned` at the top level and carry `sessions`. And the
remedies named `context`, `detail` and `receipt_level` on every pageable tool, when
`search_sessions` and `list_sessions` accept none of the three and instead hold `include`
and `preview_chars` -- three rejected arguments offered while the two that worked went
unmentioned. All three are now read from the failed tool's own advertised schema.

## Cold-agent decision tasks

Five tasks, each run in a **fresh process** of each installed client, with a prompt that
does not contain the answer and ground truth derived separately through the CLI. What is
scored is what the agent did: the arguments it chose, the hit it selected, the field it
cited, and whether it reached the answer without raw SQL or configuration changes.

| Task | What it requires | Codex 0.146.0 | Claude Code 2.1.221 |
|---|---|---|---|
| Choose the right repository among same-phrase hits | filter or read `repo_root`, not take the top hit | pass, 14 calls | pass, 3 calls |
| Tell a spawned run from its root | read `parent_session_id` | pass, 5 calls | pass, 2 calls |
| Continue a page without duplicates | pass the returned `next_offset`, keep `limit` | pass, 2 calls | pass, 2 calls |
| Expand the exact hit | `get_session(session_id, message_seq)` from the hit | pass, 2 calls | pass, 2 calls |
| Recover from an over-ceiling call | follow the returned guidance only | pass, 46 calls | pass, 10 calls |

Both clients named the same session for task 1 and cited `repo_root`; both named the same
spawned run and root for task 2 and cited `parent_session_id`; both returned the same five
`message_seq` values for task 3 and passed `offset=5` with `limit` unchanged; both reported
869 characters and the pre-revision estimate for task 4. **No run used SQL, edited
configuration, or read a transcript file directly.**

Task 1 also corrected this document's own ground truth. The expected answer had been
derived from the top 200 message hits and named a Claude session; both agents named a
Codex session that is genuinely more recent in the same repository, which a check against
the sessions table confirmed.

The task-5 call counts are the honest cost of a deliberately hostile ceiling, not a defect.
At 6,000 characters the per-session metadata is a large share of the envelope, so `limit=1`
was the correct maximum for the arguments Codex kept, and it paged twenty times rather than
trading `include` away. Measured: at that ceiling `include=[]` saves 1,144 characters of a
10,041-character five-result response, which is real but does not change the page much. The
shipped default ceiling is 48,000, and the `limit=20` page this deployment configures measured
30,759 characters against it. The shipped default page is smaller still; the generated
message-search contract table owns that number, so it is not restated here.

## Not verified here, and why

OpenCode and VS Code are not installed on this machine. Their static source-contract checks
run in the gate, but their cold-agent rows stay `unverified` rather than passing: nothing
here confirms how those clients treat the artifact.
