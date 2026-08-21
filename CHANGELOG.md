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

## [1.0.0rc2] - 2026-08-19

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

- Published wheels no longer name the machine that built them. The embedded CycloneDX SBOM records
  workspace crates by relative path, and artifact verification rejects a wheel that still carries a
  build directory.
- Wheels build reproducibly from a pinned `SOURCE_DATE_EPOCH`, and artifact verification proves the
  build received it.
- Build provenance attestation runs only for a real tag push.

## [1.0.0rc1] - 2026-08-11

First published release. See the [tag](https://github.com/ahundt/ai-session-search/releases/tag/v1.0.0rc1).

[Unreleased]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc2...HEAD
[1.0.0rc2]: https://github.com/ahundt/ai-session-search/compare/v1.0.0rc1...v1.0.0rc2
[1.0.0rc1]: https://github.com/ahundt/ai-session-search/releases/tag/v1.0.0rc1
