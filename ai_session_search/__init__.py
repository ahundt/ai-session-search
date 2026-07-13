"""Rust-indexed Python API for searching AI coding-agent sessions.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0

Quickstart (recommended):
    import ai_session_search as aise
    search = aise.SessionSearch()
    search.refresh()
    sessions = search.list_sessions(aise.SessionQuery(provider="codex", limit=20))
    messages = search.search_messages(
        "authentication",
        aise.MessageQuery(scope=aise.QueryScope(provider="codex"), limit=50),
    )

``SessionSearch`` owns one Rust application service and SQLite connection. Python
reference lifetime releases both automatically; keep one instance for a related
unit of work rather than reopening it per operation.

Legacy Python source scanners remain importable during CLI migration, but new
integrations should use ``SessionSearch`` so CLI, MCP, Rust, and Python share the
same provider adapters, index, filters, and lifecycle policy.
"""

try:
    from importlib.metadata import version
    __version__ = version("ai-session-search")
except Exception:
    __version__ = "1.0.0"

__author__ = "Andrew Hundt"

# Transitional Python-scanner and configuration compatibility exports.
from .config import (
    get_config_path,
    get_config_section,
    load_config,
    write_config,
)
from .engine import (
    AISession,
    SessionRecoveryEngine,  # Claude Code only, explicit paths (advanced)
    connect,  # Convenience alias for AISession() (connect = AISession)
    parse_date_input,
)

# Filters
from .filters import Filter, MessageFilter, SearchFilter

# Formatters
from .formatters import (
    CsvFormatter,
    JsonFormatter,
    MessageFormatter,
    PlainFormatter,
    ResultFormatter,
    TableFormatter,
    get_formatter,
)

# Models
from .models import (
    ContextMatch,
    CorrectionMatch,
    FileVersion,
    FilterSpec,
    MessageType,
    PlanningCommandCount,  # backward-compat alias for SlashCommandCount
    SessionAnalysis,
    SessionFile,
    SessionInfo,
    SessionMessage,
    SessionMetadata,
    SessionStatistics,
    SlashCommandCount,
    SlashCommandRecord,
)
from .native import (
    AnalysisQuery,
    DateRangeQuery,
    FileQueryRequest,
    MessageQuery,
    MessageSearchTarget,
    MessageSelector,
    MessageSequenceRange,
    QueryScope,
    SessionQuery,
    SessionSearch,
)

# Source backends (also importable directly from ai_session_search.sources)
from .sources import AiStudioSource, GeminiCliSource

__all__ = [
    # --- Canonical Rust-backed API ---
    "SessionSearch",
    "SessionQuery",
    "MessageQuery",
    "AnalysisQuery",
    "FileQueryRequest",
    "QueryScope",
    "DateRangeQuery",
    "MessageSelector",
    "MessageSequenceRange",
    "MessageSearchTarget",
    # --- Transitional Python-scanner API ---
    "AISession",
    "connect",
    "SessionRecoveryEngine",  # Claude Code only, explicit paths (advanced)
    "parse_date_input",       # Date/EDTF parsing utility
    # --- Source backends ---
    "AiStudioSource",
    "GeminiCliSource",
    # --- Filters ---
    "FilterSpec",             # Declarative filter (.with_since, .with_until, .with_extensions, ...)
    "Filter",                 # Generic base class for SearchFilter and MessageFilter; subclass for custom filters
    "SearchFilter",           # Imperative file filter with by_location_pattern(), by_date(), &/| operators
    "MessageFilter",          # Message filter with &/| composability
    # --- Formatters ---
    "get_formatter",          # Factory: "table" | "json" | "csv" | "plain" | "message"
    "TableFormatter",
    "JsonFormatter",
    "CsvFormatter",
    "PlainFormatter",
    "MessageFormatter",
    "ResultFormatter",
    # --- Data models ---
    "SessionFile",
    "SessionStatistics",
    "SessionInfo",
    "SessionMessage",
    "SessionMetadata",
    "SessionAnalysis",
    "FileVersion",
    "CorrectionMatch",
    "SlashCommandCount",
    "PlanningCommandCount",  # backward-compat alias for SlashCommandCount
    "SlashCommandRecord",
    "ContextMatch",
    "MessageType",
    # --- Config ---
    "load_config",
    "get_config_path",
    "write_config",
    "get_config_section",
]
# Protocol types (Searchable, Extractable, Filterable, Storage, Predicate, Composable)
# are importable from ai_session_search.types for custom implementors.
