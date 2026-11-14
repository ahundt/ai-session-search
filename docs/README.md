# AI Session Search documentation

The repository [README](../README.md) covers what `aise` does and a quick start.

## Using aise

| Task | Guide |
| --- | --- |
| Install, verify, update, or uninstall `aise` and its MCP integrations | [Installation](development/installation.md) |
| Resolve CLI, environment, TOML, Rust, Python, and MCP settings, or find which source won with `aise config origins` | [Configuration](development/configuration.md) |
| Call AI Session Search from Python or Rust | [Library guide](development/library-api.md) |
| Report a bug, or submit a change | [CONTRIBUTING.md](../CONTRIBUTING.md) |

## Maintaining and releasing aise

| Task | Guide |
| --- | --- |
| Run a release, start to finish | [RELEASING.md](../RELEASING.md) |
| Change how packaging, publishing, or the supported target matrix works | [Releasing: design decisions](development/releasing-design-decisions.md) |
| Preserve cumulative product contracts while changing the repository | [Maintainer requirements and design decisions](development/maintainer-requirements-and-design-decisions.md) |
| Change the integration installer, configuration resolution, or their acceptance tests | [Installer and configuration maintainer contracts](development/maintainer-contracts-for-install-and-config.md) |
| Re-check client-imposed MCP limits when a harness or schema changes | [MCP client limits and measured evidence](development/mcp-client-limits-and-measured-evidence.md) |

A release starts at `RELEASING.md`'s "Before tagging, confirm" list, not at the tag.

## Architecture and migration records

Architecture records describe ownership and provenance; use the guides above for current workflows.

| Record | Purpose |
| --- | --- |
| [Major migration](migration/ai-session-search-major-migration.md) | Completed Rust migration and current reference links |
| [Capability parity](migration/capability-parity.md) | Supported capability routes |
| [Rust/Python API architecture](migration/rust-python-api-architecture.md) | Public API boundaries and distribution contract |
| [Provenance](migration/provenance.md) | Source histories, transformation, licensing, and credit |
| [1.0.0 release candidates](history/changelog-1.0.0-release-candidates.md) | Changelog entries for 1.0.0rc1 to 1.0.0rc4, moved out of CHANGELOG.md |

<!-- aise-message-search-contract:start -->
## Generated message-search caller contract

Search indexed AI-session messages while separating result selection, context, presentation, optional payloads, and receipts.

### Shipped defaults by caller surface

| Caller surface | Omitted non-fuzzy result extent | Context | Lines per message | Field view | Match view | Includes / receipt |
| --- | --- | --- | ---: | --- | --- | --- |
| rust | all results; offset 0 | 0 before / 0 after | 0 | `{"kind":"no_char_limit"}` | `{"kind":"max_chars","max_chars":220}` | `[]` / `none` |
| cli | all results; offset 0 | 0 before / 0 after | 0 | `{"kind":"no_char_limit"}` | `{"kind":"max_chars","max_chars":220}` | `[]` / `none` |
| mcp | page of 15; offset 0 | 0 before / 0 after | 0 | `{"kind":"max_chars","max_chars":220}` | `{"kind":"max_chars","max_chars":220}` | `["normalized_session_metadata"]` / `none` |
| python | all results; offset 0 | 0 before / 0 after | 0 | `{"kind":"no_char_limit"}` | `{"kind":"max_chars","max_chars":220}` | `[]` / `none` |

These are shipped defaults from an empty configuration. Positive `limit` counts result rows; signed `lines_per_message` keeps the first N lines, the last N for a negative value, or all lines for 0.

### Closed vocabularies

| Canonical parameter | Accepted values | Omission semantics |
| --- | --- | --- |
| `query_mode` | `literal`, `regex`, `fuzzy` | `typed_default` |
| `field` | `content`, `tool_name`, `tool_argument` | `typed_default` |
| `role` | `user`, `assistant`, `tool`, `slash`, `compaction` | `all_eligible` |
| `kinds` | `conversation`, `compaction`, `tool_call`, `tool_result`, `harness_notice`, `unknown` | `typed_default` |
| `providers` | `claude`, `claude-desktop`, `codex`, `cursor`, `antigravity`, `pi`, `prime-agent`, `aistudio`, `gemini-cli` | `all_eligible` |
| `match_window` | `earliest`, `latest` | `typed_default` |
| `detail` | `compact`, `full` | `surface_policy` |
| `field_view` | `no_char_limit`, `max_chars {max_chars: positive_character_count}` | `surface_policy` |
| `match_view` | `minimal_span`, `max_chars {max_chars: positive_character_count}` | `typed_default` |
| `receipt_level` | `none`, `summary`, `full` | `typed_default` |
| `include` | `normalized_session_metadata`, `parsed_references`, `raw_provider_metadata`, `runtime_diagnostics` | `typed_default` |

### Executable conflict rules

- `detail_owns_presentation_budgets` — detail conflicts with lines_per_message, field_view, and match_view; omit detail to compose custom presentation budgets.
- `sequence_requires_session` — sequence bounds require one session.
- `kinds_must_remain_satisfiable` — the selected kinds exclude every message class, so nothing can match.
- `compaction_role_requires_compaction_kind` — role=compaction requires compaction among the selected kinds; include_compaction=false or a kinds set without it removes every match.
- `tool_argument_requires_tool_call_kind` — tool-argument target requires tool_call among the selected kinds.
- `match_view_requires_query` — match_view requires a literal, regex, or fuzzy query.
- `fuzzy_rejects_match_window` — match_window does not apply to fuzzy queries.
- `latest_window_requires_session` — match_window=latest requires one session.
- `fuzzy_rejects_all_results` — fuzzy search does not support all_results.

Default precedence, highest first: `explicit` → `detail_preset` → `purpose` → `operation_config` → `surface_config` → `typed_default` → `derived`.

The full configured catalogue is available from `aise messages search --describe`; MCP clients receive the same canonical parameter identities and planner-resolved MCP defaults in `tools/list`. Ordinary search responses contain only the compact effective request needed to interpret that response.
<!-- aise-message-search-contract:end -->
