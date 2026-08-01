# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Public structured-input types for the Python API."""

from typing import Literal, TypeAlias, TypedDict


class FieldViewNoCharLimit(TypedDict):
    """Apply no additional character limit after line selection."""

    kind: Literal["no_char_limit"]


class FieldViewMaxChars(TypedDict):
    """Return at most ``max_chars`` Unicode-scalar characters from a field boundary."""

    kind: Literal["max_chars"]
    max_chars: int


# Keep an actual runtime union: mypy.stubtest rejects Python 3.12 TypeAliasType here.
FieldView: TypeAlias = FieldViewNoCharLimit | FieldViewMaxChars  # noqa: UP040


class MatchViewMinimalSpan(TypedDict):
    """Return only the complete selected match span."""

    kind: Literal["minimal_span"]


class MatchViewMaxChars(TypedDict):
    """Return up to ``max_chars`` Unicode-scalar characters centered on the complete match."""

    kind: Literal["max_chars"]
    max_chars: int


# Keep an actual runtime union: mypy.stubtest rejects Python 3.12 TypeAliasType here.
MatchView: TypeAlias = MatchViewMinimalSpan | MatchViewMaxChars  # noqa: UP040


class MessageClassificationCategory(TypedDict):
    """One ordered direct classification category."""

    name: str
    patterns: list[str]


class MessageClassificationDefinition(TypedDict):
    """Direct rules replacing the selected skill's capability file for one run."""

    categories: list[MessageClassificationCategory]


__all__ = [
    "FieldView",
    "FieldViewMaxChars",
    "FieldViewNoCharLimit",
    "MatchView",
    "MatchViewMaxChars",
    "MatchViewMinimalSpan",
    "MessageClassificationCategory",
    "MessageClassificationDefinition",
]
