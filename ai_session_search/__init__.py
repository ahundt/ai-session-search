"""
AI Session Tools - Python library + CLI for Claude Code, AI Studio, and Gemini CLI sessions.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0

Quickstart (RECOMMENDED - zero-config RAII, auto-detects all sources):
    import ai_session_search as aise
    from ai_session_search import AISession, FilterSpec

    # All equivalent - AISession() auto-detects Claude, AI Studio, Gemini CLI:
    session = aise.AISession()               # direct RAII (class name = concept)
    session = aise.connect()                 # convenience alias (connect = AISession)

    with aise.AISession() as session:        # context manager (recommended)
        sessions = session.get_sessions(since="7d")
        messages = session.search_messages("authentication")
        files    = session.search_files("*.py")

        # Fluent FilterSpec builder:
        results = session.search_files(
            "*.py",
            FilterSpec().with_since("7d").with_extensions(include={"py"})
        )

Quickstart (explicit source override):
    from ai_session_search import AISession, FilterSpec

    with AISession(source="claude", claude_dir="~/.claude") as session:
        sessions = session.get_sessions(since="7d")

Quickstart (Claude Code only, explicit paths - advanced):
    from ai_session_search import SessionRecoveryEngine, FilterSpec

    engine = SessionRecoveryEngine(projects_dir, recovery_dir)
    results = engine.search("*.py", FilterSpec(min_edits=5))

Configuration:
    from ai_session_search import load_config, write_config

    cfg = load_config()
    cfg.setdefault("source_dirs", {})["aistudio"] = ["/path/to/ai-studio"]
    write_config(cfg)

Note: Protocol types (Searchable, Extractable, Filterable, Storage, Predicate,
Composable) are importable from ai_session_search.types for custom implementors.

Claude Code integration (autorun plugin):
    ``aise`` is available as a Claude Code slash command via the autorun plugin
    (https://github.com/ahundt/autorun).  autorun adds slash commands, hooks, and
    autonomous task workflows to Claude Code.  After installing autorun, use
    ``/ar:claude-session-tools`` inside a Claude Code conversation to search and
    recover session history without leaving the editor. Especially useful after
    context compaction to restore previous context inline.
"""

try:
    from importlib.metadata import version
    __version__ = version("ai-session-search")
except Exception:
    __version__ = "1.0.0"

__author__ = "Andrew Hundt"

# Primary entry point - AISession is the main class
from .engine import (
    AISession,          # RECOMMENDED: auto-detects all sources; zero-config RAII
    connect,            # Convenience alias for AISession() (connect = AISession)
    SessionRecoveryEngine,  # Claude Code only, explicit paths (advanced)
    parse_date_input,
)

# Source backends (also importable directly from ai_session_search.sources)
from .sources import AiStudioSource, GeminiCliSource

# Filters
from .filters import Filter, MessageFilter, SearchFilter

# FilterSpec in models
from .models import FilterSpec

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
    MessageType,
    SlashCommandCount,
    PlanningCommandCount,  # backward-compat alias for SlashCommandCount
    SlashCommandRecord,
    SessionAnalysis,
    SessionFile,
    SessionInfo,
    SessionMessage,
    SessionMetadata,
    SessionStatistics,
)

# Config API
from .config import (
    load_config,
    get_config_path,
    write_config,
    get_config_section,
)

__all__ = [
    # --- Primary entry point - THE main class (zero-config RAII) ---
    "AISession",              # RECOMMENDED: auto-detects all sources; connect = AISession
    "connect",                # Convenience alias for AISession() (connect = AISession)
    # --- Advanced entry points ---
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
