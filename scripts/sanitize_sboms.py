#!/usr/bin/env python3
"""Remove machine-local paths from CycloneDX documents atomically."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import pathlib
import tempfile
import urllib.parse
import urllib.request
import uuid
from typing import Any


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


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=pathlib.Path)
    parser.add_argument("--source-date-epoch", type=int)
    parser.add_argument("sboms", nargs="+", type=pathlib.Path)
    args = parser.parse_args()
    for sbom in args.sboms:
        sanitize_file(sbom, args.root, args.source_date_epoch)
        print(f"sanitized: {sbom}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
