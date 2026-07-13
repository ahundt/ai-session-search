"""
Type protocols for composable, extensible architecture.

Protocols allow dependency injection and multiple implementations.
Import from this module to implement custom backends or filters.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""

from pathlib import Path
from typing import Any, Callable, List, Optional, Protocol, runtime_checkable

from .models import FileVersion, FilterSpec, SessionFile


@runtime_checkable
class Searchable(Protocol):
    """Protocol for searchable storage backends."""

    def search(self, pattern: str, filters: Optional[FilterSpec] = None) -> List[SessionFile]:
        """Search for files matching pattern with optional filters."""
        ...


@runtime_checkable
class Extractable(Protocol):
    """Protocol for file extraction."""

    def extract_final(self, filename: str, output_dir: Path) -> Optional[Path]:
        """Extract final version of a file."""
        ...

    def extract_all(self, filename: str, output_dir: Path) -> List[Path]:
        """Extract all versions of a file."""
        ...


@runtime_checkable
class Filterable(Protocol):
    """Protocol for filtering operations."""

    def apply_filters(self, items: List[Any], filters: FilterSpec) -> List[Any]:
        """Apply filters to collection."""
        ...

    def matches(self, item: Any, filters: FilterSpec) -> bool:
        """Check if single item matches filters."""
        ...


@runtime_checkable
class Storage(Protocol):
    """Protocol for storage backends."""

    def list_files(self) -> List[SessionFile]:
        """List all available files."""
        ...

    def get_versions(self, filename: str) -> List[FileVersion]:
        """Get all versions of a file."""
        ...

    def read_file(self, path: Path) -> str:
        """Read file content."""
        ...


@runtime_checkable
class Predicate(Protocol):
    """Protocol for single-item filter predicates.

    Used for custom predicates passed to SearchFilter.custom() or
    MessageFilter.custom(). Returns True if item should be included.

    Example::

        def is_large(f: SessionFile) -> bool:
            return f.size_bytes > 10_000

        sf = SearchFilter().custom(is_large)
    """

    def __call__(self, item: Any) -> bool:
        """Return True if item passes this predicate."""
        ...


@runtime_checkable
class Composable(Protocol):
    """Protocol for composable list-based filters supporting | and & operators.

    Composable filters accept a sequence and return a filtered subset.
    Implemented by SearchFilter and MessageFilter.

    Example::

        py_filter = SearchFilter().by_extension("py")
        ts_filter = SearchFilter().by_extension("ts")
        combined  = py_filter & ts_filter   # AND composition
        either    = py_filter | ts_filter   # OR composition
    """

    def __or__(self, other: "Composable") -> "Composable":
        """OR composition — item passes either filter."""
        ...

    def __and__(self, other: "Composable") -> "Composable":
        """AND composition — item passes both filters."""
        ...

    def __call__(self, items: Any) -> Any:
        """Apply filter to sequence."""
        ...
