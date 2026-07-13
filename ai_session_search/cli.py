"""
Thin CLI layer - orchestrates library components without business logic.

Supports dual-ordering: both `aise search files` and `aise files search` work.
Root commands default to broad behavior; domain groups narrow the scope.

Copyright (c) 2026 Andrew Hundt
Licensed under the Apache License, Version 2.0
"""

from __future__ import annotations

import json
import os
import re as _re
import shutil
import sys
from dataclasses import dataclass
from enum import Enum
from pathlib import Path
from typing import Callable as _Callable, List, Optional, Set

import typer
import click
from rich.console import Console
from rich.markup import escape as _esc
from collections import defaultdict
from typer.core import HAS_RICH, TyperArgument, TyperGroup, TyperOption
import typer.rich_utils as _ru

from .engine import SessionRecoveryEngine, _check_redos_safe, _is_skill_injection
from .formatters import MessageFormatter, get_formatter
from .models import FileVersion, FilterSpec


class _CommandsFirstGroup(TyperGroup):
    """TyperGroup that renders Commands panels BEFORE Options panel in --help output."""

    def format_help(self, ctx: click.Context, formatter: click.HelpFormatter) -> None:
        if not HAS_RICH or self.rich_markup_mode is None:
            return super().format_help(ctx, formatter)

        console = _ru._get_rich_console()

        # Usage line
        console.print(
            _ru.Padding(_ru.highlighter(self.get_usage(ctx)), 1),
            style=_ru.STYLE_USAGE_COMMAND,
        )

        # Help text
        if self.help:
            console.print(
                _ru.Padding(
                    _ru.Align(
                        _ru._get_help_text(obj=self, markup_mode=self.rich_markup_mode),
                        pad=False,
                    ),
                    (0, 1, 1, 1),
                )
            )

        # Collect params into panels
        panel_to_arguments: defaultdict = defaultdict(list)
        panel_to_options: defaultdict = defaultdict(list)
        for param in self.get_params(ctx):
            if getattr(param, "hidden", False):
                continue
            panel_name = getattr(param, _ru._RICH_HELP_PANEL_NAME, None)
            if isinstance(param, TyperArgument):
                panel_to_arguments[panel_name or _ru.ARGUMENTS_PANEL_TITLE].append(param)
            elif isinstance(param, TyperOption):
                panel_to_options[panel_name or _ru.OPTIONS_PANEL_TITLE].append(param)

        # Arguments panels
        for panel_name, args in panel_to_arguments.items():
            _ru._print_options_panel(
                name=panel_name, params=args, ctx=ctx,
                markup_mode=self.rich_markup_mode, console=console,
            )

        # ── Commands panels FIRST ─────────────────────────────────────────────
        panel_to_commands: defaultdict = defaultdict(list)
        for command_name in self.list_commands(ctx):
            command = self.get_command(ctx, command_name)
            if command and not command.hidden:
                panel_name = (
                    getattr(command, _ru._RICH_HELP_PANEL_NAME, None)
                    or _ru.COMMANDS_PANEL_TITLE
                )
                panel_to_commands[panel_name].append(command)

        max_cmd_len = max(
            (len(cmd.name or "") for cmds in panel_to_commands.values() for cmd in cmds),
            default=0,
        )
        # Default commands panel first, then named panels
        for panel_name, commands in sorted(
            panel_to_commands.items(),
            key=lambda kv: (kv[0] != _ru.COMMANDS_PANEL_TITLE, kv[0]),
        ):
            _ru._print_commands_panel(
                name=panel_name, commands=commands,
                markup_mode=self.rich_markup_mode,
                console=console, cmd_len=max_cmd_len,
            )

        # ── Options panels AFTER commands ─────────────────────────────────────
        # Default options panel first, then named panels
        for panel_name, options in sorted(
            panel_to_options.items(),
            key=lambda kv: (kv[0] != _ru.OPTIONS_PANEL_TITLE, kv[0]),
        ):
            _ru._print_options_panel(
                name=panel_name, params=options, ctx=ctx,
                markup_mode=self.rich_markup_mode, console=console,
            )

        # Epilogue
        if self.epilog:
            lines = self.epilog.split("\n\n")
            epilogue = "\n".join([x.replace("\n", " ").strip() for x in lines])
            epilogue_text = _ru._make_rich_text(text=epilogue, markup_mode=self.rich_markup_mode)
            console.print(_ru.Padding(_ru.Align(epilogue_text, pad=False), 1))


app = typer.Typer(
    cls=_CommandsFirstGroup,
    help=(
        "Search and analyze AI sessions and conversations from Claude Code, AI Studio, and Gemini CLI.\n\n"
        "Sources auto-detected from standard locations. Run 'aise source list' to see what's active.\n"
        "Use --provider to filter: 'aise list --provider claude'  'aise stats --provider aistudio'"
    ),
)
files_app = typer.Typer(
    help="Search, extract, and track files that Claude wrote or edited across sessions.",
)
messages_app = typer.Typer(
    help="Search and read user/assistant conversation messages.",
)
export_app = typer.Typer(
    help="Export session messages to markdown. Use 'session' for one session, 'recent' for bulk.",
)
tools_app = typer.Typer(
    help="Search tool invocations (Bash, Edit, Write, Read, etc.) from Claude Code sessions.",
)
config_app = typer.Typer(
    help=(
        "View and manage the ai_session_search config file.\n\n"
        "Config file location (priority order):\n\n"
        "  1. --config CLI flag\n"
        "  2. AI_SESSION_SEARCH_CONFIG env var\n"
        "  3. OS default: ~/Library/Application Support/ai_session_search/config.json (macOS)\n"
        "               : ~/.config/ai_session_search/config.json (Linux)"
    ),
)


@config_app.callback(invoke_without_command=True)
def config_callback(ctx: typer.Context) -> None:
    """View and manage the ai_session_search config file."""
    if ctx.invoked_subcommand is None:
        typer.echo(ctx.get_help())
        raise typer.Exit(0)


source_app = typer.Typer(
    help="Add, remove, or list session source directories.",
)


@source_app.callback(invoke_without_command=True)
def source_callback(ctx: typer.Context) -> None:
    """Add, remove, or list session source directories."""
    if ctx.invoked_subcommand is None:
        typer.echo(ctx.get_help())
        raise typer.Exit(0)

slash_app = typer.Typer(
    help="Search and inspect slash command invocations across sessions.",
)

app.add_typer(files_app, name="files", rich_help_panel="Domain Groups")
app.add_typer(messages_app, name="messages", rich_help_panel="Domain Groups")
app.add_typer(export_app, name="export", rich_help_panel="Domain Groups")
app.add_typer(tools_app, name="tools", rich_help_panel="Domain Groups")
app.add_typer(slash_app, name="commands", rich_help_panel="Domain Groups")  # 'commands' = slash commands
app.add_typer(slash_app, name="slash", rich_help_panel="Domain Groups")     # 'slash' alias
app.add_typer(config_app, name="config", rich_help_panel="Configuration")
app.add_typer(source_app, name="source", rich_help_panel="Source Management")

# On Windows with legacy cp1252 console, Unicode box-drawing chars (e.g. ─)
# crash Rich. Detect once at import time and use ASCII-safe alternatives.
_SAFE_BOX = bool(sys.stdout.encoding and "utf" not in sys.stdout.encoding.lower())
_SEPARATOR = "-" * 60 if _SAFE_BOX else "─" * 60


def _console(**kwargs) -> Console:
    """Create a Console with platform-safe defaults (ASCII box chars on Windows cp1252)."""
    kwargs.setdefault("safe_box", _SAFE_BOX)
    return Console(**kwargs)


console = _console()
err_console = _console(stderr=True)


def _get_render_console() -> Console:
    """Return a Console sized to the real terminal width.

    When stdout is a TTY, returns the shared ``console`` (self-sizing).
    When stdout is piped, tries stderr's terminal size (fd 2) so the width
    reflects the user's actual window rather than a hardcoded guess.
    Falls back to 120 columns if no terminal is detectable on any fd.
    """
    if sys.stdout.isatty():
        return console
    try:
        width = os.get_terminal_size(2).columns  # stderr fd when stdout is piped
    except OSError:
        width = 120
    return _console(width=width)


# ── aise source CRUD commands ──────────────────────────────────────────────

@source_app.command("list")
def source_list(fmt: Optional[str] = typer.Option(None, "--format", "-f", help="Output format: table, json, plain. Default: plain.")) -> None:
    """Show all active sources (auto-detected + configured).

    Example:
        aise source list
        aise source list --format json
    """
    from ai_session_search.engine import _discover_sources
    from ai_session_search.config import load_config
    cfg = load_config()
    effective = _discover_sources(cfg)
    sd = effective.get("source_dirs", {})
    explicit_sd = cfg.get("source_dirs", {})

    from ai_session_search.config import resolve_claude_dir
    _claude_base = resolve_claude_dir()
    rows: list[dict] = [
        {"source": "claude", "type": "claude",
         "path": str(_claude_base / "projects"),
         "configured": "auto", "exists": (_claude_base / "projects").exists()}
    ]
    for src_type in ("aistudio", "gemini_cli"):
        paths = sd.get(src_type, [])
        if isinstance(paths, str):
            paths = [paths]
        explicit = explicit_sd.get(src_type, [])
        if isinstance(explicit, str):
            explicit = [explicit]
        # Deduplicate paths while preserving order
        seen_paths: set = set()
        for p in paths:
            if p in seen_paths:
                continue
            seen_paths.add(p)
            rows.append({
                "source": src_type,
                "type": src_type,
                "path": p,
                "configured": "explicit" if p in explicit else "auto-discovered",
                "exists": Path(p).exists(),
            })

    if fmt == "json":
        console.print(json.dumps(rows, indent=2))
        return

    if fmt in ("plain", "csv") or fmt is None:
        for r in rows:
            exists = "yes" if r["exists"] else "no"
            console.print(f"{r['type']}  {r['path']}  {r['configured']}  {exists}", markup=False)
        return

    from rich.table import Table
    table = Table(title="Active Session Sources")
    table.add_column("Type", style="cyan")
    table.add_column("Path")
    table.add_column("How Added", style="dim")
    table.add_column("Exists")
    for r in rows:
        exists_str = "[green]yes[/green]" if r["exists"] else "[red]no[/red]"
        table.add_row(r["type"], r["path"], r["configured"], exists_str)
    console.print(table)


@source_app.command("scan")
def source_scan(
    save: bool = typer.Option(False, "--save", help="Save all found sources to config automatically."),
) -> None:
    """Scan standard locations and report newly discoverable sources.

    Forces a full filesystem scan (bypasses the 24-hour auto-discovery cache)
    and reports any source directories not yet in your explicit config.

    Providers scanned:
      gemini_cli  ~/.gemini/tmp/
      aistudio    ~/Downloads/*Google AI Studio*, ~/Library/CloudStorage/GoogleDrive-*/...

    Without --save: shows what would be added (dry-run).
    With    --save: writes found paths to config as explicit entries.

    To disable auto-discovery for a provider: aise source disable aistudio
    To re-enable:                             aise source enable aistudio
    To view config file path:                 aise config path
    To view full config:                      aise config show

    Example:
        aise source scan
        aise source scan --save
    """
    from ai_session_search.engine import _discover_sources
    from ai_session_search.config import load_config
    cfg = load_config()
    explicit_sd = cfg.get("source_dirs", {})
    # force=True bypasses the 24-hour cache and always runs the full filesystem scan
    discovered = _discover_sources(cfg, force=True).get("source_dirs", {})

    new_sources: list[tuple[str, str]] = []
    for src_type in ("aistudio", "gemini_cli"):
        discovered_paths = discovered.get(src_type, [])
        explicit_paths = explicit_sd.get(src_type, [])
        if isinstance(discovered_paths, str):
            discovered_paths = [discovered_paths]
        if isinstance(explicit_paths, str):
            explicit_paths = [explicit_paths]
        for p in discovered_paths:
            if p not in explicit_paths:
                new_sources.append((src_type, p))

    if not new_sources:
        console.print("[green]No new sources found. Config is up to date.[/green]")
        return

    console.print(f"[bold]Found {len(new_sources)} new source(s):[/bold]")
    for src_type, path in new_sources:
        console.print(f"  [{src_type}] {path}")

    if save:
        _save_sources_to_config(new_sources, cfg)
        console.print("[green]Saved to config.[/green]")
    else:
        console.print("\nRun [bold]aise source scan --save[/bold] to add all, "
                      "or [bold]aise source add <path>[/bold] to add individually.")


@source_app.command("add")
def source_add(
    path: str = typer.Argument(..., help="Path to session directory to add."),
    src_type: str = typer.Option(
        "", "--type", "-t",
        help="Source type: aistudio, gemini. Auto-detected if not specified."
    ),
) -> None:
    """Add a session directory to config.

    Adding an explicit path locks that source type to only use your specified
    paths — auto-discovery is skipped once any explicit path exists for that type.
    To re-enable auto-discovery alongside explicit paths, remove all explicit paths
    for the type, or use 'aise source enable <type>'.

    Example:
        aise source add ~/Downloads/aistudio_sessions/Google\\ AI\\ Studio
        aise source add "~/Library/CloudStorage/GoogleDrive-me@gmail.com/My Drive/Google AI Studio"
        aise source add ~/.gemini/tmp --type gemini
    """
    from ai_session_search.config import load_config
    resolved = str(Path(path).expanduser().resolve())
    if not Path(resolved).exists():
        err_console.print(f"[red]Path does not exist: {resolved}[/red]")
        raise typer.Exit(code=1)

    # Auto-detect type if not specified
    effective_type = src_type
    if not effective_type:
        if ".gemini" in resolved or "gemini" in resolved.lower():
            effective_type = "gemini_cli"
        else:
            effective_type = "aistudio"  # default for unknown paths

    cfg = load_config()
    sd = dict(cfg.get("source_dirs", {}))
    existing = sd.get(effective_type, [])
    if isinstance(existing, str):
        existing = [existing]
    if resolved not in existing:
        existing.append(resolved)
        sd[effective_type] = existing
        cfg["source_dirs"] = sd
        _write_config(cfg)
        console.print(f"[green]Added [{effective_type}] {resolved}[/green]")
    else:
        console.print(f"[yellow]Already in config: {resolved}[/yellow]")


@source_app.command("remove")
def source_remove(
    path: str = typer.Argument(..., help="Path or partial path to remove from config."),
) -> None:
    """Remove a session directory from config.

    Example:
        aise source remove ~/Downloads/old_sessions
    """
    from ai_session_search.config import load_config
    # Use resolve() for exact path match — prevents substring collisions
    # e.g. removing /data would NOT accidentally remove /data_backup
    resolved = str(Path(path).expanduser().resolve())
    cfg = load_config()
    sd = dict(cfg.get("source_dirs", {}))
    removed = False
    for src_type in list(sd.keys()):
        paths = sd[src_type]
        if isinstance(paths, str):
            paths = [paths]
        # Exact match only (both resolved to absolute): no substring matching
        new_paths = [p for p in paths if Path(p).expanduser().resolve() != Path(resolved)]
        if len(new_paths) != len(paths):
            sd[src_type] = new_paths if new_paths else None
            removed = True
    cfg["source_dirs"] = {k: v for k, v in sd.items() if v}
    if removed:
        _write_config(cfg)
        console.print(f"[green]Removed: {resolved}[/green]")
    else:
        console.print(f"[yellow]Not found in config: {resolved}[/yellow]")


_VALID_SOURCE_TYPES = ("aistudio", "gemini_cli", "gemini")


@source_app.command("disable")
def source_disable(
    src_type: str = typer.Argument(..., help="Source type to disable auto-discovery for: aistudio, gemini_cli"),
) -> None:
    """Disable auto-discovery for a source type.

    Writes an empty list to config so aise no longer auto-discovers this source
    type on startup. Existing explicit paths are removed too.
    To re-enable, run: aise source enable <type>

    Example:
        aise source disable aistudio
        aise source disable gemini_cli
    """
    from ai_session_search.config import load_config
    effective_type = "gemini_cli" if src_type in ("gemini", "gemini_cli") else src_type
    if effective_type not in ("aistudio", "gemini_cli"):
        err_console.print(f"[red]Unknown type {src_type!r}. Use: aistudio, gemini_cli[/red]")
        raise typer.Exit(code=1)
    cfg = load_config()
    sd = dict(cfg.get("source_dirs", {}))
    sd[effective_type] = []  # empty list disables auto-discovery
    cfg["source_dirs"] = sd
    _write_config(cfg)
    console.print(f"[yellow]Auto-discovery disabled for [{effective_type}]. "
                  f"Run 'aise source enable {effective_type}' to re-enable.[/yellow]")


@source_app.command("enable")
def source_enable(
    src_type: str = typer.Argument(..., help="Source type to re-enable auto-discovery for: aistudio, gemini_cli"),
) -> None:
    """Re-enable auto-discovery for a source type.

    Removes the config entry that was blocking auto-discovery so aise will
    scan standard locations again. To add an explicit path instead, use:
        aise source add <path> --type <type>

    Example:
        aise source enable aistudio
        aise source enable gemini_cli
    """
    from ai_session_search.config import load_config
    effective_type = "gemini_cli" if src_type in ("gemini", "gemini_cli") else src_type
    if effective_type not in ("aistudio", "gemini_cli"):
        err_console.print(f"[red]Unknown type {src_type!r}. Use: aistudio, gemini_cli[/red]")
        raise typer.Exit(code=1)
    cfg = load_config()
    sd = dict(cfg.get("source_dirs", {}))
    # Remove the key entirely so _discover_sources will auto-discover again
    sd.pop(effective_type, None)
    cfg["source_dirs"] = {k: v for k, v in sd.items() if v is not None}
    _write_config(cfg)
    console.print(f"[green]Auto-discovery enabled for [{effective_type}]. "
                  f"Run 'aise source scan' to preview discovered paths.[/green]")


def _save_sources_to_config(new_sources: list[tuple[str, str]], cfg: dict) -> None:
    """Helper: save discovered sources to config.json."""
    sd = dict(cfg.get("source_dirs", {}))
    for src_type, path in new_sources:
        existing = sd.get(src_type, [])
        if isinstance(existing, str):
            existing = [existing]
        if path not in existing:
            existing.append(path)
        sd[src_type] = existing
    cfg["source_dirs"] = sd
    _write_config(cfg)


def _resolve_config_path() -> Path:
    """Return the config file path — delegates to config.get_config_path().

    Single implementation shared by _write_config() and config show/path commands
    so read path == write path in all invocation modes.
    """
    from ai_session_search.config import get_config_path
    return get_config_path()


def _write_config(cfg: dict) -> None:
    """Write config dict to the same path load_config() reads from.

    Creates parent directories if needed. Safe to call even if the file
    does not exist yet — this is the only correct place to create it.
    (set_config_path does NOT create files; config_init and source-mutation
    commands call _write_config which creates the file on first write.)

    Delegates to config.write_config() which updates the in-process cache
    so the next load_config() returns the written dict without a disk re-read.
    """
    from ai_session_search.config import write_config as _wc
    _wc(cfg)


# ── CLI rendering infrastructure ──────────────────────────────────────────────

@dataclass
class ColumnSpec:
    """One column in a Rich table: header text + optional display hints."""

    header: str
    style: str = ""
    no_wrap: bool = False
    justify: str = "left"
    overflow: str = "fold"          # Rich overflow mode: fold, crop, ellipsis, ignore
    min_width: Optional[int] = None  # Minimum column width in chars (prevents Rich collapsing)
    ratio: Optional[int] = None      # Proportional width hint when expand=True (e.g. ratio=4 for wider columns)


@dataclass
class TableSpec:
    """Render spec for a list of model objects: table, JSON, CSV, or plain text."""

    title_template: str
    columns: List[ColumnSpec]
    row_fn: _Callable
    summary_template: Optional[str] = None
    plain_fn: Optional[_Callable] = None


