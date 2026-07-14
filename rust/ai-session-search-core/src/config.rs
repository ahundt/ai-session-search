use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

use crate::util::expand_tilde;

pub const CONFIG_EXAMPLE_TOML: &str = include_str!("../config.example.toml");

pub const DEFAULT_MCP_SEARCH_SESSIONS_LIMIT: usize = 10;
pub const DEFAULT_MCP_LIST_SESSIONS_LIMIT: usize = 20;
pub const DEFAULT_MCP_ANALYZE_SESSIONS_LIMIT: usize = DEFAULT_MCP_SEARCH_SESSIONS_LIMIT;
pub const DEFAULT_MCP_SEARCH_MESSAGES_LIMIT: usize = 20;
pub const DEFAULT_MCP_GET_SESSION_TRANSCRIPT_LINES: i64 = -40;
pub const DEFAULT_MCP_PREVIEW_CHARS: usize = crate::inspect::DEFAULT_PREVIEW_CHARS;
pub const DEFAULT_MCP_QUERY_MAX_CELL_CHARS: usize = crate::sql_query::DEFAULT_MCP_MAX_CELL_CHARS;
pub const DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_TABLES: usize = 4;
pub const DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_COLUMNS: usize = 12;
pub const DEFAULT_CLI_SHOW_TRANSCRIPT_LINES: i64 = -40;
pub const DEFAULT_CLI_EVIDENCE_PREVIEW_CHARS: usize = crate::inspect::DEFAULT_PREVIEW_CHARS;
pub const DEFAULT_DB_QUERY_LIMIT: usize = crate::sql_query::DEFAULT_LIMIT;
pub const DEFAULT_DB_QUERY_TIMEOUT_MS: u64 = crate::sql_query::DEFAULT_TIMEOUT_MS;
pub const DEFAULT_ANALYTICS_VOCAB_LIMIT: usize = 50;
pub const DEFAULT_ANALYTICS_REPEAT_MAX_GROUPS: usize = 50;
pub const DEFAULT_ANALYTICS_REPEAT_MIN_MATCHES: usize = 2;
pub const DEFAULT_ANALYTICS_REPEAT_PHRASE_MIN_WORDS: usize = 2;
pub const DEFAULT_ANALYTICS_REPEAT_PHRASE_MAX_WORDS: usize = 5;

#[derive(Debug, Clone, Serialize)]
pub struct Config {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub analytics: AnalyticsConfig,
    #[serde(default)]
    pub performance: PerformanceConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub cli: CliConfig,
    #[serde(default)]
    pub db: DbConfig,
}

/// Per-invocation configuration overrides. `None` preserves lower-precedence sources.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub config_path: Option<PathBuf>,
    pub database_path: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub threads: Option<usize>,
}

/// Process environment captured once so precedence can be tested without mutating global state.
#[derive(Debug, Clone, Default)]
pub struct ConfigEnvironment {
    pub config_path: Option<PathBuf>,
    pub database_path: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub threads: Option<String>,
    pub legacy_threads: Option<String>,
}

