# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
"""Exercise the Python and CLI surfaces on the same synthetic index."""

import json
import os
import sqlite3
import subprocess
from pathlib import Path

import pytest

import ai_session_search as aise

CLI = Path(__file__).resolve().parents[1] / "target/release/aise"


@pytest.fixture
def corpus(tmp_path: Path):
    database = tmp_path / "index.db"
    search = aise.SessionSearch(database)
    search.refresh()
    with sqlite3.connect(database) as db:
        for n in range(3):
            sid = f"claude:synthetic-{n}"
            db.execute(
                "INSERT INTO sessions (id, provider, provider_session_id, title, cwd, "
                "preview_text, source_path, parse_version, discovery_source) "
                "VALUES (?, 'claude', ?, 'retry logic', '/synthetic/project', "
                "'retry logic', ?, 'test', 'fixture')",
                (sid, f"synthetic-{n}", str(tmp_path / f"{n}.jsonl")),
            )
            for seq in range(3):
                db.execute(
                    "INSERT INTO messages (session_id, provider, seq, role, kind, ts, content, "
                    "authorship, record_relation) VALUES (?, 'claude', ?, 'user', 'conversation', "
                    "'2026-01-15T12:00:00+00:00', ?, 'human', 'original')",
                    (sid, seq, f"You forgot the retry logic. Please fix retry logic {seq}."),
                )
            db.execute(
                "INSERT INTO file_edits (session_id, provider, seq, tool, file_path, file_name, new_content) "
                "VALUES (?, 'claude', 0, 'Write', '/synthetic/project/app.py', 'app.py', 'print(42)')",
                (sid,),
            )
    return search


def cli(search, *args):
    env = dict(os.environ, AI_SESSION_SEARCH_DATABASE=str(search.db_path))
    result = subprocess.run(
        [str(CLI), "--index-refresh", "existing-only", *args], env=env,
        text=True, capture_output=True, check=True,
    )
    return json.loads(result.stdout)


def test_session_dictionary_matches_cli(corpus):
    actual = [row.to_dict() for row in corpus.list_sessions()]
    assert actual == cli(corpus, "list", "--format", "json")


def test_message_dictionary_uses_cli_timestamp_name(corpus):
    message = corpus.read_session_messages("synthetic-0", limit=1)[0]
    assert message.to_dict()["ts"] == message.timestamp
    assert "timestamp" not in message.to_dict()


def test_service_errors_are_named_and_keep_runtime_handlers(corpus):
    from ai_session_search import native

    with pytest.raises(native.NotFoundError) as missing:
        corpus.read_session_messages("does-not-exist")
    assert isinstance(missing.value, RuntimeError)
    with pytest.raises(native.AmbiguousPrefixError) as ambiguous:
        corpus.read_session_messages("synthetic-")
    assert isinstance(ambiguous.value, RuntimeError)
    with pytest.raises(native.NotFoundError):
        corpus.reconstruct_file("missing.py")
    destination = Path(corpus.db_path).parent / "versions"
    corpus.publish_file_versions("app.py", destination, request=aise.FileQuery(scope=aise.QueryScope(session_id="synthetic-0")))
    with pytest.raises(native.AlreadyExistsError) as exists:
        corpus.publish_file_versions("app.py", destination, request=aise.FileQuery(scope=aise.QueryScope(session_id="synthetic-0")))
    assert isinstance(exists.value, RuntimeError)


def test_invalid_version_keeps_legacy_overflow_handler(corpus):
    from ai_session_search import native

    with pytest.raises(native.InvalidArgumentError) as invalid:
        corpus.reconstruct_file("app.py", version=-1)
    assert isinstance(invalid.value, (ValueError, RuntimeError))
    assert isinstance(invalid.value, OverflowError)
    assert "version" in str(invalid.value)
