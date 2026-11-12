// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! The failure-finding tools read only what people typed, at the entry points people run.
//!
//! Claude Code writes records that carry a `message` without being one: `api-request-blob`
//! request copies repeat every turn, mid-turn wrappers and compaction summaries in user role.
//! Indexed as turns, they made `aise skills corrections` report harness text as corrections and
//! `aise repeats` report the harness's own sentences as what the user keeps saying. A prompt
//! typed while the agent worked can exist only in a `queued_command` attachment, which was not
//! indexed at all. The fixture holds each of these beside the real turns.
//!
//! Queued input (typed while the agent worked) is read when the transcript records that a person
//! sent it, left out and counted when it records no sender, and never read when it records a
//! harness notification.

mod support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/claude_harness_records.jsonl");
const SESSION: &str = "cafe0002-cafe-cafe-cafe-000000000001";
/// The person's correction among the real turns.
const CORRECTION: &str = "you missed the second directory";
/// A prompt that exists only in a `queued_command` attachment.
const QUEUED_ONLY: &str = "also rename the output folder";
/// A queued prompt whose attachment names a human origin.
const QUEUED_HUMAN: &str = "you forgot to check the tests";
/// A queued prompt with no attachment, so no recorded sender.
const QUEUED_UNATTRIBUTED: &str = "you forgot to check the second directory";
/// Text the harness wrote, or that only request copies and unknown records hold.
const NOT_TYPED: [&str; 9] = [
    "Background job report",
    "while you were working",
    "harness shows",
    "being continued",
    "Stop hook feedback",
    "contributing guide",
    "README.md",
    "coding agent",
    "future record",
];

struct Indexed {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Indexed {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let project = projects.join("-tmp-harness-proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{SESSION}.jsonl")), FIXTURE).unwrap();
        let config = dir.path().join("config.toml");
        let mut text = format!(
            "[skills]\nsearch_paths = []\n[providers.claude]\nenabled = true\npaths = [{:?}]\n",
            projects.display().to_string()
        );
        for provider in [
            "claude-desktop",
            "codex",
            "cursor",
            "antigravity",
            "pi",
            "prime-agent",
            "aistudio",
            "gemini-cli",
        ] {
            text.push_str(&format!(
                "[providers.{provider}]\nenabled = false\npaths = []\n"
            ));
        }
        std::fs::write(&config, text).unwrap();
        let indexed = Self { dir, config };
        let output = indexed.aise().arg("reindex").output().unwrap();
        assert!(output.status.success(), "{output:?}");
        indexed
    }

    fn aise(&self) -> Command {
        let mut command = support::aise(self.dir.path());
        command.env("AI_SESSION_SEARCH_CONFIG", &self.config);
        command
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.aise().args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
}

fn assert_typed_only(texts: &[String], surface: &str) {
    for text in texts {
        for needle in NOT_TYPED {
            assert!(
                !text.contains(needle),
                "{surface} returned harness or request-copy text {needle:?}: {texts:#?}"
            );
        }
    }
}

