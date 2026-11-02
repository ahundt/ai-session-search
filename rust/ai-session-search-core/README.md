# AI Session Search core

Search your local AI coding sessions from Rust. This crate holds the provider adapters,
normalized domain model, SQLite index, query services, CLI, and MCP transport behind
[AI Session Search](https://github.com/ahundt/ai-session-search), which reads Claude Code,
Claude Desktop, ChatGPT Codex, Cursor, Antigravity, Pi, Prime Agent, Google AI Studio, and
Gemini CLI.

The crate builds one executable, `aise`:

```bash
cargo install ai-session-search --locked
```

`aise mcp serve` is the Rust MCP stdio transport, and the client entries written by
`aise integrations install` invoke that same command.

## Library use

The supported application API is available directly from the crate root:

```rust
use ai_session_search::{MessageFilters, SearchFilters, SessionSearch};
```

`SessionSearch` owns the configuration, the SQLite connection, and the worker pool. Keep one
instance for a related unit of work, then compose the immutable filter and publication types
re-exported beside it. The crate root is the entry point to build against; the storage, CLI, MCP,
provider, and PyO3 modules are implementation surfaces.

Set `default-features = false` to omit the CLI's `release-check` network client; search, index,
export, and recovery do not need it.

## Documentation

- [Repository README](https://github.com/ahundt/ai-session-search#readme) for installation,
  commands, and user-facing documentation.
- [Library guide](https://github.com/ahundt/ai-session-search/blob/main/docs/development/library-api.md)
  for the full API, feature flags, concurrency semantics, and how date ranges resolve against a
  session's indexed span.
- [Architecture and migration decisions](https://github.com/ahundt/ai-session-search/tree/main/docs/migration).

Licensed under Apache-2.0. See
[LICENSE](https://github.com/ahundt/ai-session-search/blob/main/LICENSE) and
[NOTICE](https://github.com/ahundt/ai-session-search/blob/main/NOTICE).
