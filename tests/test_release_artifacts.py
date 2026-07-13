from __future__ import annotations

import io
import tarfile
import zipfile
from pathlib import Path

import pytest

from scripts.verify_release_artifacts import VerificationError, verify

METADATA = b"Name: ai-session-search\nVersion: 1.0.0\nLicense-Expression: Apache-2.0\n\n"


def _wheel(path: Path, *, extra: str | None = None) -> Path:
    files = {
        "ai_session_search/_native.cp312.so": b"native",
        "ai_session_search/_native.pyi": b"",
        "ai_session_search/native.pyi": b"",
        "ai_session_search/py.typed": b"",
        "ai_session_search-1.0.0.dist-info/METADATA": METADATA,
        "ai_session_search-1.0.0.dist-info/licenses/LICENSE": b"license",
        "ai_session_search-1.0.0.dist-info/licenses/NOTICE": b"notice",
    }
    if extra:
        files[extra] = b"forbidden"
    with zipfile.ZipFile(path, "w") as archive:
        for name, content in files.items():
            archive.writestr(name, content)
    return path


def _sdist(path: Path, *, extra: str | None = None) -> Path:
    files = {
        "ai_session_search-1.0.0/PKG-INFO": METADATA,
        "ai_session_search-1.0.0/LICENSE": b"license",
        "ai_session_search-1.0.0/NOTICE": b"notice",
        "ai_session_search-1.0.0/Cargo.lock": b"lock",
        "ai_session_search-1.0.0/pyproject.toml": b"project",
    }
    if extra:
        files[extra] = b"forbidden"
    with tarfile.open(path, "w:gz") as archive:
        for name, content in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
    return path


def test_accepts_complete_wheel_and_sdist(tmp_path: Path) -> None:
    verify(_wheel(tmp_path / "package.whl"))
    verify(_sdist(tmp_path / "package.tar.gz"))


@pytest.mark.parametrize(
    "name",
    ["docs/demo.gif", "../escape", "/absolute/path", r"C:\\absolute\\path"],
)
def test_rejects_media_and_unsafe_wheel_paths(tmp_path: Path, name: str) -> None:
    with pytest.raises(VerificationError):
        verify(_wheel(tmp_path / "package.whl", extra=name))


def test_rejects_stale_package_identity(tmp_path: Path) -> None:
    with pytest.raises(VerificationError, match="stale package identity"):
        verify(_sdist(tmp_path / "package.tar.gz", extra="root/ai_session_tools/cli.py"))


def test_rejects_wheel_without_native_extension(tmp_path: Path) -> None:
    wheel = _wheel(tmp_path / "package.whl")
    with zipfile.ZipFile(wheel, "r") as source:
        contents = {name: source.read(name) for name in source.namelist() if "/_native.cp312.so" not in name}
    with zipfile.ZipFile(wheel, "w") as destination:
        for name, content in contents.items():
            destination.writestr(name, content)
    with pytest.raises(VerificationError, match="native extension"):
        verify(wheel)
