//! Incremental byte-offset tail-parse (plan §7) integration tests.
//!
//! The load-bearing guarantee is DIFFERENTIAL: after a file is appended to, an incremental
//! reindex (which seeks to the checkpoint and parses only the new bytes) must leave the index
//! in the SAME state as a full reindex of the final file. The fast path is a pure optimization;
//! on truncation or a rewritten head it must fall back to a full parse.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::models::{MessageFilters, SearchFilters};

/// All sessions, no filtering (for `list_recent`).
fn all_sessions() -> SearchFilters {
    SearchFilters {
        provider: None,
        path_prefix: None,
        since: None,
        until: None,
        limit: 100,
        warnings_only: false,
    }
}

/// Lines 1-5 of a Claude session: user, assistant(+Bash tool_use), tool_result(Bash),
/// assistant(+Write edit), user. Each assistant turn carries text so it yields a message.
const INITIAL: &str = concat!(
    r#"{"type":"user","sessionId":"tail-sess","timestamp":"2026-06-01T10:00:00Z","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"text","text":"first user prompt about the project"}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"tail-sess","timestamp":"2026-06-01T10:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"running a bash command now"},{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"tail-sess","timestamp":"2026-06-01T10:00:06Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"a.rs b.rs"}]}}"#,
    "\n",
    r#"{"type":"assistant","sessionId":"tail-sess","timestamp":"2026-06-01T10:00:10Z","message":{"role":"assistant","content":[{"type":"text","text":"writing the file now"},{"type":"tool_use","id":"tu_2","name":"Write","input":{"file_path":"/tmp/proj/a.rs","content":"fn main() {}"}}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"tail-sess","timestamp":"2026-06-01T10:01:00Z","message":{"role":"user","content":[{"type":"text","text":"second user prompt continues the work"}]}}"#,
    "\n",
);

/// Lines 6-8 appended later: assistant(+Bash tool_use), tool_result(Bash), user.
const APPENDED: &str = concat!(
    r#"{"type":"assistant","sessionId":"tail-sess","timestamp":"2026-06-01T10:02:00Z","message":{"role":"assistant","content":[{"type":"text","text":"running another command"},{"type":"tool_use","id":"tu_3","name":"Bash","input":{"command":"cargo test"}}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"tail-sess","timestamp":"2026-06-01T10:02:01Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_3","content":"test result ok"}]}}"#,
    "\n",
    r#"{"type":"user","sessionId":"tail-sess","timestamp":"2026-06-01T10:03:00Z","message":{"role":"user","content":[{"type":"text","text":"third user prompt after the append"}]}}"#,
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

/// Every message as a comparable tuple, in (session_id, seq) order.
type Row = (i64, String, Option<String>, String, Option<String>);
fn rows(db: &Db) -> Vec<Row> {
    db.search_messages("", &MessageFilters::default())
        .unwrap()
        .into_iter()
        .map(|h| {
            (
                h.seq,
                h.role.as_str().to_string(),
                h.tool_name.clone(),
                h.content.clone(),
                h.ts.map(|t| t.to_rfc3339()),
            )
        })
        .collect()
}

fn append(path: &Path, bytes: &str) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes.as_bytes()).unwrap();
}

