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
import fcntl
import hashlib
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import termios
import time

DEFAULT_QUERIES = "SQLite 1,SQLite 15,benchmark 15,benchmark 3"
DEFAULT_REPETITIONS = 7
SETTLE_QUIET_SECONDS = 0.3
SAMPLER_INTERVAL_SECONDS = 0.05
KEY_INTERVAL_SECONDS = 0.06
# Esc and q must reach crossterm as separate reads: written back-to-back they arrive as one
# buffer and parse as Alt+q, so the mode-exit Esc never happens and q types into the query.
ESC_SETTLE_SECONDS = 0.08
READ_CHUNK_BYTES = 65536
FINAL_KEY_SETTLE_CAP_SECONDS = 10.0
ERROR_EXCERPT_CHARS = 400
SCREEN_ROWS = 24
SCREEN_COLS = 100
# A border row/divider must span at least this fraction of its axis to count as structure,
# so session text containing a stray box-drawing glyph cannot define a region.
BORDER_RUN_FRACTION = 0.5
TUI_ARGS = ("--index-refresh", "existing-only", "tui")

CSI = re.compile(r"^\x1b\[([\x30-\x3f]*)([\x20-\x2f]*)([\x40-\x7e])")


class ScreenLayout:
    """Pane regions derived from one rendered frame; the single source for measurement regions."""

    def __init__(self, query_row: int, list_rows: range, list_cols: range) -> None:
        self.query_row = query_row
        self.list_rows = list_rows
        self.list_cols = list_cols


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
                    if len(self.pending) == 1:
                        return  # escape sequence split across chunks
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
        # The list pane is the box below the search box: rules[1] top border, rules[-1] bottom.
        list_rows = range(rules[1] + 1, rules[-1])
        list_cols = range(1, divider)
        if not list_rows or len(list_cols) < 10:
            raise SystemExit(f"degenerate list region: rows {list_rows}, cols {list_cols}")
        return ScreenLayout(query_row, list_rows, list_cols)

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


def _claim_controlling_terminal() -> None:
    """Child-side preexec: after setsid (start_new_session), make the pty slave the controlling
    terminal. Without this, crossterm's /dev/tty resolution misses the pty and all input is
    ignored — /usr/bin/script did this implicitly, a bare Popen does not."""
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


class TuiProcess:
    """The TUI running under a pty whose master side the harness drives directly."""

    def __init__(self, binary: str, fixture: str) -> None:
        master_fd, slave_fd = pty.openpty()
        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", SCREEN_ROWS, SCREEN_COLS, 0, 0))
        self.child = subprocess.Popen(
            [binary, "--database", fixture, *TUI_ARGS],
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=subprocess.PIPE,
            env={**os.environ, "TERM": os.environ.get("TERM", "xterm-256color")},
            close_fds=True,
            start_new_session=True,
            preexec_fn=_claim_controlling_terminal,
        )
        os.close(slave_fd)
        self.master_fd = master_fd
        self.captured = b""

    def send(self, data: bytes) -> None:
        os.write(self.master_fd, data)

    def read_chunk(self, timeout: float) -> bytes | None:
        """One pty read: ``None`` = nothing ready yet, ``b""`` = EOF, else the chunk."""
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
        try:
            os.close(self.master_fd)
        except OSError:
            pass


