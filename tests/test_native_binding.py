from __future__ import annotations

import ast
import inspect
import json
import sqlite3
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest

native = pytest.importorskip("ai_session_search.native", reason="native extension is not installed")


def test_message_search_request_exposes_only_final_presentation_and_include_controls() -> None:
    parameters = inspect.signature(native.MessageSearchRequest).parameters
    for name in ("detail", "field_view", "match_view", "include"):
        assert name in parameters
    for removed in ("include_refs", "match_evidence_max_chars"):
        assert removed not in parameters


def test_message_search_spec_exposes_python_defaults_and_executable_registry(tmp_path: Path) -> None:
    search = native.SessionSearch(tmp_path / "index.db")

    specification = search.message_search_spec()

    assert specification["configured_default"]["extent"] == {
        "kind": "all_results",
        "offset": 0,
    }
    registry = specification["registry"]
    assert registry["purpose"].startswith("Search indexed AI-session messages")
    parameters = {
        parameter["parameter"]: parameter for parameter in registry["parameters"]
    }
    assert parameters["providers"]["domain"] == {
        "kind": "non_empty_set",
        "accepted_values": [
            "claude",
            "claude-desktop",
            "codex",
            "cursor",
            "antigravity",
            "pi",
            "aistudio",
            "gemini-cli",
        ],
    }
    assert [descriptor["rule"] for descriptor in registry["rules"]] == [
        "detail_owns_presentation_budgets",
        "sequence_requires_session",
        "kinds_must_remain_satisfiable",
        "compaction_role_requires_compaction_kind",
        "tool_argument_requires_tool_call_kind",
        "match_view_requires_query",
        "fuzzy_rejects_match_window",
        "latest_window_requires_session",
        "fuzzy_rejects_all_results",
    ]
    assert all(descriptor["message"] for descriptor in registry["rules"])


def test_skill_selector_is_exactly_one_valid_name_or_path() -> None:
    named = native.SkillSelector(name="corrections")
    assert named.name == "corrections"
    assert named.path is None

    selected_path = native.SkillSelector(path=Path("skills/corrections"))
    assert selected_path.name is None
    assert selected_path.path == Path("skills/corrections")

    for kwargs in ({}, {"name": "corrections", "path": "skills/corrections"}):
        with pytest.raises(ValueError, match="exactly one"):
            native.SkillSelector(**kwargs)
    with pytest.raises(ValueError, match="invalid skill name"):
        native.SkillSelector(name="Bad_Name")
    with pytest.raises(TypeError):
        native.SkillSelector(name="corrections", unknown=True)


def test_direct_corrections_entrypoint_is_absent_after_skill_run_cutover() -> None:
    assert not hasattr(native, "CorrectionQuery")
    assert not hasattr(native.SessionSearch, "corrections")
    assert hasattr(native.SessionSearch, "run_skill")


