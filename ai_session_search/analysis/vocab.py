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
from ai_session_search.analysis.codebook import load_scoring_weights, load_stop_words
from ai_session_search.analysis.indexed import (
    open_analysis_service,
)
from ai_session_search.analysis.rust_policy import analyze_index_snapshot, build_analysis_policy
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
    service = open_analysis_service(search, refresh_index=refresh_index)
    policy, _ = build_analysis_policy(
        cfg,
        org_dir,
        max_classification_chars=None,
        include_classifications=False,
    )
    result = analyze_index_snapshot(
        service,
        provider=source_filter,
        policy=policy,
    )
    tri = Counter({item.phrase: item.occurrences for item in result.vocabulary if item.words == 3})
    quad = Counter({item.phrase: item.occurrences for item in result.vocabulary if item.words == 4})
    total = sum(item.has_user_text for item in result.sessions.values())
    print(f"Mined {total} sessions in one Rust snapshot")
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
