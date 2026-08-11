#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Verify release archives before they can be promoted or published."""

from __future__ import annotations

import argparse
import configparser
import datetime as dt
import email.parser
import hashlib
import json
import pathlib
import re
import stat
import sys
import tarfile
import zipfile
from collections import Counter
from collections.abc import Iterable

from scripts.release_versions import cargo_version_for_python

EXPECTED_DISTRIBUTION = "ai-session-search"
EXPECTED_LICENSE = "Apache-2.0"
EXPECTED_CONSOLE_SCRIPTS = {"aise": "ai_session_search.entrypoint:cli_main"}
NATIVE_RECEIPT_NAME = "aise-native-install.json"
EXPECTED_WHEEL_ABI_PREFIX = "cp312-abi3-"
FORBIDDEN_SUFFIXES = {".cast", ".gif", ".mp4", ".webm"}
FORBIDDEN_PARTS = {"ai_session_tools", "sessiongrep"}
FORBIDDEN_CARGO_BINARY = 'name = "aise-mcp"'
NATIVE_ARCHIVE_PATTERN = re.compile(
    r"^(?P<root>ai-session-search-[A-Za-z0-9][A-Za-z0-9._-]*-[A-Za-z0-9][A-Za-z0-9._-]*)\.(?:tar\.gz|zip)$"
)
CRATE_PATTERN = re.compile(r"^ai-session-search-(?P<version>[0-9][A-Za-z0-9.+-]*)\.crate$")
# PEP 770 places SBOMs here. maturin writes one CycloneDX document per wheel.
WHEEL_SBOM_PATTERN = re.compile(r"\.dist-info/sboms/[^/]+\.json$")
EXPECTED_NATIVE_TARGETS = {
    "x86_64-unknown-linux-gnu": "tar.gz",
    "aarch64-unknown-linux-gnu": "tar.gz",
    "aarch64-apple-darwin": "tar.gz",
    "x86_64-apple-darwin": "tar.gz",
    "x86_64-pc-windows-msvc": "zip",
}
EXPECTED_WHEEL_PLATFORMS = {
    "manylinux-x86_64",
    "manylinux-aarch64",
    "macos-arm64",
    "macos-x86_64",
    "windows-x86_64",
}


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
    duplicates = sorted(name for name, count in Counter(materialized).items() if count > 1)
    if duplicates:
        raise VerificationError(f"duplicate archive member: {duplicates[0]}")
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


def _parse_metadata(raw: bytes, artifact: pathlib.Path) -> str:
    metadata = email.parser.BytesParser().parsebytes(raw)
    if metadata.get("Name") != EXPECTED_DISTRIBUTION:
        raise VerificationError(f"{artifact.name}: unexpected Name metadata")
    version = metadata.get("Version")
    if not version:
        raise VerificationError(f"{artifact.name}: missing Version metadata")
    license_expression = metadata.get("License-Expression") or metadata.get("License")
    if license_expression != EXPECTED_LICENSE:
        raise VerificationError(f"{artifact.name}: unexpected license metadata")
    return version


def _verify_wheel_tags(
    path: pathlib.Path, archive: zipfile.ZipFile, names: list[str]
) -> None:
    wheel_names = [name for name in names if name.endswith(".dist-info/WHEEL")]
    if len(wheel_names) != 1:
        raise VerificationError(f"{path.name}: expected exactly one WHEEL file")
    wheel_metadata = email.parser.BytesParser().parsebytes(archive.read(wheel_names[0]))
    tags = wheel_metadata.get_all("Tag", [])
    if not tags or any(not tag.startswith(EXPECTED_WHEEL_ABI_PREFIX) for tag in tags):
        raise VerificationError(
            f"{path.name}: expected only CPython 3.12+ abi3 wheel tags, got {tags!r}"
        )
    filename_parts = path.name.removesuffix(".whl").split("-")
    if len(filename_parts) >= 5:
        python_tag, abi_tag, platform_tag = filename_parts[-3:]
        filename_tags = {
            f"{python}-{abi}-{platform}"
            for python in python_tag.split(".")
            for abi in abi_tag.split(".")
            for platform in platform_tag.split(".")
        }
        if set(tags) != filename_tags:
            raise VerificationError(
                f"{path.name}: filename tags {sorted(filename_tags)!r} "
                f"differ from WHEEL tags {tags!r}"
            )


