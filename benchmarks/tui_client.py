#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Exercise TUI startup and documented quit through macOS script(1)'s controlling terminal."""

from __future__ import annotations

import argparse
import os
import subprocess
import time


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--startup-wait", type=float, default=1.0)
    args = parser.parse_args()
    if not os.path.exists("/usr/bin/script"):
        raise SystemExit("TUI benchmark currently requires macOS /usr/bin/script")
    child = subprocess.Popen(
        [
            "/usr/bin/script", "-q", "/dev/null", "/bin/sh", "-c",
            'stty rows 24 cols 100; exec "$@"', "aise-tui-benchmark", args.binary,
            "--database", args.fixture, "--index-refresh", "existing-only", "tui",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={**os.environ, "TERM": os.environ.get("TERM", "xterm-256color")},
    )
    assert child.stdin is not None
    time.sleep(args.startup_wait)
    child.stdin.write(b"q")
    child.stdin.close()
    try:
        child.wait(timeout=args.timeout)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait()
        raise SystemExit("TUI did not exit after documented q key") from None
    stdout = child.stdout.read() if child.stdout is not None else b""
    stderr = child.stderr.read() if child.stderr is not None else b""
    if child.returncode != 0 or b"Sessions" not in stdout or b"Preview" not in stdout:
        raise SystemExit(
            f"TUI startup failed with exit {child.returncode}: {stderr.decode(errors='replace')}"
        )
    print('{"preview":true,"sessions":true,"terminal_restored":true}')
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
