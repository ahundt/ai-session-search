//! Phase 1 keystone integration tests: the `messages` table is populated during
//! reindex, is idempotent, and a malformed session never panics the reindex.

use std::path::Path;

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::indexer;

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
    let counts = db.message_role_counts().unwrap();
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
    assert_eq!(db.messages_fts_count().unwrap(), db.message_count().unwrap());
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
    assert_eq!(updated, 0, "unchanged file must be skipped on the second pass");
    assert_eq!(db.message_count().unwrap(), first, "message rows must not grow");
    assert_eq!(db.messages_fts_count().unwrap(), first, "FTS must not drift");
}

#[test]
fn malformed_session_warns_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/test-sess-1.jsonl"), CLAUDE_FIXTURE).unwrap();
    // Non-UTF-8 bytes: read_to_string fails → adapter returns minimal_record (parse_warning, 0 messages).
    std::fs::write(projects.join("proj1/corrupt.jsonl"), [0xff, 0xfe, 0x00, 0x80, 0x9f]).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();

    // Must not panic even though one file is unreadable.
    let (_total, updated) = indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(updated, 2, "both the good and the corrupt session are upserted");
    assert_eq!(
        db.message_count().unwrap(),
        4,
        "the corrupt session contributes zero messages; only the good session's 4 remain"
    );
    assert!(
        db.count_parse_warnings().unwrap() >= 1,
        "the corrupt session must carry a parse_warning"
    );
}
