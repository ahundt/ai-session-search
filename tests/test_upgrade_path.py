# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

from scripts.verify_upgrade_path import problems_in_status


def test_upgrade_status_accepts_current_and_linked_integrations() -> None:
    status = (
        "claude code modern ~/.claude.json: configured\n"
        "app discovery ~/.claude/skills/ai-session-search: linked -> ~/.ai-session-search/skills\n"
        "executable alias /build/aisearch: missing\n"
    )

    assert problems_in_status(status) == []


def test_upgrade_status_reports_anything_left_for_the_user_to_fix() -> None:
    # 1.0.0rc1's instruction blocks read "outdated" after upgrading; every such line would have
    # needed `aise integrations install`.
    status = (
        "claude ~/.claude/CLAUDE.md: outdated\n"
        "app ~/.ai-session-search/skills/ai-session-search: outdated, untouched\n"
        "codex ~/.codex/config.toml: configured\n"
    )

    assert problems_in_status(status) == [
        "claude ~/.claude/CLAUDE.md: outdated",
        "app ~/.ai-session-search/skills/ai-session-search: outdated, untouched",
    ]
