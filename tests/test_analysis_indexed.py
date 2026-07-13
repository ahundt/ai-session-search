from __future__ import annotations

import json
import re
import sqlite3
from pathlib import Path
from types import SimpleNamespace

import pytest

native = pytest.importorskip("ai_session_search.native", reason="native extension is not installed")
analyzer = pytest.importorskip("ai_session_search.analysis.analyzer")
run_analysis = analyzer.run_analysis
build_graph = pytest.importorskip("ai_session_search.analysis.graph").build_graph


def _seed_index(database: Path) -> native.SessionSearch:
    search = native.SessionSearch(database)
    with sqlite3.connect(database) as connection:
        connection.executemany(
            """
            insert into sessions (
                id, provider, provider_session_id, title, cwd, created_at,
                preview_text, source_path, parse_version, discovery_source
            ) values (?, ?, ?, ?, ?, ?, '', ?, 'test', 'fixture')
            """,
            [
                (
                    "gemini-cli:first",
                    "gemini-cli",
                    "first",
                    "Project v1",
                    "/repo/project",
                    "2026-04-01T12:00:00+00:00",
                    "/sessions/first.json",
                ),
                (
                    "gemini-cli:second",
                    "gemini-cli",
                    "second",
                    "Project v2",
                    "/repo/project",
                    "2026-04-02T12:00:00+00:00",
                    "/sessions/second.json",
                ),
                (
                    "codex:other",
                    "codex",
                    "other",
                    "Other",
                    "/repo/other",
                    "2026-04-03T12:00:00+00:00",
                    "/sessions/other.jsonl",
                ),
                (
                    "gemini-cli:empty",
                    "gemini-cli",
                    "empty",
                    "Empty",
                    "/repo/project",
                    "2026-04-04T12:00:00+00:00",
                    "/sessions/empty.json",
                ),
            ],
        )
        connection.executemany(
            """
            insert into messages (session_id, provider, seq, role, content)
            values (?, ?, ?, ?, ?)
            """,
            [
                ("gemini-cli:first", "gemini-cli", 0, "user", "plan the 2026-04-01 migration"),
                ("gemini-cli:first", "gemini-cli", 1, "assistant", "response"),
                ("gemini-cli:first", "gemini-cli", 2, "user", "verify the result"),
                ("gemini-cli:second", "gemini-cli", 0, "user", "continue the project"),
                ("codex:other", "codex", 0, "user", "other provider request"),
            ],
        )
    return search


def test_analysis_uses_bounded_rust_pages_and_canonical_metadata(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    search = _seed_index(tmp_path / "index.db")
    output = tmp_path / "analysis"

    records = run_analysis(
        source_filter="gemini",
        config={"org_dir": str(output), "analysis_page_size": 1},
        search=search,
        refresh_index=False,
    )

    assert [(record.name, record.chunk_count, record.user_chunk_count) for record in records] == [
        ("Project v1", 3, 2),
        ("Project v2", 1, 1),
    ]
    assert all(record.source_format == "gemini_cli" for record in records)
    assert all(record.cwd == "/repo/project" for record in records)
    assert records[0].era == "2026"
    assert records[0].prompt_role != "standalone"
    assert records[1].prompt_role == "standalone"
    assert all(record.user_text == "" for record in records)
    persisted = json.loads((output / "session_db.json").read_text(encoding="utf-8"))
    assert all("user_text" not in record for record in persisted)
    assert not list(output.glob(".*.tmp"))
    assert "1 no messages" in capsys.readouterr().out

    graph = build_graph(persisted, strategies=None, config={"tfidf_similarity_threshold": 2.0})
    project_edges = [edge for edge in graph["edges"] if edge["edge_type"] == "project_group"]
    assert len(project_edges) == 1
    assert project_edges[0]["detection_method"] == "cwd"


def test_analysis_accepts_any_canonical_rust_provider(tmp_path: Path) -> None:
    search = _seed_index(tmp_path / "index.db")

    records = run_analysis(
        source_filter="codex",
        config={"org_dir": str(tmp_path / "analysis"), "analysis_page_size": 1},
        search=search,
        refresh_index=False,
    )

    assert [(record.source_format, record.name) for record in records] == [("codex", "Other")]


def test_analysis_rejects_non_positive_page_size(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="analysis_page_size must be greater than zero"):
        run_analysis(
            config={"org_dir": str(tmp_path), "analysis_page_size": 0},
            search=_seed_index(tmp_path / "index.db"),
            refresh_index=False,
        )


def test_analysis_cli_forwards_provider_and_resolved_output_config(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    import importlib

    import ai_session_search.cli as cli

    captured: dict = {}
    module = SimpleNamespace(run_analysis=lambda **kwargs: captured.update(kwargs))
    monkeypatch.setattr(importlib, "import_module", lambda _path: module)
    monkeypatch.setattr(cli, "_check_step_dep", lambda *_args: None)

    cli._run_single_step(
        "analyze",
        "codex",
        0,
        {"marker_window": 123},
        tmp_path / "resolved-output",
    )

    assert captured == {
        "marker_window": 0,
        "source_filter": "codex",
        "config": {
            "marker_window": 123,
            "org_dir": str(tmp_path / "resolved-output"),
        },
    }


def test_apply_codes_scores_only_matches_added_by_this_pass() -> None:
    record = analyzer.SessionRecord(
        name="Example",
        source_dir="",
        filepath="/session",
        source_format="codex",
        user_text="new technique and new role",
        chunk_count=1,
        user_chunk_count=1,
        techniques=["existing_technique"],
        roles=["existing_role"],
    )

    analyzer.apply_codes(
        record,
        {"new_technique": re.compile("new technique")},
        {"new_role": re.compile("new role")},
        {},
        {"technique": 7, "role": 11},
    )

    assert record.techniques == ["existing_technique", "new_technique"]
    assert record.roles == ["existing_role", "new_role"]
    assert record.rigor_score == 18
