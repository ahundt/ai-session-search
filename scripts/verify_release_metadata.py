#!/usr/bin/env python3
"""Validate release versions and classify retry state without mutating registries."""

from __future__ import annotations

import argparse
import pathlib
import sys
import tomllib
from collections.abc import Mapping

from scripts.release_versions import cargo_version_for_python


class ReleaseMetadataError(ValueError):
    """Release metadata or observed registry state is inconsistent."""


def _manifest(path: pathlib.Path) -> dict[str, object]:
    with path.open("rb") as source:
        return tomllib.load(source)


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
    dependency = python["dependencies"]["ai-session-search"]  # type: ignore[index]
    dependency_version = dependency.get("version") if isinstance(dependency, dict) else dependency
    if dependency_version != cargo_version:
        raise ReleaseMetadataError(
            f"PyO3 core dependency version {dependency_version!r} differs from {cargo_version!r}"
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
