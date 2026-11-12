<!--
SPDX-FileCopyrightText: 2026 Andrew Hundt
SPDX-License-Identifier: Apache-2.0
-->

# Changelog

Notable changes to AI Session Search, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [1.1.0] - 2026-11-11

1.1.0 keeps the messages and file edits aise has indexed when an agent shrinks, rewrites,
compresses or deletes its transcript, and returns a page of literal or regex search results in
under a second on an index of 5.7 million messages. Stop running aise processes before upgrading
(Changed item 1). Rust callers need code changes (Changed items 2 and 3). On the command line,
`--since 1m` now means one minute, not one month (Changed item 4).

### Highlights

1. History a transcript loses stays searchable. When a harness shrinks, rewrites, compresses or
   deletes a transcript, aise keeps the messages and file edits it already indexed and adds the
   new ones. One Codex session would have gone from 4,720 indexed messages to 16.
2. Limited literal and regex searches stop once the page is full: on an index of 5.7 million
   messages, `aise messages search error --limit 50` went from 32 s to 0.19 s.
3. The MCP server loads in OpenCode, Zed on macOS, Kilo Code and OpenClaw, where earlier
   releases wrote an entry the client ignored, and Pi and Prime Agent now receive one.
   Upgrading moves the old entries without a manual step.

### Added

1. `Db::classify_injected_messages`: labels legacy user-role harness output as harness notices
   and deletes nothing. See Fixed item 6.
2. AI Session Search appears in the Codex desktop app's Plugins list. Codex lists only plugins
   from a marketplace, so `aise integrations install` registers its own `ai-session-search`
   marketplace in `~/.codex/config.toml` and an MCP-only plugin beside the skill packages, which
   the user can install from Settings > Integrations > Plugins. The existing `[mcp_servers]` entry
   keeps serving the tools, and the plugin shares its name, so Codex runs one server. Upgrading
   adds the plugin to an existing Codex install; uninstall removes the marketplace, the plugin
   and Codex's record of installing it.
3. Integration destinations honor harness environment variables and optional
   `[integrations.paths.<surface>]` paths for MCP configs, instructions, and skill discovery.
   Install, status, uninstall, and post-upgrade refresh share the resolved paths; existing CLI
   path flags continue to add extra targets.
4. `--harness-home PATH` and `AI_SESSION_SEARCH_HARNESS_HOME` select a separate home for harness
   files without changing application state. Exact integration destinations retain their
   existing precedence.
5. `aise integrations install --no-index-preparation` installs integrations without starting
   background indexing. Omission keeps the existing preparation behavior.
6. `aise config check` validates settings without opening the index and exits nonzero on unknown
   keys. Doctor and JSON index status list ignored ordinary keys with spelling suggestions.
7. Migration receipts carry `format_version=1`; receipts without a version remain readable.
   Unsupported formats refuse before recovery changes any artifact, and supported rewrites preserve
   additive fields.
8. Skill manifests preserve additive fields at every level. A 1.0.0 reader still rejects unknown
   top-level fields and drops unknown nested fields on rewrite; 1.1.0 cannot recover discarded fields.
9. Background refresh reports stay readable when an optional failure kind is unfamiliar; an unknown
   refresh state stays explicitly unrecognized.
10. Native-archive ownership checks tolerate additive receipt metadata while continuing to require
    the supported schema, package identity, and executable hash.

### Changed

1. Index generation 6 keeps 1.0.0 from deleting archived messages and file edits: a newly
   started 1.0.0 refuses to open it. A healthy generation-5 index is upgraded by changing metadata
   only, without rereading transcripts or rebuilding FTS indexes. Stop running aise processes
   (restart harnesses) before upgrading an index 1.0.0 uses; a 1.0.0 process that already has it
   open can still delete archived history. To downgrade, move the index aside and run
   `aise reindex --full` with 1.0.0, which rebuilds from the transcripts; the moved index keeps the
   archived history for a later upgrade.
2. Every exported struct and enum is `#[non_exhaustive]`, so later releases can add fields and
   variants without breaking Rust callers; 1.0.0 exported most of them exhaustive. Struct literals
   and exhaustive matches on these types no longer compile outside the crate: start from
   `T::default()` or the type's constructor, and add a wildcard match arm. Clippy's
   `exhaustive_structs` and `exhaustive_enums` lints keep new types non-exhaustive.

   ```rust
   // 1.0.0
   let filters = SearchFilters { path_prefix: Some(dir), ..SearchFilters::default() };
   // 1.1.0
   let mut filters = SearchFilters::default();
   filters.path_prefix = Some(dir);
   ```
