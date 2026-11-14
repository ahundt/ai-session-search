// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::io::Cursor;
use std::path::Path;

use serde_json::{json, Value};

use super::*;
use crate::models::{ParsedSession, Provider};

fn parse(provider: Provider, path: &Path, records: &[Value]) -> ParsedSession {
    let lines = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    match provider {
        Provider::Claude | Provider::ClaudeDesktop => claude::ClaudeAdapter::new(vec![])
            .parse_reader(Cursor::new(lines), path)
            .unwrap(),
        Provider::Codex => codex::CodexAdapter::new(vec![])
            .parse_reader(Cursor::new(lines), path)
            .unwrap(),
        Provider::Cursor => cursor::CursorAdapter::new(vec![])
            .parse_reader(Cursor::new(lines), path)
            .unwrap(),
        Provider::Antigravity => antigravity::AntigravityAdapter::new(vec![])
            .parse_reader(Cursor::new(lines), path)
            .unwrap(),
        Provider::Pi => pi::PiAdapter::new(vec![])
            .parse_reader(Cursor::new(lines), path)
            .unwrap(),
        Provider::PrimeAgent => pi::PiAdapter::prime_agent(vec![])
            .parse_reader(Cursor::new(lines), path)
            .unwrap(),
        Provider::AiStudio => aistudio::AiStudioAdapter::new(vec![])
            .parse_raw(
                path,
                json!({"chunkedPrompt":{"chunks":records}}).to_string(),
            )
            .unwrap(),
        Provider::GeminiCli => gemini_cli::GeminiCliAdapter::new(vec![])
            .parse_reader(Cursor::new(lines), path)
            .unwrap(),
    }
}

const SYNTHETIC_ID: &str = "cafe0000-cafe-cafe-cafe-000000000002";

fn input_record(provider: Provider, id: &str, text: &str) -> Value {
    match provider {
        Provider::Claude | Provider::ClaudeDesktop => {
            json!({"type":"user","uuid":id,"sessionId":SYNTHETIC_ID,"message":{"role":"user","content":text}})
        }
        Provider::Codex => {
            json!({"type":"response_item","payload":{"type":"message","id":id,"role":"user","content":[{"type":"input_text","text":text}]}})
        }
        Provider::Cursor => {
            json!({"role":"user","message":{"content":[{"type":"text","text":text}]}})
        }
        Provider::Antigravity => {
            json!({"type":"USER_INPUT","source":"USER_EXPLICIT","content":text,"created_at":"2026-01-01T00:00:00Z"})
        }
        Provider::Pi | Provider::PrimeAgent => {
            json!({"type":"message","id":id,"message":{"role":"user","content":text}})
        }
        Provider::AiStudio => json!({"role":"user","text":text}),
        Provider::GeminiCli => {
            json!({"type":"user","id":id,"content":text,"timestamp":"2026-01-01T00:00:00Z"})
        }
    }
}

fn write_records(provider: Provider, path: &Path, records: &[Value]) {
    let raw = if provider == Provider::AiStudio {
        json!({"chunkedPrompt":{"chunks":records}}).to_string()
    } else {
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    };
    std::fs::write(path, raw).unwrap();
}

