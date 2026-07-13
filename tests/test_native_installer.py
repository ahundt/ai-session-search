from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

import pytest


@pytest.mark.skipif(os.name != "posix", reason="POSIX installer contract")
def test_native_installer_requires_explicit_safe_replacement(tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    installer = bundle / "install.sh"
    shutil.copyfile(Path("scripts/install-native.sh"), installer)
    source = bundle / "aise"
    source.write_text("#!/bin/sh\necho first\n", encoding="utf-8")
    source.chmod(0o755)
    bin_dir = tmp_path / "bin"

    initial = subprocess.run(
        ["sh", str(installer), "--bin-dir", str(bin_dir)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert initial.returncode == 0, initial.stderr
    assert (bin_dir / "aise").read_bytes() == source.read_bytes()

    duplicate = subprocess.run(
        ["sh", str(installer), "--bin-dir", str(bin_dir)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert duplicate.returncode != 0
    assert "already exists" in duplicate.stderr

    original = (bin_dir / "aise").read_bytes()
    source.write_text("#!/bin/sh\necho second\n", encoding="utf-8")
    source.chmod(0o755)
    missing_backup = subprocess.run(
        ["sh", str(installer), "--bin-dir", str(bin_dir), "--replace"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert missing_backup.returncode != 0
    assert "requires an explicit --backup" in missing_backup.stderr
    assert (bin_dir / "aise").read_bytes() == original

    backup = tmp_path / "rollback" / "aise"
    replaced = subprocess.run(
        [
            "sh",
            str(installer),
            "--bin-dir",
            str(bin_dir),
            "--replace",
            "--backup",
            str(backup),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert replaced.returncode == 0, replaced.stderr
    assert backup.read_bytes() == original
    assert (bin_dir / "aise").read_bytes() == source.read_bytes()


@pytest.mark.skipif(os.name != "posix", reason="POSIX installer contract")
def test_native_installer_rejects_symbolic_link_destination(tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    installer = bundle / "install.sh"
    shutil.copyfile(Path("scripts/install-native.sh"), installer)
    source = bundle / "aise"
    source.write_text("#!/bin/sh\necho source\n", encoding="utf-8")
    source.chmod(0o755)
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    external = tmp_path / "external"
    external.write_text("keep", encoding="utf-8")
    (bin_dir / "aise").symlink_to(external)

    completed = subprocess.run(
        [
            "sh",
            str(installer),
            "--bin-dir",
            str(bin_dir),
            "--replace",
            "--backup",
            str(tmp_path / "backup"),
        ],
        capture_output=True,
        text=True,
        check=False,
    )

    assert completed.returncode != 0
    assert "symbolic-link" in completed.stderr
    assert external.read_text(encoding="utf-8") == "keep"