def _render_output(
    items: list,
    fmt: Optional[str],
    spec: "TableSpec",
    empty_msg: str = "No results found",
    output: Optional[str] = None,
) -> None:
    """Render items in the requested format — eliminates repeated json/csv/table branching.

    Args:
        output: Optional file path. When set with fmt="json", writes JSON to file
                instead of stdout. For other formats, writes Rich text to file.
    """
    if fmt is None:
        fmt = _cfg_default("format", "plain")
    if not items:
        console.print(f"[yellow]{empty_msg}[/yellow]")
        return

    def _to_dict(x):
        if hasattr(x, "to_dict"):
            return x.to_dict()
        if isinstance(x, dict):
            return x
        return vars(x)

    dicts = [_to_dict(x) for x in items]
    if fmt == "json":
        json_text = json.dumps(dicts, indent=2) + "\n"
        # Write to file if --output given, otherwise stdout
        if output:
            Path(output).expanduser().write_text(json_text, encoding="utf-8")
        else:
            sys.stdout.write(json_text)
        return
    if fmt == "jsonl":
        jsonl_lines = [json.dumps(d, default=str) for d in dicts]
        jsonl_text = "\n".join(jsonl_lines) + "\n"
        if output:
            Path(output).expanduser().write_text(jsonl_text, encoding="utf-8")
        else:
            sys.stdout.write(jsonl_text)
        return
    if fmt in ("csv", "plain"):
        lines = [spec.plain_fn(d) if spec.plain_fn else "  ".join(str(v) for v in spec.row_fn(d)) for d in dicts]
        for line in lines:
            console.print(line, markup=False)
        if output:
            Path(output).expanduser().write_text("\n".join(lines) + "\n", encoding="utf-8")
        return
    # Default: Rich table
    from rich.table import Table
    # When stdout is not a TTY (piped to head, grep, file, etc.), Rich defaults to 80 cols.
    # Use 120 cols instead — prevents column crushing while staying readable in most contexts.
    # sys already imported at cli.py:17; Console already imported at cli.py:24
    render_console = _console(record=True, width=120) if output else _get_render_console()
    # Pre-compute all rows once; used for both column suppression check and row insertion.
    # Escape Rich markup in cell values to prevent MarkupError on content like [/spawn-session].
    all_rows = [[_esc(str(v)) for v in spec.row_fn(d)] for d in dicts]
    n_cols = len(spec.columns)
    # Suppress columns where every row value is "" (falsy); avoids invisible 0-width headers.
    # Note: "0" is truthy — message count columns with all-zero values are NOT suppressed.
    active = [i for i in range(n_cols) if any(row[i] for row in all_rows)]
    if not active:
        active = list(range(n_cols))  # guard: if all columns are empty, show all
    # expand=True: table fills full console width; flexible columns get proportional space
    table = Table(title=spec.title_template.format(n=len(items)), expand=True)
    for i in active:
        col = spec.columns[i]
        table.add_column(
            col.header,
            style=col.style or None,
            no_wrap=col.no_wrap,
            justify=col.justify,
            overflow=col.overflow,
            min_width=col.min_width,
            ratio=col.ratio,
        )
    for row in all_rows:
        table.add_row(*[row[i] for i in active])
    render_console.print(table)
    if spec.summary_template:
        render_console.print(f"\n[bold]{spec.summary_template.format(n=len(items))}[/bold]")
    if output:
        _write_output(render_console, output)


def _spec_with_full_uuid(spec: "TableSpec", col_idx: int = 0) -> "TableSpec":
    """Return a copy of spec where the session ID column shows full 36-char UUID.

    col_idx: index of the Session column in spec.columns (default 0).
    The original row_fn may truncate the session ID; this wrapper replaces that
    value with d["session_id"] from the raw dict (the full UUID).
    """
    import dataclasses as _dc
    cols = list(spec.columns)
    cols[col_idx] = _dc.replace(cols[col_idx], min_width=36, no_wrap=True)
    orig_fn = spec.row_fn
    def full_row_fn(d: dict) -> list:
        row = list(orig_fn(d))
        row[col_idx] = d.get("session_id", row[col_idx])
        return row
    return _dc.replace(spec, columns=cols, row_fn=full_row_fn)


_OPT_FULL_UUID = typer.Option(
    False, "--full-uuid/--no-full-uuid",
    help="Show full 36-char session UUID instead of 8-char prefix.",
)


def _register_alias(sub_app: "typer.Typer", func: _Callable, *names: str, hidden_after_first: bool = True) -> None:
    """Register func as a command under each name in names on sub_app.

    The first name is the canonical command shown in --help; all subsequent names
    are hidden aliases (not listed in help but still functional).
    """
    for i, name in enumerate(names):
        sub_app.command(name, hidden=(hidden_after_first and i > 0))(func)


# ── Module-level TableSpec constants ─────────────────────────────────────────

def _format_ts(ts: str) -> str:
    """Format ISO 8601 timestamp for display as 'YYYY-MM-DD HH:MM'. Never raises.

    Handles all known variants (Z suffix, +00:00, bare prefix, empty string)
    by slicing the first 16 chars then replacing 'T' with a space. Safe on any input.

    Examples:
        "2026-03-01T14:23:45.123456Z"     -> "2026-03-01 14:23"
        "2026-03-01T14:23:45.123+00:00"   -> "2026-03-01 14:23"
        "2026-02-23T04:07"                -> "2026-02-23 04:07"
        "2026-03-01"                      -> "2026-03-01"   (date-only fallback)
        ""                                -> ""
    """
    if not ts:
        return ""
    s = ts[:16]
    return s.replace("T", " ") if "T" in s else s


# ── DRY date/time filter flags and normalizer ────────────────────────────────

_OPT_SINCE = typer.Option(
    None, "--since",
    help=(
        "Lower bound of date range (inclusive). "
        "Accepts: ISO date (2026-01-15), partial date (2026-01, 2026), "
        "EDTF patterns (202X, 19XX, 2026-01-1X), durations (7d, 2w, 1m, 24h, 30min), "
        "EDTF interval (2026-01/2026-03, sets both bounds). "
        "Run 'aise dates' for full format reference."
    ),
)
_OPT_UNTIL = typer.Option(
    None, "--until",
    help=(
        "Upper bound of date range (inclusive). "
        "Same formats as --since (no interval syntax). "
        "Run 'aise dates' for full format reference."
    ),
)
_OPT_WHEN = typer.Option(
    None, "--when",
    help=(
        "Filter to an entire EDTF period — sets both lower and upper bounds. "
        "Best for unspecified-digit patterns: --when 202X (whole 2020s decade), "
        "--when 2026-01-1X (Jan 10-19 2026), --when 2026-01 (all of January). "
        "Run 'aise dates' for full format reference."
    ),
)
# Hidden backward-compatible aliases for --since/--until (not shown in --help).
# ``--since`` and ``--until`` are the CANONICAL date options; ``--after``/``--before``
# are kept only for scripts/users who adopted the old names and will be retired in a
# future major release. New code must use --since/--until exclusively.
_OPT_AFTER  = typer.Option(None, "--after",  hidden=True)
_OPT_BEFORE = typer.Option(None, "--before", hidden=True)

# ── DRY shared option constants ───────────────────────────────────────────────
# Single source of truth for common options used across many commands.

_OPT_PROVIDER = typer.Option(
    None, "--provider",
    help="Sessions from: claude | aistudio | gemini | all. Overrides global --provider.",
)

#: Standard output format option for commands that support table/json/csv/plain.
_OPT_FORMAT = typer.Option(
    None, "--format", "-f",
    help="Output format: table, json, csv, plain. Default: table (or config 'defaults.format')",
)

#: Max-chars truncation option for message/result content preview.
_OPT_MAX_CHARS = typer.Option(
    None, "--max-chars",
    help="Truncate content to this many characters. 0 = full. Default: 0 (or config 'defaults.max_chars')",
)

#: Output file tee option — write output to FILE in addition to stdout.
_OPT_OUTPUT = typer.Option(
    None, "--output", "-o",
    help="Write output to FILE in addition to stdout (tee). Example: --output ~/audit.txt",
)


_OPT_IDS_ONLY = typer.Option(
    False, "--ids-only",
    help="Output only session IDs, one per line. Designed for piping to other aise commands "
         "via xargs or shell loops. Suppresses all other output.",
)


def _opt_limit(default: Optional[int] = None, help_noun: str = "results") -> typer.Option:
    """Factory for --limit option. None = unlimited (list[:None] returns full list)."""
    return typer.Option(
        default, "--limit",
        help=f"Max {help_noun} to return. Default: unlimited.",
    )


_OPT_SESSION = typer.Option(
    None, "--session", "-s",
    help="Limit to one session ID (prefix match). Find IDs via 'aise list'.",
)


_OPT_DRY_RUN = typer.Option(
    False, "--dry-run",
    help="Show what would happen without making changes.",
)


def _output_ids_only(items: list, id_field: str = "session_id") -> None:
    """Output unique session IDs, one per line. For pipe-friendly output."""
    seen: set[str] = set()
    for item in items:
        sid = getattr(item, id_field, None) or (item.get(id_field, "") if isinstance(item, dict) else "")
        if sid and sid not in seen:
            seen.add(sid)
            typer.echo(sid)


class MsgFilterType(str, Enum):
    """CLI filter enum for --type on messages search and messages timeline.

    Named MsgFilterType (not MessageType) to avoid collision with models.MessageType
    which has USER/ASSISTANT/SYSTEM values used at the data layer.
    """
    user = "user"
    assistant = "assistant"
    tool = "tool"
    slash = "slash"           # real slash command invocations (<command-name> XML tag); Claude-only
    compaction = "compaction" # context-compaction summary messages; Claude-only


def _write_output(console: Console, output_path: Optional[str]) -> None:
    """If output_path given, write recorded console output to file (tee helper)."""
    if output_path:
        text = console.export_text(clear=False)
        Path(output_path).expanduser().write_text(text, encoding="utf-8")


def _is_compaction_content(content: Optional[str]) -> bool:
    """Return True if content looks like a context-compaction summary message.

    Checks the string prefix used in compaction summaries. The isCompactSummary
    JSON field is checked separately in engine.py; this helper covers the CLI layer
    where only the content string is available (e.g., timeline event objects).
    Guards against None content.
    """
    return (content or "").startswith("This session is being continued")


def _render_invocations_with_context(
    invocations: list,
    context_after: int,
    engine: "AISession",
    output: Optional[str] = None,
    max_chars: int = 0,
) -> None:
    """Render invocations with trailing context messages.

    Used by both `messages planning --context-after` and `commands context`.
    """
    truncate = max_chars if max_chars > 0 else None
    out_console = _console(record=bool(output))
    if not invocations:
        out_console.print("[yellow]No invocations found.[/yellow]")
        _write_output(out_console, output)
        return
    for inv in invocations:
        path_display = _esc(inv.cwd) if inv.cwd else _project_display(inv.project_dir)
        line = (
            f"\n[bold cyan]{_esc(inv.command)}[/bold cyan] "
            f"[dim]{inv.timestamp[:19]}[/dim] "
            f"[blue]{inv.session_id[:8]}[/blue]"
        )
        if path_display:
            line += f" [dim]{path_display}[/dim]"
        if inv.git_branch:
            line += f" [green]{_esc(inv.git_branch)}[/green]"
        out_console.print(line)
        if inv.args:
            out_console.print(f"  [dim]args:[/dim] {_esc(inv.args)}")
        messages = engine.get_messages(inv.session_id)
        raw_after = [m for m in messages if (m.timestamp or "") > inv.timestamp]
        # Filter out skill injection messages (SKILL.md content injected after slash
        # commands). These are noise for auditing — users want the AI response, not
        # the injected skill body.
        after_msgs: list = []
        prev_slash = True  # inv itself is a slash command
        for m in raw_after:
            mtype = m.type.value if hasattr(m.type, "value") else str(m.type)
            if _is_skill_injection({"type": mtype}, m.content, prev_slash):
                prev_slash = False
                continue
            prev_slash = "<command-name>" in (m.content or "")
            after_msgs.append(m)
            if len(after_msgs) >= context_after:
                break
        for m in after_msgs:
            content = (m.content or "")[:truncate]
            out_console.print(f"  [dim][{m.type.value}] {(m.timestamp or '')[:19]}[/dim]")
            out_console.print(f"  [dim]{_esc(content)}[/dim]")
    out_console.print(f"\n[bold]Found {len(invocations)} invocations[/bold]")
    _write_output(out_console, output)


def _cfg_default(key: str, fallback):
    """Read user preference from config 'defaults' section, returning fallback if absent.

    Lazy-imports config to avoid circular imports and import-time side effects.
    Config key: 'defaults' -> {key: value}
    Example: {"defaults": {"format": "json", "max_chars": 500, "provider": "claude"}}
    """
    from ai_session_search.config import get_config_section  # lazy: config imported later in file
    return get_config_section("defaults", {}).get(key, fallback)


def _normalize_date_range(
    since: Optional[str] = None,
    until: Optional[str] = None,
    when:  Optional[str] = None,
    after:  Optional[str] = None,   # hidden alias for --since
    before: Optional[str] = None,   # hidden alias for --until
) -> tuple:
    """Return (resolved_after, resolved_before) ISO strings for engine filtering.

    Priority:
      --when: sets BOTH bounds from one EDTF expression (lower + upper bound).
      --since (or --after): lower bound, overrides --when lower if also given.
      --until (or --before): upper bound, overrides --when upper if also given.
      EDTF interval in --since (A/B) sets both bounds like --when.

    Raises typer.Exit(1) with Rich stderr on invalid input.
    """
    from ai_session_search.engine import _parse_date_input

    def _fail(label: str, exc: Exception) -> None:
        _console(stderr=True).print(f"[red]{label}: {exc}[/red]")
        raise typer.Exit(1) from exc

    resolved_after: Optional[str] = None
    resolved_before: Optional[str] = None

    # --when: sets BOTH bounds from one EDTF expression
    if when:
        try:
            result = _parse_date_input(when, "start")
        except ValueError as exc:
            _fail("--when", exc)
            return None, None  # unreachable but satisfies type checker
        if isinstance(result, tuple):
            resolved_after, resolved_before = result
        else:
            resolved_after = result
            try:
                resolved_before = _parse_date_input(when, "end")
            except ValueError as exc:
                _fail("--when (upper bound)", exc)

    # --since (or hidden --after): lower bound, overrides --when lower if given
    since_val = since or after
    if since_val:
        try:
            result = _parse_date_input(since_val, "start")
        except ValueError as exc:
            _fail("--since", exc)
            return None, None
        if isinstance(result, tuple):
            resolved_after, resolved_before = result   # interval syntax sets both
        else:
            resolved_after = result

    # --until (or hidden --before): upper bound, overrides --when upper if given
    until_val = until or before
    if until_val:
        try:
            result = _parse_date_input(until_val, "end")
        except ValueError as exc:
            _fail("--until", exc)
            return None, None
        if isinstance(result, tuple):
            _console(stderr=True).print(
                "[red]--until: interval syntax (A/B) not supported here; use --since A/B[/red]"
            )
            raise typer.Exit(1)
        resolved_before = result

    # Warn if the range is inverted (after > before) — no sessions will match.
    if resolved_after and resolved_before and resolved_after > resolved_before:
        _console(stderr=True).print(
            f"[yellow]Warning: --since/--after ({resolved_after[:10]}) is later than "
            f"--until/--before ({resolved_before[:10]}); the date range is inverted and "
            "no sessions will match.[/yellow]"
        )

    return resolved_after, resolved_before


def _project_display(encoded_dir: str) -> str:
    """Return a short human-readable project name from Claude's encoded dir name.

    Delegates to SessionRecoveryEngine.extract_project_name(), then truncates
    to 30 characters for table display.  The raw ``project_dir`` value is still
    stored in the to_dict() output so JSON consumers can access it.
    """
    decoded = SessionRecoveryEngine.extract_project_name(encoded_dir)
    return (decoded[-30:] if len(decoded) > 30 else decoded)


def _session_path_display(d: dict) -> str:
    """Best available working path for a session row.

    Priority: cwd (actual filesystem path) > project_display > project_dir.
    cwd is the real path Claude was running in — most useful for navigation.
    AI Studio/Gemini sessions typically have empty cwd; fall back to decoded name.
    """
    cwd = d.get("cwd", "")
    if cwd:
        return cwd
    return _project_display(d.get("project_display") or d.get("project_dir", ""))


_LIST_SPEC = TableSpec(
    title_template="Sessions ({n} found)",
    columns=[
        ColumnSpec("Session", style="cyan", no_wrap=True, min_width=8),    # 8-char prefix (matches _CORRECTIONS_SPEC)
        ColumnSpec("Provider", style="magenta", no_wrap=True),             # source provider
        ColumnSpec("Path", style="blue", ratio=4),                          # cwd or decoded project; ratio=4 ensures proportionally wider column
        ColumnSpec("Branch", style="green"),
        ColumnSpec("Date", style="dim", no_wrap=True, min_width=16),       # "YYYY-MM-DD HH:MM"
        ColumnSpec("Messages", justify="right"),
        ColumnSpec("Compact", style="dim"),
    ],
    row_fn=lambda d: [
        d["session_id"][:8],                                               # 8-char prefix; full ID via: aise messages get <prefix>
        d.get("provider", "claude"),                                        # source provider
        _session_path_display(d),                                           # cwd > project_display
        d.get("git_branch", ""),
        _format_ts(d.get("timestamp_first", "")),                          # "YYYY-MM-DD HH:MM"
        str(d.get("message_count", 0)),
        "\u2713" if d.get("has_compact_summary") else "",
    ],
    summary_template="Found {n} sessions",
)

_CORRECTIONS_SPEC = TableSpec(
    title_template="User Corrections ({n} found)",
    columns=[
        ColumnSpec("Timestamp", style="dim", no_wrap=True),
        ColumnSpec("Session", style="cyan", no_wrap=True),
        ColumnSpec("Category", style="yellow"),
        ColumnSpec("Pattern", style="red"),
        ColumnSpec("Message"),
    ],
    row_fn=lambda d: [
        d["timestamp"][:19],
        d["session_id"][:8],
        d["category"],
        d["matched_pattern"],
        d["content"][:80],
    ],
    summary_template="Found {n} corrections",
)

_PLANNING_SPEC = TableSpec(
    title_template="Planning Command Usage ({n} commands)",
    columns=[
        ColumnSpec("Command", style="cyan"),
        ColumnSpec("Count", justify="right"),
        ColumnSpec("Sessions", justify="right"),
        ColumnSpec("Projects", justify="right"),
    ],
    row_fn=lambda d: [
        d["command"],
        str(d["count"]),
        str(d["unique_sessions"]),
        str(d["unique_projects"]),
    ],
)

_INVOCATIONS_SPEC = TableSpec(
    title_template="Slash Command Invocations ({n} found)",
    columns=[
        ColumnSpec("Timestamp", style="dim", no_wrap=True),
        ColumnSpec("Session", style="cyan", no_wrap=True, min_width=8),
        ColumnSpec("Command", style="bold cyan"),
        ColumnSpec("Args"),
        ColumnSpec("Path", style="blue", ratio=3, min_width=20, overflow="ellipsis"),
        ColumnSpec("Branch", style="green"),
    ],
    row_fn=lambda d: [
        d.get("timestamp", "")[:19],
        d.get("session_id", "")[:8],
        d.get("command", ""),
        d.get("args", ""),
        d.get("cwd", "") or _project_display(d.get("project_dir", "")),
        d.get("git_branch", ""),
    ],
    summary_template="Found {n} invocations",
)


# Module-level overrides set by global options
_g_claude_dir: Optional[str] = None

# Import config loading and session backend from shared modules
from ai_session_search.config import load_config, set_config_path, get_config_section, invalidate_config_cache  # noqa: E402
from ai_session_search.engine import AISession  # noqa: E402


# ── Root app callback (global options + composition root) ────────────────────────

