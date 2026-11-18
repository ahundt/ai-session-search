<!--
SPDX-FileCopyrightText: 2026 Andrew Hundt
SPDX-License-Identifier: Apache-2.0
-->

# Changelog

Notable changes to AI Session Search, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [1.1.0rc2] - 2026-11-18

1.1.0rc2 corrects `aise doctor` and refresh for Codex threads rewound with `thread/revert`, and
moves rusqlite to 0.43. Upgrading from 1.1.0rc1 changes no stored format and reparses no transcript.
Two items under Changed affect Rust callers of 1.1.0rc1.

### Changed

1. rusqlite moves from 0.41 to 0.43; bundled SQLite stays at 3.54.1. Rust callers of the
   deprecated `trigram_index` helpers must pass a rusqlite 0.43 `Connection`, which the crate
   re-exports as `ai_session_search::rusqlite`.
2. Rust `dates::local_time_zone()` is no longer public. It was added in 1.1.0rc1 and was the only
   public signature that named a `jiff` type. To read the system zone with the same UTC fallback,
   call `jiff::tz::TimeZone::try_system().unwrap_or(jiff::tz::TimeZone::UTC)`.
3. The Rust and Python APIs are beta and may change in a minor release; the CLI and MCP server
   are stable. The README, the library guide, and the crate README now say so.

### Fixed

1. `aise doctor` no longer reports the rollout files of a reverted Codex thread as unindexed.
   Codex `thread/revert` keeps a thread in several `rollout-<time>-<thread>_<rollout>.jsonl`
   files, and their messages were already searchable as one session in time order. `doctor`
   said the extra files produced no session and that their content was absent from every search
   result, then prescribed an `aise reindex` that could never clear the count. An append to the
   newest rollout of such a thread is now read from the appended part; before, the whole file
   was reread.
2. A refresh no longer rereads the unchanged rollout files of a reverted Codex thread. Each
   refresh parsed every rollout of such a thread in full, except the one stored as the session's
   source, even when no file had changed. A rollout that changed, is new, or was last read by an
   older parser is still read.

### Security

1. RUSTSEC-2026-0543 reports `chrono` as unmaintained. It names no vulnerability, and no patched
   version exists to move to. aise's date handling, `rmcp`, `rusqlite`, `schemars` and `edtf`
   use `chrono`, so it stays a dependency, and `deny.toml` records the advisory.

### For contributors

1. Codex fork descendants are parsed before the SQLite write transaction and revalidated inside
   it, so that transaction is no longer open while those files are read. The `aise` CLI, MCP
   server, TUI and Python package already serialize index writes with the cross-process index
   writer lock, so they behave as before; only a Rust caller that writes through a second
   connection without that lock waits less. A descendant that changed after preparation, or that
   preparation did not cover, is parsed or invalidated inside the transaction. Pi and Prime Agent
   descendants are still parsed inside it.

## [1.1.0rc1] - 2026-11-15

1.1.0 preserves indexed history when transcripts are shortened or rewritten, finds human input
across stored message roles, and adds file-recovery filters and typed library operations.

### Upgrade notes

1. Stop running aise processes, including Model Context Protocol (MCP) servers, before upgrading.
   The first writable open promotes the index to generation 6. A newly started 1.0.0 refuses it, but an already running
   1.0.0 process can still delete archived history. Storage promotion itself does not reparse
   transcripts; every parser version changes, so the first refresh after upgrading reparses
   every source. Forked Codex, Pi, and Prime Agent sessions are reconciled while the index writer
   lock is held.
2. An upgraded index can retain unused pages. Optional `aise compact` rebuilds the full-text
   indexes and rewrites the database under the writer lock; allow up to twice the index size in
   temporary disk space.