3. Call `RawMessage::with_authorship` before normalization instead of
   `apply_user_role_authorship`; `UserRoleAuthorshipEvidence` is crate-private.
4. Duration shorthand reads `m` as minutes and `M` or `mo` as months, as humantime and systemd
   do. `--since 1m` meant 30 days and now means 1 minute; write `--since 1M` for a month.
   Months and years now step back calendar months, so `1M` from March 31 starts on the last
   day of February instead of March 1.
5. Timestamps have one machine form and one for people. JSON output, MCP results and the Python
   binding end in `Z` (`2026-11-07T20:31:22.663Z`), the form chrono's serde,
   JavaScript and Python 3.12 read; `messages`, `files`, analytics and parts of MCP used to write
   `+00:00`. Tables and `aise show` print local time with its offset
   (`2026-11-07T15:31:22.663-05:00`), which can be pasted into `--since`. Stored timestamps are
   unchanged.
6. A date or time without an offset is read in your local time zone, as
   [EDTF](https://www.loc.gov/standards/datetime/) and ISO 8601 define it. At 8 pm in New York,
   `--when yesterday` and `--when 2026-11-07` now mean your day, not the UTC day. `TZ` overrides
   the system zone. To read one value in another zone, append the zone in
   [RFC 9557](https://www.rfc-editor.org/rfc/rfc9557) form: `--since '2026-01-15[Europe/Berlin]'`.
   CLI flags, MCP `since`/`until`/`when` and the Python `DateRange` accept the same strings, and
   `reference_time` takes that form too. Days that daylight saving shortens or lengthens are
   covered, and a time a clock change skips or repeats resolves as
   [RFC 5545](https://www.rfc-editor.org/rfc/rfc5545) specifies. `aise dates` shows the zone in
   use.
7. Durations also accept ISO 8601 (`P1M`, `PT2H`), compound (`1y6M`, `1d12h`) and spelled-out
   (`2 weeks`) forms; `3 days ago` still selects that whole day. EDTF open intervals
   (`2026-01/..`, `../2026-03`) select everything after or before a date, and a space may
   separate date and time (`2026-01-15 14:30`), as RFC 3339 allows.
8. `messages search --role user` and matching Rust, Python and MCP role selectors now find
   recorded prompts, commands, queued submissions and interactive answers across stored roles.
   Known parent-agent prompts sent to spawned sessions are excluded from this selector. Returned
   records keep their original roles; omitting the role filter still searches complete content.
9. Limited literal and regex searches stop once the page is full. On an index of 5.7 million
   messages, `aise messages search error --limit 50` went from 32 s to 0.19 s, and
   `aise messages search "permission denied" --limit 50` from 3.5 s to under 1 s.
10. A needle containing `İ` uses the trigram index for the rest of its text.
    `aise messages search İstanbul --all-results` went from 4.6-6.3 minutes to 3.9-5.2 s on the
    same index.
11. The literal matcher skips positions that cannot start a match: about 3x faster when every
    candidate has to be checked.
12. Reindexing a source whose parse matches what is stored writes nothing. A file whose
    timestamp changed but whose content did not now takes 27 ms on a 20,000-message session,
    down from 1.65 s.

### Deprecated

1. `Db::purge_injected_messages`: it now relabels through `Db::classify_injected_messages`
   and deletes nothing. Call that instead.

### Fixed

1. Searches find text in Cherokee, Georgian Mtavruli, Osage, Adlam and other scripts whose
   case pairs SQLite's full-text tokenizer does not fold (445 lowercase letters). Cherokee text
   returned nothing, even searched by its exact spelling. Literal and regex searches now return
   every row the matcher accepts.
2. A needle ending in `i` matches a dotted capital `İ` in the text, so `abi` finds `abİ`.
3. Messages that tie on session and sequence number come back in one fixed order. Their order
   was undefined, so `--match-window latest` could keep the wrong message, and literal or fuzzy
   paging could move one between pages.
4. `aise skills corrections` returned messages tied on timestamp, session and sequence number in
   reverse insertion order, and `aise repeats` did the same under its default user-role filter.
   Both now page in one order whatever the filters. Context windows and file-edit history use
   the same tie-breaker, so several edits made by one message replay in the order they were
   written.
5. Appending to an indexed session could give two messages the same sequence number. On one
   40,180-message Codex session, three sequence numbers each named two messages written a day
   apart. New messages now continue after the highest stored sequence number, and a reindex of
   a session with gaps in its numbering replaces it from the parse.
6. A full reindex deleted stored user-role rows starting with `<environment_context>`,
   `<turn_aborted>` or `<local-command-*>`, records the parsers now keep on purpose. These rows
   are now labelled harness notices, which default search, corrections and repeat mining leave
   out, and nothing is deleted.
7. A reindex no longer deletes stored messages when a harness shrinks or rewrites its
   transcript. One Codex session went from 4,720 indexed messages to 16 after Codex rewrote
   its file. With the same parser version, aise keeps the stored messages and file edits and
   adds the new ones. Session search, the session's dates and its message count cover both;
   a new file edit replays before the stored edits that followed it in the source; and a
   source that gets a new session id moves its rows to that id, skipping rows the id already
   holds, where they used to be deleted with the old id.

   A new parser version keeps them too: it corrects each stored row it can match in the new
   parse, keeps the rows it cannot match, and adds the new ones after them. On a
   20,000-message session the kept reindex takes 43 ms; the full replace it avoids took 1.65 s.
8. Codex sessions that Codex compressed are indexed. After 7 idle days Codex rewrites a rollout
   as `rollout-*.jsonl.zst` and deletes the plain file; aise read only `.jsonl`, so it missed
   everything Codex wrote after its last read of such a session. The compressed file is now the
   same source as its plain form, and the session keeps its id and stored messages. A `.zst`
   that cannot be decompressed is a warned parse failure for a session not yet indexed. For an
   indexed session, aise keeps the stored messages, reports the failure in `aise reindex` and
   `aise doctor`, and reads the file again on the next refresh.
9. OpenCode loads the aise MCP server. aise wrote it directly under `mcp`, which OpenCode 2
   ignores, so `opencode mcp list` reported no servers. It is now written to
   `mcp.servers.ai-session-search` with `type: "local"`, and `aise integrations install` moves
   an entry left in the old place.
10. Zed on macOS loads the aise MCP server. aise wrote it to
    `~/Library/Application Support/Zed/settings.json`, but Zed reads `~/.config/zed/settings.json`
    there. Install now writes the file Zed reads and removes aise's entry from the old one,
    keeping any other servers in it. Linux and Windows paths were already correct.
11. `aise integrations install` edits client JSON files that contain comments or trailing
    commas, as Zed's settings and VS Code's `mcp.json` allow. One such file made the whole
    install fail for every client. aise now changes only its own entry and leaves the comments,
    commas and other settings as they were.
12. The Kilo Code CLI and JetBrains plugin load the aise MCP server. aise wrote only the VS Code
    extension's `mcp_settings.json`, which Kilo 7 reads only inside VS Code. Install now also
    writes `mcp.ai-session-search` into `~/.config/kilo/kilo.jsonc`, which every Kilo 7 surface
    reads.
13. OpenClaw loads the aise MCP server. aise wrote a top-level `mcpServers` in
    `~/.openclaw/openclaw.json`, but OpenClaw reads servers from `mcp.servers`, and its file is
    JSON5, which aise could not parse. Install now writes `mcp.servers.ai-session-search`, removes
    the old entry, and keeps the file's comments and JSON5 syntax.
14. Upgrading moves these MCP entries without a manual step. The first command after an upgrade
    already refreshed installed skills and instruction blocks; it now also moves an aise MCP
    entry an earlier release left where its client no longer reads it, keeping the executable
    that entry named. Clients without an existing aise integration are left alone.
15. Pi 0.99.0 and newer receive the aise MCP server in `~/.pi/agent/mcp.json` and MCP-first
    instructions. Earlier installs provided only CLI guidance. Untouched CLI-only integrations
    gain the MCP registration during the post-upgrade refresh.
16. Prime Agent receives a local aise MCP registration under `mcpServers` in
    `~/.prime/agent/settings.json` with its required `type: "stdio"`. It previously received only
    CLI guidance; untouched CLI-only integrations also gain the registration during refresh.
17. Cursor receives the required `type: "stdio"` in `~/.cursor/mcp.json`. aise omitted the
    field; install and the post-upgrade refresh now add it to earlier untyped registrations.
18. Claude Code no longer receives an aise entry in `~/.claude/.mcp.json`, which is not its
    user MCP configuration. Install writes `~/.claude.json` and removes aise entries from the old
    file; the post-upgrade refresh also moves an entry left there, preserving other content.
19. Gemini CLI uses `<gemini home>/.agents/skills/ai-session-search`, where Gemini home is
    `GEMINI_CLI_HOME` or the real home; Codex keeps `~/.agents/skills/ai-session-search`.
    Matching paths share one link, avoiding a duplicate skill warning; different paths get separate
    links, and uninstalling either harness preserves the other's link. Install and upgrade refresh
    remove untouched aise-owned entries from `<gemini home>/.gemini/skills/ai-session-search`;
    modified or foreign entries are preserved and reported.
20. MCP registrations retain explicitly selected config, database, cache, and harness paths
    when a client starts the server without the installer's environment. Reinstall preserves
    the entry's environment and unknown fields, and refuses conflicting state selections before
    writing. Config-file database and cache settings remain editable without reinstalling.
21. `aise config init` records CLI- or environment-selected database and cache paths instead
    of platform defaults. `aise package update` passes state selections to the replacement
    executable's skill refresh.
22. Configs with newer ordinary keys load on 1.1.0 and later. Interactive CLI commands name ignored
    keys on stderr; MCP serving emits no unknown-key warnings. Provider source restrictions, search
    scope, hard search budgets, and purpose/policy tables still reject unknown keys with recovery
    guidance. The released 1.0.0 binary remains strict: automatic upgrades never add active keys to
    an existing config, and `config init` keeps new defaults commented.
23. Claude queued submissions and structurally attributable `AskUserQuestion` answers are
    searchable. Input-focused search includes legacy queue submissions while keeping their sender unknown.
    They remain submitted because the source does not prove delivery.
    Independent same-text submissions survive source rewrites, and parser upgrades preserve
    file-edit replay order.
24. User-input message searches preserve NUL-containing prompts and answers, Unicode match
    coordinates, and boundaries between user content and generated text. Literal and regex
    queries containing NUL use the safe portions of the trigram prefilter.
25. Gemini CLI native JSONL history retains original user prompts when context patches replace
    their current content. Legacy JSON/JSONL migrations retain archived inputs and message
    references without counting the same native event again on refresh.
26. Native input regions in Codex, Cursor, Antigravity and Pi/Prime records retain their source
    distinctions from generated context and command output. Correlated Codex answers in known
    spawned sessions follow the existing agent-attribution convention.

### Security

1. Upgrade bundled SQLite from 3.53.2 to 3.54.1 to resolve RUSTSEC-2026-0459, which can expose
   uninitialized memory when opening a crafted database and WAL. Source builds now require
   Rust 1.89 or newer, up from 1.88; prebuilt CLI and MCP users do not need a Rust compiler.

### For contributors

1. The November 7, 2026 dependency refresh selected semver-compatible releases that build
   with Rust 1.88 and were at least 7 days old, matching the cooldown in
   `.github/dependabot.yml`. Among them: `rmcp` 3.4.0 to 3.8.0, `tokio`
   1.53.3 to 1.54.1, `pyo3` 0.29.2 to 0.29.3, `ureq` 3.4.2 to 3.5.0, and `uuid` 1.24.0 to
   1.26.1. `jiff` stays at 0.2.39, which `main` already uses. `pyo3` 0.30 and `rusqlite` 0.41
   fell outside that refresh's version requirements; the SQLite security upgrade above is a
   separate change.
   A later security update selects `rmcp` 3.9.0 to resolve RUSTSEC-2026-0462.
2. The workspace uses Cargo's resolver 3, so `cargo update` selects only versions that build
   with the declared `rust-version`.
3. `dirs` 6.0.0 to 7.0.0. Its only source change moves Windows `preference_dir()` from Local to
   Roaming AppData; aise calls `home_dir`, `config_dir`, `data_dir`, `data_local_dir`, and
   `cache_dir`, which return the same paths, so existing configuration and indexes stay where
   they are.
4. `./run_ci_local.sh` now runs the required MSRV library-test build and API-consumer check
   when the workspace's minimum Rust toolchain is installed. Otherwise it reports named skips
   with the exact install command; contract tests keep the commands aligned with CI.
5. Python tests and runtime verifiers share an allowlisted environment builder with temporary
   application and harness state and disabled transcript providers. Pytest sanitizes its
   environment before collection; Rust process tests use a per-test command factory. Demo homes
   are unique temporary directories.
6. Temporary integration verifiers skip background index preparation. Historical upgrade
   checks stop and wait for their process groups before removing temporary roots. Demo commands
   use owned homes and an isolated Python launcher when the executable is a console script.

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