def _version_callback(value: bool) -> None:
    """Eager --version callback: prints 'aise X.Y.Z' and exits before engine builds."""
    if value:
        from ai_session_search import __version__
        typer.echo(f"aise {__version__}")
        raise typer.Exit()


@app.callback(invoke_without_command=True)
def app_callback(
    ctx: typer.Context,
    version: Optional[bool] = typer.Option(
        None, "--version", "-V",
        is_eager=True,
        callback=_version_callback,
        help="Show version and exit.",
    ),
    claude_dir: Optional[str] = typer.Option(
        None, "--claude-dir",
        help="Path to Claude config dir. Default: ~/.claude.",
        envvar="CLAUDE_CONFIG_DIR",
    ),
    config: Optional[str] = typer.Option(
        None, "--config",
        help="Path to config JSON. Default: OS app config dir (macOS: ~/Library/Application Support/ai_session_search/config.json).",
        envvar="AI_SESSION_SEARCH_CONFIG",
    ),
    provider: Optional[str] = typer.Option(
        None, "--provider",
        help=(
            "Sessions from: claude | aistudio | gemini | all. Default: all "
            "(or config 'defaults.provider'). "
            "Most commands also accept --provider directly: 'aise list --provider claude'."
        ),
    ),
) -> None:
    """Composition root: builds AISession once, injects into ctx.obj for all subcommands.

    Priority for --config flag:
      1. --config CLI flag (absolute priority)
      2. AI_SESSION_SEARCH_CONFIG env var
      3. typer.get_app_dir("ai_session_search") / "config.json" (OS default)

    This ensures all parts of the app use the same config loader (no dual-loading bugs).
    """
    global _g_claude_dir
    _g_claude_dir = claude_dir
    # Notify shared config module about --config flag so all parts of app use same config
    set_config_path(config)

    # ── COMPOSITION ROOT: Build AISession ONCE, inject into ctx.obj ──
    # ctx.obj is inherited by all child contexts (sub-apps, sub-commands)
    ctx.ensure_object(dict)
    cfg = load_config()  # already handles _g_config_path / env var priority
    # Resolve provider: CLI flag > config 'defaults.provider' > "all"
    if provider is None:
        provider = cfg.get("defaults", {}).get("provider", "all")
    ctx.obj["engine"] = AISession(source=provider, claude_dir=claude_dir, config=cfg)
    ctx.obj["source"] = provider  # can be accessed by commands for source filtering

    if ctx.invoked_subcommand is None:
        typer.echo(ctx.get_help())
        raise typer.Exit(0)


# ── Engine factory ────────────────────────────────────────────────────────────

def _resolve_engine(ctx: typer.Context, source: Optional[str] = None) -> Optional[AISession]:
    """Return engine from ctx.obj, rebuilding with a different source if --source given per-command."""
    if source:
        cfg = load_config()
        return AISession(source=source, config=cfg, claude_dir=_g_claude_dir)
    return ctx.obj.get("engine") if ctx.obj else None


def _get_engine(projects_dir: Optional[str] = None, recovery_dir: Optional[str] = None) -> SessionRecoveryEngine:
    """
    Get or create recovery engine.

    Priority for Claude config dir: --claude-dir CLI flag > CLAUDE_CONFIG_DIR env var > ~/.claude

    Also supports:
        AI_SESSION_SEARCH_PROJECTS: Path to Claude projects directory (overrides base dir)
        AI_SESSION_SEARCH_RECOVERY: Path to recovery directory (overrides base dir)
    """
    # Priority: --claude-dir (CLI) > CLAUDE_CONFIG_DIR (env) > config claude_dir > ~/.claude
    from ai_session_search.config import resolve_claude_dir
    base = resolve_claude_dir(override=_g_claude_dir)

    if projects_dir is None:
        projects_dir = os.getenv("AI_SESSION_SEARCH_PROJECTS")
        if projects_dir is None:
            projects_dir = str(base / "projects")

    if recovery_dir is None:
        recovery_dir = os.getenv("AI_SESSION_SEARCH_RECOVERY")
        if recovery_dir is None:
            recovery_dir = str(base / "recovery")

    # expanduser() so that env var values like "~/.claude/projects" work correctly
    return SessionRecoveryEngine(Path(projects_dir).expanduser(), Path(recovery_dir).expanduser())


# ── Shared helper functions ───────────────────────────────────────────────────

def _parse_session_set(s: Optional[str]) -> Set[str]:
    """Parse comma-separated session IDs string to set. Empty string or None -> empty set."""
    if not s:
        return set()
    return {x.strip() for x in s.split(",") if x.strip()}


def _strip_quotes(s: str) -> str:
    """Strip surrounding single or double quotes from a stripped string."""
    s = s.strip()
    if len(s) >= 2 and s[0] in ('"', "'") and s[-1] == s[0]:
        s = s[1:-1].strip()
    return s


def _parse_list_input(raw: str) -> List[str]:
    """Parse a list from raw text or a file path. Tries parsers in order:

    1. **File path** — if ``raw`` is an existing readable path, read its contents first.
    2. **JSON array** (``json.loads``) — handles ``["a", "b"]``, ``["a","b"]``, etc.
    3. **Python literal** (``ast.literal_eval``) — handles ``['a', 'b']``, mixed quotes.
    4. **CSV fallback** — handles ``a,b,c``, ``[a,b,c]`` (brackets stripped then split).

    Examples::

        "a,b,c"                         -> ["a", "b", "c"]         (CSV)
        '["/ar:pn", "/ar:pr"]'          -> ["/ar:pn", "/ar:pr"]    (JSON)
        "['/ar:pn', '/ar:pr']"          -> ["/ar:pn", "/ar:pr"]    (Python literal)
        '[/ar:pn, "/ar:pr"]'            -> ["/ar:pn", "/ar:pr"]    (CSV with brackets)
        "/path/to/list.json"            -> contents parsed above   (file path)

    Empty items (trailing commas, whitespace-only) are always discarded.
    """
    import ast

    s = raw.strip()

    # 1. File path: if the string points to an existing file, read it first
    try:
        p = Path(s).expanduser()
        if p.exists() and p.is_file():
            s = p.read_text(encoding="utf-8").strip()
    except (OSError, ValueError):
        pass  # not a valid path; continue with raw string

    # 2. JSON array (handles double-quoted items, no-space format)
    if s.startswith("["):
        try:
            parsed = json.loads(s)
            if isinstance(parsed, list):
                return [str(item).strip() for item in parsed if str(item).strip()]
        except (json.JSONDecodeError, ValueError):
            pass

        # 3. Python literal (handles single-quoted items, mixed quotes)
        try:
            parsed = ast.literal_eval(s)
            if isinstance(parsed, list):
                return [str(item).strip() for item in parsed if str(item).strip()]
        except (ValueError, SyntaxError):
            pass

        # 4a. CSV with brackets: strip outer brackets then split
        inner = s[1:-1] if s.endswith("]") else s[1:]
        return [_strip_quotes(item) for item in inner.split(",") if item.strip()]

    # 4b. Plain CSV
    return [_strip_quotes(item) for item in s.split(",") if item.strip()]


def _project_dir_name(path: str) -> str:
    """Convert a working directory path to the ~/.claude/projects/ directory name.

    Matches Claude Code's own encoding: replace /[^a-zA-Z0-9-]/g with '-'.
    Hyphens are preserved; slashes, dots, underscores, spaces all become '-'.

    Source: Claude Code TypeScript source, getProjectPath() in utils/path.ts

    Examples:
        /Users/alice/.claude          -> -Users-alice--claude
        /Users/me/my_project          -> -Users-me-my-project
        /var/www/my.site.com/public   -> -var-www-my-site-com-public
    """
    abs_path = os.path.abspath(path)
    return _re.sub(r'[^a-zA-Z0-9-]', '-', abs_path)


def _sessions_for_project(engine: "AISession", project_path: str) -> Set[str]:
    """Return all session IDs whose JSONL files live under the given project directory."""
    dir_name = _project_dir_name(project_path)
    project_dir = engine.projects_dir / dir_name
    if not project_dir.is_dir():
        return set()
    return {f.stem for f in project_dir.glob("*.jsonl")}


def _filter_versions(
    versions: List[FileVersion],
    project_sessions: Set[str] = frozenset(),
    session: Optional[str] = None,
    include_sessions: Set[str] = frozenset(),
    exclude_sessions: Set[str] = frozenset(),
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
) -> List[FileVersion]:
    """Filter FileVersion list by composable criteria. All filters are ANDed."""
    result = versions
    # --project: restrict to project's session IDs
    if project_sessions:
        result = [v for v in result if v.session_id in project_sessions]
    # --session: prefix match (convenience, single value)
    if session:
        result = [v for v in result if v.session_id.startswith(session)]
    # --include-sessions: keep only matching sessions (prefix match each)
    if include_sessions:
        result = [v for v in result if any(v.session_id.startswith(i) for i in include_sessions)]
    # --exclude-sessions: remove matching sessions
    if exclude_sessions:
        result = [v for v in result if not any(v.session_id.startswith(i) for i in exclude_sessions)]
    # --since / --until: filter by version timestamp.
    # FileVersion.timestamp uses space separator ("2026-01-15 14:23") while since/until
    # use "T" separator ("2026-01-15T00:00:00").  Normalise the comparison string to use
    # space so ASCII-order comparison is consistent: space (32) matches space (32).
    if since:
        since_cmp = since.replace("T", " ")
        result = [v for v in result if v.timestamp and v.timestamp >= since_cmp[:len(v.timestamp)]]
    if until:
        until_cmp = until.replace("T", " ")
        result = [v for v in result if v.timestamp and v.timestamp <= until_cmp[:len(v.timestamp)]]
    return result


def _version_src_path(engine: "AISession", v: FileVersion) -> Path:
    """Return the on-disk path of a specific version file."""
    return (
        engine.recovery_dir
        / f"session_all_versions_{v.session_id}"
        / f"{v.filename}_v{v.version_num:06d}_line_{v.line_count}.txt"
    )


def _resolve_output_path(target: Path) -> Path:
    """Return target path; if it already exists, append .recovered[_N] before the extension.

    Examples: cli.py -> cli.recovered.py -> cli.recovered_1.py -> cli.recovered_2.py -> …
    """
    if not target.exists():
        return target
    stem, suffix = target.stem, target.suffix
    candidate = target.parent / f"{stem}.recovered{suffix}"
    counter = 1
    while candidate.exists():
        candidate = target.parent / f"{stem}.recovered_{counter}{suffix}"
        counter += 1
    return candidate


def _do_extract(  # noqa: C901
    engine: "AISession",
    name: str,
    version: Optional[int] = None,
    session: Optional[str] = None,
    output_dir: Optional[str] = None,
    restore: bool = False,
    dry_run: bool = False,
) -> None:
    """Extract a file version: stdout by default, or write with --restore/--output-dir."""
    versions = engine.get_versions(name)

    if session:
        versions = [v for v in versions if v.session_id.startswith(session)]
        if not versions:
            err_console.print(f"[red]No versions of[/red] {name} [red]found in session[/red] {session}")
            raise typer.Exit(code=1)

    if not versions:
        # Fall back to Edit-based reconstruction
        result = engine.reconstruct_from_edits(name, session_id=session)
        if result:
            content, applied_edits = result
            err_console.print(
                f"[green]Reconstructed {name} from {len(applied_edits)} "
                f"edit{'s' if len(applied_edits) != 1 else ''}[/green]"
            )
            if dry_run:
                err_console.print(f"[yellow][dry run] would output {len(content)} chars[/yellow]")
                return
            if restore:
                original = engine.get_original_path(name)
                if original:
                    target = _resolve_output_path(Path(original))
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(content)
                    err_console.print(f"Restored to: {target}")
                else:
                    err_console.print(f"[red]No original path recorded for:[/red] {name}")
                    raise typer.Exit(code=1)
            elif output_dir:
                target = Path(output_dir).expanduser() / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content)
                err_console.print(f"Written to: {target}")
            else:
                sys.stdout.write(content)
            return
        err_console.print(f"[red]File not found in session data:[/red] {name}")
        raise typer.Exit(code=1)

    if version is None:
        v = max(versions, key=lambda x: x.version_num)
    else:
        v = next((x for x in versions if x.version_num == version), None)
        if v is None:
            max_v = max(x.version_num for x in versions)
            err_console.print(f"[red]Version {version} not found.[/red] Available: 1\u2013{max_v}")
            raise typer.Exit(code=1)

    label = f"v{v.version_num}, {v.line_count} lines"
    src = _version_src_path(engine, v)

    if restore:
        original = engine.get_original_path(name)
        if original:
            target = _resolve_output_path(Path(original))
        else:
            err_console.print(f"[red]No original path recorded in session data for:[/red] {name}")
            raise typer.Exit(code=1)
        err_console.print(f"Writing to: {target}  ({label})")
        if dry_run:
            err_console.print("[yellow][dry run] no files written[/yellow]")
            return
        if not src.exists():
            err_console.print(f"[red]Version file not found on disk:[/red] {src}")
            raise typer.Exit(code=1)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(src.read_text(errors="replace"))

    elif output_dir:
        target = Path(output_dir).expanduser() / name
        err_console.print(f"Writing to: {target}  ({label})")
        if dry_run:
            err_console.print("[yellow][dry run] no files written[/yellow]")
            return
        if not src.exists():
            err_console.print(f"[red]Version file not found on disk:[/red] {src}")
            raise typer.Exit(code=1)
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(src.read_text(errors="replace"))

    else:
        # Default: stdout
        if dry_run:
            err_console.print(f"[yellow][dry run] would print {v.line_count} lines to stdout[/yellow]")
            return
        err_console.print(f"Extracting: {name}  ({label})")
        if src.exists():
            sys.stdout.write(src.read_text(errors="replace"))
        else:
            err_console.print(f"[red]Version file not found on disk:[/red] {src}")
            raise typer.Exit(code=1)


def _do_history_display(
    engine: "AISession",
    name: str,
    versions: Optional[List[FileVersion]] = None,
    fmt: Optional[str] = None,
    full_uuid: bool = False,
) -> None:
    """Show version history table (read-only, no disk writes).

    Supports fmt='table' (Rich table), 'json', 'csv', 'plain'.
    """
    if fmt is None:
        fmt = _cfg_default("format", "plain")
    if versions is None:
        versions = engine.get_versions(name)
    if not versions:
        console.print(f"[yellow]No versions found for:[/yellow] {name}")
        return

    # Build row dicts with delta (needs prev-row context, so computed before render)
    rows = []
    prev_lines = None
    for v in versions:
        if prev_lines is None:
            delta = "—"
        elif v.line_count >= prev_lines:
            delta = f"+{v.line_count - prev_lines}"
        else:
            delta = str(v.line_count - prev_lines)
        rows.append({
            "version": f"v{v.version_num}",
            "lines": v.line_count,
            "delta_lines": delta,
            "added": getattr(v, "lines_added", 0),
            "deleted": getattr(v, "lines_deleted", 0),
            "timestamp": v.timestamp or "—",
            "session": v.session_id if full_uuid else v.session_id[:8],
            "tool": getattr(v, "tool", "Write"),
        })
        prev_lines = v.line_count

    if fmt == "json":
        sys.stdout.write(json.dumps(rows, indent=2) + "\n")
        return
    if fmt in ("csv", "plain") or fmt is None:
        for r in rows:
            console.print(f"{r['version']}  {r['lines']}  {r['delta_lines']}  +{r['added']}/-{r['deleted']}  {r['timestamp']}  {r['session']}  {r['tool']}")
        return
    # Default: Rich table
    from rich.table import Table
    table = Table(title=f"Version history: {name}  ({len(versions)} versions)")
    table.add_column("Version", style="cyan", justify="right")
    table.add_column("Lines", justify="right", style="magenta")
    table.add_column("\u0394Lines", justify="right")
    table.add_column("Timestamp", style="dim")
    session_col_kw: dict = {"style": "blue"}
    if full_uuid:
        session_col_kw["min_width"] = 36
        session_col_kw["no_wrap"] = True
    table.add_column("Session", **session_col_kw)
    table.add_column("Tool", style="green", justify="center")
    table.add_column("+Add", justify="right", style="green")
    table.add_column("-Del", justify="right", style="red")
    for r in rows:
        tool_label = {"Write": "W", "Edit": "E", "NotebookEdit": "NE"}.get(r["tool"], "?")
        add_s = f"+{r['added']}" if r["added"] else "—"
        del_s = f"-{r['deleted']}" if r["deleted"] else "—"
        table.add_row(r["version"], str(r["lines"]), r["delta_lines"], r["timestamp"], r["session"], tool_label, add_s, del_s)
    console.print(table)


def _do_history_export(
    engine: "AISession",
    name: str,
    export_dir: Optional[str] = None,
    dry_run: bool = False,
    versions: Optional[List[FileVersion]] = None,
) -> None:
    """Export all versions as named files: cli_v1.py, cli_v2.py, …"""
    if versions is None:
        versions = engine.get_versions(name)
    if not versions:
        console.print(f"[yellow]No versions found:[/yellow] {name}")
        return

    stem = Path(name).stem
    suffix = Path(name).suffix

    # Resolve export dir
    env_dir = os.getenv("AI_SESSION_SEARCH_OUTPUT")
    if export_dir:
        out_dir = Path(export_dir).expanduser()
    elif env_dir:
        out_dir = Path(env_dir).expanduser()
    else:
        original = engine.get_original_path(name)
        out_dir = Path(original).parent / "versions" if original else Path("./recovered/versions")

    # Always show what will be written before writing
    console.print(f"[bold]Exporting {len(versions)} versions to:[/bold] {out_dir}")
    for v in versions:
        console.print(f"  {stem}_v{v.version_num}{suffix}  ({v.line_count} lines)")

    if dry_run:
        console.print("[yellow][dry run] no files written[/yellow]")
        return

    out_dir.mkdir(parents=True, exist_ok=True)
    written = 0
    for v in versions:
        src = _version_src_path(engine, v)
        if src.exists():
            shutil.copy2(src, out_dir / f"{stem}_v{v.version_num}{suffix}")
            written += 1

    if written:
        console.print(f"\n[green]Exported {written} files to:[/green] {out_dir}")
    else:
        console.print("[red]No version files found to export[/red]")
        raise typer.Exit(code=1)


def _do_history_stdout(engine: "AISession", name: str, versions: Optional[List[FileVersion]] = None) -> None:
    """Print all versions to stdout with === vN === headers."""
    if versions is None:
        versions = engine.get_versions(name)
    if not versions:
        err_console.print(f"[yellow]No versions found:[/yellow] {name}")
        return
    for v in versions:
        src = _version_src_path(engine, v)
        sys.stdout.write(f"=== {name} v{v.version_num} ({v.line_count} lines, session {v.session_id[:16]}) ===\n")
        if src.exists():
            sys.stdout.write(src.read_text(errors="ignore"))
        sys.stdout.write("\n")


def _do_files_search(
    engine: "AISession",
    pattern: str = "*",
    min_edits: int = 0,
    max_edits: Optional[int] = None,
    include_extensions: Optional[str] = None,
    exclude_extensions: Optional[str] = None,
    include_sessions: Optional[str] = None,
    exclude_sessions: Optional[str] = None,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
    limit: Optional[int] = None,
    fmt: str = "table",
) -> None:
    """Search recovered files with filtering."""
    fmt = fmt or "table"
    inc_ext = {e.strip() for e in include_extensions.split(",") if e.strip()} if include_extensions else set()
    exc_ext = {e.strip() for e in exclude_extensions.split(",") if e.strip()} if exclude_extensions else set()
    inc_sessions = {s.strip() for s in include_sessions.split(",") if s.strip()} if include_sessions else set()
    exc_sessions = {s.strip() for s in exclude_sessions.split(",") if s.strip()} if exclude_sessions else set()

    filters = FilterSpec(min_edits=min_edits, max_edits=max_edits, since=since, until=until)
    if inc_ext or exc_ext:
        filters.with_extensions(include=inc_ext or None, exclude=exc_ext or None)
    if inc_sessions or exc_sessions:
        filters.with_sessions(include=inc_sessions or None, exclude=exc_sessions or None)

    results = engine.search_files(pattern, filters)
    if limit:
        results = results[:limit]

    if not results:
        console.print("[yellow]No files found[/yellow]")
        return

    formatter = get_formatter(fmt, f"Recovered Files: {pattern}")
    console.print(formatter.format_many(results))
    console.print(f"\n[bold]Found {len(results)} files[/bold]")


