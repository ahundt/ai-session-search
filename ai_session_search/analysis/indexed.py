"""Shared Rust-indexed document scan lifecycle for outward Python analysis."""

from __future__ import annotations

from ai_session_search.native import SessionSearch

_PROVIDER_ALIASES = {"gemini": "gemini-cli", "gemini_cli": "gemini-cli"}


def canonical_provider(provider: str | None) -> str | None:
    """Normalize only legacy outer aliases; Rust validates canonical providers."""
    if provider in (None, "", "all"):
        return None
    return _PROVIDER_ALIASES.get(provider, provider)


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
