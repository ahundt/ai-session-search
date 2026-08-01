# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import io
import stat
import tarfile
import zipfile
from pathlib import Path

import pytest

from scripts.package_native_release import PackagingError, package_native_release
from scripts.verify_release_artifacts import VerificationError, verify, verify_release_set

METADATA = b"Name: ai-session-search\nVersion: 1.0.0\nLicense-Expression: Apache-2.0\n\n"


def _wheel(
    path: Path,
    *,
    entry_points: bytes = b"[console_scripts]\naise=ai_session_search.entrypoint:cli_main\n",
    extra: str | None = None,
    omit: str | None = None,
    wheel_tag: str = "cp312-abi3-macosx_11_0_arm64",
) -> Path:
    files = {
        "ai_session_search/_native.cp312.so": b"native",
        "ai_session_search/__init__.py": b"",
        "ai_session_search/_native.pyi": b"",
        "ai_session_search/native.pyi": b"",
        "ai_session_search/py.typed": b"",
        "ai_session_search-1.0.0.dist-info/METADATA": METADATA,
        "ai_session_search-1.0.0.dist-info/WHEEL": (
            f"Wheel-Version: 1.0\nRoot-Is-Purelib: false\nTag: {wheel_tag}\n\n".encode()
        ),
        "ai_session_search-1.0.0.dist-info/entry_points.txt": entry_points,
        "ai_session_search-1.0.0.dist-info/licenses/LICENSE": b"license",
        "ai_session_search-1.0.0.dist-info/licenses/NOTICE": b"notice",
    }
    if omit is not None:
        files.pop(omit, None)
    if extra:
        files[extra] = b"forbidden"
    with zipfile.ZipFile(path, "w") as archive:
        for name, content in files.items():
            archive.writestr(name, content)
    return path


def _sdist(
    path: Path,
    *,
    core_manifest: bytes = b"[package]\nname = \"ai-session-search\"\n",
    extra: str | None = None,
    omit: str | None = None,
) -> Path:
    files = {
        "ai_session_search-1.0.0/PKG-INFO": METADATA,
        "ai_session_search-1.0.0/LICENSE": b"license",
        "ai_session_search-1.0.0/NOTICE": b"notice",
        "ai_session_search-1.0.0/Cargo.lock": b"lock",
        "ai_session_search-1.0.0/Cargo.toml": b"workspace",
        "ai_session_search-1.0.0/pyproject.toml": b"project",
        "ai_session_search-1.0.0/ai_session_search/__init__.py": b"",
        "ai_session_search-1.0.0/ai_session_search/_native.pyi": b"",
        "ai_session_search-1.0.0/rust/ai-session-search-core/src/lib.rs": b"",
        "ai_session_search-1.0.0/rust/ai-session-search-core/Cargo.toml": core_manifest,
        "ai_session_search-1.0.0/rust/ai-session-search-python/src/lib.rs": b"",
        "ai_session_search-1.0.0/rust/ai-session-search-python/Cargo.toml": b"python",
    }
    if omit is not None:
        files.pop(omit, None)
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


def test_rejects_interpreter_specific_wheel_that_excludes_newer_python(
    tmp_path: Path,
) -> None:
    with pytest.raises(VerificationError, match=r"3\.12\+ abi3"):
        verify(
            _wheel(
                tmp_path / "package.whl",
                wheel_tag="cp312-cp312-macosx_11_0_arm64",
            )
        )


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


def test_rejects_wheel_without_aise_entry_point(tmp_path: Path) -> None:
    wheel = _wheel(
        tmp_path / "package.whl",
        omit="ai_session_search-1.0.0.dist-info/entry_points.txt",
    )
    with pytest.raises(VerificationError, match="entry_points"):
        verify(wheel)


@pytest.mark.parametrize(
    "entry_points",
    [
        b"[console_scripts]\naise=ai_session_search.cli:cli_main\n",
        b"[console_scripts]\naise=ai_session_search.entrypoint:cli_main\naise-mcp=old:main\n",
    ],
)
def test_rejects_obsolete_or_second_console_entry_point(
    tmp_path: Path, entry_points: bytes
) -> None:
    with pytest.raises(VerificationError, match="expected only"):
        verify(_wheel(tmp_path / "package.whl", entry_points=entry_points))


def test_rejects_sdist_with_removed_mcp_binary(tmp_path: Path) -> None:
    manifest = b'[[bin]]\nname = "aise-mcp"\npath = "src/mcp.rs"\n'
    with pytest.raises(VerificationError, match="removed aise-mcp"):
        verify(_sdist(tmp_path / "package.tar.gz", core_manifest=manifest))


def test_rejects_sdist_without_build_critical_rust_source(tmp_path: Path) -> None:
    sdist = _sdist(
        tmp_path / "package.tar.gz",
        omit="ai_session_search-1.0.0/rust/ai-session-search-core/src/lib.rs",
    )
    with pytest.raises(VerificationError, match=r"ai-session-search-core/src/lib\.rs"):
        verify(sdist)


