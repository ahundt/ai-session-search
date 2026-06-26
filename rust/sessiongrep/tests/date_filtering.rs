//! Wiring + composition tests (#101/#104): date bounds and the other filters
//! (role/session/regex/limit) flow correctly through every db query method that
//! claims to honor them, and they compose without cancelling each other.

use chrono::{DateTime, Utc};
use std::path::Path;

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::models::{FileQuery, MessageFilters, Role};

/// One session with user turns + file Writes on three distinct days (Jun 1/10/20).
const FIXTURE: &str = concat!(
    r#"{"type":"user","sessionId":"datesess","timestamp":"2026-06-01T10:00:00Z","cwd":"/r","message":{"role":"user","content":[{"type":"text","text":"alpha early apple"}]}}"#, "\n",
    r#"{"type":"assistant","sessionId":"datesess","timestamp":"2026-06-01T10:00:05Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/r/early.txt","content":"e1"}}]}}"#, "\n",
    r#"{"type":"user","sessionId":"datesess","timestamp":"2026-06-10T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"bravo middle banana"}]}}"#, "\n",
    r#"{"type":"assistant","sessionId":"datesess","timestamp":"2026-06-10T10:00:05Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/r/mid.txt","content":"m1"}}]}}"#, "\n",
    r#"{"type":"user","sessionId":"datesess","timestamp":"2026-06-20T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"charlie late cherry"}]}}"#, "\n",
    r#"{"type":"assistant","sessionId":"datesess","timestamp":"2026-06-20T10:00:05Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/r/late.txt","content":"l1"}}]}}"#, "\n",
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
    std::fs::create_dir_all(projects.join("p")).unwrap();
    std::fs::write(projects.join("p/datesess.jsonl"), FIXTURE).unwrap();
    let cfg = claude_only_config(dir.path(), &projects);
    let db = Db::open(&cfg.db_path()).unwrap();
    indexer::reindex(&cfg, &db, true, None).unwrap();
    (dir, db)
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

#[test]
fn search_messages_honors_since_until_and_window() {
    let (_d, db) = indexed();
    let users = |f: MessageFilters| db.search_messages("", &f).unwrap().len();

    // since excludes the Jun 1 turn.
    assert_eq!(
        users(MessageFilters { role: Some(Role::User), since: Some(at("2026-06-05T00:00:00Z")), ..Default::default() }),
        2
    );
    // until excludes the Jun 20 turn.
    assert_eq!(
        users(MessageFilters { role: Some(Role::User), until: Some(at("2026-06-15T00:00:00Z")), ..Default::default() }),
        2
    );
    // since+until brackets to the single Jun 10 turn.
    assert_eq!(
        users(MessageFilters {
            role: Some(Role::User),
            since: Some(at("2026-06-05T00:00:00Z")),
            until: Some(at("2026-06-15T00:00:00Z")),
            ..Default::default()
        }),
        1
    );
}

#[test]
fn file_search_honors_date_bounds() {
    let (_d, db) = indexed();
    let count = |q: FileQuery| db.file_search(&q).unwrap().len();
    // All three Writes.
    assert_eq!(count(FileQuery::default()), 3);
    // since Jun 5 drops early.txt.
    assert_eq!(count(FileQuery { since: Some(at("2026-06-05T00:00:00Z")), ..Default::default() }), 2);
    // bracket to just the Jun 10 write.
    assert_eq!(
        count(FileQuery {
            since: Some(at("2026-06-05T00:00:00Z")),
            until: Some(at("2026-06-15T00:00:00Z")),
            ..Default::default()
        }),
        1
    );
}

#[test]
fn role_counts_reflect_date_filter() {
    let (_d, db) = indexed();
    let users_since = db
        .message_role_counts(&MessageFilters { since: Some(at("2026-06-05T00:00:00Z")), ..Default::default() })
        .unwrap()
        .into_iter()
        .find(|(r, _)| r == "user")
        .map(|(_, n)| n)
        .unwrap_or(0);
    assert_eq!(users_since, 2, "two user turns on/after Jun 5");
}

#[test]
fn filters_compose_date_role_regex_limit_without_cancelling() {
    let (_d, db) = indexed();

    // date + role + regex: only the Jun 20 'cherry' user turn survives all three.
    let hits = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::User),
                since: Some(at("2026-06-05T00:00:00Z")),
                regex: Some(r"\bcherry\b".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].content.contains("cherry"));

    // date + role + limit: two match the date+role, limit caps to one.
    let capped = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::User),
                since: Some(at("2026-06-05T00:00:00Z")),
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(capped.len(), 1);

    // A regex that matches nothing yields zero even with a permissive date window —
    // filters AND together, none silently dominates.
    let none = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::User),
                regex: Some("zzz_nomatch".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn corrections_honor_date_window() {
    let (_d, db) = indexed();
    // "late cherry" is benign; add a correction-bearing fixture inline via a wider check:
    // every user turn is scanned, but only those in-window are considered. Use the
    // built-in 'apple/banana/cherry' turns with a pattern that matches one of them.
    let patterns = vec![(
        "test".to_string(),
        regex::Regex::new("(?i)cherry").unwrap(),
    )];
    let all = db.find_corrections(&patterns, &MessageFilters::default()).unwrap();
    assert_eq!(all.len(), 1, "only the cherry turn matches");
    // Window before Jun 20 excludes it.
    let before = db
        .find_corrections(
            &patterns,
            &MessageFilters { until: Some(at("2026-06-15T00:00:00Z")), ..Default::default() },
        )
        .unwrap();
    assert!(before.is_empty(), "cherry turn (Jun 20) is outside the until window");
}
