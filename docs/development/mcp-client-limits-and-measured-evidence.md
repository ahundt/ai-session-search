<!-- SPDX-FileCopyrightText: 2026 Andrew Hundt -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# MCP client limits and measured evidence

Last measured 2026-08-05 against the executable built from this tree.

What this build emits, measured against every client limit it can breach, and which of
those clients were actually available to check. A limit that could not be observed is
recorded as unverified. None is recorded as a pass.

Reproduce the measurements with `aise mcp schema-budget --ledger` and
`aise mcp schema-budget`, which run against the executable's own catalogue.

## Clients on this machine

| Client | Version | Schema limits checkable | Result limits checkable |
|---|---|---|---|
| Codex | codex-cli 0.146.0 | yes, sanitize and normalization reimplemented from its source, cross-checked by compiling that source | yes, from a response fixture |
| Claude Code | 2.1.222 (Claude Code) | yes, description caps | yes, from a response fixture |
| OpenCode | not installed | no | no |
| VS Code | not installed | no | no |

Codex and Claude Code are installed and their limits are measured below. OpenCode and
VS Code are not installed here, so their rows are unverified rather than passing: the
checker still measures the artifact against their published caps, but nothing on this
machine confirms how those clients treat it.

## None of these limits is announced on the wire

No client states these caps over MCP. In the specification revision these rows were
verified against (2026-07-28), `initialize` capability negotiation carries no size
limits, and where the specification addresses size at all — schema depth, subschema
count — it instructs clients to apply a bound while deliberately prescribing no number.
The one wire-adjacent channel measured runs the other way: Claude Code honors a
server-side `_meta` `anthropic/maxResultSizeChars` declaration up to 500,000 characters,
a server asking for room, not a client announcing its cap. If a future revision or
client adds a real announcement channel, the announced value must take precedence over
the compiled-in table for that client — respecting a stated limit always beats modeling
an unstated one.

Until then every number here is read from the client's own source or shipped binary at a
pinned version, and is fragile by nature: it can move in any client release with no
signal this server can observe. Where each number was read — the exact file and line,
the version, and for Codex the git blob hash held across its release tags — is recorded
per row in the `platform` field that `aise mcp schema-budget` prints, which is the
canonical where-to-find-it record; this document quotes it but does not replace it. When
a client moves first, `[mcp.client_limits]` tracks the new number between releases
([configuration guide](configuration.md)).

## Status vocabulary and the inclusive boundary

The checker uses the same boundary rule for every measured unit: `measured == limit` is
valid, and only `measured > limit` is over the limit. A notice threshold is a separate,
earlier tripwire; it never turns the inclusive ceiling itself into a breach.

| Status | Condition | Meaning |
|---|---|---|
| `PASS` | at or below the notice threshold, or no notice threshold | within the inclusive limit |
| `NOTICE` | above the notice threshold and at or below the limit | close enough to review, not a breach |
| `WARNING` | above the limit while the rule is non-blocking | a real breach, reported without stopping the default check |
| `FAIL` | above the limit in the default gate or strict mode | the check fails |

This separates proximity from severity: `NOTICE` is for an in-limit margin, while
`WARNING` starts only after an actual limit breach. Whether a warning blocks the default
check is policy detail, not another user-facing severity level.