def _do_messages_search(
    engine: "AISession",
    query: str,
    message_type: Optional[str] = None,
    limit: Optional[int] = None,
    max_chars: Optional[int] = None,
    fmt: Optional[str] = None,
    tool: Optional[str] = None,
    context: int = 0,
    context_before: int = -1,
    context_after: int = -1,
    exclude_compaction: bool = False,
    session: Optional[str] = None,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
    full_uuid: bool = False,
    output: Optional[str] = None,
    fixed_strings: bool = False,
    after_index: int = 0,
    after_timestamp: Optional[str] = None,
    tool_use_only: bool = False,
    include_tool_result: bool = False,
) -> None:
    """Search messages across all sessions. When context > 0, each result includes
    up to ``context`` surrounding messages from the same session file.

    Since/until and session are threaded into the engine's _iter_all_jsonl via
    search_messages() — no redundant get_sessions() pre-scan needed.
    """
    if max_chars is None:
        max_chars = _cfg_default("max_chars", 0)
    if fmt is None:
        fmt = _cfg_default("format", "plain")
    tag = f" [tool: {tool}]" if tool else ""

    # Resolve effective before/after context window.
    # --context-after alone -> before=0, after=N (most common slash audit pattern).
    # --context N alone -> symmetric (existing behavior unchanged).
    eff_before = context_before if context_before >= 0 else (0 if context_after >= 0 else context)
    eff_after  = context_after  if context_after  >= 0 else context

    use_context = eff_before > 0 or eff_after > 0

    if use_context:
        ctx_results = engine.search_messages(
            query,
            context=max(eff_before, eff_after),  # engine uses symmetric; we slice after
            context_before=eff_before,
            context_after=eff_after,
            message_type=message_type,
            exclude_compaction=exclude_compaction,
            tool=tool,
            since=since,
            session_id=session,
            fixed_strings=fixed_strings,
            after_index=after_index,
            after_timestamp=after_timestamp,
        )
        ctx_results = ctx_results[:limit]
        out_console = _console(record=bool(output))
        if not ctx_results:
            out_console.print(f"[yellow]No messages match query{tag}[/yellow]")
            _write_output(out_console, output)
            return
        if fmt == "json":
            json_text = json.dumps([r.to_dict() for r in ctx_results], indent=2) + "\n"
            if output:
                Path(output).expanduser().write_text(json_text, encoding="utf-8")
            else:
                sys.stdout.write(json_text)
            return
        # Flat display: separator + before context + match + after context.
        # truncate=None means no truncation (Python slice [:None] returns full string).
        truncate = max_chars if max_chars > 0 else None
        for cm in ctx_results:
            out_console.print(f"[dim]{_SEPARATOR}[/dim]")
            for m in cm.context_before:
                out_console.print(f"[dim][{m.type.value}] {m.timestamp[:19]}[/dim]")
                out_console.print(f"[dim]{_esc(m.content[:truncate])}[/dim]\n")
            out_console.print(f"[bold cyan][{cm.match.type.value}] {cm.match.timestamp[:19]}[/bold cyan]")
            out_console.print(f"{_esc(cm.match.content[:truncate])}\n")
            for m in cm.context_after:
                out_console.print(f"[dim][{m.type.value}] {m.timestamp[:19]}[/dim]")
                out_console.print(f"[dim]{_esc(m.content[:truncate])}[/dim]\n")
        out_console.print(f"\n[bold]Found {len(ctx_results)} matches{tag}[/bold]")
        _write_output(out_console, output)
        return

    all_results = engine.search_messages(
        query,
        message_type=message_type,
        exclude_compaction=exclude_compaction,
        tool=tool,
        since=since,
        session_id=session,
        fixed_strings=fixed_strings,
        after_index=after_index,
        after_timestamp=after_timestamp,
        tool_use_only=tool_use_only,
        include_tool_result=include_tool_result,
    )
    results = all_results[:limit]

    out_console = _console(record=bool(output))
    if not results:
        out_console.print(f"[yellow]No messages match query{tag}[/yellow]")
        _write_output(out_console, output)
        return

    formatter = MessageFormatter(max_chars=max_chars)
    if fmt in ("json", "csv", "plain"):
        # truncate=None means no limit (Python [:None] returns full string).
        # JSON format ignores row_fn entirely (uses to_dict() directly) so content
        # is always complete. csv/plain respects max_chars if set, else no truncation.
        truncate = max_chars if max_chars > 0 else None
        sid_fn = (lambda d: d.get("session_id", "")) if full_uuid else (lambda d: d.get("session_id", "")[:12] + "\u2026")
        sid_col = ColumnSpec("Session", style="blue", min_width=36 if full_uuid else None, no_wrap=full_uuid)
        spec = TableSpec(
            title_template=f"Messages ({{n}} found){tag}",
            columns=[
                ColumnSpec("Timestamp", style="dim", no_wrap=True),
                ColumnSpec("Type", style="cyan"),
                sid_col,
                ColumnSpec("Content"),
            ],
            row_fn=lambda d: [
                d.get("timestamp", "")[:19],
                d.get("type", ""),
                sid_fn(d),
                d.get("content", "")[:truncate],
            ],
            summary_template="Found {n} messages",
        )
        _render_output(results, fmt, spec, output=output)
        return

    # Default table format — use recording console if writing to file
    render_console = _console(record=True, width=120) if output else _get_render_console()
    render_console.print(formatter.format_many(results))
    render_console.print(f"\n[bold]Found {len(results)} messages[/bold]")
    if output:
        _write_output(render_console, output)


def _do_list_sessions(
    engine: "AISession",
    project: Optional[str] = None,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
    limit: Optional[int] = None,
    fmt: str = "table",
    full_uuid: bool = False,
) -> None:
    """List sessions with metadata."""
    sessions = engine.get_sessions(project_filter=project, since=since, until=until)
    if limit:
        sessions = sessions[:limit]
    spec = _spec_with_full_uuid(_LIST_SPEC) if full_uuid else _LIST_SPEC
    _render_output(sessions, fmt, spec, "No sessions found")


def _parse_pattern_options(raw: List[str]) -> List[tuple]:
    """Parse correction pattern entries into engine-format tuples.

    Each entry must be ``"CATEGORY:REGEX"``. Multiple entries with the same
    category are combined (OR logic). A single entry that is a bracketed list
    (e.g. ``'["regression:you deleted", "skip_step:you forgot"]'``) is expanded
    automatically, so both ``--pattern`` repetition and Python-list syntax work:

    Examples::
        # Repeatable flags (standard):
        --pattern "skip_step:you missed" --pattern "skip_step:you forgot"
        -> [("skip_step", ["you missed", "you forgot"])]

        # Single bracketed-list value (also accepted):
        --pattern '["skip_step:you missed", "skip_step:you forgot"]'
        -> [("skip_step", ["you missed", "you forgot"])]

        # Config file list (same format as bracketed value items):
        ["regression:you deleted", "skip_step:you forgot"]
        -> [("regression", ["you deleted"]), ("skip_step", ["you forgot"])]

    Raises:
        typer.BadParameter: If any entry lacks the required "CATEGORY:REGEX" format.
    """
    from collections import OrderedDict
    # Expand any single entry that is a Python-style bracketed list
    expanded: List[str] = []
    for entry in raw:
        s = entry.strip()
        if s.startswith("[") and s.endswith("]"):
            expanded.extend(_parse_list_input(s))
        else:
            expanded.append(entry)

    groups: OrderedDict[str, List[str]] = OrderedDict()
    for entry in expanded:
        if ":" not in entry:
            raise typer.BadParameter(
                f"--pattern {entry!r} must be in 'CATEGORY:REGEX' format, "
                f"e.g. --pattern 'skip_step:you missed'"
            )
        category, regex = entry.split(":", 1)
        category = category.strip()
        regex = regex.strip()
        if not category or not regex:
            raise typer.BadParameter(
                f"--pattern {entry!r}: both CATEGORY and REGEX must be non-empty"
            )
        if category not in groups:
            groups[category] = []
        groups[category].append(regex)
    return [(cat, regexes) for cat, regexes in groups.items()]


def _parse_commands_option(raw: Optional[str]) -> Optional[List[str]]:
    """Parse --commands value into a list of regex patterns.

    Returns None (use engine/config default) when raw is None or empty.

    Accepts both plain CSV and Python-style bracketed lists (see _parse_list_input):

    Examples::
        "/ar:plannew,/ar:pn,/mycommand"
        -> ["/ar:plannew", "/ar:pn", "/mycommand"]

        '["/ar:plannew", "/ar:pn"]'
        -> ["/ar:plannew", "/ar:pn"]
    """
    if not raw:
        return None
    return _parse_list_input(raw)


def _do_messages_corrections(
    engine: "AISession",
    project: Optional[str] = None,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
    limit: Optional[int] = None,   # None -> engine default (config 'defaults.limit', else 0 = all); 0 -> all
    fmt: str = "table",
    pattern_overrides: Optional[List[str]] = None,
    full_uuid: bool = False,
    output: Optional[str] = None,
    session: Optional[str] = None,
) -> None:
    """Find user corrections across sessions.

    Priority for correction patterns: CLI --pattern > config file > built-in defaults.

    Args:
        pattern_overrides: Raw --pattern option values in "CATEGORY:REGEX" format.
                           When provided, replaces config/defaults entirely.
    """
    if pattern_overrides:
        # CLI flag — highest priority
        patterns = _parse_pattern_options(pattern_overrides)
    else:
        cfg = load_config()
        cfg_patterns = cfg.get("correction_patterns")
        if cfg_patterns:
            # Config file — middle priority; same format as --pattern values
            patterns = _parse_pattern_options(list(cfg_patterns))
        else:
            # Built-in defaults — lowest priority (engine uses DEFAULT_CORRECTION_PATTERNS)
            patterns = None
    corrections = engine.find_corrections(
        project_filter=project, since=since, until=until,
        limit=limit, patterns=patterns,
        session_id_prefix=session,
    )
    spec = _spec_with_full_uuid(_CORRECTIONS_SPEC) if full_uuid else _CORRECTIONS_SPEC
    _render_output(corrections, fmt, spec, "No corrections found", output=output)


def _do_messages_planning(
    engine: "AISession",
    project: Optional[str] = None,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
    fmt: str = "table",
    commands_raw: Optional[str] = None,
    detail: bool = False,
    context_after: int = 0,
    output: Optional[str] = None,
    full_uuid: bool = False,
    max_chars: int = 0,
) -> None:
    """Show planning command usage.

    Priority for command list: CLI --commands > config file > built-in defaults.

    Args:
        commands_raw:  Raw --commands option value (CSV or bracketed list).
                       When provided, replaces config/defaults entirely.
        detail:        When True, show per-invocation table via _INVOCATIONS_SPEC.
        context_after: When > 0, show N messages after each invocation (implies detail).
        output:        Write output to FILE in addition to stdout (tee).
        full_uuid:     When True, show full 36-char session UUIDs.
        max_chars:     Truncate context content to this many chars. 0 = full.
    """
    if commands_raw:
        # CLI flag — highest priority
        commands = _parse_commands_option(commands_raw)
    else:
        cfg = load_config()
        cfg_commands = cfg.get("planning_commands")
        if cfg_commands:
            # Config file — middle priority; list of regex strings
            commands = list(cfg_commands)
        else:
            # Built-in defaults — lowest priority (engine uses DEFAULT_PLANNING_COMMANDS)
            commands = None
    if context_after > 0:
        # Context-after mode: per-invocation with trailing messages
        invocations = engine.get_planning_usage(
            project_filter=project, since=since, until=until,
            commands=commands, return_invocations=True,
        )
        max_chars_val = max_chars if max_chars > 0 else _cfg_default("max_chars", 0)
        _render_invocations_with_context(invocations, context_after, engine, output, max_chars=max_chars_val)
        return
    if detail:
        # Per-invocation listing via _render_output (table/json/csv/plain support)
        invocations = engine.get_planning_usage(
            project_filter=project, since=since, until=until,
            commands=commands, return_invocations=True,
        )
        if output:
            out_console = _console(record=True)
            for inv in invocations:
                out_console.print(f"{inv.command} {inv.timestamp[:19]} {inv.args}")
            _write_output(out_console, output)
        spec = _spec_with_full_uuid(_INVOCATIONS_SPEC, 1) if full_uuid else _INVOCATIONS_SPEC
        _render_output(invocations, fmt, spec, "No planning command invocations found")
        return
    # Default: aggregate counts (unchanged existing behavior)
    results = engine.get_planning_usage(
        project_filter=project, since=since, until=until,
        commands=commands,
    )
    if output:
        out_console = _console(record=True)
        for r in results:
            out_console.print(f"{r.command}: {r.count}")
        _write_output(out_console, output)
    _render_output(results, fmt, _PLANNING_SPEC, "No planning commands found")


_ANALYZE_TOOLS_SPEC = TableSpec(
    title_template="Tool Usage ({n} tools)",
    columns=[
        ColumnSpec("Tool", style="cyan"),
        ColumnSpec("Count", justify="right"),
    ],
    row_fn=lambda d: [d["tool"], str(d["count"])],
    summary_template="Found {n} distinct tools",
)

_TIMELINE_SPEC = TableSpec(
    title_template="Session Timeline ({n} events)",
    columns=[
        ColumnSpec("Timestamp", style="dim", no_wrap=True),
        ColumnSpec("Type", style="cyan"),
        ColumnSpec("Tools", justify="right"),
        ColumnSpec("Preview"),
    ],
    row_fn=lambda d: [
        d.get("timestamp", "")[:19],
        d.get("type", ""),
        str(d.get("tool_count", 0)),
        d.get("content_preview", ""),
    ],
)


def _do_messages_analyze(
    engine: "AISession",
    session_id: str,
    fmt: str = "table",
) -> None:
    """Show per-session statistics."""
    result = engine.get_session_analysis(session_id)
    if result is None:
        err_console.print(f"[red]No session found matching:[/red] {session_id!r}")
        raise typer.Exit(code=1)

    if fmt == "json":
        console.print(json.dumps(result.to_dict(), indent=2))
        return

    # Summary header
    console.print(f"\n[bold]Session:[/bold] {result.session_id[:16]}…")
    console.print(f"[bold]Project:[/bold] {result.project_dir}")
    console.print(f"[bold]Lines:[/bold] {result.total_lines}  "
                  f"[bold]User:[/bold] {result.user_count}  "
                  f"[bold]Assistant:[/bold] {result.assistant_count}")
    console.print(f"[bold]From:[/bold] {result.timestamp_first[:19]}  "
                  f"[bold]To:[/bold] {result.timestamp_last[:19]}\n")

    # Tool usage table
    tool_rows = [{"tool": t, "count": c}
                 for t, c in sorted(result.tool_uses_by_name.items(),
                                    key=lambda x: x[1], reverse=True)]
    if tool_rows:
        _render_output(tool_rows, fmt, _ANALYZE_TOOLS_SPEC, "No tool calls found")

    # Files touched list
    if result.files_touched:
        console.print(f"\n[bold]Files touched ({len(result.files_touched)}):[/bold]")
        for fp in result.files_touched:
            console.print(f"  {fp}")
    else:
        console.print("\n[dim]No files touched.[/dim]")


def _do_messages_timeline(
    engine: "AISession",
    session_id: str,
    fmt: str = "table",
    preview_chars: int = 150,
    message_type: Optional[str] = None,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
    grep: Optional[str] = None,
    grep_regex: bool = False,
    exclude_compaction: bool = False,
    output: Optional[str] = None,
) -> None:
    """Show chronological event timeline for a session."""
    events = engine.get_session_timeline(session_id, preview_chars=preview_chars)
    any_filter_active = bool(message_type or grep or exclude_compaction or since or until)
    has_events_before_filter = bool(events)
    # Timeline events use "content_preview" as the content key (from timeline_session()).
    # --type filter (extended to include slash and compaction)
    if message_type == "slash":
        events = [e for e in events if "<command-name>" in (e.get("content_preview") or "")]
    elif message_type == "compaction":
        events = [e for e in events if _is_compaction_content(e.get("content_preview"))]
    elif message_type:
        events = [e for e in events if e.get("type") == message_type]
    # --no-compaction post-filter (applies after --type)
    if exclude_compaction:
        events = [e for e in events if not _is_compaction_content(e.get("content_preview"))]
    if since:
        events = [e for e in events if (e.get("timestamp") or "") >= since]
    if until:
        events = [e for e in events if (e.get("timestamp") or "") <= until]
    # --grep post-filter (literal by default; regex with -E/--regex)
    if grep:
        if grep_regex:
            try:
                _check_redos_safe(grep)
                grep_re = _re.compile(grep, _re.IGNORECASE)
                events = [e for e in events if grep_re.search(e.get("content_preview") or "")]
            except (ValueError, _re.error) as exc:
                msg = str(exc)
                if "ReDoS" in msg:
                    err_console.print(f"[yellow]Warning:[/yellow] --grep pattern rejected: {msg}. Falling back to literal substring match.")
                else:
                    err_console.print(f"[yellow]Warning:[/yellow] --grep pattern is not valid regex; falling back to literal substring match.")
                grep_lower = grep.lower()
                events = [e for e in events if grep_lower in (e.get("content_preview") or "").lower()]
        else:
            grep_lower = grep.lower()
            events = [e for e in events if grep_lower in (e.get("content_preview") or "").lower()]
    if not events:
        if any_filter_active and has_events_before_filter:
            # Session exists and has events, but all were filtered out — exit 0
            console.print("[yellow]No events match the given filters.[/yellow]")
            return
        # Distinguish "session file not found" from "session has no user/assistant events"
        # Cross-backend session existence check: use _find_session_files() for Claude,
        # fall back to get_messages() for multi-source backends (AISession).
        _backend = getattr(engine, "_backend", None)
        if _backend is not None and hasattr(_backend, "_find_session_files"):
            session_exists = bool(_backend._find_session_files(session_id))
        else:
            session_exists = bool(engine.get_messages(session_id))
        if not session_exists:
            err_console.print(f"[red]No session found matching:[/red] {session_id!r}")
        else:
            err_console.print(
                f"[yellow]Session {session_id!r} exists but has no user/assistant events.[/yellow]"
            )
        raise typer.Exit(code=1)
    if output:
        out_console = _console(record=True)
        # Use plain/message format for file output since _render_output uses global console.
        for e in events:
            ts = (e.get("timestamp") or "")[:19]
            etype = e.get("type") or ""
            content = e.get("content_preview") or ""
            out_console.print(f"[dim][{etype}] {ts}[/dim]")
            out_console.print(content)
        _write_output(out_console, output)
    _render_output(events, fmt, _TIMELINE_SPEC, "No events found")


