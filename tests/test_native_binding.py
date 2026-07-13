from __future__ import annotations

import sqlite3
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

native = pytest.importorskip("ai_session_search.native", reason="native extension is not installed")


def test_native_session_search_is_typed_and_thread_safe(tmp_path: Path) -> None:
    search = native.SessionSearch(tmp_path / "index.db")
    session_query = native.SessionQuery(limit=3)
    message_query = native.MessageQuery(limit=4)
    file_query = native.FileQueryRequest(limit=5)

    with ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(search.search_messages, "missing", message_query) for _ in range(2)]

    assert search.db_path == tmp_path / "index.db"
    assert [future.result() for future in futures] == [[], []]
    assert search.list_sessions(session_query) == []
    assert search.search_sessions("missing", session_query) == []
    assert search.search_files("*.py", file_query) == []
    assert search.cross_reference_files("*.py", file_query) == []
    with pytest.raises(RuntimeError, match="no file edits found"):
        search.file_history("missing.py", file_query)
    with pytest.raises(RuntimeError, match="no file edits found"):
        search.reconstruct_file("missing.py", request=file_query)
    with pytest.raises(RuntimeError, match="no session matches"):
        search.export_session("missing")
    with pytest.raises(ValueError, match="unsupported export format: html"):
        search.export_session("missing", "html")
    assert (session_query.limit, message_query.limit, file_query.limit) == (3, 4, 5)


def test_native_session_search_rejects_empty_database_path() -> None:
    with pytest.raises(ValueError, match="db_path must not be empty"):
        native.SessionSearch("")


def test_native_query_rejects_unknown_provider(tmp_path: Path) -> None:
    search = native.SessionSearch(tmp_path / "index.db")

    with pytest.raises(ValueError, match="invalid provider"):
        search.list_sessions(native.SessionQuery(provider="unknown"))


