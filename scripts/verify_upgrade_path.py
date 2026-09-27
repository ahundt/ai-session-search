#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Prove that upgrading from a published release needs nothing beyond replacing the executable.

For each earlier release, in an empty home directory:

1. install that release's integrations from PyPI for every harness that installs files an upgrade
   could leave behind: skills and their discovery links, and both instruction variants;
2. save that release's complete `aise config show` output as `config.toml`, as a user who kept
   the printed defaults would have;
3. run one ordinary command with the candidate executable, which is all an upgrade through uv,
   pip, Cargo, or a native archive does;
4. require every integration the earlier release installed to report `configured`.

A failure names the release and what would have needed a manual step. CI runs this on every
push against the Linux build, so a change that breaks upgrades fails before it merges, and the
`upgrade` job of publish.yml runs it against the native executable before `verify` attests or
anything is published. It downloads the earlier release with `uvx`, so it needs network access.
Without `--from` it checks every release PyPI lists from `UPGRADE_FLOOR` on: between releases that
ends with the last release, and at release time the new version is not on PyPI until this check
has passed. CI, publish.yml, and RELEASING.md all run it the same way:

    uv run --no-project python -m scripts.verify_upgrade_path --executable target/release/aise

It refuses to run on Windows: aise finds the home folder there through the Windows known-folder
API, which HOME and USERPROFILE cannot redirect, so the earlier release would install into the real
profile.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import urllib.request
from collections.abc import Sequence

from scripts.release_versions import PYTHON_RELEASE_VERSION, release_sort_key

PYPI_PROJECT_URL = "https://pypi.org/pypi/ai-session-search/json"
# The oldest release an upgrade must work from without a manual step. 1.0.0rc1 is excluded: its
# instruction blocks read "outdated" after any later upgrade, because the instruction text changed
# in 1.0.0rc2 before the post-upgrade refresh existed, and it is a pre-release outside the
# compatibility baseline. Lower it only by making those upgrades work.
UPGRADE_FLOOR = "1.0.0rc2"

# Every harness whose install writes files an upgrade could leave outdated: skills and discovery
# links (claude, gemini, antigravity, pi, prime-agent), the MCP-first instruction block (claude,
# codex, gemini, opencode), and the CLI-only block (pi, prime-agent). Clients that only register an
# MCP server name the executable's path, which an upgrade keeps.
CLIENTS = ("claude", "codex", "gemini", "antigravity", "opencode", "pi", "prime-agent")


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


def _published() -> dict[str, object]:
    with urllib.request.urlopen(PYPI_PROJECT_URL, timeout=30) as response:
        return dict(json.load(response))


def published_versions_since(floor: str) -> list[str]:
    """Return every published release version at or above ``floor``, oldest first."""
    releases = _published()["releases"]
    assert isinstance(releases, dict)
    versions = [version for version in releases if PYTHON_RELEASE_VERSION.fullmatch(version)]
    return sorted(
        (version for version in versions if release_sort_key(version) >= release_sort_key(floor)),
        key=release_sort_key,
    )


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
        # A line this parser cannot read counts as a problem, so a changed status wording fails
        # the check rather than passing it.
        if not separator or (state != "configured" and not state.startswith("linked -> ")):
            problems.append(line)
    return problems


def check_upgrade(previous: str, executable: pathlib.Path) -> list[str]:
    """Return the problems an upgrade from ``previous`` to ``executable`` leaves behind."""
    # The earlier release's install starts indexing in the background, which may still be writing
    # into the temporary home when the check finishes.
    with tempfile.TemporaryDirectory(prefix="aise-upgrade-", ignore_cleanup_errors=True) as directory:
        home = pathlib.Path(directory).resolve()
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
        help="published version to upgrade from; repeat to check several (default: every "
        "release PyPI lists from --since on)",
    )
    parser.add_argument(
        "--since",
        default=UPGRADE_FLOOR,
        help=f"oldest published release to check when --from is omitted (default: {UPGRADE_FLOOR})",
    )
    args = parser.parse_args(argv)
    if os.name == "nt":
        print(
            "verify_upgrade_path refuses to run on Windows: aise finds the home folder through the "
            "Windows known-folder API, which HOME and USERPROFILE cannot redirect, so the earlier "
            "release would install into your real profile. Run it on Linux or macOS; CI runs it "
            "on Linux.",
            file=sys.stderr,
        )
        return 2
    executable = args.executable.resolve()
    if PYTHON_RELEASE_VERSION.fullmatch(args.since) is None:
        print(f"not a release version: {args.since!r}", file=sys.stderr)
        return 2
    if not args.previous:
        try:
            args.previous = published_versions_since(args.since)
        except (OSError, ValueError, KeyError) as error:
            # Passing here would report an upgrade check that checked nothing.
            print(
                f"could not read the published versions from {PYPI_PROJECT_URL}: {error}; "
                "pass --from",
                file=sys.stderr,
            )
            return 2
        if not args.previous:
            print(f"PyPI lists no release at or above {args.since}; pass --from", file=sys.stderr)
            return 2
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
