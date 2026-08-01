#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Run one MCP initialize/search/shutdown exchange and emit canonical structured JSON."""

from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
from pathlib import Path


def parse_reader_bound(value: str) -> str | int:
    if value in {"auto", "host"}:
        return value
    try:
        parsed = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError('max concurrent reads must be "auto", "host", or a positive integer') from error
    if parsed < 1:
        raise argparse.ArgumentTypeError('max concurrent reads must be "auto", "host", or a positive integer')
    return parsed


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--query", required=True)
    parser.add_argument("--mode", choices=("literal", "regex", "fuzzy"), default="literal")
    parser.add_argument("--field", choices=("content", "tool_name", "tool_argument"), default="content")
    parser.add_argument("--argument-path")
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--requests", type=int, default=1)
    parser.add_argument("--max-concurrent-reads", type=parse_reader_bound)
    args = parser.parse_args()
    if args.requests < 1:
        parser.error("--requests must be a positive integer")

    with tempfile.TemporaryDirectory(prefix="aise-mcp-benchmark-") as temporary_dir:
        command = [
            args.binary,
            "--database",
            args.fixture,
            "--index-refresh",
            "existing-only",
        ]
        if args.max_concurrent_reads is not None:
            config_path = Path(temporary_dir) / "config.toml"
            encoded_bound = f'"{args.max_concurrent_reads}"' if args.max_concurrent_reads in {"auto", "host"} else str(args.max_concurrent_reads)
            config_path.write_text(
                f"[mcp]\nmax_concurrent_reads = {encoded_bound}\n",
                encoding="utf-8",
            )
            command.extend(("--config", str(config_path)))
        command.extend(("mcp", "serve"))
        child = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        assert child.stdin is not None and child.stdout is not None
        requests = [
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "aise-benchmark", "version": "1"},
                },
            },
            {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        ]
        search_arguments = {
            "query": args.query,
            "query_mode": args.mode,
            "field": args.field,
            **({"argument_path": args.argument_path} if args.argument_path else {}),
            "limit": args.limit,
            "detail": "full",
        }
        requests.extend(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": "search_messages", "arguments": search_arguments},
            }
            for request_id in range(2, args.requests + 2)
        )
        for request in requests:
            child.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        child.stdin.close()
        responses = [json.loads(line) for line in child.stdout if line.strip()]
        stderr = child.stderr.read() if child.stderr is not None else ""
        return_code = child.wait()
    if return_code != 0:
        raise SystemExit(f"MCP server exited {return_code}: {stderr}")
    search_responses = {item.get("id"): item for item in responses if item.get("id") != 1}
    expected_ids = set(range(2, args.requests + 2))
    if set(search_responses) != expected_ids:
        raise SystemExit(f"MCP returned response IDs {sorted(search_responses)}; expected {sorted(expected_ids)}")
    structured_results = []
    for request_id in sorted(expected_ids):
        response = search_responses[request_id]
        if "structuredContent" not in response.get("result", {}):
            raise SystemExit(f"MCP search failed: {json.dumps(response, sort_keys=True)}")
        structured_results.append(response["result"]["structuredContent"]["results"])
    if any(result != structured_results[0] for result in structured_results[1:]):
        raise SystemExit("concurrent MCP searches returned different structured hits")
    print(json.dumps(structured_results[0], sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
