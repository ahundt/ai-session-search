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
fn published_generation_five_user_role_search_is_read_only() {
    use ai_session_search::config::IndexRefresh;
    use ai_session_search::db::MessageOrder;
    use ai_session_search::models::{MessageFilters, Role};
    use ai_session_search::{MessageQuery, MessageSearchRequest, MessageTarget, RequestedExtent};

    let root = tempfile::tempdir().unwrap();
    let mut config = config(root.path(), &root.path().join("sources"));
    config.index.refresh = IndexRefresh::ExistingOnly;
    let conn = Connection::open(config.db_path()).unwrap();
    conn.execute_batch(include_str!("fixtures/v1_0_0_populated.sql"))
        .unwrap();
    drop(conn);
    let before = std::fs::read(config.db_path()).unwrap();
    let app = SessionSearch::open_strict(config.clone()).unwrap();
    let expected = vec![
        ("claude:upgrade-claude".to_string(), 0),
        ("codex:upgrade-codex".to_string(), 0),
    ];
    let request = |query| {
        MessageSearchRequest::builder(query, MessageTarget::content())
            .role(Role::User)
            .extent(RequestedExtent::page(Some(10), 0).unwrap())
            .build()
            .unwrap()
    };
    for query in [
        MessageQuery::literal("question").unwrap(),
        MessageQuery::regex("question").unwrap(),
        MessageQuery::fuzzy("question").unwrap(),
        MessageQuery::All,
    ] {
        let query = request(query);
        let response = app.messages().search(query.clone()).unwrap();
        let ids = |response: &ai_session_search::MessageSearchResponse| {
            response
                .results()
                .iter()
                .map(|hit| (hit.message().session_id.clone(), hit.message().seq))
                .collect::<Vec<_>>()
        };
        let found = ids(&response);
        assert_eq!(found, ids(&app.messages().search(query).unwrap()));
        let mut sorted = found;
        sorted.sort();
        assert_eq!(sorted, expected);
    }
    let rows = app
        .messages()
        .read_session(
            &MessageFilters::default()
                .with_role(Role::User)
                .with_session_id("claude:upgrade-claude"),
            MessageOrder::OldestFirst,
        )
        .unwrap();
    assert_eq!(rows.iter().map(|row| row.seq).collect::<Vec<_>>(), vec![0]);
    drop(app);
    assert_eq!(std::fs::read(config.db_path()).unwrap(), before);
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

#[test]
fn published_queue_notifications_match_fresh_without_losing_archive() {
    use serde_json::json;
    // v1.0.0 wrote every queue-operation containing a notice, including remove.
    for arrangement in [
        "enqueue_remove",
        "repeated",
        "enqueue_only",
        "delivered_only",
        "archive",
        "archive_enqueue",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(sources.join("claude")).unwrap();
        let notice = "<task-notification>synthetic completion</task-notification>";
        let mut records = Vec::new();
        let mut legacy = Vec::new();
        let operations: &[&str] = match arrangement {
            "enqueue_remove" | "archive" => &["enqueue", "remove"],
            "repeated" => &["enqueue", "remove", "enqueue", "remove"],
            "enqueue_only" => &["enqueue"],
            "archive_enqueue" => &["enqueue", "enqueue"],
            _ => &[],
        };
        for (i, operation) in operations.iter().enumerate() {
            let ts = format!("2026-06-01T11:00:{i:02}+00:00");
            legacy.push((ts.clone(), None));
            if (arrangement != "archive" || *operation != "remove")
                && (arrangement != "archive_enqueue" || i != 0)
            {
                records.push(json!({"type":"queue-operation","operation":operation,
                    "sessionId":"upgrade-claude","timestamp":ts,"content":notice}));
            }
        }
        if arrangement != "enqueue_only" {
            records.push(
                json!({"type":"attachment","uuid":"delivery","sessionId":"upgrade-claude",
                "timestamp":"2026-06-01T11:01:00Z","attachment":{"type":"queued_command",
                "commandMode":"task-notification","prompt":notice}}),
            );
        }
        if arrangement == "archive" {
            // A replay at a different timestamp cannot retract the absent removal record.
            records.push(records.last().unwrap().clone());
        }
        if arrangement == "delivered_only" {
            let ts = "2026-06-01T11:02:00+00:00".to_string();
            records.push(
                json!({"type":"user","uuid":"direct-notice","sessionId":"upgrade-claude",
                "timestamp":ts,"message":{"role":"user","content":notice}}),
            );
            legacy.push((ts, Some("direct-notice")));
        }
        let text = include_str!("fixtures/v1_0_0_claude.jsonl").to_string()
            + &records.iter().map(|v| format!("{v}\n")).collect::<String>();
        std::fs::write(sources.join("claude/session.jsonl"), text).unwrap();
        let fresh_config = config(&dir.path().join("fresh"), &sources);
        let fresh = SessionSearch::open(fresh_config.clone()).unwrap();
        fresh.index().reindex_report(true).unwrap();
        drop(fresh);
        let expected = snapshot(&fresh_config.db_path());
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
            conn.execute("delete from messages where provider='codex'", [])
                .unwrap();
            conn.execute("delete from sessions where provider='codex'", [])
                .unwrap();
            conn.execute(
                "delete from transcripts where session_id like 'codex:%'",
                [],
            )
            .unwrap();
            for (i, (ts, id)) in legacy.iter().enumerate() {
                conn.execute("insert into messages(session_id,provider,seq,role,ts,kind,content,authorship,record_relation,correlation_authority,correlation_scope,correlation_id) values ('claude:upgrade-claude','claude',?1,'user',?2,'harness_notice',?3,'harness','original',?4,?5,?6)", rusqlite::params![4+i as i64,ts,notice,id.map(|_|"anthropic"),id.map(|_|"claude:upgrade-claude"),id]).unwrap();
            }
            drop(conn);
            let upgraded = SessionSearch::open(config.clone()).unwrap();
            upgraded.index().reindex_report(full).unwrap();
            drop(upgraded);
            let observed = snapshot(&config.db_path());
            if arrangement.starts_with("archive") {
                let notices: Vec<_> = observed[0]
                    .iter()
                    .filter(|r| r[5] == Value::Text("harness_notice".into()))
                    .collect();
                assert_eq!(
                    notices.len(),
                    2,
                    "missing source record must remain full={full}"
                );
                assert!(notices.iter().any(|r| r[4]
                    == Value::Text(
                        if arrangement == "archive" {
                            "2026-06-01T11:00:01+00:00"
                        } else {
                            "2026-06-01T11:00:00+00:00"
                        }
                        .into()
                    )
                    && r[11] == Value::Null));
            } else {
                assert_eq!(observed, expected, "arrangement={arrangement} full={full}");
            }
        }
    }
}

