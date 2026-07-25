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
    assert_eq!(
        count_matching(&db, &notices, "CANNOT STOP"),
        2,
        "both hook-feedback records must be stored and findable; dropping them is what made \
         'why did this agent stop?' unanswerable"
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
