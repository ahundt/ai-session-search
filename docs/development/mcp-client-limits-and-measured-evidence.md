<!-- SPDX-FileCopyrightText: 2026 Andrew Hundt -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# MCP client limits

Client limits can discard descriptions or truncate results without an MCP error. The server
checks its emitted catalogue and a synthetic tool response against the client contracts recorded
in `rust/ai-session-search-core/src/mcp_schema_budget.rs`. Each limit records its unit, source,
pinned client version, failure mode, and conditions for changing it.

```bash
aise mcp schema-budget
aise mcp schema-budget --catalogue
aise mcp schema-budget --ledger
```

The first command checks this executable's catalogue using its active configuration. `--catalogue`
prints what the server advertises. `--ledger` separates catalogue, schema, transport, response, and
model-input costs; an unverified client transformation stays explicitly unverified. These commands
do not establish how an arbitrary newer client behaves.

## Inclusive limits

Equality with a limit passes. A notice threshold is an earlier review point.

| Status | Meaning |
| --- | --- |
| `PASS` | Within the inclusive ceiling and any notice threshold |
| `NOTICE` | Above the notice threshold and within the ceiling |
| `WARNING` | Above a nonblocking ceiling |
| `FAIL` | Above a blocking ceiling, or any exceeded ceiling in strict mode |

Use `--strict` to fail every exceeded rule. The output names the artifact, measured value,
unit, limit, evidence source, and what must change. Repository policy rows describe schema style
or structure; they are separate from client caps.

## Schema descriptions

Codex measures a sanitized and normalized input schema. Its 5,000-byte cap can remove every
parameter description. The checker reproduces that transformation, including inferred types,
and verifies that advertised descriptions survive. Description caps in JavaScript clients count
UTF-16 code units; result budgets and schema byte budgets use their own declared units.

`EMITTED_ARTIFACT_CEILINGS` pins per-tool normalized input bytes and output-schema document depth.
Change a pin only with the schema change and its measurement. Output-schema use-path depth follows
references as leaves and excludes the definitions table; document depth counts JSON containers.
The repository's depth guard is a policy, not a universal MCP depth limit.

