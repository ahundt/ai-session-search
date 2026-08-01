#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Install one Python artifact through supported pip/uv paths and verify each result."""

from __future__ import annotations

import argparse
import os
import pathlib
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import urllib.parse

DEFAULT_INSTALL_TIMEOUT_SECONDS = 180.0
FULL_GIT_OBJECT_ID = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})\Z")


class InstallMethodError(RuntimeError):
    """A supported package installation method failed its acceptance contract."""


def _validate_timeout(timeout_seconds: float) -> None:
    if timeout_seconds <= 0:
        raise InstallMethodError("timeout must be greater than zero")


def _run(
    command: list[str],
    *,
    root: pathlib.Path,
    environment: dict[str, str],
    timeout_seconds: float,
) -> None:
    try:
        completed = subprocess.run(
            command,
            cwd=root,
            env=environment,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise InstallMethodError(
            f"{shlex.join(command)} exceeded {timeout_seconds:g} seconds"
        ) from error
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        raise InstallMethodError(
            f"{shlex.join(command)} exited {completed.returncode}: {detail}"
        )


def _environment(
    root: pathlib.Path,
    bin_dir: pathlib.Path | None = None,
    *,
    python: pathlib.Path | None = None,
) -> dict[str, str]:
    config_path = root / "config" / "config.toml"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    config_path.touch(exist_ok=True)
    environment = os.environ.copy()
    environment.pop("PYTHONPATH", None)
    environment.pop("VIRTUAL_ENV", None)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["PIP_DISABLE_PIP_VERSION_CHECK"] = "1"
    environment["UV_PYTHON"] = str(python or pathlib.Path(sys.executable))
    environment["AI_SESSION_SEARCH_CONFIG"] = str(config_path)
    environment["AI_SESSION_SEARCH_CACHE_DIR"] = str(root / "cache")
    if bin_dir is not None:
        environment["PATH"] = os.pathsep.join((str(bin_dir), environment.get("PATH", "")))
    return environment


def _venv_python(environment: pathlib.Path) -> pathlib.Path:
    if os.name == "nt":
        return environment / "Scripts" / "python.exe"
    return environment / "bin" / "python"


def _venv_bin(environment: pathlib.Path) -> pathlib.Path:
    return environment / ("Scripts" if os.name == "nt" else "bin")


def _verify_with_python(
    python: pathlib.Path,
    verifier: pathlib.Path,
    source_root: pathlib.Path,
    *,
    root: pathlib.Path,
    environment: dict[str, str],
    timeout_seconds: float,
) -> None:
    _run(
        [str(python), str(verifier), "--source-root", str(source_root)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )


def _verify_pip(
    install_source: str,
    source_root: pathlib.Path,
    verifier: pathlib.Path,
    root: pathlib.Path,
    timeout_seconds: float,
    *,
    python: pathlib.Path,
) -> None:
    root.mkdir(parents=True)
    environment_path = root / "environment"
    environment = _environment(root, python=python)
    _run(
        [str(python), "-m", "venv", str(environment_path)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    python = _venv_python(environment_path)
    _run(
        [str(python), "-m", "pip", "install", install_source],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    _verify_with_python(
        python,
        verifier,
        source_root,
        root=root,
        environment=_environment(root, _venv_bin(environment_path), python=python),
        timeout_seconds=timeout_seconds,
    )


def _verify_uv_add(
    uv: str,
    install_source: str,
    source_root: pathlib.Path,
    verifier: pathlib.Path,
    root: pathlib.Path,
    timeout_seconds: float,
    *,
    python: pathlib.Path,
) -> None:
    root.mkdir(parents=True)
    project = root / "project"
    environment = _environment(root, python=python)
    _run(
        [uv, "init", "--bare", "--python", str(python), str(project)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    _run(
        [uv, "add", "--project", str(project), install_source],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    _run(
        [
            uv,
            "run",
            "--project",
            str(project),
            "python",
            str(verifier),
            "--source-root",
            str(source_root),
        ],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )


def _verify_uv_tool(
    uv: str,
    install_source: str,
    verifier: pathlib.Path,
    root: pathlib.Path,
    timeout_seconds: float,
    *,
    python: pathlib.Path,
) -> None:
    root.mkdir(parents=True)
    tool_dir = root / "tools"
    bin_dir = root / "bin"
    environment = _environment(root, bin_dir, python=python)
    environment["UV_TOOL_DIR"] = str(tool_dir)
    environment["UV_TOOL_BIN_DIR"] = str(bin_dir)
    _run(
        [uv, "tool", "install", "--python", str(python), install_source],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    executable = bin_dir / ("aise.exe" if os.name == "nt" else "aise")
    _run(
        [
            str(python),
            str(verifier),
            "--executable",
            str(executable),
            "--command-timeout-seconds",
            str(timeout_seconds),
        ],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )


def _uvx_wrapper(uvx: str, install_source: str, root: pathlib.Path) -> pathlib.Path:
    if os.name == "nt":
        wrapper = root / "aise-uvx.cmd"
        wrapper.write_text(
            f'@"{uvx}" --from "{install_source}" aise %*\r\n',
            encoding="utf-8",
        )
    else:
        wrapper = root / "aise-uvx"
        wrapper.write_text(
            f"#!/bin/sh\nexec {shlex.quote(uvx)} --from {shlex.quote(install_source)} aise \"$@\"\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
    return wrapper


def _verify_uvx(
    uvx: str,
    install_source: str,
    verifier: pathlib.Path,
    root: pathlib.Path,
    timeout_seconds: float,
    *,
    python: pathlib.Path,
) -> None:
    root.mkdir(parents=True)
    wrapper = _uvx_wrapper(uvx, install_source, root)
    environment = _environment(root, python=python)
    # uvx creates and caches its ephemeral tool environment under UV_TOOL_DIR. Keep that state
    # inside this verifier's temporary root so the install can neither depend on nor mutate the
    # user's persistent uv tools. UV_TOOL_BIN_DIR closes the same leak if uv changes the deferred
    # execution path to publish a shim.
    environment["UV_TOOL_DIR"] = str(root / "tools")
    environment["UV_TOOL_BIN_DIR"] = str(root / "bin")
    _run(
        [
            str(python),
            str(verifier),
            "--executable",
            str(wrapper),
            "--command-timeout-seconds",
            str(timeout_seconds),
        ],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )


def _git_install_source(git_url: str, git_rev: str) -> str:
    if not FULL_GIT_OBJECT_ID.fullmatch(git_rev):
        raise InstallMethodError(
            "git revision must be a full 40- or 64-hex commit object ID"
        )

    parsed = urllib.parse.urlsplit(git_url)
    if parsed.scheme not in {"https", "file"}:
        raise InstallMethodError("git URL scheme must be https or file")
    if parsed.query or parsed.fragment:
        raise InstallMethodError("git URL must not contain a query or fragment")
    if parsed.username is not None or parsed.password is not None:
        raise InstallMethodError("git URL must not contain credentials")
    if parsed.scheme == "https" and (not parsed.netloc or not parsed.path.strip("/")):
        raise InstallMethodError("HTTPS git URL must include a host and repository path")
    if parsed.scheme == "file" and (
        parsed.netloc not in {"", "localhost"} or not pathlib.PurePosixPath(parsed.path).is_absolute()
    ):
        raise InstallMethodError("file git URL must contain an absolute local path")

    # Both pip and uv document bare VCS URLs. Unlike a named PEP 508 direct reference, this form
    # also remains valid for local ``git+file`` repositories used by the isolated CI canary.
    return f"git+{git_url}@{git_rev.lower()}"


def _verify_install_source(
    install_source: str,
    source_root: pathlib.Path,
    timeout_seconds: float = DEFAULT_INSTALL_TIMEOUT_SECONDS,
    *,
    python: pathlib.Path | None = None,
) -> None:
    source_root = source_root.resolve(strict=True)
    python = (python or pathlib.Path(sys.executable)).resolve(strict=True)
    uv = shutil.which("uv")
    uvx = shutil.which("uvx")
    if uv is None or uvx is None:
        raise InstallMethodError("both uv and uvx must be installed")
    verifier = source_root / "scripts" / "verify_installed_distribution.py"
    native = source_root / "scripts" / "verify_native_executable.py"

    with tempfile.TemporaryDirectory(prefix="aise-install-methods-") as temporary:
        root = pathlib.Path(temporary)
        _verify_pip(
            install_source,
            source_root,
            verifier,
            root / "pip",
            timeout_seconds,
            python=python,
        )
        _verify_uv_add(
            uv,
            install_source,
            source_root,
            verifier,
            root / "uv-add",
            timeout_seconds,
            python=python,
        )
        _verify_uv_tool(
            uv,
            install_source,
            native,
            root / "uv-tool",
            timeout_seconds,
            python=python,
        )
        _verify_uvx(
            uvx,
            install_source,
            native,
            root / "uvx",
            timeout_seconds,
            python=python,
        )


def verify(
    artifact: pathlib.Path,
    source_root: pathlib.Path,
    timeout_seconds: float = DEFAULT_INSTALL_TIMEOUT_SECONDS,
    *,
    python: pathlib.Path | None = None,
) -> None:
    _validate_timeout(timeout_seconds)
    artifact = artifact.resolve(strict=True)
    _verify_install_source(
        str(artifact),
        source_root,
        timeout_seconds,
        python=python,
    )


def verify_git(
    git_url: str,
    git_rev: str,
    source_root: pathlib.Path,
    timeout_seconds: float = DEFAULT_INSTALL_TIMEOUT_SECONDS,
    *,
    python: pathlib.Path | None = None,
) -> None:
    _validate_timeout(timeout_seconds)
    install_source = _git_install_source(git_url, git_rev)
    _verify_install_source(
        install_source,
        source_root,
        timeout_seconds,
        python=python,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--artifact", type=pathlib.Path)
    source.add_argument(
        "--git-url",
        help="HTTPS or absolute file URL for a Git repository containing the project",
    )
    parser.add_argument(
        "--git-rev",
        help="Full 40- or 64-hex commit object ID (required with --git-url)",
    )
    parser.add_argument("--source-root", required=True, type=pathlib.Path)
    parser.add_argument(
        "--python",
        type=pathlib.Path,
        default=pathlib.Path(sys.executable),
        help="Interpreter whose platform tags must accept the artifact (default: current Python)",
    )
    parser.add_argument(
        "--timeout-seconds",
        default=DEFAULT_INSTALL_TIMEOUT_SECONDS,
        type=float,
    )
    args = parser.parse_args()
    try:
        if args.git_url is not None:
            if args.git_rev is None:
                parser.error("--git-rev is required with --git-url")
            verify_git(
                args.git_url,
                args.git_rev,
                args.source_root,
                args.timeout_seconds,
                python=args.python,
            )
        else:
            if args.git_rev is not None:
                parser.error("--git-rev requires --git-url")
            verify(
                args.artifact,
                args.source_root,
                args.timeout_seconds,
                python=args.python,
            )
    except (InstallMethodError, OSError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
