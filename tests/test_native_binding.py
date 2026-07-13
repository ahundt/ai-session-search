from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

native = pytest.importorskip("ai_session_search.native", reason="native extension is not installed")


def test_native_session_search_is_typed_and_thread_safe(tmp_path: Path) -> None:
    search = native.SessionSearch(tmp_path / "index.db")

    with ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(search.search_messages, "missing") for _ in range(2)]

    assert search.db_path == tmp_path / "index.db"
    assert [future.result() for future in futures] == [[], []]


def test_native_session_search_rejects_empty_database_path() -> None:
    with pytest.raises(ValueError, match="db_path must not be empty"):
        native.SessionSearch("")