def test_native_export_returns_rust_document_without_writing(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    with sqlite3.connect(database) as connection:
        connection.execute(
            """
            insert into sessions (
                id, provider, provider_session_id, title, cwd, preview_text, source_path,
                message_count, parse_version, discovery_source
            ) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            ("claude:abc", "claude", "abc", "Example", "/repo", "preview", "/session", 2, "test", "fixture"),
        )
        connection.execute(
            "insert into transcripts (session_id, transcript_text) values (?, ?)",
            ("claude:abc", "[user] hello\n\n[assistant] hi"),
        )

    document = search.export_session("abc", "markdown")

    assert document.format == "markdown"
    assert document.content == (
        "# Example\n\n- Provider: claude\n- Session ID: abc\n- CWD: /repo\n- Updated At: -\n\n"
        "## Preview\n\npreview\n\n## Transcript\n\n```\n[user] hello\n\n[assistant] hi\n```\n"
    )
    assert not (tmp_path / "Example.md").exists()


def test_native_source_inventory_uses_configured_provider_policy(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    config_dir = tmp_path / "config"
    cache_dir = tmp_path / "cache"
    config_dir.mkdir()
    providers = ["claude", "claude-desktop", "codex", "cursor", "antigravity", "pi", "aistudio", "gemini-cli"]
    (config_dir / "config.toml").write_text(
        "\n".join(f"[providers.{provider}]\nenabled = false" for provider in providers),
        encoding="utf-8",
    )
    monkeypatch.setenv("AI_SESSION_SEARCH_CONFIG", str(config_dir / "config.toml"))
    monkeypatch.setenv("AI_SESSION_SEARCH_CACHE_DIR", str(cache_dir))
    search = native.SessionSearch(tmp_path / "index.db")

    inventory = search.source_inventory()

    assert [status.provider for status in inventory] == providers
    assert all(not status.enabled and status.discovered_files == 0 for status in inventory)
    assert all(isinstance(status.roots, list) for status in inventory)


def test_native_analysis_is_typed_scoped_and_index_backed(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    with sqlite3.connect(database) as connection:
        for session_id, provider in [("claude:analysis", "claude"), ("codex:other", "codex")]:
            connection.execute(
                """
                insert into sessions (
                    id, provider, provider_session_id, updated_at, preview_text, source_path,
                    parse_version, discovery_source
                ) values (?, ?, ?, ?, '', ?, 'test', 'fixture')
                """,
                (
                    session_id,
                    provider,
                    session_id.split(":", 1)[1],
                    "2026-01-15T12:00:00+00:00" if provider == "claude" else "2026-02-15T12:00:00+00:00",
                    f"/{provider}.jsonl",
                ),
            )
        connection.executemany(
            """
            insert into messages (session_id, provider, seq, role, kind, ts, content)
            values (?, ?, ?, ?, 'conversation', ?, ?)
            """,
            [
                ("claude:analysis", "claude", 0, "user", "2026-01-15T12:00:00+00:00", "actually, that is wrong"),
                ("claude:analysis", "claude", 1, "slash", "2026-01-15T12:01:00+00:00", "/plan verify migration"),
                ("codex:other", "codex", 0, "user", "2026-02-15T12:00:00+00:00", "unrelated"),
            ],
        )
        connection.executemany(
            """
            insert into file_edits (
                session_id, provider, seq, ts, tool, file_path, file_name, new_content
            ) values (?, ?, 0, ?, 'Write', ?, ?, 'fixture')
            """,
            [
                ("claude:analysis", "claude", "2026-01-15T12:02:00+00:00", "/repo/jan.py", "jan.py"),
                ("codex:other", "codex", "2026-02-15T12:02:00+00:00", "/repo/feb.py", "feb.py"),
            ],
        )

    scope = native.QueryScope(provider="claude", session_id="analysis")
    request = native.AnalysisQuery(scope=scope, limit=10)
    corrections = search.find_corrections(request)
    planning = search.planning_usage(request, ["^/plan$"])
    roles = search.role_statistics(request)
    messages = search.search_messages(
        "",
        native.MessageQuery(scope=scope, limit=10),
    )
    context = search.message_context("analysis", 1, before=1, after=0)
    files = search.search_files(
        "*.py",
        native.FileQueryRequest(
            scope=native.QueryScope(
                provider="claude",
                session_id="analysis",
                dates=native.DateRangeQuery(when="2026-01"),
            )
        ),
    )
    reconstructed = search.reconstruct_file(
        "jan.py",
        request=native.FileQueryRequest(scope=native.QueryScope(session_id="analysis")),
    )
    restored = reconstructed.restore(output_dir=tmp_path / "restored")

    assert [(hit.provider, hit.content) for hit in corrections] == [("claude", "actually, that is wrong")]
    assert [(row.command, row.count) for row in planning] == [("/plan", 1)]
    assert {row.role: row.count for row in roles} == {"slash": 1, "user": 1}
    assert [(message.provider, message.seq) for message in messages] == [("claude", 0), ("claude", 1)]
    assert [message.seq for message in context] == [0, 1]
    assert [(file.file_name, file.edits) for file in files] == [("jan.py", 1)]
    assert restored.name == "jan.recovered.py"
    assert restored.read_text(encoding="utf-8") == "fixture"
    assert len(search.role_statistics(native.AnalysisQuery(scope=native.QueryScope(provider="claude"), limit=1))) == 1
    assert [
        session.id
        for session in search.list_sessions(native.SessionQuery(dates=native.DateRangeQuery(when="2026-01")))
    ] == ["claude:analysis"]
    assert search.search_messages(
        "",
        native.MessageQuery(scope=native.QueryScope(dates=native.DateRangeQuery(when="1999"))),
    ) == []
    with pytest.raises(ValueError, match="mutually exclusive"):
        native.QueryScope(session_id="exact", session="fuzzy")
    with pytest.raises(ValueError, match="invalid provider"):
        native.QueryScope(provider="unknown")
    with pytest.raises(ValueError, match="when is mutually exclusive"):
        native.DateRangeQuery(since="2026", when="2026-01")
    with pytest.raises(ValueError):
        search.list_sessions(native.SessionQuery(dates=native.DateRangeQuery(when="2026-13-40")))
    with pytest.raises(ValueError, match="must be non-negative"):
        search.message_context("analysis", 0, before=-1)
