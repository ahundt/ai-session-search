"""Every failure that can cross the Rust/Python boundary, and how it arrives.

This surface has regressed before, so it is pinned rather than trusted. Three
things are asserted for each failure, because getting any one right while the
others are wrong still leaves a caller unable to act:

1. **The exception TYPE.** ``ValueError`` for a value the caller supplied that
   cannot be right, ``RuntimeError`` for a failure discovered while doing the
   work. A caller writes ``except ValueError`` around argument construction and
   ``except RuntimeError`` around the call; collapsing everything into one type
   makes that impossible.
2. **The full message.** Rust builds errors as ``anyhow`` chains, and rendering
   one with ``to_string()`` shows only the outermost ``.context(...)`` line --
   so ``deny_unknown_fields`` becomes "failed to parse policy" with the field
   name lost. The binding renders ``{error:#}``, and these tests assert the
   INNER detail is present, which is what catches a regression back to the
   flat form.
3. **That failure is not success.** The most dangerous version of a broken
   error path is not a bad message, it is a call that returns an empty result:
   ``report.matches == []`` reads exactly like "nothing matched". Each case
   therefore asserts the call RAISED, never that it merely returned nothing.
"""

from __future__ import annotations

import textwrap
from pathlib import Path
from typing import Any, cast

import pytest

from ai_session_search import native


def _skill(root: Path, name: str, policy: str | None, *, version: str = "0.1.0") -> Path:
    """Write a standard-shaped skill directory and return its root."""
    directory = root / name
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "SKILL.md").write_text(
        f"---\nname: {name}\ndescription: fixture skill\nmetadata:\n  version: {version}\n---\n\nbody\n",
        encoding="utf-8",
    )
    if policy is not None:
        (directory / "capability.toml").write_text(policy, encoding="utf-8")
    return directory


def _config(tmp_path: Path, body: str = "") -> Path:
    """An isolated config that discovers no provider, so nothing reads real session files."""
    config = tmp_path / "config.toml"
    providers = "\n".join(
        f"[providers.{name}]\nenabled = false\npaths = []"
        for name in (
            "claude",
            "claude-desktop",
            "codex",
            "cursor",
            "antigravity",
            "pi",
            "aistudio",
            "gemini-cli",
        )
    )
    config.write_text(
        f"[index]\ndb_path = {str(tmp_path / 'index.db')!r}\n"
        f"cache_dir = {str(tmp_path / 'cache')!r}\n{body}\n{providers}\n",
        encoding="utf-8",
    )
    return config


def _search(tmp_path: Path, body: str = "") -> native.SessionSearch:
    return native.SessionSearch(config_path=_config(tmp_path, body))


VALID_POLICY = textwrap.dedent(
    """
    schema_version = 1
    kind = "message-classification"

    [[categories]]
    name = "clobber"
    patterns = ['''\\byou overwrote\\b''']
    """
).lstrip()


def _run(
    search: native.SessionSearch,
    name: str,
    query: native.MessageClassificationQuery | None = None,
) -> native.SkillRunReport:
    return search.run_skill(
        native.SkillRunQuery(
            skill=native.SkillSelector(name=name),
            input=query or native.MessageClassificationQuery(),
        )
    )


# --------------------------------------------------------------------------
# Python -> Rust: a value the caller supplied cannot be right.
# --------------------------------------------------------------------------


def test_a_negative_limit_is_a_value_error_naming_the_value() -> None:
    """The Four Facts: what failed, the offending value, and what to pass instead."""
    with pytest.raises(ValueError) as raised:
        native.MessageClassificationQuery(limit=-1)
    message = str(raised.value)
    assert "limit must be 0 or greater, got -1" in message
    assert "pass" in message, f"the message must say what to pass instead: {message}"


def test_a_direct_definition_typo_is_a_value_error_at_construction() -> None:
    with pytest.raises(ValueError, match="definition must contain only categories"):
        native.SkillRunQuery(
            skill=native.SkillSelector(name="corrections"),
            input=native.MessageClassificationQuery(),
            # Deliberately bypass the static shape so this test can pin the runtime
            # error callers receive for malformed dynamically sourced mappings.
            definition=cast(Any, {"categoriez": []}),
        )


def test_a_negative_offset_is_a_value_error_naming_the_value() -> None:
    with pytest.raises(ValueError) as raised:
        native.MessageClassificationQuery(offset=-5)
    message = str(raised.value)
    assert "offset must be 0 or greater, got -5" in message
    assert "pass" in message, f"the message must say what to pass instead: {message}"


def test_an_unknown_session_kind_is_a_value_error_listing_the_valid_ones() -> None:
    with pytest.raises(ValueError) as raised:
        native.MessageClassificationQuery(session_kinds=["agent"])
    message = str(raised.value)
    assert "agent" in message, message
    assert "user" in message and "subagent" in message, (
        f"a rejected enum must list what IS accepted, or the caller has to guess: {message}"
    )


