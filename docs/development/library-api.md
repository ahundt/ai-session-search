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

For historical user input across storage roles, pass `role="user"` to
`MessageSearchRequest`. This selects recorded prompts, queued submissions and linked answer
regions; omit the role to search complete content. Known parent-agent prompts sent to spawned
sessions are excluded from this selector. Other filters intersect normally.
Content queries match contiguous input regions in the indexed message and report Unicode-scalar
coordinates in that message. Tool-name and tool-argument queries use their own field coordinates.
Views and context may include generated text. Results include concise source
provenance. The index retains full partition and correlation details for existing read-only SQL
inspection; ordinary search hits do not emit those internal arrays.
Range reads omit harness notices by default. Pass
`read_session_messages(id, kinds=["conversation", "harness_notice"])` to include those classes,
as `aise messages get ID --kinds conversation,harness-notice` does. Focused `message_context()`
reads include all classes. MCP range reads accept the same `kinds` list.

Structured message-search hits include `timestamp` (RFC 3339 UTC, or None when unknown),
including queryless hits and batches.

Origin and delivery evidence use `null` in JSON and `None` in Rust/Python when absent.
Legacy indexes remain searchable through an internal storage marker; `unknown` is not an
origin or delivery enum value. Both Rust enums are non-exhaustive.

Legacy Claude queue operations lack per-item sender receipts and links proving delivery. They
remain `authorship=unknown`, `input_origin=queued`, and `input_delivery_state=submitted`, while
`role="user"` still finds the recorded submission. When Claude Code also wrote a
`queued_command` attachment naming the sender, the queued row takes that sender (`human` for a
person), and a `task-notification` attachment makes it a harness notice.

Recurrence and correction analytics read rows with structural evidence of a human author,
queued rows included. Queued rows with no recorded sender are left out and counted in
`MessageClassificationReport.unattributed_input_count`;
`MessageClassificationQuery.include_unattributed_input` reads them. Matching a later message by
text alone does not prove identity, so a queued row whose exact text is also a non-queued human
row of the same session is not read: the input counts once, as the delivered prompt.

Recurrence analytics join attributable fragments within a message before matching; their
coordinates and phrase grouping describe that projection. Content search with `role="user"`
matches contiguous original regions and reports message coordinates without joining across
generated gaps.

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

Opening a service does not read transcript files. Call `refresh()` or `prepare_for_read()`
before a first query. To require an existing index, pass
`SessionSearch(db_path, index_refresh="existing-only")`: a missing path raises `NotFoundError`
without creating its directory, and an unrefreshed empty index raises `IndexNotRefreshedError`.
`index_refresh="auto"` and `"before-query"` use the corresponding CLI policy when you call
`prepare_for_read()`. Omitting the argument preserves the older empty-index read behavior unless
configuration explicitly selects existing-only.

Path arguments also accept `pathlib.Path` and other `os.PathLike` values. `DateRange`
accepts `datetime.date` and `datetime.datetime` as well as text; it sends their ISO representation
to the shared Rust date parser. Naive datetimes use the same local-zone rule as timestamp text.

`SessionQuery` and `FileQuery` accept either flat `provider`, `path_prefix`, and `dates`
filters or `scope=QueryScope(...)`. File queries also accept a flat `session_id`.
`MessageSearchRequest` accepts the same flat filters or a `QueryScope`; its path filter means
workspace path. Use `MessageScope` for multiple providers and independent transcript-path filters.
Passing both a scope and flat filters raises `InvalidArgumentError`. Session lists do not accept
`scope.session_id`; read a resolved session directly instead.
`SessionQuery(limit=N, offset=M)` pages both lists and ranked search in their existing order.
`MessageSearchRequest(query_mode="regex")` carries its search mode; the function keyword also
works, and conflicting values raise `InvalidArgumentError`.

An omitted `SessionQuery.limit` (None) resolves `[search].default_limit` on list and ranked
search calls, including when the request supplies other filters. An explicit 0 returns every
session; a positive count overrides configuration.

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

