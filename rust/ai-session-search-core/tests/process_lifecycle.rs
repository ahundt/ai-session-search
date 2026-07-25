use std::fs;
use std::io::{Read as _, Write as _};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

fn isolated_config_paths_args(root: &std::path::Path) -> Vec<String> {
    let config = root.join("config.toml");
    fs::write(&config, "").unwrap();
    vec![
        "--config".into(),
        config.display().to_string(),
        "--database".into(),
        root.join("index.db").display().to_string(),
        "--cache-dir".into(),
        root.join("cache").display().to_string(),
        "config".into(),
        "paths".into(),
    ]
}

fn write_disabled_provider_config(root: &std::path::Path) -> std::path::PathBuf {
    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"[index]
db_path = {:?}
cache_dir = {:?}
[providers.claude]
enabled = false
paths = []
[providers.claude-desktop]
enabled = false
paths = []
[providers.codex]
enabled = false
paths = []
[providers.cursor]
enabled = false
paths = []
[providers.antigravity]
enabled = false
paths = []
[providers.pi]
enabled = false
paths = []
[providers.aistudio]
enabled = false
paths = []
[providers.gemini-cli]
enabled = false
paths = []
"#,
            root.join("index.db").display().to_string(),
            root.join("cache").display().to_string(),
        ),
    )
    .unwrap();
    config
}

#[test]
fn doctor_json_with_unindexed_explanations_is_one_structured_document() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    ai_session_search::db::Db::open(&root.path().join("index.db")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "doctor",
            "--format",
            "json",
            "--explain-unindexed",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "doctor JSON must not be followed by human text: {error}: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert_eq!(
        document["unindexed_file_explanations"],
        serde_json::json!([])
    );
}

