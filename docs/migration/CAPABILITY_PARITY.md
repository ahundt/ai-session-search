# Capability and semantic-duplication matrix

Related plans: [major migration](AI_SESSION_SEARCH_MAJOR_MIGRATION.md) and
[Rust/Python API architecture](RUST_PYTHON_API_ARCHITECTURE.md).

## Decision rule

Rust indexed services become canonical for discovery, parsing, querying, recovery,
and maintenance. Python remains the public compatibility and analysis surface until
each unique capability has differential coverage through PyO3. No CLI or MCP handler
may own query, filtering, lifecycle, or migration policy.

Sessiongrep is the behavioral baseline for overlapping functionality. Its typed,
composable CLI parameters, exact provider/session/path/time/message/tool scopes,
bounded pagination and transcript controls, query diagnostics, indexed execution,
and MCP request/result schemas are generally more capable and efficient than aise's
scan-oriented equivalents. Preserve sessiongrep simplifications when they reduce
surface area without reducing outcomes. Do not reproduce aise aliases, format
branches, Python predicates, or whole-corpus scans merely to claim textual parity.

An aise behavior is ported only when it supplies useful functionality absent from
sessiongrep, including AI Studio/Gemini providers, public Python API intent, selected
analysis/codebook/graph/taxonomy workflows, or a demonstrated export/context use
case. When both implementations exist, compare real outcomes and choose the
sessiongrep contract unless the aise behavior is measurably more correct or useful.

## Superset acceptance policy

- The final capability set is the union of useful outcomes, not the union of every
  historical command, alias, flag, and implementation detail.
- Sessiongrep CLI and MCP names, types, mutual exclusions, defaults, structured
  errors, pagination, and safety bounds remain canonical across native and Python
  adapters.
- Aise-only features enter through the same typed request/result services; they do
  not create Python-only query semantics or a second MCP operation.
- Simplification is accepted when one general operation composes to replace several
  special cases and differential task tests prove no useful outcome is lost.
- Performance parity means indexed/incremental asymptotics are retained. A Python
  facade may not fall back to scanning all session files after the Rust index exists.
- CLI/MCP parity means equivalent capability and semantics, not identical transport
  syntax. MCP remains structured and bounded rather than mirroring terminal output.

## Overlap and disposition

