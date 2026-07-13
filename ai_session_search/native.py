"""Typed access to the Rust AI Session Search service.

This module is available in wheels built with maturin. Keeping the extension in
``_native`` prevents binding details from becoming the public API namespace.
"""

from ._native import (
    AnalysisQuery,
    DateRangeQuery,
    FileQueryRequest,
    MessageQuery,
    NativeCorrectionMatch,
    NativeExportDocument,
    NativeFileCrossRef,
    NativeFileEditSummary,
    NativeFileVersion,
    NativeMessageHit,
    NativePlanningCount,
    NativeProviderSourceStatus,
    NativeReconstructedFile,
    NativeRoleStatistic,
    NativeSessionRecord,
    NativeSessionSearchHit,
    RefreshOutcome,
    SessionQuery,
    SessionSearch,
)

__all__ = [
    "AnalysisQuery",
    "DateRangeQuery",
    "FileQueryRequest",
    "MessageQuery",
    "NativeCorrectionMatch",
    "NativeExportDocument",
    "NativeFileCrossRef",
    "NativeFileEditSummary",
    "NativeFileVersion",
    "NativeMessageHit",
    "NativePlanningCount",
    "NativeProviderSourceStatus",
    "NativeReconstructedFile",
    "NativeRoleStatistic",
    "NativeSessionRecord",
    "NativeSessionSearchHit",
    "RefreshOutcome",
    "SessionQuery",
    "SessionSearch",
]
