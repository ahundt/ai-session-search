//! Incremental byte-offset tail-parse (plan §7) integration tests.
//!
//! The load-bearing guarantee is DIFFERENTIAL: after a file is appended to, an incremental
//! reindex (which seeks to the checkpoint and parses only the new bytes) must leave the index
//! in the SAME state as a full reindex of the final file. The fast path is a pure optimization;
//! on truncation or a rewritten head it must fall back to a full parse.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use std::io::Cursor;

use anyhow::Result;
use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::models::{Message, MessageFilters, ParsedSession, SearchFilters};

/// A message reduced to the fields the index search on (ts excluded: cursor uses the file mtime,
/// which legitimately differs between the full and incremental parses; per-message ts is covered
/// by the claude integration test's updated_at assertions).
fn msg_key(m: &Message) -> (String, Option<String>, String) {
    (m.role.as_str().to_string(), m.tool_name.clone(), m.content.clone())
}

/// Generic per-provider differential check of the tail mechanism: parsing `initial` and then
/// tail-parsing after `appended` is added yields the SAME messages and file-edit counts as a
/// full parse of `initial + appended`. Exercises each provider's real `parse_reader` over the
/// appended byte slice, including the bounded-overlap rebuild of any cross-line tool-id map.
fn assert_tail_matches_full<F>(initial: &str, appended: &str, file_name: &str, parse: F)
where
    F: Fn(Cursor<Vec<u8>>, &std::path::Path) -> Result<ParsedSession>,
{
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(file_name);
    let final_bytes = format!("{initial}{appended}");

    // Oracle: a full parse of the final content.
    std::fs::write(&path, &final_bytes).unwrap();
    let full = parse(Cursor::new(final_bytes.clone().into_bytes()), &path).unwrap();

    // Checkpoint after `initial`, then append and tail-parse only the new bytes.
    std::fs::write(&path, initial).unwrap();
    let offset = sessiongrep::tail::complete_prefix_offset(&path).unwrap();
    let initial_parsed = parse(Cursor::new(initial.as_bytes().to_vec()), &path).unwrap();
    std::fs::write(&path, &final_bytes).unwrap();
    let tail = sessiongrep::tail::tail_parse(&path, offset, &parse)
        .unwrap()
        .expect("new complete lines were appended");

    let full_keys: Vec<_> = full.messages.iter().map(msg_key).collect();
    let init_keys: Vec<_> = initial_parsed.messages.iter().map(msg_key).collect();
    let tail_keys: Vec<_> = tail.new_messages.iter().map(msg_key).collect();
    let m0 = init_keys.len();
    assert!(m0 > 0, "the initial parse must produce some messages");
    assert!(!tail_keys.is_empty(), "the append must produce some new messages");
    assert_eq!(&full_keys[..m0], &init_keys[..], "prefix messages match the full parse");
    assert_eq!(&full_keys[m0..], &tail_keys[..], "appended messages match the full parse suffix");
    assert_eq!(
        full.file_edits.len(),
        initial_parsed.file_edits.len() + tail.new_file_edits.len(),
        "file-edit counts add up (prefix + appended == full)"
    );
}

#[test]
fn claude_tail_matches_full_with_cross_boundary_tool_result() {
    // tool_use (tu_1) is in the INITIAL prefix; its tool_result is in the APPENDED tail. The
    // bounded overlap must rebuild the id→name map so the tail tool message is tagged "Bash".
    let adapter = sessiongrep::providers::claude::ClaudeAdapter::new(vec![]);
    let initial = concat!(
        r#"{"type":"user","sessionId":"s","timestamp":"2026-06-01T10:00:00Z","cwd":"/p","message":{"role":"user","content":[{"type":"text","text":"first prompt"}]}}"#,
        "\n",
        r#"{"type":"assistant","sessionId":"s","timestamp":"2026-06-01T10:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"running bash"},{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
        "\n",
    );
    let appended = concat!(
        r#"{"type":"user","sessionId":"s","timestamp":"2026-06-01T10:00:06Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"a.rs"}]}}"#,
        "\n",
        r#"{"type":"user","sessionId":"s","timestamp":"2026-06-01T10:01:00Z","message":{"role":"user","content":[{"type":"text","text":"second prompt"}]}}"#,
        "\n",
    );
    assert_tail_matches_full(initial, appended, "11111111-2222-3333-4444-555555555555.jsonl", |c, p| {
        adapter.parse_reader(c, p)
    });
}

