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

`aise mcp serve` receives the already resolved CLI configuration. `aise mcp install` continues to
write the portable command `aise mcp serve`; use client-provided environment settings when a
persistent MCP installation needs non-default paths.

## Maintainer checks

Configuration changes require focused tests for all four precedence levels, invalid canonical
environment values, canonical/legacy conflicts, omitted versus explicit-empty provider paths,
unknown TOML keys, CLI/MCP/Python parity, and effective-config provenance. Tests must inject
`ConfigEnvironment`; do not mutate process environment in parallel Rust tests.

The embedded `config.example.toml` is documentation, not a second runtime-default source. Typed
Rust defaults remain canonical, and the example contract test must compare each uncommented
tunable with those typed defaults.

## Outstanding atomic-init constraint

`aise config init --force` predates the shared RAII directory publication layer and still writes a
single file directly. Do not claim crash-safe or symlink-safe replacement until `durable_fs` exposes
a reusable atomic-file transaction with create-new and explicit replacement modes, file and parent
sync, symlink refusal, and drop cleanup. Duplicating that state machine in the CLI would violate DRY.
