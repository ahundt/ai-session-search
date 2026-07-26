from __future__ import annotations

import ast
import json
import sqlite3
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

native = pytest.importorskip("ai_session_search.native", reason="native extension is not installed")


def test_correction_match_field_names_are_pinned_across_rust_and_python() -> None:
    """Lock the Python spelling of every CorrectionMatch field.

    Two names differ from the Rust struct, and neither was pinned before, so a refactor on
    either side could have broken the Python contract silently.

    ``timestamp`` renames Rust's ``ts``. The repo states no rationale for this anywhere -- no
    doc, doc comment, or parity note -- so intent is inferred from consistency, not quoted:
    Python uses ``timestamp`` in all five timestamp-bearing result classes and ``ts`` in none
    (``_native.pyi:320,329,417,478,729``), and the binding applies the mapping at every
    conversion site. Five-for-five with no counterexample is a convention, not a slip, and the
    unabbreviated name is the better one to standardize on -- so the rename is kept.

    Note what that evidence does NOT say: it is Python that is uniform here, while **Rust**
    mixes spellings -- ``ts`` in five result structs but ``first_timestamp``/``last_timestamp``
    at ``models.rs:924-925``. The naming inconsistency worth fixing is inside Rust, not between
    Rust and Python. Tracked in the parameter-design sweep rather than fixed here, because
    renaming public Rust struct fields is a separate change with its own blast radius.

    ``matched_text`` is the *matched substring*, not the rule that matched it -- the value is
    ``Regex::find(..).as_str()``. It was ``matched_pattern``, which named the rule input while
    carrying the output.

    Asserting the ABSENCE of the old spellings matters as much as the presence of the new ones:
    a presence-only check lets a stale alias reappear alongside the correct name.
    """
    fields = {name for name in dir(native.CorrectionMatch) if not name.startswith("_")}
    assert fields == {
        "session_id",
        "provider",
        "timestamp",
        "policy_name",
        "category",
        "matched_text",
        "content",
    }
    assert "ts" not in fields, "Rust's `ts` must stay renamed to `timestamp` on this surface"
    assert "matched_pattern" not in fields, "the field names the matched text, not the rule"


def test_advanced_facade_exports_every_session_search_result_type() -> None:
    module_path = Path(native.__file__)
    stub = ast.parse(module_path.with_name("_native.pyi").read_text())
    facade_stub = ast.parse(module_path.with_suffix(".pyi").read_text())
    session_search = next(
        node
        for node in stub.body
        if isinstance(node, ast.ClassDef) and node.name == "SessionSearch"
    )
    extension_classes = {
        node.name for node in stub.body if isinstance(node, ast.ClassDef)
    }
    returned_result_types = {
        node.id
        for method in session_search.body
        if isinstance(method, ast.FunctionDef) and method.returns is not None
        for node in ast.walk(method.returns)
        if isinstance(node, ast.Name) and node.id in extension_classes
    }
    facade_stub_imports = {
        alias.name
        for node in facade_stub.body
        if isinstance(node, ast.ImportFrom)
        for alias in node.names
    }
    facade_stub_exports = next(
        ast.literal_eval(node.value)
        for node in facade_stub.body
        if isinstance(node, ast.Assign)
        and any(isinstance(target, ast.Name) and target.id == "__all__" for target in node.targets)
    )

    assert returned_result_types
    assert not any(name.startswith("Native") for name in extension_classes)
    assert not any(name.startswith("Native") for name in facade_stub_exports)
    assert returned_result_types <= facade_stub_imports
    assert returned_result_types <= set(facade_stub_exports)
    assert returned_result_types <= set(native.__all__)
    for name in native.__all__:
        doc = getattr(native, name).__doc__
        assert doc and doc.strip(), f"{name} must explain its purpose at runtime"
    for name in returned_result_types:
        assert getattr(native, name) is not None


def test_package_root_promotes_rust_application_and_query_types() -> None:
    import ai_session_search as package

    assert package.SessionSearch is native.SessionSearch
    assert package.SessionQuery is native.SessionQuery
    assert package.MessageSearchRequest is native.MessageSearchRequest
    assert package.MessageSearchResponse is native.MessageSearchResponse
    assert package.MessageScope is native.MessageScope
    assert package.QueryExclusions is native.QueryExclusions
    assert package.QueryScope is native.QueryScope
    assert package.ResolvedDateRange is native.ResolvedDateRange
    assert package.AnalysisPublicationPlan is native.AnalysisPublicationPlan
    assert package.__all__ == [
        "SessionSearch",
        "AnalysisPublicationPlan",
        "SessionQuery",
        "MessageSearchRequest",
        "MessageSearchResponse",
        "AnalysisQuery",
        "CorrectionQuery",
        "AnalysisPolicy",
        "ClassificationRule",
        "RelationshipRule",
        "PhraseVocabulary",
        "FileQuery",
        "MessageExclusions",
        "MessageScope",
        "QueryExclusions",
        "QueryScope",
        "ResolvedDateRange",
        "DateRange",
    ]
    for name in package.__all__:
        doc = getattr(package, name).__doc__
        assert doc and doc.strip(), f"{name} must explain its purpose at runtime"
    assert not hasattr(package, "AISession")
    assert not hasattr(package, "SessionRecoveryEngine")
    assert not hasattr(package, "parse_date_input")


