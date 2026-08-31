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

### Changed

- The TUI preview honors `[ui].preview_lines` (default 34) as its total body budget. The
  historical 8/4/8/14 section weights remain fixed even when a role is absent, so the default
  preserves the previous output and smaller explicit budgets trim each section proportionally.
- The TUI session list title names the active ordering (recent vs ranked), and preview
  scrolling stops when the last line reaches the pane bottom instead of continuing until
  three lines remain.
- The bundled SQLite moves from 3.50.2 to 3.53.2, through rusqlite 0.40. Measured over 52 paired
  benchmark cases on a generated fixture, every required result digest matched, and peak memory
  moved between -10% and +2.6%, the largest drop being the terminal UI's startup.

### Added

- `?` shows every TUI command and the keys bound to it, built from the same table the key
  handler dispatches through, so a rebinding changes what it teaches and an unbound command is
  not listed. The status bar is one row and sheds most hints at eighty columns, which left
  paging, scrolling, top and bottom, and resume named nowhere. The list scrolls with the preview
  scroll keys; any other key closes it.
- The TUI search box is a text field rather than an append-only line. `cursor_left`,
  `cursor_right`, `cursor_start` (Home, Ctrl+A), and `cursor_end` (End, Ctrl+E) move the caret,
  `delete_backward`, `delete_forward`, `delete_word_backward` (Ctrl+W), and `clear_query`
  (Ctrl+U) remove text, and a typed character is inserted where the caret sits. Before this,
  Left, Right, Home, End, and Delete did nothing, and fixing a typo mid-query meant deleting
  back to it. The caret is the terminal's own rather than a drawn block, and a query wider than
  the box scrolls under it.
- `[ui].unicode` and `[ui].color` (`auto`, `on`, `off`) decide whether the TUI draws characters
  outside ASCII and whether it colours anything. `auto` falls back to ASCII borders, `...`, and
  `--` section rules only when `LC_ALL`, `LC_CTYPE`, or `LANG` names an encoding that cannot
  carry the originals, and drops colour under `NO_COLOR` or `TERM=dumb`. The selected row now
  carries a marker rather than relying on colour, so it stays visible either way.
- `[ui.keys]` binds each TUI command to the key presses that reach it: `interrupt`, `quit`,
  `enter_search`, `leave_search`, `move_down`, `move_up`, `page_down`, `page_up`, `top`,
  `bottom`, `cycle_provider`, `cycle_session_kind`, `cycle_time_window`,
  `toggle_warnings_only`, `preview_scroll_down`, `preview_scroll_up`, `preview_page_down`,
  `preview_page_up`, and `resume`. A table names only the actions it changes and the rest keep
  their defaults; `[]` unbinds one. A binding is a character or a named key, optionally chorded
  with `ctrl`, `alt`, `shift`, or `super`. One chord may not mean two things reachable from the
  same mode, and `quit` and `interrupt` may not both be unbound. The status bar names whatever
  is bound rather than the shipped defaults.
- `[ui].search_debounce_ms` (150): how long an edited TUI query must stay unchanged before it
  becomes a search. Typing never waits on it — the keystroke echoes on its own loop turn and the
  search runs off the input thread either way — so this only decides how many searches a typed
  word starts. Previously every keystroke began a corpus scan that the next keystroke cancelled;
  on a 36.5 GB index, where one session search costs 2.3 to 3.6 seconds, typing a five-letter
  word began five. `0` restores the per-keystroke behavior, and Enter searches the current query
  at once whatever the value.
- `[ui]` keys with typed defaults: `event_poll_interval_ms` (150), `list_page_step` (10),
  `preview_scroll_step` (5), `preview_page_step` (15), `provider_label_width` (9, clamped up
  to the longest provider label), `list_pane_percent` (45) — the TUI reads each one, and
  `config.example.toml` documents them beside their typed defaults. This pre-1.0 additive public
  struct change requires external Rust literals to use `UiConfig { preview_lines, ..Default::default() }`;
  the compile-only downstream consumer pins that supported construction pattern.
- The TUI gains session filter bindings: `p` cycles the provider, `f` the session class, `s`
  the time window (1/7/30 days), and `w` warnings-only. Every binding validates before the
  search runs, appears with its active value in the status bar, and mutates the same canonical
  `SearchFilters` type and validation rules used by CLI, MCP, and Python callers.

### Fixed

- The TUI preview pane's title names the visible rows and the total when the transcript is
  longer than the pane, so a reader can tell that scrolling would do something. Content that
  fits keeps the plain title.
- Ctrl+C exits `aise tui`. A full-screen terminal application turns off the terminal's own
  interrupt character, so Ctrl+C arrived as an ordinary key press and nothing handled it: the
  browser ignored it and the search box typed a literal `c` into the query, leaving `q` and Esc
  as the only ways out. The first press arms and says so in the status bar, the second quits,
  and any other key disarms.
- A chorded key no longer fires the binding for its bare letter in `aise tui`. Every command
  matched the character alone, so Ctrl+Q quit, Ctrl+S moved the time window, Ctrl+P changed the
  provider, and Ctrl+H — ASCII backspace on many terminals — scrolled the preview.
