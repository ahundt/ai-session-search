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
            },
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
        let mut config: Self =
            toml::from_str(&raw).with_context(|| "failed to parse config TOML")?;

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

    pub fn codex_home(&self) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".codex")
    }
}
