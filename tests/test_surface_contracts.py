# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

from sqlite_helpers import write_database

import ai_session_search as aise
from scripts.verify_harness_discovery import environment



def test_result_dictionaries_preserve_shared_serialization(tmp_path: Path) -> None:
    database = tmp_path / "index.db"
    with aise.SessionSearch(database) as search:
        with write_database(database) as connection:
            connection.executescript("""
                insert into sessions(id,provider,provider_session_id,preview_text,source_path,
                                     parse_version,discovery_source,raw_metadata_json)
                values ('claude:dictionary','claude','dictionary','','/dictionary','test','test','{"synthetic":true}');
            """)
        record = search.list_sessions()[0].to_dict()
        assert record["raw_metadata_json"] == '{"synthetic":true}'
        env = environment(tmp_path / "runtime")
        env["AI_SESSION_SEARCH_DATABASE"] = str(database)
        result = subprocess.run(
            [sys.executable, "-c", "from ai_session_search.entrypoint import cli_main; cli_main()",
             "--index-refresh", "existing-only", "skills", "corrections", "--format", "json"],
            env=env, capture_output=True, text=True, check=True,
        )
        report = search.run_skill(aise.SkillRunQuery(skill=aise.SkillSelector(name="corrections"), input=aise.MessageClassificationQuery()))
        assert report.to_dict() == json.loads(result.stdout)


def test_rust_result_fields_reach_native_wrappers() -> None:
    """A Rust result field addition requires an explicit Python boundary decision."""
    root = Path(__file__).resolve().parents[1]
    core = root / "rust/ai-session-search-core/src"
    binding = (root / "rust/ai-session-search-python/src/lib.rs").read_text()
    groups = {
        "models.rs": ["SessionRecord", "AnalysisDocument", "AnalysisDocumentPage",
                      "MessageHit", "FileEditSummary", "FileVersion", "FileCrossRef",
                      "SessionTimeProfile", "MessageClassificationMatch"],
        "corrections.rs": ["MessageClassificationReport"],
        "analysis_pipeline.rs": ["ClassificationMatch", "AnalyzedSession", "PhraseFrequency",
                                 "SessionGraphNode", "SessionGraphEdge", "SessionGraphGroup", "SessionGraph"],
        "inspect.rs": ["MessagePreview", "ToolActivity", "RefEvidence", "ChangedFileEvidence", "SessionInspection"],
        "analysis_publication.rs": ["AnalysisArtifact", "PublishedAnalysisArtifact", "AnalysisPublicationReceipt"],
        "skill_run.rs": ["ResolvedSkillReceipt", "MessageClassificationResult", "SkillRunReport"],
    }
    for filename, names in groups.items():
        source = (core / filename).read_text()
        for name in names:
            rust = re.search(rf"pub struct {name} \{{(.*?)\n\}}", source, re.S)
            native = re.search(rf"struct Native{name} \{{(.*?)\n\}}", binding, re.S)
            assert rust and native, name
            fields = set(re.findall(r"^    pub (\w+):", rust[1], re.M))
            fields -= {"follow_ups"}  # serde(skip); shell next_commands is the serialized contract.
            fields = {"timestamp" if field == "ts" else field for field in fields}
            python_fields = set(re.findall(r"^    (\w+):", native[1], re.M))
            assert fields <= python_fields, (name, fields - python_fields)
