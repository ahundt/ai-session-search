from __future__ import annotations

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

    install_verifier = (ROOT / "scripts/verify_python_install_methods.py").read_text(
        encoding="utf-8"
    )
    assert 'environment["UV_CACHE_DIR"] =' not in install_verifier
    assert 'environment["CARGO_TARGET_DIR"] =' not in install_verifier
    distribution_verifier = (
        ROOT / "scripts/verify_installed_distribution.py"
    ).read_text(encoding="utf-8")
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
    assert script.index("quarantine_source_native_modules ||") < script.index(
        'step "Sync locked Python development environment"'
    )
    assert script.index('step "Build current ABI3 Python extension"') < script.index(
        'step "Native runtime/stub parity"'
    )
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


def test_packaged_skill_matches_repository_skill_and_is_forced_to_lf() -> None:
    repository_skill = ROOT / "skills/ai-session-search/SKILL.md"
    packaged_skill = (
        ROOT
        / "rust/ai-session-search-core/skills/ai-session-search/SKILL.md"
    )
    attributes = (ROOT / ".gitattributes").read_text(encoding="utf-8")
    manifest = (ROOT / "rust/ai-session-search-core/Cargo.toml").read_text(
        encoding="utf-8"
    )

    assert packaged_skill.read_bytes() == repository_skill.read_bytes()
    assert '"skills/**"' in manifest
    assert "skills/ai-session-search/SKILL.md text eol=lf" in attributes
    assert (
        "rust/ai-session-search-core/skills/ai-session-search/SKILL.md text eol=lf"
        in attributes
    )


def test_python_ci_creates_its_explicit_config_before_running_tests() -> None:
    workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    create = workflow.index("Create isolated test configuration")
    tests = workflow.index('uv run pytest -m "not integration" --tb=short')

    assert create < tests
    assert 'path.write_text("", encoding="utf-8")' in workflow


def test_public_docs_match_native_abi_mcp_and_quality_gates() -> None:
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    releasing = (ROOT / "RELEASING.md").read_text(encoding="utf-8")
    architecture = (ROOT / "docs/migration/rust-python-api-architecture.md").read_text(
        encoding="utf-8"
    )
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
    assert "toml_edit::DocumentMut" in configuration
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


def test_ci_covers_release_architectures_without_repeating_static_analysis() -> None:
    from pathlib import Path

    workflow = Path(".github/workflows/ci.yml").read_text(encoding="utf-8")
    assert "ubuntu-24.04-arm" in workflow
    assert "macos-15-intel" in workflow
    assert "matrix.os == 'ubuntu-latest' && matrix.python-version == '3.12'" in workflow


def test_ci_runs_rust_portability_tests_on_native_macos_and_windows() -> None:
    workflow = Path(".github/workflows/ci.yml").read_text(encoding="utf-8")
    portability_job = workflow.split("  rust-portability:\n", 1)[1].split(
        "\n  rust-install:", 1
    )[0]

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
    assert 'zizmor==${{ env.ZIZMOR_VERSION }}' in workflow
