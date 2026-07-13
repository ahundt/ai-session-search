#!/usr/bin/env python3
"""Smoke-test an installed wheel or sdist without importing the source checkout."""

from __future__ import annotations

import argparse
import importlib.metadata
import os
import pathlib
import shutil
import subprocess
import tempfile

DEFAULT_COMMAND_TIMEOUT_SECONDS = 30.0


class InstallVerificationError(RuntimeError):
    """The installed distribution does not satisfy its runtime contract."""


def _is_within(path: pathlib.Path, root: pathlib.Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


def verify(
    source_root: pathlib.Path,
    executable_name: str = "aise",
    command_timeout_seconds: float = DEFAULT_COMMAND_TIMEOUT_SECONDS,
) -> None:
    import ai_session_search
    from ai_session_search import SessionQuery, SessionSearch

    if command_timeout_seconds <= 0:
        raise InstallVerificationError("command timeout must be greater than zero")

    package_path = pathlib.Path(ai_session_search.__file__).resolve()
    source_root = source_root.resolve()
    if _is_within(package_path, source_root):
        raise InstallVerificationError(f"import resolved to source checkout instead of installed artifact: {package_path}")

    distribution = importlib.metadata.distribution("ai-session-search")
    if distribution.version != ai_session_search.__version__:
        raise InstallVerificationError(f"metadata version {distribution.version} != package version {ai_session_search.__version__}")
    entry_points = {entry.name for entry in distribution.entry_points if entry.group == "console_scripts"}
    if executable_name not in entry_points:
        raise InstallVerificationError(f"missing console entry point: {executable_name}")

    executable = shutil.which(executable_name)
    if executable is None:
        raise InstallVerificationError(f"console executable is not on PATH: {executable_name}")
    executable_path = pathlib.Path(executable).resolve()
    if _is_within(executable_path, source_root):
        raise InstallVerificationError(f"console executable resolved to source checkout: {executable_path}")

    with tempfile.TemporaryDirectory(prefix="aise-install-smoke-") as temporary:
        root = pathlib.Path(temporary)
        os.environ["AI_SESSION_SEARCH_CONFIG"] = str(root / "config" / "config.toml")
        os.environ["AI_SESSION_SEARCH_CACHE_DIR"] = str(root / "cache")
        search = SessionSearch(root / "index.db")
        sessions = search.list_sessions(SessionQuery(limit=1))
        if sessions:
            raise InstallVerificationError("temporary native index was not empty")

        environment = os.environ.copy()
        try:
            completed = subprocess.run(
                [executable, "--help"],
                cwd=root,
                env=environment,
                capture_output=True,
                text=True,
                timeout=command_timeout_seconds,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise InstallVerificationError(f"{executable_name} --help exceeded {command_timeout_seconds:g} seconds") from error
        if completed.returncode != 0:
            detail = (completed.stderr or completed.stdout).strip()
            raise InstallVerificationError(f"{executable_name} --help exited {completed.returncode}: {detail}")

    print(f"installed distribution verified: version={distribution.version} package={package_path} executable={executable_path}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", required=True, type=pathlib.Path)
    parser.add_argument("--executable", default="aise")
    parser.add_argument(
        "--command-timeout-seconds",
        default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
        type=float,
    )
    args = parser.parse_args()
    try:
        verify(args.source_root, args.executable, args.command_timeout_seconds)
    except (InstallVerificationError, OSError) as error:
        print(f"installed distribution verification failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