| Capability | Python implementation | Rust implementation | Decision and proof gate |
|---|---|---|---|
| Session discovery/listing | `SessionRecoveryEngine.get_sessions`, `MultiSourceEngine.list_sessions`, `AISession.get_sessions` | `Db.list_recent`, `Db.search`, provider discovery/config | Rust canonical; differential ordering, metadata, project/path/date/provider fixtures |
| Message search/context | `search_messages`, `search_messages_with_context`, `get_messages` | `MessageService`, `message_context`, typed `MessageFilters` | Rust canonical and exposed through PyO3 with asymmetric indexed context plus role/kind/field/tool/sequence/compaction selectors and exact/regex/fuzzy modes; cross-session sequence bounds remain available in the public Rust and Python APIs |
| Date parsing/filtering | `parse_date_input`, `_passes_date_filter`, `FilterSpec` builders | `dates` module plus shared CLI/MCP bounds and PyO3 `DateRangeQuery` | Rust canonical parser now scopes native session, message, and analysis requests; cross-language property corpus and legacy boundary differential tests remain |
| File search/history | `search`, `get_versions`, `get_file_edits` | shared `FileService` search/history/cross-reference over indexed edits | Rust canonical and exposed through PyO3 with the same provider/session/path/date scope as messages and analysis; bulk extraction selection policy remains |
| File reconstruction | `reconstruct_from_edits`, `extract_final`, `extract_all` | `FileService::reconstruct` plus RAII collision-safe restore | Rust canonical and exposed through PyO3; indexed reconstruction, traversal/empty-path rejection, concurrent no-overwrite restore, and CLI reuse are proven; audit whether bulk extraction adds a distinct safe outcome |
| Corrections | `find_corrections` plus configurable patterns | Shared `AnalysisService` and typed PyO3 correction records over `Db.find_corrections` | Rust canonical for indexed classification and provider/session/path/date scopes; legacy false-positive differential corpus remains pending |
| Planning/slash usage | `analyze_planning_usage`, `get_planning_usage` | Shared `AnalysisService` and typed PyO3 counts over `Db.planning_usage` | Rust canonical for command-token aggregation; preserve invocation detail/args and user-only semantics when porting remaining record views |
| Tool calls | JSONL block scans and tool result attachment | typed message kind/tool/call-id/argument-pointer columns | Rust canonical; differential call/result association and malformed-event tolerance |
| Statistics | `get_statistics`, multi-source aggregation | Shared indexed role statistics plus parser/index status | Rust role counts now honor canonical structural predicates and typed PyO3 pagination; do not restore recovery-directory counters, and add multi-dimension grouping only with a bounded cross-surface contract |
| Export | Python markdown/bulk export | Shared Rust `ExportService`, native CLI, and typed PyO3 document | Rust canonical for text/Markdown/JSON single-session export with byte-level compatibility tests; bulk destination policy remains pending |
| Session analysis/timeline | `SessionAnalysis`, `timeline_session` | shared bounded `CatalogService::inspect`, indexed time profile, message search/context | Rust inspection and explicit normalized message/tool events are canonical and typed through PyO3. Legacy per-assistant `tool_count`, preview dictionaries, and post-scan predicates add no outcome beyond composable Rust selectors |
| Clipboard interpretation | Claude-only `pbcopy` heredoc scan | general tool-call kind/name/argument-pointer search | Keep shell/platform interpretation postponed unless measured. Python, CLI, and MCP can locate arbitrary command arguments without hardcoding `pbcopy` or heredoc syntax in core |
| Configuration | Python JSON config and discovery cache | Rust TOML config, portable overrides, platform paths with legacy-data fallback | One Rust config model; explicit importer maps old JSON once, reports differences, and transactional cutover writes one authoritative destination |
| Provider: Claude | Python recovery/scanning | Rust indexed provider | Rust canonical |
| Providers: Codex/Cursor/Antigravity/Pi/Claude Desktop | absent or partial Python | Rust providers | Keep Rust implementations and expose through all adapters |
| Providers: AI Studio/Gemini CLI | legacy Python source adapters remain | Rust parsers/discovery/indexing implemented | Rust canonical; delete Python scanners after remaining facade differential gates |
| Public Python API | legacy `AISession`, models, filters, formatters plus typed native facade | typed PyO3 catalog/message/file/export/source/index operations | Continue typed service exposure; major version may delete scan-oriented compatibility rather than duplicate policy |
| Composable Python filters | `Filter`, `SearchFilter`, `MessageFilter`, `FilterSpec` | typed Rust query structs plus immutable PyO3 `QueryScope`, `DateRangeQuery`, and message selector value objects | Native requests share structural scope while message-specific predicates remain nested and typed. Reject legacy `SYSTEM` fallback, duplicate substring predicates, and unused hardcoded 500-character “long” classification; add explicit Rust length bounds only if outcome evidence warrants them |
| Analysis/codebook/graph/taxonomy | Python analysis package over Rust-indexed document pages | shared Rust analysis pipeline, serializable policy specs, native `aise analyze`, and typed immutable v1 bundle publication through Rust and PyO3 | Rust now owns bounded analysis, explicit relationship resolution, a complete score-ranked dashboard, deterministic JSON/Markdown rendering, checksummed manifests, and atomic no-overwrite bundle publication. CLI and PyO3 compile the same provider-neutral policy specs; the Python pipeline-state helper was deleted because it was never consulted or persisted. MCP still has no bounded analysis result and must not gain filesystem publication; add a structured read-only operation only through the same service and without changing existing tool contracts. Keep symlink taxonomy only until differential tests prove a distinct browsing outcome; reject false lineage, silent exception suppression, quadratic all-pairs similarity, and unjournaled symlink publication |
| Index refresh/locking/schema | no persistent index | Rust `Db`/`indexer` | Rust-only canonical lifecycle; Python never coordinates a second writer |
| CLI formatting/help | large Typer implementation remains only as a legacy differential/deletion oracle | Clap CLI formatting and one process-safe Rust dispatcher | Rust `aise` is canonical for Cargo and Python distributions; the wheel entry point invokes the same dispatcher through PyO3, while `mcp serve` alone retains Python-owned stdio. Delete the legacy Typer module after remaining public-facade disposition tests, not as a console fallback |
| MCP | absent in legacy aise | Rust MCP server | Complete: one Rust transport over shared services, exposed by Cargo and PyO3 as `aise mcp serve`; installer entries include the same arguments. Local cutover validated initialize, all seven advertised tools, live structured index status, and ten portable client registrations with no active sessiongrep MCP keys |

