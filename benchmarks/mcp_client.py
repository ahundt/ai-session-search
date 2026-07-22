#!/usr/bin/env python3
"""Run one MCP initialize/search/shutdown exchange and emit canonical structured JSON."""

from __future__ import annotations

import argparse
import json
import subprocess


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--query", required=True)
    parser.add_argument("--mode", choices=("literal", "regex", "fuzzy"), default="literal")
    parser.add_argument("--field", choices=("content", "tool_name", "tool_argument"),
                        default="content")
    parser.add_argument("--argument-path")
    parser.add_argument("--limit", type=int, default=10)
    args = parser.parse_args()
    child = subprocess.Popen(
        [args.binary, "--database", args.fixture, "--index-refresh", "existing-only",
         "mcp", "serve"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    assert child.stdin is not None and child.stdout is not None
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {
                "name": "aise-benchmark", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "search_messages", "arguments": {"query": args.query,
                "query_mode": args.mode, "field": args.field,
                **({"argument_path": args.argument_path} if args.argument_path else {}),
                "limit": args.limit, "response_format": "detailed"}}},
    ]
    for request in requests:
        child.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
    child.stdin.close()
    responses = [json.loads(line) for line in child.stdout if line.strip()]
    stderr = child.stderr.read() if child.stderr is not None else ""
    return_code = child.wait()
    if return_code != 0:
        raise SystemExit(f"MCP server exited {return_code}: {stderr}")
    response = next(item for item in responses if item.get("id") == 2)
    if "structuredContent" not in response.get("result", {}):
        raise SystemExit(f"MCP search failed: {json.dumps(response, sort_keys=True)}")
    print(json.dumps(response["result"]["structuredContent"]["hits"], sort_keys=True,
                     separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
