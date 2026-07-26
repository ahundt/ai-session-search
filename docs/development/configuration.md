# Configuration lifecycle

`aise` resolves durable settings once and passes the resulting typed Rust `Config` to the CLI,
MCP server, and Python service. Keep query filters, output formats, pagination, and migration
source/destination arguments invocation-local; they are not persistent configuration.

## Configure in four steps

1. Locate the effective file with `aise config file` and inspect the complete
   template with `aise config example`.
2. Run `aise config init` if no file exists. It refuses to overwrite an
   existing entry; use `--force` only after reviewing the replacement.
3. Edit only durable source paths and runtime settings. Keep query filters,
   output formatting, and migration destinations on the command that uses them.
4. Run `aise config show`, `aise config origins`, and `aise doctor`. The first
   command prints merged values, the second identifies each winning source,
   and the third reports invalid or inaccessible runtime paths.

For automation, set a canonical `AI_SESSION_SEARCH_*` variable or pass the
corresponding CLI/API argument rather than generating machine-specific TOML.

## Precedence

From highest to lowest precedence:

1. Explicit CLI or API argument.
2. Canonical `AI_SESSION_SEARCH_*` environment variable.
3. TOML configuration value.
4. Typed Rust and platform default.

The root CLI options are `--config`, `--database`, `--cache-dir`, `--threads`, and
`--index-refresh`. Their canonical environment equivalents are `AI_SESSION_SEARCH_CONFIG`,
`AI_SESSION_SEARCH_DATABASE`, `AI_SESSION_SEARCH_CACHE_DIR`, `AI_SESSION_SEARCH_THREADS`, and
`AI_SESSION_SEARCH_INDEX_REFRESH`.

Run `aise config show` to print the merged effective configuration. Run `aise config origins` to
print the selected source for the config file, database, cache directory, worker threads,
refresh policy, and search scope.
Unknown TOML keys and invalid bounded values fail during resolution rather than being silently
ignored or normalized later.

## Stable-release notifications

The `[release_notifications]` panel controls only the optional check after ordinary
interactive CLI output:

```toml
[release_notifications]
enabled = true
minimum_check_interval_hours = 24
request_timeout_ms = 1000
```

The root option `--skip-release-notification` and
`AI_SESSION_SEARCH_SKIP_RELEASE_NOTIFICATION=1` skip that work for one
invocation. The flag and environment variable do not disable explicit
`aise package check` or `aise package update`. MCP stdio, Rust/Python library
calls, noninteractive output, and test-only dispatch do not perform notification
checks. A notification failure is silent and leaves normal command output
unchanged; explicit package commands report failures.

Provider `paths` has deliberate three-state behavior:

- Omitted provider table or omitted `paths`: use platform defaults.
- Explicit `paths = []`: search no roots for that provider.
- Explicit nonempty `paths`: replace platform roots with exactly those paths.

Set `enabled = false` when the provider itself should be disabled.

## Search access scope

`[search.scope] mode = "all"` is the default and preserves unrestricted search behavior. To
restrict every session, message, analysis, file-history, export, resume, and exact-ID read to
authorized workspaces, use `mode = "allowed-roots"` with one or both trusted standalone sources:

```toml
[search.scope]
mode = "allowed-roots"
roots = ["/absolute/project"]
include_invocation_directory = false
```

Configured roots must be nonempty absolute directory paths and cannot be a filesystem root.
`include_invocation_directory = true` adds the process working directory. MCP clients that
advertise roots contribute their live `roots/list` file URIs; a roots-change notification revokes
the previous live roots before replacements are accepted. Session cwd, repository, transcript,
and edited-file values are searchable evidence only and never grant authority. Restricted mode
fails closed when no trusted root remains, exact hidden IDs use the normal no-match response, and
arbitrary `aise db query` or MCP `query_session_index` SQL is disabled because it cannot enforce
the shared predicate. Schema inspection remains available.

`aise config show` prints the configured panel, `aise config origins` reports whether that panel
came from the config file or typed default, and `aise config paths` prints the effective standalone roots,
their canonical targets, and each contributing origin. MCP roots are connection-local and are not
reported by a separate CLI process.

## Index refresh lifecycle

`--index-refresh`, `AI_SESSION_SEARCH_INDEX_REFRESH`, and `[index].refresh` use the same enum:

- `auto` (default) serves any structurally readable index first, flushes the requested output, and
  starts one detached refresh process. The update-lock attempt is nonblocking, so another updater
  never delays the read. A readable older schema generation is upgraded fully in that process even
  when the ordinary content-refresh timestamp is recent.
- `before-query` completes discovery, parser refresh, and any compatible schema backfill before
  running the read. Use it when the result must include source changes made immediately beforehand.
