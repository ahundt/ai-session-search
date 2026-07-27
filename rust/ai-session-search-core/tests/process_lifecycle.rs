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

#[test]
fn inferred_skill_execution_uses_the_indexed_read_lifecycle_and_structured_report() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    ai_session_search::db::Db::open(&root.path().join("index.db")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "skills",
            "corrections",
            "--limit",
            "1",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "skill execution must emit one structured report: {error}: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    assert_eq!(report["requested_selector"]["name"], "corrections");
    assert_eq!(report["resolved_skill"]["name"], "corrections");
    assert_eq!(
        report["resolved_skill"]["selected_location"]["kind"],
        "embedded"
    );
    assert_eq!(report["output"]["capability"], "message-classification");
    assert_eq!(
        report["output"]["result"]["report"]["matches"],
        serde_json::json!([])
    );
    assert_eq!(report["output"]["result"]["receipt"]["name"], "corrections");
}

#[test]
fn explicit_skill_path_runs_its_adjacent_typed_capability() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    ai_session_search::db::Db::open(&root.path().join("index.db")).unwrap();
    let skill = root.path().join("my-review");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: my-review\ndescription: test message classification\nmetadata:\n  version: \
         2.1.0\n---\n\ninstructions\n",
    )
    .unwrap();
    fs::write(
        skill.join("capability.toml"),
        "schema_version = 1\nkind = \"message-classification\"\n\n\
         [[categories]]\nname = \"clobber\"\npatterns = ['''\\byou overwrote\\b''']\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "skills",
            skill.to_str().unwrap(),
            "--limit",
            "1",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["requested_selector"]["path"],
        skill.to_string_lossy().as_ref()
    );
    assert_eq!(report["resolved_skill"]["name"], "my-review");
    assert_eq!(report["output"]["capability"], "message-classification");
    assert_eq!(
        report["output"]["result"]["report"]["matches"],
        serde_json::json!([])
    );
    assert_eq!(report["output"]["result"]["receipt"]["name"], "my-review");
    assert_eq!(report["output"]["result"]["receipt"]["version"], "2.1.0");
    assert_eq!(
        report["output"]["result"]["receipt"]["sha256"]
            .as_str()
            .map(str::len),
        Some(64)
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
    let response: serde_json::Value =
        serde_json::from_slice(&search.stdout).unwrap_or_else(|error| {
            panic!(
                "self-healed search must emit a JSON response: {error}: {}",
                String::from_utf8_lossy(&search.stdout)
            )
        });
    assert_eq!(
        response["returned"], 1,
        "self-healed search must return the intact message: {response}"
    );
    assert_eq!(response["response_schema_version"], 1);
    assert_eq!(response["hits"][0]["session_id"], "claude:heal");

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
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "{field}/{mode}: {error}: {}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
        assert_eq!(response["returned"], 1, "{field}/{mode}: {response}");
        assert_eq!(response["response_schema_version"], 1, "{field}/{mode}");
        assert_eq!(
            response["hits"][0]["session_id"], "claude:matrix",
            "{field}/{mode}"
        );
        assert_eq!(response["hits"][0]["seq"], 0, "{field}/{mode}");
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
    let first_response: serde_json::Value = serde_json::from_slice(&first_page.stdout).unwrap();
    assert_eq!(first_response["returned"], 1);
    assert_eq!(first_response["next_offset"], 1);
    assert_eq!(first_response["pagination"]["consistency"], "per-call");
    let first_stderr = String::from_utf8(first_page.stderr).unwrap();
    assert!(first_stderr.contains("--offset 1"), "{first_stderr}");
    assert!(first_stderr.contains("--all-results"), "{first_stderr}");

    let omitted_limit = Command::new(executable)
        .args([
            "--config",
            config.to_str().unwrap(),
            "--index-refresh",
            "existing-only",
            "messages",
            "search",
            "tool_call",
            "--receipt-level",
            "full",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(omitted_limit.status.success());
    let omitted_response: serde_json::Value =
        serde_json::from_slice(&omitted_limit.stdout).unwrap();
    assert_eq!(omitted_response["returned"], 2);
    assert!(omitted_response["pagination"]["limit"].is_null());
    assert!(omitted_response["next_offset"].is_null());

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
    let final_response: serde_json::Value = serde_json::from_slice(&final_page.stdout).unwrap();
    assert_eq!(final_response["returned"], 1);
    assert!(final_response["next_offset"].is_null());
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
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"run_skill_capability","arguments":{{"skill":{{"name":"corrections"}},"limit":1}}}}}}"#
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
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[2]["id"], 3);
    assert_eq!(
        responses[2]["result"]["structuredContent"]["run"]["resolved_skill"]["name"],
        "corrections"
    );
}

