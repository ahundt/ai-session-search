import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

from ai_session_search import native
from ai_session_search._native import _run_cli_command

MCP_PROCESS_TIMEOUT_SECONDS = 10


def test_python_api_exposes_rust_mcp_server() -> None:
    assert callable(native.serve_mcp)


def test_python_mcp_binding_delegates_to_official_rmcp_transport() -> None:
    root = Path(__file__).resolve().parents[1]
    source = (root / "rust/ai-session-search-python/src/lib.rs").read_text(encoding="utf-8")
    binding = source.split("fn serve_mcp", maxsplit=1)[1].split("#[pyfunction]", maxsplit=1)[0]

    assert "mcp_server::serve()" in binding
    assert "py.detach" in binding
    assert "McpServer::load" not in binding
    assert ".handle_line" not in binding


def test_core_has_one_official_rmcp_protocol_owner() -> None:
    root = Path(__file__).resolve().parents[1]
    source = (root / "rust/ai-session-search-core/src/mcp_server.rs").read_text(encoding="utf-8")

    assert "impl rmcp::ServerHandler for OfficialMcpServer" in source
    assert "serve_transport" in source
    for superseded_manual_protocol_path in (
        "pub struct McpServer",
        "fn handle_line",
        "fn prepare_line",
        "fn handle_initialize",
        "fn handle_tools_call",
    ):
        assert superseded_manual_protocol_path not in source


def test_single_python_executable_advertises_mcp_serve() -> None:
    result = subprocess.run(
        _command("mcp", "--help"),
        capture_output=True,
        text=True,
        timeout=MCP_PROCESS_TIMEOUT_SECONDS,
        check=True,
    )
    assert "serve" in result.stdout
    for removed_alias in ("install", "recover", "status", "uninstall"):
        assert removed_alias not in result.stdout


def test_single_python_executable_uses_canonical_rust_cli() -> None:
    result = subprocess.run(
        _command("--help"),
        capture_output=True,
        text=True,
        timeout=MCP_PROCESS_TIMEOUT_SECONDS,
        check=True,
    )
    assert "AI Session Search (aise): search local sessions from Claude Code" in result.stdout
    assert "Google AI Studio" in result.stdout
    assert "Gemini CLI" in result.stdout
    assert "instruction-history" not in result.stdout
    for command in ("config", "integrations", "mcp", "package"):
        assert command in result.stdout


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


def test_python_console_install_starts_the_canonical_refresh_child(tmp_path: Path) -> None:
    executable = shutil.which("aise")
    assert executable is not None
    database = tmp_path / "index.db"
    cache = tmp_path / "cache"
    config = tmp_path / "config.toml"
    config.write_text(
        f"[index]\ndb_path = {str(database)!r}\ncache_dir = {str(cache)!r}\n"
        + "\n".join(
            f"[providers.{provider}]\nenabled = false\npaths = []"
            for provider in (
                "claude",
                "claude-desktop",
                "codex",
                "cursor",
                "antigravity",
                "pi",
                "aistudio",
                "gemini-cli",
            )
        ),
        encoding="utf-8",
    )
    home = tmp_path / "home"
    home.mkdir()
    result = subprocess.run(
        [
            executable,
            "--config",
            str(config),
            "integrations",
            "install",
            "--client",
            "codex",
            "--binary",
            executable,
            "--no-aliases",
            "--no-instructions",
            "--no-skill",
        ],
        capture_output=True,
        text=True,
        env={**os.environ, "HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config")},
        timeout=MCP_PROCESS_TIMEOUT_SECONDS,
    )
    assert result.returncode == 0, result.stderr
    assert "started session index preparation in the background" in result.stdout

    recorded_words = 0
    deadline = time.monotonic() + MCP_PROCESS_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if database.is_file():
            with sqlite3.connect(database) as connection:
                recorded_words = connection.execute(
                    """
                    select count(*) from index_metadata
                    where key like 'auto_reindex_parser_contract_%'
                    """
                ).fetchone()[0]
            if recorded_words == 4:
                break
        time.sleep(0.025)
    assert recorded_words == 4


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
    config_path = tmp_path / "config.toml"
    if not config_path.exists():
        config_path.write_text("", encoding="utf-8")
    return {
        **os.environ,
        "AI_SESSION_SEARCH_CONFIG": str(config_path),
        "AI_SESSION_SEARCH_CACHE_DIR": str(tmp_path / "cache"),
    }


def _initialize_request(request_id: int = 1) -> str:
    return json.dumps(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "aise-python-test", "version": "1"},
            },
        }
    )


def test_single_python_executable_serves_initialize_and_exits_on_eof(tmp_path: Path) -> None:
    request = f"{_initialize_request()}\n"
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
    instructions = response["result"]["instructions"]
    assert "AI Session Search (`aise`)" in instructions
    assert "`search_sessions`" in instructions
    assert "`search_messages`" in instructions
    assert "`get_session`" in instructions
    assert len(instructions) <= 512


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
        "aistudio",
        "gemini-cli",
    ]
    config_path.write_text(
        f"[index]\ndb_path = {str(configured_database)!r}\n" + "\n".join(f"[providers.{provider}]\nenabled = false" for provider in providers),
        encoding="utf-8",
    )
    requests = "\n".join(
        [
            _initialize_request(),
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
    )
    assert result.returncode == 0, result.stderr

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
    process.stdin.write(f"{_initialize_request()}\n")
    process.stdin.flush()
    assert json.loads(process.stdout.readline())["id"] == 1
    os.kill(process.pid, signal.SIGTERM)
    assert process.wait(timeout=MCP_PROCESS_TIMEOUT_SECONDS) == -signal.SIGTERM
