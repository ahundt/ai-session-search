# AI Session Search (`aise`)

Search, inspect, recover, export, and analyze local AI coding-agent sessions.

AI Session Search indexes Claude Code, Claude Desktop, Codex, Cursor,
Antigravity, Pi, Google AI Studio, and Gemini CLI sessions through one Rust
service. The same typed services back the Rust library, the `aise` CLI, the MCP
server, and the Python API.

## Design

- One executable: `aise`, including `aise mcp serve`.
- One index lifecycle and provider registry across every interface.
- Bounded session-level and MCP result pages by default so clients are not
  flooded; message search documents its explicit unlimited default.
- Explicit `--limit 0` semantics are stated per operation rather than guessed.
- Indexed filtering by provider, session, path, date, role, message kind, tool,
  sequence, and canonical tool-argument JSON pointer.
- Streaming file reconstruction and collision-safe restore.
- Immutable, checksummed, no-overwrite export and analysis bundles.
- Rust-owned parsing, querying, migration, and filesystem publication. Python
  does not maintain a second scanner or policy implementation.

## Install

### Python-distributed CLI and library

```bash
# Isolated command installation
uv tool install ai-session-search

# Run without a persistent installation
uvx --from ai-session-search aise --help

# Add the importable Python package to a project
uv add ai-session-search

# Standard Python installation
python -m pip install ai-session-search
```

All four paths install the same native extension and expose the `aise` command.
Wheels support GIL-enabled CPython 3.12 through 3.14 on manylinux2014
x86_64/aarch64, macOS x86_64/arm64, and Windows x86_64. Git and source
installations require Rust 1.85 or newer and a C linker for the target platform.

### Rust CLI and library

```bash
# From a registry release
cargo install ai-session-search

# From a checkout
cargo install --path rust/ai-session-search-core
```

Native release archives also contain a platform installer. It refuses to
replace an existing executable unless replacement and a rollback destination
are both explicit.

## Quick start

```bash
# Discover and index enabled session sources
aise reindex

# List recent sessions and search by relevance
aise list
aise search "database migration" --provider codex

# Search individual turns and inspect compact evidence
aise messages search "permission denied" --role user
aise messages evidence SESSION_ID

# Read a bounded transcript and print its native resume command
aise show SESSION_ID
aise resume SESSION_ID

# Inspect health and effective filesystem paths
aise doctor
aise paths
```

Run `aise COMMAND --help` for the authoritative parameters and defaults. For
limits, omission uses the displayed bounded default and explicit `0` means
unlimited only where that command's help says so. Date
bounds accept ISO, EDTF, durations, and supported natural-language forms; use
`aise dates` for the complete reference.

## Primary CLI surfaces

| Surface | Purpose |
| --- | --- |
| `aise list`, `aise search`, `aise show` | Find and read sessions |
| `aise messages search|get|timeline|evidence` | Query normalized conversation turns and tool evidence |
| `aise files search|history|cross-ref|extract` | Locate and reconstruct edited files |
| `aise corrections`, `aise planning`, `aise stats` | Query indexed behavioral summaries |
| `aise vocab`, `aise repeats` | Inspect indexed terms and recurring phrases |
| `aise export` | Render one session or publish an explicitly selected bundle |
| `aise analyze` | Apply a validated policy and publish an immutable analysis bundle |
| `aise reindex`, `aise compact`, `aise doctor` | Maintain and diagnose the index |
| `aise migrate database|config|verify` | Perform verified, reversible migration |
| `aise config path|example|init|show` | Inspect or initialize TOML configuration |
| `aise mcp serve|install|status|uninstall|recover` | Run, register, inspect, remove, or recover MCP client configuration |
| `aise db` | Execute expert read-only SQL against the index |
| `aise tui` | Browse sessions interactively |

### Composable search

```bash
# Provider, path, and time scopes compose
aise list --provider claude-desktop --path ~/source/project --since 7d

# Search user turns while excluding compaction summaries
aise messages search "regression" --role user --no-compaction

# Search normalized tool calls and a canonical argument field
aise messages search "Cargo.toml" \
  --field tool-argument \
  --argument-path /path \
  --tool Edit

# Recover all causally valid versions as a lossless stream
aise files extract path/to/file.rs --all --format jsonl
```

Session-level and MCP defaults are intentionally bounded. Message search
currently documents `0 = unlimited`; pass a positive `--limit` when its output
feeds a bounded consumer. Elsewhere, pass zero only when command help explicitly
defines zero as the complete selected corpus. Internal keyset batching never
changes which results an operation returns.

### Immutable export and analysis

```bash
# One full session to stdout
aise export SESSION_ID --format markdown

# Publish a selected session bundle into a new directory
aise export --since 7d --output-dir ./week

# Analyze the complete selected corpus and publish JSON plus Markdown
aise analyze --limit 0 --output ./analysis

# Apply a validated provider-neutral policy
aise analyze --policy ./analysis-policy.json --output ./policy-analysis
```

Bundle destinations must not already exist. AI Session Search stages, syncs,
and atomically publishes a complete directory, then returns a receipt containing
the artifact metadata. It never silently merges into or replaces an existing
bundle.

## MCP

The MCP transport is a subcommand of the same executable:

```bash
aise mcp install
aise mcp status

# Only when install/uninstall reports an interrupted transaction
aise mcp recover

# Direct stdio use by an MCP client
aise mcp serve
```

