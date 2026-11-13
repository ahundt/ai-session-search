// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use ai_session_search::config::Config;
use ai_session_search::SessionSearch;
use rusqlite::{types::Value, Connection};
use std::path::Path;

fn config(root: &Path, sources: &Path) -> Config {
    let mut config = Config::default();
    config.index.db_path = Some(root.join("index.db").to_string_lossy().into_owned());
    config.providers.claude.paths = vec![sources.join("claude").to_string_lossy().into_owned()];
    config.providers.codex.paths = vec![sources.join("codex").to_string_lossy().into_owned()];
    config.providers.claude_desktop.enabled = false;
    config.providers.cursor.enabled = false;
    config.providers.antigravity.enabled = false;
    config.providers.pi.enabled = false;
    config.providers.prime_agent.enabled = false;
    config.providers.aistudio.enabled = false;
    config.providers.gemini_cli.enabled = false;
    config
}

fn snapshot(path: &Path) -> Vec<Vec<Vec<Value>>> {
    let conn = Connection::open(path).unwrap();
    [
        "select session_id,provider,seq,role,ts,kind,content,tool_name,tool_call_id,is_compaction,authorship,correlation_authority,correlation_scope,correlation_id,input_delivery_state,input_origin,record_relation from messages order by session_id,seq,id",
        "select session_id,provider,seq,file_path,file_name,ts,tool,new_content,edits_json from file_edits order by session_id,seq,id",
        "select session_id,transcript_text from transcripts order by session_id",
        "select m.session_id,m.seq,p.ordinal,p.start_char,p.end_char,p.authorship,p.origin from message_content_parts p join messages m on m.id=p.message_id order by m.session_id,m.seq,p.ordinal",
        "select type,name from sqlite_schema order by type,name",
        "select s.name,p.name,p.type from sqlite_schema s,pragma_table_info(s.name) p where s.type='table' order by s.name,p.name",
        "select s.name,p.seqno,p.name from sqlite_schema s,pragma_index_info(s.name) p where s.type='index' order by s.name,p.seqno",
    ].iter().map(|sql| {
        let mut statement = conn.prepare(sql).unwrap();
        let count = statement.column_count();
        statement.query_map([], |row| (0..count).map(|i| row.get(i)).collect()).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }).collect()
}

#[test]
fn published_generation_five_refresh_matches_fresh_index() {
    let dir = tempfile::tempdir().unwrap();
    let sources = dir.path().join("sources");
    for (provider, text) in [
        ("claude", include_str!("fixtures/v1_0_0_claude.jsonl")),
        ("codex", include_str!("fixtures/v1_0_0_codex.jsonl")),
    ] {
        std::fs::create_dir_all(sources.join(provider)).unwrap();
        std::fs::write(sources.join(provider).join("session.jsonl"), text).unwrap();
    }
    let fresh_config = config(&dir.path().join("fresh"), &sources);
    let fresh = SessionSearch::open(fresh_config.clone()).unwrap();
    fresh.index().reindex_report(true).unwrap();
    drop(fresh);
    let expected = snapshot(&fresh_config.db_path());
    assert!(!expected[0].is_empty());
    assert!(!expected[1].is_empty());
    for full in [false, true] {
        let root = dir.path().join(if full { "full" } else { "incremental" });
        std::fs::create_dir_all(&root).unwrap();
        let config = config(&root, &sources);
        let conn = Connection::open(config.db_path()).unwrap();
        conn.execute_batch(
            &include_str!("fixtures/v1_0_0_populated.sql")
                .replace("@SOURCES@", &sources.to_string_lossy().replace('\'', "''")),
        )
        .unwrap();
        assert_eq!(
            conn.query_row("pragma user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
        drop(conn);
        // This is the service constructor used by the CLI, followed by its explicit refresh.
        let upgraded = SessionSearch::open(config.clone()).unwrap();
        assert_eq!(
            &snapshot(&config.db_path())[4..],
            &expected[4..],
            "all tables, columns, and index keys must exist before refresh"
        );
        upgraded.index().reindex_report(full).unwrap();
        drop(upgraded);
        assert_eq!(snapshot(&config.db_path()), expected, "full={full}");
    }
}

#[test]
fn failed_promotion_rolls_back_schema_and_names_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path(), &dir.path().join("sources"));
    let conn = Connection::open(config.db_path()).unwrap();
    conn.execute_batch(include_str!("fixtures/v1_0_0_populated.sql"))
        .unwrap();
    conn.execute_batch("create trigger reject_promotion before insert on index_metadata when new.key='minimum_writer_compatibility' begin select raise(abort,'synthetic migration failure'); end;").unwrap();
    let schema = |conn: &Connection| {
        conn.prepare("select type,name,sql from sqlite_schema order by type,name")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = schema(&conn);
    let messages = conn
        .query_row("select count(*) from messages", [], |r| r.get::<_, i64>(0))
        .unwrap();
    let error = match SessionSearch::open(config.clone()) {
        Ok(_) => panic!("promotion should fail"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("synthetic migration failure"), "{error}");
    assert!(
        error.contains(&config.db_path().display().to_string()),
        "{error}"
    );
    assert!(error.contains("aise reindex --full"), "{error}");
    assert!(error.contains("move"), "{error}");
    assert_eq!(
        conn.query_row("pragma user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        5
    );
    assert_eq!(schema(&conn), before);
    assert_eq!(
        conn.query_row("select count(*) from messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        messages
    );
}
