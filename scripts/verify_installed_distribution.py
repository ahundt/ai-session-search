#!/usr/bin/env python3
"""Smoke-test an installed wheel or sdist without importing the source checkout."""

from __future__ import annotations

import argparse
import importlib.metadata
import json
import os
import pathlib
import shutil
import subprocess
import tempfile

DEFAULT_COMMAND_TIMEOUT_SECONDS = 30.0
EXPECTED_MCP_COMMANDS = {"serve", "install", "status", "uninstall"}


class InstallVerificationError(RuntimeError):
    """The installed distribution does not satisfy its runtime contract."""


def _is_within(path: pathlib.Path, root: pathlib.Path) -> bool:
    try:
        path.relative_to(root)
    except ValueError:
        return False
    return True


def _run_command(
    executable: str,
    executable_name: str,
    args: tuple[str, ...],
    root: pathlib.Path,
    environment: dict[str, str],
    timeout_seconds: float,
    input_text: str | None = None,
) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            [executable, *args],
            cwd=root,
            env=environment,
            input=input_text,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        rendered = " ".join((executable_name, *args))
        raise InstallVerificationError(
            f"{rendered} exceeded {timeout_seconds:g} seconds"
        ) from error


def _require_success(
    executable_name: str,
    args: tuple[str, ...],
    completed: subprocess.CompletedProcess[str],
) -> None:
    if completed.returncode == 0:
        return
    detail = (completed.stderr or completed.stdout).strip()
    rendered = " ".join((executable_name, *args))
    raise InstallVerificationError(f"{rendered} exited {completed.returncode}: {detail}")


def _verify_cli_contract(
    executable: str,
    executable_name: str,
    root: pathlib.Path,
    environment: dict[str, str],
    timeout_seconds: float,
) -> None:
    help_args = ("--help",)
    _require_success(
        executable_name,
        help_args,
        _run_command(
            executable, executable_name, help_args, root, environment, timeout_seconds
        ),
    )

    mcp_help_args = ("mcp", "--help")
    mcp_help = _run_command(
        executable, executable_name, mcp_help_args, root, environment, timeout_seconds
    )
    _require_success(executable_name, mcp_help_args, mcp_help)
    missing = sorted(command for command in EXPECTED_MCP_COMMANDS if command not in mcp_help.stdout)
    if missing:
        raise InstallVerificationError(
            f"{executable_name} mcp --help omitted commands: {', '.join(missing)}"
        )

    serve_args = ("mcp", "serve")
    initialize = _run_command(
        executable,
        executable_name,
        serve_args,
        root,
        environment,
        timeout_seconds,
        input_text='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n',
    )
    _require_success(executable_name, serve_args, initialize)
    try:
        response = json.loads(initialize.stdout)
    except json.JSONDecodeError as error:
        raise InstallVerificationError(
            f"{executable_name} mcp serve returned invalid JSON: {initialize.stdout!r}"
        ) from error
    if response.get("id") != 1 or response.get("result", {}).get("capabilities", {}).get("tools") != {}:
        raise InstallVerificationError(
            f"{executable_name} mcp serve returned an invalid initialize response: {response!r}"
        )


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
        _verify_cli_contract(
            executable,
            executable_name,
            root,
            environment,
            command_timeout_seconds,
        )

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
