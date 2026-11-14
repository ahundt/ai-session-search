# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Non-Claude parser-health inventory parity on an isolated synthetic index."""

import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest

import ai_session_search as aise


@pytest.mark.integration
def test_codex_unknown_record_inventory_matches_python_cli_and_mcp(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    cli = shutil.which("aise")
    assert cli is not None, "run maturin develop before pytest"
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    (sessions / "rollout-cafe0000-cafe-cafe-cafe-000000000001.jsonl").write_text(
        '{"type":"session_meta","payload":{"id":"synthetic-health"}}\n'
        '{"type":"future-kind","payload":{"content":"synthetic payload"}}\n'
        '{"type":"response_item","payload":{"type":"future-payload"}}\n'
    )
    config = tmp_path / "config.toml"
    config.write_text("\n".join(
        f"[providers.{provider}]\nenabled = {'true' if provider == 'codex' else 'false'}\n"
        f"paths = {json.dumps([str(sessions)] if provider == 'codex' else [])}\n"
        for provider in (
            "claude", "claude-desktop", "codex", "cursor", "antigravity",
            "pi", "prime-agent", "aistudio", "gemini-cli",
        )
    ))
    monkeypatch.setenv("AI_SESSION_SEARCH_CONFIG", str(config))
    monkeypatch.setenv("AI_SESSION_SEARCH_DATABASE", str(tmp_path / "index.db"))

    def run(*args: str, payload: str | None = None) -> str:
        return subprocess.run(
            [cli, *args], input=payload, text=True, capture_output=True,
            check=True, timeout=30, env=os.environ.copy(),
        ).stdout

    run("reindex")
    expected = [
        {"provider": "codex", "record_type": "future-kind", "records": 1, "sessions": 1},
        {"provider": "codex", "record_type": "response_item.future-payload", "records": 1, "sessions": 1},
    ]
    python = aise.SessionSearch(tmp_path / "index.db").doctor().to_dict()
    doctor = json.loads(run("--index-refresh", "existing-only", "doctor", "--format", "json"))
    assert python["index_status"]["parser_health"]["unrecognized_record_types"] == expected
    assert doctor["parser_health"]["unrecognized_record_types"] == expected

    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "synthetic-health", "version": "1"},
        }},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "get_index_status", "arguments": {},
        }},
    ]
    output = run("mcp", "serve", payload="".join(json.dumps(row) + "\n" for row in requests))
    response = next(json.loads(line) for line in output.splitlines() if json.loads(line).get("id") == 2)
    assert "error" not in response, response
    result = response["result"]
    assert not result.get("isError"), result
    status = result.get("structuredContent") or json.loads(result["content"][0]["text"])
    assert status["parser_health"]["unrecognized_record_types"] == expected

    # New unknown records must refresh health even when no searchable row is appended.
    transcript = next(sessions.glob("*.jsonl"))
    with transcript.open("a") as stream:
        stream.write('{"type":"future-kind"}\n')
    expected[0]["records"] = 2
    for _ in range(2):
        run("reindex")
        refreshed = json.loads(run("doctor", "--format", "json"))
        assert refreshed["parser_health"]["unrecognized_record_types"] == expected
