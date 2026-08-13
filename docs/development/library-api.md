<!--
SPDX-FileCopyrightText: 2026 Andrew Hundt
SPDX-License-Identifier: Apache-2.0
-->

# Python and Rust library guide

The same typed Rust services back the `aise` CLI, the MCP server, the Rust
crate, and the Python package. This guide covers the two library surfaces. For
command-line use see the [README](../../README.md); for settings see the
[configuration guide](configuration.md).

## Python

`SessionSearch` is the lifecycle root. Query objects are immutable typed
conversions over the public Rust request types, so a request built once can be
reused without the risk that a later call mutated it.

```python
import ai_session_search as aise

search = aise.SessionSearch()
search.refresh()

scope = aise.QueryScope(
    provider="codex",
    path_prefix="/path/to/project",
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
        scope=scope,
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

`SessionQuery.dates` intersects the inclusive query period with each known indexed session span
from `created_at` through `updated_at`. An exact RFC 3339 value is a zero-width period and matches a
span containing that instant. The span can contain gaps and is not evidence of continuous process
activity. `QueryScope.dates` on messages, files, and event analytics instead compares each event's
own timestamp.

A finite session list limit bounds returned records, not necessarily SQLite rows considered; a
zero limit intentionally returns all eligible sessions. Ranked session search also scores all
eligible session/transcript bytes before retaining its top page. See `REQ010-protect-complexity-bounds`
in the maintainer requirements for current runtime, memory, I/O, and output-growth bounds.

### Concurrency and resource ownership

Long native operations release the GIL, so an indexing refresh or a large search
does not block other Python threads. Reconstruction iterators own their selected
rows without retaining the database lock, which keeps a slow consumer from
holding the index open. Publication is explicit: no library call writes a bundle
to disk as a side effect.

Detailed result classes are available from `ai_session_search.native`.

## Rust

The `ai-session-search` crate exposes provider-neutral services. Clap, MCP,
SQLite row, and PyO3 types stay out of its public contracts, so a library
consumer does not inherit the CLI's dependency graph.

```rust,no_run
use ai_session_search::models::SearchFilters;
use ai_session_search::service::SessionSearch;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = SessionSearch::load()?;
    let sessions = app.catalog().list_sessions(&SearchFilters {
        provider: None,
        path_prefix: None,
        exclude_path_prefixes: Vec::new(),
        exclude_session_ids: Vec::new(),
        since: None,
        until: None,
        limit: 20,
        warnings_only: false,
    })?;
    let status = app.index().status()?;
    println!("{} sessions; {} repairs", sessions.len(), status.repair_commands.len());
    Ok(())
}
```

The crate documents operation ordering, allocation, pagination, stale-index
semantics, and error behavior. Filesystem writes take an explicit restore or
publication plan, so no call chooses a destination on its own.

### Features

Library-only consumers that do not use the CLI release checker can omit its
network version-check dependencies:

```toml
[dependencies]
ai-session-search = { version = "1.0.0-rc.2", default-features = false }
```

The default `release-check` feature remains enabled for `cargo install`,
published Python wheels, and normal CLI builds.

### Depending on a release candidate

Name a release candidate exactly. Cargo excludes prerelease versions from an
ordinary requirement, so `version = "1"` matches no published candidate until
`1.0.0` is final. After that release, `version = "1"` is the requirement to use.

### Stable result shapes

Public result types are part of the contract and keep their shape across both
libraries. `CompactOutcome`, returned by the compaction service, reports exact
byte counts alongside binary `MiB` units, so a caller reading either field does
not have to infer the other.

## Shared semantics

Both libraries return every literal, regex, or no-text message-search match when
no operation, purpose, or call limit applies; fuzzy search requires a finite
page. Presentation settings such as per-message line windows never change which
messages matched, their ranking, the result count, pagination, or context
membership. The [generated caller contract](../README.md#generated-message-search-caller-contract)
lists the shipped defaults for every surface, and
`aise messages search --describe --describe-surface python|rust` resolves the
same contract against the active configuration.