#[cfg(unix)]
#[test]
fn config_paths_and_package_status_keep_separate_concepts() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first bin");
    let second = root.path().join("second bin");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    for directory in [&first, &second] {
        let candidate = directory.join("aise");
        fs::write(&candidate, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(candidate, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([&first, &second]).unwrap();

    let config_output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args(isolated_config_paths_args(root.path()))
        .env("PATH", &path)
        .output()
        .unwrap();

    assert!(
        config_output.status.success(),
        "{}",
        String::from_utf8_lossy(&config_output.stderr)
    );
    let config_stdout = String::from_utf8(config_output.stdout).unwrap();
    assert!(
        config_stdout.contains(&format!(
            "Config: {}",
            root.path().join("config.toml").display()
        )),
        "{config_stdout}"
    );
    assert!(!config_stdout.contains("Executable:"), "{config_stdout}");
    assert!(!config_stdout.contains("PATH aise"), "{config_stdout}");
    assert!(
        config_stdout.contains("AI Studio roots:"),
        "{config_stdout}"
    );
    assert!(
        config_stdout.contains("Gemini CLI roots:"),
        "{config_stdout}"
    );

    let package_output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args(["package", "status"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        package_output.status.success(),
        "{}",
        String::from_utf8_lossy(&package_output.stderr)
    );
    let package_stdout = String::from_utf8(package_output.stdout).unwrap();
    assert!(
        package_stdout.contains(&format!(
            "Runtime process executable: {}",
            env!("CARGO_BIN_EXE_aise")
        )),
        "{package_stdout}"
    );
    assert!(
        package_stdout.contains(&format!(
            "First aise on PATH: {}",
            first.join("aise").display()
        )),
        "{package_stdout}"
    );
    let candidates = format!(
        "All aise on PATH: {}, {}",
        first.join("aise").display(),
        second.join("aise").display()
    );
    assert!(package_stdout.contains(&candidates), "{package_stdout}");
    assert!(
        package_stdout
            .contains("Warning: multiple aise executables are on PATH; the first candidate wins."),
        "{package_stdout}"
    );
    assert!(
        package_stdout.contains("Installation owner: unknown"),
        "{package_stdout}"
    );
    assert!(!package_stdout.contains("Config:"), "{package_stdout}");

    let mut config_json_args = isolated_config_paths_args(root.path());
    config_json_args.extend(["--format".into(), "json".into()]);
    let config_json_output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args(config_json_args)
        .env("PATH", &path)
        .output()
        .unwrap();
    assert!(
        config_json_output.status.success(),
        "{}",
        String::from_utf8_lossy(&config_json_output.stderr)
    );
    let config_report: serde_json::Value =
        serde_json::from_slice(&config_json_output.stdout).unwrap();
    let provider_roots = config_report["provider_roots"].as_array().unwrap();
    assert_eq!(provider_roots.len(), 8, "{config_report}");
    let providers = provider_roots
        .iter()
        .map(|entry| entry["provider"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        providers,
        std::collections::BTreeSet::from([
            "claude",
            "claude-desktop",
            "codex",
            "cursor",
            "antigravity",
            "pi",
            "aistudio",
            "gemini-cli",
        ])
    );

    let package_json_output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args(["package", "status", "--format", "json"])
        .env("PATH", &path)
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(
        package_json_output.status.success(),
        "{}",
        String::from_utf8_lossy(&package_json_output.stderr)
    );
    let package_report: serde_json::Value =
        serde_json::from_slice(&package_json_output.stdout).unwrap();
    assert_eq!(package_report["installation_owner"], "unknown");
    assert!(package_report.get("runtime_process_executable").is_some());
    assert!(package_report.get("config_file").is_none());
}

#[cfg(unix)]
#[test]
fn short_reader_pipeline_never_prints_a_broken_pipe_panic() {
    let root = tempfile::tempdir().unwrap();
    let executable = env!("CARGO_BIN_EXE_aise");
    let mut command = Command::new("sh");
    command.arg("-c").arg("\"$0\" \"$@\" | head -n 1");
    command.arg(executable);
    command.args(isolated_config_paths_args(root.path()));

    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("Broken pipe"), "{stderr}");
}

#[test]
fn explicit_reindex_makes_a_new_empty_index_immediately_readable() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    let executable = env!("CARGO_BIN_EXE_aise");

    let reindex = Command::new(executable)
        .args(["--config", config.to_str().unwrap(), "reindex"])
        .output()
        .unwrap();
    assert!(
        reindex.status.success(),
        "{}",
        String::from_utf8_lossy(&reindex.stderr)
    );

    let list = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "list",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert_eq!(String::from_utf8(list.stdout).unwrap().trim(), "[]");
}

#[test]
fn cli_full_reindex_promotes_v3_and_releases_exclusive_database_lock() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    let executable = env!("CARGO_BIN_EXE_aise");

    let create = Command::new(executable)
        .args(["--config", config.to_str().unwrap(), "reindex"])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    let db_path = root.path().join("index.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "drop trigger messages_ai;
         drop trigger messages_ad;
         drop trigger messages_au;
         drop table messages_trigram_terms;
         drop table messages_trigram_vocab;
         drop table messages_trigram;
         pragma user_version=3;",
    )
    .unwrap();
    drop(conn);

    let promote = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "reindex",
            "--full",
        ])
        .output()
        .unwrap();
    assert!(
        promote.status.success(),
        "{}",
        String::from_utf8_lossy(&promote.stderr)
    );

    let reader =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let state: (i64, String, bool, bool) = reader
        .query_row(
            "select (select user_version from pragma_user_version),
                    (select journal_mode from pragma_journal_mode),
                    exists(select 1 from sqlite_schema where name='messages_trigram_vocab'),
                    not exists(select 1 from sqlite_schema where name='trigram_postings')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(state, (4, "wal".into(), true, true));
}

