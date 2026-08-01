# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

from email.message import Message

import pytest

from scripts.python_license_inventory import LicenseInventoryError, resolve_license, validate_expression


def metadata(**headers: str) -> Message:
    result = Message()
    for key, value in headers.items():
        result[key.replace("_", "-")] = value
    return result


def test_prefers_pep_639_expression() -> None:
    assert resolve_license(metadata(License_Expression="MPL-2.0 AND (Apache-2.0 OR MIT)")) == (
        "MPL-2.0 AND (Apache-2.0 OR MIT)",
        "License-Expression",
    )


def test_resolves_legacy_classifier_without_package_specific_rules() -> None:
    item = metadata()
    item["Classifier"] = "License :: OSI Approved :: MIT License"
    assert resolve_license(item) == ("MIT", "Classifier")


def test_rejects_missing_or_incompatible_license() -> None:
    with pytest.raises(LicenseInventoryError, match="no recognized license"):
        resolve_license(metadata())
    with pytest.raises(LicenseInventoryError, match="unapproved identifiers"):
        validate_expression("GPL-3.0-only")
