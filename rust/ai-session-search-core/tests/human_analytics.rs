// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

mod support;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::process::Stdio;

use serde_json::{json, Value};

fn fixture(root: &std::path::Path) {
    let path = root.join("index.db");
    drop(ai_session_search::db::Db::open(&path).unwrap());
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(include_str!("fixtures/human_analytics.sql"))
        .unwrap();
}

fn expected(include: bool) -> BTreeSet<i64> {
    if include {
        [0, 1, 2, 3, 4, 9, 10].into()
    } else {
        [0, 1, 2, 3, 9, 10].into()
    }
}

fn assert_report(report: &Value, include: bool, seq_key: &str) {
    assert_eq!(report["unattributed_input_count"], 1, "{report:#}");
    assert_eq!(report["unattributed_input_included"], include);
    let matches = report["matches"].as_array().unwrap();
    assert_eq!(
        matches
            .iter()
            .map(|m| m.get("message_ref").unwrap_or(m)[seq_key].as_i64().unwrap())
            .collect::<BTreeSet<_>>(),
        expected(include),
        "{report:#}"
    );
    assert_eq!(matches.len(), expected(include).len());
    for m in matches {
        let identity = m.get("message_ref").unwrap_or(m);
        assert_eq!(identity["session_id"], "claude:human-analytics");
        if identity[seq_key] == 9 {
            if let Some(classification) = m.get("classification") {
                assert_eq!(classification["field_start_char"], 12);
                assert_eq!(classification["field_end_char_exclusive"], 22);
            } else {
                assert_eq!(m["match_start_char"], 12);
                assert_eq!(m["match_end_char_exclusive"], 22);
            }
        }
    }
}

#[test]
fn cli_analytics_read_original_human_input_independent_of_stored_role() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    for include in [false, true] {
        let mut command = support::aise(dir.path());
        command.args([
            "--index-refresh",
            "existing-only",
            "skills",
            "corrections",
            "--all-results",
            "--format",
            "json",
        ]);
        if include {
            command.arg("--include-unattributed-input");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_report(
            &result["output"]["result"]["report"],
            include,
            "message_seq",
        );

        for role in [None, Some("user")] {
            let mut command = support::aise(dir.path());
            command.args([
                "--index-refresh",
                "existing-only",
                "repeats",
                "--format",
                "json",
                "--min-matches",
                "1",
                "--phrase-min-words",
                "1",
                "--phrase-max-words",
                "3",
                "--max-groups",
                "0",
                "--max-examples-per-group",
                "0",
            ]);
            if include {
                command.arg("--include-unattributed-input");
            }
            if let Some(role) = role {
                command.args(["--role", role]);
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["unattributed_input_count"], 1, "{report:#}");
            assert_eq!(report["unattributed_input_included"], include);
            let examples = report["groups"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["examples"].as_array().unwrap());
            let mut read = BTreeSet::new();
            for example in examples {
                assert_eq!(example["session_id"], "claude:human-analytics");
                read.insert(example["seq"].as_i64().unwrap());
                if example["seq"] == 9 {
                    assert_eq!(example["preview"], "you forgot mixed");
                }
            }
            assert_eq!(read, expected(include));
        }
    }
}

#[test]
fn mcp_classification_reads_original_human_input_independent_of_stored_role() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let mut child = support::aise(dir.path())
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut requests = vec![
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"synthetic","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    ];
    for include in [false, true] {
        requests.push(json!({"jsonrpc":"2.0","id":if include {2} else {1},"method":"tools/call","params":{"name":"run_skill_capability","arguments":{"skill":{"name":"corrections"},"include_unattributed_input":include,"limit":50,"index_refresh":"existing-only"}}}));
    }
    let input = requests
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut seen = BTreeSet::new();
    for line in String::from_utf8(output.stdout).unwrap().lines() {
        let response: Value = serde_json::from_str(line).unwrap();
        if let Some(id @ (1 | 2)) = response["id"].as_u64() {
            assert_ne!(response["result"]["isError"], true, "{response:#}");
            assert_report(
                &response["result"]["structuredContent"]["run"]["output"]["result"]["report"],
                id == 2,
                "message_seq",
            );
            seen.insert(id);
        }
    }
    assert_eq!(seen, [1, 2].into());
}