fn upgrade_retains_history(provider: Provider, previous: &str, expected: &str) {
    assert_eq!(crate::util::provider_parse_version(provider), expected);
    let dir = tempfile::tempdir().unwrap();
    let source_root = dir.path().join("sources");
    let relative = match provider {
        Provider::ClaudeDesktop => {
            "local-agent-mode-sessions/install/account/local_synthetic/audit.jsonl".to_string()
        }
        Provider::Cursor => format!("project/agent-transcripts/{SYNTHETIC_ID}.jsonl"),
        Provider::Antigravity => {
            format!("brain/{SYNTHETIC_ID}/.system_generated/logs/transcript.jsonl")
        }
        Provider::AiStudio => "synthetic.json".into(),
        Provider::GeminiCli => "project/chats/session-synthetic.jsonl".into(),
        _ => format!("{SYNTHETIC_ID}.jsonl"),
    };
    let path = source_root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let header = match provider {
        Provider::Codex => Some(json!({"type":"session_meta","payload":{"id":SYNTHETIC_ID}})),
        Provider::Pi | Provider::PrimeAgent => Some(json!({"type":"session","id":SYNTHETIC_ID})),
        Provider::GeminiCli => Some(json!({"sessionId":SYNTHETIC_ID,"kind":"main"})),
        _ => None,
    };
    let archived = input_record(provider, "archived", "archived synthetic input");
    let retained = input_record(provider, "retained", "retained synthetic input");
    let unknown = json!({"type":"future-kind","role":"future-kind"});
    let records = header
        .iter()
        .cloned()
        .chain([archived, retained.clone(), unknown.clone()])
        .collect::<Vec<_>>();
    write_records(provider, &path, &records);
    let path = std::fs::canonicalize(path).unwrap();
    let mut complete = parse(provider, &path, &records);
    assert_eq!(complete.session.provider, provider);
    // A recorded snapshot can outlive its tool record, including for providers whose current
    // exports no longer contain file mutations. Retention is a database contract.
    let mut edit = crate::models::FileEdit::new(0, "Write", "/synthetic/archived.txt");
    edit.new_content = Some("archived synthetic bytes".into());
    complete.file_edits.push(edit);
    let mut old = complete.clone();
    old.session.parse_version = previous.into();
    let surviving_old = old
        .messages
        .iter_mut()
        .find(|message| message.content == "retained synthetic input")
        .unwrap();
    surviving_old.provenance.authorship = crate::models::MessageAuthorship::Unknown;
    let mut metadata: Value =
        serde_json::from_str(old.session.raw_metadata_json.as_deref().unwrap_or("{}")).unwrap();
    metadata
        .as_object_mut()
        .unwrap()
        .remove(health::UNRECOGNIZED_RECORD_TYPES_KEY);
    old.session.raw_metadata_json = Some(metadata.to_string());

    let upgraded_path = dir.path().join("upgraded.db");
    let fresh_path = dir.path().join("fresh.db");
    let upgraded = crate::db::Db::open(&upgraded_path).unwrap();
    let fresh = crate::db::Db::open(&fresh_path).unwrap();
    for db in [&upgraded, &fresh] {
        db.mark_schema_current().unwrap();
    }
    upgraded.upsert_session(&old, 0, 0).unwrap();
    // Build the comparison index with the new parser over the same source history. A new
    // index of only the shortened file cannot contain source records that are already gone.
    fresh.upsert_session(&complete, 0, 0).unwrap();
    let surviving = header
        .into_iter()
        .chain([retained, unknown])
        .collect::<Vec<_>>();
    write_records(provider, &path, &surviving);
    let current = parse(provider, &path, &surviving);
    let mut config = isolated_config();
    let selected = match provider {
        Provider::Claude => &mut config.providers.claude,
        Provider::ClaudeDesktop => &mut config.providers.claude_desktop,
        Provider::Codex => &mut config.providers.codex,
        Provider::Cursor => &mut config.providers.cursor,
        Provider::Antigravity => &mut config.providers.antigravity,
        Provider::Pi => &mut config.providers.pi,
        Provider::PrimeAgent => &mut config.providers.prime_agent,
        Provider::AiStudio => &mut config.providers.aistudio,
        Provider::GeminiCli => &mut config.providers.gemini_cli,
    };
    selected.enabled = true;
    selected.paths = vec![source_root.to_string_lossy().into_owned()];
    let snapshot = |db: &crate::db::Db| {
        db.search_messages("", &Default::default())
            .unwrap()
            .into_iter()
            .map(|hit| serde_json::to_value(hit).unwrap())
            .collect::<Vec<_>>()
    };
    for (db, db_path) in [(&upgraded, &upgraded_path), (&fresh, &fresh_path)] {
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        for _ in 0..2 {
            let result = crate::service::IndexService::new(&config, db)
                .reindex_report(false)
                .unwrap();
            assert_eq!(result.files_seen, 1, "{provider:?}");
            assert_eq!(
                db.resolve_session_record(&complete.session.id)
                    .unwrap()
                    .parse_version,
                expected
            );
            assert_eq!(db.message_count().unwrap(), 2, "{provider:?}");
            assert_eq!(db.file_edit_count().unwrap(), 1, "{provider:?}");
            let connection = rusqlite::Connection::open(db_path).unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "select authorship from messages where content='retained synthetic input'",
                        [],
                        |row| row.get::<_, String>(0)
                    )
                    .unwrap(),
                "human",
                "{provider:?}"
            );
            assert_eq!(connection.query_row(
                "select new_content from file_edits where file_path='/synthetic/archived.txt'", [],
                |row| row.get::<_, String>(0)).unwrap(), "archived synthetic bytes");
            assert_eq!(
                db.search_messages("archived synthetic input", &Default::default())
                    .unwrap()
                    .len(),
                1
            );
            let stored = db
                .search_messages("retained synthetic input", &Default::default())
                .unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(
                stored[0].content, current.messages[0].content,
                "{provider:?}"
            );
            assert_eq!(stored[0].role, current.messages[0].role, "{provider:?}");
            assert_eq!(stored[0].kind, current.messages[0].kind, "{provider:?}");
            let inventory = db.parser_health().unwrap().unrecognized_record_types;
            assert_eq!(inventory.len(), 1, "{provider:?}");
            assert_eq!(inventory[0].record_type, "future-kind");
            assert_eq!(inventory[0].records, 1);
        }
    }
    assert_eq!(snapshot(&upgraded), snapshot(&fresh), "{provider:?}");
    assert_eq!(
        upgraded
            .resolve_session(&complete.session.id)
            .unwrap()
            .transcript_text,
        fresh
            .resolve_session(&complete.session.id)
            .unwrap()
            .transcript_text
    );
}

