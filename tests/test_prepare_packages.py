from __future__ import annotations

from pathlib import Path

import pytest

from scripts import prepare_packages as packaging
from scripts.prepare_packages import PreparationError, argument_parser, prepare_packages


def _builder(name: str, calls: list[str]):
    def build(staging: Path) -> list[Path]:
        calls.append(name)
        artifact = staging / name
        artifact.write_bytes(name.encode())
        return [artifact]

    return build


def test_default_scope_prepares_the_complete_package_set() -> None:
    args = argument_parser().parse_args([])
    assert args.package == "all"


@pytest.mark.parametrize(
    ("scope", "expected_calls"),
    [("all", ["package.crate", "package.whl"]), ("rust", ["package.crate"]), ("python", ["package.whl"])],
)
def test_scopes_publish_one_atomic_verified_directory(
    tmp_path: Path, scope: str, expected_calls: list[str]
) -> None:
    calls: list[str] = []
    verified: list[str] = []
    output = tmp_path / "prepared"

    artifacts = prepare_packages(
        scope,
        output,
        rust_builder=_builder("package.crate", calls),
        python_builder=_builder("package.whl", calls),
        verifier=lambda paths: verified.extend(path.name for path in paths),
    )

    assert calls == expected_calls
    assert verified == sorted(expected_calls)
    assert [path.name for path in artifacts] == sorted(expected_calls)
    assert sorted(path.name for path in output.iterdir()) == sorted(expected_calls)
    assert not list(tmp_path.glob(".aise-packages-*"))


def test_builder_failure_leaves_no_partial_output_or_staging_directory(tmp_path: Path) -> None:
    output = tmp_path / "prepared"

    def fail(_staging: Path) -> list[Path]:
        raise PreparationError("synthetic build failure")

    with pytest.raises(PreparationError, match="synthetic build failure"):
        prepare_packages("all", output, rust_builder=_builder("package.crate", []), python_builder=fail)

    assert not output.exists()
    assert not list(tmp_path.glob(".aise-packages-*"))


def test_existing_output_is_rejected_before_any_builder_runs(tmp_path: Path) -> None:
    output = tmp_path / "prepared"
    output.mkdir()
    calls: list[str] = []

    with pytest.raises(PreparationError, match="already exists"):
        prepare_packages("all", output, rust_builder=_builder("package.crate", calls))

    assert calls == []


def test_commands_enable_cacheable_rust_without_overriding_explicit_configuration(monkeypatch: pytest.MonkeyPatch) -> None:
    environments: list[dict[str, str]] = []

    def capture(*_args, **kwargs) -> None:
        environments.append(kwargs["env"])

    monkeypatch.setattr(packaging.subprocess, "run", capture)
    monkeypatch.setattr(packaging.shutil, "which", lambda name: f"/cache/{name}")
    monkeypatch.delenv("CARGO_INCREMENTAL", raising=False)
    monkeypatch.delenv("CARGO_TARGET_DIR", raising=False)
    monkeypatch.setenv("RUSTC_WRAPPER", "sccache")
    packaging._run(("cargo", "metadata"))
    monkeypatch.setenv("CARGO_INCREMENTAL", "1")
    monkeypatch.setenv("CARGO_TARGET_DIR", "/custom/target")
    monkeypatch.setenv("RUSTC_WRAPPER", "/custom/wrapper")
    packaging._run(("cargo", "metadata"))

    assert environments[0]["CARGO_INCREMENTAL"] == "0"
    assert environments[0]["CARGO_TARGET_DIR"] == str(packaging.ROOT / "target")
    assert environments[0]["RUSTC_WRAPPER"] == "/cache/sccache"
    assert environments[1]["CARGO_INCREMENTAL"] == "1"
    assert environments[1]["CARGO_TARGET_DIR"] == "/custom/target"
    assert environments[1]["RUSTC_WRAPPER"] == "/custom/wrapper"


def test_rust_builder_removes_only_its_duplicate_cargo_outputs(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    python_version = "1.2.3rc1"
    cargo_version = "1.2.3-rc.1"
    (tmp_path / "pyproject.toml").write_text(
        f'[project]\nversion = "{python_version}"\n', encoding="utf-8"
    )
    (tmp_path / "rust/ai-session-search-core").mkdir(parents=True)
    (tmp_path / "rust/ai-session-search-core/Cargo.toml").write_text(
        f'[package]\nversion = "{cargo_version}"\n', encoding="utf-8"
    )
    package_dir = tmp_path / "custom-target" / "package"

    def fake_run(_command) -> None:
        package_dir.mkdir(parents=True)
        (package_dir / f"ai-session-search-{cargo_version}.crate").write_bytes(b"crate")
        (package_dir / f"ai-session-search-{cargo_version}").mkdir()

    monkeypatch.setattr(packaging, "ROOT", tmp_path)
    monkeypatch.setattr(packaging, "_run", fake_run)
    monkeypatch.setenv("CARGO_TARGET_DIR", "custom-target")
    staging = tmp_path / "staging"
    staging.mkdir()

    artifacts = packaging.build_rust(staging)

    assert [artifact.read_bytes() for artifact in artifacts] == [b"crate"]
    assert not (package_dir / f"ai-session-search-{cargo_version}.crate").exists()
    assert not (package_dir / f"ai-session-search-{cargo_version}").exists()
