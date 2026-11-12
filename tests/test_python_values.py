# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
"""Value semantics at the native Python boundary."""

import inspect
import json
from pathlib import Path

import pytest

from ai_session_search import native


@pytest.mark.parametrize("factory", [
    lambda: native.SessionQuery(limit=3),
    lambda: native.DateRange(when="2026-01"),
    lambda: native.QueryScope(path_prefix="/synthetic"),
    lambda: native.MessageSearchRequest(context=2, detail="full"),
    lambda: native.FileQuery(limit=3),
    lambda: native.AnalysisPolicy(max_classification_chars=40),
])
def test_requests_are_inspectable_values(factory):
    left, right = factory(), factory()
    assert left == right
    assert left != object()
    assert type(left).__name__ in repr(left)
    assert "object at 0x" not in repr(left)
    assert isinstance(left.to_dict(), dict)
    json.dumps(left.to_dict())


def test_request_equality_includes_every_parameter():
    assert native.SessionQuery(limit=2) != native.SessionQuery(limit=3)
    assert native.AnalysisPolicy(max_classification_chars=40) != native.AnalysisPolicy(max_classification_chars=41)
    assert native.MessageSearchRequest(detail="full") != native.MessageSearchRequest(detail="compact")


def test_every_value_type_has_value_protocol():
    resources = {"SessionSearch", "MessageSearchBatches", "ReconstructedFileVersions"}
    for name in native.__all__:
        cls = getattr(native, name)
        if inspect.isclass(cls) and name not in resources and not issubclass(cls, Exception):
            assert callable(getattr(cls, "to_dict", None)), name
            assert cls.__repr__ is not object.__repr__, name
            assert cls.__eq__ is not object.__eq__, name


def test_results_are_inspectable_values(tmp_path: Path):
    search = native.SessionSearch(tmp_path / "index.db")
    left, right = search.index_status(), search.index_status()
    assert left == right
    assert "IndexStatus" in repr(left)
    assert left.to_dict()["parser_health"]["indexed_sessions"] == 0
    json.dumps(left.to_dict())
