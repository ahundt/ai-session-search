# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import fnmatch
import hashlib
import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# The provider set every public surface has to agree on. PROVIDER_COUNT_WORD is the spelling the
# README uses in prose; adding a provider changes it, which is what forces the README edit.
PROVIDER_IDS = (
    "claude",
    "claude-desktop",
    "codex",
    "cursor",
    "antigravity",
    "pi",
    "prime-agent",
    "aistudio",
    "gemini-cli",
)
PROVIDER_COUNT_WORD = "nine"
# Reader-facing names, which are friendlier than Provider::display_name; "Codex" alone so the
# README stays free to write "ChatGPT Codex" or "Codex CLI".
README_PROVIDER_NAMES = (
    "Claude Code",
    "Claude Desktop",
    "Codex",
    "Cursor",
    "Antigravity",
    "Pi",
    "Prime Agent",
    "Google AI Studio",
    "Gemini CLI",
)


def test_python_metadata_and_maturin_require_cp312_abi3_through_314() -> None:
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))
    classifiers = project["project"]["classifiers"]
    assert project["project"]["requires-python"] == ">=3.12"
    assert "Programming Language :: Python :: 3.12" in classifiers
    assert "Programming Language :: Python :: 3.13" in classifiers
    assert "Programming Language :: Python :: 3.14" in classifiers
    assert project["tool"]["maturin"]["features"] == ["extension-module", "abi3"]


def test_ci_runtime_matrix_covers_supported_python_versions() -> None:
    workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    assert "python-version: ['3.12', '3.13', '3.14']" in workflow
    assert "uv run maturin build --release --locked" in workflow


def test_local_ci_is_locked_isolated_and_matches_blocking_quality_gates() -> None:
    script = (ROOT / "run_ci_local.sh").read_text(encoding="utf-8")
    for required in [
        "uv lock --check",
        "uv sync --locked --all-extras",
        "AI_SESSION_SEARCH_CONFIG",
        "AI_SESSION_SEARCH_CACHE_DIR",
        "uv run ruff check .",
        "uv run mypy ai_session_search tests",
        "mypy.stubtest ai_session_search --concise --ignore-disjoint-bases",
        "cargo test --workspace --all-targets --all-features --locked",
        "cargo build --release --locked --bin aise",
        "uv run --no-project python scripts/render_message_search_docs.py --check --aise",
        "--summary-items",
        "truncated_evidence",
        "next_offset",
        "verify_python_install_methods.py",
        "--source-native-import",
    ]:
        assert required in script
    assert "hatchling" not in script.lower()
    assert "uv.lock is NOT committed" not in script
    assert "non-blocking" not in script.lower()
    assert 'reject_retired_release_schema "evidence_truncation"' in script
    assert 'reject_retired_release_schema "row_truncated"' in script
    assert 'export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$SCRIPT_DIR/target}"' in script
    assert 'export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"' in script
    assert "[providers.aistudio]" in script
    assert "[providers.ai-studio]" not in script

    install_verifier = (ROOT / "scripts/verify_python_install_methods.py").read_text(encoding="utf-8")
    assert 'environment["UV_CACHE_DIR"] =' not in install_verifier
    assert 'environment["CARGO_TARGET_DIR"] =' not in install_verifier
    distribution_verifier = (ROOT / "scripts/verify_installed_distribution.py").read_text(encoding="utf-8")
    for namespace in ('("config",)', '("integrations",)', '("mcp",)', '("package",)'):
        assert namespace in distribution_verifier
    assert '("mcp",): {"serve"}' in distribution_verifier
    for tool_name in (
        "search_sessions",
        "get_session",
        "list_sessions",
        "get_resume_command",
        "search_messages",
        "get_index_status",
        "query_session_index",
    ):
        assert f'    "{tool_name}",' in distribution_verifier


def test_local_ci_quarantines_stale_native_modules_and_restores_them() -> None:
    script = (ROOT / "run_ci_local.sh").read_text(encoding="utf-8")

    for required in (
        "quarantine_source_native_modules",
        "build_current_python_extension",
        "restore_source_native_modules",
        "FRESH_NATIVE_ARTIFACTS",
        "ORIGINAL_NATIVE_MANIFEST",
        "LOCAL_CI_LOCK",
        "uv run maturin develop --uv",
        "trap cleanup_local_ci EXIT",
    ):
        assert required in script
    assert script.index("quarantine_source_native_modules ||") < script.index('step "Sync locked Python development environment"')
    assert script.index('step "Build current ABI3 Python extension"') < script.index('step "Native runtime/stub parity"')
    assert 'rm -f -- "$NATIVE_MODULE_DIR"/_native*' not in script
    assert 'if [ "$CURRENT_PYTHON_EXTENSION_READY" != true ]' in script
    assert 'cksum <"$artifact"' in script
    assert "another local CI run owns" in script
    assert "restored native module failed checksum verification" in script
    assert "unhandled native-module recovery artifacts remain" in script


def test_demo_uses_current_identity_and_never_offers_fixture_deletion() -> None:
    demo = (ROOT / "tests/test_demo.py").read_text(encoding="utf-8")
    assert "/ar:claude-session-tools" not in demo
    assert "/ar:ai-session-tools" not in demo
    assert "github.com/ahundt/autorun" not in demo
    assert "$ai-session-search" in demo
    assert "installed by aise integrations install" in demo
    assert "uv pip install git+" not in demo
    assert 'parser.add_argument("--cleanup"' not in demo
    assert "cleanup_synthetic_data" not in demo
    assert '"--renderer", "fontdue"' not in demo


