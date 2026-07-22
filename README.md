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
  flooded; exact/regex message search can be explicitly unlimited, while fuzzy
  search requires a finite page.
- Explicit `--limit 0` semantics are stated per operation rather than guessed.
- Indexed filtering by provider, session, path, date, role, message kind, tool,
  sequence, and canonical tool-argument JSON pointer.
- Streaming file reconstruction and collision-safe restore.
- Immutable, checksummed, no-overwrite export and analysis bundles.
- Rust-owned parsing, querying, migration, and filesystem publication. Python
  does not maintain a second scanner or policy implementation.

## Naming

Use dashes for product slugs, repositories, package-manager names, skills, and
MCP server identities. Use underscores only where programming-language or
environment-variable identifiers require them.

| Surface | Canonical name |
| --- | --- |
| Product name | **AI Session Search** |
| GitHub repository | `ai-session-search` |
| PyPI distribution | `ai-session-search` |
| crates.io package | `ai-session-search` |
| Python import | `ai_session_search` |
| Rust crate path | `ai_session_search` |
| MCP server key and protocol name | `ai-session-search` |
| MCP protocol title | **AI Session Search** |
| Primary executable | `aise` |
| Descriptive executable aliases | `aisearch`, `ai_session_search` |

`aise install` creates the executable aliases; package installation alone does
not create them. Reinstall migrates the historical `ai_session_search` and
`aise` MCP keys to `ai-session-search` without retaining duplicate servers.

The PyPI distribution `ai-session-search` supersedes the retired
`ai_session_tools` package (last published as `0.3.1`, single-user Python
implementation). `ai_session_tools` receives no further releases; install
`ai-session-search` instead.

## Install

### Python-distributed CLI and library

Choose either `uv tool` here or Cargo/native installation below as the global
`aise` command owner. A project dependency may coexist, but installing two
global commands makes the selected executable depend on PATH order.

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
installations require Rust 1.88 or newer and a C linker for the target platform.
Package installation never creates command aliases or edits MCP client configuration,
instruction files, skills, or hooks; `aise install` is the shared explicit step that creates
relative `aisearch -> aise` and `ai_session_search -> aise` links and configures detected
clients. Pass `--no-aliases` when symbolic links are unavailable or unwanted.
Pass `--no-mcp`, `--no-instructions`, or `--no-skill` to omit that integration; the default
configures aliases, MCP, concise global guidance, and the full `$ai-session-search` skill.

For the recommended CLI plus detected-client setup in one fail-fast shell
command:

```bash
uv tool install ai-session-search && aise install
```

### Rust CLI and library

```bash
# From a registry release
cargo install ai-session-search --locked

# From a checkout
cargo install --path rust/ai-session-search-core
```

The equivalent Cargo setup is:

```bash
cargo install ai-session-search --locked && aise install
```

Native release archives also contain a platform installer. It refuses to
replace an existing executable unless replacement and a rollback destination
are both explicit.

### Remove an installation

Remove MCP configuration before removing the selected global command owner.
The MCP command removes only aise-owned entries and managed guidance; package
manager removal does not delete indexes, configuration, or session data.

```bash
aise uninstall
uv tool uninstall ai-session-search    # uv-owned global command
uv remove ai-session-search            # project dependency
python -m pip uninstall ai-session-search
cargo uninstall ai-session-search      # Cargo-owned global command
```

`aise uninstall` removes all owned integrations by default while preserving the
`aise` executable, index, cache, configuration, and source sessions. Use
`--keep-mcp`, `--keep-aliases`, `--keep-instructions`, or `--keep-skill` to retain one component.

For source installs, custom destinations, upgrades, integration selection, and
recovery, follow the [installation guide](docs/development/installation.md).

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
aise messages evidence SESSION_ID --summary-items -12  # last 12 aggregate records (default)
aise messages evidence SESSION_ID --summary-items 0 --format json  # all evidence for a pipeline

# Read a bounded transcript and print its native resume command
aise show SESSION_ID
aise show SESSION_ID --summary --summary-items -12
aise show SESSION_ID --transcript-lines -80   # last 80 transcript lines; 0 = entire session
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
| `aise migrate database|config|verify|recover` | Perform verified, reversible migration |
| `aise config path|example|init|show|explain` | Inspect or initialize TOML configuration |
| `aise install|status|uninstall`; `aise mcp serve|recover` | Manage executable aliases, MCP registrations, owned instructions and skills, serving, and transaction recovery |
| `aise db` | Execute expert read-only SQL against the index |
| `aise tui` | Browse and fuzzy-search session-level records interactively; message-field modes remain in `aise messages search` |

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

# Inspect indexed candidate selectivity without mixing diagnostics into stdout
aise messages search "database lock" --fuzzy --limit 20 --explain --format json

# Cap every returned message at its first 5 lines (negative keeps the tail)
aise messages get SESSION_ID --role user --lines-per-message 5

# Read the 75 most recent user messages (order SELECTS which N; result is still oldest-first).
# Direction is --order, never a negative --limit.
aise messages get SESSION_ID --role user --limit 75 --order newest

# Read a long session in non-overlapping chunks: advance --seq-from instead of growing --limit,
# which would re-send everything you already read.
aise messages get SESSION_ID --seq-from 0 --seq-to 499
aise messages get SESSION_ID --seq-from 500 --seq-to 999