#[test]
fn claude_v20_upgrade_retains_history() {
    upgrade_retains_history(Provider::Claude, "claude-v19", "claude-v20");
}
#[test]
fn claude_desktop_v20_upgrade_retains_history() {
    upgrade_retains_history(
        Provider::ClaudeDesktop,
        "claude-desktop-local-agent-v19",
        "claude-desktop-local-agent-v20",
    );
}
#[test]
fn codex_v13_upgrade_retains_history() {
    upgrade_retains_history(Provider::Codex, "codex-v12", "codex-v13");
}
#[test]
fn cursor_v6_upgrade_retains_history() {
    upgrade_retains_history(Provider::Cursor, "cursor-v5", "cursor-v6");
}
#[test]
fn antigravity_v7_upgrade_retains_history() {
    upgrade_retains_history(Provider::Antigravity, "antigravity-v6", "antigravity-v7");
}
#[test]
fn pi_v6_upgrade_retains_history() {
    upgrade_retains_history(Provider::Pi, "pi-v5", "pi-v6");
}
#[test]
fn prime_agent_v4_upgrade_retains_history() {
    upgrade_retains_history(Provider::PrimeAgent, "prime-agent-v3", "prime-agent-v4");
}
#[test]
fn aistudio_v5_upgrade_retains_history() {
    upgrade_retains_history(Provider::AiStudio, "aistudio-v4", "aistudio-v5");
}
#[test]
fn gemini_cli_v11_upgrade_retains_history() {
    upgrade_retains_history(Provider::GeminiCli, "gemini-cli-v10", "gemini-cli-v11");
}

