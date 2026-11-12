# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
"""Exercise the Python and CLI surfaces on the same synthetic index."""

import json
import os
import sqlite3
import subprocess
from datetime import UTC
from pathlib import Path

import pytest

import ai_session_search as aise

CLI = Path(__file__).resolve().parents[1] / "target/debug/aise"


@pytest.fixture
def corpus(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    executable = tmp_path / "claude"
    executable.write_text("#!/bin/sh\nexit 1\n")
    executable.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ["PATH"])
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
    preparation = [] if args[0] == "db" else ["--index-refresh", "existing-only"]
    result = subprocess.run(
        [str(CLI), *preparation, *args], env=env,
        text=True, capture_output=True, check=False,
    )
    assert result.returncode == 0, result.stderr
    if args[0] == "db":
        # SQL JSON appends a truncation comment when a row limit applies.
        value, end = json.JSONDecoder().raw_decode(result.stdout)
        assert not result.stdout[end:].strip() or result.stdout[end:].lstrip().startswith("# truncated")
        return value
    return json.loads(result.stdout)


@pytest.mark.integration
def test_session_dictionary_matches_cli(corpus):
    actual = [row.to_dict() for row in corpus.list_sessions()]
    assert actual == cli(corpus, "list", "--format", "json")
    assert [row.to_dict() for row in corpus.search_sessions("retry")] == cli(corpus, "search", "retry", "--format", "json")


@pytest.mark.integration
def test_message_dictionary_uses_cli_timestamp_name(corpus):
    message = corpus.read_session_messages("synthetic-0", limit=1)[0]
    assert message.to_dict()["ts"] == message.timestamp
    assert "timestamp" not in message.to_dict()
    assert corpus.search_messages("forgot").to_dict() == cli(corpus, "messages", "search", "forgot", "--format", "json")


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


@pytest.mark.integration
def test_analysis_calls_match_cli(corpus):
    assert corpus.vocabulary(limit=4) == cli(corpus, "vocab", "--limit", "4", "--format", "json")
    assert corpus.repeats(max_groups=3) == cli(corpus, "repeats", "--max-groups", "3", "--report", "--format", "json")
    assert corpus.repeats("forgot", query_mode="regex", min_matches=2)["groups"]


@pytest.mark.integration
def test_sql_uses_shared_read_only_service(corpus):
    sql = "select id from sessions order by id"
    result = corpus.query_sql(sql, limit=2)
    assert result["rows"] == cli(corpus, "db", "query", sql, "--limit", "2", "--format", "json")
    assert result["next_offset"] == 2
    assert len(corpus.query_sql(sql, limit=0)["rows"]) == 3
    for statement in ("delete from sessions", "attach database ':memory:' as other"):
        with pytest.raises(RuntimeError):
            corpus.query_sql(statement)
    assert len(corpus.list_sessions()) == 3


@pytest.mark.integration
def test_resume_command_and_transcript_use_catalog(corpus):
    command = corpus.resume_command("synthetic-0")
    assert command["session_id"] == "claude:synthetic-0"
    assert command["argv"] == ["claude", "--resume", "synthetic-0"]
    result = subprocess.run(
        [str(CLI), "--index-refresh", "existing-only", "resume", "synthetic-0", "--dry-run"],
        env=dict(os.environ, AI_SESSION_SEARCH_DATABASE=str(corpus.db_path)),
        text=True, capture_output=True, check=True,
    )
    assert " ".join(command["argv"]) in result.stdout
    assert isinstance(corpus.transcript("synthetic-0")["text"], str)


def test_sql_and_vocabulary_enforce_access_scope(corpus, tmp_path: Path):
    config = tmp_path / "scoped.toml"
    config.write_text('[search.scope]\nmode = "allowed-roots"\nroots = ["/synthetic/project"]\n')
    scoped = aise.SessionSearch(corpus.db_path, config_path=config)
    with pytest.raises(RuntimeError, match="allowed-roots"):
        scoped.query_sql("select * from sessions")
    with pytest.raises(RuntimeError, match="allowed-roots"):
        scoped.vocabulary()


def test_analysis_call_bounds_and_scopes(corpus):
    with pytest.raises(aise.InvalidArgumentError, match=r"min.matches"):
        corpus.repeats(min_matches=0)
    with pytest.raises(aise.InvalidArgumentError, match="max_groups"):
        corpus.repeats(max_groups=-1)
    report = corpus.repeats(scope=aise.QueryScope(session_id="synthetic-0"), max_groups=1)
    assert report["groups"][0]["sessions"] == 1
    assert corpus.query_sql("select title from sessions", max_cell_chars=3)["rows"][0]["title"] == "ret... [truncated]"


def test_sql_cell_truncation_retains_receipt(corpus):
    result = corpus.query_sql("select title from sessions", max_cell_chars=3)
    assert result["truncated_cell_char_limit"] == 3