/// Reproduce the sessiongrep->aise hybrid-migration failure and assert self-heal.
///
/// The real user index arrived via a verbatim page-level `Backup` (migration.rs:164) of a
/// sessiongrep DB that was already stamped `user_version = 4` but carried the pre-v4 layout:
/// the FTS5 `messages_trigram`/`messages_trigram_vocab` tables were never built, while the
/// obsolete custom-index tables `trigram_postings`/`trigram_meta` are still present. The base
/// `messages` rows are fully intact. In that exact state every command dead-locks at
/// `SessionSearch::open` (service.rs:367 `RecoveryRequired`), and even the advertised
/// `aise reindex --full` cannot help: it bails at open before reindexing, and its schema
/// migrate step is gated on `schema_version < 4` (indexer.rs:503).
///
/// Desired behavior (this test): an ordinary WRITABLE `aise messages search` transparently
/// rebuilds the derived trigram/FTS tables from the intact `messages` table (no transcript
/// re-read, no data loss), drops the obsolete tables, and returns the hit. Note the absence
/// of `--index-refresh existing-only`: a read-only open must still refuse (it cannot write).
#[test]
fn cli_search_self_heals_v4_hybrid_missing_trigram_from_intact_messages() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    let executable = env!("CARGO_BIN_EXE_aise");

    // Build an empty but healthy v4 index (creates messages_trigram + triggers).
    let create = Command::new(executable)
        .args(["--config", config.to_str().unwrap(), "reindex"])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    let db_path = root.path().join("index.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // Insert a searchable message while the v4 triggers still exist so messages_fts is
    // populated too, then mutate the schema into the exact hybrid state.
    conn.execute_batch(
        r#"insert into sessions (
               id, provider, provider_session_id, preview_text, source_path,
               parse_version, discovery_source
           ) values ('claude:heal', 'claude', 'heal', '', '/heal.jsonl', 'test', 'fixture');
           insert into messages (
               session_id, provider, seq, role, kind, tool_name, content
           ) values (
               'claude:heal', 'claude', 0, 'user', 'message', null,
               'selfhealneedle12345 rebuild me from intact messages'
           );
           drop trigger if exists messages_ai;
           drop trigger if exists messages_ad;
           drop trigger if exists messages_au;
           drop table if exists messages_trigram_terms;
           drop table if exists messages_trigram_vocab;
           drop table if exists messages_trigram;
           create table trigram_postings(tg text primary key, ids blob not null, df integer not null);
           create table trigram_meta(key text primary key, value integer not null);
           pragma user_version=4;"#,
    )
    .unwrap();
    let messages_before: i64 = conn
        .query_row("select count(*) from messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        messages_before, 1,
        "fixture must retain the base message row"
    );
    drop(conn);

    // Writable search must self-heal and return the needle (no existing-only).
    let search = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "messages",
            "search",
            "selfhealneedle12345",
            "--field",
            "content",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        search.status.success(),
        "writable search must self-heal a v4 hybrid index, got failure: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap_or_else(|error| {
        panic!(
            "self-healed search must emit JSON rows: {error}: {}",
            String::from_utf8_lossy(&search.stdout)
        )
    });
    assert_eq!(
        rows.as_array().map(Vec::len),
        Some(1),
        "self-healed search must return the intact message: {rows}"
    );
    assert_eq!(rows[0]["session_id"], "claude:heal");

    // The derived tables are rebuilt, the obsolete ones dropped, data preserved.
    let reader =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let (uv, has_trigram, has_vocab, obsolete_gone, messages_after): (i64, bool, bool, bool, i64) =
        reader
            .query_row(
                "select (select user_version from pragma_user_version),
                        exists(select 1 from sqlite_schema where name='messages_trigram'),
                        exists(select 1 from sqlite_schema where name='messages_trigram_vocab'),
                        not exists(select 1 from sqlite_schema where name='trigram_postings'),
                        (select count(*) from messages)",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
    assert_eq!(
        (uv, has_trigram, has_vocab, obsolete_gone, messages_after),
        (4, true, true, true, 1)
    );
}

