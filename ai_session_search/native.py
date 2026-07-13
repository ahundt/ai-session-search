"""Typed access to the Rust AI Session Search service.

This module is available in wheels built with maturin. Keeping the extension in
``_native`` prevents binding details from becoming the public API namespace.
"""

from ._native import (
    FileQueryRequest,
    MessageQuery,
    NativeExportDocument,
    NativeFileCrossRef,
    NativeFileEditSummary,
    NativeFileVersion,
    NativeMessageHit,
    NativeProviderSourceStatus,
    NativeReconstructedFile,
    NativeSessionRecord,
    NativeSessionSearchHit,
    RefreshOutcome,
    SessionQuery,
    SessionSearch,
)

__all__ = [
    "FileQueryRequest",
    "MessageQuery",
    "NativeExportDocument",
    "NativeFileCrossRef",
    "NativeFileEditSummary",
    "NativeFileVersion",
    "NativeMessageHit",
    "NativeProviderSourceStatus",
    "NativeReconstructedFile",
    "NativeSessionRecord",
    "NativeSessionSearchHit",
    "RefreshOutcome",
    "SessionQuery",
    "SessionSearch",
]
