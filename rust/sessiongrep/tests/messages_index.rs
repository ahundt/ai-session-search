//! Phase 1 keystone integration tests: the `messages` table is populated during
//! reindex, is idempotent, and a malformed session never panics the reindex.

use std::path::Path;

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::models::{MessageFilters, Role};

/// A small Claude session: 2 plain user turns, 1 assistant turn, 1 slash-command turn.
const CLAUDE_FIXTURE: &str = concat!(
    r#"{"type":"user","sessionId":"test-sess-1","timestamp":"2026-06-01T10:00:00Z","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"text","text":"hello world this is a test message"}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"test-sess-1","timestamp":"2026-06-01T10:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"hi there, responding now to the test"}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"test-sess-1","timestamp":"2026-06-01T10:01:00Z","message":{"role":"user","content":[{"type":"text","text":"another substantive question here please"}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"test-sess-1","timestamp":"2026-06-01T10:02:00Z","message":{"role":"user","content":[{"type":"text","text":"/ar:plannew make a detailed plan now"}]}}"#,
    "\n",
);

const CLAUDE_UNDATED_FIXTURE: &str = concat!(
    r#"{"type":"user","sessionId":"undated-sess","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"text","text":"undated but date-filterable message"}]}}"#,
    "\n",
);

/// Build a Config that scans only a temp Claude projects dir (all other providers off)
/// and writes its index DB under the same temp dir.
fn claude_only_config(root: &Path, projects: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.providers.claude.enabled = true;
    cfg.providers.claude.paths = vec![projects.to_string_lossy().to_string()];
    cfg.providers.codex.enabled = false;
    cfg.providers.cursor.enabled = false;
    cfg.providers.antigravity.enabled = false;
    cfg.providers.pi.enabled = false;
    cfg.index.db_path = Some(root.join("index.db").to_string_lossy().to_string());
    cfg
}

#[test]
fn messages_role_counts_match_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/test-sess-1.jsonl"), CLAUDE_FIXTURE).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    let (_total, updated) = indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(updated, 1, "exactly one session should be indexed");

    // 2 plain user + 1 assistant + 1 slash-command (classified Role::Slash).
    let counts = db
        .message_role_counts(&sessiongrep::models::MessageFilters::default())
        .unwrap();
    assert_eq!(
        counts,
        vec![
            ("assistant".to_string(), 1),
            ("slash".to_string(), 1),
            ("user".to_string(), 2),
        ],
        "role counts (ordered by role) must reflect classify_role normalization"
    );
    assert_eq!(db.message_count().unwrap(), 4);
    // External-content FTS must stay in sync with the messages table via triggers.
    assert_eq!(
        db.messages_fts_count().unwrap(),
        db.message_count().unwrap()
    );
}