`classify_messages(skill="corrections", include_unattributed_input=True)` returns match rows
directly. Pass `request=MessageClassificationQuery(...)` for filters and bounds; `run_skill()`
continues to return the full report with policy receipts and unattributed-input counts.

`repeats()` returns the CLI report dictionary (`groups` with example message references,
`unattributed_input_count`, and `unattributed_input_included`). Its bounds resolve from
`[analytics]`, and `include_unattributed_input=True` includes queued input of unknown authorship.
`vocabulary()` returns term/count dictionaries using `[analytics].vocab_limit`.
`query_sql()` returns `columns`, `rows`, `next_offset`, and `truncated_cell_char_limit`, through
`SqlService` with the same read-only authorizer and configured bounds as `aise db query`.
SQL and index-wide vocabulary refuse `allowed-roots` access. `resume_command(id)` returns
`session_id`, `argv`, `cwd`, and `unavailable_reason` without launching a provider.
`transcript(id, lines=N)` returns the Rust transcript window including completeness metadata;
positive N keeps the head, negative N the tail, and 0 every line.

Python exposes `NotFoundError`, `AmbiguousPrefixError`, `AlreadyExistsError`,
`InvalidArgumentError`, and `IndexNotRefreshedError`. They preserve existing `RuntimeError`
handlers. Validation also preserves `ValueError`; signed version validation preserves the
legacy `OverflowError` handler. Error causes are classified from Rust types.

### Concurrency and resource ownership

Use `with SessionSearch(...) as search:` to close the connection on normal or exceptional
exit, or call `search.close()` explicitly. Repeated close calls are safe; subsequent service
operations raise RuntimeError. Detached results and independently opened iterators remain usable
and those iterators retain their own close obligations.

Long native operations release the GIL, so a refresh or a large search does not
block other Python threads. Reconstruction iterators own their selected rows
without holding the database lock, so a slow consumer cannot hold the index
open. Publication is explicit: no library call writes a bundle to disk as a side
effect.

Request and result values compare by value, have readable `repr()` output, and expose
`to_dict()` for a detached JSON-ready dictionary. Dictionaries use CLI field names, including
`ts`; existing Python attributes keep `timestamp`. Resource owners and iterators retain
identity semantics.

`ReconstructedFile.restore(output_dir=...)` writes a `.recovered` copy using only the recorded
basename under your chosen directory. With no output_dir it writes beside the recorded original
path. It preserves existing files, adding `_2`, `_3`, and so on for occupied recovery names,
and returns the actual destination path.

Result and request classes are available directly from `ai_session_search`, including
`SessionRecord`, `SearchHit`, `MessageHit`, `FileVersion`, and `IndexStatus`.
`ai_session_search.native` remains a supported import path for the same classes.

## Rust

The `ai-session-search` crate exposes provider-neutral services. MCP, SQLite
row, and PyO3 types stay out of its public contracts, so a library consumer
works with the same typed requests and responses every other surface uses.

```rust,no_run
use ai_session_search::{SearchFilters, SessionSearch};

fn main() -> ai_session_search::anyhow::Result<()> {
    let app = SessionSearch::load()?;
    // Opening never reads your session files. This applies the configured refresh policy, as
    // `aise` does before every command; with the default `auto` policy it indexes the sources
    // the first time. Without it, reads on a never-indexed database fail with
    // ServiceError::IndexNotRefreshed instead of reporting zero sessions.
    app.index().prepare_for_read()?;
    // Public types are #[non_exhaustive]: start from the default and use the with_* setters.
    // Every other field keeps its default, including session_kinds, which returns both
    // user-started sessions and the subagent runs they spawned.
    let sessions = app
        .catalog()
        .list_sessions(&SearchFilters::default().with_limit(20))?;
    println!("{} sessions", sessions.len());
    Ok(())
}
```

Rustdoc on each service covers operation ordering, allocation, pagination,
stale-index semantics, and errors. Filesystem writes take an explicit
restore or publication plan, so no call chooses a destination on its own.

Everything below is imported from the crate root. Library defaults differ from the command line
in one way: a list or classification limit you leave at 0 returns every row, where the CLI
applies `[search].default_limit` and `[analytics].corrections_limit` (50 each). Set a limit when
the index may be large.