@pytest.mark.parametrize(
    ("expression", "expected_since", "expected_until"),
    [
        ("2026-01", "2026-01-01T00:00:00+00:00", "2026-01-31T23:59:59.999999999+00:00"),
        ("2024-02", "2024-02-01T00:00:00+00:00", "2024-02-29T23:59:59.999999999+00:00"),
        ("202X", "2020-01-01T00:00:00+00:00", "2029-12-31T23:59:59.999999999+00:00"),
        ("2026-01-X5", "2026-01-05T00:00:00+00:00", "2026-01-25T23:59:59.999999999+00:00"),
        ("2026-01-15T14", "2026-01-15T14:00:00+00:00", "2026-01-15T14:59:59.999999999+00:00"),
        ("2026-01/2026-03", "2026-01-01T00:00:00+00:00", "2026-03-31T23:59:59.999999999+00:00"),
        (
            "2026-07-06T22:53:22.358-04:00",
            "2026-07-07T02:53:22.358+00:00",
            "2026-07-07T02:53:22.358+00:00",
        ),
        ("7d", "2026-06-08T12:00:00+00:00", "2026-06-15T12:00:00+00:00"),
    ],
)
def test_native_date_resolution_uses_canonical_rust_semantics(
    expression: str,
    expected_since: str,
    expected_until: str,
) -> None:
    resolved = native.DateRange(when=expression).resolve_bounds(
        reference_time="2026-06-15T12:00:00Z"
    )

    assert (resolved.since, resolved.until) == (expected_since, expected_until)


def test_native_date_resolution_supports_independent_bounds() -> None:
    resolved = native.DateRange(since="2026-01", until="2026-03").resolve_bounds(
        reference_time="2026-06-15T12:00:00Z"
    )

    assert resolved.since == "2026-01-01T00:00:00+00:00"
    assert resolved.until == "2026-03-31T23:59:59.999999999+00:00"


def test_native_date_resolution_rejects_ambiguous_reference_time() -> None:
    with pytest.raises(ValueError, match="RFC 3339"):
        native.DateRange(when="7d").resolve_bounds(reference_time="2026-06-15")


def test_native_session_search_is_typed_and_thread_safe(tmp_path: Path) -> None:
    search = native.SessionSearch(tmp_path / "index.db")
    session_query = native.SessionQuery(limit=3)
    message_query = native.MessageSearchRequest(limit=4)
    file_query = native.FileQuery(limit=5, offset=2)

    with ThreadPoolExecutor(max_workers=2) as executor:
        futures = [executor.submit(search.search_messages, "missing", message_query) for _ in range(2)]

    assert search.db_path == tmp_path / "index.db"
    assert [future.result().hits for future in futures] == [[], []]
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
    assert (session_query.limit, message_query.limit, file_query.limit, file_query.offset) == (
        3,
        4,
        5,
        2,
    )


def test_query_exclusions_are_explicit_and_shared() -> None:
    exclusions = native.QueryExclusions(
        path_prefixes=["./generated", "/tmp/cache"],
        session_ids=["claude:one", "codex:two"],
    )
    sessions = native.SessionQuery(
        exclusions=exclusions,
    )
    scope = native.QueryScope(exclusions=exclusions)

    assert sessions.exclusions.path_prefixes == ["./generated", "/tmp/cache"]
    assert sessions.exclusions.session_ids == ["claude:one", "codex:two"]
    assert scope.exclusions.path_prefixes == ["./generated", "/tmp/cache"]
    assert scope.exclusions.session_ids == ["claude:one", "codex:two"]


def test_session_query_selects_session_classes_and_follows_the_spawn_link(
    tmp_path: Path,
) -> None:
    """Subagent runs are sessions of their own, selectable by class from Python.

    The two spellings are the providers' own: Codex records this distinction as
    ``thread_source: user | subagent``. ``agent`` is deliberately not accepted, because a
    subagent is also an agent and the value would not say which set it selects.
    """
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.executemany(
            """
            insert into sessions (
                id, provider, provider_session_id, updated_at, preview_text, source_path,
                parse_version, discovery_source, parent_session_id, agent_label
            ) values (?, 'claude', ?, ?, '', ?, 'test', 'fixture', ?, ?)
            """,
            [
                ("claude:parent", "parent", "2026-03-01T12:00:00+00:00", "/p.jsonl", None, None),
                (
                    "claude:parent/agent-a",
                    "parent/agent-a",
                    "2026-03-02T12:00:00+00:00",
                    "/a.jsonl",
                    "claude:parent",
                    "Explore",
                ),
            ],
        )
        connection.commit()
    finally:
        connection.close()

    def ids(**kwargs: object) -> list[str]:
        return sorted(
            session.id for session in search.list_sessions(native.SessionQuery(**kwargs))
        )

    assert ids() == ["claude:parent", "claude:parent/agent-a"], "both classes by default"
    assert ids(session_kinds=["user"]) == ["claude:parent"]
    assert ids(session_kinds=["user", "user"]) == ["claude:parent"]
    assert ids(session_kinds=["subagent"]) == ["claude:parent/agent-a"]
    assert ids(session_kinds=[]) == [], "deselecting every class matches nothing"

    # parent_session_id holds the parent row's whole id, so the value a caller already has
    # from a listing is the value that selects that session's runs.
    assert ids(parent_session_id="claude:parent") == ["claude:parent/agent-a"]

    spawned = search.list_sessions(native.SessionQuery(session_kinds=["subagent"]))[0]
    assert spawned.parent_session_id == "claude:parent"
    assert spawned.agent_label == "Explore"

    # The set round-trips through the getter as the canonical spellings.
    assert native.SessionQuery(session_kinds=["subagent"]).session_kinds == ["subagent"]
    assert native.SessionQuery().session_kinds is None

    with pytest.raises(ValueError, match="unknown session kind: agent") as raised:
        search.list_sessions(native.SessionQuery(session_kinds=["agent"]))
    assert '"user"' in str(raised.value)
    assert '"subagent"' in str(raised.value)


