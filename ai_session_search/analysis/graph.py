"""Atomic publication adapter for the canonical Rust session graph."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from ai_session_search._native import NativeSessionGraph
from ai_session_search.analysis.io import write_text_atomic
from ai_session_search.config import load_config, resolve_org_dir


def graph_document(graph: NativeSessionGraph) -> dict[str, Any]:
    """Convert a typed Rust graph to the stable JSON publication schema."""
    nodes = [
        {
            "session_id": node.session_id,
            "provider": node.provider,
            "title": node.title,
            "cwd": node.cwd,
            "repo_root": node.repo_root,
            "created_at": node.created_at,
            "updated_at": node.updated_at,
            "score": node.score,
            "classifications": [
                {
                    "dimension": item.dimension,
                    "label": item.label,
                    "target": item.target,
                    "weight": item.weight,
                }
                for item in node.classifications
            ],
        }
        for node in graph.nodes.values()
    ]
    edges = [
        {
            "source_session_id": edge.source_session_id,
            "target_session_id": edge.target_session_id,
            "kind": edge.kind,
            "rule_id": edge.rule_id,
        }
        for edge in graph.edges
    ]
    groups = [
        {
            "dimension": group.dimension,
            "key": group.key,
            "session_ids": group.session_ids,
        }
        for group in graph.groups
    ]
    return {
        "schema_version": 1,
        "node_count": len(nodes),
        "edge_count": len(edges),
        "group_count": len(groups),
        "nodes": nodes,
        "edges": edges,
        "groups": groups,
    }


def write_session_graph(graph: NativeSessionGraph, output_file: Path) -> None:
    """Atomically replace a complete graph; interrupted runs leave no partial artifact."""
    document = graph_document(graph)
    write_text_atomic(output_file, json.dumps(document, indent=2, ensure_ascii=False))
    print(
        f"SESSION_GRAPH.json: {document['node_count']} nodes, "
        f"{document['edge_count']} edges, {document['group_count']} groups -> {output_file}"
    )


def main() -> None:
    """Re-run indexed analysis and publish its canonical Rust graph."""
    from ai_session_search.analysis.analyzer import run_analysis

    cfg = load_config()
    run_analysis(config=cfg, refresh_index=False)
    output = resolve_org_dir(cfg) / "SESSION_GRAPH.json"
    if not output.exists():
        raise RuntimeError(f"analysis completed without publishing {output}")


if __name__ == "__main__":
    main()
