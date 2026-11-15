# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0
"""Value semantics at the native Python boundary."""

import ast
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
    resources = {"SessionSearch", "MessageSearchBatches", "ReconstructedFileVersions", "AnalysisCursor"}
    for name in native.__all__:
        cls = getattr(native, name)
        if inspect.isclass(cls) and name not in resources and not issubclass(cls, Exception):
            assert callable(getattr(cls, "to_dict", None)), name
            assert cls.__repr__ is not object.__repr__, name
            assert cls.__eq__ is not object.__eq__, name


def test_every_public_to_dict_returns_dict():
    root = Path(__file__).resolve().parents[1]
    stub = ast.parse((root / "ai_session_search/_native.pyi").read_text())
    classes = {node.name: node for node in stub.body if isinstance(node, ast.ClassDef)}
    for name in native.__all__:
        cls = getattr(native, name)
        if not inspect.isclass(cls) or not hasattr(cls, "to_dict"):
            continue
        method = next(
            node for node in classes[name].body
            if isinstance(node, ast.FunctionDef) and node.name == "to_dict"
        )
        assert method.returns is not None
        assert ast.unparse(method.returns) == "dict[str, Any]", name
    # The sole native implementation converts every successful return to PyDict.
    # This covers result types with no public constructor, not just request samples.
    source = (root / "rust/ai-session-search-python/src/lib.rs").read_text()
    assert source.count("fn to_dict") == 1
    assert "fn to_dict<'py>($slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyDict>>" in source
    assert "Ok(value?.cast_into::<PyDict>()?)" in source
    assert not hasattr(native.AnalysisCursor, "to_dict")


def test_results_are_inspectable_values(tmp_path: Path):
    search = native.SessionSearch(tmp_path / "index.db")
    left, right = search.index_status(), search.index_status()
    assert left == right
    assert "IndexStatus" in repr(left)
    assert left.to_dict()["parser_health"]["indexed_sessions"] == 0
    json.dumps(left.to_dict())


def test_search_dictionary_omits_absent_optional_groups(tmp_path: Path):
    search = native.SessionSearch(tmp_path / "index.db")
    search.refresh()
    value = search.search_messages().to_dict()
    assert "included" not in value
    assert "receipt" not in value


def test_package_root_exports_all_native_result_types():
    import ai_session_search as package

    for name in native.__all__:
        value = getattr(native, name)
        if inspect.isclass(value):
            assert getattr(package, name, None) is value, name
            assert name in package.__all__


def test_public_native_functions_and_types_have_runtime_help():
    missing = []
    for name in native.__all__:
        value = getattr(native, name)
        if callable(value) and not value.__doc__:
            missing.append(name)
        if inspect.isclass(value):
            for method, member in vars(value).items():
                if not method.startswith("_") and callable(member) and not member.__doc__:
                    missing.append(f"{name}.{method}")
    assert not missing, missing
