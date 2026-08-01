#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Report which contributors hold copyright in each source file, from surviving lines.

Copyright attribution is measured rather than assumed. `git blame` reports the author of
every line as it stands now, so aggregating it per file answers who wrote the code that
is actually shipped, not who touched the file at some point. A contributor whose lines
have since been replaced keeps copyright in the history; this measurement decides which
`SPDX-FileCopyrightText` lines a file carries today, not who may be removed from one.
"""

from __future__ import annotations

import argparse
import collections
import json
import pathlib
import subprocess
import sys

# Below this a contributor's surviving lines are module declarations, imports, or
# single-line edits that carry no separable expression.
DEFAULT_CREDIT_THRESHOLD = 10
SOURCE_GLOBS = ("*.rs", "*.py", "*.sh", "*.ps1", "*.nix")
# Synthetic provider fixtures, not authored source.
EXCLUDED_PREFIXES = ("tests/aise-demo/",)


def _tracked_sources(root: pathlib.Path) -> list[str]:
    listed = subprocess.run(
        ["git", "ls-files", *SOURCE_GLOBS],
        cwd=root, capture_output=True, text=True, check=True,
    ).stdout.split()
    return [path for path in listed if not path.startswith(EXCLUDED_PREFIXES)]


def _surviving_lines_by_author(root: pathlib.Path, relative: str) -> dict[str, int]:
    """Count each author's currently surviving lines in one file.

    `-w` ignores whitespace-only reattribution and `-M` follows moves within the file, so
    reformatting does not transfer authorship. Names come from git's own mailmap
    resolution, which is why `.mailmap` is the one place duplicate identities are merged.
    """
    blame = subprocess.run(
        ["git", "blame", "--line-porcelain", "-w", "-M", "--", relative],
        cwd=root, capture_output=True, text=True,
    )
    if blame.returncode != 0:
        return {}
    counts: collections.Counter[str] = collections.Counter()
    author = None
    for line in blame.stdout.splitlines():
        if line.startswith("author "):
            author = line[len("author "):].strip()
        elif line.startswith("\t") and author:
            counts[author] += 1
    return dict(counts)


def measure(root: pathlib.Path) -> dict[str, dict[str, int]]:
    measured = {}
    for relative in _tracked_sources(root):
        counts = _surviving_lines_by_author(root, relative)
        if counts:
            measured[relative] = counts
    return measured


def holders(counts: dict[str, int], primary: str, threshold: int) -> list[str]:
    """Name the primary holder first, then every other contributor above the threshold."""
    return [
        primary,
        *sorted(
            author for author, lines in counts.items()
            if author != primary and lines >= threshold
        ),
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path.cwd())
    parser.add_argument("--primary", default="Andrew Hundt")
    parser.add_argument("--threshold", type=int, default=DEFAULT_CREDIT_THRESHOLD)
    parser.add_argument("--json", type=pathlib.Path, help="write the per-file counts here")
    args = parser.parse_args(argv)

    try:
        measured = measure(args.root)
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"authorship measurement failed: {error}", file=sys.stderr)
        return 1
    if not measured:
        print("no tracked source files matched", file=sys.stderr)
        return 1

    totals: collections.Counter[str] = collections.Counter()
    for counts in measured.values():
        totals.update(counts)
    grand = sum(totals.values())
    print(f"{grand:,} surviving lines across {len(measured)} files")
    for author, lines in totals.most_common():
        print(f"  {author:24} {lines:8,}  {100 * lines / grand:6.2f}%")

    print(f"\nfiles naming a holder besides {args.primary} at >= {args.threshold} lines:")
    extra = 0
    for relative, counts in sorted(measured.items()):
        others = holders(counts, args.primary, args.threshold)[1:]
        if others:
            extra += 1
            print(f"  {relative}: {', '.join(others)}")
    if not extra:
        print("  none")

    if args.json:
        args.json.write_text(json.dumps(measured, indent=1, sort_keys=True), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