def test_rejects_duplicate_wheel_member(tmp_path: Path) -> None:
    wheel = _wheel(tmp_path / "package.whl")
    with pytest.warns(UserWarning, match="Duplicate name"):
        with zipfile.ZipFile(wheel, "a") as archive:
            archive.writestr("ai_session_search/__init__.py", b"duplicate")
    with pytest.raises(VerificationError, match="duplicate archive member"):
        verify(wheel)


def test_rejects_duplicate_sdist_member(tmp_path: Path) -> None:
    sdist = tmp_path / "package.tar.gz"
    with tarfile.open(sdist, "w:gz") as archive:
        for content in (b"first", b"second"):
            info = tarfile.TarInfo("ai_session_search-1.0.0/PKG-INFO")
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
    with pytest.raises(VerificationError, match="duplicate archive member"):
        verify(sdist)


def test_rejects_wheel_filename_tag_mismatch(tmp_path: Path) -> None:
    wheel = _wheel(
        tmp_path / "ai_session_search-1.0.0-cp312-abi3-macosx_11_0_arm64.whl",
        wheel_tag="cp312-abi3-win_amd64",
    )
    with pytest.raises(VerificationError, match="filename tag"):
        verify(wheel)


def test_rejects_wheel_filename_metadata_version_mismatch(tmp_path: Path) -> None:
    wheel = _wheel(
        tmp_path / "ai_session_search-2.0.0-cp312-abi3-win_amd64.whl",
        wheel_tag="cp312-abi3-win_amd64",
    )
    with pytest.raises(VerificationError, match="metadata version"):
        verify(wheel)


def test_rejects_sdist_filename_root_version_mismatch(tmp_path: Path) -> None:
    sdist = _sdist(tmp_path / "ai_session_search-2.0.0.tar.gz")
    with pytest.raises(VerificationError, match="archive root"):
        verify(sdist)


def test_rejects_wheel_symbolic_link_member(tmp_path: Path) -> None:
    wheel = _wheel(tmp_path / "package.whl")
    with zipfile.ZipFile(wheel, "a") as archive:
        link = zipfile.ZipInfo("ai_session_search/link")
        link.create_system = 3
        link.external_attr = (stat.S_IFLNK | 0o777) << 16
        archive.writestr(link, b"target")
    with pytest.raises(VerificationError, match="regular files"):
        verify(wheel)


def test_rejects_nonregular_sdist_member(tmp_path: Path) -> None:
    sdist = _sdist(tmp_path / "package.tar.gz")
    with tarfile.open(sdist, "r:gz") as source:
        files: dict[str, bytes] = {}
        for member in source.getmembers():
            if member.isfile():
                extracted = source.extractfile(member)
                assert extracted is not None
                files[member.name] = extracted.read()
    with tarfile.open(sdist, "w:gz") as archive:
        for name, content in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
        fifo = tarfile.TarInfo("ai_session_search-1.0.0/fifo")
        fifo.type = tarfile.FIFOTYPE
        archive.addfile(fifo)
    with pytest.raises(VerificationError, match="regular files"):
        verify(sdist)


def _crate(path: Path, *, extra: str | None = None, omit: str | None = None) -> Path:
    root = "ai-session-search-1.0.0"
    files = {
        f"{root}/Cargo.toml": b'[package]\nname="ai-session-search"\nversion="1.0.0"\n',
        f"{root}/Cargo.toml.orig": b'[package]\nname="ai-session-search"\nversion="1.0.0"\n',
        f"{root}/Cargo.lock": b"lock",
        f"{root}/LICENSE": b"license",
        f"{root}/NOTICE": b"notice",
        f"{root}/README.md": b"readme",
        f"{root}/config.example.toml": b"config",
        f"{root}/src/lib.rs": b"",
        f"{root}/src/main.rs": b"",
    }
    if omit is not None:
        files.pop(f"{root}/{omit}")
    if extra is not None:
        files[f"{root}/{extra}"] = b"extra"
    with tarfile.open(path, "w:gz") as archive:
        for name, content in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
    return path


def test_crate_requires_notice_and_rejects_development_files(tmp_path: Path) -> None:
    verify(_crate(tmp_path / "ai-session-search-1.0.0.crate"))
    missing_dir = tmp_path / "missing"
    missing_dir.mkdir()
    with pytest.raises(VerificationError, match="NOTICE"):
        verify(_crate(missing_dir / "ai-session-search-1.0.0.crate", omit="NOTICE"))
    dev_dir = tmp_path / "dev"
    dev_dir.mkdir()
    with pytest.raises(VerificationError, match="development-only"):
        verify(_crate(dev_dir / "ai-session-search-1.0.0.crate", extra="flake.nix"))


