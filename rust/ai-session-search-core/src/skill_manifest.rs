// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Durable record of exactly which skill files `aise` installed, and what bytes it wrote.
//!
//! # Why byte comparison alone is not enough
//!
//! Without this record, "does the file on disk equal the bytes this build embeds?" is the only
//! question install and uninstall can ask, and a NO answer has two very different causes:
//!
//! * the user edited the file, or
//! * they installed with an older `aise` and have not reinstalled since.
//!
//! Reporting the second as the first is not merely imprecise, it is a false statement to the user:
//! `uninstall` says *"modified since install — your edits are kept"* about a file they never
//! touched, and then preserves a stale directory forever. That ambiguity is finding S13, and it
//! exists in `status_skill_file` today.
//!
//! Recording the bytes at install time makes the two distinguishable: disk equals the manifest but
//! not the current build means *untouched, older version*; disk differs from the manifest means
//! *changed since install*.
//!
//! # What it deliberately cannot tell you
//!
//! Whether a change was an intentional edit or corruption. Nothing on disk carries that intent, so
//! every message here says **modified or damaged** and never picks one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::durable_path::EncodedPath;
use crate::hashing::sha256;

/// The only manifest `schema_version` this build writes or reads.
pub(crate) const SKILL_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// File name of the manifest, kept beside the resolved config file.
///
/// NOT inside the skill directory: a non-standard state file there would be exposed to host skill
/// validators, and uninstall could not tell it apart from a file the user added. NOT the
/// transaction receipt either — that one is transient and deleted on commit, while this must
/// outlive every command.
const SKILL_MANIFEST_FILE: &str = "skill-install-manifest.json";

/// One file `aise` wrote, relative to its skill root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstalledSkillFile {
    /// Slash-separated path under the skill root. Always UTF-8: these come from
    /// `MANAGED_SKILL_FILES`, which this build controls, so no platform encoding is needed.
    pub(crate) relative_path: String,
    pub(crate) bytes: usize,
    pub(crate) sha256: String,
}

/// One installed skill root and the exact files placed in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SkillInstallation {
    /// The skill directory, in this platform's native path encoding.
    pub(crate) root: EncodedPath,
    /// `aise` version that performed the install, for diagnostics.
    pub(crate) installed_by_version: String,
    pub(crate) files: Vec<InstalledSkillFile>,
}

/// Every skill root this machine has installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkillInstallManifest {
    pub(crate) schema_version: u32,
    pub(crate) installations: Vec<SkillInstallation>,
}

impl Default for SkillInstallManifest {
    fn default() -> Self {
        Self {
            schema_version: SKILL_MANIFEST_SCHEMA_VERSION,
            installations: Vec::new(),
        }
    }
}

/// Where the manifest lives for a given resolved config file.
pub(crate) fn manifest_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(SKILL_MANIFEST_FILE)
}

/// What reading a manifest told us. Absence and damage are different answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManifestState {
    /// No manifest file. Either nothing was installed, or it predates manifests.
    Absent,
    /// A manifest this build understands.
    Loaded(SkillInstallManifest),
    /// A manifest that exists but cannot be trusted, with the reason.
    ///
    /// Distinct from `Absent` so callers can say "ownership uncertain" rather than "never
    /// installed": deleting a tree because a JSON file was truncated would destroy the very
    /// evidence needed to explain what happened.
    Unreadable(String),
}

impl ManifestState {
    /// Return a trustworthy base for a manifest-changing operation.
    ///
    /// Read-only status and removal planning may inspect an unreadable state conservatively, but
    /// a write must not replace damaged ownership evidence with an empty record.
    pub(crate) fn writable_manifest(&self, path: &Path) -> Result<SkillInstallManifest> {
        match self {
            Self::Loaded(manifest) => Ok(manifest.clone()),
            Self::Absent => Ok(SkillInstallManifest::default()),
            Self::Unreadable(problem) => bail!(
                "cannot change managed skills because ownership manifest {} is unreadable: \
                 {problem}; repair or move that manifest, then retry",
                path.display()
            ),
        }
    }

