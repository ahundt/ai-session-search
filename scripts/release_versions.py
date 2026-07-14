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
