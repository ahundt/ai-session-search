"""Isolated tests for Python-to-Rust installation ownership evidence."""

from __future__ import annotations

import os
import sys
from collections.abc import Callable
from types import ModuleType

import pytest

from ai_session_search import entrypoint


class _InstalledDistribution:
    def __init__(self, files: dict[str, str]) -> None:
        self._files = files

    def read_text(self, filename: str) -> str | None:
        return self._files.get(filename)


def test_direct_source_metadata_and_uv_receipt_are_both_reported(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    invoked_executable = tmp_path / "bin" / "aise"
    invoked_executable.parent.mkdir()
    invoked_executable.write_text("#!/bin/sh\n", encoding="utf-8")
    (tmp_path / "uv-receipt.toml").write_text("[tool]\n", encoding="utf-8")
    monkeypatch.setattr(sys, "argv", [str(invoked_executable)])
    monkeypatch.setattr(sys, "prefix", str(tmp_path))
    monkeypatch.setattr(sys, "executable", str(tmp_path / "bin" / "python"))
    monkeypatch.setattr(
        entrypoint,
        "distribution",
        lambda _name: _InstalledDistribution(
            {
                "INSTALLER": "uv\n",
                "direct_url.json": '{"url":"file:///workspace/ai-session-search"}',
            }
        ),
    )

    evidence = entrypoint._collect_install_evidence()

    assert evidence["AI_SESSION_SEARCH_PYTHON_INSTALLER"] == "uv"
    assert evidence["AI_SESSION_SEARCH_INVOKED_EXECUTABLE"] == str(
        invoked_executable.resolve()
    )
    assert evidence["AI_SESSION_SEARCH_UV_TOOL_RECEIPT"] == str(
        tmp_path / "uv-receipt.toml"
    )
    assert evidence["AI_SESSION_SEARCH_DIRECT_URL"].startswith('{"url":"file:')


def test_pipx_metadata_requires_an_environment_bound_file(
    tmp_path, monkeypatch: pytest.MonkeyPatch
) -> None:
    (tmp_path / "pipx_metadata.json").write_text("{}", encoding="utf-8")
    monkeypatch.setattr(sys, "prefix", str(tmp_path))
    monkeypatch.setattr(
        entrypoint,
        "distribution",
        lambda _name: _InstalledDistribution({"INSTALLER": "pip"}),
    )

    evidence = entrypoint._collect_install_evidence()

    assert evidence["AI_SESSION_SEARCH_PIPX_METADATA"] == str(
        tmp_path / "pipx_metadata.json"
    )


def test_publishing_replaces_inherited_hints_from_another_environment(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    for environment_key in entrypoint.INSTALL_EVIDENCE_ENVIRONMENT_KEYS:
        monkeypatch.setenv(environment_key, "stale")
    monkeypatch.setattr(
        entrypoint,
        "_collect_install_evidence",
        lambda: {
            "AI_SESSION_SEARCH_PYTHON_EXECUTABLE": "/isolated/bin/python",
            "AI_SESSION_SEARCH_PYTHON_PREFIX": "/isolated",
        },
    )

    entrypoint._publish_install_evidence()

    assert os.environ["AI_SESSION_SEARCH_PYTHON_EXECUTABLE"] == "/isolated/bin/python"
    assert os.environ["AI_SESSION_SEARCH_PYTHON_PREFIX"] == "/isolated"
    for environment_key in entrypoint.INSTALL_EVIDENCE_ENVIRONMENT_KEYS:
        if environment_key not in {
            "AI_SESSION_SEARCH_PYTHON_EXECUTABLE",
            "AI_SESSION_SEARCH_PYTHON_PREFIX",
        }:
            assert environment_key not in os.environ


def _native_module(
    *,
    serve_mcp: Callable[[], None],
    run_cli_command: Callable[[list[str]], int],
) -> ModuleType:
    native = ModuleType("ai_session_search._native")
    native.serve_mcp = serve_mcp  # type: ignore[attr-defined]
    native._run_cli_command = run_cli_command  # type: ignore[attr-defined]
    return native


def test_mcp_stdio_dispatch_never_publishes_update_evidence(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[str] = []
    monkeypatch.setattr(sys, "argv", ["aise", "mcp", "serve"])
    monkeypatch.setattr(
        entrypoint,
        "_publish_install_evidence",
        lambda: calls.append("publish"),
    )
    monkeypatch.setitem(
        sys.modules,
        "ai_session_search._native",
        _native_module(
            serve_mcp=lambda: calls.append("serve"),
            run_cli_command=lambda _args: 0,
        ),
    )

    entrypoint.cli_main()

    assert calls == ["serve"]


def test_normal_cli_publishes_evidence_before_native_dispatch(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[str] = []

    def run_cli_command(args: list[str]) -> int:
        calls.append(f"run:{','.join(args)}")
        return 0

    monkeypatch.setattr(sys, "argv", ["aise", "config", "paths"])
    monkeypatch.setattr(
        entrypoint,
        "_publish_install_evidence",
        lambda: calls.append("publish"),
    )
    monkeypatch.setitem(
        sys.modules,
        "ai_session_search._native",
        _native_module(
            serve_mcp=lambda: calls.append("serve"),
            run_cli_command=run_cli_command,
        ),
    )

    entrypoint.cli_main()

    assert calls == ["publish", "run:config,paths"]