def test_native_session_search_rejects_empty_database_path() -> None:
    with pytest.raises(ValueError, match="db_path must not be empty") as raised:
        native.SessionSearch("")
    assert "pass a non-empty path" in str(raised.value)


def test_native_session_search_rejects_zero_threads(tmp_path: Path) -> None:
    with pytest.raises(ValueError, match="threads must be greater than zero") as raised:
        native.SessionSearch(tmp_path / "index.db", threads=0)
    assert "threads=None" in str(raised.value)


def test_native_query_rejects_unknown_provider(tmp_path: Path) -> None:
    search = native.SessionSearch(tmp_path / "index.db")

    # The rejection names the supplied value and every accepted provider. It is deliberately not
    # wrapped in an "invalid provider: " prefix, which only produced the doubled reading
    # "invalid provider: unsupported provider: unknown".
    with pytest.raises(ValueError, match="unsupported provider: unknown") as raised:
        search.list_sessions(native.SessionQuery(provider="unknown"))
    assert 'must be one of "claude"' in str(raised.value)
    assert '"gemini-cli"' in str(raised.value)


def test_native_session_search_uses_configured_current_repo_when_omitted(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    current_repo = tmp_path / "current"
    other_repo = tmp_path / "other"
    (current_repo / ".git").mkdir(parents=True)
    other_repo.mkdir()
    monkeypatch.chdir(current_repo)

    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.executemany(
            """
            insert into sessions (
                id, provider, provider_session_id, title, repo_root, preview_text,
                source_path, parse_version, discovery_source
            ) values (?, 'claude', ?, 'shared needle', ?, '', ?, 'test', 'fixture')
            """,
            [
                ("claude:a-other", "a-other", str(other_repo), "/other.jsonl"),
                ("claude:z-current", "z-current", str(current_repo), "/current.jsonl"),
            ],
        )
        connection.commit()
    finally:
        connection.close()

    implicit = search.search_sessions("needle", native.SessionQuery(limit=2))
    assert [hit.session.id for hit in implicit] == ["claude:z-current", "claude:a-other"]

    explicit = search.search_sessions(
        "needle", native.SessionQuery(current_repo=str(other_repo), limit=2)
    )
    assert [hit.session.id for hit in explicit] == ["claude:a-other", "claude:z-current"]


def test_native_export_returns_rust_document_without_writing(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
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
        connection.commit()
    finally:
        connection.close()

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


def test_native_constructor_uses_explicit_precedence_and_empty_provider_paths(
    tmp_path: Path,
) -> None:
    configured_database = tmp_path / "configured.db"
    explicit_database = tmp_path / "explicit.db"
    config_path = tmp_path / "config.toml"
    config_path.write_text(
        f"[index]\ndb_path = {str(configured_database)!r}\n"
        "[providers.codex]\nenabled = true\npaths = []\n",
        encoding="utf-8",
    )

    search = native.SessionSearch(
        explicit_database,
        config_path=config_path,
        cache_dir=tmp_path / "cache",
    )

    assert search.db_path == explicit_database
    codex = next(item for item in search.source_inventory() if item.provider == "codex")
    assert codex.enabled
    assert codex.roots == []


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

    assert status.parser_health.schema_current
    assert status.parser_health.indexed_sessions == 0
    assert status.repairable_stale_sessions == 0
    assert status.unavailable_stale_sessions == 0
    assert isinstance(status.parser_health.providers, list)
    assert isinstance(status.repair_commands, list)
    assert status.index_update is None
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


def test_native_full_reindex_promotes_v3_and_releases_exclusive_lock(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = tmp_path / "config.toml"
    providers = [
        "claude",
        "claude-desktop",
        "codex",
        "cursor",
        "antigravity",
        "pi",
        "aistudio",
        "gemini-cli",
    ]
    config.write_text(
        "\n".join(f"[providers.{provider}]\nenabled = false" for provider in providers),
        encoding="utf-8",
    )
    monkeypatch.setenv("AI_SESSION_SEARCH_CONFIG", str(config))
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    del search

    # `with sqlite3.connect(...) as connection:` only commits/rolls back on exit — it does NOT
    # close the connection, so a stray handle would otherwise stay open (via the still-referenced
    # `connection` name) while `reindex(full=True)` below tears down and rebuilds WAL mode on the
    # same file. On Windows that stray handle blocks the WAL sidecar file operations the
    # migration performs, so the connection is closed explicitly here.
    connection = sqlite3.connect(database)
    try:
        connection.executescript(
            """
            drop trigger messages_ai;
            drop trigger messages_ad;
            drop trigger messages_au;
            drop table messages_trigram_vocab;
            drop table messages_trigram;
            pragma user_version=3;
            """
        )
        connection.commit()
    finally:
        connection.close()

    search = native.SessionSearch(database)
    outcome = search.reindex(full=True)

    assert (outcome.files_seen, outcome.sessions_updated) == (0, 0)
    assert search.search_messages("missing", native.MessageSearchRequest(limit=1)).hits == []
    del search
    inspection = json.loads(
        subprocess.check_output(
            [
                sys.executable,
                "-c",
                """import json, sqlite3, sys
with sqlite3.connect(f'file:{sys.argv[1]}?mode=ro', uri=True) as db:
    print(json.dumps({
        'version': db.execute('pragma user_version').fetchone()[0],
        'journal_mode': db.execute('pragma journal_mode').fetchone()[0],
        'objects': sorted(row[0] for row in db.execute(
            \"select name from sqlite_schema where name in \"
            \"('messages_trigram', 'messages_trigram_vocab', 'trigram_postings')\"
        )),
    }))
""",
                str(database),
            ],
            text=True,
        )
    )
    assert inspection == {
        "version": 4,
        "journal_mode": "wal",
        "objects": ["messages_trigram", "messages_trigram_vocab"],
    }


def test_native_analysis_is_typed_scoped_and_index_backed(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
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
        connection.commit()
    finally:
        connection.close()

    scope = native.QueryScope(provider="claude", session_id="analysis")
    message_scope = native.MessageScope(provider="claude", session_id="analysis")
    request = native.AnalysisQuery(scope=scope, limit=10)
    # Corrections take their OWN request type: they need a session-class filter, a policy
    # selection, and an offset, none of which aggregate analysis has.
    corrections_report = search.corrections(native.CorrectionQuery(scope=scope, limit=10))
    corrections = corrections_report.matches
    assert [receipt.name for receipt in corrections_report.policies] == [
        "ai-session-search"
    ], "a default run evaluates exactly the embedded policy and says so"
    assert len(corrections_report.policies[0].sha256) == 64
    planning = search.planning(request, ["^/plan$"])
    roles = search.role_statistics(request)
    messages = search.search_messages(
        "",
        native.MessageSearchRequest(scope=message_scope, limit=10),
    ).hits
    selected_user_messages = search.search_messages(
        "wrong|missing",
        native.MessageSearchRequest(
            scope=message_scope,
            role="user",
            kind="conversation",
            seq_from=0,
            seq_to=0,
        ),
        query_mode="regex",
    ).hits
    fuzzy_user_messages = search.search_messages(
        "actully",
        native.MessageSearchRequest(scope=message_scope, role="user", limit=10),
        query_mode="fuzzy",
    ).hits
    context = search.message_context("analysis", 1, context_before=1, context_after=0)
    inspection = search.inspect_session("analysis", preview_chars=40, include_time_profile=True)
    files = search.search_files(
        "*.py",
        native.FileQuery(
            scope=native.QueryScope(
                provider="claude",
                session_id="analysis",
                dates=native.DateRange(when="2026-01"),
            )
        ),
    )
    reconstructed = search.reconstruct_file(
        "jan.py",
        request=native.FileQuery(scope=native.QueryScope(session_id="analysis")),
    )
    history_page = search.file_history(
        "jan.py",
        native.FileQuery(
            scope=native.QueryScope(session_id="analysis"),
            limit=1,
            offset=1,
        ),
    )
    reconstructed_version_iterator = search.reconstruct_file_versions(
        "jan.py",
        request=native.FileQuery(scope=native.QueryScope(session_id="analysis")),
    )
    assert iter(reconstructed_version_iterator) is reconstructed_version_iterator
    reconstructed_versions = list(reconstructed_version_iterator)
    restored = reconstructed.restore(output_dir=tmp_path / "restored")
    publication = search.publish_file_versions(
        "jan.py",
        tmp_path / "published-versions",
        request=native.FileQuery(scope=native.QueryScope(session_id="analysis")),
    )

    assert [(hit.provider, hit.content) for hit in corrections] == [
        ("claude", "actually, that is wrong; see https://example.com/docs")
    ]
    assert corrections[0].policy_name == "ai-session-search"

    # The three things CorrectionQuery adds over AnalysisQuery must each reach the behavior;
    # accepted-and-ignored is the failure mode a type change alone would not catch.
    assert (
        search.corrections(
            native.CorrectionQuery(scope=scope, offset=1)
        ).matches
        == []
    ), "offset must skip the only match, not be accepted and dropped"
    assert (
        search.corrections(
            native.CorrectionQuery(scope=scope, session_kinds=[])
        ).matches
        == []
    ), "an empty session-class set matches nothing, exactly as it does on search"
    with pytest.raises(RuntimeError, match="unknown skill 'not-installed'"):
        search.corrections(native.CorrectionQuery(scope=scope, skills=["not-installed"]))
    for bad, argument in ((-1, "limit"), (-5, "offset")):
        kwargs = {argument: bad}
        with pytest.raises(ValueError, match=f"{argument} must be 0 or greater, got {bad}"):
            native.CorrectionQuery(scope=scope, **kwargs)
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
    assert inspection.truncated_evidence == []
    assert inspection.time_profile is not None and inspection.time_profile.messages == 2
    assert any(command.startswith("aise messages timeline") for command in inspection.next_commands)
    assert [(file.file_name, file.edits) for file in files] == [("jan.py", 4)]
    assert [(version.version, version.tool) for version in history_page] == [(2, "Edit")]
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
            request=native.FileQuery(scope=native.QueryScope(session_id="analysis")),
        )
    assert len(search.role_statistics(native.AnalysisQuery(scope=native.QueryScope(provider="claude"), limit=1))) == 1
    assert [
        session.id
        for session in search.list_sessions(native.SessionQuery(dates=native.DateRange(when="2026-01")))
    ] == ["claude:analysis"]
    assert search.search_messages(
        "",
        native.MessageSearchRequest(
            scope=native.MessageScope(dates=native.DateRange(when="1999"))
        ),
    ).hits == []
    with pytest.raises(TypeError, match="session"):
        native.QueryScope(session="fuzzy")
    with pytest.raises(ValueError, match="unsupported provider: unknown"):
        native.QueryScope(provider="unknown")
    with pytest.raises(ValueError, match="when is mutually exclusive"):
        native.DateRange(since="2026", when="2026-01")
    with pytest.raises(ValueError):
        search.list_sessions(native.SessionQuery(dates=native.DateRange(when="2026-13-40")))
    with pytest.raises(ValueError, match="must be non-negative"):
        search.message_context("analysis", 0, context_before=-1)
    with pytest.raises(ValueError, match="greater than zero"):
        search.inspect_session("analysis", preview_chars=0)
    with pytest.raises(ValueError, match="summary_items cannot be i64::MIN"):
        search.inspect_session("analysis", summary_items=-(2**63))
    with pytest.raises(ValueError, match="unknown role"):
        native.MessageSearchRequest(role="system")
    with pytest.raises(ValueError, match="unknown message kind"):
        native.MessageSearchRequest(kind="chat")
    with pytest.raises(ValueError, match="query_mode must be"):
        search.search_messages("wrong", query_mode="semantic")
    assert [
        message.seq
        for message in search.search_messages(
            "",
            native.MessageSearchRequest(scope=message_scope, seq_from=1),
        ).hits
    ] == [1]
    with pytest.raises(ValueError, match="seq_from 2 exceeds seq_to 1"):
        search.search_messages(
            "",
            native.MessageSearchRequest(
                scope=message_scope,
                seq_from=2,
                seq_to=1,
            ),
        )
    with pytest.raises(ValueError, match="requires field='tool_argument'"):
        search.search_messages(
            "cargo",
            native.MessageSearchRequest(argument_path="/cmd"),
        )


def test_native_lines_per_message_caps_each_message_head_or_tail(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            """
            insert into sessions (
                id, provider, provider_session_id, updated_at, preview_text, source_path,
                parse_version, discovery_source
            ) values ('claude:capped', 'claude', 'capped', '2026-01-15T12:00:00+00:00', '',
                      '/capped.jsonl', 'test', 'fixture')
            """
        )
        connection.execute(
            """
            insert into messages (session_id, provider, seq, role, kind, ts, content)
            values ('claude:capped', 'claude', 0, 'tool', 'tool_result',
                    '2026-01-15T12:00:00+00:00', ?)
            """,
            ("needle opening line\nmiddle detail\nfinal exit status 0",),
        )
        connection.commit()
    finally:
        connection.close()

    full = search.search_messages("needle", native.MessageSearchRequest())
    assert full.hits[0].content == "needle opening line\nmiddle detail\nfinal exit status 0"

    head = search.search_messages(
        "needle", native.MessageSearchRequest(lines_per_message=1)
    )
    assert head.hits[0].content == "needle opening line"

    tail = search.message_context("capped", 0, context_before=0, context_after=0, lines_per_message=-1)
    assert tail[0].content == "final exit status 0"


def test_native_harness_notice_keeps_its_typed_kind_after_database_read(
    tmp_path: Path,
) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            """
            insert into sessions (
                id, provider, provider_session_id, preview_text, source_path,
                parse_version, discovery_source
            ) values ('claude:notice', 'claude', 'notice', '', '/notice.jsonl', 'test', 'fixture')
            """
        )
        connection.execute(
            """
            insert into messages (session_id, provider, seq, role, kind, content)
            values ('claude:notice', 'claude', 0, 'user', 'harness_notice',
                    'Stop hook feedback: CANNOT STOP')
            """
        )
        connection.commit()
    finally:
        connection.close()

    assert search.search_messages(
        "CANNOT STOP", native.MessageSearchRequest()
    ).hits == [], "harness notices stay excluded by default"
    hits = search.search_messages(
        "CANNOT STOP",
        native.MessageSearchRequest(kind="harness_notice"),
    ).hits
    assert len(hits) == 1
    assert hits[0].kind == "harness_notice"


def test_native_message_search_covers_three_modes_by_three_fields(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            """
            insert into sessions (
                id, provider, provider_session_id, preview_text, source_path,
                parse_version, discovery_source
            ) values ('claude:matrix', 'claude', 'matrix', '', '/matrix.jsonl', 'test', 'fixture')
            """
        )
        connection.executemany(
            """
            insert into messages (
                session_id, provider, seq, role, kind, tool_name, content
            ) values ('claude:matrix', 'claude', ?, 'tool', 'tool_call', ?, ?)
            """,
            [
                (
                    0,
                    "exec_command",
                    '{"args":{"cmd":"cargo test --workspace","url":"https://example.com"},"kind":"tool_call","tool_name":"exec_command"}',
                ),
                (
                    1,
                    "read_file",
                    '{"args":{"cmd":"open notes.md"},"kind":"tool_call","tool_name":"read_file"}',
                ),
            ],
        )
        connection.commit()
    finally:
        connection.close()

    cases = [
        ("content", "literal", "cargo test"),
        ("content", "regex", r"cargo\s+test"),
        ("content", "fuzzy", "crgo tst"),
        ("tool_name", "literal", "exec"),
        ("tool_name", "regex", r"^exec_"),
        ("tool_name", "fuzzy", "excmd"),
        ("tool_argument", "literal", "cargo test"),
        ("tool_argument", "regex", r"cargo\s+test"),
        ("tool_argument", "fuzzy", "crgo tst"),
    ]
    for field, mode, query in cases:
        request = native.MessageSearchRequest(
            scope=native.MessageScope(session_id="claude:matrix"),
            kind="tool_call",
            field=field,
            argument_path="/cmd" if field == "tool_argument" else None,
            limit=10,
        )
        hits = search.search_messages(query, request, query_mode=mode).hits
        assert [(hit.session_id, hit.seq) for hit in hits] == [("claude:matrix", 0)], (field, mode)
        if mode == "fuzzy":
            assert hits[0].fuzzy_score is not None

    first_page = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(
            scope=native.MessageScope(session_id="claude:matrix"),
            limit=1,
            context=1,
            include_refs=True,
            lines_per_message=1,
            receipt_level="full",
        ),
    )
    assert [(hit.session_id, hit.seq) for hit in first_page.hits] == [("claude:matrix", 0)]
    assert [[hit.seq for hit in window] for window in first_page.context_windows] == [[0, 1]]
    assert (first_page.limit, first_page.offset, first_page.next_offset) == (1, 0, 1)
    assert first_page.ordering == "session-sequence"
    assert (first_page.context_before, first_page.context_after) == (1, 1)
    assert first_page.include_refs is True
    assert first_page.lines_per_message == 1
    assert [reference.host for reference in first_page.hits[0].refs] == ["example.com"]
    assert first_page.hits[0].ref_summary == "url"
    assert [reference.host for reference in first_page.context_windows[0][0].refs] == [
        "example.com"
    ]
    assert first_page.search_explain is not None
    assert first_page.search_explain.corpus == 2
    assert first_page.origins is not None
    assert first_page.origins.limit.source == "explicit"
    assert first_page.origins.context_before.source == "explicit"
    assert first_page.origins.include_refs.source == "explicit"
    assert first_page.origins.lines_per_message.source == "explicit"
    assert first_page.origins.receipt_level.source == "explicit"

    second_page = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(
            scope=native.MessageScope(session_id="claude:matrix"),
            limit=1,
            offset=first_page.next_offset,
        ),
    )
    assert [hit.seq for hit in second_page.hits] == [1]
    assert second_page.next_offset is None
    assert second_page.search_explain is None
    assert second_page.origins is None

    all_from_second = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(all_results=True, offset=1),
    )
    assert all_from_second.limit is None
    assert all_from_second.offset == 1
    assert [hit.seq for hit in all_from_second.hits] == [1]

    defaults = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(receipt_level="full"),
    )
    assert defaults.limit is None
    assert defaults.origins is not None
    assert defaults.origins.limit.source == "typed-default"
    assert defaults.origins.limit.surface is None

    fuzzy_offset = search.search_messages(
        "tolcal",
        native.MessageSearchRequest(
            scope=native.MessageScope(session_id="claude:matrix"),
            limit=1,
            offset=1,
        ),
        query_mode="fuzzy",
    )
    assert fuzzy_offset.offset == 1
    assert fuzzy_offset.ordering == "fuzzy-relevance"

    summary = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(receipt_level="summary", limit=1),
    )
    assert summary.search_explain is not None
    assert summary.origins is None