def test_message_classification_public_result_names_and_fields_are_pinned() -> None:
    """Lock the generalized Python result names and match field spelling.

    Two fields differ from the Rust struct, and neither was pinned before, so a refactor on
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
    for old_name in ("CorrectionMatch", "CorrectionPolicyReceipt", "CorrectionReport"):
        assert not hasattr(native, old_name)
    assert hasattr(native, "MessageClassificationMatch")
    assert hasattr(native, "CapabilityReceipt")
    assert hasattr(native, "MessageClassificationReport")

    fields = {name for name in dir(native.MessageClassificationMatch) if not name.startswith("_")}
    assert fields == {
        "session_id",
        "provider",
        "timestamp",
        "policy_name",
        "category",
        "matched_text",
        "content",
        "message_seq",
        "match_start_char",
        "match_end_char_exclusive",
    }
    assert "ts" not in fields, "Rust's `ts` must stay renamed to `timestamp` on this surface"
    assert "matched_pattern" not in fields, "the field names the matched text, not the rule"
    assert {name for name in dir(native.CapabilityReceipt) if not name.startswith("_")} == {"name", "version", "sha256"}
    assert {name for name in dir(native.MessageClassificationReport) if not name.startswith("_")} == {"policies", "matches"}


def test_advanced_facade_exports_every_session_search_result_type() -> None:
    module_path = Path(native.__file__)
    stub = ast.parse(module_path.with_name("_native.pyi").read_text())
    facade_stub = ast.parse(module_path.with_suffix(".pyi").read_text())
    session_search = next(node for node in stub.body if isinstance(node, ast.ClassDef) and node.name == "SessionSearch")
    extension_classes = {node.name for node in stub.body if isinstance(node, ast.ClassDef)}
    returned_result_types = {
        node.id
        for method in session_search.body
        if isinstance(method, ast.FunctionDef) and method.returns is not None
        for node in ast.walk(method.returns)
        if isinstance(node, ast.Name) and node.id in extension_classes
    }
    facade_stub_imports = {alias.name for node in facade_stub.body if isinstance(node, ast.ImportFrom) for alias in node.names}
    facade_stub_exports = next(
        ast.literal_eval(node.value)
        for node in facade_stub.body
        if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == "__all__" for target in node.targets)
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


def test_native_facade_does_not_export_superseded_message_search_views() -> None:
    for name in ("MessageContentExtent", "MessageLiteralMatch"):
        assert name not in native.__all__
        assert not hasattr(native, name)


def test_package_root_promotes_rust_application_and_query_types() -> None:
    import ai_session_search as package

    assert package.SessionSearch is native.SessionSearch
    assert package.SessionQuery is native.SessionQuery
    assert package.MessageSearchRequest is native.MessageSearchRequest
    assert package.MessageSearchResponse is native.MessageSearchResponse
    assert package.MessageSearchBatch is native.MessageSearchBatch
    assert package.MessageSearchBatches is native.MessageSearchBatches
    assert package.MessageSearchCompletion is native.MessageSearchCompletion
    assert package.MessageSearchRuntimeDiagnostics is native.MessageSearchRuntimeDiagnostics
    assert package.MessageScope is native.MessageScope
    assert package.QueryExclusions is native.QueryExclusions
    assert package.QueryScope is native.QueryScope
    assert package.ResolvedDateRange is native.ResolvedDateRange
    assert package.AnalysisPublicationPlan is native.AnalysisPublicationPlan
    assert package.AnalysisRequest is native.AnalysisRequest
    assert package.AnalysisReceipt is native.AnalysisReceipt
    assert package.ReceiptedAnalysis is native.ReceiptedAnalysis
    assert package.MessageClassificationMatch is native.MessageClassificationMatch
    assert package.CapabilityReceipt is native.CapabilityReceipt
    assert package.MessageClassificationReport is native.MessageClassificationReport
    assert package.__all__ == [
        "SessionSearch",
        "AnalysisPublicationPlan",
        "AnalysisRequest",
        "AnalysisReceipt",
        "ReceiptedAnalysis",
        "SessionQuery",
        "MessageSearchRequest",
        "MessageSearchResponse",
        "MessageSearchBatch",
        "MessageSearchBatches",
        "MessageSearchCompletion",
        "MessageSearchRuntimeDiagnostics",
        "AnalysisQuery",
        "SkillSelector",
        "MessageClassificationQuery",
        "SkillRunQuery",
        "MessageClassificationMatch",
        "CapabilityReceipt",
        "MessageClassificationReport",
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
    resolved = native.DateRange(when=expression).resolve_bounds(reference_time="2026-06-15T12:00:00Z")

    assert (resolved.since, resolved.until) == (expected_since, expected_until)


def test_native_date_resolution_supports_independent_bounds() -> None:
    resolved = native.DateRange(since="2026-01", until="2026-03").resolve_bounds(reference_time="2026-06-15T12:00:00Z")

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
    assert [future.result().results for future in futures] == [[], []]
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
        return sorted(session.id for session in search.list_sessions(native.SessionQuery(**kwargs)))

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


def test_native_session_search_uses_configured_current_repo_when_omitted(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
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

    explicit = search.search_sessions("needle", native.SessionQuery(current_repo=str(other_repo), limit=2))
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
        f"[index]\ndb_path = {str(configured_database)!r}\n[providers.codex]\nenabled = true\npaths = []\n",
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
    assert status.readiness.snapshot_availability == "unavailable"
    assert status.readiness.refresh.state == "not_started"
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
    assert search.search_messages("missing", native.MessageSearchRequest(limit=1)).results == []
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
        "version": 5,
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
                    "claude:analysis",
                    "claude",
                    0,
                    "2026-01-15T12:02:00+00:00",
                    "Write",
                    "/repo/jan.py",
                    "jan.py",
                    "fixture",
                    None,
                ),
                (
                    "claude:analysis",
                    "claude",
                    1,
                    "2026-01-15T12:03:00+00:00",
                    "Edit",
                    "/repo/jan.py",
                    "jan.py",
                    None,
                    '[{"old":"fixture","new":"fixture two","replace_all":false}]',
                ),
                (
                    "claude:analysis",
                    "claude",
                    2,
                    "2026-01-15T12:04:00+00:00",
                    "apply_patch",
                    "/repo/jan.py",
                    "jan.py",
                    None,
                    None,
                ),
                (
                    "claude:analysis",
                    "claude",
                    3,
                    "2026-01-15T12:05:00+00:00",
                    "Write",
                    "/repo/jan.py",
                    "jan.py",
                    "reset",
                    None,
                ),
                (
                    "codex:other",
                    "codex",
                    0,
                    "2026-02-15T12:02:00+00:00",
                    "Write",
                    "/repo/feb.py",
                    "feb.py",
                    "fixture",
                    None,
                ),
            ],
        )
        connection.commit()
    finally:
        connection.close()

    scope = native.QueryScope(provider="claude", session_id="analysis")
    message_scope = native.MessageScope(
        providers=["codex", "claude", "codex"],
        session_id="analysis",
    )
    assert message_scope.providers == ["claude", "codex"]
    with pytest.raises(ValueError, match="providers must contain at least one"):
        native.MessageScope(providers=[])
    request = native.AnalysisQuery(scope=scope, limit=10)
    skill_run = search.run_skill(
        native.SkillRunQuery(
            skill=native.SkillSelector(name="corrections"),
            input=native.MessageClassificationQuery(scope=scope, limit=10),
        )
    )
    corrections_report = skill_run.output.report
    corrections = corrections_report.matches
    assert [receipt.name for receipt in corrections_report.policies] == ["corrections"], "a default run evaluates exactly the embedded policy and says so"
    assert skill_run.requested_selector.name == "corrections"
    assert skill_run.resolved_skill.name == "corrections"
    assert skill_run.resolved_skill.selected_location.kind == "embedded"
    assert skill_run.resolved_skill.execution_source.kind == "embedded"
    assert skill_run.output.receipt.name == "corrections"
    assert len(corrections_report.policies[0].sha256) == 64
    direct_request = native.SkillRunQuery(
        skill=native.SkillSelector(name="corrections"),
        input=native.MessageClassificationQuery(scope=scope, limit=10),
        definition={
            "categories": [
                {
                    "name": "direct-rule",
                    "patterns": [r"\bwrong\b"],
                }
            ]
        },
    )
    assert direct_request.definition == {
        "categories": [
            {
                "name": "direct-rule",
                "patterns": [r"\bwrong\b"],
            }
        ]
    }
    direct_run = search.run_skill(direct_request)
    assert direct_run.resolved_skill.name == "corrections"
    assert direct_run.resolved_skill.execution_source.kind == "inline"
    assert [(match.category, match.matched_text) for match in direct_run.output.report.matches] == [("direct-rule", "wrong")]
    planning = search.planning(request, ["^/plan$"])
    roles = search.role_statistics(request)
    messages = search.search_messages(
        "",
        native.MessageSearchRequest(scope=message_scope, limit=10),
    ).results
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
    ).results
    fuzzy_user_messages = search.search_messages(
        "actully",
        native.MessageSearchRequest(scope=message_scope, role="user", limit=10),
        query_mode="fuzzy",
    ).results
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

    assert [(hit.provider, hit.content) for hit in corrections] == [("claude", "actually, that is wrong; see https://example.com/docs")]
    assert corrections[0].policy_name == "corrections"
    assert corrections[0].message_seq == 0
    assert (
        corrections[0].content[
            corrections[0].match_start_char : corrections[0].match_end_char_exclusive
        ]
        == corrections[0].matched_text
    )

    # Paging and session-class arguments must reach the deterministic capability.
    assert (
        search.run_skill(
            native.SkillRunQuery(
                skill=native.SkillSelector(name="corrections"),
                input=native.MessageClassificationQuery(scope=scope, offset=1),
            )
        ).output.report.matches
        == []
    ), "offset must skip the only match, not be accepted and dropped"
    assert (
        search.run_skill(
            native.SkillRunQuery(
                skill=native.SkillSelector(name="corrections"),
                input=native.MessageClassificationQuery(scope=scope, session_kinds=[]),
            )
        ).output.report.matches
        == []
    ), "an empty session-class set matches nothing, exactly as it does on search"
    with pytest.raises(RuntimeError, match="unknown skill"):
        search.run_skill(
            native.SkillRunQuery(
                skill=native.SkillSelector(name="not-installed"),
                input=native.MessageClassificationQuery(scope=scope),
            )
        )
    for bad, argument in ((-1, "limit"), (-5, "offset")):
        kwargs = {argument: bad}
        with pytest.raises(ValueError, match=f"{argument} must be 0 or greater, got {bad}"):
            native.MessageClassificationQuery(scope=scope, **kwargs)
    assert [(row.command, row.count) for row in planning] == [("/plan", 1)]
    assert {row.role: row.count for row in roles} == {"slash": 1, "user": 1}
    assert [
        (message["message_metadata"]["provider"], message["message_ref"]["message_seq"])
        for message in messages
    ] == [("claude", 0), ("claude", 1)]
    assert [
        (message["message_metadata"]["role"], message["message_ref"]["message_seq"])
        for message in selected_user_messages
    ] == [("user", 0)]
    assert [
        (message["message_metadata"]["role"], message["message_ref"]["message_seq"])
        for message in fuzzy_user_messages
    ] == [("user", 0)]
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
    assert [session.id for session in search.list_sessions(native.SessionQuery(dates=native.DateRange(when="2026-01")))] == ["claude:analysis"]
    assert (
        search.search_messages(
            "",
            native.MessageSearchRequest(scope=native.MessageScope(dates=native.DateRange(when="1999"))),
        ).results
        == []
    )
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
        message["message_ref"]["message_seq"]
        for message in search.search_messages(
            "",
            native.MessageSearchRequest(scope=message_scope, seq_from=1),
        ).results
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
    assert full.results[0]["presentation"]["field_view"]["text"] == "needle opening line\nmiddle detail\nfinal exit status 0"

    head = search.search_messages("needle", native.MessageSearchRequest(lines_per_message=1))
    assert head.results[0]["presentation"]["field_view"]["text"] == "needle opening line"

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

    assert search.search_messages("CANNOT STOP", native.MessageSearchRequest()).results == [], (
        "harness notices stay excluded by default"
    )
    hits = search.search_messages(
        "CANNOT STOP",
        native.MessageSearchRequest(kind="harness_notice"),
    ).results
    assert len(hits) == 1
    assert hits[0]["message_metadata"]["kind"] == "harness_notice"


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
        results = search.search_messages(query, request, query_mode=mode).results
        assert [
            (result["message_ref"]["session_id"], result["message_ref"]["message_seq"])
            for result in results
        ] == [("claude:matrix", 0)], (field, mode)
        if mode == "fuzzy":
            assert results[0]["match"]["fuzzy_score"] is not None

    first_page = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(
            scope=native.MessageScope(session_id="claude:matrix"),
            limit=1,
            context=1,
            include=["parsed_references"],
            lines_per_message=1,
            receipt_level="full",
        ),
    )
    result = first_page.results[0]
    assert (result["message_ref"]["session_id"], result["message_ref"]["message_seq"]) == (
        "claude:matrix",
        0,
    )
    assert first_page.response_schema_version == 1
    assert first_page.effective_request["query"] == "tool_call"
    assert first_page.effective_request["context"] == {
        "messages_before": 1,
        "messages_after": 1,
    }
    assert first_page.effective_request["presentation"]["lines_per_message"] == 1
    assert first_page.page == {
        "returned": 1,
        "limit": 1,
        "offset": 0,
        "has_more": True,
        "next_offset": 1,
        "earlier_results": "none",
        "result_set_extent": "partial",
        "ordering": "session-sequence",
        "consistency": "per-call",
    }
    assert [reference["host"] for reference in result["included"]["parsed_references"]] == ["example.com"]
    assert result["match"]["literal_occurrence"]["text"] == "tool_call"
    field_view = result["presentation"]["field_view"]
    assert field_view["extent"]["additional_field_text"] == "none"
    assert field_view["field_end_char_exclusive"] == field_view["extent"]["field_total_chars"]
    assert [
        message["message_ref"]["message_seq"] for message in result["context"]["messages_after"]
    ] == [1]
    assert first_page.receipt is not None
    assert first_page.receipt["search_explanation"]["corpus"] == 2
    origins = first_page.receipt["parameter_origins"]
    assert origins["result_extent"]["source"] == "explicit"
    assert origins["context_messages_before"]["source"] == "explicit"
    assert origins["includes"]["source"] == "explicit"
    assert origins["lines_per_message"]["source"] == "explicit"
    assert origins["receipt_level"]["source"] == "explicit"

    second_page = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(
            scope=native.MessageScope(session_id="claude:matrix"),
            limit=1,
            offset=first_page.page["next_offset"],
        ),
    )
    assert [result["message_ref"]["message_seq"] for result in second_page.results] == [1]
    assert second_page.page["next_offset"] is None
    assert second_page.page["has_more"] is False
    assert second_page.receipt is None

    all_from_second = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(all_results=True, offset=1),
    )
    assert all_from_second.page["limit"] is None
    assert all_from_second.page["offset"] == 1
    assert [result["message_ref"]["message_seq"] for result in all_from_second.results] == [1]

    defaults = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(receipt_level="full"),
    )
    assert defaults.page["limit"] is None
    assert defaults.page["returned"] == 2
    assert defaults.page["next_offset"] is None
    assert defaults.receipt is not None
    assert defaults.receipt["parameter_origins"]["result_extent"]["source"] == "typed-default"
    assert "surface" not in defaults.receipt["parameter_origins"]["result_extent"]

    fuzzy_offset = search.search_messages(
        "tolcal",
        native.MessageSearchRequest(
            scope=native.MessageScope(session_id="claude:matrix"),
            limit=1,
            offset=1,
        ),
        query_mode="fuzzy",
    )
    assert fuzzy_offset.page["offset"] == 1
    assert fuzzy_offset.page["ordering"] == "fuzzy-relevance"

    summary = search.search_messages(
        "tool_call",
        native.MessageSearchRequest(receipt_level="summary", limit=1),
    )
    assert summary.receipt is not None
    assert summary.receipt["search_explanation"] is not None
    assert "parameter_origins" not in summary.receipt


def test_native_message_search_response_exposes_only_the_canonical_version_one_document(
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
            ) values ('claude:canonical', 'claude', 'canonical', '',
                      '/canonical.jsonl', 'test', 'fixture')
            """
        )
        connection.execute(
            """
            insert into messages (
                session_id, provider, seq, role, kind, content
            ) values (
                'claude:canonical', 'claude', 0, 'user', 'conversation',
                'prefix https://example.com exact needle suffix'
            )
            """
        )
        connection.commit()
    finally:
        connection.close()

    response = search.search_messages(
        "exact needle",
        native.MessageSearchRequest(
            limit=1,
            context=1,
            include=["parsed_references", "runtime_diagnostics"],
            field_view={"kind": "max_chars", "max_chars": 20},
            match_view={"kind": "minimal_span"},
            receipt_level="full",
        ),
    )

    assert response.response_schema_version == 1
    assert response.effective_request["query"] == "exact needle"
    assert isinstance(response.results, list)
    result = response.results[0]
    assert result["message_ref"] == {
        "session_id": "claude:canonical",
        "message_seq": 0,
    }
    assert result["presentation"]["field_view"]["extent"]["additional_field_text"] != "none"
    assert result["presentation"]["match_view"]["text"] == "exact needle"
    assert result["match"]["literal_occurrence"]["text"] == "exact needle"
    assert result["included"]["parsed_references"][0]["host"] == "example.com"
    assert result["context"] == {
        "messages_before": [],
        "messages_after": [],
    }
    assert response.page["returned"] == 1
    assert response.included["runtime_diagnostics"]["surface"] == "python"
    assert response.receipt["ordered_digest"].startswith("sha256:")
    for rejected in (
        "hits",
        "context_windows",
        "content_extent",
        "query",
        "limit",
        "offset",
        "next_offset",
        "search_explanation",
        "origins",
        "ordered_digest",
    ):
        assert not hasattr(response, rejected), rejected
    assert not hasattr(native, "MessageContentExtent")


