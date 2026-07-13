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

from ai_session_search.analysis.codebook import (
    classify_prompt_role,
    compile_codes,
    extract_prose,
    get_ngrams,
    is_meaningful,
    load_codebook,
    load_continuation_config,
    load_keyword_maps,
    load_scoring_weights,
    load_stop_words,
    prose_fraction,
)
from ai_session_search.analysis.indexed import (
    iter_analysis_documents,
    open_analysis_service,
    resolve_page_size,
)
from ai_session_search.analysis.io import write_text_atomic
from ai_session_search.config import load_config, resolve_org_dir
from ai_session_search.native import NativeAnalysisDocument, SessionSearch

DEFAULT_MARKER_WINDOW = 25_000
DEFAULT_MARKDOWN_MARKER_WINDOW = 2_000


@dataclass
class SessionRecord:
    """Analysis record for one session. user_text excluded from DB serialization.

    user_text: in-memory only during pipeline. NOT serialized to session_db.json.
    Use to_db_dict() for persistent storage.
    Memory: O(text_len) per session, GC'd after coding + vocabulary accumulation.
    """
    name: str
    source_dir: str
    filepath: str
    source_format: str       # 'aistudio_json' | 'markdown' | 'gemini_cli' | 'claude_jsonl'
    user_text: str           # in-memory only
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
    prose_frac: float = 1.0   # fraction of user_text that is prose (not code/config)
    prompt_role: str = "unknown"  # 'initial' | 'continuation' | 'standalone' | 'unknown'
    cwd: str = ""              # working directory at session time (Claude Code: from JSONL cwd; others: "")

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
                d[key] = "~" + val[len(home):]
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
    _is_uuid = bool(re.match(
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
        name, re.IGNORECASE,
    ))

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


def _matching_patterns(
    text: str,
    patterns: dict[str, re.Pattern[str]],
) -> list[str]:
    return [name for name, pattern in patterns.items() if pattern.search(text)]


def _matching_keywords(
    text: str,
    groups: dict[str, list[str]],
) -> list[str]:
    return [name for name, keywords in groups.items() if any(keyword in text for keyword in keywords)]


def _apply_name_metadata(
    record: SessionRecord,
    *,
    version_weight: int,
    corrected_bonus: int,
) -> None:
    version_match = re.search(r"\bv(\d+)\b", record.name, re.I)
    if version_match:
        record.version_num = int(version_match.group(1))
        record.rigor_score += record.version_num * version_weight
    name_lower = record.name.lower()
    if "corrected" in name_lower or "improved" in name_lower:
        record.rigor_score += corrected_bonus
    if "branch of " in name_lower:
        record.is_branch = True
        record.graph_parent = re.sub(r"(?i)branch of\s*", "", record.name).strip()
        return
    if "copy of " in name_lower:
        record.is_copy = True
        record.graph_parent = re.sub(r"(?i)copy of\s*", "", record.name).strip()
        return
    version_chain = re.search(r"^(.*?)\s+v(\d+)\s*$", record.name, re.I)
    if version_chain and int(version_chain.group(2)) > 1:
        previous_version = int(version_chain.group(2)) - 1
        record.graph_parent = f"{version_chain.group(1).strip()} v{previous_version}"


def apply_codes(
    rec: SessionRecord,
    tech_patterns: dict[str, re.Pattern[str]],
    role_patterns: dict[str, re.Pattern[str]],
    keyword_maps: dict[str, dict[str, list[str]]],
    scoring_weights: dict[str, int],
    marker_window: int = 25_000,
) -> None:
    """Apply codebook codes using pre-compiled regex patterns.

    Complexity: O(K*T) per session (K=codes, T=marker_window chars).
    All weights from scoring_weights dict (not hardcoded).
    Pattern matching inspired by Directed Content Analysis
    (Hsieh & Shannon, 2005 — https://journals.sagepub.com/doi/10.1177/1049732305276687).
    """
    text = rec.user_text_full[:marker_window]
    lower = text.lower()

    w_technique = scoring_weights.get("technique", 20)
    w_role = scoring_weights.get("role", 15)
    w_thinking = scoring_weights.get("thinking_budget", 30)
    w_anti_ai = scoring_weights.get("anti_ai", 35)
    w_version = scoring_weights.get("version_multiplier", 10)
    w_corrected = scoring_weights.get("corrected_bonus", 25)

    matched_techniques = _matching_patterns(text, tech_patterns)
    matched_roles = _matching_patterns(text, role_patterns)
    rec.techniques.extend(matched_techniques)
    rec.roles.extend(matched_roles)
    rec.rigor_score += len(matched_techniques) * w_technique
    rec.rigor_score += len(matched_roles) * w_role
    rec.task_categories.extend(
        _matching_keywords(lower, keyword_maps.get("task_categories", {}))
    )
    rec.writing_methods.extend(
        _matching_keywords(lower, keyword_maps.get("writing_methods", {}))
    )
    rec.rigor_score += int("thinkingbudget" in lower or "thinking_budget" in lower) * w_thinking
    rec.rigor_score += int("anti-ai" in lower or "wikipedia_signs_of_ai" in lower) * w_anti_ai
    _apply_name_metadata(
        rec,
        version_weight=w_version,
        corrected_bonus=w_corrected,
    )

    rec.utility = rec.rigor_score


