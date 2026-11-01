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

`SessionSearch` owns configuration, the SQLite connection, and service lifetimes. Keep one
instance for a related unit of work, then compose the immutable filter and publication types
re-exported beside it. Existing module paths remain supported; storage, CLI, MCP, provider, and
PyO3 modules are implementation surfaces rather than the recommended entry point.

Library-only consumers may set `default-features = false` to omit the CLI release-check network
client. The search, index, export, and recovery APIs do not require the `release-check` feature.

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
