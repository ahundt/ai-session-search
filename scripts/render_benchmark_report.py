#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Render an AI Session Search before/after Markdown report from benchmark JSONL."""

from __future__ import annotations

import argparse
import json
import math
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any, cast

BENCHMARK_JSON_PREFIX = "AISE_BENCHMARK_JSON="


def load_relevance(path: Path) -> dict[str, Any]:
    matches = []
    for line in path.read_text().splitlines():
        if BENCHMARK_JSON_PREFIX in line:
            payload = line.split(BENCHMARK_JSON_PREFIX, 1)[1]
            row = cast(dict[str, Any], json.loads(payload))
            if row.get("kind") == "fuzzy_relevance":
                matches.append(row)
    if len(matches) != 1:
        raise ValueError(f"{path}: expected exactly one fuzzy_relevance result")
    return matches[0]


def load(
    path: Path, build: str
) -> tuple[dict[str, Any], dict[str, list[dict[str, Any]]]]:
    run: dict[str, Any] | None = None
    samples: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for line_number, line in enumerate(path.read_text().splitlines(), 1):
        row = json.loads(line)
        if row.get("build") != build:
            continue
        if row.get("kind") == "run":
            if run is not None:
                raise ValueError(f"{path}:{line_number}: more than one run record")
            run = row
        elif row.get("kind") == "sample":
            samples[row["case"]].append(row)
        else:
            raise ValueError(f"{path}:{line_number}: unknown record kind")
    if run is None or not samples:
        raise ValueError(f"{path}: requires one {build!r} run and at least one sample")
    return run, dict(samples)


def percentile(values: list[float], quantile: float) -> float:
    ordered = sorted(values)
    index = max(0, math.ceil(quantile * len(ordered)) - 1)
    return ordered[index]


def summarize(rows: list[dict[str, Any]]) -> dict[str, Any]:
    successful = [row for row in rows if int(row.get("exit_code", 0)) == 0]
    digests = {row["result_sha256"] for row in successful}
    if len(digests) != 1:
        if digests:
            raise ValueError("case has non-deterministic successful result digests")
        digests = {"no-successful-result"}
    walls = [float(row["wall_ms"]) for row in rows]
    cpus = [float(row["cpu_seconds"]) for row in rows]
    return {
        "samples": len(rows), "digest": next(iter(digests)),
        "wall_median": statistics.median(walls), "wall_p95": percentile(walls, 0.95),
        "wall_min": min(walls), "wall_max": max(walls),
        "cpu_median": statistics.median(cpus),
        "rss_peak": max(int(row["peak_rss_kib"]) for row in rows),
        "threads_peak": max(int(row["peak_threads"]) for row in rows),
        "processes_peak": max(int(row["peak_processes"]) for row in rows),
        "mutated_samples": sum(bool(row.get("durable_fixture_mutated")) for row in rows),
        "coordination_changed_samples": sum(
            bool(row.get("fixture_files_changed")) and not bool(row.get("durable_fixture_mutated"))
            for row in rows
        ),
        "failures": sum(int(row.get("exit_code", 0)) != 0 for row in rows),
    }


def percent(before: float, after: float) -> str:
    if before == 0:
        return "n/a"
    return f"{(after - before) * 100 / before:+.1f}%"


def metadata_lines(label: str, source: Path, run: dict[str, Any]) -> list[str]:
    metadata = run["metadata"]
    fixture = run["fixture"]
    privacy = run.get(
        "artifact_privacy",
        {"classification": "legacy_unclassified", "publishable": False},
    )
    return [
        f"- {label} raw data file: `{source.name}`",
        f"- {label} artifact privacy: `{privacy['classification']}` "
        f"(publishable: `{str(privacy['publishable']).lower()}`)",
        f"- {label} commit: `{metadata['commit']}` (dirty: `{str(metadata['dirty']).lower()}`)",
        f"- {label} source-state SHA-256: `{metadata['source_state_sha256']}`",
        f"- {label} binary SHA-256: `{metadata['binary_sha256']}`",
        f"- {label} fixture: schema {fixture['schema_version']}, {fixture['bytes']} bytes, "
        f"SHA-256 `{fixture['sha256']}`, counts `{json.dumps(fixture['counts'], sort_keys=True)}`",
        f"- {label} environment: `{metadata['os']}` / `{metadata['machine']}`; Python "
        f"`{metadata['python']}`; SQLite `{metadata['sqlite']}`",
    ]


