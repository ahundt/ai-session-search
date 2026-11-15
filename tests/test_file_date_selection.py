# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
"""Date selection must not remove a delta's preceding snapshot."""

import json
from pathlib import Path

import pytest
from sqlite_helpers import write_database

import ai_session_search as aise


def test_file_dates_select_after_replay_and_page_eligible_versions(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    with aise.SessionSearch(database) as search:
        search.refresh()
    with write_database(database) as db:
        db.execute(
            "INSERT INTO sessions (id, provider, provider_session_id, source_path, "
            "parse_version, discovery_source, preview_text) VALUES "
            "('claude:dates', 'claude', 'dates', '/synthetic/dates.jsonl', 'test', 'fixture', '')"
        )
        db.execute(
            "INSERT INTO sessions (id, provider, provider_session_id, source_path, "
            "parse_version, discovery_source, preview_text) VALUES "
            "('claude:old', 'claude', 'old', '/synthetic/old.jsonl', 'test', 'fixture', '')"
        )
        db.execute(
            "INSERT INTO file_edits (session_id, provider, seq, ts, tool, file_path, "
            "file_name, new_content, edits_json) VALUES "
            "('claude:old', 'claude', 0, '2025-01-01T00:00:00Z', 'Write', "
            "'/synthetic/app.py', 'app.py', 'old', '[]')"
        )
        for seq, ts, content, edits in [
            (0, "2026-01-01T00:00:00Z", "alpha", []),
            (1, "2026-01-11T00:00:00Z", None, [{"old": "alpha", "new": "beta"}]),
            (2, "2026-01-11T12:00:00Z", None, [{"old": "beta", "new": "gamma"}]),
        ]:
            db.execute(
                "INSERT INTO file_edits (session_id, provider, seq, ts, tool, file_path, "
                "file_name, new_content, edits_json) VALUES "
                "('claude:dates', 'claude', ?, ?, 'Edit', '/synthetic/app.py', 'app.py', ?, ?)",
                (seq, ts, content, json.dumps(edits)),
            )
    with aise.SessionSearch(database) as search:
        query = aise.FileQuery(dates=aise.DateRange(when="2026-01-11"))
        assert [v.version for v in search.file_history("app.py", query)] == [2, 3]
        assert [(v.version, v.content) for v in search.reconstruct_file_versions("app.py", request=query)] == [(2, "beta"), (3, "gamma")]
        assert search.reconstruct_file("app.py", request=query, version=2).content == "beta"
        with pytest.raises(Exception, match="scope"):
            search.reconstruct_file("app.py", request=query, version=1)
        page = aise.FileQuery(dates=aise.DateRange(when="2026-01-11"), limit=1, offset=1)
        assert [(v.version, v.content) for v in search.reconstruct_file_versions("app.py", request=page)] == [(3, "gamma")]
        with pytest.raises(Exception, match="paging"):
            search.reconstruct_file("app.py", request=page)
        boundary = aise.FileQuery(dates=aise.DateRange(since="2026-01-11T00:00:00Z", until="2026-01-11T00:00:00Z"))
        assert [v.version for v in search.file_history("app.py", boundary)] == [2]
        assert search.reconstruct_file("app.py", request=boundary).content == "beta"
        for ambiguous in [aise.FileQuery(), aise.FileQuery(dates=aise.DateRange(since="2025-01-01"))]:
            with pytest.raises(Exception, match="matched 2 sessions; set an exact session_id"):
                search.reconstruct_file("app.py", request=ambiguous)
            with pytest.raises(Exception, match="matched 2 sessions; set an exact session_id"):
                list(search.reconstruct_file_versions("app.py", request=ambiguous))
        for empty in [aise.FileQuery(dates=aise.DateRange(since="2027-01-01")), aise.FileQuery(dates=aise.DateRange(until="2024-01-01"))]:
            assert search.file_history("app.py", empty) == []
            assert list(search.reconstruct_file_versions("app.py", request=empty)) == []
            with pytest.raises(aise.NotFoundError):
                search.reconstruct_file("app.py", request=empty)
