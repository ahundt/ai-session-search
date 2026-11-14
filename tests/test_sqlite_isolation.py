# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Keep direct Python SQLite access limited to reviewed non-overlapping lifetimes."""

import ast
import sqlite3
import subprocess
from pathlib import Path

import pytest
from sqlite_helpers import write_database

import ai_session_search as aise

# Counts are call sites, not executions. These functions have no live native database
# owner while Python SQLite is open. New sites require a lifetime review or the helper.
_SAFE_CONNECTIONS = {
    "test_native_binding.py": {
        # Both tests delete the native owner before opening and close before reopening.
        "test_native_full_reindex_promotes_v3_and_releases_exclusive_lock": 1,
        "test_native_compatibility_reader_only_refuses_maintenance": 1,
        # Unassigned native temporaries are dropped before Python opens; explicit close.
        "_parity_database": 1,
    },
    "test_benchmark_scripts.py": {
        # Synthetic schemas, Python-only or subprocess/mocked consumers.
        "test_temporal_overlap_oracle_uses_closed_session_spans": 1,
        "test_tui_latency_report_names_workload_symbols_and_throughput": 1,
        "test_tui_semantic_probe_rejects_equal_count_wrong_membership": 1,
        "test_generated_fixture_refresh_time_is_independent_of_build_clock": 2,
    },
    "test_upgrade_path.py": {
        # Python-only snapshot comparisons (including a private :memory: database).
        "test_archive_fixture_checks_shrink_rewrite_edits_and_injected_rows": 1,
        "test_populated_comparison_detects_content_order_edits_text_and_provenance": 2,
        "test_populated_comparison_counts_duplicate_messages_outside_seed_sessions": 2,
    },
    "test_mcp_consolidation.py": {
        # The refresh owner runs in a child process.
        "test_python_console_install_starts_the_canonical_refresh_child": 1,
        # Native owner deleted first; Python closes before MCP runs in a subprocess.
        "test_mcp_reads_an_index_requiring_a_newer_writer": 2,
    },
    "test_python_workflows.py": {},
}


def test_python_sqlite_connections_require_reviewed_lifetimes() -> None:
    """Reject new direct sqlite3.connect calls outside the reviewed function counts.

    This is a source guard, not native-handle tracking: it catches direct calls even
    before a native owner is constructed, but not aliases, imported helpers, or SQL
    inside subprocess strings. Changes to an allowed function still need review.
    """
    violations = []
    for filename, allowed in _SAFE_CONNECTIONS.items():
        tree = ast.parse(Path(__file__).with_name(filename).read_text(encoding="utf-8"))
        owners = {
            id(child): function.name
            for function in tree.body
            if isinstance(function, (ast.FunctionDef, ast.AsyncFunctionDef))
            for child in ast.walk(function)
        }
        counts: dict[str, int] = {}
        for node in ast.walk(tree):
            if not (
                isinstance(node, ast.Call)
                and isinstance(node.func, ast.Attribute)
                and isinstance(node.func.value, ast.Name)
                and node.func.value.id == "sqlite3"
                and node.func.attr == "connect"
            ):
                continue
            owner = owners.get(id(node), "<module>")
            counts[owner] = counts.get(owner, 0) + 1
            if counts[owner] > allowed.get(owner, 0):
                violations.append(f"{filename}:{node.lineno} ({owner})")
    assert not violations, (
        "Use sqlite_helpers.write_database() to avoid "
        "opening Python SQLite alongside a live native database owner:\n" + "\n".join(violations)
    )


def test_write_database_runs_scripts_and_parameters_outside_native_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    def forbidden_connection(*args: object, **kwargs: object) -> None:
        pytest.fail("use sqlite_helpers.write_database() instead of same-process SQLite")

    monkeypatch.setattr(sqlite3, "connect", forbidden_connection)
    database = tmp_path / "index.db"
    with aise.SessionSearch(database) as search:
        with write_database(database) as connection:
            connection.executescript("""
                insert into sessions(id, provider, provider_session_id, preview_text,
                    source_path, parse_version, discovery_source)
                values ('claude:helper', 'claude', 'helper', '', '/helper', 'test', 'fixture');
            """)
            connection.executemany(
                "insert into messages(session_id, provider, seq, role, kind, content) "
                "values ('claude:helper', 'claude', ?, 'user', 'conversation', ?)",
                ((seq, text) for seq, text in enumerate(("needle é\nline", "needle 'quoted'"))),
            )
            connection.execute("update sessions set title=?", (None,))
        assert [message.content for message in search.read_session_messages("claude:helper")] == [
            "needle é\nline", "needle 'quoted'",
        ]
        assert search.list_sessions()[0].title is None


@pytest.mark.parametrize("failure", ["caller", "sql"])
def test_write_database_does_not_commit_failed_context(tmp_path: Path, failure: str) -> None:
    database = tmp_path / "index.db"
    with aise.SessionSearch(database) as search:
        expected = ValueError if failure == "caller" else subprocess.CalledProcessError
        with pytest.raises(expected):
            with write_database(database) as connection:
                connection.execute(
                    "insert into sessions(id, provider, provider_session_id, preview_text, "
                    "source_path, parse_version, discovery_source) "
                    "values ('claude:rollback', 'claude', 'rollback', '', '/rollback', 'test', 'fixture')"
                )
                if failure == "caller":
                    raise ValueError("discard buffered SQL")
                connection.execute("insert into missing_table values (1)")
        assert search.list_sessions() == []