/// Index INITIAL, append, reindex (tail path). The result must equal a full reindex of the
/// final file — messages, file edits, FTS/trigram sync, and session metadata.
#[test]
fn tail_append_matches_full_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj")).unwrap();
    let file = projects.join("proj/tail-sess.jsonl");
    std::fs::write(&file, INITIAL).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    let initial_rows = rows(&db);
    assert_eq!(initial_rows.len(), 5, "5 messages before the append (2 user, 2 assistant, 1 tool)");

    // Append three turns, then an INCREMENTAL reindex (the tail fast path).
    append(&file, APPENDED);
    let (_total, updated) = indexer::reindex(&cfg, &db, false, None).unwrap();
    assert_eq!(updated, 1, "the grown file is re-touched exactly once");
    let tail_rows = rows(&db);
    assert_eq!(tail_rows.len(), 8, "3 new messages appended (1 assistant, 1 tool, 1 user)");
    // The unchanged prefix rows are byte-identical to before (append, not reparse-and-replace).
    assert_eq!(&tail_rows[..5], &initial_rows[..], "prefix rows unchanged by the tail append");

    // Oracle: a fresh DB fully reindexing the FINAL file.
    let dir2 = tempfile::tempdir().unwrap();
    let projects2 = dir2.path().join("projects");
    std::fs::create_dir_all(projects2.join("proj")).unwrap();
    std::fs::write(projects2.join("proj/tail-sess.jsonl"), format!("{INITIAL}{APPENDED}")).unwrap();
    let cfg2 = claude_only_config(dir2.path(), &projects2);
    let full_db = Db::open(&cfg2.db_path()).unwrap();
    indexer::reindex(&cfg2, &full_db, true, None).unwrap();

    // DIFFERENTIAL: the incrementally-appended index equals the full reindex.
    assert_eq!(tail_rows, rows(&full_db), "tail-append rows == full-reindex rows");
    assert_eq!(db.message_count().unwrap(), full_db.message_count().unwrap());
    assert_eq!(db.messages_fts_count().unwrap(), db.message_count().unwrap(), "FTS in sync");
    assert_eq!(db.file_edit_count().unwrap(), full_db.file_edit_count().unwrap());
    // The Write edit from line 4 survived (1 file edit, in both).
    assert_eq!(db.file_edit_count().unwrap(), 1);
    // tool_result messages are tagged with the originating tool across the boundary.
    let tools: Vec<_> = tail_rows.iter().filter(|r| r.1 == "tool").map(|r| r.2.clone()).collect();
    assert_eq!(tools, vec![Some("Bash".to_string()), Some("Bash".to_string())]);

    // Session metadata advanced: title/updated_at reflect the appended turns.
    let session = &db.list_recent(&all_sessions()).unwrap()[0];
    let full_session = &full_db.list_recent(&all_sessions()).unwrap()[0];
    assert_eq!(session.title, full_session.title, "title matches full reindex");
    assert_eq!(session.updated_at, full_session.updated_at, "updated_at matches full reindex");
    assert_eq!(session.message_count, Some(8));
}

/// Truncation (file shrinks below the checkpoint) must fall back to a full parse, not a tail.
#[test]
fn truncation_falls_back_to_full_parse() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj")).unwrap();
    let file = projects.join("proj/tail-sess.jsonl");
    std::fs::write(&file, format!("{INITIAL}{APPENDED}")).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(rows(&db).len(), 8);

    // Shrink the file to just the first 3 lines (a copytruncate-style rotation).
    std::fs::write(&file, INITIAL).unwrap();
    indexer::reindex(&cfg, &db, false, None).unwrap();
    // A full reparse re-derived the (now shorter) session: 5 messages, durable archive keeps none
    // of the removed tail (delete+insert replace on shrink).
    assert_eq!(rows(&db).len(), 5, "shrink → full reparse of the truncated file");
    assert_eq!(db.messages_fts_count().unwrap(), db.message_count().unwrap());
}

/// A rewritten head (different first bytes → fingerprint mismatch) must full-parse, not tail.
#[test]
fn rewritten_head_falls_back_to_full_parse() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj")).unwrap();
    let file = projects.join("proj/tail-sess.jsonl");
    std::fs::write(&file, INITIAL).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();

    // Rewrite the whole file with a DIFFERENT first line (same session id, new content), longer
    // than the original so it is not caught by the truncation guard — only the fingerprint differs.
    let new_head = r#"{"type":"user","sessionId":"tail-sess","timestamp":"2026-06-01T09:00:00Z","cwd":"/tmp/proj","message":{"role":"user","content":[{"type":"text","text":"completely different opening prompt now"}]}}"#;
    let rewritten = format!("{new_head}\n{APPENDED}{APPENDED}");
    std::fs::write(&file, &rewritten).unwrap();
    indexer::reindex(&cfg, &db, false, None).unwrap();

    // Oracle full reindex of the rewritten file.
    let dir2 = tempfile::tempdir().unwrap();
    let projects2 = dir2.path().join("projects");
    std::fs::create_dir_all(projects2.join("proj")).unwrap();
    std::fs::write(projects2.join("proj/tail-sess.jsonl"), &rewritten).unwrap();
    let cfg2 = claude_only_config(dir2.path(), &projects2);
    let full_db = Db::open(&cfg2.db_path()).unwrap();
    indexer::reindex(&cfg2, &full_db, true, None).unwrap();

    assert_eq!(rows(&db), rows(&full_db), "rewritten head → full reparse equals oracle");
}

