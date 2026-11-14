// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use ai_session_search::db::Db;
use ai_session_search::models::{FileEdit, Message, ParsedSession, Provider, Role, SessionRecord};
use serde_json::{json, Value};

mod support;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("index.db")).unwrap();
    for n in 0..3 {
        let id = format!("claude:cli-{n}");
        let mut record = SessionRecord::new(
            &id,
            Provider::Claude,
            format!("cli-{n}"),
            format!("/synthetic/{n}/session.jsonl"),
            "fixture-v1",
            "test",
        );
        record.cwd = Some(format!("/synthetic/{n}"));
        record.title = Some("needle".into());
        let mut parsed = ParsedSession::new(
            record,
            vec![
                Message::new(1, Role::User, "you forgot needle"),
                Message::new(2, Role::Assistant, "needle"),
            ],
        );
        parsed.messages[0].provenance.authorship =
            ai_session_search::models::MessageAuthorship::Human;
        parsed.messages[0].provenance.record_relation =
            ai_session_search::models::MessageRecordRelation::Original;
        for version in 0..62 {
            parsed.file_edits.push(serde_json::from_value::<FileEdit>(json!({
                "seq": version, "file_path": "/synthetic/app.rs", "file_name": "app.rs", "tool": "Write",
                "ts": "2026-06-02T10:00:00Z", "new_content": format!("version {version}\n"), "edits": []
            })).unwrap());
        }
        db.upsert_session(&parsed, 0, 0).unwrap();
    }
    dir
}

