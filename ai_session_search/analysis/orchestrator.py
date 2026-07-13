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

import json
import os
import re
from collections import defaultdict
from hashlib import sha256
from pathlib import Path
from urllib.parse import quote

from ai_session_search.analysis.codebook import load_keyword_maps, load_scoring_weights
from ai_session_search.analysis.io import write_text_atomic
from ai_session_search.config import get_config_section, load_config, resolve_org_dir

VALID_FORMATS: frozenset[str] = frozenset({"symlinks", "json", "markdown"})
DEFAULT_FILESYSTEM_NAME_MAX = 255
TAXONOMY_LINK_MANIFEST_SCHEMA_VERSION = 1
WINDOWS_RESERVED_PATH_STEMS = frozenset(
    {"CON", "PRN", "AUX", "NUL"}
    | {f"COM{index}" for index in range(1, 10)}
    | {f"LPT{index}" for index in range(1, 10)}
)

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


def _record_key(record: dict) -> str:
    """Return canonical identity, with title fallback only for legacy imported artifacts."""
    key = record.get("session_id") or record.get("name")
    if not isinstance(key, str) or not key.strip():
        raise ValueError("taxonomy record requires a non-empty session_id")
    return key


def _path_component(value: str, parent: Path) -> str:
    """Encode untrusted taxonomy values as one portable, bounded path component."""
    encoded = quote(value, safe="")
    if encoded in {"", ".", ".."}:
        raise ValueError(f"taxonomy path component is invalid: {value!r}")
    stem = encoded.split(".", maxsplit=1)[0].upper()
    if stem in WINDOWS_RESERVED_PATH_STEMS or encoded.endswith((".", " ")):
        encoded = f"value-{encoded}"
    try:
        name_max = int(os.pathconf(parent, "PC_NAME_MAX"))
    except (AttributeError, OSError, ValueError):
        name_max = DEFAULT_FILESYSTEM_NAME_MAX
    if len(encoded.encode("utf-8")) <= name_max:
        return encoded
    digest = sha256(value.encode("utf-8")).hexdigest()
    suffix = f"-{digest}"
    budget = name_max - len(suffix)
    if budget <= 0:
        raise ValueError(f"filesystem name limit is too small for taxonomy output: {name_max}")
    return f"{encoded[:budget]}{suffix}"


def _raise_collision(path: Path) -> bool:
    raise FileExistsError(f"taxonomy output collision: {path}")


def make_symlink(source_path: str, link_path: Path) -> bool:
    """Create one validated symlink, raising on collision instead of hiding failures."""
    source = Path(source_path).expanduser().resolve(strict=True)
    link_path.parent.mkdir(parents=True, exist_ok=True)
    if link_path.is_symlink():
        if link_path.resolve(strict=False) == source:
            return False
        return _raise_collision(link_path)
    if link_path.exists():
        return _raise_collision(link_path)
    link_path.symlink_to(os.path.relpath(source, link_path.parent))
    return True


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
        try:
            key = _record_key(rec)
        except ValueError:
            continue
        if key in result:
            raise ValueError(f"duplicate canonical taxonomy session ID: {key}")
        result[key] = assign_taxonomy(rec, keyword_maps, dimensions)
    return result


