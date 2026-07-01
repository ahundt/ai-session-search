use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::util::expand_tilde;

pub const CONFIG_EXAMPLE_TOML: &str = include_str!("../config.example.toml");

pub const DEFAULT_MCP_SEARCH_SESSIONS_LIMIT: usize = 10;
pub const DEFAULT_MCP_LIST_SESSIONS_LIMIT: usize = 20;
pub const DEFAULT_MCP_SEARCH_MESSAGES_LIMIT: usize = 20;
pub const DEFAULT_MCP_GET_SESSION_MAX_LINES: i64 = -40;
pub const DEFAULT_MCP_QUERY_MAX_CELL_CHARS: usize = crate::sql_query::DEFAULT_MCP_MAX_CELL_CHARS;
pub const DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_TABLES: usize = 4;
pub const DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_COLUMNS: usize = 12;
pub const DEFAULT_CLI_SHOW_MAX_LINES: i64 = -40;
pub const DEFAULT_DB_QUERY_LIMIT: usize = crate::sql_query::DEFAULT_LIMIT;
pub const DEFAULT_DB_QUERY_TIMEOUT_MS: u64 = crate::sql_query::DEFAULT_TIMEOUT_MS;

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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
pub struct UiConfig {
    #[serde(default = "default_preview_lines")]
    pub preview_lines: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

/// Analytics overrides (TOML). Corrections have narrowed built-in defaults; repeats are
/// data-driven phrase mining.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
}

/// Parallelism overrides (`[performance]` in config.toml). `threads` controls the worker
/// count for data-parallel CPU-bound scans (e.g. `corrections`). `0` (the default) means
/// auto-detect from the host (`std::thread::available_parallelism`), so it adapts to any
/// machine with no configuration. See [`Config::resolve_threads`] for the override chain.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
pub struct McpConfig {
    /// Default `search_sessions.limit`: session-level search page size. Does not affect CLI
    /// `sessiongrep search`, which uses `[search].default_limit`.
    #[serde(default = "default_mcp_search_sessions_limit")]
    pub search_sessions_limit: usize,
    /// Default `list_sessions.limit`: recent-session page size. Does not affect CLI
    /// `sessiongrep list`, which uses `[search].default_limit`.
    #[serde(default = "default_mcp_list_sessions_limit")]
    pub list_sessions_limit: usize,
    /// Default `search_messages.limit`: message-hit page size. Values below 1 are normalized to 1
    /// so pagination always makes progress. Does not affect CLI `sessiongrep messages search`.
    #[serde(
        default = "default_mcp_search_messages_limit",
        alias = "message_search_limit"
    )]
    pub search_messages_limit: usize,
    /// Default `get_session.max_lines` for full-transcript mode: positive=head, negative=tail,
    /// 0=entire transcript. Does not affect `get_session` calls that pass `seq`.
    #[serde(default = "default_mcp_get_session_max_lines")]
    pub get_session_max_lines: i64,
    /// Default `query_session_index.max_cell_chars`: truncates long string cells in MCP JSON
    /// responses only. It does not change SQL execution or CLI `sessiongrep db query` output.
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
pub struct CliConfig {
    /// Default `sessiongrep show --max-lines`: positive=head, negative=tail, 0=entire transcript.
    /// Use `--max-lines 0` explicitly when you want the full transcript.
    #[serde(default = "default_cli_show_max_lines")]
    pub show_max_lines: i64,
}

