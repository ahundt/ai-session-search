#!/usr/bin/env python3
"""Create a deterministic native AI Session Search release archive."""

from __future__ import annotations

import argparse
import datetime
import gzip
import io
import os
import pathlib
import re
import stat
import tarfile
import tempfile
import zipfile
from typing import BinaryIO, cast

DEFAULT_SOURCE_DATE_EPOCH = 315532800  # 1980-01-01, the earliest ZIP timestamp.
SAFE_COMPONENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


class PackagingError(ValueError):
    """A native release archive request is invalid."""


def _source_date_epoch() -> int:
    raw = os.environ.get("SOURCE_DATE_EPOCH")
    if raw is None:
        return DEFAULT_SOURCE_DATE_EPOCH
    try:
        value = int(raw)
    except ValueError as error:
        raise PackagingError("SOURCE_DATE_EPOCH must be an integer") from error
    if value < 0:
        raise PackagingError("SOURCE_DATE_EPOCH must not be negative")
    return value


def _payloads(
    binary: pathlib.Path,
    license_file: pathlib.Path,
    notice: pathlib.Path,
    installer: pathlib.Path,
) -> tuple[tuple[str, bytes, int], ...]:
    binary_name = "aise.exe" if binary.suffix.lower() == ".exe" else "aise"
    installer_name = "install.ps1" if binary_name.endswith(".exe") else "install.sh"
    if installer.suffix.lower() != pathlib.Path(installer_name).suffix:
        raise PackagingError(f"installer for {binary_name} must use the {installer_name} extension")
    return (
        ("LICENSE", license_file.read_bytes(), 0o644),
        ("NOTICE", notice.read_bytes(), 0o644),
        (binary_name, binary.read_bytes(), 0o755),
        (installer_name, installer.read_bytes(), 0o755 if installer_name.endswith(".sh") else 0o644),
    )


def _write_tar(raw: BinaryIO, root: str, payloads: tuple[tuple[str, bytes, int], ...], epoch: int) -> None:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
            for name, content, mode in payloads:
                info = tarfile.TarInfo(f"{root}/{name}")
                info.size = len(content)
                info.mode = mode
                info.mtime = epoch
                archive.addfile(info, io.BytesIO(content))


def _write_zip(raw: BinaryIO, root: str, payloads: tuple[tuple[str, bytes, int], ...], epoch: int) -> None:
    try:
        timestamp = datetime.datetime.fromtimestamp(max(epoch, DEFAULT_SOURCE_DATE_EPOCH), datetime.UTC)
    except (OverflowError, OSError, ValueError) as error:
        raise PackagingError("SOURCE_DATE_EPOCH is outside the supported timestamp range") from error
    if timestamp.year > 2107:
        raise PackagingError("SOURCE_DATE_EPOCH exceeds the ZIP timestamp range")
    date_time = (timestamp.year, timestamp.month, timestamp.day, timestamp.hour, timestamp.minute, timestamp.second)
    with zipfile.ZipFile(raw, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, content, mode in payloads:
            info = zipfile.ZipInfo(f"{root}/{name}", date_time=date_time)
            info.create_system = 3
            info.external_attr = (stat.S_IFREG | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(info, content)


def _sync_directory(path: pathlib.Path) -> None:
    if os.name != "posix":
        return
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def package_native_release(
    binary: pathlib.Path,
    license_file: pathlib.Path,
    notice: pathlib.Path,
    installer: pathlib.Path,
    output_dir: pathlib.Path,
    version: str,
    target: str,
    archive_format: str,
) -> pathlib.Path:
    """Package one already-built native executable without mutating its inputs."""
    for label, value in (("version", version), ("target", target)):
        if not SAFE_COMPONENT.fullmatch(value):
            raise PackagingError(f"{label} must be one safe path component")
    if archive_format not in {"tar.gz", "zip"}:
        raise PackagingError("format must be tar.gz or zip")
    for label, path in (
        ("binary", binary),
        ("license", license_file),
        ("notice", notice),
        ("installer", installer),
    ):
        if not path.is_file():
            raise PackagingError(f"{label} file does not exist: {path}")

    root = f"ai-session-search-{version}-{target}"
    output_dir.mkdir(parents=True, exist_ok=True)
    output = output_dir / f"{root}.{archive_format}"
    payloads = _payloads(binary, license_file, notice, installer)
    epoch = _source_date_epoch()
    staging_path: pathlib.Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w+b",
            prefix=f".{output.name}.",
            suffix=".staging",
            dir=output_dir,
            delete=False,
        ) as staging:
            staging_path = pathlib.Path(staging.name)
            output_file = cast(BinaryIO, staging)
            if archive_format == "tar.gz":
                _write_tar(output_file, root, payloads, epoch)
            else:
                _write_zip(output_file, root, payloads, epoch)
            staging.flush()
            os.fsync(staging.fileno())
        os.link(staging_path, output)
        _sync_directory(output_dir)
    finally:
        if staging_path is not None:
            staging_path.unlink(missing_ok=True)
    return output


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--license", required=True, type=pathlib.Path)
    parser.add_argument("--notice", required=True, type=pathlib.Path)
    parser.add_argument("--installer", required=True, type=pathlib.Path)
    parser.add_argument("--output-dir", required=True, type=pathlib.Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--format", required=True, choices=("tar.gz", "zip"))
    args = parser.parse_args()
    try:
        output = package_native_release(
            args.binary,
            args.license,
            args.notice,
            args.installer,
            args.output_dir,
            args.version,
            args.target,
            args.format,
        )
    except (OSError, PackagingError) as error:
        parser.error(str(error))
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
