// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::models::{DiagnosticStatus, IndexStatus, ParserHealth, Provider, ProviderHealth};
use crate::util::{normalize_path, which};

pub fn collect(config: &Config, db: &Db) -> Result<DiagnosticStatus> {
    let inventory = crate::source::inventory_snapshot(config);
    let discovery_warnings = inventory.warnings.clone();
    let parser_health = db.parser_health()?;
    let stale_sources = db.stale_session_sources()?;
    let indexed = indexed_source_set(db)?;
    let mut index_status = classify_index_status(
        parser_health,
        &stale_sources,
        &inventory.discovered,
        &indexed,
    );
    index_status.readiness = crate::background_refresh::readiness_status(config, db)?;
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
                // Discovered files for THIS provider that produced no session row. Reported
                // beside discovered_files/indexed_sessions so a reader is not left to infer a
                // relationship between two counters that come from different subsystems.
                unindexed_files: inventory
                    .discovered
                    .iter()
                    .filter(|source| source.0 == provider && !indexed.contains(*source))
                    .count() as i64,
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
        discovery_warnings,
        providers,
    })
}

pub fn index_status(config: &Config, db: &Db) -> Result<IndexStatus> {
    let inventory = crate::source::inventory_snapshot(config);
    let mut status = index_status_for_discovered(db, &inventory.discovered)?;
    status.readiness = crate::background_refresh::readiness_status(config, db)?;
    Ok(status)
}

fn index_status_for_discovered(
    db: &Db,
    discovered: &HashSet<(Provider, String)>,
) -> Result<IndexStatus> {
    let parser_health = db.parser_health()?;
    let stale_sources = db.stale_session_sources()?;
    let indexed = indexed_source_set(db)?;
    Ok(classify_index_status(
        parser_health,
        &stale_sources,
        discovered,
        &indexed,
    ))
}

/// One discovered file that produced no session row, and why.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnindexedFile {
    pub provider: Provider,
    /// The discovered file that is absent from the index.
    pub path: String,
    /// The session id this file's content resolves to when parsed now.
    pub resolves_to: String,
    /// The indexed file already holding that id, when the id is taken. `None` means the id is
    /// free, so the omission is not a collision and the file was skipped for another reason.
    pub id_already_held_by: Option<String>,
}

/// Explain each discovered-but-unindexed file by reparsing it.
///
/// The reason is recomputed rather than recorded. A stored skip-reason would need a table, a
/// schema-version bump, and a migration, and would describe a past run under past code, which
/// is the least trustworthy moment to be quoting. Every cause observed so far is deterministic
/// from the file itself, and the set is small by construction: if it were large the index
/// would be broadly broken, which is a different problem.
pub fn explain_unindexed(config: &Config, db: &Db) -> Result<Vec<UnindexedFile>> {
    let indexed = indexed_source_set(db)?;
    let holders: HashMap<(Provider, String), String> = db
        .indexed_source_identities()?
        .into_iter()
        .map(|(provider, source_path, _, id)| ((provider, id), source_path))
        .collect();

    let providers = crate::source::ProviderSet::new(config);
    let mut explained = Vec::new();
    for source in providers.discover_enabled(config).sources {
        let key = (source.provider, normalize_path(&source.path));
        if indexed.contains(&key) {
            continue;
        }
        let parsed = providers.parse(&source);
        let resolves_to = parsed.session.provider_session_id.clone();
        let holder = holders
            .get(&(
                source.provider,
                format!("{}:{resolves_to}", source.provider),
            ))
            .or_else(|| holders.get(&(source.provider, resolves_to.clone())))
            .cloned();
        explained.push(UnindexedFile {
            provider: source.provider,
            path: key.1,
            resolves_to,
            id_already_held_by: holder,
        });
    }
    Ok(explained)
}

/// The (provider, source_path) pairs that produced at least one session row. Paired with the
/// discovered set, the difference is the set of files that are on disk but absent from the index.
fn indexed_source_set(db: &Db) -> Result<HashSet<(Provider, String)>> {
    Ok(db
        .indexed_source_identities()?
        .into_iter()
        .map(|(provider, source_path, _, _)| (provider, source_path))
        .collect())
}

