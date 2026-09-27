#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Validate release versions and classify retry state without mutating registries."""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import tomllib
from collections.abc import Mapping
from datetime import date

from scripts.release_versions import cargo_version_for_python

CORE_CRATE = "ai-session-search"
# Every workspace member that names an explicit version for the published crate. A caret
# requirement such as "1.0.0-rc.1" keeps resolving after the core crate moves to 1.0.0, so
# Cargo never reports the drift and only this gate can.
CORE_DEPENDENT_MANIFESTS = (
    "rust/ai-session-search-python/Cargo.toml",
    "tests/rust-api-consumer/Cargo.toml",
)
RELEASE_SKILLS = (
    "skills/ai-session-search/SKILL.md",
    "rust/ai-session-search-core/skills/ai-session-search/SKILL.md",
)
# Documents that publish a copy-and-paste Cargo requirement on the released crate. Cargo never
# reads a code block, so a stale snippet keeps telling readers to pin a superseded candidate
# after every manifest has moved on, and only this gate reports it.
RELEASE_DOC_REQUIREMENTS = ("docs/development/library-api.md",)
_DOCUMENTED_REQUIREMENT = re.compile(
    re.escape(CORE_CRATE) + r'\s*=\s*\{[^}]*\bversion\s*=\s*"([^"]+)"'
)
# The changelog section for the released version is that release's notes, so the release
# job publishes it verbatim. Keep a Changelog spells a released section as
# "## [1.2.3] - 2026-01-02"; "## [Unreleased]" carries no date and never matches a tag.
CHANGELOG = "CHANGELOG.md"
_CHANGELOG_SECTION = re.compile(r"^## \[(?P<version>[^]]+)\](?: - (?P<date>\d{4}-\d{2}-\d{2}))?\s*$")


# A released section reads top to bottom for someone deciding whether and how to upgrade: a short
# summary, then Keep a Changelog headings in this order. "Highlights" names the few changes most
# readers came for; "For contributors" keeps build and test work out of the user-facing sections.
# There is no upgrading heading: upgrading is one command, and the generated footer gives it.
NOTES_HEADINGS = (
    "Highlights",
    "Added",
    "Changed",
    "Deprecated",
    "Removed",
    "Fixed",
    "Security",
    "For contributors",
)


class ReleaseMetadataError(ValueError):
    """Release metadata or observed registry state is inconsistent."""


def _manifest(path: pathlib.Path) -> dict[str, object]:
    with path.open("rb") as source:
        return tomllib.load(source)


def _core_dependency_version(manifest: Mapping[str, object], relative: str) -> str:
    dependencies = manifest.get("dependencies")
    if not isinstance(dependencies, Mapping) or CORE_CRATE not in dependencies:
        raise ReleaseMetadataError(f"{relative} does not depend on {CORE_CRATE}")
    dependency = dependencies[CORE_CRATE]
    version = dependency.get("version") if isinstance(dependency, Mapping) else dependency
    if not isinstance(version, str):
        raise ReleaseMetadataError(f"{relative} depends on {CORE_CRATE} without a version")
    return version


def _skill_version(path: pathlib.Path, relative: str) -> str:
    in_metadata = False
    for line in path.read_text(encoding="utf-8").splitlines():
        if line == "metadata:":
            in_metadata = True
            continue
        if in_metadata and line and not line[0].isspace():
            break
        if in_metadata:
            key, separator, value = line.strip().partition(":")
            if separator and key == "version" and value.strip():
                return value.strip().strip("'\"")
    raise ReleaseMetadataError(f"{relative} has no metadata.version")


def _verify_documented_requirements(root: pathlib.Path, cargo_version: str) -> None:
    for relative in RELEASE_DOC_REQUIREMENTS:
        text = (root / relative).read_text(encoding="utf-8")
        documented = _DOCUMENTED_REQUIREMENT.findall(text)
        if not documented:
            raise ReleaseMetadataError(f"{relative} documents no {CORE_CRATE} version requirement")
        stale = sorted({version for version in documented if version != cargo_version})
        if stale:
            raise ReleaseMetadataError(
                f"{relative} documents {CORE_CRATE} {', '.join(repr(v) for v in stale)} "
                f"instead of the release version {cargo_version!r}"
            )


