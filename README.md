
# AI Session Search (`aise`)

Ultra-fast rust-based search of every local AI coding session with on-the-fly reindexing. Reads Claude Code, Claude Desktop, ChatGPT Codex, Cursor, Antigravity, Pi, Prime Agent, Google AI Studio, and Gemini CLI, on macOS, Linux, and Windows.

[![PyPI](https://img.shields.io/pypi/v/ai-session-search)](https://pypi.org/project/ai-session-search/)
[![crates.io](https://img.shields.io/crates/v/ai-session-search)](https://crates.io/crates/ai-session-search)
[![CI](https://github.com/ahundt/ai-session-search/actions/workflows/ci.yml/badge.svg)](https://github.com/ahundt/ai-session-search/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

<!-- Demo goes here: drag demo.gif into the GitHub web editor, which uploads it and
     inserts a user-attachments image link in place of this comment. The recording
     itself is untracked; regenerate it with uv run python tests/test_demo.py --record -->

<img width="1561" height="1098" alt="demo" src="https://github.com/user-attachments/assets/757950b2-0523-4b59-b617-96d94ccf5ab1" />

## Nine session formats now interact seamlessly

Your AI coding history is already on disk. Claude Code writes JSONL files named by UUID. Codex
keeps its own transcript format under `~/.codex`. Cursor, Gemini CLI, Antigravity, and AI Studio
each store sessions somewhere else again, in shapes that share nothing with each other.

So the fix an agent made last month is in there, along with the version of a file it overwrote
and the reason it stopped halfway through a task, and you have no way to go get any of it.

`aise` parses all nine formats into one index and searches it.

That index reaches you four ways: the `aise` command line, an MCP server your coding agent can
query mid-conversation, a Rust crate, and a Python package. One parser and one set of query
types sit underneath all four, so the answer is the same whichever one you ask.

## Common tasks and Example commands

| Do this | Command |
| --- | --- |
| Find the session where something happened | `aise search "auth refactor"` |
| Search individual messages across every agent | `aise messages search "permission denied"` |
| Recover a file an agent wrote or edited | `aise files extract src/app.py` |
| See every version of a file, across sessions | `aise files history src/app.py` |
| Read a past session, then jump back into it | `aise show SESSION_ID` then `aise resume SESSION_ID` |
| Find why an agent stopped, looped, or was blocked | `aise messages search "" --kinds harness-notice` |
| See what a subagent was asked and what it found | `aise list --session-kinds subagent` |
| Find where you had to correct the agent | `aise skills corrections` |
| Recover context after a compaction | `aise messages evidence SESSION_ID` |
| Browse and fuzzy-search interactively | `aise tui` |
| Export a session to Markdown | `aise export SESSION_ID --format markdown` |
| Check index health and effective paths | `aise doctor`, `aise config paths` |

A harness notice is something the surrounding tool said to the agent: Stop-hook feedback,
PreToolUse blocks, local-command caveats, task notifications. A transcript often never mentions
the block that ended a run, so searching these answers the question the transcript cannot. They
stay out of ordinary results until `--kinds harness-notice` asks for them. Subagent runs get
indexed as sessions of their own, each one recording the session that spawned it, which keeps
delegated work searchable once it finishes.

## Install

Install the global `aise` command, via `uv` (recommended):

```bash
uv tool install ai-session-search && aise integrations install
```

via rust's `cargo`:

```bash
cargo install ai-session-search --locked && aise integrations install
```


## Quick start

```bash
# Index the sessions aise found
aise reindex

# What have I been working on?
aise list
aise search "database migration" --provider codex

# Find a specific moment, then read around it
aise messages search "permission denied" --role user
aise show SESSION_ID
aise resume SESSION_ID

# Get a file back
aise files history src/app.py
aise files extract src/app.py > src/app.py
```

`aise COMMAND --help` is authoritative for every parameter and default. Date bounds accept ISO,
EDTF, durations, and common natural-language forms; `aise dates` is the full reference.

## MCP server

Ask Claude Code why last Tuesday's run stopped, or which version of a file it wrote before the
refactor, and it queries this index and answers inline without you leaving the conversation.
The server is a subcommand of the same executable, so there is nothing else to install.

```bash
aise integrations install    # configures supported components for every detected harness
aise integrations status
aise mcp serve               # direct stdio use by a client
```

Eight tools are exposed: `search_sessions`, `search_messages`, `list_sessions`, `get_session`,
`get_resume_command`, `get_index_status`, `run_skill_capability`, and `query_session_index` for
read-only SQL.

The installer writes each MCP client's native JSON or TOML shape for Claude Code and Claude
Desktop, ChatGPT Codex desktop and Codex CLI/IDE, Gemini CLI, Antigravity App/IDE/CLI, Cursor,
Windsurf, VS Code, Zed, OpenCode, OpenClaw, and the legacy KiloCode VS Code extension. Pi and Prime
Agent instead receive the shared skill plus native `AGENTS.md` guidance: Pi has no MCP client, and
Prime's kernel currently accepts remote HTTP MCP integrations rather than a local stdio server.
Claude receives `CLAUDE.md` guidance, Codex and OpenCode receive a managed `AGENTS.md` block, and
Gemini and Antigravity share one managed block in `~/.gemini/GEMINI.md`. Every other MCP client receives MCP
configuration only. No client hooks are installed.

Omit `--client` to update everything detected, or name clients explicitly. Generated
configuration stores the absolute path of the first `aise` on your PATH, which keeps shells and
GUI clients on the same installation when their PATH order differs; `--binary PATH` overrides it.

MCP tools are read-only and bounded. Writing bundles to disk stays a CLI and library operation,
so no MCP call has a filesystem side effect. Tool inputs are closed schemas validated before the
index is even opened, and a misspelled field fails with its exact argument path.

Install and uninstall preflight every target and write a durable receipt before the first change.
A failure partway through restores the files already written. An interruption preserves the
receipt and prints the exact `aise integrations recover` invocation, and recovery only touches
files that still match its recorded before-and-after image. `aise integrations status` reads the
receipt and every target under the transaction's shared RAII lock, so it can never report a
mixture of two installer generations.

## Speed

Measured on macOS with Apple silicon against an index of roughly 2.5 million messages, best of
three warm runs. Each command also carried `--index-refresh existing-only --format json`, so
the numbers are query cost with no refresh in the path:

| Command | Time |
| --- | ---: |
| `aise list --limit 20` | 48 ms |
| `aise files search "*.rs" --limit 50` | 121 ms |
| `aise messages search "ECONNRESET\|socket hang" --query-mode regex --limit 50` | 0.3 s |
| `aise messages search "permission denied" --limit 50` | 1.4 s |

Startup accounts for 43 ms of that first row, so `aise list` spends nearly all its time
launching. Holding that number down is why the core is Rust and the CLI ships as a single
native binary: parsing, indexing, and querying all run with no Python interpreter in the path,
and the Python package imports that same compiled extension.

Indexing is incremental. A refresh over this index walked 6,324 source files in 13 s and
reparsed only the 6 sessions whose bytes had changed, so steady-state cost follows new data.
Refresh runs on access by default, including when an MCP client asks a question, and
`--index-refresh existing-only` skips it when you want the query cost alone.

Scope flags cut the corpus a query considers. For session list/search/export/analysis, date bounds
intersect the known indexed span from `created_at` through `updated_at`: `--since` requires the span
to end on or after the bound, `--until` requires it to start on or before the bound, and `--when`
requires overlap with the resolved period. That span can contain gaps; it is not a claim of
continuous process activity. Message, file, and event analytics continue to compare each event's
own timestamp. `--provider` and `--path` compose with either interpretation. Your index is probably
smaller than this one, so treat these timings as a ceiling.

## Session sources

Everything is read from local files on your own machine. Cloud-only ChatGPT, Claude, or
Antigravity conversations are never copied from vendor accounts, and this project does not
claim they are searchable.

| Session source | Provider ID | Native resume |
| --- | --- | --- |
| Claude Code CLI/Desktop | `claude` | yes |
| Claude Desktop local agent | `claude-desktop` | no; use show/export |
| ChatGPT Codex desktop and Codex CLI/IDE | `codex` | yes |
| Cursor | `cursor` | no; use show/export |
| Antigravity App/IDE/CLI | `antigravity` | no; use show/export |
| Pi coding agent | `pi` | yes |
| Prime Agent | `prime-agent` | yes |
| Google AI Studio | `aistudio` | no; use show/export |
| Gemini CLI | `gemini-cli` | no; use show/export |

Prime Agent is Pi-derived but stores its own root and RLM-child transcripts under
`~/.prime/agent`; it remains a separate provider so filters, IDs, diagnostics, and resume commands
never conflate the two harnesses. Codex desktop and Codex CLI/IDE share one local host under
`~/.codex`, so they index together.
The `codex` provider reads that local transcript format only; it does not parse Chat/Work
browser state or sync cloud history, which OpenAI documents as
[separate histories](https://help.openai.com/en/articles/20001276) in the merged desktop app.
These IDs are the TOML keys in provider configuration, and `aise config paths` prints each
source's enabled state and effective roots.

## Filters and scopes

```bash
# Provider, path, and time scopes combine
aise list --provider claude-desktop --path ~/source/project --since 7d

# Get the newest session in this directory or a descendant directory
aise list --path ~/source/project --limit 1

# Search user turns while excluding compaction summaries
aise messages search "regression" --role user --include-compaction false

# Search normalized tool calls and one canonical argument field
aise messages search "Cargo.toml" --field tool-argument --argument-path /path --tool-name-contains Edit

# Cap each returned message at its first 5 lines; negative keeps the tail instead
aise messages get SESSION_ID --role user --lines-per-message 5

# Read the 75 most recent user messages. --order selects which N; results stay oldest-first
aise messages get SESSION_ID --role user --limit 75 --order newest

# Walk a long session in non-overlapping chunks instead of growing --limit
aise messages get SESSION_ID --seq-from 0 --seq-to 499
aise messages get SESSION_ID --seq-from 500 --seq-to 999

# Recover every causally valid version of a file as a lossless stream
aise files extract path/to/file.rs --all --format jsonl

# See why the planner chose the candidates it did
aise messages search "database lock" --query-mode fuzzy --limit 20 --receipt-level full --format json
```

How many results you get back depends on the surface. Session-level and MCP searches return
bounded pages, which keeps a client from being flooded. Rust, CLI, and Python message search are
unbounded on omission for literal, regex, and no-text queries, and `--all-results` states that
choice explicitly for scripts. Fuzzy search always requires a finite page. Pass `--limit 0` only
where that command's help defines zero as the complete corpus.

Line windows share one sign convention: positive keeps the first N, negative keeps the last N,
and `0` keeps everything. `aise show --transcript-lines` windows a whole rendered transcript,
while `--lines-per-message` caps each returned message on its own. Both change presentation
only, so a large result page becomes skimmable without silently dropping hits: matching,
ranking, result count, pagination, and context membership are unaffected.

Exact and regex modes verify the requested predicate after any indexed prefilter, so the
prefilter cannot change which results you get. When you want to see why the planner picked the
candidates it did, `--receipt-level summary` adds its diagnostics to the response and
`--receipt-level full` adds the resolved origin of every parameter. MCP returns the same receipt
as structured data.

<!-- aise-message-search-contract:start -->
### Generated message-search defaults

Search indexed AI-session messages while separating result selection, context, presentation, optional payloads, and receipts.

| Caller surface | Omitted non-fuzzy result extent | Context | Lines per message | Field view | Match view | Includes / receipt |
| --- | --- | --- | ---: | --- | --- | --- |
| rust | all results; offset 0 | 0 before / 0 after | 0 | `{"kind":"no_char_limit"}` | `{"kind":"max_chars","max_chars":220}` | `[]` / `none` |
| cli | all results; offset 0 | 0 before / 0 after | 0 | `{"kind":"no_char_limit"}` | `{"kind":"max_chars","max_chars":220}` | `[]` / `none` |
| mcp | page of 15; offset 0 | 0 before / 0 after | 0 | `{"kind":"max_chars","max_chars":220}` | `{"kind":"max_chars","max_chars":220}` | `["normalized_session_metadata"]` / `none` |
| python | all results; offset 0 | 0 before / 0 after | 0 | `{"kind":"no_char_limit"}` | `{"kind":"max_chars","max_chars":220}` | `[]` / `none` |

These are shipped defaults from an empty configuration. `aise messages search --describe --describe-surface cli|mcp|python|rust` resolves the same contract with the active configuration. Positive `limit` counts result rows; signed `lines_per_message` selects the beginning, end, or complete text of each already-selected message.
<!-- aise-message-search-contract:end -->

## Export and analysis

```bash
# One full session to stdout
aise export SESSION_ID --format markdown

# Publish a selected bundle into a new directory
aise export --since 7d --output-dir ./week

# Analyze the whole corpus, publishing JSON and Markdown. Scope flags narrow it
aise analyze --output ./analysis

# Apply a validated provider-neutral policy
aise analyze --policy ./analysis-policy.json --output ./policy-analysis
```

A bundle destination must not already exist. `aise` stages the output, syncs it, publishes the
complete directory atomically, and returns a receipt with the artifact metadata. It never merges
into or replaces an existing bundle.

## Python and Rust APIs

Both libraries sit on the same typed Rust services as the CLI. The
[library guide](docs/development/library-api.md) has the full API, feature flags, and
concurrency semantics.

```python
import ai_session_search as aise

search = aise.SessionSearch()
search.refresh()
messages = search.search_messages(
    "authentication",
    aise.MessageSearchRequest(role="user", limit=50),
)
```

```rust,no_run
use ai_session_search::service::SessionSearch;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = SessionSearch::load()?;
    let status = app.index().status()?;
    println!("{} repairs pending", status.repair_commands.len());
    Ok(())
}
```

Long native operations release the GIL, and the Rust crate keeps Clap, MCP, SQLite, and PyO3
types out of its public contracts.

## Configuration

Configuration is TOML at `~/.ai-session-search/config.toml`, in an app-owned directory that sits
beside harness directories such as `~/.claude` and `~/.codex`. Database and cache paths are
configurable on their own and keep platform-appropriate defaults.

```bash
aise config show      # effective settings
aise config origins   # where each setting came from
aise config paths     # resolved paths and per-source roots
```

| Variable | Selects |
| --- | --- |
| `AI_SESSION_SEARCH_CONFIG` | TOML configuration file |
| `AI_SESSION_SEARCH_DATABASE` | SQLite index file |
| `AI_SESSION_SEARCH_CACHE_DIR` | Disposable cache directory |
| `AI_SESSION_SEARCH_THREADS` | Positive worker-thread count |

Precedence runs from CLI argument, to environment variable, to TOML, to the platform default.

Search is unrestricted by default. An opt-in `[search.scope]` panel confines reads to configured
absolute roots, the invocation directory, and live MCP client roots; restricted mode fails closed
without authority and disables arbitrary content SQL. See
[search access scope](docs/development/configuration.md#search-access-scope). Legacy settings
import through `aise migrate config`.

## Index maintenance

Never copy a live SQLite index by hand, because the file alone omits its WAL state. Use the
migration service, which takes an online backup, verifies integrity and row manifests, preserves
rollback evidence, and publishes the destination atomically. An interrupted migration is
recoverable from its receipt, and a conflicting destination is never overwritten.

```bash
aise migrate database --help
aise migrate verify --help
```

`aise doctor` reports exact allocated and reclaimable bytes. When reclaimable pages exist it
names `aise compact`, which needs an exclusive lock and temporary disk space. Migration never
vacuums the source silently; compact a published destination once the diagnostic shows the
tradeoff is worth it.

## Command reference

`aise COMMAND --help` documents each one in full.

| Command group | What it covers |
| --- | --- |
| `aise list`, `search`, `show`, `resume` | Find, read, and re-enter sessions |
| `aise messages search\|get\|timeline\|evidence` | Query normalized turns and tool evidence |
| `aise files search\|history\|cross-ref\|extract` | Locate and reconstruct edited files |
| `aise skills corrections`, `aise planning`, `aise stats` | Deterministic message classification and indexed behavioral summaries |
| `aise skills list\|show\|validate\|create\|update\|restore` | Inspect, author, and repair skill packages |
| `aise vocab`, `aise repeats` | Count how often a term appears and in how many messages (`--prefix` looks one up), or find recurring phrases |
| `aise export` | Render one session, or publish an explicitly selected bundle |
| `aise analyze` | Apply a validated policy and publish an immutable analysis bundle |
| `aise reindex`, `compact`, `doctor` | Maintain and diagnose the index |
| `aise migrate database\|config\|verify\|recover` | Verified, reversible migration |
| `aise config file\|example\|init\|show\|origins\|paths` | Inspect or initialize TOML configuration and resolved paths |
| `aise package status\|check\|update` | Package ownership, release checks, and manager-driven updates |
| `aise integrations install\|status\|uninstall\|recover`, `aise mcp serve` | Aliases, MCP registrations, owned instructions and skills, and the MCP transport |
| `aise db` | Expert read-only SQL; `aise db query --help` lists the tables and the column values a predicate misreads |
| `aise tui` | Interactive session browser; message-field modes stay in `aise messages search` |
| `aise dates` | Every accepted date and duration form |

## Architecture

A single executable backs the CLI, the MCP server, the Rust crate, and the Python package, and
they share one index lifecycle and one provider registry. Rust owns parsing, querying, migration,
and filesystem publication, so Python carries no second scanner and no parallel policy
implementation. Indexed filtering covers provider, session, path, date, role, message kind, tool,
sequence, and canonical tool-argument JSON pointer. One `--kinds` set selects message classes, so
two options can never disagree about what comes back. File reconstruction streams, restores are
collision-safe, and export and analysis bundles are immutable and checksummed.

## Uninstalling and other install details


Other paths to the same build: `uvx --from ai-session-search aise --help` to run it once,
`uv add ai-session-search` for a project dependency, `python -m pip install ai-session-search`
for a standard install.

Installing the package never touches your configuration. It does not create command aliases,
edit MCP client configuration, write instruction files or skills, or scan a single transcript.
`aise integrations install` is the explicit second step that does those things: it adds the
`aisearch` and `ai_session_search` aliases, configures supported components for every detected
harness, and starts
building the index in the background. Run `aise doctor` afterward for readiness and recovery
guidance.

Wheels cover CPython 3.12 through 3.14 on manylinux2014 x86_64 and aarch64, macOS x86_64 and
arm64, and Windows x86_64. Building from source needs Rust 1.88 or newer and a C linker.

To remove it, take the integrations out before the executable, because the uninstaller is what
knows which entries are its own:

```bash
aise integrations uninstall
uv tool uninstall ai-session-search    # or: cargo uninstall ai-session-search
```

Uninstalling removes what `aise` installed and leaves your index, configuration, and session
files alone. The [installation guide](docs/development/installation.md) covers source builds,
per-component flags, custom destinations, updates through `aise package update`, and recovery.

On PyPI, `ai-session-search` supersedes the retired `ai_session_tools` package, which was a
single-user Python implementation last published as `0.3.1` and receives no further releases.

## Development

```bash
uv sync --locked --all-extras
uv run maturin develop --uv    # required: Python tests import the compiled extension
./run_ci_local.sh              # authoritative gate; run before proposing a commit
```

The gate builds isolated config, cache, and database state, so it never touches a real user
index. Run it with no environment prefix: it inherits whatever compiler wrapper Cargo is
configured to use, and prefixing `RUSTC_WRAPPER=` disables
[sccache](https://github.com/mozilla/sccache) and forces a cold rebuild.

While iterating:

```bash
cargo test -p ai-session-search <test name>
uv run pytest tests/<file>.py -k <test name>
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
uv run ruff check . && uv run mypy ai_session_search tests
uv run python -m mypy.stubtest ai_session_search --concise --ignore-disjoint-bases
```

[CONTRIBUTING.md](CONTRIBUTING.md) covers review expectations, and the
[documentation index](docs/README.md) separates user guides from maintainer and migration
records.

## License

Apache License 2.0. Third-party dependencies keep their own licenses, and release artifacts
ship dependency inventories and software bills of materials.