def test_a_wrong_argument_type_is_a_type_error_not_a_silent_coercion() -> None:
    with pytest.raises(TypeError):
        native.MessageClassificationQuery(additional_skills="team-rules")  # type: ignore[arg-type]


# --------------------------------------------------------------------------
# Rust -> Python: a failure discovered while doing the work.
# --------------------------------------------------------------------------


def test_an_unknown_skill_raises_instead_of_returning_an_empty_report(tmp_path: Path) -> None:
    """The failure mode that matters most: NOT raising would look like a clean history."""
    search = _search(tmp_path)
    with pytest.raises(RuntimeError) as raised:
        _run(search, "not-installed")
    message = str(raised.value)
    assert "not-installed" in message
    assert "catalog" in message, f"name the value AND where to find valid ones: {message}"


def test_a_malformed_policy_surfaces_the_offending_field_not_just_the_outer_context(
    tmp_path: Path,
) -> None:
    """``to_string()`` on an anyhow chain shows only the outermost context line.

    The parse failure is the INNER error, so a regression to the flat rendering
    drops the field name and leaves the caller with "failed to parse", which
    names no fix.
    """
    skills = tmp_path / "skills"
    _skill(
        skills,
        "team-rules",
        'schema_version = 1\nkind = "message-classification"\nweights = 3\n',
    )
    search = _search(tmp_path, f"[skills]\nsearch_paths = [{str(skills)!r}]")

    with pytest.raises(RuntimeError) as raised:
        _run(search, "team-rules")
    message = str(raised.value)
    assert "weights" in message, (
        f"the unknown field must reach Python, or the anyhow chain was flattened: {message}"
    )
    assert "capability.toml" in message, f"and the file it is in: {message}"


def test_an_unsupported_schema_version_is_rejected_by_name(tmp_path: Path) -> None:
    skills = tmp_path / "skills"
    _skill(
        skills,
        "team-rules",
        'schema_version = 99\nkind = "message-classification"\n',
    )
    search = _search(tmp_path, f"[skills]\nsearch_paths = [{str(skills)!r}]")

    with pytest.raises(RuntimeError) as raised:
        _run(search, "team-rules")
    message = str(raised.value)
    assert "99" in message and "schema_version" in message, message


def test_an_invalid_regex_names_the_category_and_the_pattern(tmp_path: Path) -> None:
    skills = tmp_path / "skills"
    _skill(
        skills,
        "team-rules",
        'schema_version = 1\nkind = "message-classification"\n\n'
        '[[categories]]\nname = "broken"\npatterns = ["(unclosed"]\n',
    )
    search = _search(tmp_path, f"[skills]\nsearch_paths = [{str(skills)!r}]")

    with pytest.raises(RuntimeError) as raised:
        _run(search, "team-rules")
    message = str(raised.value)
    assert "broken" in message, f"which category: {message}"
    assert "(unclosed" in message, f"which pattern: {message}"


def test_a_skill_without_a_policy_is_a_different_error_than_an_unknown_one(
    tmp_path: Path,
) -> None:
    """Two failures with different fixes must not share one message."""
    skills = tmp_path / "skills"
    _skill(skills, "no-policy", None)
    search = _search(tmp_path, f"[skills]\nsearch_paths = [{str(skills)!r}]")

    with pytest.raises(RuntimeError) as raised:
        _run(search, "no-policy")
    message = str(raised.value)
    assert "message-classification capability" in message, message
    assert "unknown skill" not in message, (
        f"the skill EXISTS; saying it is unknown sends the caller to the wrong fix: {message}"
    )


def test_two_directories_claiming_one_skill_name_name_both_paths(tmp_path: Path) -> None:
    """First-one-wins would make the answer depend on configuration order."""
    first = tmp_path / "a"
    second = tmp_path / "b"
    _skill(first, "team-rules", VALID_POLICY)
    _skill(second, "team-rules", VALID_POLICY)
    search = _search(
        tmp_path,
        f"[skills]\nsearch_paths = [{str(first)!r}, {str(second)!r}]",
    )

    with pytest.raises(RuntimeError) as raised:
        _run(search, "team-rules")
    message = str(raised.value)
    assert str(first) in message and str(second) in message, (
        f"both conflicting paths must be named so the caller can delete one: {message}"
    )


# --------------------------------------------------------------------------
# Failure must never look like success.
# --------------------------------------------------------------------------


def test_a_default_run_reports_the_policy_that_ran_even_with_no_matches(
    tmp_path: Path,
) -> None:
    search = _search(tmp_path)
    report = _run(search, "corrections").output.report
    assert report.matches == []
    assert [receipt.name for receipt in report.policies] == ["corrections"], (
        "'these rules ran and found nothing' must be distinguishable from 'no rules ran'"
    )
    assert len(report.policies[0].sha256) == 64
