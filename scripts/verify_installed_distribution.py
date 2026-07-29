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
import sys
import tempfile
import threading
import time
from typing import Any

DEFAULT_COMMAND_TIMEOUT_SECONDS = 30.0
MCP_PROTOCOL_VERSION = "2025-11-25"
EXPECTED_COMMANDS = {
    (): {"config", "integrations", "mcp", "package", "skills"},
    ("config",): {"example", "file", "init", "origins", "paths", "show"},
    ("integrations",): {"install", "recover", "status", "uninstall"},
    # `skills` supplies the correction rules `aise corrections` evaluates, so an installed
    # distribution that cannot list or validate them ships analytics nobody can inspect.
    ("skills",): {"create", "list", "restore", "show", "update", "validate"},
    ("mcp",): {"serve"},
    ("package",): {"check", "status", "update"},
}
EXPECTED_MCP_TOOLS = {
    "search_sessions",
    "get_session",
    "list_sessions",
    "get_resume_command",
    "search_messages",
    "run_skill_capability",
    "get_index_status",
    "query_session_index",
}


class InstallVerificationError(RuntimeError):
    """The installed distribution does not satisfy its runtime contract."""


def _mcp_smoke_messages() -> tuple[dict[str, Any], ...]:
    """Return the smallest conformant initialize-and-list MCP message sequence."""
    return (
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "aise-install-verifier", "version": "1"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
    )


def _read_mcp_message(
    process: subprocess.Popen[str],
    deadline: float,
    executable_name: str,
) -> dict[str, Any]:
    """Read one protocol line without allowing a broken server to exceed the gate timeout."""
    if process.stdout is None:
        raise InstallVerificationError(f"{executable_name} mcp serve has no stdout pipe")
    result: list[str | BaseException] = []

    def read_line() -> None:
        try:
            result.append(process.stdout.readline())
        except BaseException as error:  # pragma: no cover - platform pipe failures are rare
            result.append(error)

    reader = threading.Thread(target=read_line, daemon=True)
    reader.start()
    reader.join(max(0.0, deadline - time.monotonic()))
    if reader.is_alive():
        raise InstallVerificationError(f"{executable_name} mcp serve timed out waiting for a response")
    if not result or result[0] == "":
        raise InstallVerificationError(f"{executable_name} mcp serve closed stdout before responding")
    if isinstance(result[0], BaseException):
        raise InstallVerificationError(
            f"{executable_name} mcp serve failed while reading stdout: {result[0]}"
        )
    try:
        message = json.loads(result[0])
    except json.JSONDecodeError as error:
        raise InstallVerificationError(
            f"{executable_name} mcp serve returned invalid JSON: {result[0]!r}"
        ) from error
    if not isinstance(message, dict):
        raise InstallVerificationError(
            f"{executable_name} mcp serve returned a non-object message: {message!r}"
        )
    return message


def _send_mcp_message(
    process: subprocess.Popen[str],
    message: dict[str, Any],
    executable_name: str,
) -> None:
    if process.stdin is None:
        raise InstallVerificationError(f"{executable_name} mcp serve has no stdin pipe")
    process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
    process.stdin.flush()


def _validate_mcp_initialize(
    response: dict[str, Any],
    executable_name: str,
) -> None:
    if response.get("id") != 1:
        raise InstallVerificationError(
            f"{executable_name} mcp serve returned the wrong initialize response: {response!r}"
        )
    if response.get("result", {}).get("protocolVersion") != MCP_PROTOCOL_VERSION:
        raise InstallVerificationError(
            f"{executable_name} mcp serve negotiated an unexpected protocol version: {response!r}"
        )
    if response.get("result", {}).get("capabilities", {}).get("tools") != {}:
        raise InstallVerificationError(
            f"{executable_name} mcp serve returned an invalid initialize response: {response!r}"
        )


def _wait_for_mcp_exit(
    process: subprocess.Popen[str],
    deadline: float,
    timeout_seconds: float,
    executable_name: str,
) -> None:
    if process.stdin is not None:
        process.stdin.close()
    try:
        return_code = process.wait(timeout=max(0.0, deadline - time.monotonic()))
    except subprocess.TimeoutExpired as error:
        raise InstallVerificationError(
            f"{executable_name} mcp serve exceeded {timeout_seconds:g} seconds"
        ) from error
    if return_code != 0:
        stderr = process.stderr.read().strip() if process.stderr is not None else ""
        raise InstallVerificationError(
            f"{executable_name} mcp serve exited {return_code}: {stderr}"
        )