There is no ISO-defined universal status scale for this kind of deterministic budget check.
The closest interoperable conventions are [SARIF 2.1.0's `note`, `warning`, and `error` levels](https://docs.oasis-open.org/sarif/sarif/v2.1.0/os/sarif-v2.1.0-os.html#def_level)
and [GitHub Checks' `notice`, `warning`, and `failure` levels](https://docs.github.com/en/enterprise-cloud%40latest/rest/checks/runs).
This checker uses the GitHub-facing words for the two non-failing levels and keeps `FAIL`
as the clear gate outcome. Rust's standard library has no generic diagnostic-severity enum;
`Poll::Pending` means an asynchronous operation is not ready, so it is not appropriate here.

## Emitted catalogue, per tool

Two depth figures, because one can be satisfied without helping anyone. **Use path** is
what a reader walks: a `$ref` ends it and the `$defs` table is not on it, so only steps a
value takes are counted. **Document** counts every JSON container, which is roughly twice
the nesting a response has, because a `properties` map is itself a level.

| Tool | inputSchema wire | outputSchema | use-path depth | document depth |
|---|---:|---:|---:|---:|
| `search_messages` | 5,077 B | 16,537 B | 6 | 10 |
| `run_skill_capability` | 4,969 B | 8,227 B | 5 | 9 |
| `search_sessions` | 4,341 B | 5,361 B | 4 | 8 |
| `get_session` | 4,272 B | 10,905 B | 5 | 10 |
| `list_sessions` | 3,983 B | 4,212 B | 4 | 8 |
| `query_session_index` | 1,858 B | 2,996 B | 4 | 8 |
| `get_resume_command` | 391 B | 2,811 B | 4 | 8 |
| `get_index_status` | 255 B | 6,681 B | 5 | 9 |

Catalogue `tools[]` 91,631 B. Total `outputSchema` 57,730 B, from 60,802 B before the
shapes on the deepest chains were named. Every tool's descriptions reach the model.

At the baseline this work started from, `search_messages` measured 9,954 bytes as Codex
counts it and `run_skill_capability` 5,945, both over the 5,000-byte budget, so Codex
deleted every parameter description on both before any model saw them. The first
shrinking pass brought the gate green while the checker still measured without Codex's
sanitize pass, so both tools actually shipped at 5,043 and 5,047 bytes — still over,
still stripped of all 37 and 19 parameter descriptions, with the gate reporting a
five-byte margin. Every tool is now under as Codex itself measures, every description
survives, and the sweep asserts survival directly rather than inferring it from a byte
count.

A second, larger loss was corrected at the same time. An earlier deduplication pass
had replaced 28 parameter descriptions with shared per-concept sentences — 3,843 of
5,785 bytes of per-tool documentation gone — although four of the five tools that paid
were never over the budget, and the shared sentences were wrong for some tools they
covered. Every discarded description is restored: the four tools that always fit ship
their full text again (the fields `search_sessions` and `list_sessions` share come
from shared builder helpers so the copies cannot diverge), and `run_skill_capability`
restores its facts in streamlined form at 4,742 bytes as Codex measures it, under the
4,750-byte margin threshold. The transferable lesson: a deduplication test proves the copies
agree, never that the surviving copy is right, so
`the_two_since_meanings_stay_apart_on_the_wire` now pins each tool's semantics at the
emitted catalogue instead of copy identity.

Document depth was 21, 18 and 12 on the three deepest tools against a guard of 10. Naming
the repeated shapes on those chains brought every tool inside it. The use-path figures are
what answers whether the response was ever really that deep: `search_messages` counts 18
containers but 6 levels of data, so a reader expecting five or six levels was right about
the response and the older metric was measuring the schema's spelling.

**10 is this repository's guard, not an MCP limit.** The specification asks clients to
bound schema depth and deliberately prescribes no number.

## Limits, and what was actually observed

```
NOTICE  mcp-output-schema-point-of-use-depth — this repository's schema policy, outputSchema depth along a use
        path, with $ref as a leaf and $defs excluded
        search_messages: 6 levels; notice above 5, inclusive ceiling 6 levels (deepest use path, $ref as a leaf, at
          /properties/effective_request/properties/presentation/properties/field_view/properties/kind/enum/0)
        Raise when: A response genuinely gains a level of structure. Name the path and the reason
          in the requirements document first; do not raise it to make a test green.
        Lower when: The response shape is flattened and the lower figure holds across a release.
123 measurements: 122 pass, 1 notice, 0 warning, 0 fail
```

No measured breach is present. The one notice is a margin tripwire that is expected to fire: the
deepest use path is at the inclusive ceiling, so the notice means "remeasure before adding
a level," not "the limit was breached." A warning would begin only above the ceiling.
The two `codex-input-schema-margin` notices that used to sit beside it are gone: both tools
now measure under the 4,750-byte margin threshold as Codex counts them.

### Codex sanitizes before it measures, and the checker now does too

Codex applies its 5,000-byte budget to the schema after its own sanitize pass
(`parse_tool_input_schema` → `prepare_tool_input_schema` → `compact_large_tool_schema`),
and sanitizing can make a schema larger: a bare `enum` with no `type` gets
`"type": "string"` inferred and written back, 16 bytes each, at three sites in this
catalogue. The checker previously modeled the deserialization round trip but not the
sanitize pass, so it reported `search_messages` at 4,995 bytes while Codex measured
5,043, and `run_skill_capability` at 4,983 against a real 5,047 — both over, so Codex
silently deleted every parameter description on both before any model saw them, while
the gate reported a five-byte margin as green.

Measured by compiling Codex's own `json_schema.rs` — whose content is byte-identical at
every release tag from `rust-v0.145.0-alpha.18` through `rust-v0.146.0-alpha.13`, the
newest tagged release — after moving duplicated clauses into `tool.description`, which
Codex measures separately and never charges against the 5,000:

| Tool | before | after | margin | descriptions surviving |
|---|---:|---:|---:|---|
| `search_messages` | 5,043 **over** | 4,631 | 369 | 37/37 |
| `run_skill_capability` | 5,047 **over** | 4,742 | 258 | 19/19 |

`run_skill_capability`'s figure includes the restored parameter documentation described
above; the streamlining alone had taken it to 4,191 before the discarded facts were
put back.

The byte rows are a proxy for the property that matters — the caller is shown what we
wrote. `every_advertised_description_reaches_the_model` asserts that property directly,
and `every_clause_moved_off_a_parameter_is_published_somewhere_the_caller_reads` pins
each moved clause to the channel that now carries it, so a fact deleted from both
channels at once cannot pass both tests.

### The schema is configuration-dependent, and purpose bundles are the unbounded knob

The margin is not fixed, because the schema is not fixed. The integer knobs
(`search_messages_limit`, `preview_chars`, `lines_per_message`) are interpolated into
descriptions and cost one byte per extra decimal digit — enough to spend the old
five-byte margin, not enough to reach 369. A configured purpose bundle is a user-chosen
name landing both in an `enum` Codex keeps and in the prose beside it: the first costs
about a hundred bytes and each further one about forty-five, and nothing bounds the
count or the name length. Measured: ten ordinary named bundles put `search_messages`
past 5,000. The release gate cannot see an operator's configuration — it measures the
catalogue built from the default — so the breach would be silent by construction.

The server is the last component that still knows. It measures its own emitted catalogue
when it builds it, once per connection, and writes any enforced breach to stderr where a
client shows server output. It warns rather than refuses, because the schema is degraded
rather than invalid. The warning path was verified against a live `aise mcp serve` when
the margin was five bytes and the three integer knobs breached it; the sweep now pins the
stronger set property (`no_configuration_breaches_the_budget_without_telling_the_operator`):
for the shipped default and for a configuration with every schema knob widened, the set
of tools over the budget and the set the operator is warned about are the same set.

### A configured ceiling can sit above a silent client cap

`mcp.max_tool_result_chars` decides what this server delivers; a client's own cap decides what
survives. The shipped 48,000 is Codex's, chosen because Codex truncates from the middle with no
marker while Claude Code and Gemini CLI announce the overflow and persist it -- which is why
Gemini's numerically smaller 40,000 was not the number to defend.

Measured: `max_tool_result_chars = 500000` passed the sweep clean. Ten times Codex's cap, no
signal anywhere, and every layer working as designed -- the result is inside the configured
ceiling so nothing errors, and no row compared a configured bound against a client bound.

The configured ceiling is now compared against every response row whose failure mode is silent,
at serve time and in the sweep, reading the configured client cap so an operator tracking a
client that raised its own states it once. Announced rows are excluded on purpose: exceeding one
costs a round trip and a file read rather than data.

### Every declared row is measured

All ten declared rows are measured. The three that bound a `tools/call` result were reported
as needing a fixture for as long as the checker had none; it now builds the same synthetic
corpus the stage ledger measures, runs one real search through the production dispatcher, and
sweeps those rows against the serialized `CallToolResult`. A fixture that fails to build
leaves them unmeasured and says so on stderr, because a row scored without an artifact is the
same defect as an unobserved stage reported as zero bytes.

### The checker measures in the client's own units, and a dead override is a found defect

A second adversarial pass (2026-08-04) was run against the checker itself rather than the
schemas, on the premise that a checker defect and a clean surface report identically. Five
survived verification and are fixed:

- **UTF-16 units.** Both description caps live in JavaScript clients, where `.length` counts
  UTF-16 code units, so a supplementary-plane character costs two against the client's budget.
  The checker counted Unicode scalar values: a `tool.description` of 1,030 such characters
  measured 1,030 and passed while Claude Code counts 2,060 and silently truncates at 2,048.
  Both rows now count `encode_utf16()` and their units say so. Today's catalogue is unaffected
  — its only non-ASCII is BMP punctuation, one unit either way — which is exactly why the test
  had to construct the breach.
- **The tool-name budget was dead.** `anthropic-tool-name-charset` measured a sentinel (0 when
  acceptable, 65 when not) computed against a hardcoded 64, never the resolved budget, so the
  `[mcp.client_limits]` override its own `raise_when` promises did nothing in either
  direction: a budget lowered to 32 admitted a 40-character name, and one raised to 100
  admitted a dotted name no length can cure. It now measures the real length; a charset
  violation reports above whatever budget is in force.
- **A malformed override was dropped, not rejected.** An
  `AI_SESSION_SEARCH_CLIENT_LIMIT_*` value that failed to parse was discarded during
  environment capture, before any validation could see it — the registration looked like it
  had moved a budget and the shipped one silently stayed in force, the exact defect the
  result-ceiling resolver documents refusing. Capture now keeps the raw text and `resolve`
  rejects it naming the variable and the value it carried.
- **A zero delivery ceiling loaded from the config file.** The environment path enforces a
  floor of 1 for `mcp.max_tool_result_chars`, but the config-file path did not, and the two
  sibling knobs in the same table use `0` to mean disabled — so an operator writing `0` for
  "no ceiling" got every tool call rejected instead. `validate` now enforces the same floor
  with the same rationale.
- **Structural rows took overrides.** Every `style-*` row and the root-combinator rejection
  accepted a `[mcp.client_limits]` entry that could only silence the checker while the client
  kept rejecting the schema, and every breach warning advised setting one. Rows now declare
  whether their number is a movable client cap; configuration refuses an override for one
  that is not, `resolved_budget` ignores such overrides as an invariant, and a structural
  breach warning now says to fix the schema rather than the number.

## Observed against the installed server

Run through `aise mcp serve` as a client launches it, not only in tests.

| Layer | Observed |
|---|---|
| Registered binary | `/Users/athundt/.local/bin/aise`, 1.0.0-rc.1, installed from the verified source tree with `uv tool install --force .`; Codex and Claude Code both register that exact path |
| Current installed hashes | executable `255daf9919dd2bf57487ab83bc5be6cc316664e6be35ed4afaf5d78d4fe882a3`; native module `c4cc938c01f3acef765f8ef5c0e595d69b3b98f21ca7c3a272dfce52b950d632` (arm64) |
| Retained release artifacts | The b246d1c wheel/sdist and their recorded hashes remain historical; they are not byte-identical to this source-installed follow-up |
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
