#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Prove that installed harness CLIs discover aise without an AI account.

1. Find Codex, OpenCode, Claude Code, and Gemini on PATH; name every missing CLI as skipped.
2. Create a temporary home and working directory, discard inherited credentials and config
   overrides, and redirect aise's config, database, and cache. Disable all transcript providers.
3. Run `aise integrations install --no-aliases --binary <executable>` for available clients,
   omitting skills and instructions because this check covers MCP discovery only.
4. Run each CLI's `mcp list`, printing its exact argv and output. Require exit status zero and
   the server name, including Gemini's Disabled state in an untrusted directory. Discovery
   does not prove permission to invoke tools or successful AI requests.
5. On Linux and macOS, also run `codex plugin list` and require the installed
   `ai-session-search@ai-session-search` plugin record, which is what lists aise in the Codex
   desktop Plugins page. Windows uses an explicit --codex-config, which registers no marketplace,
   so that check is a named skip there.

Windows cannot redirect aise's known-folder home API with HOME/USERPROFILE. There, select then
exclude Codex to suppress ALL native targets, add only explicit --codex-config/--opencode-config
paths, and point those CLIs at the same files using CODEX_HOME/OPENCODE_CONFIG. The receipt lives
beside the explicit aise config. Claude and Gemini are named skips on Windows until their full
profile redirection has been verified; never install into the real Windows profile.

Exit 0 if all present, supported CLIs pass; 1 for any failure; 2 if none could be checked.
CI bounds the run with its job timeout. No network installation or login is performed here.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Sequence

HARNESSES = ("codex", "opencode", "claude", "gemini")
SERVER_NAME = "ai-session-search"
# Markers the CLIs print beside a listed server that will not start: Claude Code shows
# "✗ Failed to connect" and still exits 0, so the name alone does not prove discovery.
FAILURE_MARKERS = ("fail", "error", "✗", "✘")
# The plugin record `aise integrations install` creates for its own Codex marketplace.
CODEX_PLUGIN_ID = f"{SERVER_NAME}@{SERVER_NAME}"


def classify(result: subprocess.CompletedProcess[str]) -> str:
    """Passed only when the list command succeeds and a line naming the server reports no failure.

    Gemini's "Disabled" in an untrusted directory still passes: it proves the server was found.
    """
    lines = (result.stdout + "\n" + result.stderr).splitlines()
    listed = [line.lower() for line in lines if SERVER_NAME in line]
    healthy = any(not any(marker in line for marker in FAILURE_MARKERS) for line in listed)
    return "passed" if result.returncode == 0 and healthy else "failed"


def classify_codex_plugin(result: subprocess.CompletedProcess[str]) -> str:
    """Passed only when `codex plugin list` succeeds and lists the installed aise plugin record."""
    listed = CODEX_PLUGIN_ID in result.stdout + "\n" + result.stderr
    return "passed" if result.returncode == 0 and listed else "failed"


def exit_code(statuses: Sequence[str]) -> int:
    if "failed" in statuses:
        return 1
    return 0 if "passed" in statuses else 2


HARNESS_ENVIRONMENT = frozenset(
    line.strip()
    for line in (pathlib.Path(__file__).resolve().parents[1] / "rust/ai-session-search-core/harness-environment.txt")
    .read_text(encoding="utf-8").splitlines()
    if line.strip() and not line.startswith("#")
)
PROVIDERS = ("claude", "claude-desktop", "codex", "cursor", "antigravity",
             "pi", "prime-agent", "aistudio", "gemini-cli")


