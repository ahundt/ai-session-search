//! Canonical provider discovery and public source inventory.

use std::collections::HashSet;

use serde::Serialize;

use crate::config::Config;
use crate::models::{Provider, SourceFile};
use crate::providers::{
    aistudio::AiStudioAdapter, antigravity::AntigravityAdapter, claude::ClaudeAdapter,
    codex::CodexAdapter, cursor::CursorAdapter, gemini_cli::GeminiCliAdapter, pi::PiAdapter,
};
use crate::util::normalize_path;

/// Providers supported by discovery, in stable presentation order.
pub const PROVIDERS: [Provider; 8] = [
    Provider::Claude,
    Provider::ClaudeDesktop,
    Provider::Codex,
    Provider::Cursor,
    Provider::Antigravity,
    Provider::Pi,
    Provider::AiStudio,
    Provider::GeminiCli,
];

/// Effective discovery configuration and current file count for one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderSourceStatus {
    /// Provider represented by this status.
    pub provider: Provider,
    /// Whether discovery and indexing are enabled for this provider.
    pub enabled: bool,
    /// Effective normalized roots searched for session files.
    pub roots: Vec<String>,
    /// Number of files currently discoverable beneath `roots`.
    pub discovered_files: usize,
}

pub(crate) struct SourceInventory {
    pub(crate) providers: Vec<ProviderSourceStatus>,
    pub(crate) discovered: HashSet<(Provider, String)>,
}

pub(crate) struct ProviderSet {
    pub(crate) claude: ClaudeAdapter,
    claude_desktop: ClaudeAdapter,
    pub(crate) codex: CodexAdapter,
    pub(crate) cursor: CursorAdapter,
    pub(crate) antigravity: AntigravityAdapter,
    pub(crate) pi: PiAdapter,
    pub(crate) aistudio: AiStudioAdapter,
    pub(crate) gemini_cli: GeminiCliAdapter,
}

impl ProviderSet {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            claude: ClaudeAdapter::new(roots(config, Provider::Claude)),
            claude_desktop: ClaudeAdapter::new(roots(config, Provider::ClaudeDesktop)),
            codex: CodexAdapter::new(roots(config, Provider::Codex), config.codex_home()),
            cursor: CursorAdapter::new(roots(config, Provider::Cursor)),
            antigravity: AntigravityAdapter::new(roots(config, Provider::Antigravity)),
            pi: PiAdapter::new(roots(config, Provider::Pi)),
            aistudio: AiStudioAdapter::new(roots(config, Provider::AiStudio)),
            gemini_cli: GeminiCliAdapter::new(roots(config, Provider::GeminiCli)),
        }
    }

    pub(crate) fn discover_enabled(&self, config: &Config) -> Vec<SourceFile> {
        let mut sources = Vec::new();
        if config.providers.claude.enabled {
            sources.extend(self.claude.discover());
        }
        if config.providers.claude_desktop.enabled {
            sources.extend(self.claude_desktop.discover());
        }
        if config.providers.codex.enabled {
            sources.extend(self.codex.discover());
        }
        if config.providers.cursor.enabled {
            sources.extend(self.cursor.discover());
        }
        if config.providers.antigravity.enabled {
            sources.extend(self.antigravity.discover());
        }
        if config.providers.pi.enabled {
            sources.extend(self.pi.discover());
        }
        if config.providers.aistudio.enabled {
            sources.extend(self.aistudio.discover());
        }
        if config.providers.gemini_cli.enabled {
            sources.extend(self.gemini_cli.discover());
        }
        sources
    }
}

/// Discover enabled providers and report every provider's effective configuration.
pub fn inventory(config: &Config) -> Vec<ProviderSourceStatus> {
    inventory_snapshot(config).providers
}

pub(crate) fn inventory_snapshot(config: &Config) -> SourceInventory {
    let discovered = ProviderSet::new(config).discover_enabled(config);
    let providers = PROVIDERS
        .into_iter()
        .map(|provider| ProviderSourceStatus {
            provider,
            enabled: enabled(config, provider),
            roots: roots(config, provider)
                .into_iter()
                .map(|path| normalize_path(&path))
                .collect(),
            discovered_files: discovered
                .iter()
                .filter(|source| source.provider == provider)
                .count(),
        })
        .collect();
    let discovered = discovered
        .into_iter()
        .map(|source| (source.provider, normalize_path(&source.path)))
        .collect();
    SourceInventory {
        providers,
        discovered,
    }
}

