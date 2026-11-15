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

Call `refresh()` before the first read: opening an index does not read transcripts. These five
examples run in order, sharing one service. Replace the search text and file pattern for your
history. If no sessions match, the later session-specific blocks skip their reads.

### Find a session

```python
import ai_session_search as aise

search = aise.SessionSearch(index_refresh="auto")
search.refresh()
sessions = search.search_sessions("retry", aise.SessionQuery(limit=5))
for hit in sessions:
    print(hit.session.id, hit.session.title)
```

To browse by directory instead, use `list_sessions(SessionQuery(path_prefix=..., limit=5))`.
An omitted session limit follows `[search].default_limit`; an explicit 0 returns every session.

### Find a message

```python
scope = aise.QueryScope(session_id=sessions[0].session.id if sessions else None)
message_page = search.search_messages(
    r"(?i)you forgot|you missed",
    aise.MessageSearchRequest(scope=scope, role="user", query_mode="regex", limit=5, context=1),
)
for hit in message_page.results:
    print(hit["message_ref"], hit["timestamp"], hit["presentation"]["field_view"]["text"])
print(message_page.page["next_offset"])
```

The response is not itself iterable: iterate `results`. Context is under each hit's
`context.messages_before` and `context.messages_after`. Continue a finite page by passing its
`page.next_offset` as the next request's `offset` while `page.has_more` is true. Literal mode is
the default; fuzzy mode requires a positive finite limit.

### Read a session

```python
recent_messages = []
if sessions:
    recent_messages = search.read_session_messages(sessions[0].session.id, order="newest", limit=10)
    for message in recent_messages:
        print(message.seq, message.timestamp, message.content)
```

`newest` selects the last N records and returns them chronologically. Use `seq_from`/`seq_to`
for inclusive ranges, or `message_context(id, seq, context_before=1, context_after=3)` around one
turn. `transcript(id, lines=-20)` reads the text tail; positive lines keep the head and 0 all.

### Recover a file

```python
from pathlib import Path

edited_files = search.search_files("*.py")
if edited_files:
    file_path = edited_files[0].file_path
    history = search.file_history(file_path)
    source = aise.FileQuery(session_id=history[0].session_id)
    recovered = search.reconstruct_file(file_path, version=history[0].version, request=source)
    restored_path = recovered.restore(output_dir=Path("recovered"))
    print(restored_path)
```

Choose the version you need from `history`. Reconstruction returns content in memory;
`restore()` explicitly writes a collision-safe copy. Selecting a source session disambiguates a
file edited in several sessions. `publish_file_versions(file, destination, request=source)`
writes an immutable bundle of reconstructable versions to a new directory.

### Analyse corrections and repeated phrases

```python
corrections = search.classify_messages(skill="corrections", include_unattributed_input=True)
repeat_report = search.repeats(max_groups=10, include_unattributed_input=True)
for correction in corrections:
    print(correction.session_id, correction.message_seq, correction.category)
for group in repeat_report["groups"]:
    print(group["repeat"], group["matches"], group["examples"])
search.close()
```

Use `run_skill(SkillRunQuery(...))` for the full classification report and policy receipts.
`vocabulary(prefix="retry", limit=5)` reports term counts. `query_sql("SELECT COUNT(*) AS n FROM sessions")` returns read-only rows plus paging facts, with the configured SQL restrictions.
`resume_command(id)` returns native argv without launching a provider. `index_status()` reports
`parser_health.indexed_sessions` and `readiness.snapshot_availability`; `doctor()` adds
provider discovery and resume-tool health. `source_inventory()` lists configured transcript roots.

### Requests, results, and defaults

For historical user input across storage roles, pass `role="user"` to
`MessageSearchRequest`. This selects recorded prompts, queued submissions and linked answer
regions; omit the role to search complete content. Known parent-agent prompts sent to spawned
sessions are excluded from this selector. Other filters intersect normally.
On an existing-only generation-five index without input-provenance columns, `role="user"`
preserves stored user-role filtering. A writable upgrade enables provenance-based selection.
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

