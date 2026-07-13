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
    ClassificationRule,
    DateRangeQuery,
    FileQueryRequest,
    MessageQuery,
    MessageSearchTarget,
    MessageSelector,
    MessageSequenceRange,
    PhraseVocabulary,
    QueryExclusions,
    QueryScope,
    RelationshipRule,
    ResolvedDateRange,
    SessionQuery,
    SessionSearch,
)

__version__ = version("ai-session-search")

__author__ = "Andrew Hundt"

__all__ = [  # noqa: RUF022 - keep the canonical SessionSearch entry point first
    "SessionSearch",
    "AnalysisPublicationPlan",
    "SessionQuery",
    "MessageQuery",
    "AnalysisQuery",
    "AnalysisPolicy",
    "ClassificationRule",
    "RelationshipRule",
    "PhraseVocabulary",
    "FileQueryRequest",
    "QueryExclusions",
    "QueryScope",
    "ResolvedDateRange",
    "DateRangeQuery",
    "MessageSelector",
    "MessageSequenceRange",
    "MessageSearchTarget",
]
