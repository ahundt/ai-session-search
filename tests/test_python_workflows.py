# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
"""Exercise the Python and CLI surfaces on the same synthetic index."""

import json
import os
import re
import shutil
import sqlite3
import subprocess
from datetime import UTC, datetime, timedelta
from pathlib import Path, PurePosixPath
from typing import Any

import pytest

import ai_session_search as aise

# The console script of the package under test, so the tests need no separate Cargo build.
CLI = shutil.which("aise")
assert CLI is not None, "run `uv run maturin develop --uv` to install the aise console script"


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
    preparation = [] if args[0] in {"db", "mcp"} else ["--index-refresh", "existing-only"]
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


@pytest.mark.integration
def test_gemini_resume_python_and_cli_use_native_id_and_project_cwd(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
):
    corpus = aise.SessionSearch(tmp_path / "gemini-index.db")
    corpus.refresh()
    executable = tmp_path / "gemini"
    executable.write_text("#!/bin/sh\nexit 1\n")
    executable.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ["PATH"])
    project = tmp_path / "project with spaces"
    project.mkdir()
    native_id = "11111111-2222-4333-8444-555555555555"
    session_id = "gemini-cli:synthetic-resume"
    with sqlite3.connect(corpus.db_path) as db:
        db.execute(
            "INSERT INTO sessions (id, provider, provider_session_id, cwd, "
            "preview_text, source_path, parse_version, discovery_source) "
            "VALUES (?, 'gemini-cli', ?, ?, '', ?, 'test', 'fixture')",
            (session_id, native_id, str(project), str(tmp_path / "session-synthetic.jsonl")),
        )
    db.close()
    command = corpus.resume_command(session_id)
    assert command["argv"] == ["gemini", "--resume", native_id]
    assert command["cwd"] == str(project)
    assert command["unavailable_reason"] is None
    result = subprocess.run(
        [str(CLI), "--index-refresh", "existing-only", "resume", session_id, "--dry-run"],
        env=dict(os.environ, AI_SESSION_SEARCH_DATABASE=str(corpus.db_path)),
        text=True, capture_output=True, check=True,
    )
    assert f"gemini --resume {native_id}" in result.stdout
    assert str(project) in result.stdout
    with sqlite3.connect(corpus.db_path) as db:
        db.execute("UPDATE sessions SET cwd = NULL WHERE id = ?", (session_id,))
    db.close()
    unavailable = corpus.resume_command(session_id)
    assert unavailable["argv"] is None
    assert "project directory" in unavailable["unavailable_reason"]
    corpus.close()


def test_sql_and_vocabulary_enforce_access_scope(corpus, tmp_path: Path):
    config = tmp_path / "scoped.toml"
    config.write_text(f'[search.scope]\nmode = "allowed-roots"\nroots = [{json.dumps(str(tmp_path))}]\n')
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


def test_configured_existing_only_preserves_published_empty_reads(tmp_path: Path):
    database = tmp_path / "empty.db"
    with aise.SessionSearch(database):
        pass
    config = tmp_path / "existing-only.toml"
    config.write_text('[index]\nrefresh = "existing-only"\n')
    with aise.SessionSearch(database, config_path=config) as legacy:
        assert legacy.list_sessions() == []
    with aise.SessionSearch(database, config_path=config, index_refresh="existing-only") as strict:
        with pytest.raises(aise.IndexNotRefreshedError):
            strict.list_sessions()


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
    database = corpus.db_path
    corpus.close()
    with sqlite3.connect(database) as db:
        db.execute("UPDATE messages SET ts = NULL")
    db.close()
    with aise.SessionSearch(database, index_refresh="existing-only") as reopened:
        updated = reopened.search_messages("forgot")
        assert all(row["timestamp"] is None for row in updated.results)
        assert updated.to_dict() == cli(reopened, "messages", "search", "forgot", "--format", "json")


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


def test_native_workspace_path_input(corpus, tmp_path: Path):
    with sqlite3.connect(corpus.db_path) as db:
        db.execute("UPDATE sessions SET cwd = ?", (str(tmp_path),))
    assert corpus.list_sessions(aise.SessionQuery(path_prefix=tmp_path))
    assert aise.QueryScope(path_prefix=tmp_path).path_prefix == str(tmp_path)


