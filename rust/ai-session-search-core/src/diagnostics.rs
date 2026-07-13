use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::models::{DiagnosticStatus, Provider, ProviderHealth};
use crate::util::{normalize_path, which};

pub fn collect(config: &Config, db: &Db) -> Result<DiagnosticStatus> {
    let index_status = db.index_status()?;
    let providers = crate::source::inventory(config)
        .into_iter()
        .map(|source| {
            let provider = source.provider;
            let parser = index_status
                .parser_health
                .providers
                .iter()
                .find(|item| item.provider == provider);
            let (cli_available, resume_command) = match provider {
                Provider::Claude => (
                    which("claude").is_some(),
                    Some("claude --resume <session-id>"),
                ),
                Provider::Codex => (which("codex").is_some(), Some("codex resume <session-id>")),
                Provider::Pi => (which("pi").is_some(), Some("pi --session <session-id>")),
                Provider::Cursor => (which("cursor").is_some(), None),
                Provider::Antigravity => (
                    which("agy").is_some() || which("antigravity").is_some(),
                    None,
                ),
                Provider::ClaudeDesktop => (false, None),
                Provider::AiStudio | Provider::GeminiCli => (false, None),
            };
            ProviderHealth {
                provider,
                enabled: source.enabled,
                cli_available,
                roots: source.roots,
                discovered_files: source.discovered_files,
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
