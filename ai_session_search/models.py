"""
Data models for session recovery - using modern Python patterns.

Includes dataclasses, enums, and structured types for type safety.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""

from __future__ import annotations

import re as _re
from dataclasses import dataclass, field
from enum import Enum
from typing import Dict, Iterable, List, Optional, Set


def _decode_project_dir(encoded_dir: str) -> str:
    """Decode a Claude-encoded project directory name to a human-readable project name.

    Claude encodes project paths by replacing every path separator with '-'.
    Examples:
        "-Users-alice-myproject"     → "myproject"
        "-home-bob-work-client"      → "client"
        "-Users-alice--hidden-proj"  → "-hidden-proj"

    This is the canonical implementation shared by ``SessionInfo.project_display``
    and ``SessionRecoveryEngine.extract_project_name()``.  Both delegate here to
    avoid duplicating the regex logic and to prevent divergence between the two
    call sites.

    Args:
        encoded_dir: Encoded project directory name (e.g. "-Users-alice-project").

    Returns:
        Human-readable project name (e.g. "project").
    """
    stripped = _re.sub(r'^-(Users|home)-[^-]+-', '', encoded_dir)
    if not stripped:
        return encoded_dir
    if stripped.startswith('-'):
        # A leading '-' after the home-prefix strip can mean two things:
        # (a) Encoded dot-file as a single path component: "-hidden" (represents ".hidden")
        #     → preserve the leading dash; return as-is.
        # (b) Path that did NOT match the home-prefix regex (e.g. "-work-project"):
        #     → extract the rightmost dash-separated segment.
        # Distinguish: if stripped[1:] contains no further '-', it is case (a).
        if '-' not in stripped[1:]:
            return stripped
        return stripped.rsplit('-', 1)[-1] or stripped
    # Return the last '-'-separated component (rightmost path segment).
    return stripped.rsplit('-', 1)[-1] or stripped


def _resolve_date(date_str: str, mode: str = "start") -> str:
    """Resolve a date string (ISO or duration shorthand) to a comparable ISO datetime.

    Duration shorthands like "7d", "2w", "1m" must be resolved to ISO 8601
    strings before lexicographic comparison in ``FilterSpec.matches_datetime()``.
    Storing "7d" raw causes ``"2026-01-01" < "7d"`` to always be True (ASCII "2" < "7"),
    silently excluding every timestamp.

    Uses ``parse_date_input`` from engine.py via a lazy import to avoid the
    circular dependency (engine imports models).  Falls back to storing the
    string as-is when the import fails (import cycle or test environment).

    Args:
        date_str: ISO date/datetime, duration shorthand ("7d", "2w"), or EDTF expression.
        mode:     "start" for lower bound (00:00:00), "end" for upper bound (23:59:59).

    Returns:
        Resolved ISO 8601 datetime string, or ``date_str`` unchanged if resolution fails.
    """
    try:
        from .engine import parse_date_input as _parse  # lazy: engine imports models
        resolved = _parse(date_str, mode)
        # parse_date_input may return a tuple for EDTF ranges; take the appropriate bound
        if isinstance(resolved, tuple):
            return resolved[0] if mode == "start" else resolved[1]
        return resolved
    except (ImportError, Exception):
        # Fallback: keep as-is (ISO dates already work; duration shorthands may not)
        return date_str


class MessageType(str, Enum):
    """Session message types."""

    USER = "user"
    ASSISTANT = "assistant"
    SYSTEM = "system"


@dataclass(frozen=True)
class FileVersion:
    """Immutable file version representation."""

    filename: str
    version_num: int
    line_count: int
    session_id: str
    timestamp: str = ""
    tool: str = "Write"  # "Write", "Edit", or "NotebookEdit"
    lines_added: int = 0  # gross lines added in this version's edit
    lines_deleted: int = 0  # gross lines deleted in this version's edit

    def __lt__(self, other: FileVersion) -> bool:
        """Compare by version number for sorting."""
        if not isinstance(other, FileVersion):
            return NotImplemented
        return self.version_num < other.version_num


@dataclass
class SessionFile:
    """File found in AI session history with edit tracking.

    Attributes:
        name: Filename (e.g., "session_manager.py")
        path: Full file path
        location: Where file was saved (main repo, worktree, etc)
        file_type: File extension without dot (e.g., "py", "md", "json")
        sessions: List of session IDs that touched this file
        edits: Total number of times THIS FILE was modified across ALL sessions
               (1-2 = simple one-off file, 10+ = heavily refined project file)
        created_date: When file was first created
        last_modified: When file was last edited
        size_bytes: File size in bytes
    """

    name: str
    path: str
    location: str = "recovery"
    file_type: str = "unknown"
    sessions: List[str] = field(default_factory=list)
    edits: int = 0
    created_date: Optional[str] = None
    last_modified: Optional[str] = None
    size_bytes: int = 0
    write_count: int = 0
    edit_count: int = 0
    notebook_edit_count: int = 0

    @property
    def is_versioned(self) -> bool:
        """Check if file has version history."""
        return self.edits > 0

    @property
    def session_count(self) -> int:
        """Get number of sessions this file appears in."""
        return len(self.sessions)

    def to_dict(self) -> dict:
        """Serialize to dict for JSON/CSV output and duck-typed formatter access."""
        return {
            "name": self.name,
            "path": self.path,
            "location": self.location,
            "file_type": self.file_type,
            "sessions": self.sessions,
            "edits": self.edits,
            "created_date": self.created_date,
            "last_modified": self.last_modified,
            "size_bytes": self.size_bytes,
        }


@dataclass
class SessionMessage:
    """Message from a session (user or assistant)."""

    type: MessageType
    timestamp: str
    content: str
    session_id: str
    line_count: Optional[int] = None
    tool_result: Optional[dict] = None  # Tool output: {stdout_chars, stderr_chars, output_chars, output_words} when available

    def preview(self, limit: int = 100) -> str:
        """Get preview of message content.

        Args:
            limit: Max characters to show. 0 = no limit (full content).
        """
        text = self.content.replace("\n", " ")
        if limit and len(text) > limit:
            return text[:limit]
        return text

    @property
    def is_long(self) -> bool:
        """Check if message is long."""
        return len(self.content) > 500

    def to_dict(self) -> dict:
        """Serialize for JSON output and _render_output row_fn access."""
        d = {
            "type": self.type.value if hasattr(self.type, "value") else str(self.type),
            "timestamp": self.timestamp,
            "content": self.content,
            "session_id": self.session_id,
        }
        if self.tool_result:
            d["tool_result"] = self.tool_result
        return d


@dataclass
class SessionMetadata:
    """Rich metadata for one record from a Claude Code session JSONL file.

    This is a user-side model: instantiate it yourself when parsing JSONL directly.
    The SessionRecoveryEngine does not produce SessionMetadata — use it when you want
    richer per-message metadata than SessionMessage provides.

    Example::

        with open(jsonl_file) as f:
            for line in f:
                data = json.loads(line)
                meta = SessionMetadata(
                    session_id=data["sessionId"],
                    timestamp=data.get("timestamp", ""),
                    message_type=data.get("type", ""),
                    tool_name=data.get("message", {}).get("tool_name"),
                )
    """

    session_id: str
    timestamp: str
    message_type: str
    operation: Optional[str] = None
    tool_name: Optional[str] = None
    file_path: Optional[str] = None

    @property
    def has_file_operation(self) -> bool:
        """Return True if this record is a Write, Edit, or Read tool call."""
        return self.tool_name in ("Write", "Edit", "Read")


@dataclass
class FilterSpec:
    """Advanced filter specification (mutable builder pattern).

    Construct with field kwargs, then optionally call with_*() builder methods to
    further narrow filters.  Builder methods mutate self and return self for chaining.
    """

    # File patterns
    file_patterns: List[str] = field(default_factory=lambda: ["*"])

    # File extension filtering (e.g., "py", "md", "json")
    include_extensions: Set[str] = field(default_factory=set)
    exclude_extensions: Set[str] = field(default_factory=set)

    # Edit count range: how many times a FILE was modified across all sessions
    # (1-2 = simple one-off files, 10+ = major project files with long history)
    min_edits: int = 0
    max_edits: Optional[int] = None

    # Session filtering
    include_sessions: Set[str] = field(default_factory=set)
    exclude_sessions: Set[str] = field(default_factory=set)

    # Location filtering
    include_folders: Set[str] = field(default_factory=set)
    exclude_folders: Set[str] = field(default_factory=set)

    # Datetime range filtering (ISO format: "2026-02-22" or "2026-02-22T14:30:00")
    # ``since`` and ``until`` are the CANONICAL names, matching CLI --since / --until.
    since: Optional[str] = None
    until: Optional[str] = None

    # File size range (bytes)
    min_size: int = 0
    max_size: Optional[int] = None

    def matches_session(self, session_id: str) -> bool:
        """Check if session matches filter."""
        if self.include_sessions and session_id not in self.include_sessions:
            return False
        if self.exclude_sessions and session_id in self.exclude_sessions:
            return False
        return True

    def matches_location(self, location: str) -> bool:
        """Check if location matches filter."""
        location_lower = location.lower()

        if self.include_folders:
            if not any(inc.lower() in location_lower for inc in self.include_folders):
                return False

        if self.exclude_folders:
            if any(exc.lower() in location_lower for exc in self.exclude_folders):
                return False

        return True

    def matches_extension(self, extension: str) -> bool:
        """Check if file extension matches filter.

        Args:
            extension: File extension (with or without leading dot, e.g., "py" or ".py")

        Returns:
            True if extension passes include/exclude filters.
        """
        # Normalize extension (remove leading dot if present)
        normalized_ext = extension.lstrip(".")

        if self.include_extensions:
            if normalized_ext not in self.include_extensions:
                return False

        if self.exclude_extensions:
            if normalized_ext in self.exclude_extensions:
                return False

        return True

    def matches_edits(self, edit_count: int) -> bool:
        """Check if edit count matches filter.

        Uses 'is not None' so max_edits=0 correctly excludes all files with any edits.
        """
        if edit_count < self.min_edits:
            return False
        if self.max_edits is not None and edit_count > self.max_edits:
            return False
        return True

    def matches_size(self, size_bytes: int) -> bool:
        """Check if size matches filter.

        Uses 'is not None' so max_size=0 correctly excludes all non-empty files.
        """
        if size_bytes < self.min_size:
            return False
        if self.max_size is not None and size_bytes > self.max_size:
            return False
        return True

    def __post_init__(self) -> None:
        """Normalize since/until if they look like duration shorthands or non-ISO strings.

        Allows ``FilterSpec(since="7d")`` to work correctly even without using the
        builder methods.  Builder methods (``with_since``, ``with_until``, etc.) call
        ``_resolve_date`` themselves, so the double-resolution is idempotent for
        already-resolved ISO strings (they start with four digits).
        """
        if self.since and not self.since[:4].isdigit():
            self.since = _resolve_date(self.since, "start")
        if self.until and not self.until[:4].isdigit():
            self.until = _resolve_date(self.until, "end")

    def matches_datetime(self, datetime_str: Optional[str]) -> bool:
        """Check if datetime falls within since/until range.

        Accepts any ISO 8601 prefix: "2026-02-22", "2026-02-22T14:30:00", etc.
        Lexicographic comparison works for all ISO 8601 precisions.

        Args:
            datetime_str: ISO datetime string (e.g. "2026-02-22" or "2026-02-22T14:30:00"), or None.

        Returns:
            True if datetime passes filter. Files with no datetime (None or empty string) pass
            when no filter is set; are excluded when a filter IS set (conservative: unknown
            datetime treated as out-of-range).
        """
        # Use explicit equality checks rather than truthiness to avoid "0" being falsy
        if datetime_str is None or datetime_str == "":
            return self.since is None and self.until is None
        if self.since or self.until:
            # Truncate to 19 chars to strip timezone designators (+00:00, Z) and
            # sub-second precision so that all sources compare correctly against
            # the naive ISO strings produced by _parse_date_input.
            datetime_str = datetime_str[:19]
        if self.since and datetime_str < self.since:
            return False
        if self.until and datetime_str > self.until:
            return False
        return True

    def with_pattern(self, *patterns: str) -> FilterSpec:
        """Builder: set file patterns."""
        self.file_patterns = list(patterns)
        return self

    def with_sessions(self, include: Optional[Set[str]] = None, exclude: Optional[Set[str]] = None) -> FilterSpec:
        """Builder: set session filtering.

        Args:
            include: Session IDs to include. Pass an empty set to clear any existing include filter.
            exclude: Session IDs to exclude. Pass an empty set to clear any existing exclude filter.
        """
        if include is not None:
            self.include_sessions = include
        if exclude is not None:
            self.exclude_sessions = exclude
        return self

    def with_extensions(self, include: Optional[Set[str]] = None, exclude: Optional[Set[str]] = None) -> FilterSpec:
        """Builder: filter by file extensions.

        Args:
            include: Extensions to include (e.g. {"py", "md"}). Pass empty set to clear.
                     None means "leave current setting unchanged".
            exclude: Extensions to exclude (e.g. {"pyc", "tmp"}). Applied after include.
                     Pass empty set to clear. None means "leave current setting unchanged".

        Returns:
            Self for builder chaining.

        Examples:
            FilterSpec().with_extensions(include={"py", "ts", "js"})  # Only Python/TS/JS
            FilterSpec().with_extensions(exclude={"tmp", "bak"})      # Exclude temp/backup
        """
        if include is not None:
            self.include_extensions = include
        if exclude is not None:
            self.exclude_extensions = exclude
        return self

    def with_since(self, since: str) -> FilterSpec:
        """Builder: filter to items modified ON OR AFTER this date.

        Duration shorthands ("7d", "2w", "1m") are resolved to ISO datetimes
        immediately so that ``matches_datetime()``'s lexicographic comparison works.

        Args:
            since: ISO date, duration shorthand, or EDTF expression.
                   Examples: "2026-01-01", "7d", "2w", "2026-01-01T14:30:00"

        Returns:
            Self for chaining: FilterSpec().with_since("7d").with_extensions({"py"})
        """
        self.since = _resolve_date(since, "start")
        return self

    def with_until(self, until: str) -> FilterSpec:
        """Builder: filter to items modified ON OR BEFORE this date.

        Duration shorthands are resolved to end-of-period ISO datetimes so that
        ``matches_datetime()``'s lexicographic comparison works correctly.

        Args:
            until: ISO date or duration shorthand.
                   Examples: "2026-12-31", "7d", "2026-06-01T00:00:00"

        Returns:
            Self for chaining.
        """
        self.until = _resolve_date(until, "end")
        return self

    def with_date_range(
        self,
        since: Optional[str] = None,
        until: Optional[str] = None,
    ) -> FilterSpec:
        """Builder: set both since and until bounds in one call.

        Duration shorthands are resolved to ISO datetimes (see with_since / with_until).

        Args:
            since: Lower bound (inclusive). None = no lower bound.
            until: Upper bound (inclusive). None = no upper bound.

        Returns:
            Self for chaining: FilterSpec().with_date_range(since="7d")

        Examples::

            FilterSpec().with_date_range(since="2026-01-01", until="2026-06-30")
            FilterSpec().with_date_range(since="7d")    # last 7 days, no upper bound
        """
        if since is not None:
            self.since = _resolve_date(since, "start")
        if until is not None:
            self.until = _resolve_date(until, "end")
        return self

    def with_when(self, when: str) -> FilterSpec:
        """Builder: set both since and until from an EDTF expression or duration shorthand.

        Mirrors CLI --when option. Parses the expression and sets both bounds.

        Args:
            when: Accepts:
                - Duration shorthand: "7d", "2w", "1m", "1y"
                - ISO date interval: "2026-01/2026-03" (Q1 2026)
                - EDTF decade pattern: "202X" (entire decade)
                - ISO single date: "2026-01-15" (exact day → since=that day)

        Returns:
            Self for chaining.

        Examples::

            FilterSpec().with_when("7d")              # last 7 days
            FilterSpec().with_when("2026-01/2026-03") # Q1 2026
            FilterSpec().with_when("202X")            # 2020-2029
        """
        if "/" in when:
            # ISO interval: "2026-01/2026-03"
            parts = when.split("/", 1)
            # Expand partial dates: "2026-01" → "2026-01-01", "2026-03" → "2026-03-31"
            self.since = _expand_partial_date(parts[0], start=True)
            self.until = _expand_partial_date(parts[1], start=False)
        elif "X" in when.upper() or "x" in when:
            # EDTF decade/year pattern: "202X" → 2020 to 2029
            upper = when.upper()
            decade_start = upper.replace("X", "0")
            decade_end = upper.replace("X", "9")
            self.since = f"{decade_start}-01-01T00:00:00"
            self.until = f"{decade_end}-12-31T23:59:59"
        else:
            # Duration shorthand ("7d", "2w") or plain ISO date ("2026-01-15").
            # Durations set only the lower bound (since); plain ISO dates set
            # both bounds (start and end of that day), consistent with CLI --when.
            self.since = _resolve_date(when, "start")
            # For plain ISO dates (no duration suffix), also set until=end-of-day
            # so that with_when("2026-01-15") matches only that single day.
            # Duration shorthands do NOT set an upper bound (they mean "since N ago").
            _is_duration = (
                len(when) <= 5 and when[:-1].isdigit() and when[-1:].lower() in "dwmhy"
            )
            if not _is_duration:
                self.until = _resolve_date(when, "end")
        return self

    def with_edit_range(
        self,
        min_edits: int = 0,
        max_edits: Optional[int] = None,
    ) -> FilterSpec:
        """Builder: filter by edit count range.

        Args:
            min_edits: Minimum edits (inclusive). Default 0.
            max_edits: Maximum edits (inclusive). None = no upper bound.

        Returns:
            Self for chaining: FilterSpec().with_edit_range(min_edits=3)
        """
        self.min_edits = min_edits
        self.max_edits = max_edits
        return self

    def __call__(self, files: Iterable) -> list:
        """Apply this FilterSpec as a post-glob predicate filter.

        Makes FilterSpec duck-type compatible with SearchFilter — both can be
        passed to search_files() without isinstance dispatch. Both are callable.

        Applies: extension, edits, size, location, and datetime filters.
        Session filtering uses include/exclude semantics.

        Args:
            files: Iterable of SessionFile objects.

        Returns:
            list[SessionFile] passing all filters.

        Example::

            spec = FilterSpec().with_since("7d").with_extensions(include={"py"})
            filtered = spec(all_files)   # directly callable like SearchFilter
        """
        result = []
        for f in files:
            if not self.matches_extension(f.file_type):
                continue
            if not self.matches_edits(f.edits):
                continue
            if not self.matches_size(f.size_bytes):
                continue
            if not self.matches_location(f.location):
                continue
            if not self.matches_datetime(f.last_modified or ""):
                continue
            # Session filter — mirrors _apply_all_filters() in engine.py:
            # • include_sessions: file must belong to at least one included session
            #   (files with no recorded sessions are excluded when include_sessions is set)
            # • exclude_sessions: files with no sessions are NOT excluded
            #   (they cannot belong to any excluded session, so they pass)
            if self.include_sessions:
                if not any(self.matches_session(s) for s in (f.sessions or [])):
                    continue
            if self.exclude_sessions:
                if any(s in self.exclude_sessions for s in (f.sessions or [])):
                    continue
            result.append(f)
        return result


def _expand_partial_date(date_str: str, start: bool) -> str:
    """Expand a partial ISO date string to a full YYYY-MM-DDTHH:MM:SS string.

    Args:
        date_str: Partial date like "2026-01", "2026", "2026-03-15"
        start: If True, expand to start of period; if False, expand to end.

    Examples:
        _expand_partial_date("2026-01", start=True)  → "2026-01-01T00:00:00"
        _expand_partial_date("2026-03", start=False) → "2026-03-31T23:59:59"
        _expand_partial_date("2026", start=False)    → "2026-12-31T23:59:59"
    """
    import calendar
    parts = date_str.strip().split("-")
    if len(parts) == 1:
        year = int(parts[0])
        if start:
            return f"{year:04d}-01-01T00:00:00"
        else:
            return f"{year:04d}-12-31T23:59:59"
    elif len(parts) == 2:
        year, month = int(parts[0]), int(parts[1])
        if start:
            return f"{year:04d}-{month:02d}-01T00:00:00"
        else:
            last_day = calendar.monthrange(year, month)[1]
            return f"{year:04d}-{month:02d}-{last_day:02d}T23:59:59"
    else:
        # Already has day component
        base = "-".join(parts[:3])
        if start:
            return f"{base}T00:00:00"
        else:
            return f"{base}T23:59:59"


@dataclass
class SessionStatistics:
    """Session operation statistics."""

    total_sessions: int = 0
    total_files: int = 0
    total_versions: int = 0
    largest_file: Optional[str] = None
    largest_file_edits: int = 0
    total_size_bytes: int = 0
    per_source: Dict[str, int] = field(default_factory=dict)

    @property
    def avg_versions_per_file(self) -> float:
        """Average number of recorded versions per unique file."""
        if self.total_files == 0:
            return 0.0
        return self.total_versions / self.total_files

    def to_dict(self) -> dict:
        """Convert to dictionary for serialization."""
        d: dict = {
            "total_sessions": self.total_sessions,
            "total_files": self.total_files,
            "total_versions": self.total_versions,
            "largest_file": self.largest_file,
            "largest_file_edits": self.largest_file_edits,
            "total_size_bytes": self.total_size_bytes,
            "avg_versions_per_file": self.avg_versions_per_file,
        }
        if self.per_source:
            d["per_source"] = self.per_source
        return d



@dataclass
class SessionInfo:
    """Summary metadata for one Claude Code session.

    Collected by scanning the first few lines of a session JSONL file.
    """

    session_id: str
    project_dir: str       # encoded dir name, e.g. "-Users-alice-myproject"
    cwd: str               # from first JSONL line that has a "cwd" field
    git_branch: str        # from first JSONL line that has a "gitBranch" field
    timestamp_first: str   # ISO 8601, earliest message timestamp
    timestamp_last: str    # ISO 8601, latest message timestamp
    message_count: int     # count of lines where type == "user" or "assistant"
    has_compact_summary: bool  # True if any line has isCompactSummary == true
    provider: str = ""     # "claude" | "aistudio" | "gemini_cli" | ""

    @property
    def project_display(self) -> str:
        """Human-readable project name — delegates to shared ``_decode_project_dir()``.

        Strips the home directory prefix (-Users-<name>- or -home-<name>-) and
        returns the last path component. Encoded dots (represented as leading '-')
        are preserved.
        """
        return _decode_project_dir(self.project_dir)

    def to_dict(self) -> dict:
        return {
            "session_id": self.session_id,
            "project_dir": self.project_dir,
            "project_display": self.project_display,
            "cwd": self.cwd,
            "git_branch": self.git_branch,
            "timestamp_first": self.timestamp_first,
            "timestamp_last": self.timestamp_last,
            "message_count": self.message_count,
            "has_compact_summary": self.has_compact_summary,
            "provider": self.provider,
        }


@dataclass
class CorrectionMatch:
    """A user message where the user corrected Claude's previous behavior."""

    session_id: str
    project_dir: str
    timestamp: str
    content: str           # full message text
    category: str          # "regression" | "skip_step" | "misunderstanding" | "incomplete"
    matched_pattern: str   # the substring that triggered the match (e.g. "you forgot")

    def to_dict(self) -> dict:
        return {
            "session_id": self.session_id,
            "project_dir": self.project_dir,
            "timestamp": self.timestamp,
            "content": self.content,
            "category": self.category,
            "matched_pattern": self.matched_pattern,
        }


