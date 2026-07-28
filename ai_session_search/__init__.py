"""Rust-indexed Python API for searching AI coding-agent sessions.

``SessionSearch`` owns the Rust service and SQLite connection used by the Rust
library, CLI, and MCP server. Keep one instance for a related unit of work and
compose immutable query objects for each operation.
"""

from importlib.metadata import version

from .native import (
    AnalysisPolicy,
    AnalysisPublicationPlan,
    AnalysisQuery,
    CapabilityReceipt,
    ClassificationRule,
    DateRange,
    FileQuery,
    MessageClassificationMatch,
    MessageClassificationQuery,
    MessageClassificationReport,
    MessageExclusions,
    MessageScope,
    MessageSearchBatch,
    MessageSearchBatches,
    MessageSearchCompletion,
    MessageSearchRequest,
    MessageSearchResponse,
    MessageSearchRuntimeDiagnostics,
    PhraseVocabulary,
    QueryExclusions,
    QueryScope,
    RelationshipRule,
    ResolvedDateRange,
    SessionQuery,
    SessionSearch,
    SkillRunQuery,
    SkillSelector,
)

__version__ = version("ai-session-search")

__author__ = "Andrew Hundt"

__all__ = [  # noqa: RUF022 - keep the canonical SessionSearch entry point first
    "SessionSearch",
    "AnalysisPublicationPlan",
    "SessionQuery",
    "MessageSearchRequest",
    "MessageSearchResponse",
    "MessageSearchBatch",
    "MessageSearchBatches",
    "MessageSearchCompletion",
    "MessageSearchRuntimeDiagnostics",
    "AnalysisQuery",
    "SkillSelector",
    "MessageClassificationQuery",
    "SkillRunQuery",
    "MessageClassificationMatch",
    "CapabilityReceipt",
    "MessageClassificationReport",
    "AnalysisPolicy",
    "ClassificationRule",
    "RelationshipRule",
    "PhraseVocabulary",
    "FileQuery",
    "MessageExclusions",
    "MessageScope",
    "QueryExclusions",
    "QueryScope",
    "ResolvedDateRange",
    "DateRange",
]