def compute_descendant_boost(records: list[SessionRecord], boost_per_descendant: int = 15) -> None:
    """Add utility boost to ROOT sessions that spawned descendants.

    Implements provenance-based scoring (SAGE/Nature digital archiving).
    Older roots of version chains are valued MORE, not less (MSG 128).
    """
    name_to_rec = {r.name: r for r in records}
    for rec in records:
        if rec.graph_parent and rec.graph_parent in name_to_rec:
            name_to_rec[rec.graph_parent].utility += boost_per_descendant


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
    tri_rows = [(freq, phrase) for phrase, freq in tri.most_common()
                if freq >= min_freq and is_meaningful(phrase, stop_words)]
    quad_rows = [(freq, phrase) for phrase, freq in quad.most_common()
                 if freq >= min_freq and is_meaningful(phrase, stop_words)]

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


def _source_format(document: NativeAnalysisDocument) -> str:
    provider = document.session.provider
    if provider == "aistudio":
        return "markdown" if document.session.source_path.endswith(".md") else "aistudio_json"
    if provider == "gemini-cli":
        return "gemini_cli"
    if provider == "claude":
        return "claude_jsonl"
    return provider.replace("-", "_")


def _provider_display_name(provider: str) -> str:
    if provider == "aistudio":
        return "AI Studio"
    return provider.replace("-", " ").title().replace(" Cli", " CLI")


def _record_from_document(
    document: NativeAnalysisDocument,
    *,
    marker_window: int,
    markdown_marker_window: int,
    continuation_markers: list[str],
    min_initial_len: int,
    tech_patterns: dict[str, re.Pattern[str]],
    role_patterns: dict[str, re.Pattern[str]],
    keyword_maps: dict[str, dict[str, list[str]]],
    scoring_weights: dict[str, int],
) -> tuple[SessionRecord, str]:
    session = document.session
    user_text = document.user_text
    source_format = _source_format(document)
    name = session.title or session.provider_session_id
    timestamp = None
    if session.provider != "aistudio":
        timestamp = session.created_at or session.last_message_at or session.updated_at
    source_path = session.source_path
    source_dir = session.cwd or session.repo_root
    if not source_dir and source_path:
        source_dir = str(Path(source_path).parent)
    lower_sample = user_text[:5000].lower()
    prompt_role = classify_prompt_role(
        document.first_user_text or "",
        is_first_in_session=True,
        continuation_markers=continuation_markers,
        min_initial_len=min_initial_len,
    )
    if document.user_message_count == 1:
        prompt_role = "standalone"
    record = SessionRecord(
        name=name,
        source_dir=source_dir or "",
        filepath=source_path,
        source_format=source_format,
        user_text=user_text,
        chunk_count=document.message_count,
        user_chunk_count=document.user_message_count,
        era=_detect_era(name, user_text, filepath=source_path, timestamp=timestamp),
        has_srt="srt" in lower_sample,
        has_transcript="transcript" in lower_sample,
        prose_frac=prose_fraction(user_text),
        prompt_role=prompt_role,
        cwd=session.cwd or "",
    )
    effective_window = markdown_marker_window if source_format == "markdown" else marker_window
    apply_codes(
        record,
        tech_patterns,
        role_patterns,
        keyword_maps,
        scoring_weights,
        marker_window=effective_window,
    )
    return record, extract_prose(user_text)


@dataclass(frozen=True)
class _AnalysisPolicy:
    marker_window: int
    markdown_marker_window: int
    continuation_markers: list[str]
    min_initial_len: int
    tech_patterns: dict[str, re.Pattern[str]]
    role_patterns: dict[str, re.Pattern[str]]
    keyword_maps: dict[str, dict[str, list[str]]]
    scoring_weights: dict[str, int]


