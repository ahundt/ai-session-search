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