@dataclass
class SlashCommandCount:
    """Usage count for one slash command across all sessions."""

    command: str           # e.g. "/ar:plannew", "/commit", "/plan"
    count: int
    session_ids: List[str]   # unique sessions where this command appeared
    project_dirs: List[str]  # unique project dirs where this command appeared

    def to_dict(self) -> dict:
        return {
            "command": self.command,
            "count": self.count,
            "unique_sessions": len(self.session_ids),
            "unique_projects": len(self.project_dirs),
            "session_ids": self.session_ids,
            "project_dirs": self.project_dirs,
        }


# Backward-compatible alias — deprecated; use SlashCommandCount.
PlanningCommandCount = SlashCommandCount


@dataclass
class SlashCommandRecord:
    """A single slash command invocation with args and session location."""

    command: str       # e.g. "/ar:plannew", "/commit", "/plan"
    args: str          # text after the command token (stripped)
    session_id: str
    timestamp: str
    project_dir: str
    cwd: str = ""          # working directory from JSONL record
    git_branch: str = ""   # git branch from JSONL record

    def to_dict(self) -> dict:
        return {
            "command": self.command,
            "args": self.args,
            "session_id": self.session_id,
            "timestamp": self.timestamp,
            "project_dir": self.project_dir,
            "cwd": self.cwd,
            "git_branch": self.git_branch,
        }


