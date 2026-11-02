# AI Session Search (`aise`)

Ultra-fast Rust-based search of your local AI coding sessions, reindexing on the fly. Reads
Claude Code, Claude Desktop, ChatGPT Codex, Cursor, Antigravity, Pi, Prime Agent,
Google AI Studio, and Gemini CLI, on macOS, Linux, and Windows.

[![PyPI](https://img.shields.io/pypi/v/ai-session-search)](https://pypi.org/project/ai-session-search/)
[![crates.io](https://img.shields.io/crates/v/ai-session-search)](https://crates.io/crates/ai-session-search)
[![CI](https://github.com/ahundt/ai-session-search/actions/workflows/ci.yml/badge.svg)](https://github.com/ahundt/ai-session-search/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

<!-- Demo goes here: drag demo.gif into the GitHub web editor, which uploads it and
     inserts a user-attachments image link in place of this comment. The recording
     itself is untracked; regenerate it with uv run python tests/test_demo.py --record -->

<img width="1561" height="1098" alt="aise searching indexed AI coding sessions from the terminal" src="https://github.com/user-attachments/assets/757950b2-0523-4b59-b617-96d94ccf5ab1" />

## Search nine harnesses from one index

Useful AI coding history is already on disk. It is just hard to find, and in a range of totally
different formats.

`aise` parses all nine formats into one index and searches it efficiently. Your fixes from last
month, the files an agent overwrote, and the record of why a run failed become instantly findable.

Query that index from the command line, a Rust crate, a Python package, or an MCP server your
coding agent calls mid-conversation.

## Install

Install the global `aise` command with `uv` (recommended):

```bash
uv tool install ai-session-search && aise integrations install
```

or with Rust's `cargo`:

```bash
cargo install ai-session-search --locked && aise integrations install
```

Both install the newest stable release; name an exact version to choose a different one. The
second command is the one that touches your tools: it registers the MCP server and writes
guidance for each detected harness, and `--dry-run` previews every write.

## Common tasks

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

A run that stopped without saying why usually hit a harness notice: Stop-hook feedback, a
PreToolUse block, a local-command caveat, a task notification. These are what the surrounding
tool said to the agent, not what the agent wrote, so they stay out of ordinary results until
`--kinds harness-notice` asks for them.

Subagent runs become sessions in their own right, each one recording the session that spawned
it. `--session-kinds subagent` lists them, and `--parent-session` narrows to the children of
one run.

## Quick start

```bash
# Optional: build the index now rather than on your first query
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

`aise COMMAND --help` documents every parameter and default. Date bounds accept ISO, EDTF,
durations, and common natural-language forms; `aise dates` lists every accepted form.

## MCP server

Ask Claude Code why last Tuesday's run stopped, or which version of a file it wrote before the
refactor, and it queries this index and answers inline without you leaving the conversation.
The server is a subcommand of the same executable, so there is nothing else to install.

```bash
aise integrations install    # register the server with every detected harness
aise integrations status
aise mcp serve               # direct stdio use by a client
```

The server exposes eight tools: `search_sessions`, `search_messages`, `list_sessions`,
`get_session`, `get_resume_command`, `get_index_status`, `run_skill_capability`, and
`query_session_index` for read-only SQL.

Each harness gets its own JSON or TOML shape, plus guidance and the skill where the harness reads
them. `--client` limits the install to named harnesses, and no harness gets hooks.

| Harness | Receives |
| --- | --- |
| Claude Code, Claude Desktop | MCP config, `CLAUDE.md` guidance, skill |
| ChatGPT Codex desktop and CLI/IDE, OpenCode | MCP config, managed `AGENTS.md` block, skill |
| Gemini CLI, Antigravity App/IDE/CLI | MCP config, shared `~/.gemini/GEMINI.md` block, skill |
| Pi, Prime Agent | Skill and native `AGENTS.md` only: Pi has no MCP client, and Prime's kernel currently takes remote HTTP MCP integrations |
| Cursor, Windsurf, VS Code, Zed, OpenClaw, legacy KiloCode | MCP config |

MCP tools are read-only and bounded. Writing bundles to disk stays a CLI and library operation,
so no MCP call has a filesystem side effect. Tool inputs are closed schemas, and a misspelled
field fails with its exact argument path.

A failed install restores what it had already written, and an interruption prints the exact
`aise integrations recover` invocation. `aise integrations status` reads every target under the
transaction's shared RAII lock, so it never reports a half-applied mixture.

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
launching. Keeping that number down is why the CLI is a single native binary: no Python
interpreter sits in the path of parsing, indexing, or querying.

Indexing is incremental. A refresh over this index walked 6,324 source files in 13 s and
reparsed only the 6 sessions whose bytes had changed, so steady-state cost follows new data.
Refresh runs on access by default, including when an MCP client asks a question, and
`--index-refresh existing-only` skips it when you want the query cost alone.

Scope flags cut the corpus a query considers, so a narrowed query costs less than these rows;
[Filters and scopes](#filters-and-scopes) covers what each one selects. Your index is probably
smaller than this one, so treat these timings as a ceiling.

## Session sources

Everything is read from local files on your own machine. Cloud-only ChatGPT, Claude, and
Antigravity conversations are never copied from a vendor account, and are not searchable here.

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
`~/.prime/agent`, so it stays a separate provider and never conflates with Pi in filters, IDs,
or resume commands. Codex desktop and Codex CLI/IDE share one local host under `~/.codex`, so
they index together.

The `codex` provider reads that local transcript format only. It does not parse Chat/Work
browser state or sync cloud history, which OpenAI documents as
[separate histories](https://help.openai.com/en/articles/20001276) in the merged desktop app.

These IDs are the TOML keys in provider configuration, and `aise config paths` prints each
source's enabled state and effective roots.

## Filters and scopes

```bash
# Narrow a search to one tool, one project, and one week at once
aise list --provider claude-desktop --path ~/source/project --since 7d

# Get the newest session in this directory or a descendant directory
aise list --path ~/source/project --limit 1

# Find what you asked for, without the compaction summaries repeating it back
aise messages search "regression" --role user --include-compaction false

# Find which tool call touched a file, by searching the argument rather than the prose
aise messages search "Cargo.toml" --field tool-argument --argument-path /path --tool-name-contains Edit

# Skim a long result page: keep each message's first 5 lines, or its last 5 with -5
aise messages get SESSION_ID --role user --lines-per-message 5

# Read the 75 most recent user turns. --order picks which 75; they print oldest-first
aise messages get SESSION_ID --role user --limit 75 --order newest

# Walk a long session in non-overlapping chunks instead of growing --limit
aise messages get SESSION_ID --seq-from 0 --seq-to 499
aise messages get SESSION_ID --seq-from 500 --seq-to 999

# Get every version of a file the session can account for, one JSON object per version
aise files extract path/to/file.rs --all --format jsonl

# See which setting or default decided a surprising result
aise messages search "database lock" --query-mode fuzzy --limit 20 --receipt-level full --format json
```

Date bounds test a session's span, `created_at` through `updated_at`: `--since` needs it to end
on or after the bound, `--until` to start on or before, `--when` to overlap the period. That span
can contain gaps, so it is not a claim of continuous activity. Message, file, and event analytics
compare each event's own timestamp instead.

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
prefilter cannot change which results you get. When a result surprises you, `--receipt-level
summary` shows how the query was planned and `--receipt-level full` also shows which setting or
default supplied each value you did not pass. MCP returns the same receipt as structured data.

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
# One session as Markdown you can paste into an issue; json and text also work
aise export SESSION_ID --format markdown

# Keep last week's sessions as files, before a harness rotates them away
aise export --since 7d --output-dir ./week

# What the whole corpus holds, as index.md, taxonomy.md, knowledge-graph.md; scopes narrow it
aise analyze --output ./analysis

# The same analysis, defined once as a reusable JSON policy
aise analyze --policy ./analysis-policy.json --output ./policy-analysis
```

A bundle destination must not already exist. `aise` stages the output, syncs it, publishes the
complete directory atomically, and returns a receipt, so an interrupted run leaves no half-written
bundle and an existing one is never merged into or replaced.

## Python and Rust APIs

Use these to build on the index instead of shelling out to `aise` and parsing its output.

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

A long refresh or search releases the GIL, so it will not block your other Python threads. The
Rust crate keeps Clap, MCP, SQLite, and PyO3 types out of its public contracts, so depending on it
does not pull the CLI's dependency graph into yours. The
[library guide](docs/development/library-api.md) has the full API, feature flags, and concurrency
semantics.

## Configuration

Nothing needs configuring: with no file, every provider is enabled at its platform-default paths.
To change that, `aise config init` writes `~/.ai-session-search/config.toml` with every setting
commented out but the database and cache paths, so later releases' defaults keep applying to
whatever you leave alone. `aise config example` previews it first.

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

Never copy a live SQLite index by hand, because the file alone omits its WAL state.
`aise migrate database` takes an online backup, verifies it, and publishes the destination
atomically, never overwriting a path that already exists.

```bash
aise migrate database --help   # move or copy an index safely
aise migrate verify --help     # check a source and destination against the receipt
aise migrate recover --help    # finish or roll back an interrupted migration
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
| `aise skills list\|show\|validate\|create\|update\|restore\|run` | Inspect, author, repair, and run skill packages |
| `aise vocab`, `aise repeats` | How often a term appears, and in how many messages (`vocab --prefix TERM` for one term); or recurring phrases |
| `aise export` | Render one session, or publish an explicitly selected bundle |
| `aise analyze` | Apply a validated policy and publish an immutable analysis bundle |
| `aise reindex`, `compact`, `doctor` | Maintain and diagnose the index |
| `aise migrate database\|config\|verify\|recover` | Verified, reversible migration |
| `aise config file\|example\|init\|show\|origins\|paths` | Inspect or initialize TOML configuration and resolved paths |
| `aise package status\|check\|update` | Package ownership, release checks, and manager-driven updates |
| `aise integrations install\|status\|uninstall\|recover`, `aise mcp serve` | Aliases, MCP registrations, owned instructions and skills, and the MCP transport |
| `aise db schema\|query` | Read-only SQL. `aise db schema` prints the tables; `aise db query --help` lists the columns whose stored values differ from the CLI's spelling, where a wrong guess returns no rows and no error |
| `aise tui` | Interactive browser: typeahead search, preview scroll, and filter keys (p provider, f class, s window, w warnings) |
| `aise dates` | Every accepted date and duration form |

## Architecture

One executable backs the CLI, the MCP server, the Rust crate, and the Python package, over one
index lifecycle and one provider registry, so a new provider becomes searchable on all four at
once.

Rust owns parsing, querying, migration, and filesystem publication, so every surface resolves a
query through one implementation. Filtering by provider, session, path, date, role, message kind,
tool, sequence, or canonical tool-argument JSON pointer runs inside the index, and one `--kinds`
set owns message-class selection.

File reconstruction streams rather than buffering, `files extract --restore` writes a
`.recovered` sibling and never overwrites, and export and analysis bundles are immutable and
checksummed.

## Install details and uninstalling

Other paths to the same build: `uvx --from ai-session-search aise --help` runs it once without
installing, `uv add ai-session-search` adds a project dependency, and
`python -m pip install ai-session-search` does a standard install.

Wheels cover CPython 3.12 through 3.14 on manylinux2014 x86_64 and aarch64, macOS x86_64 and
arm64, and Windows x86_64. Building from source needs Rust 1.88 or newer and a C linker.

Installing the package never touches your configuration. It does not create command aliases,
edit MCP client configuration, write instruction files or skills, or scan a single transcript.
`aise integrations install` is the second step that does: it also adds the `aisearch` and
`ai_session_search` aliases and starts building the index in the background. Run `aise doctor`
afterward for readiness and recovery guidance.

After an upgrade, the first `aise` command brings the skill and instructions it installed up to
date. Set `[integrations] refresh_after_upgrade = false` to turn that off.

To remove it, take the integrations out before the executable, because the uninstaller is what
knows which entries are its own:

```bash
aise integrations uninstall
uv tool uninstall ai-session-search    # or: cargo uninstall ai-session-search
```

Uninstalling removes what `aise` installed and leaves your index, configuration, and session
files alone. The [installation guide](docs/development/installation.md) covers source builds,
per-component flags, custom destinations, updates through `aise package update`, and recovery.

## Development

```bash
uv sync --locked --all-extras
uv run maturin develop --uv    # required: Python tests import the compiled extension
./run_ci_local.sh              # authoritative gate; run before proposing a commit
```

The gate builds isolated config, cache, and database state, so it never touches a real user
index. Run it with no environment prefix: prefixing `RUSTC_WRAPPER=` disables
[sccache](https://github.com/mozilla/sccache) and forces a cold rebuild.

While iterating:

```bash
cargo test -p ai-session-search <test name>
uv run pytest tests/<file>.py -k <test name>
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
uv run ruff check . && uv run mypy ai_session_search tests
uv run python -m mypy.stubtest ai_session_search --concise --ignore-disjoint-bases
```

[CHANGELOG.md](CHANGELOG.md) records what changed in each release.
[CONTRIBUTING.md](CONTRIBUTING.md) covers review expectations, and the
[documentation index](docs/README.md) separates user guides from maintainer and migration
records.

## License

Apache License 2.0. Third-party dependencies keep their own licenses, and release artifacts
ship dependency inventories and software bills of materials.