impl ConfigEnvironment {
    pub fn capture() -> Self {
        Self {
            config_path: nonempty_env_path("AI_SESSION_SEARCH_CONFIG"),
            database_path: nonempty_env_path("AI_SESSION_SEARCH_DATABASE"),
            cache_dir: nonempty_env_path("AI_SESSION_SEARCH_CACHE_DIR"),
            threads: nonempty_env_string("AI_SESSION_SEARCH_THREADS"),
            legacy_threads: nonempty_env_string("AISE_THREADS"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigOrigins {
    pub config: String,
    pub database: String,
    pub cache: String,
    pub threads: String,
}

/// Validated effective configuration plus provenance and non-fatal compatibility diagnostics.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub config: Config,
    pub config_path: PathBuf,
    pub origins: ConfigOrigins,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    providers: Option<ProvidersFile>,
    index: Option<IndexConfig>,
    ui: Option<UiConfig>,
    search: Option<SearchConfig>,
    analytics: Option<AnalyticsConfig>,
    performance: Option<PerformanceConfig>,
    mcp: Option<McpConfig>,
    cli: Option<CliConfig>,
    db: Option<DbConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProvidersFile {
    claude: Option<ProviderFile>,
    #[serde(rename = "claude-desktop")]
    claude_desktop: Option<ProviderFile>,
    codex: Option<ProviderFile>,
    cursor: Option<ProviderFile>,
    antigravity: Option<ProviderFile>,
    pi: Option<ProviderFile>,
    #[serde(rename = "ai-studio", alias = "aistudio")]
    aistudio: Option<ProviderFile>,
    #[serde(rename = "gemini-cli")]
    gemini_cli: Option<ProviderFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderFile {
    enabled: Option<bool>,
    paths: Option<Vec<String>>,
}

impl ProviderFile {
    fn apply(self, provider: &mut ProviderConfig) {
        if let Some(enabled) = self.enabled {
            provider.enabled = enabled;
        }
        if let Some(paths) = self.paths {
            provider.paths = paths;
        }
    }
}

impl ConfigFile {
    fn into_config(self) -> Config {
        let mut config = Config::default();
        if let Some(providers) = self.providers {
            if let Some(value) = providers.claude {
                value.apply(&mut config.providers.claude);
            }
            if let Some(value) = providers.claude_desktop {
                value.apply(&mut config.providers.claude_desktop);
            }
            if let Some(value) = providers.codex {
                value.apply(&mut config.providers.codex);
            }
            if let Some(value) = providers.cursor {
                value.apply(&mut config.providers.cursor);
            }
            if let Some(value) = providers.antigravity {
                value.apply(&mut config.providers.antigravity);
            }
            if let Some(value) = providers.pi {
                value.apply(&mut config.providers.pi);
            }
            if let Some(value) = providers.aistudio {
                value.apply(&mut config.providers.aistudio);
            }
            if let Some(value) = providers.gemini_cli {
                value.apply(&mut config.providers.gemini_cli);
            }
        }
        if let Some(mut value) = self.index {
            let defaults = IndexConfig::default();
            value.db_path = value.db_path.or(defaults.db_path);
            value.cache_dir = value.cache_dir.or(defaults.cache_dir);
            config.index = value;
        }
        if let Some(value) = self.ui {
            config.ui = value;
        }
        if let Some(value) = self.search {
            config.search = value;
        }
        if let Some(value) = self.analytics {
            config.analytics = value;
        }
        if let Some(value) = self.performance {
            config.performance = value;
        }
        if let Some(value) = self.mcp {
            config.mcp = value;
        }
        if let Some(value) = self.cli {
            config.cli = value;
        }
        if let Some(value) = self.db {
            config.db = value;
        }
        config.normalize_legacy_fields();
        config
    }
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(ConfigFile::deserialize(deserializer)?.into_config())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub claude: ProviderConfig,
    #[serde(default, rename = "claude-desktop")]
    pub claude_desktop: ProviderConfig,
    #[serde(default)]
    pub codex: ProviderConfig,
    #[serde(default)]
    pub cursor: ProviderConfig,
    #[serde(default)]
    pub antigravity: ProviderConfig,
    #[serde(default)]
    pub pi: ProviderConfig,
    #[serde(default, rename = "ai-studio", alias = "aistudio")]
    pub aistudio: ProviderConfig,
    #[serde(default, rename = "gemini-cli")]
    pub gemini_cli: ProviderConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    pub db_path: Option<String>,
    pub cache_dir: Option<String>,
    /// SQLite busy timeout in milliseconds. Applies while opening/initializing the DB too, so
    /// normal concurrent CLI/MCP use waits briefly for another writer instead of failing.
    #[serde(default = "default_busy_timeout_ms")]
    pub busy_timeout_ms: u64,
    /// Busy timeout used only for automatic pre-read reindex refreshes. When it expires on writer
    /// contention, read commands serve the existing index instead of failing.
    #[serde(default = "default_auto_reindex_busy_timeout_ms")]
    pub auto_reindex_busy_timeout_ms: u64,
    /// Cross-process interval after a successful automatic refresh where read commands skip
    /// auto-reindex entirely and stay read-only.
    #[serde(default = "default_auto_reindex_interval_ms")]
    pub auto_reindex_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    #[serde(default = "default_preview_lines")]
    pub preview_lines: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    #[serde(default = "default_limit")]
    pub default_limit: usize,
    #[serde(default = "default_true")]
    pub prefer_current_repo: bool,
    /// Fuzzy-ranker weights. The defaults are tuned; override only to retune relevance.
    #[serde(default)]
    pub scoring: ScoringConfig,
}

/// Tunable weights for the session search ranker (`[search.scoring]` in config.toml).
/// Every field defaults to the value the ranker shipped with, so an absent or partial
/// `[search.scoring]` table leaves ranking byte-for-byte unchanged — you should rarely
/// need to set any of these. A field contributes its weight when the lowercased query is
/// a substring of that haystack; `token_bonus` is added per query token found in a
/// haystack, `all_tokens_bonus` once when every token matched somewhere, recency adds
/// `(recency_max_days - age_days).max(0) * recency_weight`, and `current_repo_bonus` is
/// added when a session's repo matches the current one.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScoringConfig {
    #[serde(default = "default_title_score")]
    pub title_score: i64,
    #[serde(default = "default_summary_score")]
    pub summary_score: i64,
    /// Weight for a cwd or repo-root substring match.
    #[serde(default = "default_path_score")]
    pub path_score: i64,
    #[serde(default = "default_preview_score")]
    pub preview_score: i64,
    /// Weight for any other haystack (e.g. the transcript body).
    #[serde(default = "default_other_score")]
    pub other_score: i64,
    #[serde(default = "default_token_bonus")]
    pub token_bonus: i64,
    #[serde(default = "default_all_tokens_bonus")]
    pub all_tokens_bonus: i64,
    #[serde(default = "default_recency_weight")]
    pub recency_weight: i64,
    #[serde(default = "default_recency_max_days")]
    pub recency_max_days: i64,
    #[serde(default = "default_current_repo_bonus")]
    pub current_repo_bonus: i64,
    /// FTS candidate set size = `max(limit * fts_candidate_multiplier, fts_candidate_floor)`.
    /// A generous candidate pool lets a high-fuzzy-score session that ranks low under raw
    /// FTS `rank` still be considered.
    #[serde(default = "default_fts_candidate_multiplier")]
    pub fts_candidate_multiplier: usize,
    #[serde(default = "default_fts_candidate_floor")]
    pub fts_candidate_floor: usize,
}

/// Analytics defaults and overrides (`[analytics]` in config.toml). Corrections have narrowed
/// built-in defaults; repeats are data-driven phrase mining.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyticsConfig {
    /// `corrections`: when non-empty, fully replaces the built-in correction categories.
    /// Each entry is `"CATEGORY:REGEX"` (repeatable; same-category entries are ORed).
    /// Empty = use the narrowed built-in categories.
    #[serde(default)]
    pub correction_patterns: Vec<String>,
    /// `planning`: when non-empty, restricts the count to slash commands whose token
    /// matches one of these (case-insensitive) regexes. Empty = count every slash command.
    #[serde(default)]
    pub planning_commands: Vec<String>,
    /// Default `aise vocab --limit`. `0` means unlimited.
    #[serde(default = "default_analytics_vocab_limit")]
    pub vocab_limit: usize,
    /// Default `aise repeats --max-groups`. `0` means all groups.
    #[serde(default = "default_analytics_repeat_max_groups")]
    pub repeat_max_groups: usize,
    /// Default `aise repeats --min-matches`. Must be at least 1.
    #[serde(default = "default_analytics_repeat_min_matches")]
    pub repeat_min_matches: usize,
    /// Default `aise repeats --phrase-min-words`. Must be at least 1.
    #[serde(default = "default_analytics_repeat_phrase_min_words")]
    pub repeat_phrase_min_words: usize,
    /// Default `aise repeats --phrase-max-words`. Must be >= `repeat_phrase_min_words`.
    #[serde(default = "default_analytics_repeat_phrase_max_words")]
    pub repeat_phrase_max_words: usize,
}

/// Parallelism overrides (`[performance]` in config.toml). `threads` controls the worker
/// count for data-parallel CPU-bound scans (e.g. `corrections`). `0` (the default) means
/// auto-detect from the host (`std::thread::available_parallelism`), so it adapts to any
/// machine with no configuration. See [`Config::resolve_threads`] for the override chain.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PerformanceConfig {
    /// Worker threads for parallel scans. `0` = auto (all available cores); `1` = sequential.
    #[serde(default)]
    pub threads: usize,
    /// Corpus-size threshold (message rows) at/above which a filtered regex search still uses the
    /// trigram prefilter; below it a direct scan of the filtered slice is faster. `0` = built-in
    /// default (50,000). Tune lower on small machines, higher if direct scans feel slow.
    #[serde(default)]
    pub regex_prefilter_min_corpus: usize,
    /// Max newer-than-base messages allowed before the custom trigram base index is rebuilt in
    /// parallel; until then the delta is direct-scanned. `0` = built-in default (50,000).
    #[serde(default)]
    pub trigram_rebuild_delta: usize,
}

/// Agent-facing MCP defaults (`[mcp]` in config.toml). These affect default tool-call behavior
/// only when the MCP client omits the matching parameter; explicit tool arguments still win. They
/// matter because MCP responses are usually copied straight into an agent's context window.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Default `search_sessions.limit`: session-level search page size. Does not affect CLI
    /// `aise search`, which uses `[search].default_limit`.
    #[serde(default = "default_mcp_search_sessions_limit")]
    pub search_sessions_limit: usize,
    /// Default `list_sessions.limit`: recent-session page size. Does not affect CLI
    /// `aise list`, which uses `[search].default_limit`.
    #[serde(default = "default_mcp_list_sessions_limit")]
    pub list_sessions_limit: usize,
    /// Default `analyze_sessions.limit`: selected analysis corpus size. `0` explicitly selects
    /// every matching session and can produce a large response. Does not affect CLI analysis.
    #[serde(default = "default_mcp_analyze_sessions_limit")]
    pub analyze_sessions_limit: usize,
    /// Default `search_messages.limit`: message-hit page size. Must be at least 1 so pagination
    /// always makes progress. Does not affect CLI `aise messages search`.
    #[serde(
        default = "default_mcp_search_messages_limit",
        alias = "message_search_limit"
    )]
    pub search_messages_limit: usize,
    /// Default `get_session.transcript_lines`: positive=head, negative=tail,
    /// 0=entire transcript. Does not affect `get_session` calls that pass `message_seq`.
    #[serde(default = "default_mcp_get_session_transcript_lines")]
    pub get_session_transcript_lines: i64,
    /// Default `preview_chars` for concise MCP hit/context previews and `get_session` summary or
    /// focused-message output. Explicit MCP tool-call `preview_chars` values still win. Does not
    /// affect transcript output. Must be at least 1.
    #[serde(default = "default_mcp_preview_chars")]
    pub preview_chars: usize,
    /// Default `query_session_index.max_cell_chars`: truncates long string cells in MCP JSON
    /// responses only. It does not change SQL execution or CLI `aise db query` output.
    /// `0` disables MCP string-cell truncation.
    #[serde(default = "default_mcp_query_max_cell_chars")]
    pub query_max_cell_chars: usize,
    /// Internal MCP presentation budgets. These affect only generated tool descriptions, not
    /// search/query results. Leave unchanged unless the schema summary is too large/small for your
    /// MCP client.
    #[serde(default)]
    pub internal: McpInternalConfig,
    /// Deprecated flat `[mcp] schema_summary_tables`; deserialized for compatibility, then moved
    /// into `[mcp.internal]`. Skipped on serialization so `config show` prints the canonical shape.
    #[serde(default, skip_serializing)]
    pub schema_summary_tables: Option<usize>,
    /// Deprecated flat `[mcp] schema_summary_columns`; see `schema_summary_tables`.
    #[serde(default, skip_serializing)]
    pub schema_summary_columns: Option<usize>,
}

