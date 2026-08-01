#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Render planner-derived message-search contract blocks in public documentation."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
SURFACES = ("rust", "cli", "mcp", "python")
START = "<!-- aise-message-search-contract:start -->"
END = "<!-- aise-message-search-contract:end -->"
TARGETS = (ROOT / "README.md", ROOT / "docs/README.md", ROOT / "docs/development/configuration.md")


def load_specs(spec_directory: Path | None, executable: Path | None) -> dict[str, dict[str, Any]]:
    if spec_directory is not None:
        return {surface: json.loads((spec_directory / f"{surface}.json").read_text()) for surface in SURFACES}

    with tempfile.TemporaryDirectory(prefix="aise-doc-spec-") as temp:
        config = Path(temp) / "config.toml"
        config.write_text("")
        specs: dict[str, dict[str, Any]] = {}
        for surface in SURFACES:
            if executable is None:
                command = [
                    "cargo",
                    "run",
                    "--quiet",
                    "-p",
                    "ai-session-search",
                    "--bin",
                    "aise",
                    "--",
                ]
            else:
                command = [str(executable)]
            command.extend(
                [
                    "--config",
                    str(config),
                    "messages",
                    "search",
                    "--describe",
                    "--describe-surface",
                    surface,
                ]
            )
            environment = os.environ.copy()
            environment.setdefault("RUSTC_WRAPPER", "/usr/bin/env")
            completed = subprocess.run(command, cwd=ROOT, env=environment, check=True, capture_output=True, text=True)
            specs[surface] = json.loads(completed.stdout)
        return specs


def validate_specs(specs: dict[str, dict[str, Any]]) -> dict[str, Any]:
    registry = specs["cli"]["registry"]
    for surface in SURFACES:
        if specs[surface]["registry"] != registry:
            raise ValueError(f"{surface}: parameter registry differs from cli")
    return registry


def compact_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def extent_label(default: dict[str, Any]) -> str:
    extent = default["extent"]
    if extent["kind"] == "all_results":
        return f"all results; offset {extent['offset']}"
    return f"page of {extent['limit']}; offset {extent['offset']}"


def context_label(default: dict[str, Any]) -> str:
    context = default["context"]
    return f"{context['messages_before']} before / {context['messages_after']} after"


def defaults_table(specs: dict[str, dict[str, Any]]) -> list[str]:
    lines = [
        "| Caller surface | Omitted non-fuzzy result extent | Context | Lines per message | Field view | Match view | Includes / receipt |",
        "| --- | --- | --- | ---: | --- | --- | --- |",
    ]
    for surface in SURFACES:
        default = specs[surface]["configured_default"]
        presentation = default["presentation"]
        lines.append(
            f"| {surface} | {extent_label(default)} | {context_label(default)} | {presentation['lines_per_message']} | "
            f"`{compact_json(presentation['field_view'])}` | `{compact_json(presentation['match_view'])}` | "
            f"`{compact_json(default['include'])}` / `{default['receipt_level']}` |"
        )
    return lines


def vocabulary_table(registry: dict[str, Any]) -> list[str]:
    lines = [
        "| Canonical parameter | Accepted values | Omission semantics |",
        "| --- | --- | --- |",
    ]
    for parameter in registry["parameters"]:
        domain = parameter["domain"]
        accepted = domain.get("accepted_values")
        if accepted is None and "accepted_variants" in domain:
            accepted = []
            for variant in domain["accepted_variants"]:
                fields = ", ".join(
                    f"{field['name']}: {field['domain']['kind']}"
                    for field in variant["fields"]
                )
                accepted.append(
                    f"{variant['value']} {{{fields}}}" if fields else variant["value"]
                )
        if accepted is None:
            continue
        lines.append(f"| `{parameter['parameter']}` | {', '.join(f'`{value}`' for value in accepted)} | `{parameter['omission']}` |")
    return lines


def precedence_line(registry: dict[str, Any]) -> str:
    return " → ".join(f"`{source}`" for source in registry["precedence"])


