<!--
SPDX-FileCopyrightText: 2026 Andrew Hundt
SPDX-License-Identifier: Apache-2.0
-->

# Changelog

Notable changes to AI Session Search, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [1.1.0] - 2026-11-11

1.1.0 keeps indexed history when an agent rewrites or removes its transcript, finds recorded human
input across message roles, and adds typed library routes for SQL, repeats, word counts, transcript
reads, and resume commands. See the [library guide](docs/development/library-api.md) and
[full diff](https://github.com/ahundt/ai-session-search/compare/v1.0.0...v1.1.0).

### Upgrade notes

1. Stop running aise processes, including MCP servers in harnesses, before upgrading. First open
   promotes the index to generation 6. A newly started 1.0.0 refuses that index, but an already
   running 1.0.0 process can still delete archived history. A healthy generation-5 index upgrades
   through metadata without a transcript reparse. To downgrade, stop processes, move the index
   and its WAL files aside, and use 1.0.0 `aise reindex --full` to rebuild from source transcripts.
   Keep the moved files to retain archived history. Follow the
   [downgrade steps](docs/development/installation.md#update-and-uninstall).
2. Rust public structs and enums are now `#[non_exhaustive]`. Replace struct literals with a
   constructor or `T::default()` followed by field assignment or `with_*` setters; add wildcard
   arms to enum matches. For example, use
   `SearchFilters::default().with_path_prefix(dir)` instead of a `SearchFilters { ... }` literal.
   `util::apply_user_role_authorship` and `util::UserRoleAuthorshipEvidence` remain callable
   deprecated paths; use `RawMessage::with_authorship` before normalization.
3. Rust keeps empty-index reads through `load()`, `open()`, and `open_with_access_inputs()`.
   Use `open_strict()` or `open_strict_with_access_inputs()` to receive
   `ServiceError::IndexNotRefreshed` until `app.index().prepare_for_read()` applies the refresh
   policy. `MessageService::context` keeps exact-ID lookup and empty missing results;
   `context_resolved` adds prefix lookup and `ServiceError::SessionNotFound`.
4. Python file queries return every matching file or version by default, instead of stopping at
   50. Supply a positive limit for a page. Omitted session-list and ranked-search limits now
   follow `[search].default_limit`; explicit zero still returns every session.
   Omitted `query_mode` on Python `search_messages()` and `search_message_batches()` now follows
   the request mode instead of overriding it with `literal`; pass `query_mode="literal"` to
   preserve that override.
5. Scripts can see fewer duplicate search results and file edits: Claude Code saved API request
   copies and repeated record identities no longer add extra turns or edits. Queued submissions
   delivered as prompts count as those prompts; independent same-text submissions stay distinct.
   Copied parent history in Codex forks stays searchable but does not count as new human input.
6. Date input keeps the published UTC default and fixed-duration shorthand: `m` or `M` means
   30 days, and `y` or `Y` means 365 days. Use `min` for minutes, `mo` or `P1M` for calendar
   months, and `P1Y` for calendar years. A trailing zone such as `[America/New_York]` selects a
   local calendar explicitly. JSON, MCP, and Python timestamps use UTC `Z`; human displays use
   local time with an offset.

### Added

1. Search Claude Code attached files, edited snippets, memory and plan text, and invoked skill
   contents. Find injected instructions, session context, hook feedback, and read-truncation
   notices with `--kinds harness-notice`. Doctor and index-status reports name unrecognized record
   and attachment types; malformed attachments and skipped binary payloads produce parse warnings.
2. `--include-unattributed-input` on corrections and repeats includes queued messages with no
   recorded sender. Reports count excluded input as `unattributed_input_count` and record the
   opt-in as `unattributed_input_included`; `aise repeats --format json --report` returns the
   complete report object. MCP `run_skill_capability` and both libraries expose the same opt-in.
3. Rust adds `ServiceError` codes for lookup, ambiguity, invalid arguments, missing or unrefreshed
   indexes, file versions, and destination collisions. `SessionSearch::sql()` runs scoped read-only
   SQL; `AnalysisService::repeats` and `vocabulary` return typed reports; `CatalogService` supplies
   configured ranking, transcript windows, and resume argv. `SessionQuery` applies configured
   session limits through `list_query` and `search_query`. Requests have constructors or `with_*`
   setters, and service request/result types are re-exported from the crate root.
4. Python adds `classify_messages()`, `repeats()`, `vocabulary()`, `query_sql()`, `resume_command()`,
   `transcript()`, and `doctor()`. Named compatible exceptions distinguish lookup and argument
   failures. Request/result values support equality, readable representations, and `to_dict()`;
   result classes are exported from `ai_session_search`. Paths accept `os.PathLike`, dates accept
   Python dates and datetimes, session queries accept offsets and shared scopes, and services can
   be closed explicitly or used as context managers. `prepare_for_read()` applies explicit index
   refresh policies; existing-only reads refuse a missing index without creating it.
5. `aise show --format json|jsonl|csv|plain` returns transcript text with line counts and completeness.
   Focused reads accept independent `--context-before` and `--context-after`; MCP uses
   `context_before` and `context_after`. Session range reads accept `kinds` in CLI, MCP, and Python.
   Structured message-search hits include nullable timestamps.
6. Integrations support `--harness-home`, `AI_SESSION_SEARCH_HARNESS_HOME`, and per-surface
   `[integrations.paths]` destinations. `--no-index-preparation` installs integrations without
   starting background indexing. Codex receives a marketplace-backed MCP plugin listing;
   Pi and Prime Agent receive native MCP registrations and instructions.
7. `aise config check` validates settings without opening the index and rejects unknown keys.
   Doctor and index status report ignored ordinary keys with spelling suggestions. Duration input
   also accepts ISO 8601, compound, and spelled-out forms; date ranges accept EDTF open intervals
   and trailing IANA zones such as `2026-01-15[Europe/Berlin]`.

### Changed

1. Corrections, repeats, and analysis include queued input when the source identifies a human
   sender. The shipped correction categories remain a small starter set of fixed phrasings:
   inspect recorded input with context to find corrections worded for a particular situation.
   Add categories for recurring wording using the [package creation steps](skills/ai-session-search/SKILL.md#create-your-own-classification-package).
2. `aise resume ID` prints the native command and exits successfully when stdin or stdout is
   redirected. `--dry-run` requests that behavior in a terminal.
3. Empty message-search hints point to harness notices, tool arguments, or recoverable files as
   other places to look. A hint means the current query found no matches; it does not promise an
   answer in those alternatives.
4. `aise vocab` remains callable as a hidden compatibility command. Use Python `vocabulary()` or
   Rust `AnalysisService::vocabulary` for word counts in applications.
5. Ordinary unknown configuration keys are reported and ignored; access-restricting and search
   policy tables remain strict. Upgrades preserve existing configuration without adding active
   keys. Migration receipts, skill manifests, and native ownership receipts preserve additive
   metadata; unsupported receipt versions fail before recovery writes.
6. MCP keeps `purpose` and `purpose_version` in its schema when no search presets are configured;
   configured presets supply the accepted purpose names.
   Server instructions name `aise files extract`, `aise export`, `aise analyze`, and `aise reindex`
   for operations performed through the CLI or libraries.

### Deprecated

1. `aise planning` → `aise slash-commands`.
2. `aise repeats --limit N` → `--max-candidates N` to bound examined messages; `--max-groups N`
   bounds returned groups. `aise repeats --regex` → `--query-mode regex`.
3. `aise messages timeline --regex PATTERN` → `--grep PATTERN --query-mode regex`.
4. `aise files cross-ref --pattern PATTERN` → `aise files cross-ref PATTERN`.
5. `aise show --summary`, `--summary-items`, and `--preview-chars` → `aise messages evidence ID`
   with the corresponding evidence options.
6. `aise messages search --include raw_provider_metadata` → `--include raw-metadata`.
   Classification `--limit 0` → `--all-results`.
7. MCP `get_session.message_seq` → `seq`; `response_format=concise|detailed` → `detail=compact|full`.
   Compatible aliases remain accepted; conflicting simultaneous values fail.
8. Python `search_sessions("")` → `list_sessions(request)`; `diagnostics()` → `doctor()`.
   Both emit `DeprecationWarning`.
9. Rust compatibility items remain callable with compiler warnings. This table follows the
   public deprecation attributes, grouping duplicate aliases and fields on deprecated types:

   | Rust items | Replacement |
   | --- | --- |
   | `EffectiveAccessScope::allows_workspace_path` | CatalogService::list_sessions for authoritative scope-enforced reads |
   | `EffectiveAccessScope::is_unrestricted` | match EffectiveAccessScope for policy inspection; use CatalogService::list_sessions for scope-enforced reads |
   | `IndexService::reindex` | IndexService::reindex_report |
   | `MessageSearchBatch::into_parts` | MessageService::search, or borrow batch results, context_windows, and included |
   | `MessageSearchResponse::into_hits` | MessageService::search and MessageSearchResponse::results |
   | `MessageSearchResponse::into_rows` | MessageService::search and borrow results and context_windows |
   | `MessageService::search_legacy`, `MessageService::search_with_explain` | MessageService::search |
   | `SkillNameSelector` | SkillSelector::name |
   | `SkillPathSelector` | SkillSelector::path |
   | `dates::parse_bound`, `dates::parse_span` | DateRange::resolve and pass the result to CatalogService or MessageService |
   | `db::Db::clear_all` | no service deletes the archive; IndexService::reindex_report refreshes while preserving history |
   | `db::Db::counts_by_provider` | CatalogService::list_sessions and group its typed provider rows |
   | `db::Db::explain_message_search` | MessageService::plan or MessageService::search with a receipt level |
   | `db::Db::file_edit_count` | FileService::search for typed file summaries |
   | `db::Db::file_edits_for` | FileService::history or FileService::reconstruct for typed recovery |
   | `db::Db::file_edits_for_session_id` | FileService::history with a session-scoped FileQuery |
   | `db::Db::is_file_current` | IndexService::prepare_for_read for coordinated freshness checks |
   | `db::Db::messages_fts_count` | MaintenanceService::diagnostics for index health |
   | `db::Db::purge_injected_messages` | reclassifies instead of deleting; call `classify_injected_messages` |
   | `db::Db::replace_session` | IndexService::reindex_report for coordinated parser upgrades |
   | `files::run` | FileService::search, history, cross_reference, or reconstruct |
   | `indexer::auto_reindex` | IndexService::prepare_for_read or IndexService::refresh |
   | `mcp_schema_budget::LEDGER_STAGES` | run_cli_from for the schema-budget stage ledger; mcp_schema_budget::evaluate_all evaluates catalogue limits |
   | `mcp_schema_budget::ceiling_for`, `mcp_schema_budget::codex_strips_descriptions` | mcp_schema_budget::evaluate_all; run_cli_from exposes the schema-budget command |
   | `messages::run` | MessageService::search or read_session, or CatalogService::inspect |
   | `skills::SkillCategoryRow`, `skills::SkillDiagnostic`, `skills::SkillSummary` | run_cli_from for skill metadata or AnalysisService::run_skill for capability execution |
   | `skills::run` | run_cli_from for skill management or AnalysisService::run_skill for capability execution |
   | `sql_query::DbQueryArgs::resolve` | SqlService::query with SqlQuery, which resolves configured defaults |
   | `sql_query::run` | SessionSearch::sql or SqlService::new(&config) and render the QueryResult |
   | `tail::tail_parse` | IndexService::reindex_report for guarded incremental parsing |
   | `trigram::trigram_prefilter_all` | MessageService::search with MessageQuery::regex for verified results |
   | `trigram_index::base_max_id`, `trigram_index::build`, `trigram_index::build_in_current_transaction`, `trigram_index::candidates`, `trigram_index::ensure_schema`, `trigram_index::schema_is_compatible` | IndexService::reindex_report for index preparation and MessageService::search for verified queries |
   | `util::LegacyRawMessage`, `util::to_messages`, `util::to_messages_with_tools` | RawMessage constructors; IndexService::reindex_report owns provider normalization |
   | `util::UserRoleAuthorshipEvidence`, `util::apply_user_role_authorship` | RawMessage::with_authorship before normalization |
   | `util::to_messages_with_tools_in_scope` | RawMessage constructors; IndexService::reindex_report owns scoped provider normalization |

### Fixed

1. Indexed history survives transcripts that shrink, are rewritten, change session id, or disappear.
   Parser upgrades reconcile matching rows while preserving archived messages and file edits.
   Codex compressed `.jsonl.zst` rollouts retain their identity; decompression failures preserve
   an existing index snapshot and remain retryable.
2. Literal and regex search find case pairs SQLite's tokenizer does not fold, including Cherokee,
   Georgian Mtavruli, Osage, and Adlam. A trailing `i` also matches dotted capital `İ`.
3. Equal session/sequence or timestamp keys have a stable tie-breaker for search, corrections,
   repeats, context, and file replay. Appended messages continue after the highest stored sequence.
4. Full reindexing reclassifies stored harness notices instead of deleting them. Claude tool-call
   arguments stay in tool rows rather than also appearing as assistant prose. Interrupt markers
   and generated wrappers stay outside human-input analytics.
5. Recorded queued submissions and linked interactive answers remain searchable across providers;
   Gemini CLI context patches preserve original input. Antigravity checkpoints are compaction
   records, and system/error messages are harness notices.
6. MCP installation uses the locations and formats read by OpenCode, Zed on macOS, Kilo Code,
   OpenClaw, Claude Code, and Cursor. JSONC/JSON5 comments and unrelated settings survive edits.
   Post-upgrade refresh relocates existing owned entries and preserves their executable paths.
   Gemini and Codex share a skill discovery link when their resolved paths match.
7. MCP registrations retain explicit application and harness paths across client restarts;
   reinstall preserves unrelated environment and fields. `aise config init` and package-update
   refresh retain selected database and cache paths. `aise migrate config` accepts its required
   `--cache-dir` and writes it to the converted configuration.
8. File recovery errors identify the first version that cannot be reconstructed and the missing
   information; earlier recoverable versions remain extractable. Codex forks omit copied parent
   file edits when the parent transcript is available, and retain them when it is absent.

### Performance

These subsystem measurements use generated benchmark fixtures and unoptimized debug builds;
they describe the named workloads, not a release-build latency guarantee.

1. On a generated fixture of 128 sessions and 4,096 messages, 16 repeated MCP literal searches
   took a median 129 ms instead of 139 ms across 25 repetitions after catalogue reuse.
2. With that generated fixture plus a synthetic 1,537-message Claude session, four incremental
   refreshes of a growing transcript took a median 163 ms instead of 1,134 ms across 15
   repetitions. Unchanged sidecar metadata permits incremental transcript reads.

### Security

1. Bundled SQLite moves to 3.54.1 to address RUSTSEC-2026-0459; `rmcp` moves to 3.9.0 to address
   RUSTSEC-2026-0462. Source builds require Rust 1.89 or newer; prebuilt CLI and MCP installations
   need no Rust compiler.

## [1.0.0] - 2026-11-05

1.0.0 is the first stable release of AI Session Search. `aise` parses nine AI coding-agent session
formats into one index and searches it from the command line, a Rust crate, a Python package, or
an MCP server.

Stable release based on 1.0.0rc4, with the dependency updates under Security below. The notes for
the four release candidates, 1.0.0rc1 to 1.0.0rc4, are in
[docs/history/changelog-1.0.0-release-candidates.md](docs/history/changelog-1.0.0-release-candidates.md).

### Highlights

1. Search every agent's history from one index: Claude Code, Claude Desktop, ChatGPT Codex,
   Cursor, Antigravity, Pi, Prime Agent, Google AI Studio, and Gemini CLI, on macOS, Linux, and
   Windows. `aise messages search "permission denied"` finds the turn across all of them.
2. Get back a file an agent overwrote. `aise files history src/app.py` lists every version in the
   index, and `aise files extract src/app.py --restore` writes one to a collision-safe
   `.recovered` sibling.
3. See why a run stopped. Stop-hook feedback, PreToolUse blocks, local-command caveats and task
   notifications stay out of ordinary results, so `aise messages search "" --kinds harness-notice`
   asks for them.
4. Let your coding agent do the searching. `aise integrations install` registers an MCP server
   with every harness it finds, so Claude Code can answer "which version of this file did you
   write before the refactor?" mid-conversation.

### Changed

1. `cargo install ai-session-search --locked` installs a published stable version, and the
   documented command drops `--version '^1.0.0-rc'`. While crates.io held only candidates a bare
   install reported ``could not find `ai-session-search` in registry `crates-io` with version
   `*` ``, because `cargo install` takes a pre-release only when a requirement asks for one.

### Security

1. Update the TLS stack behind the `release-check` feature: `rustls` 0.23.45 to 0.23.46
   (RUSTSEC-2026-0346, RUSTSEC-2026-0347, RUSTSEC-2026-0348), `rustls-webpki` 0.103.14 to 0.103.16
   (RUSTSEC-2026-0382), and `webpki-roots` 1.0.9 to 1.0.10 (RUSTSEC-2026-0381). The two webpki
   advisories concern certificate paths through distrusted emSign intermediates. These crates
   reach aise only through `ureq`, which reads the released version for `aise package check` and
   for the once-a-day notification that follows ordinary interactive CLI output. MCP servers and
   library calls never make that request, and `[release_notifications] enabled = false` stops it.
2. Update `tokio` 1.53.1 to 1.53.3 for RUSTSEC-2026-0386, which reports that `AsyncFd::new` and
   its related constructors violate I/O safety. `AsyncFd` lives behind tokio's `net` feature,
   which this crate does not enable, so the unsound constructors were not reachable here.

### For contributors

1. A release starts at the "Before tagging, confirm" list in `RELEASING.md` rather than at the
   tag, and the maintainer skill works through that list item by item and records each result.
2. CI now also runs on a daily schedule. Its `licenses` job runs `cargo deny check advisories`,
   whose verdict depends on the advisory database rather than on this repository, so a new entry
   against an unchanged lockfile used to be noticed only when someone pushed.
3. The `rust-portability` job builds with `-D warnings`, so a `#[cfg(unix)]`-only item that sits
   unused on Windows now fails the job. Clippy runs on Linux alone, where every such item is
   live, so this job is the only place the class appears.
4. Fix nine Windows-only findings under that enforcement, in `integrations.rs`,
   `update.rs`, `executable_alias.rs`, `durable_path.rs`, `analysis_publication.rs`, and the
   process-lifecycle tests: four unused imports, an unused match binding, a misplaced test
   binding, and two helpers now gated to match their unix-only callers. The ninth, the Windows
   `create_directory_symlink`, is marked `#[expect(dead_code)]`, which records that no Windows test
   exercises a symlinked skill root and fails the build if one is added.
5. The public Rust API no longer carries command-line plumbing. `analytics::run_vocab`,
   `run_repeats`, `run_stats`, `run_planning`, `mcp_schema_budget::run`, and
   `sql_query::schema_path`, with the five argument structs the first five take, are now
   `pub(crate)` — eleven items. Each took clap-derived arguments and returned `Result<()>` after
   printing to stdout, so none was usable from a library, and none had a caller outside the crate.
6. The argument structs that stay reachable, `DateRange` and `DbSchemaArgs` among them,
   carry `#[non_exhaustive]`, so adding a command-line flag stays additive instead of breaking a
   crate that constructs one. The configuration structs were frozen this way before 1.0.0rc3 and
   this class was missed then. `DateRange` keeps its `Default`, which is how the Python binding
   and any other caller build one.

[Unreleased]: https://github.com/ahundt/ai-session-search/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/ahundt/ai-session-search/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc4...v1.0.0
