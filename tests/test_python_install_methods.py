from __future__ import annotations

import os
from pathlib import Path

import pytest

from scripts import verify_python_install_methods as install_methods


def test_environment_removes_python_activation_state(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setenv("PYTHONPATH", "/source-leak")
    monkeypatch.setenv("VIRTUAL_ENV", "/active-environment")
    monkeypatch.setenv("PATH", os.pathsep.join(("first", "second")))
    bin_dir = tmp_path / "bin"

    environment = install_methods._environment(tmp_path, bin_dir)

    assert "PYTHONPATH" not in environment
    assert "VIRTUAL_ENV" not in environment
    assert environment["PATH"].split(os.pathsep) == [str(bin_dir), "first", "second"]
    assert environment["AI_SESSION_SEARCH_CONFIG"] == str(
        tmp_path / "config" / "config.toml"
    )
    assert environment["AI_SESSION_SEARCH_CACHE_DIR"] == str(tmp_path / "cache")
    assert environment["UV_CACHE_DIR"] == str(tmp_path / "uv-cache")
    assert environment["PYTHONDONTWRITEBYTECODE"] == "1"
    assert environment["PIP_DISABLE_PIP_VERSION_CHECK"] == "1"
    assert environment["UV_PYTHON"] == install_methods.sys.executable
    assert os.environ["PYTHONPATH"] == "/source-leak"
    assert os.environ["VIRTUAL_ENV"] == "/active-environment"


def test_verify_rejects_nonpositive_timeout_before_resolving_paths() -> None:
    with pytest.raises(install_methods.InstallMethodError, match="greater than zero"):
        install_methods.verify(Path("missing.whl"), Path("missing-source"), 0)


def test_verify_removes_temporary_root_after_installer_failure(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    artifact = tmp_path / "artifact.whl"
    artifact.touch()
    source_root = tmp_path / "source"
    source_root.mkdir()
    temporary_roots: list[Path] = []

    monkeypatch.setattr(install_methods.shutil, "which", lambda command: f"/{command}")

    def fail_install(*args: object) -> None:
        root = args[3]
        assert isinstance(root, Path)
        temporary_roots.append(root.parent)
        root.mkdir(parents=True)
        (root / "partial-install").touch()
        raise install_methods.InstallMethodError("injected failure")

    monkeypatch.setattr(install_methods, "_verify_pip", fail_install)

    with pytest.raises(install_methods.InstallMethodError, match="injected failure"):
        install_methods.verify(artifact, source_root)

    assert len(temporary_roots) == 1
    assert not temporary_roots[0].exists()


@pytest.mark.skipif(os.name == "nt", reason="POSIX wrapper contract")
def test_uvx_wrapper_preserves_paths_and_forwards_arguments(tmp_path: Path) -> None:
    artifact = tmp_path / "artifact with spaces.whl"
    wrapper = install_methods._uvx_wrapper("/uv path/uvx", artifact, tmp_path)

    assert wrapper.read_text(encoding="utf-8") == (
        "#!/bin/sh\n"
        f"exec '/uv path/uvx' --from '{artifact}' aise \"$@\"\n"
    )
    assert wrapper.stat().st_mode & 0o111
