//! Phase 5 file-recovery integration tests: `Write`/`Edit`/`MultiEdit` tool calls are
//! extracted during reindex, persisted to `file_edits`, queryable (search/history/
//! cross-ref), idempotent, and replayable into reconstructed historical content.

use std::path::Path;

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::files::reconstruct;
use sessiongrep::indexer;
use sessiongrep::models::{FileEdit, FileQuery};

/// Session 1 edits `app.py` three times (Write → Edit → MultiEdit) and Writes `util.py`.
/// app.py versions:  v1 "line1\nline2\nline3"  v2 "line1\nLINE2\nline3"  v3 "L1\nLINE2\nL3".
const SESS1: &str = concat!(
    r#"{"type":"user","sessionId":"sess-fr-1","timestamp":"2026-06-02T10:00:00Z","cwd":"/repo","message":{"role":"user","content":[{"type":"text","text":"please create the app file now"}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"sess-fr-1","timestamp":"2026-06-02T10:00:10Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"w1","name":"Write","input":{"file_path":"/repo/app.py","content":"line1\nline2\nline3"}}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"sess-fr-1","timestamp":"2026-06-02T10:01:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"e1","name":"Edit","input":{"file_path":"/repo/app.py","old_string":"line2","new_string":"LINE2"}}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"sess-fr-1","timestamp":"2026-06-02T10:02:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"m1","name":"MultiEdit","input":{"file_path":"/repo/app.py","edits":[{"old_string":"line1","new_string":"L1"},{"old_string":"line3","new_string":"L3"}]}}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"sess-fr-1","timestamp":"2026-06-02T10:03:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"w2","name":"Write","input":{"file_path":"/repo/util.py","content":"a\nb"}}]}}"#,
    "\n",
);

/// Session 2 also Writes `app.py` (a different, later session touching the same path).
const SESS2: &str = concat!(
    r#"{"type":"user","sessionId":"sess-fr-2","timestamp":"2026-06-03T09:00:00Z","cwd":"/repo","message":{"role":"user","content":[{"type":"text","text":"rewrite the app file from scratch"}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"sess-fr-2","timestamp":"2026-06-03T09:00:10Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"w3","name":"Write","input":{"file_path":"/repo/app.py","content":"brand new"}}]}}"#,
    "\n",
);

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

fn indexed() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/sess-fr-1.jsonl"), SESS1).unwrap();
    std::fs::write(projects.join("proj1/sess-fr-2.jsonl"), SESS2).unwrap();
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    (dir, db)
}

fn edits_only(rows: Vec<(String, sessiongrep::models::Provider, FileEdit)>) -> Vec<FileEdit> {
    rows.into_iter().map(|(_, _, edit)| edit).collect()
}

#[test]
fn tool_calls_are_extracted_and_counted() {
    let (_dir, db) = indexed();
    // 4 edits in session 1 (Write app, Edit app, MultiEdit app, Write util) + 1 in session 2.
    assert_eq!(db.file_edit_count().unwrap(), 5);
}

#[test]
fn file_search_aggregates_edits_and_sessions() {
    let (_dir, db) = indexed();
    let summaries = db.file_search(&FileQuery::default()).unwrap();
    // app.py first (4 edits, ordered by edits desc), then util.py (1 edit).
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].file_name, "app.py");
    assert_eq!(summaries[0].edits, 4);
    assert_eq!(summaries[0].sessions, 2, "two sessions edited app.py");
    assert_eq!(summaries[1].file_name, "util.py");
    assert_eq!(summaries[1].edits, 1);
    assert_eq!(summaries[1].sessions, 1);
}

#[test]
fn file_search_pattern_and_min_edits_filters() {
    let (_dir, db) = indexed();
    // Glob over the basename.
    let py = db
        .file_search(&FileQuery {
            pattern: Some("*.py".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(py.len(), 2);
    let app = db
        .file_search(&FileQuery {
            pattern: Some("app.py".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(app.len(), 1);
    assert_eq!(app[0].file_name, "app.py");
    // min_edits prunes the single-edit util.py.
    let busy = db
        .file_search(&FileQuery {
            min_edits: Some(2),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(busy.len(), 1);
    assert_eq!(busy[0].file_name, "app.py");
}

#[test]
fn version_ordering_is_chronological_within_session() {
    let (_dir, db) = indexed();
    let rows = db.file_edits_for("app.py", Some("sess-fr-1")).unwrap();
    let edits = edits_only(rows);
    assert_eq!(edits.len(), 3, "session 1 has three app.py versions");
    // Ordered by seq: Write, Edit, MultiEdit.
    assert_eq!(edits[0].tool, "Write");
    assert_eq!(edits[1].tool, "Edit");
    assert_eq!(edits[2].tool, "MultiEdit");
    assert!(edits[0].seq < edits[1].seq && edits[1].seq < edits[2].seq);
}

#[test]
fn reconstruct_each_version_from_db() {
    let (_dir, db) = indexed();
    let edits = edits_only(db.file_edits_for("app.py", Some("sess-fr-1")).unwrap());
    assert_eq!(reconstruct(&edits, 1).as_deref(), Some("line1\nline2\nline3"));
    assert_eq!(reconstruct(&edits, 2).as_deref(), Some("line1\nLINE2\nline3"));
    assert_eq!(reconstruct(&edits, 3).as_deref(), Some("L1\nLINE2\nL3"));
}

#[test]
fn cross_ref_links_file_to_each_session() {
    let (_dir, db) = indexed();
    let xref = db
        .file_cross_ref(&FileQuery {
            pattern: Some("app.py".into()),
            ..Default::default()
        })
        .unwrap();
    // One row per (file, session): sess-fr-1 (3 edits) and sess-fr-2 (1 edit).
    assert_eq!(xref.len(), 2);
    let by_session: std::collections::HashMap<_, _> = xref
        .iter()
        .map(|r| (r.session_id.clone(), r.edits))
        .collect();
    assert_eq!(by_session.get("claude:sess-fr-1"), Some(&3));
    assert_eq!(by_session.get("claude:sess-fr-2"), Some(&1));
}

#[test]
fn file_edits_are_idempotent_across_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj1")).unwrap();
    std::fs::write(projects.join("proj1/sess-fr-1.jsonl"), SESS1).unwrap();
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();

    indexer::reindex(&cfg, &db, true, None).unwrap();
    let first = db.file_edit_count().unwrap();
    assert_eq!(first, 4);
    // Unchanged file on the second pass → no growth, no drift.
    let (_total, updated) = indexer::reindex(&cfg, &db, false, None).unwrap();
    assert_eq!(updated, 0);
    assert_eq!(db.file_edit_count().unwrap(), first);
}

#[test]
fn cross_session_file_has_separate_version_chains() {
    let (_dir, db) = indexed();
    // Without a session scope, app.py spans two sessions; session 2's Write is its own v1.
    let s2 = edits_only(db.file_edits_for("app.py", Some("sess-fr-2")).unwrap());
    assert_eq!(s2.len(), 1);
    assert_eq!(reconstruct(&s2, 1).as_deref(), Some("brand new"));
}
