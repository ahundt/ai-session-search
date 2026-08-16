# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import base64
import hashlib
import json
import subprocess
import sys
import zipfile
from pathlib import Path

import pytest

from scripts.sanitize_sboms import SanitizationError, sanitize_file, sanitize_wheel_sboms


def test_rewrites_workspace_paths_and_preserves_reference_graph(tmp_path: Path) -> None:
    workspace = tmp_path / "workspace"
    crate = workspace / "rust" / "core"
    crate.mkdir(parents=True)
    reference = f"path+{crate.as_uri()}#core@1.0.0"
    sbom = workspace / "core.cdx.json"
    sbom.write_text(
        json.dumps(
            {
                "metadata": {"component": {"bom-ref": reference}},
                "dependencies": [{"ref": reference, "dependsOn": [reference]}],
            }
        ),
        encoding="utf-8",
    )

    sanitize_file(sbom, workspace, source_date_epoch=1_700_000_000)

    document = json.loads(sbom.read_text(encoding="utf-8"))
    expected = "workspace:rust/core#core@1.0.0"
    assert document["metadata"]["component"]["bom-ref"] == expected
    assert document["dependencies"] == [{"dependsOn": [expected], "ref": expected}]
    assert document["metadata"]["timestamp"] == "2023-11-14T22:13:20Z"
    assert document["serialNumber"].startswith("urn:uuid:")
    assert str(workspace) not in sbom.read_text(encoding="utf-8")

    first = sbom.read_bytes()
    sanitize_file(sbom, workspace, source_date_epoch=1_700_000_000)
    assert sbom.read_bytes() == first


def test_rejects_local_dependency_outside_workspace_without_modifying_file(tmp_path: Path) -> None:
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    sbom = workspace / "core.cdx.json"
    original = json.dumps({"bom-ref": f"path+{outside.as_uri()}#outside@1.0.0"})
    sbom.write_text(original, encoding="utf-8")

    with pytest.raises(SanitizationError, match="outside workspace"):
        sanitize_file(sbom, workspace)

    assert sbom.read_text(encoding="utf-8") == original


def test_rejects_remote_file_dependency_without_modifying_file(tmp_path: Path) -> None:
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    sbom = workspace / "core.cdx.json"
    original = json.dumps(
        {"bom-ref": "path+file://remote.example/workspace/core#core@1.0.0"}
    )
    sbom.write_text(original, encoding="utf-8")

    with pytest.raises(SanitizationError, match=r"remote host: remote\.example"):
        sanitize_file(sbom, workspace)

    assert sbom.read_text(encoding="utf-8") == original


def _record_digest(payload: bytes) -> str:
    return base64.urlsafe_b64encode(hashlib.sha256(payload).digest()).rstrip(b"=").decode()


def _wheel_with_sbom(path: Path, sbom: dict, *, dist_info: str = "pkg-1.0.dist-info") -> str:
    sbom_name = f"{dist_info}/sboms/pkg.cyclonedx.json"
    sbom_bytes = json.dumps(sbom).encode()
    record = f"pkg/__init__.py,sha256=abc,3\n{sbom_name},sha256={_record_digest(sbom_bytes)},{len(sbom_bytes)}\n{dist_info}/RECORD,,\n"
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("pkg/__init__.py", "x=1")
        archive.writestr(sbom_name, sbom_bytes)
        archive.writestr(f"{dist_info}/RECORD", record)
    return sbom_name