/// Internal MCP presentation budgets (`[mcp.internal]`). These exist to keep tool descriptions
/// concise while still giving agents enough live schema context to form valid SQL.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpInternalConfig {
    /// Number of schema objects shown in the `query_session_index` tool description.
    #[serde(default = "default_mcp_internal_schema_summary_tables")]
    pub schema_summary_tables: usize,
    /// Number of columns per schema object shown in the `query_session_index` tool description.
    #[serde(default = "default_mcp_internal_schema_summary_columns")]
    pub schema_summary_columns: usize,
}

/// CLI defaults (`[cli]`). These affect command-line behavior only when the flag is omitted.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CliConfig {
    /// Default `aise show --transcript-lines`: positive=head, negative=tail, 0=entire transcript.
    /// A bounded default keeps long sessions skimmable; pass `--transcript-lines 0` explicitly
    /// when the full transcript is needed.
    #[serde(default = "default_cli_show_transcript_lines")]
    pub show_transcript_lines: i64,
    /// Default `aise messages evidence --preview-chars`. This affects only compact
    /// evidence previews; JSON message search/get output still keeps full message content. Must be
    /// at least 1.
    #[serde(default = "default_cli_evidence_preview_chars")]
    pub evidence_preview_chars: usize,
}

/// Raw SQLite query defaults (`[db]`). Applies to `aise db query` and MCP
/// `query_session_index` when callers omit the corresponding argument. These are safety defaults
/// for ad hoc SQL; they do not affect indexed search APIs such as `search_messages`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbConfig {
    /// Default maximum rows for read-only SQL. `0` means unlimited and can produce huge output.
    #[serde(default = "default_db_query_limit")]
    pub query_limit: usize,
    /// Default read-only SQL timeout in milliseconds. `0` disables interruption.
    #[serde(default = "default_db_query_timeout_ms")]
    pub query_timeout_ms: u64,
}

fn default_true() -> bool {
    true
}

fn default_limit() -> usize {
    50
}

fn default_preview_lines() -> usize {
    30
}

fn default_busy_timeout_ms() -> u64 {
    crate::db::DEFAULT_BUSY_TIMEOUT_MS
}

fn default_auto_reindex_busy_timeout_ms() -> u64 {
    crate::db::DEFAULT_AUTO_REINDEX_BUSY_TIMEOUT_MS
}

fn default_auto_reindex_interval_ms() -> u64 {
    crate::db::DEFAULT_AUTO_REINDEX_INTERVAL_MS
}

fn default_mcp_search_sessions_limit() -> usize {
    DEFAULT_MCP_SEARCH_SESSIONS_LIMIT
}
fn default_mcp_list_sessions_limit() -> usize {
    DEFAULT_MCP_LIST_SESSIONS_LIMIT
}
fn default_mcp_analyze_sessions_limit() -> usize {
    DEFAULT_MCP_ANALYZE_SESSIONS_LIMIT
}
fn default_mcp_search_messages_limit() -> usize {
    DEFAULT_MCP_SEARCH_MESSAGES_LIMIT
}
fn default_mcp_get_session_transcript_lines() -> i64 {
    DEFAULT_MCP_GET_SESSION_TRANSCRIPT_LINES
}
fn default_mcp_preview_chars() -> usize {
    DEFAULT_MCP_PREVIEW_CHARS
}
fn default_mcp_query_max_cell_chars() -> usize {
    DEFAULT_MCP_QUERY_MAX_CELL_CHARS
}
fn default_mcp_internal_schema_summary_tables() -> usize {
    DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_TABLES
}
fn default_mcp_internal_schema_summary_columns() -> usize {
    DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_COLUMNS
}
fn default_cli_show_transcript_lines() -> i64 {
    DEFAULT_CLI_SHOW_TRANSCRIPT_LINES
}
fn default_cli_evidence_preview_chars() -> usize {
    DEFAULT_CLI_EVIDENCE_PREVIEW_CHARS
}
fn default_db_query_limit() -> usize {
    DEFAULT_DB_QUERY_LIMIT
}
fn default_db_query_timeout_ms() -> u64 {
    DEFAULT_DB_QUERY_TIMEOUT_MS
}
fn default_analytics_vocab_limit() -> usize {
    DEFAULT_ANALYTICS_VOCAB_LIMIT
}
fn default_analytics_repeat_max_groups() -> usize {
    DEFAULT_ANALYTICS_REPEAT_MAX_GROUPS
}
fn default_analytics_repeat_min_matches() -> usize {
    DEFAULT_ANALYTICS_REPEAT_MIN_MATCHES
}
fn default_analytics_repeat_phrase_min_words() -> usize {
    DEFAULT_ANALYTICS_REPEAT_PHRASE_MIN_WORDS
}
fn default_analytics_repeat_phrase_max_words() -> usize {
    DEFAULT_ANALYTICS_REPEAT_PHRASE_MAX_WORDS
}
fn default_title_score() -> i64 {
    600
}
fn default_summary_score() -> i64 {
    450
}
fn default_path_score() -> i64 {
    350
}
fn default_preview_score() -> i64 {
    250
}
fn default_other_score() -> i64 {
    100
}
fn default_token_bonus() -> i64 {
    40
}
fn default_all_tokens_bonus() -> i64 {
    150
}
fn default_recency_weight() -> i64 {
    2
}
fn default_recency_max_days() -> i64 {
    90
}
fn default_current_repo_bonus() -> i64 {
    200
}
fn default_fts_candidate_multiplier() -> usize {
    5
}
fn default_fts_candidate_floor() -> usize {
    crate::db::FTS_CANDIDATE_FLOOR
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            title_score: default_title_score(),
            summary_score: default_summary_score(),
            path_score: default_path_score(),
            preview_score: default_preview_score(),
            other_score: default_other_score(),
            token_bonus: default_token_bonus(),
            all_tokens_bonus: default_all_tokens_bonus(),
            recency_weight: default_recency_weight(),
            recency_max_days: default_recency_max_days(),
            current_repo_bonus: default_current_repo_bonus(),
            fts_candidate_multiplier: default_fts_candidate_multiplier(),
            fts_candidate_floor: default_fts_candidate_floor(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let home = home_dir_fallback();
        Self {
            providers: ProvidersConfig {
                claude: ProviderConfig {
                    enabled: true,
                    paths: vec![home.join(".claude/projects").to_string_lossy().to_string()],
                },
                claude_desktop: ProviderConfig {
                    enabled: true,
                    paths: default_claude_desktop_paths(),
                },
                codex: ProviderConfig {
                    enabled: true,
                    paths: vec![home.join(".codex/sessions").to_string_lossy().to_string()],
                },
                cursor: ProviderConfig {
                    enabled: true,
                    paths: vec![home.join(".cursor/projects").to_string_lossy().to_string()],
                },
                antigravity: ProviderConfig {
                    enabled: true,
                    paths: default_antigravity_paths(),
                },
                pi: ProviderConfig {
                    enabled: true,
                    paths: vec![home
                        .join(".pi/agent/sessions")
                        .to_string_lossy()
                        .to_string()],
                },
                aistudio: ProviderConfig {
                    enabled: true,
                    paths: Vec::new(),
                },
                gemini_cli: ProviderConfig {
                    enabled: true,
                    paths: vec![home.join(".gemini/tmp").to_string_lossy().to_string()],
                },
            },
            index: IndexConfig {
                db_path: Some(default_db_path().to_string_lossy().into_owned()),
                cache_dir: Some(default_cache_dir().to_string_lossy().into_owned()),
                busy_timeout_ms: default_busy_timeout_ms(),
                auto_reindex_busy_timeout_ms: default_auto_reindex_busy_timeout_ms(),
                auto_reindex_interval_ms: default_auto_reindex_interval_ms(),
            },
            ui: UiConfig { preview_lines: 30 },
            search: SearchConfig {
                default_limit: 50,
                prefer_current_repo: true,
                scoring: ScoringConfig::default(),
            },
            analytics: AnalyticsConfig::default(),
            performance: PerformanceConfig::default(),
            mcp: McpConfig::default(),
            cli: CliConfig::default(),
            db: DbConfig::default(),
        }
    }
}