def _do_files_cross_ref(
    engine: "AISession",
    file: str,
    session: Optional[str] = None,
    fmt: str = "table",
    full_uuid: bool = False,
) -> None:
    """Cross-reference session edits against current file content."""
    file_path = Path(file)
    if not file_path.exists():
        err_console.print(f"[red]File not found:[/red] {file}")
        raise typer.Exit(code=1)
    current_content = file_path.read_text(errors="replace")
    results = engine.get_file_edits(
        file_path.name, current_content, session_id=session
    )
    sid_fn = (lambda d: d["session_id"]) if full_uuid else (lambda d: d["session_id"][:8])
    sid_col = ColumnSpec("Session", style="cyan", no_wrap=True, min_width=36 if full_uuid else None)
    spec = TableSpec(
        title_template=f"Cross-reference: {file_path.name} ({{n}} edits)",
        columns=[
            ColumnSpec("Timestamp", style="dim", no_wrap=True),
            sid_col,
            ColumnSpec("Tool", style="blue"),
            ColumnSpec("Applied", justify="center"),
            ColumnSpec("Snippet"),
        ],
        row_fn=lambda d: [
            d["timestamp"][:19],
            sid_fn(d),
            d["tool"],
            "[green]\u2713[/green]" if d["found_in_current"] else "[red]\u2717[/red]",
            d["content_snippet"][:60],
        ],
        plain_fn=lambda d: (
            "{ts}  {sid}  {tool}  {mark}  {snip}".format(
                ts=d["timestamp"][:19],
                sid=sid_fn(d),
                tool=d["tool"],
                mark="\u2713" if d["found_in_current"] else "\u2717",
                snip=d["content_snippet"][:60],
            )
        ),
    )
    _render_output(results, fmt, spec, "No session edits found for this file")
    if results and fmt not in ("json",):
        applied = sum(1 for r in results if r["found_in_current"])
        console.print(f"\n[bold]{applied}/{len(results)} edits found in current file[/bold]")


def _do_export_session(
    engine: "AISession",
    session_id: str,
    output: Optional[str] = None,
    dry_run: bool = False,
) -> None:
    """Export one session to markdown."""
    try:
        md = engine.get_session_markdown(session_id)
    except ValueError as e:
        err_console.print(f"[red]{e}[/red]")
        raise typer.Exit(code=1)
    if output:
        err_console.print(f"Writing to: {output}")
        if dry_run:
            err_console.print("[yellow][dry run] no files written[/yellow]")
            return
        Path(output).write_text(md, encoding="utf-8")
    else:
        if dry_run:
            err_console.print(f"[yellow][dry run] would write {len(md)} chars to stdout[/yellow]")
            return
        err_console.print(f"Exporting session: {session_id[:8]}")
        sys.stdout.write(md)


def _do_export_recent(
    engine: "AISession",
    days: int = 7,
    output: Optional[str] = None,
    project: Optional[str] = None,
    dry_run: bool = False,
) -> None:
    """Export all sessions from last N days to markdown."""
    import datetime as _dt
    since = (_dt.datetime.now(_dt.timezone.utc) - _dt.timedelta(days=days)).strftime("%Y-%m-%d")
    sessions = engine.get_sessions(project_filter=project, since=since)
    if not sessions:
        console.print("[yellow]No sessions found[/yellow]")
        return
    if dry_run:
        err_console.print(f"[yellow][dry run] would export {len(sessions)} sessions[/yellow]")
        for s in sessions:
            err_console.print(f"  {s.session_id[:16]}  {s.project_dir[-20:]}  {s.timestamp_first[:10]}")
        return
    parts = []
    for s in sessions:
        try:
            parts.append(engine.get_session_markdown(s.session_id))
        except ValueError:
            continue
    combined = "\n\n".join(parts)
    if output:
        err_console.print(f"Writing {len(sessions)} sessions to: {output}")
        Path(output).write_text(combined, encoding="utf-8")
    else:
        sys.stdout.write(combined)


def _do_search(  # noqa: C901
    engine: "AISession",
    domain: Optional[str],
    pattern: Optional[str],
    query: Optional[str],
    min_edits: int,
    max_edits: Optional[int],
    include_extensions: Optional[str],
    exclude_extensions: Optional[str],
    include_sessions: Optional[str],
    exclude_sessions: Optional[str],
    since: Optional[str],          # canonical; after= is a hidden alias
    until: Optional[str],          # canonical; before= is a hidden alias
    message_type: Optional[str],
    limit: Optional[int],
    max_chars: int,
    fmt: str,
    tool: Optional[str] = None,
) -> None:
    """Shared logic for search() and find() commands."""
    # When domain is 'tools' and --tool is omitted, search all tool types.
    # Track this before normalization so we can pass tool_use_only downstream.
    _tools_no_tool = domain == "tools" and tool is None
    if _tools_no_tool:
        message_type = "assistant"

    # Normalize "tools" domain -> "messages" after validation
    if domain == "tools":
        domain = "messages"

    # Validate domain (after "tools"->"messages" normalization)
    if domain is not None and domain not in ("files", "messages"):
        typer.echo(
            f"Error: domain must be 'files', 'messages', or 'tools', got {domain!r}.\n"
            f"  Hint: To search messages use:  aise search messages --query {domain!r}\n"
            f"  Or:                            aise messages search {domain!r}",
            err=True,
        )
        raise typer.Exit(1)

    # Validate flag/domain conflicts
    if domain == "messages" and pattern is not None:
        typer.echo("Error: --pattern applies to files, not messages. Use --query.", err=True)
        raise typer.Exit(1)
    if domain == "files" and query is not None:
        typer.echo("Error: --query applies to messages, not files. Use --pattern.", err=True)
        raise typer.Exit(1)
    if domain == "files" and tool is not None:
        typer.echo("Error: --tool applies to messages, not files.", err=True)
        raise typer.Exit(1)

    # Auto-detect domain from flags when not explicitly specified
    if domain is None:
        if tool is not None:
            domain = "messages"
        elif query is not None and pattern is None:
            domain = "messages"
        elif pattern is not None and query is None:
            domain = "files"
        elif pattern is not None and query is not None:
            domain = "both"
        else:
            domain = "files"

    if domain in ("files", "both"):
        _do_files_search(
            engine, pattern or "*", min_edits, max_edits,
            include_extensions, exclude_extensions,
            include_sessions, exclude_sessions,
            since, until, limit, fmt,
        )

    if domain in ("messages", "both"):
        msg_limit = limit if limit is not None else 10
        # Thread session filter: include_sessions is comma-separated; pass first as session prefix
        msg_session = include_sessions.split(",")[0].strip() if include_sessions else None
        _do_messages_search(engine, query or "", message_type, msg_limit, max_chars, fmt,
                            tool=tool, since=since, until=until, session=msg_session,
                            tool_use_only=_tools_no_tool)


def _do_get(
    engine: "AISession",
    session_id: Optional[str],
    message_type: Optional[str] = None,
    limit: Optional[int] = None,
    max_chars: Optional[int] = None,
    fmt: Optional[str] = None,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
    output: Optional[str] = None,
) -> None:
    """Get messages from a specific session — delegates to _do_messages_search."""
    if not session_id:
        err_console.print("[red]Session ID is required.[/red] Use 'aise list' to find session IDs.")
        raise typer.Exit(code=1)
    _do_messages_search(
        engine, query="", message_type=message_type,
        limit=limit, max_chars=max_chars, fmt=fmt,
        session=session_id, since=since, until=until,
        output=output,
    )


def _do_stats(
    engine: AISession,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
    fmt: Optional[str] = None,
) -> None:
    """Show recovery statistics. Default since=None, until=None: all sessions shown."""
    if fmt is None:
        fmt = _cfg_default("format", "plain")
    stats = engine.get_statistics(since=since, until=until)
    sessions = stats.total_sessions
    files = stats.total_files
    versions = stats.total_versions
    largest = stats.largest_file
    largest_edits = stats.largest_file_edits
    per_source = {
        k.replace("_sessions", ""): v
        for k, v in stats.per_source.items()
        if k != "total_sessions"
    }

    if fmt == "json":
        out = stats.to_dict()
        sys.stdout.write(json.dumps(out, indent=2) + "\n")
        return

    breakdown = ""
    if len(per_source) > 1:
        breakdown = "\n" + "\n".join(
            f"    {src:<14} {count}" for src, count in sorted(per_source.items())
        )

    largest_display = largest or "-"
    if fmt in ("csv", "plain"):
        console.print(f"Sessions: {sessions}")
        console.print(f"Files: {files}")
        console.print(f"Versions: {versions}")
        console.print(f"Largest File: {largest_display} ({largest_edits} edits)")
        return

    console.print(
        f"""
[bold cyan]Recovery Statistics[/bold cyan]
  Sessions:      {sessions}{breakdown}
  Files:         {files}
  Versions:      {versions}
  Largest File:  {largest_display} ({largest_edits} edits)
"""
    )


# ── export_app commands ───────────────────────────────────────────────────────

@export_app.command("session")
def export_session(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    session_id: str = typer.Argument(..., help="Session ID (prefix match, e.g. ab841016)."),
    output: Optional[str] = typer.Option(
        None, "--output", "-o",
        help="Write to this file instead of stdout. Example: --output session.md",
    ),
    dry_run: bool = _OPT_DRY_RUN,
) -> None:
    """Export a single session's messages to markdown. Outputs to stdout by default.

    Examples:
        aise export session ab841016                    # stdout
        aise export session ab841016 > session.md       # redirect to file
        aise export session ab841016 --output out.md    # write explicitly
        aise export session ab841016 --dry-run          # preview only
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_export_session(engine, session_id, output=output, dry_run=dry_run)


@export_app.command("recent")
def export_recent(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    days: int = typer.Argument(7, help="Number of days back to include (default: 7)."),
    output: Optional[str] = typer.Option(
        None, "--output", "-o",
        help="Output file path.",
    ),
    project: Optional[str] = typer.Option(None, "--project", help="Limit to this project directory substring."),
    dry_run: bool = _OPT_DRY_RUN,
) -> None:
    """Export all sessions from the last N days to a single markdown file.

    Examples:
        aise export recent                            # last 7 days -> stdout
        aise export recent 14 --output week2.md       # last 14 days -> file
        aise export recent --project myproject        # filter by project
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_export_recent(engine, days=days, output=output, project=project, dry_run=dry_run)


# ── files_app commands ────────────────────────────────────────────────────────

@files_app.command("search")
def files_search(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    pattern_pos: Optional[str] = typer.Argument(None, metavar="[PATTERN]", help="Filename glob/regex (shorthand for --pattern)."),
    pattern: str = typer.Option("*", "--pattern", "-p", help="Filename glob/regex to match (e.g. '*.py', 'cli*'). Default: * (all files)"),
    min_edits: int = typer.Option(0, "--min-edits", help="Only show files edited at least this many times across all sessions. Default: 0"),
    max_edits: Optional[int] = typer.Option(None, "--max-edits", help="Only show files edited at most this many times. Default: unlimited"),
    include_extensions: Optional[str] = typer.Option(None, "--include-extensions", "-i", help="Only these file extensions, comma-separated (e.g. py,md,json)"),
    exclude_extensions: Optional[str] = typer.Option(None, "--exclude-extensions", "-x", help="Skip these file extensions, comma-separated (e.g. pyc,tmp)"),
    session: Optional[str] = _OPT_SESSION,
    include_sessions: Optional[str] = typer.Option(None, "--include-sessions", help="Advanced: multiple session UUIDs, comma-separated."),
    exclude_sessions: Optional[str] = typer.Option(None, "--exclude-sessions", help="Skip these session UUIDs, comma-separated"),
    since:  Optional[str] = _OPT_SINCE,
    until:  Optional[str] = _OPT_UNTIL,
    when:   Optional[str] = _OPT_WHEN,
    after:  Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    limit: Optional[int] = typer.Option(None, "--limit", help="Max results to return. Default: unlimited"),
    fmt: Optional[str] = _OPT_FORMAT,
) -> None:
    """Search source files found in Claude Code session data.

    Finds files (*.py, *.md, etc.) that Claude wrote or edited during sessions.
    Filter by extension, edit count, datetime range, or session ID.

    Examples:
        aise files search                                  # all files
        aise files search --pattern "*.py"                 # Python files only
        aise files search --min-edits 3                    # files edited 3+ times across sessions
        aise files search -i py,md --since 2026-01-15      # Python/Markdown since Jan 15
        aise files search --when 202X                      # files edited in the 2020s decade
    """
    engine = _resolve_engine(ctx, provider or "claude")  # file ops are Claude-only
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    # Merge positional pattern and --session into existing params
    effective_pattern = pattern_pos or pattern
    if session:
        include_sessions = f"{session},{include_sessions}" if include_sessions else session
    since, until = _normalize_date_range(since, until, when, after, before)
    _do_files_search(engine, effective_pattern, min_edits, max_edits, include_extensions, exclude_extensions, include_sessions, exclude_sessions, since, until, limit, fmt)


# Register 'find' as hidden alias for 'files search'
files_app.command("find", hidden=True)(files_search)


@files_app.command("extract")
def files_extract(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    name: str = typer.Argument(..., help="Filename (e.g. cli.py). Use 'aise files search' to find names."),
    version: Optional[int] = typer.Option(
        None, "--version", "-v",
        help="Version number to extract (default: latest). Run 'files history FILENAME' to see available versions.",
    ),
    session: Optional[str] = _OPT_SESSION,
    restore: bool = typer.Option(
        False, "--restore",
        help=(
            "Write to the path Claude originally created/edited this file, "
            "as recorded in session data. Adds .recovered suffix if the file already exists."
        ),
    ),
    output_dir: Optional[str] = typer.Option(
        None, "--output-dir", "-o",
        help=(
            "Write to this directory instead of stdout. "
            "Example: --output-dir ./backup  writes ./backup/cli.py"
        ),
    ),
    dry_run: bool = _OPT_DRY_RUN,
) -> None:
    """Extract a source file (cli.py, engine.py, etc.) from Claude Code session data.

    This works on SOURCE FILES that Claude wrote or edited — NOT the session JSONL files.
    By default prints the latest version to stdout (pipe-friendly).

    Run 'files history FILENAME' to see all available versions, then --version N to get one.

    Version:
      (none)            Latest version [default]
      --version N       Specific version number

    Destination (mutually exclusive):
      (none)            Print content to stdout [default]
      --restore         Write to the path Claude originally created this file
      --output-dir DIR  Write to DIR/filename

    Use --dry-run with any destination to preview without writing.

    Examples:
        aise files history cli.py                        # see all versions first
        aise files extract cli.py                        # latest -> stdout
        aise files extract cli.py --version 2            # v2 -> stdout
        aise files extract cli.py > cli.py               # redirect to file
        aise files extract cli.py | pbcopy               # pipe to clipboard
        aise files extract cli.py --restore              # restore to original path
        aise files extract cli.py --restore --dry-run    # preview restore
        aise files extract cli.py --output-dir ./backup  # write to ./backup/
    """
    engine = _resolve_engine(ctx, provider or "claude")  # file ops are Claude-only
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_extract(engine, name, version=version, session=session, output_dir=output_dir, restore=restore, dry_run=dry_run)


@files_app.command("history")
def files_history(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    name: str = typer.Argument(..., help="Filename (e.g. cli.py). Use 'aise files search' to find names."),
    session: Optional[str] = _OPT_SESSION,
    export: bool = typer.Option(False, "--export", help="Write all versions to disk as cli_v1.py, cli_v2.py, etc."),
    export_dir: Optional[str] = typer.Option(None, "--export-dir", help="Where to write exported files. Default: versions/ alongside original path."),
    stdout_mode: bool = typer.Option(False, "--stdout", help="Print all versions to stdout with === v1 === headers (for scripting/AI)."),
    dry_run: bool = _OPT_DRY_RUN,
    fmt: Optional[str] = _OPT_FORMAT,
    since: Optional[str] = _OPT_SINCE,
    until: Optional[str] = _OPT_UNTIL,
    when: Optional[str] = _OPT_WHEN,
    after: Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    full_uuid: bool = _OPT_FULL_UUID,
) -> None:
    """Show version history of a source file from Claude Code session data. Read-only by default.

    Displays a table of all recorded versions (version number, line count, delta-lines, timestamp,
    session ID). READ-ONLY — no files are written unless you use --export or --stdout.

    SOURCE FILES ONLY: shows history of files Claude wrote/edited, not the session JSONL files.

    Examples:
        aise files history cli.py                           # show version table
        aise files history cli.py --since 7d               # versions from last 7 days
        aise files history cli.py --when 202X              # versions in the 2020s decade
        aise files history cli.py --format json            # machine-readable
        aise files history cli.py --export                  # write cli_v1.py, cli_v2.py, ...
        aise files history cli.py --export --dry-run        # preview export
        aise files history cli.py --export --export-dir ./versions
        aise files history cli.py --stdout                  # all versions to stdout
    """
    since, until = _normalize_date_range(since, until, when, after, before)
    engine = _resolve_engine(ctx, provider or "claude")  # file ops are Claude-only
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    versions = engine.get_versions(name)

    if session:
        versions = [v for v in versions if v.session_id.startswith(session)]

    # Date filter: keep only versions whose timestamp falls in [since, until]
    if since or until:
        from ai_session_search.engine import _passes_date_filter
        versions = [v for v in versions if _passes_date_filter(v.timestamp or "", since, until)]

    if not versions:
        err_console.print(f"[red]No versions found for:[/red] {name}  (check filters)")
        raise typer.Exit(code=1)

    if stdout_mode:
        _do_history_stdout(engine, name, versions=versions)
    else:
        _do_history_display(engine, name, versions=versions, fmt=fmt, full_uuid=full_uuid)
        if export:
            _do_history_export(engine, name, export_dir=export_dir, dry_run=dry_run, versions=versions)
        elif dry_run:
            console.print("[yellow]--dry-run has no effect without --export[/yellow]")


@files_app.command("cross-ref")
def files_cross_ref(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    file: str = typer.Argument(..., help="Path to a file to compare against session edits (e.g. ./cli.py)."),
    session: Optional[str] = _OPT_SESSION,
    fmt: Optional[str] = _OPT_FORMAT,
    full_uuid: bool = _OPT_FULL_UUID,
) -> None:
    """Show which edits Claude made to a file are present in its current version.

    Examples:
        aise files cross-ref ./cli.py
        aise files cross-ref ./engine.py --session ab841016
        aise files cross-ref ./cli.py --format json
    """
    engine = _resolve_engine(ctx, provider or "claude")  # file ops are Claude-only
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_files_cross_ref(engine, file, session, fmt, full_uuid=full_uuid)


# Add 'find' as alias for 'files search' (registered after files_search is defined below)

# ── messages_app commands ─────────────────────────────────────────────────────