#[test]
fn published_metadata_refresh_keeps_full_text_size_near_fresh() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let sources = dir.path().join("sources");
    std::fs::create_dir_all(sources.join("claude")).unwrap();
    let records: Vec<_> = (0..2048)
        .map(|i| {
            let content = format!(
                "synthetic migration record {i} {}end",
                "searchable notification payload ".repeat(16)
            );
            json!({"type":"user","uuid":format!("input-{i}"),"sessionId":"upgrade-claude",
            "timestamp":"2026-06-01T10:00:00Z","message":{"role":"user","content":content}})
        })
        .collect();
    std::fs::write(
        sources.join("claude/session.jsonl"),
        records.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    let fresh_config = config(&dir.path().join("fresh"), &sources);
    let fresh = SessionSearch::open(fresh_config.clone()).unwrap();
    fresh.index().reindex_report(false).unwrap();
    drop(fresh);
    let sizes = |path: &Path| {
        let conn = Connection::open(path).unwrap();
        ["messages_fts", "messages_trigram"].map(|table| {
            conn.query_row(
                "select coalesce(sum(pgsize),0) from dbstat where name glob ?1",
                [format!("{table}_*")],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        })
    };
    let expected = sizes(&fresh_config.db_path());
    for full in [false, true] {
        let root = dir.path().join(if full { "full" } else { "incremental" });
        std::fs::create_dir_all(&root).unwrap();
        let config = config(&root, &sources);
        let mut conn = Connection::open(config.db_path()).unwrap();
        conn.execute_batch(
            &include_str!("fixtures/v1_0_0_populated.sql")
                .replace("@SOURCES@", &sources.to_string_lossy().replace('\'', "''")),
        )
        .unwrap();
        conn.execute_batch("delete from messages; delete from transcripts; delete from file_edits; delete from sessions where provider='codex';").unwrap();
        let tx = conn.transaction().unwrap();
        for (i, record) in records.iter().enumerate() {
            tx.execute("insert into messages(session_id,provider,seq,role,ts,kind,content,authorship,record_relation,correlation_authority,correlation_scope,correlation_id) values ('claude:upgrade-claude','claude',?1,'user','2026-06-01T10:00:00+00:00','conversation',?2,'human','original','anthropic','claude:upgrade-claude',?3)", rusqlite::params![i as i64,record["message"]["content"].as_str().unwrap(),record["uuid"].as_str().unwrap()]).unwrap();
        }
        tx.commit().unwrap();
        conn.execute_batch("insert into messages_fts(messages_fts) values('rebuild'); insert into messages_trigram(messages_trigram) values('rebuild');").unwrap();
        drop(conn);
        let upgraded = SessionSearch::open(config.clone()).unwrap();
        upgraded.index().reindex_report(full).unwrap();
        drop(upgraded);
        for (observed, fresh) in sizes(&config.db_path()).into_iter().zip(expected) {
            assert!(
                observed <= fresh * 110 / 100 + 65536,
                "full={full} FTS bytes: upgraded={observed} fresh={fresh}"
            );
        }
        assert_eq!(
            snapshot(&config.db_path()),
            snapshot(&fresh_config.db_path())
        );
        // Repeated in-place text updates leave delete flags even after FTS5 optimize.
        let conn = Connection::open(config.db_path()).unwrap();
        conn.execute_batch(
            "update messages set content=content; update messages set content=content;",
        )
        .unwrap();
        drop(conn);
        let app = SessionSearch::open(config.clone()).unwrap();
        app.maintenance().compact().unwrap();
        drop(app);
        for (observed, fresh) in sizes(&config.db_path()).into_iter().zip(expected) {
            assert!(
                observed <= fresh * 110 / 100 + 65536,
                "compact full={full} FTS bytes: upgraded={observed} fresh={fresh}"
            );
        }
        assert_eq!(
            snapshot(&config.db_path()),
            snapshot(&fresh_config.db_path())
        );
    }
}

// Legacy queue rows have no identity; equal text and time cannot identify a removed event.
fn queue_archive_refresh(case: &str) {
    use serde_json::json;
    for full in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(sources.join("claude")).unwrap();
        let notice = "<task-notification>synthetic archived completion</task-notification>";
        let ts = "2026-06-01T10:00:00+00:00";
        let later = "2026-06-01T10:00:01+00:00";
        let queue = |operation: &str, timestamp: &str| {
            json!({
                "type":"queue-operation", "operation":operation,
                "sessionId":"upgrade-claude", "timestamp":timestamp, "content":notice
            })
        };
        let records = match case {
            "both_present" => vec![queue("enqueue", ts), queue("remove", ts)],
            "two_enqueues" => vec![queue("enqueue", ts), queue("enqueue", ts)],
            "enqueue_gone_pair" => vec![queue("remove", later)],
            "identified_remove" => {
                let mut removal = queue("remove", ts);
                removal["uuid"] = json!("distinct-removal");
                vec![removal]
            }
            _ => vec![queue("remove", ts)],
        };
        std::fs::write(
            sources.join("claude/session.jsonl"),
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let fresh_config = config(&dir.path().join("fresh"), &sources);
        let fresh = SessionSearch::open(fresh_config.clone()).unwrap();
        fresh.index().reindex_report(full).unwrap();
        drop(fresh);
        let expected = snapshot(&fresh_config.db_path());
        let config = config(&dir.path().join("upgrade"), &sources);
        std::fs::create_dir_all(config.db_path().parent().unwrap()).unwrap();
        let conn = Connection::open(config.db_path()).unwrap();
        conn.execute_batch(
            &include_str!("fixtures/v1_0_0_populated.sql")
                .replace("@SOURCES@", &sources.to_string_lossy().replace('\'', "''")),
        )
        .unwrap();
        conn.execute_batch("delete from messages; delete from file_edits; delete from transcripts; delete from sessions where provider='codex';").unwrap();
        let stored_times = match case {
            "both_present" | "two_enqueues" => vec![ts, ts],
            "enqueue_gone_pair" => vec![ts, later],
            _ => vec![ts],
        };
        for (seq, ts) in stored_times.iter().enumerate() {
            conn.execute("insert into messages(session_id,provider,seq,role,ts,kind,content,authorship,record_relation) values ('claude:upgrade-claude','claude',?1,'user',?2,'harness_notice',?3,'harness','original')",
                rusqlite::params![seq as i64, ts, notice]).unwrap();
        }
        let before: i64 = conn
            .query_row("select count(*) from messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before as usize, stored_times.len());
        drop(conn);
        let upgraded = SessionSearch::open(config.clone()).unwrap();
        upgraded.index().reindex_report(full).unwrap();
        drop(upgraded);
        let observed = snapshot(&config.db_path());
        let count = if case == "two_enqueues" { 2 } else { 1 };
        assert_eq!(observed[0].len(), count, "case={case} full={full}");
        if matches!(case, "both_present" | "two_enqueues") {
            assert_eq!(observed, expected, "case={case} full={full}");
        } else {
            assert!(expected[0].is_empty());
            assert_eq!(observed[0][0][4], Value::Text(ts.into()));
            assert_eq!(observed[0][0][13], Value::Null);
        }
    }
}

#[test]
fn published_queue_archive_keeps_last_equal_time_copy() {
    queue_archive_refresh("enqueue_gone");
}
#[test]
fn published_queue_archive_keeps_last_copy_with_distinct_removal_identity() {
    queue_archive_refresh("identified_remove");
}
#[test]
fn published_queue_archive_retracts_remove_with_enqueue_present() {
    queue_archive_refresh("both_present");
}
#[test]
fn published_queue_archive_retracts_remove_but_keeps_missing_enqueue() {
    queue_archive_refresh("enqueue_gone_pair");
}
#[test]
fn published_queue_archive_keeps_lone_removal() {
    queue_archive_refresh("remove_only");
}
#[test]
fn published_queue_archive_keeps_identical_enqueues() {
    queue_archive_refresh("two_enqueues");
}

#[test]
fn queue_removal_keeps_the_last_copy_of_a_notice_stored_from_another_record() {
    // The notice is stored from a record that then leaves the source; a queue removal record with
    // the same text and time takes its place. Equal columns do not prove the stored row came from
    // the removal record, so the row stays.
    use serde_json::json;
    for full in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let sources = dir.path().join("sources");
        std::fs::create_dir_all(sources.join("claude")).unwrap();
        let notice = "<task-notification>synthetic archived completion</task-notification>";
        let ts = "2026-06-01T10:00:00Z";
        let file = sources.join("claude/session.jsonl");
        let stored_from = json!({
            "type":"user", "sessionId":"upgrade-claude", "timestamp":ts,
            "message":{"role":"user", "content":notice}
        });
        std::fs::write(&file, format!("{stored_from}\n")).unwrap();
        let config = config(&dir.path().join("index"), &sources);
        let search = SessionSearch::open(config.clone()).unwrap();
        search.index().reindex_report(false).unwrap();
        drop(search);
        let before = snapshot(&config.db_path());
        assert_eq!(before[0].len(), 1, "the notice is stored once");
        assert_eq!(
            before[0][0][13],
            Value::Null,
            "the stored notice has no identity"
        );
        let removal = json!({
            "type":"queue-operation", "operation":"remove",
            "sessionId":"upgrade-claude", "timestamp":ts, "content":notice
        });
        std::fs::write(&file, format!("{removal}\n")).unwrap();
        let search = SessionSearch::open(config.clone()).unwrap();
        search.index().reindex_report(full).unwrap();
        drop(search);
        let after = snapshot(&config.db_path());
        assert_eq!(after[0], before[0], "full={full}");
    }
}