def verify_wheel(path: pathlib.Path) -> None:
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        parts = _verify_names(names)
        for member in archive.infolist():
            mode = member.external_attr >> 16
            file_type = stat.S_IFMT(mode)
            if not member.is_dir() and file_type not in {0, stat.S_IFREG}:
                raise VerificationError(f"{path.name}: wheel may contain only regular files")
        metadata_names = [name for name in names if name.endswith(".dist-info/METADATA")]
        if len(metadata_names) != 1:
            raise VerificationError(f"{path.name}: expected exactly one METADATA file")
        metadata_version = _parse_metadata(archive.read(metadata_names[0]), path)
        filename_parts = path.name.removesuffix(".whl").split("-")
        if len(filename_parts) >= 5 and metadata_version != filename_parts[-4]:
            raise VerificationError(
                f"{path.name}: metadata version {metadata_version!r} differs from filename"
            )
        entry_point_names = [name for name in names if name.endswith(".dist-info/entry_points.txt")]
        if len(entry_point_names) != 1:
            raise VerificationError(f"{path.name}: expected exactly one entry_points.txt file")
        entry_points = configparser.ConfigParser()
        entry_points.read_string(archive.read(entry_point_names[0]).decode("utf-8"))
        console_scripts = dict(entry_points.items("console_scripts"))
        if console_scripts != EXPECTED_CONSOLE_SCRIPTS:
            raise VerificationError(
                f"{path.name}: expected only {EXPECTED_CONSOLE_SCRIPTS!r}, got {console_scripts!r}"
            )
        _verify_wheel_tags(path, archive, names)

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


def verify_wheel_build_clock(path: pathlib.Path, source_date_epoch: int) -> None:
    """Require every embedded SBOM to prove the build observed the pinned clock.

    maturin stamps each PEP 770 SBOM with the wall clock and a fresh serialNumber unless
    SOURCE_DATE_EPOCH reaches the build. The manylinux wheels build inside a container
    that does not inherit the runner environment, so the workflow can export the variable
    and the container can still never receive it. Nothing else in the pipeline notices:
    the wheel installs, passes every other check, and simply cannot be rebuilt.

    Measured with maturin 1.14.1 by building one commit both ways: with the variable
    exported, metadata.timestamp is exactly that epoch and serialNumber is absent;
    without it, the timestamp is the build clock and serialNumber is a fresh UUID. Those
    two fields were the whole byte difference between the v1.0.0rc1 production and
    TestPyPI wheels, whose 13 other zip entries matched.
    """
    expected = dt.datetime.fromtimestamp(source_date_epoch, tz=dt.UTC)
    with zipfile.ZipFile(path) as archive:
        names = sorted(name for name in archive.namelist() if WHEEL_SBOM_PATTERN.search(name))
        if not names:
            raise VerificationError(
                f"{path.name}: no embedded SBOM, so nothing records whether the build "
                f"observed SOURCE_DATE_EPOCH={source_date_epoch}"
            )
        for name in names:
            try:
                document = json.loads(archive.read(name))
            except json.JSONDecodeError as error:
                raise VerificationError(f"{path.name}: {name} is not valid JSON: {error}") from error
            metadata = document.get("metadata") if isinstance(document, dict) else None
            recorded = metadata.get("timestamp") if isinstance(metadata, dict) else None
            if not isinstance(recorded, str):
                raise VerificationError(f"{path.name}: {name} has no metadata.timestamp")
            try:
                observed = dt.datetime.fromisoformat(recorded)
            except ValueError as error:
                raise VerificationError(
                    f"{path.name}: {name} has an unparseable metadata.timestamp {recorded!r}"
                ) from error
            if observed.tzinfo is None:
                observed = observed.replace(tzinfo=dt.UTC)
            if observed != expected:
                raise VerificationError(
                    f"{path.name}: {name} records build time {recorded!r} instead of "
                    f"SOURCE_DATE_EPOCH={source_date_epoch} ({expected.isoformat()}). "
                    "The build never received the pinned clock, so this wheel cannot be "
                    "reproduced from its commit."
                )
            if "serialNumber" in document:
                raise VerificationError(
                    f"{path.name}: {name} carries serialNumber {document['serialNumber']!r}. "
                    "maturin omits it when the build observes SOURCE_DATE_EPOCH, and a "
                    "per-build UUID changes the wheel's bytes on its own."
                )