def rule_lines(registry: dict[str, Any]) -> list[str]:
    return [f"- `{descriptor['rule']}` — {descriptor['message']}." for descriptor in registry["rules"]]


def readme_block(specs: dict[str, dict[str, Any]], registry: dict[str, Any]) -> str:
    return "\n".join(
        [
            "### Generated message-search defaults",
            "",
            registry["purpose"],
            "",
            *defaults_table(specs),
            "",
            "These are shipped defaults from an empty configuration. `aise messages search --describe --describe-surface "
            "cli|mcp|python|rust` resolves the same contract with the active configuration. Positive `limit` counts result rows; "
            "signed `lines_per_message` selects the beginning, end, or complete text of each already-selected message.",
        ]
    )


def docs_block(specs: dict[str, dict[str, Any]], registry: dict[str, Any]) -> str:
    return "\n".join(
        [
            "## Generated message-search caller contract",
            "",
            registry["purpose"],
            "",
            "### Shipped defaults by caller surface",
            "",
            *defaults_table(specs),
            "",
            "### Closed vocabularies",
            "",
            *vocabulary_table(registry),
            "",
            "### Executable conflict rules",
            "",
            *rule_lines(registry),
            "",
            f"Default precedence, highest first: {precedence_line(registry)}.",
            "",
            "The full configured catalogue is available from `aise messages search --describe`; MCP clients receive the same canonical "
            "parameter identities and planner-resolved MCP defaults in `tools/list`. Ordinary search responses contain only the compact "
            "effective request needed to interpret that response.",
        ]
    )


def configuration_block(specs: dict[str, dict[str, Any]], registry: dict[str, Any]) -> str:
    return "\n".join(
        [
            "### Generated message-search policy resolution",
            "",
            f"Resolution precedence, highest first: {precedence_line(registry)}.",
            "",
            *defaults_table(specs),
            "",
            "The table uses an empty configuration and is a shipped-default reference, not a claim about a user's effective settings. "
            "Run `aise messages search --describe --describe-surface SURFACE` to inspect active values without opening or refreshing the index.",
        ]
    )


def replace_region(document: str, body: str, path: Path) -> str:
    if document.count(START) != 1 or document.count(END) != 1:
        raise ValueError(f"{path}: expected exactly one {START!r} and one {END!r}")
    before, remainder = document.split(START, 1)
    _, after = remainder.split(END, 1)
    return f"{before}{START}\n{body.rstrip()}\n{END}{after}"


def render_targets(specs: dict[str, dict[str, Any]], documents: dict[Path, str]) -> dict[Path, str]:
    registry = validate_specs(specs)
    bodies = {
        ROOT / "README.md": readme_block(specs, registry),
        ROOT / "docs/README.md": docs_block(specs, registry),
        ROOT / "docs/development/configuration.md": configuration_block(specs, registry),
    }
    return {path: replace_region(documents[path], bodies[path], path) for path in TARGETS}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--check", action="store_true", help="fail when a marked block differs; never write")
    action.add_argument("--write", action="store_true", help="replace only the three marked blocks")
    parser.add_argument("--aise", type=Path, help="use this built aise executable instead of cargo run")
    parser.add_argument("--spec-directory", type=Path, help="read rust.json, cli.json, mcp.json, and python.json instead of running aise")
    args = parser.parse_args()

    specs = load_specs(args.spec_directory, args.aise)
    documents = {path: path.read_text() for path in TARGETS}
    rendered = render_targets(specs, documents)
    stale = [path for path in TARGETS if rendered[path] != documents[path]]
    if args.check:
        if stale:
            for path in stale:
                print(f"{path.relative_to(ROOT)}: generated message-search contract is stale")
            print("Run: uv run --no-project python scripts/render_message_search_docs.py --write")
            return 1
        return 0
    for path in stale:
        path.write_text(rendered[path])
        print(f"wrote {path.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
