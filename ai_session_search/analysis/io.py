"""Durable publication helpers for generated analysis artifacts."""

from __future__ import annotations

import os
import tempfile
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import TextIO, cast


@contextmanager
def atomic_text_writer(path: Path) -> Iterator[TextIO]:
    """Yield a temporary writer, then fsync and atomically replace ``path``."""
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
        delete=False,
    )
    temporary_path = Path(temporary.name)
    try:
        with temporary:
            yield cast(TextIO, temporary)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_path, path)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise


def write_text_atomic(path: Path, content: str) -> None:
    """Durably replace a complete text artifact without exposing partial output."""
    with atomic_text_writer(path) as output:
        output.write(content)