def test_native_message_timeline_exposes_general_tool_arguments(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
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
        connection.commit()
    finally:
        connection.close()

    scope = native.MessageScope(session_id="tool-event")
    timeline = search.search_messages("", native.MessageSearchRequest(scope=scope)).hits
    argument_request = native.MessageSearchRequest(
        scope=scope,
        kind="tool_call",
        field="tool_argument",
        argument_path="/request/path",
        tool_name_contains="exec",
    )

    assert [(event.kind, event.tool_name, event.seq) for event in timeline] == [
        ("tool_call", "exec_command", 0)
    ]
    assert [
        event.seq for event in search.search_messages("src/lib.rs", argument_request).hits
    ] == [0]
    assert [
        event.seq
        for event in search.search_messages(
            "exec_command",
            native.MessageSearchRequest(scope=scope, field="tool_name"),
        ).hits
    ] == [0]
    with pytest.raises(ValueError, match="RFC 6901"):
        search.search_messages(
            "cargo",
            native.MessageSearchRequest(
                scope=scope,
                field="tool_argument",
                argument_path="cmd",
            ),
        )
    # Class selection is a set, so the rejection names `tool_call` and the selected set rather
    # than a single `kind=` value. Asserting both halves: naming only the parameter would pass
    # on a message that never says which class is required.
    with pytest.raises(ValueError, match="tool_call"):
        search.search_messages(
            "cargo",
            native.MessageSearchRequest(
                scope=scope,
                kind="conversation",
                field="tool_argument",
                argument_path="/cmd",
            ),
        )
    with pytest.raises(ValueError, match="selected kinds"):
        search.search_messages(
            "cargo",
            native.MessageSearchRequest(
                scope=scope,
                kinds=["conversation"],
                field="tool_argument",
                argument_path="/cmd",
            ),
        )


def test_native_message_scope_keeps_workspace_and_transcript_paths_independent(
    tmp_path: Path,
) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.executemany(
            """
            insert into sessions (
                id, provider, provider_session_id, cwd, source_path, preview_text,
                parse_version, discovery_source
            ) values (?, 'claude', ?, ?, ?, '', 'test', 'fixture')
            """,
            [
                ("claude:aa", "aa", "/work/a", "/logs/a/session-aa.jsonl"),
                ("claude:ab", "ab", "/work/a", "/logs/b/session-ab.jsonl"),
                ("claude:ba", "ba", "/work/b", "/logs/a/session-ba.jsonl"),
            ],
        )
        connection.executemany(
            """
            insert into messages (session_id, provider, seq, role, kind, content)
            values (?, 'claude', 0, 'user', 'conversation', 'needle')
            """,
            [("claude:aa",), ("claude:ab",), ("claude:ba",)],
        )
        connection.commit()
    finally:
        connection.close()

    def ids(scope) -> list[str]:
        response = search.search_messages("needle", native.MessageSearchRequest(scope=scope))
        return [hit.session_id for hit in response.hits]

    assert ids(native.MessageScope(workspace_path_prefix="/work/a")) == [
        "claude:aa",
        "claude:ab",
    ]
    assert ids(native.MessageScope(transcript_path_prefix="/logs/a")) == [
        "claude:aa",
        "claude:ba",
    ]
    assert ids(
        native.MessageScope(
            workspace_path_prefix="/work/a",
            transcript_path_prefix="/logs/a",
        )
    ) == ["claude:aa"]
    assert ids(
        native.MessageScope(
            workspace_path_prefix="/work/a",
            exclusions=native.MessageExclusions(session_ids=["claude:aa"]),
        )
    ) == ["claude:ab"]


def test_native_read_session_messages_orders_ranges_and_paginates(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            """
            insert into sessions (
                id, provider, provider_session_id, updated_at, preview_text, source_path,
                parse_version, discovery_source
            ) values ('claude:reads', 'claude', 'reads', '2026-01-15T12:00:00+00:00', '',
                      '/reads.jsonl', 'test', 'fixture')
            """
        )
        connection.executemany(
            """
            insert into messages (session_id, provider, seq, role, kind, ts, content)
            values ('claude:reads', 'claude', ?, ?, 'conversation', ?, ?)
            """,
            [
                (0, "user", "2026-01-15T12:00:00+00:00", "turn 0"),
                (1, "assistant", "2026-01-15T12:01:00+00:00", "turn 1"),
                (2, "user", "2026-01-15T12:02:00+00:00", "turn 2"),
                (3, "slash", "2026-01-15T12:03:00+00:00", "turn 3"),
            ],
        )
        connection.commit()
    finally:
        connection.close()

    def seqs(hits):
        return [h.seq for h in hits]

    # order drives SELECTION; newest is reversed back to chronological.
    assert seqs(search.read_session_messages("reads")) == [0, 1, 2, 3]
    assert seqs(search.read_session_messages("reads", order="newest", limit=2)) == [2, 3]
    assert seqs(search.read_session_messages("reads", order="oldest", limit=2)) == [0, 1]
    # role composes with the newest-N window.
    assert seqs(
        search.read_session_messages("reads", order="newest", role="user", limit=1)
    ) == [2]
    # inclusive seq range is the non-overlapping chunked-read primitive.
    assert seqs(search.read_session_messages("reads", seq_from=1, seq_to=2)) == [1, 2]
    # offset paginates the oldest-first window.
    assert seqs(search.read_session_messages("reads", limit=2, offset=1)) == [1, 2]
    # limit 0 = all.
    assert seqs(search.read_session_messages("reads", limit=0)) == [0, 1, 2, 3]

    # Failure modes: invalid order, from>to, and a negative count are all rejected up front.
    with pytest.raises(ValueError, match="order must be"):
        search.read_session_messages("reads", order="recent")
    with pytest.raises(ValueError):
        search.read_session_messages("reads", seq_from=3, seq_to=1)
    with pytest.raises(ValueError):
        search.read_session_messages("reads", limit=-5)


def test_native_analysis_documents_page_indexed_user_text_with_typed_cursor(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
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
        connection.commit()
    finally:
        connection.close()

    request = native.SessionQuery(provider="claude", limit=1)
    first = search.analysis_documents(request)
    second = search.analysis_documents(request, cursor=first.next_cursor)

    assert len(first.documents) == 1
    assert first.documents[0].session.id == "claude:first"
    assert first.documents[0].user_text == "first request second request"
    assert first.documents[0].first_user_text == "first request"
    assert first.documents[0].message_count == 3
    assert first.documents[0].user_message_count == 2
    assert isinstance(first.next_cursor, native.AnalysisCursor)
    assert len(second.documents) == 1
    assert second.documents[0].session.id == "claude:second"
    assert second.documents[0].user_text == ""
    assert second.documents[0].first_user_text is None
    assert second.documents[0].message_count == 0
    assert second.documents[0].user_message_count == 0
    assert second.next_cursor is None

    with pytest.raises(RuntimeError, match="greater than zero"):
        search.analysis_documents(native.SessionQuery(limit=0))


def test_native_analyze_runs_rust_policy_over_full_corpus(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
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
        connection.commit()
    finally:
        connection.close()

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
    result = search.analyze(native.SessionQuery(limit=0), policy=policy)

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
    # A genuinely malformed pattern (unclosed character class) must surface the underlying
    # regex parser diagnostic, not just the top-level "invalid ... regex for rule" context —
    # regression lock for the anyhow chain-loss bug fixed in RelationshipRule::new. Neither
    # "regex parse error" nor "unclosed character class" appears in the rule id or the
    # context template, so this only passes when the full error chain is preserved.
    with pytest.raises(ValueError, match="invalid relationship regex for rule 'malformed'") as raised:
        native.RelationshipRule("malformed", "branch", r"(?P<parent>[")
    assert "regex parse error" in str(raised.value)
    assert "unclosed character class" in str(raised.value)
    with pytest.raises(ValueError, match="phrase widths must be greater than zero"):
        native.PhraseVocabulary([0], 100)
    with pytest.raises(ValueError, match="max_classification_chars must be greater than zero"):
        native.AnalysisPolicy(max_classification_chars=0)


@pytest.mark.parametrize(
    ("factory", "field"),
    [
        (native.SessionQuery, "limit"),
        (native.MessageSearchRequest, "offset"),
        (native.AnalysisQuery, "limit"),
        (native.FileQuery, "limit"),
        (native.FileQuery, "offset"),
    ],
)
def test_negative_paging_arguments_name_the_parameter_bound_and_meaning_of_zero(
    factory, field: str
) -> None:
    """A negative limit/offset must say what to pass instead, not only that it was rejected.

    PyO3's `usize` conversion raises `OverflowError: can't convert negative int to unsigned`,
    naming neither the parameter nor the bound. Naming the bound alone is still not actionable,
    because `0` is not merely the floor: legacy query types use `limit=0` for every match while
    the message request uses explicit `all_results=True`; `offset=0` starts at the first result.
    """
    with pytest.raises(ValueError) as raised:
        factory(**{field: -5})

    message = str(raised.value)
    assert field in message, message
    assert "0 or greater" in message, message
    assert "-5" in message, message
    expected_guidance = {
        "limit": "0 for every match",
        "offset": "0 to start at the first",
    }[field]
    assert expected_guidance in message, message

    # No unrelated presentation parameter is named. These paging fields span query types with
    # different presentation controls, so redirecting to one would be misleading.
    for elsewhere in ("lines_per_message", "transcript_lines", "summary_items"):
        assert elsewhere not in message, message

    # Guidance states accepted values, never an absence: phrases like "no negative" are double
    # negatives a reader can invert into the opposite instruction.
    assert "no negative" not in message, message
    assert "not negative" not in message, message


@pytest.mark.parametrize(
    ("factory", "field"),
    [
        (native.SessionQuery, "limit"),
        (native.MessageSearchRequest, "offset"),
        (native.AnalysisQuery, "limit"),
        (native.FileQuery, "limit"),
        (native.FileQuery, "offset"),
    ],
)
def test_zero_and_positive_paging_arguments_are_still_accepted(factory, field: str) -> None:
    """Zero keeps its documented meaning; the validation must only reject negatives."""
    assert getattr(factory(**{field: 0}), field) == 0
    assert getattr(factory(**{field: 7}), field) == 7


def test_message_search_request_requires_positive_limit_or_explicit_all_results() -> None:
    with pytest.raises(ValueError, match="use all_results=True"):
        native.MessageSearchRequest(limit=-5)
    with pytest.raises(ValueError, match="use all_results=True"):
        native.MessageSearchRequest(limit=0)
    with pytest.raises(ValueError, match="cannot be used together"):
        native.MessageSearchRequest(limit=1, all_results=True)
    assert native.MessageSearchRequest(limit=7).limit == 7
    assert native.MessageSearchRequest(all_results=True).all_results is True


def test_message_search_request_rejects_incomplete_purpose_and_invalid_windows() -> None:
    with pytest.raises(ValueError, match="purpose_version requires purpose"):
        native.MessageSearchRequest(purpose_version=1)
    with pytest.raises(ValueError, match="purpose_version must be greater than zero"):
        native.MessageSearchRequest(purpose="focused-review", purpose_version=0)
    with pytest.raises(ValueError, match="match_window must be"):
        native.MessageSearchRequest(match_window="middle")
    with pytest.raises(ValueError, match="context_before must be"):
        native.MessageSearchRequest(context_before=-1)
    with pytest.raises(ValueError, match="receipt_level must be"):
        native.MessageSearchRequest(receipt_level="verbose")
