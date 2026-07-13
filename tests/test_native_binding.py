from __future__ import annotations

import sqlite3
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

native = pytest.importorskip("ai_session_search.native", reason="native extension is not installed")


def test_package_root_promotes_rust_application_and_query_types() -> None:
    import ai_session_search as package

    assert package.SessionSearch is native.SessionSearch
    assert package.SessionQuery is native.SessionQuery
    assert package.MessageQuery is native.MessageQuery
    assert package.QueryScope is native.QueryScope
    assert package.ResolvedDateRange is native.ResolvedDateRange
    assert package.AnalysisPublicationPlan is native.AnalysisPublicationPlan


@pytest.mark.parametrize(
    ("expression", "expected_since", "expected_until"),
    [
        ("2026-01", "2026-01-01T00:00:00+00:00", "2026-01-31T23:59:59+00:00"),
        ("2024-02", "2024-02-01T00:00:00+00:00", "2024-02-29T23:59:59+00:00"),
        ("202X", "2020-01-01T00:00:00+00:00", "2029-12-31T23:59:59+00:00"),
        ("2026-01-X5", "2026-01-05T00:00:00+00:00", "2026-01-25T23:59:59+00:00"),
        ("2026-01-15T14", "2026-01-15T14:00:00+00:00", "2026-01-15T14:59:59+00:00"),
        ("2026-01/2026-03", "2026-01-01T00:00:00+00:00", "2026-03-31T23:59:59+00:00"),
        ("7d", "2026-06-08T12:00:00+00:00", "2026-06-15T12:00:00+00:00"),
    ],
)
def test_native_date_resolution_uses_canonical_rust_semantics(
    expression: str,
    expected_since: str,
    expected_until: str,
) -> None:
    resolved = native.DateRangeQuery(when=expression).resolve_bounds(
        reference_time="2026-06-15T12:00:00Z"
    )

    assert (resolved.since, resolved.until) == (expected_since, expected_until)


def test_native_date_resolution_supports_independent_bounds() -> None:
    resolved = native.DateRangeQuery(since="2026-01", until="2026-03").resolve_bounds(
        reference_time="2026-06-15T12:00:00Z"
    )

    assert resolved.since == "2026-01-01T00:00:00+00:00"
    assert resolved.until == "2026-03-31T23:59:59+00:00"


@pytest.mark.parametrize("expression", ["2026-01", "2024-02", "202X", "2026-01-X5"])
def test_legacy_date_adapter_selects_native_absolute_bounds(expression: str) -> None:
    from ai_session_search import parse_date_input

    resolved = native.DateRangeQuery(when=expression).resolve_bounds(
        reference_time="2026-06-15T12:00:00Z"
    )

    assert parse_date_input(expression, "start") == resolved.since.removesuffix("+00:00")
    assert parse_date_input(expression, "end") == resolved.until.removesuffix("+00:00")


def test_native_date_resolution_rejects_ambiguous_reference_time() -> None:
    with pytest.raises(ValueError, match="RFC 3339"):
        native.DateRangeQuery(when="7d").resolve_bounds(reference_time="2026-06-15")


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
    with pytest.raises(RuntimeError, match="no file edits found"):
        search.reconstruct_file_versions("missing.py", request=file_query)
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

    destination = tmp_path / "bundle"
    receipt = search.export_sessions(
        destination,
        native.SessionQuery(limit=0),
        format="markdown",
    )
    assert receipt.destination == destination
    assert receipt.format == "markdown"
    assert receipt.sessions == 1
    assert len(receipt.files) == 1
    assert receipt.files[0].parent == destination
    assert receipt.files[0].read_text(encoding="utf-8") == document.content
    with pytest.raises(ValueError, match="destination already exists"):
        search.export_sessions(destination)


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


