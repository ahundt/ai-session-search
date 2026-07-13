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
