#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Prepare verified Cargo and Python artifacts without publishing them."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pathlib
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from collections.abc import Callable, Sequence

from scripts.release_versions import cargo_version_for_python
from scripts.sanitize_sboms import SanitizationError, sanitize

ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT = ROOT / "dist" / "packages"
PACKAGE_SCOPES = ("all", "rust", "python")
Builder = Callable[[pathlib.Path], list[pathlib.Path]]
Verifier = Callable[[Sequence[pathlib.Path]], None]


class PreparationError(RuntimeError):
    """A package set could not be prepared without ambiguity or data loss."""


def _python_version() -> str:
    with (ROOT / "pyproject.toml").open("rb") as stream:
        return str(tomllib.load(stream)["project"]["version"])


def _cargo_version() -> str:
    python_version = _python_version()
    expected = cargo_version_for_python(python_version)
    with (ROOT / "rust/ai-session-search-core/Cargo.toml").open("rb") as stream:
        actual = str(tomllib.load(stream)["package"]["version"])
    if actual != expected:
        raise PreparationError(f"Cargo version {actual!r} must be {expected!r} for Python version {python_version!r}")
    return actual


def _cargo_target_dir() -> pathlib.Path:
    configured = os.environ.get("CARGO_TARGET_DIR")
    target = pathlib.Path(configured).expanduser() if configured else ROOT / "target"
    return target if target.is_absolute() else ROOT / target


def _run(command: Sequence[str]) -> None:
    environment = os.environ.copy()
    # sccache cannot cache incremental Rust compilations. Packaging favors reusable,
    # non-incremental objects, but an explicit caller choice still wins.
    environment.setdefault("CARGO_INCREMENTAL", "0")
    environment.setdefault("CARGO_TARGET_DIR", os.fspath(_cargo_target_dir()))
    wrapper = environment.get("RUSTC_WRAPPER")
    has_separator = wrapper and any(separator and separator in wrapper for separator in (os.sep, os.altsep))
    if wrapper and not has_separator and (resolved_wrapper := shutil.which(wrapper)):
        environment["RUSTC_WRAPPER"] = resolved_wrapper
    subprocess.run(command, cwd=ROOT, check=True, env=environment)


def build_rust(staging: pathlib.Path) -> list[pathlib.Path]:
    version = _cargo_version()
    package_dir = _cargo_target_dir() / "package"
    source = package_dir / f"ai-session-search-{version}.crate"
    extracted = package_dir / f"ai-session-search-{version}"
    destination = staging / source.name
    try:
        _run(("cargo", "package", "--locked", "-p", "ai-session-search"))
        if not source.is_file():
            raise PreparationError(f"cargo package did not create expected artifact: {source}")
        shutil.copy2(source, destination)
    finally:
        source.unlink(missing_ok=True)
        shutil.rmtree(extracted, ignore_errors=True)
    return [destination]


def build_python(staging: pathlib.Path) -> list[pathlib.Path]:
    version = _python_version()
    _run(
        (
            "uv",
            "run",
            "--locked",
            "maturin",
            "build",
            "--release",
            "--locked",
            "--compatibility",
            "pypi",
            "--out",
            os.fspath(staging),
        )
    )
    _run(("uv", "run", "--locked", "maturin", "sdist", "--out", os.fspath(staging)))
    expected_sdist = staging / f"ai_session_search-{version}.tar.gz"
    wheels = sorted(staging.glob(f"ai_session_search-{version}-*.whl"))
    if not expected_sdist.is_file() or len(wheels) != 1:
        raise PreparationError(
            "uv-managed maturin must create exactly one local-platform wheel and one source distribution; "
            f"found wheels={len(wheels)}, sdist={expected_sdist.is_file()} in {staging}"
        )
    sanitize_wheel_sboms(wheels[0], ROOT)
    return [*wheels, expected_sdist]


# PEP 770 places SBOMs here; maturin writes one CycloneDX document per wheel and records the
# checkout it built from as `path+file://<checkout>/rust/<crate>` on the workspace components,
# so a wheel built at home carries the maintainer's home directory. The separate SBOMs are
# already rewritten to `workspace:<relative>` by scripts.sanitize_sboms; this applies the same
# rewrite inside the wheel and keeps RECORD's hash and size for the member correct.
_WHEEL_SBOM_MEMBER = re.compile(r"\.dist-info/sboms/[^/]+\.json$")
_WHEEL_RECORD_MEMBER = re.compile(r"\.dist-info/RECORD$")


def _record_digest(payload: bytes) -> str:
    return base64.urlsafe_b64encode(hashlib.sha256(payload).digest()).rstrip(b"=").decode()


