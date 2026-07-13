"""
AI Studio Knowledge Base Orchestrator - backs `aise organize`.

Reads session_db.json, creates taxonomy output in one or more configurable formats,
writes INDEX.md, SESSIONS_FULL.md, and KNOWLEDGE_GRAPH.md.

Output formats (config.json["organize_formats"] or --format CLI flag):
  "symlinks"  — non-destructive symlink taxonomy dirs (default)
  "json"      — SESSION_TAXONOMY.json  {name: {dim: [cats], utility, era}}
  "markdown"  — TAXONOMY.md  grouped by dimension

Taxonomy dimensions (config.json["taxonomy_dimensions"]):
  Each dimension is a dict. Required keys depend on "match" type:

  COMMON (all dimensions):
    name             — directory name for the taxonomy dimension  (required)
    match            — "field" or "keyword"  (required)
    exclude          — list of category values to skip  (optional, default [])
    prefer_for_links — false to exclude this dim from INDEX.md link targets  (optional, default true)
    label            — human-readable display label for INDEX.md  (optional; auto-derived from name)

  match="field"  — reads a session record field directly:
    field    — name of the record field (e.g. "techniques", "era", "roles")  (required)
    scalar   — true if field holds a single string, not a list  (optional, default false)

    Example — add a dimension that groups by source format:
      {"name": "09_by_source", "match": "field", "field": "source_format",
       "scalar": true, "prefer_for_links": false}

    Example — group by working directory (only sessions with a cwd are linked):
      {"name": "08_by_working_dir", "match": "field", "field": "cwd",
       "scalar": true, "exclude": [""], "prefer_for_links": false,
       "label": "08 By Working Dir"}
    Sessions with cwd="" are skipped (no fallback). Populated for Claude Code sessions
    (from JSONL cwd field); empty for AI Studio and Gemini CLI sessions.

  match="keyword"  — classifies by matching keywords from a keyword_map:
    keyword_map  — key into config.json["keyword_maps"]  (required)
    source_field — which record field to match against (e.g. "name", "techniques")  (required)
    match_type   — "substring" (field text contains keyword) or
                   "set_intersection" (field list shares an element with keywords)  (required)
    fallback     — category to assign when no keywords match  (optional)

    Example — add a dimension grouping by language detected in session name:
      {"name": "09_by_language", "match": "keyword",
       "keyword_map": "language_map", "source_field": "name",
       "match_type": "substring", "fallback": "english"}
      Also add "language_map": {"python": ["python", "py"], ...} to keyword_maps.

  Run `aise organize --validate` to check config health before running the full pipeline.

METHODOLOGICAL REFERENCES:
- Hsieh & Shannon (2005): https://journals.sagepub.com/doi/10.1177/1049732305276687
- SAGE/Nature archiving: https://journals.sagepub.com/doi/full/10.1177/00016993211051521

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""
from __future__ import annotations

import contextlib
import json
import os
import re
from collections import defaultdict
from pathlib import Path

from ai_session_search.config import load_config, get_config_section, resolve_org_dir
from ai_session_search.analysis.codebook import load_keyword_maps, load_scoring_weights

VALID_FORMATS: frozenset[str] = frozenset({"symlinks", "json", "markdown"})

# Default taxonomy dimensions — reproduces the previous hardcoded behavior exactly.
# Override by setting config.json["taxonomy_dimensions"].
_DEFAULT_TAXONOMY_DIMENSIONS: list[dict] = [
    {
        "name": "01_by_project",
        "match": "keyword",
        "keyword_map": "project_map",
        "source_field": "name",
        "match_type": "substring",
        "fallback": "misc_research",
        "prefer_for_links": True,
    },
    {
        "name": "02_by_workflow",
        "match": "keyword",
        "keyword_map": "workflow_map",
        "source_field": "techniques",
        "match_type": "set_intersection",
        "prefer_for_links": True,
    },
    {
        "name": "03_by_technique",
        "match": "field",
        "field": "techniques",
        "prefer_for_links": True,
    },
    {
        "name": "04_by_task",
        "match": "field",
        "field": "task_categories",
        "prefer_for_links": True,
    },
    {
        "name": "05_by_expert_role",
        "match": "field",
        "field": "roles",
        "prefer_for_links": True,
    },
    {
        "name": "06_by_writing_method",
        "match": "field",
        "field": "writing_methods",
        "prefer_for_links": True,
    },
    {
        "name": "07_by_era",
        "match": "field",
        "field": "era",
        "scalar": True,
        "exclude": ["unknown"],
        "prefer_for_links": False,
    },
    {
        "name": "08_by_working_dir",
        "match": "field",
        "field": "cwd",
        "scalar": True,
        "exclude": [""],
        "prefer_for_links": False,
        "label": "08 By Working Dir",
    },
]


_VALID_MATCH_TYPES = frozenset({"substring", "set_intersection"})
_VALID_MATCH_VALUES = frozenset({"field", "keyword"})

# Per-process set to avoid repeating the same warning for every session
_warned_missing_maps: set[str] = set()


def validate_taxonomy_dimensions(dims: list[dict], keyword_maps: dict | None = None) -> list[str]:
    """Validate taxonomy dimension configs. Returns list of error strings (empty = OK).

    Checks:
    - Required keys present for each match type
    - Valid match and match_type values
    - keyword_map references exist in keyword_maps (warnings, not errors)

    Call this before running orchestration to surface config problems early.
    """
    errors: list[str] = []
    for i, dim in enumerate(dims):
        loc = f"taxonomy_dimensions[{i}] (name={dim.get('name', '<missing>')})"
        if "name" not in dim:
            errors.append(f"{loc}: missing required key 'name'")
        match = dim.get("match")
        if match not in _VALID_MATCH_VALUES:
            errors.append(
                f"{loc}: 'match' must be one of {sorted(_VALID_MATCH_VALUES)}, got {match!r}"
            )
            continue  # can't check further without knowing match type
        if match == "field":
            if "field" not in dim:
                errors.append(
                    f"{loc}: match='field' requires 'field' key "
                    f"(name of the record field to read, e.g. \"techniques\")"
                )
        elif match == "keyword":
            for key in ("keyword_map", "match_type"):
                if key not in dim:
                    errors.append(
                        f"{loc}: match='keyword' requires '{key}' key"
                    )
            mt = dim.get("match_type")
            if mt and mt not in _VALID_MATCH_TYPES:
                errors.append(
                    f"{loc}: 'match_type' must be one of {sorted(_VALID_MATCH_TYPES)}, got {mt!r}"
                )
            if "source_field" not in dim and "match_field" not in dim:
                errors.append(
                    f"{loc}: match='keyword' requires 'source_field' key "
                    f"(which record field to search, e.g. \"name\" or \"techniques\")"
                )
            # Warn (not error) if keyword_map reference is missing from keyword_maps
            if keyword_maps is not None and "keyword_map" in dim:
                kmap_name = dim["keyword_map"]
                if kmap_name not in keyword_maps or not keyword_maps[kmap_name]:
                    errors.append(
                        f"{loc}: keyword_map={kmap_name!r} is empty or missing from "
                        f"keyword_maps config. Sessions will all use fallback={dim.get('fallback')!r}. "
                        f"Add entries to config.json[\"keyword_maps\"][\"{kmap_name}\"]."
                    )
    return errors


def load_taxonomy_dimensions(keyword_maps: dict | None = None) -> list[dict]:
    """Load taxonomy dimensions from config.json["taxonomy_dimensions"] or module default.

    Validates the loaded config and raises ValueError with clear messages if invalid.
    Pass keyword_maps to also check that referenced maps exist.
    """
    dims = get_config_section("taxonomy_dimensions")
    if dims and isinstance(dims, list):
        errors = validate_taxonomy_dimensions(dims, keyword_maps)
        if errors:
            raise ValueError(
                "Invalid taxonomy_dimensions config:\n" + "\n".join(f"  - {e}" for e in errors)
            )
        return dims
    return _DEFAULT_TAXONOMY_DIMENSIONS


def _dim_label(name: str) -> str:
    """Convert dim name like '03_by_technique' to display label '03 By Technique'."""
    return re.sub(r"[_]+", " ", name).title()


def make_symlink(source_path: str, link_path: Path) -> bool:
    """Create symlink non-destructively. Skip if already exists. Returns True if created."""
    link_path.parent.mkdir(parents=True, exist_ok=True)
    if link_path.exists() or link_path.is_symlink():
        return False
    with contextlib.suppress(OSError):
        rel = os.path.relpath(source_path, link_path.parent)
        link_path.symlink_to(rel)
        return True
    return False


def assign_taxonomy(
    rec: dict,
    keyword_maps: dict[str, dict[str, list[str]]],
    dimensions: list[dict],
) -> dict[str, list[str]]:
    """Return {taxonomy_dir: [categories]} for a single session record.

    Args:
        rec:          Session DB record (dict with techniques, roles, era, etc.)
        keyword_maps: All keyword maps keyed by map name (project_map, workflow_map, etc.)
        dimensions:   Taxonomy dimension configs from load_taxonomy_dimensions()
    """
    assignments: defaultdict[str, list[str]] = defaultdict(list)

    for dim in dimensions:
        dim_name = dim["name"]
        match = dim.get("match", "field")
        exclude: set[str] = set(dim.get("exclude", []))

        if match == "field":
            field = dim["field"]
            val = rec.get(field)
            if dim.get("scalar", False):
                # Single-value field (e.g. era)
                if val is not None:
                    s = str(val)
                    if s and s not in exclude:
                        assignments[dim_name].append(s)
            else:
                # List field (e.g. techniques, roles)
                for item in (val or []):
                    if item and str(item) not in exclude:
                        assignments[dim_name].append(str(item))

        elif match == "keyword":
            kmap_name = dim["keyword_map"]
            kmap = keyword_maps.get(kmap_name, {})
            # source_field is the canonical name; match_field is kept for backward compat
            source_field = dim.get("source_field") or dim.get("match_field", "name")
            match_type = dim.get("match_type", "substring")
            fallback = dim.get("fallback")

            if not kmap:
                warn_key = f"{dim_name}:{kmap_name}"
                if warn_key not in _warned_missing_maps:
                    _warned_missing_maps.add(warn_key)
                    import sys
                    print(
                        f"WARNING: taxonomy dimension '{dim_name}' references "
                        f"keyword_map '{kmap_name}' which is empty or missing. "
                        f"Sessions will use fallback={fallback!r}. "
                        f"Add entries to config.json[\"keyword_maps\"][\"{kmap_name}\"].",
                        file=sys.stderr,
                    )

            field_val = rec.get(source_field, "")
            matched = False

            if match_type == "substring":
                field_str = (
                    field_val if isinstance(field_val, str)
                    else " ".join(str(v) for v in (field_val or []))
                ).lower()
                for cat, keywords in kmap.items():
                    if cat in exclude:
                        continue
                    if any(kw.lower() in field_str for kw in keywords):
                        assignments[dim_name].append(cat)
                        matched = True

            elif match_type == "set_intersection":
                field_set = set(field_val or [])
                for cat, keywords in kmap.items():
                    if cat in exclude:
                        continue
                    if field_set & set(keywords):
                        assignments[dim_name].append(cat)
                        matched = True

            if not matched and fallback and fallback not in exclude:
                assignments[dim_name].append(fallback)

    return dict(assignments)


def build_taxonomy(
    records: list[dict],
    keyword_maps: dict[str, dict[str, list[str]]],
    dimensions: list[dict],
) -> dict[str, dict[str, list[str]]]:
    """Compute taxonomy assignments for all records.

    Returns {session_name: {taxonomy_dim: [categories]}}.
    Pure computation — no filesystem side effects.
    """
    result: dict[str, dict[str, list[str]]] = {}
    for rec in records:
        name = rec.get("name")
        if name:
            result[name] = assign_taxonomy(rec, keyword_maps, dimensions)
    return result


def taxonomy_to_session_paths(taxonomy: dict[str, dict[str, list[str]]]) -> dict[str, list[str]]:
    """Flatten taxonomy to session_paths {name: ["dim/cat", ...]} for write_index."""
    return {
        name: [f"{dim}/{cat}" for dim, cats in dims.items() for cat in cats]
        for name, dims in taxonomy.items()
    }


def _preferred_link_path(
    primary_paths: list[str],
    dimensions: list[dict],
) -> str:
    """Select the best link path from a session's taxonomy paths.

    Prefers dims with prefer_for_links=True; skips fallback categories.
    Returns first non-skipped path, or primary_paths[0] as last resort.
    """
    non_preferred = {d["name"] for d in dimensions if not d.get("prefer_for_links", True)}
    fallback_cats = {d["fallback"] for d in dimensions if d.get("fallback")}

    for p in primary_paths:
        parts = p.split("/", 1)
        dim_part = parts[0]
        cat_part = parts[1] if len(parts) > 1 else ""
        if dim_part in non_preferred:
            continue
        if cat_part in fallback_cats:
            continue
        return p
    return primary_paths[0] if primary_paths else ""


def apply_symlinks(
    records: list[dict],
    org_dir: Path,
    taxonomy: dict[str, dict[str, list[str]]],
) -> int:
    """Create symlinks from pre-computed taxonomy. Non-destructive. Returns new symlink count."""
    created = 0
    for rec in records:
        raw_fp = rec.get("filepath", "")
        if not raw_fp:
            continue
        filepath = str(Path(raw_fp).expanduser())
        if not Path(filepath).exists():
            continue
        name = rec["name"]
        for dim, categories in taxonomy.get(name, {}).items():
            for cat in categories:
                link_path = org_dir / dim / cat / name
                if make_symlink(filepath, link_path):
                    created += 1
    return created


def write_taxonomy_json(
    taxonomy: dict[str, dict[str, list[str]]],
    records: list[dict],
    org_dir: Path,
) -> None:
    """Write SESSION_TAXONOMY.json: {name: {taxonomy, utility, era}} for all sessions."""
    name_to_rec = {r["name"]: r for r in records}
    output = {
        name: {
            "taxonomy": dims,
            "utility": name_to_rec.get(name, {}).get("utility", 0),
            "era": name_to_rec.get(name, {}).get("era", "unknown"),
        }
        for name, dims in taxonomy.items()
    }
    path = org_dir / "SESSION_TAXONOMY.json"
    path.write_text(json.dumps(output, indent=2, ensure_ascii=False), encoding="utf-8")
    print(f"SESSION_TAXONOMY.json: {len(taxonomy)} sessions")


def write_taxonomy_markdown(
    taxonomy: dict[str, dict[str, list[str]]],
    records: list[dict],
    org_dir: Path,
    dimensions: list[dict] | None = None,
) -> None:
    """Write TAXONOMY.md: sessions grouped by taxonomy dimension and category."""
    name_to_rec = {r["name"]: r for r in records}
    sw = load_scoring_weights()
    min_utility = int(sw.get("min_utility_for_index", 20))

    # Ordered dim names for display
    dim_order = [d["name"] for d in (dimensions or [])]

    # {dim: {cat: [names]}}
    dim_cat_names: defaultdict[str, defaultdict[str, list[str]]] = defaultdict(
        lambda: defaultdict(list)
    )
    for name, dims in taxonomy.items():
        for dim, cats in dims.items():
            for cat in cats:
                dim_cat_names[dim][cat].append(name)

    # Respect configured dim order; append any extra dims not in config
    ordered_dims = dim_order + [d for d in sorted(dim_cat_names) if d not in dim_order]

    lines = ["# Session Taxonomy\n\n"]
    for dim in ordered_dims:
        if dim not in dim_cat_names:
            continue
        dim_cfg = next((d for d in (dimensions or []) if d["name"] == dim), {})
        label = dim_cfg.get("label") or _dim_label(dim)
        lines.append(f"## {label}\n\n")
        for cat in sorted(dim_cat_names[dim]):
            names = dim_cat_names[dim][cat]
            qualifying = [
                n for n in names
                if name_to_rec.get(n, {}).get("utility", 0) >= min_utility
            ]
            if not qualifying:
                continue
            lines.append(f"### {cat} ({len(qualifying)} sessions)\n\n")
            lines.append("| Session | Utility | Era |\n| :--- | :--- | :--- |\n")
            for name in sorted(
                qualifying,
                key=lambda n: name_to_rec.get(n, {}).get("utility", 0),
                reverse=True,
            ):
                util = name_to_rec.get(name, {}).get("utility", 0)
                era = name_to_rec.get(name, {}).get("era", "—")
                lines.append(f"| {name} | {util} | {era} |\n")
            lines.append("\n")

    path = org_dir / "TAXONOMY.md"
    path.write_text("".join(lines), encoding="utf-8")
    print(f"TAXONOMY.md: {len(dim_cat_names)} dimensions")


def write_index(
    records: list[dict],
    session_paths: dict[str, list[str]],
    org_dir: Path,
    dimensions: list[dict] | None = None,
    source_names: list[str] | None = None,
) -> None:
    """Write INDEX.md and SESSIONS_FULL.md. Always written regardless of format.

    Uses dimensions to generate the Taxonomy section and select preferred link targets.
    min_utility_for_index loaded from scoring_weights (default 20).
    source_names: list of source names for dynamic header (e.g. ["Claude Code"]).
    """
    sw = load_scoring_weights(org_dir)
    min_utility = int(sw.get("min_utility_for_index", 20))
    dims = dimensions or _DEFAULT_TAXONOMY_DIMENSIONS

    sorted_recs = sorted(records, key=lambda r: r.get("utility", 0), reverse=True)
    all_ranked = [r for r in sorted_recs if r.get("utility", 0) >= min_utility]

    source_label = ", ".join(source_names) if source_names else "AI Session"
    lines = [
        f"# {source_label} Knowledge Base: Integrated Dashboard\n\n",
        "Ranked by utility score. Pattern matching inspired by Directed Content Analysis "
        "([Hsieh & Shannon, 2005](https://journals.sagepub.com/doi/10.1177/1049732305276687)); "
        "detects [Chain-of-Thought prompting](https://arxiv.org/abs/2201.11903) patterns "
        "(Wei et al., 2022).\n\n",
        "## Hall of Fame: Top Sessions by Utility\n\n",
        "| Rank | Utility | Session | Technique | Role | Era |\n",
        "| :--- | :--- | :--- | :--- | :--- | :--- |\n",
    ]

    for count, rec in enumerate(all_ranked, 1):
        name = rec["name"]
        primary_paths = session_paths.get(name, [])
        link_target = _preferred_link_path(primary_paths, dims)
        if not link_target and primary_paths:
            link_target = primary_paths[0]
        if not link_target:
            link_target = "01_by_project/misc_research"
        encoded = name.replace(" ", "%20").replace("&", "%26")
        link = f"[{name}]({link_target}/{encoded})"
        tech = (rec.get("techniques") or ["—"])[0]
        role = (rec.get("roles") or ["—"])[0]
        era = rec.get("era", "—")
        util = rec.get("utility", 0)
        lines.append(f"| {count} | {util} | {link} | {tech} | {role} | {era} |\n")

    lines.append(
        f"\n*Full list: {len(all_ranked)} sessions ranked. "
        f"See [SESSIONS_FULL.md](SESSIONS_FULL.md).*\n\n"
    )

    # Taxonomy section — generated from configured dimensions
    lines.append("## Taxonomy\n\n")
    for dim in dims:
        label = dim.get("label") or _dim_label(dim["name"])
        lines.append(f"- [{label}]({dim['name']}/)\n")
    lines += [
        "\n## Governance\n\n",
        "- [Codebook](CODEBOOK.md)\n",
        "- [References](REFERENCES.md)\n",
        "- [User Instructions](USER_INSTRUCTIONS_CLEAN.md)\n",
        "- [Knowledge Graph](KNOWLEDGE_GRAPH.md)\n",
        "- [Vocabulary Analysis](VOCABULARY_ANALYSIS.md)\n",
        "- [All Sessions Ranked](SESSIONS_FULL.md)\n",
    ]

    with open(org_dir / "INDEX.md", "w", encoding="utf-8") as f:
        f.writelines(lines)

    # SESSIONS_FULL.md: all ranked sessions, no truncation
    full_lines = [
        "# All Sessions: Complete Ranked List\n\n",
        f"{len(all_ranked)} sessions with utility >= {min_utility}, ranked by score.\n\n",
        "| Rank | Utility | Session | Technique | Role | Era |\n",
        "| :--- | :--- | :--- | :--- | :--- | :--- |\n",
    ]
    for i, rec in enumerate(all_ranked, 1):
        name = rec["name"]
        paths = session_paths.get(name, [])
        lp = _preferred_link_path(paths, dims)
        if not lp and paths:
            lp = paths[0]
        if not lp:
            lp = "01_by_project/misc_research"
        enc = name.replace(" ", "%20").replace("&", "%26")
        full_lines.append(
            f"| {i} | {rec.get('utility', 0)} | [{name}]({lp}/{enc}) "
            f"| {(rec.get('techniques') or ['—'])[0]} | {(rec.get('roles') or ['—'])[0]} "
            f"| {rec.get('era', '—')} |\n"
        )

    with open(org_dir / "SESSIONS_FULL.md", "w", encoding="utf-8") as f:
        f.writelines(full_lines)

    print(f"INDEX.md: {len(all_ranked)} entries; SESSIONS_FULL.md: {len(all_ranked)} total")


def write_knowledge_graph(records: list[dict], org_dir: Path) -> None:
    """Write session lineage graph in Mermaid markdown."""
    name_to_rec = {r["name"]: r for r in records}
    children: defaultdict[str, list[str]] = defaultdict(list)
    roots = []

    for rec in records:
        parent = rec.get("graph_parent")
        if parent and parent in name_to_rec:
            children[parent].append(rec["name"])
        elif not parent:
            roots.append(rec["name"])

    def emit_node(node: str, out: list[str], depth: int = 0, max_depth: int = 3) -> None:
        safe = node.replace('"', "'").replace("(", "[").replace(")", "]")[:60]
        for child in children.get(node, [])[:8]:
            child_safe = child.replace('"', "'").replace("(", "[").replace(")", "]")[:60]
            out.append(f'    "{safe}" --> "{child_safe}"\n')
            if depth < max_depth:
                emit_node(child, out, depth + 1, max_depth)

    lines = [
        "# Knowledge Graph: Session Lineage\n\n",
        "Maps provenance relationships: ROOT -> VERSION chains and BRANCH derivations.\n",
        "Source: https://journals.sagepub.com/doi/full/10.1177/00016993211051521\n\n",
        "## Major Lineage Chains\n\n",
    ]

    significant_roots = [r for r in roots if children.get(r)]
    for root in sorted(
        significant_roots,
        key=lambda n: name_to_rec.get(n, {}).get("utility", 0),
        reverse=True,
    )[:30]:
        util = name_to_rec.get(root, {}).get("utility", 0)
        lines.append(f"### {root} (utility: {util})\n\n")
        lines.append("```mermaid\ngraph TD\n")
        emit_node(root, lines)
        lines.append("```\n\n")

    orphans = [r for r in roots if not children.get(r)]
    lines.append(f"## Standalone Sessions\n\n{len(orphans)} sessions with no detected lineage.\n\n")

    with open(org_dir / "KNOWLEDGE_GRAPH.md", "w", encoding="utf-8") as f:
        f.writelines(lines)
    print("KNOWLEDGE_GRAPH.md written")


def _resolve_formats(cfg: dict, formats: list[str] | None) -> list[str]:
    """Resolve output formats: parameter > config > default ["symlinks"].

    Accepts a list or comma-separated string from config.
    Raises ValueError for unknown format names.
    """
    if formats is not None:
        resolved = formats
    else:
        cfg_val = cfg.get("organize_formats")
        if isinstance(cfg_val, str):
            resolved = [f.strip() for f in cfg_val.split(",") if f.strip()]
        elif isinstance(cfg_val, list):
            resolved = cfg_val
        else:
            resolved = ["symlinks"]

    bad = set(resolved) - VALID_FORMATS
    if bad:
        raise ValueError(
            f"Unknown organize format(s): {sorted(bad)}. "
            f"Valid: {sorted(VALID_FORMATS)}"
        )
    return resolved


def run_orchestration(formats: list[str] | None = None) -> None:
    """Read session_db.json, produce taxonomy output, write index files.

    Args:
        formats: Output formats to produce. None reads from config.json["organize_formats"].
                 Valid values: "symlinks", "json", "markdown" (combinable as a list).
                 Default when unconfigured: ["symlinks"].

    Taxonomy dimensions are read from config.json["taxonomy_dimensions"].
    INDEX.md and SESSIONS_FULL.md are always written regardless of formats.
    """
    cfg = load_config()
    org_dir = resolve_org_dir(cfg)
    db_file = org_dir / "session_db.json"

    if not db_file.exists():
        raise FileNotFoundError(f"Run `aise analyze` first: {db_file}")

    records = json.loads(db_file.read_text(encoding="utf-8"))
    print(f"Loaded {len(records)} session records")

    active_formats = _resolve_formats(cfg, formats)
    print(f"Output formats: {', '.join(active_formats)}")

    keyword_maps = load_keyword_maps()
    dimensions = load_taxonomy_dimensions(keyword_maps=keyword_maps)

    # Always compute taxonomy — needed for all format outputs and write_index
    taxonomy = build_taxonomy(records, keyword_maps, dimensions)
    session_paths = taxonomy_to_session_paths(taxonomy)

    if "symlinks" in active_formats:
        created = apply_symlinks(records, org_dir, taxonomy)
        print(f"Created {created} new symlinks")

    if "json" in active_formats:
        write_taxonomy_json(taxonomy, records, org_dir)

    if "markdown" in active_formats:
        write_taxonomy_markdown(taxonomy, records, org_dir, dimensions=dimensions)

    # Build source_names from source_format values in records
    _formats_seen = {r.get("source_format", "") for r in records}
    _source_names: list[str] = []
    if any(f in _formats_seen for f in ("aistudio_json", "markdown")):
        _source_names.append("AI Studio")
    if "gemini_json" in _formats_seen:
        _source_names.append("Gemini")
    if "claude_jsonl" in _formats_seen:
        _source_names.append("Claude Code")

    # Index files always written
    write_index(records, session_paths, org_dir, dimensions=dimensions,
                source_names=_source_names or None)
    write_knowledge_graph(records, org_dir)
    print("Orchestration complete.")


def main() -> None:
    """Entry point for `aise organize` CLI command."""
    run_orchestration()


if __name__ == "__main__":
    main()
