#!/usr/bin/env python3
"""Generate and validate a Python distribution license inventory."""

from __future__ import annotations

import argparse
import importlib.metadata
import pathlib
import re
from collections.abc import Iterable, Mapping

ALLOWED_LICENSES = {
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "MIT",
    "MPL-2.0",
    "PSF-2.0",
}

CLASSIFIER_LICENSES = {
    "License :: OSI Approved :: Apache Software License": "Apache-2.0",
    "License :: OSI Approved :: ISC License (ISCL)": "ISC",
    "License :: OSI Approved :: MIT License": "MIT",
    "License :: OSI Approved :: Mozilla Public License 2.0 (MPL 2.0)": "MPL-2.0",
    "License :: OSI Approved :: Python Software Foundation License": "PSF-2.0",
}

LICENSE_ALIASES = {
    "Apache 2.0": "Apache-2.0",
    "Apache-2.0": "Apache-2.0",
    "ISC License": "ISC",
    "MIT": "MIT",
    "MIT License": "MIT",
    "MPL-2.0": "MPL-2.0",
}

SPDX_IDENTIFIER = re.compile(r"[A-Za-z][A-Za-z0-9.-]*")
SPDX_OPERATORS = {"AND", "OR", "WITH"}


class LicenseInventoryError(ValueError):
    """A dependency has missing or unapproved license evidence."""


def resolve_license(metadata: Mapping[str, str]) -> tuple[str, str]:
    expression = metadata.get("License-Expression")
    if expression:
        return expression.strip(), "License-Expression"

    classifiers = metadata.get_all("Classifier", []) if hasattr(metadata, "get_all") else []
    resolved = sorted({CLASSIFIER_LICENSES[item] for item in classifiers if item in CLASSIFIER_LICENSES})
    if resolved:
        return " OR ".join(resolved), "Classifier"

    license_value = metadata.get("License")
    if license_value in LICENSE_ALIASES:
        return LICENSE_ALIASES[license_value], "License"
    if license_value and license_value.lstrip().startswith("The MIT License"):
        return "MIT", "License text"
    raise LicenseInventoryError("no recognized license expression, classifier, or license value")


def validate_expression(expression: str) -> None:
    identifiers = {token for token in SPDX_IDENTIFIER.findall(expression) if token not in SPDX_OPERATORS}
    unsupported = sorted(identifiers - ALLOWED_LICENSES)
    if unsupported:
        raise LicenseInventoryError(
            f"license expression {expression!r} contains unapproved identifiers: {', '.join(unsupported)}"
        )


def inventory(distributions: Iterable[importlib.metadata.Distribution]) -> list[tuple[str, str, str, str]]:
    rows = []
    for distribution in distributions:
        name = distribution.metadata.get("Name")
        if not name:
            raise LicenseInventoryError("installed distribution has no Name metadata")
        try:
            expression, evidence = resolve_license(distribution.metadata)
            validate_expression(expression)
        except LicenseInventoryError as error:
            raise LicenseInventoryError(f"{name} {distribution.version}: {error}") from error
        rows.append((name, distribution.version, expression, evidence))
    return sorted(rows, key=lambda row: (row[0].lower(), row[1]))


def render(rows: Iterable[tuple[str, str, str, str]]) -> str:
    lines = [
        "# Python runtime dependency licenses",
        "",
        "AI Session Search is Apache-2.0. The dependencies below retain their own compatible licenses.",
        "This inventory is generated from a clean locked runtime environment.",
        "",
        "| Distribution | Version | License expression | Evidence |",
        "|---|---:|---|---|",
    ]
    lines.extend(f"| {name} | {version} | `{expression}` | {evidence} |" for name, version, expression, evidence in rows)
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()
    args.output.write_text(render(inventory(importlib.metadata.distributions())), encoding="utf-8")
    print(f"wrote: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