def _terminate_mcp_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    if process.stdin is not None:
        try:
            process.stdin.close()
        except OSError:
            pass
    process.terminate()
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def _verify_mcp_lifecycle(
    executable: str,
    executable_name: str,
    root: pathlib.Path,
    environment: dict[str, str],
    timeout_seconds: float,
) -> dict[str, Any]:
    """Negotiate MCP before tool discovery, following the required three-way handshake."""
    serve_args = ("mcp", "serve")
    process = subprocess.Popen(
        [executable, *serve_args],
        cwd=root,
        env=environment,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    deadline = time.monotonic() + timeout_seconds
    try:
        initialize_request, initialized_notification, tools_list_request = _mcp_smoke_messages()
        _send_mcp_message(process, initialize_request, executable_name)
        initialize_response = _read_mcp_message(process, deadline, executable_name)
        _validate_mcp_initialize(initialize_response, executable_name)
        _send_mcp_message(process, initialized_notification, executable_name)
        _send_mcp_message(process, tools_list_request, executable_name)
        tools_list_response = _read_mcp_message(process, deadline, executable_name)
        if tools_list_response.get("id") != 2:
            raise InstallVerificationError(
                f"{executable_name} mcp serve returned the wrong tools/list response: "
                f"{tools_list_response!r}"
            )
        _wait_for_mcp_exit(process, deadline, timeout_seconds, executable_name)
        return tools_list_response
    finally:
        _terminate_mcp_process(process)


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


def _run_json_command(
    executable: str,
    executable_name: str,
    args: tuple[str, ...],
    root: pathlib.Path,
    environment: dict[str, str],
    timeout_seconds: float,
) -> object:
    completed = _run_command(
        executable, executable_name, args, root, environment, timeout_seconds
    )
    _require_success(executable_name, args, completed)
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        rendered = " ".join((executable_name, *args))
        raise InstallVerificationError(
            f"{rendered} returned invalid JSON: {completed.stdout!r}"
        ) from error


def _require_mapping(value: object, command: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise InstallVerificationError(
            f"{command} returned {type(value).__name__}; expected a JSON object"
        )
    return value


def verify_configuration_contract(
    executable: str,
    executable_name: str,
    root: pathlib.Path,
    environment: dict[str, str],
    timeout_seconds: float,
) -> None:
    """Verify installed config values and provenance across file, env, and CLI tiers."""
    config_dir = root / "config"
    config_dir.mkdir(parents=True, exist_ok=True)
    config_path = config_dir / "precedence.toml"
    file_database = root / "file-index.db"
    file_cache = root / "file-cache"
    config_path.write_text(
        "\n".join(
            (
                "[index]",
                f"db_path = {json.dumps(str(file_database))}",
                f"cache_dir = {json.dumps(str(file_cache))}",
                'refresh = "existing-only"',
                "[performance]",
                "threads = 3",
                "",
            )
        ),
        encoding="utf-8",
    )

    base_environment = environment.copy()
    for name in (
        "AI_SESSION_SEARCH_DATABASE",
        "AI_SESSION_SEARCH_CACHE_DIR",
        "AI_SESSION_SEARCH_THREADS",
        "AI_SESSION_SEARCH_INDEX_REFRESH",
    ):
        base_environment.pop(name, None)
    base_environment["AI_SESSION_SEARCH_CONFIG"] = str(config_path)

    def assert_tier(
        tier_environment: dict[str, str],
        prefix: tuple[str, ...],
        *,
        expected_origins: dict[str, str],
        database: pathlib.Path,
        cache: pathlib.Path,
        threads: int,
        index_refresh: str,
        paths_prefix: tuple[str, ...] | None = None,
    ) -> None:
        origins_args = (*prefix, "config", "origins")
        origins = _require_mapping(
            _run_json_command(
                executable,
                executable_name,
                origins_args,
                root,
                tier_environment,
                timeout_seconds,
            ),
            " ".join((executable_name, *origins_args)),
        )
        for key, expected in expected_origins.items():
            if origins.get(key) != expected:
                raise InstallVerificationError(
                    f"{executable_name} config origin {key!r} was "
                    f"{origins.get(key)!r}; expected {expected!r}"
                )

        paths_args = (
            *(prefix if paths_prefix is None else paths_prefix),
            "config",
            "paths",
            "--format",
            "json",
        )
        paths = _require_mapping(
            _run_json_command(
                executable,
                executable_name,
                paths_args,
                root,
                tier_environment,
                timeout_seconds,
            ),
            " ".join((executable_name, *paths_args)),
        )
        for key, expected in (("database", database), ("cache", cache)):
            if pathlib.Path(str(paths.get(key))) != expected:
                raise InstallVerificationError(
                    f"{executable_name} effective {key} was {paths.get(key)!r}; "
                    f"expected {str(expected)!r}"
                )

        show_args = (*prefix, "config", "show", "--format", "json")
        shown = _require_mapping(
            _run_json_command(
                executable,
                executable_name,
                show_args,
                root,
                tier_environment,
                timeout_seconds,
            ),
            " ".join((executable_name, *show_args)),
        )
        performance = _require_mapping(
            shown.get("performance"), f"{executable_name} config show performance"
        )
        index = _require_mapping(
            shown.get("index"), f"{executable_name} config show index"
        )
        if performance.get("threads") != threads:
            raise InstallVerificationError(
                f"{executable_name} effective threads was "
                f"{performance.get('threads')!r}; expected {threads}"
            )
        if index.get("refresh") != index_refresh:
            raise InstallVerificationError(
                f"{executable_name} effective index refresh was "
                f"{index.get('refresh')!r}; expected {index_refresh!r}"
            )

    assert_tier(
        base_environment,
        (),
        expected_origins={
            "config": "environment AI_SESSION_SEARCH_CONFIG",
            "database": "config file",
            "cache": "config file",
            "threads": "config file",
            "index_refresh": "config file",
        },
        database=file_database,
        cache=file_cache,
        threads=3,
        index_refresh="existing-only",
    )

    environment_database = root / "environment-index.db"
    environment_cache = root / "environment-cache"
    override_environment = {
        **base_environment,
        "AI_SESSION_SEARCH_DATABASE": str(environment_database),
        "AI_SESSION_SEARCH_CACHE_DIR": str(environment_cache),
        "AI_SESSION_SEARCH_THREADS": "7",
        "AI_SESSION_SEARCH_INDEX_REFRESH": "auto",
    }
    assert_tier(
        override_environment,
        (),
        expected_origins={
            "config": "environment AI_SESSION_SEARCH_CONFIG",
            "database": "environment AI_SESSION_SEARCH_DATABASE",
            "cache": "environment AI_SESSION_SEARCH_CACHE_DIR",
            "threads": "environment AI_SESSION_SEARCH_THREADS",
            "index_refresh": "environment AI_SESSION_SEARCH_INDEX_REFRESH",
        },
        database=environment_database,
        cache=environment_cache,
        threads=7,
        index_refresh="auto",
    )

    cli_database = root / "cli-index.db"
    cli_cache = root / "cli-cache"
    cli_config = config_dir / "cli.toml"
    cli_config.write_text(config_path.read_text(encoding="utf-8"), encoding="utf-8")
    cli_prefix = (
        "--config",
        str(cli_config),
        "--database",
        str(cli_database),
        "--cache-dir",
        str(cli_cache),
        "--threads",
        "11",
        "--index-refresh",
        "before-query",
    )
    cli_paths_prefix = (
        "--config",
        str(cli_config),
        "--database",
        str(cli_database),
        "--cache-dir",
        str(cli_cache),
    )
    assert_tier(
        override_environment,
        cli_prefix,
        expected_origins={
            "config": "cli --config",
            "database": "cli --database",
            "cache": "cli --cache-dir",
            "threads": "cli --threads",
            "index_refresh": "cli --index-refresh",
        },
        database=cli_database,
        cache=cli_cache,
        threads=11,
        index_refresh="before-query",
        paths_prefix=cli_paths_prefix,
    )


def verify_cli_contract(
    executable: str,
    executable_name: str,
    root: pathlib.Path,
    environment: dict[str, str],
    timeout_seconds: float,
) -> None:
    for namespace, expected_commands in EXPECTED_COMMANDS.items():
        help_args = (*namespace, "--help")
        help_result = _run_command(
            executable, executable_name, help_args, root, environment, timeout_seconds
        )
        _require_success(executable_name, help_args, help_result)
        missing = sorted(
            command for command in expected_commands if command not in help_result.stdout
        )
        if missing:
            rendered = " ".join((executable_name, *help_args))
            raise InstallVerificationError(
                f"{rendered} omitted commands: {', '.join(missing)}"
            )

    tools_list_response = _verify_mcp_lifecycle(
        executable,
        executable_name,
        root,
        environment,
        timeout_seconds,
    )
    advertised_tools = {
        tool.get("name")
        for tool in tools_list_response.get("result", {}).get("tools", [])
    }
    if advertised_tools != EXPECTED_MCP_TOOLS:
        raise InstallVerificationError(
            f"{executable_name} mcp serve advertised tools "
            f"{sorted(str(tool) for tool in advertised_tools)!r}; expected "
            f"{sorted(EXPECTED_MCP_TOOLS)!r}"
        )


def verify_source_native_import(
    source_root: pathlib.Path,
    command_timeout_seconds: float = DEFAULT_COMMAND_TIMEOUT_SECONDS,
) -> pathlib.Path:
    """Import the source-tree native module in a bounded child process."""
    if command_timeout_seconds <= 0:
        raise InstallVerificationError("command timeout must be greater than zero")
    source_root = source_root.resolve(strict=True)
    expected_parent = source_root / "ai_session_search"
    code = """
from pathlib import Path
import ai_session_search._native as native
print(Path(native.__file__).resolve())
"""
    completed = _run_command(
        sys.executable,
        pathlib.Path(sys.executable).name,
        ("-c", code),
        source_root,
        os.environ.copy(),
        command_timeout_seconds,
    )
    _require_success(pathlib.Path(sys.executable).name, ("-c", code), completed)
    lines = [line.strip() for line in completed.stdout.splitlines() if line.strip()]
    if not lines:
        raise InstallVerificationError("source native import did not report its module path")
    module = pathlib.Path(lines[-1]).resolve()
    if module.parent != expected_parent or not module.name.startswith("_native"):
        raise InstallVerificationError(
            f"fresh native module was not imported from {expected_parent}: {module}"
        )
    print(f"fresh native module: {module}")
    return module


def verify_empty_native_index(database_path: pathlib.Path) -> None:
    """Open and release a temporary native index before its directory is removed."""
    from ai_session_search import SessionQuery, SessionSearch

    search = SessionSearch(database_path)
    sessions = search.list_sessions(SessionQuery(limit=1))
    if sessions:
        raise InstallVerificationError("temporary native index was not empty")


def verify(
    source_root: pathlib.Path,
    executable_name: str = "aise",
    command_timeout_seconds: float = DEFAULT_COMMAND_TIMEOUT_SECONDS,
) -> None:
    import ai_session_search

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
        config_path = root / "config" / "config.toml"
        config_path.parent.mkdir(parents=True)
        config_path.write_text("", encoding="utf-8")
        os.environ["AI_SESSION_SEARCH_CONFIG"] = str(config_path)
        os.environ["AI_SESSION_SEARCH_CACHE_DIR"] = str(root / "cache")
        verify_empty_native_index(root / "index.db")

        environment = os.environ.copy()
        verify_configuration_contract(
            executable,
            executable_name,
            root,
            environment,
            command_timeout_seconds,
        )
        verify_cli_contract(
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
    parser.add_argument(
        "--source-native-import",
        action="store_true",
        help="only verify a bounded import of the source-tree native extension",
    )
    args = parser.parse_args()
    try:
        if args.source_native_import:
            verify_source_native_import(args.source_root, args.command_timeout_seconds)
        else:
            verify(args.source_root, args.executable, args.command_timeout_seconds)
    except (InstallVerificationError, OSError) as error:
        print(f"installed distribution verification failed: {error}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