Without a delivery record, legacy Claude queue submissions remain `authorship=unknown`,
`input_origin=queued`, and `input_delivery_state=submitted`, while
`role="user"` still finds the recorded submission. When Claude Code also wrote a
`queued_command` attachment naming the sender, the queued row takes that sender (`human` for a
person), and a `task-notification` attachment makes it a harness notice.

Recurrence and correction analytics read rows with structural evidence of a human author,
queued rows included. Queued rows with no recorded sender are left out and counted in
`MessageClassificationReport.unattributed_input_count`;
`MessageClassificationQuery.include_unattributed_input` reads them. Queued Claude submissions
appear once when delivered through a queued-command attachment or directly after a dequeue,
including after an earlier entry was cancelled or cleared. Independent same-text submissions stay
distinct; text equality alone does not establish that two submissions are the same event.

Recurrence analytics join attributable fragments within a message before matching; their
coordinates and phrase grouping describe that projection. Content search with `role="user"`
matches contiguous original regions and reports message coordinates without joining across
generated gaps.

`SessionSearch` owns the configuration, the database handle, and the worker
pool. Query objects are immutable mirrors of the Rust request types, so one you
build is safe to reuse.


Opening a service does not read transcript files. Call `refresh()` or `prepare_for_read()`
before a first query. To require an existing index, pass
`SessionSearch(db_path, index_refresh="existing-only")`: a missing path raises `NotFoundError`
without creating its directory, and an unrefreshed empty index raises `IndexNotRefreshedError`.
`index_refresh="auto"` and `"before-query"` use the corresponding CLI policy when you call
`prepare_for_read()`. Omitting the argument preserves published empty-index reads, including when
configuration selects existing-only; that policy still refuses missing indexes and opens read-only.

Path arguments also accept `pathlib.Path` and other `os.PathLike` values. `DateRange`
accepts `datetime.date` and `datetime.datetime` as well as text; it sends their ISO representation
to the shared Rust date parser. Naive datetimes and timestamp text without an offset or zone are
read in UTC.

File queries default to all matching files and versions (`limit=0`), as on the CLI and in Rust.
Set a positive limit and offset to page a large edit history.

`SessionQuery` and `FileQuery` accept either flat `provider`, `path_prefix`, and `dates`
filters or `scope=QueryScope(...)`. File queries also accept a flat `session_id`.
`MessageSearchRequest` accepts flat `provider`, `session_id`, `workspace_path_prefix`, and
`dates` filters or a `QueryScope`. `workspace_path_prefix=None` (the default) adds no path
constraint; a string or `os.PathLike[str]` prefix matches workspace component boundaries only,
never transcript locations. Other types raise `TypeError`. The released
`QueryScope.path_prefix` adapter has the same workspace-only meaning in message search.
Use `MessageScope` for multiple providers and independent transcript-path filters.
Passing both a scope and flat filters raises `InvalidArgumentError`. Session lists do not accept
`scope.session_id`; read a resolved session directly instead.
`SessionQuery(limit=N, offset=M)` pages both lists and ranked search in their existing order.
`MessageSearchRequest(query_mode="regex")` carries its search mode; the function keyword also
works, and conflicting values raise `InvalidArgumentError`.

`diagnostics()` is deprecated; use `doctor()` for the full health report.

`search_sessions("")` is deprecated and emits DeprecationWarning naming `list_sessions()`;
the historical ranked result remains available during the deprecation period. Use a nonempty
query to rank sessions, or `list_sessions(request)` for metadata-only retrieval.

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
eligible session/transcript bytes before retaining its top page. The
[search-cost reference](../../skills/ai-session-search/references/search-costs-and-receipts.md)
states scan, replay, retained-memory, and output bounds for session and message retrieval,
file recovery, and refresh.

