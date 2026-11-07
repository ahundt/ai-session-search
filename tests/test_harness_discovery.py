# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from scripts import verify_harness_discovery as verifier


@pytest.mark.parametrize(
    ("returncode", "stdout", "stderr", "expected"),
    [
        (0, "ai-session-search enabled", "", "passed"),
        (0, "", "✓ ai-session-search connected", "passed"),
        (0, "ai-session-search: Disabled (untrusted folder)", "", "passed"),
        (0, "No MCP servers configured", "", "failed"),
        (1, "ai-session-search", "invalid config", "failed"),
    ],
)
def test_discovery_requires_success_and_the_server_name(
    returncode: int, stdout: str, stderr: str, expected: str,
) -> None:
    result = subprocess.CompletedProcess(["harness", "mcp", "list"], returncode, stdout, stderr)
    assert verifier.classify(result) == expected


@pytest.mark.parametrize(
    ("statuses", "expected"),
    [([], 2), (["skipped"], 2), (["passed", "skipped"], 0),
     (["failed", "skipped"], 1), (["passed", "failed"], 1)],
)
def test_exit_codes_do_not_turn_skips_or_failures_into_passes(
    statuses: list[str], expected: int,
) -> None:
    assert verifier.exit_code(statuses) == expected


def test_environment_redirects_state_and_drops_credentials(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("OPENAI_API_KEY", "must-not-inherit")
    monkeypatch.setenv("ANTHROPIC_API_KEY", "must-not-inherit")
    monkeypatch.setenv("OPENCODE_CONFIG_CONTENT", "must-not-inherit")
    monkeypatch.setenv("AI_SESSION_SEARCH_DATABASE", "must-not-inherit")
    environment = verifier.environment(tmp_path)
    assert "OPENAI_API_KEY" not in environment
    assert "ANTHROPIC_API_KEY" not in environment
    assert "OPENCODE_CONFIG_CONTENT" not in environment
    assert environment["AI_SESSION_SEARCH_DATABASE"] == str(tmp_path / "index.db")
    assert environment["HOME"] == environment["USERPROFILE"] == str(tmp_path)
    assert environment["CODEX_HOME"] == str(tmp_path / ".codex")
    assert environment["OPENCODE_CONFIG"] == str(tmp_path / ".config/opencode/opencode.json")


def test_windows_install_excludes_native_targets_and_uses_only_custom_paths(tmp_path: Path) -> None:
    argv = verifier.install_command(tmp_path / "aise.exe", tmp_path, ["codex", "opencode"], windows=True)
    # Custom flags ADD targets; including then excluding the same client leaves no native targets.
    assert argv[argv.index("--client") + 1] == "codex"
    assert argv[argv.index("--exclude-client") + 1] == "codex"
    assert "--no-skill" in argv and "--no-instructions" in argv and "--no-aliases" in argv
    assert argv[argv.index("--codex-config") + 1] == str(tmp_path / ".codex/config.toml")
    assert argv[argv.index("--opencode-config") + 1] == str(tmp_path / ".config/opencode/opencode.json")


def test_posix_install_selects_available_clients(tmp_path: Path) -> None:
    argv = verifier.install_command(tmp_path / "aise", tmp_path, ["claude", "gemini"], windows=False)
    assert argv[-4:] == ["--client", "claude", "--client", "gemini"]


@pytest.mark.parametrize(("available", "returncode", "output", "expected"), [
    (False, 0, "", 2), (True, 0, "ai-session-search", 0),
    (True, 0, "No servers", 1), (True, 1, "ai-session-search", 1),
])
def test_verifier_reports_each_harness_and_checks_commands(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
    available: bool, returncode: int, output: str, expected: int,
) -> None:
    calls: list[list[str]] = []

    def run(argv: list[str], home: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
        calls.append(argv)
        assert home.is_dir() and env["HOME"] == str(home)
        if "install" in argv:
            return subprocess.CompletedProcess(argv, 0, "installed", "")
        return subprocess.CompletedProcess(argv, returncode, output, "")

    monkeypatch.setattr(verifier.shutil, "which", lambda name: f"/bin/{name}" if available else None)
    monkeypatch.setattr(verifier, "run_command", run)
    assert verifier.verify(tmp_path / "aise", windows=False) == expected
    printed = capsys.readouterr().out
    for name in verifier.HARNESSES:
        assert f"{name}: {'skipped' if not available else 'passed' if expected == 0 else 'failed'}" in printed
    if available:
        assert len(calls) == 5
        assert all(argv[1:] == ["mcp", "list"] for argv in calls[1:])
    else:
        assert calls == []


def test_failed_install_stops_before_harness_checks(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[list[str]] = []

    def run(argv: list[str], home: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
        calls.append(argv)
        return subprocess.CompletedProcess(argv, 1, "", "install failed")

    monkeypatch.setattr(verifier.shutil, "which", lambda name: f"/bin/{name}")
    monkeypatch.setattr(verifier, "run_command", run)
    assert verifier.verify(tmp_path / "aise", windows=True) == 1
    assert len(calls) == 1


def test_windows_checks_only_redirected_clients_and_names_the_others(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    calls: list[list[str]] = []

    def run(argv: list[str], home: Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
        calls.append(argv)
        return subprocess.CompletedProcess(argv, 0, "ai-session-search", "")

    monkeypatch.setattr(verifier.shutil, "which", lambda name: f"/bin/{name}")
    monkeypatch.setattr(verifier, "run_command", run)
    assert verifier.verify(tmp_path / "aise.exe", windows=True) == 0
    assert [argv[0] for argv in calls[1:]] == ["/bin/codex", "/bin/opencode"]
    printed = capsys.readouterr().out
    assert "claude: skipped (Windows profile redirection not verified)" in printed
    assert "gemini: skipped (Windows profile redirection not verified)" in printed


def test_command_failure_preserves_both_output_streams(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str],
) -> None:
    result = subprocess.CompletedProcess(["broken-cli", "mcp", "list"], 1, "partial list", "bad config")
    monkeypatch.setattr(verifier.subprocess, "run", lambda *args, **kwargs: result)
    assert verifier.run_command(result.args, tmp_path, {}) is result
    printed = capsys.readouterr().out
    assert "broken-cli mcp list" in printed
    assert "exit 1" in printed and "partial list" in printed and "bad config" in printed


def test_launch_failure_prints_command_and_reason(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    result = verifier.run_command([str(tmp_path / "missing-cli"), "mcp", "list"], tmp_path, {})
    assert result.returncode != 0
    printed = capsys.readouterr().out
    assert "missing-cli" in printed and "mcp list" in printed
    assert "No such file" in printed or "cannot find" in printed