fn isolated_config() -> crate::config::Config {
    let mut config = crate::config::Config::default();
    for provider in [
        &mut config.providers.claude,
        &mut config.providers.claude_desktop,
        &mut config.providers.codex,
        &mut config.providers.cursor,
        &mut config.providers.antigravity,
        &mut config.providers.pi,
        &mut config.providers.prime_agent,
        &mut config.providers.aistudio,
        &mut config.providers.gemini_cli,
    ] {
        provider.enabled = false;
        provider.paths.clear();
    }
    config
}

fn reports_unknown(provider: Provider) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(if provider == Provider::ClaudeDesktop {
        "local-agent-mode-sessions/install/account/local_synthetic/audit.jsonl"
    } else {
        "synthetic.jsonl"
    });
    let record = json!({"type":"future-kind","role":"future-kind",
        "content":"synthetic fallback output"});
    let parsed = parse(provider, &path, &[record.clone(), record]);
    let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
    db.mark_schema_current().unwrap();
    db.upsert_session(&parsed, 0, 0).unwrap();
    let config = isolated_config();
    let status = crate::service::IndexService::new(&config, &db)
        .status()
        .unwrap();
    let types = status.parser_health.unrecognized_record_types;
    assert_eq!(types.len(), 1, "{provider:?}");
    assert_eq!(types[0].provider, provider);
    assert_eq!(types[0].record_type, "future-kind");
    assert_eq!(types[0].records, 2);
    if provider == Provider::Antigravity {
        assert!(
            !parsed.messages.is_empty(),
            "fallback output must remain indexed"
        );
    } else {
        assert!(parsed.messages.is_empty());
    }
}

#[test]
fn claude_reports_unknown_types() {
    reports_unknown(Provider::Claude);
}
#[test]
fn claude_desktop_reports_unknown_types() {
    reports_unknown(Provider::ClaudeDesktop);
}
#[test]
fn codex_reports_unknown_types() {
    reports_unknown(Provider::Codex);
}
#[test]
fn cursor_reports_unknown_types() {
    reports_unknown(Provider::Cursor);
}
#[test]
fn antigravity_reports_unknown_types() {
    reports_unknown(Provider::Antigravity);
}
#[test]
fn pi_reports_unknown_types() {
    reports_unknown(Provider::Pi);
}
#[test]
fn prime_agent_reports_unknown_types() {
    reports_unknown(Provider::PrimeAgent);
}
#[test]
fn aistudio_reports_unknown_types() {
    reports_unknown(Provider::AiStudio);
}
#[test]
fn gemini_cli_reports_unknown_types() {
    reports_unknown(Provider::GeminiCli);
}

