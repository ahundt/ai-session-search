//! Hardening / robustness tests (plan H1/H2/H8/H12 pre-mortem): adversarial and
//! large inputs must never panic the indexer, must keep the message FTS in sync,
//! and must round-trip messages + file edits through reindex without drift.

use std::path::Path;

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::models::FileQuery;

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

fn write_session(projects: &Path, name: &str, contents: &[u8]) {
    let dir = projects.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), contents).unwrap();
}

fn index(dir: &tempfile::TempDir) -> (Config, Db) {
    let projects = dir.path().join("projects");
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    (cfg, db)
}

#[test]
fn adversarial_jsonl_lines_never_panic() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");

    // A grab-bag of malformed / hostile lines that must not panic the parser.
    let mut adversarial = String::new();
    adversarial.push_str("not json at all\n");
    adversarial.push_str("{ broken json\n");
    adversarial.push_str("{}\n"); // empty object
    adversarial.push_str("[]\n"); // array, not object
    adversarial.push_str("12345\n"); // bare number
    adversarial.push_str("\"a bare string\"\n");
    adversarial.push_str("null\n");
    adversarial.push_str(r#"{"type":"user","message":{"role":"user"}}"#); // no content
    adversarial.push('\n');
    adversarial.push_str(
        r#"{"type":"assistant","message":{"role":"assistant","content":"not an array"}}"#,
    );
    adversarial.push('\n');
    // Deeply nested content (recursion in extract_text must terminate).
    adversarial.push_str(r#"{"type":"user","message":{"role":"user","content":[{"content":[{"content":[{"text":"deep"}]}]}]}}"#);
    adversarial.push('\n');
    // A very long single message (1 MB of 'x').
    let huge = "x".repeat(1_000_000);
    adversarial.push_str(&format!(
        r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"text","text":"{huge}"}}]}}}}"#
    ));
    adversarial.push('\n');
    write_session(&projects, "adversarial.jsonl", adversarial.as_bytes());

    // Pure binary garbage (read_to_string fails → minimal_record, no panic).
    write_session(
        &projects,
        "binary.jsonl",
        &[0xff, 0x00, 0xfe, 0x80, 0x9f, 0x01],
    );

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    // The whole point: this returns Ok and does not panic.
    let (_total, updated) = indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(updated, 2, "both files upsert (one good-ish, one minimal)");
    // FTS invariant holds even with junk input.
    assert_eq!(
        db.message_count().unwrap(),
        db.messages_fts_count().unwrap()
    );
}

#[test]
fn malformed_tool_use_is_skipped_not_extracted() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let mut s = String::new();
    // tool_use missing file_path → skipped.
    s.push_str(r#"{"type":"assistant","sessionId":"h1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"content":"x"}}]}}"#);
    s.push('\n');
    // tool_use with non-string content → content defaults empty, but still a valid path.
    s.push_str(r#"{"type":"assistant","sessionId":"h1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/r/a.txt","content":12345}}]}}"#);
    s.push('\n');
    // Edit missing old/new strings → empty deltas, still recorded with a path.
    s.push_str(r#"{"type":"assistant","sessionId":"h1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/r/a.txt"}}]}}"#);
    s.push('\n');
    // Unknown tool → ignored entirely.
    s.push_str(r#"{"type":"assistant","sessionId":"h1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#);
    s.push('\n');
    write_session(&projects, "h1.jsonl", s.as_bytes());

    let (_cfg, db) = index(&dir);
    // Only the two calls carrying a file_path are recorded; the missing-path Write and
    // the Bash call are not. No panic on the non-string content.
    assert_eq!(db.file_edit_count().unwrap(), 2);
    let summaries = db.file_search(&FileQuery::default()).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].file_name, "a.txt");
    assert_eq!(summaries[0].edits, 2);
}

#[test]
fn unicode_paths_and_content_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    // Unicode in both the path and the file content (emoji + CJK + combining marks).
    let line = r#"{"type":"assistant","sessionId":"u1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/r/café_文件_🚀.txt","content":"héllo 世界 🚀\nsecond line"}}]}}"#;
    write_session(&projects, "u1.jsonl", format!("{line}\n").as_bytes());

    let (_cfg, db) = index(&dir);
    let rows = db.file_edits_for("café_文件_🚀.txt", None).unwrap();
    assert_eq!(rows.len(), 1);
    let content = sessiongrep::files::reconstruct(
        &rows.into_iter().map(|(_, _, e)| e).collect::<Vec<_>>(),
        1,
    );
    assert_eq!(content.as_deref(), Some("héllo 世界 🚀\nsecond line"));
}

