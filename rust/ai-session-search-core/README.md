# AI Session Search core

This crate contains the provider adapters, normalized domain model, SQLite
index, query services, CLI, and MCP transport for AI Session Search.

Use the [repository README](../../README.md) for installation and user-facing
documentation. Architecture and migration decisions are recorded under
[`docs/migration`](../../docs/migration/).

The crate builds one executable, `aise`. Run `aise mcp serve` for the Rust MCP
stdio transport; installer-generated client entries use that same command.

Licensed under Apache-2.0. See [LICENSE](../../LICENSE) and
[NOTICE](../../NOTICE).
