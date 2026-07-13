"""Pipeline change detection state. Tracks input hashes per stage for idempotent runs.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""
import contextlib
import hashlib
import json
from pathlib import Path

PIPELINE_STATE_FILE = ".pipeline_state.json"


def compute_file_list_hash(paths: list[Path]) -> str:
    """Hash sorted (path, mtime_ns, size) tuples. Fast: no file reads needed.

    Args:
        paths: List of file paths to hash

    Returns:
        "sha256:hexdigest" format string
    """
    h = hashlib.sha256()
    for p in sorted(str(p) for p in paths):
        try:
            s = Path(p).stat()
            h.update(f"{p}:{s.st_mtime_ns}:{s.st_size}".encode())
        except OSError:
            # File missing or unreadable — hash it as missing
            h.update(f"{p}:missing".encode())
    return f"sha256:{h.hexdigest()}"


def compute_config_hash(cfg: dict, keys: list[str]) -> str:
    """Hash selected config keys to detect config changes.

    Args:
        cfg: Configuration dict
        keys: List of config keys to include in hash

    Returns:
        "sha256:hexdigest" format string
    """
    h = hashlib.sha256()
    for k in sorted(keys):
        h.update(f"{k}:{cfg.get(k)}".encode())
    return f"sha256:{h.hexdigest()}"


def load_state(org_dir: Path) -> dict:
    """Load .pipeline_state.json; return empty dict if missing or corrupt.

    Args:
        org_dir: Organization directory where .pipeline_state.json is stored

    Returns:
        State dict {stage: {input_hash, run_time}} or empty dict if missing/invalid
    """
    with contextlib.suppress(OSError, json.JSONDecodeError):
        state_file = org_dir / PIPELINE_STATE_FILE
        if state_file.exists():
            return json.loads(state_file.read_text(encoding="utf-8"))
    return {}


def save_state(org_dir: Path, state: dict) -> None:
    """Write updated state atomically.

    Args:
        org_dir: Organization directory where .pipeline_state.json is stored
        state: State dict to save
    """
    state_file = org_dir / PIPELINE_STATE_FILE
    state_file.parent.mkdir(parents=True, exist_ok=True)
    state_file.write_text(json.dumps(state, indent=2), encoding="utf-8")


def is_stale(stage: str, current_hash: str, state: dict) -> bool:
    """True if stage inputs have changed or stage was never run.

    Args:
        stage: Pipeline stage name (e.g., "analyze", "graph", "organize")
        current_hash: Current input hash for this stage
        state: Current state dict

    Returns:
        True if stage needs to be re-run, False if current
    """
    return state.get(stage, {}).get("input_hash") != current_hash


def mark_done(stage: str, input_hash: str, state: dict) -> None:
    """Update state with completed stage info.

    Args:
        stage: Pipeline stage name
        input_hash: Input hash for this stage execution
        state: State dict to update (modified in-place)
    """
    from datetime import datetime, timezone
    state[stage] = {
        "input_hash": input_hash,
        "run_time": datetime.now(timezone.utc).isoformat(),
    }