def test_packaged_skill_tree_matches_repository_skill_tree_and_is_forced_to_lf() -> None:
    """Both copies of the bundled skill must hold the same files with the same bytes.

    ``include_str!`` resolves relative to its own source file, so the crate embeds
    ``rust/ai-session-search-core/skills/`` while a human edits the repo-root
    ``skills/``. Nothing in the build compares them: an edit to one alone compiles,
    passes every Rust test, and ships a binary whose embedded policy differs from the
    reviewed file.

    This walks the package tree rather than naming individual files. Message
    classification ships as a side file inside the one ``ai-session-search`` package:
    the Agent Skills specification permits any additional files beside ``SKILL.md``,
    and it requires ``name`` to equal the directory name, so a second package would
    have to claim the generic top-level name ``corrections`` in every flat skill root.
    """

    def tree(root: Path) -> dict[str, bytes]:
        return {
            path.relative_to(root).as_posix(): path.read_bytes()
            for path in sorted(root.rglob("*"))
            if path.is_file()
        }

    package = "ai-session-search"
    repository_files = tree(ROOT / "skills" / package)
    packaged_files = tree(ROOT / "rust/ai-session-search-core/skills" / package)
    assert repository_files.keys() == packaged_files.keys(), (
        f"the two {package} copies hold different files; "
        f"only in repo root: {sorted(repository_files.keys() - packaged_files.keys())}; "
        f"only in crate: {sorted(packaged_files.keys() - repository_files.keys())}"
    )
    differing = [name for name, data in repository_files.items() if packaged_files[name] != data]
    assert not differing, f"these files differ between the two {package} copies: {differing}"

    assert repository_files.keys() == {
        "SKILL.md",
        "aise-capability.toml",
        "references/message-classification.md",
        "references/recover-prior-work-with-evidence.md",
    }, f"unexpected shipped skill files: {sorted(repository_files)}"

    # One shipped package. A second directory here becomes a second top-level skill in every
    # harness skill root, and its description loads at startup whether or not it is ever used.
    assert [path.name for path in sorted((ROOT / "skills").iterdir()) if path.is_dir()] == [package]
    assert not (ROOT / "skills/corrections").exists()
    assert not (ROOT / "rust/ai-session-search-core/skills/corrections").exists()
    delegated_research = ROOT / "skills/ai-session-search/references/recover-prior-work-with-evidence.md"
    assert delegated_research.is_file()
    assert "bounded evidence packet" in delegated_research.read_text(encoding="utf-8")

    manifest = (ROOT / "rust/ai-session-search-core/Cargo.toml").read_text(encoding="utf-8")
    assert '"skills/**"' in manifest

    # Directory globs rather than one line per file: a per-file list silently stops covering
    # the next file added, and CRLF in a policy would change its digest on Windows checkouts.
    attributes = (ROOT / ".gitattributes").read_text(encoding="utf-8")
    for glob in (
        "skills/** text eol=lf",
        "rust/ai-session-search-core/skills/** text eol=lf",
    ):
        assert glob in attributes, f"missing .gitattributes rule: {glob}"


def test_skill_package_documentation_matches_the_one_shipped_package() -> None:
    """Maintainer and installer docs must not restore the retired sibling package."""
    documented = "\n".join(
        (ROOT / path).read_text(encoding="utf-8")
        for path in (
            "docs/development/configuration.md",
            "docs/development/installation.md",
            "docs/development/maintainer-requirements-and-design-decisions.md",
        )
    )
    for retired_claim in (
        "two sibling skill packages",
        "canonical end-user packages `ai-session-search` and `corrections`",
        "must end in `ai-session-search` or `corrections`",
        "Automatic packages live under",
    ):
        assert retired_claim not in documented


