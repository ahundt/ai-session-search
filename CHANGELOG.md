<!--
SPDX-FileCopyrightText: 2026 Andrew Hundt
SPDX-License-Identifier: Apache-2.0
-->

# Changelog

Notable changes to AI Session Search, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

1. `Db::classify_injected_messages`: labels legacy user-role harness output as harness notices
   and deletes nothing. See Fixed item 6.

### Changed

1. Duration shorthand reads `m` as minutes and `M` or `mo` as months, as humantime and systemd
   do. `--since 1m` meant 30 days and now means 1 minute; write `--since 1M` for a month.
   Months and years now step back calendar months, so `1M` from March 31 starts on the last
   day of February instead of March 1.
2. Timestamps have one machine form and one for people. JSON output, MCP results and the Python
   binding end in `Z` (`2026-11-07T20:31:22.663Z`), the form chrono's serde,
   JavaScript and Python 3.12 read; `messages`, `files`, analytics and parts of MCP used to write
   `+00:00`. Tables and `aise show` print local time with its offset
   (`2026-11-07T15:31:22.663-05:00`), which can be pasted into `--since`. Stored timestamps are
   unchanged.
3. A date or time without an offset is read in your local time zone, as
   [EDTF](https://www.loc.gov/standards/datetime/) and ISO 8601 define it. At 8 pm in New York,
   `--when yesterday` and `--when 2026-11-07` now mean your day, not the UTC day. `TZ` overrides
   the system zone. To read one value in another zone, append the zone in
   [RFC 9557](https://www.rfc-editor.org/rfc/rfc9557) form: `--since '2026-01-15[Europe/Berlin]'`.
   CLI flags, MCP `since`/`until`/`when` and the Python `DateRange` accept the same strings, and
   `reference_time` takes that form too. Days that daylight saving shortens or lengthens are
   covered, and a time a clock change skips or repeats resolves as
   [RFC 5545](https://www.rfc-editor.org/rfc/rfc5545) specifies. `aise dates` shows the zone in
   use.
4. Durations also accept ISO 8601 (`P1M`, `PT2H`), compound (`1y6M`, `1d12h`) and spelled-out
   (`2 weeks`) forms; `3 days ago` still selects that whole day. EDTF open intervals
   (`2026-01/..`, `../2026-03`) select everything after or before a date, and a space may
   separate date and time (`2026-01-15 14:30`), as RFC 3339 allows.

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
4. `aise corrections` returned messages tied on timestamp, session and sequence number in
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
   adds the new ones:
   - session search, the session's dates and its message count cover both;
   - a new file edit replays before the stored edits that followed it in the source;
   - a source that gets a new session id moves its rows to that id, skipping rows the id
     already holds. They used to be deleted with the old id.

   A new parser version still replaces the session, because it may render the same message
   differently. On a 20,000-message session the kept reindex takes 43 ms; the full replace it
   avoids took 1.65 s.

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
    that entry named. A client with no aise entry is not configured.

### Performance

1. Limited literal and regex searches stop once the page is full. On an index of 5.7 million
   messages, `aise messages search error --limit 50` went from 32 s to 0.19 s, and
   `aise messages search "permission denied" --limit 50` from 3.5 s to under 1 s.
2. A needle containing `İ` uses the trigram index for the rest of its text.
   `aise messages search İstanbul --all-results` went from 4.6-6.3 minutes to 3.9-5.2 s on the
   same index.
3. The literal matcher skips positions that cannot start a match: about 3x faster when every
   candidate has to be checked.
4. Reindexing a source whose parse matches what is stored writes nothing. A file whose
   timestamp changed but whose content did not now takes 27 ms on a 20,000-message session,
   down from 1.65 s.

### For contributors

1. `Cargo.lock` takes the newest release of each dependency that is semver-compatible, builds
   with the minimum supported Rust (1.88), and is at least 7 days old as of November 7, 2026,
   the cooldown `.github/dependabot.yml` applies. Among them: `rmcp` 3.4.0 to 3.8.0, `tokio`
   1.53.3 to 1.54.1, `pyo3` 0.29.2 to 0.29.3, `ureq` 3.4.2 to 3.5.0, and `uuid` 1.24.0 to
   1.26.1. `jiff` stays at 0.2.39, which `main` already uses. `pyo3` 0.30 and `rusqlite` 0.41
   fall outside the current version requirements; each needs its own change.
2. The workspace uses Cargo's resolver 3, so `cargo update` selects only versions that build
   with the declared `rust-version`.
3. `dirs` 6.0.0 to 7.0.0. Its only source change moves Windows `preference_dir()` from Local to
   Roaming AppData; aise calls `home_dir`, `config_dir`, `data_dir`, `data_local_dir`, and
   `cache_dir`, which return the same paths, so existing configuration and indexes stay where
   they are.

## [1.0.0] - 2026-11-05

1.0.0 is the first stable release of AI Session Search. `aise` parses nine AI coding-agent session
formats into one index and searches it from the command line, a Rust crate, a Python package, or
an MCP server.

Stable release based on 1.0.0rc4, with the dependency updates under Security below.

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

## [1.0.0rc4] - 2026-09-28

Upgrading is now one step: after any package manager replaces `aise`, the first command brings
the installed skill and aise's instructions up to date, and a `config.toml` written for 1.0.0rc2
loads again. Upgrading from 1.0.0rc3 needs no action.

### Added

1. `aise integrations status --format json` reports each integration's `component`, `client`,
   `path`, `state`, and `current`, which is true when it matches the running version.

### Changed

1. Upgrading no longer needs `aise integrations install`. After `uv tool upgrade`, pip, Cargo, or
   a native archive replaces `aise`, the first command or MCP server start updates the installed
   `ai-session-search` skill and the aise block in each harness's CLAUDE.md, AGENTS.md, or
   GEMINI.md, once, and prints a line for each one it updates. Blocks stay where they are, edits
   are kept and reported once, and deleted files stay deleted. To turn this off, set
   `[integrations] refresh_after_upgrade = false`.
2. The automatic update covers installs that included the skill, which is the default. If you
   installed with `--no-skill`, or added instruction files with `--claude-md`, `--agents-md`, or
   `--gemini-md`, run `aise integrations install` after upgrading.
3. `aise config init` writes every setting commented out except the database and cache paths, so
   settings you leave alone follow the built-in defaults, including ones a later release changes.
   Existing config files are not touched, and since no default changed in 1.0.0rc4, a file an
   earlier release wrote needs no edit.

### Fixed

1. A `config.toml` that sets `[ui].preview_lines`, the 1.0.0rc2 name for
   `[ui].preview_body_lines`, loads again. 1.0.0rc3 refused it with
   ``unknown field `preview_lines` ``. A file that sets both uses `preview_body_lines`.
2. With a config file outside the default location, set by `AI_SESSION_SEARCH_CONFIG` or
   `--config`, `aise skills update` failed with ``selected as both app and custom`` after a
   successful `aise integrations install`, and so did the skill update at the end of
   `aise package update`. It now updates the skill.
3. `aise skills update`, and so `aise package update`, put back a skill directory you had deleted
   and wrote a skill for a harness installed with `--no-skill`. It now updates only skill
   directories that exist; `aise integrations install` adds one.

### For contributors

1. CI and the local gate on every change, and the publish workflow before anything is signed,
   fail when upgrading from any published release would need a manual step. For each release
   they install its integrations for every harness it supports that writes files and its printed
   config, run one command with the new executable, and require the config to load and
   `aise integrations status --format json` to report every integration current.
2. The metadata gate checks the released changelog section's shape (summary first, known headings
   in order, numbered items) and appends a generated footer with the upgrade and install commands,
   download guide, and diff link. `--notes-only` renders the body for an already published tag.
3. Dependency updates: maturin 1.15.0, ruff 0.16.9, mypy 2.3.1, the current `setup-uv` and
   `rust-toolchain` actions, rmcp 3.4.0, clap 4.6.7, yaml-rust2 0.13, and patch releases of toml,
   toml_edit, ureq, and unicode-width. The rmcp requirement rises to 3.4.0, the first release
   with the `ServerConfig` name the MCP server now uses.

## [1.0.0rc3] - 2026-09-16

This release reworks the terminal browser, `aise tui`: typing no longer waits for a search, and you
can filter sessions, edit the query in place, rebind any key, and press `?` to list them. Three
changes can affect you: a `config.toml` that sets `[ui].preview_lines` stops every `aise` command,
including the MCP server your AI tools start, until you rename it right after upgrading (Changed,
item 1); the preview now scrolls with `K`/`J` instead of `h`/`l` (item 2); and Rust code that
builds config structs with a struct literal must start from `Default::default()` (item 3). The
index format is unchanged, so no reindex is needed. A `rustls` security update is included.

### Highlights

1. Typing responds immediately. Searches run on a background thread, a new keystroke cancels the
   search in progress, and the previous results stay on screen until new ones arrive. The TUI
   searches once the query has been still for `[ui].search_debounce_ms` (150 ms by default), so on
   a 36.5 GB index a five-letter word starts one search instead of five. Enter searches right away,
   and setting it to `0` searches on every keystroke.
2. Filter keys: `p` cycles the provider, `f` the session kind, `s` the time window (1, 7, or 30
   days), and `w` shows only sessions with warnings. The status bar shows which filters are on.
   They are the same filters the CLI, MCP server, and Python API accept.
3. `?` lists every command and the keys bound to it, built from your current bindings. The preview
   scroll and page keys scroll the list when it is taller than the terminal; any other key closes
   it.
4. The search box edits like a text field. Left and Right move the cursor, Home/Ctrl+A and
   End/Ctrl+E jump to the ends, Delete removes forward, Ctrl+W deletes a word, and Ctrl+U clears the
   query. The status bar names Ctrl+U, Ctrl+W, and Home/End while the box has focus.
5. `[ui.keys]` rebinds any of the 28 TUI commands: list the keys a command answers to, or `[]` to
   unbind it. A configuration that gives one key two meanings in the same mode, or unbinds both
   `quit` and `interrupt`, is rejected. `config.example.toml` lists the command names.
6. Ctrl+C quits: the first press warns in the status bar, the second exits, and any other key
   cancels. It used to type a `c` into the search box.
7. `[ui].unicode` and `[ui].color` (`auto`, `on`, or `off`) control box-drawing characters and
   colour. `auto` falls back to ASCII when the locale cannot display Unicode and drops colour under
   `NO_COLOR` or `TERM=dumb`. The selected row carries a marker, so it stays visible without colour.
8. An empty session list says what to press next: the filter keys when a filter hid everything, or
   `aise reindex` when the index has no sessions.
9. When a transcript is longer than the preview pane, the pane title shows the visible rows and the
   total.

### Added

New and renamed `[ui]` settings, each documented in `config.example.toml`:

| Setting | Default | Controls |
| --- | --- | --- |
| `preview_body_lines` | 34 | Transcript lines in the preview (the rc2 name was `preview_lines`) |
| `search_debounce_ms` | 150 | Quiet time before a typed query is searched |
| `idle_poll_interval_ms` | 150 | How often an idle TUI checks for work |
| `list_page_rows` | 10 | Rows moved by Page Up/Page Down in the session list |
| `preview_scroll_rows` | 5 | Rows moved by one preview scroll |
| `preview_page_rows` | 15 | Rows moved by one preview page |
| `provider_label_width` | 9 | Provider column width, widened to fit the longest label |
| `list_pane_percent` | 45 | Share of the width given to the session list, 10 to 90 |
| `unicode`, `color` | `auto` | Box-drawing characters and colour |
| `[ui.keys]` | shipped keys | Key bindings for the 28 TUI commands |

### Changed

1. `[ui].preview_lines` is now `[ui].preview_body_lines`, and it takes effect: 1.0.0rc2 accepted
   `preview_lines` but never read it. While `config.toml` still sets the old name, every 1.0.0rc3
   command, including `aise mcp serve`, exits with ``unknown field `preview_lines` ``. 1.0.0rc2
   rejects the new name, so rename it right after upgrading:

   ```toml
   [ui]
   preview_body_lines = 34
   ```

   The next release accepts both names.
2. The preview scrolls with `K`/`J` and Shift+Up/Shift+Down. `h`, `l`, Left, and Right no longer
   scroll it. To keep them as well:

   ```toml
   [ui.keys]
   preview_scroll_up = ["K", "shift+up", "h", "left"]
   preview_scroll_down = ["J", "shift+down", "l", "right"]
   ```

3. Rust API: configuration structs read from `config.toml` are `#[non_exhaustive]`, so a struct
   literal no longer compiles. Start from `Default::default()` and assign the fields you need.
   `ConfigOverrides` is unchanged. Adding a setting is now a minor release rather than a breaking
   one.
4. A zero preview, pacing, step, or label-width setting, or a pane percentage outside 10 to 90, is
   rejected when the configuration loads instead of silently doing nothing.
5. The TUI labels Antigravity `ANTIGRAV` and Gemini CLI `GEMINICLI`; the old `GEMINI` and `Gemini`
   labels were easy to confuse.
6. The session list title shows whether results are ordered by recency or by rank.
7. Preview scrolling stops when the last line reaches the bottom of the pane.
8. The bundled SQLite is 3.53.2, up from 3.50.2. Across 52 paired benchmark cases the results were
   identical and peak memory changed by between −10% and +2.6%.

### Fixed

1. Chorded keys ran the command bound to their plain letter: Ctrl+Q quit, and Ctrl+H, which many
   terminals send for Backspace, scrolled the preview. A chord now matches only its own binding.
2. Typing moved the selection back to the first row. The selection and preview scroll now stay put
   when the new results still contain the selected session.
3. A database error while typing closed the TUI. The error now shows on its own line, shortened in
   the middle so the suggested fix at its end stays visible.

### Security

1. `rustls` 0.23.45 fixes
   [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285): a TLS 1.3 peer could send
   in plaintext handshake messages that should have been encrypted. It could not alter or complete
   a handshake. `aise` uses TLS only to fetch release metadata.

### For contributors

1. Benchmark reports compare each case's registered result digest rather than hashing timing noise.
   The renderer rejects missing or duplicate samples, refuses a `GO` decision without relevance
   evidence, prints commands that reproduce a paired run, and alternates which build runs first.
2. The TUI latency benchmark checks the ordered session IDs and the rendered total over a
   128-session workload that includes one transcript larger than the 8 MiB scoring batch. Release
   samples measure the TUI process tree alone.
3. The local release gate installs and smoke-tests the exact sdist before a tag is pushed.
4. The TUI worker holds at most one pending search and one pending preview, and request
   generations keep a stale response from replacing current results. Cancellation is checked every
   64 KiB, the preview reads one SQLite snapshot, scoring shares the process's Rayon pool, and a
   settled TUI waits once per `idle_poll_interval_ms` instead of waking 100 times a second.
5. With rusqlite 0.40, installing a SQLite progress handler or authorizer can fail. An MCP tool call
   now reports when it could not arm cancellation, and `query_session_index` is refused when its
   read-only restriction cannot be installed, rather than running without it.
6. `deny.toml` no longer ignores any advisory. Its one exception, for the unmaintained `paste`
   crate, went away when ratatui 0.30.2 stopped depending on it.

## [1.0.0rc2] - 2026-08-22

### Upgrading and breaking changes

No command, flag, or configuration key was removed. What does change:

Run `aise integrations install` after upgrading. The packaged skill gained a reference file, and
`aise integrations status` reports `incomplete: 1 of 5 managed files missing` until the install
copies it.

The first search after upgrading refreshes the index, because provider parsing changed. Watch it
with `aise doctor`; searches keep answering from the previous index while the refresh runs. The
index layout stays complete for a 1.0.0rc1 process reading the same file, with one exception: once
the index holds Prime Agent sessions, an older build refuses it and names the version to install.

Two structured outputs changed shape. MCP tools stopped advertising a JSON-Schema `default`, so a
client reading defaults out of the schema finds them in the tool description text instead. And
message-search receipts give `corpus` and `candidates` a single meaning across every surface, so a
consumer comparing receipt numbers with 1.0.0rc1 sees different values for some query modes.

### Added

- Prime Agent sessions, bringing the searchable set to nine local formats: Claude Code CLI and
  Desktop, Claude Desktop local agent, ChatGPT Codex desktop and CLI/IDE, Cursor, Antigravity,
  Pi, Prime Agent, Google AI Studio, and Gemini CLI.
- Resume commands for subagent runs. Pi and Prime Agent children resume by transcript path, and a
  Claude Code child names the session that spawned it.
- Gemini CLI tool calls, tool results, and the notices its harness injects are indexed, so
  `--field tool-name` and `--field tool-argument` searches reach them.
- `aise skills` accepts custom message-classification packages. Write an `aise-capability.toml`,
  register it with `aise integrations install --skill-root`, and run it from the CLI or through the
  `run_skill_capability` MCP tool.
- Recovery receipts from `aise files extract` name the session each version came from and print the
  recovered content's checksum, including in bulk recoveries.
- Pi and Prime Agent installs receive the packaged skill and `AGENTS.md` guidance. Neither runs an
  MCP client, so they get the CLI workflow instead.

### Changed

- `aise --help` groups the root commands, and `aise messages search --help` sections its options
  under headings rather than listing them flat.
- `aise search` with no query prints the equivalent `aise list` invocation, carrying over the
  session filters and output options that were supplied.
- A retired flag spelling on `aise messages search`, such as `--project` or `--regex`, is answered
  with the spelling that replaced it rather than the parser's nearest guess. These spellings were
  already rejected; only the message changed.
- `aise integrations status` distinguishes a package whose files are all absent from one missing
  some, reporting the count and the command that restores them.
- An index refresh stopped by a full disk reports `postponed` with a retry interval and states that
  the last completed index remains intact. After space is freed it names the incremental
  `aise reindex` retry and `aise doctor` verification instead of promising an ownerless timer or
  asking for `aise reindex --full`, which would fail again while the disk is full.
- A command that runs out of disk space says what survived and what to do next. The Python API
  raises `OSError` for that case, matching what Python callers already catch for a full disk.
- The `search_messages` MCP description states what computing a receipt's corpus count costs, so a
  caller can decide whether to ask for one.
- Path filters state that they match on directory-component boundaries, so `--path /a/b` does not
  select `/a/bc`.
- Search runs faster on a synthetic benchmark corpus. Comparing 1.0.0rc1 with this version over
  three repetitions each: exact content 27.9 ms to 14.3 ms, fuzzy content 50.8 ms to 20.6 ms, regex
  content 20.9 ms to 14.8 ms. All seven benchmark cases returned an identical result digest on both
  versions. Personal session histories are larger than that corpus, and their timings differ.
- Message-search receipts count the corpus from indexes rather than scanning message rows. On one
  2.7-million-message index that cut a receipt from tens of seconds to a fraction of a second.

### Fixed

- MCP clients that fill in a schema's advertised `default` no longer break `get_session`. Calling
  `get_session` with `message_seq` or `summary` from Claude Code was rejected with "Use only one
  get_session output selector", because the client also sent the advertised `transcript_lines`
  default. No tool advertises a JSON-Schema `default` now, and the omission values appear in the
  tool descriptions instead.
- `aise repeats --regex` applies the pattern. It previously discarded the query and mined every
  message.
- Reindexing no longer aborts on a session whose recorded start falls after its recorded end.
- A literal search finds a Greek word ending in sigma. Searching for `ΟΔΟΣΣ` missed a message
  containing that exact word: Greek writes lowercase sigma as `ς` at the end of a word and `σ`
  elsewhere, the query was lowercased as a whole string and the stored text character by character,
  and the two sides then disagreed on the last letter.
- Snippets keep the first character of a match that ends inside a character whose lowercase form is
  longer than the original.
- Session date filters compare against the widest event time a session contains, so a filter no
  longer misses sessions whose first and last parsed records are not their earliest and latest.
- Pi discovery reads a configured root that holds both its own transcripts and a `sessions` child,
  and reports each transcript once when configured roots overlap.
- `aise repeats` and the corrections capability exclude text a coding harness injected into the
  user's turn, so an automated notice no longer counts as something a person repeated.

### Removed

- A full-text index over session titles, summaries, and whole transcripts that every session write
  maintained and no query read. It is retired in place, so a 1.0.0rc1 process sharing the same index
  file still sees a complete layout. On one 2.7-million-message index this reclaimed about 460 MB.

### Security

- Published wheels no longer record the directory they were built in. The embedded CycloneDX SBOM
  names workspace crates by relative path, and artifact verification rejects a wheel that still
  carries a build directory.
- Wheels build reproducibly from a pinned `SOURCE_DATE_EPOCH`, and artifact verification proves the
  build received it.
- Build provenance attestation runs only for a real tag push.

## [1.0.0rc1] - 2026-08-11

First published release. See the [tag](https://github.com/ahundt/ai-session-search/releases/tag/v1.0.0rc1).

[Unreleased]: https://github.com/ahundt/ai-session-search/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc4...v1.0.0
[1.0.0rc4]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc3...v1.0.0rc4
[1.0.0rc3]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc2...v1.0.0rc3
[1.0.0rc2]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc1...v1.0.0rc2
[1.0.0rc1]: https://github.com/ahundt/ai-session-search/releases/tag/v1.0.0rc1
