#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Prove that upgrading from a published release needs nothing beyond replacing the executable.

For each earlier release, in an empty home directory:

1. install that release's integrations for Claude Code, Codex, and Gemini CLI from PyPI;
2. save that release's complete `aise config show` output as `config.toml`, as a user who kept
   the printed defaults would have;
3. run one ordinary command with the candidate executable, which is all an upgrade through uv,
   pip, Cargo, or a native archive does;
4. require every integration the earlier release installed to report `configured`.

A failure names the release and what would have needed a manual step. The `verify` job of
publish.yml runs this against the Linux native executable before anything is published. It
downloads the earlier release with `uvx`, so it needs network access. Without `--from` it checks
the release below the current version's dated section in CHANGELOG.md, which exists only once the
release is prepared; before that, name the latest published release:

    uv run python -m scripts.verify_upgrade_path --executable target/release/aise --from 1.0.0rc3
"""

from __future__ import annotations

import argparse
import os
import pathlib
import subprocess
import sys
import tempfile
import tomllib
from collections.abc import Sequence

from scripts.release_versions import PYTHON_RELEASE_VERSION
from scripts.verify_release_metadata import previous_release

CLIENTS = ("claude", "codex", "gemini")
# Harness directories whose presence makes `aise integrations install` detect the client.
CLIENT_HOMES = (".claude", ".codex", ".gemini")


def _environment(home: pathlib.Path, executable_dir: pathlib.Path | None) -> dict[str, str]:
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("AI_SESSION_SEARCH_", "XDG_"))
    }
    environment["HOME"] = str(home)
    environment["USERPROFILE"] = str(home)
    environment["AI_SESSION_SEARCH_SKIP_RELEASE_NOTIFICATION"] = "1"
    if executable_dir is not None:
        environment["PATH"] = f"{executable_dir}{os.pathsep}{environment.get('PATH', '')}"
    return environment


def _run(argv: Sequence[str], environment: dict[str, str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, env=environment, capture_output=True, text=True, check=False)


def _client_arguments() -> list[str]:
    return [argument for client in CLIENTS for argument in ("--client", client)]


def problems_in_status(status: str) -> list[str]:
    """Return each status line that describes something other than a current integration.

    Executable aliases are skipped: they point at the executable's own directory, which this check
    changes on purpose by running the candidate from the build tree, and a real upgrade keeps.
    """
    # Fail closed on an empty or unrecognized report: a check that parsed nothing would otherwise
    # pass, whether the earlier release installed nothing or the status wording changed.
    if not any(
        "skills/ai-session-search: " in line and line.startswith("app ")
        for line in status.splitlines()
    ):
        return ["integrations status reported no installed ai-session-search skill to check"]
    problems = []
    for line in status.splitlines():
        if not line.strip() or line.startswith("executable alias "):
            continue
        _, separator, state = line.rpartition(": ")
        if not separator:
            continue
        if state != "configured" and not state.startswith("linked -> "):
            problems.append(line)
    return problems


def check_upgrade(previous: str, executable: pathlib.Path) -> list[str]:
    """Return the problems an upgrade from ``previous`` to ``executable`` leaves behind."""
    # The earlier release's install starts indexing in the background, which may still be writing
    # into the temporary home when the check finishes.
    with tempfile.TemporaryDirectory(prefix="aise-upgrade-", ignore_cleanup_errors=True) as directory:
        home = pathlib.Path(directory).resolve()
        for client_home in CLIENT_HOMES:
            (home / client_home).mkdir()
        old = ["uvx", "--quiet", "--from", f"ai-session-search=={previous}", "aise"]
        old_environment = _environment(home, None)
        installed = _run([*old, "integrations", "install", *_client_arguments()], old_environment)
        if installed.returncode != 0:
            return [f"{previous} could not install its integrations: {installed.stderr.strip()}"]
        shown = _run([*old, "config", "show"], old_environment)
        config_path = _run([*old, "config", "file"], old_environment).stdout.strip()
        if shown.returncode != 0 or not config_path:
            return [f"{previous} could not print its configuration: {shown.stderr.strip()}"]
        pathlib.Path(config_path).parent.mkdir(parents=True, exist_ok=True)
        pathlib.Path(config_path).write_text(shown.stdout, encoding="utf-8")

        new_environment = _environment(home, executable.parent)
        upgraded = _run([str(executable), "dates"], new_environment)
        if upgraded.returncode != 0:
            return [
                f"the first command after upgrading from {previous} failed: "
                f"{upgraded.stderr.strip()}"
            ]
        status = _run(
            [str(executable), "integrations", "status", *_client_arguments()], new_environment
        )
        if status.returncode != 0:
            return [f"integrations status failed after upgrading from {previous}: {status.stderr}"]
        return [
            line.replace(str(home), "~") for line in problems_in_status(status.stdout)
        ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--executable", type=pathlib.Path, required=True)
    parser.add_argument(
        "--from",
        dest="previous",
        action="append",
        help="published version to upgrade from; repeat to check several (default: the release "
        "below the current version in CHANGELOG.md)",
    )
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path.cwd())
    args = parser.parse_args(argv)
    executable = args.executable.resolve()
    if not args.previous:
        with (args.root / "pyproject.toml").open("rb") as source:
            version = tomllib.load(source)["project"]["version"]
        previous = previous_release(args.root, version)
        if previous is None:
            # Passing here would report an upgrade check that checked nothing.
            print(
                f"CHANGELOG.md has no dated section for {version} with a release below it; pass "
                "--from with the latest published version",
                file=sys.stderr,
            )
            return 2
        args.previous = [previous]
    failed = False
    for previous in args.previous:
        if PYTHON_RELEASE_VERSION.fullmatch(previous) is None:
            print(f"not a release version: {previous!r}", file=sys.stderr)
            return 2
        problems = check_upgrade(previous, executable)
        if problems:
            failed = True
            print(f"upgrading from {previous} needs a manual step:", file=sys.stderr)
            for problem in problems:
                print(f"  {problem}", file=sys.stderr)
        else:
            print(f"upgrading from {previous}: its config loads and every integration is current")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
