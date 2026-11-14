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