def renderer_command(
    baseline_path: Path,
    candidate_path: Path,
    overlay_paths: list[Path],
    baseline_overlay_paths: list[Path],
    candidate_overlay_paths: list[Path],
    scale_paths: list[tuple[str, Path]],
    candidate_scale_paths: dict[str, Path],
    relevance_path: Path | None,
) -> str:
    command = (
        "uv run python scripts/render_benchmark_report.py --baseline BASELINE_JSONL "
        "--candidate CANDIDATE_JSONL"
    )
    command += "".join(
        f" --overlay PAIRED_OVERLAY_{index}_JSONL"
        for index, _path in enumerate(overlay_paths, 1)
    )
    command += "".join(
        f" --baseline-overlay BASELINE_OVERLAY_{index}_JSONL"
        for index, _path in enumerate(baseline_overlay_paths, 1)
    )
    command += "".join(
        f" --candidate-overlay CANDIDATE_OVERLAY_{index}_JSONL"
        for index, _path in enumerate(candidate_overlay_paths, 1)
    )
    command += "".join(
        f" --scale {label}:PAIRED_SCALE_{index}_JSONL"
        for index, (label, _path) in enumerate(scale_paths, 1)
    )
    command += "".join(
        f" --candidate-scale {label}:CANDIDATE_SCALE_{index}_JSONL"
        for index, (label, _path) in enumerate(candidate_scale_paths.items(), 1)
    )
    if relevance_path is not None:
        command += " --relevance-log RELEVANCE_LOG"
    return command + (
        " --output "
        "notes/2026_07_16_1726_ai_session_search_1_0_before_after_performance_report.md"
    )


def relevance_lines(path: Path) -> list[str]:
    relevance = load_relevance(path)
    recall = float(relevance["recall_at_10"])
    mrr = float(relevance["mrr"])
    passed = recall == 1.0 and mrr >= 0.5
    return [
        "",
        "## Held-out fuzzy relevance",
        "",
        f"- Raw benchmark log: `{path}`",
        f"- Independently frozen held-out cases: {int(relevance['held_out_cases'])}.",
        f"- Recall@10: {recall:.3f} (required: 1.000).",
        f"- Mean reciprocal rank: {mrr:.3f} (required: at least 0.500).",
        f"- Gate: {'pass' if passed else '**fail**'}.",
    ]


def relevance_gate(path: Path | None) -> bool:
    if path is None:
        return True
    relevance = load_relevance(path)
    return (
        float(relevance["recall_at_10"]) == 1.0 and float(relevance["mrr"]) >= 0.5
    )


def scaling_lines(
    scale_paths: list[tuple[str, Path]],
    candidate_scale_paths: dict[str, Path],
    baseline_build: str,
    candidate_build: str,
) -> list[str]:
    if not scale_paths:
        return []
    lines = [
        "",
        "## 1x/2x/4x scaling",
        "",
        "| Scale | Messages | Build | Exact median ms / peak KiB | Regex median ms / peak KiB | Fuzzy median ms / peak KiB |",
        "|---:|---:|---|---:|---:|---:|",
    ]
    for label, path in scale_paths:
        for build, source in (
            (baseline_build, path),
            (candidate_build, candidate_scale_paths.get(label, path)),
        ):
            run, rows = load(source, build)
            values = []
            for case in ("cli-exact-content", "cli-regex-content", "cli-fuzzy-content"):
                summary = summarize(rows[case])
                values.append(f"{summary['wall_median']:.2f} / {summary['rss_peak']}")
            lines.append(
                f"| {label} | {run['fixture']['counts']['messages']} | {build} | "
                f"{values[0]} | {values[1]} | {values[2]} |"
            )
    lines.extend([
        "",
        "Scale fixtures are deterministic and independently generated per schema. Peak RSS is "
        "sampled process-tree resident memory; median latency includes process startup.",
    ])
    return lines


