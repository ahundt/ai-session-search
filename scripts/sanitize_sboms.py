#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Remove machine-local paths from CycloneDX documents, loose or embedded in a wheel, atomically."""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import hashlib
import json
import os
import pathlib
import re
import tempfile
import urllib.parse
import urllib.request
import uuid
import zipfile
from typing import Any

# PEP 770 places SBOMs here; maturin writes one CycloneDX document per wheel and records the
# checkout it built from as `path+file://<checkout>/rust/<crate>` on the workspace components,
# so a wheel carries the directory it was built in: the maintainer's home locally, the runner's
# checkout in CI. The rewrite is the same `workspace:<relative>` the loose SBOMs get.
_WHEEL_SBOM_MEMBER = re.compile(r"\.dist-info/sboms/[^/]+\.json$")
_WHEEL_RECORD_MEMBER = re.compile(r"\.dist-info/RECORD$")


class SanitizationError(ValueError):
    """An SBOM contains a local path outside the declared workspace."""


def _sanitize_reference(value: str, root: pathlib.Path) -> str:
    if not value.startswith("path+file://"):
        return value
    parsed = urllib.parse.urlsplit(value.removeprefix("path+"))
    if parsed.netloc not in {"", "localhost"}:
        raise SanitizationError(f"local dependency URL has a remote host: {parsed.netloc}")
    local_path = pathlib.Path(urllib.request.url2pathname(parsed.path)).resolve()
    try:
        relative = local_path.relative_to(root)
    except ValueError as error:
        raise SanitizationError(f"local dependency path is outside workspace: {local_path}") from error
    reference = f"workspace:{relative.as_posix()}"
    if parsed.fragment:
        reference += f"#{parsed.fragment}"
    return reference


def sanitize(value: Any, root: pathlib.Path) -> Any:
    if isinstance(value, dict):
        return {key: sanitize(item, root) for key, item in value.items()}
    if isinstance(value, list):
        return [sanitize(item, root) for item in value]
    if isinstance(value, str):
        return _sanitize_reference(value, root)
    return value


def _make_reproducible(document: dict[str, Any], source_date_epoch: int) -> None:
    metadata = document.setdefault("metadata", {})
    timestamp = dt.datetime.fromtimestamp(source_date_epoch, tz=dt.UTC)
    metadata["timestamp"] = timestamp.isoformat().replace("+00:00", "Z")
    document.pop("serialNumber", None)
    canonical = json.dumps(document, sort_keys=True, separators=(",", ":")).encode()
    digest = hashlib.sha256(canonical).hexdigest()
    document["serialNumber"] = f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, digest)}"


def sanitize_file(
    path: pathlib.Path,
    root: pathlib.Path,
    source_date_epoch: int | None = None,
) -> None:
    document = json.loads(path.read_text(encoding="utf-8"))
    sanitized = sanitize(document, root.resolve())
    if not isinstance(sanitized, dict):
        raise SanitizationError("SBOM root must be a JSON object")
    if source_date_epoch is not None:
        _make_reproducible(sanitized, source_date_epoch)
    temporary: pathlib.Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as output:
            temporary = pathlib.Path(output.name)
            json.dump(sanitized, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def _record_digest(payload: bytes) -> str:
    return base64.urlsafe_b64encode(hashlib.sha256(payload).digest()).rstrip(b"=").decode()


def _sanitized_wheel_sbom(wheel_name: str, member: str, payload: bytes, root: pathlib.Path) -> bytes:
    try:
        document = sanitize(json.loads(payload), root)
    except SanitizationError as error:
        raise SanitizationError(f"{wheel_name}: {member}: {error}") from error
    text = json.dumps(document, indent=2, ensure_ascii=False) + "\n"
    if "path+file://" in text:
        raise SanitizationError(f"{wheel_name}: {member} still names a local path after sanitizing")
    return text.encode()


def _record_with(payload: bytes, rewritten: dict[str, bytes]) -> bytes:
    lines = []
    for line in payload.decode().splitlines():
        name = line.split(",", 1)[0]
        if name in rewritten:
            body = rewritten[name]
            line = f"{name},sha256={_record_digest(body)},{len(body)}"
        lines.append(line)
    return ("\n".join(lines) + "\n").encode()


def sanitize_wheel_sboms(wheel: pathlib.Path, root: pathlib.Path) -> None:
    """Rewrite every SBOM member of `wheel` so no `path+file://` reference remains.

    ZIP has no in-place member replacement, so the archive is rebuilt: every member is written
    back in its original order, carrying its own ZipInfo and therefore its date, compression
    method, permissions, create system, extra field, comment, and internal attributes. The two
    members that change are the SBOMs and the RECORD rows describing them; every other member's
    content comes across byte for byte, and the build clock maturin stamped is untouched.

    Compressed bytes are not preserved, because ZIP records no compression level and the rebuild
    re-encodes with CPython's deflate rather than the builder's. On the published
    `ai_session_search-1.0.0rc1-cp312-abi3-macosx_11_0_arm64.whl` that changed the compressed size
    of 14 of 15 members. Nothing depends on those bytes: RECORD digests cover uncompressed
    content, and the artifacts that get verified, attested, and uploaded are the sanitized ones.

    A path outside `root` is refused before anything is written: it means the wheel was built from
    a tree this script does not know, and silently shipping the path is the failure this exists to
    stop.
    """
    root = root.resolve()
    with zipfile.ZipFile(wheel) as archive:
        members = [(info, archive.read(info.filename)) for info in archive.infolist()]
    rewritten = {
        info.filename: _sanitized_wheel_sbom(wheel.name, info.filename, payload, root) for info, payload in members if _WHEEL_SBOM_MEMBER.search(info.filename)
    }
    if not rewritten:
        return
    temporary: pathlib.Path | None = wheel.with_name(f".{wheel.name}.sanitizing")
    try:
        with zipfile.ZipFile(temporary, "w") as archive:
            for info, payload in members:
                if info.filename in rewritten:
                    payload = rewritten[info.filename]
                elif _WHEEL_RECORD_MEMBER.search(info.filename):
                    payload = _record_with(payload, rewritten)
                # The member's own ZipInfo is written back, so every field it carries travels
                # across — `date_time`, `compress_type`, `external_attr`, `create_system`,
                # `extra`, `comment`, `internal_attr` — each of which a fresh ZipInfo would start
                # empty. The compression *level* is not among them: ZIP does not record it, so a
                # ZipInfo read back from an archive has `_compresslevel` unset and the rebuild
                # re-encodes at CPython's default, as the docstring above states. `writestr`
                # recomputes the sizes and CRC for the payload it is given, which is what the two
                # rewritten members need.
                archive.writestr(info, payload)
        os.replace(temporary, wheel)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=pathlib.Path)
    parser.add_argument("--source-date-epoch", type=int, help="applies to loose documents only; a wheel keeps the clock maturin stamped")
    parser.add_argument("sboms", nargs="+", type=pathlib.Path, help="CycloneDX JSON files and/or wheels whose embedded SBOMs to rewrite")
    args = parser.parse_args()
    for sbom in args.sboms:
        if sbom.suffix == ".whl":
            sanitize_wheel_sboms(sbom, args.root)
        else:
            sanitize_file(sbom, args.root, args.source_date_epoch)
        print(f"sanitized: {sbom}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
