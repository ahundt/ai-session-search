// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;

use crate::config::Config;
use crate::db::Db;
use crate::models::{DiagnosticStatus, IndexStatus, ParserHealth, Provider, ProviderHealth};
use crate::util::{normalize_path, provider_resume_capability};

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
            let capability = provider_resume_capability(provider);
            ProviderHealth {
                provider,
                enabled: source.enabled,
                cli_available: capability.cli_available(),
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
                // Discovered files for THIS provider not represented in the index. Reported
                // beside discovered_files/indexed_sessions so a reader is not left to infer a
                // relationship between two counters that come from different subsystems.
                unindexed_files: inventory
                    .discovered
                    .iter()
                    .filter(|source| source.0 == provider && !indexed.contains(*source))
                    .count() as i64,
                resume_command: capability.command_template(),
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
        unknown_config_keys: config.unknown_config_keys.clone(),
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

/// One discovered file not represented in the index, and why.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
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
        .map(|source| ((source.provider, source.session_id), source.source_path))
        .collect();

    let providers = crate::source::ProviderSet::new(config);
    let mut explained = Vec::new();
    for source in providers.discover_enabled(config).sources {
        let key = (source.provider, normalize_path(&source.path));
        if indexed.contains(&key) {
            continue;
        }
        let parsed = providers.parse(&source).unwrap_or_else(|error| {
            crate::util::minimal_record(source.provider, &source.path, format!("{error:#}"))
        });
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

/// Source paths represented by canonical sessions, unambiguous persisted Gemini families, or the
/// rollouts of a reverted Codex thread. Processing receipts alone do not prove representation.
/// The family lookup adds one metadata scan over Gemini sessions, O(G) rows plus their metadata
/// bytes, and the Codex lookup one scan of Codex receipts; transcripts are never read.
fn indexed_source_set(db: &Db) -> Result<HashSet<(Provider, String)>> {
    let identities = db.indexed_source_identities()?;
    // Codex `thread/revert` starts another rollout file for the same thread, and every rollout's
    // parse is reconciled into the thread's one session, which keeps rows a later parse no longer
    // holds. Only one rollout can be its source path; another is represented when its thread's
    // session is held by a rollout of that thread and its receipt is at the current parser.
    let held_codex_threads: HashSet<String> = identities
        .iter()
        .filter(|source| source.provider == Provider::Codex)
        .filter_map(|source| {
            let thread =
                crate::providers::codex::rollout_thread_id(Path::new(&source.source_path))?;
            (source.session_id == format!("codex:{thread}")).then(|| thread.to_string())
        })
        .collect();
    let mut indexed: HashSet<_> = identities
        .into_iter()
        .map(|source| (source.provider, source.source_path))
        .collect();
    indexed.extend(
        db.gemini_family_sessions_by_path()?
            .into_keys()
            .map(|path| (Provider::GeminiCli, path)),
    );
    if !held_codex_threads.is_empty() {
        let parse_version = crate::util::provider_parse_version(Provider::Codex);
        indexed.extend(
            db.receipt_paths_at_parse_version(Provider::Codex, parse_version)?
                .into_iter()
                .filter(|path| {
                    crate::providers::codex::rollout_thread_id(Path::new(path))
                        .is_some_and(|thread| held_codex_threads.contains(thread))
                })
                .map(|path| (Provider::Codex, path)),
        );
    }
    Ok(indexed)
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

    fn isolated_config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.prime_agent.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;
        config.index.db_path = Some(normalize_path(&dir.join("index.db")));
        config.index.cache_dir = Some(normalize_path(&dir.join("cache")));
        config
    }

    #[test]
    fn doctor_resume_capabilities_agree_with_main_session_plans() {
        use crate::util::{minimal_record, resume_plan, tests::with_stub_binary_on_path};
        let dir = tempfile::tempdir().unwrap();
        let config = isolated_config(dir.path());
        let db = Db::open(&config.db_path()).unwrap();
        for (provider, binary) in [
            (Provider::Claude, "claude"),
            (Provider::Codex, "codex"),
            (Provider::Pi, "pi"),
            (Provider::PrimeAgent, "prime-agent"),
            (Provider::GeminiCli, "gemini"),
            (Provider::Cursor, "cursor"),
            (Provider::Antigravity, "agy"),
            (Provider::ClaudeDesktop, "synthetic-desktop"),
            (Provider::AiStudio, "synthetic-studio"),
        ] {
            with_stub_binary_on_path(binary, || {
                let mut parsed = minimal_record(
                    provider,
                    &dir.path().join("session-synthetic.jsonl"),
                    String::new(),
                );
                parsed.session.cwd = Some(normalize_path(dir.path()));
                parsed.session.provider_session_id = "synthetic".into();
                let report = collect(&config, &db).unwrap();
                let health = report
                    .providers
                    .iter()
                    .find(|health| health.provider == provider)
                    .unwrap();
                assert_eq!(
                    health.cli_available,
                    !matches!(provider, Provider::ClaudeDesktop | Provider::AiStudio),
                    "{provider}"
                );
                match resume_plan(&parsed.session) {
                    Ok((argv, _)) => {
                        assert!(health.cli_available, "{provider}");
                        assert_eq!(
                            health
                                .resume_command
                                .as_deref()
                                .unwrap()
                                .replace("<session-id>", "synthetic"),
                            argv.join(" "),
                            "{provider}"
                        );
                    }
                    Err(_) => assert!(health.resume_command.is_none(), "{provider}"),
                }
            });
        }
    }

    #[test]
    fn indexed_sources_include_only_unambiguous_persisted_family_members() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let predecessor = normalize_path(&dir.path().join("session-family.json"));
        let successor = normalize_path(&dir.path().join("session-family.jsonl"));
        let mut parsed = crate::util::minimal_record(
            Provider::GeminiCli,
            std::path::Path::new(&successor),
            String::new(),
        );
        parsed.session.id = "gemini-cli:family".into();
        parsed.session.provider_session_id = "family".into();
        parsed.session.raw_metadata_json = Some(
            serde_json::json!({
                "gemini_migration_family": {
                    "native_session_id": "family",
                    "predecessor_json": predecessor,
                    "successor_jsonl": successor
                }
            })
            .to_string(),
        );
        db.upsert_session(&parsed, 0, 0).unwrap();
        let discovered = HashSet::from([
            (Provider::GeminiCli, predecessor.clone()),
            (Provider::GeminiCli, successor.clone()),
        ]);
        assert_eq!(
            index_status_for_discovered(&db, &discovered)
                .unwrap()
                .unindexed_files,
            0
        );
        // Contradictory persisted claims cannot prove the predecessor is represented.
        for index in 0..2 {
            parsed.session.id = format!("gemini-cli:conflict-{index}");
            parsed.session.provider_session_id = format!("conflict-{index}");
            parsed.session.source_path =
                normalize_path(&dir.path().join(format!("other-{index}.jsonl")));
            let mut metadata: serde_json::Value =
                serde_json::from_str(parsed.session.raw_metadata_json.as_ref().unwrap()).unwrap();
            metadata["gemini_migration_family"]["native_session_id"] =
                serde_json::json!(format!("conflict-{index}"));
            metadata["gemini_migration_family"]["successor_jsonl"] =
                serde_json::json!(parsed.session.source_path);
            parsed.session.raw_metadata_json = Some(metadata.to_string());
            db.upsert_session(&parsed, 0, 0).unwrap();
            assert_eq!(
                index_status_for_discovered(&db, &discovered)
                    .unwrap()
                    .unindexed_files,
                1
            );
        }
    }

    fn assert_represented(config: &Config, db: &Db, provider: Provider, discovered: usize) {
        let status = collect(config, db).unwrap();
        assert_eq!(status.index_status.unindexed_files, 0);
        let health = status
            .providers
            .iter()
            .find(|item| item.provider == provider)
            .unwrap();
        assert_eq!(health.discovered_files, discovered);
        assert_eq!(health.unindexed_files, 0);
        assert!(explain_unindexed(config, db).unwrap().is_empty());
    }

    #[test]
    fn gemini_family_counts_and_explanations_use_persisted_representation() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = isolated_config(dir.path());
        let root = dir.path().join("gemini");
        let chats = root.join("synthetic-project").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let predecessor = chats.join("session-family.json");
        let successor = chats.join("session-family.jsonl");
        std::fs::write(&predecessor, r#"{"sessionId":"family","messages":[{"id":"u1","type":"user","content":"synthetic input","timestamp":"2026-01-01T00:00:00Z"},{"id":"a1","type":"gemini","content":"predecessor-only","timestamp":"2026-01-01T00:00:01Z"}]}"#).unwrap();
        std::fs::write(&successor, concat!(
            r#"{"sessionId":"family"}"#, "\n",
            r#"{"id":"u1","type":"user","content":"synthetic input","timestamp":"2026-01-01T00:00:00Z"}"#, "\n",
            r#"{"id":"a2","type":"gemini","content":"successor-only","timestamp":"2026-01-01T00:00:02Z"}"#, "\n"
        )).unwrap();
        config.providers.gemini_cli.enabled = true;
        config.providers.gemini_cli.paths = vec![normalize_path(&root)];
        let db = Db::open(&config.db_path()).unwrap();
        crate::indexer::reindex(&config, &db, false, None).unwrap();
        assert_eq!(db.indexed_source_identities().unwrap().len(), 1);
        assert_eq!(
            db.search_messages("predecessor-only", &Default::default())
                .unwrap()
                .len(),
            1
        );
        assert_represented(&config, &db, Provider::GeminiCli, 2);
        // Representation is persisted evidence, not a reparse or a filesystem pairing guess.
        std::fs::write(&predecessor, "not valid JSON").unwrap();
        std::fs::write(&successor, "not valid JSONL").unwrap();
        assert_represented(&config, &db, Provider::GeminiCli, 2);
    }

    #[test]
    fn codex_plain_and_compressed_siblings_do_not_inflate_doctor_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = isolated_config(dir.path());
        let root = dir.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        let plain = root.join("rollout-11111111-2222-4333-8444-555555555555.jsonl");
        let bytes = concat!(
            r#"{"type":"session_meta","payload":{"id":"11111111-2222-4333-8444-555555555555","cwd":"/synthetic"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"synthetic input"}]}}"#,
            "\n"
        );
        std::fs::write(&plain, bytes).unwrap();
        std::fs::write(
            plain.with_extension("jsonl.zst"),
            zstd::encode_all(bytes.as_bytes(), 1).unwrap(),
        )
        .unwrap();
        config.providers.codex.enabled = true;
        config.providers.codex.paths = vec![normalize_path(&root)];
        let db = Db::open(&config.db_path()).unwrap();
        crate::indexer::reindex(&config, &db, false, None).unwrap();
        assert_represented(&config, &db, Provider::Codex, 1);
        // The compressed rendering becomes the canonical source once the plain one is gone.
        std::fs::rename(&plain, plain.with_extension("saved")).unwrap();
        crate::indexer::reindex(&config, &db, false, None).unwrap();
        assert_represented(&config, &db, Provider::Codex, 1);
        assert!(db.indexed_source_identities().unwrap()[0]
            .source_path
            .ends_with(".jsonl.zst"));
    }

    #[test]
    fn claude_sidecars_and_renamed_sources_do_not_inflate_doctor_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = isolated_config(dir.path());
        let root = dir.path().join("claude");
        let runs = root.join("synthetic-parent").join("subagents");
        std::fs::create_dir_all(&runs).unwrap();
        let path = runs.join("agent-synthetic.jsonl");
        std::fs::write(&path, concat!(
            r#"{"type":"user","sessionId":"synthetic-parent","isSidechain":true,"agentId":"synthetic","message":{"role":"user","content":"synthetic input"}}"#, "\n"
        )).unwrap();
        std::fs::write(
            path.with_extension("meta.json"),
            r#"{"agentType":"synthetic"}"#,
        )
        .unwrap();
        config.providers.claude.enabled = true;
        config.providers.claude.paths = vec![normalize_path(&root)];
        let db = Db::open(&config.db_path()).unwrap();
        crate::indexer::reindex(&config, &db, false, None).unwrap();
        assert_represented(&config, &db, Provider::Claude, 1);
        let renamed = runs.join("agent-renamed.jsonl");
        std::fs::rename(&path, &renamed).unwrap();
        crate::indexer::reindex(&config, &db, false, None).unwrap();
        assert_represented(&config, &db, Provider::Claude, 1);
        assert!(indexed_source_set(&db).unwrap().contains(&(
            Provider::Claude,
            normalize_path(&renamed.canonicalize().unwrap())
        )));
    }

    #[test]
    fn collision_receipts_alone_do_not_prove_source_representation() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = isolated_config(dir.path());
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        for name in ["first", "second"] {
            std::fs::write(root.join(format!("{name}.jsonl")), concat!(
                r#"{"type":"user","sessionId":"collision","message":{"role":"user","content":"synthetic input"}}"#, "\n"
            )).unwrap();
        }
        config.providers.claude.enabled = true;
        config.providers.claude.paths = vec![normalize_path(&root)];
        let db = Db::open(&config.db_path()).unwrap();
        for _ in 0..2 {
            crate::indexer::reindex(&config, &db, false, None).unwrap();
            assert_eq!(
                collect(&config, &db).unwrap().index_status.unindexed_files,
                1
            );
            assert_eq!(explain_unindexed(&config, &db).unwrap().len(), 1);
        }
    }

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
            unrecognized_record_types: Vec::new(),
        }
    }

    /// A discovered file that produced no session row is invisible to stale-session
    /// accounting: every health signal is keyed by a session that WAS indexed, so a file
    /// that never became one contributes to none of them. Count the set difference against
    /// indexed source paths; subtracting counts is wrong when the index retains pruned sources.
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