3. To downgrade, stop processes, move the index and its write-ahead-log (WAL) files aside, and use
   1.0.0 `aise reindex --full` to rebuild from source transcripts. Keep the moved files to retain
   archived history; see the [downgrade steps](docs/development/installation.md#update-and-uninstall).
4. Before upgrading scripts or library callers, see Changed for differences in `aise repeats`,
   `--role user`, file queries, path/date filters, `aise resume`, configuration validation,
   MCP requests, and Rust/Python calls.

### Added

1. Search Claude Code attached files, edited snippets, memory and plan text, and invoked skill
   contents. Harness notices (text injected by the harness) include Codex instruction snapshots,
   compaction and abort notices, Claude Agent SDK system text, and Gemini CLI attached files;
   find them with `--kinds harness-notice`. Pi and Prime Agent branch summaries and compaction
   prompt state are searchable. Doctor and index-status reports name unrecognized record types
   for all nine providers; Claude reports also name attachment types.
2. `--include-unattributed-input` on corrections and repeats includes queued input with no
   recorded sender. Reports expose `unattributed_input_count` and `unattributed_input_included`;
   MCP `run_skill_capability` and the libraries accept the same opt-in. User-role search retains
   legacy queued submissions with no recorded sender. SQL rows expose input origin and delivery
   state. Correction categories remain fixed starter phrasings; use
   [custom categories](skills/ai-session-search/SKILL.md#create-your-own-classification-package)
   for other wording.
3. `aise list` and `aise search` accept `--offset`. File history and extraction accept
   `--since`, `--until`, and `--when`; `aise files extract --all` accepts `--limit` and
   `--offset`. File commands, corrections, stats, repeats, and MCP classification accept
   session and path exclusions.
4. Message search accepts `--session-kinds` (alias `--session-kind`) and `--parent-session`;
   MCP and Python use `session_kinds` and `parent_session_id`. A parent selects direct children,
   even if its own row is absent. `--kind` and `--session-kind` combine values with their plural
   spellings; Python `kind=` and `session_kind=` combine with `kinds=` and `session_kinds=`.
5. `aise show --format json|jsonl|csv|plain` returns transcript text with line counts and
   completeness; structured message-search hits include nullable timestamps.
   MCP `get_session` focused reads accept `context_before` and `context_after`; range reads
   accept `role`, `limit`, `offset`, `order`, `since`, `until`, `when`, and `kinds`,
   including `kinds` alone. `session_metadata` on `get_session` reads includes
   `parent_session_id` and `agent_label`.
6. Rust adds typed errors and services for read-only SQL, repeat and vocabulary reports,
   configured session ranking, transcript windows, and resume-command arguments. Request
   constructors and setters support the non-exhaustive types. `open_strict()` and
   `open_strict_with_access_inputs()` reject an unrefreshed index until
   `app.index().prepare_for_read()` applies refresh policy; existing open methods retain empty-index
   reads. `MessageService::context_resolved` adds prefix lookup and a missing-session error,
   while `context` retains exact-ID lookup and empty missing results.
7. Python adds `repeats()`, `vocabulary()`, `query_sql()`, `resume_command()`, `transcript()`,
   `doctor()`, and `SessionSearch.schema()`. Classification uses `run_skill(...)`.
   Compatible named exceptions distinguish lookup and argument failures; typed values support
   equality, representations, and `to_dict()`. Paths accept `os.PathLike`, dates accept Python
   dates and datetimes, and session queries accept offsets and shared scopes.
   `prepare_for_read()` applies refresh policy, and existing-only reads refuse a missing index
   without creating it.
   See the [library guide](docs/development/library-api.md).
8. Integrations accept `--harness-home`, `AI_SESSION_SEARCH_HARNESS_HOME`, and
   `[integrations.paths]` destinations. `--no-index-preparation` skips background indexing.
   Codex receives a marketplace-backed MCP plugin; Pi and Prime Agent receive native MCP
   registrations and instructions. On Windows without symbolic-link privilege, a preflight
   probe selects manifest-owned skill copies and skips executable aliases. Reinstall refuses
   modified copies; uninstall removes only matching owned copies or links and keeps shared
   discovery entries until their last user is removed. Windows directory-link removal preserves
   the target directory's contents.
9. `aise config check` validates settings without opening the index. Doctor and index status
   report ignored keys with spelling suggestions; doctor identifies the selected config and
   discovery-location config files not read. Duration input adds ISO 8601, compound, and
   spelled-out forms; date ranges add open intervals and trailing named time zones such as
   `2026-01-15[Europe/Berlin]`. Structured timestamps use UTC `Z`; human displays include a local
   offset.

### Changed

1. `aise repeats --format json` returns one report object with groups under `groups`;
   `--format jsonl` returns that object on one line. To consume groups as before, select
   `.groups` from JSON or pipe JSON through `jq -c '.groups[]'` for one group per line.
2. `--role user`, `aise skills corrections`, and default `aise repeats` select source-attributable
   human input, including input stored under another role, and exclude generated text and mirrors
   (provider-recorded copies of an earlier input). Compared with 1.0.0, results can gain queued
   prompts and interactive answers and lose skill expansions, attached context, and copied parent
   history in Codex, Pi, and Prime Agent forks. Omit `--role` on message search for complete
   stored content; use `aise db query` with `WHERE role = 'user'` for stored-role rows.
3. `aise files history FILE` returns every version by default; 1.0.0 stopped at 50.
   Use `--limit 50` for the earlier extent. Python file queries also default to every match
   instead of 50; pass `request=FileQuery(limit=50)` for the earlier page.
4. Path filters, exclusions, and access scopes match an exact directory and its component-boundary
   descendants, treating `%` and `_` literally. Windows drive and UNC (network share) paths
   accept either separator and ignore ASCII case; macOS also ignores ASCII case for Unix-style
   paths, including the directory itself. Unix-style paths on other systems are case-sensitive,
   unlike 1.0.0 descendant matching. Use the recorded case on those systems.
5. Library date bounds on file history and reconstruction now select edit timestamps instead of
   being ignored. Reconstruction replays earlier edits before selecting versions, keeping their
   numbers; one-version reads choose the last version in the period and reject an explicitly
   chosen version outside it or nonzero paging. Multi-version reads apply paging after replay.
   Omit date bounds and use zero limit and offset to retain the earlier unfiltered extent.
6. MCP integer arguments with a wrong type or out-of-range value return
   an argument error instead of being defaulted or discarded; pass an integer within the
   parameter's bounds. `get_session` rejects `summary_items` or `include` on focused or range
   reads; omit those summary-only options.
7. Default discovery also reads `~/.gemini/antigravity-ide/brain` and, on macOS,
   `~/.cache/.gemini/tmp`. To retain the 1.0.0 roots, set `[providers.antigravity]`
   `paths = ["~/.gemini/antigravity-cli/brain", "~/.gemini/antigravity/brain"]` and
   `[providers.gemini-cli]` `paths = ["~/.gemini/tmp"]`.
8. Cursor uses recorded turn clocks, inherited by following records, instead of assigning every
   message the file modification time; absent a clock it uses a stable file-derived time. Date
   filters can select different rows after refresh.
   Codex Add File extraction now keeps the final line feed on each added line, normalizes Windows
   line endings to line feeds, and leaves zero-line additions empty; remove the final line feed if
   an older consumer requires its omission.
9. More public Rust structs and enums are `#[non_exhaustive]`: use constructors or
   `T::default()` followed by field assignment or `with_*` setters, and wildcard enum arms.
   For example, replace a `SearchFilters { ... }` literal with
   `SearchFilters::default().with_path_prefix(dir)`.
10. Omitted Python session-list and ranked-search limits follow `[search].default_limit`;
    pass `request=SessionQuery(limit=50)` for the earlier default or use zero for every session.
    Omitted `query_mode` on `search_messages()` and `search_message_batches()` follows the request
    mode; pass `query_mode="literal"` to retain the earlier override.
11. `aise resume ID` prints the native command and succeeds when stdin or stdout is redirected,
    without attempting confirmation or execution; use `--dry-run` for the same result in a terminal.
    Run the printed command in a terminal to resume.
12. Unknown ordinary configuration keys are reported and ignored instead of preventing loading;
    access-restricting and search policy tables remain strict. Use `aise config check` to reject
    unknown keys. Upgrades preserve existing config without adding active keys; install records
    retain unknown fields, and migration recovery rejects unsupported receipt formats.

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
9. Rust low-level helpers, command-printing functions, compatibility search methods, and selector
   aliases remain callable with compiler warnings; use the
   [replacement table](docs/development/library-api.md#deprecated-rust-entry-points).

### Fixed

1. Shortened, rewritten, or replaced transcripts could lose indexed history or leave stale rows;
   refresh now reconciles matching rows and preserves archived messages and file edits, including
   same-size, same-time replacements with a changed file identity. Codex discovery now reads
   compressed `.jsonl.zst` rollouts and keeps the session identity when a transcript switches
   between plain and compressed storage; decompression failures preserve the snapshot for retry. A rewritten growing transcript is detected within its last 1 MiB; older rewrites
   outside the head fingerprint and overlap still need `aise reindex --full`. Discovery warns
   when source names normalize to the same provider path instead of silently dropping a source.
2. Claude saved API request copies and repeated record identities could add duplicate turns and
   edits; they no longer do. Queued submissions delivered by attachment or after dequeue appear
   once, including after cancellation or clearing, while independent same-text submissions stay
   distinct. Tool arguments no longer also appear as assistant prose; full reindex reclassifies
   harness notices rather than deleting them.
3. Cursor could miss project directories containing dashes, dots, or underscores, drop text
   surrounding query tags, and list file edits for failed or unanswered tool calls. It resolves
   existing directory entries, preserves the whole text, and records edits only after a correlated
   result without `is_error: true`. Whole-turn query wrappers identify human input; literal query
   tags do not. Ambiguous, unreadable, or over-budget workspace resolution leaves the directory
   unset.
4. Gemini CLI file writes and replacements, successful Codex shell `apply_patch` calls, and Prime
   Agent `ipython` diffs were missing from file history. They are now recorded; positional
   `ipython` diffs remain non-replayable. Gemini JSON/JSONL migration preserves linked edits and
   original input, nested sessions link to parents, and main sessions with a project directory
   have a `gemini --resume` command. Doctor recognizes indexed session-family members.
   Antigravity checkpoints are compaction records; system and error text are harness notices
   rather than tool results.
5. Literal and regex search could miss Unicode case pairs, including Cherokee, Georgian Mtavruli,
   Osage, Adlam, and dotted capital `İ`; they now match. Tied sequence or timestamp keys have
   stable ordering, merged context retains distinct rows at the same sequence, and appends
   continue after the highest stored sequence.
6. File recovery failures now identify the first unreconstructable version and missing information,
   leaving earlier recoverable versions extractable. Date-scoped extraction ignores sessions
   with no edit in the period before checking ambiguity. Copied Codex fork edits count once only
   when an equivalent edit is retained in their indexed ancestry; otherwise the copy remains
   recoverable.
7. MCP registrations could be written where a client did not read them. Installation now uses the
   locations and formats read by OpenCode, Zed on macOS, Kilo Code, OpenClaw, Claude Code, and
   Cursor, preserving JSONC/JSON5 comments and unrelated settings. The first ordinary command
   after upgrade relocates recognized owned entries, preserving executable paths and launch
   arguments; `[integrations] refresh_after_upgrade = false` disables this. Explicit install
   regenerates launch arguments; unrecognized same-name entries remain unchanged.
8. Restarted MCP clients could lose explicit app or harness paths; registrations now retain them.
   Config initialization, package-update refresh, and `aise migrate config --cache-dir` preserve
   selected database and cache paths. Codex marketplace reinstall and uninstall preserve
   modified files. Install rollback and recovery refuse to follow a parent directory replaced by
   a link and retain the recovery receipt; older receipts without parent locations cannot detect
   that replacement.
9. An index published by `aise migrate` could stay outside WAL (write-ahead logging) mode.
   Writable opens now attempt conversion without waiting for the writer lock and retry later if
   a writer or reader prevents it.
10. Empty message-search results now suggest harness notices, tool arguments, or recoverable
    files as other places to look. MCP keeps `purpose` and `purpose_version` selectable without
    configured search presets, reporting an argument error for an unknown purpose.

### Performance

1. An oldest-first content search with a limit and many candidate messages reads them in
   session and sequence order and stops once the page is full. Searches that filter by role,
   message class, provider, tool, or path keep the earlier plan.
2. Literal matching skips positions that cannot begin a match before applying Unicode case
   folding.

### Security

1. Bundled SQLite 3.54.1 fixes RUSTSEC-2026-0459, which could expose process memory when opening
   a database beside a crafted write-ahead-log file with a different page size. The binding moves
   from rusqlite 0.40 to 0.41; callers of deprecated `trigram_index` helpers must pass 0.41
   connections, available through `ai_session_search::rusqlite`.
2. `rmcp` moves from 3.4.0 to 3.9.0 for RUSTSEC-2026-0462. Source builds require Rust 1.89
   or newer; prebuilt CLI and MCP installations need no Rust compiler.

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

[Unreleased]: https://github.com/ahundt/ai-session-search/compare/v1.1.0rc2...HEAD
[1.1.0rc2]: https://github.com/ahundt/ai-session-search/compare/v1.1.0rc1...v1.1.0rc2
[1.1.0rc1]: https://github.com/ahundt/ai-session-search/compare/v1.0.0...v1.1.0rc1
[1.0.0]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc4...v1.0.0
