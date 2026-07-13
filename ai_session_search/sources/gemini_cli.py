"""
Gemini CLI session source for ai_session_search.

Implements StreamableStorage protocol for Gemini CLI JSON sessions at
~/.gemini/tmp/{hash}/chats/session-{date}-{id}.json

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""
from __future__ import annotations

import contextlib
import hashlib
import json
import re
from pathlib import Path
from typing import Generator

from ai_session_search.models import MessageType, SessionInfo, SessionMessage
from ai_session_search.config import load_config


class GeminiCliSource:
    """Storage + StreamableStorage for Gemini CLI session JSON files.

    Discovers sessions in ~/.gemini/tmp/{hash}/chats/session-*.json
    Gemini session structure: {sessionId, projectHash, messages: [{id, type, content, timestamp}]}
    """

    def __init__(self, gemini_tmp_dir: Path | None = None) -> None:
        """Initialize with path to Gemini tmp dir.

        Args:
            gemini_tmp_dir: Path to ~/.gemini/tmp or similar.
                           None → read from config.json source_dirs.gemini_cli.
        """
        if gemini_tmp_dir is None:
            cfg = load_config()
            gc = cfg.get("source_dirs", {}).get("gemini_cli")
            gemini_tmp_dir = Path(gc) if gc else Path.home() / ".gemini" / "tmp"
        self.gemini_tmp_dir = Path(gemini_tmp_dir)
        # Lazy-initialised reverse map: projectHash → real project path
        self._hash_to_path: dict[str, str] | None = None

    # ── Storage protocol ────────────────────────────────────────────────────

    def list_files(self):  # type: ignore[override]
        """List all Gemini CLI session files (Storage protocol)."""
        from ai_session_search.models import SessionFile
        result = []
        for chat_file in self._iter_chat_files():
            result.append(SessionFile(
                name=chat_file.name,
                path=str(chat_file),
                location=str(chat_file.parent),
                file_type="json",
            ))
        return result

    def get_versions(self, filename: str):  # type: ignore[override]
        """Gemini sessions are single-version."""
        return []

    def read_file(self, path: Path) -> str:
        """Read file content (Storage protocol)."""
        with contextlib.suppress(OSError):
            return path.read_text(encoding="utf-8", errors="ignore")
        return ""

    # ── StreamableStorage extension ─────────────────────────────────────────

    def stream_sessions(self) -> Generator[SessionInfo, None, None]:
        """Yield SessionInfo for each Gemini CLI session. Generator: O(1) memory."""
        for chat_file in self._iter_chat_files():
            with contextlib.suppress(Exception):
                yield self._make_session_info(chat_file)

    def read_session(self, session_info: SessionInfo) -> list[SessionMessage]:
        """Load and parse all messages for one Gemini CLI session.

        session_id is the file stem (e.g. 'session-2026-02-23T04-07-bd7e3697').
        Reconstructs full path as: project_dir / (session_id + '.json').
        """
        # Primary: reconstruct from project_dir + stem + .json
        path = Path(session_info.project_dir) / (session_info.session_id + ".json")
        if not path.exists():
            # Fallback: session_id might be a full path (backward compat)
            alt = Path(session_info.session_id)
            if alt.exists():
                path = alt
            else:
                return []
        with contextlib.suppress(OSError, json.JSONDecodeError):
            raw = path.read_text(encoding="utf-8", errors="ignore")
            data = json.loads(raw)
            return self._parse_messages(data, str(path))
        return []

    # ── Search ──────────────────────────────────────────────────────────────

    def search_messages(self, pattern: str, filters=None) -> list[SessionMessage]:
        """Search all sessions for messages matching pattern."""
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
        """List all sessions."""
        return list(self.stream_sessions())

    def stats(self) -> dict[str, int]:
        """Return basic statistics."""
        count = sum(1 for _ in self._iter_chat_files())
        return {"gemini_cli_sessions": count}

    # ── Private helpers ──────────────────────────────────────────────────────

    def _get_hash_to_path(self) -> dict[str, str]:
        """Return (and cache) a mapping of projectHash → real project path.

        Reads ~/.gemini/trustedFolders.json and ~/.gemini/projects.json to
        collect all known project paths, then computes SHA-256 of each (the
        same algorithm used by Gemini CLI's Storage.getFilePathHash()).
        Unresolvable hashes remain absent from the map — callers fall back to "".
        """
        if self._hash_to_path is not None:
            return self._hash_to_path

        all_paths: set[str] = set()
        gemini_dir = self.gemini_tmp_dir.parent  # ~/.gemini

        with contextlib.suppress(OSError, json.JSONDecodeError):
            tf = gemini_dir / "trustedFolders.json"
            if tf.exists():
                for p in json.loads(tf.read_text(encoding="utf-8")).keys():
                    all_paths.add(p)

        with contextlib.suppress(OSError, json.JSONDecodeError):
            pf = gemini_dir / "projects.json"
            if pf.exists():
                for p in json.loads(pf.read_text(encoding="utf-8")).get("projects", {}).keys():
                    all_paths.add(p)

        self._hash_to_path = {
            hashlib.sha256(p.encode()).hexdigest(): p
            for p in all_paths
        }
        return self._hash_to_path

    def _iter_chat_files(self) -> Generator[Path, None, None]:
        """Yield all Gemini CLI chat JSON files."""
        if not self.gemini_tmp_dir.exists():
            return
        for hash_dir in self.gemini_tmp_dir.iterdir():
            if not hash_dir.is_dir():
                continue
            chats_dir = hash_dir / "chats"
            if not chats_dir.exists():
                continue
            for f in chats_dir.glob("session-*.json"):
                yield f

    def _make_session_info(self, path: Path) -> SessionInfo:
        """Build SessionInfo from Gemini session file (reads minimal headers).

        Uses path.stem as session_id (e.g. 'session-2026-02-23T04-07-bd7e3697')
        so aise list shows a readable short identifier instead of a full path.
        project_dir stores the parent chats/ directory so read_session can
        reconstruct the full path as: Path(project_dir) / (session_id + '.json')
        """
        session_id = path.stem  # filename without .json extension
        project_hash = path.parent.parent.name
        timestamp = ""
        m = re.search(
            r"session-(\d{4}-\d{2}-\d{2})"  # date (required)
            r"(?:T(\d{2})"                    # T + HH (optional)
            r"(?:-(\d{2})"                    # -MM (optional)
            r"(?:-(\d{2})(?=-))?)?)?",        # -SS only if followed by '-' (avoids eating hash)
            path.name,
        )
        if m:
            date = m.group(1)
            hh = m.group(2) or "00"
            mm = m.group(3) or "00"
            ss = m.group(4) or "00"
            timestamp = f"{date}T{hh}:{mm}:{ss}"  # always produces YYYY-MM-DDTHH:MM:SS (19 chars)

        # Resolve hash → real project path (best-effort; "" if unknown)
        cwd = self._get_hash_to_path().get(project_hash, "")

        # Read just enough to get session metadata
        with contextlib.suppress(OSError, json.JSONDecodeError):
            raw = path.read_text(encoding="utf-8", errors="ignore")
            data = json.loads(raw)
            msgs = data.get("messages", [])
            user_count = sum(1 for msg in msgs if msg.get("type") == "user")
            return SessionInfo(
                session_id=session_id,
                project_dir=str(path.parent),
                cwd=cwd,
                git_branch="",
                timestamp_first=data.get("startTime") or timestamp,  # "startTime": null → fallback
                timestamp_last=data.get("lastUpdated", ""),
                message_count=user_count,
                has_compact_summary=False,
                provider="gemini_cli",
            )

        return SessionInfo(
            session_id=session_id,
            project_dir=str(path.parent),
            cwd=cwd,
            git_branch="",
            timestamp_first=timestamp,
            timestamp_last="",
            message_count=0,
            has_compact_summary=False,
            provider="gemini_cli",
        )

    def _parse_messages(self, data: dict, session_id: str) -> list[SessionMessage]:
        """Parse Gemini CLI JSON message format."""
        messages = []
        for msg in data.get("messages", []):
            msg_type_str = msg.get("type", "")
            if msg_type_str not in ("user", "gemini"):
                continue

            msg_type = MessageType.USER if msg_type_str == "user" else MessageType.ASSISTANT
            content = msg.get("content", "")

            # Content can be str or list[{text: str}]
            if isinstance(content, list):
                text = " ".join(
                    p.get("text", "") if isinstance(p, dict) else str(p)
                    for p in content
                )
            else:
                text = str(content)

            # Strip embedded file reference blocks
            text = re.sub(
                r"--- Content from referenced files ---.*?--- End of content ---\n?",
                "",
                text,
                flags=re.DOTALL,
            ).strip()

            if text:
                messages.append(SessionMessage(
                    type=msg_type,
                    timestamp=msg.get("timestamp", ""),
                    content=text,
                    session_id=session_id,
                ))
        return messages
