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

`purpose` and `purpose_version` are advertised only when presets are configured. Purpose names
appear in both the enum and its description, so adding presets grows the schema. The server checks
the configured catalogue once per connection and warns on stderr if it exceeds an enforced budget.

## Tool results

`mcp.max_tool_result_chars` limits the serialized `CallToolResult`, including its text and
structured payload. A tool response beyond that ceiling fails with tool-specific retry guidance
instead of silently truncating matching records. Smaller pages, fewer context messages, and
presentation budgets can reduce a response; use only the controls that tool advertises.
The checker dispatches a real search over a synthetic fixture to measure response artifacts.

The server's delivery ceiling and a client's output cap are separate. The checker compares the
configured ceiling with silent client caps as well as measuring fixture output. A cap that announces
overflow has different recovery behavior from one that drops content without a marker.

## Tracking a client change

MCP initialization does not negotiate these numeric caps. Update an operator override only when
that client's contract has changed; see [client limit budgets](configuration.md#client-limit-budgets).
`[mcp.client_limits]` and matching environment variables override movable client caps. Unknown row
names, invalid numbers, and overrides of structural policy rows are rejected.

For a source update, change the owning row's provenance and tests, then run the schema-budget suite
and measure the built executable. Keep byte ceilings separate from measurements of response content,
startup time, and model behavior. A green static check establishes the declared contract for the
emitted artifact; client runtime conformance requires its own test.