fn default_claude_desktop_paths() -> Vec<String> {
    let mut paths = Vec::new();
    if cfg!(target_os = "macos") {
        if let Some(home) = dirs::home_dir() {
            push_unique_path(
                &mut paths,
                home.join("Library/Application Support/Claude/local-agent-mode-sessions"),
            );
        }
    }
    if let Some(config_dir) = dirs::config_dir() {
        push_unique_path(
            &mut paths,
            config_dir.join("Claude/local-agent-mode-sessions"),
        );
        push_unique_path(
            &mut paths,
            config_dir.join("claude/local-agent-mode-sessions"),
        );
    }
    if let Some(data_dir) = dirs::data_dir() {
        push_unique_path(
            &mut paths,
            data_dir.join("Claude/local-agent-mode-sessions"),
        );
    }
    if let Some(data_local_dir) = dirs::data_local_dir() {
        push_unique_path(
            &mut paths,
            data_local_dir.join("Claude/local-agent-mode-sessions"),
        );
    }
    paths
}

fn default_antigravity_paths() -> Vec<String> {
    let home = home_dir_fallback();
    let mut paths = Vec::new();
    push_unique_path(&mut paths, home.join(".gemini/antigravity-cli/brain"));
    push_unique_path(&mut paths, home.join(".gemini/antigravity/brain"));
    paths
}

fn push_unique_path(paths: &mut Vec<String>, path: PathBuf) {
    let value = path.to_string_lossy().to_string();
    if !paths.iter().any(|existing| existing == &value) {
        paths.push(value);
    }
}

impl Config {
    /// Resolve the already-merged worker-thread setting, falling back to host parallelism.
    /// Always returns `>= 1`. `1` means run sequentially (single worker).
    pub fn resolve_threads(&self) -> usize {
        if self.performance.threads > 0 {
            return self.performance.threads;
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Config::default().providers
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: Vec::new(),
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Config::default().index
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Config::default().ui
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Config::default().search
    }
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            correction_patterns: Vec::new(),
            planning_commands: Vec::new(),
            vocab_limit: default_analytics_vocab_limit(),
            repeat_max_groups: default_analytics_repeat_max_groups(),
            repeat_min_matches: default_analytics_repeat_min_matches(),
            repeat_phrase_min_words: default_analytics_repeat_phrase_min_words(),
            repeat_phrase_max_words: default_analytics_repeat_phrase_max_words(),
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            search_sessions_limit: default_mcp_search_sessions_limit(),
            list_sessions_limit: default_mcp_list_sessions_limit(),
            analyze_sessions_limit: default_mcp_analyze_sessions_limit(),
            search_messages_limit: default_mcp_search_messages_limit(),
            get_session_transcript_lines: default_mcp_get_session_transcript_lines(),
            preview_chars: default_mcp_preview_chars(),
            query_max_cell_chars: default_mcp_query_max_cell_chars(),
            internal: McpInternalConfig::default(),
            schema_summary_tables: None,
            schema_summary_columns: None,
        }
    }
}

impl Default for McpInternalConfig {
    fn default() -> Self {
        Self {
            schema_summary_tables: default_mcp_internal_schema_summary_tables(),
            schema_summary_columns: default_mcp_internal_schema_summary_columns(),
        }
    }
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            show_transcript_lines: default_cli_show_transcript_lines(),
            evidence_preview_chars: default_cli_evidence_preview_chars(),
        }
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            query_limit: default_db_query_limit(),
            query_timeout_ms: default_db_query_timeout_ms(),
        }
    }
}

impl Config {
    pub fn selected_config_path(override_path: Option<PathBuf>) -> PathBuf {
        override_path
            .or_else(|| nonempty_env_path("AI_SESSION_SEARCH_CONFIG"))
            .map_or_else(Self::config_path, expand_override_path)
    }

    pub fn load() -> Result<Self> {
        Ok(Self::resolve(ConfigOverrides::default())?.config)
    }

    pub fn resolve(overrides: ConfigOverrides) -> Result<ResolvedConfig> {
        Self::resolve_with_environment(overrides, ConfigEnvironment::capture())
    }

    pub fn resolve_with_environment(
        overrides: ConfigOverrides,
        environment: ConfigEnvironment,
    ) -> Result<ResolvedConfig> {
        let (config_path, config_origin, explicit_config_path) =
            if let Some(path) = overrides.config_path {
                (expand_override_path(path), "cli --config".to_string(), true)
            } else if let Some(path) = environment.config_path {
                (
                    expand_override_path(path),
                    "environment AI_SESSION_SEARCH_CONFIG".to_string(),
                    true,
                )
            } else {
                (
                    Self::config_path(),
                    "platform/legacy discovery".to_string(),
                    false,
                )
            };
        let raw = read_config_text(&config_path, explicit_config_path)?;
        let document: toml::Value = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file {}", config_path.display()))?;
        let mut config: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file {}", config_path.display()))?;
        let has_database_config = toml_has_key(&document, "index", "db_path");
        let has_cache_config = toml_has_key(&document, "index", "cache_dir");
        let has_threads_config =
            toml_has_key(&document, "performance", "threads") && config.performance.threads > 0;
        anchor_toml_paths(
            &mut config,
            &document,
            &absolute_config_parent(&config_path)?,
            has_database_config,
            has_cache_config,
        );

        let (database, database_origin) = if let Some(path) = overrides.database_path {
            (path, "cli --database".to_string())
        } else if let Some(path) = environment.database_path {
            (path, "environment AI_SESSION_SEARCH_DATABASE".to_string())
        } else {
            (
                config.db_path(),
                if has_database_config {
                    "config file"
                } else {
                    "typed/platform default"
                }
                .to_string(),
            )
        };
        config.index.db_path = Some(database.to_string_lossy().into_owned());

        let (cache, cache_origin) = if let Some(path) = overrides.cache_dir {
            (path, "cli --cache-dir".to_string())
        } else if let Some(path) = environment.cache_dir {
            (path, "environment AI_SESSION_SEARCH_CACHE_DIR".to_string())
        } else {
            (
                config.cache_dir(),
                if has_cache_config {
                    "config file"
                } else {
                    "typed/platform default"
                }
                .to_string(),
            )
        };
        config.index.cache_dir = Some(cache.to_string_lossy().into_owned());

        let mut diagnostics = Vec::new();
        if environment.threads.is_some() && environment.legacy_threads.is_some() {
            diagnostics.push(
                "AI_SESSION_SEARCH_THREADS overrides deprecated AISE_THREADS; remove AISE_THREADS"
                    .to_string(),
            );
        }
        let (threads, threads_origin) = resolve_threads_setting(
            overrides.threads,
            environment.threads.as_deref(),
            environment.legacy_threads.as_deref(),
            config.performance.threads,
            has_threads_config,
            &mut diagnostics,
        )?;
        config.performance.threads = threads;
        config.validate()?;
        Ok(ResolvedConfig {
            config,
            config_path,
            origins: ConfigOrigins {
                config: config_origin,
                database: database_origin,
                cache: cache_origin,
                threads: threads_origin,
            },
            diagnostics,
        })
    }

    fn normalize_legacy_fields(&mut self) {
        if let Some(value) = self.mcp.schema_summary_tables {
            self.mcp.internal.schema_summary_tables = value;
        }
        if let Some(value) = self.mcp.schema_summary_columns {
            self.mcp.internal.schema_summary_columns = value;
        }
    }