def _messages_search_cmd(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    query: Optional[list[str]] = typer.Argument(None, help="Text to search for in messages. Multi-word queries work with or without quotes."),
    query_opt: Optional[str] = typer.Option(None, "--query", "-q", hidden=True),
    message_type: Optional[MsgFilterType] = typer.Option(
        None, "--type", "-t",
        help="Filter by message type: user, assistant, tool, slash (real slash command invocations only, identified by <command-name> XML tag), compaction (context-compaction summaries). Typer shows valid values automatically.",
    ),
    limit: Optional[int] = _opt_limit(help_noun="messages"),
    max_chars: Optional[int] = _OPT_MAX_CHARS,
    fmt: Optional[str] = _OPT_FORMAT,
    tool: Optional[str] = typer.Option(
        None, "--tool",
        help="Filter for tool call invocations (e.g. Bash, Edit, Write). Implies --type assistant.",
    ),
    context: int = typer.Option(
        0, "--context", "-c",
        help="Include N messages before and after each match (from the same session). Default: 0.",
    ),
    context_before: int = typer.Option(
        -1, "--context-before",
        help="Show N messages before each match. Overrides --context for the before-side. 0 = no before context. Default (-1): uses --context value.",
    ),
    context_after: int = typer.Option(
        -1, "--context-after",
        help="Show N messages after each match. Overrides --context for the after-side. 0 = no after context. Default (-1): uses --context value.",
    ),
    exclude_compaction: bool = typer.Option(
        False, "--no-compaction",
        help="Exclude context-compaction summary messages from results. Most useful with --type user. Redundant with --type slash. Contradicts --type compaction (warns, returns 0 results).",
    ),
    session: Optional[str] = typer.Option(
        None, "--session",
        help="Scope search to one session ID or prefix. Find IDs via 'aise list'.",
    ),
    since:  Optional[str] = _OPT_SINCE,
    until:  Optional[str] = _OPT_UNTIL,
    when:   Optional[str] = _OPT_WHEN,
    after:  Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    regex: bool = typer.Option(
        False, "--regex",
        help="Treat query as a regex pattern (case-insensitive). "
             "Default is literal substring match (faster, no metacharacter surprises). "
             "Enables |, .*, \\b, etc.",
    ),
    after_index: int = typer.Option(
        0, "--after-index",
        help="Skip the first N messages in each session before searching. "
             "Combine with --session to search after a specific point.",
    ),
    after_timestamp: Optional[str] = typer.Option(
        None, "--after-timestamp",
        help="Skip messages before this timestamp within each session. "
             "Accepts same date formats as --since (ISO, relative like '14d').",
    ),
    full_uuid: bool = _OPT_FULL_UUID,
    output: Optional[str] = _OPT_OUTPUT,
) -> None:
    """Search conversation messages across all configured session sources.

    Accessible as both 'messages search' and 'messages find' (aliases).
    Query is a literal substring match by default (fast, case-insensitive).
    Use --regex for regex patterns.

    Examples:
        aise messages search "authentication"                         # literal match (default)
        aise messages search "file.txt"                               # . is literal, not regex
        aise messages search "forgot|missed" --regex                   # regex OR (requires --regex)
        aise messages search "step by step" --type assistant          # AI turns only
        aise messages find "error"                                    # find is an alias
        aise messages search "*" --tool Write                         # all Write tool calls
        aise messages search "error" --context 3                      # show surrounding messages
        aise messages search "" --type slash --context-after 5        # slash commands + trailing context
        aise messages search "" --type user --no-compaction           # typed user messages only
        aise messages search "" --type compaction --since 14d         # inspect compacted content
        aise messages search "error" --session 83326782               # scope to one session
        aise messages search "error" --session abc --after-index 50   # skip first 50 msgs
    """
    q = " ".join(query) if query else query_opt
    msg_type_str = message_type.value if message_type else None
    # fixed_strings is the inverse of regex: literal by default, regex opt-in
    fixed_strings = not regex
    # Contradiction check: --type compaction --no-compaction -> empty result
    if message_type == MsgFilterType.compaction and exclude_compaction:
        err_console.print("[yellow]Warning:[/yellow] --type compaction --no-compaction contradicts itself. Returning 0 results.")
        raise typer.Exit(0)
    # Get backend from composition root (ctx.obj injected by app_callback)
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    since, until = _normalize_date_range(since, until, when, after, before)
    # Normalize after_timestamp the same way as --since
    if after_timestamp:
        from ai_session_search.engine import _parse_date_input
        after_timestamp = _parse_date_input(after_timestamp)
    _do_messages_search(engine, q or "", msg_type_str, limit, max_chars, fmt,
                        tool=tool, context=context,
                        context_before=context_before, context_after=context_after,
                        exclude_compaction=exclude_compaction, session=session,
                        since=since, until=until,
                        full_uuid=full_uuid, output=output,
                        fixed_strings=fixed_strings,
                        after_index=after_index, after_timestamp=after_timestamp)


_register_alias(messages_app, _messages_search_cmd, "search", "find")


@messages_app.command("get")
def messages_get(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    session_id: Optional[str] = typer.Argument(None, help="Session ID (prefix match, e.g. ab841016). Find IDs via 'aise list'."),
    session_opt: Optional[str] = typer.Option(None, "--session", "-s", hidden=True),
    message_type: Optional[str] = typer.Option(None, "--type", "-t", help="Show only 'user' or 'assistant' messages. Default: both"),
    limit: Optional[int] = _opt_limit(help_noun="messages"),
    max_chars: Optional[int] = _OPT_MAX_CHARS,
    fmt: Optional[str] = _OPT_FORMAT,
    since: Optional[str] = _OPT_SINCE,
    until: Optional[str] = _OPT_UNTIL,
    when: Optional[str] = _OPT_WHEN,
    after: Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
) -> None:
    """Read messages from one specific Claude Code session.

    Examples:
        aise messages get ab841016              # positional
        aise messages get --session ab841016    # flag (backward compat)
        aise messages get ab841016 --type user
        aise messages get ab841016 --since 14:00  # messages after 2pm
    """
    since, until = _normalize_date_range(since, until, when, after, before)
    sid = session_id or session_opt
    # Get backend from composition root (ctx.obj injected by app_callback)
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_get(engine, sid, message_type, limit, max_chars, fmt, since=since, until=until)


@messages_app.command("corrections")
def messages_corrections(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    project: Optional[str] = typer.Option(None, "--project", help="Filter by project directory substring."),
    session: Optional[str] = typer.Option(
        None, "--session",
        help="Scope corrections to one session ID or prefix. Find IDs via 'aise list'.",
    ),
    since:  Optional[str] = _OPT_SINCE,
    until:  Optional[str] = _OPT_UNTIL,
    when:   Optional[str] = _OPT_WHEN,
    after:  Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    limit: Optional[int] = _opt_limit(help_noun="corrections"),
    fmt: Optional[str] = _OPT_FORMAT,
    pattern: Optional[List[str]] = typer.Option(
        None, "--pattern",
        help=(
            "Custom correction pattern: 'CATEGORY:REGEX'. Repeatable. "
            "When provided, replaces built-in patterns entirely. "
            "Categories are arbitrary labels (e.g. regression, skip_step). "
            "Multiple --pattern flags with the same category are ORed together.\n\n"
            "Example: --pattern 'skip_step:you missed' --pattern 'skip_step:you forgot' "
            "--pattern 'custom:you broke it'"
        ),
    ),
    full_uuid: bool = _OPT_FULL_UUID,
    ids_only: bool = _OPT_IDS_ONLY,
    output: Optional[str] = _OPT_OUTPUT,
) -> None:
    """Find user messages where corrections were given to Claude.

    By default detects patterns like 'you forgot', 'nono', 'that's wrong', etc.
    across four built-in categories: regression, skip_step, misunderstanding, incomplete.

    Use --pattern to supply your own patterns instead of the built-in set.
    Each --pattern value is 'CATEGORY:REGEX' where REGEX is a Python regex.
    Multiple --pattern values with the same category are combined (OR logic).
    When any --pattern is given, built-in patterns are NOT used.

    Returns ALL matches by default. On large session corpora this can be
    thousands of rows (and may overwhelm an LLM consuming the output); pass
    --limit N, or set config 'defaults.limit', to cap it.

    Examples:
        aise messages corrections
        aise messages corrections --limit 50 --project myproject
        aise messages corrections --session 83326782
        aise messages corrections --ids-only | xargs -I{} aise export session {}
        aise messages corrections --format json
        aise messages corrections --pattern 'regression:you deleted' --pattern 'regression:you removed'
        aise messages corrections --pattern 'oops:nono' --pattern 'oops:that.s wrong'
        aise messages corrections --output ~/corrections.txt
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    since, until = _normalize_date_range(since, until, when, after, before)
    if ids_only:
        corrections = engine.find_corrections(
            project_filter=project, since=since, until=until,
            limit=limit, session_id_prefix=session,
        )
        _output_ids_only(corrections)
        return
    _do_messages_corrections(engine, project, since, until, limit, fmt,
                              pattern_overrides=pattern, full_uuid=full_uuid, output=output,
                              session=session)


@messages_app.command("planning")
def messages_planning(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    project: Optional[str] = typer.Option(None, "--project", help="Filter by project directory substring."),
    since:  Optional[str] = _OPT_SINCE,
    until:  Optional[str] = _OPT_UNTIL,
    when:   Optional[str] = _OPT_WHEN,
    after:  Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    fmt: Optional[str] = _OPT_FORMAT,
    commands: Optional[str] = typer.Option(
        None, "--commands",
        help=(
            "Comma-separated list of slash-command patterns to count. "
            "When provided, replaces the built-in list entirely. "
            "Each entry is a Python regex (word boundaries added automatically if absent). "
            "Example: --commands '/ar:plannew,/ar:pn,/myplanning,/plan'"
        ),
    ),
    detail: bool = typer.Option(
        False, "--detail",
        help="Show individual invocations instead of aggregate counts. Includes timestamp, session, args, cwd, and branch per invocation. Implied by --context-after.",
    ),
    context_after: int = typer.Option(
        0, "--context-after",
        help="For each slash command invocation, show N messages that followed it. Implies --detail. Default: 0 (count summary only).",
    ),
    full_uuid: bool = _OPT_FULL_UUID,
    ids_only: bool = _OPT_IDS_ONLY,
    max_chars: Optional[int] = _OPT_MAX_CHARS,
    output: Optional[str] = _OPT_OUTPUT,
) -> None:
    """Show slash-command usage frequency across all sessions.

    By default counts the built-in planning commands:
      /ar:plannew, /ar:pn, /ar:planrefine, /ar:pr, /ar:planupdate, /ar:pu,
      /ar:planprocess, /ar:pp, /plannew, /planrefine, /planupdate, /planprocess

    These defaults reflect autorun plugin commands. Use --commands to count
    your own slash commands instead (built-in list is not used when --commands
    is given).

    Each entry in --commands is matched as a Python regex, case-insensitive.
    The display name strips trailing \\b from each pattern.

    Examples:
        aise messages planning
        aise messages planning --project myproject
        aise messages planning --format json
        aise messages planning --commands '/ar:plannew,/ar:pn'
        aise messages planning --detail --since 14d              # per-invocation table
        aise messages planning --context-after 3 --since 14d    # show 3 messages after each
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    since, until = _normalize_date_range(since, until, when, after, before)
    if ids_only:
        invocations = engine.get_planning_usage(
            project_filter=project, since=since, until=until,
            commands=_parse_commands_option(commands) if commands else None,
            return_invocations=True,
        )
        _output_ids_only(invocations)
        return
    max_chars_val = max_chars if max_chars is not None else _cfg_default("max_chars", 0)
    _do_messages_planning(engine, project, since, until, fmt, commands_raw=commands,
                          detail=detail or context_after > 0,
                          context_after=context_after, output=output,
                          full_uuid=full_uuid, max_chars=max_chars_val)


@messages_app.command("inspect")
def messages_inspect(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    session_id: str = typer.Argument(..., help="Session ID prefix (e.g. ab841016). Find IDs via 'aise list'."),
    fmt: Optional[str] = _OPT_FORMAT,
) -> None:
    """Show per-session statistics: message counts, tool usage, and files touched.

    Files touched are detected from any tool_use block that includes a file_path
    input — not hardcoded to specific tool names.

    Examples:
        aise messages inspect ab841016
        aise messages inspect ab841016 --format json
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_messages_analyze(engine, session_id, fmt)


@messages_app.command("timeline")
def messages_timeline(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    session_id: str = typer.Argument(..., help="Session ID prefix (e.g. ab841016). Find IDs via 'aise list'."),
    fmt: Optional[str] = _OPT_FORMAT,
    preview_chars: int = typer.Option(
        150, "--preview-chars",
        help="Max characters to show in content preview column. Default: 150",
    ),
    message_type: Optional[MsgFilterType] = typer.Option(
        None, "--type", "-t",
        help="Show only events of this type: user, assistant, tool, slash (real slash command invocations via <command-name> XML tag), compaction (context-compaction summaries). Default: all.",
    ),
    grep: Optional[str] = typer.Option(
        None, "--grep",
        help="Show only events whose content matches this pattern (case-insensitive literal substring). Use -E for regex. Applied after --type filtering.",
    ),
    grep_regex: bool = typer.Option(
        False, "--regex", "-E",
        help="Treat --grep pattern as regex instead of literal substring.",
    ),
    exclude_compaction: bool = typer.Option(
        False, "--no-compaction",
        help="Exclude context-compaction summary events. Most useful with --type user. Contradicts --type compaction (warns, returns 0 results).",
    ),
    since: Optional[str] = _OPT_SINCE,
    until: Optional[str] = _OPT_UNTIL,
    when: Optional[str] = _OPT_WHEN,
    after: Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    output: Optional[str] = _OPT_OUTPUT,
) -> None:
    """Show a chronological timeline of user/assistant events for a session.

    Each row shows the message type, timestamp, number of tool calls invoked,
    and a preview of the message content.

    Examples:
        aise messages timeline ab841016
        aise messages timeline ab841016 --format json
        aise messages timeline ab841016 --preview-chars 80
        aise messages timeline ab841016 --type assistant        # AI turns only
        aise messages timeline ab841016 --type slash            # slash command invocations
        aise messages timeline ab841016 --type compaction       # compaction summary events
        aise messages timeline ab841016 --type user --no-compaction  # typed user messages only
        aise messages timeline ab841016 --grep "plannew"        # events mentioning plannew
        aise messages timeline ab841016 --since 14:00           # events after 2pm
        aise messages timeline ab841016 --output ~/timeline.txt # save to file
    """
    # Contradiction check: --type compaction --no-compaction -> empty result
    if message_type == MsgFilterType.compaction and exclude_compaction:
        err_console.print("[yellow]Warning:[/yellow] --type compaction --no-compaction contradicts itself. Returning 0 results.")
        raise typer.Exit(0)
    msg_type_str = message_type.value if message_type else None
    since, until = _normalize_date_range(since, until, when, after, before)
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_messages_timeline(engine, session_id, fmt, preview_chars=preview_chars,
                          message_type=msg_type_str, since=since, until=until,
                          grep=grep, grep_regex=grep_regex,
                          exclude_compaction=exclude_compaction,
                          output=output)


# ── aise commands / aise slash subcommand group ──────────────────────────────
# Lists and inspects slash command invocations across all sessions.
# 'commands' and 'slash' are both registered as group names (aliases).


@slash_app.command("list")
def slash_list(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    command: Optional[str] = typer.Option(
        None, "--command",
        help="Filter to invocations matching this pattern (substring or regex). Default: all slash commands.",
    ),
    since:  Optional[str] = _OPT_SINCE,
    until:  Optional[str] = _OPT_UNTIL,
    when:   Optional[str] = _OPT_WHEN,
    after:  Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    fmt:    Optional[str] = _OPT_FORMAT,
    full_uuid: bool = _OPT_FULL_UUID,
    ids_only: bool = _OPT_IDS_ONLY,
    limit: Optional[int] = _opt_limit(help_noun="invocations"),
    output: Optional[str] = _OPT_OUTPUT,
) -> None:
    """List every slash command invocation across all sessions.

    Each row shows the command, args, timestamp, session ID, and project.
    Use --command to filter to a specific command or regex pattern.

    Examples:
        aise commands list                                      # all slash commands
        aise commands list --command /ar:plannew                # only /ar:plannew
        aise commands list --since 14d --output ~/cmds.txt      # last 14 days, save to file
        aise commands list --format json                        # JSON output
        aise commands list --ids-only | xargs -I{} aise export session {}
        aise slash list                                         # same via alias
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    since, until = _normalize_date_range(since, until, when, after, before)
    invocations = engine.get_planning_usage(
        since=since, until=until, return_invocations=True, limit=0,
    )
    if command:
        try:
            _check_redos_safe(command)
            cmd_re = _re.compile(command, _re.IGNORECASE)
            invocations = [inv for inv in invocations if cmd_re.search(inv.command)]
        except (ValueError, _re.error) as exc:
            msg = str(exc)
            if "ReDoS" in msg:
                err_console.print(f"[yellow]Warning:[/yellow] --command pattern rejected: {msg}. Falling back to literal substring match.")
            invocations = [inv for inv in invocations if command.lower() in inv.command.lower()]
    if limit is not None:
        invocations = invocations[:limit]
    if ids_only:
        _output_ids_only(invocations)
        return
    spec = _spec_with_full_uuid(_INVOCATIONS_SPEC, 1) if full_uuid else _INVOCATIONS_SPEC
    _render_output(invocations, fmt, spec, "No slash command invocations found.", output=output)


@slash_app.command("context")
def slash_context(
    ctx: typer.Context,
    command: str = typer.Argument(..., help="Slash command to look up (e.g. /ar:plannew, /commit)."),
    provider: Optional[str] = _OPT_PROVIDER,
    context_after: int = typer.Option(
        5, "--context-after", "-n",
        help="Number of messages to show after each invocation. Default: 5.",
    ),
    since:  Optional[str] = _OPT_SINCE,
    until:  Optional[str] = _OPT_UNTIL,
    when:   Optional[str] = _OPT_WHEN,
    after:  Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    max_chars: Optional[int] = _OPT_MAX_CHARS,
    fmt: Optional[str] = _OPT_FORMAT,
    output: Optional[str] = _OPT_OUTPUT,
) -> None:
    """For each invocation of COMMAND, show what followed it.

    Shows the N messages that came after each slash command invocation,
    useful for reviewing the AI's response to a planning or task command.

    Examples:
        aise commands context /ar:plannew
        aise commands context /ar:plannew --context-after 10
        aise commands context /commit --since 14d --output ~/commit-context.txt
        aise commands context /ar:plannew --format json
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    since, until = _normalize_date_range(since, until, when, after, before)
    invocations = engine.get_planning_usage(
        since=since, until=until, return_invocations=True, limit=0,
    )
    try:
        _check_redos_safe(command)
        cmd_re = _re.compile(command, _re.IGNORECASE)
        invocations = [inv for inv in invocations if cmd_re.search(inv.command)]
    except (ValueError, _re.error) as exc:
        msg = str(exc)
        if "ReDoS" in msg:
            err_console.print(f"[yellow]Warning:[/yellow] --command pattern rejected: {msg}. Falling back to literal substring match.")
        invocations = [inv for inv in invocations if command.lower() in inv.command.lower()]
    if fmt and fmt in ("json", "csv"):
        import json as _json
        items = []
        for inv in invocations:
            messages = engine.get_messages(inv.session_id)
            raw_after = [m for m in messages if (m.timestamp or "") > inv.timestamp]
            # Filter skill injections (same logic as _render_invocations_with_context)
            filtered: list = []
            _prev_slash = True  # inv is a slash command
            for m in raw_after:
                _mtype = m.type.value if hasattr(m.type, "value") else str(m.type)
                if _is_skill_injection({"type": _mtype}, m.content, _prev_slash):
                    _prev_slash = False
                    continue
                _prev_slash = "<command-name>" in (m.content or "")
                filtered.append(m)
                if len(filtered) >= context_after:
                    break
            items.append({
                **inv.to_dict(),
                "context_after": [{"type": m.type.value, "timestamp": m.timestamp, "content": m.content} for m in filtered],
            })
        if fmt == "json":
            import json as _json2
            json_text = _json2.dumps(items, indent=2) + "\n"
            if output:
                Path(output).expanduser().write_text(json_text, encoding="utf-8")
            else:
                sys.stdout.write(json_text)
        else:
            for item in items:
                console.print(f"{item['timestamp'][:19]}  {item['command']}  {item.get('args', '')}")
            if output:
                _write_output(console, output)
        return
    max_chars_val = max_chars if max_chars is not None else _cfg_default("max_chars", 0)
    _render_invocations_with_context(invocations, context_after, engine, output, max_chars=max_chars_val)


#: Supported content extraction types for ``messages extract``.
_EXTRACT_TYPES = ("pbcopy",)

_EXTRACT_PBCOPY_SPEC = TableSpec(
    title_template="Clipboard entries ({n} found)",
    columns=[
        ColumnSpec("Timestamp", style="dim", no_wrap=True),
        ColumnSpec("Content"),
    ],
    row_fn=lambda d: [d["timestamp"][:19], d["content"][:120]],
    summary_template="Found {n} clipboard entries",
    plain_fn=lambda d: d["content"],
)


def _do_messages_extract(
    engine: "SessionRecoveryEngine",
    session_id: str,
    content_type: str,
    fmt: str = "table",
    limit: Optional[int] = None,
    since: Optional[str] = None,   # canonical; after= is a hidden alias
    until: Optional[str] = None,   # canonical; before= is a hidden alias
) -> None:
    """Extract specific content type from a session."""
    if content_type not in _EXTRACT_TYPES:
        err_console.print(
            f"[red]Unknown type: {content_type!r}. "
            f"Supported types: {', '.join(_EXTRACT_TYPES)}[/red]"
        )
        raise typer.Exit(code=1)
    if content_type == "pbcopy":
        results = engine.get_clipboard_content(session_id)
        if not results:
            err_console.print(
                f"[red]No session found or no clipboard content for: {session_id!r}[/red]"
            )
            raise typer.Exit(code=1)
        if since or until:
            from ai_session_search.engine import _passes_date_filter
            results = [r for r in results if _passes_date_filter(r.get("timestamp", ""), since, until)]
        if limit:
            results = results[:limit]
        _render_output(results, fmt, _EXTRACT_PBCOPY_SPEC, "No clipboard content found")


@messages_app.command("extract")
def messages_extract(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    session_id: str = typer.Argument(..., help="Session ID prefix (e.g. dddd0001). Find IDs via 'aise list'."),
    content_type: str = typer.Argument(
        ...,
        help=f"Content type to extract. Supported: {', '.join(_EXTRACT_TYPES)}.",
        metavar="TYPE",
    ),
    fmt: Optional[str] = _OPT_FORMAT,
    limit: Optional[int] = typer.Option(None, "--limit", help="Max items to return. Default: unlimited."),
    since: Optional[str] = _OPT_SINCE,
    until: Optional[str] = _OPT_UNTIL,
    when: Optional[str] = _OPT_WHEN,
    after: Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
) -> None:
    """Extract specific content from a Claude Code session.

    Currently supports:

    \\b

    **pbcopy** — text that was piped to the system clipboard via:

        cat <<'EOF' | pbcopy

        ...content...

        EOF

    Examples:

        aise messages extract dddd0001 pbcopy

        aise messages extract dddd0001 pbcopy --format json

        aise messages extract dddd0001 pbcopy --format plain

        aise messages extract dddd0001 pbcopy --limit 3
    """
    since, until = _normalize_date_range(since, until, when, after, before)
    engine = _resolve_engine(ctx, provider or "claude")  # clipboard ops are Claude-only
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_messages_extract(engine, session_id, content_type, fmt, limit=limit, since=since, until=until)


# ── tools_app commands ────────────────────────────────────────────────────────

def _tools_search_cmd(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    tool: str = typer.Argument(..., help="Tool name (e.g. Bash, Edit, Write, Read, Glob, Grep)."),
    query: Optional[str] = typer.Argument(None, help="Optional text to match in tool input. Omit to list all uses."),
    limit: Optional[int] = _opt_limit(),
    max_chars: Optional[int] = _OPT_MAX_CHARS,
    fmt: Optional[str] = _OPT_FORMAT,
    since: Optional[str] = _OPT_SINCE,
    until: Optional[str] = _OPT_UNTIL,
    when: Optional[str] = _OPT_WHEN,
    after: Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    include_result: bool = typer.Option(False, "--include-result",
        help="Include tool output metadata (stdout/stderr char and word counts) in JSON output. "
             "Requires a second pass over session files; off by default for performance."),
) -> None:
    """Search or find tool invocations from Claude Code sessions.

    Accessible as both 'tools search' and 'tools find' (aliases).

    Examples:
        aise tools search Write                      # all Write calls
        aise tools search Bash "git commit"          # Bash calls with "git commit"
        aise tools find Edit "cli.py"                # find alias
        aise tools search Write --format json        # JSON output
        aise tools search Bash --since 7d            # recent Bash calls only
        aise tools search Bash --when 202X           # Bash calls in the 2020s decade
        aise tools search Bash --include-result --format json  # include output metadata
    """
    since, until = _normalize_date_range(since, until, when, after, before)
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_messages_search(
        engine, query or "", message_type="assistant",
        limit=limit, max_chars=max_chars, fmt=fmt, tool=tool,
        since=since, until=until, include_tool_result=include_result,
    )


_register_alias(tools_app, _tools_search_cmd, "search", "find")


# ── Root commands ─────────────────────────────────────────────────────────────

@app.command("dates")
def dates_reference() -> None:
    """Show full date/time format reference for --since, --until, and --when flags."""
    from rich.markdown import Markdown
    _console().print(Markdown("""\
