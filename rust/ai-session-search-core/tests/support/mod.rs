// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

pub fn aise(root: &Path) -> Command {
    command(root, std::env::vars_os())
}

fn command(root: &Path, inherited: impl Iterator<Item = (OsString, OsString)>) -> Command {
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
    std::fs::write(&config, text).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_aise"));
    command.env_clear().envs(inherited.filter(|(key, _)| {
        matches!(
            key.to_str(),
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
fn poisoned_environment_cannot_escape_the_owned_root() {
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
    let mut child = command(
        &root.path().join("sandbox"),
        names.map(|name| (OsString::from(name), production.as_os_str().to_owned())),
    );
    assert!(child
        .get_envs()
        .all(|(_, value)| value != Some(production.as_os_str())));
    assert!(child
        .args(["config", "paths", "--format", "json"])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
    assert_eq!(std::fs::read_dir(production).unwrap().count(), 1);
}
