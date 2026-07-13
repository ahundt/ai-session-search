"""
Vocabulary Miner: Recurring Phrase Analysis - backs `aise vocab`.

Standalone indexed analysis; normally vocabulary is mined inline by `aise analyze`.

Source: PromptEngineering.org
https://www.promptengineering.org/building-a-reusable-prompt-library/

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""
from __future__ import annotations

from collections import Counter
from typing import Any

from ai_session_search.analysis.analyzer import write_vocab_report
from ai_session_search.analysis.codebook import (
    extract_prose,
    get_ngrams,
    load_scoring_weights,
    load_stop_words,
)
from ai_session_search.analysis.indexed import (
    iter_analysis_documents,
    open_analysis_service,
    resolve_page_size,
)
from ai_session_search.config import load_config, resolve_org_dir
from ai_session_search.native import SessionSearch


def mine_all(
    source_filter: str | None = None,
    config: dict[str, Any] | None = None,
    *,
    search: SessionSearch | None = None,
    refresh_index: bool = True,
) -> tuple[Counter[str], Counter[str]]:
    """Mine prose from bounded Rust-indexed documents across canonical providers.

    Uses prose-only extraction to avoid polluting n-grams with code tokens.
    min_session_text_len loaded from config.json[scoring_weights] (default 50).
    """
    cfg = load_config() if config is None else config
    org_dir = resolve_org_dir(cfg)
    sw = load_scoring_weights(org_dir)
    min_len = int(sw.get("min_session_text_len", 50))
    page_size = resolve_page_size(cfg)
    service = open_analysis_service(search, refresh_index=refresh_index)
    tri: Counter[str] = Counter()
    quad: Counter[str] = Counter()
    total = 0

    for document in iter_analysis_documents(
        service,
        provider=source_filter,
        page_size=page_size,
    ):
        if len(document.user_text) < min_len:
            continue
        prose_text = extract_prose(document.user_text)
        tri.update(get_ngrams(prose_text, 3))
        quad.update(get_ngrams(prose_text, 4))
        total += 1

    print(f"Mined {total} sessions")
    return tri, quad


def write_report(
    tri: Counter[str],
    quad: Counter[str],
    config: dict[str, Any] | None = None,
) -> None:
    """Publish the shared vocabulary report with configured thresholds."""
    cfg = load_config() if config is None else config
    org_dir = resolve_org_dir(cfg)
    output_file = org_dir / cfg.get("vocab_output_filename", "VOCABULARY_ANALYSIS.md")

    sw = load_scoring_weights(org_dir)
    min_freq = int(sw.get("min_ngram_freq", 3))
    stop_words = load_stop_words(org_dir)

    write_vocab_report(
        tri,
        quad,
        output_file,
        min_freq=min_freq,
        stop_words=stop_words,
    )


def main() -> None:
    """Entry point for `aise vocab` CLI command."""
    config = load_config()
    tri, quad = mine_all(config=config)
    write_report(tri, quad, config=config)


if __name__ == "__main__":
    main()