def test_release_set_requires_every_target_and_metadata(tmp_path: Path) -> None:
    version = "1.0.0rc1"
    cargo_version = "1.0.0-rc.1"
    artifacts = [
        tmp_path / f"ai_session_search-{version}-cp312-abi3-manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
        tmp_path / f"ai_session_search-{version}-cp312-abi3-manylinux_2_17_aarch64.manylinux2014_aarch64.whl",
        tmp_path / f"ai_session_search-{version}-cp312-abi3-macosx_11_0_arm64.whl",
        tmp_path / f"ai_session_search-{version}-cp312-abi3-macosx_10_12_x86_64.whl",
        tmp_path / f"ai_session_search-{version}-cp312-abi3-win_amd64.whl",
        tmp_path / f"ai_session_search-{version}.tar.gz",
        tmp_path / f"ai-session-search-{cargo_version}.crate",
        *[
            tmp_path / f"ai-session-search-{version}-{target}.{suffix}"
            for target, suffix in (
                ("x86_64-unknown-linux-gnu", "tar.gz"),
                ("aarch64-unknown-linux-gnu", "tar.gz"),
                ("aarch64-apple-darwin", "tar.gz"),
                ("x86_64-apple-darwin", "tar.gz"),
                ("x86_64-pc-windows-msvc", "zip"),
            )
        ],
        tmp_path / "ai-session-search-python-runtime.cdx.json",
        tmp_path / "ai-session-search.cdx.json",
        tmp_path / "ai-session-search-python.cdx.json",
        tmp_path / "python-runtime-licenses.md",
        tmp_path / "rust-dependency-licenses.txt",
    ]
    for artifact in artifacts:
        artifact.touch()
    verify_release_set(artifacts, version)
    with pytest.raises(VerificationError, match="release artifact set differs"):
        verify_release_set(artifacts[:-1], version)


def test_release_set_rejects_wrong_wheel_platform_or_version(tmp_path: Path) -> None:
    artifacts = [tmp_path / "ai_session_search-2.0.0-cp312-abi3-win32.whl"]
    with pytest.raises(VerificationError, match="release artifact set differs"):
        verify_release_set(artifacts, "1.0.0")


@pytest.mark.parametrize(
    ("archive_format", "binary_name", "target"),
    [
        ("tar.gz", "aise", "aarch64-apple-darwin"),
        ("zip", "aise.exe", "x86_64-pc-windows-msvc"),
    ],
)
def test_native_archives_are_deterministic_and_verifiable(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    archive_format: str,
    binary_name: str,
    target: str,
) -> None:
    binary = tmp_path / binary_name
    binary.write_bytes(b"native executable")
    license_file = tmp_path / "LICENSE"
    license_file.write_text("Apache-2.0", encoding="utf-8")
    notice = tmp_path / "NOTICE"
    notice.write_text("attribution", encoding="utf-8")
    installer = tmp_path / ("install.ps1" if binary_name.endswith(".exe") else "install.sh")
    installer.write_text("installer", encoding="utf-8")
    monkeypatch.setenv("SOURCE_DATE_EPOCH", "1700000000")

    first = package_native_release(
        binary,
        license_file,
        notice,
        installer,
        tmp_path / "first",
        "1.0.0",
        target,
        archive_format,
    )
    second = package_native_release(
        binary,
        license_file,
        notice,
        installer,
        tmp_path / "second",
        "1.0.0",
        target,
        archive_format,
    )

    assert first.read_bytes() == second.read_bytes()
    verify(first)


def test_native_packaging_rejects_unsafe_identity_and_overwrite(tmp_path: Path) -> None:
    binary = tmp_path / "aise"
    binary.write_bytes(b"native executable")
    license_file = tmp_path / "LICENSE"
    license_file.write_text("license", encoding="utf-8")
    notice = tmp_path / "NOTICE"
    notice.write_text("notice", encoding="utf-8")
    installer = tmp_path / "install.sh"
    installer.write_text("installer", encoding="utf-8")

    with pytest.raises(PackagingError, match="safe path component"):
        package_native_release(
            binary,
            license_file,
            notice,
            installer,
            tmp_path / "dist",
            "../escape",
            "target",
            "tar.gz",
        )

    package_native_release(
        binary,
        license_file,
        notice,
        installer,
        tmp_path / "dist",
        "1.0.0",
        "target",
        "tar.gz",
    )
    with pytest.raises(FileExistsError):
        package_native_release(
            binary,
            license_file,
            notice,
            installer,
            tmp_path / "dist",
            "1.0.0",
            "target",
            "tar.gz",
        )
    assert not any((tmp_path / "dist").glob("*.staging"))


def test_native_zip_rejects_symbolic_link_member(tmp_path: Path) -> None:
    archive_path = tmp_path / "ai-session-search-1.0.0-x86_64-pc-windows-msvc.zip"
    root = "ai-session-search-1.0.0-x86_64-pc-windows-msvc"
    with zipfile.ZipFile(archive_path, "w") as archive:
        archive.writestr(f"{root}/LICENSE", b"license")
        archive.writestr(f"{root}/NOTICE", b"notice")
        archive.writestr(f"{root}/install.ps1", b"installer")
        link = zipfile.ZipInfo(f"{root}/aise.exe")
        link.create_system = 3
        link.external_attr = 0o120777 << 16
        archive.writestr(link, b"target")

    with pytest.raises(VerificationError, match="regular files"):
        verify(archive_path)