/// Raw SQLite query defaults (`[db]`). Applies to `sessiongrep db query` and MCP
/// `query_session_index` when callers omit the corresponding argument. These are safety defaults
/// for ad hoc SQL; they do not affect indexed search APIs such as `search_messages`.
#[derive(Debug, Clone, Deserialize, Serialize)]
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
fn default_mcp_search_messages_limit() -> usize {
    DEFAULT_MCP_SEARCH_MESSAGES_LIMIT
}
fn default_mcp_get_session_max_lines() -> i64 {
    DEFAULT_MCP_GET_SESSION_MAX_LINES
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
fn default_cli_show_max_lines() -> i64 {
    DEFAULT_CLI_SHOW_MAX_LINES
}
fn default_db_query_limit() -> usize {
    DEFAULT_DB_QUERY_LIMIT
}
fn default_db_query_timeout_ms() -> u64 {
    DEFAULT_DB_QUERY_TIMEOUT_MS
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
            },
            index: IndexConfig {
                db_path: Some(
                    home.join(".local/share/sessiongrep/index.db")
                        .to_string_lossy()
                        .to_string(),
                ),
                cache_dir: Some(
                    home.join(".cache/sessiongrep")
                        .to_string_lossy()
                        .to_string(),
                ),
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
    /// Resolve the worker-thread count for data-parallel CPU-bound scans. Override chain,
    /// most- to least-specific (per-invocation env beats persistent config beats auto-detect):
    ///
    /// 1. `SESSIONGREP_THREADS` env var (if a positive integer) — per-run override;
    /// 2. `[performance] threads` config (if `> 0`) — persistent project/user setting;
    /// 3. auto: `std::thread::available_parallelism()` — adapts to the host.
    ///
    /// Always returns `>= 1`. `1` means run sequentially (single worker).
    pub fn resolve_threads(&self) -> usize {
        if let Ok(raw) = std::env::var("SESSIONGREP_THREADS") {
            let trimmed = raw.trim();
            match trimmed.parse::<usize>() {
                Ok(n) if n > 0 => return n,
                // A set-but-unusable value is a likely misconfiguration; warn (don't silently
                // ignore) before falling through to config/auto, so the user can see it.
                _ => eprintln!(
                    "sessiongrep: ignoring invalid SESSIONGREP_THREADS={trimmed:?} \
                     (want a positive integer); using config/auto instead"
                ),
            }
        }
        if self.performance.threads > 0 {
            return self.performance.threads;
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

/// Configure the process-global Rayon thread pool used by data-parallel scans. Called exactly once
/// per process from the binary entry point (CLI or MCP), before any Rayon use — so there is no
/// concurrency here and no guard is needed (`build_global` is itself internally synchronized).
/// Returns the builder error (non-fatal: Rayon falls back to its own default pool) so the CALLER
/// reports it on the channel appropriate to that binary — the CLI prints to stderr; the MCP server
/// must keep its JSON-RPC stdout clean and logs to stderr. This is why the error is returned rather
/// than printed here.
pub fn init_thread_pool(threads: usize) -> Result<(), rayon::ThreadPoolBuildError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
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

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            search_sessions_limit: default_mcp_search_sessions_limit(),
            list_sessions_limit: default_mcp_list_sessions_limit(),
            search_messages_limit: default_mcp_search_messages_limit(),
            get_session_max_lines: default_mcp_get_session_max_lines(),
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
            show_max_lines: default_cli_show_max_lines(),
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
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        config.normalize_legacy_fields();

        let defaults = Self::default();
        if config.providers.claude.paths.is_empty() {
            config.providers.claude.paths = defaults.providers.claude.paths;
        }
        if config.providers.claude_desktop.paths.is_empty() {
            config.providers.claude_desktop.paths = defaults.providers.claude_desktop.paths;
        }
        if config.providers.codex.paths.is_empty() {
            config.providers.codex.paths = defaults.providers.codex.paths;
        }
        if config.providers.cursor.paths.is_empty() {
            config.providers.cursor.paths = defaults.providers.cursor.paths;
        }
        if config.providers.antigravity.paths.is_empty() {
            config.providers.antigravity.paths = defaults.providers.antigravity.paths;
        }
        if config.providers.pi.paths.is_empty() {
            config.providers.pi.paths = defaults.providers.pi.paths;
        }
        if config.index.db_path.is_none() {
            config.index.db_path = defaults.index.db_path;
        }
        if config.index.cache_dir.is_none() {
            config.index.cache_dir = defaults.index.cache_dir;
        }
        Ok(config)
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
            .join("sessiongrep/config.toml");
        let legacy = home.join(".config/sessiongrep/config.toml");
        choose_config_path(platform, legacy)
    }

    pub fn db_path(&self) -> PathBuf {
        expand_tilde(
            self.index
                .db_path
                .as_deref()
                .unwrap_or("~/.local/share/sessiongrep/index.db"),
        )
    }

    pub fn cache_dir(&self) -> PathBuf {
        expand_tilde(
            self.index
                .cache_dir
                .as_deref()
                .unwrap_or("~/.cache/sessiongrep"),
        )
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

    pub fn codex_home(&self) -> PathBuf {
        home_dir_fallback().join(".codex")
    }
}

fn home_dir_fallback() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn choose_config_path(platform_path: PathBuf, legacy_path: PathBuf) -> PathBuf {
    // New installs use the platform-standard config dir from `dirs::config_dir`: XDG on Linux,
    // Application Support on macOS, Roaming AppData on Windows. Existing legacy
    // `~/.config/sessiongrep/config.toml` users are still honored when no platform-standard file
    // exists, so adopting platform paths does not silently drop a working config.
    if platform_path.exists() || !legacy_path.exists() {
        platform_path
    } else {
        legacy_path
    }
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
            cfg.mcp.search_messages_limit,
            DEFAULT_MCP_SEARCH_MESSAGES_LIMIT
        );
        assert_eq!(
            cfg.mcp.get_session_max_lines,
            DEFAULT_MCP_GET_SESSION_MAX_LINES
        );
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
            search_messages_limit = 9
            get_session_max_lines = -12
            query_max_cell_chars = 13

            [mcp.internal]
            schema_summary_tables = 2
            schema_summary_columns = 3
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mcp.search_sessions_limit, 7);
        assert_eq!(cfg.mcp.list_sessions_limit, 8);
        assert_eq!(cfg.mcp.search_messages_limit, 9);
        assert_eq!(cfg.mcp.get_session_max_lines, -12);
        assert_eq!(cfg.mcp.query_max_cell_chars, 13);
        assert_eq!(cfg.mcp.internal.schema_summary_tables, 2);
        assert_eq!(cfg.mcp.internal.schema_summary_columns, 3);
    }

    #[test]
    fn cli_defaults_parse_and_default_to_bounded_show() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.cli.show_max_lines, DEFAULT_CLI_SHOW_MAX_LINES);

        let cfg: Config = toml::from_str(
            r#"
            [cli]
            show_max_lines = -12
            "#,
        )
        .unwrap();
        assert_eq!(cfg.cli.show_max_lines, -12);
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
        assert_eq!(cfg.db.query_timeout_ms, DEFAULT_DB_QUERY_TIMEOUT_MS);
        assert_eq!(
            cfg.mcp.internal.schema_summary_tables,
            DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_TABLES
        );
    }

    #[test]
    fn config_path_selection_prefers_platform_and_preserves_legacy_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let platform = dir.path().join("platform/sessiongrep/config.toml");
        let legacy = dir.path().join("home/.config/sessiongrep/config.toml");

        assert_eq!(
            choose_config_path(platform.clone(), legacy.clone()),
            platform,
            "new installs use the platform-standard config path"
        );

        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "").unwrap();
        assert_eq!(
            choose_config_path(platform.clone(), legacy.clone()),
            legacy,
            "existing legacy config remains active if no platform config exists"
        );

        fs::create_dir_all(platform.parent().unwrap()).unwrap();
        fs::write(&platform, "").unwrap();
        assert_eq!(
            choose_config_path(platform.clone(), legacy),
            platform,
            "platform config wins once explicitly created"
        );
    }

    #[test]
    fn effective_config_serializes_for_config_show() {
        let cfg = Config::default();

        let toml = toml::to_string(&cfg).unwrap();
        assert!(toml.contains("auto_reindex_busy_timeout_ms"));
        assert!(toml.contains("auto_reindex_interval_ms"));
        assert!(toml.contains("search_messages_limit"));
        assert!(toml.contains("get_session_max_lines"));
        assert!(toml.contains("show_max_lines"));
        assert!(toml.contains("query_timeout_ms"));
        assert!(toml.contains("schema_summary_tables"));

        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("auto_reindex_busy_timeout_ms"));
        assert!(json.contains("auto_reindex_interval_ms"));
        assert!(json.contains("search_messages_limit"));
        assert!(json.contains("get_session_max_lines"));
        assert!(json.contains("show_max_lines"));
        assert!(json.contains("query_timeout_ms"));
        assert!(json.contains("schema_summary_tables"));
    }

    #[test]
    fn resolve_threads_precedence() {
        // Override chain: env (SESSIONGREP_THREADS) > config > auto. Save/restore the env so this
        // test never leaks into others (only `resolve_threads` reads this var, and only this test
        // mutates it, so there is no intra-suite race on it).
        let saved = std::env::var("SESSIONGREP_THREADS").ok();
        std::env::remove_var("SESSIONGREP_THREADS");

        let mut cfg = Config::default();

        // Auto: threads=0 + no env → host parallelism (always >= 1).
        cfg.performance.threads = 0;
        assert!(cfg.resolve_threads() >= 1, "auto resolves to >= 1 core");

        // Config: threads>0 + no env → use the configured value.
        cfg.performance.threads = 3;
        assert_eq!(cfg.resolve_threads(), 3);

        // Env overrides config.
        std::env::set_var("SESSIONGREP_THREADS", "7");
        assert_eq!(cfg.resolve_threads(), 7, "env beats config");

        // Invalid or zero env is ignored → falls back to config.
        std::env::set_var("SESSIONGREP_THREADS", "0");
        assert_eq!(cfg.resolve_threads(), 3, "zero env ignored → config");
        std::env::set_var("SESSIONGREP_THREADS", "notanumber");
        assert_eq!(cfg.resolve_threads(), 3, "unparsable env ignored → config");

        match saved {
            Some(v) => std::env::set_var("SESSIONGREP_THREADS", v),
            None => std::env::remove_var("SESSIONGREP_THREADS"),
        }
    }
}
