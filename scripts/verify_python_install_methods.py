#!/usr/bin/env python3
"""Install one Python artifact through supported pip/uv paths and verify each result."""

from __future__ import annotations

import argparse
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
import tempfile

DEFAULT_INSTALL_TIMEOUT_SECONDS = 180.0


class InstallMethodError(RuntimeError):
    """A supported package installation method failed its acceptance contract."""


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


def _environment(root: pathlib.Path, bin_dir: pathlib.Path | None = None) -> dict[str, str]:
    environment = os.environ.copy()
    environment.pop("PYTHONPATH", None)
    environment.pop("VIRTUAL_ENV", None)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["PIP_DISABLE_PIP_VERSION_CHECK"] = "1"
    environment["UV_PYTHON"] = sys.executable
    environment["AI_SESSION_SEARCH_CONFIG"] = str(root / "config" / "config.toml")
    environment["AI_SESSION_SEARCH_CACHE_DIR"] = str(root / "cache")
    environment["UV_CACHE_DIR"] = str(root / "uv-cache")
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
    artifact: pathlib.Path,
    source_root: pathlib.Path,
    verifier: pathlib.Path,
    root: pathlib.Path,
    timeout_seconds: float,
) -> None:
    root.mkdir(parents=True)
    environment_path = root / "environment"
    environment = _environment(root)
    _run(
        [sys.executable, "-m", "venv", str(environment_path)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    python = _venv_python(environment_path)
    _run(
        [str(python), "-m", "pip", "install", str(artifact)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    _verify_with_python(
        python,
        verifier,
        source_root,
        root=root,
        environment=_environment(root, _venv_bin(environment_path)),
        timeout_seconds=timeout_seconds,
    )


def _verify_uv_add(
    uv: str,
    artifact: pathlib.Path,
    source_root: pathlib.Path,
    verifier: pathlib.Path,
    root: pathlib.Path,
    timeout_seconds: float,
) -> None:
    root.mkdir(parents=True)
    project = root / "project"
    environment = _environment(root)
    _run(
        [uv, "init", "--bare", "--python", sys.executable, str(project)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    _run(
        [uv, "add", "--project", str(project), str(artifact)],
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
    artifact: pathlib.Path,
    verifier: pathlib.Path,
    root: pathlib.Path,
    timeout_seconds: float,
) -> None:
    root.mkdir(parents=True)
    tool_dir = root / "tools"
    bin_dir = root / "bin"
    environment = _environment(root, bin_dir)
    environment["UV_TOOL_DIR"] = str(tool_dir)
    environment["UV_TOOL_BIN_DIR"] = str(bin_dir)
    _run(
        [uv, "tool", "install", "--python", sys.executable, str(artifact)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )
    executable = bin_dir / ("aise.exe" if os.name == "nt" else "aise")
    _run(
        [sys.executable, str(verifier), "--executable", str(executable)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )


def _uvx_wrapper(uvx: str, artifact: pathlib.Path, root: pathlib.Path) -> pathlib.Path:
    if os.name == "nt":
        wrapper = root / "aise-uvx.cmd"
        wrapper.write_text(
            f'@"{uvx}" --from "{artifact}" aise %*\r\n',
            encoding="utf-8",
        )
    else:
        wrapper = root / "aise-uvx"
        wrapper.write_text(
            f"#!/bin/sh\nexec {shlex.quote(uvx)} --from {shlex.quote(str(artifact))} aise \"$@\"\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
    return wrapper


def _verify_uvx(
    uvx: str,
    artifact: pathlib.Path,
    verifier: pathlib.Path,
    root: pathlib.Path,
    timeout_seconds: float,
) -> None:
    root.mkdir(parents=True)
    wrapper = _uvx_wrapper(uvx, artifact, root)
    environment = _environment(root)
    _run(
        [sys.executable, str(verifier), "--executable", str(wrapper)],
        root=root,
        environment=environment,
        timeout_seconds=timeout_seconds,
    )


def verify(
    artifact: pathlib.Path,
    source_root: pathlib.Path,
    timeout_seconds: float = DEFAULT_INSTALL_TIMEOUT_SECONDS,
) -> None:
    if timeout_seconds <= 0:
        raise InstallMethodError("timeout must be greater than zero")
    artifact = artifact.resolve(strict=True)
    source_root = source_root.resolve(strict=True)
    uv = shutil.which("uv")
    uvx = shutil.which("uvx")
    if uv is None or uvx is None:
        raise InstallMethodError("both uv and uvx must be installed")
    verifier = source_root / "scripts" / "verify_installed_distribution.py"
    native = source_root / "scripts" / "verify_native_executable.py"

    with tempfile.TemporaryDirectory(prefix="aise-install-methods-") as temporary:
        root = pathlib.Path(temporary)
        _verify_pip(artifact, source_root, verifier, root / "pip", timeout_seconds)
        _verify_uv_add(uv, artifact, source_root, verifier, root / "uv-add", timeout_seconds)
        _verify_uv_tool(uv, artifact, native, root / "uv-tool", timeout_seconds)
        _verify_uvx(uvx, artifact, native, root / "uvx", timeout_seconds)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", required=True, type=pathlib.Path)
    parser.add_argument("--source-root", required=True, type=pathlib.Path)
    parser.add_argument(
        "--timeout-seconds",
        default=DEFAULT_INSTALL_TIMEOUT_SECONDS,
        type=float,
    )
    args = parser.parse_args()
    try:
        verify(args.artifact, args.source_root, args.timeout_seconds)
    except (InstallMethodError, OSError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