def release_notes(root: pathlib.Path, version: str) -> str:
    """Return the one valid dated changelog body describing ``version``.

    The section runs from its own heading to the next one, so what a reader sees in the file is
    what the release publishes. A tag is permanent: missing, duplicate, undated, impossible-date,
    or empty sections are reported here rather than discovered on the published release.
    """
    lines = (root / CHANGELOG).read_text(encoding="utf-8").splitlines()
    headings = [
        (index, section)
        for index, line in enumerate(lines)
        if (section := _CHANGELOG_SECTION.match(line)) is not None
    ]
    matches = [(index, section) for index, section in headings if section["version"] == version]
    if not matches:
        raise ReleaseMetadataError(
            f"{CHANGELOG} has no '## [{version}]' section; rename '## [Unreleased]' to "
            f"'## [{version}] - YYYY-MM-DD', open a fresh unreleased heading above it, and "
            "point the link definitions at the new tag"
        )
    if len(matches) != 1:
        raise ReleaseMetadataError(
            f"{CHANGELOG} has more than one '## [{version}]' section; each release version "
            "must have exactly one body"
        )

    start, section = matches[0]
    release_date = section["date"]
    if release_date is None:
        raise ReleaseMetadataError(
            f"{CHANGELOG} section '## [{version}]' needs the release date, "
            f"written as '## [{version}] - YYYY-MM-DD'"
        )
    try:
        date.fromisoformat(release_date)
    except ValueError as error:
        raise ReleaseMetadataError(
            f"{CHANGELOG} section '## [{version}]' needs a valid ISO calendar date; "
            f"found {release_date!r}"
        ) from error

    end = next((index for index, _ in headings if index > start), len(lines))
    notes = "\n".join(lines[start + 1 : end]).strip()
    if not notes:
        raise ReleaseMetadataError(f"{CHANGELOG} section '## [{version}]' describes no changes")
    return notes + "\n"


def check_notes_shape(notes: str, version: str) -> None:
    """Reject a released section that does not open with a summary or strays from the headings."""
    lines = []
    fenced = False
    for line in notes.splitlines():
        if line.startswith("```"):
            fenced = not fenced
        elif not fenced:
            lines.append(line)
    where = f"{CHANGELOG} section '## [{version}]'"
    first = next((line for line in lines if line.strip()), "")
    if first.startswith(("#", "- ", "* ", "1. ")):
        raise ReleaseMetadataError(
            f"{where} must open with a short summary paragraph of what the release means for a "
            "user, before its first heading or list"
        )
    headings = [line[4:].strip() for line in lines if line.startswith("### ")]
    unknown = [heading for heading in headings if heading not in NOTES_HEADINGS]
    if unknown:
        raise ReleaseMetadataError(
            f"{where} uses {unknown}; release headings are {list(NOTES_HEADINGS)}. Upgrading is "
            "one command, which the generated footer gives, so there is no upgrade section"
        )
    order = [NOTES_HEADINGS.index(heading) for heading in headings]
    if order != sorted(order) or len(set(order)) != len(order):
        raise ReleaseMetadataError(
            f"{where} headings must appear once each in the order {list(NOTES_HEADINGS)}"
        )
    bullets = [line for line in lines if line.startswith(("- ", "* "))]
    if bullets:
        raise ReleaseMetadataError(
            f"{where} uses bullet lists; number the items so a reader can cite one: {bullets[0]!r}"
        )


def release_body(root: pathlib.Path, version: str) -> str:
    """Return the GitHub Release body: the changelog section plus generated install guidance.

    The footer is derived from the version, so the commands in it cannot drift from the release
    they describe and no author has to write them.
    """
    notes = release_notes(root, version)
    check_notes_shape(notes, version)
    cargo_version = cargo_version_for_python(version)
    repository = str(_manifest(root / "pyproject.toml")["project"]["urls"]["Repository"])  # type: ignore[index]
    previous = previous_release(root, version)
    diff = (
        f"[v{previous}...v{version}]({repository}/compare/v{previous}...v{version})"
        if previous is not None
        else f"[v{version}]({repository}/tree/v{version})"
    )
    return (
        f"{notes}\n"
        "---\n\n"
        "Upgrade with `aise package update`, which uses whichever package manager installed "
        "`aise`.\n\n"
        "Install:\n\n"
        "```bash\n"
        f"uv tool install ai-session-search=={version}\n"
        f"python -m pip install ai-session-search=={version}\n"
        f"cargo install ai-session-search --locked --version {cargo_version}\n"
        "```\n\n"
        "Then run `aise integrations install` once to connect your AI tools.\n\n"
        "| To get | Download |\n"
        "| --- | --- |\n"
        f"| The `aise` executable alone | `ai-session-search-{version}-<target>.tar.gz`, or `.zip` "
        "on Windows |\n"
        f"| The Python package, offline | `ai_session_search-{version}-cp312-abi3-<platform>.whl` |\n"
        f"| Source | `ai_session_search-{version}.tar.gz` or the `.crate` |\n\n"
        "Check a download with `shasum -a 256 -c SHA256SUMS --ignore-missing`. The `*.cdx.json` "
        "SBOMs and the license files are for audits.\n\n"
        f"Full diff: {diff}\n"
    )


