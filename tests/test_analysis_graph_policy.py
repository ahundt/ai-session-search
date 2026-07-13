from pathlib import Path

import pytest

from ai_session_search.analysis.rust_policy import build_analysis_policy


@pytest.mark.parametrize(
    ("rule", "message"),
    [
        (
            {"id": "related", "kind": "similar", "pattern": r"(?P<parent>.+)"},
            "kind must be branch, copy, or version",
        ),
        (
            {"id": "branch", "kind": "branch"},
            "missing required field 'pattern'",
        ),
    ],
)
def test_relationship_configuration_fails_before_analysis(
    tmp_path: Path,
    rule: dict[str, str],
    message: str,
) -> None:
    with pytest.raises(ValueError, match=message):
        build_analysis_policy(
            {"analysis_relationship_rules": [rule]},
            tmp_path,
            max_classification_chars=100,
            include_classifications=False,
        )
