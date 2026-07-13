# Configuration lifecycle

`aise` resolves durable settings once and passes the resulting typed Rust `Config` to the CLI,
MCP server, and Python service. Keep query filters, output formats, pagination, and migration
source/destination arguments invocation-local; they are not persistent configuration.

## Precedence

From highest to lowest precedence:

1. Explicit CLI or API argument.
2. Canonical `AI_SESSION_SEARCH_*` environment variable.
3. TOML configuration value.
4. Typed Rust and platform default.

The global CLI flags are `--config`, `--database`, `--cache-dir`, and `--threads`. Their canonical
environment equivalents are `AI_SESSION_SEARCH_CONFIG`, `AI_SESSION_SEARCH_DATABASE`,
`AI_SESSION_SEARCH_CACHE_DIR`, and `AI_SESSION_SEARCH_THREADS`. The old `AISE_THREADS` variable is
accepted only below `AI_SESSION_SEARCH_THREADS` and emits a deprecation diagnostic. Conflicting
canonical and legacy thread variables never make the legacy value win.

Run `aise config show` to print the merged effective configuration. Run `aise config explain` to
print the selected source for the config file, database, cache directory, and thread setting.
Unknown TOML keys and invalid bounded values fail during resolution rather than being silently
ignored or normalized later.

Provider `paths` has deliberate three-state behavior:

- Omitted provider table or omitted `paths`: use platform defaults.
- Explicit `paths = []`: search no roots for that provider.
- Explicit nonempty `paths`: replace platform roots with exactly those paths.

Set `enabled = false` when the provider itself should be disabled.

## Rust, Python, and MCP

Rust embedders should construct `ConfigOverrides` and call `Config::resolve`, then retain the
returned `ResolvedConfig` for diagnostics and provenance. `SessionSearch::open` and
`McpServer::new` accept the resolved typed configuration without rereading process state.

Python's `SessionSearch` accepts `db_path`, `config_path`, `cache_dir`, and `threads`; explicit
arguments use the same precedence as CLI flags. Rayon is process-global in this release. The first
Python `SessionSearch` initializes it, repeated instances may reuse the same size, and a conflicting
later size fails with the existing and requested values instead of silently ignoring configuration.

`aise mcp serve` receives the already resolved CLI configuration. `aise mcp install` writes the
portable command `aise mcp serve`; use client-provided environment settings when a persistent MCP
installation needs non-default paths. Install, status, uninstall, and recover derive their default
transaction receipt from the selected config path, so global `--config` and
`AI_SESSION_SEARCH_CONFIG` select the same recovery namespace without loading the session index.

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
confirmation failure preserves evidence and prints one recovery command; recovery never overwrites
content that differs from both recorded images. Successful publication or complete rollback removes
the receipt. The adjacent advisory lock file remains because deleting a lock pathname can split
concurrent waiters across different inodes.

The implementation retains the shared `durable_fs` primitive rather than adding a second atomic-file
crate. Rust's [`OsStrExt`](https://doc.rust-lang.org/std/os/unix/ffi/trait.OsStrExt.html) defines the
lossless Unix byte representation used in receipts. `tempfile::NamedTempFile::persist_noclobber`
provides no-clobber publication but explicitly does not sync file contents or the containing
directory, so it cannot replace the durability layer without extra policy:
[`tempfile` documentation](https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist_noclobber).
The [`os_str_bytes` documentation](https://docs.rs/os_str_bytes/latest/os_str_bytes/) says its
platform encoding may change and should not be used for storage, while
[`camino::Utf8PathBuf`](https://docs.rs/camino/latest/camino/struct.Utf8PathBuf.html) rejects
non-Unicode paths. Neither is a safer receipt format than explicit platform-native units.
