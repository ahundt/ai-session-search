// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::io::Write;
use std::process::Stdio;

use ai_session_search::config::Config;
use ai_session_search::mcp_schema_budget::{
    codex_measured_bytes, evaluate, schema_depth, Status, EMITTED_ARTIFACT_CEILINGS,
};
use serde_json::{json, Value};

mod support;

fn catalogue(client: &str, version: &str) -> Value {
    let root = tempfile::tempdir().unwrap();
    let mut process = support::aise(root.path())
        .args(["--index-refresh", "existing-only", "mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25", "capabilities":{},
            "clientInfo":{"name":client,"version":version}
        }}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ] {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("stdio must contain only JSON-RPC"))
        .collect();
    assert!(
        responses.iter().any(
            |response| response["id"] == 1 && response["result"]["protocolVersion"].is_string()
        ),
        "initialize failed: {responses:?}"
    );
    responses
        .into_iter()
        .find(|response| response["id"] == 2)
        .expect("stdio tools/list response")["result"]["tools"]
        .clone()
}

#[test]
fn stdio_artifacts_obey_caps_and_global_policy_does_not_select_by_client_identity() {
    // No paths, hostnames, elapsed times or machine-dependent metadata occur in this artifact.
    // Compare real transports, then measure their parsed JSON, as each client does.
    let codex = catalogue("codex", "0.146.0");
    let claude = catalogue("claude-code", "2.1.220");
    let unknown = catalogue("synthetic-unknown-client", "999.0.0");
    assert_eq!(codex, claude, "clientInfo unexpectedly selected a different catalogue; document any deliberate connection-local policy in docs/development/mcp-client-limits-and-measured-evidence.md");
    assert_eq!(
        codex, unknown,
        "unknown client must retain the global configured policy"
    );
    let tools = codex.as_array().expect("tools/list array");
    assert_eq!(
        tools.len(),
        EMITTED_ARTIFACT_CEILINGS.len(),
        "a short catalogue must not pass by omitting tools"
    );
    let config = Config::default();
    for finding in evaluate(tools, &config.mcp.client_limits, false) {
        assert_ne!(finding.status, Status::Fail, "{}", finding.diagnostic());
    }
    for (name, bytes, depth) in EMITTED_ARTIFACT_CEILINGS {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .expect("pinned tool served over stdio");
        for (artifact, measured, pinned, unit) in [
            (
                "inputSchema",
                codex_measured_bytes(&tool["inputSchema"]),
                bytes,
                "bytes",
            ),
            (
                "outputSchema depth",
                schema_depth(&tool["outputSchema"]),
                depth,
                "levels",
            ),
        ] {
            assert_eq!(
                measured,
                pinned,
                "{}",
                ai_session_search::mcp_schema_budget::describe_pin_change(
                    name, artifact, measured, pinned, unit
                )
            );
        }
    }
}

#[test]
fn schema_budget_ledger_exposes_evidence_and_actionable_override_breaches() {
    let root = tempfile::tempdir().unwrap();
    let ledger = support::aise(root.path())
        .args(["mcp", "schema-budget", "--ledger"])
        .output()
        .unwrap();
    assert!(
        ledger.status.success(),
        "{}",
        String::from_utf8_lossy(&ledger.stderr)
    );
    let ledger: Value = serde_json::from_slice(&ledger.stdout).unwrap();
    assert!(!ledger["limit_evidence"].as_array().unwrap().is_empty());
    assert!(ledger["evidence_notices"].is_array());
    assert!(ledger["cap_selection"].as_str().unwrap().contains("global"));
    let failure = support::aise(root.path())
        .env(
            "AI_SESSION_SEARCH_CLIENT_LIMIT_CODEX_INPUT_SCHEMA_BYTES",
            "1",
        )
        .args(["mcp", "schema-budget"])
        .output()
        .unwrap();
    assert!(!failure.status.success());
    let text = String::from_utf8(failure.stdout).unwrap();
    for required in [
        "codex-input-schema-bytes",
        "search_messages",
        "excess",
        "bytes",
        "rust-v0.146.0",
        "2026-08-04",
        "aise mcp schema-budget --ledger",
        "input_field_costs",
        "1.",
        "2.",
        "3.",
        "https://github.com/openai/codex/",
        "docs/development/mcp-client-limits-and-measured-evidence.md",
    ] {
        assert!(text.contains(required), "missing {required}: {text}");
    }
}
