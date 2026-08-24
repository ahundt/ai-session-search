# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Trusted permission-adapter contracts at the Python embedding boundary."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from ai_session_search import native


def _config(tmp_path: Path) -> Path:
    path = tmp_path / "config.toml"
    path.write_text(
        f"""
[index]
db_path = {str(tmp_path / "index.db")!r}
cache_dir = {str(tmp_path / "cache")!r}

[search.permissions]
default_profile = "native"

[search.permissions.profiles.native]
default = "hard-block"

[[search.permissions.profiles.native.rules]]
rule_id = "native-model"
effect = "allow"
resource = "session"
caller_harness = ["pi"]
caller_model_id = ["openai/gpt-5"]
session_workspace_root = ["/work"]
""",
        encoding="utf-8",
    )
    return path


def test_python_embedding_rejects_unsupported_native_attestation_version(
    tmp_path: Path,
) -> None:
    attestation = json.dumps(
        {
            "version": 2,
            "generation": 1,
            "harness": "pi",
            "model_id": "openai/gpt-5",
            "working_directory": None,
            "session_binding": "session-1",
        }
    )
    with pytest.raises(ValueError, match="unsupported native adapter attestation version 2"):
        native.SessionSearch(
            config_path=_config(tmp_path),
            trusted_adapter_attestation_json=attestation,
        )


def test_python_embedding_accepts_versioned_native_attestation(tmp_path: Path) -> None:
    attestation = json.dumps(
        {
            "version": 1,
            "generation": 3,
            "harness": "pi",
            "model_id": "openai/gpt-5",
            "working_directory": "/work",
            "session_binding": "session-1",
        }
    )
    search = native.SessionSearch(
        config_path=_config(tmp_path),
        trusted_adapter_attestation_json=attestation,
    )
    assert search.db_path == tmp_path / "index.db"