#[test]
fn reconstruct_handles_multibyte_edit_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let mut s = String::new();
    s.push_str(r#"{"type":"assistant","sessionId":"m1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/r/u.txt","content":"αβγ 世界 end"}}]}}"#);
    s.push('\n');
    // Replace a multibyte substring with another multibyte substring.
    s.push_str(r#"{"type":"assistant","sessionId":"m1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/r/u.txt","old_string":"世界","new_string":"🌍"}}]}}"#);
    s.push('\n');
    write_session(&projects, "m1.jsonl", s.as_bytes());

    let (_cfg, db) = index(&dir);
    let edits: Vec<_> = db
        .file_edits_for("u.txt", None)
        .unwrap()
        .into_iter()
        .map(|(_, _, e)| e)
        .collect();
    assert_eq!(
        sessiongrep::files::reconstruct(&edits, 2).as_deref(),
        Some("αβγ 🌍 end")
    );
}

#[test]
fn duplicate_session_ids_do_not_double_count() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    // Two distinct files that declare the SAME sessionId → same logical session id;
    // the second upsert replaces the first (no duplicate rows).
    let body = r#"{"type":"user","sessionId":"dup","message":{"role":"user","content":[{"type":"text","text":"only one of me should remain"}]}}"#;
    write_session(&projects, "first.jsonl", format!("{body}\n").as_bytes());
    write_session(&projects, "second.jsonl", format!("{body}\n").as_bytes());

    let (_cfg, db) = index(&dir);
    // Exactly one session id → its single user message survives, FTS stays consistent.
    assert_eq!(db.message_count().unwrap(), 1);
    assert_eq!(db.messages_fts_count().unwrap(), 1);
}

#[test]
fn large_session_indexes_with_correct_counts() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let mut s = String::new();
    for i in 0..2_000 {
        s.push_str(&format!(
            r#"{{"type":"user","sessionId":"big","message":{{"role":"user","content":[{{"type":"text","text":"message number {i} with some words"}}]}}}}"#
        ));
        s.push('\n');
    }
    write_session(&projects, "big.jsonl", s.as_bytes());

    let (_cfg, db) = index(&dir);
    assert_eq!(db.message_count().unwrap(), 2_000);
    assert_eq!(db.messages_fts_count().unwrap(), 2_000);
}

#[test]
fn reindex_after_edit_keeps_fts_in_sync_and_drops_stale_content() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    let v1 = r#"{"type":"user","sessionId":"r1","message":{"role":"user","content":[{"type":"text","text":"alpha unique-token-aaa"}]}}"#;
    write_session(&projects, "r1.jsonl", format!("{v1}\n").as_bytes());
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(
        db.search_messages(
            "unique-token-aaa",
            &sessiongrep::models::MessageFilters::default()
        )
        .unwrap()
        .len(),
        1
    );

    // Rewrite the same session with different content, then full reindex.
    let v2 = r#"{"type":"user","sessionId":"r1","message":{"role":"user","content":[{"type":"text","text":"beta unique-token-bbb"}]}}"#;
    std::fs::write(projects.join("proj/r1.jsonl"), format!("{v2}\n")).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();

    // FTS invariant; old content gone, new content present (triggers kept FTS in sync).
    assert_eq!(
        db.message_count().unwrap(),
        db.messages_fts_count().unwrap()
    );
    assert_eq!(
        db.search_messages(
            "unique-token-aaa",
            &sessiongrep::models::MessageFilters::default()
        )
        .unwrap()
        .len(),
        0,
        "stale content must not remain searchable"
    );
    assert_eq!(
        db.search_messages(
            "unique-token-bbb",
            &sessiongrep::models::MessageFilters::default()
        )
        .unwrap()
        .len(),
        1
    );
}

