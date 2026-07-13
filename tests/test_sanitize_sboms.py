from __future__ import annotations

import json
from pathlib import Path

import pytest

from scripts.sanitize_sboms import SanitizationError, sanitize_file


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
