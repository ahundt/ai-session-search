use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::models::{DiagnosticStatus, Provider, ProviderHealth};
use crate::providers::{
    antigravity::AntigravityAdapter, claude::ClaudeAdapter, codex::CodexAdapter,
    cursor::CursorAdapter, pi::PiAdapter,
};
use crate::util::{normalize_path, which};

pub fn collect(config: &Config, db: &Db) -> Result<DiagnosticStatus> {
    let index_status = db.index_status()?;
    let claude_sources = ClaudeAdapter::new(config.claude_paths()).discover();
    let desktop_sources = ClaudeAdapter::new(config.claude_desktop_paths()).discover();
    let discovered = [
        (
            Provider::Claude,
            claude_sources
                .iter()
                .filter(|source| source.provider == Provider::Claude)
                .count(),
        ),
        (
            Provider::ClaudeDesktop,
            desktop_sources
                .iter()
                .filter(|source| source.provider == Provider::ClaudeDesktop)
                .count(),
        ),
        (
            Provider::Codex,
            CodexAdapter::new(config.codex_paths(), config.codex_home())
                .discover()
                .len(),
        ),
        (
            Provider::Cursor,
            CursorAdapter::new(config.cursor_paths()).discover().len(),
        ),
        (
            Provider::Antigravity,
            AntigravityAdapter::new(config.antigravity_paths())
                .discover()
                .len(),
        ),
        (
            Provider::Pi,
            PiAdapter::new(config.pi_paths()).discover().len(),
        ),
    ];

    let roots = |provider| match provider {
        Provider::Claude => config.claude_paths(),
        Provider::ClaudeDesktop => config.claude_desktop_paths(),
        Provider::Codex => config.codex_paths(),
        Provider::Cursor => config.cursor_paths(),
        Provider::Antigravity => config.antigravity_paths(),
        Provider::Pi => config.pi_paths(),
    };
    let providers = discovered
        .into_iter()
        .map(|(provider, discovered_files)| {
            let parser = index_status
                .parser_health
                .providers
                .iter()
                .find(|item| item.provider == provider);
            let (cli_available, resume_command) = match provider {
                Provider::Claude => (which("claude").is_some(), Some("claude --resume <session-id>")),
                Provider::Codex => (which("codex").is_some(), Some("codex resume <session-id>")),
                Provider::Pi => (which("pi").is_some(), Some("pi --session <session-id>")),
                Provider::Cursor => (which("cursor").is_some(), None),
                Provider::Antigravity => (
                    which("agy").is_some() || which("antigravity").is_some(),
                    None,
                ),
                Provider::ClaudeDesktop => (false, None),
            };
            ProviderHealth {
                provider,
                cli_available,
                roots: roots(provider)
                    .iter()
                    .map(|path| normalize_path(path))
                    .collect(),
                discovered_files,
                indexed_sessions: parser.map_or(0, |item| item.indexed_sessions),
                expected_parse_version: parser.map_or_else(
                    || crate::util::provider_parse_version(provider).to_string(),
                    |item| item.expected_parse_version.clone(),
                ),
                current_sessions: parser.map_or(0, |item| item.current_sessions),
                stale_sessions: parser.map_or(0, |item| item.stale_sessions),
                resume_supported: resume_command.is_some(),
                resume_command: resume_command.map(str::to_string),
            }
        })
        .collect();

    Ok(DiagnosticStatus {
        db_path: normalize_path(&config.db_path()),
        index_status,
        providers,
    })
}