- The TUI status bar sheds whole hints on a narrow frame instead of eliding characters out of
  the middle of the joined line. At 80 columns it rendered `P…rs` where `p/f/s/w: filters`
  belonged; it now keeps `j/k: move`, `p/f/s/w: filters`, `/: search`, and `q: quit` readable,
  dropping the paging and scroll hints first.
- A TUI preview re-applied for the row already on screen keeps the reader's scroll position.
  The worker counts logical lines, so clamping against its count pulled a reader out of a
  word-wrapped tail that only the renderer's row count can measure.
- TUI preview metadata and canonical transcript are read in one SQLite snapshot; word-wrapped
  scroll bounds use Ratatui's own line composer, wide/newline errors remain one display-width-bounded
  row, and a current preview failure cannot leave another session's content beside the selection.
- TUI worker failure renders `stopped` rather than `ready`; returning to an already-rendered
  preview invalidates errors from an overtaken preview, and new navigation cancels obsolete
  preview scans without cancelling an in-flight search. The worker accepts every schema generation
  the shared read contract declares readable and gives upgrade guidance for newer indexes.
- TUI echo frames format only terminal-visible session rows rather than every retained result.
  Preview bookends now scan the canonical transcript used by CLI `show`, MCP `get_session`, and
  export without cloning it or retaining every turn; this keeps provider harness notices and
  generated mixed-content parts out of prompts, restores Session/CWD metadata, and preserves
  transcript-only readable indexes. Search and preview scans observe typed cancellation.
- The TUI worker retains at most one pending search and one pending preview instead of every
  cumulative pasted prefix; a newer search cancels the in-flight one without blocking input, and
  a failed search preserves the latest navigation preview. Its read-only SQLite connection now
  shares the caller's Rayon runtime, keeping the configured scoring-worker budget process-wide.
- TUI responses and errors now carry allocation-backed request generations, so an old search
  with the same query but different provider/class/window filters cannot overwrite current state.
  The list title exposes when the current generation is searching; a worker panic reports once
  without requiring another key. Worker/database resources are released before the resume prompt
  or resumed process, and terminal mode is entered only after worker startup succeeds.
- Zero-valued `[ui]` preview/pacing/step/label settings and pane percentages outside 10–90 are rejected
  instead of becoming silent no-ops or impossible geometry. Active worker
  output is checked within 10 ms, while a settled TUI performs one configured idle wait instead
  of waking 100 times per second. Extreme page/scroll steps saturate, and provider-label width is
  bounded by the rendered pane instead of allocating the configured width blindly.
- Typing in `aise tui` no longer runs the search on the input thread: each keystroke renders
  immediately, searches run on a worker thread through the same `CatalogService` seam as the
  CLI, MCP, and Python surfaces, and a superseded search is cancelled instead of running to
  completion while a newer one waits. Caseless matching, snippet compaction, and transcript preview
  copies check cancellation every 64 KiB, including one record above the 8 MiB batch target. The
  previous results stay on screen until new ones arrive, and preview resolves off the input thread.
- The TUI's provider labels no longer collide or misalign: Antigravity renders as ANTIGRAV
  and Gemini CLI as GEMINICLI (the old GEMINI/Gemini pair were near-identical, and AI Studio
  overflowed its fixed-width column), with the column width configurable and clamped up to
  the longest label. The help and error lines middle-elide to the frame width, so the
  recovery guidance at the end of an error always survives, and the session list title names
  the active ordering (recent vs ranked).
- The TUI selection and preview scroll survive typing: a result set that still contains the
  selected session keeps the user's place instead of resetting to the first row.
- Typing in `aise tui` no longer risks freezing the event loop on one keystroke: queued input
  drains in one loop turn, and a database error during a keystroke shows on a dedicated error
  line instead of exiting the TUI.
- The registered TUI latency benchmark now hashes every canonical ordered session ID and checks
  the rendered total, rather than treating visible labels/ages as semantics. It imports on Windows
  while failing PTY execution with a POSIX-specific message, handles repeated final characters,
  measures `/` mode entry separately from typed echo, buffers split terminal controls, fails closed
  on stopped/incomplete searches, sums process-tree resources, and requires current-generation
  readiness plus a stable frame. Release samples promote the inner TUI-only wall/CPU/RSS/thread/
  process measurements instead of timing semantic probes and helper processes. Its generated
  workload has 128 sessions, selective/empty/full queries, offscreen traversal, and one transcript
  above the 8 MiB scoring-batch target.
- A tool call that cannot arm its own cancellation now says so. It previously ran uncancellable
  while the client believed its cancellation still applied.
- A `query_session_index` call whose read-only restriction fails to install is refused rather than
  run without it.

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

[Unreleased]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc2...HEAD
[1.0.0rc2]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc1...v1.0.0rc2
[1.0.0rc1]: https://github.com/ahundt/ai-session-search/releases/tag/v1.0.0rc1
