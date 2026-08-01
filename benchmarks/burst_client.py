#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Launch simultaneous short-lived CLI readers and verify byte-identical results."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import tempfile
import time
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--clients", required=True, type=int, choices=(1, 2, 4, 8))
    args = parser.parse_args()
    argv = [
        args.binary, "--database", args.fixture, "--index-refresh", "existing-only",
        "messages", "search", "databse", "--query-mode", "fuzzy", "--limit", "10", "--format", "json",
    ]
    with tempfile.TemporaryDirectory(prefix="aise-burst-") as temporary:
        root = Path(temporary)
        gate = root / "start"
        children = []
        for index in range(args.clients):
            ready = root / f"ready-{index}"
            children.append(subprocess.Popen(
                [
                    "/bin/sh", "-c",
                    'touch "$1"; while [ ! -e "$2" ]; do sleep 0.01; done; shift 2; exec "$@"',
                    "aise-burst-gate", str(ready), str(gate), *argv,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            ))
        deadline = time.monotonic() + 5
        while sum(path.name.startswith("ready-") for path in root.iterdir()) < args.clients:
            if time.monotonic() >= deadline:
                raise SystemExit("readers did not reach the start barrier")
            time.sleep(0.005)
        gate.write_bytes(b"")
        outputs = [child.communicate() for child in children]
    for child, (_, stderr) in zip(children, outputs, strict=True):
        if child.returncode != 0:
            raise SystemExit(f"reader exited {child.returncode}: {stderr.decode(errors='replace')}")
    digests = {hashlib.sha256(stdout).hexdigest() for stdout, _ in outputs}
    if len(digests) != 1:
        raise SystemExit("concurrent readers returned different result digests")
    print(json.dumps({"clients": args.clients, "result_sha256": next(iter(digests))},
                     sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
