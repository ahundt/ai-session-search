from __future__ import annotations

import os
from pathlib import Path

import pytest

from scripts import verify_python_install_methods as install_methods


def test_environment_removes_python_activation_state(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setenv("PYTHONPATH", "/source-leak")
    monkeypatch.setenv("VIRTUAL_ENV", "/active-environment")
    monkeypatch.setenv("PATH", os.pathsep.join(("first", "second")))
    monkeypatch.setenv("UV_CACHE_DIR", "/shared/uv-cache")
    monkeypatch.setenv("CARGO_TARGET_DIR", "/shared/cargo-target")
    bin_dir = tmp_path / "bin"

    environment = install_methods._environment(tmp_path, bin_dir)

    assert "PYTHONPATH" not in environment
    assert "VIRTUAL_ENV" not in environment
    assert environment["PATH"].split(os.pathsep) == [str(bin_dir), "first", "second"]
    assert environment["AI_SESSION_SEARCH_CONFIG"] == str(
        tmp_path / "config" / "config.toml"
    )
    assert (tmp_path / "config" / "config.toml").read_text(encoding="utf-8") == ""
    assert environment["AI_SESSION_SEARCH_CACHE_DIR"] == str(tmp_path / "cache")
    assert environment["UV_CACHE_DIR"] == "/shared/uv-cache"
    assert environment["CARGO_TARGET_DIR"] == "/shared/cargo-target"
    assert environment["PYTHONDONTWRITEBYTECODE"] == "1"
    assert environment["PIP_DISABLE_PIP_VERSION_CHECK"] == "1"
    assert environment["UV_PYTHON"] == install_methods.sys.executable
    assert os.environ["PYTHONPATH"] == "/source-leak"
    assert os.environ["VIRTUAL_ENV"] == "/active-environment"


def test_environment_does_not_invent_build_cache_roots(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.delenv("UV_CACHE_DIR", raising=False)
    monkeypatch.delenv("CARGO_TARGET_DIR", raising=False)

    environment = install_methods._environment(tmp_path)

    assert "UV_CACHE_DIR" not in environment
    assert "CARGO_TARGET_DIR" not in environment


def test_environment_uses_explicit_install_interpreter(tmp_path: Path) -> None:
    selected = tmp_path / "python"

    environment = install_methods._environment(tmp_path, python=selected)

    assert environment["UV_PYTHON"] == str(selected)


def test_verify_rejects_nonpositive_timeout_before_resolving_paths() -> None:
    with pytest.raises(install_methods.InstallMethodError, match="greater than zero"):
        install_methods.verify(Path("missing.whl"), Path("missing-source"), 0)


def test_git_requirement_uses_distribution_name_and_full_commit() -> None:
    revision = "A" * 40

    requirement = install_methods._git_requirement(
        "https://github.com/example/ai-session-search", revision
    )

    assert requirement == (
        "ai-session-search @ "
        f"git+https://github.com/example/ai-session-search@{revision.lower()}"
    )


def test_git_requirement_accepts_absolute_local_repository() -> None:
    revision = "1" * 64

    requirement = install_methods._git_requirement(
        "file:///tmp/ai-session-search", revision
    )

    assert requirement == (
        f"ai-session-search @ git+file:///tmp/ai-session-search@{revision}"
    )


@pytest.mark.parametrize(
    ("git_url", "git_rev", "error"),
    [
        ("https://github.com/example/project", "main", "full 40- or 64-hex"),
        ("https://github.com/example/project", "1" * 12, "full 40- or 64-hex"),
        ("http://github.com/example/project", "1" * 40, "scheme must be"),
        ("git+https://github.com/example/project", "1" * 40, "scheme must be"),
        ("https://token@github.com/example/project", "1" * 40, "credentials"),
        ("https://github.com/example/project#main", "1" * 40, "fragment"),
        ("file://remotehost/tmp/project", "1" * 40, "absolute local path"),
        ("file:relative/project", "1" * 40, "absolute local path"),
    ],
)
def test_git_requirement_rejects_mutable_or_unsafe_sources(
    git_url: str, git_rev: str, error: str
) -> None:
    with pytest.raises(install_methods.InstallMethodError, match=error):
        install_methods._git_requirement(git_url, git_rev)


def test_verify_git_dispatches_all_methods_with_one_immutable_requirement(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    source_root = tmp_path / "source"
    source_root.mkdir()
    revision = "2" * 40
    calls: list[tuple[str, str]] = []

    monkeypatch.setattr(install_methods.shutil, "which", lambda command: f"/{command}")

    def capture(method: str):
        def call(*args: object, **_kwargs: object) -> None:
            install_source = args[1] if method != "pip" else args[0]
            assert isinstance(install_source, str)
            calls.append((method, install_source))

        return call

    monkeypatch.setattr(install_methods, "_verify_pip", capture("pip"))
    monkeypatch.setattr(install_methods, "_verify_uv_add", capture("uv-add"))
    monkeypatch.setattr(install_methods, "_verify_uv_tool", capture("uv-tool"))
    monkeypatch.setattr(install_methods, "_verify_uvx", capture("uvx"))

    install_methods.verify_git(
        "file:///tmp/example-repository", revision, source_root
    )

    expected = f"ai-session-search @ git+file:///tmp/example-repository@{revision}"
    assert calls == [
        ("pip", expected),
        ("uv-add", expected),
        ("uv-tool", expected),
        ("uvx", expected),
    ]


def test_uvx_deferred_install_uses_configured_timeout(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    commands: list[list[str]] = []

    def capture(command: list[str], **_kwargs: object) -> None:
        commands.append(command)

    monkeypatch.setattr(install_methods, "_run", capture)

    install_methods._verify_uvx(
        "/tools/uvx",
        "ai-session-search @ git+file:///tmp/repository@" + "3" * 40,
        tmp_path / "verify_native.py",
        tmp_path / "run",
        321.0,
        python=Path("/runtime/python"),
    )

    assert commands == [
        [
            "/runtime/python",
            str(tmp_path / "verify_native.py"),
            "--executable",
            str(tmp_path / "run" / "aise-uvx"),
            "--command-timeout-seconds",
            "321.0",
        ]
    ]


def test_uv_tool_runtime_verifier_uses_configured_timeout(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    commands: list[list[str]] = []

    def capture(command: list[str], **_kwargs: object) -> None:
        commands.append(command)

    monkeypatch.setattr(install_methods, "_run", capture)

    install_methods._verify_uv_tool(
        "/tools/uv",
        str(tmp_path / "artifact.whl"),
        tmp_path / "verify_native.py",
        tmp_path / "run",
        321.0,
        python=Path("/runtime/python"),
    )

    assert commands[-1] == [
        "/runtime/python",
        str(tmp_path / "verify_native.py"),
        "--executable",
        str(tmp_path / "run" / "bin" / "aise"),
        "--command-timeout-seconds",
        "321.0",
    ]


def test_verify_removes_temporary_root_after_installer_failure(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    artifact = tmp_path / "artifact.whl"
    artifact.touch()
    source_root = tmp_path / "source"
    source_root.mkdir()
    temporary_roots: list[Path] = []

    monkeypatch.setattr(install_methods.shutil, "which", lambda command: f"/{command}")

    def fail_install(*args: object, **_kwargs: object) -> None:
        root = args[3]
        assert isinstance(root, Path)
        temporary_roots.append(root.parent)
        root.mkdir(parents=True)
        (root / "partial-install").touch()
        raise install_methods.InstallMethodError("injected failure")

    monkeypatch.setattr(install_methods, "_verify_pip", fail_install)

    with pytest.raises(install_methods.InstallMethodError, match="injected failure"):
        install_methods.verify(artifact, source_root)

    assert len(temporary_roots) == 1
    assert not temporary_roots[0].exists()


@pytest.mark.skipif(os.name == "nt", reason="POSIX wrapper contract")
def test_uvx_wrapper_preserves_paths_and_forwards_arguments(tmp_path: Path) -> None:
    artifact = tmp_path / "artifact with spaces.whl"
    wrapper = install_methods._uvx_wrapper("/uv path/uvx", str(artifact), tmp_path)

    assert wrapper.read_text(encoding="utf-8") == (
        "#!/bin/sh\n"
        f"exec '/uv path/uvx' --from '{artifact}' aise \"$@\"\n"
    )
    assert wrapper.stat().st_mode & 0o111
