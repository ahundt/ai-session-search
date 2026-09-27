# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

from pathlib import Path

from scripts.verify_upgrade_path import main, problems_in_status


def test_upgrade_status_accepts_current_and_linked_integrations() -> None:
    status = (
        "app ~/.ai-session-search/skills/ai-session-search: configured\n"
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


def test_upgrade_status_fails_closed_when_it_finds_no_installed_skill() -> None:
    # An earlier release that installed nothing, or a changed status wording, must not pass.
    assert problems_in_status("") == [
        "integrations status reported no installed ai-session-search skill to check"
    ]


def test_upgrade_check_refuses_to_guess_the_previous_release(tmp_path: Path) -> None:
    # Before the changelog names the new version, there is no release "below" it; reporting
    # success there would be an upgrade check that checked nothing.
    (tmp_path / "pyproject.toml").write_text('[project]\nversion = "1.0.0rc4"\n', encoding="utf-8")
    (tmp_path / "CHANGELOG.md").write_text(
        "# Changelog\n\n## [Unreleased]\n\n## [1.0.0rc3] - 2026-09-16\n\nNotes.\n", encoding="utf-8"
    )

    assert main(["--executable", "aise", "--root", str(tmp_path)]) == 2


def test_upgrade_status_fails_closed_on_a_line_it_cannot_read() -> None:
    status = (
        "app ~/.ai-session-search/skills/ai-session-search: configured\n"
        "claude ~/.claude/CLAUDE.md needs attention\n"
    )

    assert problems_in_status(status) == ["claude ~/.claude/CLAUDE.md needs attention"]