// --- Atomicity / bulk-clear / cross-connection visibility (plan H2, H10) ---

const TWO_MSG_SESSION: &str = concat!(
    r#"{"type":"user","sessionId":"a1","message":{"role":"user","content":[{"type":"text","text":"first message here"}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"a1","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/r/x.txt","content":"hi"}}]}}"#,
    "\n",
);

#[test]
fn clear_all_leaves_no_orphan_fts_or_edit_rows() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    write_session(&projects, "a1.jsonl", TWO_MSG_SESSION.as_bytes());
    let (_cfg, db) = index(&dir);
    assert!(db.message_count().unwrap() >= 1);
    assert!(db.file_edit_count().unwrap() >= 1);

    db.clear_all().unwrap();
    // Bulk delete must fire the messages_ad trigger for every row (external-content
    // FTS5 stays consistent — no orphaned terms left behind).
    assert_eq!(db.message_count().unwrap(), 0);
    assert_eq!(db.messages_fts_count().unwrap(), 0);
    assert_eq!(db.file_edit_count().unwrap(), 0);
}

#[test]
fn removed_source_file_is_retained_in_index() {
    // The DB is a durable archive: when a harness clears/removes a session file, the
    // already-indexed history must survive — even a full reindex must not drop it.
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    write_session(&projects, "keep.jsonl", TWO_MSG_SESSION.as_bytes());
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    let before = db.message_count().unwrap();
    assert!(before >= 1);

    // Source file removed by the harness; only the DB retains the history now.
    std::fs::remove_file(projects.join("proj").join("keep.jsonl")).unwrap();

    // A full reindex (the schema-migration path) must NOT wipe the orphaned session.
    indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(
        db.message_count().unwrap(),
        before,
        "orphaned session retained after full reindex"
    );
    assert!(
        db.file_edit_count().unwrap() >= 1,
        "file-edit history retained too"
    );
}

#[test]
fn full_reindex_is_stable_across_repeats() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    write_session(&projects, "a1.jsonl", TWO_MSG_SESSION.as_bytes());
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();

    indexer::reindex(&cfg, &db, true, None).unwrap();
    let (msgs, edits) = (db.message_count().unwrap(), db.file_edit_count().unwrap());
    // A second FULL reindex re-parses the same file and upserts in place: identical
    // counts, FTS in sync (idempotent, no clear — retention preserved).
    indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(
        db.message_count().unwrap(),
        msgs,
        "full reindex must not double rows"
    );
    assert_eq!(db.file_edit_count().unwrap(), edits);
    assert_eq!(
        db.messages_fts_count().unwrap(),
        db.message_count().unwrap()
    );
}

#[test]
fn committed_data_is_visible_to_a_second_connection() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    write_session(&projects, "a1.jsonl", TWO_MSG_SESSION.as_bytes());
    let cfg = claude_only_config(dir.path(), &projects);

    let writer = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &writer, true, None).unwrap();
    let expected = writer.message_count().unwrap();

    // A separate connection (as the MCP server / a concurrent CLI would open) sees the
    // committed rows — the upsert transaction is durable, not stuck in an open tx.
    let reader = Db::open(&cfg.db_path()).unwrap();
    assert_eq!(reader.message_count().unwrap(), expected);
    assert_eq!(
        reader.file_edit_count().unwrap(),
        writer.file_edit_count().unwrap()
    );
}