## Complete legacy Python disposition ledger

The comparison unit is a useful outcome, not a Python method name. Every legacy
provider-file read was compared to the public Rust service first. No Python scanner is
retained as a fallback after its replacement gate passes.

| Legacy Python method family | Canonical Rust composition | Disposition |
|---|---|---|
| `SessionRecoveryEngine._iter_all_jsonl`, `_scan_jsonl`, `_process_message_line`; `AiStudioSource`; `GeminiCliSource`; `ClaudeSource`; `MultiSourceEngine`; `_discover_sources` | provider adapters + `SourceService::inventory` + `IndexService::refresh/reindex` | Delete. Rust already supports eight providers, normalized tool events, incremental parsing, one writer lock, parser health, and durable archives |
| `parse_date_input`, `_passes_date_filter`, partial-date `FilterSpec` builders | `DateRange`, typed query bounds, `DateRangeQuery` | Delete Python parsing. Add the pending cross-language property corpus; do not maintain two calendars |
| `search`, `get_versions`, `extract_final`, `extract_all`, `reconstruct_from_edits`, `get_original_path` | `FileService::{search,history,cross_reference,reconstruct,restore}` | Delete after bulk-selection differential tasks. Restoration remains one collision-safe RAII primitive |
| `get_messages`, `search_messages`, `search_messages_with_context`, `timeline_session` | `MessageService::{search,context}` + typed role/kind/tool/argument/sequence selectors | Delete. Timeline is ordered message search, not a second model |
| `get_sessions`, `get_latest_session_context` | `CatalogService::{list_sessions,resolve_session,inspect}` + `MessageService::context` | Delete. “Latest context” is list-one then inspect/context composition |
| `find_corrections`, `analyze_planning_usage`, `get_statistics` | `AnalysisService::{corrections,planning,role_statistics}` + `IndexService::status` | Delete scan implementation. Keep configurable correction/planning policy in Rust |
| `cross_reference_session`, `analyze_session` | `FileService::cross_reference` + `CatalogService::inspect` | Delete narrower dictionaries and Claude-only counters |
| `export_session_markdown`, `get_session_markdown` | `ExportService::render_full` | Delete duplicate renderer. Text/Markdown/JSON bytes remain covered |
| `export_sessions_markdown`, legacy `export recent` | `CatalogService::list_sessions` followed by `ExportService::render_full` on the same `SessionSearch` | Prefer composition. Add `render_many` only if profiling proves repeated setup or snapshot drift; destination naming/atomic publication belongs outside rendering |
| `get_clipboard_content` | `MessageService::search` with tool-call kind, tool name, and RFC 6901 argument pointer | Postponed shell interpretation. Do not hardcode `pbcopy`, heredocs, or any uncommon tool in core |
| `AISession` facade, legacy models/filters/formatters/protocols | `SessionSearch` plus immutable PyO3 request objects and typed results | Delete at the major boundary after import/API task tests. Do not preserve aliases that weaken validation |
| source/config CRUD Typer commands | `Config`, `SourceService::inventory`, `migrate config`, `config init/show/path` | Default discovery needs no CRUD. Add a typed Rust config mutation transaction only if a task test shows manual TOML editing is inadequate; never revive JSON and Python discovery caches |
| analysis document scan, instruction history, vocabulary | `AnalysisService::documents`, `MessageService::search`, Rust vocabulary primitives | Keep outcomes, port orchestration/policy to Rust, and bind the same request/result types to Python |
| graph/taxonomy/organization | `AnalysisService::run` + `AnalysisPublicationPlan` + `AnalysisPolicySpec` | Rust redesign implemented for canonical IDs, explicit/ambiguous relationships, bounded vocabulary, score-ranked dashboards, and immutable bundles. Delete Python orchestration after differential output acceptance; do not port symlink mutation or all-pairs similarity without distinct evidence |

