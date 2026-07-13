#!/usr/bin/env python3
"""Smoke-test an unpacked native aise executable with isolated state."""

from __future__ import annotations

import argparse
import os
import pathlib
import sys
import tempfile

if not __package__:
    sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

from scripts import verify_installed_distribution as installed_verifier


def verify(executable: pathlib.Path, timeout_seconds: float) -> None:
    """Verify CLI and MCP startup without reading the user's configuration or index."""
    if timeout_seconds <= 0:
        raise installed_verifier.InstallVerificationError("command timeout must be greater than zero")
    executable = executable.resolve(strict=True)
    if not executable.is_file():
        raise installed_verifier.InstallVerificationError(f"native executable is not a file: {executable}")
    with tempfile.TemporaryDirectory(prefix="aise-native-smoke-") as temporary:
        root = pathlib.Path(temporary)
        config_path = root / "config" / "config.toml"
        config_path.parent.mkdir(parents=True)
        config_path.write_text("", encoding="utf-8")
        environment = os.environ.copy()
        environment["AI_SESSION_SEARCH_CONFIG"] = str(config_path)
        environment["AI_SESSION_SEARCH_CACHE_DIR"] = str(root / "cache")
        installed_verifier.verify_cli_contract(
            str(executable),
            executable.name,
            root,
            environment,
            timeout_seconds,
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--executable", required=True, type=pathlib.Path)
    parser.add_argument(
        "--command-timeout-seconds",
        default=installed_verifier.DEFAULT_COMMAND_TIMEOUT_SECONDS,
        type=float,
    )
    args = parser.parse_args()
    try:
        verify(args.executable, args.command_timeout_seconds)
    except (installed_verifier.InstallVerificationError, OSError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
