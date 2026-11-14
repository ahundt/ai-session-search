# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
"""Bounds shared by programmatic calls and their command-line counterparts."""

from pathlib import Path

import pytest

import ai_session_search as aise


def test_repeat_example_context_is_a_nonnegative_count(tmp_path: Path) -> None:
    with aise.SessionSearch(tmp_path / "index.db") as search:
        search.refresh()
        with pytest.raises(aise.InvalidArgumentError, match="context"):
            search.repeats(context=-1)