### Concrete API replacement

Before, Python constructs a provider scanner and repeats filtering in memory:

```python
with AISession(source="all") as sessions:
    hits = sessions.search_messages("timeout", message_type="user", since="7d")
    context = sessions.get_latest_session_context(project_filter="sessiongrep")
```

After, Python composes immutable requests over the Rust-owned index and lifecycle:

```python
from ai_session_search import (
    DateRangeQuery,
    MessageQuery,
    MessageSelector,
    QueryScope,
    SessionQuery,
    SessionSearch,
)

search = SessionSearch()
hits = search.search_messages(
    "timeout",
    MessageQuery(
        scope=QueryScope(dates=DateRangeQuery(since="7d")),
        selector=MessageSelector(role="user"),
    ),
)
latest = search.list_sessions(SessionQuery(path_prefix="sessiongrep", limit=1))
context = search.inspect_session(latest[0].id) if latest else None
```

Rust callers use the same services without PyO3 or subprocesses:

```rust
use ai_session_search::models::{MessageFilters, SearchFilters};
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
        limit: 50,
        warnings_only: false,
    })?;
    let messages = app.messages().search("timeout", &MessageFilters::default())?;
    let status = app.index().status()?;
    println!("{} {} {}", sessions.len(), messages.len(), status.repair_commands.len());
    Ok(())
}
```

### Implemented analysis design and remaining deletion gate

The current Python analysis layer consumes bounded Rust results, so provider parsing is
not a remaining Python capability. Configurable classification, vocabulary, explicit
name-derived relationship hints, taxonomy planning, and artifact publication now run
through the shared Rust service and immutable publication plan:

```rust,ignore
let policy = AnalysisPolicySpec {
    classification_rules,
    relationship_rules,
    phrase_vocabulary,
    max_classification_chars,
}.compile()?;
let result = app.analysis().run(&filters, page_size, &policy)?;
let plan = AnalysisPublicationPlan::new(output_dir, formats)?;
plan.preflight()?;
let receipt = plan.publish(&result)?;
```

Required changes from the Python behavior:

- Key records by canonical session ID. Titles are labels and may collide.
- Represent explicit branch/copy/version-name evidence as relationship hints. Reject
  ambiguous parents instead of selecting the first title match.
- Represent shared project/cwd as group membership, not parent-child lineage.
- Represent topical similarity as `RelatedSession`, never provenance. Use bounded
  indexed/top-k candidates; prohibit all-pairs `O(N^2 * V)` execution.
- Remove hardcoded confidence values, provider-specific marker windows, sample lengths,
  score weights, depth/fanout truncation, and silent exception suppression. Typed config
  supplies validated bounds; defaults are named and serialized in effective config.
- Separate pure `AnalysisResult` and `PublicationPlan`. Validate every destination,
  collision, link target, and output format before writing.