@pytest.mark.integration
def test_direct_classification_returns_cli_rows(corpus):
    matches = corpus.classify_messages(skill="corrections", include_unattributed_input=True)
    expected = cli(corpus, "skills", "corrections", "--include-unattributed-input", "--format", "json")
    assert matches
    assert [row.to_dict() for row in matches] == expected["output"]["result"]["report"]["matches"]
    scoped = corpus.classify_messages(request=aise.MessageClassificationQuery(scope=aise.QueryScope(session_id="synthetic-0")))
    assert {row.session_id for row in scoped} == {"claude:synthetic-0"}


@pytest.mark.integration
def test_session_limits_follow_configuration(corpus, tmp_path: Path):
    config = tmp_path / "limits.toml"
    config.write_text('[search]\ndefault_limit = 2\n')
    search = aise.SessionSearch(corpus.db_path, config_path=config)
    for request in (None, aise.SessionQuery(), aise.SessionQuery(path_prefix="/synthetic")):
        assert len(search.list_sessions(request)) == 2
        assert len(search.search_sessions("retry", request)) == 2
    assert [row.to_dict() for row in search.list_sessions()] == cli(search, "list", "--config", str(config), "--format", "json")
    assert len(search.list_sessions(aise.SessionQuery(limit=0))) == 3
    assert len(search.search_sessions("retry", aise.SessionQuery(limit=1))) == 1


def test_existing_index_mode_never_creates_missing_database(tmp_path: Path):
    database = tmp_path / "missing" / "index.db"
    with pytest.raises(aise.NotFoundError):
        aise.SessionSearch(database, index_refresh="existing-only")
    assert not database.parent.exists()


def test_strict_unprepared_index_has_actionable_error(tmp_path: Path):
    database = tmp_path / "index.db"
    aise.SessionSearch(database)
    strict = aise.SessionSearch(database, index_refresh="existing-only")
    with pytest.raises(aise.IndexNotRefreshedError):
        strict.list_sessions()
    assert strict.prepare_for_read() is None
    with pytest.raises(aise.IndexNotRefreshedError):
        strict.list_sessions()
    aise.SessionSearch(database).reindex()
    assert strict.list_sessions() == []


def test_prepare_for_read_applies_auto_refresh(tmp_path: Path):
    search = aise.SessionSearch(tmp_path / "index.db", index_refresh="auto")
    with pytest.raises(aise.IndexNotRefreshedError):
        search.list_sessions()
    search.prepare_for_read()
    assert search.list_sessions() == []


def test_query_filters_accept_flat_and_scope_shapes(corpus):
    scope = aise.QueryScope(provider="claude", path_prefix="/synthetic")
    assert corpus.list_sessions(aise.SessionQuery(scope=scope)) == corpus.list_sessions(aise.SessionQuery(provider="claude", path_prefix="/synthetic"))
    assert corpus.search_files("*.py", aise.FileQuery(scope=scope)) == corpus.search_files("*.py", aise.FileQuery(provider="claude", path_prefix="/synthetic"))
    flat = corpus.search_messages("forgot", aise.MessageSearchRequest(provider="claude", path_prefix="/synthetic"))
    grouped = corpus.search_messages("forgot", aise.MessageSearchRequest(scope=scope))
    assert flat == grouped
    with pytest.raises(aise.InvalidArgumentError, match="scope"):
        aise.FileQuery(scope=scope, path_prefix="/different")
    with pytest.raises(aise.InvalidArgumentError, match="scope"):
        aise.SessionQuery(scope=scope, path_prefix="/different")


def test_message_query_mode_can_travel_with_request(corpus):
    request = aise.MessageSearchRequest(query_mode="regex")
    assert corpus.search_messages("forgot.*retry", request) == corpus.search_messages("forgot.*retry", query_mode="regex")
    with pytest.raises(aise.InvalidArgumentError, match="query_mode"):
        corpus.search_messages("forgot", request, query_mode="literal")


@pytest.mark.integration
def test_session_pages_match_unpaged_cli_order(corpus):
    all_listed = cli(corpus, "list", "--limit", "0", "--format", "json")
    all_ranked = cli(corpus, "search", "retry", "--limit", "0", "--format", "json")
    listed: list[dict[str, object]] = []
    ranked: list[dict[str, object]] = []
    for offset in range(3):
        request = aise.SessionQuery(limit=1, offset=offset)
        listed.extend(row.to_dict() for row in corpus.list_sessions(request))
        ranked.extend(row.to_dict() for row in corpus.search_sessions("retry", request))
    assert listed == all_listed
    assert ranked == all_ranked


