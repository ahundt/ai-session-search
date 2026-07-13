"""
Core session recovery engine - refactored for composition and extensibility.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""

from __future__ import annotations

import contextlib
import datetime
import fnmatch
import functools
import glob as _glob_module
import json
import os
import re
from collections import defaultdict
from pathlib import Path
from typing import TYPE_CHECKING, Callable, Dict, Iterator, List, Optional, Set

if TYPE_CHECKING:
    from .filters import SearchFilter

try:
    from orjson import loads as _json_loads
except ImportError:
    from json import loads as _json_loads  # type: ignore[assignment]

from .models import (
    ContextMatch,
    CorrectionMatch,
    FileVersion,
    FilterSpec,
    MessageType,
    SlashCommandCount,
    PlanningCommandCount,  # backward-compat alias for SlashCommandCount
    SlashCommandRecord,
    SessionAnalysis,
    SessionFile,
    SessionInfo,
    SessionMessage,
    SessionStatistics,
)
from ai_session_search.config import get_config_path, load_config, write_config


def parse_date_input(s: str, mode: str = "start") -> str | tuple[str, str]:
    """Normalize flexible date/EDTF input to ISO 8601 for lexicographic comparison.

    Public utility for library users who need the same date normalization as the
    CLI date flags. Handles ISO dates, EDTF patterns (202X, 2026-01-1X), and
    duration shorthands (7d, 2w, 1m, 24h, 30min).

    mode="start" → lower_strict() bound (earliest date in period)
    mode="end"   → upper_strict() bound (latest date in period)

    Accepted formats:
      Any valid EDTF (Level 0-2):
        YYYY-MM-DDTHH:MM:SS    full ISO 8601 datetime
        YYYY-MM-DD             date-only
        YYYY-MM, YYYY          month/year precision (expanded to period bounds)
        202X, 19XX             EDTF Level 1 unspecified digits (decade/century)
        2026-01-1X             EDTF Level 1 unspecified day digit
        2026-01/2026-03        EDTF interval — returns (start_iso, end_iso) 2-tuple
      Duration shorthands (not EDTF, handled before library):
        7d, 2w, 1m, 24h, 30min   time ago from now
      NLP via python-dateutil (transitive dep of edtf):
        "yesterday", "3 days ago", "last Monday"

    Returns str for single date bound, tuple[str, str] for EDTF interval (A/B).
    Raises ValueError with user-friendly message on invalid/unrecognised input.
    """
    import re as _re
    import time as _time

    # Guard against None/empty input before calling .strip()
    if s is None:
        raise ValueError(
            "parse_date_input: date string must not be None. "
            "Run 'aise dates' for supported formats."
        )
    s = s.strip()
    if not s:
        raise ValueError(
            "parse_date_input: date string must not be empty. "
            "Run 'aise dates' for supported formats.\n"
            "Quick formats: 2026-01-15, 2026-01, 2026, 202X, 7d, 2w, 1m, 24h, 1y"
        )

    # ── Duration shorthand (7d, 2w, 1m, 24h, 30min, 1y) ─────────────────────
    # Handled before edtf: "1m" is not valid EDTF and would fail parsing.
    # "y" (years) approximated as 365 days, matching with_when docstring examples.
    _dur = _re.fullmatch(r"(\d+)(min|[dwmhy])", s, _re.IGNORECASE)
    if _dur:
        n, unit = int(_dur.group(1)), _dur.group(2).lower()
        _deltas = {
            "d":   datetime.timedelta(days=n),
            "w":   datetime.timedelta(weeks=n),
            "h":   datetime.timedelta(hours=n),
            "m":   datetime.timedelta(days=n * 30),   # 1m ≈ 30 days (approximate)
            "min": datetime.timedelta(minutes=n),
            "y":   datetime.timedelta(days=n * 365),  # 1y ≈ 365 days (approximate)
        }
        dt = datetime.datetime.now(datetime.timezone.utc) - _deltas[unit]
        return dt.strftime("%Y-%m-%dT%H:%M:%S")

    # ── Normalize case: EDTF library requires uppercase X for unspecified digits ──
    # Uppercase only date-like strings (digits, X, x, T, t, hyphens, colons, slash).
    # NLP strings like "yesterday" contain other letters and are left unchanged.
    if _re.fullmatch(r"[\dXxTt:/\-]+", s):
        s = s.upper()

    # ── Full ISO datetime (exact second) — return as-is, no expansion ────────
    # The edtf library's lower_strict()/upper_strict() floor full datetimes to
    # day boundaries (2026-01-15T14:30:25 → 2026-01-15T00:00:00). A fully-
    # specified YYYY-MM-DDTHH:MM:SS is an exact point; both modes return it.
    if _re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}", s):
        return s  # exact second — start and end are the same point

    # ── Partial datetime: hour or minute precision ────────────────────────────
    # EDTF Level 0 requires full YYYY-MM-DDTHH:MM:SS; partial times like
    # YYYY-MM-DDTHH or YYYY-MM-DDTHH:MM are not valid EDTF and fall through
    # to dateutil, which returns the same start-of-period for both modes.
    # Expand to full-period bounds based on detected precision:
    #   hour   (YYYY-MM-DDTHH)      → [THH:00:00, THH:59:59]
    #   minute (YYYY-MM-DDTHH:MM)   → [THH:MM:00, THH:MM:59]
    _partial_dt = _re.fullmatch(r"(\d{4}-\d{2}-\d{2}T\d{2})(?::(\d{2}))?", s)
    if _partial_dt:
        date_hh = _partial_dt.group(1)  # "2026-01-15T14"
        mm = _partial_dt.group(2)       # "30" or None (hour-only)
        if mm is None:
            return f"{date_hh}:00:00" if mode == "start" else f"{date_hh}:59:59"
        else:
            return f"{date_hh}:{mm}:00" if mode == "start" else f"{date_hh}:{mm}:59"

    # ── Day-level unspecified digits: edtf library bug workaround ────────────
    # parse_edtf("2026-01-1X") succeeds, but lower_strict()/upper_strict() crash
    # with ValueError because they try int("1X"). We handle these manually.
    _day_x = _re.fullmatch(r"(\d{4})-(\d{2})-([0-3X])([0-9X])", s)
    if _day_x and ("X" in _day_x.group(3) or "X" in _day_x.group(4)):
        import calendar as _cal
        year_s, month_s, tens_c, units_c = _day_x.groups()
        year, month = int(year_s), int(month_s)
        max_day = _cal.monthrange(year, month)[1]
        if tens_c == "X" and units_c == "X":
            # XX = all days in the month
            lo_day, hi_day = 1, max_day
        elif units_c == "X":
            # e.g. "1X" = days 10–19; "2X" = 20–29; "3X" = 30–31
            tens = int(tens_c)
            lo_day = tens * 10
            hi_day = min(tens * 10 + 9, max_day)
        else:
            # e.g. "X5" = days 5, 15, 25 (tens unspecified, units fixed)
            # Use the full span that digit could appear in
            units = int(units_c)
            lo_day = units if units >= 1 else 10   # day 0 is invalid; X0 → 10
            hi_day = min(units + 20, max_day)
        return (
            f"{year_s}-{month_s}-{lo_day:02d}T00:00:00"
            if mode == "start"
            else f"{year_s}-{month_s}-{hi_day:02d}T23:59:59"
        )

    # ── EDTF parsing ─────────────────────────────────────────────────────────
    try:
        from edtf import parse_edtf  # noqa: PLC0415
    except ImportError as exc:
        raise RuntimeError(
            "Package 'edtf' is required. Run: pip install aise"
        ) from exc

    def _st(st: "_time.struct_time") -> str:
        """Convert struct_time to ISO 8601 string for lexicographic comparison."""
        return _time.strftime("%Y-%m-%dT%H:%M:%S", st)

    try:
        parsed = parse_edtf(s)
        lo = _st(parsed.lower_strict())
        hi = _st(parsed.upper_strict())
        # EDTF upper_strict() returns midnight (T00:00:00) for dates without time precision
        # (day, month, year).  For "end" mode we want the end of that period, not midnight.
        # Replace trailing midnight with 23:59:59 so filters include the full last day.
        # Full datetimes (YYYY-MM-DDTHH:MM:SS) are handled by the exact-second path above
        # and never reach here, so this substitution is safe.
        if hi.endswith("T00:00:00"):
            hi = hi[:-8] + "23:59:59"
        # EDTF interval syntax contains "/": return 2-tuple so caller splits after+before
        if "/" in s:
            return (lo, hi)
        return lo if mode == "start" else hi

    except Exception as edtf_exc:
        # Fallback: python-dateutil NLP ("yesterday", "3 days ago")
        # — installed as transitive dep of edtf, so always available
        try:
            from dateutil import parser as _du  # noqa: PLC0415
            dt = _du.parse(s)
            if dt.tzinfo is None:
                dt = dt.replace(tzinfo=datetime.timezone.utc)
            return dt.strftime("%Y-%m-%dT%H:%M:%S")
        except ImportError:
            pass
        except Exception:
            pass

        raise ValueError(
            f"Unrecognised date/time: {s!r}. Run 'aise dates' for full format reference.\n"
            "Quick formats: 2026-01-15, 2026-01, 2026, 202X, 2026-01-1X, 7d, 2w, 1m, 24h"
        ) from edtf_exc


#: Default correction patterns: (category, [regex_keywords...])
#: Uses \b word boundaries (matching claude_session_tools.py:103-127).
DEFAULT_CORRECTION_PATTERNS: List[tuple] = [
    ("regression",       [r"\byou deleted\b", r"\byou removed\b", r"\blost\b",
                          r"\bregressed\b", r"\brollback\b", r"\brevert\b",
                          r"\bbroke\b"]),
    ("skip_step",        [r"\byou forgot\b", r"\byou missed\b", r"\byou skipped\b",
                          r"\bdon't forget\b", r"\bmissing step\b",
                          r"\byou didn't\b"]),
    ("misunderstanding", [r"\bwrong\b", r"\bincorrect\b", r"\bmistake\b",
                          r"\bnono\b", r"\bno,\s", r"\bthat's not correct\b",
                          r"\bactually\b", r"\bwait,?\s", r"\bwhat,"]),
    ("incomplete",       [r"\balso need\b", r"\bmust also\b", r"\bnot done\b",
                          r"\bnot finished\b", r"\bstill need\b",
                          r"\bshould have\b", r"\bbut you\b"]),
    # Catch-all for correction signals that don't fit the above 4 categories.
    # Checked last so it only fires when no specific category matches first.
    ("other",            [r"\bstop\b"]),
]

#: Default planning command regex patterns.
#: Using \b word boundaries (matching claude_session_tools.py:130-136).
DEFAULT_PLANNING_COMMANDS: List[str] = [
    r"/ar:plannew\b", r"/ar:pn\b",
    r"/ar:planrefine\b", r"/ar:pr\b",
    r"/ar:planupdate\b", r"/ar:pu\b",
    r"/ar:planprocess\b", r"/ar:pp\b",
    r"/plannew\b", r"/planrefine\b", r"/planupdate\b", r"/planprocess\b",
]

# Backward-compat alias: cli.py:673 imports _parse_date_input dynamically;
# all internal callers continue to work without modification.
_parse_date_input = parse_date_input

#: Regex to auto-discover slash commands that START a user message.
#: Matches e.g. "/ar:plannew", "/commit", "/help" — not file paths or URLs.
#: Used when analyze_planning_usage() is called with commands=None (discovery mode).
_SLASH_CMD_DISCOVERY_RE = re.compile(r"^/(\w[\w:.-]*)(?=\s|$)")

#: Fast timestamp extraction from raw JSONL lines (avoids json.loads for --after-timestamp).
_TS_EXTRACT_RE = re.compile(r'"timestamp"\s*:\s*"([^"]+)"')

#: Extract slash command from Claude Code's XML tag format:
#: <command-name>/ar:plannew</command-name>
#: Returns the command name (e.g. "/ar:plannew") from the XML tag.
_COMMAND_NAME_TAG_RE = re.compile(r"<command-name>(/[\w][\w:.-]*)</command-name>")

#: Raw-line pre-filter for discovery mode: checks if a JSONL line could contain
#: a slash command in the message content fields. Matches '"/ar:plannew' but NOT
#: '"/Users/foo' in a "cwd" or "path" field.
#:
#: _extract_content reads content from three JSON field names: "content", "text",
#: "message". A slash command must appear at the START of one of those values
#: (after optional leading space/tab — lstrip() handles it). "cwd", "path", and
#: other fields are excluded, eliminating the false-positive that caused every
#: user message to pass the naive '"/' check (every message has "cwd":"/Users/...").
#:
#: The \w after '/' mirrors _SLASH_CMD_DISCOVERY_RE's first char requirement —
#: only word characters start valid slash command names.
#:
#: Claude Code wraps slash commands in XML tags: <command-name>/ar:plannew</command-name>
#: so the regex also matches content starting with '<command' to catch both formats.
_SLASH_CMD_CONTENT_RE = re.compile(
    r'"(?:content|text|message)"\s*:\s*"[ \t]*(?:/\w|<command)'
)

#: System message patterns to filter from export
_EXPORT_FILTER_PATTERNS = (
    "[Request interrupted",
    "<task-notification>",
    "<system-reminder>",
)


def _check_redos_safe(pattern: str) -> None:
    """Reject regex patterns with nested quantifiers that cause catastrophic backtracking.

    Detects the most common ReDoS patterns: a quantifier (+, *, {n,}) applied to a
    group that itself contains a quantifier (e.g. ``(a+)+``, ``(a*)*``, ``(a|a)+``).

    This is a fast static heuristic, not a full NFA analysis. It catches the patterns
    that cause exponential backtracking in Python's re engine. Safe patterns pass through.

    Args:
        pattern: The regex string to validate.

    Raises:
        ValueError: If the pattern contains nested quantifier structures.
    """
    # Parse the regex to detect nested quantifiers using re's internal parser.
    # re._parser (Python 3.11+) or sre_parse (older) — both expose the same API.
    try:
        import re._parser as _sre  # Python 3.11+
    except ImportError:
        import sre_parse as _sre  # Python 3.6-3.10

    try:
        parsed = _sre.parse(pattern)
    except re.error:
        return  # Invalid regex will be caught by re.compile(); not our concern here.

    def _has_quantifier(items) -> bool:
        """Check if any item in a parsed regex group contains a quantifier."""
        for op, av in items:
            if op in (_sre.MAX_REPEAT, _sre.MIN_REPEAT):
                return True
            if op == _sre.SUBPATTERN and av[3] is not None:
                if _has_quantifier(av[3]):
                    return True
            if op == _sre.BRANCH:
                for branch in av[1]:
                    if _has_quantifier(branch):
                        return True
        return False

    def _check_nested(items) -> None:
        """Walk parsed regex tree and raise on nested quantifiers."""
        for op, av in items:
            if op in (_sre.MAX_REPEAT, _sre.MIN_REPEAT):
                # av = (min, max, [contents])
                inner = av[2]
                if _has_quantifier(inner):
                    raise ValueError(
                        f"ReDoS risk: nested quantifiers in pattern {pattern!r}. "
                        "Patterns like (a+)+, (a*)*, (a|a)+ cause catastrophic "
                        "backtracking. Use a simpler pattern."
                    )
                _check_nested(inner)
            elif op == _sre.SUBPATTERN and av[3] is not None:
                _check_nested(av[3])
            elif op == _sre.BRANCH:
                for branch in av[1]:
                    _check_nested(branch)

    _check_nested(parsed)


def _passes_date_filter(ts: str, since: Optional[str], until: Optional[str]) -> bool:
    """Return True iff ISO timestamp ts falls within [since, until] (inclusive).

    # ``since`` and ``until`` are the canonical param names; ``after``/``before``
    # are deprecated hidden aliases kept for backward compatibility.

    - Both since and until default to None (no restriction); any combination works.
    - None/empty ts with any active filter → False (consistent with FilterSpec semantics).
    - Normalizes ts to 19 chars (YYYY-MM-DDTHH:MM:SS) before comparison, stripping
      timezone designators (+00:00, Z) and sub-second precision so that timestamps
      from all sources (Claude, AI Studio, Gemini CLI) compare correctly against
      the naive ISO strings produced by _parse_date_input.
    - ISO 8601 lexicographic order == chronological for fixed-width prefixes.
    - Pure function: no I/O, no side effects. O(1).
    """
    if (since or until) and not ts:
        return False   # unknown/None timestamp excluded when filtering is active
    if ts and (since or until):
        # Strip timezone suffix and sub-second precision for uniform comparison.
        # since/until from _parse_date_input are always 19-char naive ISO strings.
        ts = ts[:19]
    if since and ts < since:
        return False
    if until and ts > until:
        return False
    return True


def _path_stat_iso(path: Path) -> tuple:
    """Return (mtime_iso, birthtime_iso) from a single stat() call.

    One stat() call yields both timestamps. birthtime is platform-specific:
    - macOS/BSD: st_birthtime (true creation time)
    - Windows:   st_ctime    (creation time; Windows st_ctime semantics differ from POSIX)
    - Linux:     None        (st_birthtime unavailable; st_ctime = inode change time, not creation)

    Both values are 19-char UTC ISO strings or None on unavailability/OSError.
    None means "cannot determine" — callers treat as "do not skip" to avoid false exclusions.
    Used by _iter_all_jsonl for the combined since/until mtime+birthtime pre-filter.
    """
    import sys as _sys
    try:
        s = path.stat()

        def _to_iso(ts: float) -> str:
            return datetime.datetime.fromtimestamp(
                ts, tz=datetime.timezone.utc
            ).strftime("%Y-%m-%dT%H:%M:%S")

        mtime_iso = _to_iso(s.st_mtime)
        bt = getattr(s, "st_birthtime", None)       # macOS/BSD: true creation time
        if bt is None and _sys.platform == "win32":
            bt = s.st_ctime                          # Windows: ctime = creation time
        # Linux: bt stays None (st_ctime = inode change time, wrong semantics)
        birthtime_iso = _to_iso(bt) if bt is not None else None
        return mtime_iso, birthtime_iso
    except OSError:
        return None, None


def _path_mtime_iso(path: Path) -> Optional[str]:
    """Return a file's mtime as a 19-char UTC ISO string, or None on OSError.

    Backward-compatible wrapper around _path_stat_iso. New code should use
    _path_stat_iso to get both mtime and birthtime in a single stat() call.
    """
    mtime_iso, _ = _path_stat_iso(path)
    return mtime_iso


def _read_first_timestamp(path: Path, max_lines: int = 10) -> Optional[str]:
    """Return the first valid timestamp from a JSONL file, reading at most max_lines.

    Reads only until a timestamp is found — typically 1 line, at most a few KB.
    Used for the until pre-filter: if first_ts > until, the entire file can be
    skipped without reading 100MB+ of content.

    Returns None on OSError or if no timestamp found within max_lines.
    Callers treat None as "cannot determine" — do not skip (conservative).
    """
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            for _ in range(max_lines):
                line = f.readline()
                if not line:
                    break
                line = line.strip()
                if not line:
                    continue
                try:
                    ts = _json_loads(line).get("timestamp", "")
                    if ts:
                        return ts[:19]  # normalize to 19-char ISO
                except (json.JSONDecodeError, KeyError, ValueError):
                    continue
    except OSError:
        pass
    return None


def _is_compaction(data: dict, content: Optional[str]) -> bool:
    """Return True if this JSONL record is a compaction summary message.

    Detects compaction via two signals:
    1. isCompactSummary=True field on the raw record (Claude Code flag).
    2. Content starts with "This session is being continued" (canonical prefix).

    Guards against None content — some JSONL records have missing/null text.
    Module-level so it can be reused by search_messages and search_messages_with_context.
    """
    return bool(data.get("isCompactSummary")) or (content or "").startswith(
        "This session is being continued"
    )


def _is_continuation_summary(content: Optional[str]) -> bool:
    """Return True if this message is a context continuation summary.

    Stricter subset of _is_compaction: checks only the text prefix (not the
    isCompactSummary flag). Used by find_corrections() to filter false positives
    from continuation summaries that quote previous user messages verbatim.
    """
    if not content:
        return False
    return content.lstrip().startswith("This session is being continued from")


def _is_skill_injection(data: dict, content: Optional[str], prev_was_slash: bool) -> bool:
    """Detect skill injection messages: user messages injected by hook system.

    Skill injections are user-role messages immediately following a <command-name>
    message. They typically start with a markdown heading (# Skill Name) and
    contain the full SKILL.md body. They never have their own <command-name> tag.

    Two detection modes:
    1. prev_was_slash=True: previous user message was a slash command → detect by
       heading prefix (# ) without own <command-name> tag
    2. Structural: content matches known SKILL.md patterns regardless of prev_was_slash
       (catches cases where assistant messages interleave between slash cmd and injection)
    """
    if data.get("type") != "user":
        return False
    if not content:
        return False
    if "<command-name>" in content:
        return False
    stripped = content.lstrip()
    # Mode 1: immediately after slash command
    if prev_was_slash:
        if stripped.startswith("# ") or stripped.startswith("<command-"):
            return True
    # Mode 2: structural SKILL.md detection — skill bodies contain markdown headings
    # with slash command references like "# Create New Plan (/ar:plannew)"
    # and structural sections like "## 1. Foundation" or "### 1.1 Key Principles"
    if _SKILL_HEADING_RE.search(stripped[:500]):
        return True
    # Mode 3: long structured content — SKILL.md/CLAUDE.md bodies start with #/## heading
    # and contain many ### subsections. Real user corrections are short.
    if _is_structured_instruction_body(stripped):
        return True
    return False


# Compiled pattern for structural skill body detection.
# Matches skill headings like "# Title (/ar:plannew)" or "# Title (short: /ar:go)".
_SKILL_HEADING_RE = re.compile(
    r"^#\s+.+(?:\(/(?:ar|cr):[a-z]|/(?:ar|cr):[a-z][a-z0-9-]*\))"
)

# Subsection heading pattern for structured instruction detection.
_SUBSECTION_RE = re.compile(r"^#{2,4}\s+", re.MULTILINE)


def _is_structured_instruction_body(content: str) -> bool:
    """Detect structured instruction bodies (SKILL.md, CLAUDE.md sections).

    Skill/instruction bodies are long, start with a markdown heading (#/##),
    and contain many subsection headings (###). Real user messages with
    corrections are short and conversational.

    Heuristic: starts with heading + has 5+ subsection headings + >500 chars.
    """
    if len(content) < 500:
        return False
    if not content.startswith("#"):
        return False
    # Count ## / ### / #### headings in first 5000 chars.
    # Real skill bodies spread headings across thousands of chars.
    subsections = len(_SUBSECTION_RE.findall(content[:5000]))
    return subsections >= 5


def _iter_tool_use_blocks(content: list) -> "Iterator[dict]":
    """Yield tool_use items from a message content array."""
    for item in content:
        if isinstance(item, dict) and item.get("type") == "tool_use":
            yield item


class SessionRecoveryEngine:
    """Core recovery engine with clean separation of concerns."""

    def __init__(self, projects_dir: Path, recovery_dir: Path):
        """Initialize engine with directories.

        Args:
            projects_dir: Path to Claude Code projects directory
                          (default: ~/.claude/projects). Must contain JSONL session files.
            recovery_dir: Path to recovery output directory
                          (default: ~/.claude/recovery). Contains session_*/ subdirs
                          with extracted source files and version history.
        """
        self.projects_dir = Path(projects_dir)
        self.recovery_dir = Path(recovery_dir)
        self._file_cache: Dict[str, SessionFile] = {}
        self._version_cache: Dict[str, List[FileVersion]] = {}

    # ── Project name helpers ─────────────────────────────────────────────────

    @staticmethod
    def extract_project_name(encoded_dir: str) -> str:
        """Return a human-readable project name from Claude's encoded directory name.

        Delegates to ``_decode_project_dir()`` in models.py — the canonical
        implementation shared with ``SessionInfo.project_display``.

        Claude encodes project paths by replacing every non-alphanumeric, non-hyphen
        character with '-'.  Examples:
            /Users/alice/project   →  -Users-alice-project
            /Users/alice/source/p  →  -Users-alice-source-p
            /home/bob/project      →  -home-bob-project

        Args:
            encoded_dir: Encoded project directory name (e.g. "-Users-alice-project").

        Returns:
            Human-readable project name (e.g. "project").
        """
        from .models import _decode_project_dir
        return _decode_project_dir(encoded_dir)

    # ── Private JSONL iteration helpers ──────────────────────────────────────

    def _iter_all_jsonl(
        self,
        project_filter: Optional[str] = None,
        session_id_prefix: Optional[str] = None,
        since: Optional[str] = None,
        until: Optional[str] = None,
    ):
        """Yield (project_dir_name, jsonl_path, skip_until_check) for every session.

        Handles missing directory gracefully. All multi-session scanning methods
        use this to avoid duplicating the glob + filter scaffolding.

        Performance optimizations (avoid reading 100MB+ JSONL files):
        - since:  mtime < since → skip file (no I/O; mtime = last message timestamp).
        - until:  first_ts > until → skip file (1-line read; see two-stage design below).
          Fast path: birthtime <= until (macOS st_birthtime / Windows st_ctime) → yield
          without reading the first line (file created before until, can't be entirely too new).
          Verify path: birthtime unavailable (Linux) or birthtime > until (possible sync-reset
          on synced/restored machines where birthtime is reset to transfer time) → read first
          line to confirm. mtime is preserved by sync tools; birthtime is not.
        - skip_until_check (3rd yield value): True when mtime < until, meaning ALL records
          are before until — callers skip the per-record ts > until check for the whole file.

        Args:
            project_filter:    Substring to match project_dir name. None = all projects.
            session_id_prefix: If set, only yield files whose stem starts with this prefix.
            since:             mtime pre-filter cutoff. Accepts parse_date_input formats.
            until:             first-line pre-filter cutoff. Accepts parse_date_input formats.

        Yields:
            (project_dir_name: str, jsonl_path: Path, skip_until_check: bool)
        """
        if not self.projects_dir.exists():
            return
        # Normalize since/until once — handles raw relative ("14d") and ISO from CLI.
        since_iso: Optional[str] = None
        if since:
            try:
                _s = parse_date_input(since, "start")
                since_iso = _s[0] if isinstance(_s, tuple) else _s
            except (ValueError, AttributeError, TypeError):
                pass  # invalid since → no mtime pre-filter; per-record filter still applies
        until_iso: Optional[str] = None
        if until:
            try:
                _u = parse_date_input(until, "end")
                until_iso = _u[1] if isinstance(_u, tuple) else _u
            except (ValueError, AttributeError, TypeError):
                pass  # invalid until → no first-line pre-filter; per-record filter still applies
        for project_dir in self.projects_dir.glob("*"):
            if not project_dir.is_dir():
                continue
            if project_filter:
                pf_raw = project_filter.lower()
                pf_norm = pf_raw.replace("_", "-")        # ai_session_search → ai-session-tools
                decoded = self.extract_project_name(project_dir.name).lower()
                encoded = project_dir.name.lower()
                if pf_norm not in encoded and pf_raw not in decoded and pf_norm not in decoded:
                    continue
            for jsonl_file in project_dir.glob("*.jsonl"):
                # Session scope filter
                if session_id_prefix and not jsonl_file.stem.startswith(session_id_prefix):
                    continue
                # One stat() call for both mtime and birthtime
                mtime_iso, birthtime_iso = _path_stat_iso(jsonl_file)
                # since: mtime < since → all messages too old → skip (no I/O)
                if since_iso and mtime_iso and not _passes_date_filter(mtime_iso, since_iso, None):
                    continue
                # until: skip file if all messages are after until
                if until_iso:
                    if birthtime_iso is not None and birthtime_iso <= until_iso:
                        # File created before until → can't be entirely too new → yield
                        pass
                    else:
                        # No reliable birthtime (Linux) OR birthtime > until (possible
                        # sync-reset: birthtime reset to transfer time on synced machines).
                        # Verify with first-line read — cheap vs skipping a valid 100MB file.
                        first_ts = _read_first_timestamp(jsonl_file)
                        if first_ts is not None and first_ts > until_iso:
                            continue  # confirmed: all messages after until → skip
                # Per-record hint: mtime < until → all records ≤ mtime < until
                skip_until_check = bool(until_iso and mtime_iso and mtime_iso < until_iso)
                yield project_dir.name, jsonl_file, skip_until_check

    def _find_session_files(self, session_id: str) -> List[tuple]:
        """Return all (jsonl_path, project_dir_name) tuples for the given session ID prefix.

        Returns an empty list if no session matches.
        Results are sorted by file modification time descending (newest first),
        so callers that need a single session can use matches[0].
        """
        matches = []
        for project_dir_name, jsonl_path, _ in self._iter_all_jsonl():
            if jsonl_path.stem.startswith(session_id):
                try:
                    mtime = jsonl_path.stat().st_mtime
                except OSError:
                    mtime = 0.0
                matches.append((mtime, jsonl_path, project_dir_name))
        # Sort newest first, then strip the mtime sort key
        matches.sort(key=lambda x: x[0], reverse=True)
        return [(path, proj) for _mtime, path, proj in matches]

    @staticmethod
    def _scan_jsonl(path: Path):
        """Yield parsed JSON dicts from a JSONL file, silently skipping malformed lines.

        Handles OSError (permission denied, missing file) by returning nothing.
        """
        try:
            with open(path, encoding="utf-8", errors="replace") as f:
                for line in f:
                    try:
                        data = _json_loads(line)
                        if isinstance(data, dict):
                            yield data
                    except (json.JSONDecodeError, ValueError):
                        continue
        except OSError:
            return

    # ── End private helpers ───────────────────────────────────────────────────

    @functools.cached_property
    def _version_dirs(self) -> List[Path]:
        """Session all-versions dirs — scanned once per engine instance."""
        if not self.recovery_dir.exists():
            return []
        return [
            d for d in self.recovery_dir.iterdir()
            if d.is_dir() and d.name.startswith("session_all_versions_")
        ]

    @staticmethod
    def _compile_pattern(pattern: str) -> re.Pattern:
        """Compile pattern, auto-detecting glob vs regex.

        Args:
            pattern: Glob pattern (e.g. '*.py') or regex (e.g. 'cli.*').

        Returns:
            Compiled regex, case-insensitive.

        Raises:
            ValueError: If pattern is not a valid glob or regex, or has ReDoS risk.
        """
        if "*" in pattern or "?" in pattern:
            regex_pattern = fnmatch.translate(pattern)
            return re.compile(regex_pattern, re.IGNORECASE)
        _check_redos_safe(pattern)
        try:
            return re.compile(pattern, re.IGNORECASE)
        except re.error as exc:
            raise ValueError(f"Invalid search pattern {pattern!r}: {exc}") from exc

    def _get_or_create_file_info(self, file_path: Path) -> Optional[SessionFile]:
        """Get or create cached file info.

        Cache is keyed by filename (basename): across multiple session dirs the same
        filename is intentionally deduplicated — metadata (size, dates) comes from the
        first-encountered copy; version history comes from get_versions() which scans
        all session_all_versions_*/ dirs.

        Returns:
            SessionFile, or None if the file's stat() call fails (e.g. broken symlink).
        """
        if file_path.name not in self._file_cache:
            try:
                versions = self.get_versions(file_path.name)
                stat = file_path.stat()
                last_modified = datetime.datetime.fromtimestamp(
                    stat.st_mtime, tz=datetime.timezone.utc
                ).strftime("%Y-%m-%dT%H:%M:%S")
                created_date = datetime.datetime.fromtimestamp(
                    stat.st_ctime, tz=datetime.timezone.utc
                ).strftime("%Y-%m-%dT%H:%M:%S")
                self._file_cache[file_path.name] = SessionFile(
                    name=file_path.name,
                    path=str(file_path.resolve()),
                    location="recovery",
                    file_type=file_path.suffix[1:] or "unknown",
                    sessions=[v.session_id for v in versions],
                    edits=len(versions),
                    size_bytes=stat.st_size,
                    last_modified=last_modified,
                    created_date=created_date,
                )
            except OSError:
                return None
        return self._file_cache.get(file_path.name)

    def _apply_all_filters(self, file_info: SessionFile, filters: FilterSpec) -> bool:
        """Check if file passes all filters. Returns True if file should be included.

        Order: cheapest checks first (extension, size, datetime) before expensive ones
        (sessions iterates a list; edits unavoidable; location always passes after pre-check).
        """
        if not filters.matches_extension(file_info.file_type):
            return False

        if not filters.matches_size(file_info.size_bytes):
            return False

        if not filters.matches_datetime(file_info.last_modified):
            return False

        # Session filter: apply include and exclude independently to mirror
        # FilterSpec.__call__() semantics (the two callsites must agree).
        # include_sessions: at least one of the file's sessions must be in the set.
        # exclude_sessions: NONE of the file's sessions may be in the set.
        # Files with no sessions fail include_sessions (they cannot satisfy inclusion)
        # but pass exclude_sessions (nothing to exclude matches).
        if filters.include_sessions:
            if not any(s in filters.include_sessions for s in (file_info.sessions or [])):
                return False
        if filters.exclude_sessions:
            if any(s in filters.exclude_sessions for s in (file_info.sessions or [])):
                return False

        if not filters.matches_edits(file_info.edits):
            return False

        if not filters.matches_location(file_info.location):
            return False

        return True

    def search(  # noqa: C901
        self,
        pattern: str,
        filters: Optional[FilterSpec] = None,
    ) -> List[SessionFile]:
        """Search files with optional filtering.

        Args:
            pattern: Glob or regex pattern (e.g. '*.py', 'cli.*')
            filters: Optional FilterSpec for advanced filtering

        Returns:
            List of SessionFile objects sorted by edits (descending).
            Returns empty list if recovery_dir does not exist.

        Raises:
            ValueError: If pattern is not a valid glob or regex.
        """
        if filters is None:
            filters = FilterSpec()

        # Compile first: raise ValueError on bad patterns before any I/O.
        pattern_re = self._compile_pattern(pattern)

        results: List[SessionFile] = []
        seen_names: Set[str] = set()

        # Phase 1: Scan recovery_dir for Write-based files
        skip_phase1 = (
            not self.recovery_dir.exists()
            or (filters.include_folders and not filters.matches_location("recovery"))
        )

        if not skip_phase1:
            with os.scandir(self.recovery_dir) as it:
                for entry in it:
                    if not entry.is_dir():
                        continue
                    name = entry.name
                    if not name.startswith("session_") or "all_versions" in name:
                        continue

                    # Session dir-level pre-filter: skip entire dir before iterating files.
                    session_id_str = name[len("session_"):]
                    if filters.include_sessions and session_id_str not in filters.include_sessions:
                        continue
                    if filters.exclude_sessions and session_id_str in filters.exclude_sessions:
                        continue

                    session_dir = Path(entry.path)

                    with os.scandir(session_dir) as it2:
                        for entry2 in it2:
                            if not entry2.is_file() or not pattern_re.search(entry2.name):
                                continue
                            if entry2.name in seen_names:
                                continue

                            file_path = Path(entry2.path)

                            # Extension pre-filter: skip stat+get_versions for wrong extensions.
                            ext = file_path.suffix.lstrip(".")
                            if not filters.matches_extension(ext):
                                continue

                            # Stat pre-filter: check date/size before expensive get_versions.
                            if filters.since or filters.until or filters.min_size or (filters.max_size is not None):
                                try:
                                    s = file_path.stat()
                                    mtime = datetime.datetime.fromtimestamp(
                                        s.st_mtime, tz=datetime.timezone.utc
                                    ).strftime("%Y-%m-%dT%H:%M:%S")
                                    if not filters.matches_datetime(mtime) or not filters.matches_size(s.st_size):
                                        continue
                                except OSError:
                                    continue

                            file_info = self._get_or_create_file_info(file_path)
                            if file_info is None:
                                continue

                            if self._apply_all_filters(file_info, filters):
                                results.append(file_info)
                                seen_names.add(file_path.name)

        # Phase 2: Scan JSONL for Edit/Write/NotebookEdit tool calls not in recovery_dir
        edit_files: dict[str, dict] = {}  # filename -> aggregated info
        for call in self._iter_file_tool_calls(
            filename="*",
            session_id=None,
            tools=("Edit", "Write", "NotebookEdit"),
        ):
            fname = Path(call["file_path"]).name
            if fname in seen_names:
                continue
            if not pattern_re.search(fname):
                continue
            ext = Path(fname).suffix.lstrip(".")
            if not filters.matches_extension(ext):
                continue
            ts = call.get("timestamp", "")
            if not filters.matches_datetime(ts):
                continue
            if fname not in edit_files:
                edit_files[fname] = {
                    "sessions": set(), "edits": 0,
                    "first_ts": ts, "last_ts": ts, "path": call["file_path"],
                    "write_count": 0, "edit_count": 0, "notebook_edit_count": 0,
                }
            info = edit_files[fname]
            info["sessions"].add(call["session_id"])
            info["edits"] += 1
            tool_name = call["tool"]
            if tool_name == "Write":
                info["write_count"] += 1
            elif tool_name == "Edit":
                info["edit_count"] += 1
            elif tool_name == "NotebookEdit":
                info["notebook_edit_count"] += 1
            if ts and (not info["first_ts"] or ts < info["first_ts"]):
                info["first_ts"] = ts
            if ts and (not info["last_ts"] or ts > info["last_ts"]):
                info["last_ts"] = ts

        for fname, info in edit_files.items():
            if not filters.matches_edits(info["edits"]):
                continue
            if filters.include_sessions and not info["sessions"] & filters.include_sessions:
                continue
            if filters.exclude_sessions and info["sessions"] & filters.exclude_sessions:
                continue
            results.append(SessionFile(
                name=fname,
                path=info["path"],
                edits=info["edits"],
                sessions=sorted(info["sessions"]),
                created_date=info["first_ts"],
                last_modified=info["last_ts"],
                write_count=info["write_count"],
                edit_count=info["edit_count"],
                notebook_edit_count=info["notebook_edit_count"],
            ))
            seen_names.add(fname)

        return sorted(results, key=lambda f: f.edits, reverse=True)

    def get_versions(self, filename: str) -> List[FileVersion]:
        """Get all versions of a file across all sessions.

        Args:
            filename: Exact filename (e.g. 'cli.py')

        Returns:
            List of FileVersion objects sorted by version number (ascending).
        """
        if filename in self._version_cache:
            return self._version_cache[filename]

        versions = []
        version_pattern = r"_v(\d+)_line_(\d+)\.txt$"

        for session_dir in self._version_dirs:
            # Escape glob metacharacters ([, ], ?, *) in the filename so that
            # filenames like "data[0].py" or "file?.py" produce valid glob patterns.
            escaped = _glob_module.escape(filename)
            for version_file in session_dir.glob(f"{escaped}_v*_line_*.txt"):
                match = re.search(version_pattern, version_file.name)
                if match:
                    version_num = int(match.group(1))
                    line_count = int(match.group(2))
                    try:
                        ts = datetime.datetime.fromtimestamp(
                            version_file.stat().st_mtime, tz=datetime.timezone.utc
                        ).strftime("%Y-%m-%d %H:%M")
                    except OSError:
                        ts = ""
                    versions.append(
                        FileVersion(
                            filename=filename,
                            version_num=version_num,
                            line_count=line_count,
                            session_id=session_dir.name.replace("session_all_versions_", ""),
                            timestamp=ts,
                        )
                    )

        # Phase 2: Append Edit/NotebookEdit entries from JSONL
        max_write_version = max((v.version_num for v in versions), default=0)
        # Estimate running line count from last Write version or disk file
        running_line_count = 0
        if versions:
            last_write = max(versions, key=lambda v: v.version_num)
            running_line_count = last_write.line_count
        else:
            orig = self.get_original_path(filename)
            if orig and Path(orig).exists():
                try:
                    with open(orig, encoding="utf-8", errors="replace") as _f:
                        running_line_count = sum(1 for _ in _f)
                except OSError:
                    pass
        edit_version_num = max_write_version + 1
        # Scan all tool types: Write calls update baseline, Edit/NotebookEdit create entries
        all_calls = sorted(
            self._iter_file_tool_calls(filename, tools=("Edit", "Write", "NotebookEdit")),
            key=lambda x: x.get("timestamp", ""),
        )
        for call in all_calls:
            inp = call["input"]
            if call["tool"] == "Write":
                # Write calls update running baseline but don't create new version entries
                # (Phase 1 already has them from recovery_dir)
                content = inp.get("content", "")
                running_line_count = content.count("\n") + (1 if content and not content.endswith("\n") else 0)
                continue
            old_s = inp.get("old_string", "")
            new_s = inp.get("new_string", "")
            if not old_s and not new_s:
                continue
            old_lines = old_s.count("\n")
            new_lines = new_s.count("\n")
            added = max(new_lines - old_lines, 0)
            deleted = max(old_lines - new_lines, 0)
            running_line_count += (new_lines - old_lines)
            versions.append(
                FileVersion(
                    filename=filename,
                    version_num=edit_version_num,
                    line_count=running_line_count,
                    session_id=call["session_id"],
                    timestamp=call.get("timestamp", ""),
                    tool=call["tool"],
                    lines_added=added,
                    lines_deleted=deleted,
                )
            )
            edit_version_num += 1

        # Sort by timestamp when Edit entries present for chronological interleaving
        if edit_version_num > max_write_version + 1:
            versions.sort(key=lambda v: (v.timestamp or "", v.version_num))
        else:
            versions.sort()
        self._version_cache[filename] = versions
        return versions

    def _resolve_version_paths(
        self, filename: str,
    ) -> tuple:
        """Return (version_file_paths, fallback_path).

        version_file_paths: list of (Path, FileVersion) from session_all_versions_*/ dirs.
        fallback_path: Path from session_*/ dirs (first match), only if version_file_paths is empty.
        """
        versions = self.get_versions(filename)
        version_paths = []
        if versions:
            for v in sorted(versions):
                session_dir = self.recovery_dir / f"session_all_versions_{v.session_id}"
                vf = session_dir / f"{filename}_v{v.version_num:06d}_line_{v.line_count}.txt"
                if vf.exists():
                    version_paths.append((vf, v))

        fallback = None
        if not version_paths and self.recovery_dir.exists():
            for session_dir in self.recovery_dir.glob("session_*/"):
                if "all_versions" not in session_dir.name:
                    fp = session_dir / filename
                    if fp.exists():
                        fallback = fp
                        break
        return version_paths, fallback

    def extract_final(self, filename: str, output_dir: Path) -> Optional[Path]:
        """Extract the most recent version of a file (highest version number).

        Args:
            filename: Exact filename to extract (e.g. 'cli.py')
            output_dir: Directory to write the extracted file

        Returns:
            Path to extracted file, or None if not found.
        """
        version_paths, fallback = self._resolve_version_paths(filename)
        source = version_paths[-1][0] if version_paths else fallback
        if source is None:
            return None
        output_dir.mkdir(parents=True, exist_ok=True)
        output_path = output_dir / filename
        output_path.write_text(source.read_text(errors="ignore"))
        return output_path

    def extract_all(self, filename: str, output_dir: Path) -> List[Path]:
        """Extract all recorded versions of a file.

        Args:
            filename: Exact filename to extract (e.g. 'cli.py')
            output_dir: Directory to write version files

        Returns:
            List of paths to extracted version files.
        """
        version_paths, fallback = self._resolve_version_paths(filename)
        output_dir.mkdir(parents=True, exist_ok=True)
        extracted = []
        if version_paths:
            for vf, v in version_paths:
                target = output_dir / f"v{v.version_num:06d}_line_{v.line_count}.txt"
                target.write_text(vf.read_text(errors="ignore"))
                extracted.append(target)
        elif fallback:
            target = output_dir / "v000001_final.txt"
            target.write_text(fallback.read_text(errors="ignore"))
            extracted.append(target)
        return extracted

    @staticmethod
    def _parse_message_type(msg_type: str) -> MessageType:
        """Parse a message type string into a MessageType enum value.

        Falls back to MessageType.SYSTEM for unrecognised types.
        """
        try:
            return MessageType(msg_type)
        except ValueError:
            return MessageType.SYSTEM

    def _process_message_line(
        self, line: str, session_id: str, message_type: Optional[str]
    ) -> Optional[SessionMessage]:
        """Process a single JSONL line into a SessionMessage.

        Supports prefix matching: session_id 'ab841016' matches 'ab841016-f07b-...'.
        """
        try:
            # Fast pre-filter: session_id must appear in the raw line (no false negatives).
            if session_id not in line:
                return None
            data = _json_loads(line)
            msg_session = data.get("sessionId", "")
            # Prefix match: allow short IDs (e.g. 'ab841016') to match full UUIDs
            if msg_session != session_id and not msg_session.startswith(session_id):
                return None

            msg_type = data.get("type", "").lower()
            if message_type and msg_type != message_type.lower():
                return None

            content = self._extract_content(data)
            if not content:
                return None

            return SessionMessage(
                type=self._parse_message_type(msg_type),
                timestamp=data.get("timestamp", ""),
                content=content,
                session_id=msg_session,
            )
        except (json.JSONDecodeError, KeyError, ValueError):
            return None

    def get_messages(self, session_id: str, message_type: Optional[str] = None) -> List[SessionMessage]:
        """Extract messages from a session.

        Args:
            session_id: Full or prefix UUID (e.g. 'ab841016' or 'ab841016-f07b-...')
            message_type: Optional filter: 'user', 'assistant', or 'system'

        Returns:
            List of SessionMessage objects. Empty list if projects_dir does not exist.
        """
        messages: List[SessionMessage] = []

        if not self.projects_dir.exists():
            return messages

        for project_dir in self.projects_dir.glob("*"):
            if not project_dir.is_dir():
                continue

            for jsonl_file in project_dir.glob(f"{session_id}*.jsonl"):
                try:
                    with open(jsonl_file, encoding="utf-8", errors="replace") as f:
                        for line in f:
                            msg = self._process_message_line(line, session_id, message_type)
                            if msg:
                                messages.append(msg)
                except OSError:
                    continue

        return messages

    def search_messages(  # noqa: C901
        self,
        query: str,
        message_type: Optional[str] = None,
        tool: Optional[str] = None,
        exclude_compaction: bool = False,
        since: Optional[str] = None,
        session_id: Optional[str] = None,
        fixed_strings: bool = False,
        after_index: int = 0,
        after_timestamp: Optional[str] = None,
        tool_use_only: bool = False,
        include_tool_result: bool = False,
    ) -> List[SessionMessage]:
        """Search for messages across all sessions.

        Args:
            query: Text or glob/regex pattern to search for in message content.
                   When tool is set, matches against serialized tool input JSON.
            message_type: Optional filter: 'user', 'assistant', or 'system'.
                          Defaults to 'assistant' when tool is set.
            tool: Optional tool name to filter by (e.g. 'Bash', 'Write', 'Edit').
                  When set, searches assistant messages for tool_use blocks with
                  this name. query is matched against the serialized tool input.
            fixed_strings: When True, treat query as literal string (no regex).
                           Consistent with grep -F / ripgrep -F.
            after_index: Skip the first N messages in each session before searching.
            after_timestamp: Skip messages before this timestamp within each session.
            tool_use_only: When True, only return messages that contain tool_use blocks.
                           Used by 'search tools' without --tool to exclude pure text messages.
            include_tool_result: When True and tool is set, attach tool output
                  data (stdout/stderr char/word counts) to each SessionMessage via
                  the tool_result field. Pairs tool_use with its toolUseResult by
                  matching sourceToolAssistantUUID to the assistant message uuid.

        Returns:
            List of matching SessionMessage objects. Empty list if projects_dir does not exist.
            When tool is set, SessionMessage.content holds json.dumps(tool_input).

        Raises:
            ValueError: If query is not a valid glob or regex.
        """
        # tool_use only appears in assistant messages
        if tool is not None and message_type is None:
            message_type = "assistant"

        # Allow empty query when filtering by tool (list all invocations)
        if fixed_strings:
            pattern = None
            is_literal = True
            query_lower = query.lower() if query else None
        else:
            pattern = self._compile_pattern(query) if query else None

        # Pre-compute literal pre-filter: only safe when query has no regex metacharacters.
        if not fixed_strings:
            is_literal = not query or re.escape(query) == query
            query_lower = query.lower() if (is_literal and query) else None
        # Heuristic: message_type value must appear in the raw JSON line (no false negatives).
        # "slash" and "compaction" are derived values (content-based), not literal JSONL fields —
        # they both come from "user" records, so pre-filter on "user" for those two.
        _RAW_TYPE_MAP = {"slash": "user", "compaction": "user"}
        _raw_type = _RAW_TYPE_MAP.get(message_type, message_type) if message_type else None
        msg_type_hint = f'"{_raw_type}"' if _raw_type else None
        tool_lower = tool.lower() if tool else None

        messages: List[SessionMessage] = []

        # When include_tool_result is True, track which JSONL files had matches
        # and the UUIDs we need to look up results for.
        _files_with_matches: list = []  # [(jsonl_file, start_idx_in_messages)]

        for _project_dir_name, jsonl_file, _ in self._iter_all_jsonl(
            session_id_prefix=session_id, since=since
        ):
            _file_start_idx = len(messages)
            try:
                with open(jsonl_file, encoding="utf-8", errors="replace") as f:
                    _msg_idx = 0
                    for line in f:
                        _msg_idx += 1
                        # Skip messages before after_index within each session
                        if after_index > 0 and _msg_idx <= after_index:
                            continue
                        # Skip messages before after_timestamp (fast raw-line check)
                        if after_timestamp:
                            _ts_m = _TS_EXTRACT_RE.search(line)
                            if _ts_m and _ts_m.group(1) < after_timestamp:
                                continue
                        try:
                            # Raw line pre-filters: skip json.loads when possible.
                            if query_lower and query_lower not in line.lower():
                                continue
                            if msg_type_hint and msg_type_hint not in line:
                                continue
                            # Tool pre-filter: tool name value must appear in raw line.
                            # Use f'"{tool}"' (value-only check) — robust against both
                            # json (space-separated) and orjson (no-space) serialization.
                            if tool and f'"{tool}"' not in line:
                                continue
                            data = _json_loads(line)
                            msg_type = data.get("type", "").lower()
                            content = self._extract_content(data)

                            # Extended type filter: slash and compaction are content-based
                            if message_type == "slash":
                                if msg_type != "user" or "<command-name>" not in (content or ""):
                                    continue
                                # Exclude compaction summaries that quote <command-name> from prior messages
                                if _is_compaction(data, content):
                                    continue
                            elif message_type == "compaction":
                                if not _is_compaction(data, content):
                                    continue
                            elif message_type:
                                if msg_type != message_type.lower():
                                    continue

                            if exclude_compaction and _is_compaction(data, content):
                                continue

                            if tool is not None:
                                # Tool filtering: scan content array DIRECTLY for tool_use blocks.
                                # IMPORTANT: _extract_content() only returns type=="text" blocks
                                # and would miss tool_use entries entirely — must NOT use it here.
                                msg_content = data.get("message", {}).get("content", [])
                                if not isinstance(msg_content, list):
                                    continue
                                for item in _iter_tool_use_blocks(msg_content):
                                    if item.get("name", "").lower() == tool_lower:
                                        # Serialize input for query matching + display
                                        input_str = json.dumps(item.get("input", {}))
                                        # Match query against tool input (literal or regex)
                                        if query_lower:
                                            matched = query_lower in input_str.lower()
                                        elif pattern:
                                            matched = bool(pattern.search(input_str))
                                        else:
                                            matched = True  # no query = match all
                                        if matched:
                                            # Store uuid for tool_result pairing
                                            _msg_uuid = data.get("uuid", "") if include_tool_result else ""
                                            messages.append(SessionMessage(
                                                type=self._parse_message_type(msg_type),
                                                timestamp=data.get("timestamp", ""),
                                                content=input_str,
                                                session_id=data.get("sessionId", ""),
                                                tool_result={"_uuid": _msg_uuid} if _msg_uuid else None,
                                            ))
                                            break  # one match per message line
                            else:
                                # tool_use_only: skip messages without tool_use blocks
                                if tool_use_only:
                                    msg_content = data.get("message", {}).get("content", [])
                                    if not isinstance(msg_content, list):
                                        continue
                                    if not any(True for _ in _iter_tool_use_blocks(msg_content)):
                                        continue
                                # content already extracted above for type/compaction filtering
                                if not content:
                                    continue
                                # Match query against content (literal or regex)
                                if query_lower:
                                    if query_lower not in content.lower():
                                        continue
                                elif pattern:
                                    if not pattern.search(content):
                                        continue
                                # else: no query = match all
                                messages.append(
                                        SessionMessage(
                                            type=self._parse_message_type(msg_type),
                                            timestamp=data.get("timestamp", ""),
                                            content=content,
                                            session_id=data.get("sessionId", ""),
                                        )
                                    )
                        except (json.JSONDecodeError, KeyError, ValueError):
                            continue
            except OSError:
                continue
            # Track files that produced new matches for the result-pairing pass
            if include_tool_result and tool is not None and len(messages) > _file_start_idx:
                _files_with_matches.append((jsonl_file, _file_start_idx, len(messages)))

        # ── Second pass: pair tool_use with toolUseResult ────────────────
        if include_tool_result and tool is not None and _files_with_matches:
            self._attach_tool_results(messages, _files_with_matches)

        return messages

    @staticmethod
    def _attach_tool_results(
        messages: List[SessionMessage],
        files_with_matches: list,
    ) -> None:
        """Pair tool_use messages with their toolUseResult output data.

        For each JSONL file that produced matches, scan for toolUseResult
        lines whose sourceToolAssistantUUID matches a matched message's uuid.
        Attaches stdout/stderr char and word counts to SessionMessage.tool_result.
        """
        for jsonl_file, start_idx, end_idx in files_with_matches:
            # Build lookup: uuid -> message index (scoped to this file's messages)
            uuid_to_idx: dict = {}
            for i in range(start_idx, end_idx):
                tr = messages[i].tool_result
                if tr and "_uuid" in tr:
                    uuid_to_idx[tr["_uuid"]] = i
            if not uuid_to_idx:
                continue

            try:
                with open(jsonl_file, encoding="utf-8", errors="replace") as f:
                    for line in f:
                        # Fast pre-filter: toolUseResult lines contain this key
                        if '"toolUseResult"' not in line and '"sourceToolAssistantUUID"' not in line:
                            continue
                        try:
                            data = _json_loads(line)
                            src_uuid = data.get("sourceToolAssistantUUID", "")
                            if src_uuid not in uuid_to_idx:
                                continue
                            tr = data.get("toolUseResult")
                            if not isinstance(tr, dict):
                                continue
                            stdout = tr.get("stdout", "")
                            stderr = tr.get("stderr", "")
                            if not isinstance(stdout, str):
                                stdout = ""
                            if not isinstance(stderr, str):
                                stderr = ""
                            combined = stdout + stderr
                            idx = uuid_to_idx.pop(src_uuid)
                            messages[idx].tool_result = {
                                "stdout_chars": len(stdout),
                                "stderr_chars": len(stderr),
                                "output_chars": len(combined),
                                "output_words": len(combined.split()),
                            }
                        except (json.JSONDecodeError, KeyError, ValueError):
                            continue
            except OSError:
                continue

            # Clean up any unmatched uuid placeholders
            for idx in uuid_to_idx.values():
                messages[idx].tool_result = None

    def get_sessions(
        self,
        project_filter: Optional[str] = None,
        since: Optional[str] = None,   # canonical; --after is a hidden alias
        until: Optional[str] = None,   # canonical; --before is a hidden alias
    ) -> List[SessionInfo]:
        """List all sessions with metadata, sorted newest-first.

        Args:
            project_filter: Substring to match against project_dir name. None = all projects.
            since:  Only sessions with timestamp_first >= this (ISO prefix, e.g. "2026-01-15").
            until:  Only sessions with timestamp_first <= this (ISO prefix, e.g. "2026-12-31").

        Returns:
            List of SessionInfo, sorted by timestamp_first descending (newest first).
        """
        sessions: List[SessionInfo] = []
        for project_dir_name, jsonl_file, _ in self._iter_all_jsonl(project_filter, since=since):
            session_id = jsonl_file.stem
            cwd, git_branch = "", ""
            ts_first, ts_last = "", ""
            message_count = 0
            has_compact = False
            for data in self._scan_jsonl(jsonl_file):
                ts = data.get("timestamp", "")
                if ts and not ts_first:
                    ts_first = ts
                if ts:
                    ts_last = ts
                if not cwd and data.get("cwd"):
                    cwd = data["cwd"]
                if not git_branch and data.get("gitBranch"):
                    git_branch = data["gitBranch"]
                if data.get("isCompactSummary"):
                    has_compact = True
                if data.get("type") in ("user", "assistant"):
                    message_count += 1
            # Fallback: use file mtime when no timestamp found in JSONL records
            # (e.g., empty sessions, system-only records, aborted sessions)
            if not ts_first:
                try:
                    mtime = jsonl_file.stat().st_mtime
                    ts_first = datetime.datetime.fromtimestamp(
                        mtime, tz=datetime.timezone.utc
                    ).strftime("%Y-%m-%dT%H:%M:%S")
                except OSError:
                    pass
            if not _passes_date_filter(ts_first, since, until):
                continue
            sessions.append(SessionInfo(
                session_id=session_id,
                project_dir=project_dir_name,
                cwd=cwd,
                git_branch=git_branch,
                timestamp_first=ts_first,
                timestamp_last=ts_last,
                message_count=message_count,
                has_compact_summary=has_compact,
                provider="claude",
            ))
        sessions.sort(key=lambda s: s.timestamp_first, reverse=True)
        return sessions

    def _scan_user_messages(
        self,
        since: Optional[str] = None,
        until: Optional[str] = None,
        project_filter: Optional[str] = None,
        session_id_prefix: Optional[str] = None,
        message_type: str = "user",
        raw_line_filter: Optional[Callable[[str], bool]] = None,
    ) -> Iterator[tuple]:
        """Yield (session_id, project_dir, timestamp, data, content) for messages.

        Handles: _iter_all_jsonl iteration, file open, raw type prefilter, json.loads,
        type check, date range filtering, _extract_content, empty content skip.

        Args:
            message_type: Message type to filter for. Default "user".
                          Pass None to yield all message types.
            raw_line_filter: Optional callable applied to each raw JSONL line BEFORE
                             json.loads. Return True to keep, False to skip. Used for
                             efficient pre-filtering (e.g. slash command detection).

        Callers apply their own domain-specific filters (skill injection, continuation
        summary, slash command detection, regex matching, etc.).
        """
        # Build raw type prefilter strings for the specified message_type
        _type_raw_a = f'"type":"{message_type}"' if message_type else None
        _type_raw_b = f'"type": "{message_type}"' if message_type else None

        for project_dir_name, jsonl_file, skip_until_check in self._iter_all_jsonl(
            project_filter, since=since, until=until,
            session_id_prefix=session_id_prefix,
        ):
            try:
                with open(jsonl_file, encoding="utf-8", errors="replace") as f:
                    for line in f:
                        # Raw type prefilter (skip json.loads on ~60-70% of lines)
                        if _type_raw_a is not None:
                            if _type_raw_a not in line and _type_raw_b not in line:
                                continue
                        # Optional caller-supplied raw line filter
                        if raw_line_filter is not None and not raw_line_filter(line):
                            continue
                        try:
                            data = _json_loads(line)
                            if message_type and data.get("type") != message_type:
                                continue
                            ts = data.get("timestamp", "")
                            if (since or until) and not ts:
                                continue
                            if since and ts and ts < since:
                                continue
                            if not skip_until_check and until and ts and ts > until:
                                continue
                            content = self._extract_content(data)
                            if not content:
                                continue
                            sid = data.get("sessionId", "")
                            yield (sid, project_dir_name, ts, data, content)
                        except (json.JSONDecodeError, KeyError, ValueError):
                            continue
            except OSError:
                continue

    def find_corrections(
        self,
        project_filter: Optional[str] = None,
        since: Optional[str] = None,   # canonical; --after is a hidden alias
        until: Optional[str] = None,   # canonical; --before is a hidden alias
        patterns: Optional[List[tuple]] = None,
        limit: int = 0,
        session_id_prefix: Optional[str] = None,
    ) -> List[CorrectionMatch]:
        """Find user messages where corrections were given to Claude.

        Args:
            project_filter: Substring to match project_dir. None = all projects.
            since:    Only messages >= this timestamp (ISO prefix).
            until:    Only messages <= this timestamp (ISO prefix).
            patterns: Override DEFAULT_CORRECTION_PATTERNS. Each tuple is
                      (category, [regex_keyword_strings]).
            limit:    Max results. Default: 0 (all results); pass a positive N to
                      cap. Matches the '0 = all' convention used across the engine.
                      The user-facing default is owned by AISession.find_corrections
                      (config 'defaults.limit', else all), which always passes an
                      explicit limit here.

        Returns:
            List of CorrectionMatch, sorted by timestamp descending.
        """
        _patterns = patterns or DEFAULT_CORRECTION_PATTERNS
        compiled = [
            (cat, re.compile("|".join(kws), re.IGNORECASE), kws)
            for cat, kws in _patterns
        ]
        results: List[CorrectionMatch] = []
        prev_was_slash = False
        for sid, project_dir_name, ts, data, content in self._scan_user_messages(
            since=since, until=until, project_filter=project_filter,
            session_id_prefix=session_id_prefix,
        ):
            if _is_continuation_summary(content):
                prev_was_slash = False
                continue
            is_slash = "<command-name>" in content
            if _is_skill_injection(data, content, prev_was_slash):
                prev_was_slash = False
                continue
            prev_was_slash = is_slash
            for cat, regex, _kws in compiled:
                m = regex.search(content)
                if m:
                    results.append(CorrectionMatch(
                        session_id=sid,
                        project_dir=project_dir_name,
                        timestamp=ts,
                        content=content,
                        category=cat,
                        matched_pattern=m.group(0),
                    ))
                    break
        results.sort(key=lambda x: x.timestamp, reverse=True)
        return results[:limit] if limit else results

    def analyze_planning_usage(
        self,
        commands: Optional[List[str]] = None,
        project_filter: Optional[str] = None,
        since: Optional[str] = None,   # canonical; --after is a hidden alias
        until: Optional[str] = None,   # canonical; --before is a hidden alias
        return_invocations: bool = False,
    ) -> "List[SlashCommandCount] | List[SlashCommandRecord]":
        """Count slash command usage across all sessions, sorted by frequency.

        Two modes depending on whether ``commands`` is provided:

        **Discovery mode** (``commands=None``, the default):
            Auto-discovers every slash command actually used — any user message whose
            content starts with ``/word`` (e.g. ``/commit``, ``/ar:plannew``, ``/help``).
            No configuration required; works on any Claude Code workspace.

        **Pattern mode** (``commands`` is a list of regex strings):
            Counts only the commands matching the supplied regex patterns.
            Useful when you want to track a specific set of commands and apply
            word-boundary anchors for precision.

        Args:
            commands:           Regex patterns to match (pattern mode). ``None`` = auto-discover.
            project_filter:     Substring to match project_dir. None = all projects.
            since:              Only messages >= this timestamp (ISO prefix).
            until:              Only messages <= this timestamp (ISO prefix).
            return_invocations: When True, return List[SlashCommandRecord] (individual records
                                sorted by timestamp) instead of List[PlanningCommandCount] (aggregated
                                counts). Discovery mode only — pattern mode is unchanged.

        Returns:
            List[PlanningCommandCount] sorted by count descending (default), or
            List[SlashCommandRecord] sorted by timestamp when return_invocations=True.
        """
        discovery_mode = commands is None
        # Pattern mode only: compile provided patterns (with ReDoS protection)
        compiled = []
        if not discovery_mode:
            for cmd in commands:  # type: ignore[union-attr]
                _check_redos_safe(cmd)
                compiled.append((cmd, re.compile(cmd, re.IGNORECASE)))
        counts: Dict[str, int] = defaultdict(int)
        session_ids_by_cmd: Dict[str, set] = defaultdict(set)
        project_dirs_by_cmd: Dict[str, set] = defaultdict(set)
        invocations_list: List = []

        # Use _scan_user_messages() with appropriate type and raw_line_filter.
        # Discovery mode: user messages only, with slash command raw prefilter.
        # Pattern mode: all message types, no raw prefilter.
        _msg_type = "user" if discovery_mode else None
        _raw_filter = (lambda line: bool(_SLASH_CMD_CONTENT_RE.search(line))) if discovery_mode else None

        for session_id, project_dir_name, ts, data, content in self._scan_user_messages(
            since=since, until=until,
            project_filter=project_filter,
            message_type=_msg_type,
            raw_line_filter=_raw_filter,
        ):
            if discovery_mode:
                # Match slash command: try bare "/command" at start,
                # then fall back to <command-name> XML tag (Claude Code format).
                m = _SLASH_CMD_DISCOVERY_RE.match(content.lstrip())
                cmd = m.group(0) if m else None
                args_text = ""
                if not cmd:
                    # Claude Code wraps invocations in XML tags
                    tag_m = _COMMAND_NAME_TAG_RE.search(content)
                    if tag_m:
                        cmd = tag_m.group(1)
                        # Extract args from <command-args> tag if present
                        args_m = re.search(
                            r"<command-args>(.*?)</command-args>",
                            content, re.DOTALL,
                        )
                        args_text = args_m.group(1).strip() if args_m else ""
                if cmd:
                    counts[cmd] += 1
                    session_ids_by_cmd[cmd].add(session_id)
                    project_dirs_by_cmd[cmd].add(project_dir_name)
                    if return_invocations:
                        args = args_text or content.lstrip()[len(cmd):].strip()
                        invocations_list.append(SlashCommandRecord(
                            command=cmd,
                            args=args,
                            session_id=session_id,
                            timestamp=ts,
                            project_dir=project_dir_name,
                            cwd=data.get("cwd", ""),
                            git_branch=data.get("gitBranch", ""),
                        ))
            else:
                for cmd, regex in compiled:
                    if regex.search(content):
                        counts[cmd] += 1
                        session_ids_by_cmd[cmd].add(session_id)
                        project_dirs_by_cmd[cmd].add(project_dir_name)
                        if return_invocations:
                            # Extract args from XML tag or from content after command
                            tag_m = _COMMAND_NAME_TAG_RE.search(content)
                            args_m = re.search(
                                r"<command-args>(.*?)</command-args>",
                                content, re.DOTALL,
                            )
                            inv_args = args_m.group(1).strip() if args_m else ""
                            inv_cmd = tag_m.group(1) if tag_m else cmd.rstrip(r"\b")
                            invocations_list.append(SlashCommandRecord(
                                command=inv_cmd,
                                args=inv_args,
                                session_id=session_id,
                                timestamp=ts,
                                project_dir=project_dir_name,
                                cwd=data.get("cwd", ""),
                                git_branch=data.get("gitBranch", ""),
                            ))
                        break  # one match per message
        if return_invocations:
            invocations_list.sort(key=lambda x: x.timestamp)
            return invocations_list
        if discovery_mode:
            result = [
                SlashCommandCount(
                    command=cmd,
                    count=counts[cmd],
                    session_ids=sorted(session_ids_by_cmd[cmd]),
                    project_dirs=sorted(project_dirs_by_cmd[cmd]),
                )
                for cmd in counts
            ]
        else:
            result = [
                SlashCommandCount(
                    # Normalize display name: strip trailing \b regex suffix
                    command=re.sub(r"\\b$", "", cmd),
                    count=counts[cmd],
                    session_ids=sorted(session_ids_by_cmd[cmd]),
                    project_dirs=sorted(project_dirs_by_cmd[cmd]),
                )
                for cmd in (commands or []) if counts[cmd] > 0
            ]
        result.sort(key=lambda x: x.count, reverse=True)
        return result

    def cross_reference_session(
        self,
        filename: str,
        current_content: str,
        session_id: Optional[str] = None,
        snippet_chars: int = 200,
    ) -> List[dict]:
        """Find Edit/Write calls for a file and check if their content appears in current_content.

        Args:
            filename:        Basename to match (e.g. "cli.py"). Matched against file_path ending.
            current_content: Current file content to compare against.
            session_id:      Session ID prefix to restrict search. None = all sessions.
            snippet_chars:   Characters to include in content_snippet. Default: 200.

        Returns:
            List of dicts sorted by timestamp ascending, each with keys:
                session_id (str), project_dir (str), timestamp (str),
                tool ("Edit"|"Write"), file_path (str),
                content_snippet (str, first snippet_chars chars), found_in_current (bool)
        """
        results: List[dict] = []
        for call in self._iter_file_tool_calls(filename, session_id=session_id,
                                                tools=("Edit", "Write")):
            inp = call["input"]
            snippet_src = inp.get("new_string") or inp.get("content", "")
            snippet = snippet_src[:snippet_chars]
            results.append({
                "session_id": call["session_id"],
                "project_dir": "",  # not available from _iter_file_tool_calls
                "timestamp": call["timestamp"],
                "tool": call["tool"],
                "file_path": call["file_path"],
                "content_snippet": snippet,
                "found_in_current": bool(snippet and snippet in current_content),
            })
        results.sort(key=lambda x: x["timestamp"])
        return results

    def _iter_file_tool_calls(
        self,
        filename: str,
        session_id: Optional[str] = None,
        tools: tuple = ("Edit", "Write", "NotebookEdit"),
    ) -> Iterator[dict]:
        """Yield tool call dicts for a file from JSONL, in file-scan order (NOT sorted).

        Each dict: {tool, file_path, timestamp, session_id, input: {…}}
        Reuses _iter_all_jsonl() for session/date filtering.
        Callers needing chronological order must sort by timestamp themselves.
        """
        for _, jsonl_file, _ in self._iter_all_jsonl(
            session_id_prefix=session_id,
        ):
            try:
                with open(jsonl_file, encoding="utf-8", errors="replace") as f:
                    for line in f:
                        if '"type":"assistant"' not in line and '"type": "assistant"' not in line:
                            continue
                        try:
                            data = _json_loads(line)
                            if data.get("type") != "assistant":
                                continue
                            sid = data.get("sessionId", "")
                            if session_id and not sid.startswith(session_id):
                                continue
                            for item in _iter_tool_use_blocks(
                                data.get("message", {}).get("content") or []
                            ):
                                tool_name = item.get("name", "")
                                if tool_name not in tools:
                                    continue
                                inp = item.get("input", {})
                                fp = inp.get("file_path", "")
                                if filename != "*" and Path(fp).name != filename:
                                    continue
                                yield {
                                    "tool": tool_name,
                                    "file_path": fp,
                                    "timestamp": data.get("timestamp", ""),
                                    "session_id": sid,
                                    "input": inp,
                                }
                        except (json.JSONDecodeError, KeyError, ValueError):
                            continue
            except OSError:
                continue

    def reconstruct_from_edits(
        self,
        filename: str,
        session_id: Optional[str] = None,
        base_content: Optional[str] = None,
    ) -> Optional[tuple]:
        """Reconstruct a file by replaying Edit/Write tool calls from session JSONL.

        Returns (reconstructed_content, applied_edits_list) or None if no edits found.
        """
        calls = sorted(
            self._iter_file_tool_calls(filename, session_id=session_id),
            key=lambda x: x["timestamp"],
        )
        if not calls:
            return None

        # Build edit sequence
        edits: list = []
        for call in calls:
            inp = call["input"]
            if call["tool"] == "Write":
                edits.append({"tool": "Write", "content": inp.get("content", ""),
                              "timestamp": call["timestamp"], "session_id": call["session_id"]})
            elif call["tool"] in ("Edit", "NotebookEdit"):
                old = inp.get("old_string", "")
                new = inp.get("new_string", "")
                if old or new:
                    edits.append({"tool": "Edit", "old_string": old, "new_string": new,
                                  "replace_all": inp.get("replace_all", False),
                                  "timestamp": call["timestamp"], "session_id": call["session_id"]})

        if not edits:
            return None

        # Determine base content and edit start index
        content = base_content
        base_edit_idx = 0
        if content is None:
            # Strategy 1: Use most recent Write as base, apply only edits AFTER it
            for i in range(len(edits) - 1, -1, -1):
                if edits[i]["tool"] == "Write":
                    content = edits[i]["content"]
                    base_edit_idx = i + 1
                    break
        if content is None:
            # Strategy 2: git show HEAD:<path>
            original_path = self.get_original_path(filename)
            if original_path:
                try:
                    import subprocess
                    result = subprocess.run(
                        ["git", "show", f"HEAD:{original_path}"],
                        capture_output=True, text=True, timeout=5,
                    )
                    if result.returncode == 0:
                        content = result.stdout
                except (subprocess.TimeoutExpired, FileNotFoundError):
                    pass
        if content is None:
            # Strategy 3: Read current file from disk
            original_path = self.get_original_path(filename)
            if original_path and Path(original_path).exists():
                content = Path(original_path).read_text(errors="replace")
        if content is None:
            return None

        # Apply edits sequentially, starting from base_edit_idx
        applied = []
        for edit in edits[base_edit_idx:]:
            if edit["tool"] == "Write":
                content = edit["content"]
                applied.append(edit)
            elif edit["tool"] == "Edit":
                if edit["old_string"] in content:
                    if edit.get("replace_all"):
                        content = content.replace(edit["old_string"], edit["new_string"])
                    else:
                        content = content.replace(edit["old_string"], edit["new_string"], 1)
                    applied.append(edit)

        return (content, applied)

    def export_session_markdown(self, session_id: str) -> str:
        """Export one session's messages as a markdown string.

        Args:
            session_id: Session ID prefix to match. Must match exactly one session.

        Returns:
            Markdown string with metadata header + all non-system messages.

        Raises:
            ValueError: If no session matches, or multiple sessions match.
        """
        matches = self._find_session_files(session_id)
        if not matches:
            raise ValueError(f"No session found matching: {session_id!r}")
        if len(matches) > 1:
            candidates = ", ".join(m[0].stem[:16] for m in matches)
            raise ValueError(
                f"Ambiguous session ID {session_id!r} matches {len(matches)} sessions: {candidates}"
            )
        session_file, _project_dir_name = matches[0]

        # Scan file for metadata + messages
        cwd, git_branch, ts_first = "", "", ""
        message_count = 0
        lines_md: List[str] = []

        for data in self._scan_jsonl(session_file):
            ts = data.get("timestamp", "")
            if ts and not ts_first:
                ts_first = ts
            if not cwd and data.get("cwd"):
                cwd = data["cwd"]
            if not git_branch and data.get("gitBranch"):
                git_branch = data["gitBranch"]
            msg_type = data.get("type", "")
            if msg_type not in ("user", "assistant"):
                continue
            is_summary = data.get("isCompactSummary", False)
            content = self._extract_content(data)
            # Filter system noise — count only messages that are actually rendered
            if any(pat in content for pat in _EXPORT_FILTER_PATTERNS):
                continue
            if not content.strip():
                continue
            message_count += 1
            ts_short = ts[:16].replace("T", " ") if ts else "\u2014"
            if is_summary:
                lines_md.append(f"## Session Summary\n\n{content}\n\n---\n")
            else:
                lines_md.append(f"## [{msg_type}] {ts_short}\n\n{content}\n\n---\n")

        short_id = session_id[:8]
        header = (
            f"# Session {short_id}\n\n"
            f"**Date**: {ts_first}\n"
            f"**Branch**: {git_branch}\n"
            f"**Directory**: {cwd}\n"
            f"**Messages**: {message_count}\n\n"
            f"---\n\n"
        )
        return header + "\n".join(lines_md)

    # Regex for cat-to-pbcopy heredoc pattern (compiled once, module-level is not possible
    # because this is a class method — kept here as a class-level constant for efficiency).
    _PBCOPY_RE = re.compile(
        r"cat\s+<<['\"]?EOF['\"]?\s*\|\s*pbcopy\s*\n(.*?)\nEOF",
        re.DOTALL | re.IGNORECASE,
    )

    def get_clipboard_content(self, session_id: str) -> List[dict]:
        """Extract text content that was piped to clipboard (pbcopy) in a session.

        Scans Bash tool_use blocks for cat-to-pbcopy heredoc patterns::

            cat <<'EOF' | pbcopy
            ... content ...
            EOF

        Args:
            session_id: Session ID prefix to match. Uses newest match when ambiguous.

        Returns:
            List of dicts with keys: ``timestamp`` (str), ``content`` (str).
            Empty list if session not found or no pbcopy calls.
        """
        matches = self._find_session_files(session_id)
        if not matches:
            return []
        session_file, _project_dir_name = matches[0]
        results: List[dict] = []
        for data in self._scan_jsonl(session_file):
            if data.get("type") != "assistant":
                continue
            msg_content = data.get("message", {}).get("content", [])
            if not isinstance(msg_content, list):
                continue
            for item in msg_content:
                if not isinstance(item, dict) or item.get("type") != "tool_use":
                    continue
                if item.get("name") != "Bash":
                    continue
                cmd = item.get("input", {}).get("command", "")
                m = self._PBCOPY_RE.search(cmd)
                if m:
                    results.append({
                        "timestamp": data.get("timestamp", ""),
                        "content": m.group(1),
                    })
        return results

    def analyze_session(self, session_id: str) -> Optional[SessionAnalysis]:
        """Return per-session statistics: message counts, tool usage, and files touched.

        Files touched are detected by scanning every ``tool_use`` block for an input
        key named ``file_path`` — this is not hardcoded to Edit/Write/Read, so it
        captures any tool that operates on files, including future or custom tools.

        Args:
            session_id: Session ID prefix. Must match exactly one session JSONL file.

        Returns:
            SessionAnalysis, or None if no matching session is found.
        """
        matches = self._find_session_files(session_id)
        if not matches:
            return None
        session_file, project_dir_name = matches[0]  # newest first on ambiguous prefix

        total_lines = 0
        user_count = 0
        assistant_count = 0
        tool_uses_by_name: Dict[str, int] = {}
        files_touched_set: Set[str] = set()
        ts_first = ""
        ts_last = ""

        for data in self._scan_jsonl(session_file):
            total_lines += 1
            ts = data.get("timestamp", "")
            if ts and not ts_first:
                ts_first = ts
            if ts:
                ts_last = ts
            msg_type = data.get("type", "")
            if msg_type == "user":
                user_count += 1
            elif msg_type == "assistant":
                assistant_count += 1
                # Scan tool_use blocks — general: detect any tool with file_path input
                msg_content = data.get("message", {}).get("content", [])
                if isinstance(msg_content, list):
                    for item in msg_content:
                        if not isinstance(item, dict) or item.get("type") != "tool_use":
                            continue
                        tool_name = item.get("name", "unknown")
                        tool_uses_by_name[tool_name] = tool_uses_by_name.get(tool_name, 0) + 1
                        inp = item.get("input", {})
                        if isinstance(inp, dict) and "file_path" in inp:
                            fp = inp["file_path"]
                            if fp:
                                files_touched_set.add(fp)

        return SessionAnalysis(
            session_id=session_id,
            project_dir=project_dir_name,
            total_lines=total_lines,
            user_count=user_count,
            assistant_count=assistant_count,
            tool_uses_by_name=tool_uses_by_name,
            files_touched=sorted(files_touched_set),
            timestamp_first=ts_first,
            timestamp_last=ts_last,
        )

    def timeline_session(
        self,
        session_id: str,
        preview_chars: int = 150,
    ) -> List[dict]:
        """Return a chronological timeline of user/assistant events for one session.

        Each event includes the message type, timestamp, a content preview,
        and the number of tool_use blocks invoked (for assistant messages).

        Args:
            session_id:    Session ID prefix. Must match exactly one JSONL file.
            preview_chars: Maximum characters to include in content_preview. Default: 150.

        Returns:
            List of dicts sorted by timestamp ascending, each with keys:
                type (str), timestamp (str), content_preview (str, ≤preview_chars),
                tool_count (int)
        """
        matches = self._find_session_files(session_id)
        if not matches:
            return []
        session_file, _project_dir_name = matches[0]  # newest first on ambiguous prefix

        events: List[dict] = []
        for data in self._scan_jsonl(session_file):
            msg_type = data.get("type", "")
            if msg_type not in ("user", "assistant"):
                continue
            ts = data.get("timestamp", "")
            content = self._extract_content(data)
            tool_count = 0
            if msg_type == "assistant":
                msg_content = data.get("message", {}).get("content", [])
                if isinstance(msg_content, list):
                    tool_count = sum(1 for _ in _iter_tool_use_blocks(msg_content))
            events.append({
                "type": msg_type,
                "timestamp": ts,
                "content_preview": content[:preview_chars],
                "tool_count": tool_count,
            })
        return events

    def search_messages_with_context(
        self,
        query: str,
        context: int = 3,
        context_before: int = -1,
        context_after: int = -1,
        message_type: Optional[str] = None,
        tool: Optional[str] = None,
        exclude_compaction: bool = False,
        since: Optional[str] = None,
        session_id: Optional[str] = None,
        fixed_strings: bool = False,
        skip_injection: bool = True,
        after_index: int = 0,
        after_timestamp: Optional[str] = None,
    ) -> List[ContextMatch]:
        """Search messages and return each match with surrounding context messages.

        Unlike ``search_messages``, this buffers each JSONL file in memory to
        retrieve up to ``context`` messages before and after each match from the
        same session file.

        Args:
            query:        Text or glob/regex pattern to search for.
            context:      Number of messages before AND after each match to include.
            message_type: Optional filter: 'user', 'assistant', or 'system'.
            tool:         Optional tool name filter (same semantics as search_messages).
            fixed_strings: When True, treat query as literal (no regex). Consistent with grep -F.
            skip_injection: Exclude skill injection messages (SKILL.md content) from
                context windows. Default True — injections are noise for auditing.
            after_index:  Skip the first N messages in each session before matching.
            after_timestamp: Skip messages before this timestamp within each session.

        Returns:
            List of ContextMatch, one per matching message (not deduplicated).
        """
        if tool is not None and message_type is None:
            message_type = "assistant"

        if fixed_strings:
            pattern = None
        else:
            pattern = self._compile_pattern(query) if query else None
        tool_lower = tool.lower() if tool else None

        results: List[ContextMatch] = []

        for _project_dir_name, jsonl_file, _ in self._iter_all_jsonl(
            session_id_prefix=session_id, since=since
        ):
            # Buffer ALL messages from this file (needed for context window).
            # NOTE: message_type filter is intentionally NOT applied here — the buffer
            # must include all adjacent messages so context windows can span different
            # message types (e.g., context_before for a user match includes assistant msgs).
            # The message_type filter is applied below when checking match candidates.
            all_msgs: List[SessionMessage] = []
            try:
                with open(jsonl_file, encoding="utf-8", errors="replace") as f:
                    for line in f:
                        try:
                            data = _json_loads(line)
                            msg_type_raw = data.get("type", "").lower()
                            if msg_type_raw not in ("user", "assistant", "system"):
                                continue
                            if tool is not None:
                                msg_content = data.get("message", {}).get("content", [])
                                if not isinstance(msg_content, list):
                                    continue
                                for item in _iter_tool_use_blocks(msg_content):
                                    if item.get("name", "").lower() == tool_lower:
                                        input_str = json.dumps(item.get("input", {}))
                                        all_msgs.append(SessionMessage(
                                            type=self._parse_message_type(msg_type_raw),
                                            timestamp=data.get("timestamp", ""),
                                            content=input_str,
                                            session_id=data.get("sessionId", ""),
                                        ))
                                        break
                            else:
                                content = self._extract_content(data)
                                if content:
                                    all_msgs.append(SessionMessage(
                                        type=self._parse_message_type(msg_type_raw),
                                        timestamp=data.get("timestamp", ""),
                                        content=content,
                                        session_id=data.get("sessionId", ""),
                                    ))
                        except (json.JSONDecodeError, KeyError, ValueError):
                            continue
            except OSError:
                continue

            # Find matches and collect context windows.
            # message_type filter applied here (not in buffer loop) so context windows
            # include chronologically adjacent messages of any type.
            nb = context_before if context_before >= 0 else context
            na = context_after if context_after >= 0 else context
            for i, msg in enumerate(all_msgs):
                # Skip messages before after_index/after_timestamp thresholds
                if after_index > 0 and (i + 1) <= after_index:
                    continue
                if after_timestamp and msg.timestamp and msg.timestamp < after_timestamp:
                    continue
                msg_type_str = msg.type.value if hasattr(msg.type, "value") else str(msg.type)
                # Extended type filter: slash/compaction are content-based
                if message_type == "slash":
                    if msg_type_str != "user" or "<command-name>" not in (msg.content or ""):
                        continue
                    # Exclude compaction summaries that quote <command-name> tags
                    if (msg.content or "").startswith("This session is being continued"):
                        continue
                elif message_type == "compaction":
                    if not (msg.content or "").startswith("This session is being continued"):
                        continue
                elif message_type:
                    if msg_type_str != message_type.lower():
                        continue
                if exclude_compaction and (msg.content or "").startswith("This session is being continued"):
                    continue
                # Match content against query: regex (default) or literal (-F)
                content_text = msg.content or ""
                if fixed_strings:
                    matched = not query or query.lower() in content_text.lower()
                else:
                    matched = not pattern or pattern.search(content_text)
                if matched:
                    ctx_before = all_msgs[max(0, i - nb): i]
                    raw_after = all_msgs[i + 1: i + 1 + na + (5 if skip_injection and na > 0 else 0)]
                    if skip_injection and na > 0:
                        # Filter out skill injections from context_after.
                        # The match itself might be a slash command, making the next
                        # user message a potential injection.
                        is_slash = "<command-name>" in (msg.content or "")
                        filtered = []
                        prev_slash = is_slash
                        for am in raw_after:
                            if _is_skill_injection(
                                {"type": am.type.value if hasattr(am.type, "value") else str(am.type)},
                                am.content, prev_slash
                            ):
                                prev_slash = False
                                continue
                            prev_slash = "<command-name>" in (am.content or "")
                            filtered.append(am)
                            if len(filtered) >= na:
                                break
                        ctx_after = filtered
                    else:
                        ctx_after = raw_after[:na]
                    results.append(ContextMatch(
                        match=msg,
                        context_before=ctx_before,
                        context_after=ctx_after,
                    ))

        return results

    def get_statistics(
        self,
        since: Optional[str] = None,   # canonical; --after is a hidden alias
        until: Optional[str] = None,   # canonical; --before is a hidden alias
    ) -> SessionStatistics:
        """Get recovery statistics. Defaults to all sessions (no date restriction).

        When since/until given, counts only sessions whose timestamp_first is in range.
        Delegates to get_sessions() for date-aware counting — no filter logic duplicated.
        """
        if since or until:
            # Date filter active: scan JSONL for timestamps via get_sessions()
            filtered = self.get_sessions(since=since, until=until)
            total_sessions = len(filtered)
            filtered_ids: Optional[frozenset] = frozenset(s.session_id for s in filtered)
        else:
            # No filter: fast path (file enumeration only, no JSONL content read)
            total_sessions = sum(1 for _ in self._iter_all_jsonl())
            filtered_ids = None   # None → include all session dirs below

        total_files = 0
        total_versions = 0
        largest_file = None
        largest_edits = 0

        if self.recovery_dir.exists():
            # File count: recovery_dir/session_<session_id>/ (excludes all_versions dirs)
            for session_dir in self.recovery_dir.glob("session_*"):
                if not session_dir.is_dir() or "all_versions" in session_dir.name:
                    continue
                sid = session_dir.name.removeprefix("session_")
                if filtered_ids is not None and sid not in filtered_ids:
                    continue   # O(1) frozenset lookup
                for file_path in session_dir.glob("*"):
                    if file_path.is_file():
                        total_files += 1

            # Version count: recovery_dir/session_all_versions_<session_id>/
            file_version_totals: Dict[str, int] = defaultdict(int)
            for session_dir in self._version_dirs:
                # _version_dirs: dirs named "session_all_versions_<session_id>"
                sid = session_dir.name.removeprefix("session_all_versions_")
                if filtered_ids is not None and sid not in filtered_ids:
                    continue   # O(1) frozenset lookup
                version_files = list(session_dir.glob("*_v*_line_*.txt"))
                total_versions += len(version_files)
                for v_file in version_files:
                    filename = self._extract_filename(v_file.name)
                    file_version_totals[filename] += 1

            # Find globally largest file after accumulating all sessions
            for filename, edits in file_version_totals.items():
                if edits > largest_edits:
                    largest_edits = edits
                    largest_file = filename

        return SessionStatistics(
            total_sessions=total_sessions,
            total_files=total_files,
            total_versions=total_versions,
            largest_file=largest_file,
            largest_file_edits=largest_edits,
        )

    @staticmethod
    def _extract_filename(version_filename: str) -> str:
        """Extract original filename from versioned name (e.g. 'cli.py_v000001_line_10.txt' → 'cli.py')."""
        match = re.match(r"(.+?)_v\d+_line_\d+\.txt$", version_filename)
        return match.group(1) if match else version_filename

    @staticmethod
    def _extract_content(data: dict) -> str:
        """Extract text content from a JSONL message record.

        Handles three message formats:
          - dict with content list of {type, text} blocks (multi-part messages)
          - dict with content string (simple messages)
          - raw string message
        """
        if "message" not in data:
            return ""

        msg = data["message"]
        if isinstance(msg, dict):
            content_data = msg.get("content", "")
            if isinstance(content_data, list):
                return " ".join(
                    item.get("text", "")
                    for item in content_data
                    if isinstance(item, dict) and item.get("type") == "text"
                )
            if isinstance(content_data, str):
                return content_data
        elif isinstance(msg, str):
            return msg

        return ""

    def get_original_path(self, filename: str) -> Optional[str]:  # noqa: C901
        """Find the most recent original path where Claude wrote or edited this filename.

        Searches project JSONL files for Write/Edit/NotebookEdit tool calls and
        toolUseResult confirmation messages.  Returns the last recorded absolute path,
        or None if not found.

        Args:
            filename: Exact filename to look up (e.g. 'cli.py')

        Returns:
            Most recently recorded absolute path string, or None.
        """
        if not self.projects_dir.exists():
            return None

        last_path: Optional[str] = None

        for project_dir in self.projects_dir.iterdir():
            if not project_dir.is_dir():
                continue
            for jsonl_file in project_dir.glob("*.jsonl"):
                try:
                    with open(jsonl_file, encoding="utf-8", errors="replace") as f:
                        for line in f:
                            # Fast pre-filter: filename must appear in the raw line.
                            if filename not in line:
                                continue
                            try:
                                data = _json_loads(line)
                                # Path 1: toolUseResult.filePath (user confirmation message)
                                tool_result = data.get("toolUseResult") or {}
                                if isinstance(tool_result, dict):
                                    fp = tool_result.get("filePath", "")
                                    if fp and Path(fp).name == filename:
                                        last_path = fp
                                        continue
                                # Path 2: message.content[].input.file_path (assistant tool_use)
                                msg = data.get("message") or {}
                                if isinstance(msg, dict):
                                    for item in _iter_tool_use_blocks(msg.get("content") or []):
                                        if item.get("name") in ("Write", "Edit", "NotebookEdit"):
                                            fp = (item.get("input") or {}).get("file_path", "")
                                            if fp and Path(fp).name == filename:
                                                last_path = fp
                            except (json.JSONDecodeError, KeyError, ValueError):
                                continue
                except OSError:
                    continue

        return last_path


# ── Multi-source engine ──────────────────────────────────────────────────────


class MultiSourceEngine:
    """Composable engine: delegates to multiple Storage backends simultaneously.

    Adding a new format = new Storage impl, zero changes to search/filter/CLI layers.
    """

    def __init__(self, sources: list) -> None:
        self._sources = sources

    def search_messages(self, query: str, message_type: str | None = None) -> list:
        """Aggregate search across ALL sources. Unified signature with SessionRecoveryEngine."""
        import warnings
        results = []
        for source in self._sources:
            try:
                results.extend(source.search_messages(query, message_type))
            except Exception as exc:  # noqa: BLE001
                warnings.warn(
                    f"MultiSourceEngine.search_messages: source {type(source).__name__} "
                    f"raised {type(exc).__name__}: {exc}",
                    stacklevel=2,
                )
        return sorted(results, key=lambda m: m.timestamp or "", reverse=True)

    def list_sessions(self) -> list:
        """List all sessions from all sources, sorted newest-first.

        ISO 8601 strings are lexicographically sortable, so string comparison
        is correct for all known timestamp formats. Sessions with no timestamp
        (empty string) sort to the end (oldest).
        """
        import warnings
        all_sessions = []
        for source in self._sources:
            try:
                all_sessions.extend(source.list_sessions())
            except Exception as exc:  # noqa: BLE001
                warnings.warn(
                    f"MultiSourceEngine.list_sessions: source {type(source).__name__} "
                    f"raised {type(exc).__name__}: {exc}",
                    stacklevel=2,
                )
        all_sessions.sort(key=lambda s: s.timestamp_first or "", reverse=True)
        return all_sessions

    def read_session(self, session_info) -> list:
        """Read messages for a session (delegates to first source that has it)."""
        import warnings
        for source in self._sources:
            try:
                msgs = source.read_session(session_info)
                if msgs:
                    return msgs
            except Exception as exc:  # noqa: BLE001
                warnings.warn(
                    f"MultiSourceEngine.read_session: source {type(source).__name__} "
                    f"raised {type(exc).__name__}: {exc}",
                    stacklevel=2,
                )
        return []

    def stats(self) -> dict:
        """Aggregate stats across all sources."""
        import warnings
        total: dict = {}
        for source in self._sources:
            try:
                for k, v in source.stats().items():
                    total[k] = total.get(k, 0) + v
            except Exception as exc:  # noqa: BLE001
                warnings.warn(
                    f"MultiSourceEngine.stats: source {type(source).__name__} "
                    f"raised {type(exc).__name__}: {exc}",
                    stacklevel=2,
                )
        return total


class ClaudeSource:
    """Adapter: wraps SessionRecoveryEngine so Claude participates in MultiSourceEngine.

    Provides the same interface as AiStudioSource/GeminiCliSource so that
    MultiSourceEngine can aggregate Claude sessions alongside other providers.
    """

    def __init__(self, engine: "SessionRecoveryEngine") -> None:
        self._engine = engine

    def search_messages(self, query: str, message_type: str | None = None) -> list:
        return self._engine.search_messages(query, message_type)

    def list_sessions(self) -> list:
        return self._engine.get_sessions()

    def read_session(self, session_info) -> list:
        return self._engine.get_messages(session_info.session_id)

    def stats(self) -> dict:
        s = self._engine.get_statistics()
        return {"claude_sessions": s.total_sessions}


def _get_multi_engine(config: dict | None = None):
    """Internal factory: builds MultiSourceEngine from config. Not for library users — use AISession()."""
    import contextlib
    from pathlib import Path as _Path
    from ai_session_search.sources.aistudio import AiStudioSource, load_config
    from ai_session_search.sources.gemini_cli import GeminiCliSource

    if config is None:
        config = load_config()

    sources = []
    sd = config.get("source_dirs", {})

    if ai := sd.get("aistudio"):
        dirs = [ai] if isinstance(ai, str) else ai
        sources.append(AiStudioSource([_Path(p) for p in dirs]))

    if gc := sd.get("gemini_cli"):
        sources.append(GeminiCliSource(_Path(gc)))

    return MultiSourceEngine(sources)


def _aistudio_candidate_dirs(home: Path) -> list[Path]:
    """Return ordered list of candidate directories to probe for AI Studio sessions.

    Checks Downloads folder and Google Drive mount points across macOS, Linux,
    and Windows. Any directory whose name contains 'Google AI Studio' is included.
    Results are deduplicated while preserving discovery order.

    Candidates (in priority order):
    - ~/Downloads/Google AI Studio/                              (direct export)
    - ~/Downloads/*Google AI Studio*/                            (variant names)
    - ~/Downloads/drive-download-*/Google AI Studio/             (Drive bulk exports)
    - ~/Downloads/aistudio_sessions/Google AI Studio/            (custom subfolder)
    - ~/Downloads/aistudio_sessions/*Google AI Studio*/          (variant names)
    - macOS Google Drive: ~/Library/CloudStorage/GoogleDrive-*/My Drive/
    - macOS Google Drive legacy: ~/Google Drive/
    - Linux Google Drive: ~/GoogleDrive/, ~/google-drive/
    - Windows Google Drive: C:/Users/<user>/Google Drive/
    - Any of the above / *Google AI Studio*                      (glob within Drive)
    """
    import contextlib
    candidates: list[Path] = []
    seen: set[str] = set()

    def _add(p: Path) -> None:
        key = str(p.resolve()) if p.exists() else str(p)
        if key not in seen:
            seen.add(key)
            candidates.append(p)

    def _glob_aistudio(base: Path) -> None:
        """Add all *Google AI Studio* matches under base."""
        with contextlib.suppress(OSError):
            for d in sorted(base.glob("*Google AI Studio*")):
                if d.is_dir():
                    _add(d)

    downloads = home / "Downloads"

    # ── Downloads folder ────────────────────────────────────────────────────
    with contextlib.suppress(OSError):
        direct = downloads / "Google AI Studio"
        if direct.is_dir():
            _add(direct)
        _glob_aistudio(downloads)
        for drive_export in sorted(downloads.glob("drive-download-*")):
            if drive_export.is_dir():
                _glob_aistudio(drive_export)
        for subfolder in ("aistudio_sessions", "aistudio"):
            base = downloads / subfolder
            if base.is_dir():
                _glob_aistudio(base)

    # ── macOS Google Drive (modern: CloudStorage) ────────────────────────────
    cloud_storage = home / "Library" / "CloudStorage"
    with contextlib.suppress(OSError):
        for drive_mount in sorted(cloud_storage.glob("GoogleDrive-*")):
            if not drive_mount.is_dir():
                continue
            for sub in ("My Drive", "MyDrive", ""):
                root = drive_mount / sub if sub else drive_mount
                _glob_aistudio(root)

    # ── macOS Google Drive (legacy: ~/Google Drive/) ────────────────────────
    legacy_mac = home / "Google Drive"
    with contextlib.suppress(OSError):
        if legacy_mac.is_dir():
            _glob_aistudio(legacy_mac)

    # ── Linux Google Drive mount points ─────────────────────────────────────
    for linux_drive in ("GoogleDrive", "google-drive", "Google Drive"):
        candidate = home / linux_drive
        with contextlib.suppress(OSError):
            if candidate.is_dir():
                _glob_aistudio(candidate)

    # ── Windows Google Drive (C:\Users\<user>\Google Drive\) ─────────────────
    # Path.home() on Windows returns C:\Users\<user>, so ~/Google Drive works there too.
    # Additional Windows variant: OneDrive / Google Drive synced folder.
    for win_drive in ("Google Drive", "Google Drive (My Drive)"):
        candidate = home / win_drive
        with contextlib.suppress(OSError):
            if candidate.is_dir():
                _glob_aistudio(candidate)

    return candidates


# How long auto-discovered source paths are cached in config.json before a re-scan.
# Users can force an immediate refresh with: aise source scan
_DISCOVERY_TTL_SECONDS: int = 86400  # 24 hours


def _discover_sources(config: dict, force: bool = False) -> dict:
    """Scan standard install locations and merge discovered sources into config.

    Priority: explicit config > auto-discovered (cached or freshly scanned).
    Claude Code is always included via SessionRecoveryEngine (not in source_dirs).
    Returns effective config dict with discovered paths merged in.

    Caching (config.json["_auto_discovered"]):
    ─────────────────────────────────────────
    Auto-discovery runs filesystem globs which are O(entries in Downloads / Drive).
    Results are cached in config.json under "_auto_discovered" for
    _DISCOVERY_TTL_SECONDS (24 h). On cache hit the function is O(1).
    To force an immediate re-scan: aise source scan  (passes force=True).

    The cache stores only auto-discovered values — explicit source_dirs entries
    are never overwritten. Cache structure (written to config.json):
        {
          "_auto_discovered": {
            "_discovered_at": "2026-03-01T14:23:45+00:00",  # ISO 8601 UTC
            "gemini_cli": "/Users/you/.gemini/tmp",
            "aistudio":   ["/Users/you/Downloads/Google AI Studio"]
          }
        }

    Auto-discovery locations probed per provider:
    ── Claude Code  always included (SessionRecoveryEngine, no source_dirs entry)
    ── Gemini CLI   ~/.gemini/tmp/          (standard install, all platforms)
    ── AI Studio    ~/Downloads/*Google AI Studio*/
                    ~/Library/CloudStorage/GoogleDrive-*/My Drive/*Google AI Studio*/
                    ~/Google Drive/*Google AI Studio*/  (macOS legacy / Linux / Windows)

    To disable auto-discovery for a provider: aise source disable <type>
      (writes {"source_dirs": {"aistudio": []}} — empty list blocks auto-discovery)
    To add explicit paths:   aise source add <path> --type <type>
    To remove explicit path: aise source remove <path>
    To see config file path: aise config path
    To view config:          aise config show
    """
    explicit_sd = config.get("source_dirs", {})

    # ── Cache hit: use _auto_discovered if fresh ────────────────────────────
    if not force:
        auto = config.get("_auto_discovered", {})
        discovered_at_str = auto.get("_discovered_at", "")
        if discovered_at_str:
            with contextlib.suppress(ValueError, TypeError):
                ts = datetime.datetime.fromisoformat(discovered_at_str)
                if ts.tzinfo is None:
                    ts = ts.replace(tzinfo=datetime.timezone.utc)
                age = (datetime.datetime.now(datetime.timezone.utc) - ts).total_seconds()
                if age < _DISCOVERY_TTL_SECONDS:
                    # Merge cached auto-discovered values; explicit config always wins
                    effective = dict(config)
                    sd = dict(explicit_sd)
                    if not sd.get("gemini_cli") and auto.get("gemini_cli"):
                        sd["gemini_cli"] = auto["gemini_cli"]
                    if "aistudio" not in explicit_sd and auto.get("aistudio"):
                        sd["aistudio"] = auto["aistudio"]
                    effective["source_dirs"] = sd
                    return effective

    # ── Cache miss or forced: run full filesystem scan ───────────────────────
    home = Path.home()
    effective = dict(config)
    sd = dict(explicit_sd)

    # Gemini CLI: standard install at ~/.gemini/tmp/ (all platforms)
    if not sd.get("gemini_cli"):
        gemini_tmp = home / ".gemini" / "tmp"
        with contextlib.suppress(OSError):
            if gemini_tmp.exists() and any(gemini_tmp.iterdir()):
                sd["gemini_cli"] = str(gemini_tmp)

    # AI Studio: explicit config (incl. empty list) disables auto-discovery.
    if "aistudio" not in explicit_sd:
        candidates = _aistudio_candidate_dirs(home)
        found = [str(p) for p in candidates if p.exists()]
        if found:
            sd["aistudio"] = found

    # ── Persist cache entry back to config.json ──────────────────────────────
    new_auto: dict = {
        "_discovered_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    }
    # Only cache auto-discovered values (never copy explicit config into cache)
    if sd.get("gemini_cli") and not explicit_sd.get("gemini_cli"):
        new_auto["gemini_cli"] = sd["gemini_cli"]
    if sd.get("aistudio") and "aistudio" not in explicit_sd:
        new_auto["aistudio"] = sd["aistudio"]

    updated_config = dict(config)
    updated_config["_auto_discovered"] = new_auto
    # Only persist cache if this looks like an aise config (has source_dirs key)
    # AND the config file already exists on disk. Avoids creating config files
    # as a side effect of scanning, and avoids corrupting unrelated config files.
    with contextlib.suppress(Exception):  # never fail on write errors
        if "source_dirs" in config and get_config_path().exists():
            write_config(updated_config)

    effective["source_dirs"] = sd
    return effective


def _detect_default_source(cfg: dict) -> str:
    """Return 'all' if any non-Claude sources configured or discovered, else 'claude'.

    Callers should pass effective_cfg (the result of _discover_sources()) rather
    than the raw config so the TTL cache is hit on this second call, avoiding
    a redundant filesystem scan within the same process invocation.
    """
    effective_cfg = _discover_sources(cfg)
    sd = effective_cfg.get("source_dirs", {})
    if sd.get("aistudio") or sd.get("gemini_cli"):
        return "all"
    return "claude"


def _get_correction_patterns() -> list[tuple]:
    """Return DEFAULT_CORRECTION_PATTERNS merged with any user patterns from config.

    Reads ``config["correction_patterns"]`` as a list of [category, [keyword, ...]]
    entries and appends them to DEFAULT_CORRECTION_PATTERNS.  User patterns are
    appended after the defaults so the built-in categories always apply first.

    Returns:
        List of (category, [regex_keyword_strings]) tuples.
    """
    from .config import get_config_section
    user_patterns = get_config_section("correction_patterns", [])
    if not user_patterns or not isinstance(user_patterns, list):
        return list(DEFAULT_CORRECTION_PATTERNS)
    # Accept both list-of-tuples and list-of-lists from JSON config
    extra = []
    for item in user_patterns:
        if isinstance(item, (list, tuple)) and len(item) == 2:
            extra.append((str(item[0]), list(item[1])))
    return list(DEFAULT_CORRECTION_PATTERNS) + extra


def _engine_cfg_default(key: str, fallback: int) -> int:
    """Return config.defaults.<key> as int, or fallback if not set.

    Called by AISession methods to resolve configurable numeric defaults
    (e.g. limit=50, preview_chars=150). Reads from config ``defaults`` section:

        {
            "defaults": {
                "limit":          50,
                "preview_chars":  150,
                "snippet_chars":  200,
                "message_limit":  20
            }
        }

    Users can override any value in their config.json without changing code.
    """
    from .config import get_config_section
    defaults = get_config_section("defaults", {})
    if isinstance(defaults, dict) and key in defaults:
        try:
            return int(defaults[key])
        except (TypeError, ValueError):
            pass
    return fallback


class AISession:
    """Main entry point for ai_session_search.

    Auto-detects and connects to all configured AI session sources
    (Claude Code, AI Studio, Gemini CLI) on instantiation (RAII).

    The library is named ``ai_session_search``; the main class is ``AISession``.
    Both names share the same root: your AI sessions, all in one object.

    Example (RECOMMENDED — zero-config RAII)::

        import ai_session_search as aise

        # All of these are equivalent:
        session = aise.AISession()          # direct RAII — class name IS the concept
        session = aise.connect()            # convenience alias (connect = AISession)
        with aise.AISession() as session:   # context manager (recommended)

    Args:
        source:     Which source to use: "claude", "aistudio", "gemini", "all".
                    Default None → auto-detects based on configured source dirs.
        claude_dir: Override Claude projects directory (default: ~/.claude).
        config:     Pre-loaded config dict. Default None → loads from config.json.

    Supports multiple sources::

        aise.AISession(source="aistudio")   # AI Studio only
        aise.AISession(source="all")        # all configured sources
        aise.AISession()                    # auto-detect (recommended)
    """

    def __init__(
        self,
        source: str | None = None,
        claude_dir: str | None = None,
        config: dict | None = None,
    ) -> None:
        """Initialize and auto-connect. All arguments optional (RAII).

        Called as: AISession() or AISession(source="claude") or AISession(source="all").
        Delegates source detection to _build_ai_session() which uses _from_backend()
        to avoid re-entering __init__.
        """
        _sb = _build_ai_session(source=source, claude_dir=claude_dir, config=config)
        self._backend = _sb._backend
        self._source = _sb._source

    @classmethod
    def _from_backend(cls, backend: SessionRecoveryEngine | MultiSourceEngine,
                      source: str) -> AISession:
        """Internal: construct AISession from an already-built backend (no __init__ loop).

        Used by get_session_backend() to avoid recursive __init__ calls.
        """
        obj = cls.__new__(cls)
        obj._backend = backend
        obj._source = source
        return obj

    # ── Context manager protocol ──────────────────────────────────────────────

    def __enter__(self) -> AISession:
        """Support use as a context manager. Returns self.

        Enables::

            with AISession() as s:
                sessions = s.get_sessions(since="7d")

            with connect() as s:
                files = s.search_files("*.py")
        """
        return self

    def __exit__(
        self,
        exc_type: "type | None",
        exc_val: "BaseException | None",
        exc_tb: "object | None",
    ) -> None:
        """Context manager exit. No-op — AISession holds only in-memory caches.

        Future: may flush caches or close connection pools.
        """
        pass

    @property
    def source(self) -> str:
        return self._source

    @property
    def _claude_backend(self) -> "SessionRecoveryEngine | None":
        """Return the Claude backend if available, regardless of multi-source wrapping.

        When source="all", _backend is a MultiSourceEngine whose _sources list
        contains a ClaudeSource adapter (not SessionRecoveryEngine directly).
        ClaudeSource wraps SessionRecoveryEngine as self._engine. This property
        unwraps that chain to find the fast Claude backend — enabling Claude-
        specific fast paths (analyze_planning_usage with mtime + content
        pre-filters) even in multi-source mode.
        """
        if isinstance(self._backend, SessionRecoveryEngine):
            return self._backend
        if isinstance(self._backend, MultiSourceEngine):
            for src in self._backend._sources:
                if isinstance(src, SessionRecoveryEngine):
                    return src
                # ClaudeSource wraps SessionRecoveryEngine as _engine
                if isinstance(src, ClaudeSource):
                    return src._engine
        return None

    @property
    def _is_claude(self) -> bool:
        """True only when _backend is directly a SessionRecoveryEngine (not multi-source).

        Use _claude_backend instead when you need the Claude backend even inside
        a MultiSourceEngine (e.g. for Claude-only features like slash command analysis).
        Aggregate methods (get_sessions, search_messages) should use _is_claude because
        they need to return combined results from all sources when source='all'.
        """
        return isinstance(self._backend, SessionRecoveryEngine)

    def _warn_claude_only(self, feature: str) -> None:
        """Print graceful warning for Claude-only feature on non-Claude backend."""
        from rich.console import Console
        Console(stderr=True).print(
            f"[yellow]{feature} requires --provider claude (Claude Code sessions only)[/yellow]"
        )

    def _claude_only(self, method_name: str, default: object, *args: object, **kwargs: object) -> object:
        """Delegate to Claude backend or warn + return default. DRY pattern for all claude-only features."""
        be = self._claude_backend
        if be:
            return getattr(be, method_name)(*args, **kwargs)
        self._warn_claude_only(method_name.replace("_", " ").title())
        return default

    # ── Cross-backend operations (work for all sources) ──────────────────────

    def search_messages(
        self,
        query: str,
        *,
        context: int = 0,
        context_before: int = -1,
        context_after: int = -1,
        message_type: str | None = None,
        tool: str | None = None,
        exclude_compaction: bool = False,
        since: str | None = None,
        session_id: str | None = None,
        fixed_strings: bool = False,
        after_index: int = 0,
        after_timestamp: str | None = None,
        tool_use_only: bool = False,
        include_tool_result: bool = False,
    ) -> list[SessionMessage] | list[ContextMatch]:
        """Search messages across all configured AI session sources.

        Args:
            query:             Text or regex pattern to match in message content.
            context:           Symmetric surrounding messages per match (default 0).
                               When >0 (or context_before/after set), returns list[ContextMatch].
            context_before:    Override before-side of context window. -1 = use ``context``.
            context_after:     Override after-side of context window. -1 = use ``context``.
            message_type:      Filter by message type: user, assistant, tool, slash, compaction.
            tool:              (Claude-only) Filter to messages containing a specific tool call.
            exclude_compaction: Exclude compaction summary messages from results.
            since:             Only messages from sessions with mtime >= this ISO timestamp.
            session_id:        Scope to one session ID or prefix.
            fixed_strings:     When True, treat query as literal string (no regex). Consistent with grep -F.
            after_index:       Skip the first N messages in each session before searching.
            after_timestamp:   Skip messages before this timestamp within each session.
            tool_use_only:     Only return messages containing tool_use blocks (Claude-only).
            include_tool_result: (Claude-only) Attach tool output metadata (stdout/stderr
                               char/word counts) to each result. Off by default for performance.

        Returns:
            list[SessionMessage] when context=0 and no context_before/after (default).
            list[ContextMatch]   when context>0 or context_before/after are set.
        """
        use_context = context > 0 or context_before >= 0 or context_after >= 0
        claude_be = self._claude_backend

        # Collect non-Claude results when in multi-source mode.
        # Non-Claude sources don't support tool/since/session_id/exclude_compaction
        # so we only pass query + message_type.
        def _non_claude_results() -> list[SessionMessage]:
            if not isinstance(self._backend, MultiSourceEngine):
                return []
            results: list[SessionMessage] = []
            for src in self._backend._sources:
                if isinstance(src, ClaudeSource):
                    continue  # handled by claude_be
                try:
                    results.extend(src.search_messages(query, message_type))
                except Exception:  # noqa: BLE001
                    pass
            return results

        if use_context:
            if claude_be:
                claude_results = claude_be.search_messages_with_context(
                    query, context,
                    context_before=context_before,
                    context_after=context_after,
                    message_type=message_type,
                    tool=tool,
                    exclude_compaction=exclude_compaction,
                    since=since,
                    session_id=session_id,
                    fixed_strings=fixed_strings,
                    after_index=after_index,
                    after_timestamp=after_timestamp,
                )
                # Merge non-Claude results as ContextMatch (no context window available)
                non_claude = _non_claude_results()
                if non_claude:
                    claude_results.extend(
                        ContextMatch(match=m, context_before=[], context_after=[])
                        for m in non_claude
                    )
                return claude_results
            # Pure non-Claude: context window not available; wrap plain results as ContextMatch
            # so callers always receive list[ContextMatch] when context>0 (consistent API).
            if tool:
                from rich.console import Console as _Console
                _Console(stderr=True).print(
                    "[yellow]--tool filter not supported for non-Claude sources "
                    "(ignored when context>0)[/yellow]"
                )
            plain = self._backend.search_messages(query, message_type)
            return [ContextMatch(match=m, context_before=[], context_after=[]) for m in plain]
        if claude_be:
            claude_results = claude_be.search_messages(
                query, message_type, tool,
                exclude_compaction=exclude_compaction,
                since=since,
                session_id=session_id,
                fixed_strings=fixed_strings,
                after_index=after_index,
                after_timestamp=after_timestamp,
                tool_use_only=tool_use_only,
                include_tool_result=include_tool_result,
            )
            # Merge non-Claude results in multi-source mode
            non_claude = _non_claude_results()
            if non_claude:
                claude_results.extend(non_claude)
                claude_results.sort(key=lambda m: m.timestamp or "", reverse=True)
            return claude_results
        if tool:
            from rich.console import Console
            Console(stderr=True).print(
                "[yellow]--tool filter not supported for non-Claude sources[/yellow]"
            )
        return self._backend.search_messages(query, message_type)

    def get_sessions(self, project_filter: str | None = None,
                     since: str | None = None,   # canonical; after= is a hidden alias
                     until: str | None = None,   # canonical; before= is a hidden alias
                     ) -> list[SessionInfo]:
        """List sessions. Applies date filter for all backends via _passes_date_filter.

        In multi-source mode (source="all"): uses the fast Claude path
        (with mtime pre-filter) for Claude sessions and the generic path
        for non-Claude sources, then merges results.
        """
        if self._is_claude:
            return self._backend.get_sessions(project_filter, since, until)
        # Multi-source: use fast Claude path if available, generic for others
        claude_be = self._claude_backend
        if claude_be:
            # Fast path for Claude sessions (mtime pre-filter via _iter_all_jsonl)
            claude_sessions = claude_be.get_sessions(project_filter, since, until)
            # Generic path for non-Claude sources only
            non_claude = []
            for src in getattr(self._backend, "_sources", []):
                if isinstance(src, ClaudeSource):
                    continue  # already handled above
                try:
                    non_claude.extend(src.list_sessions())
                except Exception:  # noqa: BLE001
                    pass
            if since or until:
                non_claude = [
                    s for s in non_claude
                    if _passes_date_filter(s.timestamp_first, since, until)
                ]
            all_sessions = claude_sessions + non_claude
            all_sessions.sort(key=lambda s: s.timestamp_first or "", reverse=True)
            return all_sessions
        # Pure non-Claude: use generic list + filter
        sessions = self._backend.list_sessions()
        if since or until:
            sessions = [
                s for s in sessions
                if _passes_date_filter(s.timestamp_first, since, until)
            ]
        return sessions

    def get_messages(self, session_id: str,
                     message_type: str | None = None) -> list[SessionMessage]:
        """Get messages by session ID (substring match for non-Claude backends).

        Uses MultiSourceEngine.list_sessions() + read_session() public API — no private attribute access.
        """
        claude_be = self._claude_backend
        if claude_be:
            result = claude_be.get_messages(session_id, message_type)
            if result:
                return result
        if self._is_claude:
            return []  # Claude-only mode, session not found
        import contextlib
        found: list = []
        # Use public list_sessions() + read_session() — do NOT access _sources directly
        for si in self._backend.list_sessions():
            if session_id.lower() in si.session_id.lower():
                with contextlib.suppress(Exception):
                    msgs = self._backend.read_session(si)
                    if message_type:
                        msgs = [m for m in msgs if m.type.value == message_type]
                    found.extend(msgs)
                    break
        return found

    def get_statistics(self, since: str | None = None,   # canonical; after= is a hidden alias
                       until: str | None = None,         # canonical; before= is a hidden alias
                       ) -> SessionStatistics:
        """Return session statistics. Default since=None, until=None: no date restriction."""
        claude_be = self._claude_backend
        if claude_be and self._is_claude:
            # Direct Claude-only mode: fast path via SessionRecoveryEngine.get_statistics
            return claude_be.get_statistics(since=since, until=until)
        # Non-Claude or multi-source backends: aggregate stats.
        if since or until:
            sessions = self.get_sessions(since=since, until=until)
            per_source: dict[str, int] = {}
            for s in sessions:
                key = f"{s.provider}_sessions"
                per_source[key] = per_source.get(key, 0) + 1
            return SessionStatistics(total_sessions=len(sessions), per_source=per_source)
        raw = self._backend.stats()
        # MultiSourceEngine.stats() returns source-keyed ints (e.g. aistudio_sessions=1167).
        per_source = {k: v for k, v in raw.items() if isinstance(v, int)}
        total = sum(per_source.values())
        return SessionStatistics(total_sessions=total, per_source=per_source)

    # Alias for display helpers that called get_stats()
    get_stats = get_statistics

    # ── Cross-backend: latest session context ────────────────────────────────

    def get_latest_session_context(
        self,
        *,
        message_limit: int | None = None,
        project_filter: str | None = None,
    ) -> tuple[SessionInfo, list[SessionMessage]] | None:
        """Get the most recent session and its messages in a single call.

        THE most common use case: "What was I just working on?"

        Args:
            message_limit: Max messages to return. Default: config.defaults.message_limit
                           or 20 if not configured. 0 = all messages (no limit).
            project_filter: Optional project name substring to filter sessions.

        Returns:
            (SessionInfo, list[SessionMessage]) or None if no sessions exist.

        Example::

            context = session.get_latest_session_context()
            if context:
                info, messages = context
                print(f"Last session: {info.project_display}")
        """
        sessions = self.get_sessions(project_filter=project_filter)
        if not sessions:
            return None
        if message_limit is None:
            message_limit = _engine_cfg_default("message_limit", 20)
        latest = sessions[0]  # newest first
        messages = self.get_messages(latest.session_id)
        if message_limit and message_limit > 0:
            messages = messages[:message_limit]
        return (latest, messages)

    # ── Source introspection ─────────────────────────────────────────────────

    def get_sources(self) -> list[str]:
        """List active session source names.

        Returns:
            List of source names: e.g. ["claude"], ["aistudio", "gemini_cli"]

        Example::

            with AISession() as s:
                print(s.get_sources())   # ["claude"]
        """
        if self._is_claude:
            return ["claude"]
        # Hardcoded mapping avoids fragile string manipulation that could produce
        # wrong names for third-party source classes or classes whose name contains
        # "source" or "cli" in unexpected positions.
        _SOURCE_NAMES = {
            "AiStudioSource":  "aistudio",
            "GeminiCliSource": "gemini_cli",
            "ClaudeSource":    "claude",
        }
        sources = []
        for src in getattr(self._backend, "_sources", []):
            cls_name = type(src).__name__
            name = _SOURCE_NAMES.get(cls_name, cls_name.lower())
            sources.append(name)
        return sources or [self._source]

    # ── Claude-only operations — graceful degradation (using DRY _claude_only helper) ──

    def search_files(
        self,
        pattern: str,
        filters: FilterSpec | SearchFilter | None = None,
    ) -> list[SessionFile]:
        """Search recovered files matching glob pattern.

        Args:
            pattern: Glob pattern (e.g. "*.py", "src/**/*.ts")
            filters: Optional filter. Accepts:
                     - ``FilterSpec``: declarative spec (date/size/ext bounds).
                     - ``SearchFilter``: composable predicate chain (|/& operators).
                     - ``None``: no filtering.

        Returns:
            list[SessionFile] matching pattern and filters.

        Example::

            with AISession() as s:
                py = SearchFilter().by_extension("py")
                ts = SearchFilter().by_extension("ts")
                files = s.search_files("*", py | ts)
        """
        from .filters import SearchFilter as _SF
        from .models import FilterSpec as _FS
        if filters is None or isinstance(filters, _FS):
            # FilterSpec: pass directly to the engine's native search() which applies
            # pre-filters (extension, size, date) efficiently before returning results.
            return self._claude_only("search", [], pattern, filters)
        if callable(filters) and not isinstance(filters, type):
            # SearchFilter (or any custom callable): fetch unfiltered file list first,
            # then apply the callable predicate chain.  This path handles SearchFilter's
            # | (OR) and & (AND) operator compositions that cannot be expressed as a
            # declarative FilterSpec.
            all_files = self._claude_only("search", [], pattern, None)
            return list(filters(all_files))
        return self._claude_only("search", [], pattern, filters)

    def get_versions(self, filename: str) -> list[FileVersion]:
        return self._claude_only("get_versions", [], filename)

    def extract_final(self, filename: str, output_dir: Path) -> Path | None:
        return self._claude_only("extract_final", None, filename, output_dir)

    def extract_all(self, filename: str, output_dir: Path) -> list[Path]:
        return self._claude_only("extract_all", [], filename, output_dir)

    def find_corrections(
        self,
        *,
        since: str | None = None,
        until: str | None = None,
        project_filter: str | None = None,
        patterns: list | None = None,
        limit: int | None = None,
        session_id_prefix: str | None = None,
    ) -> list[CorrectionMatch]:
        """Find user messages where corrections were given to the AI.

        For Claude Code sessions: fast direct JSONL scan.
        For AI Studio / Gemini CLI sessions: uses get_sessions() + get_messages() API.
        Both return list[CorrectionMatch] sorted by timestamp descending.

        Args:
            since:          Only search sessions after this date.
            until:          Only search sessions before this date.
            project_filter: Filter to sessions in projects matching this substring.
            patterns:       Custom correction pattern list. None = use defaults + any
                            config ``correction_patterns`` additions.
            limit:          Max results to return. Default: config.defaults.limit
                            or 0 (all results). Pass a positive N to cap.

        Returns:
            list[CorrectionMatch] sorted by timestamp descending.
        """
        if limit is None:
            # Default to all matches; config 'defaults.limit' may override to cap.
            limit = _engine_cfg_default("limit", 0)
        # Merge user-supplied patterns from config with defaults (T58)
        _patterns = patterns or _get_correction_patterns()
        # Fast Claude path: direct JSONL scan with mtime pre-filter.
        # Works even in multi-source mode (source="all") since corrections
        # are detected from Claude JSONL records only.
        claude_be = self._claude_backend
        if claude_be:
            return claude_be.find_corrections(
                project_filter=project_filter,
                since=since,
                until=until,
                patterns=_patterns,
                limit=limit,
                session_id_prefix=session_id_prefix,
            )
        # Generic fallback: iterate sessions + messages
        compiled = [
            (cat, re.compile("|".join(kws), re.IGNORECASE), kws)
            for cat, kws in _patterns
        ]
        results: list = []
        sessions = self.get_sessions(
            project_filter=project_filter, since=since, until=until
        )
        for session_info in sessions:
            messages = self.get_messages(session_info.session_id, message_type="user")
            for msg in messages:
                if since and msg.timestamp and msg.timestamp < since:
                    continue
                if until and msg.timestamp and msg.timestamp > until:
                    continue
                content = msg.content or ""
                if not content:
                    continue
                for cat, regex, _kws in compiled:
                    m = regex.search(content)
                    if m:
                        results.append(CorrectionMatch(
                            session_id=session_info.session_id,
                            project_dir=getattr(session_info, "project_dir", ""),
                            timestamp=msg.timestamp,
                            content=content,
                            category=cat,
                            matched_pattern=m.group(0),
                        ))
                        break
        results.sort(key=lambda x: x.timestamp, reverse=True)
        return results[:limit] if limit else results

    def get_planning_usage(
        self,
        *,
        commands: list[str] | None = None,
        project_filter: str | None = None,
        since: str | None = None,
        until: str | None = None,
        limit: int | None = None,
        return_invocations: bool = False,
    ) -> "list[SlashCommandCount] | list[SlashCommandRecord]":
        """Count slash command usage across sessions.

        For Claude Code sessions: fast direct JSONL scan.
        For AI Studio / Gemini CLI sessions: uses get_sessions() + get_messages() API.
        Both return list[SlashCommandCount] sorted by count descending (default).

        Args:
            commands:           Filter to specific command strings. Default None = all.
            project_filter:     Filter to sessions in projects matching this substring.
            since:              Only count commands from sessions after this date.
            until:              Only count commands from sessions before this date.
            limit:              Max number of results. Default: config.defaults.limit or 50.
                                0 = all results.
            return_invocations: When True, return List[SlashCommandRecord] (individual records
                                sorted by timestamp) instead of aggregated counts.

        Returns:
            list[SlashCommandCount] sorted by count descending (default).
            list[SlashCommandRecord] sorted by timestamp when return_invocations=True.
        """
        if limit is None:
            limit = _engine_cfg_default("limit", 50)
        # Fast Claude path: direct JSONL scan with mtime + content pre-filters.
        # Works even in multi-source mode (source="all") since slash commands
        # are a Claude Code concept — AI Studio and Gemini CLI don't have them.
        claude_be = self._claude_backend
        if claude_be:
            results = claude_be.analyze_planning_usage(
                commands=commands,
                project_filter=project_filter,
                since=since,
                until=until,
                return_invocations=return_invocations,
            )
            return results[:limit] if limit else results
        # Generic fallback: iterate sessions + messages
        discovery_mode = commands is None
        compiled = (
            []
            if discovery_mode
            else [(cmd, re.compile(cmd, re.IGNORECASE)) for cmd in commands]  # type: ignore[union-attr]
        )
        from collections import defaultdict as _defaultdict
        counts: dict = _defaultdict(int)
        session_ids_by_cmd: dict = _defaultdict(set)
        project_dirs_by_cmd: dict = _defaultdict(set)
        sessions = self.get_sessions(
            project_filter=project_filter, since=since, until=until
        )
        for session_info in sessions:
            # Always fetch user messages only — slash commands appear in user messages.
            # Fetching all message types in pattern mode could match assistant messages
            # that *discuss* a command (e.g. "you can use /ar:plannew to…"), producing
            # false positives. Consistent with the Claude-backend path which pre-filters
            # to '"type":"user"' at the JSONL level before scanning for commands.
            messages = self.get_messages(
                session_info.session_id,
                message_type="user",
            )
            for msg in messages:
                if since and msg.timestamp and msg.timestamp < since:
                    continue
                if until and msg.timestamp and msg.timestamp > until:
                    continue
                content = msg.content or ""
                if not content:
                    continue
                if discovery_mode:
                    m = _SLASH_CMD_DISCOVERY_RE.match(content.lstrip())
                    if m:
                        cmd = m.group(0)
                        counts[cmd] += 1
                        session_ids_by_cmd[cmd].add(msg.session_id)
                        project_dirs_by_cmd[cmd].add(
                            getattr(session_info, "project_dir", "")
                        )
                else:
                    for cmd, regex in compiled:
                        if regex.search(content):
                            counts[cmd] += 1
                            session_ids_by_cmd[cmd].add(msg.session_id)
                            project_dirs_by_cmd[cmd].add(
                                getattr(session_info, "project_dir", "")
                            )
        if not counts:
            return []
        results = [
            SlashCommandCount(
                command=cmd,
                count=cnt,
                session_ids=sorted(session_ids_by_cmd[cmd]),
                project_dirs=sorted(project_dirs_by_cmd[cmd]),
            )
            for cmd, cnt in counts.items()
        ]
        results.sort(key=lambda x: x.count, reverse=True)
        return results[:limit] if limit else results

    def get_file_edits(
        self,
        filename: str,
        current_content: str | None = None,
        session_id: str | None = None,
        snippet_chars: int | None = None,
    ) -> list[dict]:
        """Find Edit/Write tool calls for a file across sessions. Claude-only.

        If current_content is provided, each result includes 'found_in_current'
        indicating whether that edit's content appears in current_content.

        Args:
            filename:        Filename to search for.
            current_content: Optional current file contents for diff matching.
            session_id:      Optional session ID to restrict search to.
            snippet_chars:   Max chars per snippet. Default: config.defaults.snippet_chars or 200.

        Returns:
            list[dict] with session_id, timestamp, snippet, found_in_current fields.
        """
        if snippet_chars is None:
            snippet_chars = _engine_cfg_default("snippet_chars", 200)
        return self._claude_only(
            "cross_reference_session", [],
            filename,
            current_content or "",
            session_id,
            snippet_chars,
        )

    def get_session_markdown(self, session_id: str) -> str:
        """Export session messages as a markdown string.

        For Claude Code sessions: full markdown including git branch, cwd metadata.
        For AI Studio / Gemini CLI sessions: markdown from message content (project display name used).

        Returns:
            Markdown string — caller handles writing to file.
            Returns "" if session not found.
        """
        claude_be = self._claude_backend
        if claude_be:
            result = claude_be.export_session_markdown(session_id)
            if result:
                return result
        # Generic fallback: build markdown from messages API
        messages = self.get_messages(session_id)
        if not messages:
            return ""
        # Look up session info for display name
        sessions = self.get_sessions()
        session_info = next(
            (s for s in sessions
             if s.session_id.startswith(session_id) or session_id.startswith(s.session_id[:8])),
            None,
        )
        ts_first = messages[0].timestamp if messages else ""
        project = session_info.project_display if session_info else session_id[:8]
        provider = getattr(session_info, "provider", self._source or "unknown")
        short_id = session_id[:8]
        header = (
            f"# Session {short_id}\n\n"
            f"**Date**: {ts_first}\n"
            f"**Project**: {project}\n"
            f"**Source**: {provider}\n"
            f"**Messages**: {len(messages)}\n\n"
            f"---\n\n"
        )
        lines_md = []
        for msg in messages:
            msg_type = msg.type.value if hasattr(msg.type, "value") else str(msg.type)
            if msg_type == "system":
                continue
            ts_short = msg.timestamp[:16].replace("T", " ") if msg.timestamp else "\u2014"
            lines_md.append(f"## [{msg_type}] {ts_short}\n\n{msg.content}\n\n---\n")
        return header + "\n".join(lines_md)

    def export_sessions_markdown(
        self,
        since: str | None = None,
        until: str | None = None,
        project_filter: str | None = None,
    ) -> list[str]:
        """Export multiple sessions as markdown strings.

        Works for all backends (Claude, AI Studio, Gemini CLI).
        Bulk version of get_session_markdown(). Returns one markdown string per
        session, newest-first.

        Args:
            since:          Only sessions after this date. Default: all.
            until:          Only sessions before this date. Default: all.
            project_filter: Optional project name substring filter.

        Returns:
            list[str]: One markdown string per session.
        """
        sessions = self.get_sessions(since=since, until=until, project_filter=project_filter)
        results = []
        for session in sessions:
            md = self.get_session_markdown(session.session_id)
            if md:
                results.append(md)
        return results

    def get_clipboard_content(self, session_id: str) -> list[dict]:
        return self._claude_only("get_clipboard_content", [], session_id)

    def get_session_analysis(self, session_id: str) -> SessionAnalysis | None:
        """Per-session statistics: message counts, tool usage, and files touched.

        For Claude Code sessions: full analysis including tool usage and files touched.
        For AI Studio / Gemini CLI sessions: message counts and timestamps only
        (tool_uses_by_name={}, files_touched=[] — not tracked by non-Claude backends).
        """
        claude_be = self._claude_backend
        if claude_be:
            return claude_be.analyze_session(session_id)
        # Generic fallback: compute stats from messages API
        messages = self.get_messages(session_id)
        if not messages:
            return None
        # Look up session info for project_dir
        sessions = self.get_sessions()
        session_info = next(
            (s for s in sessions
             if s.session_id.startswith(session_id) or session_id.startswith(s.session_id[:8])),
            None,
        )
        project_dir = getattr(session_info, "project_dir", "") if session_info else ""
        sid = session_info.session_id if session_info else session_id
        user_count = sum(1 for m in messages if m.type == MessageType.USER)
        assistant_count = sum(1 for m in messages if m.type == MessageType.ASSISTANT)
        ts_first = messages[0].timestamp if messages else ""
        ts_last = messages[-1].timestamp if messages else ""
        return SessionAnalysis(
            session_id=sid,
            project_dir=project_dir,
            total_lines=len(messages),
            user_count=user_count,
            assistant_count=assistant_count,
            tool_uses_by_name={},   # not tracked by non-Claude backends
            files_touched=[],        # not tracked by non-Claude backends
            timestamp_first=ts_first,
            timestamp_last=ts_last,
        )

    def get_session_timeline(
        self,
        session_id: str,
        preview_chars: int | None = None,
    ) -> list[dict]:
        """Chronological event timeline for one session.

        For Claude Code sessions: full timeline including tool call counts per message.
        For AI Studio / Gemini CLI sessions: message type, timestamp, and content preview
        (tool_count=0 — tool invocations are not tracked by non-Claude backends).
        """
        if preview_chars is None:
            preview_chars = _engine_cfg_default("preview_chars", 150)
        claude_be = self._claude_backend
        if claude_be:
            return claude_be.timeline_session(session_id, preview_chars)
        # Generic fallback: build from messages API
        messages = self.get_messages(session_id)
        events = []
        for msg in messages:
            content = msg.content or ""
            msg_type = msg.type.value if hasattr(msg.type, "value") else str(msg.type)
            if msg_type == "system":
                continue  # skip system messages; consistent with Claude's timeline_session()
            events.append({
                "type": msg_type,
                "timestamp": msg.timestamp,
                "tool_count": 0,   # tool invocations not tracked in non-Claude message format
                "content_preview": content[:preview_chars],
            })
        return events

    def get_original_path(self, filename: str) -> str | None:
        return self._claude_only("get_original_path", None, filename)

    @property
    def recovery_dir(self) -> Path:
        """Recovery directory (Claude-only). Used by _version_src_path in cli.py."""
        claude_be = self._claude_backend
        if claude_be:
            return claude_be.recovery_dir
        from pathlib import Path
        return Path()

    @property
    def projects_dir(self) -> Path:
        """Claude projects directory (Claude-only). Used by _sessions_for_project in cli.py.

        Returns Path() (empty path) for non-Claude backends.
        """
        claude_be = self._claude_backend
        if claude_be:
            return claude_be.projects_dir
        from pathlib import Path
        return Path()



def _build_ai_session(
    source: str | None = None,
    claude_dir: str | None = None,
    config: dict | None = None,
) -> AISession:
    """Internal factory: build AISession without triggering __init__ recursion.

    NOTE: External code should use ``AISession()`` directly.
    This internal function exists to avoid infinite recursion:
    AISession.__init__ → _build_ai_session → AISession._from_backend (no __init__).

    Args:
        source: "claude", "aistudio", "gemini", "all", or None (auto-detect)
        claude_dir: Override Claude config dir (default: ~/.claude)
        config: Config dict (default: load from config.json)

    Returns:
        AISession wrapping either SessionRecoveryEngine or MultiSourceEngine.

    Strategy:
        source=None      → auto: 'all' if any non-Claude sources discovered, else 'claude'
        source="claude"  → Claude Code only (SessionRecoveryEngine)
        source="aistudio"→ AI Studio only
        source="gemini"  → Gemini CLI only
        source="all"     → all discovered/configured sources via MultiSourceEngine
    """
    from ai_session_search.sources.aistudio import AiStudioSource
    from ai_session_search.sources.gemini_cli import GeminiCliSource

    if config is None:
        config = load_config()

    # Merge auto-discovered sources into config (explicit config takes priority)
    effective_cfg = _discover_sources(config)
    # Pass effective_cfg (which includes _auto_discovered) so _detect_default_source
    # hits the TTL cache rather than re-scanning the filesystem.
    effective_source = source if source is not None else _detect_default_source(effective_cfg)

    if effective_source == "claude":
        from ai_session_search.config import resolve_claude_dir
        base: Path = resolve_claude_dir(override=claude_dir)
        projects = Path(os.getenv(
            "AI_SESSION_SEARCH_PROJECTS", str(base / "projects")
        )).expanduser()
        recovery = Path(os.getenv(
            "AI_SESSION_SEARCH_RECOVERY", str(base / "recovery")
        )).expanduser()
        backend = SessionRecoveryEngine(projects, recovery)
        return AISession._from_backend(backend, "claude")

    # Non-Claude: build MultiSourceEngine with requested sources
    sources: list = []
    sd = effective_cfg.get("source_dirs", {})

    if effective_source in ("aistudio", "all"):
        with contextlib.suppress(Exception):
            ai = sd.get("aistudio")
            if ai:
                dirs = [ai] if isinstance(ai, str) else ai
                sources.append(AiStudioSource([Path(p) for p in dirs]))

    if effective_source in ("gemini", "all"):
        with contextlib.suppress(Exception):
            gc = sd.get("gemini_cli")
            if gc:
                sources.append(GeminiCliSource(Path(gc)))

    # Include Claude as a source when source="all" (it is the default provider).
    # Claude sessions live in projects_dir, not in source_dirs, so they must be
    # added explicitly as a ClaudeSource adapter wrapping SessionRecoveryEngine.
    if effective_source == "all":
        with contextlib.suppress(Exception):
            from ai_session_search.config import resolve_claude_dir
            _base = resolve_claude_dir(override=claude_dir)
            _projects = Path(os.getenv(
                "AI_SESSION_SEARCH_PROJECTS", str(_base / "projects")
            )).expanduser()
            _recovery = Path(os.getenv(
                "AI_SESSION_SEARCH_RECOVERY", str(_base / "recovery")
            )).expanduser()
            sources.insert(0, ClaudeSource(SessionRecoveryEngine(_projects, _recovery)))

    if not sources:
        # Nothing configured/discovered — fall back to Claude
        return _build_ai_session("claude", claude_dir, config)

    backend = MultiSourceEngine(sources)
    return AISession._from_backend(backend, effective_source)


# ── Module-level aliases (after AISession class) ──────────────────────────────

# stdlib-consistent alias: connect() is how Python DB-API users expect to start.
# connect = AISession means type(connect()) is AISession — no wrapper, no indirection.
connect = AISession

