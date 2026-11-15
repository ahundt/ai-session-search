# AI Session Search (`aise`)

Search and recover your local AI coding-agent session history from Claude Code, Claude Desktop
local agent, Codex, Cursor, Antigravity, Pi coding agent, Prime Agent, Google AI Studio, and
Gemini CLI. Use one index from the CLI, an MCP server, Python, or Rust on macOS, Linux, and Windows.

[![PyPI](https://img.shields.io/pypi/v/ai-session-search)](https://pypi.org/project/ai-session-search/)
[![crates.io](https://img.shields.io/crates/v/ai-session-search)](https://crates.io/crates/ai-session-search)
[![CI](https://github.com/ahundt/ai-session-search/actions/workflows/ci.yml/badge.svg)](https://github.com/ahundt/ai-session-search/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/ahundt/ai-session-search/blob/main/LICENSE)

<img width="1561" height="1098" alt="aise searching indexed AI coding sessions from the terminal" src="https://github.com/user-attachments/assets/757950b2-0523-4b59-b617-96d94ccf5ab1" />

## Search nine providers from one index

`aise` indexes local transcripts so you can find earlier messages, recover file versions, and
read the context around a failed run. Queries refresh the index as needed; use
`--index-refresh existing-only` to read the index without refreshing it.

## Install

Install the global `aise` command with `uv` (recommended):

```bash
uv tool install ai-session-search && aise integrations install
```

or with Rust's `cargo`:

```bash
cargo install ai-session-search --locked && aise integrations install
```

Both install the newest stable release. The second command registers the MCP server and writes
guidance for each detected harness; `--dry-run` previews every write.

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

Human input includes prompts typed while the agent was busy and answers recorded through tools.
`--role user` searches those recorded input regions across stored roles; returned context can
contain generated text. Known prompts from a parent agent to a subagent stay outside that
selector. Omit `--role` to search complete content.

Claude Code conversation and attachment records supply the searchable text; saved API request
copies do not add repeated turns or file edits. Queued Claude submissions appear once when
delivered through a queued-command attachment or directly after a dequeue, including after an
earlier entry was cancelled or cleared; independent same-text submissions remain distinct. Files a
person mentions, attached file snippets, memory and plan text, and invoked skill contents are
searchable by default.
Injected instructions, session context, hook messages, and interrupt notices are harness notices:
find them with `--kinds harness-notice`. `aise doctor` reports unrecognized record and attachment
types; malformed attachments and skipped binary payloads appear as parse warnings.

Corrections and repeats use input with evidence of a human sender, including queued input. Older
Claude Code transcripts can omit the sender: reports count those rows as `unattributed_input_count`
and leave them out by default. Pass `--include-unattributed-input` to include them; a program may
have sent them. `aise repeats --format json --report` includes that count alongside its groups.

The shipped correction categories are a small starter set of fixed phrasings. They will miss
corrections worded for the situation. To find where a session went wrong, list its recorded input
and read it with context; use categories as a quick first filter:

```bash
aise list --path ~/source/project --when 7d --limit 10
aise messages search "" --session-id SESSION_ID --role user --all-results --format json
aise messages get SESSION_ID --seq 42 --context 3
```

With MCP, use `list_sessions(path_prefix="/path/to/project", when="7d", limit=10)`,
`search_messages(session_id="SESSION_ID", role="user", all_results=true)`, then
`get_session(session_id="SESSION_ID", seq=42, context=3)`. Read the provenance when sender
attribution is uncertain. For a large MCP result, replace `all_results=true` with `limit=50` and
follow `page.next_offset`. Add categories for wording you see repeatedly using the
[skill package creation steps](skills/ai-session-search/SKILL.md#create-your-own-classification-package).

A subagent's full work lives in its own session; the parent may show only delegation and its result.
`--session-kinds subagent` lists those sessions, and `--parent-session` narrows to one run's
children.

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

# Get a file back, into a .recovered sibling that never overwrites what is there
aise files history src/app.py
aise files extract src/app.py --restore
```

When an empty message search prints a hint, the active query and filters found no matches.
The hint suggests related content that may live in harness notices, decoded tool arguments, or
file history; it does not prove that such content exists.

`aise resume SESSION_ID` prints its command when stdin or stdout is redirected; use `--dry-run`
to request that explicitly in a terminal.

`aise COMMAND --help` lists command parameters. The [configuration reference](docs/development/configuration.md)
and [generated caller reference](docs/README.md#generated-message-search-caller-contract) describe
defaults and precedence. Date bounds accept ISO 8601,
[EDTF](https://www.loc.gov/standards/datetime/), durations, and common natural-language forms,
read in UTC unless the value names an offset or a trailing
[RFC 9557](https://www.rfc-editor.org/rfc/rfc9557) zone such as `[Europe/Berlin]`;
`aise dates` lists each accepted family with examples.

## MCP server

Ask Claude Code why last Tuesday's run stopped, or what happened before the
refactor, and it queries this index and answers inline without you leaving the conversation.
The server is a subcommand of the same executable, so there is nothing else to install.

```bash
aise integrations install    # register the server with every detected harness
aise integrations status
aise mcp serve               # direct stdio use by a client
```

The server exposes `search_sessions`, `search_messages`, `list_sessions`, `get_session`,
`get_resume_command`, `get_index_status`, `run_skill_capability`, and `query_session_index` for
read-only SQL.

What a harness receives depends on what it can read, in the format and location it expects.
`--client` limits the install to named harnesses, and no harness gets hooks.

| Harness | Receives |
| --- | --- |
| Claude Code CLI/Desktop | MCP config, `CLAUDE.md` guidance, skill |
| Claude Desktop local agent | MCP config |
| ChatGPT Codex desktop and CLI/IDE | MCP config, managed `AGENTS.md` block, skill |
| OpenCode | MCP config, managed `AGENTS.md` block |
| Gemini CLI, Antigravity App/IDE/CLI | MCP config, shared `~/.gemini/GEMINI.md` block, skill |
| Pi 0.99.0+ | MCP config in `~/.pi/agent/mcp.json`, skill, native `AGENTS.md` guidance |
| Prime Agent | MCP config in `~/.prime/agent/settings.json` with `type: "stdio"`, skill, native `AGENTS.md` guidance |
| Cursor, Windsurf, VS Code, Zed, OpenClaw, KiloCode | MCP config |

File recovery, export, analysis, and explicit reindexing use `aise files extract`, `aise export`,
`aise analyze`, and `aise reindex`, or the library APIs. A query may refresh the index, writing `aise`'s own state;
`--index-refresh existing-only` opens SQLite read-only. Tool inputs are closed schemas, so a
misspelled field fails with its exact argument path.

A failed install rolls back what it had already written, and when it cannot finish the rollback
it keeps the receipt and prints the exact `aise integrations recover` command.
`aise integrations status` reads every target under the installer's own transaction lock, so it
never shows you a half-applied state.

## Search cost

Literal and regex queries use word and trigram indexes to narrow candidates, then verify the
requested match. A selective fragment and structural filters reduce the scan. Fuzzy search scores
every eligible row, so narrow by project, provider, session, or date first. `--receipt-level
summary` explains the selected query plan.

Refresh is separate from query cost: incremental reads cover appended bytes plus at most 1 MiB
of overlap. [Whole-source fallbacks](skills/ai-session-search/references/search-costs-and-receipts.md#per-operation)
apply to older checkpoints or parser versions, changed overlap bytes, and new records that change
indexed rows. Use `--index-refresh before-query` to wait
for source updates, or `existing-only` to skip refresh. See the
[search cost reference](skills/ai-session-search/references/search-costs-and-receipts.md).

## Providers

Everything is read from local files on your own machine. Cloud-only ChatGPT, Claude, and
Antigravity conversations are never copied from a vendor account, and are not searchable here.

| Provider | Provider ID | Native resume |
| --- | --- | --- |
| Claude Code CLI/Desktop | `claude` | yes |
| Claude Desktop local agent | `claude-desktop` | no; use show/export |
| ChatGPT Codex desktop and Codex CLI/IDE | `codex` | yes |
| Cursor | `cursor` | no; use show/export |
| Antigravity App/IDE/CLI | `antigravity` | no; use show/export |
| Pi coding agent | `pi` | yes |
| Prime Agent | `prime-agent` | yes |
| Google AI Studio | `aistudio` | no; use show/export |
| Gemini CLI | `gemini-cli` | `gemini --resume <id>` from the recorded project directory (main sessions only) |

Prime Agent is built on Pi but keeps its own root and child-run transcripts under
`~/.prime/agent`, so `--provider pi` never returns Prime sessions and the two keep separate IDs
and resume commands. Codex desktop and Codex CLI/IDE share `~/.codex`, so they index together.

The `codex` provider reads that local transcript format only. It does not parse Chat/Work
browser state or sync cloud history, which OpenAI documents as
[separate histories](https://help.openai.com/en/articles/20001276) in the merged desktop app.

These IDs are the TOML keys in provider configuration, and `aise config paths` prints each
provider's enabled state and effective roots.

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

# Keep each message's first 5 lines; use -5 for its last 5
aise messages get SESSION_ID --role user --lines-per-message 5

# Read the 75 most recent user turns. --order picks which 75; they print oldest-first
aise messages get SESSION_ID --role user --limit 75 --order newest

# Walk a long session in non-overlapping chunks instead of growing --limit
aise messages get SESSION_ID --seq-from 0 --seq-to 499
aise messages get SESSION_ID --seq-from 500 --seq-to 999
aise messages get SESSION_ID --seq-from 0 --seq-to 99 --kinds conversation,harness-notice

# One JSON object per version; --all requests the complete recorded history
aise files extract path/to/file.rs --all --format jsonl

# See which setting or default decided a surprising result
aise messages search "database lock" --query-mode fuzzy --limit 20 --receipt-level full --format json
```

Date bounds test a session's span, `created_at` through `updated_at`: `--since` needs it to end
on or after the bound, `--until` to start on or before, `--when` to overlap it. Spans can contain
gaps, so a week-long match may hold one edit. Message, file, and event analytics use each event's
timestamp.

With no configured operation or purpose default, omitting the limit on a Rust, CLI, or Python
message search returns every literal, regex, or no-text match; `--all-results` says so explicitly
in a script. Session-level and MCP searches
page by default. Fuzzy search always needs a finite page, and
`--limit 0` means the whole corpus only where that command's help says so.

Line windows share one sign convention: positive keeps the first N lines, negative the last N,
`0` everything. `aise show --transcript-lines` windows a whole rendered transcript;
`--lines-per-message` caps each returned message on its own. Both shorten what prints and nothing
else, so a long page stays skimmable with no hit lost: matching, ranking, result count,
pagination, and context membership are unaffected.

The index only narrows the scan: exact and regex modes recheck your pattern on every candidate,
so indexing cannot change your results. `--receipt-level summary` shows how a surprising query
was planned; `full` adds where each unpassed value came from. MCP returns the same receipt as
structured data.

<!-- aise-message-search-contract:start -->
See the [generated message-search reference](docs/README.md#generated-message-search-caller-contract) for shipped defaults, accepted values, and conflict rules. Run `aise messages search --describe --describe-surface cli` for active settings.
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

A bundle destination must not already exist, so a run never merges into or replaces one you
already have. `aise` publishes the complete directory in one atomic step and returns a receipt,
so an interruption leaves no half-written bundle.

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
use ai_session_search::SessionSearch;

fn main() -> ai_session_search::anyhow::Result<()> {
    let app = SessionSearch::load()?;
    // Apply the configured refresh policy before the first read, as `aise` does.
    app.index().prepare_for_read()?;
    let status = app.index().status()?;
    println!("{} repairs pending", status.repair_commands.len());
    Ok(())
}
```

Long native operations release the GIL so other Python threads can progress. The
application service API uses provider-neutral requests and results. Word counts are available through Python `vocabulary()` and Rust
`AnalysisService::vocabulary`; `aise vocab` also provides index-wide word counts. The
[library guide](docs/development/library-api.md) has the full API, feature flags, and concurrency
semantics.

## Configuration

Nothing needs configuring: with no file, every provider is enabled at its platform-default paths.
Google AI Studio is the exception, shipping no default root, so set `[providers.aistudio].paths`
to search it. `aise config init` writes `~/.ai-session-search/config.toml` with every setting
commented out but the database and cache paths, so later releases' defaults keep applying to
whatever you leave alone. `aise config example` previews it first.

```bash
aise config show      # effective settings
aise config origins   # where each setting came from
aise config paths     # resolved paths and per-provider roots
```

| Variable | Selects |
| --- | --- |
| `AI_SESSION_SEARCH_CONFIG` | TOML configuration file |
| `AI_SESSION_SEARCH_DATABASE` | SQLite index file |
| `AI_SESSION_SEARCH_CACHE_DIR` | Disposable cache directory |
| `AI_SESSION_SEARCH_THREADS` | Positive worker-thread count |
| `AI_SESSION_SEARCH_INDEX_REFRESH` | Index refresh policy for read commands: `auto`, `before-query`, or `existing-only` |
| `AI_SESSION_SEARCH_HARNESS_HOME` | Home directory under which `aise integrations` finds harness files |
| `AI_SESSION_SEARCH_SKIP_RELEASE_NOTIFICATION` | Set to `1` to skip the release notification and its network check |
| `AI_SESSION_SEARCH_MAX_TOOL_RESULT_CHARS` | Largest MCP tool result, in characters (MCP only) |
| `AI_SESSION_SEARCH_CLIENT_LIMIT_<ROW>` | Per-client MCP schema budget for one `aise mcp schema-budget` row |

Precedence runs from CLI argument, to environment variable, to TOML, to the platform default.

Search is unrestricted by default. To keep an agent out of your other projects, `[search.scope]`
confines it to absolute roots you list, the invocation directory, and live MCP client roots. That
mode refuses free-form `aise db query` SQL throughout, and when no authoritative root resolves it
fails and names the fix instead of searching wider. See
[search access scope](docs/development/configuration.md#search-access-scope). Legacy settings
import through `aise migrate config`.

## Index maintenance

Never copy a live SQLite index by hand, because the file alone omits its WAL state.
`aise migrate database` takes an online backup, verifies it, and publishes the destination
atomically, never overwriting a path that already exists.

```bash
aise migrate database --help   # copy an index to a new location safely
aise migrate verify --help     # check a source and destination against the receipt
aise migrate recover --help    # finish or roll back an interrupted migration
```

`aise doctor` reports exact allocated and reclaimable bytes, and names `aise compact` when
reclaiming them is worth an exclusive lock and temporary disk space. Migration never vacuums the
source, so run `aise compact` against the published destination.

## Command reference

`aise COMMAND --help` documents each one in full.

| Command group | What it covers |
| --- | --- |
| `aise list`, `search`, `show`, `resume` | Find, read, and re-enter sessions |
| `aise messages search\|get\|timeline\|evidence` | Query normalized turns and tool evidence |
| `aise files search\|history\|cross-ref\|extract` | Locate and reconstruct edited files |
| `aise skills corrections`, `aise slash-commands`, `aise stats` | Deterministic message classification and indexed behavioral summaries |
| `aise skills list\|show\|validate\|create\|update\|restore\|run` | Inspect, author, repair, and run skill packages |
| `aise repeats` | Recurring phrases; `--max-candidates` bounds examined messages, `--max-groups` bounds returned groups, and `--query-mode regex` selects regex matching |
| `aise export` | Render one session, or publish an explicitly selected bundle |
| `aise analyze` | Apply a validated policy and publish an immutable analysis bundle |
| `aise reindex`, `compact`, `doctor` | Maintain and diagnose the index |
| `aise migrate database\|config\|verify\|recover` | Verified, reversible migration |
| `aise config file\|example\|init\|show\|origins\|paths\|check` | Inspect or initialize TOML configuration and resolved paths |
| `aise package status\|check\|update` | Package ownership, release checks, and manager-driven updates |
| `aise integrations install\|status\|uninstall\|recover`, `aise mcp serve` | Aliases, MCP registrations, owned instructions and skills, and the MCP transport |
| `aise db schema\|query` | Read-only SQL. `aise db schema` prints the tables; `aise db query --help` lists the columns whose stored values differ from the CLI's spelling, where a wrong guess returns no rows and no error |
| `aise tui` | Interactive browser: typeahead search, preview scroll, and filter keys (p provider, f class, s window, w warnings) |
| `aise dates` | The accepted date and duration families, with examples |

## Architecture

One Rust implementation backs the CLI, the MCP server, the Rust crate, and the Python package,
over one index lifecycle and one provider registry, so a new provider becomes searchable on all
four at once.

Rust owns parsing, querying, migration, and filesystem publication. Filtering by provider,
session, path, date, role, message kind,
tool, sequence, or canonical tool-argument JSON pointer runs inside the index, and one `--kinds`
filter owns message-class selection.

Reconstruction holds one version at a time, so `--all` never accumulates them;
`files extract --restore` writes a `.recovered` sibling and never overwrites; export and analysis
bundles publish atomically and are never replaced, and analysis bundles checksum every artifact.

## Install details and uninstalling

Other paths to the same build: `uvx --from ai-session-search aise --help` runs it once without
installing, `uv add ai-session-search` adds a project dependency, and
`python -m pip install ai-session-search` also works.

Wheels cover CPython 3.12 through 3.14 on manylinux2014 x86_64 and aarch64, macOS x86_64 and
arm64, and Windows x86_64. Building from source needs Rust 1.89 or newer and a C compiler,
because SQLite is built from bundled C.

Installing the package never touches your configuration: no aliases, no MCP client edits, no
instruction files or skills, and not one transcript read. `aise integrations install` is the
second step that does all of that, adds the `aisearch` and `ai_session_search` aliases, and
starts building the index in the background. Run `aise doctor` afterward for readiness and
recovery guidance.

After an upgrade, the first ordinary `aise` command refreshes the skill and instructions it
installed and migrates recognized MCP registrations even without a skill install, preserving
launch arguments; explicit `aise integrations install` regenerates those arguments.
Unrecognized entries stay unchanged and `aise integrations status` reports them; empty checks
are recorded once per version when possible. Set `[integrations] refresh_after_upgrade = false`
to turn automatic refresh off.

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

[CONTRIBUTING.md](CONTRIBUTING.md) covers builds, the local verification gate, and review.
[CHANGELOG.md](CHANGELOG.md) describes release changes; the
[documentation index](docs/README.md) links user guides and maintainer references.

## License

Apache License 2.0. Third-party dependencies keep their own licenses, and release artifacts
ship dependency inventories and software bills of materials.
