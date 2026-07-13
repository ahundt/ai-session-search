from __future__ import annotations

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
    assert (session_query.limit, message_query.limit, file_query.limit) == (3, 4, 5)


def test_native_session_search_rejects_empty_database_path() -> None:
    with pytest.raises(ValueError, match="db_path must not be empty"):
        native.SessionSearch("")


def test_native_query_rejects_unknown_provider(tmp_path: Path) -> None:
    search = native.SessionSearch(tmp_path / "index.db")

    with pytest.raises(ValueError, match="invalid provider"):
        search.list_sessions(native.SessionQuery(provider="unknown"))
