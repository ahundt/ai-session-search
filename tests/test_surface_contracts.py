# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

from sqlite_helpers import write_database

import ai_session_search as aise
from scripts.verify_harness_discovery import environment


def test_read_event_dates_and_warning_scopes(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    with aise.SessionSearch(database) as search:
        with write_database(database) as connection:
            connection.executescript("""
                insert into sessions(id,provider,provider_session_id,preview_text,source_path,
                                     parse_version,discovery_source,parse_warning)
                values ('claude:healthy','claude','healthy','','/healthy','test','test',null),
                       ('claude:warning','claude','warning','','/warning','test','test','synthetic warning');
                insert into messages(session_id,provider,seq,role,kind,ts,content)
                values ('claude:warning','claude',0,'assistant','conversation','2026-01-01T00:00:00Z','first'),
                       ('claude:warning','claude',1,'assistant','conversation','2026-02-01T00:00:00Z','second');
            """)
        hits = search.read_session_messages("claude:warning", dates=aise.DateRange(when="2026-02"))
        assert [(hit.session_id, hit.seq) for hit in hits] == [("claude:warning", 1)]
        request = aise.SessionQuery(warnings_only=True, limit=1)
        assert request.warnings_only is True
        assert [row.id for row in search.list_sessions(request)] == ["claude:warning"]
        assert aise.AnalysisRequest(warnings_only=True).warnings_only is True
        selected = search.read_session_messages("claude:warning", role="assistant", order="newest",
                                               limit=1, dates=aise.DateRange(since="2026-02-01T00:00:00Z"))
        expected = [("claude:warning", 1)]
        assert [(row.session_id, row.seq) for row in selected] == expected
        env = environment(tmp_path / "runtime")
        env["AI_SESSION_SEARCH_DATABASE"] = str(database)
        command = [sys.executable, "-c", "from ai_session_search.entrypoint import cli_main; cli_main()",
                   "--index-refresh", "existing-only"]
        cli = subprocess.run([*command, "messages", "get", "claude:warning", "--role", "assistant",
                              "--order", "newest", "--limit", "1", "--since", "2026-02-01T00:00:00Z",
                              "--format", "json"], env=env, capture_output=True, text=True, check=True)
        assert [(row["session_id"], row["seq"]) for row in json.loads(cli.stdout)] == expected
        requests = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "synthetic", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "get_session", "arguments": {"session_id": "claude:warning", "role": "assistant",
                "order": "newest", "limit": 1, "since": "2026-02-01T00:00:00Z"}}},
        ]
        mcp = subprocess.run([*command, "mcp", "serve"], env=env, capture_output=True, text=True,
                             input="".join(json.dumps(request) + "\n" for request in requests), check=True)
        response = next(json.loads(line) for line in mcp.stdout.splitlines() if json.loads(line).get("id") == 2)
        assert not response["result"].get("isError"), response
        payload = response["result"]["structuredContent"]
        assert [(payload["session_id"], row["seq"]) for row in payload["messages"]] == expected


def test_python_schema_returns_semantic_notes(tmp_path: Path) -> None:
    with aise.SessionSearch(tmp_path / "index.db") as search:
        result = search.schema(table="messages")
        assert isinstance(result, aise.SqlSchemaResult)
        payload = result.to_dict()
        assert payload["columns"] == result.columns
        assert payload["rows"] == result.rows
        assert payload["truncated"] is False
        assert payload["truncated_cell_char_limit"] is None
        notes = {row["name"]: row["note"] for row in result.rows}
        assert "underscores" in notes["kind"]
        assert "Provider spelling" in notes["tool_name"]
        assert search.schema().rows


def test_result_dictionaries_preserve_shared_serialization(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    with aise.SessionSearch(database) as search:
        with write_database(database) as connection:
            connection.executescript("""
                insert into sessions(id,provider,provider_session_id,preview_text,source_path,
                                     parse_version,discovery_source,raw_metadata_json)
                values ('claude:dictionary','claude','dictionary','','/dictionary','test','test','{"synthetic":true}');
            """)
        record = search.list_sessions()[0].to_dict()
        assert record["raw_metadata_json"] == '{"synthetic":true}'
        env = environment(tmp_path / "runtime")
        env["AI_SESSION_SEARCH_DATABASE"] = str(database)
        result = subprocess.run(
            [sys.executable, "-c", "from ai_session_search.entrypoint import cli_main; cli_main()",
             "--index-refresh", "existing-only", "skills", "corrections", "--format", "json"],
            env=env, capture_output=True, text=True, check=True,
        )
        report = search.run_skill(aise.SkillRunQuery(skill=aise.SkillSelector(name="corrections"), input=aise.MessageClassificationQuery()))
        assert report.to_dict() == json.loads(result.stdout)


def test_rust_result_fields_reach_native_wrappers() -> None:
    """A Rust result field addition requires an explicit Python boundary decision."""
    root = Path(__file__).resolve().parents[1]
    core = root / "rust/ai-session-search-core/src"
    binding = (root / "rust/ai-session-search-python/src/lib.rs").read_text()
    groups = {
        "models.rs": ["SessionRecord", "AnalysisDocument", "AnalysisDocumentPage",
                      "MessageHit", "FileEditSummary", "FileVersion", "FileCrossRef",
                      "SessionTimeProfile", "MessageClassificationMatch"],
        "corrections.rs": ["MessageClassificationReport"],
        "analysis_pipeline.rs": ["ClassificationMatch", "AnalyzedSession", "PhraseFrequency",
                                 "SessionGraphNode", "SessionGraphEdge", "SessionGraphGroup", "SessionGraph"],
        "inspect.rs": ["MessagePreview", "ToolActivity", "RefEvidence", "ChangedFileEvidence", "SessionInspection"],
        "analysis_publication.rs": ["AnalysisArtifact", "PublishedAnalysisArtifact", "AnalysisPublicationReceipt"],
        "skill_run.rs": ["ResolvedSkillReceipt", "MessageClassificationResult", "SkillRunReport"],
    }
    for filename, names in groups.items():
        source = (core / filename).read_text()
        for name in names:
            rust = re.search(rf"pub struct {name} \{{(.*?)\n\}}", source, re.S)
            native = re.search(rf"struct Native{name} \{{(.*?)\n\}}", binding, re.S)
            assert rust and native, name
            fields = set(re.findall(r"^    pub (\w+):", rust[1], re.M))
            fields -= {"follow_ups"}  # serde(skip); shell next_commands is the serialized contract.
            fields = {"timestamp" if field == "ts" else field for field in fields}
            python_fields = set(re.findall(r"^    (\w+):", native[1], re.M))
            assert fields <= python_fields, (name, fields - python_fields)