/// Write a minimal standard-shaped skill directory under `root/skills/<name>/`.
fn write_test_skill(
    root: &std::path::Path,
    name: &str,
    frontmatter: &str,
    capability: Option<&str>,
) {
    let dir = root.join("skills").join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), frontmatter).unwrap();
    if let Some(capability) = capability {
        fs::write(dir.join("capability.toml"), capability).unwrap();
    }
}

fn skills_config(root: &std::path::Path) -> std::path::PathBuf {
    let config = write_disabled_provider_config(root);
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[skills]\nsearch_paths = [{:?}]\n",
        root.join("skills").display().to_string()
    ));
    fs::write(&config, text).unwrap();
    config
}

/// Dynamic capability help is ordinary successful help, not an operational failure.
#[test]
fn dynamic_skill_help_exits_zero_on_stdout_without_opening_configuration() {
    let output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args(["skills", "corrections", "--help"])
        .env(
            "AI_SESSION_SEARCH_CONFIG",
            "/path/that/must/not/be/opened/for-help.toml",
        )
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "help must exit zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: aise-skill-capability"));
    assert!(stdout.contains("--session-id"));
    assert!(
        output.stderr.is_empty(),
        "successful help belongs only on stdout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `aise skills validate` must EXIT NON-ZERO on an invalid skill.
///
/// A validator that exits 0 on bad input cannot gate anything: `aise skills validate x && deploy`
/// would deploy. The report belongs on stdout so `--format json` stays parseable, and only the
/// one-line verdict goes to stderr, so this pins the stream split as well as the code.
#[test]
fn skills_validate_exits_nonzero_and_keeps_the_report_on_stdout() {
    let root = tempfile::tempdir().unwrap();
    // One invalid SKILL.md and one invalid capability.toml prove diagnostics from both files are
    // reported in one run.
    write_test_skill(
        root.path(),
        "Bad_Name",
        "---\nname: other\n---\n\nbody\n",
        Some("schema_version = 1\nkind = \"message-classification\"\nweights = 3\n"),
    );
    let config = skills_config(root.path());

    let output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "validate",
            &root
                .path()
                .join("skills")
                .join("Bad_Name")
                .display()
                .to_string(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "an invalid skill must fail the process, or no script can gate on it"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout must stay parseable JSON: {err}\n{stdout}"));
    assert_eq!(report["valid"], serde_json::json!(false));
    assert_eq!(
        report["diagnostics"].as_array().map(Vec::len),
        Some(2),
        "every problem is reported at once, not one per run: {report:#}"
    );
    for diagnostic in report["diagnostics"].as_array().unwrap() {
        assert!(
            diagnostic["fix"]
                .as_str()
                .is_some_and(|fix| !fix.is_empty()),
            "each diagnostic names a fix, not only a refusal: {diagnostic:#}"
        );
    }
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("is not a valid skill") && stderr.contains("2 problems"),
        "the verdict goes to stderr so stdout stays machine-readable: {stderr}"
    );
}

/// A valid skill exits 0 and says so, `skills list` sees it beside the built-in skill, and
/// `skills team-rules` can then execute it.
#[test]
fn skills_list_shows_the_built_in_skill_beside_a_user_authored_skill() {
    let root = tempfile::tempdir().unwrap();
    write_test_skill(
        root.path(),
        "team-rules",
        "---\nname: team-rules\ndescription: Team correction categories.\nmetadata:\n  version: 0.2.0\n---\n\nbody\n",
        Some(concat!(
            "schema_version = 1\nkind = \"message-classification\"\n\n",
            "[[categories]]\nname = \"clobber\"\npatterns = [\"\\\\byou overwrote\\\\b\"]\n"
        )),
    );
    let config = skills_config(root.path());

    let valid = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "validate",
            &root
                .path()
                .join("skills")
                .join("team-rules")
                .display()
                .to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        valid.status.success(),
        "a well-formed skill must pass: {}",
        String::from_utf8_lossy(&valid.stderr)
    );
    assert!(
        String::from_utf8(valid.stdout).unwrap().contains("valid:"),
        "success must SAY it succeeded; an empty report reads as a broken command"
    );

    let listed = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "list",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let rows: serde_json::Value = serde_json::from_str(&String::from_utf8(listed.stdout).unwrap())
        .expect("skills list --format json is one JSON document");
    let names: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["corrections", "ai-session-search", "team-rules"],
        "the listing must include both built-in packages and every configured user skill"
    );
    let team = &rows.as_array().unwrap()[2];
    assert_eq!(team["ownership"], serde_json::json!("user"));
    assert_eq!(team["capability_status"], serde_json::json!("ok"));
    assert_eq!(team["package_version"], serde_json::json!("0.2.0"));
    assert_eq!(team["capability_sha256"].as_str().map(str::len), Some(64));

    let built_in_show = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "show",
            "ai-session-search",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        built_in_show.status.success(),
        "the harness-only skill promised by show --help must resolve: {}",
        String::from_utf8_lossy(&built_in_show.stderr)
    );
    let built_in: serde_json::Value =
        serde_json::from_str(&String::from_utf8(built_in_show.stdout).unwrap()).unwrap();
    assert_eq!(built_in["name"], serde_json::json!("ai-session-search"));
    assert_eq!(built_in["path"], serde_json::json!("(built in)"));
    assert_eq!(
        built_in["capability_status"],
        serde_json::json!("harness-only")
    );

    // And the selected catalog name reaches the typed capability execution path.
    let execution = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "team-rules",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        execution.status.success(),
        "`skills team-rules` must resolve and execute the discovered skill: {}",
        String::from_utf8_lossy(&execution.stderr)
    );
}

