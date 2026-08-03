// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Adam Zhao
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::util::expand_tilde;

pub const CONFIG_EXAMPLE_TOML: &str = include_str!("../config.example.toml");

pub const DEFAULT_MCP_SEARCH_SESSIONS_LIMIT: usize = 10;
pub const DEFAULT_MCP_LIST_SESSIONS_LIMIT: usize = 20;
/// Default MCP page size for `search_messages`.
///
/// WHY 15. The result ceiling below is a backstop, not the mechanism: a response that errors is
///   a worse outcome than one that never grew. At the measured marginal cost of about 1,583
///   characters per result, 15 results land near 25,000 characters, roughly 53% of the ceiling,
///   leaving about 2x headroom for a long matched message. At 20 the figure was 69%, which the
///   multiplicative levers consume immediately: `detail=full` measured 4.8x and
///   `include=[...,"parsed_references"]` 2.7x, and they compose.
/// PLATFORM: this repository's MCP surface only. CLI, Rust and Python return all results by
///   design and are untouched.
/// RAISE IT WHEN: `max_tool_result_chars` is raised for this deployment, or the per-result cost
///   drops. Keep an ordinary call under about 55% of the ceiling.
/// LOWER IT WHEN: the ceiling is lowered, or per-result cost grows.
/// NOT A CAP. A caller passes an explicit `limit` for more; `has_more` and `next_offset` are
///   unchanged, so paging stays correct and a caller wanting 20 asks for 20.
pub const DEFAULT_MCP_SEARCH_MESSAGES_LIMIT: usize = 15;

/// Maximum characters in one serialized MCP `CallToolResult`.
///
/// WHY 48,000. It is Codex's model-facing result cap, about 12,000 tokens at four characters
///   each. Codex is the only measured client that truncates a result **silently**, from the
///   middle, with no marker -- which keeps a plausible head and tail and so looks complete. That,
///   not being the smallest number, is why it sets the default: Gemini CLI's 40,000 is smaller
///   and announces the overflow, persists it, and states how many characters it omitted, so it
///   loses nothing.
/// PLATFORMS measured 2026-08-03:
///   Codex          ~12,000 tok = ~48,000 chars   SILENT middle truncation   <- binding
///   Claude Code    25,000 tok, persists >100,000 announces, saves to a file
///   Gemini CLI     40,000 chars                  announces, saves to a file
///   VS Code        50% of the model prompt budget, token-budgeted
///   OpenCode       none; MCP results pass through untouched
///   Cursor, Windsurf, Antigravity, Claude Desktop   unverified, closed source
/// RAISE IT WHEN: the deployment talks only to clients that announce truncation, or Codex raises
///   its own cap. Codex's schema budget already moved 4,000 -> 5,000, so its constants do move.
/// LOWER IT WHEN: a client is found that truncates silently below this. Antigravity is the live
///   risk: it superseded Gemini CLI, holds three of the thirteen registrations here, and its
///   truncation behaviour is unverified.
/// Measured over the serialized `CallToolResult`, not the JSON-RPC envelope around it: the
/// dispatcher owns the former and not the latter, and they differ by 34 characters.
pub const DEFAULT_MCP_MAX_TOOL_RESULT_CHARS: usize = 48_000;
pub const DEFAULT_MCP_RUN_MESSAGE_CLASSIFICATION_LIMIT: usize = 20;
/// Signed whole-transcript presentation window used by MCP `get_session` when omitted.
pub const DEFAULT_MCP_GET_SESSION_TRANSCRIPT_LINE_WINDOW: i64 = -40;
pub const DEFAULT_MCP_PREVIEW_CHARS: usize = crate::inspect::DEFAULT_PREVIEW_CHARS;
pub const DEFAULT_MCP_SUMMARY_ITEMS: i64 = -(crate::inspect::DEFAULT_EVIDENCE_LIMIT as i64);
pub const DEFAULT_MCP_QUERY_MAX_CELL_CHARS: usize = crate::sql_query::DEFAULT_MCP_MAX_CELL_CHARS;
pub const DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_TABLES: usize = 4;
pub const DEFAULT_MCP_INTERNAL_SCHEMA_SUMMARY_COLUMNS: usize = 12;
pub const MAX_MCP_CONCURRENT_READS: usize = tokio::sync::Semaphore::MAX_PERMITS;
/// Signed whole-transcript presentation window used by `aise show` when omitted.
pub const DEFAULT_CLI_SHOW_TRANSCRIPT_LINE_WINDOW: i64 = -40;
/// Signed per-message presentation window; zero preserves complete message content.
pub const DEFAULT_MESSAGE_LINE_WINDOW: i64 = 0;
pub const DEFAULT_CLI_EVIDENCE_PREVIEW_CHARS: usize = crate::inspect::DEFAULT_PREVIEW_CHARS;
pub const DEFAULT_CLI_SUMMARY_ITEMS: i64 = -(crate::inspect::DEFAULT_EVIDENCE_LIMIT as i64);
pub const DEFAULT_DB_QUERY_LIMIT: usize = crate::sql_query::DEFAULT_LIMIT;
pub const DEFAULT_DB_QUERY_TIMEOUT_MS: u64 = crate::sql_query::DEFAULT_TIMEOUT_MS;
pub const DEFAULT_MESSAGE_CLASSIFICATION_LIMIT: usize = 50;
pub const DEFAULT_ANALYTICS_PLANNING_LIMIT: usize = 50;
pub const DEFAULT_ANALYTICS_VOCAB_LIMIT: usize = 50;
pub const DEFAULT_ANALYTICS_REPEAT_MAX_GROUPS: usize = 50;
/// Structured repeat output exposes representative examples, not every matching message.
/// Three matches the long-standing table presentation; callers can request 0 for all examples.
pub const DEFAULT_ANALYTICS_REPEAT_MAX_EXAMPLES_PER_GROUP: usize = 3;
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
    pub skills: SkillsConfig,
    #[serde(default)]
    pub capabilities: CapabilitiesConfig,
    #[serde(default)]
    pub performance: PerformanceConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub cli: CliConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub release_notifications: ReleaseNotificationConfig,
}

/// Per-invocation configuration overrides. `None` preserves lower-precedence sources.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub config_path: Option<PathBuf>,
    pub database_path: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub threads: Option<usize>,
    pub index_refresh: Option<IndexRefresh>,
}

/// Process environment captured once so precedence can be tested without mutating global state.
#[derive(Debug, Clone, Default)]
pub struct ConfigEnvironment {
    pub config_path: Option<PathBuf>,
    pub database_path: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub threads: Option<String>,
    pub index_refresh: Option<String>,
}

