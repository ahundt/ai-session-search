#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

"""Prove that upgrading from a published release needs nothing beyond replacing the executable.

For each earlier release, in an empty home directory, first build a populated synthetic index
with that release, open it with the candidate, run incremental and full refresh, and compare
ordered messages, provenance, content parts, file edits, and transcript text with a fresh
candidate. Every provider the release supports must contribute messages. Then:

1. install that release's integrations from PyPI for every harness it supports that installs files
   an upgrade could leave behind: skills and their discovery links, and both instruction variants;
2. save that release's complete `aise config show` output as `config.toml`, as a user who kept
   the printed defaults would have;
3. run one ordinary command with the candidate executable, which is all an upgrade through uv,
   pip, Cargo, or a native archive does;
4. require `aise integrations status --format json` to report every integration the earlier
   release installed as `current`;
5. require the earlier release to still read that config and integration state, as it must
   after a downgrade.

From 1.0.0 it also proves the documented index downgrade: 1.0.0 refuses the candidate's index,
rebuilds from the transcripts once that index is moved aside, and the moved index keeps every
archived row for the next upgrade.

A failure names the release and what would have needed a manual step. CI runs this on every
push against the Linux build, so a change that breaks upgrades fails before it merges, and the
`upgrade` job of publish.yml runs it against the native executable before `verify` attests or
anything is published. It downloads the earlier release with `uvx`, so it needs network access.
Without `--from` it checks every release PyPI lists from `UPGRADE_FLOOR` on: between releases that
ends with the last release, and at release time the new version is not on PyPI until this check
has passed. CI, publish.yml, and RELEASING.md all run it the same way:

    uv run --no-project python -m scripts.verify_upgrade_path --executable target/release/aise

It refuses to run on Windows: aise finds the home folder there through the Windows known-folder
API, which HOME and USERPROFILE cannot redirect, so the earlier release would install into the real
profile.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.request
from collections.abc import Sequence

from scripts.release_versions import PYTHON_RELEASE_VERSION, release_sort_key
from scripts.verify_harness_discovery import HARNESS_ENVIRONMENT
from scripts.verify_harness_discovery import environment as isolated_environment

PYPI_PROJECT_URL = "https://pypi.org/pypi/ai-session-search/json"
# The oldest release an upgrade must work from without a manual step: the first one published.
UPGRADE_FLOOR = "1.0.0rc1"

# Every harness whose install writes files an upgrade could leave outdated: skills and discovery
# links (claude, gemini, antigravity, pi, prime-agent), the MCP-first instruction block (claude,
# codex, gemini, opencode), and the CLI-only block (pi, prime-agent). Clients that only register an
# MCP server name the executable's path, which an upgrade keeps. An earlier release is asked only
# for the ones it supports: 1.0.0rc1 predates pi and prime-agent.
CLIENTS = ("claude", "codex", "gemini", "antigravity", "opencode", "pi", "prime-agent")
_POSSIBLE_CLIENTS = re.compile(r"\[possible values: ([^\]]+)\]")


def _environment(home: pathlib.Path, executable_dir: pathlib.Path | None) -> dict[str, str]:
    config = home / "aise/config.toml"
    config.parent.mkdir(parents=True, exist_ok=True)
    if not config.exists():
        # Historical schemas reject newer provider names; Unix defaults stay under the fake home.
        config.write_text("[index]\n", encoding="utf-8")
    environment = isolated_environment(home, config=config)
    for key in HARNESS_ENVIRONMENT:
        environment.pop(key, None)
    if executable_dir is not None:
        environment["PATH"] = f"{executable_dir}{os.pathsep}{environment.get('PATH', '')}"
    return environment


def _run(argv: Sequence[str], environment: dict[str, str]) -> subprocess.CompletedProcess[str]:
    with subprocess.Popen(argv, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, start_new_session=os.name != "nt") as process:
        try:
            stdout, stderr = process.communicate()
            return subprocess.CompletedProcess(argv, process.wait(), stdout, stderr)
        finally:
            if os.name != "nt":
                _stop_process_group(process)


def _stop_process_group(process: subprocess.Popen[str]) -> None:
    group = process.pid
    for termination in (signal.SIGTERM, signal.SIGKILL):
        # macOS answers EPERM, not ESRCH, for a group whose only members are zombies waiting
        # for launchd to reap them. This verifier created the session, so EPERM cannot mean a
        # live member it lacks permission for: the group has exited.
        try:
            os.killpg(group, termination)
        except (ProcessLookupError, PermissionError):
            return
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            process.poll()
            try:
                os.killpg(group, 0)
            except (ProcessLookupError, PermissionError):
                return
            time.sleep(0.01)
    raise BackgroundChildrenAlive(f"background process group {group} did not exit after SIGKILL")


def _published() -> dict[str, object]:
    with urllib.request.urlopen(PYPI_PROJECT_URL, timeout=30) as response:
        return dict(json.load(response))


def published_versions_since(floor: str) -> list[str]:
    """Return every published release version at or above ``floor``, oldest first."""
    releases = _published()["releases"]
    assert isinstance(releases, dict)
    versions = [version for version in releases if PYTHON_RELEASE_VERSION.fullmatch(version)]
    return sorted(
        (version for version in versions if release_sort_key(version) >= release_sort_key(floor)),
        key=release_sort_key,
    )


class UpgradeCheckUnavailable(Exception):
    """The earlier release could not be installed or run, so nothing about the upgrade is known."""


class BackgroundChildrenAlive(UpgradeCheckUnavailable):
    """Temporary roots must survive until the owned process group exits."""


def problems_in_status(status: str) -> list[str]:
    """Return each integration in ``aise integrations status --format json`` that is not current.

    Executable aliases are skipped: they point at the executable's own directory, which this check
    changes on purpose by running the candidate from the build tree, and a real upgrade keeps.
    """
    try:
        entries = json.loads(status)["integrations"]
    except (ValueError, KeyError, TypeError) as error:
        return [f"integrations status printed no readable JSON report: {error}"]
    # Fail closed on an empty report: a check that found nothing would otherwise pass, whether the
    # earlier release installed nothing or the report changed shape.
    if not any(
        entry.get("component") == "skill"
        and re.split(r"[/\\]", entry.get("path", ""))[-1] == "ai-session-search"
        for entry in entries
    ):
        return ["integrations status reported no installed ai-session-search skill to check"]
    return [
        f"{entry.get('client')} {entry.get('path')}: {entry.get('state')}"
        for entry in entries
        if entry.get("component") != "executable_alias" and entry.get("current") is not True
    ]


def supported_clients(install_help: str) -> list[str]:
    """Return the harnesses in ``CLIENTS`` that an ``integrations install --help`` accepts."""
    listed = _POSSIBLE_CLIENTS.search(install_help)
    accepted = {value.strip() for value in listed.group(1).split(",")} if listed else set()
    return [client for client in CLIENTS if client in accepted]


def _client_arguments(clients: Sequence[str]) -> list[str]:
    return [argument for client in clients for argument in ("--client", client)]


def archive_evidence(database: sqlite3.Connection) -> dict[str, bool]:
    """Read the four synthetic archive markers after destructive source changes."""
    messages = {row[0] for row in database.execute("select content from messages")}
    return {
        "shrink": "alpha" in messages,
        "rewrite": "beta" in messages,
        "file_edits": database.execute(
            "select exists(select 1 from file_edits where new_content = 'saved file')"
        ).fetchone()[0] == 1,
        "injected_rows": "<turn_aborted>archive" in messages,
    }


def _archive_writer_evidence(command: Sequence[str], home: pathlib.Path) -> dict[str, bool]:
    return _archive_home(command, home)[0]


def _archive_home(command: Sequence[str], home: pathlib.Path) -> tuple[dict[str, bool], dict[str, str]]:
    """Index a transcript, shrink and rewrite it, and return the archive markers and environment."""
    home.mkdir()
    environment = isolated_environment(home)
    config = pathlib.Path(environment["AI_SESSION_SEARCH_CONFIG"])
    config.parent.mkdir(parents=True, exist_ok=True)
    sources = home / "sources"
    sources.mkdir()
    config.write_text(
        f"[providers.claude]\nenabled = true\npaths = [{json.dumps(str(sources))}]\n" +
        "\n".join(f"[providers.{provider}]\nenabled = false" for provider in
                  ("claude-desktop", "codex", "cursor", "antigravity", "pi",
                   "prime-agent", "aistudio", "gemini-cli")), encoding="utf-8",
    )
    source = sources / "archive.jsonl"

    def write_source(texts: Sequence[str], *, edit: bool = False) -> None:
        records = [{"type": "user", "sessionId": "archive", "message": {
            "role": "user", "content": [{"type": "text", "text": text}],
        }} for text in texts]
        if edit:
            records.append({"type": "assistant", "sessionId": "archive", "message": {
                "role": "assistant", "content": [{"type": "tool_use", "id": "write",
                    "name": "Write", "input": {"file_path": str(home / "saved.txt"),
                                                "content": "saved file"}}],
            }})
            records.append({"type": "user", "sessionId": "archive", "message": {
                "role": "user", "content": [{"type": "tool_result", "tool_use_id": "write",
                    "is_error": False, "content": "written"}],
            }})
        source.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")

    def reindex() -> None:
        result = _run([*command, "reindex", "--full"], environment)
        if result.returncode:
            raise UpgradeCheckUnavailable(f"archive reindex failed: {result.stderr.strip()}")

    write_source(["alpha", "beta"], edit=True)
    reindex()
    with sqlite3.connect(environment["AI_SESSION_SEARCH_DATABASE"]) as database:
        database.execute("insert into messages(session_id, provider, seq, role, content) "
                         "select id, provider, 99, 'user', '<turn_aborted>archive' from sessions limit 1")
        seeded = archive_evidence(database)
        if not all(seeded.values()):
            raise UpgradeCheckUnavailable(f"archive writer did not seed every archive marker: {seeded}")
    write_source(["beta"])
    reindex()
    write_source(["gamma"])
    reindex()
    with sqlite3.connect(environment["AI_SESSION_SEARCH_DATABASE"]) as database:
        return archive_evidence(database), environment


DOWNGRADE_GUIDE = pathlib.Path(__file__).resolve().parents[1] / "docs/development/installation.md"
DOCUMENTED_REBUILD = 'aise --database "$DB" reindex --full'


def _index_files(database: pathlib.Path) -> list[pathlib.Path]:
    return [database.with_name(database.name + suffix) for suffix in ("", "-wal", "-shm")]


def check_downgrade(old: Sequence[str], executable: pathlib.Path) -> list[str]:
    """Prove the documented downgrade: move the index aside and let the earlier release rebuild.

    The candidate's index refuses the earlier release, the earlier release rebuilds from the
    transcripts still on disk, and the index moved aside keeps every archived row for an upgrade
    back.
    """
    if os.name == "nt":
        raise UpgradeCheckUnavailable("historical archive writers require a disposable Unix home")
    with tempfile.TemporaryDirectory(prefix="aise-downgrade-") as directory:
        evidence, environment = _archive_home([str(executable)], pathlib.Path(directory).resolve() / "home")
        if not all(evidence.values()):
            return [f"candidate did not retain every archive marker before the downgrade: {evidence}"]
        database = pathlib.Path(environment["AI_SESSION_SEARCH_DATABASE"])
        refused = _run([*old, "reindex"], environment)
        if refused.returncode == 0 or "newer than this aise build" not in refused.stderr:
            return [f"the earlier release did not refuse the candidate's index: exit "
                    f"{refused.returncode}, {refused.stderr.strip()}"]
        moved = _move_index_as_documented(executable, environment, database)
        if isinstance(moved, str):
            return [moved]
        problems = _rebuild_with_earlier_release(old, environment, database, moved)
        for path in _index_files(database):
            path.unlink(missing_ok=True)
        _rename_existing({kept: path for path, kept in moved.items()})
        restored = _run([str(executable), "reindex"], environment)
        if restored.returncode != 0:
            problems.append(f"the candidate could not reopen the restored index: {restored.stderr.strip()}")
        with sqlite3.connect(database) as connection:
            after = archive_evidence(connection)
        problems.extend(f"restored index lost {name}" for name, kept in after.items() if not kept)
        return problems


def _rename_existing(moves: dict[pathlib.Path, pathlib.Path]) -> None:
    for source, target in moves.items():
        if source.exists():
            source.rename(target)


def _documented_downgrade_lines() -> list[str]:
    """The shell lines of the downgrade block in installation.md, comments and blanks removed."""
    text = DOWNGRADE_GUIDE.read_text(encoding="utf-8")
    block = re.search(r"To downgrade, stop every aise process.*?```bash\n(.*?)```", text, re.S)
    if block is None:
        raise UpgradeCheckUnavailable(f"{DOWNGRADE_GUIDE} has no downgrade command block")
    lines = [line.split("  #")[0].strip() for line in block.group(1).splitlines()]
    return [line for line in lines if line and not line.startswith("#")]


def _move_index_as_documented(
    executable: pathlib.Path, environment: dict[str, str], database: pathlib.Path
) -> dict[pathlib.Path, pathlib.Path] | str:
    """Run the guide's own path lookup and move loop with the candidate first on PATH.

    Returns each moved index file mapped to where the guide put it, or the problem that stopped
    it, so a wrong command in the guide fails here rather than on a user's index.
    """
    lines = _documented_downgrade_lines()
    if DOCUMENTED_REBUILD not in lines:
        return f"the downgrade guide no longer rebuilds with `{DOCUMENTED_REBUILD}`"
    steps = [line for line in lines if not line.startswith(("uv tool install", DOCUMENTED_REBUILD))]
    shell = {**environment, "PATH": f"{executable.parent}{os.pathsep}{environment['PATH']}"}
    result = _run(["bash", "-euc", "\n".join([*steps, 'printf "%s" "$DB"'])], shell)
    if result.returncode != 0 or result.stdout != str(database):
        return (f"the guide's commands did not move {database}: exit {result.returncode}, "
                f"printed {result.stdout!r}, {result.stderr.strip()}")
    moved = {path: path.with_name(path.name + ".from-1.1.0") for path in _index_files(database)}
    left = [str(path) for path in _index_files(database) if path.exists()]
    if left or not moved[database].exists():
        return f"the guide's commands left {left or [str(database)]} in place"
    return {path: kept for path, kept in moved.items() if kept.exists()}


def _rebuild_with_earlier_release(
    old: Sequence[str],
    environment: dict[str, str],
    database: pathlib.Path,
    moved: dict[pathlib.Path, pathlib.Path],
) -> list[str]:
    """Rebuild at the same path with the earlier release; the moved files must stay byte-identical."""
    moved_bytes = {kept: kept.read_bytes() for kept in moved.values()}
    rebuilt = _run([*old, "--database", str(database), "reindex", "--full"], environment)
    if rebuilt.returncode != 0:
        return [f"the earlier release could not rebuild after the index was moved aside: "
                f"{rebuilt.stderr.strip()}"]
    with sqlite3.connect(database) as connection:
        current = {row[0] for row in connection.execute("select content from messages")}
    problems = [] if "gamma" in current else ["the rebuilt index lacks the transcript's current message"]
    problems += [f"{kept.name} changed while the earlier release ran"
                 for kept, before in moved_bytes.items() if kept.read_bytes() != before]
    return problems


def check_archive_write_paths(old: Sequence[str], executable: pathlib.Path) -> list[str]:
    """Compare historical and candidate writers in independent disposable indexes."""
    if os.name == "nt":
        raise UpgradeCheckUnavailable("historical archive writers require a disposable Unix home")
    problems = []
    with tempfile.TemporaryDirectory(prefix="aise-archive-") as directory:
        for label, command, expected in (("1.0.0", old, False), ("candidate", [str(executable)], True)):
            evidence = _archive_writer_evidence(command, pathlib.Path(directory).resolve() / label)
            for name, retained in evidence.items():
                if retained != expected:
                    problems.append(f"{label} {name}: retained={retained}, expected {expected}")
    return problems


def _earlier_release_still_reads(
    previous: str, old: Sequence[str], environment: dict[str, str], clients: Sequence[str], home: pathlib.Path
) -> list[str]:
    """Downgrading replaces the executable again; the earlier release must still read the config
    and integration state the candidate left behind."""
    problems = []
    for command in (["config", "show"], ["integrations", "status", *_client_arguments(clients)]):
        downgraded = _run([*old, *command], environment)
        if downgraded.returncode != 0:
            problems.append(f"{previous} `aise {' '.join(command[:2])}` failed after the candidate "
                            f"ran: {downgraded.stderr.strip().replace(str(home), '~')}")
    return problems


# These provider fixtures exercise both changed parser contracts and the released schema.
# Keep them shared with the Rust upgrade regression instead of maintaining two transcript shapes.
UPGRADE_FIXTURES = pathlib.Path(__file__).resolve().parents[1] / "rust/ai-session-search-core/tests/fixtures"


def populated_index_snapshot(path: pathlib.Path) -> dict[str, list[tuple[object, ...]]]:
    """Compare content and provenance, excluding physical row IDs and refresh timestamps."""
    queries = {
        "messages": "select session_id,provider,seq,role,ts,kind,content,tool_name,tool_call_id,is_compaction,authorship,correlation_authority,correlation_scope,correlation_id,input_delivery_state,input_origin,record_relation from messages order by session_id,seq,id",
        "file_edits": "select session_id,provider,seq,file_path,file_name,ts,tool,new_content,edits_json from file_edits order by session_id,seq,id",
        "transcripts": "select session_id,transcript_text from transcripts order by session_id",
        "content_parts": "select m.session_id,m.seq,p.ordinal,p.start_char,p.end_char,p.authorship,p.origin from message_content_parts p join messages m on m.id=p.message_id order by m.session_id,m.seq,p.ordinal",
    }
    with sqlite3.connect(path.as_uri() + "?immutable=1", uri=True) as database:
        return {name: database.execute(sql).fetchall() for name, sql in queries.items()}


def write_upgrade_sources(sources: pathlib.Path) -> None:
    """Synthetic formats for every provider supported by the published stable release."""
    def write(relative: str, value: object, *, jsonl: bool = False) -> None:
        path = sources / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        text = "".join(json.dumps(row) + "\n" for row in value) if jsonl else json.dumps(value)
        path.write_text(text, encoding="utf-8")

    for provider in ("claude", "codex"):
        destination = sources / provider
        destination.mkdir(parents=True)
        shutil.copyfile(UPGRADE_FIXTURES / f"v1_0_0_{provider}.jsonl", destination / "session.jsonl")
    write("claude-desktop/local-agent-mode-sessions/install/account/local_upgrade-desktop/audit.jsonl", [
        {"type": "user", "uuid": "desktop-input", "session_id": "upgrade-desktop",
         "_audit_timestamp": "2026-06-01T10:00:00Z", "message": {"role": "user", "content": "synthetic desktop question"}},
    ], jsonl=True)
    write("cursor/project/agent-transcripts/upgrade-cursor/upgrade-cursor.jsonl", [
        {"role": "user", "message": {"content": [{"type": "text", "text": "synthetic cursor question"}]}},
    ], jsonl=True)
    write("antigravity/upgrade-antigravity/.system_generated/logs/transcript.jsonl", [
        {"step_index": 1, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE",
         "created_at": "2026-06-01T10:00:00Z", "content": "synthetic antigravity question"},
    ], jsonl=True)
    for provider in ("pi", "prime-agent"):
        name = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl" if provider == "prime-agent" else "2026-06-01T10-00-00-000Z_upgrade-pi.jsonl"
        write(f"{provider}/project/{name}", [
            {"type": "session", "version": 3, "id": f"upgrade-{provider}",
             "timestamp": "2026-06-01T10:00:00Z", "cwd": "/fixture/project"},
            {"type": "message", "id": "input", "timestamp": "2026-06-01T10:00:00Z",
             "message": {"role": "user", "content": [{"type": "text", "text": f"synthetic {provider} question"}]}},
        ], jsonl=True)
    write("aistudio/session.json", {"chunkedPrompt": {"chunks": [{"role": "user", "text": "synthetic studio question"}]}})
    write("gemini-cli/project/chats/session-upgrade.json", {"sessionId": "upgrade-gemini",
        "startTime": "2026-06-01T10:00:00Z", "lastUpdated": "2026-06-01T10:01:00Z",
        "messages": [{"id": "input", "type": "user", "timestamp": "2026-06-01T10:00:00Z", "content": "synthetic gemini question"}]})


def check_populated_upgrade(old: Sequence[str], executable: pathlib.Path) -> list[str]:
    """Refresh a published writer's populated index and compare with a fresh candidate.

    Callable offline with a locally supplied baseline command; the release orchestrator still
    uses the published uvx command for each release. Sources are shared, so source timestamps
    and paths cannot create false differences between the fresh and upgraded indexes.
    """
    with tempfile.TemporaryDirectory(prefix="aise-populated-upgrade-") as directory:
        root = pathlib.Path(directory).resolve()
        sources = root / "sources"
        write_upgrade_sources(sources)

        probe = isolated_environment(root / "probe")
        probe_config = pathlib.Path(probe["AI_SESSION_SEARCH_CONFIG"])
        probe_config.parent.mkdir(parents=True, exist_ok=True)
        probe_config.write_text("[index]\n", encoding="utf-8")
        paths = _run([*old, "config", "paths", "--format", "json"], probe)
        shown = _run([*old, "config", "show", "--format", "json"], probe)
        if paths.returncode or shown.returncode:
            raise UpgradeCheckUnavailable(f"could not learn published providers: {shown.stderr.strip()}")
        supported = {name.replace("_", "-") for name in json.loads(shown.stdout)["providers"]}

        def environment(label: str) -> dict[str, str]:
            home = root / label
            home.mkdir()
            env = isolated_environment(home)
            config = pathlib.Path(env["AI_SESSION_SEARCH_CONFIG"])
            config.parent.mkdir(parents=True, exist_ok=True)
            text = '[index]\nrefresh = "auto"\n'
            names = (supported if label == "seed" else {
                "claude", "codex", "claude-desktop", "cursor", "antigravity",
                "pi", "prime-agent", "aistudio", "gemini-cli",
            })
            for provider in sorted(names):
                enabled = True
                paths = [str(sources / provider)]
                text += f"[providers.{provider}]\nenabled = {str(enabled).lower()}\npaths = {json.dumps(paths)}\n"
            config.write_text(text, encoding="utf-8")
            return env

        def run(command: Sequence[str], args: Sequence[str], env: dict[str, str]) -> None:
            paths = _run([*command, "config", "paths", "--format", "json"], env)
            if paths.returncode:
                raise UpgradeCheckUnavailable(f"config paths failed: {paths.stderr.strip()}")
            result = _run([*command, *args], env)
            if result.returncode:
                raise UpgradeCheckUnavailable(f"populated {' '.join(args)} failed: {result.stderr.strip()}")

        seed = environment("seed")
        run(old, ["reindex", "--full"], seed)
        seed_path = pathlib.Path(seed["AI_SESSION_SEARCH_DATABASE"])
        with sqlite3.connect(seed_path.as_uri() + "?immutable=1", uri=True) as database:
            providers = {row[0] for row in database.execute("select distinct provider from messages")}
            edits = database.execute("select count(*) from file_edits").fetchone()[0]
        if providers != supported or not edits:
            return [f"published index provider coverage differs: expected {sorted(supported)}, "
                    f"populated {sorted(providers)}, file edits={edits}"]
        try:
            fresh = environment("fresh")
            run([str(executable)], ["reindex", "--full"], fresh)
            expected = populated_index_snapshot(pathlib.Path(fresh["AI_SESSION_SEARCH_DATABASE"]))
            problems = []
            for full in (False, True):
                env = environment("full" if full else "incremental")
                database = pathlib.Path(env["AI_SESSION_SEARCH_DATABASE"])
                database.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(seed_path, database)
                # An ordinary writable open must finish the additive migration before refresh.
                run([str(executable)], ["list", "--format", "json"], env)
                run([str(executable)], ["reindex", *(["--full"] if full else [])], env)
                observed = populated_index_snapshot(database)
                problems += [f"populated full={full}: {name} differs from fresh candidate"
                             for name in expected if observed[name] != expected[name]]
            return problems
        except UpgradeCheckUnavailable as error:
            return [str(error)]


def check_upgrade(previous: str, executable: pathlib.Path) -> list[str]:
    """Return the problems an upgrade from ``previous`` to ``executable`` leaves behind."""
    released = ["uvx", "--quiet", "--from", f"ai-session-search=={previous}", "aise"]
    problems = check_populated_upgrade(released, executable)
    if previous == "1.0.0":
        problems += check_archive_write_paths(released, executable)
        problems += check_downgrade(released, executable)
    if problems:
        return problems
    home = pathlib.Path(tempfile.mkdtemp(prefix="aise-upgrade-")).resolve()
    cleanup = True
    try:
        old = ["uvx", "--quiet", "--from", f"ai-session-search=={previous}", "aise"]
        old_environment = _environment(home, None)
        install_help = _run([*old, "integrations", "install", "--help"], old_environment)
        clients = supported_clients(install_help.stdout)
        if not clients:
            raise UpgradeCheckUnavailable(
                f"{previous} could not list the harnesses it supports: {install_help.stderr.strip()}"
            )
        preparation = ["--no-index-preparation"] if "--no-index-preparation" in install_help.stdout else []
        installed = _run([*old, "integrations", "install", *preparation, *_client_arguments(clients)], old_environment)
        # Failing to download or run the earlier release says nothing about the upgrade, and
        # reporting it as "needs a manual step" would send the reader after a bug that is not there.
        if installed.returncode != 0:
            raise UpgradeCheckUnavailable(
                f"{previous} could not install its integrations: {installed.stderr.strip()}"
            )
        shown = _run([*old, "config", "show"], old_environment)
        config_path = _run([*old, "config", "file"], old_environment).stdout.strip()
        if shown.returncode != 0 or not config_path:
            raise UpgradeCheckUnavailable(
                f"{previous} could not print its configuration: {shown.stderr.strip()}"
            )
        pathlib.Path(config_path).parent.mkdir(parents=True, exist_ok=True)
        pathlib.Path(config_path).write_text(shown.stdout, encoding="utf-8")

        new_environment = _environment(home, executable.parent)
        upgraded = _run([str(executable), "dates"], new_environment)
        if upgraded.returncode != 0:
            return [
                f"the first command after upgrading from {previous} failed: "
                f"{upgraded.stderr.strip()}"
            ]
        status = _run(
            [
                str(executable), "integrations", "status", "--format", "json",
                *_client_arguments(clients),
            ],
            new_environment,
        )
        if status.returncode != 0:
            return [f"integrations status failed after upgrading from {previous}: {status.stderr}"]
        problems = [line.replace(str(home), "~") for line in problems_in_status(status.stdout)]
        return problems + _earlier_release_still_reads(previous, old, old_environment, clients, home)

    except BackgroundChildrenAlive as error:
        cleanup = False
        raise UpgradeCheckUnavailable(f"{error}; preserved temporary home {home}; stop that group before removing it") from error
    finally:
        if cleanup:
            shutil.rmtree(home)


def _versions_to_check(requested: list[str] | None, since: str) -> list[str] | None:
    """Return the releases to upgrade from, or None after saying why none can be chosen."""
    for version in [*(requested or []), since]:
        if PYTHON_RELEASE_VERSION.fullmatch(version) is None:
            print(f"not a release version: {version!r}", file=sys.stderr)
            return None
    if requested:
        return requested
    try:
        versions = published_versions_since(since)
    except (OSError, ValueError, KeyError) as error:
        # Passing here would report an upgrade check that checked nothing.
        print(
            f"could not read the published versions from {PYPI_PROJECT_URL}: {error}; pass --from",
            file=sys.stderr,
        )
        return None
    if not versions:
        print(f"PyPI lists no release at or above {since}; pass --from", file=sys.stderr)
        return None
    return versions


def _check_all(versions: list[str], executable: pathlib.Path) -> int:
    """Check each upgrade: 0 when all pass, 1 when any needs a manual step, 2 when any was unknown."""
    failed = False
    unavailable = False
    for previous in versions:
        try:
            problems = check_upgrade(previous, executable)
        except UpgradeCheckUnavailable as error:
            unavailable = True
            print(
                f"could not check upgrading from {previous}, so this is not an upgrade failure: "
                f"{error}",
                file=sys.stderr,
            )
            continue
        if problems:
            failed = True
            print(f"upgrading from {previous} needs a manual step:", file=sys.stderr)
            for problem in problems:
                print(f"  {problem}", file=sys.stderr)
        else:
            print(f"upgrading from {previous}: its config loads, every integration is current, "
                  f"and {previous} still loads both after a downgrade")
    if failed:
        return 1
    return 2 if unavailable else 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--executable", type=pathlib.Path, required=True)
    parser.add_argument(
        "--from",
        dest="previous",
        action="append",
        help="published version to upgrade from; repeat to check several (default: every "
        "release PyPI lists from --since on)",
    )
    parser.add_argument(
        "--since",
        default=UPGRADE_FLOOR,
        help=f"oldest published release to check when --from is omitted (default: {UPGRADE_FLOOR})",
    )
    args = parser.parse_args(argv)
    if os.name == "nt":
        print(
            "verify_upgrade_path refuses to run on Windows: aise finds the home folder through the "
            "Windows known-folder API, which HOME and USERPROFILE cannot redirect, so the earlier "
            "release would install into your real profile. Run it on Linux or macOS; CI runs it "
            "on Linux.",
            file=sys.stderr,
        )
        return 2
    versions = _versions_to_check(args.previous, args.since)
    if versions is None:
        return 2
    return _check_all(versions, args.executable.resolve())


if __name__ == "__main__":
    raise SystemExit(main())
