# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Normalize one release identity across Python packaging and Cargo SemVer."""

from __future__ import annotations

import re

PYTHON_RELEASE_VERSION = re.compile(
    r"(?P<release>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))"
    r"(?:(?P<phase>a|b|rc)(?P<number>0|[1-9][0-9]*))?"
)
_CARGO_PHASE = {"a": "alpha", "b": "beta", "rc": "rc"}


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


_PHASE_ORDER = {"a": 0, "b": 1, "rc": 2, None: 3}


def release_sort_key(python_version: str) -> tuple[int, int, int, int, int]:
    """Order release versions as PEP 440 does: 1.0.0a1 < 1.0.0rc2 < 1.0.0 < 1.0.1."""
    match = PYTHON_RELEASE_VERSION.fullmatch(python_version)
    if match is None:
        raise ValueError(f"unsupported Python release version {python_version!r}")
    major, minor, patch = (int(part) for part in match.group("release").split("."))
    number = int(match.group("number") or 0)
    return (major, minor, patch, _PHASE_ORDER[match.group("phase")], number)