    /// The installation recorded for `root`, if this manifest can be read at all.
    pub(crate) fn installation(&self, root: &Path) -> Option<&SkillInstallation> {
        match self {
            Self::Loaded(manifest) => manifest.installation(root),
            Self::Absent | Self::Unreadable(_) => None,
        }
    }
}

/// Read the manifest, distinguishing absent from damaged.
///
/// # Errors
///
/// Returns an error only when the file exists but cannot be read at all (permissions, I/O). A file
/// that reads but does not parse is reported as [`ManifestState::Unreadable`] rather than failing
/// the command: a damaged manifest must not make `status` or `uninstall` refuse to run, because
/// those are the commands you reach for when something is wrong.
pub(crate) fn load_manifest(path: &Path) -> Result<ManifestState> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManifestState::Absent)
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read skill install manifest {}", path.display())
            })
        }
    };
    match serde_json::from_str::<SkillInstallManifest>(&text) {
        Ok(manifest) if manifest.schema_version == SKILL_MANIFEST_SCHEMA_VERSION => {
            Ok(ManifestState::Loaded(manifest))
        }
        Ok(manifest) => Ok(ManifestState::Unreadable(format!(
            "manifest schema_version {} is not the {} this build understands; it was probably \
             written by a newer aise",
            manifest.schema_version, SKILL_MANIFEST_SCHEMA_VERSION
        ))),
        Err(error) => Ok(ManifestState::Unreadable(format!("{error}"))),
    }
}

impl SkillInstallManifest {
    /// The installation recorded for `root`, if any.
    pub(crate) fn installation(&self, root: &Path) -> Option<&SkillInstallation> {
        self.installations.iter().find(|installation| {
            installation
                .root
                .to_path_buf()
                .is_ok_and(|recorded| recorded == root)
        })
    }

    /// Record `files` as the current contents of `root`, replacing any earlier entry for it.
    ///
    /// Only this root's entry changes. Two harnesses installed at different times must not clear
    /// each other's record, which is the whole reason installations is a list rather than one
    /// global snapshot.
    pub(crate) fn record(&mut self, root: &Path, files: &[(String, &str)]) {
        let installation = SkillInstallation {
            root: EncodedPath::from_path(root),
            installed_by_version: env!("CARGO_PKG_VERSION").to_string(),
            files: files
                .iter()
                .map(|(relative_path, content)| InstalledSkillFile {
                    relative_path: relative_path.clone(),
                    bytes: content.len(),
                    sha256: sha256(content.as_bytes()),
                })
                .collect(),
        };
        self.forget(root);
        self.installations.push(installation);
        self.sort();
    }

    /// Drop `root`'s entry. Silent when there is none: uninstalling a root that was never
    /// recorded is a normal state, not an error.
    pub(crate) fn forget(&mut self, root: &Path) {
        self.installations.retain(|installation| {
            installation
                .root
                .to_path_buf()
                .is_ok_and(|recorded| recorded != root)
        });
    }

    /// Sort by recorded path so the serialized file is stable across runs.
    ///
    /// Without this the JSON would reorder whenever install visits roots in a different order,
    /// producing a diff that says nothing.
    fn sort(&mut self) {
        let mut keyed: BTreeMap<Vec<u8>, SkillInstallation> = BTreeMap::new();
        for installation in self.installations.drain(..) {
            let key = installation
                .root
                .to_path_buf()
                .map(|path| path.as_os_str().as_encoded_bytes().to_vec())
                .unwrap_or_default();
            keyed.insert(key, installation);
        }
        self.installations = keyed.into_values().collect();
    }

    /// Serialize with a trailing newline, as every other managed text file this repo writes.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization fails.
    pub(crate) fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }
}

