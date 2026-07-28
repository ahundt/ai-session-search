from __future__ import annotations

import importlib.util
import json
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


def test_mcp_benchmark_client_uses_only_canonical_search_contract() -> None:
    source = (ROOT / "benchmarks/mcp_client.py").read_text()

    assert '"response_format"' not in source
    assert '["structuredContent"]["hits"]' not in source
    assert '["structuredContent"]["results"]' in source


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