#[test]
fn nested_and_unimplemented_content_types_are_reported_without_new_indexing() {
    let cases = [
        (
            Provider::Codex,
            vec![
                json!({"type":"world_state"}),
                json!({"type":"inter_agent_communication_metadata"}),
                json!({"type":"response_item","payload":{"type":"additional_tools"}}),
                json!({"type":"response_item","payload":{"type":"agent_message"}}),
                json!({"type":"event_msg","payload":{"type":"item_completed"}}),
                json!({"type":"response_item","payload":{"type":"reasoning"}}),
                json!({"type":"response_item","payload":{"type":"message","role":"developer"}}),
            ],
            vec![
                "event_msg.item_completed",
                "inter_agent_communication_metadata",
                "response_item.additional_tools",
                "response_item.agent_message",
                "response_item.message.developer",
                "response_item.reasoning",
                "world_state",
            ],
        ),
        (
            Provider::Pi,
            vec![
                json!({"type":"message","message":{"role":"future-role"}}),
                json!({"type":"context_edit"}),
            ],
            vec!["context_edit", "message.future-role"],
        ),
        (
            Provider::PrimeAgent,
            vec![
                json!({"type":"message","message":{"role":"future-role"}}),
                json!({"type":"rlm_subagent"}),
            ],
            vec!["message.future-role", "rlm_subagent"],
        ),
        (
            Provider::GeminiCli,
            vec![
                json!({"$set":{"messages":[{"id":"new","type":"future-message"}]}}),
                json!({"type":"future-event"}),
            ],
            vec!["future-event", "future-message"],
        ),
    ];
    for (provider, records, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        let parsed = parse(provider, &dir.path().join("synthetic.jsonl"), &records);
        assert!(parsed.messages.is_empty(), "{provider:?}");
        let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let health = crate::service::IndexService::new(&isolated_config(), &db)
            .status()
            .unwrap()
            .parser_health;
        assert_eq!(
            health
                .unrecognized_record_types
                .iter()
                .map(|item| item.record_type.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        let metadata: Value =
            serde_json::from_str(parsed.session.raw_metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(metadata["requires_full_parse_for_tail"], true);
    }
}

#[test]
fn explicitly_ignored_bookkeeping_is_not_reported() {
    for (provider, names) in [
        (Provider::Codex, vec!["turn_context", "token_usage_record"]),
        (
            Provider::Pi,
            vec![
                "thinking_level_change",
                "model_change",
                "usage",
                "custom",
                "label",
                "session_info",
            ],
        ),
        (
            Provider::PrimeAgent,
            vec![
                "service_tier_change",
                "child_usage_attributed",
                "session_state",
                "agent_status",
                "git_state",
            ],
        ),
        (Provider::GeminiCli, vec!["console", "network"]),
        (Provider::Antigravity, vec!["CONVERSATION_HISTORY"]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let records = names
            .iter()
            .map(|name| json!({"type":name}))
            .collect::<Vec<_>>();
        let parsed = parse(provider, &dir.path().join("synthetic.jsonl"), &records);
        let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        assert!(
            crate::service::IndexService::new(&isolated_config(), &db)
                .status()
                .unwrap()
                .parser_health
                .unrecognized_record_types
                .is_empty(),
            "{provider:?}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let records = ["token_count", "task_started", "task_complete"]
        .map(|name| json!({"type":"event_msg", "payload":{"type":name}}));
    let parsed = parse(
        Provider::Codex,
        &dir.path().join("synthetic.jsonl"),
        &records,
    );
    let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
    db.upsert_session(&parsed, 0, 0).unwrap();
    assert!(db
        .parser_health()
        .unwrap()
        .unrecognized_record_types
        .is_empty());
}

#[test]
fn gemini_migration_family_preserves_both_source_inventories() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = gemini_cli::GeminiCliAdapter::new(vec![]);
    let old = adapter
        .parse_raw(
            &dir.path().join("session-synthetic.json"),
            json!({"sessionId":"synthetic","messages":[{"type":"old-unknown"}]}).to_string(),
        )
        .unwrap();
    let new = parse(
        Provider::GeminiCli,
        &dir.path().join("session-synthetic.jsonl"),
        &[
            json!({"sessionId":"synthetic"}),
            json!({"type":"new-unknown"}),
        ],
    );
    let merged = gemini_cli::merge_migration_family(new, old).unwrap();
    let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
    db.upsert_session(&merged, 0, 0).unwrap();
    assert_eq!(
        db.parser_health()
            .unwrap()
            .unrecognized_record_types
            .iter()
            .map(|item| item.record_type.as_str())
            .collect::<Vec<_>>(),
        ["new-unknown", "old-unknown"]
    );
}

#[test]
fn gemini_legacy_json_reports_unknown_message_types() {
    let dir = tempfile::tempdir().unwrap();
    let parsed = gemini_cli::GeminiCliAdapter::new(vec![])
        .parse_raw(
            &dir.path().join("synthetic.json"),
            json!({"messages":[{"type":"future-message"}]}).to_string(),
        )
        .unwrap();
    let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
    db.upsert_session(&parsed, 0, 0).unwrap();
    let health = crate::service::IndexService::new(&isolated_config(), &db)
        .status()
        .unwrap()
        .parser_health;
    assert_eq!(
        health.unrecognized_record_types[0].record_type,
        "future-message"
    );
}
