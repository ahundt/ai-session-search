from __future__ import annotations

import importlib.util
import json
import re
import subprocess
import sys
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[1]


def load_script(name: str) -> ModuleType:
    path = ROOT / "scripts" / name
    spec = importlib.util.spec_from_file_location(path.stem, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_release_manifest_clients_use_only_canonical_query_modes() -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    client_cases = [case for case in manifest["cases"] if case["surface"] in {"python", "mcp"}]
    assert client_cases
    for case in client_cases:
        mode_index = case["argv"].index("--mode") + 1
        assert case["argv"][mode_index] in {"literal", "regex", "fuzzy"}, case["id"]


def test_release_manifest_rust_driver_uses_only_canonical_query_modes() -> None:
    manifest = json.loads((ROOT / "benchmarks/release_manifest.json").read_text())
    rust_cases = [case for case in manifest["cases"] if case["surface"] == "rust"]

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


def test_benchmark_help_does_not_claim_an_obsolete_fixture_schema() -> None:
    source = (ROOT / "scripts/benchmark_release.py").read_text()

    assert "schema-v4" not in source


def test_mcp_benchmark_client_uses_only_canonical_search_contract() -> None:
    source = (ROOT / "benchmarks/mcp_client.py").read_text()

    assert '"response_format"' not in source
    assert '["structuredContent"]["hits"]' not in source
    assert '["structuredContent"]["results"]' in source


def test_python_benchmark_client_uses_only_canonical_search_contract() -> None:
    source = (ROOT / "benchmarks/python_client.py").read_text()

    assert ".hits" not in source
    assert ".results" in source


def test_benchmark_clients_do_not_use_removed_query_mode_flags() -> None:
    for name in ("burst_client.py", "mcp_client.py", "python_client.py", "tui_client.py"):
        source = (ROOT / "benchmarks" / name).read_text()
        assert '"--fuzzy"' not in source, name
        assert '"--regex"' not in source, name


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


def test_renderer_emits_scale_table_without_relevance_log(tmp_path: Path) -> None:
    renderer = load_script("render_benchmark_report.py")
    raw = tmp_path / "scale.jsonl"
    rows: list[dict[str, Any]] = []
    for build in ("baseline", "candidate"):
        rows.append({"kind": "run", "build": build, "fixture": {"counts": {"messages": 64}}})
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
    assert "private_local_fixture" in report


def test_tracked_docs_contain_no_personal_install_paths() -> None:
    personal_home = str(Path.home())
    for path in (
        ROOT / "docs/development/maintainer-requirements-and-design-decisions.md",
        ROOT / "docs/migration/ai-session-search-major-migration.md",
    ):
        assert personal_home not in path.read_text(), path