# Date/Time Format Reference

All date flags (`--since`, `--until`, `--when`) accept the following formats:

## ISO 8601 Dates
| Input | Meaning |
|-------|---------|
| `2026-01-15` | Any time on January 15, 2026 |
| `2026-01-15T14:30:00` | Specific date and time |

## Partial Dates (EDTF Level 0)
| Input | Meaning |
|-------|---------|
| `2026-01` | All of January 2026 |
| `2026` | All of year 2026 |

## Unspecified Digits (EDTF Level 1)
| Input | Meaning |
|-------|---------|
| `202X` or `202x` | The 2020s decade (2020-01-01 to 2029-12-31) |
| `19XX` | The 1900s century (1900-01-01 to 1999-12-31) |
| `2026-01-1X` | January 10-19, 2026 |

## EDTF Intervals (--since only, sets both lower and upper bounds)
| Input | Meaning |
|-------|---------|
| `2026-01/2026-03` | January through March 2026 |

## Durations (relative to now)
| Input | Meaning |
|-------|---------|
| `7d` | 7 days ago |
| `2w` | 2 weeks ago |
| `1m` | ~30 days ago |
| `24h` | 24 hours ago |
| `30min` | 30 minutes ago |

## Natural Language (via python-dateutil)
| Input | Meaning |
|-------|---------|
| `yesterday` | Yesterday at midnight |
| `3 days ago` | 3 days ago |

## Flag Semantics

| Flag | Lower bound | Upper bound | Best for |
|------|------------|------------|---------|
| `--since VALUE` | lower_strict(VALUE) | — | "from this date onward" |
| `--until VALUE` | — | upper_strict(VALUE) | "up to this date" |
| `--when VALUE` | lower_strict(VALUE) | upper_strict(VALUE) | EDTF periods: `202X`, `2026-01-1X`, `2026-01` |

**Key difference:** `--when 202X` filters to exactly the 2020s decade.
`--since 202X` means "from 2020 onward" with no upper limit.

## Examples

```
aise list --since 7d
aise list --when 202X
aise list --when 2026-01-1X
aise list --since 2026-01 --until 2026-03
aise list --since 2026-01/2026-03
aise search messages --query "bug" --since 2w
aise messages corrections --when 2026
```

## EDTF Specification

Full specification: https://www.loc.gov/standards/datetime/
"""))


@app.command("list")
def list_sessions(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    project: Optional[str] = typer.Option(None, "--project", help="Filter by project directory substring."),
    since:  Optional[str] = _OPT_SINCE,
    until:  Optional[str] = _OPT_UNTIL,
    when:   Optional[str] = _OPT_WHEN,
    after:  Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    limit: Optional[int] = typer.Option(None, "--limit", help="Max sessions to return. Default: unlimited."),
    fmt: Optional[str] = _OPT_FORMAT,
    full_uuid: bool = _OPT_FULL_UUID,
    ids_only: bool = _OPT_IDS_ONLY,
) -> None:
    """List sessions with metadata.

    Examples:
        aise list                              # all configured sessions
        aise list --provider aistudio          # AI Studio sessions only
        aise list --project myproject          # filter by project
        aise list --since 7d                   # sessions in the last 7 days
        aise list --when 202X                  # all sessions in the 2020s decade
        aise list --since 2026-01 --until 2026-03  # January through March 2026
        aise list --format json                # JSON output
        aise list --full-uuid                  # show full 36-char session UUIDs
        aise list --ids-only | xargs -I{} aise messages search "error" --session {}
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    since, until = _normalize_date_range(since, until, when, after, before)
    if ids_only:
        sessions = engine.get_sessions(project_filter=project, since=since, until=until)
        if limit:
            sessions = sessions[:limit]
        _output_ids_only(sessions)
        return
    _do_list_sessions(engine, project, since, until, limit, fmt, full_uuid=full_uuid)