def previous_release(root: pathlib.Path, version: str) -> str | None:
    """Return the dated release immediately below ``version`` in the changelog, if any."""
    versions = [
        section["version"]
        for line in (root / CHANGELOG).read_text(encoding="utf-8").splitlines()
        if (section := _CHANGELOG_SECTION.match(line)) is not None and section["date"] is not None
    ]
    if version not in versions:
        return None
    index = versions.index(version)
    return versions[index + 1] if index + 1 < len(versions) else None


def verify_release_metadata(root: pathlib.Path, tag: str) -> str:
    project = _manifest(root / "pyproject.toml")
    core = _manifest(root / "rust/ai-session-search-core/Cargo.toml")
    python = _manifest(root / "rust/ai-session-search-python/Cargo.toml")
    version = str(project["project"]["version"])  # type: ignore[index]
    try:
        cargo_version = cargo_version_for_python(version)
    except ValueError as error:
        raise ReleaseMetadataError(str(error)) from error
    cargo_versions = {
        str(core["package"]["version"]),  # type: ignore[index]
        str(python["package"]["version"]),  # type: ignore[index]
    }
    if cargo_versions != {cargo_version}:
        raise ReleaseMetadataError(
            f"Cargo versions differ; expected {cargo_version!r} "
            f"for Python version {version!r}; "
            f"found {sorted(cargo_versions)}"
        )
    if tag != f"v{version}":
        raise ReleaseMetadataError(f"release tag {tag!r} must equal canonical v{version}")
    for relative in CORE_DEPENDENT_MANIFESTS:
        dependency_version = _core_dependency_version(_manifest(root / relative), relative)
        if dependency_version != cargo_version:
            raise ReleaseMetadataError(
                f"{relative} requires {CORE_CRATE} {dependency_version!r} "
                f"instead of the release version {cargo_version!r}"
            )
    for relative in RELEASE_SKILLS:
        skill_version = _skill_version(root / relative, relative)
        if skill_version != cargo_version:
            raise ReleaseMetadataError(
                f"{relative} declares {skill_version!r} "
                f"instead of the release version {cargo_version!r}"
            )
    _verify_documented_requirements(root, cargo_version)
    release_body(root, version)
    return version


def reconcile_registry_artifacts(
    expected: Mapping[str, str], observed: Mapping[str, str]
) -> str:
    if not observed:
        return "publish"
    expected_names = set(expected)
    observed_names = set(observed)
    if expected_names != observed_names:
        raise ReleaseMetadataError(
            "registry contains a partial or unexpected artifact set; "
            f"missing={sorted(expected_names - observed_names)}, "
            f"extra={sorted(observed_names - expected_names)}"
        )
    mismatches = sorted(name for name in expected if expected[name] != observed[name])
    if mismatches:
        raise ReleaseMetadataError(f"registry checksum mismatch for: {', '.join(mismatches)}")
    return "already-published"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path.cwd())
    parser.add_argument("--tag", required=True)
    parser.add_argument(
        "--notes-out",
        type=pathlib.Path,
        help="write the GitHub Release body here: the changelog section and generated install guidance",
    )
    parser.add_argument(
        "--notes-only",
        action="store_true",
        help="check and render the notes for an already published tag without the version checks, "
        "to revise its release body with `gh release edit --notes-file`",
    )
    args = parser.parse_args(argv)
    try:
        if args.notes_only:
            version = args.tag.removeprefix("v")
            body = release_body(args.root, version)
        else:
            version = verify_release_metadata(args.root, args.tag)
            body = release_body(args.root, version)
        if args.notes_out is not None:
            args.notes_out.write_text(body, encoding="utf-8")
        print(version)
    except (KeyError, OSError, ReleaseMetadataError, tomllib.TOMLDecodeError) as error:
        print(f"release metadata verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