#[test]
fn cli_message_search_covers_three_modes_by_three_fields_on_read_only_open() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    let executable = env!("CARGO_BIN_EXE_aise");
    let create = Command::new(executable)
        .args(["--config", config.to_str().unwrap(), "reindex"])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    let db_path = root.path().join("index.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"insert into sessions (
               id, provider, provider_session_id, preview_text, source_path,
               parse_version, discovery_source
           ) values ('claude:matrix', 'claude', 'matrix', '', '/matrix.jsonl', 'test', 'fixture');
           insert into messages (
               session_id, provider, seq, role, kind, tool_name, content
           ) values (
               'claude:matrix', 'claude', 0, 'tool', 'tool_call', 'exec_command',
               '{"args":{"cmd":"cargo test --workspace"},"kind":"tool_call","tool_name":"exec_command"}'
           );
           insert into messages (
               session_id, provider, seq, role, kind, tool_name, content
           ) values (
               'claude:matrix', 'claude', 1, 'tool', 'tool_call', 'read_file',
               '{"args":{"cmd":"open notes.md"},"kind":"tool_call","tool_name":"read_file"}'
           );"#,
    )
    .unwrap();
    drop(conn);

    let cases = [
        ("content", "exact", "cargo test"),
        ("content", "regex", r"cargo\s+test"),
        ("content", "fuzzy", "crgo tst"),
        ("tool-name", "exact", "exec"),
        ("tool-name", "regex", r"^exec_"),
        ("tool-name", "fuzzy", "excmd"),
        ("tool-argument", "exact", "cargo test"),
        ("tool-argument", "regex", r"cargo\s+test"),
        ("tool-argument", "fuzzy", "crgo tst"),
    ];
    for (field, mode, query) in cases {
        let mut args = vec![
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "messages",
            "search",
            query,
            "--field",
            field,
            "--kind",
            "tool-call",
            "--session-id",
            "claude:matrix",
            "--limit",
            "10",
            "--query-mode",
            if mode == "exact" { "literal" } else { mode },
            "--format",
            "json",
        ];
        if field == "tool-argument" {
            args.extend(["--argument-path", "/cmd"]);
        }
        let output = Command::new(executable).args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{field}/{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "{field}/{mode}: {error}: {}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
        assert_eq!(
            rows.as_array().map(Vec::len),
            Some(1),
            "{field}/{mode}: {rows}"
        );
        assert_eq!(rows[0]["session_id"], "claude:matrix", "{field}/{mode}");
        assert_eq!(rows[0]["seq"], 0, "{field}/{mode}");
    }

    let explained = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "messages",
            "search",
            "cargo test",
            "--receipt-level",
            "summary",
            "--limit",
            "1",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(explained.status.success());
    assert!(serde_json::from_slice::<serde_json::Value>(&explained.stdout).is_ok());
    let explain_stderr = String::from_utf8(explained.stderr).unwrap();
    assert!(explain_stderr.contains("[explain]"), "{explain_stderr}");
    assert!(!explain_stderr.contains("[origins]"), "{explain_stderr}");

    let full_receipt = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "messages",
            "search",
            "cargo test",
            "--receipt-level",
            "full",
            "--limit",
            "1",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(full_receipt.status.success());
    let full_stderr = String::from_utf8(full_receipt.stderr).unwrap();
    assert!(full_stderr.contains("[explain]"), "{full_stderr}");
    assert!(full_stderr.contains("[origins]"), "{full_stderr}");
    assert!(full_stderr.contains("\"receipt_level\""), "{full_stderr}");

    let first_page = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "messages",
            "search",
            "tool_call",
            "--limit",
            "1",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(first_page.status.success());
    let first_rows: serde_json::Value = serde_json::from_slice(&first_page.stdout).unwrap();
    assert_eq!(first_rows.as_array().map(Vec::len), Some(1));
    let first_stderr = String::from_utf8(first_page.stderr).unwrap();
    assert!(first_stderr.contains("--offset 1"), "{first_stderr}");
    assert!(first_stderr.contains("--all-results"), "{first_stderr}");

    let final_page = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "messages",
            "search",
            "tool_call",
            "--limit",
            "1",
            "--offset",
            "1",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(final_page.status.success());
    let final_rows: serde_json::Value = serde_json::from_slice(&final_page.stdout).unwrap();
    assert_eq!(final_rows.as_array().map(Vec::len), Some(1));
    let final_stderr = String::from_utf8(final_page.stderr).unwrap();
    assert!(!final_stderr.contains("--offset 2"), "{final_stderr}");

    let invalid = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "messages",
            "search",
            "[",
            "--query-mode",
            "regex",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid regex"));
}