Generated client configuration uses the portable command name and argument
array, not a machine-specific absolute executable path. MCP tools remain
read-only and bounded; filesystem publication is a CLI/library operation rather
than an MCP side effect. Input objects are closed schemas and are validated before
the index is opened or refreshed, so misspelled fields and invalid types fail with
the exact argument path instead of being ignored. Tools returning structured data
declare object output schemas; text-only tools use standard MCP text content.

Install and uninstall preflight every selected client and instruction file, then
write a private durable receipt before the first change. A handled later-file
failure restores earlier files. An interruption or concurrent edit preserves the
receipt and prints the exact `aise mcp recover --transaction-receipt PATH` command;
recovery changes only files that still match a recorded before/after image. `mcp
status` refuses to describe a partial transaction as authoritative. The default
receipt is beside the selected AI Session Search config file; override it consistently
with `--transaction-receipt PATH` on install, status, uninstall, and recover.

## Python API

`SessionSearch` is the Python lifecycle root. Query objects are immutable typed
conversions over the public Rust request types.

```python
import ai_session_search as aise

search = aise.SessionSearch()
search.refresh()

scope = aise.QueryScope(
    provider="codex",
    path_prefix="/path/to/project",
    dates=aise.DateRangeQuery(when="7d"),
)

sessions = search.list_sessions(
    aise.SessionQuery(provider="codex", limit=20),
)
messages = search.search_messages(
    "authentication",
    aise.MessageQuery(
        scope=scope,
        selector=aise.MessageSelector(role="user", no_compaction=True),
        limit=50,
    ),
)
files = search.search_files(
    "*.py",
    aise.FileQueryRequest(scope=scope, min_edits=3, limit=50),
)

if sessions:
    evidence = search.inspect_session(
        sessions[0].id,
        include_time_profile=True,
    )
    markdown = search.export_session(sessions[0].id, "markdown")

status = search.index_status()
```

Long native operations release the GIL. Reconstruction iterators own their
selected rows without retaining the database lock, and publication is explicit.
Detailed result classes are available from `ai_session_search.native`.

## Rust API

The `ai-session-search` crate exposes provider-neutral services without Clap,
MCP, SQLite row, or PyO3 types in its public contracts.

```rust,no_run
use ai_session_search::models::SearchFilters;
use ai_session_search::service::SessionSearch;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = SessionSearch::load()?;
    let sessions = app.catalog().list_sessions(&SearchFilters {
        provider: None,
        path_prefix: None,
        exclude_path_prefixes: Vec::new(),
        exclude_session_ids: Vec::new(),
        since: None,
        until: None,
        limit: 20,
        warnings_only: false,
    })?;
    let status = app.index().status()?;
    println!("{} sessions; {} repairs", sessions.len(), status.repair_commands.len());
    Ok(())
}
```

The crate documents operation ordering, allocation, pagination, stale-index
semantics, and error behavior. Filesystem writes use explicit restore or
publication plans rather than implicit destinations.

## Configuration and paths

Configuration is TOML and follows platform-standard directories. Do not embed a
home directory or toolchain path in client configuration.

```bash
aise config path
aise config example
aise config init
aise config show
aise paths
```

Portable overrides are available for automation:

| Variable | Purpose |
| --- | --- |
| `AI_SESSION_SEARCH_CONFIG` | Explicit TOML configuration file |
| `AI_SESSION_SEARCH_DATABASE` | Explicit SQLite index file |
| `AI_SESSION_SEARCH_CACHE_DIR` | Explicit disposable cache directory |
| `AI_SESSION_SEARCH_THREADS` | Explicit positive worker-thread count |

Precedence is CLI/API argument, then canonical environment variable, then TOML,
then typed/platform default. Run `aise config explain` to see the selected source
for every durable setting.

Canonical session-source IDs are:

| Session source | Provider ID | Native resume |
| --- | --- | --- |
| Claude Code | `claude` | yes |
| Claude Desktop local agent | `claude-desktop` | no; use show/export guidance |
| Codex | `codex` | yes |
| Cursor | `cursor` | no; use show/export guidance |
| Antigravity | `antigravity` | no; use show/export guidance |
| Pi coding agent | `pi` | yes |
| Google AI Studio | `aistudio` | no; use show/export guidance |
| Gemini CLI | `gemini-cli` | no; use show/export guidance |

Provider tables use these IDs as TOML keys. `aise paths` prints the effective
roots and discovery status for every source.

The configuration schema and provider registry are shared by CLI, MCP, Rust,
and Python. Existing legacy data can be imported through `aise migrate config`;
runtime code does not maintain a second JSON configuration system.

## Safe database migration

Never copy a live SQLite database file without its WAL state. Use the migration
service:

```bash
aise migrate database --help
aise migrate verify --help
```

Migration uses SQLite online backup, verifies integrity and row manifests,
preserves rollback evidence, and atomically publishes the destination. An
interrupted prepared migration can be recovered from its durable receipt; a
conflicting destination is never overwritten.

## Development

```bash
uv lock --check
uv sync --locked --all-extras

RUSTC_WRAPPER= cargo test --workspace --all-features
RUSTC_WRAPPER= cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTC_WRAPPER= uv run pytest
RUSTC_WRAPPER= uv run ruff check .
RUSTC_WRAPPER= uv run mypy ai_session_search tests
RUSTC_WRAPPER= uv run python -m mypy.stubtest ai_session_search --concise --ignore-disjoint-bases
```

Release gates build wheels, an sdist, Cargo packages, and native archives from
locked dependency graphs. Exact artifacts are installed and smoke-tested rather
than rebuilt independently during verification.

## License

AI Session Search is licensed under Apache License 2.0. Compatible third-party
dependencies retain their own licenses; release artifacts include dependency
inventories and software bills of materials.
