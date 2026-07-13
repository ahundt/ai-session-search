#!/usr/bin/env python3
"""Validate release versions and classify retry state without mutating registries."""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import tomllib
from collections.abc import Mapping

CANONICAL_VERSION = re.compile(r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)")


class ReleaseMetadataError(ValueError):
    """Release metadata or observed registry state is inconsistent."""


def _manifest(path: pathlib.Path) -> dict[str, object]:
    with path.open("rb") as source:
        return tomllib.load(source)


def verify_release_metadata(root: pathlib.Path, tag: str) -> str:
    project = _manifest(root / "pyproject.toml")
    core = _manifest(root / "rust/ai-session-search-core/Cargo.toml")
    python = _manifest(root / "rust/ai-session-search-python/Cargo.toml")
    versions = {
        str(project["project"]["version"]),  # type: ignore[index]
        str(core["package"]["version"]),  # type: ignore[index]
        str(python["package"]["version"]),  # type: ignore[index]
    }
    if len(versions) != 1:
        raise ReleaseMetadataError(f"package versions differ: {sorted(versions)}")
    version = next(iter(versions))
    if CANONICAL_VERSION.fullmatch(version) is None or tag != f"v{version}":
        raise ReleaseMetadataError(f"release tag {tag!r} must equal canonical v{version}")
    dependency = python["dependencies"]["ai-session-search"]  # type: ignore[index]
    dependency_version = dependency.get("version") if isinstance(dependency, dict) else dependency
    if dependency_version != version:
        raise ReleaseMetadataError(
            f"PyO3 core dependency version {dependency_version!r} differs from {version!r}"
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