fn classify_index_status(
    parser_health: ParserHealth,
    stale_sources: &[(Provider, String)],
    discovered: &HashSet<(Provider, String)>,
    indexed: &HashSet<(Provider, String)>,
) -> IndexStatus {
    let repairable_stale_sessions = stale_sources
        .iter()
        .filter(|source| discovered.contains(*source))
        .count() as i64;
    let unavailable_stale_sessions = parser_health
        .stale_sessions
        .saturating_sub(repairable_stale_sessions);
    // Discovered but absent from the index entirely. Counted by set difference rather than by
    // subtracting the two adjacent counters, because retained sessions make indexed exceed
    // discovered and the subtraction would then measure the wrong thing (and clamp to zero).
    let unindexed_files = discovered.difference(indexed).count() as i64;
    let repair_commands = if !parser_health.schema_current || repairable_stale_sessions > 0 {
        vec!["aise reindex --full".to_string()]
    } else if unindexed_files > 0 {
        vec!["aise reindex".to_string()]
    } else {
        Vec::new()
    };
    IndexStatus {
        parser_health,
        repairable_stale_sessions,
        unavailable_stale_sessions,
        unindexed_files,
        repair_commands,
        readiness: crate::models::IndexReadinessStatus::not_started(),
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

    /// A discovered file that produced no session row is invisible to stale-session
    /// accounting: every health signal is keyed by a session that WAS indexed, so a file
    /// that never became one contributes to none of them. That is how 65 of 414 codex
    /// rollouts stayed missing while `get_index_status` reported stale_sessions 0,
    /// parse_warnings 0 and repair_commands []. The gap is counted by set difference
    /// against the indexed source paths, NOT by subtracting indexed_sessions from
    /// discovered_files: retained sessions make indexed exceed discovered (claude reported
    /// 858 indexed against 645 discovered), so the subtraction is not the same quantity.
    #[test]
    fn unindexed_discovered_files_are_counted_and_offered_a_repair() {
        let discovered = HashSet::from([
            (Provider::Codex, "/parent.jsonl".to_string()),
            (Provider::Codex, "/fork.jsonl".to_string()),
        ]);
        // Only the parent produced a row; the fork collided onto it and was dropped.
        let indexed = HashSet::from([(Provider::Codex, "/parent.jsonl".to_string())]);
        let status = classify_index_status(health(true, 0), &[], &discovered, &indexed);
        assert_eq!(status.unindexed_files, 1);
        assert_eq!(
            status.repair_commands,
            ["aise reindex"],
            "a newly discovered or newly reclaimable file needs the incremental repair"
        );

        // Retained sessions (indexed source no longer discoverable) are NOT unindexed files.
        let retained = HashSet::from([
            (Provider::Claude, "/live.jsonl".to_string()),
            (Provider::Claude, "/deleted.jsonl".to_string()),
        ]);
        let only_live = HashSet::from([(Provider::Claude, "/live.jsonl".to_string())]);
        let status = classify_index_status(health(true, 0), &[], &only_live, &retained);
        assert_eq!(
            status.unindexed_files, 0,
            "an indexed source that is no longer discoverable is retained, not unindexed"
        );
        assert!(status.repair_commands.is_empty());
    }

    #[test]
    fn repair_guidance_only_targets_schema_or_discoverable_stale_sources() {
        let stale = vec![
            (Provider::Claude, "/live.jsonl".to_string()),
            (Provider::Claude, "/archived.jsonl".to_string()),
        ];
        let discovered = HashSet::from([(Provider::Claude, "/live.jsonl".to_string())]);
        // Everything discovered is indexed here, so unindexed_files stays 0 and these
        // assertions continue to exercise only the stale/schema repair logic.
        let mixed = classify_index_status(health(true, 2), &stale, &discovered, &discovered);
        assert_eq!(mixed.repairable_stale_sessions, 1);
        assert_eq!(mixed.unavailable_stale_sessions, 1);
        assert_eq!(mixed.repair_commands, ["aise reindex --full"]);

        let archived =
            classify_index_status(health(true, 1), &stale[1..], &discovered, &discovered);
        assert_eq!(archived.repairable_stale_sessions, 0);
        assert_eq!(archived.unavailable_stale_sessions, 1);
        assert!(archived.repair_commands.is_empty());

        let schema = classify_index_status(health(false, 1), &stale[1..], &discovered, &discovered);
        assert_eq!(schema.repair_commands, ["aise reindex --full"]);
    }
}
