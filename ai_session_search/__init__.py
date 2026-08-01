# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

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
    AnalysisReceipt,
    AnalysisRequest,
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
    ReceiptedAnalysis,
    RelationshipRule,
    ResolvedDateRange,
    SessionQuery,
    SessionSearch,
    SkillRunQuery,
    SkillSelector,
)
from .types import (
    FieldView,
    FieldViewMaxChars,
    FieldViewNoCharLimit,
    MatchView,
    MatchViewMaxChars,
    MatchViewMinimalSpan,
    MessageClassificationCategory,
    MessageClassificationDefinition,
)

__version__ = version("ai-session-search")

__author__ = "Andrew Hundt"

__all__ = [  # noqa: RUF022 - keep the canonical SessionSearch entry point first
    "SessionSearch",
    "AnalysisPublicationPlan",
    "AnalysisRequest",
    "AnalysisReceipt",
    "ReceiptedAnalysis",
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
    "MessageClassificationCategory",
    "MessageClassificationDefinition",
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
    "FieldView",
    "FieldViewMaxChars",
    "FieldViewNoCharLimit",
    "MatchView",
    "MatchViewMaxChars",
    "MatchViewMinimalSpan",
]