impl SkillInstallation {
    /// The recorded digest for one relative path, if this install wrote it.
    pub(crate) fn file(&self, relative_path: &str) -> Option<&InstalledSkillFile> {
        self.files
            .iter()
            .find(|file| file.relative_path == relative_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<(String, &'static str)> {
        vec![
            ("SKILL.md".to_string(), "skill body"),
            ("corrections/policy.toml".to_string(), "policy body"),
        ]
    }

    #[test]
    fn a_recorded_install_round_trips_through_json() {
        let mut manifest = SkillInstallManifest::default();
        let root = Path::new("/home/test/.claude/skills/ai-session-search");
        manifest.record(root, &files());

        let json = manifest.to_json().unwrap();
        assert!(json.ends_with('\n'));
        let decoded: SkillInstallManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, manifest);

        let installation = decoded.installation(root).expect("root is recorded");
        assert_eq!(installation.files.len(), 2);
        let policy = installation.file("corrections/policy.toml").unwrap();
        assert_eq!(policy.bytes, "policy body".len());
        assert_eq!(policy.sha256, sha256(b"policy body"));
        assert!(decoded.installation(Path::new("/elsewhere")).is_none());
    }

    /// Two harnesses installed at different times must not clear each other's record.
    #[test]
    fn recording_one_root_leaves_every_other_root_untouched() {
        let mut manifest = SkillInstallManifest::default();
        let claude = Path::new("/home/test/.claude/skills/ai-session-search");
        let codex = Path::new("/home/test/.agents/skills/ai-session-search");
        manifest.record(claude, &files());
        manifest.record(codex, &files());
        assert_eq!(manifest.installations.len(), 2);

        manifest.record(claude, &[("SKILL.md".to_string(), "new body")]);
        assert_eq!(
            manifest.installations.len(),
            2,
            "re-recording replaces one entry, it does not append a second"
        );
        assert_eq!(manifest.installation(claude).unwrap().files.len(), 1);
        assert_eq!(
            manifest.installation(codex).unwrap().files.len(),
            2,
            "the other root's record must survive"
        );

        manifest.forget(claude);
        assert!(manifest.installation(claude).is_none());
        assert!(manifest.installation(codex).is_some());
    }

    /// Serialization must not reorder between runs, or every install produces a meaningless diff.
    #[test]
    fn installations_serialize_in_a_stable_order_regardless_of_install_order() {
        let roots = [Path::new("/b/skills/x"), Path::new("/a/skills/x")];
        let mut forward = SkillInstallManifest::default();
        for root in roots {
            forward.record(root, &files());
        }
        let mut backward = SkillInstallManifest::default();
        for root in roots.iter().rev() {
            backward.record(root, &files());
        }
        assert_eq!(forward.to_json().unwrap(), backward.to_json().unwrap());
    }

    #[test]
    fn a_missing_manifest_is_absent_and_a_damaged_one_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = manifest_path(&dir.path().join("config.toml"));
        assert_eq!(path.file_name().unwrap(), "skill-install-manifest.json");
        assert_eq!(load_manifest(&path).unwrap(), ManifestState::Absent);

        std::fs::write(&path, "{ truncated").unwrap();
        let state = load_manifest(&path).unwrap();
        assert!(
            matches!(state, ManifestState::Unreadable(_)),
            "a damaged manifest must not read as absent: {state:?}"
        );
        assert!(
            state.installation(Path::new("/anything")).is_none(),
            "an unreadable manifest proves nothing about any root"
        );

        // A future schema is refused by name rather than parsed on a guess.
        std::fs::write(&path, r#"{"schema_version": 99, "installations": []}"#).unwrap();
        match load_manifest(&path).unwrap() {
            ManifestState::Unreadable(reason) => assert!(reason.contains("99"), "{reason}"),
            other => panic!("expected an unreadable future schema, got {other:?}"),
        }
    }

    /// A write must never erase damaged ownership evidence.
    #[test]
    fn writable_manifest_rejects_damaged_ownership_evidence() {
        let state = ManifestState::Unreadable("truncated".to_string());
        let path = Path::new("/state/skill-install-manifest.json");
        let error = state
            .writable_manifest(path)
            .expect_err("a write must preserve damaged ownership evidence");
        let message = format!("{error:#}");
        assert!(
            message.contains(&path.display().to_string())
                && message.contains("truncated")
                && message.contains("repair or move"),
            "{message}"
        );

        assert_eq!(
            ManifestState::Absent
                .writable_manifest(path)
                .unwrap()
                .installations,
            Vec::new()
        );
    }
}
