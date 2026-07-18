#!/usr/bin/env python3
"""Emit a stable JSON projection from the public Python message-search API."""

from __future__ import annotations

import argparse
import json
import os

import ai_session_search as aise


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--query", required=True)
    parser.add_argument("--mode", choices=("exact", "regex", "fuzzy"), default="exact")
    parser.add_argument("--field", choices=("content", "tool_name", "tool_argument"),
                        default="content")
    parser.add_argument("--argument-path")
    parser.add_argument("--limit", type=int, default=10)
    args = parser.parse_args()
    os.environ["AI_SESSION_SEARCH_INDEX_REFRESH"] = "existing-only"
    search = aise.SessionSearch(args.fixture, threads=1)
    request = aise.MessageQuery(
        field=args.field, argument_path=args.argument_path, limit=args.limit
    )
    hits = search.search_messages(args.query, request, match_mode=args.mode)
    fields = (
        "session_id", "provider", "seq", "role", "kind", "timestamp", "tool_name",
        "tool_call_id", "fuzzy_score", "content",
    )
    print(json.dumps([{field: getattr(hit, field) for field in fields} for hit in hits],
                     sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