def test_standard_dates_and_paths_are_native_inputs(corpus):
    from datetime import date, datetime

    assert aise.DateRange(when=date(2026, 1, 15)) == aise.DateRange(when="2026-01-15")
    instant = datetime(2026, 1, 15, 12, tzinfo=UTC)
    assert aise.DateRange(since=instant) == aise.DateRange(since=instant.isoformat())
    assert corpus.list_sessions(aise.SessionQuery(path_prefix=PurePosixPath("/synthetic")))
    assert corpus.search_messages("forgot", aise.MessageSearchRequest(path_prefix=PurePosixPath("/synthetic"), dates=aise.DateRange(when=date(2026, 1, 15)))).results
    assert aise.QueryScope(path_prefix=PurePosixPath("/synthetic")) == aise.QueryScope(path_prefix="/synthetic")
    assert aise.MessageScope(workspace_path_prefix=PurePosixPath("/synthetic")) == aise.MessageScope(workspace_path_prefix="/synthetic")
    assert aise.QueryExclusions(path_prefixes=[PurePosixPath("/excluded")]).path_prefixes == ["/excluded"]
    assert aise.MessageExclusions(transcript_path_prefixes=[PurePosixPath("/excluded")]).transcript_path_prefixes == ["/excluded"]
    assert corpus.file_history(Path("app.py"))
    recovered = corpus.reconstruct_file(Path("app.py"), request=aise.FileQuery(session_id="synthetic-0", path_prefix=PurePosixPath("/synthetic")))
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


def test_empty_ranked_query_is_deprecated_with_working_replacement(corpus):
    with pytest.warns(DeprecationWarning, match="list_sessions"):
        hits = corpus.search_sessions("")
    assert [hit.session for hit in hits] == corpus.list_sessions()


def test_native_errors_and_fuzzy_help_name_python_actions(corpus):
    with pytest.raises(aise.NotFoundError) as missing:
        corpus.read_session_messages("absent")
    assert "list_sessions()" in str(missing.value)
    assert "aise list" not in str(missing.value)
    doc = aise.SessionSearch.search_messages.__doc__
    assert doc and "letters in order" in doc


@pytest.mark.integration
def test_file_query_default_matches_cli_without_hidden_fifty_row_limit(corpus):
    with sqlite3.connect(corpus.db_path) as db:
        for n in range(60):
            db.execute("INSERT INTO file_edits (session_id, provider, seq, tool, file_path, file_name, new_content) VALUES ('claude:synthetic-0', 'claude', ?, 'Write', ?, ?, 'fixture')", (100 + n, f"/synthetic/generated-{n}.py", f"generated-{n}.py"))
    files = corpus.search_files("generated-*.py")
    assert len(files) == 60
    assert [row.to_dict() for row in files] == cli(corpus, "files", "search", "generated-*.py", "--format", "json")
    assert len(corpus.search_files("generated-*.py", aise.FileQuery(limit=1))) == 1


def mcp_call(search, arguments):
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "synthetic-test", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "get_session", "arguments": dict(arguments, index_refresh="existing-only")}},
    ]
    result = subprocess.run([str(CLI), "mcp", "serve"], input="".join(json.dumps(row) + "\n" for row in requests), env=dict(os.environ, AI_SESSION_SEARCH_DATABASE=str(search.db_path)), text=True, capture_output=True, check=True, timeout=15)
    response = next(row for line in result.stdout.splitlines() if (row := json.loads(line)).get("id") == 2)
    assert "error" not in response, response
    return response["result"]


@pytest.mark.integration
def test_detail_vocabulary_is_shared_and_legacy_spelling_is_deprecated(corpus):
    args = {"session_id": "synthetic-0", "message_seq": 0, "preview_chars": 5}
    full = mcp_call(corpus, dict(args, detail="full"))
    assert not full.get("isError"), full
    legacy = mcp_call(corpus, dict(args, response_format="detailed"))
    assert full["structuredContent"] == legacy["structuredContent"]
    compact = mcp_call(corpus, dict(args, detail="compact"))
    assert len(full["structuredContent"]["messages"][0]["content"]) > len(compact["structuredContent"]["messages"][0]["content"])
    conflict = mcp_call(corpus, dict(args, detail="full", response_format="concise"))
    assert conflict["isError"]
    catalogue = cli(corpus, "mcp", "schema-budget", "--catalogue")
    tool = next(tool for tool in catalogue if tool["name"] == "get_session")
    assert tool["inputSchema"]["properties"]["response_format"]["description"].startswith("Deprecated: use detail")


