// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Hook feedback is stored and findable, and excluded from ordinary results.
//!
//! Claude Code injects records the user did not write: Stop-hook feedback, PreToolUse blocks,
//! local-command caveats, task notifications. They were dropped at index time, which also
//! removed the only evidence of what a hook told an agent and what it did next. Measured on
//! real data: 82 of 82 records containing "CANNOT STOP" in one session carried `isMeta`, and
//! search returned none of them while reporting a healthy index.
//!
//! These tests pin both halves of the fix: the records exist after indexing, and they stay out
//! of results unless asked for, so user prose and the analytics over it are unchanged.

use std::fs;

use ai_session_search::db::Db;
use ai_session_search::models::{MessageFilters, MessageKind};
use ai_session_search::providers::claude::ClaudeAdapter;
use tempfile::tempdir;

const HOOK_FEEDBACK: &str = "Stop hook feedback: CANNOT STOP - incomplete tasks: 1. #24";
const USER_PROSE: &str = "please finish the refactor and run the tests";

/// Index one claude transcript containing both prose and harness-injected records.
fn indexed_fixture() -> (tempfile::TempDir, Db) {
    let dir = tempdir().unwrap();
    let project = dir.path().join("-tmp-proj");
    fs::create_dir_all(&project).unwrap();
    let session = "2e55571b-9383-4fbc-b84f-19bbd779ed26";
    fs::write(
        project.join(format!("{session}.jsonl")),
        format!(
            r#"{{"type":"user","sessionId":"{session}","cwd":"/tmp/proj","message":{{"role":"user","content":"{USER_PROSE}"}}}}
{{"type":"user","sessionId":"{session}","isMeta":true,"message":{{"role":"user","content":"{HOOK_FEEDBACK}"}}}}
{{"type":"user","sessionId":"{session}","isMeta":true,"message":{{"role":"user","content":"{HOOK_FEEDBACK} again"}}}}
{{"type":"user","sessionId":"{session}","message":{{"role":"user","content":"<task-notification>agent done</task-notification>"}}}}
"#
        ),
    )
    .unwrap();

    let db = Db::open(&dir.path().join("index.db")).unwrap();
    let adapter = ClaudeAdapter::new(vec![dir.path().to_path_buf()]);
    for source in adapter.discover() {
        let parsed = adapter.parse(&source);
        db.upsert_session(&parsed, source.mtime_ns, source.size_bytes)
            .unwrap();
    }
    (dir, db)
}

fn count_matching(db: &Db, filters: &MessageFilters, needle: &str) -> usize {
    db.search_messages(needle, filters).unwrap().len()
}

#[test]
fn hook_feedback_is_indexed_rather_than_discarded() {
    let (_dir, db) = indexed_fixture();
    let notices = MessageFilters {
        kinds: Some(vec![MessageKind::HarnessNotice]),
        limit: 0,
        ..Default::default()
    };
    let hits = db.search_messages("CANNOT STOP", &notices).unwrap();
    assert_eq!(
        hits.len(),
        2,
        "both hook-feedback records must be stored and findable; dropping them is what made \
         'why did this agent stop?' unanswerable"
    );
    assert!(
        hits.iter()
            .all(|hit| hit.kind == MessageKind::HarnessNotice),
        "database rows stored as harness_notice must remain harness notices when decoded"
    );
}

#[test]
fn ordinary_search_excludes_harness_notices_but_keeps_prose() {
    let (_dir, db) = indexed_fixture();
    let default = MessageFilters {
        limit: 0,
        ..Default::default()
    };
    assert_eq!(
        count_matching(&db, &default, "CANNOT STOP"),
        0,
        "harness notices must stay out of ordinary results so user-prose analytics are unchanged"
    );
    assert_eq!(
        count_matching(&db, &default, "finish the refactor"),
        1,
        "user prose is unaffected by storing notices alongside it"
    );
}

#[test]
fn a_task_notification_without_the_meta_flag_is_still_a_harness_notice() {
    // These arrive as role:user with userType "external" and no isMeta, so they are matched on
    // their leading tag. Missing this case is how they previously reached user analytics.
    let (_dir, db) = indexed_fixture();
    let notices = MessageFilters {
        kinds: Some(vec![MessageKind::HarnessNotice]),
        limit: 0,
        ..Default::default()
    };
    assert_eq!(count_matching(&db, &notices, "agent done"), 1);

    let default = MessageFilters {
        limit: 0,
        ..Default::default()
    };
    assert_eq!(count_matching(&db, &default, "agent done"), 0);
}