def test_native_lifecycle_services_return_typed_rust_outcomes(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config_dir = tmp_path / "config"
    config_dir.mkdir()
    providers = ["claude", "claude-desktop", "codex", "cursor", "antigravity", "pi", "aistudio", "gemini-cli"]
    (config_dir / "config.toml").write_text(
        "\n".join(f"[providers.{provider}]\nenabled = false" for provider in providers),
        encoding="utf-8",
    )
    monkeypatch.setenv("AI_SESSION_SEARCH_CONFIG", str(config_dir / "config.toml"))
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)

    status = search.index_status()
    reindex = search.reindex()
    diagnostics = search.diagnostics()
    compact = search.compact()

    assert not status.parser_health.schema_current
    assert status.parser_health.indexed_sessions == 0
    assert status.repairable_stale_sessions == 0
    assert status.unavailable_stale_sessions == 0
    assert isinstance(status.parser_health.providers, list)
    assert isinstance(status.repair_commands, list)
    assert (reindex.files_seen, reindex.sessions_updated) == (0, 0)
    assert diagnostics.db_path == str(database)
    assert diagnostics.index_status.parser_health.schema_current
    assert [provider.provider for provider in diagnostics.providers] == providers
    assert all(not provider.enabled for provider in diagnostics.providers)
    assert all(provider.repairable_stale_sessions == 0 for provider in diagnostics.providers)
    assert all(provider.unavailable_stale_sessions == 0 for provider in diagnostics.providers)
    assert compact.before_bytes >= 0
    assert compact.after_bytes >= 0
    assert compact.reclaimed_bytes == max(0, compact.before_bytes - compact.after_bytes)


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
                (
                    "claude:analysis",
                    "claude",
                    0,
                    "user",
                    "2026-01-15T12:00:00+00:00",
                    "actually, that is wrong; see https://example.com/docs",
                ),
                ("claude:analysis", "claude", 1, "slash", "2026-01-15T12:01:00+00:00", "/plan verify migration"),
                ("codex:other", "codex", 0, "user", "2026-02-15T12:00:00+00:00", "unrelated"),
            ],
        )
        connection.executemany(
            """
            insert into file_edits (
                session_id, provider, seq, ts, tool, file_path, file_name, new_content, edits_json
            ) values (?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            [
                (
                    "claude:analysis", "claude", 0, "2026-01-15T12:02:00+00:00",
                    "Write", "/repo/jan.py", "jan.py", "fixture", None,
                ),
                (
                    "claude:analysis", "claude", 1, "2026-01-15T12:03:00+00:00",
                    "Edit", "/repo/jan.py", "jan.py", None,
                    '[{"old":"fixture","new":"fixture two","replace_all":false}]',
                ),
                (
                    "claude:analysis", "claude", 2, "2026-01-15T12:04:00+00:00",
                    "apply_patch", "/repo/jan.py", "jan.py", None, None,
                ),
                (
                    "claude:analysis", "claude", 3, "2026-01-15T12:05:00+00:00",
                    "Write", "/repo/jan.py", "jan.py", "reset", None,
                ),
                (
                    "codex:other", "codex", 0, "2026-02-15T12:02:00+00:00",
                    "Write", "/repo/feb.py", "feb.py", "fixture", None,
                ),
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
    selected_user_messages = search.search_messages(
        "wrong|missing",
        native.MessageQuery(
            scope=scope,
            selector=native.MessageSelector(
                role="user",
                kind="conversation",
                sequence=native.MessageSequenceRange(seq_from=0, seq_to=0),
            ),
        ),
        mode="regex",
    )
    fuzzy_user_messages = search.search_messages(
        "actully",
        native.MessageQuery(scope=scope, selector=native.MessageSelector(role="user")),
        mode="fuzzy",
    )
    context = search.message_context("analysis", 1, before=1, after=0)
    inspection = search.inspect_session("analysis", preview_chars=40, include_time_profile=True)
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
    reconstructed_version_iterator = search.reconstruct_file_versions(
        "jan.py",
        request=native.FileQueryRequest(scope=native.QueryScope(session_id="analysis")),
    )
    assert iter(reconstructed_version_iterator) is reconstructed_version_iterator
    reconstructed_versions = list(reconstructed_version_iterator)
    restored = reconstructed.restore(output_dir=tmp_path / "restored")
    publication = search.publish_file_versions(
        "jan.py",
        tmp_path / "published-versions",
        request=native.FileQueryRequest(scope=native.QueryScope(session_id="analysis")),
    )

    assert [(hit.provider, hit.content) for hit in corrections] == [
        ("claude", "actually, that is wrong; see https://example.com/docs")
    ]
    assert [(row.command, row.count) for row in planning] == [("/plan", 1)]
    assert {row.role: row.count for row in roles} == {"slash": 1, "user": 1}
    assert [(message.provider, message.seq) for message in messages] == [("claude", 0), ("claude", 1)]
    assert [(message.role, message.seq) for message in selected_user_messages] == [("user", 0)]
    assert [(message.role, message.seq) for message in fuzzy_user_messages] == [("user", 0)]
    assert [message.seq for message in context] == [0, 1]
    assert inspection.session.id == "claude:analysis"
    assert inspection.user_intent[0].preview == "actually, that is wrong; see https://..."
    assert inspection.refs[0].refs[0].host == "example.com"
    assert inspection.changed_files[0].file_path == "/repo/jan.py"
    assert inspection.time_profile is not None and inspection.time_profile.messages == 2
    assert any(command.startswith("aise messages timeline") for command in inspection.next_commands)
    assert [(file.file_name, file.edits) for file in files] == [("jan.py", 4)]
    assert restored.name == "jan.recovered.py"
    assert restored.read_text(encoding="utf-8") == "reset"
    assert [(item.version, item.content) for item in reconstructed_versions] == [
        (1, "fixture"),
        (2, "fixture two"),
        (4, "reset"),
    ]
    assert publication.destination == tmp_path / "published-versions"
    assert [path.name for path in publication.files] == ["jan_v1.py", "jan_v2.py", "jan_v4.py"]
    assert (publication.destination / "jan_v4.py").read_text(encoding="utf-8") == "reset"
    with pytest.raises(RuntimeError, match="already exists"):
        search.publish_file_versions(
            "jan.py",
            publication.destination,
            request=native.FileQueryRequest(scope=native.QueryScope(session_id="analysis")),
        )
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
    with pytest.raises(ValueError, match="greater than zero"):
        search.inspect_session("analysis", preview_chars=0)
    with pytest.raises(ValueError, match="unknown role"):
        native.MessageSelector(role="system")
    with pytest.raises(ValueError, match="unknown message kind"):
        native.MessageSelector(kind="chat")
    with pytest.raises(ValueError, match="unknown message search mode"):
        search.search_messages("wrong", mode="semantic")
    assert [
        message.seq
        for message in search.search_messages(
            "",
            native.MessageQuery(
                selector=native.MessageSelector(
                    sequence=native.MessageSequenceRange(seq_from=1)
                )
            ),
        )
    ] == [1]
    with pytest.raises(RuntimeError, match="must be <="):
        search.search_messages(
            "",
            native.MessageQuery(
                scope=scope,
                selector=native.MessageSelector(
                    sequence=native.MessageSequenceRange(seq_from=2, seq_to=1)
                ),
            ),
        )
    with pytest.raises(RuntimeError, match="requires field=tool_argument"):
        search.search_messages(
            "cargo",
            native.MessageQuery(
                selector=native.MessageSelector(
                    target=native.MessageSearchTarget(argument_path="/cmd")
                )
            ),
        )


def test_native_message_timeline_exposes_general_tool_arguments(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    with sqlite3.connect(database) as connection:
        connection.execute(
            """
            insert into sessions (
                id, provider, provider_session_id, updated_at, preview_text, source_path,
                parse_version, discovery_source
            ) values ('codex:tool-event', 'codex', 'tool-event',
                      '2026-03-01T12:00:00+00:00', '', '/codex.jsonl', 'test', 'fixture')
            """
        )
        connection.execute(
            """
            insert into messages (
                session_id, provider, seq, role, kind, ts, tool_name, content
            ) values (
                'codex:tool-event', 'codex', 0, 'tool', 'tool_call',
                '2026-03-01T12:00:00+00:00', 'exec_command',
                '{"args":{"cmd":"cargo test","request":{"path":"src/lib.rs"}},"kind":"tool_call","tool_name":"exec_command"}'
            )
            """
        )

    scope = native.QueryScope(session_id="tool-event")
    timeline = search.search_messages("", native.MessageQuery(scope=scope))
    argument_request = native.MessageQuery(
        scope=scope,
        selector=native.MessageSelector(
            kind="tool_call",
            target=native.MessageSearchTarget(
                field="tool_argument",
                argument_path="/request/path",
            ),
            tool="exec",
        ),
    )

    assert [(event.kind, event.tool_name, event.seq) for event in timeline] == [
        ("tool_call", "exec_command", 0)
    ]
    assert [event.seq for event in search.search_messages("src/lib.rs", argument_request)] == [0]
    assert [
        event.seq
        for event in search.search_messages(
            "exec_command",
            native.MessageQuery(
                scope=scope,
                selector=native.MessageSelector(
                    target=native.MessageSearchTarget(field="tool_name")
                ),
            ),
        )
    ] == [0]
    with pytest.raises(RuntimeError, match="RFC 6901"):
        search.search_messages(
            "cargo",
            native.MessageQuery(
                scope=scope,
                selector=native.MessageSelector(
                    target=native.MessageSearchTarget(
                        field="tool_argument",
                        argument_path="cmd",
                    )
                ),
            ),
        )
    with pytest.raises(RuntimeError, match="only compatible with kind=tool_call"):
        search.search_messages(
            "cargo",
            native.MessageQuery(
                scope=scope,
                selector=native.MessageSelector(
                    kind="conversation",
                    target=native.MessageSearchTarget(
                        field="tool_argument",
                        argument_path="/cmd",
                    ),
                ),
            ),
        )


def test_native_analysis_documents_page_indexed_user_text_with_typed_cursor(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    with sqlite3.connect(database) as connection:
        connection.executemany(
            """
            insert into sessions (
                id, provider, provider_session_id, updated_at, preview_text, source_path,
                parse_version, discovery_source
            ) values (?, ?, ?, '2026-04-01T12:00:00+00:00', '', ?, 'test', 'fixture')
            """,
            [
                ("claude:first", "claude", "first", "/claude-first.jsonl"),
                ("claude:second", "claude", "second", "/claude-second.jsonl"),
                ("codex:other", "codex", "other", "/codex.jsonl"),
            ],
        )
        connection.executemany(
            """
            insert into messages (session_id, provider, seq, role, content)
            values (?, ?, ?, ?, ?)
            """,
            [
                ("claude:first", "claude", 0, "user", "first request"),
                ("claude:first", "claude", 1, "assistant", "answer is not analysis input"),
                ("claude:first", "claude", 2, "user", "second request"),
                ("codex:other", "codex", 0, "user", "other provider"),
            ],
        )
        connection.execute("drop table transcripts")

    request = native.SessionQuery(provider="claude", limit=1)
    first = search.analysis_documents(request)
    second = search.analysis_documents(request, cursor=first.next_cursor)

    assert len(first.documents) == 1
    assert first.documents[0].session.id == "claude:first"
    assert first.documents[0].user_text == "first request second request"
    assert first.documents[0].first_user_text == "first request"
    assert first.documents[0].message_count == 3
    assert first.documents[0].user_message_count == 2
    assert isinstance(first.next_cursor, native.NativeAnalysisCursor)
    assert len(second.documents) == 1
    assert second.documents[0].session.id == "claude:second"
    assert second.documents[0].user_text == ""
    assert second.documents[0].first_user_text is None
    assert second.documents[0].message_count == 0
    assert second.documents[0].user_message_count == 0
    assert second.next_cursor is None

    with pytest.raises(RuntimeError, match="greater than zero"):
        search.analysis_documents(native.SessionQuery(limit=0))


def test_native_analyze_sessions_runs_rust_policy_over_full_corpus(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    with sqlite3.connect(database) as connection:
        connection.executemany(
            """
            insert into sessions (
                id, provider, provider_session_id, title, updated_at, preview_text, source_path,
                parse_version, discovery_source
            ) values (?, ?, ?, ?, '2026-04-01T12:00:00+00:00', '', ?, 'test', 'fixture')
            """,
            [
                ("claude:root", "claude", "root", "Root", "/claude-root.jsonl"),
                ("codex:root", "codex", "root", "Root", "/codex-root.jsonl"),
                (
                    "gemini-cli:child",
                    "gemini-cli",
                    "child",
                    "Branch of Root",
                    "/gemini-child.json",
                ),
            ],
        )
        connection.executemany(
            "insert into messages (session_id, provider, seq, role, content) values (?, ?, 0, 'user', ?)",
            [
                ("claude:root", "claude", "Use TDD"),
                ("gemini-cli:child", "gemini-cli", "Use TDD"),
            ],
        )

    policy = native.AnalysisPolicy(
        classification_rules=[
            native.ClassificationRule("technique", "tdd", r"(?i)\btdd\b", weight=7)
        ],
        relationship_rules=[
            native.RelationshipRule("branch_of", "branch", r"^Branch of (?P<parent>.+)$")
        ],
        phrase_vocabulary=native.PhraseVocabulary([2], 100, prose_only=True),
        max_classification_chars=100,
    )
    result = search.analyze_sessions(native.SessionQuery(limit=0), policy=policy)

    assert list(result.sessions) == ["claude:root", "codex:root", "gemini-cli:child"]
    child = result.sessions["gemini-cli:child"]
    assert child.score == 7
    assert child.message_count == 1
    assert child.user_message_count == 1
    assert [(item.dimension, item.label) for item in child.classifications] == [
        ("technique", "tdd")
    ]
    hint = child.relationship_hints[0]
    assert hint.status == "ambiguous"
    assert hint.resolved_session_id is None
    assert hint.candidate_session_ids == ["claude:root", "codex:root"]
    repeated = next(item for item in result.vocabulary if item.phrase == "use tdd")
    assert repeated.documents == 2
    assert repeated.occurrences == 2
    assert list(result.graph.nodes) == ["claude:root", "codex:root", "gemini-cli:child"]
    assert result.graph.edges == []
    assert result.graph.groups == []

    publication = native.AnalysisPublicationPlan(
        tmp_path / "analysis-bundle",
        ["json", "markdown"],
    )
    rendered = publication.render(result)
    assert publication.destination == tmp_path / "analysis-bundle"
    assert publication.formats == ["json", "markdown"]
    assert {artifact.name for artifact in rendered} == {
        "analysis.v1.json",
        "index.md",
        "knowledge-graph.md",
        "manifest.v1.json",
        "session-graph.v1.json",
        "taxonomy.md",
    }
    assert all(artifact.bytes == len(artifact.content.encode()) for artifact in rendered)
    assert all(len(artifact.sha256) == 64 for artifact in rendered)
    receipt = publication.publish(result)
    assert receipt.destination == tmp_path / "analysis-bundle"
    assert {artifact.name for artifact in receipt.artifacts} == {
        artifact.name for artifact in rendered
    }
    with pytest.raises(RuntimeError, match="destination already exists"):
        publication.publish(result)
    with pytest.raises(ValueError, match="at least one format"):
        native.AnalysisPublicationPlan(tmp_path / "empty", [])
    with pytest.raises(ValueError, match="unknown analysis publication format"):
        native.AnalysisPublicationPlan(tmp_path / "unknown", ["html"])

    with pytest.raises(ValueError, match="named 'parent' capture"):
        native.RelationshipRule("broken", "branch", r"Branch of (.+)")
    with pytest.raises(ValueError, match="phrase widths must be greater than zero"):
        native.PhraseVocabulary([0], 100)
    with pytest.raises(ValueError, match="max_classification_chars must be greater than zero"):
        native.AnalysisPolicy(max_classification_chars=0)