def test_internal_maintainer_skill_is_project_scoped_and_not_packaged() -> None:
    """Developer guidance has one repo-owned copy and never enters user installs."""
    internal = ROOT / ".agents/skills/maintain-ai-session-search"
    claude_link = ROOT / ".claude/skills/maintain-ai-session-search"
    skill = internal / "SKILL.md"
    requirements = ROOT / "docs/development/maintainer-requirements-and-design-decisions.md"

    assert skill.is_file()
    skill_text = skill.read_text(encoding="utf-8")
    assert "repository-internal developer guidance" in skill_text
    assert "not an end-user skill shipped by aise" in skill_text
    assert "maintainer-requirements-and-design-decisions.md" in skill_text
    assert "`REQ037-explore-before-change`" in skill_text
    assert "`REQ038-map-semantic-ownership`" in skill_text
    assert "`REQ039-reuse-or-improve-architecture`" in skill_text
    assert "`REQ040-eliminate-semantic-duplication`" in skill_text
    assert "`REQ041-optimize-multi-objective-outcomes`" in skill_text
    assert "`REQ010-protect-complexity-bounds`" in skill_text
    assert "`REQ027-use-tdd`" in skill_text
    assert "`REQ042-plan-fine-grained-work`" in skill_text
    assert "`REQ001-preserve-user-data`" in skill_text
    assert "`REQ003-preserve-surface-semantics`" in skill_text
    assert "`REQ023-accept-capability-parameters`" in skill_text
    assert "`REQ036-preserve-existing-strengths`" in skill_text
    assert re.search(r"REQ\d{3}(?!-)", skill_text) is None
    assert "exact user wording" not in skill_text
    assert "numbered quote" not in skill_text
    catalog_text = skill_text.split("## Prioritized requirement catalog", 1)[1].split(
        "## Execution sequence", 1
    )[0]
    skill_requirement_ids = re.findall(r"`(REQ\d{3}-[a-z0-9-]+)`", catalog_text)

    assert claude_link.is_symlink()
    assert claude_link.resolve(strict=True) == internal.resolve(strict=True)

    requirements_text = requirements.read_text(encoding="utf-8")
    assert "## P0 — discovery, architecture, correctness, and data safety" in requirements_text
    assert "### REQ037-explore-before-change" in requirements_text
    assert "### REQ041-optimize-multi-objective-outcomes" in requirements_text
    assert "### REQ010-protect-complexity-bounds" in requirements_text
    assert "### REQ042-plan-fine-grained-work" in requirements_text
    assert "### REQ003-preserve-surface-semantics" in requirements_text
    assert "### REQ023-accept-capability-parameters" in requirements_text
    assert "## Verification map" in requirements_text
    assert "~/.gemini/config/mcp_config.json" in requirements_text
    assert "ModuleNotFoundError: No module named 'encodings'" in requirements_text
    assert "Cumulative user requirements" not in requirements_text
    assert re.search(r"(?m)^\d+\. > ", requirements_text) is None
    assert re.search(r"REQ\d{3}(?!-)", requirements_text) is None
    documented_requirement_ids = re.findall(
        r"(?m)^### (REQ\d{3}-[a-z0-9-]+)$",
        requirements_text,
    )
    assert skill_requirement_ids
    assert len(set(skill_requirement_ids)) == len(skill_requirement_ids)
    assert skill_requirement_ids == documented_requirement_ids

    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))
    maturin_includes = {entry["path"] for entry in project["tool"]["maturin"].get("include", [])}
    assert not any(path.startswith((".agents/", ".claude/")) for path in maturin_includes)

    crate = tomllib.loads((ROOT / "rust/ai-session-search-core/Cargo.toml").read_text(encoding="utf-8"))
    assert not any(path.startswith(("../../.agents/", "../../.claude/")) for path in crate["package"]["include"])

    integrations = (ROOT / "rust/ai-session-search-core/src/integrations.rs").read_text(encoding="utf-8")
    assert "maintain-ai-session-search" not in integrations


def test_rust_crate_packages_the_release_benchmark_driver() -> None:
    """The published crate must retain the benchmark example used for RC comparisons."""
    manifest = tomllib.loads((ROOT / "rust/ai-session-search-core/Cargo.toml").read_text(encoding="utf-8"))

    assert "examples/**" in manifest["package"]["include"]
    assert (ROOT / "rust/ai-session-search-core/examples/benchmark_core.rs").is_file()


def test_python_ci_creates_its_explicit_config_before_running_tests() -> None:
    workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    create = workflow.index("Create isolated test configuration")
    tests = workflow.index('uv run pytest -m "not integration" --tb=short')

    assert create < tests
    assert 'path.write_text("", encoding="utf-8")' in workflow


def test_public_docs_match_native_abi_mcp_and_quality_gates() -> None:
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    releasing = (ROOT / "RELEASING.md").read_text(encoding="utf-8")
    architecture = (ROOT / "docs/migration/rust-python-api-architecture.md").read_text(encoding="utf-8")
    parity = (ROOT / "docs/migration/capability-parity.md").read_text(encoding="utf-8")

    assert "uv sync --locked --all-extras" in readme
    assert "mypy.stubtest ai_session_search" in readme
    assert "closed schemas" in readme
    configuration = (ROOT / "docs/development/configuration.md").read_text(encoding="utf-8")
    # Both line-window scopes stay documented with their shared sign convention.
    assert "--transcript-lines" in readme
    assert "--lines-per-message" in readme
    assert "show_transcript_lines" in configuration
    assert "lines_per_message" in configuration
    assert "get_session_transcript_lines" in configuration
    assert "Per-message windows are presentation-only" in configuration
    assert "do not change matches, ranking, result" in configuration
    assert "skimmable without silently discarding hits" in configuration
    installation = (ROOT / "docs/development/installation.md").read_text(encoding="utf-8")
    assert "legacy VS Code extension adapter" in installation
    assert "does not install" in installation and "hooks" in installation
    assert "ChatGPT Codex desktop" in installation
    assert "~/.gemini/config/mcp_config.json" in installation
    assert "~/.gemini/antigravity-cli/skills/" in installation
    for native_root in (
        "~/.pi/agent/skills/",
        "~/.prime/agent/skills/",
        "~/.pi/agent/AGENTS.md",
        "~/.prime/agent/AGENTS.md",
    ):
        assert native_root in installation
    assert "Pi has" in installation and "MCP client" in installation
    assert "Prime currently accepts remote HTTP MCP servers" in installation
    acceptance = installation.split("The integration acceptance matrix", 1)[1]
    assert "Pi" in acceptance and "Prime Agent" in acceptance
    assert "toml_edit::DocumentMut" in configuration
    assert "ChatGPT Codex desktop" in configuration
    assert "~/.gemini/config/mcp_config.json" in configuration
    assert "~/.gemini/antigravity-cli/skills/" in configuration
    assert "Pi has no native MCP client" in configuration
    assert "Prime accepts remote HTTP MCP" in configuration
    assert "shared RAII lock" in readme
    assert "CPython 3.12 through 3.14" in releasing
    assert "cp312-abi3" in releasing
    assert "Free-threaded CPython is not supported" in releasing
    assert "rust/ai-session-search-core/" in architecture
    assert "Target `abi3-py312` only after" not in architecture
    assert "additionalProperties=false" in parity
    requirements = (ROOT / "docs/development/maintainer-requirements-and-design-decisions.md").read_text(
        encoding="utf-8"
    )
    assert "peak growth is `O(B_session)`" in requirements