def taxonomy_to_session_paths(taxonomy: dict[str, dict[str, list[str]]]) -> dict[str, list[str]]:
    """Flatten taxonomy to session_paths {name: ["dim/cat", ...]} for write_index."""
    return {
        session_id: [
            f"{_path_component(dim, Path.cwd())}/{_path_component(cat, Path.cwd())}"
            for dim, cats in dims.items()
            for cat in cats
        ]
        for session_id, dims in taxonomy.items()
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


def _plan_symlinks(
    records: list[dict],
    org_dir: Path,
    taxonomy: dict[str, dict[str, list[str]]],
) -> list[tuple[Path, Path]]:
    planned: list[tuple[Path, Path]] = []
    for rec in records:
        raw_fp = rec.get("filepath", "")
        if not raw_fp:
            continue
        source = Path(raw_fp).expanduser().resolve(strict=True)
        if source == org_dir or org_dir in source.parents:
            raise ValueError(f"taxonomy source must be outside output directory: {source}")
        session_id = _record_key(rec)
        link_name = _path_component(session_id, org_dir)
        for dim, categories in taxonomy.get(session_id, {}).items():
            for cat in categories:
                planned.append((
                    source,
                    org_dir / _path_component(dim, org_dir) / _path_component(cat, org_dir) / link_name,
                ))
    return planned


def _validate_symlink_plan(planned: list[tuple[Path, Path]]) -> None:
    targets = [target for _, target in planned]
    if len(targets) != len(set(targets)):
        raise ValueError("taxonomy plan contains duplicate symlink destinations")
    for source, target in planned:
        if target.is_symlink() and target.resolve(strict=False) == source:
            continue
        if target.exists() or target.is_symlink():
            raise FileExistsError(f"taxonomy output collision: {target}")


def apply_symlinks(
    records: list[dict],
    org_dir: Path,
    taxonomy: dict[str, dict[str, list[str]]],
) -> int:
    """Apply a prevalidated symlink batch, rolling back every link on failure."""
    org_dir = org_dir.resolve()
    planned = _plan_symlinks(records, org_dir, taxonomy)
    _validate_symlink_plan(planned)

    created: list[Path] = []
    try:
        for source, target in planned:
            if make_symlink(str(source), target):
                created.append(target)
        manifest = {
            "schema_version": TAXONOMY_LINK_MANIFEST_SCHEMA_VERSION,
            "links": [
                {"path": str(path.relative_to(org_dir)), "source": str(source)}
                for source, path in planned
            ],
        }
        write_text_atomic(
            org_dir / "SESSION_TAXONOMY_LINKS.json",
            json.dumps(manifest, indent=2, ensure_ascii=False),
        )
    except BaseException:
        for path in reversed(created):
            path.unlink(missing_ok=True)
        raise
    return len(created)


def write_taxonomy_json(
    taxonomy: dict[str, dict[str, list[str]]],
    records: list[dict],
    org_dir: Path,
) -> None:
    """Write SESSION_TAXONOMY.json: {name: {taxonomy, utility, era}} for all sessions."""
    name_to_rec = {_record_key(r): r for r in records}
    output = {
        name: {
            "taxonomy": dims,
            "utility": name_to_rec.get(name, {}).get("utility", 0),
            "era": name_to_rec.get(name, {}).get("era", "unknown"),
        }
        for name, dims in taxonomy.items()
    }
    path = org_dir / "SESSION_TAXONOMY.json"
    write_text_atomic(path, json.dumps(output, indent=2, ensure_ascii=False))
    print(f"SESSION_TAXONOMY.json: {len(taxonomy)} sessions")


def write_taxonomy_markdown(
    taxonomy: dict[str, dict[str, list[str]]],
    records: list[dict],
    org_dir: Path,
    dimensions: list[dict] | None = None,
) -> None:
    """Write TAXONOMY.md: sessions grouped by taxonomy dimension and category."""
    name_to_rec = {_record_key(r): r for r in records}
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
                label = name_to_rec.get(name, {}).get("name", name)
                lines.append(f"| {label} | {util} | {era} |\n")
            lines.append("\n")

    path = org_dir / "TAXONOMY.md"
    write_text_atomic(path, "".join(lines))
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
        primary_paths = session_paths.get(_record_key(rec), [])
        link_target = _preferred_link_path(primary_paths, dims)
        if not link_target and primary_paths:
            link_target = primary_paths[0]
        if not link_target:
            link_target = "01_by_project/misc_research"
        encoded = _path_component(_record_key(rec), org_dir)
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

    write_text_atomic(org_dir / "INDEX.md", "".join(lines))

    # SESSIONS_FULL.md: all ranked sessions, no truncation
    full_lines = [
        "# All Sessions: Complete Ranked List\n\n",
        f"{len(all_ranked)} sessions with utility >= {min_utility}, ranked by score.\n\n",
        "| Rank | Utility | Session | Technique | Role | Era |\n",
        "| :--- | :--- | :--- | :--- | :--- | :--- |\n",
    ]
    for i, rec in enumerate(all_ranked, 1):
        name = rec["name"]
        paths = session_paths.get(_record_key(rec), [])
        lp = _preferred_link_path(paths, dims)
        if not lp and paths:
            lp = paths[0]
        if not lp:
            lp = "01_by_project/misc_research"
        enc = _path_component(_record_key(rec), org_dir)
        full_lines.append(
            f"| {i} | {rec.get('utility', 0)} | [{name}]({lp}/{enc}) "
            f"| {(rec.get('techniques') or ['—'])[0]} | {(rec.get('roles') or ['—'])[0]} "
            f"| {rec.get('era', '—')} |\n"
        )

    write_text_atomic(org_dir / "SESSIONS_FULL.md", "".join(full_lines))

    print(f"INDEX.md: {len(all_ranked)} entries; SESSIONS_FULL.md: {len(all_ranked)} total")


def write_knowledge_graph(records: list[dict], org_dir: Path) -> None:
    """Render the canonical Rust graph without re-inferring lineage in Python."""
    graph = json.loads((org_dir / "SESSION_GRAPH.json").read_text(encoding="utf-8"))
    name_to_rec = {_record_key(r): r for r in records}
    children: defaultdict[str, list[str]] = defaultdict(list)
    for edge in graph.get("edges", []):
        parent = edge["source_session_id"]
        child = edge["target_session_id"]
        children[parent].append(child)

    def mermaid_node(session_id: str) -> tuple[str, str]:
        node_id = f"n_{sha256(session_id.encode('utf-8')).hexdigest()}"
        label = str(name_to_rec.get(session_id, {}).get("name", session_id))
        return node_id, label.replace('"', "'").replace("(", "[").replace(")", "]")

    lines = [
        "# Knowledge Graph: Session Lineage\n\n",
        "Maps provenance relationships: ROOT -> VERSION chains and BRANCH derivations.\n",
        "Source: https://journals.sagepub.com/doi/full/10.1177/00016993211051521\n\n",
        "## Explicit Resolved Lineage\n\n",
        "```mermaid\ngraph TD\n",
    ]
    linked: set[str] = set()
    for parent in sorted(children):
        parent_id, parent_label = mermaid_node(parent)
        for child in sorted(set(children[parent])):
            child_id, child_label = mermaid_node(child)
            linked.update((parent, child))
            lines.append(
                f'    {parent_id}["{parent_label}"] --> {child_id}["{child_label}"]\n'
            )
    lines.append("```\n\n")
    standalone = set(name_to_rec) - linked
    lines.append(
        f"## Standalone Sessions\n\n{len(standalone)} sessions with no resolved lineage.\n\n"
    )

    write_text_atomic(org_dir / "KNOWLEDGE_GRAPH.md", "".join(lines))
    print("KNOWLEDGE_GRAPH.md written")


def _resolve_formats(cfg: dict, formats: list[str] | None) -> list[str]:
    """Resolve output formats: parameter > config > safe JSON/Markdown defaults.

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
            resolved = ["json", "markdown"]

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
