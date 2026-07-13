#!/usr/bin/env python3
"""Verify release archives before they can be promoted or published."""

from __future__ import annotations

import argparse
import configparser
import email.parser
import pathlib
import re
import sys
import tarfile
import zipfile
from collections.abc import Iterable

EXPECTED_DISTRIBUTION = "ai-session-search"
EXPECTED_LICENSE = "Apache-2.0"
FORBIDDEN_SUFFIXES = {".cast", ".gif", ".mp4", ".webm"}
FORBIDDEN_PARTS = {"ai_session_tools", "sessiongrep"}


class VerificationError(ValueError):
    """An artifact violates a release invariant."""


def _normalized_parts(name: str) -> tuple[str, ...]:
    normalized = name.replace("\\", "/")
    path = pathlib.PurePosixPath(normalized)
    if path.is_absolute() or re.match(r"^[A-Za-z]:/", normalized) or ".." in path.parts:
        raise VerificationError(f"unsafe archive path: {name!r}")
    return tuple(part for part in path.parts if part not in {"", "."})


def _verify_names(names: Iterable[str]) -> list[tuple[str, ...]]:
    materialized = list(names)
    parts = [_normalized_parts(name) for name in materialized]
    for original, item in zip(materialized, parts, strict=True):
        lowered = tuple(part.lower() for part in item)
        if pathlib.PurePosixPath(original).suffix.lower() in FORBIDDEN_SUFFIXES:
            raise VerificationError(f"embedded demo/media file: {original}")
        if FORBIDDEN_PARTS.intersection(lowered):
            raise VerificationError(f"stale package identity: {original}")
    return parts


def _has_basename(parts: Iterable[tuple[str, ...]], name: str) -> bool:
    return any(item and item[-1] == name for item in parts)


def _has_suffix(parts: Iterable[tuple[str, ...]], suffix: tuple[str, ...]) -> bool:
    return any(len(item) >= len(suffix) and item[-len(suffix) :] == suffix for item in parts)


def _parse_metadata(raw: bytes, artifact: pathlib.Path) -> None:
    metadata = email.parser.BytesParser().parsebytes(raw)
    if metadata.get("Name") != EXPECTED_DISTRIBUTION:
        raise VerificationError(f"{artifact.name}: unexpected Name metadata")
    if not metadata.get("Version"):
        raise VerificationError(f"{artifact.name}: missing Version metadata")
    license_expression = metadata.get("License-Expression") or metadata.get("License")
    if license_expression != EXPECTED_LICENSE:
        raise VerificationError(f"{artifact.name}: unexpected license metadata")


def verify_wheel(path: pathlib.Path) -> None:
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        parts = _verify_names(names)
        metadata_names = [name for name in names if name.endswith(".dist-info/METADATA")]
        if len(metadata_names) != 1:
            raise VerificationError(f"{path.name}: expected exactly one METADATA file")
        _parse_metadata(archive.read(metadata_names[0]), path)
        entry_point_names = [name for name in names if name.endswith(".dist-info/entry_points.txt")]
        if len(entry_point_names) != 1:
            raise VerificationError(f"{path.name}: expected exactly one entry_points.txt file")
        entry_points = configparser.ConfigParser()
        entry_points.read_string(archive.read(entry_point_names[0]).decode("utf-8"))
        if not entry_points.has_option("console_scripts", "aise"):
            raise VerificationError(f"{path.name}: missing aise console entry point")

    required = {
        "LICENSE",
        "NOTICE",
        "__init__.py",
        "_native.pyi",
        "native.pyi",
        "py.typed",
    }
    missing = sorted(name for name in required if not _has_basename(parts, name))
    if missing:
        raise VerificationError(f"{path.name}: missing required files: {', '.join(missing)}")
    native_pattern = re.compile(r"^_native(?:\.[^.]+)*\.(?:so|pyd|dylib)$")
    if not any(item and native_pattern.match(item[-1]) for item in parts):
        raise VerificationError(f"{path.name}: missing native extension module")


def verify_sdist(path: pathlib.Path) -> None:
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        parts = _verify_names(member.name for member in members)
        if any(member.issym() or member.islnk() for member in members):
            raise VerificationError(f"{path.name}: links are forbidden in source distributions")
        metadata_members = [member for member in members if member.name.endswith("/PKG-INFO")]
        if len(metadata_members) != 1:
            raise VerificationError(f"{path.name}: expected exactly one PKG-INFO file")
        extracted = archive.extractfile(metadata_members[0])
        if extracted is None:
            raise VerificationError(f"{path.name}: unreadable PKG-INFO")
        _parse_metadata(extracted.read(), path)

    required_paths = (
        ("LICENSE",),
        ("NOTICE",),
        ("Cargo.lock",),
        ("Cargo.toml",),
        ("pyproject.toml",),
        ("ai_session_search", "__init__.py"),
        ("ai_session_search", "_native.pyi"),
        ("rust", "ai-session-search-core", "src", "lib.rs"),
        ("rust", "ai-session-search-core", "Cargo.toml"),
        ("rust", "ai-session-search-python", "src", "lib.rs"),
        ("rust", "ai-session-search-python", "Cargo.toml"),
    )
    for required in required_paths:
        if not _has_suffix(parts, required):
            raise VerificationError(f"{path.name}: missing required path: {'/'.join(required)}")


def verify(path: pathlib.Path) -> None:
    if path.suffix == ".whl":
        verify_wheel(path)
    elif path.name.endswith(".tar.gz"):
        verify_sdist(path)
    else:
        raise VerificationError(f"unsupported artifact type: {path.name}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", nargs="+", type=pathlib.Path)
    args = parser.parse_args(argv)
    try:
        for artifact in args.artifacts:
            verify(artifact)
            print(f"verified: {artifact}")
    except (
        OSError,
        UnicodeError,
        configparser.Error,
        VerificationError,
        tarfile.TarError,
        zipfile.BadZipFile,
    ) as error:
        print(f"release artifact verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