#[test]
fn reindex_backfills_missing_message_timestamps_from_file_time() {
    use chrono::TimeZone;

    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(
        projects.join("proj1/undated-sess.jsonl"),
        CLAUDE_UNDATED_FIXTURE,
    )
    .unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();

    let hits = db
        .search_messages(
            "date-filterable",
            &MessageFilters {
                since: Some(chrono::Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap()),
                until: Some(chrono::Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "a parsed message with no own timestamp should still be found by date filters"
    );
    assert!(
        hits[0].ts.is_some(),
        "the persisted message got a fallback ts"
    );
}

#[test]
fn full_reindex_optimizes_fts_without_breaking_search() {
    // A full reindex deletes+reinserts every message, fragmenting messages_fts into many segments
    // (measured ~2x on-disk bloat on the real corpus). The full-reindex path runs FTS5 'optimize'
    // to merge them; this must NOT corrupt the external-content index or change search results.
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/test-sess-1.jsonl"), CLAUDE_FIXTURE).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();

    // Several full reindexes churn (and now optimize) the FTS; it must stay in sync each time.
    for _ in 0..3 {
        indexer::reindex(&cfg, &db, true, None).unwrap();
        assert_eq!(
            db.messages_fts_count().unwrap(),
            db.message_count().unwrap(),
            "optimize must keep the external-content FTS in sync with messages"
        );
    }

    // Search still returns the expected content after optimize (no index corruption).
    let hits = db
        .search_messages("substantive", &MessageFilters::default())
        .unwrap();
    assert!(
        hits.iter().any(|h| h.content.contains("substantive")),
        "full-text search must still find content after optimize"
    );

    // optimize is idempotent and safe to call directly.
    db.optimize_fts().unwrap();
    assert_eq!(
        db.messages_fts_count().unwrap(),
        db.message_count().unwrap()
    );
}

#[test]
fn compact_optimize_then_vacuum_preserves_data_and_search() {
    // `compact` = Db::optimize_fts() then Db::vacuum() (the documented OPTIMIZE → VACUUM order).
    // VACUUM rewrites the file and re-creates the fts5 virtual tables + custom trigram tables; this
    // must not corrupt the index — message rows, FTS sync, and search results must all survive.
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/test-sess-1.jsonl"), CLAUDE_FIXTURE).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    let before = db.message_count().unwrap();

    db.optimize_fts().unwrap();
    db.vacuum().unwrap();

    assert_eq!(
        db.message_count().unwrap(),
        before,
        "vacuum must not lose rows"
    );
    assert_eq!(
        db.messages_fts_count().unwrap(),
        before,
        "external-content FTS stays in sync through optimize+vacuum"
    );
    let hits = db
        .search_messages("substantive", &MessageFilters::default())
        .unwrap();
    assert!(
        hits.iter().any(|h| h.content.contains("substantive")),
        "search still works after compact"
    );
}

#[test]
fn reindex_is_idempotent_for_messages() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/test-sess-1.jsonl"), CLAUDE_FIXTURE).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();

    indexer::reindex(&cfg, &db, true, None).unwrap();
    let first = db.message_count().unwrap();

    // Second incremental pass: file unchanged → nothing re-parsed, counts identical.
    let (_total, updated) = indexer::reindex(&cfg, &db, false, None).unwrap();
    assert_eq!(
        updated, 0,
        "unchanged file must be skipped on the second pass"
    );
    assert_eq!(
        db.message_count().unwrap(),
        first,
        "message rows must not grow"
    );
    assert_eq!(
        db.messages_fts_count().unwrap(),
        first,
        "FTS must not drift"
    );
}

#[test]
fn malformed_session_handled_gracefully_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/test-sess-1.jsonl"), CLAUDE_FIXTURE).unwrap();
    // Non-UTF-8 bytes decode to U+FFFD; this 5-byte file is not valid JSON even then, so it
    // contributes 0 messages — handled like any non-JSON content (no crash, no spurious warning).
    std::fs::write(
        projects.join("proj1/corrupt.jsonl"),
        [0xff, 0xfe, 0x00, 0x80, 0x9f],
    )
    .unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();

    // Must not panic even though one file is malformed.
    let (_total, updated) = indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(
        updated, 2,
        "both the good and the malformed session are upserted"
    );
    assert_eq!(
        db.message_count().unwrap(),
        4,
        "the malformed file contributes zero messages; only the good session's 4 remain"
    );
    assert_eq!(
        db.count_parse_warnings().unwrap(),
        0,
        "malformed-but-recoverable content yields 0 messages, not a parse error/warning"
    );
}