class ResourceSampler:
    """Peak RSS / CPU / threads / process count for the aise process tree (root is the TUI)."""

    def __init__(self, root_pid: int) -> None:
        self.root_pid = root_pid
        self.peak_rss_kb: int | None = None
        self.peak_cpu_pct: float | None = None
        self.peak_threads: int | None = None
        self.process_count: int | None = None
        self._stop = False

    def start(self) -> None:
        import threading

        def sample() -> None:
            while not self._stop:
                pids = self._tree()
                self.process_count = len(pids)
                for pid in pids:
                    try:
                        line = subprocess.run(
                            ["ps", "-o", "rss=,pcpu=", "-p", str(pid)],
                            capture_output=True, text=True, timeout=1,
                        ).stdout.strip()
                        if line:
                            rss_kb, cpu_pct = line.split()
                            rss = int(rss_kb)
                            cpu = float(cpu_pct)
                            self.peak_rss_kb = rss if self.peak_rss_kb is None else max(self.peak_rss_kb, rss)
                            self.peak_cpu_pct = cpu if self.peak_cpu_pct is None else max(self.peak_cpu_pct, cpu)
                            threads = self._threads(pid)
                            if threads is not None:
                                self.peak_threads = (
                                    threads if self.peak_threads is None else max(self.peak_threads, threads)
                                )
                    except (OSError, ValueError, subprocess.SubprocessError):
                        pass
                time.sleep(SAMPLER_INTERVAL_SECONDS)

        thread = threading.Thread(target=sample, daemon=True)
        thread.start()

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
            listing = subprocess.run(
                ["ps", "-M", str(pid)], capture_output=True, text=True, timeout=1
            ).stdout.strip()
            return max(0, len(listing.splitlines()) - 1) if listing else None
        except (OSError, subprocess.SubprocessError):
            return None

    def stop(self) -> None:
        self._stop = True


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


def measure_latency(
    binary: str, fixture: str, queries: list[str], repetitions: int, startup_wait: float, timeout: float
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
            )
            runs.append(run)
            peak_rss_kb = run["peak_rss_kb"] if peak_rss_kb is None else max(peak_rss_kb, run["peak_rss_kb"])
            peak_threads = run["peak_threads"] if peak_threads is None else max(peak_threads, run["peak_threads"])
            peak_cpu_pct = max(peak_cpu_pct, run.get("peak_cpu_pct") or 0.0)
            process_count = max(process_count, run.get("process_count") or 0)
            total_output_bytes += run["output_bytes"]
    wal_growth = max(0, _wal_size(fixture) - wal_before)
    echo_samples = [value for run in runs for value in run["echo_ms"]]
    results_samples = [run["results_ms"] for run in runs if run["results_ms"] is not None]
    return {
        "queries": queries,
        "repetitions": repetitions,
        "runs": len(runs),
        "echo_ms_p50": _percentile(echo_samples, 50),
        "echo_ms_p95": _percentile(echo_samples, 95),
        "results_ms_p50": _percentile(results_samples, 50),
        "results_ms_p95": _percentile(results_samples, 95),
        "list_transitions_total": sum(run["transitions"] for run in runs),
        "peak_rss_kb": peak_rss_kb,
        "peak_cpu_pct": peak_cpu_pct,
        "peak_threads": peak_threads,
        "process_count": process_count,
        "output_bytes": total_output_bytes,
        "wal_growth_bytes": wal_growth,
        "result_digest": runs[-1]["digest"] if runs else None,
        "per_query": [
            {
                "query": run["query"].split("#")[0],
                "echo_ms_p50": _percentile(run["echo_ms"], 50),
                "echo_ms_p95": _percentile(run["echo_ms"], 95),
                "results_ms": run["results_ms"],
                "transitions": run["transitions"],
            }
            for run in runs
        ],
    }


