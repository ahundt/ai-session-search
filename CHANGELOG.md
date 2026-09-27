<!--
SPDX-FileCopyrightText: 2026 Andrew Hundt
SPDX-License-Identifier: Apache-2.0
-->

# Changelog

Notable changes to AI Session Search, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Version 1.0.0 is the first public
compatibility baseline; tags below it do not define a compatibility contract.

## [Unreleased]

## [1.0.0rc4] - 2026-09-27

Upgrading is now a single step: after any package manager replaces `aise`, the first command
updates the installed skill for you, and a `config.toml` written for 1.0.0rc2 loads again.
`aise config init` now leaves settings commented out, so defaults a later release changes reach
files it writes from now on. Upgrading from 1.0.0rc3 needs no action.

### Changed

1. Upgrading no longer needs `aise integrations install`. After `uv tool upgrade`, pip, Cargo,
   or a native archive replaces `aise`, the first command or MCP server start updates the
   installed `ai-session-search` skill once and prints a line for each skill it updates. Skill
   files you edited are left alone and reported once, and a skill directory you deleted stays
   deleted; `aise integrations install` restores it. Set
   `[integrations] refresh_after_upgrade = false` to turn this off.
2. `aise config init` writes every setting commented out except the database and cache paths.
   Uncommenting a line changes that setting; everything else follows the built-in defaults,
   including ones a later release changes. Existing config files are not touched, so a file
   `aise config init` wrote in 1.0.0rc3 or earlier keeps pinning that release's defaults.

### Fixed

1. A `config.toml` that sets `[ui].preview_lines`, the 1.0.0rc2 name for
   `[ui].preview_body_lines`, loads again. 1.0.0rc3 refused it with
   ``unknown field `preview_lines` ``.

### For contributors

1. CI and the local gate on every change, and the publish workflow before anything is signed,
   fail when upgrading from any published release since 1.0.0rc2 would need a manual step. For
   each release they install its integrations for every harness that writes files and its printed
   config, run one command with the new executable, and require the config to load and every
   integration to report current.
2. The metadata gate checks the released changelog section's shape (summary first, known headings
   in order, numbered items) and appends a generated footer with the upgrade and install commands,
   download guide, and diff link. `--notes-only` renders the body for an already published tag.
3. Dependency updates: maturin 1.15.0, ruff 0.16.7, mypy 2.3.1, the current `setup-uv` and
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

[Unreleased]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc4...HEAD
[1.0.0rc4]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc3...v1.0.0rc4
[1.0.0rc3]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc2...v1.0.0rc3
[1.0.0rc2]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc1...v1.0.0rc2
[1.0.0rc1]: https://github.com/ahundt/ai-session-search/releases/tag/v1.0.0rc1
