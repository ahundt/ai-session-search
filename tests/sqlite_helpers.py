# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Execute fixture SQL outside the process hosting the native SQLite library."""

import json
import subprocess
import sys
from collections.abc import Iterable, Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any


class SqlStatements:
    def __init__(self) -> None:
        self.statements: list[tuple[str, str, tuple[Any, ...]]] = []

    def execute(self, sql: str, parameters: tuple[Any, ...] = ()) -> None:
        self.statements.append(("execute", sql, parameters))

    def executemany(self, sql: str, parameters: Iterable[tuple[Any, ...]]) -> None:
        for row in parameters:
            self.execute(sql, row)

    def executescript(self, sql: str) -> None:
        self.statements.append(("executescript", sql, ()))


@contextmanager
def write_database(database: str | Path) -> Iterator[SqlStatements]:
    """Commit fixture SQL in a process with no native aise SQLite connections.

    Python and rusqlite link separate SQLite copies. Their in-process Unix lock/WAL
    bookkeeping is not shared, even when writes are sequential. In particular,
    sqlite3's transaction context does not close its connection; a later finalizer
    can checkpoint stale pages after the Rust service has closed and reopened.
    A child process preserves real cross-process reader/writer locking instead.

    Statements are buffered until normal context exit; cursors and query results
    are not returned. Like sqlite3, executescript commits preceding statements.
    The child commits successful writes and closes its connection before returning.
    """
    statements = SqlStatements()
    yield statements
    subprocess.run(
        [sys.executable, "-c", """
import json
import sqlite3
import sys
from contextlib import closing

with closing(sqlite3.connect(sys.argv[1])) as connection, connection:
    for operation, sql, parameters in json.load(sys.stdin):
        if operation == "executescript":
            connection.executescript(sql)
        else:
            connection.execute(sql, parameters)
""", str(database)],
        input=json.dumps(statements.statements), text=True, capture_output=True,
        check=True, timeout=15,
    )
