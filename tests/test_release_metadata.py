from __future__ import annotations

from pathlib import Path

import pytest

from scripts.release_versions import cargo_version_for_python
from scripts.verify_release_metadata import (
    ReleaseMetadataError,
    reconcile_registry_artifacts,
    verify_release_metadata,
)


def _write_manifests(
    root: Path, python_version: str = "1.0.0", cargo_version: str | None = None
) -> None:
    cargo_version = cargo_version or python_version
    (root / "rust/ai-session-search-core").mkdir(parents=True)
    (root / "rust/ai-session-search-python").mkdir(parents=True)
    (root / "tests/rust-api-consumer").mkdir(parents=True)
    # The consumer crate stays unpublished at 0.0.0; only its requirement on the released
    # core crate belongs to the release identity.
    (root / "tests/rust-api-consumer/Cargo.toml").write_text(
        '[package]\nname = "ai-session-search-api-consumer"\nversion = "0.0.0"\npublish = false\n'
        f'[dependencies]\nai-session-search = {{ path = "../../rust/ai-session-search-core", version = "{cargo_version}" }}\n',
        encoding="utf-8",
    )
    (root / "pyproject.toml").write_text(
        f'[project]\nname = "ai-session-search"\nversion = "{python_version}"\n', encoding="utf-8"
    )
    (root / "rust/ai-session-search-core/Cargo.toml").write_text(
        f'[package]\nname = "ai-session-search"\nversion = "{cargo_version}"\n', encoding="utf-8"
    )
    (root / "rust/ai-session-search-python/Cargo.toml").write_text(
        f'''[package]\nname = "ai-session-search-python"\nversion = "{cargo_version}"\n'''
        f'''[dependencies]\nai-session-search = {{ version = "{cargo_version}", path = "../ai-session-search-core" }}\n''',
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


@pytest.mark.parametrize(
    "relative",
    ["rust/ai-session-search-python/Cargo.toml", "tests/rust-api-consumer/Cargo.toml"],
)
def test_release_metadata_rejects_a_stale_core_dependency_requirement(
    tmp_path: Path, relative: str
) -> None:
    # Cargo resolves a caret requirement of 1.0.0-rc.1 against a 1.0.0 core crate without
    # complaint, so a stale requirement survives `cargo check --locked` and only this gate
    # can report it.
    _write_manifests(tmp_path, "1.0.0", "1.0.0")
    manifest = tmp_path / relative
    manifest.write_text(
        manifest.read_text(encoding="utf-8").replace(
            'ai-session-search = { version = "1.0.0"', 'ai-session-search = { version = "1.0.0-rc.1"'
        ).replace(
            'ai-session-search = { path = "../../rust/ai-session-search-core", version = "1.0.0"',
            'ai-session-search = { path = "../../rust/ai-session-search-core", version = "1.0.0-rc.1"',
        ),
        encoding="utf-8",
    )
    with pytest.raises(ReleaseMetadataError, match=f"{relative} requires"):
        verify_release_metadata(tmp_path, "v1.0.0")


def test_release_metadata_normalizes_python_rc_to_cargo_semver(tmp_path: Path) -> None:
    _write_manifests(tmp_path, "1.0.0rc1", "1.0.0-rc.1")

    assert verify_release_metadata(tmp_path, "v1.0.0rc1") == "1.0.0rc1"

    core_manifest = tmp_path / "rust/ai-session-search-core/Cargo.toml"
    core_manifest.write_text(
        core_manifest.read_text(encoding="utf-8").replace("1.0.0-rc.1", "1.0.0-rc1"),
        encoding="utf-8",
    )
    with pytest.raises(ReleaseMetadataError, match="Cargo version"):
        verify_release_metadata(tmp_path, "v1.0.0rc1")


@pytest.mark.parametrize(
    ("python_version", "cargo_version"),
    [
        ("1.2.3", "1.2.3"),
        ("1.2.3a4", "1.2.3-alpha.4"),
        ("1.2.3b5", "1.2.3-beta.5"),
        ("1.2.3rc6", "1.2.3-rc.6"),
    ],
)
def test_release_version_mapping_uses_native_python_and_cargo_spellings(
    python_version: str, cargo_version: str
) -> None:
    assert cargo_version_for_python(python_version) == cargo_version


@pytest.mark.parametrize(
    "version", ["01.2.3", "1.2", "1.2.3-rc.1", "1.2.3.post1", "1.2.3.dev1"]
)
def test_release_version_mapping_rejects_unsupported_release_spellings(version: str) -> None:
    with pytest.raises(ValueError, match="unsupported Python release version"):
        cargo_version_for_python(version)


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
