// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

/// Run explicitly with AISE_PUBLICATION_BENCH_DB pointing to a synthetic database and --ignored --nocapture.
/// Each invocation appends 50 parsed apply_patch edits; interleave four invocations per build.
#[test]
#[ignore]
fn no_fork_publication_benchmark() {
    let path =
        std::env::var("AISE_PUBLICATION_BENCH_DB").expect("explicit synthetic database required");
    let db = Db::open(Path::new(&path)).unwrap();
    let mut parsed = archive_session(Vec::new(), Vec::new());
    parsed.session.raw_metadata_json = Some(
        serde_json::json!({
            "id":"archive", "cwd":"/synthetic/project", "originator":"codex_cli_rs",
            "cli_version":"0.60.0", "source":"cli", "model_provider":"openai",
            "base_instructions":{"text":"Synthetic coding instructions. ".repeat(140)},
            "git":{"branch":"main","commit_hash":"0123456789abcdef0123456789abcdef01234567"}
        })
        .to_string(),
    );
    let count: i64 = db
        .conn
        .query_row("select count(*) from sessions", [], |r| r.get(0))
        .unwrap();
    if count == 0 {
        db.upsert_session(&parsed, 1, 1).unwrap();
        let tx = db.conn.unchecked_transaction().unwrap();
        for i in 1..20_000 {
            let id = format!("codex:synthetic-{i}");
            let source = format!("/synthetic/{i}.jsonl");
            tx.execute("insert into sessions (id,provider,provider_session_id,preview_text,source_path,parse_version,raw_metadata_json,discovery_source) values (?1,'codex',?1,'',?2,'v1',?3,'test')", params![id, source, parsed.session.raw_metadata_json]).unwrap();
            tx.execute("insert into files_seen (provider,source_path,mtime_ns,size_bytes,last_indexed_at,parse_version) values ('codex',?1,1,1,'2026-11-01','v1')", params![source]).unwrap();
        }
        tx.commit().unwrap();
    }
    assert_eq!(
        db.conn
            .query_row("select count(*) from sessions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        20_000
    );
    let prior = db.file_edit_count().unwrap();
    let adapter = crate::providers::codex::CodexAdapter::new(Vec::new());
    let gated = std::env::var_os("AISE_PUBLICATION_BENCH_STDIN").is_some();
    println!("bench_ready");
    for i in 0..50 {
        let n = prior + i;
        let input = format!("*** Begin Patch\n*** Add File: /synthetic/edit-{n}.rs\n+synthetic content {n}\n*** End Patch");
        let body = format!(
            "{}\n{}\n{}\n",
            serde_json::json!({"type":"session_meta","payload":{"id":"archive"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":format!("edit-{n}"),"name":"apply_patch","input":input}}),
            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":format!("edit-{n}"),"output":format!("Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA /synthetic/edit-{n}.rs\n")}})
        );
        let edit = adapter
            .parse_reader(
                std::io::Cursor::new(body),
                Path::new("/synthetic/archive.jsonl"),
            )
            .unwrap();
        assert_eq!(edit.file_edits.len(), 1);
        let tail = crate::tail::TailParse {
            session: parsed.session.clone(),
            overlap_messages: Vec::new(),
            new_messages: Vec::new(),
            new_file_edits: edit.file_edits,
            new_transcript: String::new(),
            new_tail_offset: n + 1,
            new_fingerprint: "synthetic".into(),
        };
        if gated {
            std::io::Read::read_exact(&mut std::io::stdin().lock(), &mut [0]).unwrap();
        }
        let start = std::time::Instant::now();
        db.append_tail(&tail, n + 2, n + 2).unwrap();
        println!("append_us={:.3}", start.elapsed().as_secs_f64() * 1e6);
    }
    assert_eq!(db.file_edit_count().unwrap(), prior + 50);
}

/// Verify an actual database produced by the historical benchmark executable.
#[test]
#[ignore]
fn previous_writer_fork_index_upgrade() {
    let source =
        std::env::var("AISE_UPGRADE_BENCH_DB").expect("historical synthetic database required");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upgrade.db");
    fs::copy(source, &path).unwrap();
    let reader = Db::open_existing_read_only_with_threads(&path, 250, NonZeroUsize::MIN).unwrap();
    let before = schema_fingerprint(&reader.conn);
    assert!(!before
        .iter()
        .any(|row| row.1 == "idx_sessions_codex_fork_parent"));
    assert_eq!(
        reader
            .conn
            .query_row("select count(*) from sessions", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        20_000
    );
    let version = reader.schema_version().unwrap();
    drop(reader);
    let writer = Db::open(&path).unwrap();
    assert_eq!(writer.schema_version().unwrap(), version);
    assert!(schema_fingerprint(&writer.conn)
        .iter()
        .any(|row| row.1 == "idx_sessions_codex_fork_parent"));
    drop(writer);
    let reader = Db::open_existing_read_only_with_threads(&path, 250, NonZeroUsize::MIN).unwrap();
    assert_eq!(
        reader
            .conn
            .query_row("select count(*) from file_edits", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        200
    );
}