def test_readme_opening_and_format_count_cover_every_provider() -> None:
    # Prime Agent landed as a distinct provider: the session-source table gained a row and the
    # body said "all nine formats", while the heading above it and the one-line summary still
    # said eight and never named Prime Agent. Tie the summary, the count, and the table to one
    # provider set so the next provider cannot land in the table alone.
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    summary = readme.split("\n## ", 1)[0]
    for name in README_PROVIDER_NAMES:
        assert name in summary, f"README summary above the first heading omits {name}"
    assert f"{PROVIDER_COUNT_WORD.capitalize()} session formats" in readme
    assert f"parses all {PROVIDER_COUNT_WORD} formats" in readme
    for provider_id in PROVIDER_IDS:
        assert f"`{provider_id}`" in readme, f"README has no session-source row for {provider_id}"


def test_message_query_docs_distinguish_query_field_from_tool_filter() -> None:
    models = (ROOT / "rust/ai-session-search-core/src/models.rs").read_text(encoding="utf-8")
    cli = (ROOT / "rust/ai-session-search-core/src/messages.rs").read_text(encoding="utf-8")
    binding = (ROOT / "rust/ai-session-search-python/src/lib.rs").read_text(encoding="utf-8")
    stub = (ROOT / "ai_session_search/_native.pyi").read_text(encoding="utf-8")
    normalized_models = " ".join(models.replace("///", "").split())
    normalized_cli = " ".join(cli.replace("///", "").split())
    normalized_binding = " ".join(binding.replace("///", "").split())
    normalized_stub = " ".join(stub.split())

    for provider_id in PROVIDER_IDS:
        assert provider_id in models
    assert "The query searches only `field`" in normalized_models
    assert "independent of `field`" in normalized_models
    assert "QUERY searches only this field" in normalized_cli
    assert "independent of --field" in normalized_cli
    assert "The query searches only ``field``" in normalized_stub
    assert "independent of ``field``" in normalized_stub
    assert "The query searches only `field`" in normalized_binding
    assert "independent of `field`" in normalized_binding


def test_message_search_default_extent_is_documented_consistently() -> None:
    # Agent guidance lives in AGENTS.md for every tool following that convention, and
    # CLAUDE.md imports it rather than holding a second copy that could drift. Assert the
    # import is intact, otherwise Claude Code silently reads no project guidance at all.
    assert (ROOT / "CLAUDE.md").read_text(encoding="utf-8").strip() == "@AGENTS.md"
    # Normalized because the guidance is wrapped prose, not the single line it used to be.
    guidance = " ".join((ROOT / "AGENTS.md").read_text(encoding="utf-8").split())
    assert "Rust, CLI, and Python preserve all literal/regex/no-text matches" in guidance
    assert "MCP alone supplies a bounded default" in guidance
    assert "presentation bounds never change hit membership" in guidance

    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    cli = (ROOT / "rust/ai-session-search-core/src/messages.rs").read_text(encoding="utf-8")
    core = (ROOT / "rust/ai-session-search-core/src/message_search.rs").read_text(encoding="utf-8")
    service = (ROOT / "rust/ai-session-search-core/src/service.rs").read_text(encoding="utf-8")
    binding = (ROOT / "rust/ai-session-search-python/src/lib.rs").read_text(encoding="utf-8")
    stub = (ROOT / "ai_session_search/_native.pyi").read_text(encoding="utf-8")

    assert "Rust, CLI, and Python message search are unbounded on omission" in " ".join(readme.split())
    assert "every literal, regex, or no-text CLI match" in cli
    assert "Rust, CLI, and Python" in core and "MCP" in core
    assert "Native programmatic/interactive surfaces preserve" in service
    assert "omitting `limit` returns all literal, regex, or no-text matches in Python" in binding
    assert "omitting ``limit`` returns all literal, regex, or no-text Python matches" in stub


def test_elapsed_time_policy_is_optional_and_surface_specific() -> None:
    config_example = (ROOT / "rust/ai-session-search-core/config.example.toml").read_text(encoding="utf-8")
    recovery = (ROOT / "skills/ai-session-search/references/recover-prior-work-with-evidence.md").read_text(encoding="utf-8")

    assert "sqlite_timeout_ms" not in config_example
    assert "query_timeout_ms = 0" in config_example
    assert "MCP raw-SQL execution timeout" in config_example
    assert "max_elapsed_minutes: <optional positive integer>" in recovery
    assert "Every supplied budget must be finite and positive" in recovery