```rust,no_run
use ai_session_search::{
    MessageClassificationQuery, MessageFilters, MessageKind, MessageOrder, MessageQuery,
    MessageSearchRequest, MessageTarget, SearchFilters, SessionSearch, SkillCapabilityInput,
    SkillCapabilityOutput, SkillRunQuery, SkillSelector, SqlQuery, FileQuery,
};

fn main() -> ai_session_search::anyhow::Result<()> {
    let app = SessionSearch::load()?;
    app.index().prepare_for_read()?;

    // The 5 most recent sessions whose working directory is under a project.
    let recent = app.catalog().list_sessions(
        &SearchFilters::default()
            .with_path_prefix("/home/me/project")
            .with_limit(5),
    )?;

    // The exact message where something was said, with one message of context each side.
    let request = MessageSearchRequest::builder(
        MessageQuery::literal("run the tests")?,
        MessageTarget::content(),
    )
    .context(ai_session_search::ContextWindow::symmetric(1))
    .build()?;
    for hit in app.messages().search(request)?.hits() {
        println!("{} #{}", hit.session_id, hit.seq);
    }

    // How a session ended: its last 10 messages, returned oldest first.
    if let Some(session) = recent.first() {
        let last = app.messages().read_session(
            &MessageFilters::default()
                .with_session_id(session.id.clone())
                .with_limit(10),
            MessageOrder::NewestFirst,
        )?;
        println!("{} messages", last.len());
    }

    // An overwritten file: version 2 of every recorded edit to retry.py.
    let version = app.files().reconstruct("retry.py", &FileQuery::default(), Some(2))?;
    println!("{} bytes", version.content.len());

    // Where the agent went wrong: user corrections, with the built-in categories.
    let report = app.analysis().run_skill(&SkillRunQuery::new(
        SkillSelector::name("corrections")?,
        SkillCapabilityInput::MessageClassification(
            MessageClassificationQuery::default()
                .with_filters(MessageFilters::default().with_limit(50)),
        ),
    ))?;
    if let SkillCapabilityOutput::MessageClassification(result) = report.output {
        println!("{} corrections", result.report.matches.len());
    }

    // Harness notices (Stop-hook feedback and similar), which search leaves out by default.
    let notices = MessageSearchRequest::builder(MessageQuery::All, MessageTarget::content())
        .kind(MessageKind::HarnessNotice)
        .build()?;
    println!("{} notices", app.messages().search(notices)?.hits().len());

    // Read-only SQL with the configured row limit.
    let rows = app.sql().query(&SqlQuery::new("select provider, count(*) as n from sessions group by provider"))?;
    println!("{:?}", rows.rows);
    Ok(())
}
```

### Errors

Every call returns `anyhow::Result`. Failures a program branches on are
`ai_session_search::ServiceError` (session not found, ambiguous prefix with its candidates, file
not found, version out of range with the valid range, destination exists, index missing, index
not refreshed, invalid argument) or `MessageSearchError`, each with a stable `code()`. Downcast
the `anyhow::Error` before converting it: `?` into `Box<dyn std::error::Error>` keeps the message
and loses the type.

```rust,no_run
use ai_session_search::{ServiceError, SessionSearch};

fn main() -> ai_session_search::anyhow::Result<()> {
    let app = SessionSearch::load()?;
    app.index().prepare_for_read()?;
    match app.catalog().resolve_session("claude:1234") {
        Ok(session) => println!("{}", session.id),
        Err(error) => match error.downcast_ref::<ServiceError>() {
            Some(ServiceError::SessionPrefixAmbiguous { candidates, .. }) => {
                println!("which one? {candidates:?}")
            }
            Some(other) => println!("{}: {other}", other.code()),
            None => return Err(error),
        },
    }
    Ok(())
}
```

`SessionSearch` is `Send` but not `Sync`: it owns one SQLite connection. Move it to a thread,
open one per thread, or share one behind a `Mutex`.

### Features

To drop the network version-check dependency, disable the default feature:

```toml
[dependencies]
ai-session-search = { version = "1.1.0", default-features = false }
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