Defaults are stated in prose. The catalogue omits JSON Schema `default` keywords so clients do
not turn omitted values into explicit arguments that conflict with a caller's choices. Shared
message-search defaults, accepted values, and conflict rules are generated from the planner;
see the [caller reference](../README.md#generated-message-search-caller-contract).

`purpose` and `purpose_version` remain advertised when no presets are configured, with guidance
to omit them. Configured purpose names appear in both the enum and its description, so adding
presets grows the schema. The server checks the configured catalogue once per connection and
warns on stderr if it exceeds an enforced budget.

## Tool results

`mcp.max_tool_result_chars` limits the serialized `CallToolResult`, including its text and
structured payload. A tool response beyond that ceiling fails with tool-specific retry guidance
instead of silently truncating matching records. Smaller pages, fewer context messages, and
presentation budgets can reduce a response; use only the controls that tool advertises.
The checker dispatches a real search over a synthetic fixture to measure response artifacts.

The server's delivery ceiling and a client's output cap are separate. The checker compares the
configured ceiling with silent client caps as well as measuring fixture output. A cap that announces
overflow has different recovery behavior from one that drops content without a marker.

## Offline evidence and review

`HarnessLimit.evidence` is the structured provenance used by the checker and its tests.
The table below is checked byte-for-byte by
`evidence_document_and_config_defaults_match_the_owning_rows`. Its failure prints the replacement
table and the matching config-example comments. Caps remain inclusive. `--ledger` includes the
same records as `limit_evidence`, per-field normalized value byte costs, and pin-change reports.
A field cost excludes its property name, colon and map separators.

The `checked` date records the last source check, not a claim about the client currently connected.
Historical binary inspections retain their original dates; only the pinned Codex schema and Gemini
constant were rechecked on 2026-11-14. The VS Code snapshot lacks an immutable commit: capture one on
the next review. Closed-client package URLs identify the inspected release; the package page does
not establish a constant without inspecting that release's binary.

After 30 days, `aise mcp schema-budget` prints a non-failing evidence notice;
`--ledger` exposes these as `evidence_notices`. Tests inject the clock and never fetch sources.
Thirty days is a review policy based on the recorded Codex move from 4,000 to 5,000 on
2026-07-08, 27 days before the 2026-08-04 observation. That history does not establish a release
cadence for other clients. Old evidence remains usable and visible; it does not expire a build.

<!-- client-limit-evidence:start -->
| Rule | Cap (inclusive) | Client | Revision | Observed | Checked | Source | Measurement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `codex-input-schema-bytes` | 5000 bytes | Codex | rust-v0.146.0; blob 7c9edd08e167626cc524962884baa3ad626247a0 | 2026-08-04 | 2026-11-14 | https://github.com/openai/codex/blob/rust-v0.146.0/codex-rs/tools/src/json_schema.rs | MAX_COMPACT_TOOL_SCHEMA_BYTES; compact UTF-8 JSON after sanitize, inferred types and JsonSchema projection; first compaction pass strips all descriptions |
| `codex-input-schema-margin` | 4750 bytes | Codex | rust-v0.146.0; blob 7c9edd08e167626cc524962884baa3ad626247a0 | 2026-08-04 | 2026-11-14 | https://github.com/openai/codex/blob/rust-v0.146.0/codex-rs/tools/src/json_schema.rs | MAX_COMPACT_TOOL_SCHEMA_BYTES; compact UTF-8 JSON after sanitize, inferred types and JsonSchema projection; first compaction pass strips all descriptions |
| `claude-code-tool-description-chars` | 2048 UTF-16 units | Claude Code | 2.1.220 (corroborated by 2.1.88 source) | 2026-08-03 | 2026-08-03 | https://www.npmjs.com/package/@anthropic-ai/claude-code/v/2.1.220 | Recorded binary/source inspection: services/mcp/client.ts MAX_MCP_DESCRIPTION_LENGTH uses JS UTF-16 length; utils/mcpValidation.ts MAX_MCP_OUTPUT_TOKENS persists overflow. Reinspect that installed version; package page alone does not prove the constants. |
| `vscode-parameter-description-chars` | 1024 UTF-16 units | VS Code / Copilot | 2026-08-02 source snapshot; immutable commit not recorded | 2026-08-02 | 2026-08-02 | https://github.com/microsoft/vscode-copilot-chat/blob/main/src/extension/tools/common/toolSchemaNormalizer.ts | gpt4oMaxStringLength uses JS UTF-16 length for schema-internal descriptions on GPT-4 families; root combinators rejected for all families. Capture an immutable commit when rechecking. |
| `vscode-no-root-combinator` | 0 root combinators | VS Code / Copilot | 2026-08-02 source snapshot; immutable commit not recorded | 2026-08-02 | 2026-08-02 | https://github.com/microsoft/vscode-copilot-chat/blob/main/src/extension/tools/common/toolSchemaNormalizer.ts | gpt4oMaxStringLength uses JS UTF-16 length for schema-internal descriptions on GPT-4 families; root combinators rejected for all families. Capture an immutable commit when rechecking. |
| `mcp-output-schema-depth` | 10 levels | MCP specification / repository policy | MCP 2026-07-28; repository policy 1.1.0 | 2026-08-03 | 2026-08-03 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | Schema depth and subschema count are repository choices, NOT numeric MCP requirements; count actual JSON containers/schema positions. Local refs and described reachable definitions are repository guards. |
| `mcp-output-schema-point-of-use-depth` | 6 levels | MCP specification / repository policy | MCP 2026-07-28; repository policy 1.1.0 | 2026-08-03 | 2026-08-03 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | Schema depth and subschema count are repository choices, NOT numeric MCP requirements; count actual JSON containers/schema positions. Local refs and described reachable definitions are repository guards. |
| `style-named-definitions-are-described` | 0 undescribed definitions | MCP specification / repository policy | MCP 2026-07-28; repository policy 1.1.0 | 2026-08-03 | 2026-08-03 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | Schema depth and subschema count are repository choices, NOT numeric MCP requirements; count actual JSON containers/schema positions. Local refs and described reachable definitions are repository guards. |
| `mcp-output-schema-subschemas` | 250 subschemas | MCP specification / repository policy | MCP 2026-07-28; repository policy 1.1.0 | 2026-08-03 | 2026-08-03 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | Schema depth and subschema count are repository choices, NOT numeric MCP requirements; count actual JSON containers/schema positions. Local refs and described reachable definitions are repository guards. |
| `anthropic-tool-name-charset` | 64 characters matching ^[a-zA-Z0-9_-]{1,64}$ | Anthropic Messages API | API documentation snapshot; Claude Code SDK 2.1.220 | 2026-08-03 | 2026-08-03 | https://platform.claude.com/docs/en/api/messages | Tool name pattern ^[a-zA-Z0-9_-]{1,64}$; compare Codex 64 and MCP SDK 128 (dots allowed). Recheck name pattern, not schema description bytes. |
| `codex-tool-result-chars` | 48000 characters | Codex | 0.146.0 | 2026-08-03 | 2026-08-03 | https://github.com/openai/codex/blob/rust-v0.146.0/codex-rs/core/src/tools/context.rs | Recorded context.rs and models-manager/models.json inspection: about 12000 tokens at four characters/token, an ESTIMATE, not a universal character guarantee; middle truncation has no marker |
| `claude-code-tool-result-tokens` | 25000 tokens | Claude Code | 2.1.220 (corroborated by 2.1.88 source) | 2026-08-03 | 2026-08-03 | https://www.npmjs.com/package/@anthropic-ai/claude-code/v/2.1.220 | Recorded binary/source inspection: services/mcp/client.ts MAX_MCP_DESCRIPTION_LENGTH uses JS UTF-16 length; utils/mcpValidation.ts MAX_MCP_OUTPUT_TOKENS persists overflow. Reinspect that installed version; package page alone does not prove the constants. |
| `gemini-cli-tool-result-chars` | 40000 characters | Gemini CLI | f47d6c6f | 2026-07-31 | 2026-11-14 | https://github.com/google-gemini/gemini-cli/blob/f47d6c6f/packages/core/src/config/config.ts | DEFAULT_TRUNCATE_TOOL_OUTPUT_THRESHOLD; announced head/tail truncation persists omitted output. Forks have different constants. |
| `style-no-aise-vendor-keys` | 0 keys | MCP specification / repository policy | MCP 2026-07-28; repository policy 1.1.0 | 2026-08-03 | 2026-08-03 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | Schema depth and subschema count are repository choices, NOT numeric MCP requirements; count actual JSON containers/schema positions. Local refs and described reachable definitions are repository guards. |
| `style-no-input-const` | 0 sites | Codex | rust-v0.146.0; blob 7c9edd08e167626cc524962884baa3ad626247a0 | 2026-08-04 | 2026-11-14 | https://github.com/openai/codex/blob/rust-v0.146.0/codex-rs/tools/src/json_schema.rs | MAX_COMPACT_TOOL_SCHEMA_BYTES; compact UTF-8 JSON after sanitize, inferred types and JsonSchema projection; first compaction pass strips all descriptions |
| `style-no-fake-integer-maxima` | 0 sites | MCP specification / repository policy | MCP 2026-07-28; repository policy 1.1.0 | 2026-08-03 | 2026-08-03 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | Schema depth and subschema count are repository choices, NOT numeric MCP requirements; count actual JSON containers/schema positions. Local refs and described reachable definitions are repository guards. |
| `style-local-refs-only` | 0 non-local refs | MCP specification / repository policy | MCP 2026-07-28; repository policy 1.1.0 | 2026-08-03 | 2026-08-03 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | Schema depth and subschema count are repository choices, NOT numeric MCP requirements; count actual JSON containers/schema positions. Local refs and described reachable definitions are repository guards. |
| `style-reachable-definitions` | 0 entries | Codex | rust-v0.146.0; blob 7c9edd08e167626cc524962884baa3ad626247a0 | 2026-08-04 | 2026-11-14 | https://github.com/openai/codex/blob/rust-v0.146.0/codex-rs/tools/src/json_schema.rs | MAX_COMPACT_TOOL_SCHEMA_BYTES; compact UTF-8 JSON after sanitize, inferred types and JsonSchema projection; first compaction pass strips all descriptions |
<!-- client-limit-evidence:end -->

## Connected client and protocol versions

The checker sweeps a global configured set of contracts. The server does not select numeric caps
from `initialize.clientInfo.name`, `clientInfo.version` or the negotiated MCP protocol version.
The protocol version describes protocol features; it does not negotiate these client constants.
The result ceiling defaults to the measured silently-truncating client's estimate, rather than the
smallest announced cap. Clients with larger budgets may need more pages. Registration-specific
environment overrides allow an operator to track a known deployment without changing the machine
default. They change validation/delivery policy, not the upstream client's behavior.

Automatic per-client selection would need a verified client-name alias map, version ranges tied
to the evidence records, a conservative unknown-client fallback, connection-local resolved policy
passed into catalogue checks and result delivery, and initialize/transport tests for known,
unknown, missing and misleading client identities. Protocol negotiation alone cannot supply it.

## Tracking a client change

MCP initialization does not negotiate these numeric caps. Update an operator override only when
that client's contract has changed; see [client limit budgets](configuration.md#client-limit-budgets).
`[mcp.client_limits]` and matching environment variables override movable client caps. Unknown file
keys are ignored and reported by `aise config check` and `aise doctor`; unknown environment row
names, invalid numbers, and overrides of structural policy rows are rejected.

For a source update, change the owning row's provenance and tests, then run the schema-budget suite
and measure the built executable. Keep byte ceilings separate from measurements of response content,
startup time, and model behavior. A green static check establishes the declared contract for the
emitted artifact; client runtime conformance requires its own test.