impl ConfigEnvironment {
    pub fn capture() -> Self {
        Self {
            config_path: nonempty_env_path("AI_SESSION_SEARCH_CONFIG"),
            database_path: nonempty_env_path("AI_SESSION_SEARCH_DATABASE"),
            cache_dir: nonempty_env_path("AI_SESSION_SEARCH_CACHE_DIR"),
            threads: nonempty_env_string("AI_SESSION_SEARCH_THREADS"),
            index_refresh: nonempty_env_string("AI_SESSION_SEARCH_INDEX_REFRESH"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigOrigins {
    pub config: String,
    pub database: String,
    pub cache: String,
    pub threads: String,
    pub index_refresh: String,
    pub search_scope: String,
}

/// Validated effective configuration plus provenance.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub config: Config,
    pub config_path: PathBuf,
    pub origins: ConfigOrigins,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFile {
    providers: Option<ProvidersFile>,
    index: Option<IndexConfig>,
    ui: Option<UiConfig>,
    search: Option<SearchConfig>,
    analytics: Option<AnalyticsConfig>,
    skills: Option<SkillsConfig>,
    capabilities: Option<CapabilitiesConfig>,
    performance: Option<PerformanceConfig>,
    mcp: Option<McpConfig>,
    cli: Option<CliConfig>,
    db: Option<DbConfig>,
    release_notifications: Option<ReleaseNotificationConfig>,
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
        if let Some(value) = self.skills {
            config.skills = value;
        }
        if let Some(value) = self.capabilities {
            config.capabilities = value;
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
        if let Some(value) = self.release_notifications {
            config.release_notifications = value;
        }
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
    #[serde(default)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum IndexRefresh {
    #[default]
    Auto,
    BeforeQuery,
    ExistingOnly,
}

impl std::str::FromStr for IndexRefresh {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim() {
            "auto" => Ok(Self::Auto),
            "before-query" => Ok(Self::BeforeQuery),
            "existing-only" => Ok(Self::ExistingOnly),
            other => Err(format!(
                "expected auto, before-query, or existing-only; got {other:?}"
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    pub db_path: Option<String>,
    pub cache_dir: Option<String>,
    /// When implicit read paths refresh indexed session sources.
    #[serde(default)]
    pub refresh: IndexRefresh,
    /// SQLite busy timeout in milliseconds. Applies while opening/initializing the DB too, so
    /// normal concurrent CLI/MCP use waits briefly for another writer instead of failing.
    #[serde(default = "default_busy_timeout_ms")]
    pub busy_timeout_ms: u64,
    /// Busy timeout used only by automatic background reindex refreshes. When it expires on writer
    /// contention, read commands have already served the existing readable index.
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
    /// Shared message-search preferences. Omission preserves each surface's current default.
    #[serde(default, rename = "message-search")]
    pub message_search: MessageSearchConfig,
    /// Optional hard ceilings. Every omitted field preserves current runtime behavior.
    #[serde(default)]
    pub budgets: SearchBudgetConfig,
    /// Trusted search authority. `all` preserves current unrestricted behavior.
    #[serde(default)]
    pub scope: SearchScopeConfig,
    /// User-defined, versioned soft preference bundles. No built-in purpose ships by default.
    #[serde(default)]
    pub purposes: BTreeMap<String, PurposeDefinition>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessageSearchConfig {
    /// Optional shared positive page size. `None` preserves current CLI, MCP, Python, and Rust
    /// surface defaults; requesting every result remains an explicit per-call decision.
    pub default_limit: Option<NonZeroUsize>,
    /// Maximum Unicode scalar characters in automatic selected-field match evidence.
    /// Omission uses the typed default.
    pub match_evidence_max_chars: Option<NonZeroUsize>,
    #[serde(default)]
    pub context: MessageContextDefaults,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessageContextDefaults {
    pub context_before: Option<usize>,
    pub context_after: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchBudgetConfig {
    /// Maximum hits in a finite message-search page.
    ///
    /// This does not cap an explicit all-results request: `all_results` deliberately selects a
    /// different, unpaged extent. The service never silently converts all-results into a page or
    /// truncates either extent.
    pub max_hits_per_page: Option<NonZeroUsize>,
    pub max_context_neighbors_per_hit: Option<NonZeroUsize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SearchScopeMode {
    #[default]
    All,
    AllowedRoots,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchScopeConfig {
    pub mode: SearchScopeMode,
    pub roots: Vec<String>,
    pub include_invocation_directory: bool,
}

impl Default for SearchScopeConfig {
    fn default() -> Self {
        Self {
            mode: SearchScopeMode::All,
            roots: Vec::new(),
            include_invocation_directory: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchOperation {
    MessageSearch,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PurposeDefinition {
    pub version: NonZeroU32,
    pub operation: SearchOperation,
    #[serde(default)]
    pub preferences: MessagePurposePreferences,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessagePurposePreferences {
    pub default_limit: Option<NonZeroUsize>,
    pub context_before: Option<usize>,
    pub context_after: Option<usize>,
    pub receipt_level: Option<crate::message_search::ReceiptLevel>,
    pub include_refs: Option<bool>,
    pub lines_per_message: Option<i64>,
    pub match_evidence_max_chars: Option<NonZeroUsize>,
}

/// Tunable weights for the session search ranker (`[search.scoring]` in config.toml).
/// Every field defaults to the value the ranker shipped with, so an absent or partial
/// `[search.scoring]` table leaves ranking byte-for-byte unchanged — you should rarely
/// need to set any of these. A field contributes its weight when the lowercased query is
/// a substring of that haystack; `token_bonus` is added per query token found in a
/// haystack, `all_tokens_bonus` once when every token matched across fields of one session, recency adds
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
}

/// Analytics defaults and overrides (`[analytics]` in config.toml).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyticsConfig {
    /// `planning`: when non-empty, restricts the count to slash commands whose token
    /// matches one of these (case-insensitive) regexes. Empty = count every slash command.
    #[serde(default)]
    pub planning_commands: Vec<String>,
    /// Default `aise planning --limit`. `0` means every command.
    #[serde(default = "default_analytics_planning_limit")]
    pub planning_limit: usize,
    /// Default `aise vocab --limit`. `0` means unlimited.
    #[serde(default = "default_analytics_vocab_limit")]
    pub vocab_limit: usize,
    /// Default `aise repeats --max-groups`. `0` means all groups.
    #[serde(default = "default_analytics_repeat_max_groups")]
    pub repeat_max_groups: usize,
    /// Default `aise repeats --max-examples-per-group`. `0` means every matching message.
    #[serde(default = "default_analytics_repeat_max_examples_per_group")]
    pub repeat_max_examples_per_group: usize,
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

/// Deterministic skill discovery and authoring (`[skills]` in config.toml).
///
/// A skill is a standard Agent Skills directory. Deterministic capabilities are declared by
/// adjacent, strictly parsed metadata; `aise` does not execute instructions from `SKILL.md`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// Directories to scan for skills, each either a skill directory or a directory containing
    /// skill directories. `~` is expanded. Empty = only capabilities embedded in the executable.
    ///
    /// These are DISCOVERY roots and are deliberately not the install destinations from
    /// `aise integrations install`: install writes the same bundled skill to three client roots,
    /// so scanning those would find three copies of one name. Discovery is also not activation --
    /// a file appearing here never changes results until it is selected by name.
    #[serde(default)]
    pub search_paths: Vec<String>,
    /// Parent directory `aise skills create` writes into when `--output-dir` is omitted.
    /// `None` = require the flag rather than guessing a destination for a new directory.
    #[serde(default)]
    pub authoring_root: Option<String>,
}

/// Defaults for typed deterministic capabilities.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilitiesConfig {
    pub message_classification: MessageClassificationConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessageClassificationConfig {
    /// Default native CLI and Python result count. `0` means every match.
    #[serde(default = "default_message_classification_limit")]
    pub limit: usize,
}

/// Parallelism overrides (`[performance]` in config.toml). `threads` controls the worker
/// count for data-parallel CPU-bound scans (e.g. message classification). `0` (the default) means
/// auto-detect from the host (`std::thread::available_parallelism`), so it adapts to any
/// machine with no configuration. See [`Config::resolve_threads`] for the override chain.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PerformanceConfig {
    /// Worker threads for parallel scans. `0` = auto (all available cores); `1` = sequential.
    #[serde(default)]
    pub threads: usize,
}

/// Simultaneous MCP read-only SQLite calls per server process.
///
/// `Auto` admits half the available host parallelism, rounded up, because admitted fuzzy searches
/// share a separate host-sized Rayon worker pool. `Host` admits one call per available logical CPU.
/// `Fixed` is an explicit positive cap. Neither value changes the separate single-writer election
/// used by index refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpReadConcurrency {
    #[default]
    Auto,
    Host,
    Fixed(NonZeroUsize),
}

impl McpReadConcurrency {
    fn resolve(self) -> Result<NonZeroUsize> {
        let host = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        let resolved = match self {
            Self::Auto => NonZeroUsize::new(host.get().div_ceil(2))
                .expect("half of nonzero host parallelism remains nonzero"),
            Self::Host => host,
            Self::Fixed(value) => value,
        };
        if resolved.get() > MAX_MCP_CONCURRENT_READS {
            bail!("{}", invalid_mcp_read_concurrency(&resolved.get()));
        }
        Ok(resolved)
    }
}

fn invalid_mcp_read_concurrency(got: &impl fmt::Display) -> String {
    format!(
        "max_concurrent_reads must be \"auto\", \"host\", or an integer from 1 through \
         {MAX_MCP_CONCURRENT_READS}, got {got}; use \"auto\" for the balanced default, \"host\" \
         for one slot per available logical CPU, or an integer in that range to cap simultaneous \
         read-only SQLite calls"
    )
}

impl Serialize for McpReadConcurrency {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Host => serializer.serialize_str("host"),
            Self::Fixed(value) => serializer.serialize_u64(value.get() as u64),
        }
    }
}

impl<'de> Deserialize<'de> for McpReadConcurrency {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct McpReadConcurrencyVisitor;

        impl serde::de::Visitor<'_> for McpReadConcurrencyVisitor {
            type Value = McpReadConcurrency;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("\"auto\", \"host\", or a positive integer")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "auto" => return Ok(McpReadConcurrency::Auto),
                    "host" => return Ok(McpReadConcurrency::Host),
                    _ => {}
                }
                Err(E::custom(invalid_mcp_read_concurrency(&format_args!(
                    "{value:?}"
                ))))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let value = usize::try_from(value)
                    .ok()
                    .and_then(NonZeroUsize::new)
                    .filter(|value| value.get() <= MAX_MCP_CONCURRENT_READS)
                    .ok_or_else(|| E::custom(invalid_mcp_read_concurrency(&value)))?;
                Ok(McpReadConcurrency::Fixed(value))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                u64::try_from(value)
                    .ok()
                    .and_then(|value| usize::try_from(value).ok())
                    .and_then(NonZeroUsize::new)
                    .filter(|value| value.get() <= MAX_MCP_CONCURRENT_READS)
                    .map(McpReadConcurrency::Fixed)
                    .ok_or_else(|| E::custom(invalid_mcp_read_concurrency(&value)))
            }
        }

        deserializer.deserialize_any(McpReadConcurrencyVisitor)
    }
}

/// Agent-facing MCP defaults (`[mcp]` in config.toml). These affect default tool-call behavior
/// only when the MCP client omits the matching parameter; explicit tool arguments still win. They
/// matter because MCP responses are usually copied straight into an agent's context window.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Maximum tool calls per MCP server process that may hold independent read-only SQLite
    /// connections simultaneously. Additional calls wait without opening a connection. This
    /// bounds page-cache/result working memory while allowing WAL readers to overlap. `"auto"`
    /// is the balanced default; `"host"` admits one reader per available logical CPU.
    #[serde(default)]
    pub max_concurrent_reads: McpReadConcurrency,
    /// Default `search_sessions.limit`: session-level search page size. Does not affect CLI
    /// `aise search`, which uses `[search].default_limit`.
    #[serde(default = "default_mcp_search_sessions_limit")]
    pub search_sessions_limit: usize,
    /// Default `list_sessions.limit`: recent-session page size. Does not affect CLI
    /// `aise list`, which uses `[search].default_limit`.
    #[serde(default = "default_mcp_list_sessions_limit")]
    pub list_sessions_limit: usize,
    /// Default `search_messages.limit`: message-hit page size. Must be at least 1 so pagination
    /// always makes progress. Does not affect CLI `aise messages search`.
    #[serde(default = "default_mcp_search_messages_limit")]
    pub search_messages_limit: usize,
    /// Ceiling on one serialized MCP `CallToolResult`, in characters.
    ///
    /// Typed and named rather than a constant because `REQ008-reject-hidden-cutoffs` requires an
    /// intentional bound to be named, typed, documented and rejected when it conflicts, and
    /// because no single value is right for thirteen registrations whose clients differ by more
    /// than 2x in what they tolerate and differ in kind in how they fail. A registration
    /// overrides it with `AI_SESSION_SEARCH_MAX_TOOL_RESULT_CHARS` in its own `env` block, the
    /// same mechanism `AI_SESSION_SEARCH_INDEX_REFRESH` already uses.
    ///
    /// MCP `tools/call` only. CLI, Rust and Python return every result by design.
    #[serde(default = "default_mcp_max_tool_result_chars")]
    pub max_tool_result_chars: usize,
    /// Default `run_skill_capability` message-classification page size. Must be at least 1 so
    /// pagination always makes progress. Deliberately smaller than the CLI and Python default of
    /// `[capabilities.message_classification].limit`: an MCP result is pasted straight into an
    /// agent context window, where a classification row carries a whole user message. Callers
    /// that want everything pass `all_results`.
    #[serde(default = "default_mcp_run_message_classification_limit")]
    pub run_message_classification_limit: usize,
    /// Default `get_session.transcript_lines`: positive=head, negative=tail,
    /// 0=entire transcript. Does not affect `get_session` calls that pass `message_seq`.
    #[serde(default = "default_mcp_get_session_transcript_line_window")]
    pub get_session_transcript_lines: i64,
    /// Default `preview_chars` for concise MCP hit/context previews and `get_session` summary or
    /// focused-message output. Explicit MCP tool-call `preview_chars` values still win. Does not
    /// affect transcript output. Must be at least 1.
    #[serde(default = "default_mcp_preview_chars")]
    pub preview_chars: usize,
    /// Default aggregate evidence window for get_session summary: positive=first,
    /// negative=last, zero=all.
    #[serde(default = "default_mcp_summary_items")]
    pub summary_items: i64,
    /// Default `lines_per_message` for `search_messages` hits/context and `get_session`
    /// focused `message_seq` output: caps each individual message's content to this many lines
    /// (positive=head, negative=tail, 0=full content). Distinct from
    /// `get_session_transcript_lines`, which windows one whole session transcript.
    #[serde(default = "default_message_line_window")]
    pub lines_per_message: i64,
    /// Default `query_session_index.max_cell_chars`: truncates long string cells in MCP JSON
    /// responses only. It does not change SQL execution or CLI `aise db query` output.
    /// `0` disables MCP string-cell truncation.
    #[serde(default = "default_mcp_query_max_cell_chars")]
    pub query_max_cell_chars: usize,
    /// MCP-only raw-SQL execution timeout in milliseconds. `0` (the default) disables
    /// interruption. This is separate from `[db].query_timeout_ms`; adopt a finite default only
    /// after representative valid-query and concurrency calibration justifies the availability
    /// guard.
    #[serde(default)]
    pub query_timeout_ms: u64,
    /// Internal MCP presentation budgets. These affect only generated tool descriptions, not
    /// search/query results. Leave unchanged unless the schema summary is too large/small for your
    /// MCP client.
    #[serde(default)]
    pub internal: McpInternalConfig,
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
    #[serde(default = "default_cli_show_transcript_line_window")]
    pub show_transcript_lines: i64,
    /// Default `aise messages search/get/timeline --lines-per-message`: caps each individual
    /// message's content to this many lines (positive=head, negative=tail, 0=full content).
    /// Distinct from `show_transcript_lines`, which windows one whole session transcript.
    #[serde(default = "default_message_line_window")]
    pub lines_per_message: i64,
    /// Default `aise messages evidence --preview-chars`. This affects only compact
    /// evidence previews; JSON message search/get output still keeps full message content. Must be
    /// at least 1.
    #[serde(default = "default_cli_evidence_preview_chars")]
    pub evidence_preview_chars: usize,
    /// Default aggregate evidence window for compact CLI summaries: positive=first,
    /// negative=last, zero=all.
    #[serde(default = "default_cli_summary_items")]
    pub summary_items: i64,
}

/// Native raw SQLite query defaults (`[db]`).
///
/// These apply to `aise db query` and public Rust callers when an argument is omitted. MCP has a
/// separate `mcp.query_timeout_ms` availability guard because its synchronous tool lifecycle is
/// distinct. Neither configuration affects indexed search APIs such as `search_messages`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbConfig {
    /// Default maximum rows for read-only SQL. `0` means unlimited and can produce huge output.
    #[serde(default = "default_db_query_limit")]
    pub query_limit: usize,
    /// Native read-only SQL timeout in milliseconds. `0` (the default) disables interruption.
    #[serde(default = "default_db_query_timeout_ms")]
    pub query_timeout_ms: u64,
}

pub const DEFAULT_RELEASE_NOTIFICATION_MINIMUM_CHECK_INTERVAL_HOURS: u64 = 24;
pub const DEFAULT_RELEASE_NOTIFICATION_REQUEST_TIMEOUT_MS: u64 = 1_000;

/// Stable-release notification defaults (`[release_notifications]`).
/// Explicit `aise package check|update` requests remain available when notifications are disabled.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReleaseNotificationConfig {
    /// Check for an applicable release after ordinary interactive CLI output.
    pub enabled: bool,
    /// Minimum interval between completed notification checks.
    pub minimum_check_interval_hours: u64,
    /// End-to-end timeout for the GitHub release request.
    pub request_timeout_ms: u64,
}

impl Default for ReleaseNotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            minimum_check_interval_hours: DEFAULT_RELEASE_NOTIFICATION_MINIMUM_CHECK_INTERVAL_HOURS,
            request_timeout_ms: DEFAULT_RELEASE_NOTIFICATION_REQUEST_TIMEOUT_MS,
        }
    }
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
fn default_mcp_max_tool_result_chars() -> usize {
    DEFAULT_MCP_MAX_TOOL_RESULT_CHARS
}
fn default_mcp_run_message_classification_limit() -> usize {
    DEFAULT_MCP_RUN_MESSAGE_CLASSIFICATION_LIMIT
}

fn default_mcp_get_session_transcript_line_window() -> i64 {
    DEFAULT_MCP_GET_SESSION_TRANSCRIPT_LINE_WINDOW
}
fn default_mcp_preview_chars() -> usize {
    DEFAULT_MCP_PREVIEW_CHARS
}
fn default_mcp_summary_items() -> i64 {
    DEFAULT_MCP_SUMMARY_ITEMS
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
fn default_cli_show_transcript_line_window() -> i64 {
    DEFAULT_CLI_SHOW_TRANSCRIPT_LINE_WINDOW
}
fn default_message_line_window() -> i64 {
    DEFAULT_MESSAGE_LINE_WINDOW
}
fn default_cli_evidence_preview_chars() -> usize {
    DEFAULT_CLI_EVIDENCE_PREVIEW_CHARS
}
fn default_cli_summary_items() -> i64 {
    DEFAULT_CLI_SUMMARY_ITEMS
}
fn default_db_query_limit() -> usize {
    DEFAULT_DB_QUERY_LIMIT
}
fn default_db_query_timeout_ms() -> u64 {
    DEFAULT_DB_QUERY_TIMEOUT_MS
}
fn default_message_classification_limit() -> usize {
    DEFAULT_MESSAGE_CLASSIFICATION_LIMIT
}
fn default_analytics_planning_limit() -> usize {
    DEFAULT_ANALYTICS_PLANNING_LIMIT
}
fn default_analytics_vocab_limit() -> usize {
    DEFAULT_ANALYTICS_VOCAB_LIMIT
}
fn default_analytics_repeat_max_groups() -> usize {
    DEFAULT_ANALYTICS_REPEAT_MAX_GROUPS
}
fn default_analytics_repeat_max_examples_per_group() -> usize {
    DEFAULT_ANALYTICS_REPEAT_MAX_EXAMPLES_PER_GROUP
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
                refresh: IndexRefresh::Auto,
                busy_timeout_ms: default_busy_timeout_ms(),
                auto_reindex_busy_timeout_ms: default_auto_reindex_busy_timeout_ms(),
                auto_reindex_interval_ms: default_auto_reindex_interval_ms(),
            },
            ui: UiConfig { preview_lines: 30 },
            search: SearchConfig {
                default_limit: 50,
                prefer_current_repo: true,
                scoring: ScoringConfig::default(),
                message_search: MessageSearchConfig::default(),
                budgets: SearchBudgetConfig::default(),
                scope: SearchScopeConfig::default(),
                purposes: BTreeMap::new(),
            },
            analytics: AnalyticsConfig::default(),
            skills: SkillsConfig::default(),
            capabilities: CapabilitiesConfig::default(),
            performance: PerformanceConfig::default(),
            mcp: McpConfig::default(),
            cli: CliConfig::default(),
            db: DbConfig::default(),
            release_notifications: ReleaseNotificationConfig::default(),
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

    /// Resolve the named MCP reader policy to the positive admission bound used by the semaphore.
    pub fn resolve_mcp_max_concurrent_reads(&self) -> Result<NonZeroUsize> {
        self.mcp.max_concurrent_reads.resolve()
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
            planning_commands: Vec::new(),
            planning_limit: default_analytics_planning_limit(),
            vocab_limit: default_analytics_vocab_limit(),
            repeat_max_groups: default_analytics_repeat_max_groups(),
            repeat_max_examples_per_group: default_analytics_repeat_max_examples_per_group(),
            repeat_min_matches: default_analytics_repeat_min_matches(),
            repeat_phrase_min_words: default_analytics_repeat_phrase_min_words(),
            repeat_phrase_max_words: default_analytics_repeat_phrase_max_words(),
        }
    }
}

impl Default for MessageClassificationConfig {
    fn default() -> Self {
        Self {
            limit: default_message_classification_limit(),
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            max_concurrent_reads: McpReadConcurrency::Auto,
            search_sessions_limit: default_mcp_search_sessions_limit(),
            list_sessions_limit: default_mcp_list_sessions_limit(),
            search_messages_limit: default_mcp_search_messages_limit(),
            max_tool_result_chars: default_mcp_max_tool_result_chars(),
            run_message_classification_limit: default_mcp_run_message_classification_limit(),
            get_session_transcript_lines: default_mcp_get_session_transcript_line_window(),
            preview_chars: default_mcp_preview_chars(),
            summary_items: default_mcp_summary_items(),
            lines_per_message: default_message_line_window(),
            query_max_cell_chars: default_mcp_query_max_cell_chars(),
            query_timeout_ms: 0,
            internal: McpInternalConfig::default(),
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
            show_transcript_lines: default_cli_show_transcript_line_window(),
            lines_per_message: default_message_line_window(),
            evidence_preview_chars: default_cli_evidence_preview_chars(),
            summary_items: default_cli_summary_items(),
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
        let has_index_refresh_config = toml_has_key(&document, "index", "refresh");
        let has_threads_config =
            toml_has_key(&document, "performance", "threads") && config.performance.threads > 0;
        let has_search_scope_config = document
            .get("search")
            .and_then(|search| search.get("scope"))
            .is_some();
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

        let (index_refresh, index_refresh_origin) = resolve_index_refresh_setting(
            overrides.index_refresh,
            environment.index_refresh.as_deref(),
            config.index.refresh,
            has_index_refresh_config,
        )?;
        config.index.refresh = index_refresh;

        let (threads, threads_origin) = resolve_threads_setting(
            overrides.threads,
            environment.threads.as_deref(),
            config.performance.threads,
            has_threads_config,
        )?;
        config.performance.threads = threads;
        config
            .validate()
            .with_context(|| format!("invalid config at {}", config_path.display()))?;
        Ok(ResolvedConfig {
            config,
            config_path,
            origins: ConfigOrigins {
                config: config_origin,
                database: database_origin,
                cache: cache_origin,
                threads: threads_origin,
                index_refresh: index_refresh_origin,
                search_scope: if has_search_scope_config {
                    "config file"
                } else {
                    "typed default"
                }
                .to_string(),
            },
        })
    }

    pub fn config_path() -> PathBuf {
        let home = home_dir_fallback();
        let app = home.join(".ai-session-search/config.toml");
        let platform_legacy = dirs::config_dir()
            .unwrap_or_else(|| home.join(".config"))
            .join("ai-session-search/config.toml");
        let xdg_legacy = home.join(".config/ai-session-search/config.toml");
        choose_config_path(
            nonempty_env_path("AI_SESSION_SEARCH_CONFIG"),
            app,
            &[platform_legacy, xdg_legacy],
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
        const FIX: &str = "edit the invalid value in the config file; run `aise config example` \
                           to view the defaults (`aise config init --force` replaces the entire file)";
        if self.search.default_limit == 0 {
            bail!("search.default_limit must be greater than zero; {FIX}");
        }
        if self.release_notifications.minimum_check_interval_hours == 0 {
            bail!(
                "release_notifications.minimum_check_interval_hours must be 1 or greater, got 0; \
                 pass the minimum whole hours between completed release-notification checks; {FIX}"
            );
        }
        if self.release_notifications.request_timeout_ms == 0 {
            bail!(
                "release_notifications.request_timeout_ms must be 1 or greater, got 0; \
                 pass the end-to-end release-notification request timeout in milliseconds; {FIX}"
            );
        }
        if self.mcp.search_messages_limit == 0 {
            bail!("mcp.search_messages_limit must be greater than zero; {FIX}");
        }
        self.resolve_mcp_max_concurrent_reads()?;
        if self.mcp.run_message_classification_limit == 0 {
            bail!(
                "mcp.run_message_classification_limit must be 1 or greater, got 0; \
                 pass the default number of classification matches in one MCP page; {FIX}"
            );
        }
        if self.search.scoring.recency_max_days < 0 {
            bail!(
                "search.scoring.recency_max_days must be 0 or greater, got {}; {FIX}",
                self.search.scoring.recency_max_days
            );
        }
        if let Some(maximum) = self.search.budgets.max_hits_per_page {
            if self
                .search
                .message_search
                .default_limit
                .is_some_and(|limit| limit > maximum)
            {
                bail!(
                    "search.message-search.default_limit exceeds search.budgets.max_hits_per_page {}; {FIX}",
                    maximum
                );
            }
            if self.mcp.search_messages_limit > maximum.get() {
                bail!(
                    "mcp.search_messages_limit {} exceeds search.budgets.max_hits_per_page {}; {FIX}",
                    self.mcp.search_messages_limit,
                    maximum
                );
            }
        }
        let context_total = self
            .search
            .message_search
            .context
            .context_before
            .unwrap_or(0)
            .checked_add(
                self.search
                    .message_search
                    .context
                    .context_after
                    .unwrap_or(0),
            )
            .ok_or_else(|| {
                anyhow::anyhow!("search.message-search.context total overflows usize; {FIX}")
            })?;
        if self
            .search
            .budgets
            .max_context_neighbors_per_hit
            .is_some_and(|maximum| context_total > maximum.get())
        {
            bail!(
                "search.message-search.context exceeds search.budgets.max_context_neighbors_per_hit; {FIX}"
            );
        }
        if self.search.scope.mode == SearchScopeMode::All
            && (!self.search.scope.roots.is_empty()
                || self.search.scope.include_invocation_directory)
        {
            bail!(
                "search.scope roots and include_invocation_directory require mode = \"allowed-roots\"; {FIX}"
            );
        }
        if self
            .search
            .scope
            .roots
            .iter()
            .any(|root| root.trim().is_empty())
        {
            bail!("search.scope.roots must not contain an empty path; {FIX}");
        }
        for root in &self.search.scope.roots {
            crate::search_scope::validate_configured_root(std::path::Path::new(root)).map_err(
                |error| anyhow::anyhow!("search.scope.roots entry {root:?}: {error}; {FIX}"),
            )?;
        }
        for (name, purpose) in &self.search.purposes {
            if !crate::message_search::is_dash_separated_phrase(name) {
                bail!(
                    "search.purposes name {name:?} must be a short lowercase dash-separated phrase; {FIX}"
                );
            }
            if purpose.operation != SearchOperation::MessageSearch {
                bail!("search.purposes.{name}.operation is not supported; {FIX}");
            }
            if let Some(lines) = purpose.preferences.lines_per_message {
                crate::message_search::LineWindow::from_signed(lines).map_err(|error| {
                    anyhow::anyhow!("search.purposes.{name}.lines_per_message: {error}; {FIX}")
                })?;
            }
            if let (Some(limit), Some(maximum)) = (
                purpose.preferences.default_limit,
                self.search.budgets.max_hits_per_page,
            ) {
                if limit > maximum {
                    bail!(
                        "search.purposes.{name}.default_limit {} exceeds search.budgets.max_hits_per_page {}; {FIX}",
                        limit,
                        maximum
                    );
                }
            }
            if let Some(maximum) = self.search.budgets.max_context_neighbors_per_hit {
                let total = purpose
                    .preferences
                    .context_before
                    .unwrap_or(0)
                    .checked_add(purpose.preferences.context_after.unwrap_or(0))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "search.purposes.{name} context total overflows usize; {FIX}"
                        )
                    })?;
                if total > maximum.get() {
                    bail!(
                        "search.purposes.{name} context total {total} exceeds search.budgets.max_context_neighbors_per_hit {}; {FIX}",
                        maximum
                    );
                }
            }
        }
        if self.mcp.preview_chars == 0 {
            bail!("mcp.preview_chars must be greater than zero; {FIX}");
        }
        if self.cli.evidence_preview_chars == 0 {
            bail!("cli.evidence_preview_chars must be greater than zero; {FIX}");
        }
        if self.analytics.repeat_min_matches == 0 {
            bail!("analytics.repeat_min_matches must be greater than zero; {FIX}");
        }
        if self.analytics.repeat_phrase_min_words == 0 {
            bail!("analytics.repeat_phrase_min_words must be greater than zero; {FIX}");
        }
        if self.analytics.repeat_phrase_max_words < self.analytics.repeat_phrase_min_words {
            bail!("analytics.repeat_phrase_max_words must be >= repeat_phrase_min_words; {FIX}");
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

fn resolve_index_refresh_setting(
    cli: Option<IndexRefresh>,
    environment: Option<&str>,
    configured: IndexRefresh,
    configured_explicitly: bool,
) -> Result<(IndexRefresh, String)> {
    if let Some(value) = cli {
        return Ok((value, "cli --index-refresh".to_string()));
    }
    if let Some(raw) = environment {
        return Ok((
            raw.parse().map_err(|error: String| {
                anyhow::anyhow!("AI_SESSION_SEARCH_INDEX_REFRESH {error}")
            })?,
            "environment AI_SESSION_SEARCH_INDEX_REFRESH".to_string(),
        ));
    }
    Ok((
        configured,
        if configured_explicitly {
            "config file"
        } else {
            "typed default"
        }
        .to_string(),
    ))
}

fn resolve_threads_setting(
    cli: Option<usize>,
    canonical_env: Option<&str>,
    configured: usize,
    configured_explicitly: bool,
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
        ("aistudio", &mut config.providers.aistudio),
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
    app_path: PathBuf,
    legacy_paths: &[PathBuf],
) -> PathBuf {
    if let Some(path) = override_path {
        return expand_override_path(path);
    }
    // The app-owned configuration is deliberately a sibling of harness directories such as
    // ~/.claude and ~/.codex. Older platform/XDG paths remain readable until the user creates the
    // new file, so an upgrade cannot silently discard working configuration.
    if app_path.exists() {
        return app_path;
    }
    legacy_paths
        .iter()
        .find(|path| path.exists())
        .cloned()
        .unwrap_or(app_path)
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
    fn validate_rejects_every_zero_or_inverted_field_with_a_concrete_fix() {
        // Each condition, in isolation, must reject and name a concrete recovery action —
        // not just restate the constraint. Regression-lock for validate()'s bail! messages.
        type BreakField = fn(&mut Config);
        let cases: &[(BreakField, &str)] = &[
            (
                |c| c.release_notifications.minimum_check_interval_hours = 0,
                "release_notifications.minimum_check_interval_hours must be 1 or greater, got 0",
            ),
            (
                |c| c.release_notifications.request_timeout_ms = 0,
                "release_notifications.request_timeout_ms must be 1 or greater, got 0",
            ),
            (
                |c| c.search.default_limit = 0,
                "search.default_limit must be greater than zero",
            ),
            (
                |c| c.mcp.search_messages_limit = 0,
                "mcp.search_messages_limit must be greater than zero",
            ),
            (
                |c| c.mcp.preview_chars = 0,
                "mcp.preview_chars must be greater than zero",
            ),
            (
                |c| c.cli.evidence_preview_chars = 0,
                "cli.evidence_preview_chars must be greater than zero",
            ),
            (
                |c| c.analytics.repeat_min_matches = 0,
                "analytics.repeat_min_matches must be greater than zero",
            ),
            (
                |c| c.analytics.repeat_phrase_min_words = 0,
                "analytics.repeat_phrase_min_words must be greater than zero",
            ),
            (
                |c| c.analytics.repeat_phrase_max_words = c.analytics.repeat_phrase_min_words - 1,
                "analytics.repeat_phrase_max_words must be >= repeat_phrase_min_words",
            ),
        ];
        for (break_field, expected_prefix) in cases {
            let mut config = Config::default();
            break_field(&mut config);
            let error = config.validate().unwrap_err().to_string();
            assert!(error.starts_with(expected_prefix), "{error}");
            assert!(
                error.contains("aise config example") && error.contains("replaces the entire file"),
                "missing non-destructive recovery guidance: {error}"
            );
        }
        assert!(Config::default().validate().is_ok());
    }

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
        // Sibling settings still take their defaults.
        assert!(cfg.search.prefer_current_repo);
        assert_eq!(cfg.search.default_limit, 50);
    }

    #[test]
    fn message_search_panels_preserve_current_behavior_when_omitted() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.search.message_search.default_limit, None);
        assert_eq!(cfg.search.message_search.context.context_before, None);
        assert_eq!(cfg.search.message_search.context.context_after, None);
        assert!(cfg.search.budgets.max_hits_per_page.is_none());
        assert!(cfg.search.budgets.max_context_neighbors_per_hit.is_none());
        assert_eq!(cfg.search.scope.mode, SearchScopeMode::All);
        assert!(cfg.search.scope.roots.is_empty());
        assert!(!cfg.search.scope.include_invocation_directory);
        assert!(cfg.search.purposes.is_empty());
    }

    #[test]
    fn message_search_panels_parse_typed_values_and_purpose_preferences() {
        let cfg: Config = toml::from_str(
            r#"
            [search.message-search]
            default_limit = 25
            match_evidence_max_chars = 120

            [search.message-search.context]
            context_before = 2
            context_after = 3

            [search.budgets]
            max_hits_per_page = 100
            max_context_neighbors_per_hit = 8

            [search.scope]
            mode = "allowed-roots"
            roots = ["/workspace/a", "/workspace/b"]
            include_invocation_directory = true

            [search.purposes.historical-audit]
            version = 1
            operation = "message-search"

            [search.purposes.historical-audit.preferences]
            default_limit = 40
            context_before = 1
            context_after = 4
            receipt_level = "summary"
            include_refs = true
            lines_per_message = -8
            match_evidence_max_chars = 90
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.search
                .message_search
                .default_limit
                .map(NonZeroUsize::get),
            Some(25)
        );
        assert_eq!(cfg.search.message_search.context.context_before, Some(2));
        assert_eq!(cfg.search.message_search.context.context_after, Some(3));
        assert_eq!(
            cfg.search
                .message_search
                .match_evidence_max_chars
                .map(NonZeroUsize::get),
            Some(120)
        );
        assert_eq!(cfg.search.scope.mode, SearchScopeMode::AllowedRoots);
        assert_eq!(cfg.search.scope.roots.len(), 2);
        let purpose = &cfg.search.purposes["historical-audit"];
        assert_eq!(purpose.version.get(), 1);
        assert_eq!(purpose.operation, SearchOperation::MessageSearch);
        assert_eq!(
            purpose.preferences.receipt_level,
            Some(crate::message_search::ReceiptLevel::Summary)
        );
        assert_eq!(purpose.preferences.lines_per_message, Some(-8));
        assert_eq!(
            purpose
                .preferences
                .match_evidence_max_chars
                .map(NonZeroUsize::get),
            Some(90)
        );
    }

    #[test]
    fn message_search_panels_reject_zero_unknown_and_conflicting_values() {
        for toml in [
            "[search.message-search]\ndefault_limit = 0\n",
            "[search.message-search]\nmatch_evidence_max_chars = 0\n",
            "[search.budgets]\nmax_context_neighbors_per_hit = 0\n",
            "[search.message-search.context]\nmessages_before = 2\n",
            "[search.message-search.context]\nmessages_after = 2\n",
            "[search.budgets]\nmax_results_per_page = 10\n",
            "[search.budgets]\nmax_context_messages = 10\n",
            "[search.budgets]\ntimeout_ms = 5000\n",
            "[search.purposes.audit]\nversion = 0\noperation = \"message-search\"\n",
        ] {
            assert!(toml::from_str::<Config>(toml).is_err(), "{toml}");
        }
        let unknown = toml::from_str::<Config>(
            "[search.purposes.audit]\nversion = 1\noperation = \"message-search\"\n\
             [search.purposes.audit.preferences]\nquery_mode = \"regex\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(unknown.contains("unknown field"), "{unknown}");

        let mut cfg: Config =
            toml::from_str("[search.scope]\nmode = \"all\"\nroots = [\"/workspace\"]\n").unwrap();
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("allowed-roots"));

        cfg.search.scope = SearchScopeConfig::default();
        cfg.search.message_search.context.context_before = Some(3);
        cfg.search.message_search.context.context_after = Some(4);
        cfg.search.budgets.max_context_neighbors_per_hit = NonZeroUsize::new(6);
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_context_neighbors_per_hit"));

        cfg.search.message_search.context = MessageContextDefaults::default();
        cfg.search.budgets.max_context_neighbors_per_hit = None;
        cfg.search.purposes.insert(
            "hard2parse".into(),
            PurposeDefinition {
                version: NonZeroU32::new(1).unwrap(),
                operation: SearchOperation::MessageSearch,
                preferences: MessagePurposePreferences::default(),
            },
        );
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("dash-separated phrase"));
    }