def environment(home: pathlib.Path, *, config: pathlib.Path | None = None) -> dict[str, str]:
    """Build isolated runtime state; callers own the root until their children exit."""
    home = home.resolve()
    config = (config or home / "aise/config.toml").resolve()
    config.parent.mkdir(parents=True, exist_ok=True)
    if not config.exists() or not config.read_text(encoding="utf-8").strip():
        config.write_text(
            "[skills]\nsearch_paths = []\n" + "\n".join(
                f"[providers.{provider}]\nenabled = false\npaths = []" for provider in PROVIDERS
            ), encoding="utf-8",
        )
    # An allowlist keeps API keys, auth tokens, and harness-specific overrides out of the proof.
    result = {key: value for key, value in os.environ.items() if key.upper() in {
        "PATH", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "TEMP", "TMP", "TMPDIR",
    }}
    result.update({
        "HOME": str(home), "USERPROFILE": str(home),
        "APPDATA": str(home / "AppData/Roaming"),
        "LOCALAPPDATA": str(home / "AppData/Local"),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "XDG_DATA_HOME": str(home / ".local/share"),
        "XDG_STATE_HOME": str(home / ".local/state"),
        "CODEX_HOME": str(home / ".codex"),
        "CLAUDE_CONFIG_DIR": str(home / ".claude"),
        "GEMINI_CLI_HOME": str(home / "gemini"),
        "PI_CODING_AGENT_DIR": str(home / ".pi/agent"),
        "PRIME_AGENT_CODING_AGENT_DIR": str(home / ".prime/agent"),
        "FLATPAK_XDG_CONFIG_HOME": str(home / ".config"),
        "OPENCLAW_CONFIG_PATH": str(home / ".openclaw/openclaw.json"),
        "OPENCLAW_STATE_DIR": str(home / ".openclaw"),
        "OPENCLAW_HOME": str(home),
        "OPENCLAW_PROFILE": "aise-isolated",
        "OPENCODE_CONFIG": str(home / ".config/opencode/opencode.json"),
        "AI_SESSION_SEARCH_CONFIG": str(config),
        "AI_SESSION_SEARCH_DATABASE": str(home / "index.db"),
        "AI_SESSION_SEARCH_CACHE_DIR": str(home / "cache"),
        "AI_SESSION_SEARCH_HARNESS_HOME": str(home),
        "AI_SESSION_SEARCH_SKIP_RELEASE_NOTIFICATION": "1",
        "NO_COLOR": "1", "CI": "true",
    })
    assert HARNESS_ENVIRONMENT <= result.keys()
    return result


def install_command(
    executable: pathlib.Path, home: pathlib.Path, clients: Sequence[str], *, windows: bool,
) -> list[str]:
    argv = [str(executable), "integrations", "install", "--no-aliases", "--binary", str(executable),
            "--no-instructions", "--no-skill", "--no-index-preparation"]
    if windows:
        # Custom config flags are additive; this empty selection prevents real-profile writes.
        argv += ["--client", "codex", "--exclude-client", "codex"]
        for client in clients:
            flag, relative = {
                "codex": ("--codex-config", ".codex/config.toml"),
                "opencode": ("--opencode-config", ".config/opencode/opencode.json"),
            }[client]
            argv += [flag, str(home / relative)]
    else:
        argv += [argument for client in clients for argument in ("--client", client)]
    return argv


def run_command(argv: list[str], home: pathlib.Path, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    print(f"$ {subprocess.list2cmdline(argv) if os.name == 'nt' else shlex.join(argv)}", flush=True)
    try:
        result = subprocess.run(argv, cwd=home, env=env, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", check=False)
    except OSError as error:
        result = subprocess.CompletedProcess(argv, 1, "", str(error))
    print(f"exit {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}", flush=True)
    return result


def verify(executable: pathlib.Path, *, windows: bool) -> int:
    statuses: list[str] = []
    available: dict[str, str] = {}
    for name in HARNESSES:
        command = shutil.which(name)
        if command is None:
            print(f"{name}: skipped (CLI not installed)")
            statuses.append("skipped")
        elif windows and name in {"claude", "gemini"}:
            print(f"{name}: skipped (Windows profile redirection not verified)")
            statuses.append("skipped")
        else:
            available[name] = command
    if not available:
        return exit_code(statuses)
    with tempfile.TemporaryDirectory(prefix="aise-harness-") as temporary:
        home = pathlib.Path(temporary).resolve()
        env = environment(home)
        config = pathlib.Path(env["AI_SESSION_SEARCH_CONFIG"])
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text("\n".join(
            f"[providers.{provider}]\nenabled = false"
            for provider in ("claude", "claude-desktop", "codex", "cursor", "antigravity",
                             "pi", "prime-agent", "aistudio", "gemini-cli")
        ), encoding="utf-8")
        installed = run_command(install_command(executable, home, list(available), windows=windows), home, env)
        if installed.returncode != 0:
            print("aise integrations install: failed")
            return 1
        for name, command in available.items():
            status = classify(run_command([command, "mcp", "list"], home, env))
            statuses.append(status)
            print(f"{name}: {status}")
            if name != "codex":
                continue
            if windows:
                print("codex plugins: skipped (an explicit --codex-config registers no marketplace)")
                statuses.append("skipped")
                continue
            status = classify_codex_plugin(run_command([command, "plugin", "list"], home, env))
            statuses.append(status)
            print(f"codex plugins: {status}")
    return exit_code(statuses)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--executable", required=True, type=pathlib.Path)
    args = parser.parse_args(argv)
    # Harness CLIs print box-drawing characters that a Windows cp1252 console cannot encode;
    # escape them rather than crash after the check has already run.
    sys.stdout.reconfigure(errors="backslashreplace")  # type: ignore[union-attr]
    return verify(args.executable.resolve(), windows=os.name == "nt")


if __name__ == "__main__":
    raise SystemExit(main())
