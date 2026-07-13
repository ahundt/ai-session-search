"""
Session Content Analysis Engine - backs `aise analyze`.

Bounded indexed pipeline: pattern-matching (inspired by qualitative coding) + vocabulary mining.
Reads provider-normalized documents from the Rust catalog. Writes session_db.json +
VOCABULARY_ANALYSIS.md.

METHODOLOGICAL REFERENCES (inspiration, not full implementations):
- Hsieh & Shannon (2005): https://journals.sagepub.com/doi/10.1177/1049732305276687
  Directed Content Analysis — we use keyword/regex pattern matching inspired by this approach.
- Wei et al. (2022): https://arxiv.org/abs/2201.11903
  Chain-of-Thought Prompting — we detect CoT prompting patterns; scoring is our own weighted system.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""

from __future__ import annotations

import json
import re
from collections import Counter
from dataclasses import asdict, dataclass, field
from pathlib import Path

from ai_session_search._native import NativeAnalyzedSession, NativeSessionRecord
from ai_session_search.analysis.codebook import (
    is_meaningful,
    load_stop_words,
)
from ai_session_search.analysis.indexed import (
    open_analysis_service,
    resolve_page_size,
)
from ai_session_search.analysis.io import write_text_atomic
from ai_session_search.analysis.rust_policy import (
    analyze_index_snapshot,
    build_analysis_policy,
)
from ai_session_search.config import load_config, resolve_org_dir
from ai_session_search.native import SessionSearch

DEFAULT_MARKER_WINDOW = 25_000


@dataclass
class SessionRecord:
    """Analysis record for one session. user_text excluded from DB serialization.

    user_text: in-memory only during pipeline. NOT serialized to session_db.json.
    Use to_db_dict() for persistent storage.
    The Rust policy consumes raw text inside a bounded-page snapshot; this publication DTO
    receives metadata and classifications only.
    """

    name: str
    source_dir: str
    filepath: str
    source_format: str  # 'aistudio_json' | 'markdown' | 'gemini_cli' | 'claude_jsonl'
    user_text: str  # in-memory only
    chunk_count: int
    user_chunk_count: int
    techniques: list[str] = field(default_factory=list)
    roles: list[str] = field(default_factory=list)
    task_categories: list[str] = field(default_factory=list)
    writing_methods: list[str] = field(default_factory=list)
    rigor_score: int = 0
    utility: int = 0
    version_num: int | None = None
    is_branch: bool = False
    is_copy: bool = False
    graph_parent: str | None = None
    era: str = ""
    has_srt: bool = False
    has_transcript: bool = False
    project_hash: str = ""
    prose_frac: float = 1.0  # fraction of user_text that is prose (not code/config)
    prompt_role: str = "unknown"  # 'initial' | 'continuation' | 'standalone' | 'unknown'
    cwd: str = ""  # working directory at session time (Claude Code: from JSONL cwd; others: "")
    session_id: str = ""  # canonical provider-qualified Rust session ID
    parent_session_ids: list[str] = field(default_factory=list)
    relationship_hints: list[dict[str, object]] = field(default_factory=list)

    @property
    def user_text_full(self) -> str:
        return self.user_text

    def user_text_sample(self, max_chars: int) -> str:
        return self.user_text[:max_chars]

    def to_db_dict(self) -> dict:
        """Serialize for session_db.json — excludes user_text. Stores ~/... paths (no PII)."""
        d = asdict(self)
        d.pop("user_text", None)
        home = str(Path.home())
        for key in ("source_dir", "filepath", "cwd"):
            val = d.get(key, "")
            if val and val.startswith(home):
                d[key] = "~" + val[len(home) :]
        return d


def _detect_era(
    name: str,
    user_text: str,
    filepath: str | None = None,
    timestamp: str | None = None,
) -> str:
    """Detect era (year) from session signals. Returns actual year or 'legacy'/'unknown'.

    Never hardcodes year buckets — all signals come from the data itself.
    NOTE: timestamp should only be passed when it is authoritative (e.g. Gemini CLI
    sessions have actual startTime in JSON). AI Studio file mtime reflects download date
    (unreliable) — do NOT pass it as timestamp.

    Priority (highest to lowest):
    1. 4-digit year at start of name (e.g. "2024-03-meeting")
    2. 2-digit year prefix at start of name: YY-MM-DD (e.g. "25-08-27") or YYMMDD (e.g. "250509")
    3. Standalone 4-digit year anywhere in name (e.g. "Meeting Notes 2024")
    4. Authoritative ISO timestamp from session JSON metadata (Gemini CLI, Claude Code)
    5. Year in first 2000 chars of user_text (e.g. "as of 2024")
    6. .md extension → legacy AI Studio format (2023-2024 era, exact year unknown)
    7. "unknown" — no year signal found
    """
    _yr4_re = re.compile(r"\b(20\d\d)\b")

    # Skip name-based heuristics for UUIDs — hex digits look like dates/years.
    # UUID format: 8-4-4-4-12 hex chars (e.g. "86042459-a91b-4d63-9197-ca066e214b02")
    _is_uuid = bool(
        re.match(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
            name,
            re.IGNORECASE,
        )
    )

    if not _is_uuid:
        # Priority 1: 4-digit year at start of name
        m = re.match(r"(20\d\d)", name)
        if m:
            return m.group(1)

        # Priority 2: 2-digit year prefix at start of name (YY-MM-DD or YYMMDD)
        # Validate month (01-12) and day (01-31) to reject UUID hex digits.
        m2 = re.match(r"(\d{2})[-]?(\d{2})[-]?(\d{2})", name)
        if m2:
            yy, mm, dd = int(m2.group(1)), int(m2.group(2)), int(m2.group(3))
            if 20 <= yy <= 99 and 1 <= mm <= 12 and 1 <= dd <= 31:
                return str(2000 + yy)

        # Priority 3: standalone 4-digit year anywhere in name
        m3 = _yr4_re.search(name)
        if m3:
            return m3.group(1)

    # Priority 4: authoritative ISO timestamp (Gemini CLI startTime, Claude Code session ts)
    # Do NOT use AI Studio file mtime here — it reflects download date, not creation date.
    if timestamp:
        m4 = re.match(r"(20\d\d)", timestamp)
        if m4:
            return m4.group(1)

    # Priority 5: ISO date pattern (YYYY-MM-DD) in early content — specific enough to be reliable
    sample = user_text[:2000]
    m5 = re.search(r"\b(20[2-9]\d)-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])\b", sample)
    if m5:
        return m5.group(1)

    # Priority 6: .md extension = legacy AI Studio format (2023-2024 era, exact year unknown)
    fp = filepath or name
    if fp.endswith(".md"):
        return "legacy"

    return "unknown"


def write_vocab_report(
    tri: Counter[str],
    quad: Counter[str],
    output_file: Path,
    min_freq: int = 3,
    stop_words: frozenset[str] | None = None,
    source_names: list[str] | None = None,
) -> None:
    """Write vocabulary analysis to markdown. No arbitrary truncation.

    min_freq: loaded from scoring_weights.json["min_ngram_freq"] (default 3).
    stop_words: loaded from stop_words.json (default _DEFAULT_STOP_WORDS).
    source_names: list of source names to display in header (e.g. ["Claude Code", "AI Studio"]).
    """
    tri_rows = [(freq, phrase) for phrase, freq in tri.most_common() if freq >= min_freq and is_meaningful(phrase, stop_words)]
    quad_rows = [(freq, phrase) for phrase, freq in quad.most_common() if freq >= min_freq and is_meaningful(phrase, stop_words)]

    source_label = ", ".join(source_names) if source_names else "all"
    lines = [
        "# Vocabulary Analysis: Recurring Prompt Phrases\n\n",
        f"N-gram analysis of user turns across {source_label} sessions.\n",
        "Source: PromptEngineering.org — https://www.promptengineering.org/building-a-reusable-prompt-library/\n\n",
        f"## 3-Word Phrases ({len(tri_rows)} total with freq >= {min_freq})\n\n",
        "| Count | Phrase |\n| :--- | :--- |\n",
    ]
    lines.extend(f"| {freq} | {phrase} |\n" for freq, phrase in tri_rows)
    lines += [
        f"\n## 4-Word Phrases ({len(quad_rows)} total with freq >= {min_freq})\n\n",
        "| Count | Phrase |\n| :--- | :--- |\n",
    ]
    lines.extend(f"| {freq} | {phrase} |\n" for freq, phrase in quad_rows)

    write_text_atomic(output_file, "".join(lines))
    print(f"Vocabulary: {len(tri_rows)} trigrams, {len(quad_rows)} quadgrams -> {output_file}")


def _source_format(session: NativeSessionRecord) -> str:
    provider = session.provider
    if provider == "aistudio":
        return "markdown" if session.source_path.endswith(".md") else "aistudio_json"
    if provider == "gemini-cli":
        return "gemini_cli"
    if provider == "claude":
        return "claude_jsonl"
    return provider.replace("-", "_")


def _provider_display_name(provider: str) -> str:
    if provider == "aistudio":
        return "AI Studio"
    return provider.replace("-", " ").title().replace(" Cli", " CLI")


def _record_from_analysis(analyzed: NativeAnalyzedSession) -> SessionRecord:
    session = analyzed.session
    source_format = _source_format(session)
    name = session.title or session.provider_session_id
    timestamp = None
    if session.provider != "aistudio":
        timestamp = session.created_at or session.last_message_at or session.updated_at
    source_path = session.source_path
    source_dir = session.cwd or session.repo_root
    if not source_dir and source_path:
        source_dir = str(Path(source_path).parent)
    classifications: dict[str, list[str]] = {
        "technique": [],
        "role": [],
        "task_category": [],
        "writing_method": [],
    }
    for item in analyzed.classifications:
        if item.dimension in classifications:
            classifications[item.dimension].append(item.label)
    relationship_hints: list[dict[str, object]] = [
        {
            "rule_id": hint.rule_id,
            "kind": hint.kind,
            "parent_title": hint.parent_title,
            "status": hint.status,
            "resolved_session_id": hint.resolved_session_id,
            "candidate_session_ids": hint.candidate_session_ids,
        }
        for hint in analyzed.relationship_hints
    ]
    parent_session_ids = sorted({hint.resolved_session_id for hint in analyzed.relationship_hints if hint.resolved_session_id is not None})
    return SessionRecord(
        name=name,
        source_dir=source_dir or "",
        filepath=source_path,
        source_format=source_format,
        user_text="",
        chunk_count=analyzed.message_count,
        user_chunk_count=analyzed.user_message_count,
        techniques=classifications["technique"],
        roles=classifications["role"],
        task_categories=classifications["task_category"],
        writing_methods=classifications["writing_method"],
        rigor_score=analyzed.score,
        utility=analyzed.score,
        era=_detect_era(name, "", filepath=source_path, timestamp=timestamp),
        prompt_role="standalone" if analyzed.user_message_count == 1 else "unknown",
        cwd=session.cwd or "",
        session_id=session.id,
        parent_session_ids=parent_session_ids,
        relationship_hints=relationship_hints,
        is_branch=any(hint.kind == "branch" and hint.status == "resolved" for hint in analyzed.relationship_hints),
        is_copy=any(hint.kind == "copy" and hint.status == "resolved" for hint in analyzed.relationship_hints),
    )


def run_analysis(
    marker_window: int | None = None,
    source_filter: str | None = None,
    config: dict | None = None,
    *,
    search: SessionSearch | None = None,
    refresh_index: bool = True,
) -> list[SessionRecord]:
    """Analyze bounded pages from the shared Rust session index.

    Args:
        marker_window: Chars for marker matching (0 = from config)
        source_filter: Canonical provider name, legacy ``gemini`` alias, or None (all)
        config: Config dict (if None, loads from config.json)
        search: Existing native service for embedding/tests; defaults to the configured service
        refresh_index: Refresh configured sources once before the keyset scan

    Session text memory is bounded by one configured Rust page. Returned records and
    serialized output retain metadata only; raw user text is cleared after coding.
    """
    cfg = load_config() if config is None else config
    org_dir = resolve_org_dir(cfg)
    db_file = org_dir / "session_db.json"
    vocab_output = org_dir / cfg.get("vocab_output_filename", "VOCABULARY_ANALYSIS.md")
    mw = marker_window or int(cfg.get("marker_window", DEFAULT_MARKER_WINDOW))
    page_size = resolve_page_size(cfg)
    if mw <= 0:
        raise ValueError("marker_window must be greater than zero")

    policy, scoring_weights = build_analysis_policy(
        cfg,
        org_dir,
        max_classification_chars=mw,
    )
    min_ngram_freq = int(scoring_weights.get("min_ngram_freq", 3))
    stop_words = load_stop_words(org_dir)
    service = open_analysis_service(search, refresh_index=refresh_index)
    print(f"Analyzing indexed sessions in pages of {page_size}...")
    result = analyze_index_snapshot(
        service,
        provider=source_filter,
        page_size=page_size,
        policy=policy,
    )
    analyzed = list(result.sessions.values())
    empty_sessions = sum(item.message_count == 0 for item in analyzed)
    no_user_text = sum(item.message_count > 0 and (item.user_message_count == 0 or not item.has_user_text) for item in analyzed)
    parse_warnings = sum(item.session.parse_warning is not None for item in analyzed)
    usable = [item for item in analyzed if item.message_count > 0 and item.user_message_count > 0 and item.has_user_text]
    records = [_record_from_analysis(item) for item in usable]
    skipped = empty_sessions + no_user_text
    if skipped or parse_warnings:
        print(
            f"Skipped {skipped} indexed sessions "
            f"({empty_sessions} no messages, {no_user_text} no user text, 0 errors); "
            f"{parse_warnings} parser warnings observed"
        )
    print(f"Total indexed: {len(analyzed)}; analyzed: {len(records)} sessions")

    # Write DB (metadata only, no user_text)
    db = [record.to_db_dict() for record in records]
    write_text_atomic(db_file, json.dumps(db, indent=2))
    print(f"Analysis complete: {len(records)} sessions -> {db_file}")

    providers = sorted({item.session.provider for item in usable})
    source_names = [_provider_display_name(provider_name) for provider_name in providers]
    trigrams = Counter({item.phrase: item.occurrences for item in result.vocabulary if item.words == 3})
    quadgrams = Counter({item.phrase: item.occurrences for item in result.vocabulary if item.words == 4})
    write_vocab_report(trigrams, quadgrams, vocab_output, min_freq=min_ngram_freq, stop_words=stop_words, source_names=source_names or None)
    return records


def main(source_filter: str | None = None, marker_window: int | None = None) -> None:
    """Entry point for `aise analyze` CLI command.

    Args:
        source_filter: Narrow to one backend: 'aistudio', 'gemini', or None (all)
        marker_window: Chars for marker matching (0 = from config)
    """
    run_analysis(marker_window=marker_window, source_filter=source_filter)


if __name__ == "__main__":
    main()