    #[test]
    fn scoring_config_rejects_negative_recency_range() {
        let config = toml::from_str::<Config>("[search.scoring]\nrecency_max_days = -1\n").unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("search.scoring.recency_max_days"), "{error}");
        assert!(error.contains("0 or greater"), "{error}");
    }

    #[test]
    fn configured_message_defaults_must_fit_their_explicit_policy_budget() {
        let maximum = NonZeroUsize::new(10).unwrap();

        let mut operation = Config::default();
        operation.search.budgets.max_hits_per_page = Some(maximum);
        operation.search.message_search.default_limit = NonZeroUsize::new(11);
        let error = operation.validate().unwrap_err().to_string();
        assert!(
            error.contains("search.message-search.default_limit"),
            "{error}"
        );

        let mut surface = Config::default();
        surface.search.budgets.max_hits_per_page = Some(maximum);
        surface.mcp.search_messages_limit = 11;
        let error = surface.validate().unwrap_err().to_string();
        assert!(error.contains("mcp.search_messages_limit"), "{error}");

        let mut purpose = Config::default();
        purpose.search.budgets.max_hits_per_page = Some(maximum);
        purpose.mcp.search_messages_limit = maximum.get();
        purpose.search.purposes.insert(
            "focused-review".into(),
            PurposeDefinition {
                version: NonZeroU32::new(1).unwrap(),
                operation: SearchOperation::MessageSearch,
                preferences: MessagePurposePreferences {
                    default_limit: NonZeroUsize::new(11),
                    ..Default::default()
                },
            },
        );
        let error = purpose.validate().unwrap_err().to_string();
        assert!(
            error.contains("search.purposes.focused-review.default_limit"),
            "{error}"
        );
    }

    #[test]
    fn allowed_roots_configuration_rejects_relative_root_and_file_paths() {
        for root in ["relative/project", "/"] {
            let mut cfg = Config::default();
            cfg.search.scope = SearchScopeConfig {
                mode: SearchScopeMode::AllowedRoots,
                roots: vec![root.into()],
                include_invocation_directory: false,
            };
            let error = cfg.validate().unwrap_err().to_string();
            assert!(error.contains("search.scope.roots entry"), "{error}");
        }

        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("not-a-directory");
        std::fs::write(&file, b"fixture").unwrap();
        let mut cfg = Config::default();
        cfg.search.scope = SearchScopeConfig {
            mode: SearchScopeMode::AllowedRoots,
            roots: vec![file.to_string_lossy().into_owned()],
            include_invocation_directory: false,
        };
        let error = cfg.validate().unwrap_err().to_string();
        assert!(error.contains("not a directory"), "{error}");
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
    fn performance_rejects_removed_custom_trigram_tuning() {
        for key in ["regex_prefilter_min_corpus", "trigram_rebuild_delta"] {
            let error = toml::from_str::<Config>(&format!("[performance]\n{key} = 10000\n"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("unknown field"), "{key}: {error}");
        }
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
    fn mcp_defaults_parse_bounded_agent_pages_and_unbounded_sql_time() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.mcp.max_concurrent_reads, McpReadConcurrency::Auto);
        assert_eq!(
            cfg.resolve_mcp_max_concurrent_reads().unwrap().get(),
            std::thread::available_parallelism()
                .unwrap_or(NonZeroUsize::MIN)
                .get()
                .div_ceil(2)
        );
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
            cfg.mcp.get_session_transcript_lines,
            DEFAULT_MCP_GET_SESSION_TRANSCRIPT_LINE_WINDOW
        );
        assert_eq!(cfg.mcp.preview_chars, DEFAULT_MCP_PREVIEW_CHARS);
        assert_eq!(cfg.mcp.summary_items, DEFAULT_MCP_SUMMARY_ITEMS);
        assert_eq!(
            cfg.mcp.query_max_cell_chars,
            DEFAULT_MCP_QUERY_MAX_CELL_CHARS
        );
        assert_eq!(cfg.mcp.query_timeout_ms, 0);
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
            max_concurrent_reads = "auto"
            search_sessions_limit = 7
            list_sessions_limit = 8
            search_messages_limit = 9
            get_session_transcript_lines = -12
            preview_chars = 77
            query_max_cell_chars = 13
            query_timeout_ms = 2500

            [mcp.internal]
            schema_summary_tables = 2
            schema_summary_columns = 3
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mcp.max_concurrent_reads, McpReadConcurrency::Auto);
        assert_eq!(
            cfg.resolve_mcp_max_concurrent_reads().unwrap().get(),
            std::thread::available_parallelism()
                .unwrap_or(NonZeroUsize::MIN)
                .get()
                .div_ceil(2)
        );
        let host: Config = toml::from_str("[mcp]\nmax_concurrent_reads = \"host\"\n").unwrap();
        assert_eq!(host.mcp.max_concurrent_reads, McpReadConcurrency::Host);
        assert_eq!(
            host.resolve_mcp_max_concurrent_reads().unwrap(),
            std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
        );
        assert_eq!(cfg.mcp.search_sessions_limit, 7);
        assert_eq!(cfg.mcp.list_sessions_limit, 8);
        assert_eq!(cfg.mcp.search_messages_limit, 9);
        assert_eq!(cfg.mcp.get_session_transcript_lines, -12);
        assert_eq!(cfg.mcp.preview_chars, 77);
        assert_eq!(cfg.mcp.query_max_cell_chars, 13);
        assert_eq!(cfg.mcp.query_timeout_ms, 2500);
        assert_eq!(cfg.mcp.internal.schema_summary_tables, 2);
        assert_eq!(cfg.mcp.internal.schema_summary_columns, 3);

        let cfg: Config = toml::from_str("[mcp]\nmax_concurrent_reads = 2\n").unwrap();
        assert_eq!(
            cfg.mcp.max_concurrent_reads,
            McpReadConcurrency::Fixed(NonZeroUsize::new(2).unwrap())
        );
        assert_eq!(cfg.resolve_mcp_max_concurrent_reads().unwrap().get(), 2);
        let maximum = tokio::sync::Semaphore::MAX_PERMITS;
        let cfg: Config =
            toml::from_str(&format!("[mcp]\nmax_concurrent_reads = {maximum}\n")).unwrap();
        assert_eq!(
            cfg.resolve_mcp_max_concurrent_reads().unwrap().get(),
            maximum
        );

        for (invalid, expected) in [("0", "got 0"), ("-2", "got -2"), ("\"all\"", "got \"all\"")] {
            let error =
                toml::from_str::<Config>(&format!("[mcp]\nmax_concurrent_reads = {invalid}\n"))
                    .unwrap_err()
                    .to_string();
            assert!(
                error.contains(&format!(
                    "must be \"auto\", \"host\", or an integer from 1 through {}",
                    tokio::sync::Semaphore::MAX_PERMITS
                )),
                "{error}"
            );
            assert!(error.contains(expected), "{error}");
            assert!(
                error.contains("use \"auto\" for the balanced default"),
                "{error}"
            );
        }

        if let Some(too_many) = maximum.checked_add(1) {
            let error =
                toml::from_str::<Config>(&format!("[mcp]\nmax_concurrent_reads = {too_many}\n"))
                    .unwrap_err()
                    .to_string();
            assert!(
                error.contains(&format!(
                    "must be \"auto\", \"host\", or an integer from 1 through {maximum}"
                )),
                "{error}"
            );
            assert!(error.contains(&format!("got {too_many}")), "{error}");
        }
    }

    #[test]
    fn cli_defaults_parse_and_default_to_bounded_show() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(
            cfg.cli.show_transcript_lines,
            DEFAULT_CLI_SHOW_TRANSCRIPT_LINE_WINDOW
        );
        assert_eq!(cfg.cli.lines_per_message, DEFAULT_MESSAGE_LINE_WINDOW);
        assert_eq!(cfg.mcp.lines_per_message, DEFAULT_MESSAGE_LINE_WINDOW);
        assert_eq!(
            cfg.cli.evidence_preview_chars,
            DEFAULT_CLI_EVIDENCE_PREVIEW_CHARS
        );
        assert_eq!(cfg.cli.summary_items, DEFAULT_CLI_SUMMARY_ITEMS);

        let cfg: Config = toml::from_str(
            r#"
            [mcp]
            lines_per_message = -6

            [cli]
            show_transcript_lines = -12
            lines_per_message = 8
            evidence_preview_chars = 88
            "#,
        )
        .unwrap();
        assert_eq!(cfg.cli.show_transcript_lines, -12);
        assert_eq!(cfg.cli.lines_per_message, 8);
        assert_eq!(cfg.mcp.lines_per_message, -6);
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
            cfg.analytics.repeat_max_examples_per_group,
            DEFAULT_ANALYTICS_REPEAT_MAX_EXAMPLES_PER_GROUP
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
            repeat_max_examples_per_group = 7
            repeat_min_matches = 3
            repeat_phrase_min_words = 4
            repeat_phrase_max_words = 9
            "#,
        )
        .unwrap();
        assert_eq!(cfg.analytics.vocab_limit, 17);
        assert_eq!(cfg.analytics.repeat_max_groups, 18);
        assert_eq!(cfg.analytics.repeat_max_examples_per_group, 7);
        assert_eq!(cfg.analytics.repeat_min_matches, 3);
        assert_eq!(cfg.analytics.repeat_phrase_min_words, 4);
        assert_eq!(cfg.analytics.repeat_phrase_max_words, 9);
    }

    #[test]
    fn mcp_config_rejects_noncanonical_field_names() {
        for config in [
            "[mcp]\nmessage_search_limit = 11\n",
            "[mcp]\nschema_summary_tables = 12\n",
            "[mcp]\nschema_summary_columns = 13\n",
            "[search.budgets]\nmax_response_bytes = 1024\n",
            "[mcp]\nmax_response_bytes = 1024\n",
        ] {
            assert!(toml::from_str::<Config>(config).is_err());
        }
    }

    #[test]
    fn native_db_query_defaults_parse_and_default_to_unbounded_execution_time() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.db.query_limit, DEFAULT_DB_QUERY_LIMIT);
        assert_eq!(cfg.db.query_timeout_ms, 0);
        assert!(cfg.release_notifications.enabled);
        assert_eq!(
            cfg.release_notifications.minimum_check_interval_hours,
            DEFAULT_RELEASE_NOTIFICATION_MINIMUM_CHECK_INTERVAL_HOURS
        );
        assert_eq!(
            cfg.release_notifications.request_timeout_ms,
            DEFAULT_RELEASE_NOTIFICATION_REQUEST_TIMEOUT_MS
        );

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
    fn release_notification_names_are_exact_and_old_update_names_are_rejected() {
        let cfg: Config = toml::from_str(
            r#"
            [release_notifications]
            enabled = false
            minimum_check_interval_hours = 12
            request_timeout_ms = 750
            "#,
        )
        .unwrap();
        assert!(!cfg.release_notifications.enabled);
        assert_eq!(cfg.release_notifications.minimum_check_interval_hours, 12);
        assert_eq!(cfg.release_notifications.request_timeout_ms, 750);

        for stale_config in [
            "[release_updates]\npassive_check = false\n",
            "[release_notifications]\npassive_check = false\n",
            "[release_notifications]\npassive_check_interval_hours = 12\n",
            "[release_notifications]\npassive_request_timeout_ms = 750\n",
        ] {
            assert!(
                toml::from_str::<Config>(stale_config).is_err(),
                "stale release-notification name unexpectedly parsed: {stale_config}"
            );
        }
    }

    #[test]
    fn planning_keeps_its_bounded_analytics_default() {
        let cfg = Config::default();
        assert_eq!(cfg.analytics.planning_limit, DEFAULT_ANALYTICS_VOCAB_LIMIT);
        let overridden: Config = toml::from_str("[analytics]\nplanning_limit = 9\n").unwrap();
        assert_eq!(overridden.analytics.planning_limit, 9);
    }

    #[test]
    fn skills_section_rejects_unknown_keys() {
        // deny_unknown_fields: a typo must be named, not silently ignored, or a user who wrote
        // `search_path` would get the embedded default and no indication why.
        let err = toml::from_str::<Config>("[skills]\nsearch_path = []\n")
            .expect_err("a misspelled key must be rejected")
            .to_string();
        assert!(err.contains("search_path"), "{err}");
    }

    #[test]
    fn message_classification_defaults_have_capability_and_mcp_specific_names() {
        let configured: Config = toml::from_str(
            "[capabilities.message_classification]\nlimit = 7\n\
             [mcp]\nrun_message_classification_limit = 11\n",
        )
        .unwrap();
        assert_eq!(configured.capabilities.message_classification.limit, 7);
        assert_eq!(configured.mcp.run_message_classification_limit, 11);

        let zero: Config = toml::from_str("[mcp]\nrun_message_classification_limit = 0\n").unwrap();
        let error = zero
            .validate()
            .expect_err("a zero MCP page cannot make pagination progress")
            .to_string();
        assert!(
            error.contains("mcp.run_message_classification_limit")
                && error.contains("1 or greater")
                && error.contains("got 0"),
            "{error}"
        );

        for removed in [
            "[analytics]\ncorrections_limit = 7\n",
            "[mcp]\nfind_corrections_limit = 11\n",
            "[skills]\nenabled = [\"mine\"]\n",
        ] {
            assert!(
                toml::from_str::<Config>(removed).is_err(),
                "removed prerelease correction settings must fail instead of being ignored"
            );
        }
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
        assert!(
            CONFIG_EXAMPLE_TOML.contains("An explicit `--limit 0` requests every matching session"),
            "the shipped example must not claim explicit zero uses search.default_limit"
        );
        assert!(!CONFIG_EXAMPLE_TOML.contains("omitted or set to 0"));
    }

    #[test]
    fn config_path_selection_prefers_sibling_app_root_and_preserves_legacy_fallbacks() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("home/.ai-session-search/config.toml");
        let platform_legacy = dir.path().join("platform/ai-session-search/config.toml");
        let xdg_legacy = dir
            .path()
            .join("home/.config/ai-session-search/config.toml");

        assert_eq!(
            choose_config_path(
                None,
                app.clone(),
                &[platform_legacy.clone(), xdg_legacy.clone()]
            ),
            app,
            "new installs use the app-owned sibling directory"
        );

        fs::create_dir_all(xdg_legacy.parent().unwrap()).unwrap();
        fs::write(&xdg_legacy, "").unwrap();
        assert_eq!(
            choose_config_path(
                None,
                app.clone(),
                &[platform_legacy.clone(), xdg_legacy.clone()]
            ),
            xdg_legacy,
            "an existing XDG config remains active until the app config exists"
        );

        fs::create_dir_all(platform_legacy.parent().unwrap()).unwrap();
        fs::write(&platform_legacy, "").unwrap();
        assert_eq!(
            choose_config_path(None, app.clone(), &[platform_legacy.clone(), xdg_legacy]),
            platform_legacy,
            "the first existing legacy location has deterministic precedence"
        );

        fs::create_dir_all(app.parent().unwrap()).unwrap();
        fs::write(&app, "").unwrap();
        assert_eq!(
            choose_config_path(None, app.clone(), &[platform_legacy]),
            app,
            "the sibling app config wins once explicitly created"
        );
    }

    #[test]
    fn explicit_config_path_override_has_highest_precedence() {
        let override_path = PathBuf::from("/portable/config.toml");
        assert_eq!(
            choose_config_path(
                Some(override_path.clone()),
                PathBuf::from("/home/.ai-session-search/config.toml"),
                &[PathBuf::from("/legacy/config.toml")],
            ),
            override_path
        );
    }

    #[test]
    fn resolve_wraps_a_validation_failure_with_the_offending_config_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "[search]\ndefault_limit = 0\n").unwrap();
        let error = Config::resolve_with_environment(
            ConfigOverrides {
                config_path: Some(config_path.clone()),
                ..Default::default()
            },
            ConfigEnvironment::default(),
        )
        .unwrap_err();
        let chain = format!("{error:#}");
        assert!(
            chain.contains(&config_path.display().to_string()),
            "{chain}"
        );
        assert!(
            chain.contains("search.default_limit must be greater than zero"),
            "{chain}"
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
        assert!(toml.contains("lines_per_message"));
        assert!(toml.contains("evidence_preview_chars"));
        assert!(toml.contains("[capabilities.message_classification]"));
        assert!(toml.contains("run_message_classification_limit"));
        assert!(!toml.contains("corrections_limit"));
        assert!(toml.contains("planning_limit"));
        assert!(toml.contains("vocab_limit"));
        assert!(toml.contains("repeat_max_groups"));
        assert!(toml.contains("repeat_max_examples_per_group"));
        assert!(toml.contains("query_timeout_ms"));
        assert!(toml.contains("schema_summary_tables"));

        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("auto_reindex_busy_timeout_ms"));
        assert!(json.contains("auto_reindex_interval_ms"));
        assert!(json.contains("search_messages_limit"));
        assert!(json.contains("get_session_transcript_lines"));
        assert!(json.contains("preview_chars"));
        assert!(json.contains("show_transcript_lines"));
        assert!(json.contains("lines_per_message"));
        assert!(json.contains("evidence_preview_chars"));
        assert!(json.contains("\"capabilities\""));
        assert!(json.contains("run_message_classification_limit"));
        assert!(!json.contains("corrections_limit"));
        assert!(json.contains("planning_limit"));
        assert!(json.contains("vocab_limit"));
        assert!(json.contains("repeat_max_groups"));
        assert!(json.contains("repeat_max_examples_per_group"));
        assert!(json.contains("query_timeout_ms"));
        assert!(json.contains("schema_summary_tables"));
    }

    #[test]
    fn resolve_threads_precedence_uses_pure_inputs() {
        let (threads, origin) = resolve_threads_setting(Some(11), Some("7"), 3, true).unwrap();
        assert_eq!((threads, origin.as_str()), (11, "cli --threads"));

        let (threads, origin) = resolve_threads_setting(None, Some("7"), 3, true).unwrap();
        assert_eq!(
            (threads, origin.as_str()),
            (7, "environment AI_SESSION_SEARCH_THREADS")
        );
        assert!(resolve_threads_setting(None, Some("0"), 3, true).is_err());
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
    fn provider_config_uses_the_public_aistudio_identifier() {
        let serialized = toml::to_string(&Config::default()).unwrap();
        assert!(serialized.contains("[providers.aistudio]"));
        assert!(!serialized.contains("[providers.ai-studio]"));
        assert!(toml::from_str::<Config>("[providers.aistudio]\nenabled = false\n").is_ok());
        assert!(toml::from_str::<Config>("[providers.ai-studio]\nenabled = false\n").is_err());
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
    }

    #[test]
    fn index_refresh_resolver_applies_cli_environment_config_default_precedence() {
        let configured = IndexRefresh::ExistingOnly;
        assert_eq!(
            resolve_index_refresh_setting(
                Some(IndexRefresh::BeforeQuery),
                Some("auto"),
                configured,
                true,
            )
            .unwrap(),
            (IndexRefresh::BeforeQuery, "cli --index-refresh".to_string())
        );
        assert_eq!(
            resolve_index_refresh_setting(None, Some("auto"), configured, true).unwrap(),
            (
                IndexRefresh::Auto,
                "environment AI_SESSION_SEARCH_INDEX_REFRESH".to_string(),
            )
        );
        assert_eq!(
            resolve_index_refresh_setting(None, None, configured, true).unwrap(),
            (IndexRefresh::ExistingOnly, "config file".to_string())
        );
        assert_eq!(
            resolve_index_refresh_setting(None, None, IndexRefresh::Auto, false).unwrap(),
            (IndexRefresh::Auto, "typed default".to_string())
        );
        assert!(
            resolve_index_refresh_setting(None, Some("later"), configured, true)
                .unwrap_err()
                .to_string()
                .contains(
                    "AI_SESSION_SEARCH_INDEX_REFRESH expected auto, before-query, or existing-only"
                )
        );
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
[providers.aistudio]
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
