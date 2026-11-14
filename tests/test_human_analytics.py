# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Human analytics use provenance rather than the stored role."""

import sqlite3
from pathlib import Path
from typing import Any

import pytest

from ai_session_search import _native as native


@pytest.mark.parametrize("include", [False, True])
def test_python_classification_and_repeats_read_cross_role_human_input(tmp_path: Path, include: bool) -> None:
    database = tmp_path / "index.db"
    search = native.SessionSearch(database, cache_dir=tmp_path / "cache")
    fixture = Path(__file__).resolve().parents[1] / "rust/ai-session-search-core/tests/fixtures/human_analytics.sql"
    with sqlite3.connect(database) as connection:
        connection.executescript(fixture.read_text(encoding="utf-8"))
    expected = {0, 1, 2, 3, 9, 10} | ({4} if include else set())
    matches = search.classify_messages(include_unattributed_input=include)
    assert {(m.session_id, m.message_seq) for m in matches} == {("claude:human-analytics", seq) for seq in expected}
    assert len(matches) == len(expected)
    mixed = next(m for m in matches if m.message_seq == 9)
    assert (mixed.match_start_char, mixed.match_end_char_exclusive) == (12, 22)
    report = search.run_skill(native.SkillRunQuery(
        skill=native.SkillSelector(name="corrections"),
        input=native.MessageClassificationQuery(include_unattributed_input=include),
    )).output.report
    assert report.matches == matches
    assert (report.unattributed_input_count, report.unattributed_input_included) == (1, include)
    for role in [None, "user"]:
        repeats: Any = search.repeats(
            role=role, include_unattributed_input=include, min_matches=1,
            phrase_min_words=1, phrase_max_words=3, max_groups=0, max_examples_per_group=0,
        )
        assert repeats["unattributed_input_count"] == 1
        assert repeats["unattributed_input_included"] == include
        examples = [example for group in repeats["groups"] for example in group["examples"]]
        assert {(e["session_id"], e["seq"]) for e in examples} == {("claude:human-analytics", seq) for seq in expected}
        assert all(e["preview"] == "you forgot mixed" for e in examples if e["seq"] == 9)