def test_wheel_sboms_lose_the_build_machine_path_and_record_follows(tmp_path: Path) -> None:
    """maturin records the checkout as `path+file://<checkout>/...` inside the wheel's PEP 770 SBOM.

    A wheel therefore carries the directory it was built in: the maintainer's home locally, the
    runner's checkout in CI. The build root is rewritten to the same `workspace:` form the separate
    SBOMs get, and RECORD's hash and size for the rewritten member are updated so the wheel stays
    installable. The build clock is untouched, so the SOURCE_DATE_EPOCH check still holds after.
    """
    root = tmp_path / "checkout"
    (root / "rust/pkg").mkdir(parents=True)
    wheel = tmp_path / "pkg-1.0-py3-none-any.whl"
    sbom_name = _wheel_with_sbom(
        wheel,
        {
            "bomFormat": "CycloneDX",
            "metadata": {"timestamp": "2026-01-01T00:00:00Z", "component": {"bom-ref": f"path+file://{root}/rust/pkg#1.0"}},
            "components": [{"purl": "pkg:cargo/serde@1.0", "bom-ref": "pkg:cargo/serde@1.0"}],
        },
    )

    sanitize_wheel_sboms(wheel, root)

    with zipfile.ZipFile(wheel) as archive:
        assert archive.testzip() is None
        sbom_bytes = archive.read(sbom_name)
        document = json.loads(sbom_bytes)
        assert document["metadata"]["component"]["bom-ref"] == "workspace:rust/pkg#1.0"
        assert document["metadata"]["timestamp"] == "2026-01-01T00:00:00Z", "the build clock is untouched"
        assert "path+file://" not in sbom_bytes.decode()
        record = archive.read("pkg-1.0.dist-info/RECORD").decode()
        assert f"{sbom_name},sha256={_record_digest(sbom_bytes)},{len(sbom_bytes)}" in record
        assert "pkg/__init__.py,sha256=abc,3" in record, "other RECORD rows are untouched"
        assert archive.namelist() == ["pkg/__init__.py", sbom_name, "pkg-1.0.dist-info/RECORD"], "member order is preserved"

    first = wheel.read_bytes()
    sanitize_wheel_sboms(wheel, root)
    assert wheel.read_bytes() == first, "a second pass is a no-op, so the step is safe to rerun"


