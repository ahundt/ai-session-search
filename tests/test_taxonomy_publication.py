import json
from pathlib import Path

import pytest

from ai_session_search.analysis.orchestrator import (
    _path_component,
    _resolve_formats,
    apply_symlinks,
    build_taxonomy,
)


def test_taxonomy_keys_duplicate_titles_by_canonical_session_id() -> None:
    dimensions = [{"name": "role", "match": "field", "field": "roles"}]
    records = [
        {"session_id": "claude:same", "name": "Same", "roles": ["author"]},
        {"session_id": "codex:same", "name": "Same", "roles": ["reviewer"]},
    ]
    assert build_taxonomy(records, {}, dimensions) == {
        "claude:same": {"role": ["author"]},
        "codex:same": {"role": ["reviewer"]},
    }


def test_absolute_and_parent_categories_remain_one_component(tmp_path: Path) -> None:
    assert _path_component("/Users/alice/project", tmp_path) == "%2FUsers%2Falice%2Fproject"
    assert _path_component("CON", tmp_path) == "value-CON"
    assert _path_component("category.", tmp_path) == "value-category."
    with pytest.raises(ValueError, match="component is invalid"):
        _path_component("..", tmp_path)


def test_symlink_batch_is_opt_in_bounded_and_manifested(tmp_path: Path) -> None:
    source = tmp_path / "source.jsonl"
    source.write_text("{}", encoding="utf-8")
    output = tmp_path / "output"
    records = [{"session_id": "claude:one", "name": "duplicate title", "filepath": str(source)}]
    taxonomy = {"claude:one": {"cwd": ["/Users/alice/project"]}}
    assert apply_symlinks(records, output, taxonomy) == 1
    manifest = json.loads((output / "SESSION_TAXONOMY_LINKS.json").read_text(encoding="utf-8"))
    link = output / manifest["links"][0]["path"]
    assert link.is_symlink()
    assert link.resolve() == source
    assert output in link.parents
    assert apply_symlinks(records, output, taxonomy) == 0
    repeated = json.loads((output / "SESSION_TAXONOMY_LINKS.json").read_text(encoding="utf-8"))
    assert repeated == manifest
    assert _resolve_formats({}, None) == ["json", "markdown"]


def test_symlink_plan_fails_before_mutation_on_missing_source(tmp_path: Path) -> None:
    records = [{"session_id": "claude:missing", "name": "missing", "filepath": str(tmp_path / "nope")}]
    taxonomy = {"claude:missing": {"role": ["author"]}}
    with pytest.raises(FileNotFoundError):
        apply_symlinks(records, tmp_path / "output", taxonomy)
    assert not (tmp_path / "output").exists()


def test_symlink_batch_rolls_back_after_mid_publish_failure(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    import ai_session_search.analysis.orchestrator as orchestrator

    sources = [tmp_path / "one.jsonl", tmp_path / "two.jsonl"]
    for source in sources:
        source.write_text("{}", encoding="utf-8")
    records = [
        {"session_id": f"claude:{index}", "name": "same", "filepath": str(source)}
        for index, source in enumerate(sources)
    ]
    taxonomy = {
        f"claude:{index}": {"role": ["author"]}
        for index in range(len(sources))
    }
    original = orchestrator.make_symlink
    calls = 0

    def fail_second(source: str, target: Path) -> bool:
        nonlocal calls
        calls += 1
        if calls == 2:
            raise OSError("injected symlink failure")
        return original(source, target)

    monkeypatch.setattr(orchestrator, "make_symlink", fail_second)
    output = tmp_path / "output"
    with pytest.raises(OSError, match="injected symlink failure"):
        apply_symlinks(records, output, taxonomy)
    assert not list(output.rglob("claude%3A*"))
    assert not (output / "SESSION_TAXONOMY_LINKS.json").exists()
