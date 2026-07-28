from __future__ import annotations

import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


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

    This walks both package trees rather than naming individual files. Message
    classification is a sibling ``corrections`` package, while the general
    ``ai-session-search`` package contains harness guidance only.
    """

    def tree(root: Path) -> dict[str, bytes]:
        return {str(path.relative_to(root)): path.read_bytes() for path in sorted(root.rglob("*")) if path.is_file()}

    for package in ("ai-session-search", "corrections"):
        repository_files = tree(ROOT / "skills" / package)
        packaged_files = tree(ROOT / "rust/ai-session-search-core/skills" / package)
        assert repository_files.keys() == packaged_files.keys(), (
            f"the two {package} copies hold different files; "
            f"only in repo root: {sorted(repository_files.keys() - packaged_files.keys())}; "
            f"only in crate: {sorted(packaged_files.keys() - repository_files.keys())}"
        )
        differing = [name for name, data in repository_files.items() if packaged_files[name] != data]
        assert not differing, f"these files differ between the two {package} copies: {differing}"
        assert "SKILL.md" in repository_files

    assert (ROOT / "skills/corrections/capability.toml").is_file()
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
    assert "ChatGPT/Codex App and Codex CLI/IDE" in installation
    assert "~/.gemini/config/mcp_config.json" in installation
    assert "~/.gemini/antigravity-cli/skills/" in installation
    assert "toml_edit::DocumentMut" in configuration
    assert "ChatGPT/Codex App and Codex CLI/IDE" in configuration
    assert "~/.gemini/config/mcp_config.json" in configuration
    assert "~/.gemini/antigravity-cli/skills/" in configuration
    assert "shared RAII lock" in readme
    assert "CPython 3.12 through 3.14" in releasing
    assert "cp312-abi3" in releasing
    assert "Free-threaded CPython is not supported" in releasing
    assert "rust/ai-session-search-core/" in architecture
    assert "Target `abi3-py312` only after" not in architecture
    assert "additionalProperties=false" in parity


def test_message_query_docs_distinguish_query_field_from_tool_filter() -> None:
    models = (ROOT / "rust/ai-session-search-core/src/models.rs").read_text(encoding="utf-8")
    cli = (ROOT / "rust/ai-session-search-core/src/messages.rs").read_text(encoding="utf-8")
    binding = (ROOT / "rust/ai-session-search-python/src/lib.rs").read_text(encoding="utf-8")
    stub = (ROOT / "ai_session_search/_native.pyi").read_text(encoding="utf-8")
    normalized_models = " ".join(models.replace("///", "").split())
    normalized_cli = " ".join(cli.replace("///", "").split())
    normalized_binding = " ".join(binding.replace("///", "").split())
    normalized_stub = " ".join(stub.split())

    for provider_id in (
        "claude",
        "claude-desktop",
        "codex",
        "cursor",
        "antigravity",
        "pi",
        "aistudio",
        "gemini-cli",
    ):
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
    one_liner = (ROOT / "CLAUDE.md").read_text(encoding="utf-8")
    assert one_liner.count("\n") == 1
    assert "Rust, CLI, and Python preserve all literal/regex/no-text matches" in one_liner
    assert "MCP alone supplies a bounded default" in one_liner
    assert "presentation bounds never change hit membership" in one_liner

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
    local_gate = Path("run_ci_local.sh").read_text(encoding="utf-8")
    assert "actionlint .github/workflows/ci.yml .github/workflows/prepare-packages.yml .github/workflows/publish.yml" in local_gate


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
