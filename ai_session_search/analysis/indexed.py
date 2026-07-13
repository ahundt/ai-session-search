"""Shared Rust-indexed document scan lifecycle for outward Python analysis."""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from ai_session_search.native import SessionSearch

DEFAULT_ANALYSIS_PAGE_SIZE = 50
_PROVIDER_ALIASES = {"gemini": "gemini-cli", "gemini_cli": "gemini-cli"}


def canonical_provider(provider: str | None) -> str | None:
    """Normalize only legacy outer aliases; Rust validates canonical providers."""
    if provider in (None, "", "all"):
        return None
    return _PROVIDER_ALIASES.get(provider, provider)


def resolve_page_size(config: Mapping[str, Any]) -> int:
    page_size = int(config.get("analysis_page_size", DEFAULT_ANALYSIS_PAGE_SIZE))
    if page_size <= 0:
        raise ValueError("analysis_page_size must be greater than zero")
    return page_size


def open_analysis_service(
    search: SessionSearch | None,
    *,
    refresh_index: bool,
) -> SessionSearch:
    """Own one native service and optionally refresh configured sources once."""
    service = search or SessionSearch()
    if refresh_index:
        outcome = service.refresh()
        if outcome.status == "skipped_lock_unavailable":
            print(f"Warning: index refresh skipped; analyzing existing index: {outcome.reason}")
    return service
