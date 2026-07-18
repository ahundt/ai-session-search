#!/usr/bin/env python3
"""Build an immutable tagged baseline in a detached temporary worktree."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TAG = "checkpoint/pre-sqlite-consolidation-20260716"
DRIVER = ROOT / "rust/ai-session-search-core/examples/benchmark_core.rs"


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(argv: list[str], *, cwd: Path, environment: dict[str, str] | None = None) -> None:
    subprocess.run(argv, cwd=cwd, env=environment, check=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", default=DEFAULT_TAG)
    parser.add_argument("--output-root", required=True)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    output_root = Path(args.output_root).expanduser().resolve()
    if output_root == ROOT or ROOT in output_root.parents:
        raise SystemExit("baseline output must be outside the primary repository")
    worktree = output_root / "worktree"
    target = output_root / "target"
    receipt = output_root / "baseline-build.json"
    if output_root.exists():
        raise SystemExit(f"refusing existing baseline output root: {output_root}")
    commands = [
        ["git", "worktree", "add", "--detach", str(worktree), args.tag],
        ["cargo", "build", "--release", "--locked", "-p", "ai-session-search", "--bin",
         "aise", "--example", "benchmark_core"],
        ["uv", "sync", "--project", str(worktree), "--python", str(ROOT / ".venv/bin/python3"),
         "--extra", "dev", "--frozen"],
        ["uv", "run", "--project", str(worktree), "maturin", "develop", "--release", "--uv"],
    ]
    if args.dry_run:
        print(json.dumps({"worktree": str(worktree), "target": str(target), "commands": commands},
                         indent=2))
        return 0
    output_root.mkdir(parents=True)
    run(commands[0], cwd=ROOT)
    example = worktree / "rust/ai-session-search-core/examples/benchmark_core.rs"
    example.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(DRIVER, example)
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(target)
    environment["RUSTC_WRAPPER"] = ""
    run(commands[1], cwd=worktree, environment=environment)
    run(commands[2], cwd=worktree, environment=environment)
    run(commands[3], cwd=worktree, environment=environment)
    binary = target / "release/aise"
    core = target / "release/examples/benchmark_core"
    python = worktree / ".venv/bin/python3"
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=worktree, text=True,
        stdout=subprocess.PIPE, check=True,
    ).stdout.strip()
    data = {
        "tag": args.tag, "commit": commit, "worktree": str(worktree), "target": str(target),
        "binary": str(binary), "binary_sha256": sha256(binary),
        "core": str(core), "core_sha256": sha256(core),
        "python": str(python), "driver_sha256": sha256(DRIVER),
        "commands": commands,
    }
    receipt.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")
    print(receipt)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