def _verify_sdist_identity(
    path: pathlib.Path, parts: list[tuple[str, ...]], metadata_version: str
) -> None:
    filename_match = re.fullmatch(r"ai_session_search-(?P<version>[^/]+)\.tar\.gz", path.name)
    if filename_match is None:
        return
    expected_version = filename_match.group("version")
    expected_root = f"ai_session_search-{expected_version}"
    roots = {item[0] for item in parts if item}
    if roots != {expected_root}:
        raise VerificationError(
            f"{path.name}: archive root {sorted(roots)!r} differs from {expected_root!r}"
        )
    if metadata_version != expected_version:
        raise VerificationError(
            f"{path.name}: metadata version {metadata_version!r} differs from filename"
        )


def verify_sdist(path: pathlib.Path) -> None:
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        parts = _verify_names(member.name for member in members)
        if any(not (member.isfile() or member.isdir()) for member in members):
            raise VerificationError(
                f"{path.name}: source distribution may contain only regular files and directories"
            )
        metadata_members = [member for member in members if member.name.endswith("/PKG-INFO")]
        if len(metadata_members) != 1:
            raise VerificationError(f"{path.name}: expected exactly one PKG-INFO file")
        extracted = archive.extractfile(metadata_members[0])
        if extracted is None:
            raise VerificationError(f"{path.name}: unreadable PKG-INFO")
        metadata_version = _parse_metadata(extracted.read(), path)
        _verify_sdist_identity(path, parts, metadata_version)
        core_manifests = [
            member
            for member in members
            if member.name.endswith("/rust/ai-session-search-core/Cargo.toml")
        ]
        if len(core_manifests) != 1:
            raise VerificationError(f"{path.name}: expected exactly one core Cargo.toml")
        core_manifest = archive.extractfile(core_manifests[0])
        if core_manifest is None:
            raise VerificationError(f"{path.name}: unreadable core Cargo.toml")
        if FORBIDDEN_CARGO_BINARY in core_manifest.read().decode("utf-8"):
            raise VerificationError(f"{path.name}: contains removed aise-mcp executable")

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


def _native_archive_contract(path: pathlib.Path, names: Iterable[str]) -> tuple[str, str]:
    match = NATIVE_ARCHIVE_PATTERN.fullmatch(path.name)
    if match is None:
        raise VerificationError(f"invalid native archive name: {path.name}")
    root = match.group("root")
    binary_name = "aise.exe" if "windows" in root else "aise"
    installer_name = "install.ps1" if binary_name.endswith(".exe") else "install.sh"
    parts = _verify_names(names)
    if len(parts) != len(set(parts)):
        raise VerificationError(f"{path.name}: duplicate archive members")
    expected = {
        (root, "LICENSE"),
        (root, "NOTICE"),
        (root, binary_name),
        (root, NATIVE_RECEIPT_NAME),
        (root, installer_name),
    }
    actual = set(parts)
    if actual != expected:
        missing = sorted("/".join(item) for item in expected - actual)
        extra = sorted("/".join(item) for item in actual - expected)
        raise VerificationError(
            f"{path.name}: native archive contents differ; missing={missing}, extra={extra}"
        )
    return root, binary_name


