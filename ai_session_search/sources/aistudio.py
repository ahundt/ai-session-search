"""
Google AI Studio session source for ai_session_search.

Implements StreamableStorage protocol for AI Studio JSON (chunkedPrompt.chunks[])
and legacy .md format sessions.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""
from __future__ import annotations

import contextlib
import json
import os
from pathlib import Path
from typing import Generator

from ai_session_search.models import MessageType, SessionInfo, SessionMessage
from ai_session_search.config import load_config

BINARY_EXTS = frozenset({
    ".png", ".jpg", ".jpeg", ".gif", ".pdf", ".mp3", ".mp4", ".m4a",
    ".wav", ".docx", ".xlsx", ".pptx", ".rtf", ".zip", ".pyc", ".dylib",
    ".so", ".dll", ".exe", ".bin", ".dat", ".db", ".sqlite",
})


class AiStudioSource:
    """Storage + StreamableStorage implementation for Google AI Studio sessions.

    Handles both JSON format (chunkedPrompt.chunks) and legacy .md format.
    Implements Storage protocol (types.py) plus stream_sessions() / read_session()
    extension methods used by the analysis pipeline.

    Three source dirs supported simultaneously — pass all in source_dirs list.
    """

    def __init__(self, source_dirs: list[Path] | None = None) -> None:
        """Initialize with list of source directories.

        Args:
            source_dirs: List of Path objects to AI Studio session directories.
                         None → read from config.json source_dirs.aistudio.
        """
        if source_dirs is None:
            cfg = load_config()
            aistudio_cfg = cfg.get("source_dirs", {}).get("aistudio", [])
            if isinstance(aistudio_cfg, str):
                aistudio_cfg = [aistudio_cfg]
            source_dirs = [Path(p) for p in aistudio_cfg]
        self.source_dirs = source_dirs

    # ── Storage protocol ────────────────────────────────────────────────────

    def list_files(self):  # type: ignore[override]
        """List all session files (Storage protocol). Returns list of SessionFile objects."""
        from ai_session_search.models import SessionFile
        result = []
        for d in self.source_dirs:
            if not d.exists():
                continue
            for f in d.iterdir():
                if f.suffix.lower() in BINARY_EXTS or f.is_dir():
                    continue
                result.append(SessionFile(
                    name=f.name,
                    path=str(f),
                    location=str(d),
                    file_type=f.suffix.lstrip(".") or "json",
                ))
        return result

    def get_versions(self, filename: str):  # type: ignore[override]
        """Get versions (Storage protocol). AI Studio sessions are single-version."""
        return []

    def read_file(self, path: Path) -> str:
        """Read file content (Storage protocol)."""
        with contextlib.suppress(OSError):
            return path.read_text(encoding="utf-8", errors="ignore")
        return ""

    # ── StreamableStorage extension ─────────────────────────────────────────

    def stream_sessions(self) -> Generator[SessionInfo, None, None]:
        """Yield SessionInfo for each session file. Generator: O(1) memory per session.

        Yields lightweight metadata only — no full content loaded.
        Call read_session(session_info) to get messages for a specific session.
        """
        for d in self.source_dirs:
            if not d.exists():
                continue
            for f in sorted(d.iterdir()):
                if f.suffix.lower() in BINARY_EXTS or f.is_dir():
                    continue
                with contextlib.suppress(Exception):
                    yield self._make_session_info(f)

    def read_session(self, session_info: SessionInfo) -> list[SessionMessage]:
        """Load and parse all messages for one session.

        Full content loaded only here — caller should GC after use.
        """
        path = Path(session_info.project_dir) / session_info.session_id
        if not path.exists():
            # Try session_id as full path
            alt = Path(session_info.session_id)
            if alt.exists():
                path = alt
            else:
                return []
        with contextlib.suppress(OSError, UnicodeDecodeError):
            raw = path.read_text(encoding="utf-8", errors="ignore")
            return self._parse_messages(path, raw, session_info.session_id)
        return []

    # ── Search (for aise --source aistudio messages search) ─────────────────

    def search_messages(self, pattern: str, filters=None) -> list[SessionMessage]:
        """Search all sessions for messages matching pattern."""
        import re
        try:
            regex = re.compile(pattern, re.IGNORECASE)
        except re.error:
            regex = re.compile(re.escape(pattern), re.IGNORECASE)

        results = []
        for session_info in self.stream_sessions():
            messages = self.read_session(session_info)
            for msg in messages:
                if regex.search(msg.content):
                    results.append(msg)
        return results

    def list_sessions(self) -> list[SessionInfo]:
        """List all sessions (for MultiSourceEngine.list_sessions())."""
        return list(self.stream_sessions())

    def stats(self) -> dict[str, int]:
        """Return basic statistics."""
        count = sum(
            1 for d in self.source_dirs if d.exists()
            for f in d.iterdir()
            if f.suffix.lower() not in BINARY_EXTS and not f.is_dir()
        )
        return {"aistudio_sessions": count}

    # ── Private helpers ──────────────────────────────────────────────────────

    def _make_session_info(self, path: Path) -> SessionInfo:
        """Build SessionInfo from file metadata + quick JSON peek for message count.

        Uses file mtime as timestamp_first — best available signal for AI Studio
        sessions, which embed no createTime in their JSON.
        Reads file content to count user/model chunks; accepts the I/O cost since
        aise list inherently shows all sessions.
        """
        import datetime
        try:
            mtime = path.stat().st_mtime
            ts = datetime.datetime.fromtimestamp(mtime, tz=datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S")
        except OSError:
            ts = ""
        message_count = 0
        with contextlib.suppress(OSError, json.JSONDecodeError, UnicodeDecodeError):
            raw = path.read_text(encoding="utf-8", errors="ignore")
            data = json.loads(raw)
            chunks = data.get("chunkedPrompt", {}).get("chunks", [])
            message_count = sum(
                1 for c in chunks if c.get("role") in ("user", "model", "assistant")
            )
        return SessionInfo(
            session_id=path.name,
            project_dir=str(path.parent),
            cwd="",
            git_branch="",
            timestamp_first=ts,
            timestamp_last=ts,
            message_count=message_count,
            has_compact_summary=False,
            provider="aistudio",
        )

    _MIN_CONTENT_BYTES: int = 20

    def _parse_messages(self, path: Path, raw: str, session_id: str) -> list[SessionMessage]:
        """Parse session file into SessionMessage list. Handles JSON + .md formats."""
        if len(raw) < self._MIN_CONTENT_BYTES:
            import warnings
            warnings.warn(
                f"AiStudioSource: skipping {path.name!r} — content is only "
                f"{len(raw)} bytes (< {self._MIN_CONTENT_BYTES} byte minimum). "
                "The file may be empty, corrupted, or still being written.",
                stacklevel=3,
            )
            return []

        # Try JSON format (AI Studio chunkedPrompt)
        with contextlib.suppress(json.JSONDecodeError, UnicodeDecodeError):
            data = json.loads(raw)
            if "chunkedPrompt" in data:
                return self._parse_aistudio_json(data, session_id)
            # Might be some other JSON format — skip
            return []

        # Fallback: legacy .md format (2023-2024)
        if path.suffix == ".md":
            return [SessionMessage(
                type=MessageType.USER,
                timestamp="",
                content=raw,
                session_id=session_id,
            )]
        return []

    def _parse_aistudio_json(self, data: dict, session_id: str) -> list[SessionMessage]:
        """Parse AI Studio chunkedPrompt format."""
        chunks = data.get("chunkedPrompt", {}).get("chunks", [])
        messages = []
        for chunk in chunks:
            role = chunk.get("role", "")
            text = chunk.get("text", "")
            if not text:
                continue
            if role == "user":
                msg_type = MessageType.USER
            elif role in ("model", "assistant"):
                msg_type = MessageType.ASSISTANT
            else:
                continue
            messages.append(SessionMessage(
                type=msg_type,
                timestamp="",
                content=text,
                session_id=session_id,
            ))
        return messages
