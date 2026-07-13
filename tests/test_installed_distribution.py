from __future__ import annotations

import subprocess
from pathlib import Path

import pytest

from scripts import verify_installed_distribution as verifier


def test_source_native_import_accepts_only_module_in_source_package(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    package = tmp_path / "ai_session_search"
    package.mkdir()
    module = package / "_native.abi3.so"
    module.touch()

    monkeypatch.setattr(
        verifier,
        "_run_command",
        lambda *_args, **_kwargs: subprocess.CompletedProcess(
            args=[], returncode=0, stdout=f"{module}\n", stderr=""
        ),
    )

    assert verifier.verify_source_native_import(tmp_path, 2.0) == module


def test_source_native_import_rejects_module_outside_source_package(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    (tmp_path / "ai_session_search").mkdir()
    outside = tmp_path / "installed" / "_native.abi3.so"
    outside.parent.mkdir()
    outside.touch()
    monkeypatch.setattr(
        verifier,
        "_run_command",
        lambda *_args, **_kwargs: subprocess.CompletedProcess(
            args=[], returncode=0, stdout=f"{outside}\n", stderr=""
        ),
    )

    with pytest.raises(verifier.InstallVerificationError, match="not imported from"):
        verifier.verify_source_native_import(tmp_path, 2.0)


def test_source_native_import_rejects_nonpositive_timeout(tmp_path: Path) -> None:
    with pytest.raises(verifier.InstallVerificationError, match="greater than zero"):
        verifier.verify_source_native_import(tmp_path, 0)
