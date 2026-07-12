# Capability and semantic-duplication matrix

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
| Message search/context | `search_messages`, `search_messages_with_context`, `get_messages` | `Db.search_messages`, `message_context`, typed `MessageFilters` | Rust canonical; port asymmetric context and every Python message-type semantic before deleting scans |
| Date parsing/filtering | `parse_date_input`, `_passes_date_filter`, `FilterSpec` builders | `dates` module plus shared CLI/MCP bounds | Rust canonical parser; Python requests convert to typed bounds; cross-language property corpus |
| File search/history | `search`, `get_versions`, `get_file_edits` | `Db.file_search`, `file_edits_for_query` | Rust canonical; compare versions, edit counts, session/path filters, Unicode and missing bases |
| File reconstruction | `reconstruct_from_edits`, `extract_final`, `extract_all` | `files::reconstruct`, restore-target safety | Rust canonical; replay every Write/Edit/MultiEdit fixture and collision/traversal cases |
| Corrections | `find_corrections` plus configurable patterns | `Db.find_corrections`, analytics configuration | Rust canonical; compare categories, match text, session/path/date scope, false-positive corpus |
| Planning/slash usage | `analyze_planning_usage`, `get_planning_usage` | `Db.planning_usage`, configurable command filters | Rust canonical; preserve invocation detail/args and user-only semantics |
| Tool calls | JSONL block scans and tool result attachment | typed message kind/tool/call-id/argument-pointer columns | Rust canonical; differential call/result association and malformed-event tolerance |
| Statistics | `get_statistics`, multi-source aggregation | counts, provider counts, parser/index status | Rust canonical primitives; typed aggregate service preserves useful Python fields |
| Export | Python markdown/bulk export | Rust transcript render/export CLI | One Rust `ExportService`; compare bytes or normalized structured sections for all bounded modes |
| Clipboard/session analysis/timeline | Python-only convenience methods | partial Rust transcript/time-profile primitives | Port useful operations as typed service requests; postpone clipboard platform integration if unmeasured |
| Configuration | Python JSON config and discovery cache | Rust TOML config, platform paths, typed defaults | One Rust config model; explicit importer maps old JSON once, reports differences, never reads two live defaults |
| Provider: Claude | Python recovery/scanning | Rust indexed provider | Rust canonical |
| Providers: Codex/Cursor/Antigravity/Pi/Claude Desktop | absent or partial Python | Rust providers | Keep Rust implementations and expose through all adapters |
| Providers: AI Studio/Gemini CLI | Python source adapters | absent in Rust | Port parsers/discovery to Rust with golden fixtures before Python backend removal |
| Public Python API | `AISession`, `connect`, models, filters, formatters | absent | Preserve user-facing intent through PyO3 facade; major version may simplify names but must remain typed and documented |
| Composable Python filters | `Filter`, `SearchFilter`, `MessageFilter`, `FilterSpec` | typed Rust query structs | Convert immutable Python builders to Rust request types; do not pass Python predicates into indexed queries |
| Analysis/codebook/graph/taxonomy | Python analysis package | only corrections/planning/vocab/repeats primitives | Keep as optional outward layer initially; port measured hot paths to `aise-analysis` without core dependency reversal |
| Index refresh/locking/schema | no persistent index | Rust `Db`/`indexer` | Rust-only canonical lifecycle; Python never coordinates a second writer |
| CLI formatting/help | large Typer implementation | clap CLI formatting | Native `aise` becomes default; Python CLI remains differential oracle until native parity, then delete duplicate handlers |
| MCP | absent in legacy aise | Rust MCP server | Rust-only adapter over shared services; rename to `aise-mcp` after parity and config migration tests |

## Required service boundaries

| Service | Typed responsibilities | Must not expose |
|---|---|---|
| `CatalogService` | list/search/resolve session metadata, stats, parser health | SQLite rows, clap or PyO3 types |
| `MessageService` | message search/context, corrections, planning, tool evidence, timelines | raw SQL, unbounded transcripts |
| `FileService` | file search/history/reconstruction/extraction plans | direct arbitrary writes |
| `ExportService` | bounded structured export and render plans | terminal globals |
| `SourceService` | provider discovery/probe/effective configuration | developer paths |
| `MaintenanceService` | status, refresh, reindex, backup/migrate/compact | silent stale-success policy |
| `AnalysisService` | optional codebook/graph/taxonomy operations | inward dependency from core |

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
