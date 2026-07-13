# AI Session Search core

This crate contains the provider adapters, normalized domain model, SQLite
index, query services, CLI, and MCP transport for AI Session Search.

Use the [repository README](../../README.md) for installation and user-facing
documentation. Architecture and migration decisions are recorded under
[`docs/migration`](../../docs/migration/).

The temporary `aise-mcp` executable remains available while CLI and MCP parity,
packaging, and lifecycle gates are completed. The migration plan makes
`aise mcp serve` the final consolidation step, after which the second executable
will be removed.

Licensed under Apache-2.0. See [LICENSE](../../LICENSE) and
[NOTICE](../../NOTICE).