def _verify_native_receipt(root: str, binary: bytes, receipt: bytes) -> None:
    target = next(
        (candidate for candidate in EXPECTED_NATIVE_TARGETS if root.endswith(f"-{candidate}")),
        None,
    )
    if target is None:
        raise VerificationError(f"{root}: native target is not supported")
    version = root[len("ai-session-search-") : -(len(target) + 1)]
    try:
        parsed = json.loads(receipt)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"{root}: invalid native install receipt: {error}") from error
    expected = {
        "schema_version": 1,
        "package": EXPECTED_DISTRIBUTION,
        "archive_version": version,
        "target": target,
        "executable_sha256": hashlib.sha256(binary).hexdigest(),
    }
    if parsed != expected:
        raise VerificationError(
            f"{root}: native install receipt differs from the executable and archive identity"
        )


def verify_native_archive(path: pathlib.Path) -> None:
    if path.name.endswith(".tar.gz"):
        with tarfile.open(path, "r:gz") as archive:
            members = archive.getmembers()
            if any(not member.isfile() for member in members):
                raise VerificationError(f"{path.name}: native archive may contain only regular files")
            root, binary_name = _native_archive_contract(path, (member.name for member in members))
            tar_binary = next(member for member in members if member.name == f"{root}/{binary_name}")
            receipt = next(
                member for member in members if member.name == f"{root}/{NATIVE_RECEIPT_NAME}"
            )
            installer = next(member for member in members if member.name == f"{root}/install.sh")
            if tar_binary.size == 0 or (tar_binary.mode & 0o111) == 0:
                raise VerificationError(f"{path.name}: native executable is empty or not executable")
            if installer.size == 0 or (installer.mode & 0o111) == 0:
                raise VerificationError(f"{path.name}: native installer is empty or not executable")
            binary_bytes = archive.extractfile(tar_binary).read()
            receipt_bytes = archive.extractfile(receipt).read()
            _verify_native_receipt(root, binary_bytes, receipt_bytes)
        return

    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        root, binary_name = _native_archive_contract(path, names)
        for member in archive.infolist():
            mode = member.external_attr >> 16
            file_type = stat.S_IFMT(mode)
            if member.is_dir() or file_type not in {0, stat.S_IFREG}:
                raise VerificationError(f"{path.name}: native archive may contain only regular files")
        zip_binary = archive.getinfo(f"{root}/{binary_name}")
        if zip_binary.file_size == 0:
            raise VerificationError(f"{path.name}: native executable is empty")
        if archive.getinfo(f"{root}/install.ps1").file_size == 0:
            raise VerificationError(f"{path.name}: native installer is empty")
        _verify_native_receipt(
            root,
            archive.read(f"{root}/{binary_name}"),
            archive.read(f"{root}/{NATIVE_RECEIPT_NAME}"),
        )


def verify_crate(path: pathlib.Path) -> None:
    match = CRATE_PATTERN.fullmatch(path.name)
    if match is None:
        raise VerificationError(f"invalid crate artifact name: {path.name}")
    root = f"ai-session-search-{match.group('version')}"
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        parts = _verify_names(member.name for member in members)
        if any(not (member.isfile() or member.isdir()) for member in members):
            raise VerificationError(f"{path.name}: crate may contain only regular files and directories")
    required = {
        (root, "Cargo.toml"),
        (root, "Cargo.toml.orig"),
        (root, "Cargo.lock"),
        (root, "LICENSE"),
        (root, "NOTICE"),
        (root, "README.md"),
        (root, "config.example.toml"),
        (root, "src", "lib.rs"),
        (root, "src", "main.rs"),
    }
    actual = set(parts)
    missing = sorted("/".join(item) for item in required - actual)
    if missing:
        raise VerificationError(f"{path.name}: missing required crate paths: {', '.join(missing)}")
    development = sorted(
        "/".join(item)
        for item in actual
        if item and item[-1] in {".gitignore", "flake.lock", "flake.nix"}
    )
    if development:
        raise VerificationError(
            f"{path.name}: contains development-only files: {', '.join(development)}"
        )


