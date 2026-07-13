from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from types import SimpleNamespace

import pytest

native = pytest.importorskip("ai_session_search.native", reason="native extension is not installed")
analyzer = pytest.importorskip("ai_session_search.analysis.analyzer")
run_analysis = analyzer.run_analysis


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
                (
                    "gemini-cli:first",
                    "gemini-cli",
                    0,
                    "user",
                    "plan the approach for the 2026-04-01 migration",
                ),
                ("gemini-cli:first", "gemini-cli", 1, "assistant", "response"),
                ("gemini-cli:first", "gemini-cli", 2, "user", "verify the result"),
                ("gemini-cli:second", "gemini-cli", 0, "user", "continue the project"),
                ("codex:other", "codex", 0, "user", "other provider request"),
            ],
        )
    return search


def test_analysis_uses_native_snapshot_and_canonical_metadata(
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    search = _seed_index(tmp_path / "index.db")
    output = tmp_path / "analysis"

    records = run_analysis(
        source_filter="gemini",
        config={"org_dir": str(output)},
        search=search,
        refresh_index=False,
    )

    assert [(record.name, record.chunk_count, record.user_chunk_count) for record in records] == [
        ("Project v1", 3, 2),
        ("Project v2", 1, 1),
    ]
    assert all(record.source_format == "gemini_cli" for record in records)
    assert all(record.cwd == "/repo/project" for record in records)
    assert [record.session_id for record in records] == ["gemini-cli:first", "gemini-cli:second"]
    assert records[0].era == "2026"
    assert "planning" in records[0].techniques
    assert "planning" in records[0].task_categories
    assert records[0].prompt_role != "standalone"
    assert records[1].prompt_role == "standalone"
    assert all(record.graph_parent is None for record in records)
    assert all(record.user_text == "" for record in records)
    persisted = json.loads((output / "session_db.json").read_text(encoding="utf-8"))
    assert all("user_text" not in record for record in persisted)
    assert not list(output.glob(".*.tmp"))
    assert "1 no messages" in capsys.readouterr().out

    graph = json.loads((output / "SESSION_GRAPH.json").read_text(encoding="utf-8"))
    assert graph["edges"] == []
    assert graph["groups"] == [
        {
            "dimension": "working_directory",
            "key": "/repo/project",
            "session_ids": ["gemini-cli:empty", "gemini-cli:first", "gemini-cli:second"],
        }
    ]


def test_analysis_accepts_any_canonical_rust_provider(tmp_path: Path) -> None:
    search = _seed_index(tmp_path / "index.db")

    records = run_analysis(
        source_filter="codex",
        config={"org_dir": str(tmp_path / "analysis")},
        search=search,
        refresh_index=False,
    )

    assert [(record.source_format, record.name) for record in records] == [("codex", "Other")]


def test_analysis_rejects_invalid_phrase_configuration(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="analysis_phrase_widths must be a list"):
        run_analysis(
            config={"org_dir": str(tmp_path), "analysis_phrase_widths": "3,4"},
            search=_seed_index(tmp_path / "second-index.db"),
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


def test_standalone_vocabulary_uses_shared_index_pages(tmp_path: Path) -> None:
    vocabulary = pytest.importorskip("ai_session_search.analysis.vocab")
    output = tmp_path / "analysis"
    output.mkdir()
    (output / "scoring_weights.json").write_text(
        json.dumps({"min_session_text_len": 1, "min_ngram_freq": 1}),
        encoding="utf-8",
    )
    config = {"org_dir": str(output)}

    trigrams, quadgrams = vocabulary.mine_all(
        source_filter="codex",
        config=config,
        search=_seed_index(tmp_path / "index.db"),
        refresh_index=False,
    )
    vocabulary.write_report(trigrams, quadgrams, config=config)

    assert trigrams["other provider request"] == 1
    assert not quadgrams
    report = (output / "VOCABULARY_ANALYSIS.md").read_text(encoding="utf-8")
    assert "other provider request" in report
    assert "AI Studio sessions" not in report