- `existing-only` performs no discovery, update-lock creation, or database writes. It serves a
  readable older generation as-is and reports an actionable error only when the schema cannot be
  queried correctly by the running binary.

Absent databases and structurally unreadable old schemas are repaired synchronously because no
correct snapshot exists to serve. Schema generations newer than the binary fail closed with an
upgrade instruction; the binary never guesses that a future layout is compatible. Parser-derived
data upgrades and archive cleanup complete under one RAII update-lock guard, and schema/freshness
markers are written only after all work succeeds. A crash leaves the prior readable generation and
causes the next `auto` read to retry in the background.

## Rust, Python, and MCP

Rust embedders should construct `ConfigOverrides` and call `Config::resolve`, then retain the
returned `ResolvedConfig` for diagnostics and provenance. `SessionSearch::open` and
`McpServer::new` accept the resolved typed configuration without rereading process state.

Python's `SessionSearch` accepts `db_path`, `config_path`, `cache_dir`, and `threads`; explicit
arguments use the same precedence as CLI flags. Each Rust or Python `SessionSearch` owns a
fixed-size Rayon worker pool with the resolved setting. Independent instances may use different
sizes in one process; dropping an instance closes its database and terminates its workers after
outstanding work. This follows Rayon's documented local-pool lifecycle and avoids its one-time
process-global configuration: [`rayon::ThreadPool`](https://docs.rs/rayon/latest/rayon/struct.ThreadPool.html).

`aise mcp serve` receives the already resolved CLI configuration.
`aise integrations install` writes the
portable command `aise mcp serve`; `--binary PATH` stores an explicit executable only when a GUI
client cannot resolve `aise` from its own PATH. The installer supports Claude Code/Desktop, Codex,
Gemini CLI, Antigravity, Cursor, Windsurf, VS Code, Zed, OpenCode, OpenClaw, and the legacy KiloCode
VS Code extension. Claude, Codex, OpenCode, Gemini, and Antigravity receive managed instruction-file
guidance; Gemini and Antigravity share one sentinel-owned `~/.gemini/GEMINI.md` block. The repository
does not install client hooks. It installs the owned `$ai-session-search` skill for detected
Claude, Codex, and Gemini/Antigravity harnesses unless `--no-skill` is passed. Install, status,
uninstall, and recover derive their default
transaction receipt from the selected config path, so global `--config` and
`AI_SESSION_SEARCH_CONFIG` select the same recovery namespace without loading the session index.
Omitting `--client` selects every detected client. Repeated `--client` values
form an explicit include set; repeated `--exclude-client` values subtract from
that set. Custom destinations are format-specific: `--json-mcp-config`,
`--vscode-config`, `--zed-config`, `--opencode-config`, `--codex-config`,
`--claude-md`, `--gemini-md`, and `--agents-md`. All are repeatable and pass
through the same preflight, transaction, status, recovery, and uninstall path. Exact custom
skill destinations use repeatable `--skill-root DIR` (the directory, not a file inside it);
uninstall preserves them with `--keep-skill`, refuses to remove a directory without the embedded
ownership marker, and preserves the whole directory whenever any file in it differs from what
install recorded.
Instruction status is content-aware: `configured` means the current generated
content is active; `outdated`, `instruction file missing`, `instruction file
modified`, and `orphaned managed file` identify the exact repair or ownership
condition. Install upgrades sentinel-owned or recognized legacy content,
refuses unmanaged imported files before publishing any MCP change, and
normalizes duplicate managed inline blocks to one. Uninstall removes every
managed inline block and only whole files that carry recognized aise ownership.
The same concise text is returned as MCP initialize `instructions`, so clients
that support server instructions receive the workflow even without a Markdown
integration. The first 512 characters are self-contained and contract-tested,
following Codex's current
[MCP guidance](https://learn.chatgpt.com/docs/extend/mcp.md). Markdown remains
necessary for harnesses that do not consume server instructions or need the
guidance before choosing an MCP tool.

## Output windowing defaults

Two distinct line-window scopes exist and must never be conflated: whole-transcript windows
(`[cli] show_transcript_lines` for `aise show`, `[mcp] get_session_transcript_lines` for the MCP
`get_session` transcript view) and per-message caps (`[cli] lines_per_message` for
`aise messages` commands, `[mcp] lines_per_message` for `search_messages` and focused
`get_session` message windows). All four share one sign convention — positive keeps the first N
lines, negative keeps the last N, `0` keeps everything — and per-message caps default to `0`
(uncapped). Per-message windows are presentation-only: they do not change matches, ranking, result
count, pagination, context membership, or reference extraction. This makes a large result page
skimmable without silently discarding hits. `config.example.toml` documents each key beside its
typed Rust default.

Queried message hits also include automatic selected-field `match_evidence`, independent of the
raw head, tail, or full message window. `[search.message-search].match_evidence_max_chars` bounds
each evidence excerpt in Unicode scalar characters; an explicit CLI, MCP, Python, or Rust request
and a purpose preference can override it. The typed default is 220. This bound is applied only
after matching, ranking, page selection, and offset handling, so it cannot hide a better hit or
change `next_offset`. Queryless listings and neighboring context rows have no match evidence.

Compact summaries use the same sign convention through `summary_items`:
positive selects the first N records, negative selects the last N, and `0` explicitly keeps all
records for pipelines. The bounded CLI and MCP default is `-12`. One fair aggregate budget is
shared by user intent, tool activity, references, and changed-file aggregates; message-derived
records follow first/last sequence order, while `changed_files` remains an aggregate ordered by
path and edit count. This is presentation-only and does not change indexing, matching, ranking, or
detailed retrieval. Rust callers use the typed `EvidenceWindow::First | Last | All` API rather
than signed integers; first/last database ordering remains an internal inspection primitive.
Use bounded `search_messages` pages for deterministic non-overlapping detail traversal. CLI
pipelines can request all summary evidence with `--summary-items 0 --format json`.

## Maintainer checks

Configuration changes require focused tests for all four precedence levels, invalid canonical
environment values, canonical/legacy conflicts, omitted versus explicit-empty provider paths,
unknown TOML keys, CLI/MCP/Python parity, and effective-config provenance. Tests must inject
`ConfigEnvironment`; do not mutate process environment in parallel Rust tests.

The embedded `config.example.toml` is documentation, not a second runtime-default source. Typed
Rust defaults remain canonical, and the example contract test must compare each uncommented
tunable with those typed defaults.

## Atomic configuration initialization

`aise config init` uses the shared `durable_fs` staged-file transaction. Create-new mode refuses any
existing entry. `--force` replaces only a regular file, preserves its permissions, rejects symbolic
links and other file types, syncs file and parent directory data, and removes unpublished staging
files on drop. A parent-directory sync error reports that publication already occurred so callers do
not retry blindly.

## MCP client configuration transactions

Cross-directory filesystem updates are not atomic. The MCP installer therefore uses an advisory
lock and a versioned receipt containing exact UTF-8 before/after images and lossless platform path
units. New receipt files use mode `0600` on Unix and the destination directory's inherited ACL on
Windows. It validates every input before the receipt, revalidates each preimage before
publication, atomically publishes each file, and syncs its parent. A handled failure rolls back
unchanged outputs in reverse order. A crash, external edit, unreadable receipt, or durability-
confirmation failure preserves evidence and prints a shell-independent executable/argv description
plus the exact receipt path; recovery never overwrites
content that differs from both recorded images. Successful publication or complete rollback removes
the receipt. The adjacent advisory lock file remains because deleting a lock pathname can split
concurrent waiters across different inodes.

Status holds an `fd_lock` shared guard across the receipt check and every selected config/instruction
read; install, uninstall, and recovery take the corresponding exclusive guard. Codex TOML edits use
`toml_edit::DocumentMut` to remove/replace the semantic `mcp_servers.ai-session-search` item, including quoted and
nested table forms, while preserving unrelated comments and ordering. Generated TOML is reparsed
before the durable transaction can publish it. Executable paths that cannot be represented as UTF-8
are rejected before any JSON or TOML mutation instead of being lossily rewritten.

The implementation retains the shared `durable_fs` primitive rather than adding a second atomic-file
crate. Rust's [`OsStrExt`](https://doc.rust-lang.org/std/os/unix/ffi/trait.OsStrExt.html) defines the
lossless Unix byte representation used in receipts. `tempfile::NamedTempFile::persist_noclobber`
provides no-clobber publication but explicitly does not sync file contents or the containing
directory, so it cannot replace the durability layer without extra policy:
[`tempfile` documentation](https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist_noclobber).
The formatting-preserving TOML API and keyed removal contract are documented by
[`toml_edit::DocumentMut`](https://docs.rs/toml_edit/latest/toml_edit/struct.DocumentMut.html).
The [`os_str_bytes` documentation](https://docs.rs/os_str_bytes/latest/os_str_bytes/) says its
platform encoding may change and should not be used for storage, while
[`camino::Utf8PathBuf`](https://docs.rs/camino/latest/camino/struct.Utf8PathBuf.html) rejects
non-Unicode paths. Neither is a safer receipt format than explicit platform-native units.