def test_every_surface_that_shows_a_resume_command_discloses_its_policy_boundary() -> None:
    # resume_plan emits the provider's resume verb and the session id and nothing else. A
    # cold-recovery audit found the returned command correct but not behaviorally equivalent: that
    # user's Codex history always carries `-c approval_policy=on-request` and their Claude history
    # always carries `--dangerously-skip-permissions`. Running the bare command reopens the right
    # conversation under different permission behavior, so each surface that shows one must say so
    # before it is run, not after.
    util = (ROOT / "rust/ai-session-search-core/src/util.rs").read_text(encoding="utf-8")
    assert "pub const RESUME_COMMAND_POLICY_NOTE" in util, "one authority for the sentence"

    # The two interactive surfaces print it above their own "Execute resume command?" prompt.
    for surface in ("cli.rs", "tui.rs"):
        source = (ROOT / "rust/ai-session-search-core/src" / surface).read_text(encoding="utf-8")
        assert "RESUME_COMMAND_POLICY_NOTE" in source, surface

    # MCP callers read the published description rather than terminal output, so the boundary
    # belongs in the output schema they already fetch.
    mcp = (ROOT / "rust/ai-session-search-core/src/mcp_server.rs").read_text(encoding="utf-8")
    resume_field = mcp.split('"resume_command": { "type": "string", "description": "', 1)[1]
    resume_field = resume_field.split('" },', 1)[0]
    assert "does not reproduce local permission or approval flags" in resume_field


def test_ci_covers_release_architectures_without_repeating_static_analysis() -> None:
    from pathlib import Path

    workflow = Path(".github/workflows/ci.yml").read_text(encoding="utf-8")
    assert "ubuntu-24.04-arm" in workflow
    assert "macos-15-intel" in workflow
    assert "matrix.os == 'ubuntu-latest' && matrix.python-version == '3.12'" in workflow


def test_ci_runs_rust_portability_tests_on_native_macos_and_windows() -> None:
    workflow = Path(".github/workflows/ci.yml").read_text(encoding="utf-8")
    portability_job = workflow.split("  rust-portability:\n", 1)[1].split("\n  rust-install:", 1)[0]

    assert "os: [macos-latest, windows-latest]" in portability_job
    assert "ubuntu-latest" not in portability_job
    assert "fail-fast: false" in portability_job
    assert "cargo test -p ai-session-search --all-targets --locked" in portability_job
    assert "cargo clippy" not in portability_job
    assert "uv run ruff" not in portability_job


def test_release_uses_trusted_publishing_for_both_package_registries() -> None:
    from pathlib import Path

    workflow = Path(".github/workflows/publish.yml").read_text(encoding="utf-8")
    assert "rust-lang/crates-io-auth-action@c6f97d42243bad5fab37ca0427f495c86d5b1a18" in workflow
    assert "CARGO_REGISTRY_TOKEN: ${{ steps.crates-io-auth.outputs.token }}" in workflow
    assert "cargo publish --locked -p ai-session-search" in workflow
    assert "pypa/gh-action-pypi-publish@" in workflow
    assert workflow.count("timeout-minutes:") >= 9


def test_release_checksums_validate_beside_downloaded_assets() -> None:
    workflow = (ROOT / ".github/workflows/publish.yml").read_text(encoding="utf-8")

    assert "(cd dist && sha256sum -- * > SHA256SUMS)" in workflow
    assert "sha256sum dist/* > dist/SHA256SUMS" not in workflow


def test_wheels_and_native_jobs_pin_the_build_timestamp_to_the_commit() -> None:
    # maturin stamps the wheel's embedded PEP 770 SBOM with metadata.timestamp and a
    # fresh serialNumber. Those two fields were the entire difference between the
    # v1.0.0rc1 production wheels and the TestPyPI rehearsal wheels built from the same
    # commit: 13 of 15 zip entries matched, including the compiled extension, and the
    # RECORD differed only because it hashes the SBOM. Exporting SOURCE_DATE_EPOCH makes
    # the whole wheel byte-identical across builds.
    jobs = _workflow_jobs((ROOT / ".github/workflows/publish.yml").read_text(encoding="utf-8"))

    wheels = jobs["wheels"]
    assert 'SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)' in wheels
    # The manylinux targets build inside a container that does not inherit the runner
    # environment, so setting the variable alone would silently not apply to them.
    assert "docker-options: -e SOURCE_DATE_EPOCH" in wheels

    assert "SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)" in jobs["native"]


def test_wheels_job_proves_the_pinned_build_clock_reached_the_build() -> None:
    # Exporting SOURCE_DATE_EPOCH and forwarding it with docker-options are two separate
    # claims, and the second one crosses a container boundary that nothing observes. If
    # the forwarding silently stopped working the wheels would still build, install, and
    # pass every other check; they would just no longer be reproducible from the commit.
    # Passing the epoch to the verifier turns that into a build failure: measured against
    # the real v1.0.0rc1 production wheel, which was built before the pin existed, the
    # check rejects it and names the recorded build time.
    wheels = _workflow_jobs((ROOT / ".github/workflows/publish.yml").read_text(encoding="utf-8"))["wheels"]

    assert '--source-date-epoch "$SOURCE_DATE_EPOCH"' in wheels


