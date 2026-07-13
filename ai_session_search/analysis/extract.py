"""
Extract clean verbatim user instruction history from Gemini CLI session JSON.

Session file: located via config key 'gemini_org_task_session' or auto-discovered
              from ~/.gemini/tmp/*/chats/session-*.json (no hardcoded path in source).

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""
from __future__ import annotations

import json
import re
from pathlib import Path

from ai_session_search.config import load_config, resolve_org_dir

# Context notes: brief description of what was happening at each message stage
_CONTEXT_RANGES: list[tuple[range, str]] = [
    (range(1, 14),    "Setting up audio pipeline, finding prompts, creating README"),
    (range(14, 26),   "Fixing prompt files, debugging file open errors, fixing wrecked transcript prompt"),
    (range(26, 45),   "Pipeline parameterization: stages, CLI flags, model config"),
    (range(45, 70),   "Test coverage, chunk size, Otter.ai integration"),
    (range(70, 90),   "Prompt file loading, WhisperX, torchcodec library error"),
    (range(90, 104),  "Pipeline runs, anti-AI polish stage, instruction history request"),
    (range(104, 114), "Anti-AI polish logic, output file naming, GLM-5 run"),
    (range(114, 133), "Organizing aistudio sessions: taxonomy, content analysis, methodology"),
    (range(133, 143), "Instruction history recovery, read-only commit, finalizing"),
]


def get_context_note(msg_num: int) -> str:
    """Return context note for message number."""
    for r, note in _CONTEXT_RANGES:
        if msg_num in r:
            return note
    return "General pipeline development"


def extract_text(content: object) -> str:
    """Extract text string from content field (str or list of part dicts)."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(
            p.get("text", "") if isinstance(p, dict) else str(p)
            for p in content
        )
    return ""


def strip_embedded_files(text: str) -> str:
    """Remove @-referenced file content blocks Gemini CLI embeds in messages."""
    text = re.sub(
        r"--- Content from referenced files ---.*?--- End of content ---\n?",
        "",
        text,
        flags=re.DOTALL,
    )
    # Collapse excess blank lines
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip()


def extract_history(session_file: str | Path, output_file: str | Path) -> int:
    """Extract clean user instruction history from a Gemini CLI session.

    Args:
        session_file: Path to Gemini CLI session JSON
        output_file: Path to write USER_INSTRUCTIONS_CLEAN.md

    Returns:
        Number of user messages extracted
    """
    session_path = Path(session_file)
    output_path = Path(output_file)

    print(f"Reading session: {session_path}")
    if not session_path.exists():
        raise FileNotFoundError(f"Session file not found: {session_path}")

    with open(session_path, encoding="utf-8") as f:
        data = json.load(f)

    messages = data.get("messages", [])
    user_messages = [m for m in messages if m.get("type") == "user"]
    print(f"Found {len(user_messages)} user messages")

    lines = [
        "# User Instruction History: Clean Verbatim Extract\n\n",
        "Extracted from Gemini CLI session JSON. ",
        "All user messages, verbatim, with embedded file content stripped.\n\n",
        f"Source: `{session_path.name}`\n\n---\n\n",
    ]

    for i, msg in enumerate(user_messages, start=1):
        raw = extract_text(msg.get("content", ""))
        clean = strip_embedded_files(raw)
        context = get_context_note(i)
        lines.append(f"## {i}. Instruction\n\n")
        lines.append(f"*Context: {context}*\n\n")
        quoted_lines = "\n".join(f"> {ln}" if ln.strip() else ">" for ln in clean.splitlines())
        quoted = quoted_lines if quoted_lines.strip() else "> (empty)"
        lines.append(f"{quoted}\n\n---\n\n")

    output_path.write_text("".join(lines), encoding="utf-8")
    print(f"Written {len(user_messages)} entries to: {output_path}")
    return len(user_messages)


def main() -> None:
    """Entry point for `aise extract` CLI command."""
    cfg = load_config()
    org_dir = resolve_org_dir(cfg)
    output_file = org_dir / "USER_INSTRUCTIONS_CLEAN.md"

    # Find session file: from config or auto-discover
    session_file = cfg.get("gemini_org_task_session", "")
    if not session_file or not Path(session_file).exists():
        # Auto-discover: find most recent session-2026-02-23 file
        from ai_session_search.config import resolve_gemini_dir
        gemini_tmp = resolve_gemini_dir(cfg)
        candidates = sorted(gemini_tmp.glob("*/chats/session-*.json"))
        if candidates:
            session_file = str(candidates[-1])
        else:
            print("ERROR: No Gemini org task session found. Set gemini_org_task_session in config.json")
            return

    # Verify archive file is not overwritten
    archive = org_dir / "ORGANIZATION_TASK_INSTRUCTIONS.md"

    count = extract_history(session_file, output_file)

    if archive.exists():
        print(f"Archive preserved: {archive}")
    print(f"Done: {count} instructions extracted")


if __name__ == "__main__":
    main()
