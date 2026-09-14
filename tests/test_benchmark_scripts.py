# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import importlib.util
import json
import re
import sqlite3
import subprocess
import sys
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[1]


def load_script(name: str) -> ModuleType:
    return load_python_file(ROOT / "scripts" / name)


def load_python_file(path: Path) -> ModuleType:
    spec = importlib.util.spec_from_file_location(path.stem, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_release_manifest_clients_use_only_canonical_query_modes() -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    client_cases = [case for case in manifest["cases"] if case["surface"] in {"python", "mcp"} and "--mode" in case["argv"]]
    assert client_cases
    for case in client_cases:
        mode_index = case["argv"].index("--mode") + 1
        assert case["argv"][mode_index] in {"literal", "regex", "fuzzy"}, case["id"]


def test_release_manifest_rust_driver_uses_only_canonical_query_modes() -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    rust_cases = [case for case in manifest["cases"] if case["surface"] == "rust" and "--operation" not in case["argv"]]

    assert rust_cases
    for case in rust_cases:
        assert case["argv"][3] in {"literal", "regex", "fuzzy"}, case["id"]


def test_rust_benchmark_driver_uses_the_canonical_versioned_response() -> None:
    source = (ROOT / "rust/ai-session-search-core/examples/benchmark_core.rs").read_text()

    assert "search_legacy" not in source
    assert "MessageSearchRequest::builder" in source
    assert ".messages().search(" in source


def test_release_manifest_fixture_schema_matches_the_rust_database_owner() -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    database_source = (ROOT / "rust/ai-session-search-core/src/db.rs").read_text()
    match = re.search(r"^pub const SCHEMA_VERSION: i64 = (\d+);$", database_source, re.MULTILINE)

    assert match is not None, "db.rs must keep one public integer SCHEMA_VERSION owner"
    assert manifest["fixture"]["required_schema_version"] == int(match.group(1))


@pytest.mark.parametrize("removed_flag", ["--regex", "--fuzzy"])
def test_release_manifest_uses_canonical_cli_query_mode(removed_flag: str) -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    cli_search_cases = [case for case in manifest["cases"] if case["surface"] == "cli" and "messages" in case["argv"]]
    assert cli_search_cases
    assert all(removed_flag not in case["argv"] for case in cli_search_cases)


def test_release_manifest_has_complete_four_surface_search_matrix() -> None:
    benchmark = load_script("benchmark_release.py")
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    benchmark.validate_manifest(manifest)
    broken = json.loads(json.dumps(manifest))
    broken["cases"] = [case for case in broken["cases"] if case["id"] != "mcp-fuzzy-content"]
    with pytest.raises(ValueError, match="mcp 3x3 matrix missing"):
        benchmark.validate_manifest(broken)


def test_temporal_benchmarks_reuse_clients_and_require_oracle_semantics() -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    temporal = [case for case in manifest["cases"] if case.get("temporal_mode")]

    assert {(case["surface"], "search" in case["id"]) for case in temporal} == {
        ("cli", False),
        ("cli", True),
        ("python", False),
        ("python", True),
        ("mcp", False),
        ("mcp", True),
    }
    assert all(case["expected_relation"] == "intentional_change_with_oracle" for case in temporal)
    assert all(case["require_equal"] is False for case in temporal)
    assert all("--when" in case["argv"] for case in temporal)
    assert all(case["oracle_start"] <= case["oracle_end"] for case in temporal)


def test_temporal_overlap_oracle_uses_closed_session_spans(tmp_path: Path) -> None:
    benchmark = load_script("benchmark_release.py")
    database = tmp_path / "fixture.db"
    connection = sqlite3.connect(database)
    try:
        connection.execute("create table sessions (id text, created_at text, updated_at text)")
        connection.executemany(
            "insert into sessions values (?, ?, ?)",
            [
                ("long", "2026-01-10T00:00:00+00:00", "2026-03-10T00:00:00+00:00"),
                ("late", "2026-03-11T00:00:00+00:00", "2026-03-12T00:00:00+00:00"),
            ],
        )
        connection.commit()
    finally:
        connection.close()

    oracle = benchmark.temporal_overlap_oracle(
        database,
        {
            "id": "test",
            "expected_relation": "intentional_change_with_oracle",
            "oracle_start": "2026-02-15T00:00:00+00:00",
            "oracle_end": "2026-02-15T00:00:00+00:00",
        },
    )
    assert oracle is not None
    assert oracle["eligible_ids"] == ["long"]
    assert len(oracle["eligible_ids_sha256"]) == 64


def test_temporal_oracle_is_release_blocking_for_candidate_not_historical_baseline() -> None:
    source = (ROOT / "scripts/benchmark_release.py").read_text()

    assert 'label == "candidate" and not sample["temporal_oracle_match"]' in source
    assert 'sample["temporal_oracle_match"]' in source


def test_generated_fixture_has_point_spans_multi_day_spans_and_all_provider_isolation() -> None:
    source = (ROOT / "scripts/benchmark_release.py").read_text()
    config = load_script("benchmark_release.py").generated_fixture_config()

    assert "session_number % 2 == 0" in source
    assert "datetime.timedelta(days=35)" in source
    for provider in (
        "claude",
        "claude-desktop",
        "codex",
        "cursor",
        "antigravity",
        "pi",
        "prime-agent",
        "aistudio",
        "gemini-cli",
    ):
        assert f"[providers.{provider}]" in config
    assert "[providers.prime-agent]" not in load_script("benchmark_release.py").generated_fixture_config(include_prime_agent=False)


def test_release_manifest_has_same_server_mcp_reader_bound_matrix() -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    cases = {case["reader_bound"]: case for case in manifest["cases"] if case.get("workload") == "same-server-mcp-fuzzy-readers"}

    assert set(cases) == {"auto", "host", 1, 2, 4, 8}
    for bound, case in cases.items():
        argv = case["argv"]
        assert case["operations"] == 16
        assert argv[argv.index("--mode") + 1] == "fuzzy"
        assert argv[argv.index("--requests") + 1] == "16"
        assert argv[argv.index("--max-concurrent-reads") + 1] == str(bound)


def test_benchmark_samples_retain_declared_work_units_and_reader_bound() -> None:
    benchmark = load_script("benchmark_release.py")
    case = {
        "operations": 16,
        "reader_bound": "host",
        "workload": "same-server-mcp-fuzzy-readers",
    }

    assert benchmark.case_measurement_metadata(case) == {
        "operations": 16,
        "reader_bound": "host",
        "workload": "same-server-mcp-fuzzy-readers",
    }
    assert benchmark.case_measurement_metadata({}) == {"operations": 1}
    assert benchmark.case_measurement_metadata(
        {
            "temporal_mode": "when",
            "expected_relation": "intentional_change_with_oracle",
        }
    ) == {
        "operations": 1,
        "temporal_mode": "when",
        "expected_relation": "intentional_change_with_oracle",
    }


@pytest.mark.parametrize("tier", ["smoke", "subsystem", "release"])
def test_every_benchmark_tier_accepts_the_portable_generated_fixture(tier: str) -> None:
    benchmark = load_script("benchmark_release.py")

    benchmark.validate_fixture_policy(tier, "generated", False)


@pytest.mark.parametrize("tier", ["smoke", "subsystem"])
def test_local_profiling_requires_an_explicit_private_artifact_opt_in(
    tier: str,
) -> None:
    benchmark = load_script("benchmark_release.py")
    fixture = f"/tmp/{tier}-disposable.db"

    with pytest.raises(SystemExit, match="--allow-private-fixture"):
        benchmark.validate_fixture_policy(tier, fixture, False)
    benchmark.validate_fixture_policy(tier, fixture, True)


def test_release_tier_rejects_a_local_fixture_even_with_private_opt_in() -> None:
    benchmark = load_script("benchmark_release.py")

    with pytest.raises(SystemExit, match="release benchmarks require --fixture generated"):
        benchmark.validate_fixture_policy("release", "/tmp/disposable.db", True)


def test_benchmark_artifact_privacy_distinguishes_generated_and_local_fixtures() -> None:
    benchmark = load_script("benchmark_release.py")

    assert benchmark.artifact_privacy("generated") == {
        "classification": "portable_generated",
        "publishable": True,
    }
    assert benchmark.artifact_privacy("/tmp/disposable.db") == {
        "classification": "private_local_fixture",
        "publishable": False,
    }


def test_benchmark_metadata_and_samples_do_not_publish_local_paths(
    tmp_path: Path,
) -> None:
    benchmark = load_script("benchmark_release.py")
    repository = tmp_path / "private-user" / "repository"
    repository.mkdir(parents=True)
    subprocess.run(["git", "init", "-q"], cwd=repository, check=True)
    subprocess.run(["git", "config", "user.name", "Benchmark Author"], cwd=repository, check=True)
    subprocess.run(
        ["git", "config", "user.email", "benchmark-author@example.invalid"],
        cwd=repository,
        check=True,
    )
    (repository / "tracked.txt").write_text("deterministic\n")
    subprocess.run(["git", "add", "tracked.txt"], cwd=repository, check=True)
    subprocess.run(["git", "commit", "-qm", "fixture"], cwd=repository, check=True)
    binary = tmp_path / "private-user" / "aise"
    manifest = tmp_path / "private-user" / "manifest.json"
    binary.write_bytes(b"binary")
    manifest.write_text("{}")

    run_metadata = benchmark.metadata(binary, manifest, repository)
    serialized_metadata = json.dumps(run_metadata, sort_keys=True)
    assert str(tmp_path) not in serialized_metadata
    assert str(repository) not in serialized_metadata
    assert {"commit", "binary_sha256", "manifest_sha256"} <= run_metadata.keys()
    assert {"repository", "binary", "manifest", "git_status", "processor"}.isdisjoint(run_metadata)

    private_fixture = tmp_path / "private-user" / "fixture.db"
    sample = benchmark.sample_process(
        [
            sys.executable,
            "-c",
            "import sys; sys.stderr.write(sys.argv[1])",
            str(private_fixture),
        ],
        {str(private_fixture).encode(): b"{fixture}"},
    )
    serialized_sample = json.dumps(sample, sort_keys=True)
    assert str(tmp_path) not in serialized_sample
    assert str(private_fixture) not in serialized_sample
    assert sample["stderr"] == "{fixture}"
    assert "argv" not in sample


def test_public_fixture_metadata_omits_the_local_database_path() -> None:
    benchmark = load_script("benchmark_release.py")
    fixture = {
        "path": "/private/home/user/generated.db",
        "sha256": "a" * 64,
        "bytes": 4096,
        "schema_version": 5,
        "counts": {"sessions": 1, "messages": 2, "file_edits": 0},
    }

    public = benchmark.public_fixture_metadata(fixture)

    assert "path" not in public
    assert public["schema_version"] == 5
    assert "/private/home/user" not in json.dumps(public)


def test_generated_fixture_config_contains_only_portable_app_paths() -> None:
    benchmark = load_script("benchmark_release.py")

    config = benchmark.generated_fixture_config()

    assert 'db_path = "generated.db"' in config
    assert 'cache_dir = "cache"' in config
    assert "/Users/" not in config
    assert "/home/" not in config
    assert "\\\\" not in config


def test_generated_fixture_reindex_cannot_reach_the_running_user_s_transcripts() -> None:
    # The generated fixture disables every provider by name, which only works for the provider
    # names the config text happens to list. `generate_fixture` deliberately omits the
    # `prime-agent` stanza for the baseline build, so that provider kept its default
    # `~/.prime/agent/sessions` root: measured here, the baseline config indexed 119 real
    # transcripts while the all-disabled config indexed none. That put real session content into
    # an artifact the classifier calls `portable_generated` and `publishable`, and it made the
    # two builds measure different corpora — 22,546 messages against 512 — so every before/after
    # comparison and the `require_equal` digest check compared unlike things.
    #
    # Naming providers cannot fix that, because the omission exists precisely for binaries that
    # do not know a name. Pointing HOME at the fixture directory makes the default roots resolve
    # inside it, so the reindex is hermetic whatever the config says and whatever the binary
    # knows.
    source = (ROOT / "scripts/benchmark_release.py").read_text()
    reindex = source.split("def generate_fixture(", 1)[1].split("connection = sqlite3.connect", 1)[0]

    assert "env=" in reindex, (
        "the fixture reindex must run with an explicit environment; inheriting the caller's HOME "
        "lets any provider the config does not disable read the running user's transcripts"
    )
    assert '"HOME"' in reindex
    assert "fixture_dir" in reindex


def test_generated_fixture_populates_canonical_tui_preview_transcripts() -> None:
    source = (ROOT / "scripts/benchmark_release.py").read_text()
    generator = source.split("def generate_fixture(", 1)[1].split("def metadata(", 1)[0]
    assert "insert into transcripts" in generator
    assert "transcript_lines" in generator
    assert "role in" in generator


def test_benchmark_help_does_not_claim_an_obsolete_fixture_schema() -> None:
    source = (ROOT / "scripts/benchmark_release.py").read_text()

    assert "schema-v4" not in source


def test_mcp_benchmark_client_uses_only_canonical_search_contract() -> None:
    source = (ROOT / "benchmarks/mcp_client.py").read_text()

    assert '"response_format"' not in source
    assert '["structuredContent"]["hits"]' not in source
    assert '["structuredContent"][result_field]' in source
    assert 'result_field = "results"' in source
    assert 'result_field = "sessions"' in source


def test_python_benchmark_client_uses_only_canonical_search_contract() -> None:
    source = (ROOT / "benchmarks/python_client.py").read_text()

    assert ".hits" not in source
    assert ".results" in source


def test_benchmark_clients_do_not_use_removed_query_mode_flags() -> None:
    for name in ("burst_client.py", "mcp_client.py", "python_client.py", "tui_client.py"):
        source = (ROOT / "benchmarks" / name).read_text()
        assert '"--fuzzy"' not in source, name
        assert '"--regex"' not in source, name


def test_tui_client_imports_without_unix_only_modules(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    os_module = __import__("os")
    monkeypatch.setattr(os_module, "name", "nt")
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    assert not client.PTY_SUPPORTED
    with pytest.raises(SystemExit, match="POSIX"):
        client._require_pty_support()


def test_tui_client_latency_case_is_registered_and_opt_in() -> None:
    """The latency measurement is a separate manifest case; the startup case stays untouched."""
    source = (ROOT / "benchmarks" / "tui_client.py").read_text()
    assert '"--measure-latency"' in source
    assert '"--queries"' in source
    assert '"--repetitions"' in source
    assert source.count('print(\'{"preview":true,"sessions":true,"terminal_restored":true}\')') == 1
    manifest = json.loads((ROOT / "benchmarks" / "release_manifest.json").read_text())
    cases = {case["id"]: case for case in manifest["cases"]}
    assert "--measure-latency" in cases["tui-typeahead-latency"]["argv"]
    assert cases["tui-typeahead-latency"]["result_json_field"] == "result_digest"
    assert cases["tui-typeahead-latency"]["resource_json_fields"] == {
        "wall_ms": "wall_ms",
        "peak_rss_kib": "peak_rss_kb",
        "peak_threads": "peak_threads",
        "peak_processes": "process_count",
        "cpu_seconds": "cpu_seconds",
    }
    assert "--measure-latency" not in cases["tui-startup-list"]["argv"]


def test_release_runner_compares_tui_semantic_digest_not_performance_noise() -> None:
    benchmark = load_script("benchmark_release.py")
    first = benchmark.sample_process(
        [sys.executable, "-c", 'print(\'{"result_digest":"same","echo_ms_p50":1}\')'],
        result_json_field="result_digest",
    )
    second = benchmark.sample_process(
        [sys.executable, "-c", 'print(\'{"result_digest":"same","echo_ms_p50":99}\')'],
        result_json_field="result_digest",
    )
    assert first["result_sha256"] != second["result_sha256"]
    assert first["semantic_result_sha256"] == second["semantic_result_sha256"]

    renderer = load_script("render_benchmark_report.py")
    rows = [
        {
            **sample,
            "wall_ms": wall_ms,
            "cpu_seconds": 0,
            "peak_rss_kib": 1,
            "peak_threads": 1,
            "peak_processes": 1,
        }
        for sample, wall_ms in ((first, 1), (second, 99))
    ]
    assert renderer.summarize(rows)["digest"] == first["semantic_result_sha256"]


def test_linux_tui_sampler_reads_the_process_thread_count(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    monkeypatch.setattr(client.platform, "system", lambda: "Linux")

    def run(command: list[str], **_kwargs: Any) -> Any:
        stdout = "7\n" if command == ["ps", "-o", "nlwp=", "-p", "123"] else "LABEL PID\nlabel 123\n"
        return type("Completed", (), {"stdout": stdout})()

    monkeypatch.setattr(client.subprocess, "run", run)
    assert client.ResourceSampler._threads(123) == 7


def test_macos_tui_sampler_counts_thread_rows(monkeypatch: pytest.MonkeyPatch) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    monkeypatch.setattr(client.platform, "system", lambda: "Darwin")
    commands: list[list[str]] = []

    def run(command: list[str], **_kwargs: Any) -> Any:
        commands.append(command)
        return type("Completed", (), {"stdout": "PID TT\n123 ??\n123 ??\n123 ??\n"})()

    monkeypatch.setattr(client.subprocess, "run", run)
    assert client.ResourceSampler._threads(123) == 3
    assert commands == [["ps", "-M", "123"]]


def test_release_runner_accumulates_cpu_across_tree_total_resets(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    benchmark = load_script("benchmark_release.py")
    polls = iter([None, None, None, None, 0])
    resources = iter(
        [
            (1, 1, 1.0, 1),
            (1, 1, 3.0, 2),
            (1, 1, 1.0, 1),
            (1, 1, 4.0, 2),
        ]
    )

    class Child:
        pid = 123
        returncode = 0

        @staticmethod
        def poll() -> int | None:
            return next(polls)

        @staticmethod
        def communicate() -> tuple[bytes, bytes]:
            return b"", b""

    monkeypatch.setattr(benchmark.subprocess, "Popen", lambda *_args, **_kwargs: Child())
    monkeypatch.setattr(benchmark, "process_tree_resources", lambda _pid: next(resources))
    monkeypatch.setattr(benchmark.time, "sleep", lambda _seconds: None)
    sample = benchmark.sample_process(["ignored"])
    assert sample["cpu_seconds"] == 6.0


def test_release_runner_promotes_inner_tui_resource_measurements() -> None:
    benchmark = load_script("benchmark_release.py")
    report = {
        "result_digest": "same",
        "wall_ms": 123,
        "peak_rss_kb": 456,
        "peak_threads": 4,
        "process_count": 1,
        "cpu_seconds": 0.75,
    }
    sample = benchmark.sample_process(
        [sys.executable, "-c", f"print({json.dumps(json.dumps(report))})"],
        result_json_field="result_digest",
        resource_json_fields={
            "wall_ms": "wall_ms",
            "peak_rss_kib": "peak_rss_kb",
            "peak_threads": "peak_threads",
            "peak_processes": "process_count",
            "cpu_seconds": "cpu_seconds",
        },
    )
    assert sample["wall_ms"] == 123
    assert sample["peak_rss_kib"] == 456
    assert sample["peak_threads"] == 4
    assert sample["peak_processes"] == 1
    assert sample["cpu_seconds"] == 0.75


def test_tui_screen_tracker_buffers_every_split_csi_prefix() -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    sequence = "\x1b[38;5;6;49mX"
    for split in range(1, len(sequence)):
        tracker = client.ScreenTracker()
        tracker.feed(sequence[:split])
        tracker.feed(sequence[split:])
        assert tracker.line(0) == "X", f"CSI split at byte {split} leaked control text"


def test_tui_screen_tracker_derives_ascii_layout_and_search_state() -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    tracker = client.ScreenTracker()
    for row in (0, 2, 22):
        tracker.rows[row] = list("-" * client.SCREEN_COLS)
    tracker.rows[0][:20] = list("+ Search (press /) -")
    tracker.rows[3][:35] = list("+ Sessions - recent - ready (1/100)")
    for row in range(3, 23):
        tracker.rows[row][44] = "|"

    layout = tracker.derive_layout()

    assert layout.query_row == 1
    assert layout.list_cols.stop == 44
    assert tracker.search_state(layout) == "ready"
    assert "Search (press /)" in tracker.search_title(layout)


def test_tui_browse_wait_requires_a_post_escape_title_frame() -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    layout = client.ScreenLayout(1, range(4, 22), range(1, 44), range(46, 99))

    class Tracker:
        def full_screen(self) -> str:
            return "preview transcript says Search (press /)"

        def search_title(self, _layout: object) -> str:
            return "+ Search (press /) "

        def feed_bytes(self, _chunk: bytes) -> None:
            raise AssertionError("no post-Esc frame was emitted")

    class Tui:
        def read_chunk(self, _timeout: float) -> None:
            return None

    with pytest.raises(SystemExit, match="did not leave search mode"):
        client._await_browse_mode(
            Tui(), Tracker(), layout, 0.001, "before quit (query test#0)"
        )


def test_tui_browse_wait_ignores_preview_content_that_looks_like_the_title() -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    layout = client.ScreenLayout(1, range(4, 22), range(1, 44), range(46, 99))

    class Tracker:
        def full_screen(self) -> str:
            return "preview transcript says Search (press /)"

        def search_title(self, _layout: object) -> str:
            return "+ Search: still-active - Enter/Esc to browse "

        def feed_bytes(self, _chunk: bytes) -> None:
            pass

    class Tui:
        chunks = iter([b"post-Esc frame"])

        def read_chunk(self, _timeout: float) -> bytes | None:
            return next(self.chunks, None)

    with pytest.raises(SystemExit, match="did not leave search mode"):
        client._await_browse_mode(
            Tui(), Tracker(), layout, 0.001, "before quit (query test#0)"
        )


def test_tui_screen_tracker_rejoins_a_session_id_wrapped_across_preview_rows() -> None:
    """A subagent id is 67 characters and wraps in the preview pane at 100 columns.

    Reading only the row that carries the `Session: ` label returned the first fragment, which
    never equalled the canonical id, so the semantic traversal waited out its timeout and
    reported `could not collect TUI session ID at position 13` — a parser limit stated as a TUI
    failure. Measured against the maintainer's own index, position 13 of a `rust worker` search
    was exactly such a row.
    """
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    identifier = "claude:08a88453-1d07-48fd-8166-a00f1ca70115/agent-a2b498896d3f06295"
    pane_width = 44
    # Two renderings of the same header. The first is what a mid-value wrap looks like; the
    # second is what the pane actually produced, because an id longer than the pane does not fit
    # after its label at all, so the wrapper puts `Session:` on a row by itself. The second form
    # is the one the maintainer's own index hit, and matching `"Session: "` returned None for it.
    split_after_label = [
        client.SESSION_FIELD_LABEL,
        *[identifier[start : start + pane_width] for start in range(0, len(identifier), pane_width)],
    ]
    body = f"{client.SESSION_FIELD_LABEL} {identifier}"
    split_mid_value = [body[start : start + pane_width] for start in range(0, len(body), pane_width)]

    for wrapped in (split_mid_value, split_after_label):
        assert len(wrapped) > 1, "the fixture must actually wrap for this to prove anything"
        rendered = [
            *wrapped,
            f"{client.CWD_FIELD_LABEL} /tmp/example",
            "",
            "── First prompt ──",
        ]
        tracker = client.ScreenTracker()
        for row, text in enumerate(rendered):
            for column, character in enumerate(text):
                tracker.rows[row][column] = character

        class Layout:
            list_rows = range(len(rendered))
            preview_cols = range(0, pane_width)

        assert tracker.preview_session_id(Layout()) == identifier, f"failed on {wrapped!r}"


def test_tui_final_result_latency_uses_the_last_transition_before_stability(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    monkeypatch.setattr(client, "SETTLE_QUIET_SECONDS", 0.001)

    class Tracker:
        state = "old"

        def feed_bytes(self, chunk: bytes) -> None:
            self.state = {b"prefix": "prefix", b"final": "final"}[chunk]

        def session_list(self, _layout: object) -> str:
            return self.state

        def full_screen(self) -> str:
            return self.state

    class Tui:
        chunks = iter([b"prefix", b"final"])

        def read_chunk(self, _timeout: float) -> bytes | None:
            return next(self.chunks, None)

    tracker = Tracker()
    result_ms, final_list, _bytes = client._await_settled_change(
        Tui(), tracker, object(), "old", client.time.monotonic(), 0.05
    )
    assert result_ms is not None
    assert final_list == "final"


def test_tui_completion_signal_records_equal_final_rows(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    monkeypatch.setattr(client, "SETTLE_QUIET_SECONDS", 0.001)

    class Tracker:
        def search_state(self, _layout: object) -> str:
            return "ready"

        def session_list(self, _layout: object) -> str:
            return "unchanged rows"

        def feed_bytes(self, _chunk: bytes) -> None:
            pass

    class Tui:
        def read_chunk(self, _timeout: float) -> None:
            return None

    result_ms, final_list, _bytes = client._await_settled_change(
        Tui(),
        Tracker(),
        object(),
        "unchanged rows",
        client.time.monotonic(),
        0.05,
        completion_signal=True,
    )
    assert result_ms is not None
    assert final_list == "unchanged rows"


def test_tui_stopped_worker_never_counts_as_completed_results() -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")

    class Tracker:
        def search_state(self, _layout: object) -> str:
            return "stopped"

        def session_list(self, _layout: object) -> str:
            return "stale rows"

    class Tui:
        def read_chunk(self, _timeout: float) -> None:
            return None

    result_ms, _final_list, _bytes = client._await_settled_change(
        Tui(),
        Tracker(),
        object(),
        "stale rows",
        client.time.monotonic(),
        0.01,
        completion_signal=True,
    )
    assert result_ms is None


def test_tui_completion_signal_timeout_does_not_accept_a_list_transition(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    monkeypatch.setattr(client, "SETTLE_QUIET_SECONDS", 0.001)

    class Tracker:
        state = "old"

        def search_state(self, _layout: object) -> str:
            return "searching"

        def session_list(self, _layout: object) -> str:
            return self.state

        def full_screen(self) -> str:
            return self.state

        def feed_bytes(self, _chunk: bytes) -> None:
            self.state = "changed but not complete"

    class Tui:
        chunks = iter([b"transition"])

        def read_chunk(self, _timeout: float) -> bytes | None:
            return next(self.chunks, None)

    result_ms, _final_list, _bytes = client._await_settled_change(
        Tui(),
        Tracker(),
        object(),
        "old",
        client.time.monotonic(),
        0.01,
        completion_signal=True,
    )
    assert result_ms is None


def test_tui_repeated_final_character_settles_once_and_slash_is_not_echo(  # noqa: C901
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    sent: list[bytes] = []
    settled = 0

    class FakeTui:
        child = type("Child", (), {"pid": 1})()

        def __init__(self, *_args: object) -> None:
            pass

        def send(self, data: bytes) -> None:
            sent.append(data)

        def kill(self) -> None:
            pass

    class FakeTracker:
        def full_screen(self) -> str:
            return "Sessions Preview Enter/Esc to browse"

        def derive_layout(self) -> object:
            return type("Layout", (), {"list_rows": range(3)})()

        def search_state(self, _layout: object) -> str:
            return "ready"

        def session_list(self, _layout: object) -> str:
            return "rows"

        def session_result_count(self) -> int:
            return 1

    class FakeSampler:
        def __init__(self, _pid: int) -> None:
            pass

        def start(self) -> None:
            pass

        def stop(self) -> None:
            pass

    def key_effects(
        _tui: object,
        _tracker: object,
        _layout: object,
        _prefix: str,
        list_before: str,
        _sent_at: float,
        _timeout: float,
        need_results: bool,
    ) -> tuple[int, None, str, int, bool]:
        assert not need_results
        return 1, None, list_before, 1, False

    def settle(*_args: object, **_kwargs: object) -> tuple[int, str, int]:
        nonlocal settled
        settled += 1
        return 2, "rows", 1

    monkeypatch.setattr(client, "TuiProcess", FakeTui)
    monkeypatch.setattr(client, "ScreenTracker", FakeTracker)
    monkeypatch.setattr(client, "ResourceSampler", FakeSampler)
    monkeypatch.setattr(client, "drain_output", lambda *_args: None)
    monkeypatch.setattr(client, "_await_search_mode", lambda *_args: (1, 1))
    monkeypatch.setattr(client, "_await_key_effects", key_effects)
    monkeypatch.setattr(client, "_await_settled_change", settle)
    monkeypatch.setattr(
        client,
        "_finish_run",
        lambda _tui, _tracker, _layout, _timeout, label, echo, results, transitions, _sampler, output, digest, **_kwargs: {
            "query": label,
            "echo_ms": echo,
            "results_ms": results,
            "transitions": transitions,
            "output_bytes": output,
            "digest": digest,
        },
    )
    monkeypatch.setattr(client.time, "sleep", lambda _seconds: None)

    run = client._measure_once("aise", "fixture.db", "aba", 0.0, 0.01, "aba#0")
    assert sent == [b"/", b"a", b"b", b"a"]
    assert settled == 1
    assert run["echo_ms"] == [1, 1, 1]
    assert run["mode_entry_ms"] == 1


def test_tui_latency_missing_echo_or_default_result_fails_closed(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")

    def incomplete_run(*_args: object, **_kwargs: object) -> dict[str, object]:
        return {
            "query": "SQLite 1#0",
            "mode_entry_ms": 1,
            "echo_ms": [0, -1],
            "results_ms": None,
            "transitions": 0,
            "peak_rss_kb": 1,
            "peak_cpu_pct": 0.0,
            "peak_threads": 1,
            "process_count": 1,
            "output_bytes": 1,
            "digest": "a" * 64,
        }

    monkeypatch.setattr(client, "_canonical_result_ids", lambda *_args: [])
    monkeypatch.setattr(client, "_measure_once", incomplete_run)
    with pytest.raises(SystemExit, match=r"missing.*echo|missing.*result"):
        client.measure_latency("aise", "fixture.db", ["SQLite 1"], 1, 0.0, 0.01)


def test_tui_latency_report_names_workload_symbols_and_throughput(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    fixture = tmp_path / "fixture.db"
    connection = sqlite3.connect(fixture)
    try:
        connection.executescript(
            """
            create table sessions (id text primary key);
            create table messages (session_id text);
            create table transcripts (session_id text primary key, transcript_text text);
            insert into sessions values ('s1');
            insert into messages values ('s1'), ('s1');
            insert into transcripts values ('s1', '12345');
            """
        )
        connection.commit()
    finally:
        connection.close()

    def complete_run(*_args: object, **_kwargs: object) -> dict[str, object]:
        return {
            "query": "SQLite 1#0",
            "mode_entry_ms": 1,
            "echo_ms": [1],
            "results_ms": 2,
            "transitions": 1,
            "peak_rss_kb": 1,
            "peak_cpu_pct": 0.0,
            "peak_threads": 3,
            "process_count": 1,
            "output_bytes": 10,
            "digest": "a" * 64,
            "result_count": 3,
            "visible_rows": 19,
            "wall_ms": 100,
        }

    monkeypatch.setattr(client, "_canonical_result_ids", lambda *_args: ["s1", "s2", "s3"])
    monkeypatch.setattr(client, "_probe_tui_result_ids", lambda *_args: ["s1", "s2", "s3"])
    monkeypatch.setattr(client, "_measure_once", complete_run)
    report = client.measure_latency(
        "aise", str(fixture), ["SQLite 1"], 1, 0.0, 0.01
    )
    assert report["workload"] == {
        "query_characters": {"SQLite 1": 8},
        "retained_sessions_max": 3,
        "visible_rows": 19,
        "transcript_bytes_max": 5,
        "messages_per_session_max": 2,
        "scoring_workers": 2,
    }
    assert report["typed_keys_per_second"] == 80
    expected_query_digest = client.hashlib.sha256(b'["s1","s2","s3"]').hexdigest()
    assert report["result_digests_by_query"] == {"SQLite 1": expected_query_digest}


def test_tui_semantic_probe_rejects_equal_count_wrong_membership(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    fixture = tmp_path / "fixture.db"
    connection = sqlite3.connect(fixture)
    connection.executescript(
        "create table messages(session_id text); create table transcripts(session_id text, transcript_text text);"
    )
    connection.close()

    def complete_run(*_args: object, **_kwargs: object) -> dict[str, object]:
        return {
            "query": "q#0",
            "mode_entry_ms": 1,
            "echo_ms": [1],
            "results_ms": 2,
            "transitions": 1,
            "peak_rss_kb": 1,
            "peak_cpu_pct": 0.0,
            "peak_threads": 1,
            "process_count": 1,
            "output_bytes": 1,
            "digest": "a" * 64,
            "result_count": 2,
            "visible_rows": 2,
            "wall_ms": 100,
        }

    monkeypatch.setattr(client, "_measure_once", complete_run)
    monkeypatch.setattr(client, "_probe_tui_result_ids", lambda *_args: ["wrong", "s2"])
    monkeypatch.setattr(client, "_canonical_result_ids", lambda *_args: ["s1", "s2"])
    with pytest.raises(SystemExit, match="ordered session IDs differ"):
        client.measure_latency("aise", str(fixture), ["q"], 1, 0.0, 0.01)


def test_tui_result_limit_protocol_matches_rust_browser_floor() -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    source = (ROOT / "rust/ai-session-search-core/src/tui.rs").read_text()
    match = re.search(r"const TUI_MIN_BROWSER_RESULTS: usize = (\d+);", source)
    assert match is not None
    assert client.TUI_RESULT_LIMIT == int(match.group(1))


def test_tui_latency_repetitions_require_one_digest_per_query(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    calls = 0

    def nondeterministic_run(*_args: object, **_kwargs: object) -> dict[str, object]:
        nonlocal calls
        calls += 1
        return {
            "query": f"SQLite 1#{calls - 1}",
            "mode_entry_ms": 1,
            "echo_ms": [1],
            "results_ms": 2,
            "transitions": 1,
            "peak_rss_kb": 1,
            "peak_cpu_pct": 0.0,
            "peak_threads": 1,
            "process_count": 1,
            "output_bytes": 1,
            "digest": ("a" if calls == 1 else "b") * 64,
            "result_count": 1,
            "visible_rows": 19,
            "wall_ms": 100,
        }

    monkeypatch.setattr(client, "_canonical_result_ids", lambda *_args: ["s1"])
    monkeypatch.setattr(client, "_probe_tui_result_ids", lambda *_args: ["s1"])
    monkeypatch.setattr(client, "_measure_once", nondeterministic_run)
    with pytest.raises(SystemExit, match=r"non-deterministic.*SQLite 1"):
        client.measure_latency("aise", "fixture.db", ["SQLite 1"], 2, 0.0, 0.01)


def test_benchmark_build_order_balances_first_run_bias() -> None:
    benchmark = load_python_file(ROOT / "scripts" / "benchmark_release.py")
    builds = [{"label": "baseline"}, {"label": "candidate"}]

    observed = [
        [build["label"] for build in benchmark.balanced_build_order(builds, repetition)]
        for repetition in range(4)
    ]

    assert observed == [
        ["baseline", "candidate"],
        ["candidate", "baseline"],
        ["baseline", "candidate"],
        ["candidate", "baseline"],
    ]


def test_tui_output_bytes_use_the_single_captured_pty_ledger() -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")

    class FakeTui:
        child = type("Child", (), {"returncode": 0, "stderr": None})()
        terminal_attributes_restored = True

        def __init__(self) -> None:
            self.browse_observed = False
            self.sent: list[bytes] = []

        def send(self, data: bytes) -> None:
            if data == b"q":
                assert self.browse_observed, "q was sent before Esc rendered browse mode"
            self.sent.append(data)

        def read_chunk(self, _timeout: float) -> bytes:
            self.browse_observed = True
            return b"Search (press /)"

        def wait_draining(self, _timeout: float) -> int:
            return 0

        def drain_after_exit(self) -> bytes:
            restored = b"\x1b[?1049l\x1b[?25h"
            return b"x" * (120 - len(restored)) + restored

    sampler = type(
        "Sampler",
        (),
        {
            "peak_rss_kb": 1,
            "peak_cpu_pct": 0.0,
            "cpu_seconds": 1.0,
            "peak_threads": 1,
            "process_count": 1,
            "stop": lambda self: setattr(self, "peak_rss_kb", 9),
        },
    )()
    tui = FakeTui()
    result = client._finish_run(
        tui,
        client.ScreenTracker(),
        client.ScreenLayout(1, range(4, 22), range(1, 44), range(46, 99)),
        0.01,
        "q#0",
        [1],
        2,
        1,
        sampler,
        100,
        "a" * 64,
    )
    assert tui.sent == [b"\x1b", b"q"]
    assert result["output_bytes"] == 120
    assert result["peak_rss_kb"] == 9, "report must copy peaks after sampler.stop() joins"


def test_tui_resource_sampler_retains_process_peak(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    sampled_second = __import__("threading").Event()
    trees = iter([[1, 2, 3], [1]])
    sampler = client.ResourceSampler(1)

    def tree() -> list[int]:
        try:
            pids = next(trees)
        except StopIteration:
            sampled_second.set()
            return [1]
        if pids == [1]:
            sampled_second.set()
        return pids

    monkeypatch.setattr(sampler, "_tree", tree)
    monkeypatch.setattr(sampler, "_threads", lambda _pid: 1)
    monkeypatch.setattr(
        client.subprocess,
        "run",
        lambda *_args, **_kwargs: type("Result", (), {"stdout": "1 0.0 00:01"})(),
    )
    sampler.start()
    assert sampled_second.wait(1), "sampler did not take two process-tree samples"
    sampler.stop()
    assert sampler.process_count == 3
    assert sampler.peak_rss_kb == 3
    assert sampler.peak_threads == 3


def test_tui_resource_sampler_stop_joins_an_in_progress_sample(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    threading = __import__("threading")
    entered = threading.Event()
    release = threading.Event()
    sampler = client.ResourceSampler(1)

    def blocked_tree() -> list[int]:
        entered.set()
        release.wait(1)
        return []

    monkeypatch.setattr(sampler, "_tree", blocked_tree)
    sampler.start()
    assert entered.wait(1)
    stopper = threading.Thread(target=sampler.stop)
    stopper.start()
    stopper.join(0.02)
    assert stopper.is_alive(), "stop returned before the in-progress sample was joined"
    release.set()
    stopper.join(1)
    assert not stopper.is_alive()


@pytest.mark.skipif(sys.platform == "win32", reason="real PTY ownership is POSIX-only")
def test_tui_process_uses_an_owned_hermetic_config_environment(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    captured: dict[str, object] = {}
    master_fd, slave_fd = __import__("os").openpty()

    class FakeChild:
        pid = 123
        returncode = 0
        stderr = None

        @staticmethod
        def poll() -> int:
            return 0

    def fake_popen(argv: list[str], **kwargs: object) -> FakeChild:
        captured["argv"] = argv
        captured.update(kwargs)
        return FakeChild()

    monkeypatch.setenv("AI_SESSION_SEARCH_CONFIG", "/private/live/config.toml")
    monkeypatch.setattr(client.pty, "openpty", lambda: (master_fd, slave_fd))
    monkeypatch.setattr(client.fcntl, "ioctl", lambda *_args: None)
    monkeypatch.setattr(client.subprocess, "Popen", fake_popen)
    tui = client.TuiProcess("/fixture/aise", "/fixture/generated.db")
    try:
        env = captured["env"]
        assert isinstance(env, dict)
        assert env["AI_SESSION_SEARCH_CONFIG"] != "/private/live/config.toml"
        assert str(env["AI_SESSION_SEARCH_CONFIG"]).startswith(str(env["HOME"]))
        assert env["HOME"] != __import__("os").environ.get("HOME")
        argv = captured["argv"]
        assert isinstance(argv, list)
        assert argv[argv.index("--threads") + 1] == "2"
    finally:
        tui.kill()


@pytest.mark.skipif(sys.platform == "win32", reason="real PTY ownership is POSIX-only")
def test_tui_process_closes_owned_resources_when_spawn_fails(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")
    os_module = __import__("os")
    master_fd, slave_fd = os_module.openpty()
    monkeypatch.setattr(client.pty, "openpty", lambda: (master_fd, slave_fd))
    monkeypatch.setattr(client.fcntl, "ioctl", lambda *_args: None)
    monkeypatch.setattr(
        client.subprocess,
        "Popen",
        lambda *_args, **_kwargs: (_ for _ in ()).throw(OSError("spawn failed")),
    )

    with pytest.raises(OSError, match="spawn failed"):
        client.TuiProcess("/fixture/aise", "/fixture/generated.db")
    for descriptor in (master_fd, slave_fd):
        with pytest.raises(OSError):
            os_module.fstat(descriptor)


def test_tui_startup_refuses_to_claim_unobserved_terminal_restoration(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    client = load_python_file(ROOT / "benchmarks" / "tui_client.py")

    class FakeTui:
        child = type("Child", (), {"returncode": 0, "stderr": None})()

        def __init__(self, _binary: str, _fixture: str) -> None:
            pass

        def send(self, _data: bytes) -> None:
            pass

        def read_chunk(self, _timeout: float) -> bytes:
            return b"Sessions Preview Session: claude:s1 CWD: /fixture"

        def wait_draining(self, _timeout: float) -> int:
            return 0

        def drain_after_exit(self) -> bytes:
            return b"Sessions Preview Session: claude:s1 CWD: /fixture"

        def kill(self) -> None:
            pass

    monkeypatch.setattr(client, "TuiProcess", FakeTui)
    monkeypatch.setattr(client.time, "sleep", lambda _seconds: None)
    with pytest.raises(SystemExit, match=r"terminal.*restore"):
        client.run_startup_case("aise", "fixture.db", 0.0, 0.01)
    assert "terminal_restored" not in capsys.readouterr().out


def test_release_manifest_does_not_pass_search_refresh_policy_to_db_commands() -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    db_cases = [case for case in manifest["cases"] if "db" in case["argv"]]

    assert db_cases
    assert all("--index-refresh" not in case["argv"] for case in db_cases)


def test_sqlite_state_distinguishes_coordination_files_from_durable_wal(tmp_path: Path) -> None:
    benchmark = load_script("benchmark_release.py")
    database = tmp_path / "fixture.db"
    database.write_bytes(b"database")
    Path(f"{database}-shm").write_bytes(b"coordination")
    Path(f"{database}-wal").write_bytes(b"")
    state = benchmark.sqlite_file_state(database)
    assert set(state) == {"database", "-shm", "-wal"}
    assert set(benchmark.durable_sqlite_state(state)) == {"database"}
    Path(f"{database}-wal").write_bytes(b"committed pages")
    assert set(benchmark.durable_sqlite_state(benchmark.sqlite_file_state(database))) == {
        "database",
        "-wal",
    }


def test_renderer_rejects_nondeterministic_case_digests() -> None:
    renderer = load_script("render_benchmark_report.py")
    rows = [
        {"result_sha256": "a", "wall_ms": 1, "cpu_seconds": 0, "peak_rss_kib": 1, "peak_threads": 1, "peak_processes": 1},
        {"result_sha256": "b", "wall_ms": 1, "cpu_seconds": 0, "peak_rss_kib": 1, "peak_threads": 1, "peak_processes": 1},
    ]
    with pytest.raises(ValueError, match="non-deterministic"):
        renderer.summarize(rows)


def test_renderer_rejects_missing_or_duplicate_repetitions(tmp_path: Path) -> None:
    renderer = load_script("render_benchmark_report.py")
    run = {"kind": "run", "build": "candidate", "selected_cases": ["case"], "repetitions": 2}
    one_sample = {"case": [{"kind": "sample", "build": "candidate", "case": "case", "repetition": 0}]}
    with pytest.raises(ValueError, match=r"case.*repetitions.*expected 2"):
        renderer.validate_sample_set(run, one_sample)

    evidence = tmp_path / "truncated.jsonl"
    evidence.write_text("".join(json.dumps(row) + "\n" for row in [run, *one_sample["case"]]))
    with pytest.raises(ValueError, match=r"case.*repetitions.*expected 2"):
        renderer.load(evidence, "candidate")

    duplicate = {"case": [{"repetition": 0}, {"repetition": 0}]}
    with pytest.raises(ValueError, match=r"case.*repetitions.*expected.*0, 1"):
        renderer.validate_sample_set(run, duplicate)


def test_renderer_loads_structured_relevance_result(tmp_path: Path) -> None:
    renderer = load_script("render_benchmark_report.py")
    log = tmp_path / "relevance.log"
    log.write_text('test output\nAISE_BENCHMARK_JSON={"kind":"fuzzy_relevance","held_out_cases":8,"recall_at_10":1.0,"mrr":0.75}\n')
    assert renderer.load_relevance(log) == {
        "kind": "fuzzy_relevance",
        "held_out_cases": 8,
        "recall_at_10": 1.0,
        "mrr": 0.75,
    }
    log.write_text("no structured result\n")
    with pytest.raises(ValueError, match="fuzzy_relevance"):
        renderer.load_relevance(log)


def test_renderer_missing_relevance_evidence_fails_closed() -> None:
    renderer = load_script("render_benchmark_report.py")

    assert renderer.relevance_gate(None) is False


def test_renderer_emits_scale_table_without_relevance_log(tmp_path: Path) -> None:
    renderer = load_script("render_benchmark_report.py")
    raw = tmp_path / "scale.jsonl"
    rows: list[dict[str, Any]] = []
    for build in ("baseline", "candidate"):
        rows.append(
            {
                "kind": "run",
                "build": build,
                "fixture": {"counts": {"messages": 64}},
                "selected_cases": [
                    "cli-exact-content",
                    "cli-regex-content",
                    "cli-fuzzy-content",
                ],
                "repetitions": 1,
            }
        )
        for case in ("cli-exact-content", "cli-regex-content", "cli-fuzzy-content"):
            rows.append(
                {
                    "kind": "sample",
                    "build": build,
                    "case": case,
                    "result_sha256": "same",
                    "wall_ms": 1,
                    "cpu_seconds": 0,
                    "peak_rss_kib": 2,
                    "peak_threads": 1,
                    "peak_processes": 1,
                    "repetition": 0,
                }
            )
    raw.write_text("".join(json.dumps(row) + "\n" for row in rows))
    output = renderer.scaling_lines([("1x", raw)], {}, "baseline", "candidate")
    assert "## 1x/2x/4x scaling" in output
    assert any("| 1x | 64 | candidate |" in line for line in output)


def test_renderer_uses_portable_artifact_labels() -> None:
    renderer = load_script("render_benchmark_report.py")
    private_root = Path("/Users/private-user/release-evidence")
    command = renderer.renderer_command(
        private_root / "baseline.jsonl",
        private_root / "candidate.jsonl",
        [private_root / "paired.jsonl"],
        [],
        [],
        [],
        {},
        None,
    )

    assert "/Users/private-user" not in command
    assert "BASELINE_JSONL" in command
    assert "CANDIDATE_JSONL" in command


def test_renderer_refuses_a_release_go_decision_for_private_fixture_artifacts(
    tmp_path: Path,
) -> None:
    renderer = load_script("render_benchmark_report.py")
    evidence = tmp_path / "private-profile.jsonl"
    fixture = {
        "sha256": "a" * 64,
        "bytes": 4096,
        "schema_version": 5,
        "counts": {"sessions": 1, "messages": 2, "file_edits": 0},
    }
    metadata = {
        "commit": "b" * 40,
        "dirty": False,
        "source_state_sha256": "c" * 64,
        "binary_sha256": "d" * 64,
        "manifest_sha256": "e" * 64,
        "os": "TestOS 1",
        "machine": "test-machine",
        "python": "3.12",
        "sqlite": "3.47",
    }
    sample = {
        "kind": "sample",
        "case": "portable-case",
        "surface": "cli",
        "exit_code": 0,
        "result_sha256": "f" * 64,
        "wall_ms": 1,
        "cpu_seconds": 0,
        "peak_rss_kib": 1,
        "peak_threads": 1,
        "peak_processes": 1,
        "repetition": 0,
    }
    rows = []
    for build in ("baseline", "candidate"):
        rows.extend(
            [
                {
                    "kind": "run",
                    "build": build,
                    "metadata": metadata,
                    "fixture": fixture,
                    "contracts": {"portable-case": {"require_equal": True}},
                    "selected_cases": ["portable-case"],
                    "repetitions": 1,
                    "artifact_privacy": {
                        "classification": "private_local_fixture",
                        "publishable": False,
                    },
                },
                {**sample, "build": build},
            ]
        )
    evidence.write_text("".join(json.dumps(row) + "\n" for row in rows))

    report = renderer.render(
        evidence,
        evidence,
        "baseline",
        "candidate",
        [],
        {},
        [],
        [],
        [],
        None,
    )

    assert "**NO-GO" in report
    assert "publishable generated fixture: no" in report
    assert "held-out relevance gate: not supplied" in report
    assert "--baseline BASELINE_BINARY" in report
    assert "--baseline-repository BASELINE_REPOSITORY" in report
    assert "--candidate CANDIDATE_BINARY" in report
    assert "--relevance-log RELEVANCE_LOG" in report
    assert "nine-repetition" not in report
    assert "The measured benchmark table is a regression signal" in report
    assert "private_local_fixture" in report


def test_tracked_docs_contain_no_personal_install_paths() -> None:
    personal_home = str(Path.home())
    for path in (
        ROOT / "docs/development/maintainer-requirements-and-design-decisions.md",
        ROOT / "docs/migration/ai-session-search-major-migration.md",
    ):
        assert personal_home not in path.read_text(encoding="utf-8"), path