def _workflow_jobs(text: str) -> dict[str, str]:
    """Split a workflow into job name -> job body, keyed on the two-space job indent."""
    body = text.split("\njobs:\n", 1)[1]
    jobs: dict[str, str] = {}
    name: str | None = None
    lines: list[str] = []
    for line in body.splitlines():
        header = re.fullmatch(r"  ([A-Za-z0-9_-]+):\s*", line)
        if header:
            if name is not None:
                jobs[name] = "\n".join(lines)
            name = header.group(1)
            lines = []
        elif name is not None:
            lines.append(line)
    if name is not None:
        jobs[name] = "\n".join(lines)
    return jobs


def test_gh_cli_calls_name_the_repository_when_the_job_has_no_checkout() -> None:
    # `gh` resolves the target repository from the git remote. A job that only downloads
    # artifacts has no working tree, so gh exits 1 with "fatal: not a git repository"
    # before it does anything. That failed the `release` job of the v1.0.0rc1 run after
    # crates.io and PyPI had both published, leaving the tag without a GitHub Release.
    # Every gh operation used here is API-backed, so naming the repository is enough.
    offenders: list[str] = []
    for workflow in sorted((ROOT / ".github/workflows").glob("*.yml")):
        for job_name, job in _workflow_jobs(workflow.read_text(encoding="utf-8")).items():
            if "actions/checkout" in job:
                continue
            calls = [
                line.strip()
                for line in job.splitlines()
                if re.match(r"\s*gh\s+[a-z-]+", line) and not line.lstrip().startswith("#")
            ]
            if calls and "--repo" not in job:
                offenders.append(f"{workflow.name}:{job_name}: {calls[0]}")

    assert not offenders, (
        "gh runs without a checkout and without --repo, so it cannot resolve the "
        f"repository: {offenders}"
    )


_ARTIFACT_STEP = re.compile(r"^(\s*)- uses: actions/(upload|download)-artifact@")
_ARTIFACT_KEY = re.compile(r"^\s*(name|pattern):\s*(\S.*?)\s*$")
_CALLED_WORKFLOW = re.compile(r"^\s*uses:\s*\./(\.github/workflows/[A-Za-z0-9._-]+)\s*$", re.M)


def _artifact_flows(text: str) -> tuple[set[str], set[str]]:
    """Return the artifact names a workflow uploads and the ones it asks to download."""
    uploaded: set[str] = set()
    requested: set[str] = set()
    lines = text.splitlines()
    for index, line in enumerate(lines):
        step = _ARTIFACT_STEP.match(line)
        if step is None:
            continue
        indent, direction = step.group(1), step.group(2)
        for follower in lines[index + 1 :]:
            # The next step starts at the same indent, which ends this step's `with:`.
            if follower.strip() and len(follower) - len(follower.lstrip()) <= len(indent):
                break
            key = _ARTIFACT_KEY.match(follower)
            if key is None:
                continue
            value = key.group(2).strip("'\"")
            if direction == "upload":
                if key.group(1) == "name":
                    # A matrix name is one template standing for many real artifacts.
                    uploaded.add(re.sub(r"\$\{\{[^}]*\}\}", "*", value))
            else:
                requested.add(value)
    return uploaded, requested


def test_every_downloaded_artifact_is_uploaded_by_the_same_run() -> None:
    # download-artifact fails the job when no artifact carries the name, and the jobs that
    # download are the last ones in the pipeline: `release` runs after crates.io and PyPI
    # have both published, where a failure is unrecoverable for that version. Renaming an
    # upload is a one-line edit whose only consequence appears at that point. The `test`
    # job calls ci.yml as a reusable workflow, so its uploads are in the same run and
    # count here; that is how `dependency-license-inventories` reaches `verify`.
    offenders: list[str] = []
    for workflow in sorted((ROOT / ".github/workflows").glob("*.yml")):
        text = workflow.read_text(encoding="utf-8")
        uploaded, requested = _artifact_flows(text)
        for called in _CALLED_WORKFLOW.findall(text):
            uploaded |= _artifact_flows((ROOT / called).read_text(encoding="utf-8"))[0]
        for name in sorted(requested):
            if not any(fnmatch.fnmatchcase(candidate, name) for candidate in uploaded):
                offenders.append(f"{workflow.name}: downloads {name!r}, uploaded={sorted(uploaded)}")

    assert not offenders, (
        "a job downloads an artifact name that no job in the same run uploads: " f"{offenders}"
    )


def test_maturin_action_pins_the_maturin_version_the_project_pins() -> None:
    # maturin-action's `maturin-version` input defaults to `latest`, and none of the
    # workflows set it, so the wheels a release publishes are built by whatever maturin
    # was newest that day. ci.yml's `uv run maturin` and every local test use the version
    # pyproject pins instead. That splits the pipeline two ways: the same commit stops
    # rebuilding to the same bytes as soon as upstream releases, which is the property the
    # SOURCE_DATE_EPOCH pin exists to provide, and the release path runs a build tool no
    # test ever exercised. Deriving the expected pin from pyproject keeps one source of
    # truth, so bumping the dependency without bumping the workflows fails here.
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))
    pins = [
        requirement
        for requirement in project["project"]["optional-dependencies"]["dev"]
        if requirement.startswith("maturin==")
    ]
    assert len(pins) == 1, f"expected exactly one pinned maturin requirement, found {pins}"
    expected = f"maturin-version: v{pins[0].removeprefix('maturin==')}"

    offenders: list[str] = []
    for workflow in sorted((ROOT / ".github/workflows").glob("*.yml")):
        text = workflow.read_text(encoding="utf-8")
        steps = text.count("uses: PyO3/maturin-action@")
        pinned = text.count(expected)
        if steps != pinned:
            offenders.append(f"{workflow.name}: {steps} maturin-action steps, {pinned} pinned")

    assert not offenders, (
        f"every maturin-action step must set {expected!r}, or the release builds with a "
        f"different maturin than the tests do: {offenders}"
    )