    pub fn config_path() -> PathBuf {
        let home = home_dir_fallback();
        let platform = dirs::config_dir()
            .unwrap_or_else(|| home.join(".config"))
            .join("ai-session-search/config.toml");
        let legacy = home.join(".config/ai-session-search/config.toml");
        choose_config_path(
            nonempty_env_path("AI_SESSION_SEARCH_CONFIG"),
            platform,
            legacy,
        )
    }

    pub fn db_path(&self) -> PathBuf {
        self.index
            .db_path
            .as_deref()
            .map(expand_tilde)
            .unwrap_or_else(default_db_path)
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.index
            .cache_dir
            .as_deref()
            .map(expand_tilde)
            .unwrap_or_else(default_cache_dir)
    }

    pub fn claude_paths(&self) -> Vec<PathBuf> {
        self.providers
            .claude
            .paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect()
    }

    pub fn claude_desktop_paths(&self) -> Vec<PathBuf> {
        self.providers
            .claude_desktop
            .paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect()
    }

    pub fn codex_paths(&self) -> Vec<PathBuf> {
        self.providers
            .codex
            .paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect()
    }

    pub fn cursor_paths(&self) -> Vec<PathBuf> {
        self.providers
            .cursor
            .paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect()
    }

    pub fn antigravity_paths(&self) -> Vec<PathBuf> {
        self.providers
            .antigravity
            .paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect()
    }

    pub fn pi_paths(&self) -> Vec<PathBuf> {
        self.providers
            .pi
            .paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect()
    }

    pub fn aistudio_paths(&self) -> Vec<PathBuf> {
        self.providers
            .aistudio
            .paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect()
    }

    pub fn gemini_cli_paths(&self) -> Vec<PathBuf> {
        self.providers
            .gemini_cli
            .paths
            .iter()
            .map(|path| expand_tilde(path))
            .collect()
    }

    pub fn codex_home(&self) -> PathBuf {
        home_dir_fallback().join(".codex")
    }

    pub fn validate(&self) -> Result<()> {
        if self.search.default_limit == 0 {
            bail!("search.default_limit must be greater than zero");
        }
        if self.mcp.search_messages_limit == 0 {
            bail!("mcp.search_messages_limit must be greater than zero");
        }
        if self.mcp.preview_chars == 0 {
            bail!("mcp.preview_chars must be greater than zero");
        }
        if self.cli.evidence_preview_chars == 0 {
            bail!("cli.evidence_preview_chars must be greater than zero");
        }
        if self.analytics.repeat_min_matches == 0 {
            bail!("analytics.repeat_min_matches must be greater than zero");
        }
        if self.analytics.repeat_phrase_min_words == 0 {
            bail!("analytics.repeat_phrase_min_words must be greater than zero");
        }
        if self.analytics.repeat_phrase_max_words < self.analytics.repeat_phrase_min_words {
            bail!("analytics.repeat_phrase_max_words must be >= repeat_phrase_min_words");
        }
        Ok(())
    }
}

fn home_dir_fallback() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn nonempty_env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_positive_threads(name: &str, raw: &str) -> Result<usize> {
    match raw.trim().parse::<usize>() {
        Ok(value) if value > 0 => Ok(value),
        _ => bail!("{name} must be a positive integer, got {raw:?}"),
    }
}

fn resolve_threads_setting(
    cli: Option<usize>,
    canonical_env: Option<&str>,
    legacy_env: Option<&str>,
    configured: usize,
    configured_explicitly: bool,
    diagnostics: &mut Vec<String>,
) -> Result<(usize, String)> {
    if let Some(value) = cli {
        if value == 0 {
            bail!("--threads must be a positive integer");
        }
        return Ok((value, "cli --threads".to_string()));
    }
    if let Some(raw) = canonical_env {
        return Ok((
            parse_positive_threads("AI_SESSION_SEARCH_THREADS", raw)?,
            "environment AI_SESSION_SEARCH_THREADS".to_string(),
        ));
    }
    if let Some(raw) = legacy_env {
        diagnostics
            .push("AISE_THREADS is deprecated; use AI_SESSION_SEARCH_THREADS instead".to_string());
        return Ok((
            parse_positive_threads("AISE_THREADS", raw)?,
            "deprecated environment AISE_THREADS".to_string(),
        ));
    }
    Ok((
        configured,
        if configured_explicitly {
            "config file"
        } else {
            "typed/host default"
        }
        .to_string(),
    ))
}

fn toml_has_key(document: &toml::Value, table: &str, key: &str) -> bool {
    document
        .get(table)
        .cloned()
        .and_then(|value| value.get(key).cloned())
        .is_some()
}

fn read_config_text(path: &std::path::Path, explicit: bool) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !explicit => {
            Ok(String::new())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("explicit config file does not exist: {}", path.display())
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to read config file {}", path.display()))
        }
    }
}

fn absolute_config_parent(path: &std::path::Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    if parent.is_absolute() {
        Ok(parent.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("failed to resolve current directory for relative config path")?
            .join(parent))
    }
}

fn anchor_toml_paths(
    config: &mut Config,
    document: &toml::Value,
    config_parent: &std::path::Path,
    has_database_config: bool,
    has_cache_config: bool,
) {
    if has_database_config {
        anchor_optional_path(&mut config.index.db_path, config_parent);
    }
    if has_cache_config {
        anchor_optional_path(&mut config.index.cache_dir, config_parent);
    }
    for (provider_name, provider) in [
        ("claude", &mut config.providers.claude),
        ("claude-desktop", &mut config.providers.claude_desktop),
        ("codex", &mut config.providers.codex),
        ("cursor", &mut config.providers.cursor),
        ("antigravity", &mut config.providers.antigravity),
        ("pi", &mut config.providers.pi),
        ("ai-studio", &mut config.providers.aistudio),
        ("gemini-cli", &mut config.providers.gemini_cli),
    ] {
        if toml_has_nested_key(document, "providers", provider_name, "paths") {
            for path in &mut provider.paths {
                *path = anchored_path(path, config_parent);
            }
        }
    }
}

fn anchor_optional_path(value: &mut Option<String>, config_parent: &std::path::Path) {
    if let Some(path) = value {
        *path = anchored_path(path, config_parent);
    }
}

fn anchored_path(value: &str, config_parent: &std::path::Path) -> String {
    let path = std::path::Path::new(value);
    if value.is_empty() || value == "~" || value.starts_with("~/") || path.is_absolute() {
        value.to_string()
    } else {
        config_parent.join(path).to_string_lossy().into_owned()
    }
}

fn toml_has_nested_key(document: &toml::Value, table: &str, nested: &str, key: &str) -> bool {
    document
        .get(table)
        .cloned()
        .and_then(|value| value.get(nested).cloned())
        .and_then(|value| value.get(key).cloned())
        .is_some()
}

fn choose_config_path(
    override_path: Option<PathBuf>,
    platform_path: PathBuf,
    legacy_path: PathBuf,
) -> PathBuf {
    if let Some(path) = override_path {
        return expand_override_path(path);
    }
    // New installs use the platform-standard config dir from `dirs::config_dir`: XDG on Linux,
    // Application Support on macOS, Roaming AppData on Windows. Existing legacy
    // `~/.config/ai-session-search/config.toml` users are still honored when no platform-standard file
    // exists, so adopting platform paths does not silently drop a working config.
    if platform_path.exists() || !legacy_path.exists() {
        platform_path
    } else {
        legacy_path
    }
}

fn default_db_path() -> PathBuf {
    let home = home_dir_fallback();
    let platform = dirs::data_local_dir()
        .unwrap_or_else(|| home.join(".local/share"))
        .join("ai-session-search/index.db");
    let legacy = home.join(".local/share/ai-session-search/index.db");
    choose_default_state_path(platform, legacy)
}