#[cfg(unix)]
#[test]
fn killing_active_read_only_query_releases_connection_and_preserves_writer_progress() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    let executable = env!("CARGO_BIN_EXE_aise");
    let create = Command::new(executable)
        .args(["--config", config.to_str().unwrap(), "reindex"])
        .output()
        .unwrap();
    assert!(create.status.success());

    let db_path = root.path().join("index.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "create table query_load (value integer primary key);
         with recursive values_to_insert(value) as (
             values(1) union all select value + 1 from values_to_insert where value < 1000
         ) insert into query_load select value from values_to_insert;",
    )
    .unwrap();
    drop(conn);

    let mut child = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "db",
            "query",
            // This intentionally expensive query runs only against the temporary database above.
            // The test kills it after 100 ms to verify connection and writer recovery.
            "select count(*) from query_load a cross join query_load b cross join query_load c",
            "--timeout-ms",
            "0",
            "--format",
            "json",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    if let Some(status) = child.try_wait().unwrap() {
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        panic!("load query ended before kill with {status}: {stderr}");
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());

    let writer = rusqlite::Connection::open(&db_path).unwrap();
    let quick_check: String = writer
        .query_row("pragma quick_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(quick_check, "ok");
    writer
        .execute_batch(
            "begin immediate;
             insert into index_metadata(key, value) values ('post_kill_writer', 1)
             on conflict(key) do update set value=excluded.value;
             commit;",
        )
        .unwrap();
}

#[test]
fn one_two_four_and_eight_cli_readers_return_identical_json_pages() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    let executable = env!("CARGO_BIN_EXE_aise");
    assert!(Command::new(executable)
        .args(["--config", config.to_str().unwrap(), "reindex"])
        .output()
        .unwrap()
        .status
        .success());
    let conn = rusqlite::Connection::open(root.path().join("index.db")).unwrap();
    conn.execute_batch(
        "insert into sessions (
             id, provider, provider_session_id, preview_text, source_path,
             parse_version, discovery_source
         ) values ('claude:parallel', 'claude', 'parallel', '', '/parallel.jsonl', 'test', 'fixture');
         insert into messages (session_id, provider, seq, role, kind, content)
         values ('claude:parallel', 'claude', 0, 'user', 'conversation', 'parallel reader sentinel');",
    )
    .unwrap();
    drop(conn);

    let run = || {
        Command::new(executable)
            .args([
                "--config",
                config.to_str().unwrap(),
                "--index-refresh",
                "existing-only",
                "messages",
                "search",
                "parallel reader sentinel",
                "--limit",
                "1",
                "--format",
                "json",
            ])
            .output()
            .unwrap()
    };
    let expected = run();
    assert!(expected.status.success());

    for clients in [1, 2, 4, 8] {
        let outputs = std::thread::scope(|scope| {
            (0..clients)
                .map(|_| scope.spawn(run))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        for output in outputs {
            assert!(
                output.status.success(),
                "{clients} clients: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, expected.stdout, "{clients} clients");
        }
    }
}

#[test]
fn cli_resume_confirmation_reads_stdin_and_cancels_without_spawning_provider() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    let executable = env!("CARGO_BIN_EXE_aise");
    let create = Command::new(executable)
        .args(["--config", config.to_str().unwrap(), "reindex"])
        .output()
        .unwrap();
    assert!(create.status.success());
    let conn = rusqlite::Connection::open(root.path().join("index.db")).unwrap();
    conn.execute(
        "insert into sessions (
             id, provider, provider_session_id, cwd, preview_text, source_path,
             parse_version, discovery_source
         ) values ('codex:resume-test', 'codex', 'resume-test', ?1, '', '/resume.jsonl',
                   'test', 'fixture')",
        [root.path().to_string_lossy().as_ref()],
    )
    .unwrap();
    drop(conn);

    let mut child = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "resume",
            "codex:resume-test",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("codex resume resume-test"), "{stdout}");
    assert!(stdout.contains("resume cancelled"), "{stdout}");
}

#[test]
fn mcp_stdio_exits_cleanly_after_client_closes_stdin() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    let executable = env!("CARGO_BIN_EXE_aise");
    let mut child = Command::new(executable)
        .args(["--config", config.to_str().unwrap(), "mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#
    )
    .unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[1]["id"], 2);
}