`classify_messages(skill="corrections", include_unattributed_input=True)` returns match rows
directly. Pass `request=MessageClassificationQuery(...)` for filters and bounds; `run_skill()`
continues to return the full report with policy receipts and unattributed-input counts.

`repeats()` returns the CLI report dictionary (`groups` with example message references,
`unattributed_input_count`, and `unattributed_input_included`). Its bounds resolve from
`[analytics]`, and `include_unattributed_input=True` includes queued input of unknown authorship.
`max_candidates=0` (default) examines all input messages; positive integers cap candidate
messages, not output groups. `max_groups=None` (default) uses `analytics.repeat_max_groups`
(50 without configuration); 0 returns all groups and positive integers cap returned groups.
Both counts accept 0 through `2**63-1`; invalid values raise `InvalidArgumentError` and wrong
types raise `TypeError`.
`vocabulary()` returns term/count dictionaries using `[analytics].vocab_limit`.
`query_sql()` returns `columns`, `rows`, `next_offset`, and `truncated_cell_char_limit`, through
`SqlService` with the same read-only authorizer and configured bounds as `aise db query`.
SQL and index-wide vocabulary refuse `allowed-roots` access. `resume_command(id)` returns
`session_id`, `argv`, `cwd`, and `unavailable_reason` without launching a provider.
`transcript(id, lines=N)` returns the Rust transcript window including completeness metadata;
positive N keeps the head, negative N the tail, and 0 every line.

Fuzzy message search allows gaps: a shorter query may skip characters in the text, but every
query character must occur in order.
It is not an edit-distance search.
It requires at least 3 Unicode scalar values and a positive finite limit. Typed error messages name Python methods for lookup and
index preparation, while preserving error causes.

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
without holding the database lock, so a slow consumer does not block database work.
`export_sessions()`, `publish_file_versions()`, and `AnalysisPublicationPlan.publish()` write
to explicit destinations. Render and reconstruction-iterator calls do not publish files.

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

The `ai-session-search` application services use provider-neutral requests and results.
The adapters convert them for the CLI, MCP, and Python; use those services for new library code.
`load()`, `open()`, and `open_with_access_inputs()` retain published empty-index reads. Use
`open_strict()` or `open_strict_with_access_inputs()` to require a prepared index. `messages().context()`
keeps exact-ID lookup and empty missing results; `context_resolved()` adds prefix lookup and lookup errors.

```rust,no_run
use ai_session_search::{SearchFilters, SessionSearch};

fn main() -> ai_session_search::anyhow::Result<()> {
    let app = SessionSearch::open_strict(ai_session_search::Config::load()?)?;
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

Everything below is imported from the crate root. `SearchFilters::default()` and
`MessageClassificationQuery` retain zero-means-all limits. Use `SessionQuery` with
`CatalogService::list_query` or `search_query` to resolve an omitted limit from
`[search].default_limit`, like the CLI; see [configured Rust session limits](#configured-rust-session-limits).
Set a finite limit when the index may be large.

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

### Deprecated Rust entry points

Use the service handles above for new code. The [1.1.0 deprecation table](../../CHANGELOG.md#deprecated)
lists retained low-level helpers, command-printing functions, compatibility search methods,
and selector aliases with their replacements. `Db::purge_injected_messages` now reclassifies
harness notices through `classify_injected_messages`; it preserves stored history.

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

### Configured Rust session limits

`SessionQuery::default()` uses `[search].default_limit` when passed to
`app.catalog().list_query(&request)` or `search_query("text", &request)`.
Use `SessionQuery::default().with_limit(0)` for every match, or a positive limit for a finite page.
`SessionQuery::from_filters(filters)` preserves `filters.limit`; add `.with_configured_limit()`
to use its structural filters with the configured default. The original `SearchFilters` methods
retain their zero-means-all behavior, including direct assignments to `limit`.
