"""Low-overhead console dispatch for protocol and interactive CLI commands."""

from __future__ import annotations

import sys

MCP_COMMAND = "mcp"
MCP_SERVE_ARGS = ("serve",)


def cli_main() -> None:
    """Run the canonical Rust CLI while keeping Python-owned MCP stdio."""
    args = tuple(sys.argv[1:])
    if args == (MCP_COMMAND, *MCP_SERVE_ARGS):
        from ai_session_search._native import serve_mcp

        serve_mcp()
        return

    from ai_session_search._native import _run_cli_command

    try:
        exit_code = _run_cli_command(list(args))
    except RuntimeError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
    if exit_code:
        raise SystemExit(exit_code)