def test_a_member_the_sanitizer_does_not_rewrite_keeps_all_of_its_zip_metadata(tmp_path: Path) -> None:
    """Rewriting the archive has to carry every member's ZIP metadata across, not just the fields
    the sanitizer happens to read.

    A wheel is rewritten member by member because ZIP has no in-place replacement. Anything the
    rewrite drops silently changes an artifact the release then attests to, and a builder or ZIP
    feature this script has never seen is exactly the case where the loss goes unnoticed. The
    fields below are the ones a fresh ZipInfo starts empty: `extra` carries timestamp and Unix uid
    or gid records, and `comment` and `internal_attr` carry per-member metadata.

    Compressed bytes are a separate matter and deliberately unasserted: ZIP records no compression
    level, so a rewrite re-encodes with CPython's deflate rather than the builder's. Measured on
    the published `ai_session_search-1.0.0rc1-cp312-abi3-macosx_11_0_arm64.whl`, that changes the
    compressed size of 14 of 15 members while leaving every member's content identical. What has
    to survive is the content and the recorded metadata, which is what this asserts.
    """
    root = tmp_path / "checkout"
    (root / "rust/pkg").mkdir(parents=True)
    wheel = tmp_path / "pkg-1.0-py3-none-any.whl"
    sbom_name = "pkg-1.0.dist-info/sboms/pkg.cyclonedx.json"
    sbom_bytes = json.dumps({"metadata": {"component": {"bom-ref": f"path+file://{root}/rust/pkg#1.0"}}}).encode()
    untouched = zipfile.ZipInfo("pkg/__init__.py", date_time=(2026, 1, 2, 3, 4, 6))
    untouched.compress_type = zipfile.ZIP_DEFLATED
    untouched.external_attr = 0o644 << 16
    untouched.create_system = 3
    untouched.extra = b"\x01\x00\x02\x00ab"
    untouched.comment = b"c"
    untouched.internal_attr = 7
    with zipfile.ZipFile(wheel, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(untouched, "x=1")
        archive.writestr(sbom_name, sbom_bytes)
        archive.writestr("pkg-1.0.dist-info/RECORD", "pkg/__init__.py,sha256=abc,3\n")

    sanitize_wheel_sboms(wheel, root)

    with zipfile.ZipFile(wheel) as archive:
        assert archive.testzip() is None
        rewritten = archive.getinfo("pkg/__init__.py")
        assert rewritten.extra == b"\x01\x00\x02\x00ab"
        assert rewritten.comment == b"c"
        assert rewritten.internal_attr == 7
        assert rewritten.date_time == (2026, 1, 2, 3, 4, 6)
        assert rewritten.compress_type == zipfile.ZIP_DEFLATED
        assert rewritten.external_attr == 0o644 << 16
        assert rewritten.create_system == 3
        assert archive.read("pkg/__init__.py") == b"x=1"
        assert b"path+file://" not in archive.read(sbom_name), "the SBOM was still sanitized"


def test_every_member_keeps_its_exact_content_through_the_rewrite(tmp_path: Path) -> None:
    """RECORD digests and installers read uncompressed content, so that is what must survive.

    The whole archive is rebuilt because ZIP has no in-place member replacement, which is the step
    that could corrupt a member the sanitizer never meant to touch. The binary payload here holds
    every byte value so a text-mode or encoding slip would show up.
    """
    root = tmp_path / "checkout"
    (root / "rust/pkg").mkdir(parents=True)
    wheel = tmp_path / "pkg-1.0-py3-none-any.whl"
    payloads = {
        "pkg/__init__.py": bytes(range(256)) * 40,
        "pkg/data.bin": b"\x00" * 1024,
        "pkg/text.txt": "café \U0001f600\n".encode(),
    }
    sbom_name = "pkg-1.0.dist-info/sboms/pkg.cyclonedx.json"
    sbom_bytes = json.dumps({"metadata": {"component": {"bom-ref": f"path+file://{root}/rust/pkg#1.0"}}}).encode()
    with zipfile.ZipFile(wheel, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, payload in payloads.items():
            archive.writestr(name, payload)
        archive.writestr(sbom_name, sbom_bytes)
        archive.writestr("pkg-1.0.dist-info/RECORD", "".join(f"{name},sha256=x,{len(payload)}\n" for name, payload in payloads.items()))

    sanitize_wheel_sboms(wheel, root)

    with zipfile.ZipFile(wheel) as archive:
        assert archive.testzip() is None
        for name, payload in payloads.items():
            assert archive.read(name) == payload, name


def test_a_wheel_sbom_path_outside_the_checkout_is_refused_without_modifying_the_wheel(tmp_path: Path) -> None:
    root = tmp_path / "checkout"
    root.mkdir()
    wheel = tmp_path / "pkg-1.0-py3-none-any.whl"
    _wheel_with_sbom(wheel, {"metadata": {"component": {"bom-ref": f"path+file://{tmp_path}/elsewhere#1.0"}}})
    original = wheel.read_bytes()

    with pytest.raises(SanitizationError, match="outside workspace"):
        sanitize_wheel_sboms(wheel, root)

    assert wheel.read_bytes() == original
    assert sorted(tmp_path.iterdir()) == [root, wheel], "no temporary file is left beside the wheel"


def test_command_line_accepts_wheels_beside_loose_sboms(tmp_path: Path) -> None:
    """The wheels job in publish.yml calls the same script the sbom job does, on the wheel it built."""
    root = tmp_path / "checkout"
    (root / "rust/pkg").mkdir(parents=True)
    wheel = tmp_path / "pkg-1.0-py3-none-any.whl"
    _wheel_with_sbom(wheel, {"metadata": {"component": {"bom-ref": f"path+file://{root}/rust/pkg#1.0"}}})
    loose = tmp_path / "loose.cdx.json"
    loose.write_text(json.dumps({"bom-ref": f"path+file://{root}/rust/pkg#1.0"}), encoding="utf-8")

    result = subprocess.run(
        [sys.executable, "scripts/sanitize_sboms.py", "--root", str(root), str(wheel), str(loose)],
        check=True,
        capture_output=True,
        text=True,
        cwd=Path(__file__).resolve().parents[1],
    )

    assert result.stdout.splitlines() == [f"sanitized: {wheel}", f"sanitized: {loose}"]
    with zipfile.ZipFile(wheel) as archive:
        assert b"path+file://" not in archive.read("pkg-1.0.dist-info/sboms/pkg.cyclonedx.json")
    assert json.loads(loose.read_text(encoding="utf-8")) == {"bom-ref": "workspace:rust/pkg#1.0"}
