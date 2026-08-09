# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Low-overhead console dispatch for protocol and interactive CLI commands."""

from __future__ import annotations

import os
import sys
from importlib.metadata import PackageNotFoundError, distribution
from pathlib import Path

MCP_COMMAND = "mcp"
MCP_SERVE_ARGS = ("serve",)
PACKAGE_DISTRIBUTION_NAME = "ai-session-search"
EXECUTABLE_NAMES = frozenset({"aise", "aisearch", "ai_session_search"})

INSTALL_EVIDENCE_ENVIRONMENT_KEYS = (
    "AI_SESSION_SEARCH_INVOKED_EXECUTABLE",
    "AI_SESSION_SEARCH_PYTHON_INSTALLER",
    "AI_SESSION_SEARCH_PYTHON_BASE_EXECUTABLE",
    "AI_SESSION_SEARCH_PYTHON_EXECUTABLE",
    "AI_SESSION_SEARCH_PYTHON_PREFIX",
    "AI_SESSION_SEARCH_UV_TOOL_RECEIPT",
    "AI_SESSION_SEARCH_PIPX_METADATA",
    "AI_SESSION_SEARCH_DIRECT_URL",
)


def _collect_install_evidence() -> dict[str, str]:
    """Collect metadata for the current Python environment without changing it."""
    evidence = {
        "AI_SESSION_SEARCH_PYTHON_EXECUTABLE": sys.executable,
        "AI_SESSION_SEARCH_PYTHON_PREFIX": sys.prefix,
    }
    base_executable = getattr(sys, "_base_executable", None)
    if base_executable:
        evidence["AI_SESSION_SEARCH_PYTHON_BASE_EXECUTABLE"] = base_executable
    invoked_path = Path(sys.argv[0])
    if invoked_path.stem in EXECUTABLE_NAMES:
        try:
            evidence["AI_SESSION_SEARCH_INVOKED_EXECUTABLE"] = str(invoked_path.resolve(strict=True))
        except OSError:
            pass
    try:
        installed_distribution = distribution(PACKAGE_DISTRIBUTION_NAME)
    except PackageNotFoundError:
        return evidence

    installer = (installed_distribution.read_text("INSTALLER") or "").strip()
    if installer:
        evidence["AI_SESSION_SEARCH_PYTHON_INSTALLER"] = installer

    direct_url = (installed_distribution.read_text("direct_url.json") or "").strip()
    if direct_url:
        evidence["AI_SESSION_SEARCH_DIRECT_URL"] = direct_url

    python_prefix = Path(sys.prefix)
    for environment_key, metadata_path in (
        (
            "AI_SESSION_SEARCH_UV_TOOL_RECEIPT",
            python_prefix / "uv-receipt.toml",
        ),
        (
            "AI_SESSION_SEARCH_PIPX_METADATA",
            python_prefix / "pipx_metadata.json",
        ),
    ):
        if metadata_path.is_file():
            evidence[environment_key] = str(metadata_path)
    return evidence


def _publish_install_evidence() -> None:
    """Replace inherited hints with evidence from this exact Python environment."""
    evidence = _collect_install_evidence()
    for environment_key in INSTALL_EVIDENCE_ENVIRONMENT_KEYS:
        value = evidence.get(environment_key)
        if value is None:
            os.environ.pop(environment_key, None)
        else:
            os.environ[environment_key] = value


def cli_main() -> None:
    """Run the canonical Rust CLI and its official-rmcp stdio server."""
    args = tuple(sys.argv[1:])
    if args == (MCP_COMMAND, *MCP_SERVE_ARGS):
        from ai_session_search._native import serve_mcp

        serve_mcp()
        return

    _publish_install_evidence()

    from ai_session_search._native import _run_cli_command

    try:
        exit_code = _run_cli_command(list(args))
    except BrokenPipeError:
        return
    except RuntimeError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from None
    if exit_code:
        raise SystemExit(exit_code)
