# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import os
import re
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
    receipt = bundle / "aise-native-install.json"
    receipt.write_text('{"schema_version":1,"archive_version":"first"}\n', encoding="utf-8")
    bin_dir = tmp_path / "bin"

    initial = subprocess.run(
        ["sh", str(installer), "--bin-dir", str(bin_dir)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert initial.returncode == 0, initial.stderr
    assert (bin_dir / "aise").read_bytes() == source.read_bytes()
    assert (bin_dir / "aise-native-install.json").read_bytes() == receipt.read_bytes()

    duplicate = subprocess.run(
        ["sh", str(installer), "--bin-dir", str(bin_dir)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert duplicate.returncode != 0
    assert "already exists" in duplicate.stderr

    original = (bin_dir / "aise").read_bytes()
    original_receipt = (bin_dir / "aise-native-install.json").read_bytes()
    source.write_text("#!/bin/sh\necho second\n", encoding="utf-8")
    source.chmod(0o755)
    receipt.write_text(
        '{"schema_version":1,"archive_version":"second"}\n', encoding="utf-8"
    )
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
    backup_receipt = Path(f"{backup}.aise-native-install.json")
    assert backup_receipt.read_bytes() == original_receipt
    assert (bin_dir / "aise").read_bytes() == source.read_bytes()
    assert (bin_dir / "aise-native-install.json").read_bytes() == receipt.read_bytes()

    shutil.copyfile(backup, bin_dir / "aise")
    shutil.copyfile(backup_receipt, bin_dir / "aise-native-install.json")
    assert (bin_dir / "aise").read_bytes() == original
    assert (bin_dir / "aise-native-install.json").read_bytes() == original_receipt


def test_windows_native_installer_uses_windows_powershell_51_file_moves() -> None:
    installer = Path("scripts/install-native.ps1").read_text(encoding="utf-8")

    assert "function Move-FileCompatible" in installer
    assert not re.search(r"\[System\.IO\.File\]::Move\([^\n]+,\s*\$true\)", installer)


@pytest.mark.skipif(os.name != "posix", reason="POSIX installer contract")
def test_native_installer_migrates_symbolic_link_with_rollback_copy(tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    installer = bundle / "install.sh"
    shutil.copyfile(Path("scripts/install-native.sh"), installer)
    source = bundle / "aise"
    source.write_text("#!/bin/sh\necho source\n", encoding="utf-8")
    source.chmod(0o755)
    (bundle / "aise-native-install.json").write_text(
        '{"schema_version":1}\n', encoding="utf-8"
    )
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    external = tmp_path / "external"
    external.write_text("keep", encoding="utf-8")
    (bin_dir / "aise").symlink_to(external)

    without_replacement = subprocess.run(
        ["sh", str(installer), "--bin-dir", str(bin_dir)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert without_replacement.returncode != 0
    assert "already exists" in without_replacement.stderr
    assert (bin_dir / "aise").is_symlink()

    backup = tmp_path / "backup"
    completed = subprocess.run(
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

    assert completed.returncode == 0, completed.stderr
    assert not (bin_dir / "aise").is_symlink()
    assert (bin_dir / "aise").read_bytes() == source.read_bytes()
    assert backup.is_symlink()
    assert os.readlink(backup) == str(external)
    assert external.read_text(encoding="utf-8") == "keep"


@pytest.mark.skipif(os.name != "posix", reason="POSIX installer contract")
@pytest.mark.parametrize(
    ("failure_action", "expected_returncode"),
    [
        ("exit 73", 73),
        ('kill -TERM "$PPID"; exit 74', 1),
    ],
)
def test_native_installer_restores_symbolic_link_when_publish_fails(
    tmp_path: Path,
    failure_action: str,
    expected_returncode: int,
) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    installer = bundle / "install.sh"
    shutil.copyfile(Path("scripts/install-native.sh"), installer)
    source = bundle / "aise"
    source.write_text("#!/bin/sh\necho source\n", encoding="utf-8")
    source.chmod(0o755)
    (bundle / "aise-native-install.json").write_text(
        '{"schema_version":1}\n', encoding="utf-8"
    )
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    external = tmp_path / "external"
    external.write_text("keep", encoding="utf-8")
    destination = bin_dir / "aise"
    destination.symlink_to(external)
    backup = tmp_path / "backup"

    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    counter = tmp_path / "mv-count"
    mv_shim = shim_dir / "mv"
    mv_shim.write_text(
        "#!/bin/sh\n"
        'count=$(cat "$AISE_MV_COUNTER" 2>/dev/null || printf 0)\n'
        "count=$((count + 1))\n"
        'printf "%s" "$count" > "$AISE_MV_COUNTER"\n'
        f'[ "$count" -ne 2 ] || {{ {failure_action}; }}\n'
        'exec /bin/mv "$@"\n',
        encoding="utf-8",
    )
    mv_shim.chmod(0o755)
    env = os.environ.copy()
    env["PATH"] = f"{shim_dir}:{env['PATH']}"
    env["AISE_MV_COUNTER"] = str(counter)

    completed = subprocess.run(
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
        env=env,
    )

    assert completed.returncode == expected_returncode
    assert destination.is_symlink()
    assert os.readlink(destination) == str(external)
    assert not backup.exists()
    assert not backup.is_symlink()
    assert external.read_text(encoding="utf-8") == "keep"


@pytest.mark.skipif(os.name != "posix", reason="POSIX installer contract")
def test_native_installer_restores_executable_and_receipt_after_post_move_signal(
    tmp_path: Path,
) -> None:
    bundle = tmp_path / "bundle"
    bundle.mkdir()
    installer = bundle / "install.sh"
    shutil.copyfile(Path("scripts/install-native.sh"), installer)
    source = bundle / "aise"
    source.write_text("#!/bin/sh\necho new\n", encoding="utf-8")
    source.chmod(0o755)
    (bundle / "aise-native-install.json").write_text("new receipt\n", encoding="utf-8")
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    destination = bin_dir / "aise"
    destination.write_text("#!/bin/sh\necho old\n", encoding="utf-8")
    destination.chmod(0o755)
    receipt = bin_dir / "aise-native-install.json"
    receipt.write_text("old receipt\n", encoding="utf-8")
    backup = tmp_path / "rollback" / "aise"

    shim_dir = tmp_path / "shim"
    shim_dir.mkdir()
    counter = tmp_path / "mv-count"
    mv_shim = shim_dir / "mv"
    mv_shim.write_text(
        "#!/bin/sh\n"
        'count=$(cat "$AISE_MV_COUNTER" 2>/dev/null || printf 0)\n'
        "count=$((count + 1))\n"
        'printf "%s" "$count" > "$AISE_MV_COUNTER"\n'
        'if [ "$count" -eq 2 ]; then\n'
        '  /bin/mv "$@"\n'
        '  kill -TERM "$PPID"\n'
        "  exit 74\n"
        "fi\n"
        'exec /bin/mv "$@"\n',
        encoding="utf-8",
    )
    mv_shim.chmod(0o755)
    env = os.environ.copy()
    env["PATH"] = f"{shim_dir}:{env['PATH']}"
    env["AISE_MV_COUNTER"] = str(counter)

    completed = subprocess.run(
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
        env=env,
    )

    assert completed.returncode == 1
    assert destination.read_text(encoding="utf-8") == "#!/bin/sh\necho old\n"
    assert receipt.read_text(encoding="utf-8") == "old receipt\n"
    assert not backup.exists()
    assert not Path(f"{backup}.aise-native-install.json").exists()
