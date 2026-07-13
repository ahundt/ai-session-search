"""
Session Content Analysis Engine - backs `aise analyze`.

Single-pass streaming pipeline: pattern-matching (inspired by qualitative coding) + vocabulary mining.
Reads all sessions from all three source directories. Writes session_db.json + VOCABULARY_ANALYSIS.md.

METHODOLOGICAL REFERENCES (inspiration, not full implementations):
- Hsieh & Shannon (2005): https://journals.sagepub.com/doi/10.1177/1049732305276687
  Directed Content Analysis — we use keyword/regex pattern matching inspired by this approach.
- Wei et al. (2022): https://arxiv.org/abs/2201.11903
  Chain-of-Thought Prompting — we detect CoT prompting patterns; scoring is our own weighted system.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""
from __future__ import annotations

import contextlib
import json
import re
from collections import Counter
from dataclasses import dataclass, field, asdict
from pathlib import Path

from ai_session_search.sources.aistudio import AiStudioSource
from ai_session_search.config import load_config, resolve_org_dir
from ai_session_search.analysis.codebook import (
    load_codebook, load_keyword_maps, load_scoring_weights, compile_codes, get_ngrams,
    is_meaningful, extract_prose, prose_fraction, classify_prompt_role,
    load_continuation_config, load_stop_words,
)


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

    for tech, pattern in tech_patterns.items():
        if pattern.search(text):
            rec.techniques.append(tech)
            rec.rigor_score += w_technique

    for role, pattern in role_patterns.items():
        if pattern.search(text):
            rec.roles.append(role)
            rec.rigor_score += w_role

    # Task categories from external task_categories.json
    for cat, kws in keyword_maps.get("task_categories", {}).items():
        if any(k in lower for k in kws):
            rec.task_categories.append(cat)

    # Writing methods from external writing_methods.json
    for method, kws in keyword_maps.get("writing_methods", {}).items():
        if any(k in lower for k in kws):
            rec.writing_methods.append(method)

    if "thinkingbudget" in lower or "thinking_budget" in lower:
        rec.rigor_score += w_thinking
    if "anti-ai" in lower or "wikipedia_signs_of_ai" in lower:
        rec.rigor_score += w_anti_ai

    v_match = re.search(r"\bv(\d+)\b", rec.name, re.I)
    if v_match:
        rec.version_num = int(v_match.group(1))
        rec.rigor_score += rec.version_num * w_version

    if "corrected" in rec.name.lower() or "improved" in rec.name.lower():
        rec.rigor_score += w_corrected

    name_lower = rec.name.lower()
    if "branch of " in name_lower:
        rec.is_branch = True
        rec.graph_parent = re.sub(r"(?i)branch of\s*", "", rec.name).strip()
    elif "copy of " in name_lower:
        rec.is_copy = True
        rec.graph_parent = re.sub(r"(?i)copy of\s*", "", rec.name).strip()
    else:
        v_chain = re.search(r"^(.*?)\s+v(\d+)\s*$", rec.name, re.I)
        if v_chain and int(v_chain.group(2)) > 1:
            prev_v = int(v_chain.group(2)) - 1
            rec.graph_parent = f"{v_chain.group(1).strip()} v{prev_v}"

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

    output_file.write_text("".join(lines), encoding="utf-8")
    print(f"Vocabulary: {len(tri_rows)} trigrams, {len(quad_rows)} quadgrams -> {output_file}")


def _filter_config_by_source(cfg: dict, source_filter: str) -> dict:
    """Filter config to include only specified source backend.

    Args:
        cfg: Configuration dict
        source_filter: 'aistudio', 'gemini', 'claude', or None (all)

    Returns:
        Filtered config dict with only requested sources in source_dirs.
        Claude uses auto-discovery (not source_dirs), so 'claude' filter
        sets a '_include_claude' marker for run_analysis to check.
    """
    filtered = dict(cfg)
    if not source_filter:
        return filtered

    sd = filtered.get("source_dirs", {})
    new_sd = {}

    if source_filter in ("aistudio", "all"):
        if "aistudio" in sd:
            new_sd["aistudio"] = sd["aistudio"]

    if source_filter in ("gemini", "all"):
        if "gemini_cli" in sd:
            new_sd["gemini_cli"] = sd["gemini_cli"]

    # Claude uses auto-discovery, not explicit source_dirs.
    # Set marker so run_analysis knows to include/exclude Claude.
    filtered["_include_claude"] = source_filter in ("claude", "all")

    filtered["source_dirs"] = new_sd
    return filtered


def run_analysis(
    marker_window: int | None = None,
    source_filter: str | None = None,
    config: dict | None = None,
) -> list[SessionRecord]:
    """Single-pass streaming analysis: code + vocabulary simultaneously.

    Args:
        marker_window: Chars for marker matching (0 = from config)
        source_filter: Narrow to one backend: 'aistudio', 'gemini', or None (all)
        config: Config dict (if None, loads from config.json)

    WOLOG: all paths from config; no hardcoded truncation.
    Single source scan, O(1) memory per session (generator).
    """
    if config is None:
        cfg = load_config()
    else:
        cfg = config

    # Filter sources based on source_filter parameter
    if source_filter:
        cfg = _filter_config_by_source(cfg, source_filter)
    source_dirs_cfg = cfg.get("source_dirs", {}).get("aistudio", [])
    if isinstance(source_dirs_cfg, str):
        source_dirs_cfg = [source_dirs_cfg]
    source_dirs = [Path(p) for p in source_dirs_cfg]
    org_dir = resolve_org_dir(cfg)
    db_file = org_dir / "session_db.json"
    vocab_output = org_dir / cfg.get("vocab_output_filename", "VOCABULARY_ANALYSIS.md")
    mw = marker_window or cfg.get("marker_window", 25_000)
    md_mw = cfg.get("md_marker_window", 2_000)  # .md files include model responses; limit window

    # Load scoring weights from config.json[scoring_weights] or scoring_weights.json
    scoring_weights = load_scoring_weights(org_dir)
    min_ngram_freq = int(scoring_weights.get("min_ngram_freq", 3))
    min_session_text_len = int(scoring_weights.get("min_session_text_len", 50))

    tech_codes, role_codes = load_codebook(org_dir)
    keyword_maps = load_keyword_maps(org_dir)
    tech_patterns = compile_codes(tech_codes)
    role_patterns = compile_codes(role_codes)
    continuation_markers, min_initial_len = load_continuation_config(org_dir)
    stop_words = load_stop_words(org_dir)

    source = AiStudioSource(source_dirs=source_dirs)
    records: list[SessionRecord] = []
    tri: Counter[str] = Counter()
    quad: Counter[str] = Counter()

    print(f"Analyzing sessions from {len(source_dirs)} source directories...")
    for session_info in source.stream_sessions():
        with contextlib.suppress(Exception):
            messages = source.read_session(session_info)
            if not messages:
                continue

            user_text = " ".join(m.content for m in messages if m.type.value == "user")
            name = session_info.session_id
            fp = str(Path(session_info.project_dir) / name)
            # AI Studio: timestamp_first is file mtime (download date, not creation date) — do NOT use
            era = _detect_era(name, user_text, filepath=fp, timestamp=None)
            lower_sample = user_text[:5000].lower()

            # Detect source format from content
            source_format = "markdown" if name.endswith(".md") else "aistudio_json"

            prose_text = extract_prose(user_text)
            pf = prose_fraction(user_text)
            user_msgs = [m for m in messages if m.type.value == "user"]
            first_user = user_msgs[0].content if user_msgs else ""
            is_single = len(user_msgs) == 1
            p_role = classify_prompt_role(
                first_user, is_first_in_session=True,
                continuation_markers=continuation_markers,
                min_initial_len=min_initial_len,
            )
            if is_single:
                p_role = "standalone"
            rec = SessionRecord(
                name=name,
                source_dir=session_info.project_dir,
                filepath=fp,
                source_format=source_format,
                user_text=user_text,
                chunk_count=len(messages),
                user_chunk_count=sum(1 for m in messages if m.type.value == "user"),
                era=era,
                has_srt="srt" in lower_sample,
                has_transcript="transcript" in lower_sample,
                prose_frac=pf,
                prompt_role=p_role,
            )

            effective_mw = md_mw if source_format == "markdown" else mw
            apply_codes(rec, tech_patterns, role_patterns, keyword_maps, scoring_weights, marker_window=effective_mw)
            records.append(rec)

            # Vocabulary: prose-only text to avoid polluting n-grams with code tokens
            tri.update(get_ngrams(prose_text, 3))
            quad.update(get_ngrams(prose_text, 4))

    # Also process Gemini CLI sessions if configured (Gap 2 fix)
    gemini_cli_dir_cfg = cfg.get("source_dirs", {}).get("gemini_cli", "")
    if gemini_cli_dir_cfg:
        from ai_session_search.sources.gemini_cli import GeminiCliSource
        gemini_source = GeminiCliSource(Path(gemini_cli_dir_cfg))
        for session_info in gemini_source.stream_sessions():
            with contextlib.suppress(Exception):
                messages = gemini_source.read_session(session_info)
                if not messages:
                    continue
                user_text = " ".join(m.content for m in messages if m.type.value == "user")
                if not user_text.strip():
                    continue
                name = session_info.session_id
                fp_g = str(Path(gemini_cli_dir_cfg) / name)
                era = _detect_era(name, user_text, filepath=fp_g, timestamp=session_info.timestamp_first)
                lower_sample = user_text[:5000].lower()
                prose_text_g = extract_prose(user_text)
                pf_g = prose_fraction(user_text)
                user_msgs_g = [m for m in messages if m.type.value == "user"]
                first_user_g = user_msgs_g[0].content if user_msgs_g else ""
                is_single_g = len(user_msgs_g) == 1
                p_role_g = classify_prompt_role(
                    first_user_g, is_first_in_session=True,
                    continuation_markers=continuation_markers,
                    min_initial_len=min_initial_len,
                )
                if is_single_g:
                    p_role_g = "standalone"
                rec = SessionRecord(
                    name=name,
                    source_dir=gemini_cli_dir_cfg,
                    filepath=fp_g,
                    source_format="gemini_cli",
                    user_text=user_text,
                    chunk_count=len(messages),
                    user_chunk_count=sum(1 for m in messages if m.type.value == "user"),
                    era=era,
                    has_srt="srt" in lower_sample,
                    has_transcript="transcript" in lower_sample,
                    project_hash=getattr(session_info, "project_hash", ""),
                    prose_frac=pf_g,
                    prompt_role=p_role_g,
                )
                apply_codes(rec, tech_patterns, role_patterns, keyword_maps, scoring_weights, marker_window=mw)
                records.append(rec)
                tri.update(get_ngrams(prose_text_g, 3))
                quad.update(get_ngrams(prose_text_g, 4))

    # Process Claude Code sessions via auto-discovery
    include_claude = cfg.get("_include_claude", True)  # True when no filter or filter=claude/all
    if source_filter and source_filter not in ("claude", "all"):
        include_claude = False
    if include_claude:
        try:
            import os as _os
            from ai_session_search.engine import SessionRecoveryEngine
            from ai_session_search.config import resolve_claude_dir
            _claude_base = resolve_claude_dir()
            _projects = Path(_os.getenv(
                "AI_SESSION_SEARCH_PROJECTS", str(_claude_base / "projects")
            )).expanduser()
            _recovery = Path(_os.getenv(
                "AI_SESSION_SEARCH_RECOVERY", str(_claude_base / "recovery")
            )).expanduser()
            claude_engine = SessionRecoveryEngine(_projects, _recovery)
            claude_sessions = claude_engine.get_sessions()
            print(f"Analyzing {len(claude_sessions)} Claude Code sessions...")
            _claude_ok = _claude_empty = _claude_no_user = _claude_err = 0
            for session_info in claude_sessions:
                try:
                    messages = claude_engine.get_messages(session_info.session_id)
                    if not messages:
                        _claude_empty += 1
                        continue
                    user_text = " ".join(
                        m.content for m in messages
                        if m.type.value == "user" and m.content
                    )
                    if not user_text.strip():
                        _claude_no_user += 1
                        continue
                    name = session_info.session_id
                    fp_c = f"claude/{session_info.project_dir}/{name}"
                    era = _detect_era(
                        name, user_text, filepath=fp_c,
                        timestamp=session_info.timestamp_first,
                    )
                    lower_sample = user_text[:5000].lower()
                    prose_text_c = extract_prose(user_text)
                    pf_c = prose_fraction(user_text)
                    user_msgs_c = [m for m in messages if m.type.value == "user"]
                    first_user_c = user_msgs_c[0].content if user_msgs_c else ""
                    is_single_c = len(user_msgs_c) == 1
                    p_role_c = classify_prompt_role(
                        first_user_c, is_first_in_session=True,
                        continuation_markers=continuation_markers,
                        min_initial_len=min_initial_len,
                    )
                    if is_single_c:
                        p_role_c = "standalone"
                    rec = SessionRecord(
                        name=name,
                        source_dir=session_info.project_dir,
                        filepath=fp_c,
                        source_format="claude_jsonl",
                        user_text=user_text,
                        chunk_count=len(messages),
                        user_chunk_count=len(user_msgs_c),
                        era=era,
                        has_srt="srt" in lower_sample,
                        has_transcript="transcript" in lower_sample,
                        prose_frac=pf_c,
                        prompt_role=p_role_c,
                        cwd=session_info.cwd or "",
                    )
                    apply_codes(rec, tech_patterns, role_patterns, keyword_maps, scoring_weights, marker_window=mw)
                    records.append(rec)
                    tri.update(get_ngrams(prose_text_c, 3))
                    quad.update(get_ngrams(prose_text_c, 4))
                    _claude_ok += 1
                except Exception:
                    _claude_err += 1
            _claude_skip = len(claude_sessions) - _claude_ok
            if _claude_skip:
                print(f"  Skipped {_claude_skip} Claude sessions "
                      f"({_claude_empty} no messages, {_claude_no_user} no user text, "
                      f"{_claude_err} errors)")
        except ImportError:
            pass  # SessionRecoveryEngine not available
        except Exception as exc:
            print(f"Warning: Claude session analysis failed: {exc}")

    print(f"Total after all sources: {len(records)} sessions")

    # Build source_names from which sources contributed records
    _source_names: list[str] = []
    _formats_seen = {r.source_format for r in records}
    if any(f in _formats_seen for f in ("aistudio_json", "markdown")):
        _source_names.append("AI Studio")
    if "gemini_json" in _formats_seen:
        _source_names.append("Gemini")
    if "claude_jsonl" in _formats_seen:
        _source_names.append("Claude Code")

    compute_descendant_boost(records, scoring_weights.get("descendant_boost", 15))

    # Write DB (metadata only, no user_text)
    db = [r.to_db_dict() for r in records]
    with open(db_file, "w", encoding="utf-8") as f:
        json.dump(db, f, indent=2)
    print(f"Analysis complete: {len(records)} sessions -> {db_file}")

    write_vocab_report(tri, quad, vocab_output, min_freq=min_ngram_freq, stop_words=stop_words,
                       source_names=_source_names or None)
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