def test_native_message_search_batches_match_the_simple_materialized_api(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.execute(
            """
            insert into sessions (
                id, provider, provider_session_id, preview_text, source_path,
                parse_version, discovery_source
            ) values ('claude:batches', 'claude', 'batches', '', '/batches.jsonl',
                      'test', 'fixture')
            """
        )
        connection.executemany(
            """
            insert into messages (session_id, provider, seq, role, kind, content)
            values ('claude:batches', 'claude', ?, 'user', 'conversation', ?)
            """,
            [(seq, f"needle message {seq} https://example.com/{seq}") for seq in range(5)],
        )
        connection.commit()
    finally:
        connection.close()

    request = native.MessageSearchRequest(
        all_results=True,
        include=["parsed_references", "runtime_diagnostics"],
        receipt_level="full",
    )
    expected = search.search_messages("needle", request)
    with search.search_message_batches("needle", request, batch_rows=2) as batches:
        assert iter(batches) is batches
        runtime_diagnostics = batches.runtime_diagnostics
        returned = [hit for batch in batches for hit in batch.results]
        completion = batches.completion

    assert [result["message_ref"] for result in returned] == [result["message_ref"] for result in expected.results]
    assert all(result["included"]["parsed_references"] for result in returned)
    assert [len(batch.results) for batch in search.search_message_batches("needle", request, batch_rows=2)] == [2, 2, 1]
    assert completion.page["returned"] == len(expected.results)
    assert completion.page["next_offset"] is None
    assert completion.page["result_set_extent"] == "all"
    assert completion.receipt["ordered_digest"] == expected.receipt["ordered_digest"]
    assert runtime_diagnostics is not None
    expected_diagnostics = expected.included["runtime_diagnostics"]
    assert isinstance(expected_diagnostics, dict)
    assert runtime_diagnostics.package_version == expected_diagnostics["package_version"]
    assert runtime_diagnostics.database_schema_version == expected_diagnostics["database_schema_version"]
    assert runtime_diagnostics.response_schema_version == expected_diagnostics["response_schema_version"]
    assert runtime_diagnostics.surface == expected_diagnostics["surface"] == "python"
    assert runtime_diagnostics.config_digest == expected_diagnostics["config_digest"]


def test_native_message_search_batches_close_without_draining_and_validate_batch_rows(tmp_path: Path) -> None:
    search = native.SessionSearch(tmp_path / "index.db")

    with pytest.raises(ValueError, match="batch_rows must be a positive integer"):
        search.search_message_batches("", native.MessageSearchRequest(all_results=True), batch_rows=0)
    with pytest.raises(ValueError, match="batch_rows must be a positive integer"):
        search.search_message_batches("", native.MessageSearchRequest(all_results=True), batch_rows=-1)
    with pytest.raises(ValueError, match="unknown include"):
        native.MessageSearchRequest(include=["not_metadata"])
    with pytest.raises(ValueError, match=r"requires all_results.*search_messages\(\)"):
        search.search_message_batches("", native.MessageSearchRequest(limit=1))
    with pytest.raises(ValueError, match=r"fuzzy.*search_messages\(\)"):
        search.search_message_batches("needle", native.MessageSearchRequest(all_results=True), query_mode="fuzzy")

    batches = search.search_message_batches("", native.MessageSearchRequest(all_results=True), batch_rows=1)
    with pytest.raises(RuntimeError, match=r"unread results.*next\(\).*close\(\)"):
        _ = batches.completion
    batches.close()
    batches.close()
    with pytest.raises(RuntimeError, match="closed"):
        next(batches)
    with pytest.raises(RuntimeError, match="closed before natural exhaustion"):
        _ = batches.completion

    batches = search.search_message_batches("", native.MessageSearchRequest(all_results=True), batch_rows=1)
    with pytest.raises(RuntimeError, match="consumer failed"):
        with batches:
            raise RuntimeError("consumer failed")
    with pytest.raises(RuntimeError, match="closed"):
        next(batches)


def test_native_message_search_batch_projection_failure_closes_the_producer(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
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
            ) values (
                'claude:projection-cleanup', 'claude', 'projection-cleanup', '',
                '/projection-cleanup.jsonl', 'test', 'fixture'
            )
            """
        )
        connection.execute(
            """
            insert into messages (session_id, provider, seq, role, kind, content)
            values (
                'claude:projection-cleanup', 'claude', 0, 'user', 'conversation',
                'projection cleanup evidence'
            )
            """
        )
        connection.commit()
    finally:
        connection.close()

    batches = search.search_message_batches(
        "projection cleanup",
        native.MessageSearchRequest(all_results=True),
        batch_rows=1,
    )

    def reject_projection(_encoded: str) -> object:
        raise RuntimeError("injected Python projection failure")

    monkeypatch.setattr(json, "loads", reject_projection)
    with pytest.raises(RuntimeError, match="injected Python projection failure"):
        next(batches)
    with pytest.raises(RuntimeError, match="closed"):
        next(batches)


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
    timeline = search.search_messages("", native.MessageSearchRequest(scope=scope)).results
    argument_request = native.MessageSearchRequest(
        scope=scope,
        kind="tool_call",
        field="tool_argument",
        argument_path="/request/path",
        tool_name_contains="exec",
    )

    assert [
        (
            event["message_metadata"]["kind"],
            event["presentation"]["field_view"]["text"],
            event["message_ref"]["message_seq"],
        )
        for event in timeline
    ] == [
        (
            "tool_call",
            '{"args":{"cmd":"cargo test","request":{"path":"src/lib.rs"}},"kind":"tool_call","tool_name":"exec_command"}',
            0,
        )
    ]
    argument_response = search.search_messages("src/lib.rs", argument_request)
    assert [event["message_ref"]["message_seq"] for event in argument_response.results] == [0]
    assert argument_response.effective_request["query_mode"] == "literal"
    assert argument_response.effective_request["target"] == {
        "field": "tool_argument",
        "argument_path": "/request/path",
    }
    match_view = argument_response.results[0]["presentation"]["match_view"]
    assert match_view["text"] == "src/lib.rs"
    assert match_view["markers"] == [
        {
            "view_start_char": 0,
            "view_end_char_exclusive": 10,
        }
    ]
    assert "match_view" not in timeline[0]["presentation"]
    assert [
        event["message_ref"]["message_seq"]
        for event in search.search_messages(
            "exec_command",
            native.MessageSearchRequest(scope=scope, field="tool_name"),
        ).results
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
        return [result["message_ref"]["session_id"] for result in response.results]

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
    assert seqs(search.read_session_messages("reads", order="newest", role="user", limit=1)) == [2]
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
            insert into messages (
                session_id, provider, seq, role, authorship, record_relation, content
            )
            values (?, ?, ?, ?, ?, 'original', ?)
            """,
            [
                ("claude:first", "claude", 0, "user", "human", "first request"),
                ("claude:first", "claude", 1, "assistant", "agent", "answer is not analysis input"),
                ("claude:first", "claude", 2, "user", "human", "second request"),
                ("codex:other", "codex", 0, "user", "human", "other provider"),
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
            "insert into messages ("
            "session_id, provider, seq, role, authorship, record_relation, content"
            ") values (?, ?, 0, 'user', 'human', 'original', ?)",
            [
                ("claude:root", "claude", "Use TDD"),
                ("gemini-cli:child", "gemini-cli", "Use TDD"),
            ],
        )
        connection.commit()
    finally:
        connection.close()

    policy = native.AnalysisPolicy(
        classification_rules=[native.ClassificationRule("technique", "tdd", r"(?i)\btdd\b", weight=7)],
        relationship_rules=[native.RelationshipRule("branch_of", "branch", r"^Branch of (?P<parent>.+)$")],
        phrase_vocabulary=native.PhraseVocabulary([2], 100, prose_only=True),
        max_classification_chars=100,
    )
    analysis = search.analyze(native.AnalysisRequest(), policy=policy)
    result = analysis.result

    assert analysis.receipt.selection_kind == "all_eligible"
    assert analysis.receipt.max_selected_sessions is None
    assert analysis.receipt.selected_sessions == 3
    assert analysis.receipt.messages_in_selected_sessions == 2
    assert analysis.receipt.analyzed_user_messages == 2
    assert analysis.receipt.has_more is False
    assert analysis.receipt.policy_digest.startswith("sha256:")
    assert analysis.receipt.corpus_digest.startswith("sha256:")
    assert analysis.receipt.result_digest.startswith("sha256:")

    assert list(result.sessions) == ["claude:root", "codex:root", "gemini-cli:child"]
    child = result.sessions["gemini-cli:child"]
    assert child.score == 7
    assert child.message_count == 1
    assert child.user_message_count == 1
    assert [(item.dimension, item.label) for item in child.classifications] == [("technique", "tdd")]
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

    bounded = search.analyze(
        native.AnalysisRequest(first_canonical_sessions=1),
        policy=policy,
    )
    assert list(bounded.result.sessions) == ["claude:root"]
    assert bounded.receipt.selection_kind == "first_canonical_sessions"
    assert bounded.receipt.max_selected_sessions == 1
    assert bounded.receipt.selected_sessions == 1
    assert bounded.receipt.has_more is True

    with pytest.raises(ValueError, match="omit it to analyze every eligible session"):
        native.AnalysisRequest(first_canonical_sessions=0)

    publication = native.AnalysisPublicationPlan(
        tmp_path / "analysis-bundle",
        ["json", "markdown"],
    )
    rendered = publication.render(analysis)
    assert publication.destination == tmp_path / "analysis-bundle"
    assert publication.formats == ["json", "markdown"]
    assert {artifact.name for artifact in rendered} == {
        "analysis.v1.json",
        "analysis-receipt.v1.json",
        "index.md",
        "knowledge-graph.md",
        "manifest.v1.json",
        "session-graph.v1.json",
        "taxonomy.md",
    }
    assert all(artifact.bytes == len(artifact.content.encode()) for artifact in rendered)
    assert all(len(artifact.sha256) == 64 for artifact in rendered)
    receipt = publication.publish(analysis)
    assert receipt.destination == tmp_path / "analysis-bundle"
    assert {artifact.name for artifact in receipt.artifacts} == {artifact.name for artifact in rendered}
    with pytest.raises(RuntimeError, match="destination already exists"):
        publication.publish(analysis)
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


def test_native_analyze_omitted_request_does_not_silently_select_only_fifty_sessions(
    tmp_path: Path,
) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database)
    connection = sqlite3.connect(database)
    try:
        connection.executemany(
            """
            insert into sessions (
                id, provider, provider_session_id, title, updated_at, preview_text, source_path,
                parse_version, discovery_source
            ) values (?, 'claude', ?, ?, '2026-04-01T12:00:00+00:00', '', ?, 'test', 'fixture')
            """,
            [
                (
                    f"claude:analysis-{index:02}",
                    f"analysis-{index:02}",
                    f"Session {index}",
                    f"/session-{index}.jsonl",
                )
                for index in range(51)
            ],
        )
        connection.commit()
    finally:
        connection.close()

    analysis = search.analyze()
    assert len(analysis.result.sessions) == 51
    assert analysis.receipt.selected_sessions == 51
    assert analysis.receipt.has_more is False


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
def test_negative_paging_arguments_name_the_parameter_bound_and_meaning_of_zero(factory, field: str) -> None:
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
    request = native.MessageSearchRequest(
        detail="compact",
        field_view={"kind": "max_chars", "max_chars": 80},
        match_view={"kind": "minimal_span"},
        include=["parsed_references"],
    )
    assert request.detail == "compact"
    assert request.field_view == {"kind": "max_chars", "max_chars": 80}
    assert request.match_view == {"kind": "minimal_span"}
    assert request.include == ["parsed_references"]
    with pytest.raises(ValueError, match=r"field_view\.max_chars must be an integer from 1"):
        native.MessageSearchRequest(field_view={"kind": "max_chars", "max_chars": 0})
    with pytest.raises(ValueError, match="unknown field"):
        native.MessageSearchRequest(match_view={"kind": "minimal_span", "extra": 1})


def test_message_search_request_rejects_kind_and_kinds_together() -> None:
    with pytest.raises(ValueError, match="kind and kinds cannot be used together"):
        native.MessageSearchRequest(
            kind="conversation",
            kinds=["tool_result"],
        )


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
