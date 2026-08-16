#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Cost-tiered, reproducible AI Session Search process benchmark orchestrator."""

from __future__ import annotations

import argparse
import ctypes
import datetime
import hashlib
import json
import os
import platform
import shutil
import sqlite3
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, ClassVar, TypedDict

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "benchmarks" / "release_manifest.json"
TIER_ORDER = {"smoke": 0, "subsystem": 1, "release": 2}


class Build(TypedDict):
    label: str
    binary: Path
    python: Path
    core: Path
    repository: Path


class MacProcessTaskInfo(ctypes.Structure):
    _fields_: ClassVar[list[tuple[str, Any]]] = [
        ("virtual_size", ctypes.c_uint64),
        ("resident_size", ctypes.c_uint64),
        ("total_user", ctypes.c_uint64),
        ("total_system", ctypes.c_uint64),
        ("threads_user", ctypes.c_uint64),
        ("threads_system", ctypes.c_uint64),
        ("policy", ctypes.c_int32),
        ("faults", ctypes.c_int32),
        ("pageins", ctypes.c_int32),
        ("cow_faults", ctypes.c_int32),
        ("messages_sent", ctypes.c_int32),
        ("messages_received", ctypes.c_int32),
        ("syscalls_mach", ctypes.c_int32),
        ("syscalls_unix", ctypes.c_int32),
        ("context_switches", ctypes.c_int32),
        ("thread_count", ctypes.c_int32),
        ("running_threads", ctypes.c_int32),
        ("priority", ctypes.c_int32),
    ]


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def clone_fixture(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    if sys.platform == "darwin":
        subprocess.run(["cp", "-c", str(source), str(destination)], check=True)
    else:
        shutil.copy2(source, destination)


def sqlite_file_state(database: Path) -> dict[str, dict[str, Any]]:
    state: dict[str, dict[str, Any]] = {}
    for suffix in ("", "-wal", "-shm", ".update.lock"):
        path = Path(f"{database}{suffix}")
        if path.exists():
            state[suffix or "database"] = {
                "bytes": path.stat().st_size,
                "sha256": sha256(path),
            }
    return state


def durable_sqlite_state(state: dict[str, dict[str, Any]]) -> dict[str, dict[str, Any]]:
    durable = {"database": state["database"]}
    wal = state.get("-wal")
    if wal is not None and wal["bytes"] > 0:
        durable["-wal"] = wal
    return durable


def absolute_path_preserving_symlink(value: str) -> Path:
    return Path(os.path.abspath(os.path.expanduser(value)))


def configured_live_database(binary: Path) -> Path | None:
    value = os.environ.get("AI_SESSION_SEARCH_DATABASE")
    if value:
        return Path(value).expanduser().resolve()
    result = subprocess.run(
        [str(binary), "config", "show", "--format", "json"],
        text=True,
        capture_output=True,
        check=True,
    )
    value = json.loads(result.stdout).get("index", {}).get("db_path")
    return Path(value).expanduser().resolve() if value else None


def validate_fixture(path: Path, artifact_dir: Path, required_version: int | None, binary: Path) -> dict[str, Any]:
    resolved = path.expanduser().resolve()
    live = configured_live_database(binary)
    if live is not None and resolved == live:
        raise SystemExit(f"refusing configured live database: {resolved}")
    if not resolved.is_file():
        raise SystemExit(f"fixture is not a regular file: {resolved}")
    artifact_dir.mkdir(parents=True, exist_ok=True)
    copied = artifact_dir / "fixture" / resolved.name
    copied.parent.mkdir(parents=True, exist_ok=True)
    if resolved != copied.resolve():
        shutil.copy2(resolved, copied)
    connection = sqlite3.connect(f"file:{copied}?mode=ro", uri=True)
    try:
        version = connection.execute("pragma user_version").fetchone()[0]
        quick_check = connection.execute("pragma quick_check").fetchone()[0]
        counts = {table: connection.execute(f"select count(*) from {table}").fetchone()[0] for table in ("sessions", "messages", "file_edits")}
    finally:
        connection.close()
    if (required_version is not None and version != required_version) or quick_check != "ok":
        raise SystemExit(f"invalid fixture: schema={version}, quick_check={quick_check!r}")
    return {"path": str(copied), "sha256": sha256(copied), "bytes": copied.stat().st_size, "schema_version": version, "counts": counts}


def public_fixture_metadata(fixture: dict[str, Any]) -> dict[str, Any]:
    """Return reproducibility facts without publishing the machine-local database path."""
    return {key: value for key, value in fixture.items() if key != "path"}


def artifact_privacy(fixture: str) -> dict[str, Any]:
    """Classify whether benchmark evidence is portable and publishable."""
    generated = fixture == "generated"
    return {
        "classification": ("portable_generated" if generated else "private_local_fixture"),
        "publishable": generated,
    }


def generated_fixture_config(*, include_prime_agent: bool = True) -> str:
    """Return a portable config whose app-owned paths stay relative to the fixture directory."""
    provider_names = (
        "claude",
        "claude-desktop",
        "codex",
        "cursor",
        "antigravity",
        "pi",
        "prime-agent",
        "aistudio",
        "gemini-cli",
    )
    if not include_prime_agent:
        provider_names = tuple(name for name in provider_names if name != "prime-agent")
    disabled = "\n".join(f"[providers.{name}]\nenabled = false\npaths = []" for name in provider_names)
    return f'[index]\ndb_path = "generated.db"\ncache_dir = "cache"\n{disabled}\n'


def generate_fixture(
    artifact_dir: Path,
    binary: Path,
    seed: int,
    sessions: int,
    messages_per_session: int,
    *,
    include_prime_agent: bool = True,
) -> Path:
    """Generate deterministic point and multi-day-span sessions in O(sessions * messages)."""
    fixture_dir = artifact_dir / "fixture"
    fixture_dir.mkdir(parents=True, exist_ok=True)
    database = fixture_dir / "generated.db"
    config = fixture_dir / "config.toml"
    config.write_text(generated_fixture_config(include_prime_agent=include_prime_agent))
    # HOME points at the fixture so every provider's default root resolves inside it and finds
    # nothing. The config disables providers by name, which only covers the names it lists, and
    # `include_prime_agent` deliberately omits one for baselines that predate the key — that
    # omission left `~/.prime/agent/sessions` live and indexed 119 real transcripts into a
    # fixture the classifier calls publishable, while the candidate indexed none. Isolating the
    # home directory is what makes the fixture hermetic whatever a given binary knows about.
    subprocess.run(
        [str(binary), "--config", str(config), "reindex"],
        cwd=fixture_dir,
        check=True,
        env={**os.environ, "HOME": str(fixture_dir), "USERPROFILE": str(fixture_dir)},
    )
    connection = sqlite3.connect(database)
    try:
        for session_number in range(sessions):
            session_id = f"codex:benchmark-{seed}-{session_number:02d}"
            year = 2026 + session_number // 336
            month = 1 + (session_number // 28) % 12
            day = 1 + session_number % 28
            timestamp = f"{year:04d}-{month:02d}-{day:02d}T12:00:00.000000Z"
            # Even-numbered sessions are point spans; odd-numbered sessions cross 35 days so
            # temporal benchmarks exercise overlap that terminal-timestamp filtering misses.
            if session_number % 2 == 0:
                created_at = timestamp
            else:
                created_at = (
                    (datetime.datetime.fromisoformat(timestamp.replace("Z", "+00:00")) - datetime.timedelta(days=35)).isoformat().replace("+00:00", "Z")
                )
            connection.execute(
                "insert into sessions (id, provider, provider_session_id, title, cwd, repo_root, "
                "created_at, updated_at, last_message_at, preview_text, source_path, message_count, "
                "parse_version, discovery_source) values (?, 'codex', ?, ?, '/benchmark/repo', "
                "'/benchmark/repo', ?, ?, ?, ?, ?, ?, 'benchmark-v1', 'fixture')",
                (
                    session_id,
                    session_id.removeprefix("codex:"),
                    f"SQLite benchmark {session_number}",
                    created_at,
                    timestamp,
                    timestamp,
                    "deterministic database search fixture",
                    f"/benchmark/session-{session_number:02d}.jsonl",
                    messages_per_session,
                ),
            )
            for sequence in range(messages_per_session):
                role = ("user", "assistant", "tool")[sequence % 3]
                kind = "tool_call" if role == "tool" else "conversation"
                tool_name = "exec_command" if role == "tool" else None
                content = f"database sqlite migration lock benchmark session {session_number} sequence {sequence} deterministic payload"
                if role == "tool":
                    content = json.dumps({"tool_name": tool_name, "args": {"cmd": content}})
                connection.execute(
                    "insert into messages (session_id, provider, seq, role, ts, tool_name, kind, content) values (?, 'codex', ?, ?, ?, ?, ?, ?)",
                    (session_id, sequence, role, timestamp, tool_name, kind, content),
                )
            connection.execute(
                "insert into file_edits (session_id, provider, seq, ts, tool, file_path, file_name, new_content) values (?, 'codex', 31, ?, 'Edit', ?, ?, ?)",
                (session_id, timestamp, f"src/file_{session_number:02d}.rs", f"file_{session_number:02d}.rs", f"// deterministic edit {session_number}\n"),
            )
        connection.commit()
        connection.execute("pragma wal_checkpoint(truncate)").fetchone()
    finally:
        connection.close()
    return database


def metadata(binary: Path, manifest: Path, repository: Path) -> dict[str, Any]:
    def git(*args: str) -> str:
        return subprocess.run(["git", *args], cwd=repository, text=True, stdout=subprocess.PIPE, check=True).stdout.strip()

    status = git("status", "--porcelain")
    source_digest = hashlib.sha256()
    source_digest.update(
        subprocess.run(
            ["git", "diff", "--binary", "HEAD"],
            cwd=repository,
            stdout=subprocess.PIPE,
            check=True,
        ).stdout
    )
    for line in status.splitlines():
        if line.startswith("?? "):
            path = repository / line[3:]
            source_digest.update(line[3:].encode())
            if path.is_file():
                source_digest.update(path.read_bytes())
            elif path.is_dir():
                for child in sorted(item for item in path.rglob("*") if item.is_file()):
                    source_digest.update(str(child.relative_to(repository)).encode())
                    source_digest.update(child.read_bytes())
    return {
        "commit": git("rev-parse", "HEAD"),
        "dirty": bool(status),
        "source_state_sha256": source_digest.hexdigest(),
        "manifest_sha256": sha256(manifest),
        "binary_sha256": sha256(binary),
        "python": sys.version.split()[0],
        "sqlite": sqlite3.sqlite_version,
        "os": f"{platform.system()} {platform.release()}",
        "machine": platform.machine(),
    }


def validate_manifest(manifest: dict[str, Any]) -> None:
    cases = manifest.get("cases")
    if not isinstance(cases, list) or not cases:
        raise ValueError("manifest cases must be a non-empty list")
    ids = [case.get("id") for case in cases]
    if len(ids) != len(set(ids)):
        raise ValueError("manifest case IDs must be unique")
    for case in cases:
        if case.get("tier") not in TIER_ORDER or not case.get("argv"):
            raise ValueError(f"invalid tier or argv for case {case.get('id')!r}")
    fields = ("content", "tool-name", "tool-argument")
    modes = ("exact", "regex", "fuzzy")
    available = set(ids)
    for surface, prefix in (("cli", "cli"), ("python", "python"), ("mcp", "mcp"), ("rust", "rust-core")):
        missing = {f"{prefix}-{mode}-{field}" for mode in modes for field in fields} - available
        if missing:
            raise ValueError(f"{surface} 3x3 matrix missing: {sorted(missing)}")


def parse_cpu_seconds(value: str) -> float:
    fields = value.split(":")
    seconds = float(fields.pop())
    multiplier = 60.0
    while fields:
        seconds += float(fields.pop()) * multiplier
        multiplier *= 60.0
    return seconds


def mac_process_tree_resources(root_pid: int) -> tuple[int, int, float, int]:
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    libproc.proc_pidinfo.argtypes = [
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_uint64,
        ctypes.c_void_p,
        ctypes.c_int,
    ]
    libproc.proc_pidinfo.restype = ctypes.c_int
    libproc.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
    libproc.proc_listchildpids.restype = ctypes.c_int
    pending = [root_pid]
    selected: set[int] = set()
    rss_bytes = 0
    threads = 0
    cpu_nanoseconds = 0
    while pending:
        pid = pending.pop()
        if pid in selected:
            continue
        info = MacProcessTaskInfo()
        returned = libproc.proc_pidinfo(pid, 4, 0, ctypes.byref(info), ctypes.sizeof(info))
        if returned != ctypes.sizeof(info):
            continue
        selected.add(pid)
        rss_bytes += info.resident_size
        threads += info.thread_count
        cpu_nanoseconds += info.total_user + info.total_system
        child_buffer = (ctypes.c_int * 1024)()
        child_count = libproc.proc_listchildpids(pid, child_buffer, ctypes.sizeof(child_buffer))
        if child_count > 0:
            pending.extend(child_buffer[: min(child_count, len(child_buffer))])
    return rss_bytes // 1024, threads, cpu_nanoseconds / 1_000_000_000, len(selected)


def ps_process_tree_resources(root_pid: int) -> tuple[int, int, float, int]:
    probe = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss=,time=,nlwp="],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    rows: dict[int, tuple[int, int, float, int]] = {}
    children: dict[int, list[int]] = {}
    for line in probe.stdout.splitlines():
        fields = line.split()
        if len(fields) != 5:
            continue
        pid_text, parent, rss, cpu, threads = fields
        rows[int(pid_text)] = (int(parent), int(rss), parse_cpu_seconds(cpu), int(threads))
        children.setdefault(int(parent), []).append(int(pid_text))
    pending = [root_pid]
    selected: set[int] = set()
    while pending:
        pid = pending.pop()
        if pid in selected:
            continue
        selected.add(pid)
        pending.extend(children.get(pid, ()))
    present = [rows[pid] for pid in selected if pid in rows]
    return (
        sum(row[1] for row in present),
        sum(row[3] for row in present),
        sum(row[2] for row in present),
        len(present),
    )


def process_tree_resources(root_pid: int) -> tuple[int, int, float, int]:
    if sys.platform == "darwin":
        return mac_process_tree_resources(root_pid)
    return ps_process_tree_resources(root_pid)


def sample_process(
    argv: list[str],
    normalizations: dict[bytes, bytes] | None = None,
    *,
    extract_session_ids: bool = False,
) -> dict[str, Any]:
    started = time.perf_counter_ns()
    child = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    peak_rss_kib = 0
    peak_threads = 0
    peak_processes = 0
    cpu_seconds = 0.0
    while child.poll() is None:
        rss_kib, threads, cpu, processes = process_tree_resources(child.pid)
        peak_rss_kib = max(peak_rss_kib, rss_kib)
        peak_threads = max(peak_threads, threads)
        peak_processes = max(peak_processes, processes)
        cpu_seconds = max(cpu_seconds, cpu)
        # libproc is an in-process syscall wrapper on macOS; sample continuously so sub-10 ms CLI
        # calls cannot finish between a fixed polling interval and silently under-report peak RSS.
        if sys.platform != "darwin":
            time.sleep(0.001)
    stdout, stderr = child.communicate()
    normalized_stdout = stdout
    normalized_stderr = stderr
    for source, replacement in sorted((normalizations or {}).items(), key=lambda item: len(item[0]), reverse=True):
        normalized_stdout = normalized_stdout.replace(source, replacement)
        normalized_stderr = normalized_stderr.replace(source, replacement)
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    session_ids = None
    if extract_session_ids and child.returncode == 0:
        decoded = json.loads(normalized_stdout)
        if not isinstance(decoded, list):
            raise ValueError("temporal benchmark output must be a JSON array")
        session_ids = sorted(item if isinstance(item, str) else str(item["id"]) for item in decoded)
    return {
        "exit_code": child.returncode,
        "wall_ms": elapsed_ms,
        "peak_rss_kib": peak_rss_kib,
        "peak_threads": peak_threads,
        "peak_processes": peak_processes,
        "cpu_seconds": cpu_seconds,
        "stdout_bytes": len(stdout),
        "stderr_bytes": len(stderr),
        "result_sha256": hashlib.sha256(normalized_stdout).hexdigest(),
        "normalized_stdout_bytes": len(normalized_stdout),
        "stderr": normalized_stderr.decode("utf-8", "replace")[-4000:],
        **({"session_ids": session_ids} if session_ids is not None else {}),
    }


def temporal_overlap_oracle(database: Path, case: dict[str, Any]) -> dict[str, Any] | None:
    """Return canonical overlap IDs/digest for a declared closed temporal case in O(S)."""
    if case.get("expected_relation") != "intentional_change_with_oracle":
        return None
    start = case.get("oracle_start")
    end = case.get("oracle_end")
    if not isinstance(start, str) or not isinstance(end, str):
        raise ValueError(f"temporal case {case.get('id')!r} requires oracle_start/oracle_end")
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True)
    try:
        rows = connection.execute(
            "select id from sessions where coalesce(updated_at, created_at) >= ? and coalesce(created_at, updated_at) <= ? order by id",
            (start, end),
        ).fetchall()
    finally:
        connection.close()
    ids = [str(row[0]) for row in rows]
    encoded = json.dumps(ids, separators=(",", ":")).encode()
    return {"eligible_ids": ids, "eligible_ids_sha256": hashlib.sha256(encoded).hexdigest()}


def case_measurement_metadata(case: dict[str, Any]) -> dict[str, Any]:
    """Return declared work units and workload dimensions needed to interpret a sample."""
    metadata = {"operations": int(case.get("operations", 1))}
    for key in (
        "reader_bound",
        "workload",
        "temporal_mode",
        "expected_relation",
        "oracle_start",
        "oracle_end",
    ):
        if key in case:
            metadata[key] = case[key]
    return metadata


def validate_fixture_policy(
    tier: str,
    fixture: str,
    allow_private_fixture: bool,
) -> None:
    """Separate portable release evidence from explicitly private local profiling."""
    if fixture == "generated":
        return
    if tier == "release":
        raise SystemExit("release benchmarks require --fixture generated; local databases may contain private session data")
    if not allow_private_fixture:
        raise SystemExit("local benchmark fixtures require --allow-private-fixture; resulting artifacts are private and not release evidence")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tier", choices=TIER_ORDER, default="smoke")
    parser.add_argument("--case", action="append", dest="cases")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--baseline")
    parser.add_argument("--baseline-repository")
    parser.add_argument("--candidate", default=str(ROOT / "target/release/aise"))
    parser.add_argument("--baseline-python")
    parser.add_argument(
        "--candidate-python",
        default=str(ROOT / ".venv/bin/python3") if (ROOT / ".venv/bin/python3").exists() else sys.executable,
    )
    parser.add_argument("--baseline-core")
    parser.add_argument("--candidate-core", default=str(ROOT / "target/release/examples/benchmark_core"))
    parser.add_argument(
        "--fixture",
        required=True,
        help="Use 'generated' for portable evidence, or a disposable DB with --allow-private-fixture",
    )
    parser.add_argument(
        "--allow-private-fixture",
        action="store_true",
        help="Permit a local DB for smoke/subsystem profiling; artifacts are private and never release evidence",
    )
    parser.add_argument("--fixture-sessions", type=int, default=16)
    parser.add_argument("--fixture-messages", type=int, default=32)
    parser.add_argument("--fixture-scale", type=int, choices=(1, 2, 4), default=1)
    parser.add_argument("--artifact-dir", required=True)
    parser.add_argument("--manifest", default=str(DEFAULT_MANIFEST))
    return parser.parse_args()


def main() -> int:  # noqa: C901 - orchestration branches mirror fail-fast benchmark gates.
    args = parse_args()
    manifest_path = Path(args.manifest).resolve()
    manifest = json.loads(manifest_path.read_text())
    validate_manifest(manifest)
    validate_fixture_policy(args.tier, args.fixture, args.allow_private_fixture)
    selected = [case for case in manifest["cases"] if TIER_ORDER[case["tier"]] <= TIER_ORDER[args.tier] and (not args.cases or case["id"] in args.cases)]
    repetitions = manifest["tiers"][args.tier]
    print(json.dumps({"cases": len(selected), "repetitions": repetitions, "samples": len(selected) * repetitions, "dry_run": args.dry_run}))
    if args.dry_run:
        return 0
    artifact_dir = Path(args.artifact_dir).expanduser().resolve()
    first_binary = Path(args.baseline or args.candidate).resolve()
    if not first_binary.is_file():
        raise SystemExit(f"missing benchmark binary: {first_binary}")
    results_path = artifact_dir / "samples.jsonl"
    if results_path.exists():
        raise SystemExit(f"refusing to append to existing result set: {results_path}")
    builds: list[Build] = [
        {
            "label": "candidate",
            "binary": Path(args.candidate).resolve(),
            "python": absolute_path_preserving_symlink(args.candidate_python),
            "core": Path(args.candidate_core).resolve(),
            "repository": ROOT,
        }
    ]
    if args.baseline:
        builds.insert(
            0,
            {
                "label": "baseline",
                "binary": Path(args.baseline).resolve(),
                "python": absolute_path_preserving_symlink(args.baseline_python or args.candidate_python),
                "core": Path(args.baseline_core or args.candidate_core).resolve(),
                "repository": Path(args.baseline_repository).resolve() if args.baseline_repository else ROOT,
            },
        )
    fixtures: dict[str, dict[str, Any]] = {}
    if args.fixture == "generated":
        for build in builds:
            fixture_root = artifact_dir / "generated" / str(build["label"])
            source = generate_fixture(
                fixture_root,
                build["binary"],
                manifest["seed"],
                args.fixture_sessions * args.fixture_scale,
                args.fixture_messages,
                include_prime_agent=(build["label"] == "candidate"),
            )
            required_version = manifest["fixture"]["required_schema_version"] if build["label"] == "candidate" else None
            fixtures[str(build["label"])] = validate_fixture(source, fixture_root, required_version, build["binary"])
    else:
        fixture = validate_fixture(
            Path(args.fixture),
            artifact_dir,
            manifest["fixture"]["required_schema_version"],
            first_binary,
        )
        fixtures = {str(build["label"]): fixture for build in builds}
    expected_by_case: dict[str, str] = {}
    for build in builds:
        label = build["label"]
        binary = build["binary"]
        fixture = fixtures[str(label)]
        if not binary.is_file():
            raise SystemExit(f"missing {label} binary: {binary}")
        run_metadata = metadata(binary, manifest_path, build["repository"])
        with results_path.open("a", encoding="utf-8") as output:
            contracts = {case["id"]: {"require_equal": case.get("require_equal", True)} for case in manifest["cases"]}
            output.write(
                json.dumps(
                    {
                        "kind": "run",
                        "build": label,
                        "metadata": run_metadata,
                        "fixture": public_fixture_metadata(fixture),
                        "contracts": contracts,
                        "artifact_privacy": artifact_privacy(args.fixture),
                    },
                    sort_keys=True,
                )
                + "\n"
            )
            for case in selected:
                expected_digest = None
                for repetition in range(repetitions):
                    sample_fixture = artifact_dir / "sample-fixtures" / str(label) / case["id"] / f"{repetition}.db"
                    clone_fixture(Path(fixture["path"]), sample_fixture)
                    before_fixture_state = sqlite_file_state(sample_fixture)
                    argv = [
                        part.format(
                            binary=binary,
                            python=build["python"],
                            core=build["core"],
                            client_root=ROOT / "benchmarks",
                            fixture=sample_fixture,
                        )
                        for part in case["argv"]
                    ]
                    if argv[0] == str(build["python"]):
                        argv.insert(1, "-I")
                    path_normalizations = {
                        str(sample_fixture).encode(): b"{fixture}",
                        str(artifact_dir).encode(): b"{artifact_dir}",
                        str(build["python"]).encode(): b"{python}",
                        str(build["core"]).encode(): b"{core}",
                        str(binary).encode(): b"{binary}",
                        str(ROOT / "benchmarks").encode(): b"{client_root}",
                        str(ROOT).encode(): b"{repository}",
                        str(Path.home()).encode(): b"{home}",
                    }
                    sample = sample_process(
                        argv,
                        path_normalizations,
                        extract_session_ids=(case.get("expected_relation") == "intentional_change_with_oracle"),
                    )
                    after_fixture_state = sqlite_file_state(sample_fixture)
                    durable_before = durable_sqlite_state(before_fixture_state)
                    durable_after = durable_sqlite_state(after_fixture_state)
                    sample.update(case_measurement_metadata(case))
                    oracle = temporal_overlap_oracle(sample_fixture, case)
                    if oracle is not None:
                        sample["temporal_oracle"] = oracle
                        sample["temporal_oracle_match"] = sample.get("session_ids") == oracle["eligible_ids"]
                        if label == "candidate" and not sample["temporal_oracle_match"]:
                            raise SystemExit(
                                f"{label}/{case['id']} differs from temporal overlap oracle: "
                                f"observed={sample.get('session_ids')!r}, "
                                f"expected={oracle['eligible_ids']!r}"
                            )
                    sample.update(
                        kind="sample",
                        build=label,
                        case=case["id"],
                        surface=case["surface"],
                        repetition=repetition,
                        fixture_state_before=before_fixture_state,
                        fixture_state_after=after_fixture_state,
                        fixture_files_changed=before_fixture_state != after_fixture_state,
                        durable_fixture_mutated=durable_before != durable_after,
                    )
                    if sample["exit_code"] != 0:
                        output.write(json.dumps(sample, sort_keys=True) + "\n")
                        output.flush()
                        if label == "baseline" and case.get("baseline_allow_failure", False):
                            continue
                        raise SystemExit(f"{label}/{case['id']} failed: {sample['stderr']}")
                    expected_digest = expected_digest or sample["result_sha256"]
                    if sample["result_sha256"] != expected_digest:
                        raise SystemExit(f"non-deterministic result digest: {label}/{case['id']}")
                    prior_build_digest = expected_by_case.setdefault(case["id"], expected_digest)
                    if case.get("require_equal", True) and sample["result_sha256"] != prior_build_digest:
                        raise SystemExit(f"baseline/candidate result mismatch: {case['id']}")
                    if label == "candidate" and case.get("read_only", True) and sample["durable_fixture_mutated"]:
                        raise SystemExit(f"candidate durably mutated read-only fixture: {case['id']}")
                    output.write(json.dumps(sample, sort_keys=True) + "\n")
                    output.flush()
        if sha256(Path(fixture["path"])) != fixture["sha256"]:
            raise SystemExit(f"source fixture mutated during {label} benchmark")
    print(results_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
