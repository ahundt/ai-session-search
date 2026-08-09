#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Validate release versions and classify retry state without mutating registries."""

from __future__ import annotations

import argparse
import pathlib
import sys
import tomllib
from collections.abc import Mapping

from scripts.release_versions import cargo_version_for_python

CORE_CRATE = "ai-session-search"
# Every workspace member that names an explicit version for the published crate. A caret
# requirement such as "1.0.0-rc.1" keeps resolving after the core crate moves to 1.0.0, so
# Cargo never reports the drift and only this gate can.
CORE_DEPENDENT_MANIFESTS = (
    "rust/ai-session-search-python/Cargo.toml",
    "tests/rust-api-consumer/Cargo.toml",
)
PACKAGED_SKILL = "rust/ai-session-search-core/skills/ai-session-search/SKILL.md"


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


def _packaged_skill_version(path: pathlib.Path) -> str:
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
    raise ReleaseMetadataError(f"{PACKAGED_SKILL} has no metadata.version")


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
    skill_version = _packaged_skill_version(root / PACKAGED_SKILL)
    if skill_version != cargo_version:
        raise ReleaseMetadataError(
            f"{PACKAGED_SKILL} declares {skill_version!r} "
            f"instead of the release version {cargo_version!r}"
        )
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
    args = parser.parse_args(argv)
    try:
        print(verify_release_metadata(args.root, args.tag))
    except (KeyError, OSError, ReleaseMetadataError, tomllib.TOMLDecodeError) as error:
        print(f"release metadata verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