fn output(dir: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let out = support::aise(dir.path())
        .args(["--index-refresh", "existing-only"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn rows(dir: &tempfile::TempDir, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.extend(["--format", "json"]);
    serde_json::from_slice(&output(dir, &args).stdout).unwrap()
}

#[test]
fn session_cli_pages_reproduce_the_whole_order() {
    let dir = fixture();
    for command in [vec!["list"], vec!["search", "needle"]] {
        let mut args = command.clone();
        args.extend(["--limit", "0"]);
        let all = rows(&dir, &args);
        let all = all.as_array().unwrap();
        assert_eq!(all.len(), 3);
        for offset in 0..=all.len() {
            let offset_text = offset.to_string();
            let mut args = command.clone();
            args.extend(["--limit", "1", "--offset", &offset_text]);
            let page = rows(&dir, &args);
            assert_eq!(
                page.as_array().unwrap(),
                &all[offset..(offset + 1).min(all.len())]
            );
        }
    }
}

#[test]
fn file_history_default_returns_every_version_and_explicit_pages_still_work() {
    let dir = fixture();
    let all = rows(
        &dir,
        &["files", "history", "app.rs", "--session-id", "claude:cli-0"],
    );
    assert_eq!(all.as_array().unwrap().len(), 62);
    let page = rows(
        &dir,
        &[
            "files",
            "history",
            "app.rs",
            "--session-id",
            "claude:cli-0",
            "--limit",
            "1",
            "--offset",
            "61",
        ],
    );
    assert_eq!(page.as_array().unwrap(), &all.as_array().unwrap()[61..]);
}

#[test]
fn file_exclusions_and_dates_apply_before_paging() {
    let dir = fixture();
    for command in ["search", "cross-ref", "history"] {
        let got = rows(
            &dir,
            &[
                "files",
                command,
                "app.rs",
                "--exclude-path",
                "/synthetic/0",
                "--exclude-session",
                "claude:cli-1",
                "--since",
                "2026-06-01",
                "--until",
                "2026-06-03",
                "--limit",
                "1",
            ],
        );
        assert_eq!(got.as_array().unwrap().len(), 1);
        if command != "search" {
            assert_eq!(got[0]["session_id"], "claude:cli-2");
        }
        let empty = rows(&dir, &["files", command, "app.rs", "--since", "2026-06-04"]);
        assert!(empty.as_array().unwrap().is_empty());
    }
    let extracted = output(
        &dir,
        &[
            "files",
            "extract",
            "app.rs",
            "--exclude-path",
            "/synthetic/0",
            "--exclude-session",
            "claude:cli-1",
        ],
    );
    assert_eq!(extracted.stdout, b"version 61\n");
}

#[test]
fn analytics_exclusions_remove_the_candidate_population() {
    let dir = fixture();
    for command in [
        vec!["skills", "corrections"],
        vec!["repeats", "needle"],
        vec!["stats"],
    ] {
        let unfiltered = rows(&dir, &command);
        if command[0] == "skills" {
            assert_eq!(
                unfiltered["output"]["result"]["report"]["matches"]
                    .as_array()
                    .unwrap()
                    .len(),
                3
            );
        }
        if command[0] == "repeats" {
            assert!(!unfiltered.as_array().unwrap().is_empty());
        }
        if command[0] == "stats" {
            assert_eq!(
                unfiltered[0]["count"].as_i64().unwrap() + unfiltered[1]["count"].as_i64().unwrap(),
                6
            );
        }
        let mut session_args = command.clone();
        session_args.extend([
            "--exclude-session",
            "claude:cli-0",
            "--exclude-session",
            "claude:cli-1",
        ]);
        if command[0] == "skills" {
            session_args.extend(["--limit", "1"]);
            let scoped = rows(&dir, &session_args);
            assert_eq!(
                scoped["output"]["result"]["report"]["matches"][0]["session_id"],
                "claude:cli-2"
            );
        } else if command[0] == "stats" {
            let scoped = rows(&dir, &session_args);
            assert_eq!(
                scoped[0]["count"].as_i64().unwrap() + scoped[1]["count"].as_i64().unwrap(),
                2
            );
        } else {
            let scoped = rows(&dir, &session_args);
            assert!(scoped.as_array().unwrap().is_empty());
        }
        let mut args = command;
        args.extend(["--exclude-path", "/synthetic", "--format", "json"]);
        let value: Value = serde_json::from_slice(&output(&dir, &args).stdout).unwrap();
        if args[0] == "repeats" {
            assert_eq!(value, json!([]));
        } else if args[0] == "skills" {
            assert_eq!(value["output"]["result"]["report"]["matches"], json!([]));
        } else {
            assert_eq!(value, json!([]));
        }
    }
}

#[test]
fn deprecated_cli_spellings_keep_the_replacement_output() {
    let dir = fixture();
    let aliases = [
        (vec!["planning"], vec!["slash-commands"]),
        (
            vec!["repeats", "needle", "--limit", "2"],
            vec!["repeats", "needle", "--max-candidates", "2"],
        ),
        (
            vec!["repeats", "needle", "--regex"],
            vec!["repeats", "needle", "--query-mode", "regex"],
        ),
        (
            vec!["messages", "timeline", "claude:cli-0", "--regex", "needle"],
            vec![
                "messages",
                "timeline",
                "claude:cli-0",
                "--grep",
                "needle",
                "--query-mode",
                "regex",
            ],
        ),
        (
            vec!["files", "cross-ref", "--pattern", "app.rs"],
            vec!["files", "cross-ref", "app.rs"],
        ),
        (
            vec![
                "show",
                "claude:cli-0",
                "--summary",
                "--summary-items",
                "2",
                "--preview-chars",
                "40",
            ],
            vec![
                "messages",
                "evidence",
                "claude:cli-0",
                "--summary-items",
                "2",
                "--preview-chars",
                "40",
            ],
        ),
        (
            vec![
                "messages",
                "search",
                "needle",
                "--include",
                "raw_provider_metadata",
            ],
            vec!["messages", "search", "needle", "--include", "raw-metadata"],
        ),
        (
            vec!["skills", "corrections", "--limit", "0"],
            vec!["skills", "corrections", "--all-results"],
        ),
    ];
    for (mut old, mut replacement) in aliases {
        old.extend(["--format", "json"]);
        replacement.extend(["--format", "json"]);
        let old_output = output(&dir, &old);
        assert_eq!(
            old_output.stdout,
            output(&dir, &replacement).stdout,
            "{old:?}"
        );
        assert!(
            String::from_utf8_lossy(&old_output.stderr).contains("deprecated"),
            "{old:?}"
        );
    }
}

#[test]
fn cli_read_and_file_output_matches_published_layouts() {
    let dir = fixture();
    let commands = [
        vec!["messages", "get", "claude:cli-0"],
        vec![
            "messages",
            "get",
            "claude:cli-0",
            "--seq",
            "1",
            "--context",
            "1",
        ],
        vec!["messages", "timeline", "claude:cli-0"],
        vec![
            "messages",
            "timeline",
            "claude:cli-0",
            "--grep",
            "needle",
            "--order",
            "newest",
            "--limit",
            "1",
        ],
        vec!["files", "search", "app.rs"],
        vec!["files", "cross-ref", "app.rs"],
        vec![
            "files",
            "history",
            "app.rs",
            "--session-id",
            "claude:cli-0",
            "--limit",
            "2",
        ],
    ];
    let mut captured = serde_json::Map::new();
    for command in commands {
        for format in ["json", "jsonl", "csv", "plain", "table"] {
            let mut args = command.clone();
            args.extend(["--format", format]);
            let mut cmd = support::aise(dir.path());
            cmd.env("TZ", "UTC")
                .args(["--index-refresh", "existing-only"])
                .args(&args);
            let out = if let Some(program) = std::env::var_os("CLI_SURFACE_BASELINE") {
                let mut baseline = std::process::Command::new(program);
                baseline
                    .env_clear()
                    .envs(cmd.get_envs().filter_map(|(k, v)| v.map(|v| (k, v))))
                    .args(cmd.get_args());
                baseline.output().unwrap()
            } else {
                cmd.output().unwrap()
            };
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            captured.insert(
                args.join(" "),
                Value::String(String::from_utf8(out.stdout).unwrap()),
            );
        }
    }
    let value = Value::Object(captured);
    if let Some(path) = std::env::var_os("CLI_SURFACE_RECORD") {
        std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    } else {
        let golden: Value =
            serde_json::from_str(include_str!("fixtures/cli_surface_output.json")).unwrap();
        assert_eq!(value, golden);
    }
}