def _wheel_platform(name: str, version: str) -> str | None:
    prefix = f"ai_session_search-{version}-cp312-abi3-"
    if not name.startswith(prefix) or not name.endswith(".whl"):
        return None
    platform = name[len(prefix) : -len(".whl")]
    if "manylinux" in platform and platform.endswith("x86_64"):
        return "manylinux-x86_64"
    if "manylinux" in platform and platform.endswith("aarch64"):
        return "manylinux-aarch64"
    if platform.startswith("macosx_") and platform.endswith("arm64"):
        return "macos-arm64"
    if platform.startswith("macosx_") and platform.endswith("x86_64"):
        return "macos-x86_64"
    if platform == "win_amd64":
        return "windows-x86_64"
    return None


def verify_release_set(paths: Iterable[pathlib.Path], version: str) -> None:
    try:
        cargo_version = cargo_version_for_python(version)
    except ValueError as error:
        raise VerificationError(str(error)) from error
    names = [path.name for path in paths]
    duplicates = sorted(name for name, count in Counter(names).items() if count > 1)
    wheel_platforms = {_wheel_platform(name, version) for name in names if name.endswith(".whl")}
    wheel_platforms.discard(None)
    expected_names = {
        f"ai_session_search-{version}.tar.gz",
        f"ai-session-search-{cargo_version}.crate",
        "ai-session-search-python-runtime.cdx.json",
        "ai-session-search.cdx.json",
        "ai-session-search-python.cdx.json",
        "python-runtime-licenses.md",
        "rust-dependency-licenses.txt",
    }
    expected_names.update(
        f"ai-session-search-{version}-{target}.{suffix}"
        for target, suffix in EXPECTED_NATIVE_TARGETS.items()
    )
    non_wheels = {name for name in names if not name.endswith(".whl")}
    if (
        duplicates
        or len(names) != 17
        or len([name for name in names if name.endswith(".whl")]) != 5
        or wheel_platforms != EXPECTED_WHEEL_PLATFORMS
        or non_wheels != expected_names
    ):
        raise VerificationError(
            "release artifact set differs from the required five wheels, five native archives, "
            f"sdist, crate, SBOMs, and inventories; duplicates={duplicates}, "
            f"wheel_platforms={sorted(wheel_platforms)}, "
            f"missing={sorted(expected_names - non_wheels)}, extra={sorted(non_wheels - expected_names)}"
        )


def verify(path: pathlib.Path) -> None:
    if path.suffix == ".whl":
        verify_wheel(path)
    elif NATIVE_ARCHIVE_PATTERN.fullmatch(path.name):
        verify_native_archive(path)
    elif path.suffix == ".crate":
        verify_crate(path)
    elif path.name.endswith(".tar.gz"):
        verify_sdist(path)
    else:
        raise VerificationError(f"unsupported artifact type: {path.name}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", nargs="+", type=pathlib.Path)
    parser.add_argument("--release-set", action="store_true")
    parser.add_argument("--version")
    # Pass this in the job that builds the wheels, where SOURCE_DATE_EPOCH is set. The
    # verify job checks downloaded artifacts and has no build clock to compare against.
    parser.add_argument("--source-date-epoch", type=int)
    args = parser.parse_args(argv)
    try:
        if args.release_set:
            if not args.version:
                raise VerificationError("--release-set requires --version")
            verify_release_set(args.artifacts, args.version)
        for artifact in args.artifacts:
            is_distribution = (
                artifact.suffix in {".whl", ".crate", ".zip"}
                or artifact.name.endswith(".tar.gz")
            )
            if is_distribution:
                verify(artifact)
                if args.source_date_epoch is not None and artifact.suffix == ".whl":
                    verify_wheel_build_clock(artifact, args.source_date_epoch)
                print(f"verified: {artifact}")
            elif not args.release_set:
                raise VerificationError(f"unsupported artifact type: {artifact.name}")
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
