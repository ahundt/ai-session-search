from __future__ import annotations

from pathlib import Path

import pytest

from scripts.verify_release_metadata import (
    ReleaseMetadataError,
    reconcile_registry_artifacts,
    verify_release_metadata,
)


def _write_manifests(root: Path, version: str = "1.0.0") -> None:
    (root / "rust/ai-session-search-core").mkdir(parents=True)
    (root / "rust/ai-session-search-python").mkdir(parents=True)
    (root / "pyproject.toml").write_text(
        f'[project]\nname = "ai-session-search"\nversion = "{version}"\n', encoding="utf-8"
    )
    (root / "rust/ai-session-search-core/Cargo.toml").write_text(
        f'[package]\nname = "ai-session-search"\nversion = "{version}"\n', encoding="utf-8"
    )
    (root / "rust/ai-session-search-python/Cargo.toml").write_text(
        f'''[package]\nname = "ai-session-search-python"\nversion = "{version}"\n'''
        f'''[dependencies]\nai-session-search = {{ version = "{version}", path = "../ai-session-search-core" }}\n''',
        encoding="utf-8",
    )


def test_release_metadata_requires_tag_manifests_and_dependency_to_match(tmp_path: Path) -> None:
    _write_manifests(tmp_path)
    assert verify_release_metadata(tmp_path, "v1.0.0") == "1.0.0"

    python_manifest = tmp_path / "rust/ai-session-search-python/Cargo.toml"
    python_manifest.write_text(
        python_manifest.read_text(encoding="utf-8").replace('version = "1.0.0"', 'version = "2.0.0"', 1),
        encoding="utf-8",
    )
    with pytest.raises(ReleaseMetadataError, match="versions differ"):
        verify_release_metadata(tmp_path, "v1.0.0")


@pytest.mark.parametrize("tag", ["1.0.0", "v1", "release-1.0.0", "v01.0.0"])
def test_release_metadata_rejects_noncanonical_tag(tmp_path: Path, tag: str) -> None:
    _write_manifests(tmp_path)
    with pytest.raises(ReleaseMetadataError, match="tag"):
        verify_release_metadata(tmp_path, tag)


def test_retry_reconciliation_is_idempotent_only_for_exact_registry_state() -> None:
    expected = {"package-1.0.0.whl": "abc", "package-1.0.0.tar.gz": "def"}
    assert reconcile_registry_artifacts(expected, {}) == "publish"
    assert reconcile_registry_artifacts(expected, expected) == "already-published"
    with pytest.raises(ReleaseMetadataError, match="partial"):
        reconcile_registry_artifacts(expected, {"package-1.0.0.whl": "abc"})
    with pytest.raises(ReleaseMetadataError, match="checksum"):
        reconcile_registry_artifacts(expected, {**expected, "package-1.0.0.whl": "wrong"})
