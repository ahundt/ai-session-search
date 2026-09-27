# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Keep every test away from the developer's real aise state.

`aise` commands run through the extension, including the post-upgrade skill refresh, read and
write beside the resolved config file. CI and `run_ci_local.sh` export an isolated
`AI_SESSION_SEARCH_CONFIG`, but a direct `pytest` run would otherwise resolve the real
`~/.ai-session-search/config.toml`. A test that needs a specific config sets its own, which
replaces this one.
"""

from __future__ import annotations

from collections.abc import Iterator
from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def _isolated_aise_config(tmp_path_factory: pytest.TempPathFactory, monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    # An explicitly named config must exist; an empty file is every default, as CI's is.
    config = Path(tmp_path_factory.mktemp("aise-config")) / "config.toml"
    config.write_text("", encoding="utf-8")
    monkeypatch.setenv("AI_SESSION_SEARCH_CONFIG", str(config))
    yield
