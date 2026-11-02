<!--
SPDX-FileCopyrightText: 2026 Andrew Hundt
SPDX-License-Identifier: Apache-2.0
-->

# Python and Rust library guide

The same typed Rust services back the `aise` CLI, the MCP server, the Rust
crate, and the Python package, so either library answers a query the way the CLI
does. For command-line use see the [README](../../README.md); for settings see
the [configuration guide](configuration.md).

## Python

`SessionSearch` owns the configuration, the database handle, and the worker
pool. Query objects are immutable mirrors of the Rust request types, so one you
build is safe to reuse.

```python
import ai_session_search as aise

search = aise.SessionSearch()
search.refresh()

# Session and file queries share QueryScope; message search takes MessageScope, whose
# workspace and provider fields are plural and message-specific.
scope = aise.QueryScope(
    provider="codex",
    path_prefix="/path/to/project",
    dates=aise.DateRange(when="7d"),
)
message_scope = aise.MessageScope(
    providers=["codex"],
    workspace_path_prefix="/path/to/project",
    dates=aise.DateRange(when="7d"),
)

sessions = search.list_sessions(
    aise.SessionQuery(
        provider="codex",
        dates=aise.DateRange(when="2026-01"),
        limit=20,
    ),
)
messages = search.search_messages(
    "authentication",
    aise.MessageSearchRequest(
        scope=message_scope,
        role="user",
        include_compaction=False,
        limit=50,
    ),
)
files = search.search_files(
    "*.py",
    aise.FileQuery(scope=scope, min_edits=3, limit=50),
)
history_page = search.file_history(
    "src/app.py",
    aise.FileQuery(scope=scope, limit=50, offset=0),
)

if sessions:
    evidence = search.inspect_session(
        sessions[0].id,
        include_time_profile=True,
    )
    markdown = search.export_session(sessions[0].id, "markdown")

status = search.index_status()
```

`SessionQuery.dates` intersects the inclusive query period with each indexed session span,
`created_at` through `updated_at`. An exact RFC 3339 value is a zero-width period and matches a
span containing that instant. A span can contain gaps, so a session matching a one-week period
may have been touched once in it. File queries, event analytics, and `MessageScope.dates`
compare each event's own timestamp.

A finite session list limit bounds the records you get back; it does not bound the SQLite rows
considered, and a zero limit returns all eligible sessions. Ranked session search also scores all
eligible session/transcript bytes before retaining its top page. Runtime, memory, I/O, and
output-growth bounds are recorded under `REQ010-protect-complexity-bounds` in the
[maintainer requirements](maintainer-requirements-and-design-decisions.md).

### Concurrency and resource ownership

Long native operations release the GIL, so a refresh or a large search does not
block other Python threads. Reconstruction iterators own their selected rows
without holding the database lock, so a slow consumer cannot hold the index
open. Publication is explicit: no library call writes a bundle to disk as a side
effect.

Detailed result classes come from `ai_session_search.native`.

## Rust

The `ai-session-search` crate exposes provider-neutral services. MCP, SQLite
row, and PyO3 types stay out of its public contracts, so a library consumer
works with the same typed requests and responses every other surface uses.

```rust,no_run
use ai_session_search::models::SearchFilters;
use ai_session_search::service::SessionSearch;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = SessionSearch::load()?;
    // Every field but the limit keeps its default, including session_kinds, which returns both
    // user-started sessions and the subagent runs they spawned.
    let sessions = app.catalog().list_sessions(&SearchFilters {
        limit: 20,
        ..SearchFilters::default()
    })?;
    let status = app.index().status()?;
    println!("{} sessions; {} repairs", sessions.len(), status.repair_commands.len());
    Ok(())
}
```

Rustdoc on each service covers operation ordering, allocation, pagination,
stale-index semantics, and errors. Filesystem writes take an explicit
restore or publication plan, so no call chooses a destination on its own.

### Features

To drop the network version-check dependency, disable the default feature:

```toml
[dependencies]
ai-session-search = { version = "1.0.0", default-features = false }
```

`release-check` stays enabled for `cargo install`, published Python wheels, and
normal CLI builds.

### Depending on a release candidate

Cargo excludes prereleases from an ordinary requirement, so `version = "1"`
resolves 1.0.0 or a later stable and never a candidate. Name a candidate
exactly: `version = "1.1.0-rc.1"`.

### Result shapes

Both libraries return the same shape for a given public result type.
`CompactOutcome` from the compaction service reports `before_bytes`,
`after_bytes`, and `reclaimed_bytes` as exact byte counts (Rust exposes
`reclaimed_bytes()` as a method); any human-readable unit is the caller's rendering.

## Shared semantics

Both libraries return every literal, regex, or no-text message-search match when
no operation, purpose, or call limit applies; fuzzy search requires a finite
page. Presentation settings such as per-message line windows never change which
messages matched, their ranking, the result count, pagination, or context
membership. The [generated caller contract](../README.md#generated-message-search-caller-contract)
lists the shipped defaults, and
`aise messages search --describe --describe-surface python|rust` resolves them
against your active configuration.
