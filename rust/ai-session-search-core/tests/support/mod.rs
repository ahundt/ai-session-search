// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;
use std::process::Command;

pub fn aise(root: &Path) -> Command {
    command(
        root,
        std::ffi::OsStr::new(env!("CARGO_BIN_EXE_aise")),
        std::env::vars_os(),
    )
}

fn command(
    root: &Path,
    program: &std::ffi::OsStr,
    inherited: impl Iterator<Item = (OsString, OsString)>,
) -> Command {
    let config = root.join("isolated-config.toml");
    let mut text = String::from("[skills]\nsearch_paths = []\n");
    for provider in [
        "claude",
        "claude-desktop",
        "codex",
        "cursor",
        "antigravity",
        "pi",
        "prime-agent",
        "aistudio",
        "gemini-cli",
    ] {
        text.push_str(&format!(
            "[providers.{provider}]\nenabled = false\npaths = []\n"
        ));
    }
    std::fs::create_dir_all(root).unwrap();
    let mut staged = tempfile::NamedTempFile::new_in(root).unwrap();
    staged.write_all(text.as_bytes()).unwrap();
    staged.persist(&config).unwrap();
    let mut command = Command::new(program);
    command.env_clear().envs(inherited.filter(|(key, _)| {
        matches!(
            key.to_str().map(str::to_ascii_uppercase).as_deref(),
            Some(
                "PATH"
                    | "SYSTEMROOT"
                    | "WINDIR"
                    | "COMSPEC"
                    | "PATHEXT"
                    | "TEMP"
                    | "TMP"
                    | "TMPDIR"
            )
        )
    }));
    for key in include_str!("../../harness-environment.txt")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        command.env(key, root.join(key.to_ascii_lowercase()));
    }
    command
        .env("OPENCLAW_PROFILE", "aise-isolated")
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("APPDATA", root.join("AppData/Roaming"))
        .env("LOCALAPPDATA", root.join("AppData/Local"))
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("XDG_DATA_HOME", root.join(".local/share"))
        .env("XDG_CACHE_HOME", root.join(".cache"))
        .env("XDG_STATE_HOME", root.join(".local/state"))
        .env("AI_SESSION_SEARCH_HARNESS_HOME", root)
        .env("AI_SESSION_SEARCH_CONFIG", config)
        .env("AI_SESSION_SEARCH_DATABASE", root.join("index.db"))
        .env("AI_SESSION_SEARCH_CACHE_DIR", root.join("cache"))
        .env("AI_SESSION_SEARCH_SKIP_RELEASE_NOTIFICATION", "1");
    command
}

#[test]
fn execution_allowlist_accepts_windows_environment_casing() {
    let root = tempfile::tempdir().unwrap();
    let child = command(
        root.path(),
        std::ffi::OsStr::new(env!("CARGO_BIN_EXE_aise")),
        [
            ("SystemRoot".into(), "system".into()),
            ("Path".into(), "bin".into()),
        ]
        .into_iter(),
    );
    for (key, value) in [("SystemRoot", "system"), ("Path", "bin")] {
        assert!(child.get_envs().any(|(name, selected)| name
            .to_string_lossy()
            .eq_ignore_ascii_case(key)
            && selected == Some(std::ffi::OsStr::new(value))));
    }
}

#[test]
fn poisoned_environment_cannot_escape_the_owned_root() {
    const STAGE: &str = "AISE_TEST_ENV_STAGE";
    const ROOT: &str = "AISE_TEST_ENV_ROOT";
    const TEST: &str = "support::poisoned_environment_cannot_escape_the_owned_root";
    if let Some(stage) = std::env::var_os(STAGE) {
        let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
        let production = root.parent().unwrap().join("production");
        if stage == "probe" {
            assert!(std::env::vars_os().all(|(_, value)| value != production.as_os_str()));
        } else {
            let executable = std::env::current_exe().unwrap();
            let output = command(&root, executable.as_os_str(), std::env::vars_os())
                .env(STAGE, "probe")
                .env(ROOT, &root)
                .args(["--exact", TEST, "--nocapture"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let production = root.path().join("production");
    std::fs::create_dir(&production).unwrap();
    let sentinel = production.join("sentinel");
    std::fs::write(&sentinel, "unchanged").unwrap();
    let names = include_str!("../../harness-environment.txt")
        .lines()
        .chain([
            "AI_SESSION_SEARCH_FUTURE_PATH",
            "OPENCLAW_FUTURE_PATH",
            "XDG_FUTURE_PATH",
            "AI_SESSION_SEARCH_CONFIG",
            "AI_SESSION_SEARCH_DATABASE",
            "AI_SESSION_SEARCH_CACHE_DIR",
        ])
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let sandbox = root.path().join("sandbox");
    let output = Command::new(std::env::current_exe().unwrap())
        .envs(names.map(|name| (name, production.as_os_str())))
        .env(STAGE, "parent")
        .env(ROOT, &sandbox)
        .args(["--exact", TEST, "--nocapture"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(aise(&sandbox)
        .args(["config", "paths", "--format", "json"])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
    assert_eq!(std::fs::read_dir(production).unwrap().count(), 1);
}
