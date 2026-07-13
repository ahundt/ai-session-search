"""Low-overhead console dispatch for protocol and interactive CLI commands."""

from __future__ import annotations

import sys

MCP_COMMAND = "mcp"
MCP_SERVE_ARGS = ("serve",)


def cli_main() -> None:
    """Start MCP without importing the legacy CLI; delegate every other command."""
    args = tuple(sys.argv[1:])
    if args[:1] == (MCP_COMMAND,):
        from ai_session_search._native import _run_mcp_command, serve_mcp

        mcp_args = args[1:]
        if mcp_args == MCP_SERVE_ARGS:
            serve_mcp()
            return
        exit_code = _run_mcp_command(list(mcp_args))
        if exit_code:
            raise SystemExit(exit_code)
        return

    from ai_session_search.cli import cli_main as legacy_cli_main

    legacy_cli_main()