@dataclass
class _AnalysisState:
    records: list[SessionRecord] = field(default_factory=list)
    trigrams: Counter[str] = field(default_factory=Counter)
    quadgrams: Counter[str] = field(default_factory=Counter)
    providers: set[str] = field(default_factory=set)
    total_seen: int = 0
    empty_sessions: int = 0
    no_user_text: int = 0
    errors: int = 0
    parse_warnings: int = 0

    def consume(self, document: NativeAnalysisDocument, policy: _AnalysisPolicy) -> None:
        self.total_seen += 1
        self.parse_warnings += int(document.session.parse_warning is not None)
        if document.message_count == 0:
            self.empty_sessions += 1
            return
        if document.user_message_count == 0 or not document.user_text.strip():
            self.no_user_text += 1
            return
        try:
            record, prose_text = _record_from_document(
                document,
                marker_window=policy.marker_window,
                markdown_marker_window=policy.markdown_marker_window,
                continuation_markers=policy.continuation_markers,
                min_initial_len=policy.min_initial_len,
                tech_patterns=policy.tech_patterns,
                role_patterns=policy.role_patterns,
                keyword_maps=policy.keyword_maps,
                scoring_weights=policy.scoring_weights,
            )
        except Exception as exc:
            self.errors += 1
            print(f"Warning: failed to analyze {document.session.id}: {exc}")
            return
        self.trigrams.update(get_ngrams(prose_text, 3))
        self.quadgrams.update(get_ngrams(prose_text, 4))
        self.providers.add(document.session.provider)
        record.user_text = ""
        self.records.append(record)

    def report(self) -> None:
        skipped = self.empty_sessions + self.no_user_text + self.errors
        if skipped or self.parse_warnings:
            print(
                f"Skipped {skipped} indexed sessions "
                f"({self.empty_sessions} no messages, {self.no_user_text} no user text, "
                f"{self.errors} errors); {self.parse_warnings} parser warnings observed"
            )
        print(f"Total indexed: {self.total_seen}; analyzed: {len(self.records)} sessions")


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
    md_mw = int(cfg.get("md_marker_window", DEFAULT_MARKDOWN_MARKER_WINDOW))
    page_size = resolve_page_size(cfg)
    if mw <= 0 or md_mw <= 0:
        raise ValueError("marker_window and md_marker_window must be greater than zero")

    # Load scoring weights from config.json[scoring_weights] or scoring_weights.json
    scoring_weights = load_scoring_weights(org_dir)
    min_ngram_freq = int(scoring_weights.get("min_ngram_freq", 3))
    tech_codes, role_codes = load_codebook(org_dir)
    keyword_maps = load_keyword_maps(org_dir)
    tech_patterns = compile_codes(tech_codes)
    role_patterns = compile_codes(role_codes)
    continuation_markers, min_initial_len = load_continuation_config(org_dir)
    stop_words = load_stop_words(org_dir)

    policy = _AnalysisPolicy(
        marker_window=mw,
        markdown_marker_window=md_mw,
        continuation_markers=continuation_markers,
        min_initial_len=min_initial_len,
        tech_patterns=tech_patterns,
        role_patterns=role_patterns,
        keyword_maps=keyword_maps,
        scoring_weights=scoring_weights,
    )
    state = _AnalysisState()
    service = open_analysis_service(search, refresh_index=refresh_index)
    print(f"Analyzing indexed sessions in pages of {page_size}...")
    for document in iter_analysis_documents(
        service,
        provider=source_filter,
        page_size=page_size,
    ):
        state.consume(document, policy)

    state.report()

    compute_descendant_boost(state.records, scoring_weights.get("descendant_boost", 15))

    # Write DB (metadata only, no user_text)
    db = [record.to_db_dict() for record in state.records]
    write_text_atomic(db_file, json.dumps(db, indent=2))
    print(f"Analysis complete: {len(state.records)} sessions -> {db_file}")

    source_names = [_provider_display_name(provider_name) for provider_name in sorted(state.providers)]
    write_vocab_report(state.trigrams, state.quadgrams, vocab_output, min_freq=min_ngram_freq, stop_words=stop_words,
                       source_names=source_names or None)
    return state.records


def main(source_filter: str | None = None, marker_window: int | None = None) -> None:
    """Entry point for `aise analyze` CLI command.

    Args:
        source_filter: Narrow to one backend: 'aistudio', 'gemini', or None (all)
        marker_window: Chars for marker matching (0 = from config)
    """
    run_analysis(marker_window=marker_window, source_filter=source_filter)


if __name__ == "__main__":
    main()
