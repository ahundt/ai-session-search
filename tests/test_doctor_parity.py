# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
"""Health adapters preserve the shared report's fields."""

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any

import ai_session_search as aise


def test_doctor_python_and_mcp_only_omit_cli_config_file_fields(tmp_path: Path) -> None:
    config = tmp_path / "config.toml"
    sources = tmp_path / "claude"
    sources.mkdir()
    for name in ["first", "second"]:
        (sources / f"{name}.jsonl").write_text(json.dumps({
            "type": "user", "sessionId": "synthetic-collision",
            "message": {"role": "user", "content": "synthetic input"},
        }) + "\n")
    config.write_text("[search]\nunknown_synthetic_key=true\n"
                      "[providers.claude]\nenabled=true\npaths=[" + json.dumps(str(sources)) + "]\n")
    database = tmp_path / "index.db"
    with aise.SessionSearch(database, config_path=config) as search:
        search.reindex()
        python = search.doctor().to_dict()
        assert search.doctor().unknown_config_keys[0].path == "search.unknown_synthetic_key"
        assert search.doctor().index_status.unindexed_files == 1
        assert next(p for p in search.doctor().providers if p.provider == "claude").unindexed_files == 1
    command = [sys.executable, "-c", "from ai_session_search.entrypoint import cli_main; cli_main()",
               "--config", str(config), "--index-refresh", "existing-only"]
    env = dict(os.environ, AI_SESSION_SEARCH_DATABASE=str(database))
    cli = json.loads(subprocess.check_output([*command, "doctor", "--format", "json"], env=env, text=True))
    # Rust serializes IndexStatus flattened; Python exposes a typed nested value.
    python.update(python.pop("index_status"))
    # Python's typed readiness exposes the snapshot's fields without its envelope.
    snapshot = cli["readiness"].pop("snapshot")
    snapshot["snapshot_availability"] = snapshot.pop("availability")
    cli["readiness"].update(snapshot)
    assert set(cli) - set(python) == {"config_file", "config_files_not_read"}
    assert set(python) - set(cli) == set()
    for key in ["config_file", "config_files_not_read"]:
        cli.pop(key)
    def keys(value: Any) -> Any:
        if isinstance(value, dict):
            return {k: keys(v) for k, v in value.items()}
        if isinstance(value, list):
            return [keys(v) for v in value]
        return None
    assert keys(python) == keys(cli)
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "synthetic", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "get_index_status", "arguments": {}}},
    ]
    result = subprocess.run([*command, "mcp", "serve"], env=env, text=True, capture_output=True,
                            input="".join(json.dumps(r) + "\n" for r in requests), check=True, timeout=30)
    response = next(json.loads(line) for line in result.stdout.splitlines() if json.loads(line).get("id") == 2)
    mcp = response["result"]["structuredContent"]
    snapshot = mcp["readiness"].pop("snapshot")
    snapshot["snapshot_availability"] = snapshot.pop("availability")
    mcp["readiness"].update(snapshot)
    assert keys(mcp) == keys(cli)
