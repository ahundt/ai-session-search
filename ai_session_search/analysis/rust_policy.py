"""Translate publication configuration into the canonical Rust analysis policy."""

from __future__ import annotations

import re
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, Literal, cast

from ai_session_search._native import (
    AnalysisPolicy,
    ClassificationRule,
    NativeAnalysisResult,
    PhraseVocabulary,
    RelationshipRule,
    SessionQuery,
    SessionSearch,
)
from ai_session_search.analysis.codebook import (
    load_codebook,
    load_keyword_maps,
    load_scoring_weights,
    load_stop_words,
)
from ai_session_search.analysis.indexed import canonical_provider

DEFAULT_MAX_UNIQUE_PHRASES = 250_000
DEFAULT_PHRASE_WIDTHS = (3, 4)
MIN_CODEBOOK_MARKER_CHARS = 5

DEFAULT_TECHNIQUE_WEIGHT = 20
DEFAULT_ROLE_WEIGHT = 15
DEFAULT_THINKING_BUDGET_WEIGHT = 30
DEFAULT_ANTI_AI_WEIGHT = 35
DEFAULT_CORRECTED_TITLE_WEIGHT = 25

DEFAULT_RELATIONSHIP_RULES: tuple[dict[str, str], ...] = (
    {
        "id": "branch_of",
        "kind": "branch",
        "pattern": r"(?i)^branch of (?P<parent>.+)$",
    },
    {
        "id": "copy_of",
        "kind": "copy",
        "pattern": r"(?i)^copy of (?P<parent>.+)$",
    },
)
RelationshipKindName = Literal["branch", "copy", "version"]


def _regex_for_markers(
    markers: Sequence[str],
    *,
    word_boundary: bool,
    min_marker_chars: int,
) -> str | None:
    valid = [marker.strip() for marker in markers if len(marker.strip()) >= min_marker_chars]
    if not valid:
        return None
    prefix = r"\b" if word_boundary else ""
    return "(?i)(?:" + "|".join(prefix + re.escape(marker) for marker in valid) + ")"


def _classification_rules(
    org_dir: Path,
    scoring_weights: Mapping[str, Any],
) -> list[ClassificationRule]:
    techniques, roles = load_codebook(org_dir)
    keyword_maps = load_keyword_maps(org_dir)
    rules: list[ClassificationRule] = []

    for dimension, groups, weight, boundary, min_chars in (
        (
            "technique",
            techniques,
            int(scoring_weights.get("technique", DEFAULT_TECHNIQUE_WEIGHT)),
            True,
            MIN_CODEBOOK_MARKER_CHARS,
        ),
        (
            "role",
            roles,
            int(scoring_weights.get("role", DEFAULT_ROLE_WEIGHT)),
            True,
            MIN_CODEBOOK_MARKER_CHARS,
        ),
        ("task_category", keyword_maps.get("task_categories", {}), 0, False, 1),
        ("writing_method", keyword_maps.get("writing_methods", {}), 0, False, 1),
    ):
        for label, markers in groups.items():
            pattern = _regex_for_markers(
                markers,
                word_boundary=boundary,
                min_marker_chars=min_chars,
            )
            if pattern is not None:
                rules.append(ClassificationRule(dimension, label, pattern, weight=weight))

    rules.extend(
        [
            ClassificationRule(
                "analysis_signal",
                "thinking_budget",
                r"(?i)thinking_?budget",
                weight=int(scoring_weights.get("thinking_budget", DEFAULT_THINKING_BUDGET_WEIGHT)),
            ),
            ClassificationRule(
                "analysis_signal",
                "anti_ai",
                r"(?i)(?:anti-ai|wikipedia_signs_of_ai)",
                weight=int(scoring_weights.get("anti_ai", DEFAULT_ANTI_AI_WEIGHT)),
            ),
            ClassificationRule(
                "analysis_signal",
                "corrected_title",
                r"(?i)\b(?:corrected|improved)\b",
                target="title",
                weight=int(scoring_weights.get("corrected_bonus", DEFAULT_CORRECTED_TITLE_WEIGHT)),
            ),
        ]
    )
    return rules


def build_phrase_vocabulary(
    config: Mapping[str, Any],
    org_dir: Path,
) -> PhraseVocabulary:
    """Build an explicitly bounded Rust phrase policy from publication config."""
    raw_widths = config.get("analysis_phrase_widths", DEFAULT_PHRASE_WIDTHS)
    if not isinstance(raw_widths, (list, tuple)):
        raise ValueError("analysis_phrase_widths must be a list of positive integers")
    widths = [int(width) for width in raw_widths]
    max_phrases = int(config.get("analysis_max_unique_phrases", DEFAULT_MAX_UNIQUE_PHRASES))
    min_tokens = int(config.get("analysis_min_document_tokens", 0))
    if not widths or any(width <= 0 for width in widths):
        raise ValueError("analysis_phrase_widths must contain positive integers")
    if max_phrases <= 0:
        raise ValueError("analysis_max_unique_phrases must be greater than zero")
    if min_tokens < 0:
        raise ValueError("analysis_min_document_tokens must be zero or greater")
    return PhraseVocabulary(
        widths,
        max_phrases,
        min_document_tokens=min_tokens,
        excluded_tokens=sorted(load_stop_words(org_dir)),
        exclude_numeric_tokens=True,
        prose_only=True,
    )


def build_analysis_policy(
    config: Mapping[str, Any],
    org_dir: Path,
    *,
    max_classification_chars: int | None,
    include_classifications: bool = True,
) -> tuple[AnalysisPolicy, dict[str, Any]]:
    """Compile publication settings once; Rust validates all executable policy."""
    if include_classifications and (max_classification_chars is None or max_classification_chars <= 0):
        raise ValueError("max_classification_chars must be greater than zero")
    scoring_weights = load_scoring_weights(org_dir)
    rules = _classification_rules(org_dir, scoring_weights) if include_classifications else None
    relationship_specs = config.get("analysis_relationship_rules", DEFAULT_RELATIONSHIP_RULES)
    if not isinstance(relationship_specs, (list, tuple)):
        raise ValueError("analysis_relationship_rules must be a list of rule objects")
    relationship_rules: list[RelationshipRule] = []
    for index, spec in enumerate(relationship_specs):
        if not isinstance(spec, Mapping):
            raise ValueError(f"analysis_relationship_rules[{index}] must be an object")
        try:
            rule_id = str(spec["id"])
            kind = str(spec["kind"])
            pattern = str(spec["pattern"])
        except KeyError as error:
            raise ValueError(
                f"analysis_relationship_rules[{index}] is missing required field {error.args[0]!r}"
            ) from error
        if kind not in {"branch", "copy", "version"}:
            raise ValueError(
                f"analysis_relationship_rules[{index}].kind must be branch, copy, or version"
            )
        relationship_rules.append(
            RelationshipRule(rule_id, cast(RelationshipKindName, kind), pattern)
        )
    policy = AnalysisPolicy(
        classification_rules=rules,
        relationship_rules=relationship_rules,
        phrase_vocabulary=build_phrase_vocabulary(config, org_dir),
        max_classification_chars=max_classification_chars if include_classifications else None,
    )
    return policy, scoring_weights


def analyze_index_snapshot(
    service: SessionSearch,
    *,
    provider: str | None,
    page_size: int,
    policy: AnalysisPolicy,
) -> NativeAnalysisResult:
    """Run one bounded-page analysis over one Rust-managed SQLite read snapshot."""
    request = SessionQuery(provider=canonical_provider(provider), limit=0)
    return service.analyze_sessions(request, policy=policy, page_size=page_size)