/// A scaffold must be usable the moment it is created, through the real CLI.
///
/// The chain is the point: `skills create` writes it, `skills validate` accepts it, `skills list`
/// discovers it, and `skills my-rules` executes it. Any break in that chain leaves a new
/// author holding a directory that looks right and does nothing.
#[test]
fn a_scaffolded_skill_is_discoverable_validatable_and_selectable() {
    let root = tempfile::tempdir().unwrap();
    let config = skills_config(root.path());
    let output_dir = root.path().join("skills");
    let skill_root = output_dir.join("my-rules");

    let dry = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "create",
            "my-rules",
            "--output-dir",
            &output_dir.display().to_string(),
            "--capability",
            "message-classification",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(dry.status.success());
    assert!(
        !skill_root.exists(),
        "--dry-run must write nothing, and this is the assertion that proves it"
    );

    let created = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "create",
            "my-rules",
            "--output-dir",
            &output_dir.display().to_string(),
            "--capability",
            "message-classification",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(skill_root.join("SKILL.md").is_file());
    assert!(skill_root.join("capability.toml").is_file());
    assert!(
        !fs::read_to_string(skill_root.join("SKILL.md"))
            .unwrap()
            .contains("ai-session-search-managed-skill"),
        "a scaffold is the caller's, so it must carry no managed marker"
    );

    let again = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "create",
            "my-rules",
            "--output-dir",
            &output_dir.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        !again.status.success(),
        "creating over an existing directory could overwrite the caller's own files"
    );

    let validated = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "validate",
            &skill_root.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        validated.status.success(),
        "what `create` writes must pass what `validate` checks: {}",
        String::from_utf8_lossy(&validated.stderr)
    );

    let execution = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "my-rules",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        execution.status.success(),
        "a freshly scaffolded capability skill must be executable: {}",
        String::from_utf8_lossy(&execution.stderr)
    );
}

/// `aise skills corrections --format json` emits one typed [`SkillRunReport`] envelope.
///
/// Selectors, resolution provenance, and capability output are separate public concepts. An empty
/// match set must still say which skill and capability actually ran.
///
/// [`SkillRunReport`]: ai_session_search::skill_run::SkillRunReport
#[test]
fn skill_run_json_names_selector_resolution_and_output_even_when_matches_are_empty() {
    let root = tempfile::tempdir().unwrap();
    let config = write_disabled_provider_config(root.path());
    ai_session_search::db::Db::open(&root.path().join("index.db")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args([
            "--config",
            &config.display().to_string(),
            "skills",
            "corrections",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("one JSON document: {err}\n{stdout}"));
    assert!(
        report.is_object(),
        "the report is an object, not the pre-1.0 bare match array: {report:#}"
    );
    assert_eq!(
        report
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["output", "requested_selector", "resolved_skill"]),
        "the top level is exactly the public SkillRunReport envelope"
    );
    assert_eq!(report["requested_selector"]["name"], "corrections");
    assert_eq!(report["resolved_skill"]["name"], "corrections");
    assert_eq!(
        report["resolved_skill"]["selected_location"]["kind"],
        "embedded"
    );
    assert_eq!(
        report["resolved_skill"]["execution_source"]["kind"],
        "embedded"
    );
    assert_eq!(report["output"]["capability"], "message-classification");
    assert_eq!(
        report["output"]["result"]["report"]["matches"],
        serde_json::json!([])
    );

    let policies = report["output"]["result"]["report"]["policies"]
        .as_array()
        .unwrap();
    assert_eq!(
        policies.len(),
        1,
        "the built-in skill evaluates exactly its embedded capability: {report:#}"
    );
    assert_eq!(policies[0]["name"], serde_json::json!("corrections"));
    assert_eq!(
        policies[0]["sha256"].as_str().map(str::len),
        Some(64),
        "the digest is what makes a run reproducible; a name and version alone are not"
    );
}