fn strings(values: impl IntoIterator<Item = Value>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn cli_corrections_repeats_and_doctor_read_only_typed_text() {
    let indexed = Indexed::new();

    let corrections = |extra: &[&str]| {
        let mut args = vec!["skills", "corrections", "--format", "json"];
        args.extend_from_slice(extra);
        let report = indexed.json(&args)["output"]["result"]["report"].clone();
        let matched = strings(
            report["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| hit["content"].clone()),
        );
        (matched, report)
    };
    let (matched, report) = corrections(&[]);
    assert_eq!(matched, [CORRECTION, QUEUED_HUMAN], "{report:#}");
    assert_eq!(report["unattributed_input_count"], json!(1), "{report:#}");
    assert_eq!(report["unattributed_input_included"], json!(false));
    let (matched, report) = corrections(&["--include-unattributed-input"]);
    assert_eq!(
        matched,
        [CORRECTION, QUEUED_UNATTRIBUTED, QUEUED_HUMAN],
        "{report:#}"
    );
    assert_eq!(report["unattributed_input_count"], json!(1), "{report:#}");
    assert_eq!(report["unattributed_input_included"], json!(true));
    let table = indexed
        .aise()
        .args(["skills", "corrections"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&table.stdout).contains(
            "1 queued message left out because its transcript does not record who sent it; \
             --include-unattributed-input reads it"
        ),
        "{table:?}"
    );

    let repeats = |extra: &[&str]| {
        let mut args = vec!["repeats", "--format", "json"];
        args.extend_from_slice(extra);
        let output = indexed.aise().args(&args).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let groups: Value = serde_json::from_slice(&output.stdout).unwrap();
        let groups = groups.as_array().unwrap();
        let mut texts = strings(groups.iter().map(|group| group["repeat"].clone()));
        texts.extend(strings(groups.iter().flat_map(|group| {
            group["examples"]
                .as_array()
                .unwrap()
                .iter()
                .map(|example| example["preview"].clone())
                .collect::<Vec<_>>()
        })));
        assert_typed_only(&texts, "aise repeats");
        (texts, String::from_utf8_lossy(&output.stderr).into_owned())
    };
    let (texts, stderr) = repeats(&[]);
    assert!(
        !texts.iter().any(|text| text == QUEUED_UNATTRIBUTED),
        "{texts:#?}"
    );
    assert!(stderr.contains("1 queued message left out"), "{stderr}");
    let (texts, stderr) = repeats(&["--include-unattributed-input"]);
    assert!(
        texts.iter().any(|text| text == QUEUED_UNATTRIBUTED),
        "{texts:#?}"
    );
    assert!(!stderr.contains("left out"), "{stderr}");

    // `--report` carries the same groups with the left-out count inside the JSON, while plain
    // `--format json` keeps printing the bare array current readers parse.
    let bare = indexed.json(&["repeats", "--format", "json"]);
    let report = indexed.json(&["repeats", "--format", "json", "--report"]);
    assert_eq!(report["groups"], bare, "{report:#}");
    assert_eq!(report["unattributed_input_count"], json!(1), "{report:#}");
    assert_eq!(report["unattributed_input_included"], json!(false));
    let line = indexed
        .aise()
        .args(["repeats", "--format", "jsonl", "--report"])
        .output()
        .unwrap();
    assert!(line.status.success(), "{line:?}");
    let line: Value = serde_json::from_slice(&line.stdout).unwrap();
    assert_eq!(line, report);
    let table = indexed
        .aise()
        .args(["repeats", "--report"])
        .output()
        .unwrap();
    assert!(!table.status.success());
    assert!(
        String::from_utf8_lossy(&table.stderr)
            .contains("--report requires --format json or --format jsonl"),
        "{table:?}"
    );

    let queued = indexed.json(&[
        "messages",
        "search",
        QUEUED_ONLY,
        "--role",
        "user",
        "--format",
        "json",
    ]);
    assert_eq!(queued["results"].as_array().unwrap().len(), 1, "{queued:#}");

    // The Stop-hook evidence stays reachable when asked for.
    let notices = indexed.json(&[
        "messages",
        "search",
        "CANNOT STOP",
        "--kinds",
        "harness-notice",
        "--all-results",
        "--format",
        "json",
    ]);
    assert_eq!(
        notices["results"].as_array().unwrap().len(),
        1,
        "{notices:#}"
    );

    let doctor = indexed.aise().arg("doctor").output().unwrap();
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(stdout.contains("Parse warnings indexed: 1"), "{stdout}");
    assert!(!stdout.contains("attachment/instructions"), "{stdout}");
    assert!(
        stdout.contains(
            r#"Unrecognized record type, not indexed: claude "future-record" (1 records in 1 sessions)"#
        ),
        "{stdout}"
    );
}

#[test]
fn mcp_corrections_search_and_status_read_only_typed_text() {
    let indexed = Indexed::new();
    let calls = [
        (
            "run_skill_capability",
            json!({"skill": {"name": "corrections"}, "index_refresh": "existing-only"}),
        ),
        (
            "run_skill_capability",
            json!({"skill": {"name": "corrections"}, "include_unattributed_input": true,
                   "index_refresh": "existing-only"}),
        ),
        (
            "search_messages",
            json!({"query": QUEUED_ONLY, "role": "user", "index_refresh": "existing-only"}),
        ),
        (
            "search_messages",
            json!({"query": "while you were working", "index_refresh": "existing-only"}),
        ),
        (
            "search_messages",
            json!({"query": "you", "role": "user", "index_refresh": "existing-only"}),
        ),
        (
            "get_index_status",
            json!({"index_refresh": "existing-only"}),
        ),
    ];
    let mut child = indexed
        .aise()
        .args(["mcp", "serve"])
        .current_dir(indexed.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = vec![
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
               "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                          "clientInfo": {"name": "harness-records", "version": "1"}}})
        .to_string(),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
    ];
    for (id, (tool, arguments)) in calls.iter().enumerate() {
        lines.push(
            json!({"jsonrpc": "2.0", "id": id + 1, "method": "tools/call",
                   "params": {"name": tool, "arguments": arguments}})
            .to_string(),
        );
    }
    child
        .stdin
        .take()
        .unwrap()
        .write_all((lines.join("\n") + "\n").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.stderr.is_empty(),
        "`aise mcp serve` prints nothing to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut results = std::collections::HashMap::new();
    for line in String::from_utf8(output.stdout).unwrap().lines() {
        let response: Value = serde_json::from_str(line).unwrap();
        if let Some(id) = response["id"].as_u64().filter(|id| *id > 0) {
            let result = &response["result"];
            assert_ne!(result["isError"], json!(true), "{response:#}");
            results.insert(id, result["structuredContent"].clone());
        }
    }

    // MCP returns each hit's text as a bounded field view.
    let text = |hit: &Value| hit["presentation"]["field_view"]["text"].clone();
    let report = |id: u64| results[&id]["run"]["output"]["result"]["report"].clone();
    let matched = |id: u64| {
        strings(
            report(id)["matches"]
                .as_array()
                .unwrap_or_else(|| panic!("{:#}", results[&id]))
                .iter()
                .map(text),
        )
    };
    assert_eq!(matched(1), [CORRECTION, QUEUED_HUMAN], "{:#}", results[&1]);
    assert_eq!(report(1)["unattributed_input_count"], json!(1));
    assert_eq!(report(1)["unattributed_input_included"], json!(false));
    assert_eq!(
        matched(2),
        [CORRECTION, QUEUED_UNATTRIBUTED, QUEUED_HUMAN],
        "{:#}",
        results[&2]
    );
    assert_eq!(report(2)["unattributed_input_included"], json!(true));

    let hits = |id: u64| results[&id]["results"].as_array().unwrap().clone();
    assert_eq!(hits(3).len(), 1, "{:#}", results[&3]);
    assert!(hits(4).is_empty(), "{:#}", results[&4]);
    let user_texts = strings(hits(5).iter().map(text));
    assert!(!user_texts.is_empty(), "{:#}", results[&5]);
    assert_typed_only(&user_texts, "MCP search_messages role=user");

    assert_eq!(results[&6]["parser_health"]["parse_warnings"], json!(1));
    let unrecognized = &results[&6]["parser_health"]["unrecognized_record_types"];
    assert_eq!(
        unrecognized,
        &json!([{"provider": "claude", "record_type": "future-record",
                 "records": 1, "sessions": 1}]),
        "{:#}",
        results[&6]
    );
}
