# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import importlib.util
from copy import deepcopy
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

ROOT = Path(__file__).resolve().parents[1]


def load_renderer() -> ModuleType:
    path = ROOT / "scripts/render_message_search_docs.py"
    spec = importlib.util.spec_from_file_location("render_message_search_docs", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fake_specs() -> dict[str, dict[str, Any]]:
    registry = {
        "purpose": "Search indexed AI-session messages.",
        "parameters": [
            {
                "parameter": "query_mode",
                "domain": {"kind": "enum", "accepted_values": ["literal", "regex", "fuzzy"]},
                "omission": "typed_default",
            },
            {
                "parameter": "providers",
                "domain": {"kind": "non_empty_set", "accepted_values": ["claude", "codex"]},
                "omission": "all_eligible",
            },
        ],
        "precedence": ["explicit", "surface_config", "typed_default"],
        "rules": [{"rule": "sequence_requires_session", "message": "sequence bounds require one session"}],
    }

    def specification(surface: str) -> dict[str, Any]:
        extent = {"kind": "page", "limit": 20, "offset": 0} if surface == "mcp" else {"kind": "all_results", "offset": 0}
        return {
            "registry": deepcopy(registry),
            "configured_default": {
                "extent": extent,
                "context": {"messages_before": 0, "messages_after": 0},
                "presentation": {
                    "lines_per_message": 0,
                    "field_view": {"kind": "max_chars", "max_chars": 500} if surface == "mcp" else {"kind": "no_char_limit"},
                    "match_view": {"kind": "max_chars", "max_chars": 220},
                },
                "include": [],
                "receipt_level": "none",
            },
        }

    return {surface: specification(surface) for surface in ("rust", "cli", "mcp", "python")}


def marked_document(prefix: str, suffix: str) -> str:
    return f"{prefix}\n<!-- aise-message-search-contract:start -->\nstale\n<!-- aise-message-search-contract:end -->\n{suffix}\n"


def test_renderer_is_deterministic_and_preserves_every_byte_outside_markers() -> None:
    renderer = load_renderer()
    documents = {path: marked_document(f"before:{path.name}", f"after:{path.name}") for path in renderer.TARGETS}

    first = renderer.render_targets(fake_specs(), documents)
    second = renderer.render_targets(fake_specs(), documents)

    assert first == second
    for path, rendered in first.items():
        assert rendered.startswith(f"before:{path.name}\n")
        assert rendered.endswith(f"\nafter:{path.name}\n")
        assert "stale" not in rendered
        assert rendered.count(renderer.START) == 1
        assert rendered.count(renderer.END) == 1
    assert "| mcp | page of 20; offset 0 |" in first[ROOT / "README.md"]
    assert "`sequence_requires_session` — sequence bounds require one session." in first[ROOT / "docs/README.md"]


def test_renderer_rejects_cross_surface_registry_drift_and_ambiguous_markers() -> None:
    renderer = load_renderer()
    specs = fake_specs()
    specs["python"]["registry"]["precedence"].append("derived")
    documents = {path: marked_document("before", "after") for path in renderer.TARGETS}

    with pytest.raises(ValueError, match="python: parameter registry differs"):
        renderer.render_targets(specs, documents)
    with pytest.raises(ValueError, match="expected exactly one"):
        renderer.replace_region("no markers", "body", Path("README.md"))


def test_renderer_rejects_a_rule_without_its_executable_message() -> None:
    renderer = load_renderer()
    specs = fake_specs()
    del specs["cli"]["registry"]["rules"][0]["message"]

    with pytest.raises(KeyError, match="message"):
        renderer.docs_block(specs, specs["cli"]["registry"])