fn enabled(config: &Config, provider: Provider) -> bool {
    match provider {
        Provider::Claude => config.providers.claude.enabled,
        Provider::ClaudeDesktop => config.providers.claude_desktop.enabled,
        Provider::Codex => config.providers.codex.enabled,
        Provider::Cursor => config.providers.cursor.enabled,
        Provider::Antigravity => config.providers.antigravity.enabled,
        Provider::Pi => config.providers.pi.enabled,
        Provider::AiStudio => config.providers.aistudio.enabled,
        Provider::GeminiCli => config.providers.gemini_cli.enabled,
    }
}

fn roots(config: &Config, provider: Provider) -> Vec<std::path::PathBuf> {
    let configured = match provider {
        Provider::Claude => config.claude_paths(),
        Provider::ClaudeDesktop => config.claude_desktop_paths(),
        Provider::Codex => config.codex_paths(),
        Provider::Cursor => config.cursor_paths(),
        Provider::Antigravity => config.antigravity_paths(),
        Provider::Pi => config.pi_paths(),
        Provider::AiStudio => config.aistudio_paths(),
        Provider::GeminiCli => config.gemini_cli_paths(),
    };
    canonical_unique_roots(configured)
}

fn canonical_unique_roots(paths: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter_map(|path| {
            let identity = std::fs::canonicalize(&path).unwrap_or(path);
            seen.insert(identity.clone()).then_some(identity)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disable_all(config: &mut Config) {
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;
    }

    #[test]
    fn inventory_includes_all_disabled_providers_without_discovery() {
        let mut config = Config::default();
        disable_all(&mut config);

        let statuses = inventory(&config);

        assert_eq!(statuses.len(), PROVIDERS.len());
        assert!(statuses.iter().all(|status| !status.enabled));
        assert!(statuses.iter().all(|status| status.discovered_files == 0));
    }

    #[test]
    fn inventory_discovers_enabled_snapshot_providers() {
        let dir = tempfile::tempdir().unwrap();
        let aistudio = dir.path().join("aistudio");
        let gemini = dir.path().join("gemini");
        std::fs::create_dir_all(&aistudio).unwrap();
        std::fs::create_dir_all(gemini.join("project/chats")).unwrap();
        std::fs::write(aistudio.join("chat.json"), "{}").unwrap();
        std::fs::write(gemini.join("project/chats/session-one.json"), "{}").unwrap();
        let mut config = Config::default();
        disable_all(&mut config);
        config.providers.aistudio.enabled = true;
        config.providers.aistudio.paths = vec![aistudio.to_string_lossy().into_owned()];
        config.providers.gemini_cli.enabled = true;
        config.providers.gemini_cli.paths = vec![gemini.to_string_lossy().into_owned()];

        let statuses = inventory(&config);

        assert_eq!(
            statuses
                .iter()
                .find(|status| status.provider == Provider::AiStudio)
                .unwrap()
                .discovered_files,
            1
        );
        assert_eq!(
            statuses
                .iter()
                .find(|status| status.provider == Provider::GeminiCli)
                .unwrap()
                .discovered_files,
            1
        );
    }

    #[test]
    fn inventory_canonicalizes_alias_roots_before_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("aistudio");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("chat.json"), "{}").unwrap();
        let mut config = Config::default();
        disable_all(&mut config);
        config.providers.aistudio.enabled = true;
        config.providers.aistudio.paths = vec![
            root.to_string_lossy().into_owned(),
            root.join(".").to_string_lossy().into_owned(),
        ];

        let status = inventory(&config)
            .into_iter()
            .find(|status| status.provider == Provider::AiStudio)
            .unwrap();

        assert_eq!(status.roots.len(), 1);
        assert_eq!(status.discovered_files, 1);
    }
}
