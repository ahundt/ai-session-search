use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::util::expand_tilde;

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub claude: ProviderConfig,
    #[serde(default)]
    pub codex: ProviderConfig,
    #[serde(default)]
    pub cursor: ProviderConfig,
    #[serde(default)]
    pub antigravity: ProviderConfig,
    #[serde(default)]
    pub pi: ProviderConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexConfig {
    pub db_path: Option<String>,
    pub cache_dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiConfig {
    #[serde(default = "default_preview_lines")]
    pub preview_lines: usize,
}

#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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

/// Analytics overrides (TOML; parity with aise's config.json). All correction/planning
/// criteria are configurable here — none are hard-coded fixed lists; the built-ins are
/// the documented fallback.
#[derive(Debug, Clone, Default, Deserialize)]
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

fn default_true() -> bool {
    true
}

fn default_limit() -> usize {
    50
}

fn default_preview_lines() -> usize {
    30
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
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
        Self {
            providers: ProvidersConfig {
                claude: ProviderConfig {
                    enabled: true,
                    paths: vec![home.join(".claude/projects").to_string_lossy().to_string()],
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
                    paths: vec![home.join(".gemini/antigravity/brain").to_string_lossy().to_string()],
                },
                pi: ProviderConfig {
                    enabled: true,
                    paths: vec![home.join(".pi/agent/sessions").to_string_lossy().to_string()],
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
            },
            ui: UiConfig { preview_lines: 30 },
            search: SearchConfig {
                default_limit: 50,
                prefer_current_repo: true,
                scoring: ScoringConfig::default(),
            },
            analytics: AnalyticsConfig::default(),
        }
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

        let defaults = Self::default();
        if config.providers.claude.paths.is_empty() {
            config.providers.claude.paths = defaults.providers.claude.paths;
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

    pub fn config_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".config/sessiongrep/config.toml")
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
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".codex")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(cfg.search.scoring.summary_score, 450, "untouched weight keeps its default");
        assert_eq!(cfg.search.scoring.fts_candidate_floor, crate::db::FTS_CANDIDATE_FLOOR);
        // Sibling settings still take their defaults.
        assert!(cfg.search.prefer_current_repo);
        assert_eq!(cfg.search.default_limit, 50);
    }
}
