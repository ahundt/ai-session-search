# AI Session Search core

This crate contains the provider adapters, normalized domain model, SQLite
index, query services, CLI, and MCP transport for AI Session Search.

Use the [repository README](../../README.md) for installation and user-facing
documentation. Architecture and migration decisions are recorded under
[`docs/migration`](../../docs/migration/).

The crate builds one executable, `aise`. Run `aise mcp serve` for the Rust MCP
stdio transport; installer-generated client entries use that same command.

The supported application API is available directly from the crate root:

```rust
use ai_session_search::{MessageFilters, SearchFilters, SessionSearch};
```

Library-only consumers may set `default-features = false` to omit the CLI
release-check network client. The search, index, export, and recovery APIs do
not require the `release-check` feature.

Session list, ranked search, bundle export, and analysis intersect each resolved inclusive date
period with the known indexed session span `[created_at, updated_at]`. `since` requires the span end
to be on or after the query start; `until` requires the span start to be on or before the query end;
`when` applies both. Exact RFC 3339 values remain zero-width instants. A known span can contain gaps
and does not imply continuous process activity. Message, file, and event-analytics filters retain
scalar event-timestamp semantics.

`SessionSearch` owns configuration, the SQLite connection, and service lifetimes.
Keep one instance for a related unit of work, then compose the immutable filter and
publication types re-exported beside it. Existing module paths remain supported; storage,
CLI, MCP, provider, and PyO3 modules are implementation surfaces rather than the recommended
entry point.

Licensed under Apache-2.0. See [LICENSE](../../LICENSE) and
[NOTICE](../../NOTICE).