def render(
    baseline_path: Path,
    candidate_path: Path,
    baseline_build: str,
    candidate_build: str,
    scale_paths: list[tuple[str, Path]],
    candidate_scale_paths: dict[str, Path],
    overlay_paths: list[Path],
    baseline_overlay_paths: list[Path],
    candidate_overlay_paths: list[Path],
    relevance_path: Path | None,
) -> str:
    baseline_run, baseline_rows = load(baseline_path, baseline_build)
    candidate_run, candidate_rows = load(candidate_path, candidate_build)
    for overlay in overlay_paths:
        _, baseline_overlay = load(overlay, baseline_build)
        _, candidate_overlay = load(overlay, candidate_build)
        baseline_rows.update(baseline_overlay)
        candidate_rows.update(candidate_overlay)
    for overlay in baseline_overlay_paths:
        _, replacement = load(overlay, baseline_build)
        baseline_rows.update(replacement)
    for overlay in candidate_overlay_paths:
        _, replacement = load(overlay, candidate_build)
        candidate_rows.update(replacement)
    shared = sorted(set(baseline_rows) & set(candidate_rows))
    if not shared:
        raise ValueError("baseline and candidate have no shared cases")
    contracts = candidate_run.get("contracts")
    if contracts is None:
        # Compatibility for private evidence produced before contracts were embedded.
        manifest = json.loads(Path(candidate_run["metadata"]["manifest"]).read_text())
        contracts = {case["id"]: case for case in manifest["cases"]}
    semantic_mismatches = [
        case for case in shared
        if contracts.get(case, {}).get("require_equal", True)
        and summarize(baseline_rows[case])["digest"] != summarize(candidate_rows[case])["digest"]
    ]
    candidate_durable_mutations = sum(
        bool(row.get("durable_fixture_mutated"))
        for rows in candidate_rows.values()
        for row in rows
    )
    candidate_failures = sum(
        int(row.get("exit_code", 0)) != 0
        for rows in candidate_rows.values()
        for row in rows
    )
    relevance_passed = relevance_gate(relevance_path)
    publishable = bool(candidate_run.get("artifact_privacy", {}).get("publishable"))
    decision = (
        "GO" if publishable and not semantic_mismatches and candidate_durable_mutations == 0
        and candidate_failures == 0 and relevance_passed else "NO-GO"
    )
    exact_renderer_command = renderer_command(
        baseline_path,
        candidate_path,
        overlay_paths,
        baseline_overlay_paths,
        candidate_overlay_paths,
        scale_paths,
        candidate_scale_paths,
        relevance_path,
    )
    lines = [
        "# AI Session Search before/after benchmark report",
        "",
        "This file is generated only from the raw JSONL named below. Re-run the renderer with "
        "`--check` to detect any hand-edited number.",
        "",
        "## Executive decision",
        "",
        f"**{decision} for the measured search/runtime consolidation gates.** Semantic digest "
        f"mismatches: {len(semantic_mismatches)}; candidate durable read mutations: "
        f"{candidate_durable_mutations}; candidate process failures: {candidate_failures}; paired "
        f"release cases: {len(shared)}; held-out relevance gate: "
        f"{'pass' if relevance_passed else 'fail'}; publishable generated fixture: "
        f"{'yes' if publishable else 'no'}.",
        "",
        "## Reproducibility metadata",
        "",
        *metadata_lines("Baseline", baseline_path, baseline_run),
        *metadata_lines("Candidate", candidate_path, candidate_run),
        *(f"- Corrected paired-case overlay file: `{path.name}`" for path in overlay_paths),
        *(
            f"- Corrected baseline-case overlay file: `{path.name}`"
            for path in baseline_overlay_paths
        ),
        *(
            f"- Corrected candidate-case overlay file: `{path.name}`"
            for path in candidate_overlay_paths
        ),
        *(f"- Paired scale data ({label}) file: `{path.name}`" for label, path in scale_paths),
        *(
            f"- Candidate scale data ({label}) file: `{path.name}`"
            for label, path in candidate_scale_paths.items()
        ),
        f"- Query manifest SHA-256: `{candidate_run['metadata']['manifest_sha256']}`",
        "",
        "## Comparable public-surface cases",
        "",
        "| Case | Surface | Digests equal | n | Failures before → after | Median ms before → after (Δ) | p95 ms before → after | Median CPU s before → after | Peak RSS KiB before → after (Δ) | Peak processes before → after | Peak threads before → after | Mutated samples before → after |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    regressions: list[str] = []
    for case in shared:
        before = summarize(baseline_rows[case])
        after = summarize(candidate_rows[case])
        equal = before["digest"] == after["digest"]
        surface = candidate_rows[case][0]["surface"]
        lines.append(
            f"| `{case}` | {surface} | {'yes' if equal else '**no**'} | "
            f"{before['samples']}/{after['samples']} | {before['failures']} → {after['failures']} | "
            f"{before['wall_median']:.2f} → "
            f"{after['wall_median']:.2f} ({percent(before['wall_median'], after['wall_median'])}) | "
            f"{before['wall_p95']:.2f} → {after['wall_p95']:.2f} | "
            f"{before['cpu_median']:.4f} → {after['cpu_median']:.4f} | "
            f"{before['rss_peak']} → {after['rss_peak']} "
            f"({percent(before['rss_peak'], after['rss_peak'])}) | "
            f"{before['processes_peak']} → {after['processes_peak']} | "
            f"{before['threads_peak']} → {after['threads_peak']} | "
            f"{before['mutated_samples']} → {after['mutated_samples']} |"
        )
        if not equal and contracts.get(case, {}).get("require_equal", True):
            regressions.append(f"- `{case}` returned a different result digest.")
        if after["wall_median"] > before["wall_median"] * 1.1:
            regressions.append(
                f"- `{case}` median latency increased {percent(before['wall_median'], after['wall_median'])}."
            )
    lines.extend([
        "", "## Automatic checks", "",
        f"- Shared comparable cases: {len(shared)}.",
        f"- Required-equal result-digest mismatches: {len(semantic_mismatches)}.",
        f"- Candidate durable read mutations: {candidate_durable_mutations}.",
        f"- Candidate process failures: {candidate_failures}.",
        f"- Candidate-only cases: {', '.join(f'`{case}`' for case in sorted(set(candidate_rows) - set(baseline_rows))) or 'none'}.",
        f"- Baseline-only cases: {', '.join(f'`{case}`' for case in sorted(set(baseline_rows) - set(candidate_rows))) or 'none'}.",
        "", "## Regressions and limitations", "",
        *(regressions or ["- No automatic digest mismatch or >10% median-latency regression in this sample set."]),
        "- The nine-repetition release table is a regression signal, not proof for unmeasured "
        "hardware, corpora, or workloads; the scale, concurrency, relevance, storage, and lifecycle "
        "gates below bound the claims made here.",
        "- Peak values are maxima of sampled process-tree observations; wall and CPU figures include "
        "the named adapter/client process where applicable.",
    ])
    lines.extend(
        scaling_lines(scale_paths, candidate_scale_paths, baseline_build, candidate_build)
    )
    if relevance_path is not None:
        lines.extend(relevance_lines(relevance_path))
    lines.extend([
        "", "## Reproduction and primary references", "",
        "Run the cost-tiered harness against disposable generated fixtures; it refuses the configured "
        "live database. Replace the artifact paths with a new empty directory:",
        "",
        "```sh",
        "uv run python scripts/benchmark_release.py --tier smoke --fixture generated --artifact-dir /private/tmp/aise-benchmark-smoke --dry-run",
        "uv run python scripts/benchmark_release.py --tier smoke --fixture generated --artifact-dir /private/tmp/aise-benchmark-smoke",
        "uv run python scripts/benchmark_release.py --tier release --fixture generated --artifact-dir /private/tmp/aise-benchmark-release",
        "uv run python scripts/render_benchmark_report.py --baseline /private/tmp/aise-benchmark-release/samples.jsonl --candidate /private/tmp/aise-benchmark-release/samples.jsonl --output notes/aise-report.md",
        "uv run python scripts/render_benchmark_report.py --baseline /private/tmp/aise-benchmark-release/samples.jsonl --candidate /private/tmp/aise-benchmark-release/samples.jsonl --output notes/aise-report.md --check",
        "```",
        "",
        "This report's exact renderer command is:",
        "",
        "```sh",
        exact_renderer_command,
        "```",
        "",
        "- [SQLite WAL concurrency](https://www.sqlite.org/wal.html)",
        "- [SQLite transaction semantics](https://www.sqlite.org/lang_transaction.html)",
        "- [SQLite FTS5 and trigram tokenizer](https://www.sqlite.org/fts5.html)",
        "- [SQLite query planner](https://www.sqlite.org/queryplanner.html)",
        "- [SQLite EXPLAIN QUERY PLAN](https://www.sqlite.org/eqp.html)",
        "- [SQLite application-defined functions](https://www.sqlite.org/appfunc.html)",
        "- [MCP cancellation](https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation)",
        "- [MCP lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)",
    ])
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--baseline-build", default="baseline")
    parser.add_argument("--candidate-build", default="candidate")
    parser.add_argument("--output", required=True)
    parser.add_argument(
        "--scale", action="append", default=[], metavar="LABEL:JSONL",
        help="Add a paired scale JSONL, for example 1x:/tmp/scale1/samples.jsonl",
    )
    parser.add_argument(
        "--candidate-scale", action="append", default=[], metavar="LABEL:JSONL",
        help="Replace one candidate scale with a candidate-only exact-tree rerun.",
    )
    parser.add_argument(
        "--overlay", action="append", default=[],
        help="Paired JSONL whose cases replace matching base cases, used for corrected reruns.",
    )
    parser.add_argument(
        "--baseline-overlay", action="append", default=[],
        help="JSONL whose baseline cases replace matching base cases.",
    )
    parser.add_argument(
        "--candidate-overlay", action="append", default=[],
        help="JSONL whose candidate cases replace matching base cases.",
    )
    parser.add_argument(
        "--relevance-log",
        help="Cargo test log containing one structured AISE fuzzy-relevance result.",
    )
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    scale_paths = []
    for value in args.scale:
        label, separator, path = value.partition(":")
        if not separator:
            raise SystemExit(f"invalid --scale {value!r}; expected LABEL:JSONL")
        scale_paths.append((label, Path(path)))
    candidate_scale_paths = {}
    for value in args.candidate_scale:
        label, separator, path = value.partition(":")
        if not separator:
            raise SystemExit(f"invalid --candidate-scale {value!r}; expected LABEL:JSONL")
        candidate_scale_paths[label] = Path(path)
    content = render(
        Path(args.baseline), Path(args.candidate), args.baseline_build, args.candidate_build,
        scale_paths, candidate_scale_paths, [Path(path) for path in args.overlay],
        [Path(path) for path in args.baseline_overlay],
        [Path(path) for path in args.candidate_overlay],
        Path(args.relevance_log) if args.relevance_log else None,
    )
    output = Path(args.output)
    if args.check:
        if not output.exists() or output.read_text() != content:
            raise SystemExit(f"generated report differs: {output}")
        print(f"report cross-check passed: {output}")
    else:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(content)
        print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