def test_msrv_job_compiles_library_tests() -> None:
    workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    msrv_job = workflow.split("  rust-msrv:\n", 1)[1].split("\n  rust-portability:", 1)[0]

    assert "cargo test -p ai-session-search --lib --locked --no-run" in msrv_job


# These three scripts import `scripts.release_versions`, so the repository root must be on
# sys.path. `python -m scripts.<name>` puts the working directory there; `python scripts/<name>.py`
# puts `scripts/` there instead and raises ModuleNotFoundError. The file-path spelling only appears
# to work under an editable install whose .pth file happens to add the root, which is not present in
# the isolated interpreters the release jobs use.
RELEASE_PIPELINE_MODULES = ("prepare_packages", "verify_release_artifacts", "verify_release_metadata")
RELEASE_PIPELINE_CALLERS = (
    ".github/workflows/ci.yml",
    ".github/workflows/prepare-packages.yml",
    ".github/workflows/publish.yml",
    "run_ci_local.sh",
    "RELEASING.md",
    "docs/development/releasing.md",
)


def test_release_pipeline_scripts_run_without_an_editable_install_on_sys_path() -> None:
    for module in RELEASE_PIPELINE_MODULES:
        # -S skips site-packages .pth processing, reproducing the isolated interpreter the
        # release jobs run without depending on this checkout's virtual environment layout.
        completed = subprocess.run(
            [sys.executable, "-S", "-m", f"scripts.{module}", "--help"],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        assert completed.returncode == 0, f"scripts.{module} is not runnable: {completed.stderr}"


def test_release_pipeline_callers_run_release_scripts_as_modules() -> None:
    for relative in RELEASE_PIPELINE_CALLERS:
        text = (ROOT / relative).read_text(encoding="utf-8")
        for module in RELEASE_PIPELINE_MODULES:
            assert f"scripts/{module}.py" not in text, (
                f"{relative} runs scripts/{module}.py by path; use -m scripts.{module} so the "
                "repository root stays on sys.path in isolated interpreters"
            )


def test_manual_package_preparation_defaults_to_all_without_publish_credentials() -> None:
    workflow = Path(".github/workflows/prepare-packages.yml").read_text(encoding="utf-8")
    assert "default: all" in workflow
    assert "options: [all, rust, python]" in workflow
    assert "cargo package --locked -p ai-session-search" in workflow
    assert "uv build --no-sources --sdist" in workflow
    assert workflow.count("maturin-action@e83996d129638aa358a18fbd1dfb82f0b0fb5d3b") == 1
    assert "id-token: write" not in workflow
    assert "cargo publish" not in workflow
    assert "gh-action-pypi-publish" not in workflow


def test_every_workflow_file_is_syntax_checked_by_a_blocking_ci_job() -> None:
    # actionlint with no file arguments checks every file under .github/workflows, so a workflow
    # added later cannot escape the check by not appearing in an argument list.
    local_gate = (ROOT / "run_ci_local.sh").read_text(encoding="utf-8")
    assert 'step "GitHub workflow syntax" actionlint\n' in local_gate

    # The local gate prints SKIPPED rather than failing when actionlint is absent, so the check
    # only blocks a merge if a required CI job also runs it. workflow-security is a required
    # status check on main; slice that job so the step cannot drift into an optional one.
    workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    security_job = workflow.split("\n  workflow-security:\n", 1)[1].split("\n  rust:\n", 1)[0]
    assert "\n        run: actionlint\n" in security_job
    # Pinned by version rather than tracking a moving tag, matching cargo-deny and zizmor.
    assert "actionlint/cmd/actionlint@v${ACTIONLINT_VERSION}" in security_job
    assert re.search(r"^  ACTIONLINT_VERSION: \d+\.\d+\.\d+$", workflow, re.MULTILINE)


# SHA-256 of https://www.apache.org/licenses/LICENSE-2.0.txt. Every manifest declares the
# SPDX expression Apache-2.0, and each LICENSE copy is shipped inside the wheel, sdist, crate,
# and native archives, so the bytes must be the unmodified upstream text rather than a re-wrapped
# or summarized paraphrase of it.
APACHE_2_0_SHA256 = "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
LICENSE_COPIES = ("LICENSE", "rust/ai-session-search-core/LICENSE")


def test_every_shipped_license_copy_is_the_verbatim_apache_2_0_text() -> None:
    attributes = (ROOT / ".gitattributes").read_text(encoding="utf-8")
    for relative in LICENSE_COPIES:
        digest = hashlib.sha256((ROOT / relative).read_bytes()).hexdigest()
        assert digest == APACHE_2_0_SHA256, (
            f"{relative} is not the verbatim Apache-2.0 text; every manifest declares the "
            "Apache-2.0 SPDX expression, so altered wording would misdeclare the published license"
        )
        rule = f"{relative} text eol=lf"
        assert rule in attributes, f"missing .gitattributes rule: {rule}"


def test_manifests_and_notice_declare_one_consistent_license_and_copyright_holder() -> None:
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))
    core = tomllib.loads((ROOT / "rust/ai-session-search-core/Cargo.toml").read_text(encoding="utf-8"))
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    assert project["project"]["license"] == "Apache-2.0"
    assert workspace["workspace"]["package"]["license"] == "Apache-2.0"
    assert core["package"]["license"]["workspace"] is True

    notice = (ROOT / "NOTICE").read_text(encoding="utf-8")
    # Apache-2.0 section 4(d) puts the attribution notice in NOTICE, so the copyright holder
    # named there must match the distribution authors rather than an unrelated entity.
    authors = {author["name"] for author in project["project"]["authors"]}
    assert any(f"Copyright 2026 {name}" in notice for name in authors), notice


