#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Exercise TUI startup and documented quit through a real pty.

The default invocation is the registered ``tui-startup-list`` benchmark case: start the TUI,
assert ``Sessions``/``Preview`` rendered, quit with ``q``. ``--measure-latency`` (the
``tui-typeahead-latency`` case) types each query one character at a time through the same pty,
reconstructs the screen from the crossterm byte stream, and reports typed-character echo latency
and result-appearance latency separately, plus the REQ010 resource figures.

The pty is created with :mod:`pty` directly: macOS ``script(1)`` block-buffers its own stdout
when that stdout is a pipe, so per-keystroke frames never reach the harness (measured 2026-08-29:
the post-exit default case worked, streaming stalled past every bound). A pty master read is
kernel-unbuffered and preserves per-frame arrival order. No measurement region is hardcoded: the
panes' divider column and border rows are derived from the rendered frame itself, so a TUI layout
change cannot silently redirect the measurement.
"""

from __future__ import annotations

import argparse
import codecs
import hashlib
import importlib
import json
import os
import platform
import re
import signal
import sqlite3
import struct
import subprocess
import tempfile
import threading
import time
from typing import Any

# The screen parser and benchmark-report tests are portable; only the real PTY driver is POSIX.
# Importing this module on Windows must therefore work and fail only when PTY execution is asked for.
fcntl: Any = importlib.import_module("fcntl") if os.name == "posix" else None
pty: Any = importlib.import_module("pty") if os.name == "posix" else None
select: Any = importlib.import_module("select") if os.name == "posix" else None
termios: Any = importlib.import_module("termios") if os.name == "posix" else None
PTY_SUPPORTED = all(module is not None for module in (fcntl, pty, select, termios))

DEFAULT_QUERIES = "mixedscope,sessiontoken001,sessiontoken015,oversized,missing-sentinel"
DEFAULT_REPETITIONS = 7
SETTLE_QUIET_SECONDS = 0.3
SAMPLER_INTERVAL_SECONDS = 0.05
# Keep the next character inside nontrivial search work so the mailbox/cancellation path is
# exercised; echo observation itself remains the synchronization point.
KEY_INTERVAL_SECONDS = 0.005
READ_CHUNK_BYTES = 65536
# How long the last keystroke's results may take to settle. Ten seconds covers the generated
# fixture with room to spare; a maintainer's own multi-gigabyte index needs more, and
# --final-settle-seconds raises it without editing this file.
FINAL_KEY_SETTLE_CAP_SECONDS = 10.0
ERROR_EXCERPT_CHARS = 400
SCREEN_ROWS = 24
SCREEN_COLS = 100
# A border row/divider must span at least this fraction of its axis to count as structure,
# so session text containing a stray box-drawing glyph cannot define a region.
BORDER_RUN_FRACTION = 0.5
# Fix the scoring-worker budget so ambient CPU count cannot change baseline/candidate thread,
# CPU, or RSS measurements. This is a benchmark protocol value, not a product default.
BENCHMARK_THREADS = 2
# The hermetic config uses search.default_limit=50; the TUI's documented browser floor raises it
# to 100. A contract test pins this protocol constant to Rust's TUI_MIN_BROWSER_RESULTS.
TUI_RESULT_LIMIT = 100
TUI_ARGS = (
    "--threads",
    str(BENCHMARK_THREADS),
    "--index-refresh",
    "existing-only",
    "tui",
)

CSI = re.compile(r"^\x1b\[([\x30-\x3f]*)([\x20-\x2f]*)([\x40-\x7e])")

# The preview header's two labelled fields, as tui.rs writes them, without the space that follows
# each label: word wrapping breaks between the label and a value too long to fit, leaving `Session:`
# alone on its row. Matching `"Session: "` missed exactly that row. The second label bounds where a
# wrapped session id can still be continuing.
SESSION_FIELD_LABEL = "Session:"
CWD_FIELD_LABEL = "CWD:"


class ScreenLayout:
    """Pane regions derived from one rendered frame; the single source for measurement regions."""

    def __init__(
        self, query_row: int, list_rows: range, list_cols: range, preview_cols: range
    ) -> None:
        self.query_row = query_row
        self.list_rows = list_rows
        self.list_cols = list_cols
        self.preview_cols = preview_cols


class ScreenTracker:
    """Rebuild the terminal grid from the crossterm/ratatui escape subset.

    Handles absolute and relative cursor moves, EL/ED clears, and printable writes; SGR and
    private modes are consumed and ignored. Good enough to answer "which cell region changed",
    which is what echo- versus results-latency needs.
    """

    def __init__(self) -> None:
        self.rows: list[list[str]] = [[" "] * SCREEN_COLS for _ in range(SCREEN_ROWS)]
        self.y = 0
        self.x = 0
        self.pending = ""
        # The stream is UTF-8: a box-drawing border is three bytes for ONE cell, and a chunk
        # boundary can split it. Decoding per-chunk as raw bytes would shatter "─" into three
        # junk cells and skew the cursor; the incremental decoder holds partial sequences.
        self.decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")

    def feed_bytes(self, data: bytes) -> None:
        self.feed(self.decoder.decode(data))

    def feed(self, text: str) -> None:
        self.pending += text
        while self.pending:
            char = self.pending[0]
            if char == "\x1b":
                match = CSI.match(self.pending)
                if match is None:
                    # CSI may split after ESC, '[', parameters, or intermediates. Buffer every
                    # syntactically incomplete prefix; discarding ESC early turns the remaining
                    # style bytes into visible cells and makes screen digests chunk-dependent.
                    if len(self.pending) == 1 or (
                        self.pending.startswith("\x1b[")
                        and not any("@" <= value <= "~" for value in self.pending[2:])
                    ):
                        return
                    self.pending = self.pending[1:]
                    continue
                self.pending = self.pending[match.end() :]
                self._csi(match.group(1), match.group(3))
            elif char == "\r":
                self.pending = self.pending[1:]
                self.x = 0
            elif char == "\n":
                self.pending = self.pending[1:]
                self.y = min(SCREEN_ROWS - 1, self.y + 1)
            elif char >= " ":
                self.pending = self.pending[1:]
                if self.y < SCREEN_ROWS and self.x < SCREEN_COLS:
                    self.rows[self.y][self.x] = char
                self.x += 1
            else:
                self.pending = self.pending[1:]

    def _csi(self, params: str, final: str) -> None:
        numbers = [int(value) for value in params.split(";") if value.isdigit()]
        if final == "H":
            row = (numbers[0] if len(numbers) > 0 else 1) - 1
            col = (numbers[1] if len(numbers) > 1 else 1) - 1
            self.y = max(0, min(SCREEN_ROWS - 1, row))
            self.x = max(0, min(SCREEN_COLS - 1, col))
        elif final == "K":
            for column in range(self.x, SCREEN_COLS):
                self.rows[self.y][column] = " "
        elif final == "J":
            for row in range(self.y + 1, SCREEN_ROWS):
                self.rows[row] = [" "] * SCREEN_COLS
            for column in range(self.x, SCREEN_COLS):
                self.rows[self.y][column] = " "
        else:
            count = numbers[0] if numbers else 1
            self._move(final, count)

    def _move(self, final: str, count: int) -> None:
        if final == "A":
            self.y = max(0, self.y - count)
        elif final == "B":
            self.y = min(SCREEN_ROWS - 1, self.y + count)
        elif final == "C":
            self.x = min(SCREEN_COLS - 1, self.x + count)
        elif final == "D":
            self.x = max(0, self.x - count)

    def line(self, row: int) -> str:
        return "".join(self.rows[row]).rstrip()

    def full_screen(self) -> str:
        return "\n".join(self.line(row) for row in range(SCREEN_ROWS))

    def derive_layout(self) -> ScreenLayout:
        """Locate the search-box interior row, the list pane's rows, and its columns.

        Reads the rendered borders: full-width horizontal rules bound the stacked blocks and the
        tall vertical rule between the panes is the split. Derived per run so the measurement
        follows the real layout instead of a copied percentage.
        """
        rules = [
            row
            for row in range(SCREEN_ROWS)
            if self._run_length(self.rows[row], "─") >= BORDER_RUN_FRACTION * SCREEN_COLS
        ]
        divider = self._divider_column()
        if len(rules) < 3 or divider is None:
            raise SystemExit(
                "could not derive TUI pane layout from the rendered frame; "
                f"horizontal rules at rows {rules}, divider column {divider}"
            )
        # rules[0]/rules[1] bound the search box; its interior row sits between them.
        if rules[1] - rules[0] != 2:
            raise SystemExit(f"unexpected search-box geometry: rules at {rules[:2]}")
        query_row = rules[0] + 1
        # The split pane's top border is one row below the full-width search-box rule. Exclude
        # that title border from the deterministic presentation digest: mode/activity text is
        # transient state. Cross-build semantic equality uses complete canonical session IDs.
        list_rows = range(rules[1] + 2, rules[-1])
        list_cols = range(1, divider)
        # Adjacent bordered panes contribute two divider cells: the list's right border at
        # `divider` and the preview's left border immediately after it.
        preview_cols = range(divider + 2, SCREEN_COLS - 1)
        if not list_rows or len(list_cols) < 10 or len(preview_cols) < 10:
            raise SystemExit(
                f"degenerate pane region: rows {list_rows}, list {list_cols}, preview {preview_cols}"
            )
        return ScreenLayout(query_row, list_rows, list_cols, preview_cols)

    def _divider_column(self) -> int | None:
        best_column, best_count = None, 0
        threshold = BORDER_RUN_FRACTION * (SCREEN_ROWS - 4)
        for column in range(1, SCREEN_COLS - 1):
            count = sum(1 for row in range(2, SCREEN_ROWS - 2) if self.rows[row][column] == "│")
            if count >= threshold and count > best_count:
                best_column, best_count = column, count
        return best_column

    @staticmethod
    def _run_length(cells: list[str], glyph: str) -> int:
        best = current = 0
        for cell in cells:
            current = current + 1 if cell == glyph else 0
            best = max(best, current)
        return best

    def query_line(self, layout: ScreenLayout) -> str:
        return self.line(layout.query_row)

    def session_list(self, layout: ScreenLayout) -> str:
        return "\n".join(
            "".join(self.rows[row][layout.list_cols.start : layout.list_cols.stop]).rstrip()
            for row in layout.list_rows
        )

    def search_state(self) -> str | None:
        match = re.search(r"Sessions[^\n]*· (searching|ready|stopped) ", self.full_screen())
        return match.group(1) if match is not None else None

    def session_position(self) -> int:
        match = re.search(r"Sessions[^\n]*\((\d+)/(\d+)\)", self.full_screen())
        if match is None:
            raise SystemExit("could not read the selected session position")
        return int(match.group(1))

    def preview_session_id(self, layout: ScreenLayout) -> str | None:
        rows = [
            "".join(self.rows[row][layout.preview_cols.start : layout.preview_cols.stop]).strip()
            for row in layout.list_rows
        ]
        for index, row in enumerate(rows):
            if not row.startswith(SESSION_FIELD_LABEL):
                continue
            # An id wider than the preview pane wraps, and a subagent id
            # (`claude:<uuid>/agent-<hash>`, 67 characters) does at 100 columns against a real
            # index: the wrapper leaves `Session:` alone on its row and breaks the id itself
            # across the next ones. Rejoin them — the pane inserts nothing when it breaks a run
            # with no spaces, so concatenating the stripped rows reproduces the id exactly.
            # Reading one row returned a truncated id that could never match the canonical one,
            # and the traversal reported it as a TUI that never showed the row.
            identifier = row[len(SESSION_FIELD_LABEL) :].lstrip()
            for continuation in rows[index + 1 :]:
                if not continuation or continuation.startswith(CWD_FIELD_LABEL):
                    break
                identifier += continuation
            return identifier or None
        return None

    def session_result_count(self) -> int:
        match = re.search(r"Sessions[^\n]*\(\d+/(\d+)\)", self.full_screen())
        if match is None:
            raise SystemExit("could not read the session result count from the rendered list title")
        return int(match.group(1))


def _require_pty_support() -> None:
    if not PTY_SUPPORTED:
        raise SystemExit("the TUI PTY benchmark requires a POSIX host")


def _claim_controlling_terminal() -> None:
    """Child-side preexec: after setsid (start_new_session), make the pty slave the controlling
    terminal. Without this, crossterm's /dev/tty resolution misses the pty and all input is
    ignored — /usr/bin/script did this implicitly, a bare Popen does not."""
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class TuiProcess:
    """The TUI running under a pty whose master side the harness drives directly."""

    def __init__(self, binary: str, fixture: str) -> None:
        _require_pty_support()
        self._sandbox = tempfile.TemporaryDirectory(prefix="aise-tui-benchmark-")
        sandbox = self._sandbox.name
        config_path = os.path.join(sandbox, "config.toml")
        with open(config_path, "w", encoding="utf-8") as config_file:
            config_file.write("")
        master_fd, slave_fd = pty.openpty()
        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", SCREEN_ROWS, SCREEN_COLS, 0, 0))
        self.master_fd: int | None = master_fd
        self.slave_fd: int | None = slave_fd
        child_env = {
            key: os.environ[key]
            for key in ("LANG", "LC_ALL", "PATH", "TMPDIR")
            if key in os.environ
        }
        child_env.update(
            {
                "HOME": sandbox,
                "XDG_CONFIG_HOME": os.path.join(sandbox, "xdg"),
                "AI_SESSION_SEARCH_CONFIG": config_path,
                "TERM": os.environ.get("TERM", "xterm-256color"),
            }
        )
        try:
            self.child = subprocess.Popen(
                [binary, "--database", fixture, *TUI_ARGS],
                stdin=slave_fd,
                stdout=slave_fd,
                stderr=subprocess.PIPE,
                env=child_env,
                close_fds=True,
                start_new_session=True,
                preexec_fn=_claim_controlling_terminal,
            )
        except BaseException:
            self.close()
            raise
        self._close_fd("slave_fd")
        self.captured = b""

    def send(self, data: bytes) -> None:
        os.write(self.master_fd, data)

    def read_chunk(self, timeout: float) -> bytes | None:
        """One pty read: ``None`` = nothing ready yet, ``b""`` = EOF, else the chunk."""
        if self.master_fd is None:
            return b""
        ready, _, _ = select.select([self.master_fd], [], [], timeout)
        if not ready:
            return None
        try:
            chunk = os.read(self.master_fd, READ_CHUNK_BYTES)
        except OSError:
            chunk = b""  # macOS signals master EOF as EIO
        self.captured += chunk
        return chunk

    def drain_after_exit(self) -> bytes:
        while True:
            chunk = self.read_chunk(0.05)
            if chunk is None:
                continue
            if chunk == b"":
                return self.captured

    def wait_draining(self, timeout: float) -> int | None:
        """Wait for exit while draining the pty. The pty buffer is a few KB — smaller than one
        startup render — so an undrained TUI blocks on stdout, never reads its input, and a
        sent ``q`` sits unread; this is why a bare ``wait()`` here times out. ``None`` = timeout."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.child.poll() is not None:
                return self.child.returncode
            self.read_chunk(0.02)
        return None

    def close(self) -> None:
        """Idempotently release both pty descriptors and the owned config sandbox."""
        self._close_fd("slave_fd")
        self._close_fd("master_fd")
        self._sandbox.cleanup()

    def _close_fd(self, name: str) -> None:
        descriptor = getattr(self, name, None)
        if descriptor is None:
            return
        try:
            os.close(descriptor)
        except OSError:
            pass
        setattr(self, name, None)

    def kill(self) -> None:
        # The child is a session leader (start_new_session), so kill the whole group: a
        # lone child kill leaves the TUI spinning on a dead pty for hours if its own exit
        # path never fires — measured as stray 30-80% CPU orphans after harness crashes.
        if self.child.poll() is None:
            try:
                os.killpg(self.child.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                self.child.kill()
            self.child.wait()
        self.close()


def _parse_cpu_time(value: str) -> float:
    days = 0
    clock = value
    if "-" in value:
        day_text, clock = value.split("-", 1)
        days = int(day_text)
    parts = [float(part) for part in clock.split(":")]
    seconds = 0.0
    for part in parts:
        seconds = seconds * 60 + part
    return days * 86_400 + seconds


class ResourceSampler:
    """Peak RSS / CPU / threads / process count for the aise process tree (root is the TUI)."""

    def __init__(self, root_pid: int) -> None:
        self.root_pid = root_pid
        self.peak_rss_kb: int | None = None
        self.peak_cpu_pct: float | None = None
        self.cpu_seconds: float | None = None
        self.peak_threads: int | None = None
        self.process_count: int | None = None
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        def sample() -> None:
            while not self._stop.is_set():
                pids = self._tree()
                count = len(pids)
                self.process_count = count if self.process_count is None else max(self.process_count, count)
                sample_rss_kb = 0
                sample_cpu_pct = 0.0
                sample_threads = 0
                sample_cpu_seconds = 0.0
                for pid in pids:
                    try:
                        line = subprocess.run(
                            ["ps", "-o", "rss=,pcpu=,time=", "-p", str(pid)],
                            capture_output=True, text=True, timeout=1,
                        ).stdout.strip()
                        if line:
                            rss_kb, cpu_pct, cpu_time = line.split()
                            sample_rss_kb += int(rss_kb)
                            sample_cpu_pct += float(cpu_pct)
                            sample_cpu_seconds += _parse_cpu_time(cpu_time)
                            threads = self._threads(pid)
                            if threads is not None:
                                sample_threads += threads
                    except (OSError, ValueError, subprocess.SubprocessError):
                        pass
                self.peak_rss_kb = (
                    sample_rss_kb
                    if self.peak_rss_kb is None
                    else max(self.peak_rss_kb, sample_rss_kb)
                )
                self.peak_cpu_pct = (
                    sample_cpu_pct
                    if self.peak_cpu_pct is None
                    else max(self.peak_cpu_pct, sample_cpu_pct)
                )
                self.cpu_seconds = (
                    sample_cpu_seconds
                    if self.cpu_seconds is None
                    else max(self.cpu_seconds, sample_cpu_seconds)
                )
                self.peak_threads = (
                    sample_threads
                    if self.peak_threads is None
                    else max(self.peak_threads, sample_threads)
                )
                if self._stop.wait(SAMPLER_INTERVAL_SECONDS):
                    break

        self._thread = threading.Thread(target=sample, daemon=True)
        self._thread.start()

    def _tree(self) -> list[int]:
        pids = [self.root_pid]
        frontier = [self.root_pid]
        while frontier:
            next_frontier: list[int] = []
            for pid in frontier:
                try:
                    children = subprocess.run(
                        ["pgrep", "-P", str(pid)],
                        capture_output=True, text=True, timeout=1,
                    ).stdout.split()
                except (OSError, subprocess.SubprocessError):
                    continue
                next_frontier.extend(int(child) for child in children if child.isdigit())
            frontier = next_frontier
            pids.extend(frontier)
        return pids

    @staticmethod
    def _threads(pid: int) -> int | None:
        try:
            if platform.system() == "Darwin":
                listing = subprocess.run(
                    ["ps", "-M", str(pid)], capture_output=True, text=True, timeout=1
                ).stdout.strip()
                return max(0, len(listing.splitlines()) - 1) if listing else None
            listing = subprocess.run(
                ["ps", "-o", "nlwp=", "-p", str(pid)],
                capture_output=True,
                text=True,
                timeout=1,
            ).stdout.strip()
            return int(listing) if listing else None
        except (OSError, ValueError, subprocess.SubprocessError):
            return None

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join()
            self._thread = None


def drain_output(tui: TuiProcess, tracker: ScreenTracker, quiet_seconds: float) -> None:
    """Consume pty chunks until the SCREEN stops changing for ``quiet_seconds``.

    Settling on byte-quietness is impossible: ratatui 0.30.2 emits a 25-byte idle trailer
    (SGR reset + hide-cursor) on every ``event::poll`` iteration — measured 2026-08-29, one
    25-byte chunk per ~151 ms, indefinitely. Those trailers mutate no cells, so state-stability
    is the correct settle signal and is immune to any future idle traffic."""
    last_state = tracker.full_screen()
    stable_since = time.monotonic()
    while True:
        chunk = tui.read_chunk(0.02)
        if chunk is None:
            if time.monotonic() - stable_since >= quiet_seconds:
                return
            continue
        if chunk == b"":
            return
        tracker.feed_bytes(chunk)
        if tracker.full_screen() != last_state:
            last_state = tracker.full_screen()
            stable_since = time.monotonic()


def _canonical_result_ids(binary: str, fixture: str, query: str, limit: int) -> list[str]:
    """Return the complete ordered service result, independent of viewport/presentation text."""
    with tempfile.TemporaryDirectory(prefix="aise-tui-semantics-") as sandbox:
        config_path = os.path.join(sandbox, "config.toml")
        with open(config_path, "w", encoding="utf-8") as config_file:
            config_file.write("")
        child_env = {
            key: os.environ[key]
            for key in ("LANG", "LC_ALL", "PATH", "TMPDIR")
            if key in os.environ
        }
        child_env.update(
            {
                "HOME": sandbox,
                "XDG_CONFIG_HOME": os.path.join(sandbox, "xdg"),
                "AI_SESSION_SEARCH_CONFIG": config_path,
            }
        )
        completed = subprocess.run(
            [
                binary,
                "--database",
                fixture,
                "--threads",
                str(BENCHMARK_THREADS),
                "--index-refresh",
                "existing-only",
                "search",
                query,
                "--limit",
                str(limit),
                "--format",
                "json",
            ],
            capture_output=True,
            text=True,
            check=True,
            env=child_env,
        )
    payload = json.loads(completed.stdout)
    if not isinstance(payload, list) or any(
        not isinstance(row, dict) or not isinstance(row.get("id"), str) for row in payload
    ):
        raise SystemExit("canonical search did not return a JSON array of session IDs")
    ids = [row["id"] for row in payload]
    if len(ids) != len(set(ids)):
        raise SystemExit("canonical search returned duplicate session IDs")
    return ids


def _validate_run_timings(runs: list[dict]) -> None:
    for run in runs:
        if run["mode_entry_ms"] < 0:
            raise SystemExit(f"missing search-mode entry observation for query {run['query']}")
        if any(value < 0 for value in run["echo_ms"]):
            raise SystemExit(f"missing typed-character echo observation for query {run['query']}")
        if run["results_ms"] is None:
            raise SystemExit(
                f"missing final-result observation for query {run['query']}: the last keystroke's "
                "results did not settle within --final-settle-seconds. A larger index needs a "
                "larger value; a stuck search needs investigating."
            )


def _validate_result_counts(runs: list[dict], tui_ids: dict[str, list[str]]) -> None:
    for run in runs:
        query = run["query"].rsplit("#", 1)[0]
        expected_count = len(tui_ids[query])
        if run["result_count"] != expected_count:
            raise SystemExit(
                f"TUI result count for {query!r} was {run['result_count']}, canonical search returned {expected_count}"
            )


def _stable_result_digests(
    runs: list[dict], tui_ids: dict[str, list[str]]
) -> tuple[dict[str, str], dict[str, str], str]:
    presentations: dict[str, set[str]] = {}
    for run in runs:
        query = run["query"].rsplit("#", 1)[0]
        presentations.setdefault(query, set()).add(run["digest"])
    for query, digests in presentations.items():
        if len(digests) != 1:
            raise SystemExit(
                f"non-deterministic final session-list presentation for query {query}: {sorted(digests)}"
            )
    stable_presentations = {
        query: next(iter(digests)) for query, digests in sorted(presentations.items())
    }
    semantic = {
        query: hashlib.sha256(json.dumps(ids, separators=(",", ":")).encode()).hexdigest()
        for query, ids in sorted(tui_ids.items())
    }
    aggregate = hashlib.sha256(
        json.dumps(semantic, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    return semantic, stable_presentations, aggregate


def _probe_tui_result_ids(
    binary: str,
    fixture: str,
    query: str,
    startup_wait: float,
    timeout: float,
    final_settle_seconds: float,
) -> list[str]:
    return _measure_once(
        binary,
        fixture,
        query,
        startup_wait,
        timeout,
        f"{query}#semantic-probe",
        collect_ids=True,
        final_settle_seconds=final_settle_seconds,
    )["tui_ids"]


def measure_latency(
    binary: str,
    fixture: str,
    queries: list[str],
    repetitions: int,
    startup_wait: float,
    timeout: float,
    final_settle_seconds: float = FINAL_KEY_SETTLE_CAP_SECONDS,
) -> dict:
    runs = []
    peak_rss_kb = peak_threads = None
    peak_cpu_pct = 0.0
    process_count = 0
    total_output_bytes = 0
    wal_before = _wal_size(fixture)
    for query in queries:
        for repetition in range(repetitions):
            run = _measure_once(
                binary, fixture, query, startup_wait, timeout,
                f"{query}#{repetition}",
                final_settle_seconds=final_settle_seconds,
            )
            runs.append(run)
            peak_rss_kb = run["peak_rss_kb"] if peak_rss_kb is None else max(peak_rss_kb, run["peak_rss_kb"])
            peak_threads = run["peak_threads"] if peak_threads is None else max(peak_threads, run["peak_threads"])
            peak_cpu_pct = max(peak_cpu_pct, run.get("peak_cpu_pct") or 0.0)
            process_count = max(process_count, run.get("process_count") or 0)
            total_output_bytes += run["output_bytes"]
    wal_growth = max(0, _wal_size(fixture) - wal_before)
    _validate_run_timings(runs)
    observed_counts: dict[str, set[int]] = {}
    for run in runs:
        query = run["query"].rsplit("#", 1)[0]
        observed_counts.setdefault(query, set()).add(run["result_count"])
    for query, counts in observed_counts.items():
        if len(counts) != 1:
            raise SystemExit(f"non-deterministic TUI result count for {query}: {sorted(counts)}")
    tui_ids = {
        query: _probe_tui_result_ids(
            binary, fixture, query, startup_wait, timeout, final_settle_seconds
        )
        for query in queries
    }
    canonical_ids = {
        query: _canonical_result_ids(binary, fixture, query, 0)[:TUI_RESULT_LIMIT]
        for query in queries
    }
    for query in queries:
        if tui_ids[query] != canonical_ids[query]:
            raise SystemExit(
                f"TUI ordered session IDs differ from canonical search for {query!r}: "
                f"tui={tui_ids[query]!r}, canonical={canonical_ids[query]!r}"
            )
    _validate_result_counts(runs, tui_ids)
    stable_digests, stable_presentation_digests, aggregate_digest = _stable_result_digests(
        runs, tui_ids
    )
    mode_entry_samples = [run["mode_entry_ms"] for run in runs]
    echo_samples = [value for run in runs for value in run["echo_ms"]]
    results_samples = [run["results_ms"] for run in runs]
    cpu_seconds = sum(run.get("cpu_seconds") or 0.0 for run in runs)
    fixture_workload = _fixture_workload(fixture)
    total_seconds = sum(run["wall_ms"] for run in runs) / 1000
    typed_keys = repetitions * sum(len(query) for query in queries)
    workload = {
        "query_characters": {query: len(query) for query in queries},
        "retained_sessions_max": max(run["result_count"] for run in runs),
        "visible_rows": max(run["visible_rows"] for run in runs),
        **fixture_workload,
        "scoring_workers": BENCHMARK_THREADS,
    }
    return {
        "queries": queries,
        "repetitions": repetitions,
        "runs": len(runs),
        "mode_entry_ms_p50": _percentile(mode_entry_samples, 50),
        "mode_entry_ms_p95": _percentile(mode_entry_samples, 95),
        "echo_ms_p50": _percentile(echo_samples, 50),
        "echo_ms_p95": _percentile(echo_samples, 95),
        "results_ms_p50": _percentile(results_samples, 50),
        "results_ms_p95": _percentile(results_samples, 95),
        "list_transitions_total": sum(run["transitions"] for run in runs),
        "peak_rss_kb": peak_rss_kb,
        "peak_cpu_pct": peak_cpu_pct,
        "cpu_seconds": cpu_seconds,
        "peak_threads": peak_threads,
        "process_count": process_count,
        "output_bytes": total_output_bytes,
        "typed_keys_per_second": round(typed_keys / total_seconds) if total_seconds else None,
        "wall_ms": round(total_seconds * 1000),
        "workload": workload,
        "completion_signal_observed": all(
            bool(run.get("completion_signal")) for run in runs
        ),
        "wal_growth_bytes": wal_growth,
        "result_digest": aggregate_digest,
        "result_digests_by_query": stable_digests,
        "presentation_digests_by_query": stable_presentation_digests,
        "per_query": [
            {
                "query": run["query"].rsplit("#", 1)[0],
                "echo_ms_p50": _percentile(run["echo_ms"], 50),
                "echo_ms_p95": _percentile(run["echo_ms"], 95),
                "results_ms": run["results_ms"],
                "transitions": run["transitions"],
            }
            for run in runs
        ],
    }


def _measure_once(
    binary: str,
    fixture: str,
    query: str,
    startup_wait: float,
    timeout: float,
    label: str,
    *,
    collect_ids: bool = False,
    final_settle_seconds: float = FINAL_KEY_SETTLE_CAP_SECONDS,
) -> dict:
    run_started = time.monotonic()
    tui = TuiProcess(binary, fixture)
    tracker = ScreenTracker()
    sampler = ResourceSampler(tui.child.pid)
    sampler.start()
    try:
        time.sleep(startup_wait)
        drain_output(tui, tracker, SETTLE_QUIET_SECONDS)
        if "Sessions" not in tracker.full_screen() or "Preview" not in tracker.full_screen():
            raise SystemExit("TUI startup did not render the expected panes")
        layout = tracker.derive_layout()
        completion_signal = tracker.search_state() is not None
        echo_ms: list[int] = []
        results_ms: int | None = None
        transitions = 0
        prefix = ""
        list_before = tracker.session_list(layout)
        output_bytes = 0
        mode_sent_at = time.monotonic()
        tui.send(b"/")
        mode_entry_found, added_bytes = _await_search_mode(
            tui, tracker, mode_sent_at, timeout
        )
        output_bytes += added_bytes
        mode_entry_ms = int(mode_entry_found) if mode_entry_found is not None else -1
        for index, key in enumerate(query):
            prefix += key
            sent_at = time.monotonic()
            tui.send(key.encode())
            echo_found, results_found, list_before, added_bytes, changed = _await_key_effects(
                tui, tracker, layout, prefix, list_before, sent_at, timeout,
                need_results=False,
            )
            output_bytes += added_bytes
            if changed:
                transitions += 1
            echo_ms.append(int(echo_found) if echo_found is not None else -1)
            if index == len(query) - 1:
                before_settle = list_before
                results_found, list_before, added_bytes = _await_settled_change(
                    tui,
                    tracker,
                    layout,
                    list_before,
                    sent_at,
                    final_settle_seconds,
                    initial_result_ms=results_found,
                    completion_signal=completion_signal,
                )
                output_bytes += added_bytes
                if list_before != before_settle:
                    transitions += 1
            if index == len(query) - 1 and results_found is not None:
                results_ms = int(results_found)
            time.sleep(KEY_INTERVAL_SECONDS)
        digest = hashlib.sha256(tracker.session_list(layout).encode()).hexdigest()
        tui_ids = _collect_tui_result_ids(tui, tracker, layout, timeout) if collect_ids else None
        result = _finish_run(
            tui,
            tracker,
            timeout,
            label,
            echo_ms,
            results_ms,
            transitions,
            sampler,
            output_bytes,
            digest,
            search_mode=not collect_ids,
        )
        if tui_ids is not None:
            result["tui_ids"] = tui_ids
        result["mode_entry_ms"] = mode_entry_ms
        result.update(
            {
                "result_count": tracker.session_result_count(),
                "visible_rows": len(layout.list_rows),
                "wall_ms": round((time.monotonic() - run_started) * 1000),
                "completion_signal": completion_signal,
            }
        )
        return result
    finally:
        sampler.stop()
        tui.kill()


def _await_tui_position_id(
    tui: TuiProcess,
    tracker: ScreenTracker,
    layout: ScreenLayout,
    position: int,
    previous: str | None,
    allow_same: bool,
    timeout: float,
) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        chunk = tui.read_chunk(0.01)
        if chunk not in (None, b""):
            tracker.feed_bytes(chunk)
        selected = tracker.preview_session_id(layout)
        if (
            tracker.session_position() == position
            and selected is not None
            and (selected != previous or allow_same)
        ):
            return selected
    # Say which of the three conditions did not hold. The bare message named a position and
    # nothing else, so a preview that failed to load, a selection that never moved, and a parser
    # that could not read the id were indistinguishable, and each one reads as a broken TUI.
    header = [
        "".join(tracker.rows[row][layout.preview_cols.start : layout.preview_cols.stop]).strip()
        for row in layout.list_rows
    ][:3]
    raise SystemExit(
        f"could not collect TUI session ID at position {position} within {timeout}s: "
        f"selection reached position {tracker.session_position()}, "
        f"preview parsed as {tracker.preview_session_id(layout)!r} "
        f"(previous {previous!r}), preview header {header!r}"
    )


def _await_browse_mode(
    tui: TuiProcess, tracker: ScreenTracker, timeout: float, context: str
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        chunk = tui.read_chunk(0.01)
        if chunk == b"":
            break
        if chunk is not None:
            tracker.feed_bytes(chunk)
        if "Search (press /)" in tracker.full_screen():
            return
    raise SystemExit(f"TUI did not leave search mode {context}")


def _collect_tui_result_ids(
    tui: TuiProcess, tracker: ScreenTracker, layout: ScreenLayout, timeout: float
) -> list[str]:
    count = tracker.session_result_count()
    tui.send(b"\x1b")
    _await_browse_mode(tui, tracker, timeout, "before semantic traversal")
    if count == 0:
        return []
    initial_position = tracker.session_position()
    initial_preview = tracker.preview_session_id(layout)
    tui.send(b"g")
    ids: list[str] = []
    for position in range(1, count + 1):
        if position > 1:
            tui.send(b"j")
        previous = ids[-1] if ids else initial_preview
        ids.append(
            _await_tui_position_id(
                tui,
                tracker,
                layout,
                position,
                previous,
                position == initial_position == 1,
                timeout,
            )
        )
    if len(ids) != len(set(ids)):
        raise SystemExit("TUI traversal returned duplicate session IDs")
    return ids


def _await_search_mode(
    tui: TuiProcess, tracker: ScreenTracker, sent_at: float, timeout: float
) -> tuple[float | None, int]:
    observed: float | None = None
    added_bytes = 0
    deadline = sent_at + timeout
    while time.monotonic() < deadline:
        chunk = tui.read_chunk(0.01)
        if chunk is None:
            continue
        if chunk == b"":
            break
        added_bytes += len(chunk)
        tracker.feed_bytes(chunk)
        if "Enter/Esc to browse" in tracker.full_screen():
            observed = (time.monotonic() - sent_at) * 1000
            break
    return observed, added_bytes


def _await_key_effects(
    tui: TuiProcess, tracker: ScreenTracker, layout: ScreenLayout, prefix: str,
    list_before: str, sent_at: float, timeout: float, need_results: bool,
) -> tuple[float | None, float | None, str, int, bool]:
    """Read output until this key's echo is observed — and its list change too, when the key is
    the burst's final one. Intermediate prefixes often leave the list unchanged, so waiting for
    both would burn the full timeout on every such key; their result changes stay opportunistic."""
    echo_found: float | None = None
    results_found: float | None = None
    added_bytes = 0
    changed = False
    deadline = sent_at + timeout
    while time.monotonic() < deadline:
        chunk = tui.read_chunk(0.01)
        if chunk is None:
            continue
        if chunk == b"":
            break
        added_bytes += len(chunk)
        now = time.monotonic()
        tracker.feed_bytes(chunk)
        if echo_found is None and prefix.lstrip("/") in tracker.query_line(layout):
            echo_found = (now - sent_at) * 1000
        if tracker.session_list(layout) != list_before:
            results_found = (now - sent_at) * 1000
            list_before = tracker.session_list(layout)
            changed = True
        if echo_found is not None and (not need_results or results_found is not None):
            break
    return echo_found, results_found, list_before, added_bytes, changed


def _await_settled_change(
    tui: TuiProcess,
    tracker: ScreenTracker,
    layout: ScreenLayout,
    list_before: str,
    sent_at: float,
    budget: float,
    initial_result_ms: float | None = None,
    completion_signal: bool = False,
) -> tuple[float | None, str, int]:
    """Return the LAST list transition before screen stability, never the first.

    A prefix search may finish after the final key was sent. Treating that first transition as
    the final result is a false-low latency sample. The candidate's rendered generation state
    later provides positive completion proof; this stability fallback is retained for the saved
    pre-generation baseline and records that limitation in its report.
    """
    deadline = time.monotonic() + budget
    added_bytes = 0
    last_result_ms = initial_result_ms
    stable_since = time.monotonic() if initial_result_ms is not None else None
    completion_seen_ms: float | None = None
    current_list = list_before
    while time.monotonic() < deadline:
        now = time.monotonic()
        state = tracker.search_state() if completion_signal else None
        if state == "stopped":
            return None, current_list, added_bytes
        if state == "searching":
            completion_seen_ms = None
        elif state == "ready" and completion_seen_ms is None:
            completion_seen_ms = (now - sent_at) * 1000
            stable_since = now
        chunk = tui.read_chunk(0.01)
        if chunk is None:
            observation = completion_seen_ms if completion_signal else last_result_ms
            if (
                observation is not None
                and stable_since is not None
                and time.monotonic() - stable_since >= SETTLE_QUIET_SECONDS
            ):
                return max(last_result_ms or 0.0, observation), current_list, added_bytes
            continue
        if chunk == b"":
            break
        added_bytes += len(chunk)
        now = time.monotonic()
        screen_before = tracker.full_screen()
        tracker.feed_bytes(chunk)
        if tracker.full_screen() != screen_before:
            stable_since = now
        observed = tracker.session_list(layout)
        if observed != current_list:
            current_list = observed
            last_result_ms = (now - sent_at) * 1000
            stable_since = now
    return (None if completion_signal else last_result_ms), current_list, added_bytes


def _finish_run(
    tui: TuiProcess, tracker: ScreenTracker, timeout: float, label: str,
    echo_ms: list[int], results_ms: int | None,
    transitions: int, sampler: ResourceSampler, _measured_output_bytes: int, digest: str,
    *, search_mode: bool = True,
) -> dict:
    if search_mode:
        tui.send(b"\x1b")  # leave search mode so q is a quit, not a query character
        _await_browse_mode(tui, tracker, timeout, f"before quit (query {label})")
    tui.send(b"q")
    if tui.wait_draining(timeout) is None:
        raise SystemExit(f"TUI did not exit after q (query {label})") from None
    captured = tui.drain_after_exit()
    _assert_terminal_restored(tui, captured)
    stderr = tui.child.stderr.read() if tui.child.stderr is not None else b""
    if tui.child.returncode != 0:
        raise SystemExit(
            f"TUI run failed (query {label}): exit {tui.child.returncode}: "
            f"{stderr.decode(errors='replace')[:ERROR_EXCERPT_CHARS]}"
        )
    # Join any in-progress sample before copying peak fields into the report.
    sampler.stop()
    return {
        "query": label,
        "echo_ms": echo_ms,
        "results_ms": results_ms,
        "transitions": transitions,
        "peak_rss_kb": sampler.peak_rss_kb,
        "peak_cpu_pct": sampler.peak_cpu_pct,
        "cpu_seconds": sampler.cpu_seconds,
        "peak_threads": sampler.peak_threads,
        "process_count": sampler.process_count,
        # `TuiProcess.read_chunk` appends every byte to this one authoritative ledger. The
        # per-wait counters are diagnostic subsets, never another total (R9-F8).
        "output_bytes": len(captured),
        "digest": digest,
    }


def _fixture_workload(fixture: str) -> dict[str, int]:
    connection = sqlite3.connect(f"file:{fixture}?mode=ro", uri=True)
    try:
        transcript_bytes = connection.execute(
            "select coalesce(max(length(cast(transcript_text as blob))), 0) from transcripts"
        ).fetchone()[0]
        messages_per_session = connection.execute(
            "select coalesce(max(message_count), 0) from "
            "(select count(*) as message_count from messages group by session_id)"
        ).fetchone()[0]
    finally:
        connection.close()
    return {
        "transcript_bytes_max": int(transcript_bytes),
        "messages_per_session_max": int(messages_per_session),
    }


def _wal_size(fixture: str) -> int:
    try:
        return os.stat(f"{fixture}-wal").st_size
    except OSError:
        return 0


def _percentile(values: list[int | float], percentile: float) -> int | None:
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, round((percentile / 100) * (len(ordered) - 1)))
    return int(ordered[index])


def _assert_terminal_restored(_tui: TuiProcess, captured: bytes) -> None:
    """Require restoration to be the final applicable terminal-mode controls.

    Substring presence is insufficient: a later enter-alternate-screen or hide-cursor control
    would reverse an earlier restore while still satisfying a naive contains check. A direct
    post-exit termios comparison is not portable here: after the pty's controlling session leader
    exits, macOS returns ENOTTY for the slave before the parent can observe it. Raw-mode ownership
    remains covered by TerminalGuard; this real-terminal oracle checks the emitted final state.
    """
    controls = {
        "alternate screen": (b"\x1b[?1049h", b"\x1b[?1049l"),
        "cursor visibility": (b"\x1b[?25l", b"\x1b[?25h"),
    }
    for name, (active, restored) in controls.items():
        active_at = captured.rfind(active)
        restored_at = captured.rfind(restored)
        if restored_at < 0 or restored_at < active_at:
            raise SystemExit(f"terminal restore was not observed for {name}")


def run_startup_case(binary: str, fixture: str, startup_wait: float, timeout: float) -> None:
    tui = TuiProcess(binary, fixture)
    try:
        tracker = ScreenTracker()
        required = ("Sessions", "Preview", "Session:", "CWD:")
        startup_deadline = time.monotonic() + startup_wait + timeout
        while time.monotonic() < startup_deadline:
            chunk = tui.read_chunk(0.02)
            if chunk in (None, b""):
                continue
            tracker.feed_bytes(chunk)
            if all(marker in tracker.full_screen() for marker in required):
                break
        startup_ready = all(marker in tracker.full_screen() for marker in required)
        tui.send(b"q")
        if tui.wait_draining(timeout) is None:
            raise SystemExit("TUI did not exit after documented q key") from None
        captured = tui.drain_after_exit()
        stderr = tui.child.stderr.read() if tui.child.stderr is not None else b""
        if tui.child.returncode != 0 or not startup_ready:
            raise SystemExit(
                f"TUI startup failed with exit {tui.child.returncode}; missing final pane/preview content: "
                f"{stderr.decode(errors='replace')}"
            )
        _assert_terminal_restored(tui, captured)
        print('{"preview":true,"sessions":true,"terminal_restored":true}')
    finally:
        tui.kill()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--timeout", type=float, default=5.0)
    parser.add_argument("--startup-wait", type=float, default=1.0)
    parser.add_argument("--measure-latency", action="store_true")
    parser.add_argument("--queries", default=DEFAULT_QUERIES)
    parser.add_argument("--repetitions", type=int, default=DEFAULT_REPETITIONS)
    parser.add_argument(
        "--final-settle-seconds",
        type=float,
        default=FINAL_KEY_SETTLE_CAP_SECONDS,
        help=(
            "how long the last keystroke's results may take to settle. The default fits the "
            "generated fixture; point this at a multi-gigabyte index and raise it."
        ),
    )
    args = parser.parse_args()
    if args.measure_latency:
        queries = [query.strip() for query in args.queries.split(",") if query.strip()]
        if not queries:
            raise SystemExit("--queries must contain at least one non-empty query")
        if args.final_settle_seconds <= 0:
            raise SystemExit("--final-settle-seconds must be greater than zero")
        report = measure_latency(
            args.binary, args.fixture, queries, args.repetitions,
            args.startup_wait, args.timeout, args.final_settle_seconds,
        )
        print(json.dumps(report, separators=(",", ":")))
        return 0
    run_startup_case(args.binary, args.fixture, args.startup_wait, args.timeout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
