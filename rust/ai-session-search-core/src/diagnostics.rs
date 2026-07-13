use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::models::{DiagnosticStatus, IndexStatus, ParserHealth, Provider, ProviderHealth};
use crate::util::{normalize_path, which};

pub fn collect(config: &Config, db: &Db) -> Result<DiagnosticStatus> {
    let inventory = crate::source::inventory_snapshot(config);
    let parser_health = db.parser_health()?;
    let stale_sources = db.stale_session_sources()?;
    let index_status = classify_index_status(parser_health, &stale_sources, &inventory.discovered);
    let repairable_by_provider = stale_sources
        .iter()
        .filter(|source| inventory.discovered.contains(*source))
        .fold(
            HashMap::<Provider, i64>::new(),
            |mut counts, (provider, _)| {
                *counts.entry(*provider).or_default() += 1;
                counts
            },
        );
    let providers = inventory
        .providers
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
                repairable_stale_sessions: repairable_by_provider
                    .get(&provider)
                    .copied()
                    .unwrap_or_default(),
                unavailable_stale_sessions: 0,
                resume_supported: resume_command.is_some(),
                resume_command: resume_command.map(str::to_string),
            }
        })
        .collect::<Vec<_>>();
    let providers = providers
        .into_iter()
        .map(|mut provider| {
            provider.unavailable_stale_sessions = provider
                .stale_sessions
                .saturating_sub(provider.repairable_stale_sessions);
            provider
        })
        .collect();

    Ok(DiagnosticStatus {
        db_path: normalize_path(&config.db_path()),
        index_status,
        providers,
    })
}

pub fn index_status(config: &Config, db: &Db) -> Result<IndexStatus> {
    let inventory = crate::source::inventory_snapshot(config);
    index_status_for_discovered(db, &inventory.discovered)
}

fn index_status_for_discovered(
    db: &Db,
    discovered: &HashSet<(Provider, String)>,
) -> Result<IndexStatus> {
    let parser_health = db.parser_health()?;
    let stale_sources = db.stale_session_sources()?;
    Ok(classify_index_status(
        parser_health,
        &stale_sources,
        discovered,
    ))
}

fn classify_index_status(
    parser_health: ParserHealth,
    stale_sources: &[(Provider, String)],
    discovered: &HashSet<(Provider, String)>,
) -> IndexStatus {
    let repairable_stale_sessions = stale_sources
        .iter()
        .filter(|source| discovered.contains(*source))
        .count() as i64;
    let unavailable_stale_sessions = parser_health
        .stale_sessions
        .saturating_sub(repairable_stale_sessions);
    let repair_commands = if parser_health.schema_current && repairable_stale_sessions == 0 {
        Vec::new()
    } else {
        vec!["aise reindex --full".to_string()]
    };
    IndexStatus {
        parser_health,
        repairable_stale_sessions,
        unavailable_stale_sessions,
        repair_commands,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SCHEMA_VERSION;

    fn health(schema_current: bool, stale_sessions: i64) -> ParserHealth {
        ParserHealth {
            schema_version: if schema_current { SCHEMA_VERSION } else { 0 },
            expected_schema_version: SCHEMA_VERSION,
            schema_current,
            indexed_sessions: stale_sessions,
            current_sessions: 0,
            stale_sessions,
            parse_warnings: 0,
            providers: Vec::new(),
        }
    }

    #[test]
    fn repair_guidance_only_targets_schema_or_discoverable_stale_sources() {
        let stale = vec![
            (Provider::Claude, "/live.jsonl".to_string()),
            (Provider::Claude, "/archived.jsonl".to_string()),
        ];
        let discovered = HashSet::from([(Provider::Claude, "/live.jsonl".to_string())]);
        let mixed = classify_index_status(health(true, 2), &stale, &discovered);
        assert_eq!(mixed.repairable_stale_sessions, 1);
        assert_eq!(mixed.unavailable_stale_sessions, 1);
        assert_eq!(mixed.repair_commands, ["aise reindex --full"]);

        let archived = classify_index_status(health(true, 1), &stale[1..], &discovered);
        assert_eq!(archived.repairable_stale_sessions, 0);
        assert_eq!(archived.unavailable_stale_sessions, 1);
        assert!(archived.repair_commands.is_empty());

        let schema = classify_index_status(health(false, 1), &stale[1..], &discovered);
        assert_eq!(schema.repair_commands, ["aise reindex --full"]);
    }
}
