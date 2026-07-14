use std::fs;
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

fn isolated_paths_args(root: &std::path::Path) -> Vec<String> {
    let config = root.join("config.toml");
    fs::write(&config, "").unwrap();
    vec![
        "--config".into(),
        config.display().to_string(),
        "--database".into(),
        root.join("index.db").display().to_string(),
        "--cache-dir".into(),
        root.join("cache").display().to_string(),
        "paths".into(),
    ]
}

#[cfg(unix)]
#[test]
fn paths_reports_active_executable_and_ordered_executable_candidates() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first bin");
    let second = root.path().join("second bin");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    for directory in [&first, &second] {
        let candidate = directory.join("aise");
        fs::write(&candidate, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(candidate, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([&first, &second]).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_aise"))
        .args(isolated_paths_args(root.path()))
        .env("PATH", path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("Executable: {}", env!("CARGO_BIN_EXE_aise"))));
    assert!(
        stdout.contains(&format!(
            "Config: {}",
            root.path().join("config.toml").display()
        )),
        "{stdout}"
    );
    let candidates = format!(
        "PATH aise candidates: {}, {}",
        first.join("aise").display(),
        second.join("aise").display()
    );
    assert!(stdout.contains(&candidates), "{stdout}");
}

#[cfg(unix)]
#[test]
fn short_reader_pipeline_never_prints_a_broken_pipe_panic() {
    let root = tempfile::tempdir().unwrap();
    let executable = env!("CARGO_BIN_EXE_aise");
    let mut command = Command::new("sh");
    command.arg("-c").arg("\"$0\" \"$@\" | head -n 1");
    command.arg(executable);
    command.args(isolated_paths_args(root.path()));

    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(!stderr.contains("Broken pipe"), "{stderr}");
}