- Publish a versioned, checksummed JSON/Markdown bundle through a same-parent staging
  directory, sync every file and directory, and expose the complete bundle through one
  atomic no-replace directory rename. Existing destinations, including broken symlinks
  and entries created after validation, are rejected.
  The implemented v1 manifest covers immutable payloads and deliberately excludes legacy
  filenames whose schemas differ. Symlink taxonomy remains opt-in future work and must use
  a separate manifest plus an RAII rollback guard so a failed run cannot leave a half-applied
  taxonomy.
- Do not add incremental state until measurements justify it. If introduced, use an
  index generation/content fingerprint plus canonical analysis-config digest; never use
  only path/mtime/size or claim a plain `write_text` is atomic.
- Stream/keyset-page documents. Memory is `O(page + result metadata + configured
  vocabulary state)`, not `O(total user text)`. Every limit is validated nonzero and
  carried by a newtype/builder rather than a magic primitive.

Pre-mortem tests must cover duplicate titles, ambiguous parents, missing/renamed source
files, symlink cycles and collisions, output paths outside the destination, interruption
between publish phases, corrupt prior state, invalid regex/config, Unicode/non-UTF-8
paths, empty corpora, huge sessions, cancellation, and deterministic output ordering.

Dependency licenses are not required to be Apache-2.0. The project is Apache-2.0;
compatible dependency licenses retain their own terms and are validated/inventoried
through `deny.toml` and the isolated Python runtime license gate.

## Required service boundaries

| Service | Typed responsibilities | Must not expose |
|---|---|---|
| `CatalogService` | list/search/resolve/inspect session metadata | SQLite rows, clap or PyO3 types |
| `MessageService` | message search/context, corrections, planning, tool evidence, timelines | raw SQL, unbounded transcripts |
| `FileService` | file search/history/reconstruction/extraction plans | direct arbitrary writes |
| `ExportService` | bounded structured export and render plans | terminal globals |
| `SourceService` | provider discovery/probe/effective configuration | developer paths |
| `IndexService` | status, opportunistic refresh, explicit reindex | repair commands that cannot affect currently discoverable data |
| `MaintenanceService` | diagnostics, backup/migrate/compact | silent stale-success policy |
| `AnalysisService` | indexed documents, corrections, planning, role statistics; optional pipeline delegates outward | inward dependency from core to publication adapters |

## TDD migration order

1. Freeze cross-language JSON fixtures and expected typed results for overlapping
   operations before changing either implementation.
2. Introduce request/result types and services around existing Rust functions with
   no behavior change; CLI/MCP tests must remain green.
3. Add AI Studio and Gemini Rust parsers using Python-generated golden fixtures and
   malformed/partial/cancellation tests.
4. Add PyO3 conversions over service requests/results; run Python differential tests
   against legacy `AISession` on the fixed corpus.
5. Port unique analysis/export/context capabilities only after profiling proves the
   appropriate library boundary.
6. Replace CLI/MCP handlers with service calls and generate parity assertions for
   operations, parameters, defaults, errors, pagination, and output schemas.
7. Delete Python scanning/query duplication only when the new major API, native CLI,
   and MCP all pass the same correctness, lifecycle, and performance gates.
8. Port the redesigned optional analysis service with pure-result tests first, then
   filesystem failure injection and Python differential task tests; delete the Python
   pipeline only after deterministic artifact equivalence is accepted.

## Complexity and performance requirements

- Discovery and refresh are proportional to changed files/bytes after warm index,
  not the complete corpus per Python process.
- Search uses bounded indexed candidates and pages; adapters never collect an
  unbounded result merely to truncate it.
- File reconstruction is linear in selected edits/content and streams large output.
- Provider parsing is incremental where formats permit and cancellation-aware.
- One configured writer coordinates processes; readers use SQLite snapshots and
  bounded busy timeouts.
- Python native calls release the GIL around parsing, indexing, SQLite work, and
  serialization that does not touch Python objects.
- Source alias reconciliation is `O(indexed source paths + discovered files)` and runs
  once per reindex. It canonicalizes only existing paths, so missing-source archives are
  never merged by string case or guessed identity.
