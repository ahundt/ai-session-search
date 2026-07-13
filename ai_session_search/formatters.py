"""
Output formatters with multiple output types.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""

import csv
import io
import json
from abc import ABC, abstractmethod
from collections.abc import Iterable
from typing import Any, List

from .models import SessionFile, SessionMessage

try:
    from rich.console import Console
    from rich.markup import escape as _rich_escape
    from rich.table import Table

    HAS_RICH = True
except ImportError:
    HAS_RICH = False
    _rich_escape = str  # no-op fallback when Rich is not installed


class ResultFormatter(ABC):
    """Base formatter protocol."""

    @abstractmethod
    def format(self, data: Any) -> str:
        """Format data for output."""
        pass

    @abstractmethod
    def format_many(self, items: Iterable[Any]) -> str:
        """Format multiple items. Accepts any iterable (list, generator, iterator)."""
        pass


class TableFormatter(ResultFormatter):
    """Format results as table using Rich."""

    def __init__(self, title: str = "Results"):
        """Initialize with title."""
        self.title = title
        if not HAS_RICH:
            raise ImportError("Rich library required for table formatting")

    def format(self, data: SessionFile) -> str:
        """Format single file."""
        lines = [
            f"Name:          {data.name}",
            f"Location:      {data.location}",
            f"Type:          {data.file_type}",
            f"Edits:         {data.edits}",
            f"Sessions:      {len(data.sessions)}",
            f"Last Modified: {data.last_modified or 'unknown'}",
            f"Created:       {data.created_date or 'unknown'}",
        ]
        return "\n".join(lines)

    def format_many(self, items: Iterable[Any]) -> Any:
        """Format multiple items as a Rich Table renderable.

        Returns a ``rich.table.Table`` object (not a pre-rendered string) so
        the caller's console can render it directly with correct terminal
        capabilities.  Returning a pre-rendered string via Console.capture()
        embeds ANSI escape codes that Rich's markup parser then misinterprets
        as markup tags when the string is passed back to console.print().
        """
        table = Table(title=self.title)
        table.add_column("File", style="cyan")
        table.add_column("Edits", justify="right", style="magenta")
        table.add_column("Type", style="green")
        table.add_column("Last Modified", style="blue")
        table.add_column("Location", style="yellow")
        table.add_column("Sessions", style="dim")

        for item in items:
            session_str = ", ".join(s[:8] + ("\u2026" if len(s) > 8 else "") for s in item.sessions[:3])
            if len(item.sessions) > 3:
                session_str += f" (+{len(item.sessions) - 3})"
            table.add_row(item.name, str(item.edits), item.file_type, item.last_modified or "", item.location, session_str)

        return table


class JsonFormatter(ResultFormatter):
    """Format results as JSON. Accepts any model with to_dict() or dataclass fields."""

    def _to_dict(self, data: Any) -> dict:
        """Duck-typed conversion: works with any model that has to_dict() or dataclass fields."""
        if hasattr(data, "to_dict"):
            return data.to_dict()
        try:
            from dataclasses import asdict
            return asdict(data)
        except TypeError:
            return {"value": str(data)}

    def format(self, data: Any) -> str:
        """Format single item as JSON."""
        return json.dumps(self._to_dict(data), indent=2)

    def format_many(self, items: Iterable[Any]) -> str:
        """Format multiple items as JSON array. Accepts any iterable."""
        return json.dumps([self._to_dict(item) for item in items], indent=2)


_CSV_HEADER = ["name", "location", "type", "edits", "sessions", "size_bytes", "last_modified", "created_date"]


def _file_to_csv_row(data: "SessionFile") -> "list[str | int]":
    """Convert a SessionFile to a CSV row aligned with _CSV_HEADER.

    Args:
        data: A SessionFile instance. Falls back to duck-typing via to_dict()
              for forward compatibility with SessionFile subclasses.

    Returns:
        A list of [name, location, type, edits, sessions, size_bytes,
        last_modified, created_date] matching _CSV_HEADER column order.
    """
    if hasattr(data, "to_dict"):
        d = data.to_dict()
        return [
            d.get("name", ""),
            d.get("location", ""),
            d.get("file_type", d.get("type", "")),
            d.get("edits", 0),
            len(d.get("sessions", [])),
            d.get("size_bytes", 0),
            d.get("last_modified", "") or "",
            d.get("created_date", "") or "",
        ]
    # Fallback: direct attribute access for SessionFile without to_dict()
    return [
        getattr(data, "name", ""),
        getattr(data, "location", ""),
        getattr(data, "file_type", ""),
        getattr(data, "edits", 0),
        len(getattr(data, "sessions", [])),
        getattr(data, "size_bytes", 0),
        getattr(data, "last_modified", "") or "",
        getattr(data, "created_date", "") or "",
    ]


class CsvFormatter(ResultFormatter):
    """Format results as RFC 4180-compliant CSV (fields properly quoted).

    Accepts any model with to_dict() via duck-typing.
    """

    def format(self, data: Any) -> str:
        """Format single item as CSV with header."""
        buf = io.StringIO()
        writer = csv.writer(buf)
        writer.writerow(_CSV_HEADER)
        writer.writerow(_file_to_csv_row(data))
        return buf.getvalue()

    def format_many(self, items: Iterable[Any]) -> str:
        """Format multiple items as CSV with header. Accepts any iterable."""
        buf = io.StringIO()
        writer = csv.writer(buf)
        writer.writerow(_CSV_HEADER)
        for item in items:
            writer.writerow(_file_to_csv_row(item))
        return buf.getvalue()


class MessageFormatter(ResultFormatter):
    """Format session messages."""

    def __init__(self, max_chars: int = 0):
        """Initialize with max_chars. 0 = full content (no truncation)."""
        self.max_chars = max_chars

    @staticmethod
    def _type_str(msg_type: Any) -> str:
        """Return the string representation of a message type.

        Guards against plain-string type values (e.g. ``type="user"`` without the
        ``MessageType`` enum), which would raise ``AttributeError: 'str' has no .value``.
        SessionMessage.to_dict() already has this guard; we mirror it here.
        """
        return msg_type.value if hasattr(msg_type, "value") else str(msg_type)

    def format(self, data: SessionMessage) -> str:
        """Format single message."""
        text = data.preview(self.max_chars) if self.max_chars else data.content.replace("\n", " ")
        text = _rich_escape(text)
        return f"""Type:       {self._type_str(data.type)}