#[test]
fn codex_tail_matches_full_with_cross_boundary_call_output() {
    // function_call (c1) in the prefix; its function_call_output in the tail → "shell" via overlap.
    let dir = tempfile::tempdir().unwrap();
    let adapter = sessiongrep::providers::codex::CodexAdapter::new(vec![], dir.path().join("no-home"));
    let initial = concat!(
        r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/tmp/proj"}}"#,
        "\n",
        r#"{"timestamp":"2026-06-25T07:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"do the thing"}]}}"#,
        "\n",
        r#"{"timestamp":"2026-06-25T07:00:02.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1","arguments":"{}"}}"#,
        "\n",
    );
    let appended = concat!(
        r#"{"timestamp":"2026-06-25T07:00:03.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"shell ran ok"}}"#,
        "\n",
        r#"{"timestamp":"2026-06-25T07:00:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"next thing"}]}}"#,
        "\n",
    );
    let file = "rollout-2026-06-25T03-04-06-019efd97-d602-7922-89dd-467272106505.jsonl";
    assert_tail_matches_full(initial, appended, file, |c, p| adapter.parse_reader(c, p));
}

#[test]
fn cursor_tail_matches_full_with_cross_boundary_tool_result() {
    // Cursor delegates to claude's id→name map; tool_use (tu_e) in prefix, tool_result in tail.
    let adapter = sessiongrep::providers::cursor::CursorAdapter::new(vec![]);
    let initial = concat!(
        r#"{"role":"user","message":{"content":[{"type":"text","text":"edit the file"}]}}"#,
        "\n",
        r#"{"role":"assistant","message":{"content":[{"type":"text","text":"editing"},{"type":"tool_use","id":"tu_e","name":"Edit","input":{"file_path":"/p/app.ts","old_string":"a","new_string":"b"}}]}}"#,
        "\n",
    );
    let appended = concat!(
        r#"{"role":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_e","content":"edit applied"}]}}"#,
        "\n",
        r#"{"role":"user","message":{"content":[{"type":"text","text":"another change please"}]}}"#,
        "\n",
    );
    assert_tail_matches_full(initial, appended, "22222222-3333-4444-5555-666666666666.jsonl", |c, p| {
        adapter.parse_reader(c, p)
    });
}

#[test]
fn antigravity_tail_matches_full() {
    // Tail-safe: step types self-identify, no cross-line id map.
    let adapter = sessiongrep::providers::antigravity::AntigravityAdapter::new(vec![]);
    let initial = concat!(
        r#"{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-19T23:13:00Z","content":"hello agent"}"#,
        "\n",
        r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-19T23:13:05Z","content":"hello user"}"#,
        "\n",
    );
    let appended = concat!(
        r#"{"step_index":3,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-19T23:14:00Z","content":"do more work"}"#,
        "\n",
        r#"{"step_index":4,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-19T23:14:05Z","content":"on it now"}"#,
        "\n",
    );
    assert_tail_matches_full(initial, appended, "transcript.jsonl", |c, p| adapter.parse_reader(c, p));
}