def _root_search_cmd(
    ctx: typer.Context,
    domain: Optional[str] = typer.Argument(None, metavar="[files|messages|tools]",
        help="Domain: 'files', 'messages', or 'tools' (tool calls). Auto-detected from flags."),
    pattern: Optional[str] = typer.Option(None, "--pattern", "-p", help="[files] Filename glob/regex to match (e.g. '*.py', 'cli*'). Default: * (all files)"),
    query: Optional[str] = typer.Option(None, "--query", "-q", help="[messages] Text to search for in user/assistant message content"),
    min_edits: int = typer.Option(0, "--min-edits", help="[files] Only show files edited at least this many times. Default: 0"),
    max_edits: Optional[int] = typer.Option(None, "--max-edits", help="[files] Only show files edited at most this many times. Default: unlimited"),
    include_extensions: Optional[str] = typer.Option(None, "--include-extensions", "-i", help="[files] Only these file extensions, comma-separated (e.g. py,md,json)"),
    exclude_extensions: Optional[str] = typer.Option(None, "--exclude-extensions", "-x", help="[files] Skip these file extensions, comma-separated (e.g. pyc,tmp)"),
    session: Optional[str] = _OPT_SESSION,
    include_sessions: Optional[str] = typer.Option(None, "--include-sessions", help="[files] Advanced: multiple session UUIDs, comma-separated."),
    exclude_sessions: Optional[str] = typer.Option(None, "--exclude-sessions", help="[files] Skip these session UUIDs, comma-separated"),
    since:  Optional[str] = _OPT_SINCE,
    until:  Optional[str] = _OPT_UNTIL,
    when:   Optional[str] = _OPT_WHEN,
    after:  Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
    message_type: Optional[str] = typer.Option(None, "--type", "-t", help="[messages] Show only 'user' or 'assistant' messages. Default: both"),
    limit: Optional[int] = typer.Option(None, "--limit", help="Max results to return. Default: unlimited (None)"),
    max_chars: Optional[int] = _OPT_MAX_CHARS,
    fmt: Optional[str] = _OPT_FORMAT,
    tool: Optional[str] = typer.Option(None, "--tool",
        help="[messages/tools] Filter for tool call invocations (e.g. Bash, Edit, Write). Auto-routes to messages domain."),
    provider: Optional[str] = _OPT_PROVIDER,
) -> None:
    """Search messages, files, and tool calls across all sources.

    Accessible as both 'search' and 'find' at the root level (aliases).

    Examples:
        aise search messages --query "error"                 # messages containing "error"
        aise search messages --query "error" --provider claude # Claude sessions only
        aise search files --pattern "*.py"                   # Python files edited by Claude
        aise search tools --tool Write --query "login"       # Write calls with "login"
        aise search --tool Bash --query "git commit"         # auto-routes to messages
        aise search messages --query "bug" --since 2w        # last 2 weeks
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    # Merge --session into --include-sessions for unified filtering
    if session:
        include_sessions = f"{session},{include_sessions}" if include_sessions else session
    since, until = _normalize_date_range(since, until, when, after, before)
    _do_search(
        engine, domain, pattern, query, min_edits, max_edits,
        include_extensions, exclude_extensions, include_sessions, exclude_sessions,
        since, until, message_type, limit, max_chars, fmt, tool=tool,
    )


_register_alias(app, _root_search_cmd, "search", "find")


@app.command()
def extract(
    ctx: typer.Context,
    name: str = typer.Argument(..., help="Filename to extract (e.g. cli.py). Use 'aise search' to find available names."),
    version: Optional[int] = typer.Option(
        None, "--version", "-v",
        help="Version number to extract (default: latest). Run 'history FILENAME' to see available versions.",
    ),
    session: Optional[str] = _OPT_SESSION,
    restore: bool = typer.Option(
        False, "--restore",
        help="Write to the path Claude originally created/edited this file.",
    ),
    output_dir: Optional[str] = typer.Option(
        None, "--output-dir", "-o",
        help="Write to this directory instead of stdout. Example: --output-dir ./backup",
    ),
    dry_run: bool = _OPT_DRY_RUN,
) -> None:
    """Extract the latest (or a specific) version of a source file from session history.

    Equivalent to 'aise files extract'. Use 'aise search' to find available filenames.
    By default prints the latest version to stdout (pipe-friendly).

    See also: 'aise messages get <session-id>' to retrieve session messages.
    """
    engine = ctx.obj.get("engine") if ctx.obj else None
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_extract(engine, name, version=version, session=session, output_dir=output_dir, restore=restore, dry_run=dry_run)


@app.command()
def history(
    ctx: typer.Context,
    name: str = typer.Argument(..., help="Filename to show history for (e.g. cli.py)."),
    session: Optional[str] = _OPT_SESSION,
    export: bool = typer.Option(False, "--export", help="Write all versions to disk as cli_v1.py, cli_v2.py, etc."),
    export_dir: Optional[str] = typer.Option(None, "--export-dir", help="Where to write exported files."),
    stdout_mode: bool = typer.Option(False, "--stdout", help="Print all versions to stdout with === v1 === headers."),
    dry_run: bool = _OPT_DRY_RUN,
    fmt: Optional[str] = _OPT_FORMAT,
    provider: Optional[str] = _OPT_PROVIDER,
    full_uuid: bool = _OPT_FULL_UUID,
    since: Optional[str] = _OPT_SINCE,
    until: Optional[str] = _OPT_UNTIL,
    when: Optional[str] = _OPT_WHEN,
    after: Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
) -> None:
    """Show all recorded versions of a file across sessions.

    Equivalent to 'aise files history'. Creates a table of all recorded versions.
    READ-ONLY — no files are written unless you use --export or --stdout.
    """
    engine = _resolve_engine(ctx, provider or "claude")  # file ops are Claude-only
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    since_val, until_val = _normalize_date_range(since, until, when, after, before)
    versions = engine.get_versions(name)

    if session:
        versions = [v for v in versions if v.session_id.startswith(session)]
    if since_val or until_val:
        from ai_session_search.engine import _passes_date_filter
        versions = [v for v in versions if _passes_date_filter(v.timestamp, since_val, until_val)]

    if not versions:
        err_console.print(f"[red]No versions found for:[/red] {name}  (check filters)")
        raise typer.Exit(code=1)

    if stdout_mode:
        _do_history_stdout(engine, name, versions=versions)
    else:
        _do_history_display(engine, name, versions=versions, fmt=fmt, full_uuid=full_uuid)
        if export:
            _do_history_export(engine, name, export_dir=export_dir, dry_run=dry_run, versions=versions)
        elif dry_run:
            console.print("[yellow]--dry-run has no effect without --export[/yellow]")


@app.command()
def get(
    ctx: typer.Context,
    session_id: Optional[str] = typer.Argument(None, help="Session ID (prefix match, e.g. ab841016)."),
    session_opt: Optional[str] = typer.Option(None, "--session", "-s", hidden=True),
    message_type: Optional[str] = typer.Option(None, "--type", "-t", help="Show only 'user' or 'assistant' messages. Default: both"),
    limit: Optional[int] = _opt_limit(help_noun="messages"),
    max_chars: Optional[int] = _OPT_MAX_CHARS,
    fmt: Optional[str] = _OPT_FORMAT,
    provider: Optional[str] = _OPT_PROVIDER,
    since: Optional[str] = _OPT_SINCE,
    until: Optional[str] = _OPT_UNTIL,
    when: Optional[str] = _OPT_WHEN,
    after: Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
) -> None:
    """Read messages from one specific session.

    Equivalent to 'aise messages get'. Find session IDs with 'aise list'.

    Examples:
        aise get ab841016
        aise get ab841016 --type user --limit 50
    """
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    sid = session_id or session_opt
    since_val, until_val = _normalize_date_range(since, until, when, after, before)
    _do_get(engine, sid, message_type, limit, max_chars, fmt, since=since_val, until=until_val)


@app.command()
def stats(
    ctx: typer.Context,
    provider: Optional[str] = _OPT_PROVIDER,
    fmt: Optional[str] = _OPT_FORMAT,
    since: Optional[str] = _OPT_SINCE,
    until: Optional[str] = _OPT_UNTIL,
    when: Optional[str] = _OPT_WHEN,
    after: Optional[str] = _OPT_AFTER,
    before: Optional[str] = _OPT_BEFORE,
) -> None:
    """Show session, file, and version counts per source.

    Examples:
        aise stats                              # all configured sources (no date restriction)
        aise stats --provider aistudio         # AI Studio only
        aise stats --provider claude           # Claude Code only
        aise stats --format json               # machine-readable output
        aise stats --since 7d                  # sessions from the last 7 days
        aise stats --when 202X                 # sessions in the 2020s decade
        aise stats --since 2026-01 --until 2026-03  # January through March 2026
    """
    since, until = _normalize_date_range(since, until, when, after, before)
    engine = _resolve_engine(ctx, provider)
    if not engine:
        err_console.print("[red]Internal error: engine not initialized[/red]")
        raise typer.Exit(code=1)
    _do_stats(engine, since=since, until=until, fmt=fmt)


# ── Config app ───────────────────────────────────────────────────────────────

def _get_config_file_path() -> Path:
    """Return the resolved config file path based on current priority chain.

    Delegates to _resolve_config_path() — single source of truth for path resolution.
    """
    return _resolve_config_path()


@config_app.command("path")
def config_path() -> None:
    """Print the config file path (whether or not the file exists).

    Examples:
        aise config path
        aise --config /tmp/my.json config path   # show path after override
    """
    console.print(str(_get_config_file_path()))


@config_app.command("show")
def config_show(
    fmt: Optional[str] = typer.Option(None, "--format", "-f", help="Output format: table, json, plain. Default: plain."),
) -> None:
    """Show the effective configuration (file contents + resolved path).

    Displays all keys loaded from the config file, or a message if no file exists.

    Examples:
        aise config show
        aise config show --format json
    """
    config_file = _get_config_file_path()
    cfg = load_config()

    if fmt == "json":
        sys.stdout.write(json.dumps({
            "config_file": str(config_file),
            "exists": config_file.exists(),
            "config": cfg,
        }, indent=2) + "\n")
        return

    console.print(f"Config file: [cyan]{config_file}[/cyan]")
    if not config_file.exists():
        console.print("[yellow]File does not exist. Run 'aise config init' to create it.[/yellow]")
        return

    if not cfg:
        console.print("[dim]File exists but is empty (no keys set).[/dim]")
        return

    if fmt in ("plain", "csv") or fmt is None:
        for k, v in cfg.items():
            console.print(f"{k}: {v}")
        return

    from rich.table import Table
    table = Table(title=f"Config ({config_file.name})")
    table.add_column("Key", style="cyan")
    table.add_column("Value")
    for k, v in cfg.items():
        table.add_row(k, json.dumps(v) if not isinstance(v, str) else v)
    console.print(table)


_CONFIG_INIT_TEMPLATE = {
    "_comment": (
        "ai_session_search configuration. "
        "Set org_dir and source_dirs before running 'aise analyze'. "
        "Config path: override with --config flag or AI_SESSION_SEARCH_CONFIG env var."
    ),
    # ── Required for aise analyze ──────────────────────────────────────────
    "org_dir": "",          # Output destination. Default if empty: <app config dir>/organized/
    "source_dirs": {
        "aistudio": [],     # List of Google AI Studio export directories
        "gemini_cli": "",   # Path to Gemini CLI tmp dir (auto-detected: ~/.gemini/tmp)
    },
    # ── Optional analysis settings ─────────────────────────────────────────
    "vocab_output_filename": "VOCABULARY_ANALYSIS.md",
    "instruction_history_session": "",  # Indexed session ID for instruction-history
    "marker_window": 25000,         # Chars of user text to scan for codebook markers
    "analysis_page_size": 50,       # Indexed sessions held per bounded analysis page
    "analysis_relationship_rules": [
        {"id": "branch_of", "kind": "branch", "pattern": r"(?i)^branch of (?P<parent>.+)$"},
        {"id": "copy_of", "kind": "copy", "pattern": r"(?i)^copy of (?P<parent>.+)$"},
    ],
    # ── Scoring weights (all numeric thresholds in one place) ──────────────
    "scoring_weights": {
        "technique": 20,
        "role": 15,
        "thinking_budget": 30,
        "anti_ai": 35,
        "version_multiplier": 10,
        "corrected_bonus": 25,
        "descendant_boost": 15,
        "min_ngram_freq": 3,
        "min_session_text_len": 50,
        "min_utility_for_index": 20,
    },
    # ── Stop words: common function words excluded from n-gram analysis ────
    "stop_words": [
        "the", "this", "that", "and", "is", "for", "with", "from", "you", "are",
        "into", "of", "to", "a", "in", "it", "as", "be", "an", "or", "on", "at",
        "by", "we", "i", "can", "but", "not", "so", "if", "do", "its", "all",
        "my", "me", "have", "has", "was", "will", "your", "our", "they", "them",
        "also", "then", "than", "when", "just", "up", "out", "about",
    ],
    # ── aise organize output formats ───────────────────────────────────────
    # Valid values: "symlinks", "json", "markdown" (list to combine)
    # "symlinks" — opt-in transactional symlink taxonomy dirs in org_dir
    # "json"     — SESSION_TAXONOMY.json: {name: {taxonomy, utility, era}}
    # "markdown" — TAXONOMY.md: sessions grouped by dimension and category
    "organize_formats": ["json", "markdown"],
    # ── Taxonomy dimension definitions ─────────────────────────────────────
    # Each dimension controls one axis of the taxonomy. Fields:
    #   name           — directory name (e.g. "03_by_technique")
    #   match          — "field" (read record field) or "keyword" (match via keyword_map)
    #   field          — record field name         (for match=field)
    #   scalar         — true if field is a single value, not a list (for match=field)
    #   keyword_map    — key into keyword_maps     (for match=keyword)
    #   match_field    — record field to match against  (for match=keyword)
    #   match_type     — "substring" or "set_intersection"  (for match=keyword)
    #   fallback       — category when no keywords match  (for match=keyword, optional)
    #   exclude        — category values to skip  (optional, default [])
    #   prefer_for_links — use this dim for INDEX.md link targets (default true)
    #   label          — human-readable label for INDEX.md (optional)
    # Remove this key or set to [] to use the built-in 7-dimension default.
    "taxonomy_dimensions": [
        {
            "name": "01_by_project",
            "match": "keyword",
            "keyword_map": "project_map",
            "source_field": "name",
            "match_type": "substring",
            "fallback": "misc_research",
            "prefer_for_links": True,
        },
        {
            "name": "02_by_workflow",
            "match": "keyword",
            "keyword_map": "workflow_map",
            "source_field": "techniques",
            "match_type": "set_intersection",
            "prefer_for_links": True,
        },
        {
            "name": "03_by_technique",
            "match": "field",
            "field": "techniques",
            "prefer_for_links": True,
        },
        {
            "name": "04_by_task",
            "match": "field",
            "field": "task_categories",
            "prefer_for_links": True,
        },
        {
            "name": "05_by_expert_role",
            "match": "field",
            "field": "roles",
            "prefer_for_links": True,
        },
        {
            "name": "06_by_writing_method",
            "match": "field",
            "field": "writing_methods",
            "prefer_for_links": True,
        },
        {
            "name": "07_by_era",
            "match": "field",
            "field": "era",
            "scalar": True,
            "exclude": ["unknown"],
            "prefer_for_links": False,
        },
        {
            # Sessions with cwd="" (AI Studio, Gemini CLI) are skipped (no fallback).
            # Populated for Claude Code sessions from the JSONL cwd field.
            "name": "08_by_working_dir",
            "match": "field",
            "field": "cwd",
            "scalar": True,
            "exclude": [""],
            "prefer_for_links": False,
            "label": "08 By Working Dir",
        },
    ],
    # ── Continuation marker detection for prompt role classification ───────
    "continuation_markers": {
        "min_initial_len": 50,
        "prefix_markers": [
            "ok", "okay", "yes", "yeah", "sure", "great", "good", "nice",
            "thanks", "thank you", "now", "also", "and", "but", "so",
            "wait", "hmm", "hm", "continue", "proceed", "go on", "go ahead",
            "can you", "could you", "please", "i want", "i need",
            "actually", "by the way", "one more", "also can", "also please",
            "commit", "push", "run", "execute", "fix the", "fix that", "fix it",
        ],
    },
    # ── Keyword maps for taxonomy classification (populate to improve results)
    "keyword_maps": {
        "task_categories": {},
        "writing_methods": {},
        "project_map": {},
        "workflow_map": {},
    },
    # ── Claude Code session analysis settings ──────────────────────────────
    "correction_patterns": [
        "regression:you deleted",
        "regression:you removed",
        "skip_step:you forgot",
        "skip_step:you missed",
        "misunderstanding:that's wrong",
        "incomplete:also need",
    ],
    "planning_commands": [
        "/ar:plannew", "/ar:pn", "/ar:planrefine", "/ar:pr",
        "/ar:planupdate", "/ar:pu", "/ar:planprocess", "/ar:pp",
    ],
}


@config_app.command("init")
def config_init(
    force: bool = typer.Option(
        False, "--force",
        help="Overwrite the config file if it already exists.",
    ),
) -> None:
    """Create a starter config.json with documented default values.

    Safe by default — will NOT overwrite an existing config file unless --force is given.
    Creates any missing parent directories automatically.

    Examples:
        aise config init             # create if not exists
        aise config init --force     # overwrite existing file
    """
    config_file = _get_config_file_path()

    if config_file.exists() and not force:
        err_console.print(
            f"[yellow]Config file already exists:[/yellow] {config_file}\n"
            "Use --force to overwrite."
        )
        raise typer.Exit(code=1)

    _write_config(_CONFIG_INIT_TEMPLATE)
    # _write_config always calls invalidate_config_cache() so no extra step needed

    console.print(f"[green]Created:[/green] {config_file}")
    console.print(
        "[dim]Edit org_dir and source_dirs.aistudio to enable 'aise analyze'.[/dim]\n"
        "[dim]All scoring thresholds, stop words, and keyword maps are in one place.[/dim]\n"
        "[dim]Run 'aise config show' to verify the active configuration.[/dim]"
    )


# ── Analysis pipeline configuration ──────────────────────────────────────────

# Pipeline stage mapping: stage name -> module path
_PIPELINE_STEPS = {
    "instruction-history": "ai_session_search.analysis.extract",
    "analyze":             "ai_session_search.analysis.analyzer",
    "graph":               "ai_session_search.analysis.graph",
    "organize":            "ai_session_search.analysis.orchestrator",
    "vocab":               "ai_session_search.analysis.vocab",
}

# Pipeline dependencies: stage -> (required_predecessor_stage, required_file)
_STEP_DEPS = {
    "graph":    ("analyze", "session_db.json"),
    "organize": ("graph",   "SESSION_GRAPH.json"),
}


def _pipeline_order(cfg: dict) -> list[str]:
    """Build pipeline step list; omit instruction-history if not configured.

    Args:
        cfg: Configuration dict

    Returns:
        List of stage names in execution order
    """
    base = ["analyze", "graph", "organize"]
    if cfg.get("instruction_history_session"):
        return ["analyze", "instruction-history", "graph", "organize"]
    return base


def _resolve_org_dir(cfg: dict, org_dir_override: Optional[str] = None) -> Path:
    """Thin CLI wrapper around config.resolve_org_dir — adds stderr hint when defaulting."""
    from ai_session_search.config import resolve_org_dir
    had_org = bool(cfg.get("org_dir", "").strip()) or bool(org_dir_override)
    org = resolve_org_dir(cfg, override=org_dir_override)
    if not had_org:
        err_console.print(f"[dim]org_dir not set — using default: {org}[/dim]")
    return org


def _check_step_dep(step: str, cfg: dict, org_dir: Path) -> None:
    """Raise helpful error if required predecessor output is missing.

    Args:
        step: Pipeline stage name
        cfg: Configuration dict
        org_dir: Organization directory

    Raises:
        typer.Exit: If required predecessor output is missing
    """
    if step not in _STEP_DEPS:
        return
    prev_step, required_file = _STEP_DEPS[step]
    if not (org_dir / required_file).exists():
        err_console.print(
            f"[red]Missing {required_file}.[/red] "
            f"Run [bold]aise analyze --step {prev_step}[/bold] first."
        )
        raise typer.Exit(code=1)


def _run_single_step(
    step: str,
    source_filter: Optional[str],
    marker_window: int,
    cfg: dict,
    org_dir: Path,
    organize_formats: Optional[list[str]] = None,
) -> None:
    """Run a single pipeline step.

    Args:
        step: Pipeline stage name
        source_filter: Canonical provider name, legacy gemini alias, or None for all
        marker_window: Chars for marker matching (0 = use config default)
        cfg: Configuration dict
        org_dir: Organization directory
        organize_formats: Output formats for the organize step (None = read from config)

    Raises:
        typer.Exit: If step fails
    """
    import importlib
    _check_step_dep(step, cfg, org_dir)
    mod_path = _PIPELINE_STEPS[step]
    mod = importlib.import_module(mod_path)

    try:
        if step == "analyze":
            analysis_config = {**cfg, "org_dir": str(org_dir)}
            mod.run_analysis(
                marker_window=marker_window,
                source_filter=source_filter,
                config=analysis_config,
            )
        elif step == "instruction-history":
            analysis_config = {**cfg, "org_dir": str(org_dir)}
            mod.main(
                session_id=cfg.get("instruction_history_session"),
                config=analysis_config,
                refresh_index=False,
            )
        elif step == "organize":
            mod.run_orchestration(formats=organize_formats)
        else:
            mod.main()
    except SystemExit:
        raise
    except Exception as exc:
        err_console.print(f"[red]Step '{step}' failed: {exc}[/red]")
        raise


# ── Analysis commands (aise analyze / graph / organize / vocab / instruction-history) ──


@app.command("analyze")
def cmd_analyze(
    ctx: typer.Context,
    step: Optional[str] = typer.Option(
        None, "--step", hidden=True,
        help="Advanced: run only one pipeline step. Use 'aise analyze' for full pipeline."
    ),
    marker_window: int = typer.Option(
        0, "--window", "-w",
        help="Chars for marker matching (0=from config)."
    ),
    force: bool = typer.Option(
        False, "--force",
        help="Force re-run all stages even if inputs unchanged."
    ),
    status_only: bool = typer.Option(
        False, "--status",
        help="Show which stages are stale/current without running."
    ),
    org_dir: Optional[str] = typer.Option(
        None, "--org-dir",
        help="Override config.org_dir for this run."
    ),
    fmt: Optional[str] = typer.Option(
        None, "--format", "-f",
        help=(
            "Output format(s) for the organize step, comma-separated: "
            "symlinks, json, markdown. Default: from config or 'symlinks'."
        ),
    ),
) -> None:
    """Run the full analysis pipeline: pattern matching -> graph -> taxonomy symlinks.

    Runs the full pipeline automatically. Stages are skipped when inputs
    have not changed since the last run (idempotent).

    Full pipeline (in order):
      1. analyze   -> session_db.json + VOCABULARY_ANALYSIS.md
      2. graph     -> SESSION_GRAPH.json
      3. organize  -> symlinks + INDEX.md + SESSIONS_FULL.md
      (instruction-history -> USER_INSTRUCTIONS_CLEAN.md if its session ID is configured)

    Re-run one stage with --step (advanced):
        aise analyze --step analyze
        aise analyze --step graph

    To narrow to one indexed provider, use --provider:
        aise analyze --provider aistudio  (analyze only AI Studio sessions)
        aise analyze --provider gemini-cli (analyze only Gemini CLI sessions)
        aise analyze --provider claude    (analyze only Claude Code sessions)
        aise analyze --provider codex     (analyze only Codex sessions)
        aise analyze                      (analyze all configured sources)
    """
    from ai_session_search.analysis import pipeline_state as ps

    cfg = load_config()
    org = _resolve_org_dir(cfg, org_dir)

    # Get source filter from ctx.obj (set by app_callback composition root)
    ctx_obj = ctx.obj if ctx.obj else {}
    source_filter = ctx_obj.get("source")
    organize_formats = [f.strip() for f in fmt.split(",")] if fmt else None
    pipeline_order = _pipeline_order(cfg)
    state = ps.load_state(org) if (org and not force) else {}

    # Single step mode (advanced)
    if step:
        if step not in _PIPELINE_STEPS:
            err_console.print(
                f"[red]Invalid --step '{step}'.[/red] "
                f"Valid: {', '.join(sorted(_PIPELINE_STEPS.keys()))}"
            )
            raise typer.Exit(code=1)
        _run_single_step(step, source_filter, marker_window, cfg, org,
                         organize_formats=organize_formats)
        # Note: Not updating state for single-step runs; use full pipeline for tracking
        return

    # Status dry-run mode
    if status_only:
        console.print("[bold]Pipeline status:[/bold]")
        for name in pipeline_order:
            # Simple check: does output file exist?
            if name == "instruction-history":
                output = org / "USER_INSTRUCTIONS_CLEAN.md"
            elif name == "analyze":
                output = org / "session_db.json"
            elif name == "graph":
                output = org / "SESSION_GRAPH.json"
            elif name == "organize":
                output = org / "INDEX.md"
            else:
                output = None
            if output and output.exists():
                console.print(f"  [green]current[/green]  {name}")
            else:
                console.print(f"  [red]STALE[/red]   {name}")
        return

    # Full pipeline with change detection
    console.print("[bold]Running full analysis pipeline...[/bold]")
    ran_any = False
    for i, name in enumerate(pipeline_order, 1):
        # Simple heuristic: check if output exists and --force not specified
        if name == "instruction-history":
            output = org / "USER_INSTRUCTIONS_CLEAN.md"
        elif name == "analyze":
            output = org / "session_db.json"
        elif name == "graph":
            output = org / "SESSION_GRAPH.json"
        elif name == "organize":
            output = org / "INDEX.md"
        else:
            output = None

        if not force and output and output.exists():
            console.print(f"[dim][{name}] skipped (output up to date)[/dim]")
            continue

        console.print(f"[cyan][{name}] running...[/cyan]")
        try:
            _run_single_step(name, source_filter, marker_window, cfg, org,
                             organize_formats=organize_formats)
            ran_any = True
        except SystemExit:
            raise

    if not ran_any:
        console.print("[green]All pipeline stages are current. Nothing to do.[/green]")
    else:
        console.print("[bold green]Pipeline complete.[/bold green]")

    # Print output summary
    console.print(f"\n[bold]Output directory:[/bold] {org}")
    key_files = [
        ("INDEX.md", "Ranked session table"),
        ("SESSIONS_FULL.md", "Detailed per-session breakdowns"),
        ("session_db.json", "Session database"),
        ("SESSION_GRAPH.json", "Provenance graph"),
        ("VOCABULARY_ANALYSIS.md", "Recurring prompt patterns"),
    ]
    for fname, desc in key_files:
        fpath = org / fname
        if fpath.exists():
            console.print(f"  [green]{fname}[/green] — {desc}")


@app.command("graph", hidden=True, rich_help_panel="Analysis Steps (advanced — use 'aise analyze')")
def cmd_graph() -> None:
    """Publish the canonical indexed Rust provenance graph.

    Resolved configured relationship hints become edges. Working directories and repositories
    remain set-valued groups; ambiguous hints never become guessed lineage.

    Tip: Use 'aise analyze' to run the full pipeline automatically.
    """
    cfg = load_config()
    org = _resolve_org_dir(cfg)
    _check_step_dep("graph", cfg, org)
    from ai_session_search.analysis.graph import main as graph_main
    graph_main()


@app.command("organize", hidden=True, rich_help_panel="Analysis Steps (advanced — use 'aise analyze')")
def cmd_organize(
    fmt: Optional[str] = typer.Option(
        None, "--format", "-f",
        help=(
            "Output format(s), comma-separated: symlinks, json, markdown. "
            "Default: from config.json['organize_formats'] or 'json,markdown'."
        ),
    ),
    validate: bool = typer.Option(
        False, "--validate", "-V",
        help=(
            "Check taxonomy config health without running orchestration. "
            "Reports each dimension: OK, WARN (empty keyword_map), or ERROR (missing key). "
            "Exits 1 if any errors found."
        ),
    ),
) -> None:
    """Create taxonomy output in one or more formats + INDEX.md + SESSIONS_FULL.md.

    Formats: json and markdown by default; transactional symlinks are opt-in.
    Use --format symlinks,json,markdown to produce all three simultaneously.
    Symlink batches are prevalidated, manifested, and rolled back on failure.

    Requires SESSION_GRAPH.json from 'aise analyze --step graph'.
    Tip: Use 'aise analyze' to run the full pipeline automatically.

    Validate config without running: aise organize --validate
    """
    from ai_session_search.analysis.orchestrator import (
        load_taxonomy_dimensions, validate_taxonomy_dimensions, _DEFAULT_TAXONOMY_DIMENSIONS
    )
    from ai_session_search.analysis.codebook import load_keyword_maps

    if validate:
        cfg = load_config()
        keyword_maps = load_keyword_maps()
        raw_dims = get_config_section("taxonomy_dimensions")
        if raw_dims and isinstance(raw_dims, list):
            dims = raw_dims
            source = "config.json[taxonomy_dimensions]"
        else:
            dims = _DEFAULT_TAXONOMY_DIMENSIONS
            source = "built-in defaults (no taxonomy_dimensions in config)"

        console.print(f"[bold]Validating taxonomy dimensions[/bold] ({source})\n")
        errors = validate_taxonomy_dimensions(dims, keyword_maps)

        has_errors = any("ERROR" in e or "missing required key" in e or "must be one of" in e
                         for e in errors)
        has_warns = any("empty or missing" in e for e in errors)

        for dim in dims:
            name = dim.get("name", "<unnamed>")
            match = dim.get("match", "?")
            if match == "field":
                detail = f"field={dim.get('field', '?')}, scalar={dim.get('scalar', False)}"
            else:
                sf = dim.get("source_field") or dim.get("match_field", "?")
                kmap = dim.get("keyword_map", "?")
                kmap_size = len(keyword_maps.get(kmap, {}))
                detail = f"source_field={sf}, keyword_map={kmap} ({kmap_size} categories)"
            console.print(f"  [cyan]{name}[/cyan]  match={match}  {detail}")

        if errors:
            console.print()
            for e in errors:
                if "empty or missing" in e:
                    console.print(f"  [yellow]WARN[/yellow] {e}")
                else:
                    console.print(f"  [red]ERROR[/red] {e}")
        else:
            console.print("\n  [green]All dimensions OK[/green]")

        if has_errors:
            raise typer.Exit(code=1)
        return

    cfg = load_config()
    org = _resolve_org_dir(cfg)
    _check_step_dep("organize", cfg, org)
    formats = [f.strip() for f in fmt.split(",")] if fmt else None
    from ai_session_search.analysis import orchestrator as _orch
    _orch.run_orchestration(formats=formats)


@app.command("vocab", hidden=True, rich_help_panel="Analysis Steps (advanced — use 'aise analyze')")
def cmd_vocab() -> None:
    """Standalone vocabulary analysis -> VOCABULARY_ANALYSIS.md.

    Normally vocabulary is mined inline by `aise analyze`. Use this to re-run independently.
    """
    from ai_session_search.analysis.vocab import main as vocab_main
    vocab_main()


@app.command("instruction-history", hidden=True, rich_help_panel="Analysis Steps (advanced — use 'aise analyze')")
def cmd_instruction_history(
    session_id: Optional[str] = typer.Option(
        None,
        "--session",
        help="Indexed session ID or unambiguous ID prefix.",
    ),
) -> None:
    """Export one indexed session's user messages -> USER_INSTRUCTIONS_CLEAN.md.

    Session: from --session or config key 'instruction_history_session'.
    Tip: Use 'aise analyze' to run the full pipeline automatically.
    """
    from ai_session_search.analysis.extract import main as extract_main
    extract_main(session_id=session_id)


# ── Entry point ───────────────────────────────────────────────────────────────

def cli_main():
    """CLI entry point."""
    app()
