import json
import os
import signal
import subprocess
import sys
from pathlib import Path

from ai_session_search import native
from ai_session_search._native import _run_cli_command

MCP_PROCESS_TIMEOUT_SECONDS = 10


def test_python_api_exposes_rust_mcp_server() -> None:
    assert callable(native.serve_mcp)


def test_single_python_executable_advertises_mcp_serve() -> None:
    result = subprocess.run(
        _command("mcp", "--help"),
        capture_output=True,
        text=True,
        timeout=MCP_PROCESS_TIMEOUT_SECONDS,
        check=True,
    )
    for command in ("serve", "install", "status", "uninstall"):
        assert command in result.stdout


def test_single_python_executable_uses_canonical_rust_cli() -> None:
    result = subprocess.run(
        _command("--help"),
        capture_output=True,
        text=True,
        timeout=MCP_PROCESS_TIMEOUT_SECONDS,
        check=True,
    )
    assert "Search local sessions from Claude Code" in result.stdout
    assert "Google AI Studio" in result.stdout
    assert "Gemini CLI" in result.stdout
    assert "instruction-history" not in result.stdout


def test_rust_cli_parse_error_does_not_terminate_python() -> None:
    assert _run_cli_command(["--definitely-not-a-command"]) == 2


def test_python_executable_formats_rust_runtime_error_without_traceback(tmp_path: Path) -> None:
    result = subprocess.run(
        _command("migrate", "verify", "--receipt", str(tmp_path / "missing.json")),
        capture_output=True,
        text=True,
        timeout=MCP_PROCESS_TIMEOUT_SECONDS,
    )
    assert result.returncode == 1
    assert result.stderr.startswith("error: ")
    assert "missing.json" in result.stderr
    assert "Traceback" not in result.stderr


def test_package_manifests_expose_no_second_mcp_executable() -> None:
    root = Path(__file__).resolve().parents[1]
    cargo = (root / "rust/ai-session-search-core/Cargo.toml").read_text(encoding="utf-8")
    flake = (root / "rust/ai-session-search-core/flake.nix").read_text(encoding="utf-8")
    assert 'name = "aise-mcp"' not in cargo
    assert "aise-mcp =" not in flake


def _command(*args: str) -> list[str]:
    return [
        sys.executable,
        "-c",
        "from ai_session_search.entrypoint import cli_main; cli_main()",
        *args,
    ]


def _environment(tmp_path: Path) -> dict[str, str]:
    return {
        **os.environ,
        "AI_SESSION_SEARCH_CONFIG": str(tmp_path / "config.toml"),
        "AI_SESSION_SEARCH_CACHE_DIR": str(tmp_path / "cache"),
    }


def test_single_python_executable_serves_initialize_and_exits_on_eof(tmp_path: Path) -> None:
    request = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n'
    result = subprocess.run(
        _command("mcp", "serve"),
        input=request,
        capture_output=True,
        text=True,
        env=_environment(tmp_path),
        timeout=MCP_PROCESS_TIMEOUT_SECONDS,
        check=True,
    )
    response = json.loads(result.stdout)
    assert response["id"] == 1
    assert response["result"]["capabilities"]["tools"] == {}


def test_mcp_serve_uses_global_cli_configuration_overrides(tmp_path: Path) -> None:
    configured_database = tmp_path / "configured.db"
    explicit_database = tmp_path / "explicit.db"
    config_path = tmp_path / "config.toml"
    providers = [
        "claude",
        "claude-desktop",
        "codex",
        "cursor",
        "antigravity",
        "pi",
        "ai-studio",
        "gemini-cli",
    ]
    config_path.write_text(
        f"[index]\ndb_path = {str(configured_database)!r}\n"
        + "\n".join(f"[providers.{provider}]\nenabled = false" for provider in providers),
        encoding="utf-8",
    )
    requests = "\n".join(
        [
            '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}',
            '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_index_status","arguments":{}}}',
            "",
        ]
    )

    result = subprocess.run(
        _command(
            "--config",
            str(config_path),
            "--database",
            str(explicit_database),
            "mcp",
            "serve",
        ),
        input=requests,
        capture_output=True,
        text=True,
        env=_environment(tmp_path),
        timeout=MCP_PROCESS_TIMEOUT_SECONDS,
        check=True,
    )

    responses = [json.loads(line) for line in result.stdout.splitlines()]
    assert [response["id"] for response in responses] == [1, 2]
    assert explicit_database.exists()
    assert not configured_database.exists()


def test_single_python_executable_terminates_under_sigterm(tmp_path: Path) -> None:
    if sys.platform == "win32":
        return
    process = subprocess.Popen(
        _command("mcp", "serve"),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=_environment(tmp_path),
    )
    assert process.stdin is not None
    assert process.stdout is not None
    process.stdin.write('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}\n')
    process.stdin.flush()
    assert json.loads(process.stdout.readline())["id"] == 1
    os.kill(process.pid, signal.SIGTERM)
    assert process.wait(timeout=MCP_PROCESS_TIMEOUT_SECONDS) == -signal.SIGTERM