@dataclass
class SessionAnalysis:
    """Per-session statistics: message counts, tool usage, and files touched.

    Files touched are detected from any tool_use block where the tool's input
    contains a ``file_path`` key — not hardcoded to Edit/Write/Read, so it
    captures any future or custom tool that operates on files.
    """

    session_id: str
    project_dir: str
    total_lines: int               # JSONL lines (all types)
    user_count: int                # type == "user" messages
    assistant_count: int           # type == "assistant" messages
    tool_uses_by_name: Dict[str, int]   # tool_name -> invocation count
    files_touched: List[str]       # unique file paths from any tool with file_path input
    timestamp_first: str           # ISO 8601 or ""
    timestamp_last: str            # ISO 8601 or ""

    def to_dict(self) -> dict:
        return {
            "session_id": self.session_id,
            "project_dir": self.project_dir,
            "total_lines": self.total_lines,
            "user_count": self.user_count,
            "assistant_count": self.assistant_count,
            "tool_uses_by_name": self.tool_uses_by_name,
            "files_touched": self.files_touched,
            "timestamp_first": self.timestamp_first,
            "timestamp_last": self.timestamp_last,
        }


@dataclass
class ContextMatch:
    """A message search match with surrounding context messages.

    When ``search_messages_with_context`` is called with ``context > 0``,
    each result wraps the matching message with up to ``context`` messages
    before and after it from the same session file.
    """

    match: SessionMessage
    context_before: List[SessionMessage]  # messages before match, oldest first
    context_after: List[SessionMessage]   # messages after match, oldest first

    def to_dict(self) -> dict:
        return {
            "match": self.match.to_dict(),
            "context_before": [m.to_dict() for m in self.context_before],
            "context_after": [m.to_dict() for m in self.context_after],
        }
