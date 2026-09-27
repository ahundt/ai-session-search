# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

from pathlib import Path

import pytest

from scripts.release_versions import cargo_version_for_python, release_sort_key
from scripts.verify_release_metadata import (
    ReleaseMetadataError,
    check_notes_shape,
    main,
    previous_release,
    reconcile_registry_artifacts,
    release_body,
    release_notes,
    verify_release_metadata,
)


def _write_changelog(
    root: Path,
    version: str,
    *,
    heading_suffix: str = " - 2026-01-02",
    body: str = "One concrete fix.\n\n### Fixed\n\n1. A concrete change.\n",
) -> None:
    (root / "CHANGELOG.md").write_text(
        "# Changelog\n\n"
        "## [Unreleased]\n\n"
        f"## [{version}]{heading_suffix}\n\n"
        f"{body}\n"
        "## [0.9.0] - 2025-12-01\n\n"
        "First release.\n",
        encoding="utf-8",
    )


def _write_manifests(
    root: Path, python_version: str = "1.0.0", cargo_version: str | None = None
) -> None:
    cargo_version = cargo_version or python_version
    _write_changelog(root, python_version)
    (root / "skills/ai-session-search").mkdir(parents=True)
    (root / "rust/ai-session-search-core").mkdir(parents=True)
    (root / "rust/ai-session-search-core/skills/ai-session-search").mkdir(parents=True)
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
        f'[project]\nname = "ai-session-search"\nversion = "{python_version}"\n'
        '[project.urls]\nRepository = "https://github.com/example/aise"\n',
        encoding="utf-8",
    )
    (root / "rust/ai-session-search-core/Cargo.toml").write_text(
        f'[package]\nname = "ai-session-search"\nversion = "{cargo_version}"\n', encoding="utf-8"
    )
    (root / "rust/ai-session-search-core/skills/ai-session-search/SKILL.md").write_text(
        f"---\nname: ai-session-search\nmetadata:\n  version: {cargo_version}\n---\n",
        encoding="utf-8",
    )
    (root / "skills/ai-session-search/SKILL.md").write_text(
        f"---\nname: ai-session-search\nmetadata:\n  version: {cargo_version}\n---\n",
        encoding="utf-8",
    )
    (root / "rust/ai-session-search-python/Cargo.toml").write_text(
        f'''[package]\nname = "ai-session-search-python"\nversion = "{cargo_version}"\n'''
        f'''[dependencies]\nai-session-search = {{ version = "{cargo_version}", path = "../ai-session-search-core" }}\n''',
        encoding="utf-8",
    )
    (root / "docs/development").mkdir(parents=True)
    (root / "docs/development/library-api.md").write_text(
        "### Features\n\n```toml\n[dependencies]\n"
        f'ai-session-search = {{ version = "{cargo_version}", default-features = false }}\n'
        "```\n",
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


def test_release_metadata_rejects_a_stale_packaged_skill_version(tmp_path: Path) -> None:
    _write_manifests(tmp_path, "1.0.0rc1", "1.0.0-rc.1")
    skill = tmp_path / "rust/ai-session-search-core/skills/ai-session-search/SKILL.md"
    skill.write_text(
        skill.read_text(encoding="utf-8").replace("version: 1.0.0-rc.1", "version: 1.0.0-rc.0"),
        encoding="utf-8",
    )

    with pytest.raises(ReleaseMetadataError, match=r"SKILL\.md declares"):
        verify_release_metadata(tmp_path, "v1.0.0rc1")


def test_release_metadata_rejects_a_stale_repository_skill_version(tmp_path: Path) -> None:
    _write_manifests(tmp_path, "1.0.0rc1", "1.0.0-rc.1")
    skill = tmp_path / "skills/ai-session-search/SKILL.md"
    skill.write_text(
        skill.read_text(encoding="utf-8").replace("version: 1.0.0-rc.1", "version: 1.0.0-rc.0"),
        encoding="utf-8",
    )

    with pytest.raises(ReleaseMetadataError, match=r"SKILL\.md declares"):
        verify_release_metadata(tmp_path, "v1.0.0rc1")


def test_release_metadata_rejects_a_stale_documented_core_requirement(tmp_path: Path) -> None:
    # Cargo never reads a docs code block, so a copied install snippet keeps telling readers
    # to pin a superseded candidate long after every manifest moved on.
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")
    doc = tmp_path / "docs/development/library-api.md"
    doc.write_text(
        doc.read_text(encoding="utf-8").replace("1.0.0-rc.2", "1.0.0-rc.1"), encoding="utf-8"
    )

    with pytest.raises(ReleaseMetadataError, match=r"library-api\.md documents"):
        verify_release_metadata(tmp_path, "v1.0.0rc2")


def test_release_metadata_rejects_a_removed_documented_core_requirement(tmp_path: Path) -> None:
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")
    (tmp_path / "docs/development/library-api.md").write_text(
        "### Features\n\nNo snippet here.\n", encoding="utf-8"
    )

    with pytest.raises(ReleaseMetadataError, match="documents no ai-session-search version"):
        verify_release_metadata(tmp_path, "v1.0.0rc2")


def test_release_metadata_rejects_a_tag_the_changelog_does_not_describe(tmp_path: Path) -> None:
    # 1.0.0rc1 shipped with no release notes at all. The pre-tag checklist bullet that
    # replaced that habit is a line a human reads, so only this gate reports skipping it.
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")
    _write_changelog(tmp_path, "1.0.0rc1")

    with pytest.raises(ReleaseMetadataError, match=r"CHANGELOG\.md has no"):
        verify_release_metadata(tmp_path, "v1.0.0rc2")


def test_release_metadata_rejects_a_changelog_section_left_undated(tmp_path: Path) -> None:
    # A heading renamed off `## [Unreleased]` without its date means the pre-tag step
    # stopped halfway, and the published notes would carry no release date.
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")
    _write_changelog(tmp_path, "1.0.0rc2", heading_suffix="")

    with pytest.raises(ReleaseMetadataError, match="needs the release date"):
        verify_release_metadata(tmp_path, "v1.0.0rc2")


def test_release_metadata_rejects_an_empty_changelog_section(tmp_path: Path) -> None:
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")
    _write_changelog(tmp_path, "1.0.0rc2", body="")

    with pytest.raises(ReleaseMetadataError, match="describes no changes"):
        verify_release_metadata(tmp_path, "v1.0.0rc2")


@pytest.mark.parametrize("release_date", ["2026-02-30", "2026-99-99", "0000-00-00"])
def test_release_metadata_rejects_an_impossible_changelog_date(
    tmp_path: Path, release_date: str
) -> None:
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")
    _write_changelog(tmp_path, "1.0.0rc2", heading_suffix=f" - {release_date}")

    with pytest.raises(ReleaseMetadataError, match="valid ISO calendar date"):
        verify_release_metadata(tmp_path, "v1.0.0rc2")


def test_release_metadata_rejects_duplicate_version_sections(tmp_path: Path) -> None:
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")
    changelog = tmp_path / "CHANGELOG.md"
    changelog.write_text(
        changelog.read_text(encoding="utf-8")
        + "\n## [1.0.0rc2] - 2026-01-03\n\nA conflicting second body.\n",
        encoding="utf-8",
    )

    with pytest.raises(ReleaseMetadataError, match=r"more than one.*1\.0\.0rc2"):
        verify_release_metadata(tmp_path, "v1.0.0rc2")


def test_previous_release_is_the_dated_section_below_the_version(tmp_path: Path) -> None:
    _write_changelog(tmp_path, "1.0.0rc2")

    assert previous_release(tmp_path, "1.0.0rc2") == "0.9.0"
    assert previous_release(tmp_path, "0.9.0") is None
    assert previous_release(tmp_path, "2.0.0") is None


@pytest.mark.parametrize(
    ("notes", "complaint"),
    [
        ("### Fixed\n\n1. A fix.\n", "must open with a short summary"),
        ("Summary.\n\n### Upgrading from 1.0.0rc1\n\n1. Rename a key.\n", "no upgrade section"),
        ("Summary.\n\n### Fixed\n\n1. A.\n\n### Added\n\n1. B.\n", "in the order"),
        ("Summary.\n\n### Fixed\n\n- A fix.\n", "number the items"),
        ("Summary.\n\n### Fixed\n\n1. A fix:\n   + nested.\n", "number the items"),
        ("2) Starts with a list.\n", "must open with a short summary"),
        ("Summary.\n\n```toml\nx = 1\n\n### Bogus\n\n- hidden\n", "never closes"),
    ],
)
def test_release_notes_shape_is_enforced(notes: str, complaint: str) -> None:
    # 1.0.0rc3's notes opened with design rationale, buried six upgrade steps across sections,
    # and put build internals under Fixed. The shape is what a reader deciding to upgrade needs.
    with pytest.raises(ReleaseMetadataError, match=complaint):
        check_notes_shape(notes, "1.0.0rc2")


def test_release_notes_shape_ignores_fenced_code() -> None:
    check_notes_shape("Summary.\n\n### Changed\n\n1. Now:\n\n```toml\n- not a list\n```\n", "1.0.0")
    check_notes_shape("Summary.\n\n### Changed\n\n1. Now:\n\n   ~~~\n   - not a list\n   ~~~\n", "1.0.0")


def test_release_body_appends_install_guidance_for_the_exact_version(tmp_path: Path) -> None:
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")

    body = release_body(tmp_path, "1.0.0rc2")

    assert body.startswith(release_notes(tmp_path, "1.0.0rc2"))
    assert "uv tool install ai-session-search==1.0.0rc2" in body
    assert "cargo install ai-session-search --locked --version 1.0.0-rc.2" in body
    assert "https://github.com/example/aise/compare/v0.9.0...v1.0.0rc2" in body
    assert "After a first install, run `aise integrations install`" in body


def test_notes_only_renders_a_published_tag_without_the_version_checks(tmp_path: Path) -> None:
    # Revising an earlier release's body happens after the manifests moved to the next version,
    # which the full gate would reject.
    _write_manifests(tmp_path, "1.0.0rc3", "1.0.0-rc.3")
    _write_changelog(tmp_path, "1.0.0rc2")
    out = tmp_path / "notes.md"

    assert main(["--root", str(tmp_path), "--tag", "v1.0.0rc2", "--notes-only", "--notes-out", str(out)]) == 0
    assert out.read_text(encoding="utf-8") == release_body(tmp_path, "1.0.0rc2")


def test_release_notes_return_one_version_section(tmp_path: Path) -> None:
    _write_manifests(tmp_path, "1.0.0rc2", "1.0.0-rc.2")

    notes = release_notes(tmp_path, "1.0.0rc2")

    assert notes == "One concrete fix.\n\n### Fixed\n\n1. A concrete change.\n"
    assert "Unreleased" not in notes
    assert "First release." not in notes


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


def test_release_versions_sort_the_way_pep_440_orders_them() -> None:
    versions = ["1.0.1", "1.0.0", "1.0.0rc10", "1.0.0rc2", "1.0.0b1", "1.0.0a3"]

    assert sorted(versions, key=release_sort_key) == ["1.0.0a3", "1.0.0b1", "1.0.0rc2", "1.0.0rc10", "1.0.0", "1.0.1"]