@pytest.mark.integration
def test_structured_search_hits_include_timestamp(corpus):
    response = corpus.search_messages("forgot")
    assert response.results[0]["timestamp"] == "2026-01-15T12:00:00Z"
    assert response.to_dict() == cli(corpus, "messages", "search", "forgot", "--format", "json")
    with sqlite3.connect(corpus.db_path) as db:
        db.execute("UPDATE messages SET ts = NULL")
    assert all(row["timestamp"] is None for row in corpus.search_messages("forgot").results)


@pytest.mark.integration
def test_message_ranges_can_include_harness_notices(corpus):
    with sqlite3.connect(corpus.db_path) as db:
        db.execute("INSERT INTO messages (session_id, provider, seq, role, kind, content) VALUES ('claude:synthetic-0', 'claude', 3, 'user', 'harness_notice', 'synthetic hook notice')")
    assert [row.seq for row in corpus.read_session_messages("synthetic-0")] == [0, 1, 2]
    kinds = ["conversation", "harness_notice"]
    rows = corpus.read_session_messages("synthetic-0", kinds=kinds)
    expected = cli(corpus, "messages", "get", "synthetic-0", "--kinds", "conversation,harness-notice", "--format", "json")
    assert [row.seq for row in rows] == [0, 1, 2, 3]
    assert [row.seq for row in rows] == [row["seq"] for row in expected]
    assert corpus.message_context("synthetic-0", 3)[0].kind == "harness_notice"


def test_standard_dates_and_paths_are_native_inputs(corpus):
    from datetime import date, datetime

    assert aise.DateRange(when=date(2026, 1, 15)) == aise.DateRange(when="2026-01-15")
    instant = datetime(2026, 1, 15, 12, tzinfo=UTC)
    assert aise.DateRange(since=instant) == aise.DateRange(since=instant.isoformat())
    assert corpus.list_sessions(aise.SessionQuery(path_prefix=Path("/synthetic")))
    assert corpus.search_messages("forgot", aise.MessageSearchRequest(path_prefix=Path("/synthetic"), dates=aise.DateRange(when=date(2026, 1, 15)))).results
    assert aise.QueryScope(path_prefix=Path("/synthetic")) == aise.QueryScope(path_prefix="/synthetic")
    assert aise.MessageScope(workspace_path_prefix=Path("/synthetic")) == aise.MessageScope(workspace_path_prefix="/synthetic")
    assert aise.QueryExclusions(path_prefixes=[Path("/excluded")]).path_prefixes == ["/excluded"]
    assert aise.MessageExclusions(transcript_path_prefixes=[Path("/excluded")]).transcript_path_prefixes == ["/excluded"]
    assert corpus.file_history(Path("app.py"))
    recovered = corpus.reconstruct_file(Path("app.py"), request=aise.FileQuery(session_id="synthetic-0", path_prefix=Path("/synthetic")))
    assert recovered.content == "print(42)"
    with pytest.raises(TypeError):
        aise.DateRange(since=object())  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        aise.QueryScope(path_prefix=object())  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        aise.QueryExclusions(path_prefixes="/bad")  # type: ignore[arg-type]


def test_restore_documents_and_preserves_destinations(corpus, tmp_path: Path):
    doc = aise.ReconstructedFile.restore.__doc__
    assert doc and "output_dir" in doc and ".recovered" in doc
    original = tmp_path / "app.py"
    original.write_text("current file")
    with sqlite3.connect(corpus.db_path) as db:
        db.execute("UPDATE file_edits SET file_path = ? WHERE session_id = 'claude:synthetic-0'", (str(original),))
    recovered = corpus.reconstruct_file(original, request=aise.FileQuery(session_id="synthetic-0"))
    first = recovered.restore()
    second = recovered.restore()
    assert first == tmp_path / "app.recovered.py"
    assert second == tmp_path / "app.recovered_2.py"
    assert first.read_text() == second.read_text() == "print(42)"
    assert original.read_text() == "current file"
    assert recovered.restore(output_dir=tmp_path / "copies").name == "app.recovered.py"


def test_service_close_is_idempotent_and_context_managed(corpus):
    versions = corpus.reconstruct_file_versions("app.py", request=aise.FileQuery(session_id="synthetic-0"))
    batches = corpus.search_message_batches("forgot")
    corpus.close()
    corpus.close()
    assert next(versions).content == "print(42)"
    with batches:
        assert next(batches).results
    with pytest.raises(RuntimeError, match="closed"):
        corpus.list_sessions()
    with pytest.raises(RuntimeError, match="closed"):
        corpus.query_sql("select 1")
    with pytest.raises(RuntimeError, match="closed"):
        corpus.refresh()
    with pytest.raises(LookupError):
        with aise.SessionSearch() as other:
            raise LookupError("test exit")
    with pytest.raises(RuntimeError, match="closed"):
        other.index_status()
