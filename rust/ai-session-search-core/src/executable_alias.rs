use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::util::which;

const ALIAS_NAMES: [&str; 2] = ["aisearch", "ai_session_search"];

#[derive(Debug)]
pub(crate) struct ExecutableAliases {
    aliases: Vec<PathBuf>,
    target: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AliasStatus {
    Missing,
    Owned,
    Conflict,
}

impl ExecutableAliases {
    pub(crate) fn discover() -> Result<Self> {
        let executable = which("aise")
            .ok_or_else(|| anyhow!("aise is not on PATH; executable aliases cannot be managed"))?;
        let parent = executable
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "resolved aise executable has no parent directory: {}",
                    executable.display()
                )
            })?;
        let target = executable.file_name().map(PathBuf::from).ok_or_else(|| {
            anyhow!(
                "resolved aise executable has no file name: {}",
                executable.display()
            )
        })?;
        let aliases = ALIAS_NAMES
            .iter()
            .map(|name| parent.join(executable_name(name)))
            .collect();
        Ok(Self { aliases, target })
    }

    pub(crate) fn preflight_install(&self) -> Result<()> {
        for alias in &self.aliases {
            if self.status(alias)? == AliasStatus::Conflict {
                bail!(
                    "refusing to replace executable alias {} because it is not a symbolic link to {}; remove or relocate it, or rerun with --no-aliases",
                    alias.display(),
                    self.target.display()
                );
            }
        }
        Ok(())
    }

    pub(crate) fn install(&self) -> Result<AliasInstallGuard> {
        self.preflight_install()?;
        let mut guard = AliasInstallGuard::default();
        for alias in &self.aliases {
            if self.status(alias)? == AliasStatus::Missing {
                create_file_symlink(&self.target, alias).with_context(|| {
                    format!(
                        "failed to create executable alias {} -> {}; rerun with --no-aliases to skip aliases",
                        alias.display(),
                        self.target.display()
                    )
                })?;
                guard.created.push(alias.clone());
            }
        }
        Ok(guard)
    }

    pub(crate) fn status_lines(&self) -> Result<Vec<String>> {
        self.aliases
            .iter()
            .map(|alias| {
                let state = match self.status(alias)? {
                    AliasStatus::Missing => "missing",
                    AliasStatus::Owned => "configured",
                    AliasStatus::Conflict => "conflict (preserved)",
                };
                Ok(format!("executable alias {}: {state}", alias.display()))
            })
            .collect()
    }

    pub(crate) fn install_lines(&self, dry_run: bool) -> Result<Vec<String>> {
        self.aliases
            .iter()
            .map(|alias| {
                let action = match (dry_run, self.status(alias)?) {
                    (true, AliasStatus::Missing) => "dry-run: would create",
                    (true, AliasStatus::Owned) => "dry-run: already configured",
                    (_, AliasStatus::Conflict) => {
                        bail!(
                            "executable alias changed after preflight: {}",
                            alias.display()
                        )
                    }
                    (false, _) => "configured",
                };
                Ok(format!(
                    "{action} executable alias {} -> {}",
                    alias.display(),
                    self.target.display()
                ))
            })
            .collect()
    }

    pub(crate) fn uninstall_lines(&self, dry_run: bool) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        for alias in &self.aliases {
            match self.status(alias)? {
                AliasStatus::Owned if dry_run => lines.push(format!(
                    "dry-run: would remove executable alias {}",
                    alias.display()
                )),
                AliasStatus::Owned => {
                    fs::remove_file(alias).with_context(|| {
                        format!("failed to remove executable alias {}", alias.display())
                    })?;
                    lines.push(format!("removed executable alias {}", alias.display()));
                }
                AliasStatus::Conflict => lines.push(format!(
                    "preserved non-owned executable alias path {}",
                    alias.display()
                )),
                AliasStatus::Missing => {}
            }
        }
        Ok(lines)
    }

    fn status(&self, alias: &Path) -> Result<AliasStatus> {
        match fs::symlink_metadata(alias) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(alias).with_context(|| {
                    format!("failed to read executable alias {}", alias.display())
                })?;
                Ok(if target == self.target {
                    AliasStatus::Owned
                } else {
                    AliasStatus::Conflict
                })
            }
            Ok(_) => Ok(AliasStatus::Conflict),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(AliasStatus::Missing),
            Err(error) => Err(error)
                .with_context(|| format!("failed to inspect executable alias {}", alias.display())),
        }
    }

    #[cfg(test)]
    fn for_test(executable: PathBuf) -> Self {
        let parent = executable.parent().unwrap();
        let target = executable.file_name().unwrap().into();
        Self {
            aliases: ALIAS_NAMES
                .iter()
                .map(|name| parent.join(executable_name(name)))
                .collect(),
            target,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct AliasInstallGuard {
    created: Vec<PathBuf>,
    committed: bool,
}

impl AliasInstallGuard {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for AliasInstallGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for alias in self.created.iter().rev() {
            if let Err(error) = fs::remove_file(alias) {
                eprintln!(
                    "warning: failed to roll back executable alias {}: {error}",
                    alias.display()
                );
            }
        }
    }
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, alias: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, alias)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, alias: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, alias)
}