/// Storing harness notices must not change user-prose analytics. They carry `role: "user"`
/// because that is how the harness records them, so anything selecting on role alone would
/// newly pick them up. Corrections runs through the shared `append_message_filters`, whose
/// default excludes the class, and this pins that: a hook-feedback record containing a phrase
/// the correction patterns match must not be reported as a user correction.
#[test]
fn harness_notices_stay_out_of_correction_analytics() {
    let dir = tempdir().unwrap();
    let project = dir.path().join("-tmp-proj");
    fs::create_dir_all(&project).unwrap();
    let session = "0b3fdcac-1453-4891-9c43-9f2e1a2fb8c3";
    // "you forgot" is correction-shaped. One instance is the user; one is hook feedback.
    fs::write(
        project.join(format!("{session}.jsonl")),
        format!(
            r#"{{"type":"user","sessionId":"{session}","cwd":"/tmp/proj","message":{{"role":"user","content":"you forgot to run the tests"}}}}
{{"type":"user","sessionId":"{session}","isMeta":true,"message":{{"role":"user","content":"Stop hook feedback: you forgot to complete the tasks"}}}}
"#
        ),
    )
    .unwrap();
    let db = Db::open(&dir.path().join("index.db")).unwrap();
    let adapter = ClaudeAdapter::new(vec![dir.path().to_path_buf()]);
    for source in adapter.discover() {
        let parsed = adapter.parse(&source);
        db.upsert_session(&parsed, source.mtime_ns, source.size_bytes)
            .unwrap();
    }

    // Through the public service, so this exercises the same path the CLI and MCP use.
    let config = ai_session_search::config::Config::default();
    let run = ai_session_search::service::AnalysisService::new(&config, &db)
        .run_skill(&ai_session_search::SkillRunQuery {
            skill: ai_session_search::SkillSelector::name("corrections").unwrap(),
            definition: None,
            input: ai_session_search::SkillCapabilityInput::MessageClassification(
                ai_session_search::MessageClassificationQuery::default(),
            ),
        })
        .unwrap();
    let ai_session_search::SkillCapabilityOutput::MessageClassification(classification) =
        run.output;
    let found = classification.report.matches;
    assert!(
        found
            .iter()
            .all(|c| !c.content.contains("Stop hook feedback")),
        "hook feedback must never be counted as a user correction: {found:#?}"
    );
}

/// `explain_unindexed` names the file, the id its content resolves to, and which indexed file
/// already holds that id. This is the reconciliation that previously required joining index
/// state against a directory listing by hand, which no SQL-only interface can express because
/// half the question is the filesystem. The reason is recomputed, so no table stores it.
#[test]
fn unindexed_files_are_explained_by_naming_the_file_that_took_their_id() {
    let dir = tempdir().unwrap();
    let project = dir.path().join("-tmp-proj");
    fs::create_dir_all(&project).unwrap();
    // Two transcripts whose content claims the SAME session id: the second cannot be stored
    // under it, because the first already is.
    let shared = "3f260ba5-b3b0-4e79-9488-b9832ded0835";
    for name in [shared, "aaaaaaaa-1111-2222-3333-444444444444"] {
        fs::write(
            project.join(format!("{name}.jsonl")),
            format!(
                r#"{{"type":"user","sessionId":"{shared}","cwd":"/tmp/proj","message":{{"role":"user","content":"turn in {name}"}}}}
"#
            ),
        )
        .unwrap();
    }

    // Canonicalize once and use the same root for both sides. Config roots are canonicalized
    // by `provider_roots`, so on macOS a bare tempdir (/var/...) and its canonical form
    // (/private/var/...) would otherwise look like different files to the reconciliation.
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut config = ai_session_search::config::Config::default();
    config.providers.claude.paths = vec![root.to_string_lossy().into_owned()];
    for provider in [
        &mut config.providers.claude_desktop,
        &mut config.providers.codex,
        &mut config.providers.cursor,
        &mut config.providers.antigravity,
        &mut config.providers.pi,
        &mut config.providers.aistudio,
        &mut config.providers.gemini_cli,
    ] {
        provider.enabled = false;
    }
    let db = Db::open(&dir.path().join("index.db")).unwrap();
    let adapter = ClaudeAdapter::new(vec![root.clone()]);
    for source in adapter.discover() {
        let parsed = adapter.parse(&source);
        db.upsert_session(&parsed, source.mtime_ns, source.size_bytes)
            .unwrap();
    }

    let explained = ai_session_search::diagnostics::explain_unindexed(&config, &db).unwrap();
    assert_eq!(
        explained.len(),
        1,
        "exactly one of the two colliding files ends up unindexed: {explained:#?}"
    );
    let only = &explained[0];
    assert_eq!(only.resolves_to, shared);
    assert!(
        only.id_already_held_by.is_some(),
        "the explanation must name the file holding that id, which is the whole point of \
         recomputing the reason rather than reporting a bare count"
    );
}

// Rows whose stored `kind` is outside the current enum must survive the default filter, since
// any row written by another build looks like that. An inclusion-list default silently dropped
// them, which is the same silent omission this feature exists to remove.
//
// That case is already covered end-to-end, and is what caught the bug:
// `process_lifecycle.rs:cli_search_self_heals_v4_hybrid_missing_trigram_from_intact_messages`
// inserts `kind = 'message'` and asserts search returns it. The shape of the decision is
// pinned directly by `models::tests::default_class_selection_excludes_rather_than_enumerates`.
// Neither is duplicated here, and no test-only database accessor is added to the public API
// to repeat them.
