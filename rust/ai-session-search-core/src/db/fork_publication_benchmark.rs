// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

/// Synthetic 80 x 5 MiB forks; run in an isolated HOME with --release --lib and
/// db::tests::fork_publication_benchmark -- --exact --ignored --nocapture.
#[test]
#[ignore]
fn fork_publication_benchmark() {
    let tail_ms = fork_large_family_case(2_000, true);
    fork_large_family_case(2_000, false);
    if tail_ms < 1_000.0 {
        fork_large_family_case(20_000, true);
    }
    for case in ["unique-path", "common-path-no-match", "common-path-match"] {
        fork_publication_case(case);
    }
}

// Publish through the real writers; setup and parsing are outside the timed region.
fn fork_large_family_case(edits: i64, tail_only: bool) -> f64 {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("index.db")).unwrap();
    let make_edit = |seq: i64| {
        let mut edit = FileEdit::new(seq, "apply_patch", format!("/synthetic/path-{}.rs", seq % 50));
        edit.new_content = Some(format!("publisher content {seq}"));
        edit
    };
    let mut parent = archive_session(Vec::new(), (0..edits).map(make_edit).collect());
    parent.session.source_path = dir.path().join("parent.jsonl").to_string_lossy().into_owned();
    db.upsert_session(&parent, 1, 1).unwrap();
    db.with_immediate_transaction(|| {
        for child in 0..80 {
            let id = format!("codex:child-{child}");
            let path = dir.path().join(format!("child-{child}.jsonl")).to_string_lossy().into_owned();
            db.conn.execute("insert into sessions(id,provider,provider_session_id,preview_text,source_path,parse_version,raw_metadata_json,discovery_source) values (?1,'codex',?1,'',?2,'v1',?3,'test')", params![id, path, r#"{"forked_from_id":"archive"}"#])?;
            db.conn.execute("insert into files_seen(provider,source_path,mtime_ns,size_bytes,last_indexed_at,parse_version) values ('codex',?1,1,1,'2026-11-01','v1')", [&path])?;
            db.conn.execute("with recursive n(i) as (values(0) union all select i+1 from n where i+1<?2)
                insert into file_edits(session_id,provider,seq,tool,file_path,file_name,new_content)
                select ?1,'codex',i,'apply_patch','/synthetic/path-'||(i%50)||'.rs','path-'||(i%50)||'.rs','child content '||?1||':'||i from n", params![id, edits])?;
        }
        Ok(())
    }).unwrap();
    let mut times = Vec::new();
    for trial in 0..5 {
        // Restore P exactly: each sample appends one edit to the same stored publisher size.
        db.conn.execute("delete from file_edits where session_id = ?1 and seq >= ?2", params![parent.session.id, edits]).unwrap();
        let tail = crate::tail::TailParse {
            session: parent.session.clone(),
            overlap_digest: String::new(),
            new_overlap_digest: String::new(),
            new_messages: Vec::new(),
            new_file_edits: vec![make_edit(edits)],
            new_transcript: String::new(),
            new_tail_offset: 2,
            new_fingerprint: "synthetic".into(),
        };
        let start = std::time::Instant::now();
        if tail_only {
            db.append_tail(&tail, trial + 2, 2).unwrap();
        } else {
            db.upsert_session(&parent, trial + 2, 1).unwrap();
        }
        times.push(start.elapsed().as_secs_f64() * 1_000.0);
        assert_eq!(db.file_edit_count().unwrap(), 81 * edits + i64::from(tail_only));
    }
    times.sort_by(f64::total_cmp);
    println!("large-family: K=80, P=C={edits}, {}, 5 trials: min/median/max ms = {:.3}/{:.3}/{:.3}", if tail_only { "tail one edit" } else { "full" }, times[0], times[2], times[4]);
    times[2]
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