def test_published_crate_declares_every_field_cargo_asks_for_before_publishing() -> None:
    # https://doc.rust-lang.org/cargo/reference/publishing.html lists these as the fields to fill
    # in before a first crates.io publication. A missing one is accepted by `cargo publish` and
    # only shows up as a gap on the rendered crate page.
    core = tomllib.loads((ROOT / "rust/ai-session-search-core/Cargo.toml").read_text(encoding="utf-8"))
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]
    package = core["package"]
    for field in ("description", "license", "homepage", "repository", "readme", "keywords", "categories"):
        value = package.get(field)
        if isinstance(value, dict) and value.get("workspace") is True:
            value = workspace.get(field)
        assert value, f"rust/ai-session-search-core/Cargo.toml declares no {field}"
    # crates.io caps keywords at five, each at most twenty characters.
    assert len(package["keywords"]) <= 5
    assert all(len(keyword) <= 20 for keyword in package["keywords"])


def test_ci_checks_rust_security_advisories_with_individually_recorded_exceptions() -> None:
    workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    assert "cargo deny --locked check advisories licenses sources bans" in workflow

    policy = tomllib.loads((ROOT / "deny.toml").read_text(encoding="utf-8"))
    # An empty [advisories] table silently accepts every future advisory once the check runs,
    # so require the exception list to exist and to name each identifier it waives.
    ignored = policy["advisories"].get("ignore", [])
    assert all(str(entry).startswith("RUSTSEC-") for entry in ignored), ignored
    for identifier in ignored:
        assert identifier in (ROOT / "deny.toml").read_text(encoding="utf-8")


def test_dependency_automation_covers_each_locked_ecosystem() -> None:
    from pathlib import Path

    policy = Path(".github/dependabot.yml").read_text(encoding="utf-8")
    for ecosystem in ("uv", "cargo", "github-actions"):
        assert f'package-ecosystem: "{ecosystem}"' in policy
    assert policy.count('interval: "weekly"') == 3


def test_release_guide_records_standard_tool_adoption_decisions() -> None:
    from pathlib import Path

    guide = Path("docs/development/releasing.md").read_text(encoding="utf-8")
    for tool in (
        "maturin-action",
        "cibuildwheel",
        "cargo-dist",
        "release-plz",
        "cargo-semver-checks",
        "cargo-binstall",
        "Dependabot",
        "zizmor",
    ):
        assert tool in guide


def test_temporal_complexity_contracts_are_kept_with_their_owners() -> None:
    owners: dict[str, tuple[str, ...]] = {
        "rust/ai-session-search-core/src/db.rs": ("fn push_session_time_window", "O(S)", "O(1)` application memory"),
        "rust/ai-session-search-core/src/service.rs": ("pub fn list_sessions", "O(S log S + O + K)", "pub fn search_sessions"),
        "rust/ai-session-search-python/src/lib.rs": ("fn list_sessions", "O(K + D_K)", "fn search_sessions"),
        "scripts/benchmark_release.py": ("def temporal_overlap_oracle", "in O(S)"),
    }
    for path, contracts in owners.items():
        source = (ROOT / path).read_text(encoding="utf-8")
        for contract in contracts:
            assert contract in source, f"{path} lacks {contract!r}"


def test_readme_and_packaged_skill_explain_temporal_and_recent_directory_retrieval() -> None:
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    skill = (ROOT / "skills/ai-session-search/SKILL.md").read_text(encoding="utf-8")

    for text in (readme, skill):
        assert "aise list --path ~/source/project --limit 1" in text
    for phrase in (
        "known indexed span",
        "`since` tests the span end",
        "`until` tests the span start",
        "span can contain gaps",
        "component-boundary descendant",
        "excludes lexical siblings",
    ):
        assert phrase in skill


def test_documentation_has_conventional_names_and_task_navigation() -> None:
    docs = Path("docs")
    markdown_paths = sorted(path.relative_to(docs) for path in docs.rglob("*.md"))
    assert markdown_paths
    assert all(path.name == "README.md" or path.name == path.name.lower() for path in markdown_paths)
    index = (docs / "README.md").read_text(encoding="utf-8")
    for path in (
        "development/installation.md",
        "development/configuration.md",
        "development/releasing.md",
        "migration/ai-session-search-major-migration.md",
    ):
        assert f"]({path})" in index


def test_ci_runs_one_pinned_offline_workflow_security_audit() -> None:
    from pathlib import Path

    workflow = Path(".github/workflows/ci.yml").read_text(encoding="utf-8")
    assert "ZIZMOR_VERSION: 1.26.1" in workflow
    assert workflow.count("zizmor --offline .") == 1
    assert "zizmor==${{ env.ZIZMOR_VERSION }}" in workflow