#[test]
fn pi_tail_matches_full() {
    // Tail-safe: toolResult carries its toolName inline, no cross-line id map.
    let adapter = sessiongrep::providers::pi::PiAdapter::new(vec![]);
    let initial = concat!(
        r#"{"type":"session","version":3,"id":"019edbc9-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/p"}"#,
        "\n",
        r#"{"type":"message","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"edit some files"}]}}"#,
        "\n",
        r#"{"type":"message","timestamp":"2026-06-18T17:31:36.595Z","message":{"role":"assistant","content":[{"type":"text","text":"writing it"},{"type":"toolCall","id":"t1","name":"write","arguments":{"path":"src/new.ts","content":"export const x = 1;"}}]}}"#,
        "\n",
    );
    let appended = concat!(
        r#"{"type":"message","timestamp":"2026-06-18T17:31:40.000Z","message":{"role":"toolResult","toolName":"write","content":[{"type":"text","text":"file written"}]}}"#,
        "\n",
        r#"{"type":"message","timestamp":"2026-06-18T17:31:44.000Z","message":{"role":"user","content":[{"type":"text","text":"now run the tests"}]}}"#,
        "\n",
    );
    let file = "2026-06-18T17-31-17-343Z_019edbc9-83df-72a0-a95b-64e6d810ad75.jsonl";
    assert_tail_matches_full(initial, appended, file, |c, p| adapter.parse_reader(c, p));
}

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

fn codex_only_config(root: &Path, codex_root: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.providers.claude.enabled = false;
    cfg.providers.codex.enabled = true;
    cfg.providers.codex.paths = vec![codex_root.to_string_lossy().to_string()];
    cfg.providers.cursor.enabled = false;
    cfg.providers.antigravity.enabled = false;
    cfg.providers.pi.enabled = false;
    cfg.index.db_path = Some(root.join("index.db").to_string_lossy().to_string());
    cfg
}

const CODEX_FILE: &str = "rollout-2026-06-25T03-04-06-019efd97-d602-7922-89dd-467272106505.jsonl";
const CODEX_INITIAL: &str = concat!(
    r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/tmp/proj"}}"#,
    "\n",
    r#"{"timestamp":"2026-06-25T07:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"do the thing"}]}}"#,
    "\n",
    r#"{"timestamp":"2026-06-25T07:00:02.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1","arguments":"{}"}}"#,
    "\n",
);
const CODEX_APPENDED: &str = concat!(
    r#"{"timestamp":"2026-06-25T07:00:03.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"shell ran ok"}}"#,
    "\n",
    r#"{"timestamp":"2026-06-25T07:00:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"next thing please"}]}}"#,
    "\n",
);

/// Drive the REAL indexer reindex tail path for codex (non-claude dispatch), end-to-end: a tail
/// append after the initial index must match a full reindex of the final file.
#[test]
fn codex_tail_reindex_matches_full_reindex() {
    let dir = tempfile::tempdir().unwrap();
    let codex_root = dir.path().join("codex");
    std::fs::create_dir_all(&codex_root).unwrap();
    let file = codex_root.join(CODEX_FILE);
    std::fs::write(&file, CODEX_INITIAL).unwrap();

    let cfg = codex_only_config(dir.path(), &codex_root);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    let before = rows(&db);
    // Only the user message; session_meta and function_call produce no message row.
    assert_eq!(before.len(), 1, "1 message before the append (the user turn)");

    append(&file, CODEX_APPENDED);
    let (_total, updated) = indexer::reindex(&cfg, &db, false, None).unwrap();
    assert_eq!(updated, 1, "the grown codex file is re-touched once via the tail path");
    let after = rows(&db);
    assert!(after.len() > before.len(), "the tail append added messages");
    // The cross-boundary function_call_output is tagged with the originating tool via the overlap.
    let tool = after.iter().find(|r| r.1 == "tool").expect("tool output indexed");
    assert_eq!(tool.2, Some("shell".to_string()), "tool output tagged with shell across the boundary");

    let dir2 = tempfile::tempdir().unwrap();
    let codex_root2 = dir2.path().join("codex");
    std::fs::create_dir_all(&codex_root2).unwrap();
    std::fs::write(codex_root2.join(CODEX_FILE), format!("{CODEX_INITIAL}{CODEX_APPENDED}")).unwrap();
    let cfg2 = codex_only_config(dir2.path(), &codex_root2);
    let full_db = Db::open(&cfg2.db_path()).unwrap();
    indexer::reindex(&cfg2, &full_db, true, None).unwrap();

    assert_eq!(after, rows(&full_db), "codex tail-append reindex == full reindex");
    assert_eq!(db.messages_fts_count().unwrap(), db.message_count().unwrap(), "FTS in sync");
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
