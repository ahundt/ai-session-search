#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Emit stable JSON from public Python message or temporal-session APIs."""

from __future__ import annotations

import argparse
import json
import os

import ai_session_search as aise


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", required=True)
    parser.add_argument(
        "--operation",
        choices=("message-search", "session-list", "session-search"),
        default="message-search",
    )
    parser.add_argument("--query")
    parser.add_argument("--mode", choices=("literal", "regex", "fuzzy"), default="literal")
    parser.add_argument("--field", choices=("content", "tool_name", "tool_argument"), default="content")
    parser.add_argument("--argument-path")
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--since")
    parser.add_argument("--until")
    parser.add_argument("--when")
    args = parser.parse_args()
    os.environ["AI_SESSION_SEARCH_INDEX_REFRESH"] = "existing-only"
    search = aise.SessionSearch(args.fixture, threads=1)
    if args.operation == "message-search":
        if args.query is None:
            parser.error("--query is required for message-search")
        request = aise.MessageSearchRequest(field=args.field, argument_path=args.argument_path, limit=args.limit)
        results = search.search_messages(args.query, request, query_mode=args.mode).results
    else:
        dates = aise.DateRange(since=args.since, until=args.until, when=args.when)
        request = aise.SessionQuery(dates=dates, limit=args.limit)
        if args.operation == "session-list":
            results = [session.id for session in search.list_sessions(request)]
        else:
            if args.query is None:
                parser.error("--query is required for session-search")
            results = [hit.session.id for hit in search.search_sessions(args.query, request)]
    print(json.dumps(results, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
