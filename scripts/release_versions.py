# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Normalize one release identity across Python packaging and Cargo SemVer."""

from __future__ import annotations

import re
from pathlib import Path

_PHASES_FILE = Path(__file__).resolve().parents[1] / "rust/ai-session-search-core/src/release-phases.txt"


def parse_release_phases(text: str, source: str) -> list[tuple[str, str]]:
    """Return the `(cargo phase, python phase)` pairs of a release-phases file, named `source` in errors."""
    phases = []
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        fields = line.split()
        if len(fields) != 2:
            raise ValueError(f'{source} row {line!r} must be "<cargo phase> <python phase>"')
        phases.append((fields[0], fields[1]))
    return phases


_PHASES = parse_release_phases(_PHASES_FILE.read_text(encoding="utf-8"), _PHASES_FILE.name)
_CARGO_PHASE = {python: cargo for cargo, python in _PHASES}

PYTHON_RELEASE_VERSION = re.compile(
    r"(?P<release>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))"
    rf"(?:(?P<phase>{'|'.join(re.escape(phase) for phase in _CARGO_PHASE)})(?P<number>0|[1-9][0-9]*))?"
)


def cargo_version_for_python(python_version: str) -> str:
    """Return the Cargo SemVer spelling for one canonical Python release version."""
    match = PYTHON_RELEASE_VERSION.fullmatch(python_version)
    if match is None:
        raise ValueError(
            f"unsupported Python release version {python_version!r}; expected X.Y.Z, "
            "X.Y.ZaN, X.Y.ZbN, or X.Y.ZrcN"
        )
    phase = match.group("phase")
    if phase is None:
        return match.group("release")
    return f'{match.group("release")}-{_CARGO_PHASE[phase]}.{match.group("number")}'


_PHASE_ORDER: dict[str | None, int] = {python: index for index, (_, python) in enumerate(_PHASES)}
_PHASE_ORDER[None] = len(_PHASES)


def release_sort_key(python_version: str) -> tuple[int, int, int, int, int]:
    """Order release versions as PEP 440 does: 1.0.0a1 < 1.0.0rc2 < 1.0.0 < 1.0.1."""
    match = PYTHON_RELEASE_VERSION.fullmatch(python_version)
    if match is None:
        raise ValueError(f"unsupported Python release version {python_version!r}")
    major, minor, patch = (int(part) for part in match.group("release").split("."))
    number = int(match.group("number") or 0)
    return (major, minor, patch, _PHASE_ORDER[match.group("phase")], number)
