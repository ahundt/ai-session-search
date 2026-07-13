"""
Composable filter implementations for advanced searching.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""

from __future__ import annotations

import fnmatch as _fnmatch
from collections.abc import Callable as _Callable, Iterable as _Iterable
from typing import Generic, List, Optional, TypeVar

from .models import MessageType, SessionFile, SessionMessage

T = TypeVar("T")


class Filter(Generic[T]):
    """Generic composable filter — base class for SearchFilter and MessageFilter.

    Subclasses inherit & (AND) and | (OR) operators automatically.
    Each .by_*() builder appends a predicate; apply() / __call__() runs all predicates.

    AND composition: combine predicate lists (both must pass)
    OR  composition: wrap both in a single disjunctive predicate

    Type-parameterized for IDE autocomplete:
        SearchFilter = Filter[SessionFile]
        MessageFilter = Filter[SessionMessage]
    """

    def __init__(self) -> None:
        self._predicates: List[_Callable[[T], bool]] = []

    def custom(self, predicate: _Callable[[T], bool]) -> "Filter[T]":
        """Add an arbitrary predicate function. Returns self for chaining.

        Type-independent — works for both SessionFile and SessionMessage predicates.
        Moved from SearchFilter and MessageFilter to Filter[T] base (DRY fix).
        """
        self._predicates.append(predicate)
        return self

    def by_session(self, session_id: str) -> "Filter[T]":
        """Filter items to those belonging to a specific session.

        Handles both SessionFile (.sessions list) and SessionMessage (.session_id str).
        Moved from SearchFilter and MessageFilter to Filter[T] base (DRY fix).
        """
        def predicate(item: T) -> bool:
            sid = getattr(item, "session_id", None)
            if sid is None:
                sessions = getattr(item, "sessions", [])
                return session_id in sessions
            return sid == session_id
        self._predicates.append(predicate)
        return self

    def apply(self, items: _Iterable[T]) -> List[T]:
        """Apply all predicates to items — AND logic (all must pass)."""
        result = list(items)
        for pred in self._predicates:
            result = [i for i in result if pred(i)]
        return result

    def __call__(self, items: _Iterable[T]) -> List[T]:
        """Support callable interface — same as apply()."""
        return self.apply(items)

    def _new_empty(self) -> "Filter[T]":
        """Create an empty filter of the same concrete type, safe for subclasses.

        Uses ``copy.copy`` (shallow copy) rather than ``type(self)()`` so that
        subclasses with required ``__init__`` arguments (e.g., a custom filter
        that takes a mandatory ``config`` param) are not accidentally broken by
        composition operators.  The copy has the same class and any instance
        attributes the subclass set in ``__init__``, but its ``_predicates``
        list is replaced immediately by the caller.

        Override this in subclasses if they require different copying semantics
        (e.g., deep-copy of mutable init args).
        """
        import copy
        result = copy.copy(self)
        result._predicates = []
        return result

    def __and__(self, other: "Filter[T]") -> "Filter[T]":
        """AND composition: item must pass ALL predicates from BOTH filters.

        Merges predicate lists from both sides.  The result has the same concrete
        type as ``self`` (e.g., SearchFilter & SearchFilter → SearchFilter).
        Uses ``_new_empty()`` instead of ``type(self)()`` so subclasses with
        required ``__init__`` args are not broken.

        Example::
            py_big = SearchFilter().by_extension("py") & SearchFilter().by_size(min_size=100)
        """
        result = self._new_empty()
        result._predicates = list(self._predicates) + list(other._predicates)
        return result

    def __or__(self, other: "Filter[T]") -> "Filter[T]":
        """OR composition: item must pass at LEAST ONE of the two filters.

        Wrapped as a single disjunctive predicate to preserve AND-chain semantics.
        An empty filter (no predicates) contributes nothing to OR — it is treated as
        "no match" rather than "match all" to avoid vacuous truth surprising behaviour.
        Use a plain ``Filter()`` / ``SearchFilter()`` with no predicates to pass all items.

        Uses ``_new_empty()`` for subclass safety (see ``__and__`` / ``_new_empty``).

        Example::
            py_or_ts = SearchFilter().by_extension("py") | SearchFilter().by_extension("ts")
            # Only files with .py OR .ts extension are returned.
            # (SearchFilter() | py_filter) == py_filter — empty side is ignored.
        """
        left_preds = list(self._predicates)
        right_preds = list(other._predicates)

        def _or_predicate(item: T) -> bool:
            # Guard against vacuous truth: all([]) == True would make an empty filter
            # pass everything in an OR. Instead, an empty predicate list means "no match".
            left_pass = bool(left_preds) and all(p(item) for p in left_preds)
            right_pass = bool(right_preds) and all(p(item) for p in right_preds)
            return left_pass or right_pass

        result = self._new_empty()
        result._predicates.append(_or_predicate)
        return result


class SearchFilter(Filter[SessionFile]):
    """Composable file search filter — imperative predicate chain.

    Inherits & / | operators and by_session() / custom() from Filter[T].
    Use by_location_pattern() for glob-based location filtering.

    Example::
        recent_py = (SearchFilter()
                     .by_date(since="7d")
                     .by_location_pattern(include=["*/src/*"])
                     .by_extension("py"))
        files = session.search_files("*", recent_py)

        # OR: Python or TypeScript
        py_or_ts = SearchFilter().by_extension("py") | SearchFilter().by_extension("ts")
    """

    def by_edits(self, min_edits: int = 0, max_edits: Optional[int] = None) -> "SearchFilter":
        """Filter by edit count range.

        max_edits=0 means "only files with zero edits" (not unlimited).
        """

        def predicate(f: SessionFile) -> bool:
            if f.edits < min_edits:
                return False
            if max_edits is not None and f.edits > max_edits:
                return False
            return True

        self._predicates.append(predicate)
        return self

    def by_extension(self, extension: str) -> "SearchFilter":
        """Filter by file extension (e.g., 'py', 'md', 'json').

        Matches against ``file_type`` first; falls back to inferring from the
        filename when ``file_type`` is unset (``"unknown"`` or ``""``).
        """
        ext = extension.lstrip(".")

        def predicate(f: SessionFile) -> bool:
            if f.file_type and f.file_type not in ("unknown", ""):
                return f.file_type == ext
            # Fallback: infer from filename
            return f.name.endswith(f".{ext}")

        self._predicates.append(predicate)
        return self

    def by_location(self, location: str) -> "SearchFilter":
        """Filter by location substring (simple string match).

        For glob-pattern matching with include/exclude, use by_location_pattern().
        """

        def predicate(f: SessionFile) -> bool:
            return location.lower() in f.location.lower()

        self._predicates.append(predicate)
        return self

    def by_location_pattern(
        self,
        include: "List[str] | None" = None,
        exclude: "List[str] | None" = None,
    ) -> "SearchFilter":
        """Filter by location using fnmatch glob patterns.

        Replaces ``LocationMatcher`` with a composable SearchFilter predicate.

        Args:
            include: fnmatch patterns for locations to include (e.g. ``["*/src/*", "*/lib/*"]``).
                     If given, only files whose location matches ANY pattern are kept.
                     If None or empty, all locations pass the include check.
            exclude: fnmatch patterns for locations to exclude (e.g. ``["*/test/*"]``).
                     Files matching ANY exclude pattern are dropped. Applied AFTER include.

        Returns:
            Self for chaining.

        Example::

            # Only Python files in src/ or lib/, not in tests/:
            sf = (SearchFilter()
                  .by_location_pattern(include=["*/src/*", "*/lib/*"], exclude=["*/test*/*"])
                  .by_extension("py"))

            # Compose with OR:
            src_files  = SearchFilter().by_location_pattern(include=["*/src/*"])
            busy_files = SearchFilter().by_edits(min_edits=5)
            combined   = src_files | busy_files
        """
        _include = [p.lower() for p in (include or [])]
        _exclude = [p.lower() for p in (exclude or [])]

        def predicate(f: SessionFile) -> bool:
            loc = f.location.lower()
            if _include and not any(_fnmatch.fnmatch(loc, p) for p in _include):
                return False
            if _exclude and any(_fnmatch.fnmatch(loc, p) for p in _exclude):
                return False
            return True

        self._predicates.append(predicate)
        return self

    def by_size(self, min_size: int = 0, max_size: Optional[int] = None) -> "SearchFilter":
        """Filter by file size in bytes.

        max_size=0 means "only empty files" (not unlimited).
        """

        def predicate(f: SessionFile) -> bool:
            if f.size_bytes < min_size:
                return False
            if max_size is not None and f.size_bytes > max_size:
                return False
            return True

        self._predicates.append(predicate)
        return self

    def by_date(
        self,
        since: "str | None" = None,
        until: "str | None" = None,
    ) -> "SearchFilter":
        """Filter by last_modified date range using FilterSpec.matches_datetime().

        Args:
            since: ISO date or duration shorthand (e.g. "7d", "2026-01-01").
                   Files with no last_modified date are excluded when since/until set.
            until: ISO date upper bound. None = no upper bound.

        Returns:
            Self for chaining: SearchFilter().by_date(since="7d").by_extension("py")

        Example::

            recent_py = SearchFilter().by_date(since="7d").by_extension("py")
            files = session.search_files("*", recent_py)
        """
        from .models import FilterSpec
        _spec = FilterSpec(since=since, until=until)

        def predicate(f: SessionFile) -> bool:
            return _spec.matches_datetime(f.last_modified or "")

        self._predicates.append(predicate)
        return self


class MessageFilter(Filter[SessionMessage]):
    """Composable message filter.

    Inherits & / | operators and by_session() / custom() from Filter[T].

    Why separate from SearchFilter? MessageFilter predicates operate on SessionMessage
    fields (type, content, is_long). SearchFilter predicates operate on SessionFile
    fields (edits, extension, location, size). Merging would produce a type-unsafe class.
    Filter[T] base already unifies all shared behavior.

    Example::
        user_auth = (MessageFilter()
                     .by_type(MessageType.USER)
                     .by_content("authentication"))
        matches = user_auth(all_messages)

        # OR: user messages OR long assistant messages:
        user_msgs = MessageFilter().by_type(MessageType.USER)
        long_msgs = MessageFilter().by_type(MessageType.ASSISTANT).long_messages_only()
        either = user_msgs | long_msgs
    """

    def by_type(self, message_type: MessageType) -> "MessageFilter":
        """Filter by message type."""

        def predicate(m: SessionMessage) -> bool:
            return m.type == message_type

        self._predicates.append(predicate)
        return self

    def by_content(self, pattern: str) -> "MessageFilter":
        """Filter by content pattern."""

        def predicate(m: SessionMessage) -> bool:
            return pattern.lower() in m.content.lower()

        self._predicates.append(predicate)
        return self

    def long_messages_only(self) -> "MessageFilter":
        """Filter to long messages only."""
        self._predicates.append(lambda m: m.is_long)
        return self