def _measure_once(binary: str, fixture: str, query: str, startup_wait: float, timeout: float, label: str) -> dict:
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
        keys = ["/", *query]
        echo_ms: list[int] = []
        results_ms: int | None = None
        transitions = 0
        prefix = ""
        list_before = tracker.session_list(layout)
        output_bytes = 0
        for key in keys:
            prefix = "/" if key == "/" else prefix + key
            sent_at = time.monotonic()
            tui.send(key.encode())
            echo_found, results_found, list_before, added_bytes, changed = _await_key_effects(
                tui, tracker, layout, prefix, list_before, sent_at, timeout,
                need_results=key == keys[-1],
            )
            output_bytes += added_bytes
            if changed:
                transitions += 1
            echo_ms.append(int(echo_found) if echo_found is not None else -1)
            if key == keys[-1] and results_found is None:
                results_found, list_before, added_bytes = _await_settled_change(
                    tui, tracker, layout, list_before, sent_at, FINAL_KEY_SETTLE_CAP_SECONDS
                )
                output_bytes += added_bytes
                if results_found is not None:
                    transitions += 1
            if key == keys[-1] and results_found is not None:
                results_ms = int(results_found)
            time.sleep(KEY_INTERVAL_SECONDS)
        digest = hashlib.sha256(tracker.session_list(layout).encode()).hexdigest()
        return _finish_run(
            tui, timeout, label, echo_ms, results_ms, transitions, sampler, output_bytes, digest
        )
    finally:
        sampler.stop()
        tui.kill()


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
    tui: TuiProcess, tracker: ScreenTracker, layout: ScreenLayout, list_before: str,
    sent_at: float, budget: float,
) -> tuple[float | None, str, int]:
    """Wait out the final key's settle window for its list change (echo already observed)."""
    deadline = time.monotonic() + budget
    added_bytes = 0
    while time.monotonic() < deadline and tracker.session_list(layout) == list_before:
        chunk = tui.read_chunk(0.01)
        if chunk is None:
            continue
        if chunk == b"":
            break
        added_bytes += len(chunk)
        now = time.monotonic()
        tracker.feed_bytes(chunk)
        if tracker.session_list(layout) != list_before:
            return (now - sent_at) * 1000, tracker.session_list(layout), added_bytes
    return None, list_before, added_bytes


def _finish_run(
    tui: TuiProcess, timeout: float, label: str, echo_ms: list[int], results_ms: int | None,
    transitions: int, sampler: ResourceSampler, output_bytes: int, digest: str,
) -> dict:
    tui.send(b"\x1b")  # leave search mode so q is a quit, not a query character
    time.sleep(ESC_SETTLE_SECONDS)
    tui.send(b"q")
    if tui.wait_draining(timeout) is None:
        raise SystemExit(f"TUI did not exit after q (query {label})") from None
    captured = tui.drain_after_exit()
    stderr = tui.child.stderr.read() if tui.child.stderr is not None else b""
    if tui.child.returncode != 0:
        raise SystemExit(
            f"TUI run failed (query {label}): exit {tui.child.returncode}: "
            f"{stderr.decode(errors='replace')[:ERROR_EXCERPT_CHARS]}"
        )
    return {
        "query": label,
        "echo_ms": echo_ms,
        "results_ms": results_ms,
        "transitions": transitions,
        "peak_rss_kb": sampler.peak_rss_kb,
        "peak_cpu_pct": sampler.peak_cpu_pct,
        "peak_threads": sampler.peak_threads,
        "process_count": sampler.process_count,
        "output_bytes": output_bytes + len(captured),
        "digest": digest,
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


def run_startup_case(binary: str, fixture: str, startup_wait: float, timeout: float) -> None:
    tui = TuiProcess(binary, fixture)
    try:
        time.sleep(startup_wait)
        tui.send(b"q")
        if tui.wait_draining(timeout) is None:
            raise SystemExit("TUI did not exit after documented q key") from None
        captured = tui.drain_after_exit()
        stderr = tui.child.stderr.read() if tui.child.stderr is not None else b""
        if tui.child.returncode != 0 or b"Sessions" not in captured or b"Preview" not in captured:
            raise SystemExit(
                f"TUI startup failed with exit {tui.child.returncode}: {stderr.decode(errors='replace')}"
            )
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
    args = parser.parse_args()
    if args.measure_latency:
        report = measure_latency(
            args.binary, args.fixture, args.queries.split(","), args.repetitions,
            args.startup_wait, args.timeout,
        )
        print(json.dumps(report, separators=(",", ":")))
        return 0
    run_startup_case(args.binary, args.fixture, args.startup_wait, args.timeout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
