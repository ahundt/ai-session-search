"""
Vocabulary Miner: Recurring Phrase Analysis - backs `aise vocab`.

Standalone fallback; normally vocabulary is mined inline by `aise analyze`.

Source: PromptEngineering.org
https://www.promptengineering.org/building-a-reusable-prompt-library/

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""
from __future__ import annotations

import contextlib
from collections import Counter
from pathlib import Path

from ai_session_search.config import load_config, resolve_org_dir
from ai_session_search.sources.aistudio import AiStudioSource
from ai_session_search.analysis.codebook import (
    get_ngrams, is_meaningful, load_stop_words, load_scoring_weights, extract_prose,
)


def mine_all() -> tuple[Counter[str], Counter[str]]:
    """Stream prose text from AI Studio sources. O(1) memory per session.

    Uses prose-only extraction to avoid polluting n-grams with code tokens.
    min_session_text_len loaded from config.json[scoring_weights] (default 50).
    """
    cfg = load_config()
    source_dirs_cfg = cfg.get("source_dirs", {}).get("aistudio", [])
    if isinstance(source_dirs_cfg, str):
        source_dirs_cfg = [source_dirs_cfg]
    source_dirs = [Path(p) for p in source_dirs_cfg]

    sw = load_scoring_weights()
    min_len = int(sw.get("min_session_text_len", 50))

    source = AiStudioSource(source_dirs=source_dirs)
    tri: Counter[str] = Counter()
    quad: Counter[str] = Counter()
    total = 0

    for session_info in source.stream_sessions():
        with contextlib.suppress(Exception):
            messages = source.read_session(session_info)
            user_text = " ".join(m.content for m in messages if m.type.value == "user")
            if len(user_text) < min_len:
                continue
            prose_text = extract_prose(user_text)
            tri.update(get_ngrams(prose_text, 3))
            quad.update(get_ngrams(prose_text, 4))
            total += 1

    print(f"Mined {total} sessions")
    return tri, quad


def write_report(tri: Counter[str], quad: Counter[str]) -> None:
    """Write vocabulary report. No arbitrary truncation.

    min_ngram_freq and stop_words loaded from config.json or org_dir files.
    """
    cfg = load_config()
    org_dir = resolve_org_dir(cfg)
    output_file = org_dir / cfg.get("vocab_output_filename", "VOCABULARY_ANALYSIS.md")

    sw = load_scoring_weights(org_dir)
    min_freq = int(sw.get("min_ngram_freq", 3))
    stop_words = load_stop_words(org_dir)

    tri_rows = [(freq, phrase) for phrase, freq in tri.most_common()
                if freq >= min_freq and is_meaningful(phrase, stop_words)]
    quad_rows = [(freq, phrase) for phrase, freq in quad.most_common()
                 if freq >= min_freq and is_meaningful(phrase, stop_words)]

    lines = [
        "# Vocabulary Analysis: Recurring Prompt Phrases\n\n",
        "N-gram analysis of user turns across all AI Studio sessions.\n",
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


def main() -> None:
    """Entry point for `aise vocab` CLI command."""
    tri, quad = mine_all()
    write_report(tri, quad)


if __name__ == "__main__":
    main()
