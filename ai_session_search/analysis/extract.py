"""Publish one indexed session's user instruction history."""

from __future__ import annotations

from collections.abc import Iterator, Mapping
from itertools import chain
from pathlib import Path
from typing import Any

from ai_session_search.analysis.indexed import (
    DEFAULT_ANALYSIS_PAGE_SIZE,
    open_analysis_service,
    resolve_page_size,
)
from ai_session_search.analysis.io import atomic_text_writer
from ai_session_search.config import load_config, resolve_org_dir
from ai_session_search.native import (
    MessageQuery,
    MessageSelector,
    MessageSequenceRange,
    NativeMessageHit,
    QueryScope,
    SessionSearch,
)


def iter_user_messages(
    search: SessionSearch,
    session_id: str,
    *,
    page_size: int,
) -> Iterator[NativeMessageHit]:
    """Yield a session's user messages in sequence order using bounded keyset pages."""
    if not session_id.strip():
        raise ValueError("session_id must not be empty")
    if page_size <= 0:
        raise ValueError("page_size must be greater than zero")

    seq_from = None
    while True:
        request = MessageQuery(
            scope=QueryScope(session_id=session_id),
            selector=MessageSelector(
                role="user",
                sequence=MessageSequenceRange(seq_from=seq_from),
            ),
            limit=page_size,
        )
        page = search.search_messages("", request)
        if not page:
            return
        yield from page
        seq_from = page[-1].seq + 1
        if len(page) < page_size:
            return


def _write_history(
    output_file: Path,
    session_id: str,
    messages: Iterator[NativeMessageHit],
) -> int:
    iterator = iter(messages)
    first = next(iterator, None)
    resolved_id = first.session_id if first else session_id
    provider = first.provider if first else "unknown"
    count = 0
    with atomic_text_writer(output_file) as output:
        output.write(
            "# User Instruction History\n\n"
            "User messages from the canonical AI Session Search index, in session order.\n\n"
            f"- Session: `{resolved_id}`\n"
            f"- Provider: `{provider}`\n\n"
            "---\n\n"
        )
        messages_in_order = chain((first,), iterator) if first else ()
        for count, message in enumerate(messages_in_order, start=1):
            timestamp = message.timestamp or "timestamp unavailable"
            quoted = (
                "\n".join(
                    f"> {line}" if line.strip() else ">"
                    for line in message.content.splitlines()
                )
                or "> (empty)"
            )
            output.write(
                f"## {count}. Instruction\n\n"
                f"*Sequence {message.seq}; {timestamp}*\n\n"
                f"{quoted}\n\n---\n\n"
            )
    return count


def extract_history(
    session_id: str,
    output_file: str | Path,
    *,
    search: SessionSearch | None = None,
    page_size: int = DEFAULT_ANALYSIS_PAGE_SIZE,
    refresh_index: bool = False,
) -> int:
    """Publish all indexed user messages for one provider-independent session."""
    service = open_analysis_service(search, refresh_index=refresh_index)
    messages = iter_user_messages(service, session_id, page_size=page_size)
    return _write_history(Path(output_file), session_id, messages)


def main(
    session_id: str | None = None,
    *,
    config: Mapping[str, Any] | None = None,
    search: SessionSearch | None = None,
    refresh_index: bool | None = None,
) -> int:
    """Run the configured provider-independent instruction-history export."""
    cfg = dict(config) if config is not None else load_config()
    selected_session = session_id or str(cfg.get("instruction_history_session", ""))
    if not selected_session.strip():
        raise ValueError(
            "instruction history requires a session ID; pass --session or set "
            "instruction_history_session in the configuration"
        )
    output_file = resolve_org_dir(cfg) / "USER_INSTRUCTIONS_CLEAN.md"
    count = extract_history(
        selected_session,
        output_file,
        search=search,
        page_size=resolve_page_size(cfg),
        refresh_index=search is None if refresh_index is None else refresh_index,
    )
    print(f"Wrote {count} user messages to {output_file}")
    return count


if __name__ == "__main__":
    main()
