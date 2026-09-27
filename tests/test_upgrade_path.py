# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

from pathlib import Path

import pytest
from pytest import MonkeyPatch

from scripts import verify_upgrade_path
from scripts.verify_upgrade_path import problems_in_status


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


def test_upgrade_check_fails_when_it_cannot_learn_what_to_upgrade_from(monkeypatch: MonkeyPatch) -> None:
    # Without --from the check upgrades from every PyPI release since the floor. When that fails,
    # reporting success would be an upgrade check that checked nothing.
    def unreachable() -> dict[str, object]:
        raise OSError("network is unreachable")

    monkeypatch.setattr(verify_upgrade_path, "_published", unreachable)

    # _versions_to_check rather than main(): main() returns 2 on Windows before reaching this, so
    # asserting its exit code there would pass without testing anything.
    assert verify_upgrade_path._versions_to_check(None, "1.0.0rc2") is None


def test_since_checks_every_published_release_from_the_floor(monkeypatch: MonkeyPatch) -> None:
    # A key or file any of these releases wrote must keep working, not only the latest one's.
    published: dict[str, object] = {"releases": {"1.0.0rc1": [], "1.0.0rc10": [], "1.0.0rc2": [], "1.0.0rc3": [], "junk": []}}
    monkeypatch.setattr(verify_upgrade_path, "_published", lambda: published)

    assert verify_upgrade_path.published_versions_since("1.0.0rc2") == ["1.0.0rc2", "1.0.0rc3", "1.0.0rc10"]


def test_upgrade_check_refuses_to_touch_a_real_windows_profile(monkeypatch: MonkeyPatch) -> None:
    # aise finds the Windows home folder through the known-folder API, which HOME and USERPROFILE
    # cannot redirect, so running the earlier release there would install into the real profile.
    monkeypatch.setattr(verify_upgrade_path.os, "name", "nt")

    assert verify_upgrade_path.main(["--executable", "aise", "--from", "1.0.0rc3"]) == 2


def test_a_release_that_cannot_be_downloaded_is_not_reported_as_an_upgrade_failure(
    monkeypatch: MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    # A PyPI timeout was reported as "upgrading from 1.0.0rc2 needs a manual step".
    def unreachable(previous: str, executable: object) -> list[str]:
        raise verify_upgrade_path.UpgradeCheckUnavailable(f"{previous} could not install: timed out")

    monkeypatch.setattr(verify_upgrade_path, "check_upgrade", unreachable)

    # _check_all rather than main(): main() refuses to run at all on Windows, before this code.
    assert verify_upgrade_path._check_all(["1.0.0rc2"], Path("aise")) == 2
    error = capsys.readouterr().err
    assert "not an upgrade failure" in error and "needs a manual step" not in error