def _sanitized_sbom(wheel_name: str, member: str, payload: bytes, root: pathlib.Path) -> bytes:
    try:
        document = sanitize(json.loads(payload), root)
    except SanitizationError as error:
        raise PreparationError(f"{wheel_name}: {member}: {error}") from error
    text = json.dumps(document, indent=2, ensure_ascii=False) + "\n"
    if "path+file://" in text:
        raise PreparationError(f"{wheel_name}: {member} still names a local path after sanitizing")
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

    Every member is rewritten in its original order with its original ZipInfo, so the wheel is
    byte-stable apart from the sanitized SBOMs and the RECORD rows that describe them. A path
    outside `root` is refused rather than left in place: it means the wheel was built from a tree
    this script does not know, and silently shipping the path is the failure this exists to stop.
    """
    root = root.resolve()
    with zipfile.ZipFile(wheel) as archive:
        members = [(info, archive.read(info.filename)) for info in archive.infolist()]
    rewritten = {
        info.filename: _sanitized_sbom(wheel.name, info.filename, payload, root) for info, payload in members if _WHEEL_SBOM_MEMBER.search(info.filename)
    }
    if not rewritten:
        return
    temporary = wheel.with_name(f".{wheel.name}.sanitizing")
    with zipfile.ZipFile(temporary, "w") as archive:
        for info, payload in members:
            if info.filename in rewritten:
                payload = rewritten[info.filename]
            elif _WHEEL_RECORD_MEMBER.search(info.filename):
                payload = _record_with(payload, rewritten)
            replacement = zipfile.ZipInfo(info.filename, date_time=info.date_time)
            replacement.compress_type = info.compress_type
            replacement.external_attr = info.external_attr
            replacement.create_system = info.create_system
            archive.writestr(replacement, payload)
    os.replace(temporary, wheel)


def verify_artifacts(artifacts: Sequence[pathlib.Path]) -> None:
    _run(
        (
            sys.executable,
            os.fspath(ROOT / "scripts" / "verify_release_artifacts.py"),
            *(os.fspath(artifact) for artifact in artifacts),
        )
    )


def prepare_packages(
    scope: str,
    output_dir: pathlib.Path,
    *,
    rust_builder: Builder = build_rust,
    python_builder: Builder = build_python,
    verifier: Verifier = verify_artifacts,
) -> list[pathlib.Path]:
    if scope not in PACKAGE_SCOPES:
        raise PreparationError(f"unsupported package scope {scope!r}; choose: {', '.join(PACKAGE_SCOPES)}")
    output_dir = output_dir.resolve()
    if output_dir.exists():
        raise PreparationError(f"output path already exists: {output_dir}; choose a new --output-dir so stale and current artifacts cannot mix")

    output_dir.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".aise-packages-", dir=output_dir.parent) as temporary:
        staging = pathlib.Path(temporary) / "complete"
        staging.mkdir()
        artifacts: list[pathlib.Path] = []
        if scope in {"all", "rust"}:
            artifacts.extend(rust_builder(staging))
        if scope in {"all", "python"}:
            artifacts.extend(python_builder(staging))
        artifacts = sorted(artifacts)
        verifier(artifacts)

        expected = {artifact.resolve() for artifact in artifacts}
        for candidate in staging.iterdir():
            if candidate.resolve() not in expected:
                if candidate.is_dir():
                    shutil.rmtree(candidate)
                else:
                    candidate.unlink()
        os.replace(staging, output_dir)

    return [output_dir / artifact.name for artifact in artifacts]


def argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Prepare verified local Cargo and/or PyPI artifacts without uploading them. Defaults to the complete set.")
    parser.add_argument(
        "--package",
        choices=PACKAGE_SCOPES,
        default="all",
        help="artifact scope: all (default), rust (.crate), or python (local wheel and sdist)",
    )
    parser.add_argument(
        "--output-dir",
        type=pathlib.Path,
        default=DEFAULT_OUTPUT,
        help=f"new directory that receives the verified set (default: {DEFAULT_OUTPUT.relative_to(ROOT)})",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = argument_parser().parse_args(argv)
    try:
        artifacts = prepare_packages(args.package, args.output_dir)
    except subprocess.CalledProcessError as error:
        command = shlex.join(os.fspath(part) for part in error.cmd)
        print(
            f"error: package preparation command failed with exit {error.returncode}: {command}\n"
            "The command inherited Cargo, Rust compiler-wrapper, uv, Python, and platform settings unchanged; "
            "review the command's diagnostic or rerun with an intentional environment override.",
            file=sys.stderr,
        )
        return 1
    except (OSError, PreparationError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    for artifact in artifacts:
        print(artifact)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