def test_library_guide_runs_each_of_the_five_purposes(corpus, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    guide = (Path(__file__).resolve().parents[1] / "docs/development/library-api.md").read_text()
    section = guide.split("## Python", 1)[1].split("## Rust", 1)[0]
    examples = re.findall(r"```python\n(.*?)```", section, re.DOTALL)
    assert len(examples) == 5
    monkeypatch.setenv("AI_SESSION_SEARCH_DATABASE", str(corpus.db_path))
    monkeypatch.chdir(tmp_path)
    namespace: dict[str, Any] = {}
    for number, example in enumerate(examples):
        exec(compile(example, f"library-api.md:python-example-{number + 1}", "exec"), namespace)
    assert namespace["sessions"]
    assert namespace["message_page"].results
    assert namespace["recent_messages"]
    assert namespace["restored_path"].read_text() == "print(42)"
    assert namespace["corrections"]
    assert namespace["repeat_report"]["groups"]


def test_signed_windows_keep_head_tail_or_all(corpus):
    with sqlite3.connect(corpus.db_path) as db:
        db.execute("INSERT INTO transcripts (session_id, transcript_text) VALUES ('claude:synthetic-0', 'head\nmiddle\ntail')")
        db.execute("UPDATE messages SET content = 'head\nmiddle\ntail' WHERE session_id = 'claude:synthetic-0' AND seq = 0")
    for count, expected in [(1, "head"), (-1, "tail"), (0, "head\nmiddle\ntail")]:
        assert corpus.transcript("synthetic-0", lines=count)["text"] == expected
        assert corpus.read_session_messages("synthetic-0", seq_from=0, seq_to=0, lines_per_message=count)[0].content == expected
    first = corpus.inspect_session("synthetic-0", summary_items=1).user_intent
    last = corpus.inspect_session("synthetic-0", summary_items=-1).user_intent
    assert len(first) == len(last) == 1
    assert first[0].seq == 0 and last[0].seq == 2
    assert len(corpus.inspect_session("synthetic-0", summary_items=0).user_intent) == 3


@pytest.mark.parametrize("pattern", ["rate_%.py", "*/rate_%.py", "rate_*.py"])
def test_file_search_keeps_percent_and_underscore_literal(corpus, pattern):
    with sqlite3.connect(corpus.db_path) as db:
        for n, name in enumerate(["rate_%.py", "rateXY.py", "rate_12.py"]):
            db.execute(
                "INSERT INTO file_edits (session_id, provider, seq, tool, file_path, "
                "file_name, new_content) VALUES ('claude:synthetic-0', 'claude', ?, "
                "'Write', ?, ?, 'fixture')",
                (100 + n, f"/synthetic/{name}", name),
            )
    rows = corpus.search_files(pattern)
    expected = {"rate_%.py", "rate_12.py"} if pattern == "rate_*.py" else {"rate_%.py"}
    assert {row.file_name for row in rows} == expected
    assert [row.to_dict() for row in rows] == cli(corpus, "files", "search", pattern, "--format", "json")


def test_readme_and_classification_reference_python_examples(corpus, tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    root = Path(__file__).resolve().parents[1]
    monkeypatch.setenv("AI_SESSION_SEARCH_DATABASE", str(corpus.db_path))
    monkeypatch.chdir(tmp_path)
    for relative in ("README.md", "skills/ai-session-search/references/message-classification.md"):
        examples = re.findall(r"```python\n(.*?)```", (root / relative).read_text(), re.DOTALL)
        assert examples, relative
        for number, example in enumerate(examples):
            namespace: dict[str, Any] = {}
            exec(compile(example, f"{relative}:python-example-{number + 1}", "exec"), namespace)
            if "search" in namespace:
                namespace["search"].close()
            if relative == "README.md":
                assert isinstance(namespace["messages"], aise.MessageSearchResponse)
            else:
                assert namespace["report"].output.report.policies


@pytest.mark.parametrize("case", ["month-shorthand", "utc-date"])
def test_cli_preserves_published_date_interpretations(corpus, monkeypatch: pytest.MonkeyPatch, case):
    monkeypatch.setenv("TZ", "America/New_York")
    timestamp = (datetime.now(UTC) - timedelta(days=2)).isoformat() if case == "month-shorthand" else "2026-01-15T00:30:00Z"
    with sqlite3.connect(corpus.db_path) as db:
        db.execute(
            "INSERT INTO messages (session_id, provider, seq, role, kind, ts, content, authorship) "
            "VALUES ('claude:synthetic-0', 'claude', 50, 'user', 'conversation', ?, 'compatibility-date-boundary', 'human')",
            (timestamp,),
        )
    bounds = ["--since", "1m"] if case == "month-shorthand" else ["--when", "2026-01-15"]
    response = cli(corpus, "messages", "search", "compatibility-date-boundary", *bounds, "--all-results", "--format", "json")
    assert [row["message_ref"]["message_seq"] for row in response["results"]] == [50]
