// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

/// Synthetic 80 x 5 MiB forks; run in an isolated HOME with --release --lib and
/// db::tests::fork_publication_benchmark -- --exact --ignored --nocapture.
#[test]
#[ignore]
fn fork_publication_benchmark() {
    for case in ["unique-path", "common-path-no-match", "common-path-match"] {
        fork_publication_case(case);
    }
}

fn fork_publication_case(case: &str) {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("index.db")).unwrap();
    let adapter = crate::providers::codex::CodexAdapter::new(Vec::new());
    let mutation = |n: &str| {
        let path = if case == "unique-path" { n } else { "common" };
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":n,"name":"apply_patch","input":format!("*** Begin Patch\n*** Add File: /synthetic/{path}.rs\n+content {n}\n*** End Patch")}}),
            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":n,"output":format!("Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA /synthetic/{path}.rs\n")}})
        )
    };
    let header = |id: &str, parent: Option<&str>| {
        format!(
            "{}\n",
            serde_json::json!({"type":"session_meta","payload":{"id":id,"forked_from_id":parent}})
        )
    };
    let parent_path = dir.path().join("parent.jsonl");
    fs::write(
        &parent_path,
        format!("{}{}", header("parent", None), mutation("shared")),
    )
    .unwrap();
    let publish = |path: &Path| {
        let parsed = adapter
            .parse_reader(std::io::BufReader::new(fs::File::open(path).unwrap()), path)
            .unwrap();
        let meta = fs::metadata(path).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        db.upsert_session(&parsed, mtime, meta.len() as i64)
            .unwrap();
        parsed
    };
    let parent = publish(&parent_path);
    let padding = format!(
        "{}\n",
        serde_json::json!({"type":"synthetic_padding","payload":"x".repeat(5 * 1024 * 1024)})
    );
    for i in 0..80 {
        let path = dir.path().join(format!("child-{i}.jsonl"));
        let mut body = header(&format!("child-{i}"), Some("parent"));
        if i < 4 {
            body.push_str(&mutation("shared"));
        }
        if case == "common-path-match" {
            for n in 0..50 {
                body.push_str(&mutation(&format!("parent-tail-{n}")));
            }
        }
        body.push_str(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n",
        );
        body.push_str(&mutation(&format!("child-{i}")));
        body.push_str(&padding);
        fs::write(&path, body).unwrap();
        publish(&path);
    }
    // Unrelated sessions share the hot path but must not affect child-side probe work.
    db.with_immediate_transaction(|| {
        db.conn.execute_batch(
            "with recursive n(i) as (values(0) union all select i+1 from n where i<49999)
             insert into sessions(id,provider,provider_session_id,preview_text,source_path,parse_version,discovery_source)
             select 'codex:unrelated-'||i,'codex','unrelated-'||i,'','/synthetic/unrelated-'||i,'synthetic','synthetic' from n;
             insert into file_edits(session_id,provider,seq,tool,file_path,file_name,new_content)
             select id,'codex',0,'apply_patch','/synthetic/common.rs','common.rs','unrelated content'
             from sessions where id like 'codex:unrelated-%';"
        )?;
        Ok(())
    }).unwrap();
    assert_eq!(
        db.conn
            .query_row(
                "select count(*) from file_edits where session_id like 'codex:unrelated-%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        50_000
    );
    let prior = db.file_edit_count().unwrap();
    let mut times = Vec::new();
    for i in 0..50 {
        let body = mutation(&format!("parent-tail-{i}"));
        fs::OpenOptions::new()
            .append(true)
            .open(&parent_path)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let parsed = adapter
            .parse_reader(
                std::io::Cursor::new(format!("{}{}", header("parent", None), body)),
                &parent_path,
            )
            .unwrap();
        assert_eq!(parsed.file_edits.len(), 1);
        let meta = fs::metadata(&parent_path).unwrap();
        let tail = crate::tail::TailParse {
            session: parent.session.clone(),
            overlap_digest: String::new(),
            new_overlap_digest: String::new(),
            new_messages: Vec::new(),
            new_file_edits: parsed.file_edits,
            new_transcript: String::new(),
            new_tail_offset: meta.len() as i64,
            new_fingerprint: "synthetic".into(),
        };
        let start = std::time::Instant::now();
        db.append_tail(&tail, i + 2, meta.len() as i64).unwrap();
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(
        db.file_edit_count().unwrap(),
        if case == "common-path-match" {
            prior + 50 - 80 * 50
        } else {
            prior + 50
        }
    );
    times.sort_by(f64::total_cmp);
    println!(
        "{case}: Q=50000, K=80, 5 MiB each, 50 appends: min/median/max ms = {:.3}/{:.3}/{:.3}",
        times[0],
        (times[24] + times[25]) / 2.0,
        times[49]
    );
}
