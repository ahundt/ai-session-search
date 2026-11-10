# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Select temporary application, provider, skill, and harness roots before collection."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

import pytest

from scripts.verify_harness_discovery import environment

# Demo globals capture the environment during collection.
_COLLECTION_ROOT = tempfile.TemporaryDirectory(prefix="aise-pytest-")
_EXECUTION_ENV = environment(Path(_COLLECTION_ROOT.name))
os.environ.clear()
os.environ.update(_EXECUTION_ENV)


@pytest.fixture(autouse=True)
def _isolated_aise_config(
    tmp_path_factory: pytest.TempPathFactory, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A root of its own, not under tmp_path: tests that assert what tmp_path holds must not see it.
    for key, value in environment(tmp_path_factory.mktemp("runtime")).items():
        monkeypatch.setenv(key, value)