#[test]
fn search_messages_filters_by_role_substring_and_regex() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/test-sess-1.jsonl"), CLAUDE_FIXTURE).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();

    // Role filter: exactly the 2 plain user turns (the slash turn is Role::Slash).
    let users = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::User),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(users.len(), 2);
    assert!(users.iter().all(|m| m.role == Role::User));

    // Role::Slash matches the single slash-command turn.
    let slash = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::Slash),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(slash.len(), 1);
    assert!(slash[0].content.starts_with("/ar:plannew"));

    // Literal, case-insensitive substring.
    let lit = db
        .search_messages("ANOTHER substantive", &MessageFilters::default())
        .unwrap();
    assert_eq!(lit.len(), 1);

    // Regex over content (anchored).
    let re = db
        .search_messages(
            "",
            &MessageFilters {
                regex: Some(r"^/ar:plannew\b".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(re.len(), 1);

    // limit == 0 means unlimited (all 4 turns); limit 1 caps to one.
    assert_eq!(
        db.search_messages("", &MessageFilters::default())
            .unwrap()
            .len(),
        4
    );
    let capped = db
        .search_messages(
            "",
            &MessageFilters {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(capped.len(), 1);

    // Invalid regex is a clean error, not a panic.
    assert!(db
        .search_messages(
            "",
            &MessageFilters {
                regex: Some("(".to_string()),
                ..Default::default()
            },
        )
        .is_err());
}

/// One real user prompt + one tool result (Claude records tool output as role:user).
/// The tool result carries correction keywords that must NOT pollute user analytics.
const TOOL_RESULT_FIXTURE: &str = concat!(
    r#"{"type":"user","sessionId":"trsess","timestamp":"2026-06-01T10:00:00Z","cwd":"/tmp","message":{"role":"user","content":[{"type":"text","text":"please add the feature now"}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"trsess","timestamp":"2026-06-01T10:00:05Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"npm test failed: you broke the build, revert the change"}]}}"#,
    "\n",
);

/// A real user prompt + a claude compaction summary (isCompactSummary: true), which
/// carries continuation text that must not count as a user message / correction.
const COMPACTION_FIXTURE: &str = concat!(
    r#"{"type":"user","sessionId":"cmpsess","timestamp":"2026-06-01T10:00:00Z","cwd":"/tmp","message":{"role":"user","content":[{"type":"text","text":"real prompt please continue"}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"cmpsess","isCompactSummary":true,"timestamp":"2026-06-01T10:00:05Z","message":{"role":"user","content":[{"type":"text","text":"This session is being continued from a previous conversation. You forgot the tests and broke the build."}]}}"#,
    "\n",
);

#[test]
fn claude_compaction_summaries_are_compaction_role_not_user() {
    use sessiongrep::models::MessageFilters;
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("p")).unwrap();
    std::fs::write(projects.join("p/cmpsess.jsonl"), COMPACTION_FIXTURE).unwrap();
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();

    let counts = db.message_role_counts(&MessageFilters::default()).unwrap();
    assert_eq!(
        counts,
        vec![("compaction".to_string(), 1), ("user".to_string(), 1)]
    );

    // The compaction digest's 'forgot/broke' keywords must not surface as a user message.
    let users = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::User),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(users.len(), 1);
    assert!(users[0].content.contains("real prompt"));
    assert!(!users[0].content.contains("broke the build"));

    // --no-compaction excludes it from a plain (all-role) search.
    let without = db
        .search_messages(
            "",
            &MessageFilters {
                no_compaction: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(without.len(), 1);
    assert_eq!(without[0].role, Role::User);
}

#[test]
fn claude_tool_results_are_tool_role_not_user() {
    use sessiongrep::models::MessageFilters;
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("p")).unwrap();
    std::fs::write(projects.join("p/trsess.jsonl"), TOOL_RESULT_FIXTURE).unwrap();
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();

    // One real user prompt, one tool message — the tool_result is NOT counted as user.
    let counts = db.message_role_counts(&MessageFilters::default()).unwrap();
    assert_eq!(
        counts,
        vec![("tool".to_string(), 1), ("user".to_string(), 1)]
    );

    // The correction-keyword-laden tool output is searchable as `tool`, excluded from `user`.
    let users = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::User),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(users.len(), 1);
    assert!(users[0].content.contains("add the feature"));
    assert!(
        !users[0].content.contains("broke"),
        "tool output must not be a user message"
    );

    let tools = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::Tool),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(tools.len(), 1);
    assert!(tools[0].content.contains("you broke the build"));
}

#[test]
fn message_context_returns_seq_window() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/test-sess-1.jsonl"), CLAUDE_FIXTURE).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();

    // Window of ±1 around the assistant turn (seq 1) → seq 0,1,2.
    let window = db.message_context("claude:test-sess-1", 1, 1, 1).unwrap();
    assert_eq!(
        window.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    // Window clamps at the start: ±2 around seq 0 → seq 0,1,2 (no negative seqs).
    let head = db.message_context("claude:test-sess-1", 0, 2, 2).unwrap();
    assert_eq!(
        head.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}