#[cfg(not(any(unix, windows)))]
fn create_file_symlink(_target: &Path, _alias: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "symbolic links are unsupported on this platform",
    ))
}

fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[test]
    fn install_is_relative_idempotent_and_uninstall_preserves_conflicts() {
        let dir = tempdir().unwrap();
        let executable = dir.path().join("aise");
        fs::write(&executable, "binary").unwrap();
        let aliases = ExecutableAliases::for_test(executable);

        aliases.install().unwrap().commit();
        aliases.install().unwrap().commit();
        assert_eq!(
            fs::read_link(dir.path().join("aisearch")).unwrap(),
            Path::new("aise")
        );
        assert_eq!(
            fs::read_link(dir.path().join("ai_session_search")).unwrap(),
            Path::new("aise")
        );

        fs::remove_file(dir.path().join("aisearch")).unwrap();
        fs::write(dir.path().join("aisearch"), "user-owned").unwrap();
        let lines = aliases.uninstall_lines(false).unwrap();

        assert!(dir.path().join("aisearch").is_file());
        assert!(!dir.path().join("ai_session_search").exists());
        assert!(lines
            .iter()
            .any(|line| line.contains("preserved non-owned")));
    }

    #[cfg(unix)]
    #[test]
    fn install_guard_removes_only_links_created_by_failed_install() {
        let dir = tempdir().unwrap();
        let executable = dir.path().join("aise");
        fs::write(&executable, "binary").unwrap();
        let aliases = ExecutableAliases::for_test(executable);

        let guard = aliases.install().unwrap();
        assert!(dir.path().join("aisearch").is_symlink());
        drop(guard);

        assert!(!dir.path().join("aisearch").exists());
        assert!(!dir.path().join("ai_session_search").exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_every_non_owned_destination_before_writing() {
        let dir = tempdir().unwrap();
        let executable = dir.path().join("aise");
        fs::write(&executable, "binary").unwrap();
        fs::write(dir.path().join("ai_session_search"), "user-owned").unwrap();
        let aliases = ExecutableAliases::for_test(executable);

        let error = aliases.install().unwrap_err().to_string();

        assert!(error.contains("refusing to replace executable alias"));
        assert!(!dir.path().join("aisearch").exists());
    }

    #[cfg(unix)]
    #[test]
    fn status_lines_report_missing_then_configured_then_conflict() {
        let dir = tempdir().unwrap();
        let executable = dir.path().join("aise");
        fs::write(&executable, "binary").unwrap();
        let aliases = ExecutableAliases::for_test(executable);

        // Before install every alias path is missing.
        let lines = aliases.status_lines().unwrap();
        assert!(lines.iter().all(|l| l.ends_with("missing")), "{lines:?}");

        // After a committed install every alias is a configured owned symlink.
        aliases.install().unwrap().commit();
        let lines = aliases.status_lines().unwrap();
        assert!(lines.iter().all(|l| l.ends_with("configured")), "{lines:?}");

        // Replacing one alias with a non-owned regular file reports a preserved conflict.
        fs::remove_file(dir.path().join("aisearch")).unwrap();
        fs::write(dir.path().join("aisearch"), "user-owned").unwrap();
        let lines = aliases.status_lines().unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("aisearch") && l.ends_with("conflict (preserved)")),
            "{lines:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_lines_preview_dry_run_and_report_configured_after_install() {
        let dir = tempdir().unwrap();
        let executable = dir.path().join("aise");
        fs::write(&executable, "binary").unwrap();
        let aliases = ExecutableAliases::for_test(executable);

        // Dry-run over missing aliases previews creation and writes nothing.
        let lines = aliases.install_lines(true).unwrap();
        assert!(
            lines.iter().all(|l| l.contains("dry-run: would create")),
            "{lines:?}"
        );
        assert!(!dir.path().join("aisearch").exists());

        // After a real install, dry-run reports the aliases already configured.
        aliases.install().unwrap().commit();
        let lines = aliases.install_lines(true).unwrap();
        assert!(
            lines
                .iter()
                .all(|l| l.contains("dry-run: already configured")),
            "{lines:?}"
        );

        // A non-dry-run reports each alias configured with the relative target arrow.
        let lines = aliases.install_lines(false).unwrap();
        assert!(
            lines
                .iter()
                .all(|l| l.contains("configured executable alias") && l.contains("-> aise")),
            "{lines:?}"
        );
    }

    #[test]
    fn executable_name_matches_platform_extension() {
        // The alias file name carries the platform executable extension so a
        // Windows alias resolves as a program, not an extensionless file.
        #[cfg(windows)]
        assert_eq!(executable_name("aisearch"), "aisearch.exe");
        #[cfg(not(windows))]
        assert_eq!(executable_name("aisearch"), "aisearch");
    }
}
