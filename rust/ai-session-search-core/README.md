# AI Session Search core

Search and recover local AI coding-agent session history from Claude Code, Claude Desktop local
agent, Codex, Cursor, Antigravity, Pi coding agent, Prime Agent, Google AI Studio, and Gemini CLI.
One SQLite index backs the `aise` CLI, MCP server, Python package, and Rust library in
[AI Session Search](https://github.com/ahundt/ai-session-search).

mcp-name: io.github.ahundt/ai-session-search

The crate builds one executable, `aise`:

```bash
cargo install ai-session-search --locked
```

`aise mcp serve` is the Rust MCP stdio transport, and the client entries written by
`aise integrations install` invoke that same command.

## Library use

The library API is beta and may change in a minor release; the CLI and MCP server are stable.

The supported application API is available directly from the crate root:

```rust
use ai_session_search::{MessageFilters, SearchFilters, SessionSearch};
```

`SessionSearch` owns the configuration, the SQLite connection, and the worker pool. Keep one
instance for a related unit of work, then compose the filter and publication types
re-exported beside it. Opening never reads your session files. Call
`app.index().prepare_for_read()` once to apply the configured refresh policy.
`SessionSearch::load()` and `SessionSearch::open()` preserve empty reads before the first refresh.
`SessionSearch::open_strict()` returns `ServiceError::IndexNotRefreshed` on reads until the index
contains sessions or has completed a refresh.

```rust,no_run
use ai_session_search::SessionSearch;

fn main() -> ai_session_search::anyhow::Result<()> {
    let app = SessionSearch::load()?;
    app.index().prepare_for_read()?;
    let recent = app.catalog().list_query(&ai_session_search::SessionQuery::default().with_limit(5))?;
    println!("{} sessions", recent.len());
    Ok(())
}
```
The crate root is the entry point to build against; the storage, CLI, MCP,
provider, and PyO3 modules are implementation surfaces.

Public structs and enums are `#[non_exhaustive]`: construct requests with `Default`, constructors,
and `with_*` setters, and include a wildcard when matching enums. `SessionQuery` applies configured
session limits; `SearchFilters` retains zero-means-all behavior. The library guide covers
[deprecated entry points](https://github.com/ahundt/ai-session-search/blob/main/docs/development/library-api.md#deprecated-rust-entry-points).

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
