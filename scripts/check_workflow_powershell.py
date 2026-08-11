#!/usr/bin/env -S uv run --script
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
# /// script
# requires-python = ">=3.12"
# dependencies = ["pyyaml>=6"]
# ///
"""Parse every PowerShell ``run:`` body in the workflows and report syntax errors.

``actionlint`` hands ``run:`` bodies to ``shellcheck``, which only understands
POSIX shells, so a ``shell: pwsh`` step is never parsed by anything until a
runner executes it. A ``pwsh`` step continued with a backslash instead of a
backtick therefore reached a tag push and failed the release at
``native (windows-latest)``, after every other artifact had already been built.

This parses those bodies with PowerShell itself, so the same class of error
fails the local gate and CI instead.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import yaml

POWERSHELL_SHELLS = {"pwsh", "powershell"}
# A GitHub expression can sit anywhere a literal can, including inside a quoted
# PowerShell string. Substituting a bare identifier keeps the surrounding syntax
# intact without pretending to evaluate it.
EXPRESSION = re.compile(r"\$\{\{.*?\}\}", re.DOTALL)

# Invoked with -File so $args is populated; -Command would append the paths to
# the script text instead. "parsed" is echoed back so Python can assert that
# PowerShell saw every file, rather than reporting a pass it never earned.
PARSE_SCRIPT = """
$parsed = @()
$findings = @()
foreach ($path in $args) {
    $parsed += $path
    $errors = $null
    [System.Management.Automation.Language.Parser]::ParseFile(
        $path, [ref]$null, [ref]$errors) | Out-Null
    foreach ($e in $errors) {
        $findings += [pscustomobject]@{
            path = $path
            line = $e.Extent.StartLineNumber
            message = $e.Message
        }
    }
}
[pscustomobject]@{ parsed = $parsed; findings = $findings } |
    ConvertTo-Json -Depth 4 -Compress
"""


def powershell_steps(workflow: Path):
    """Yield (job, step name, body) for each PowerShell ``run:`` step."""
    document = yaml.safe_load(workflow.read_text(encoding="utf-8"))
    for job_name, job in (document or {}).get("jobs", {}).items():
        for index, step in enumerate(job.get("steps", []) or []):
            if not isinstance(step, dict):
                continue
            if step.get("shell") not in POWERSHELL_SHELLS:
                continue
            body = step.get("run")
            if not body:
                continue
            label = step.get("name") or f"step {index}"
            yield job_name, label, body


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "workflows",
        nargs="*",
        type=Path,
        help="workflow files; defaults to every .yml/.yaml under .github/workflows",
    )
    arguments = parser.parse_args()

    paths = arguments.workflows or sorted(path for pattern in ("*.yml", "*.yaml") for path in Path(".github/workflows").glob(pattern))
    if not paths:
        print("no workflow files found", file=sys.stderr)
        return 1

    pwsh = shutil.which("pwsh") or shutil.which("powershell")
    if pwsh is None:
        print(
            "SKIPPED: PowerShell is not installed, so pwsh run: bodies were not parsed.\n"
            "The workflow-security CI job parses them and blocks the merge.\n"
            "Install it to see failures here first:\n"
            "  brew install powershell",
            file=sys.stderr,
        )
        return 0

    steps: dict[str, tuple[Path, str, str]] = {}
    with tempfile.TemporaryDirectory() as directory:
        for workflow in paths:
            for job, label, body in powershell_steps(workflow):
                scratch = Path(directory) / f"{len(steps)}.ps1"
                scratch.write_text(EXPRESSION.sub("GitHubExpression", body), encoding="utf-8")
                steps[str(scratch)] = (workflow, job, label)

        if not steps:
            print("no PowerShell run: bodies to parse")
            return 0

        driver = Path(directory) / "parse.ps1"
        driver.write_text(PARSE_SCRIPT, encoding="utf-8")
        completed = subprocess.run(
            [pwsh, "-NoProfile", "-NonInteractive", "-File", str(driver), *steps],
            capture_output=True,
            text=True,
            check=False,
        )
        if completed.returncode != 0 or not completed.stdout.strip():
            print(completed.stderr.strip() or "PowerShell failed to run", file=sys.stderr)
            return 1
        report = json.loads(completed.stdout)

    findings = report["findings"] or []
    unparsed = set(steps) - set(report["parsed"] or [])
    if unparsed:
        print(
            f"PowerShell did not parse {len(unparsed)} of {len(steps)} extracted steps",
            file=sys.stderr,
        )
        return 1

    for finding in findings:
        workflow, job, label = steps[finding["path"]]
        print(
            f"{workflow}: job '{job}', step '{label}', PowerShell line {finding['line']}: {finding['message']}",
            file=sys.stderr,
        )

    count = len(steps)
    plural = "" if count == 1 else "s"
    if findings:
        print(f"\n{len(findings)} PowerShell syntax error(s) in {count} step{plural}", file=sys.stderr)
        return 1

    print(f"parsed {count} PowerShell step{plural}, no syntax errors")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