# Recover all causally valid versions as a lossless stream
aise files extract path/to/file.rs --all --format jsonl
```

Session-level and MCP defaults are intentionally bounded. Exact and regex
message search define `0 = unlimited`; fuzzy message search requires a positive
limit and `offset + limit <= 10,000`. Elsewhere, pass zero only when command help
explicitly defines zero as the complete selected corpus. Internal keyset
batching never changes which results an operation returns.

Exact and regex modes verify the requested predicate after any indexed prefilter, so the prefilter
does not change their result set. Fuzzy mode is deliberately bounded approximate retrieval. With
`--explain`, CLI diagnostics go to stderr while stdout keeps the selected text/JSON format; the MCP
response instead returns the same structured planner receipt. A true
`candidate_source_saturated` value means an indexed fuzzy candidate source reached its admission
budget: narrow provider, path, session, role, kind, tool, or date filters when better recall is
required. It does not mean the trigram prefilter was skipped.

Two line windows share one sign convention: `aise show --transcript-lines`
windows the whole rendered transcript, and `--lines-per-message` on
`aise messages` commands caps each returned message individually. Positive
values keep the first N lines, negative values keep the last N, and `0` keeps
everything. The same scopes exist as `[cli]`/`[mcp]` configuration keys and as
MCP tool parameters. Per-message windows change presentation only: they never
change matches, ranking, result count, pagination, context membership, or
reference extraction, so they can make a large result page skimmable without
silently discarding hits.

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
aise install
aise status
aise uninstall

# Only when install/uninstall reports an interrupted transaction
aise mcp recover

# Direct stdio use by an MCP client
aise mcp serve
```

The installer supports Claude Code and Claude Desktop, Codex, Gemini CLI,
Antigravity, Cursor, Windsurf, VS Code, Zed, OpenCode, OpenClaw, and the legacy
KiloCode VS Code extension. It writes each client's native JSON/TOML shape.
Claude receives `CLAUDE.md` guidance; Codex and OpenCode receive a managed
`AGENTS.md` block; Gemini and Antigravity share one managed block in
`~/.gemini/GEMINI.md`. Other clients receive only MCP configuration. This
repository does not install client hooks.

Omitting `--client` updates detected clients. Repeat `--client CLIENT` to
create an explicit include set and `--exclude-client CLIENT` to subtract from
it. Typed custom destinations cover common JSON (`--json-mcp-config`), VS Code
(`--vscode-config`), Zed (`--zed-config`), OpenCode (`--opencode-config`),
Codex TOML (`--codex-config`), Claude (`--claude-md`), Gemini/Antigravity
(`--gemini-md`), and AGENTS.md (`--agents-md`) formats. Install, status, and
uninstall share this exact selector schema.

Generated client configuration uses the portable `aise` command name and
argument array by default, not a machine-specific absolute executable path.
Pass `--binary PATH` only when a GUI client cannot resolve `aise` from its own
PATH. The Kilo selector currently targets the legacy VS Code extension storage;
it does not modify the current standalone Kilo `~/.config/kilo/kilo.jsonc` file.
MCP tools remain
read-only and bounded; filesystem publication is a CLI/library operation rather
than an MCP side effect. Input objects are closed schemas and are validated before
the index is opened or refreshed, so misspelled fields and invalid types fail with
the exact argument path instead of being ignored. Tools returning structured data
declare object output schemas; text-only tools use standard MCP text content.

Install and uninstall preflight every selected client and instruction file, then
write a private durable receipt before the first change. A handled later-file
failure restores earlier files. An interruption or concurrent edit preserves the
receipt and prints the platform-independent `aise` argv plus the exact receipt path;
recovery changes only files that still match a recorded before/after image. `aise
status` holds the transaction's shared RAII lock while reading the receipt and every
target, so it cannot combine different installer generations. The default
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
    dates=aise.DateRange(when="7d"),
)

sessions = search.list_sessions(
    aise.SessionQuery(provider="codex", limit=20),
)
messages = search.search_messages(
    "authentication",
    aise.MessageQuery(
        scope=scope,
        role="user",
        no_compaction=True,
        limit=50,
    ),
)
files = search.search_files(
    "*.py",
    aise.FileQuery(scope=scope, min_edits=3, limit=50),
)
history_page = search.file_history(
    "src/app.py",
    aise.FileQuery(scope=scope, limit=50, offset=0),
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

Search remains unrestricted by default. An opt-in `[search.scope]` panel can restrict
session, message, analysis, file-history, export, resume, and exact-ID reads to configured
absolute roots, the invocation directory, and live MCP client roots. Restricted mode fails
closed without authority and disables arbitrary content SQL. See
[configuration lifecycle](docs/development/configuration.md#search-access-scope).

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

`aise doctor` table output reports exact allocated and reclaimable bytes. If
reclaimable pages exist, it names `aise compact` and warns that compaction needs
an exclusive database lock and temporary disk space. `aise compact` reports
exact bytes plus binary `MiB` units and preserves the public Rust/Python
`CompactOutcome` shape. Migration does not silently vacuum the source or change
latency/peak-space behavior; compact a published destination explicitly when
the diagnostic shows that the tradeoff is appropriate.

## Development

The [documentation index](docs/README.md) separates user workflows from
maintainer and migration references. Start with the
[configuration guide](docs/development/configuration.md) for runtime settings
or the [release guide](docs/development/releasing.md) for
package preparation and publication.

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