fn default_cache_dir() -> PathBuf {
    let home = home_dir_fallback();
    let platform = dirs::cache_dir()
        .unwrap_or_else(|| home.join(".cache"))
        .join("ai-session-search");
    let legacy = home.join(".cache/ai-session-search");
    choose_default_state_path(platform, legacy)
}

fn choose_default_state_path(platform_path: PathBuf, legacy_path: PathBuf) -> PathBuf {
    // A legacy path wins whenever it exists. In particular, do not let an empty or partial
    // platform destination created by a failed cutover hide the user's working legacy index.
    // A completed migration writes an explicit index.db_path, which bypasses this default.
    if legacy_path.exists() {
        legacy_path
    } else {
        platform_path
    }
}

fn expand_override_path(path: PathBuf) -> PathBuf {
    path.to_str().map_or_else(
        || path.clone(),
        |value| {
            if value == "~" || value.starts_with("~/") {
                expand_tilde(value)
            } else {
                path.clone()
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BUSY_TIMEOUT_MS: u64 = 250;
    const TEST_AUTO_REINDEX_BUSY_TIMEOUT_MS: u64 = 10;
    const TEST_AUTO_REINDEX_INTERVAL_MS: u64 = 11;

    #[test]
    fn scoring_defaults_match_shipped_weights() {
        // The defaults must equal the ranker's original hard-coded weights so a config
        // without a [search.scoring] table leaves ranking unchanged.
        let s = ScoringConfig::default();
        assert_eq!(s.title_score, 600);
        assert_eq!(s.summary_score, 450);
        assert_eq!(s.path_score, 350);
        assert_eq!(s.preview_score, 250);
        assert_eq!(s.other_score, 100);
        assert_eq!(s.token_bonus, 40);
        assert_eq!(s.all_tokens_bonus, 150);
        assert_eq!(s.recency_weight, 2);
        assert_eq!(s.recency_max_days, 90);
        assert_eq!(s.current_repo_bonus, 200);
        assert_eq!(s.fts_candidate_multiplier, 5);
        assert_eq!(s.fts_candidate_floor, crate::db::FTS_CANDIDATE_FLOOR);
    }

    #[test]
    fn partial_scoring_toml_overrides_one_field_and_keeps_other_defaults() {
        // Overriding a single weight must not reset the rest — minimal-config friendliness.
        let toml = "[search.scoring]\ntitle_score = 999\n";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.search.scoring.title_score, 999);
        assert_eq!(
            cfg.search.scoring.summary_score, 450,
            "untouched weight keeps its default"
        );
        assert_eq!(
            cfg.search.scoring.fts_candidate_floor,
            crate::db::FTS_CANDIDATE_FLOOR
        );
        // Sibling settings still take their defaults.
        assert!(cfg.search.prefer_current_repo);
        assert_eq!(cfg.search.default_limit, 50);
    }

    #[test]
    fn claude_desktop_provider_has_separate_hyphenated_config_table() {
        let cfg: Config = toml::from_str(
            r#"
            [providers.claude]
            paths = ["/tmp/claude-code"]

            [providers.claude-desktop]
            paths = ["/tmp/claude-desktop"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.providers.claude.paths, vec!["/tmp/claude-code"]);
        assert_eq!(
            cfg.providers.claude_desktop.paths,
            vec!["/tmp/claude-desktop"]
        );
    }

    #[test]
    fn claude_desktop_default_paths_are_deduplicated_candidates() {
        let paths = default_claude_desktop_paths();
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            paths.len(),
            "defaults should not duplicate roots"
        );
        assert!(
            paths
                .iter()
                .all(|path| path.ends_with("Claude/local-agent-mode-sessions")
                    || path.ends_with("claude/local-agent-mode-sessions")),
            "all candidates point at Claude Desktop local agent session roots: {paths:?}"
        );
    }

    #[test]
    fn antigravity_default_paths_include_cli_and_legacy_brain_roots() {
        let cfg = Config::default();
        let paths = &cfg.providers.antigravity.paths;
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            paths.len(),
            "defaults should not duplicate roots"
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(".gemini/antigravity-cli/brain")),
            "Antigravity CLI writes transcripts under ~/.gemini/antigravity-cli/brain: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(".gemini/antigravity/brain")),
            "keep legacy Antigravity brain root for existing users: {paths:?}"
        );
    }

    #[test]
    fn performance_threads_parses_and_defaults_to_auto() {
        // Absent [performance] → threads = 0 (auto-detect).
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.performance.threads, 0);
        // Explicit override parses.
        let cfg: Config = toml::from_str("[performance]\nthreads = 4\n").unwrap();
        assert_eq!(cfg.performance.threads, 4);
    }

    #[test]
    fn performance_thresholds_parse_and_default_to_zero() {
        // Absent → 0 (= "use built-in default"); present → parsed value.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.performance.regex_prefilter_min_corpus, 0);
        assert_eq!(cfg.performance.trigram_rebuild_delta, 0);
        let cfg: Config = toml::from_str(
            "[performance]\nregex_prefilter_min_corpus = 10000\ntrigram_rebuild_delta = 25000\n",
        )
        .unwrap();
        assert_eq!(cfg.performance.regex_prefilter_min_corpus, 10000);
        assert_eq!(cfg.performance.trigram_rebuild_delta, 25000);
    }

    #[test]
    fn index_busy_timeout_parses_and_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(
            cfg.index.busy_timeout_ms,
            crate::db::DEFAULT_BUSY_TIMEOUT_MS
        );
        assert_eq!(
            cfg.index.auto_reindex_busy_timeout_ms,
            crate::db::DEFAULT_AUTO_REINDEX_BUSY_TIMEOUT_MS
        );
        assert_eq!(
            cfg.index.auto_reindex_interval_ms,
            crate::db::DEFAULT_AUTO_REINDEX_INTERVAL_MS
        );

        let cfg: Config = toml::from_str(&format!(
            "[index]\nbusy_timeout_ms = {TEST_BUSY_TIMEOUT_MS}\nauto_reindex_busy_timeout_ms = {TEST_AUTO_REINDEX_BUSY_TIMEOUT_MS}\nauto_reindex_interval_ms = {TEST_AUTO_REINDEX_INTERVAL_MS}\n"
        ))
        .unwrap();
        assert_eq!(cfg.index.busy_timeout_ms, TEST_BUSY_TIMEOUT_MS);
        assert_eq!(
            cfg.index.auto_reindex_busy_timeout_ms,
            TEST_AUTO_REINDEX_BUSY_TIMEOUT_MS
        );
        assert_eq!(
            cfg.index.auto_reindex_interval_ms,
            TEST_AUTO_REINDEX_INTERVAL_MS
        );
    }

    #[test]
    fn mcp_defaults_parse_and_default_to_bounded_agent_pages() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(
            cfg.mcp.search_sessions_limit,
            DEFAULT_MCP_SEARCH_SESSIONS_LIMIT
        );
        assert_eq!(cfg.mcp.list_sessions_limit, DEFAULT_MCP_LIST_SESSIONS_LIMIT);
        assert_eq!(
            cfg.mcp.analyze_sessions_limit,
            DEFAULT_MCP_ANALYZE_SESSIONS_LIMIT
        );
        assert_eq!(
            cfg.mcp.search_messages_limit,
            DEFAULT_MCP_SEARCH_MESSAGES_LIMIT
        );
        assert_eq!(
            cfg.mcp.get_session_transcript_lines,
            DEFAULT_MCP_GET_SESSION_TRANSCRIPT_LINES
        );
        assert_eq!(cfg.mcp.preview_chars, DEFAULT_MCP_PREVIEW_CHARS);
        assert_eq!(
            cfg.mcp.query_max_cell_chars,
            DEFAULT_MCP_QUERY_MAX_CELL_CHARS
        );
        assert_eq!(
            cfg.mcp.internal.schema_summary_tables,
            DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_TABLES
        );
        assert_eq!(
            cfg.mcp.internal.schema_summary_columns,
            DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_COLUMNS
        );

        let cfg: Config = toml::from_str(
            r#"
            [mcp]
            search_sessions_limit = 7
            list_sessions_limit = 8
            analyze_sessions_limit = 6
            search_messages_limit = 9
            get_session_transcript_lines = -12
            preview_chars = 77
            query_max_cell_chars = 13

            [mcp.internal]
            schema_summary_tables = 2
            schema_summary_columns = 3
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mcp.search_sessions_limit, 7);
        assert_eq!(cfg.mcp.list_sessions_limit, 8);
        assert_eq!(cfg.mcp.analyze_sessions_limit, 6);
        assert_eq!(cfg.mcp.search_messages_limit, 9);
        assert_eq!(cfg.mcp.get_session_transcript_lines, -12);
        assert_eq!(cfg.mcp.preview_chars, 77);
        assert_eq!(cfg.mcp.query_max_cell_chars, 13);
        assert_eq!(cfg.mcp.internal.schema_summary_tables, 2);
        assert_eq!(cfg.mcp.internal.schema_summary_columns, 3);
    }

    #[test]
    fn cli_defaults_parse_and_default_to_bounded_show() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(
            cfg.cli.show_transcript_lines,
            DEFAULT_CLI_SHOW_TRANSCRIPT_LINES
        );
        assert_eq!(
            cfg.cli.evidence_preview_chars,
            DEFAULT_CLI_EVIDENCE_PREVIEW_CHARS
        );

        let cfg: Config = toml::from_str(
            r#"
            [cli]
            show_transcript_lines = -12
            evidence_preview_chars = 88
            "#,
        )
        .unwrap();
        assert_eq!(cfg.cli.show_transcript_lines, -12);
        assert_eq!(cfg.cli.evidence_preview_chars, 88);
    }

    #[test]
    fn analytics_defaults_parse_for_user_facing_scan_controls() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.analytics.vocab_limit, DEFAULT_ANALYTICS_VOCAB_LIMIT);
        assert_eq!(
            cfg.analytics.repeat_max_groups,
            DEFAULT_ANALYTICS_REPEAT_MAX_GROUPS
        );
        assert_eq!(
            cfg.analytics.repeat_min_matches,
            DEFAULT_ANALYTICS_REPEAT_MIN_MATCHES
        );
        assert_eq!(
            cfg.analytics.repeat_phrase_min_words,
            DEFAULT_ANALYTICS_REPEAT_PHRASE_MIN_WORDS
        );
        assert_eq!(
            cfg.analytics.repeat_phrase_max_words,
            DEFAULT_ANALYTICS_REPEAT_PHRASE_MAX_WORDS
        );

        let cfg: Config = toml::from_str(
            r#"
            [analytics]
            vocab_limit = 17
            repeat_max_groups = 18
            repeat_min_matches = 3
            repeat_phrase_min_words = 4
            repeat_phrase_max_words = 9
            "#,
        )
        .unwrap();
        assert_eq!(cfg.analytics.vocab_limit, 17);
        assert_eq!(cfg.analytics.repeat_max_groups, 18);
        assert_eq!(cfg.analytics.repeat_min_matches, 3);
        assert_eq!(cfg.analytics.repeat_phrase_min_words, 4);
        assert_eq!(cfg.analytics.repeat_phrase_max_words, 9);
    }

    #[test]
    fn mcp_config_accepts_legacy_field_names_without_serializing_them() {
        let mut cfg: Config = toml::from_str(
            r#"
            [mcp]
            message_search_limit = 11
            schema_summary_tables = 12
            schema_summary_columns = 13
            "#,
        )
        .unwrap();
        cfg.normalize_legacy_fields();

        assert_eq!(cfg.mcp.search_messages_limit, 11);
        assert_eq!(cfg.mcp.internal.schema_summary_tables, 12);
        assert_eq!(cfg.mcp.internal.schema_summary_columns, 13);

        let serialized = toml::to_string(&cfg).unwrap();
        assert!(serialized.contains("search_messages_limit"));
        assert!(serialized.contains("[mcp.internal]"));
        assert!(!serialized.contains("message_search_limit"));
    }

    #[test]
    fn db_query_defaults_parse_and_default_to_bounded_sql() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.db.query_limit, DEFAULT_DB_QUERY_LIMIT);
        assert_eq!(cfg.db.query_timeout_ms, DEFAULT_DB_QUERY_TIMEOUT_MS);

        let cfg: Config = toml::from_str(
            r#"
            [db]
            query_limit = 17
            query_timeout_ms = 2500
            "#,
        )
        .unwrap();
        assert_eq!(cfg.db.query_limit, 17);
        assert_eq!(cfg.db.query_timeout_ms, 2500);
    }

    #[test]
    fn embedded_example_config_stays_parseable() {
        let cfg: Config = toml::from_str(CONFIG_EXAMPLE_TOML).unwrap();
        assert_eq!(
            cfg.mcp.search_messages_limit,
            DEFAULT_MCP_SEARCH_MESSAGES_LIMIT
        );
        assert_eq!(cfg.mcp.preview_chars, DEFAULT_MCP_PREVIEW_CHARS);
        assert_eq!(
            cfg.cli.evidence_preview_chars,
            DEFAULT_CLI_EVIDENCE_PREVIEW_CHARS
        );
        assert_eq!(cfg.analytics.vocab_limit, DEFAULT_ANALYTICS_VOCAB_LIMIT);
        assert_eq!(
            cfg.analytics.repeat_max_groups,
            DEFAULT_ANALYTICS_REPEAT_MAX_GROUPS
        );
        assert_eq!(cfg.db.query_timeout_ms, DEFAULT_DB_QUERY_TIMEOUT_MS);
        assert_eq!(
            cfg.mcp.internal.schema_summary_tables,
            DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_TABLES
        );
    }

    #[test]
    fn config_path_selection_prefers_platform_and_preserves_legacy_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let platform = dir.path().join("platform/ai-session-search/config.toml");
        let legacy = dir
            .path()
            .join("home/.config/ai-session-search/config.toml");

        assert_eq!(
            choose_config_path(None, platform.clone(), legacy.clone()),
            platform,
            "new installs use the platform-standard config path"
        );

        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "").unwrap();
        assert_eq!(
            choose_config_path(None, platform.clone(), legacy.clone()),
            legacy,
            "existing legacy config remains active if no platform config exists"
        );

        fs::create_dir_all(platform.parent().unwrap()).unwrap();
        fs::write(&platform, "").unwrap();
        assert_eq!(
            choose_config_path(None, platform.clone(), legacy),
            platform,
            "platform config wins once explicitly created"
        );
    }

    #[test]
    fn explicit_config_path_override_has_highest_precedence() {
        let override_path = PathBuf::from("/portable/config.toml");
        assert_eq!(
            choose_config_path(
                Some(override_path.clone()),
                PathBuf::from("/platform/config.toml"),
                PathBuf::from("/legacy/config.toml"),
            ),
            override_path
        );
    }

    #[test]
    fn cache_override_precedes_configured_and_default_paths() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "[index]\ncache_dir = '/configured/cache'\n").unwrap();
        let resolved = Config::resolve_with_environment(
            ConfigOverrides {
                config_path: Some(config_path),
                cache_dir: Some(PathBuf::from("/portable/cache")),
                ..Default::default()
            },
            ConfigEnvironment {
                cache_dir: Some(PathBuf::from("/environment/cache")),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            resolved.config.cache_dir(),
            PathBuf::from("/portable/cache")
        );
        assert_eq!(resolved.origins.cache, "cli --cache-dir");
    }

    #[test]
    fn state_path_selection_uses_platform_for_new_installs_and_preserves_legacy_data() {
        let dir = tempfile::tempdir().unwrap();
        let platform = dir.path().join("platform/ai-session-search/index.db");
        let legacy = dir.path().join("legacy/ai-session-search/index.db");

        assert_eq!(
            choose_default_state_path(platform.clone(), legacy.clone()),
            platform,
            "new installs use the platform-standard state path"
        );

        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "legacy index").unwrap();
        assert_eq!(
            choose_default_state_path(platform.clone(), legacy.clone()),
            legacy,
            "an existing legacy index remains active"
        );

        fs::create_dir_all(platform.parent().unwrap()).unwrap();
        fs::write(&platform, "partial destination").unwrap();
        assert_eq!(
            choose_default_state_path(platform, legacy.clone()),
            legacy,
            "an ambiguous destination must not silently hide legacy data"
        );
    }

    #[test]
    fn effective_config_serializes_for_config_show() {
        let cfg = Config::default();

        let toml = toml::to_string(&cfg).unwrap();
        assert!(toml.contains("auto_reindex_busy_timeout_ms"));
        assert!(toml.contains("auto_reindex_interval_ms"));
        assert!(toml.contains("search_messages_limit"));
        assert!(toml.contains("get_session_transcript_lines"));
        assert!(toml.contains("preview_chars"));
        assert!(toml.contains("show_transcript_lines"));
        assert!(toml.contains("evidence_preview_chars"));
        assert!(toml.contains("vocab_limit"));
        assert!(toml.contains("repeat_max_groups"));
        assert!(toml.contains("query_timeout_ms"));
        assert!(toml.contains("schema_summary_tables"));

        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("auto_reindex_busy_timeout_ms"));
        assert!(json.contains("auto_reindex_interval_ms"));
        assert!(json.contains("search_messages_limit"));
        assert!(json.contains("get_session_transcript_lines"));
        assert!(json.contains("preview_chars"));
        assert!(json.contains("show_transcript_lines"));
        assert!(json.contains("evidence_preview_chars"));
        assert!(json.contains("vocab_limit"));
        assert!(json.contains("repeat_max_groups"));
        assert!(json.contains("query_timeout_ms"));
        assert!(json.contains("schema_summary_tables"));
    }

    #[test]
    fn resolve_threads_precedence_uses_pure_inputs() {
        let mut diagnostics = Vec::new();
        let (threads, origin) =
            resolve_threads_setting(Some(11), Some("7"), Some("5"), 3, true, &mut diagnostics)
                .unwrap();
        assert_eq!((threads, origin.as_str()), (11, "cli --threads"));

        diagnostics.clear();
        let (threads, origin) =
            resolve_threads_setting(None, Some("7"), Some("5"), 3, true, &mut diagnostics).unwrap();
        assert_eq!(
            (threads, origin.as_str()),
            (7, "environment AI_SESSION_SEARCH_THREADS")
        );

        diagnostics.clear();
        let (threads, origin) =
            resolve_threads_setting(None, None, Some("5"), 3, true, &mut diagnostics).unwrap();
        assert_eq!(
            (threads, origin.as_str()),
            (5, "deprecated environment AISE_THREADS")
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(resolve_threads_setting(None, Some("0"), None, 3, true, &mut Vec::new()).is_err());
    }

    #[test]
    fn explicit_empty_provider_paths_are_not_replaced_by_defaults() {
        let config: Config =
            toml::from_str("[providers.codex]\nenabled = true\npaths = []\n").unwrap();
        assert!(config.providers.codex.paths.is_empty());
        assert!(!Config::default().providers.codex.paths.is_empty());
    }

    #[test]
    fn unknown_config_keys_are_rejected_instead_of_silently_ignored() {
        let error =
            toml::from_str::<Config>("[providers.gemini_cli]\nenabled = false\n").unwrap_err();
        assert!(error.to_string().contains("gemini_cli"));
    }

    #[test]
    fn resolver_applies_cli_then_canonical_env_then_config_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            "[index]\ndb_path = '/config/index.db'\ncache_dir = '/config/cache'\n[performance]\nthreads = 3\n",
        )
        .unwrap();
        let resolved = Config::resolve_with_environment(
            ConfigOverrides {
                config_path: Some(config_path),
                database_path: Some(PathBuf::from("/cli/index.db")),
                threads: Some(11),
                ..Default::default()
            },
            ConfigEnvironment {
                database_path: Some(PathBuf::from("/env/index.db")),
                cache_dir: Some(PathBuf::from("/env/cache")),
                threads: Some("7".to_string()),
                legacy_threads: Some("5".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.config.db_path(), PathBuf::from("/cli/index.db"));
        assert_eq!(resolved.config.cache_dir(), PathBuf::from("/env/cache"));
        assert_eq!(resolved.config.performance.threads, 11);
        assert_eq!(resolved.origins.database, "cli --database");
        assert_eq!(
            resolved.origins.cache,
            "environment AI_SESSION_SEARCH_CACHE_DIR"
        );
        assert_eq!(resolved.origins.threads, "cli --threads");
        assert_eq!(resolved.diagnostics.len(), 1);
    }

    #[test]
    fn resolver_anchors_toml_relative_paths_to_config_parent() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("portable");
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        fs::write(
            &config_path,
            r#"
[index]
db_path = "state/index.db"
cache_dir = "cache"
[providers.claude]
paths = ["sessions/claude"]
[providers.claude-desktop]
paths = ["sessions/claude-desktop"]
[providers.codex]
paths = ["sessions/codex"]
[providers.cursor]
paths = ["sessions/cursor"]
[providers.antigravity]
paths = ["sessions/antigravity"]
[providers.pi]
paths = ["sessions/pi"]
[providers.ai-studio]
paths = ["sessions/ai-studio"]
[providers.gemini-cli]
paths = ["sessions/gemini-cli"]
"#,
        )
        .unwrap();

        let resolved = Config::resolve_with_environment(
            ConfigOverrides {
                config_path: Some(config_path),
                ..Default::default()
            },
            ConfigEnvironment::default(),
        )
        .unwrap();

        assert_eq!(resolved.config.db_path(), config_dir.join("state/index.db"));
        assert_eq!(resolved.config.cache_dir(), config_dir.join("cache"));
        for (actual, relative) in [
            (resolved.config.claude_paths(), "sessions/claude"),
            (
                resolved.config.claude_desktop_paths(),
                "sessions/claude-desktop",
            ),
            (resolved.config.codex_paths(), "sessions/codex"),
            (resolved.config.cursor_paths(), "sessions/cursor"),
            (resolved.config.antigravity_paths(), "sessions/antigravity"),
            (resolved.config.pi_paths(), "sessions/pi"),
            (resolved.config.aistudio_paths(), "sessions/ai-studio"),
            (resolved.config.gemini_cli_paths(), "sessions/gemini-cli"),
        ] {
            assert_eq!(actual, vec![config_dir.join(relative)]);
        }
    }

    #[test]
    fn resolver_leaves_cli_and_environment_relative_overrides_cwd_relative() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "").unwrap();

        let resolved = Config::resolve_with_environment(
            ConfigOverrides {
                config_path: Some(config_path),
                database_path: Some(PathBuf::from("cli/index.db")),
                ..Default::default()
            },
            ConfigEnvironment {
                cache_dir: Some(PathBuf::from("environment/cache")),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(resolved.config.db_path(), PathBuf::from("cli/index.db"));
        assert_eq!(
            resolved.config.cache_dir(),
            PathBuf::from("environment/cache")
        );
    }

    #[test]
    fn explicit_missing_config_errors_but_implicit_missing_config_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");

        let override_error = Config::resolve_with_environment(
            ConfigOverrides {
                config_path: Some(missing.clone()),
                ..Default::default()
            },
            ConfigEnvironment::default(),
        )
        .unwrap_err();
        assert_eq!(
            override_error.to_string(),
            format!("explicit config file does not exist: {}", missing.display())
        );

        let environment_error = Config::resolve_with_environment(
            ConfigOverrides::default(),
            ConfigEnvironment {
                config_path: Some(missing.clone()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            environment_error.to_string(),
            format!("explicit config file does not exist: {}", missing.display())
        );

        assert_eq!(read_config_text(&missing, false).unwrap(), "");
    }
}