/// Perf benchmark (opt-in: `cargo test --test tail_parse -- --ignored --nocapture`). Builds a
/// large session, then compares an INCREMENTAL reindex after a small append (the tail fast path)
/// against a FULL reindex of the same final file. The tail path reads only ~1 MiB of overlap plus
/// the appended bytes, so it should be dramatically faster than re-reading the whole file.
#[test]
#[ignore]
fn bench_incremental_vs_full_on_large_session() {
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj")).unwrap();
    let file = projects.join("proj/tail-sess.jsonl");

    // ~40 MB session: many user/assistant turns.
    let mut big = String::with_capacity(42 * 1024 * 1024);
    let turn = |i: usize| {
        format!(
            "{}\n{}\n",
            format_args!(
                r#"{{"type":"user","sessionId":"tail-sess","timestamp":"2026-06-01T10:00:00Z","cwd":"/tmp/proj","message":{{"role":"user","content":[{{"type":"text","text":"user turn {i} with a reasonably long body of text to make the file sizable for the benchmark padding padding padding"}}]}}}}"#
            ),
            format_args!(
                r#"{{"type":"assistant","sessionId":"tail-sess","timestamp":"2026-06-01T10:00:01Z","message":{{"role":"assistant","content":[{{"type":"text","text":"assistant reply {i} also with a reasonably long body of text to make the file sizable for the benchmark padding padding padding"}}]}}}}"#
            ),
        )
    };
    let mut i = 0;
    while big.len() < 40 * 1024 * 1024 {
        big.push_str(&turn(i));
        i += 1;
    }
    std::fs::write(&file, &big).unwrap();
    let mb = big.len() as f64 / (1024.0 * 1024.0);

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    let t0 = Instant::now();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    let initial_full = t0.elapsed();

    // Append two small turns, then time the INCREMENTAL reindex (tail path).
    append(&file, &turn(i));
    let t1 = Instant::now();
    indexer::reindex(&cfg, &db, false, None).unwrap();
    let incremental = t1.elapsed();

    // Time a FULL reindex of the same final file for comparison.
    let t2 = Instant::now();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    let full = t2.elapsed();

    println!(
        "tail-parse benchmark ({mb:.1} MB session):\n  initial full index: {initial_full:?}\n  \
         incremental (tail) after append: {incremental:?}\n  full reindex of final file: {full:?}\n  \
         speedup (full / incremental): {:.1}x",
        full.as_secs_f64() / incremental.as_secs_f64().max(1e-9)
    );
    assert!(incremental < full, "incremental tail reindex must beat a full reindex");
}

/// A partially written trailing line (no newline yet) adds no message; once it is completed the
/// next reindex appends it.
#[test]
fn partial_trailing_line_is_indexed_only_once_complete() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    std::fs::create_dir_all(projects.join("proj")).unwrap();
    let file = projects.join("proj/tail-sess.jsonl");
    std::fs::write(&file, INITIAL).unwrap();

    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    assert_eq!(rows(&db).len(), 5);

    // Append a partial line (no trailing newline) — mid-flush.
    let partial = r#"{"type":"user","sessionId":"tail-sess","timestamp":"2026-06-01T10:02:00Z","message":{"role":"user","content":[{"type":"text","text":"a half-written prompt"#;
    append(&file, partial);
    indexer::reindex(&cfg, &db, false, None).unwrap();
    assert_eq!(rows(&db).len(), 5, "a partial line yields no message yet");

    // Complete the line (close the JSON + newline) → the next reindex appends exactly one message.
    append(&file, "\"}]}}\n");
    indexer::reindex(&cfg, &db, false, None).unwrap();
    let r = rows(&db);
    assert_eq!(r.len(), 6, "the completed line is appended once");
    assert_eq!(r[5].3, "a half-written prompt", "the completed user message content is correct");
}