Session:    {data.session_id}
Timestamp:  {data.timestamp}
Content:    {text}
Length:     {len(data.content)} chars"""

    def format_many(self, items: Iterable[Any]) -> str:
        """Format multiple messages. Accepts any iterable."""
        lines = []
        for msg in items:
            text = msg.preview(self.max_chars) if self.max_chars else msg.content.replace("\n", " ")
            text = _rich_escape(text)
            lines.append(f"[{self._type_str(msg.type)}] {msg.timestamp[:19]} - {text}")
        return "\n".join(lines)


class PlainFormatter(ResultFormatter):
    """Simple plain text formatter."""

    def format(self, data: Any) -> str:
        """Format single item."""
        if isinstance(data, SessionFile):
            return f"{data.name} ({data.edits} edits) - {data.location}"
        elif isinstance(data, SessionMessage):
            return f"[{data.type.value}] {data.preview()}"
        return str(data)

    def format_many(self, items: Iterable[Any]) -> str:
        """Format multiple items. Accepts any iterable."""
        return "\n".join(self.format(item) for item in items)


def get_formatter(format_type: str, title: str = "Results") -> ResultFormatter:
    """Factory function to get formatter by type.

    Args:
        format_type: One of "table", "json", "csv", "plain", "message".
        title:       Display title for formatters that support it (TableFormatter).

    Returns:
        A ResultFormatter instance for the requested format type.

    Raises:
        ValueError: If format_type is unknown.
    """
    # Explicit title-flag avoids the old try/except TypeError pattern which would
    # silently mask real bugs in formatter __init__ methods.
    _formatters_with_title = {
        "table": TableFormatter,
    }
    _formatters_no_title = {
        "json": JsonFormatter,
        "csv": CsvFormatter,
        "plain": PlainFormatter,
        "message": MessageFormatter,
    }

    fmt = format_type.lower()
    if fmt in _formatters_with_title:
        return _formatters_with_title[fmt](title)
    if fmt in _formatters_no_title:
        return _formatters_no_title[fmt]()
    raise ValueError(f"Unknown format: {format_type!r}. Valid options: table, json, csv, plain, message")
