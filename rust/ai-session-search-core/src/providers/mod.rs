// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

pub mod aistudio;
pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod cursor;
pub mod gemini_cli;
pub mod pi;
mod snapshot;
pub(crate) mod spawn;

use std::fs::Metadata;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::models::SourceFile;

pub(crate) struct ProviderDiscovery {
    pub(crate) sources: Vec<SourceFile>,
    pub(crate) warnings: Vec<ProviderPathWarning>,
}

pub(crate) struct ProviderPathWarning {
    pub(crate) path: PathBuf,
    pub(crate) operation: &'static str,
    pub(crate) message: String,
}

pub(crate) struct DiscoveredPath {
    pub(crate) root: PathBuf,
    pub(crate) path: PathBuf,
    pub(crate) metadata: Metadata,
}

pub(crate) struct WalkDiscovery {
    pub(crate) entries: Vec<DiscoveredPath>,
    pub(crate) warnings: Vec<ProviderPathWarning>,
}

/// Walk configured roots once, retaining readable entries and contextual non-fatal failures.
/// Missing roots are a normal empty state. `max_depth` follows `ignore::WalkBuilder` semantics.
pub(crate) fn walk_roots(roots: &[PathBuf], max_depth: Option<usize>) -> WalkDiscovery {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for root in roots {
        match std::fs::symlink_metadata(root) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(path_warning(root, "inspect_root", error));
                continue;
            }
        }
        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_exclude(false)
            .parents(false)
            .sort_by_file_path(|left, right| left.cmp(right));
        if let Some(depth) = max_depth {
            builder.max_depth(Some(depth));
        }
        for entry in builder.build() {
            match entry {
                Ok(entry) => match entry.metadata() {
                    Ok(metadata) => entries.push(DiscoveredPath {
                        root: root.clone(),
                        path: entry.into_path(),
                        metadata,
                    }),
                    Err(error) => warnings.push(path_warning(entry.path(), "read_metadata", error)),
                },
                Err(error) => warnings.push(ProviderPathWarning {
                    path: ignore_error_path(&error).unwrap_or_else(|| root.clone()),
                    operation: "traverse",
                    message: error.to_string(),
                }),
            }
        }
    }
    WalkDiscovery { entries, warnings }
}

fn path_warning(
    path: &Path,
    operation: &'static str,
    error: impl std::fmt::Display,
) -> ProviderPathWarning {
    ProviderPathWarning {
        path: path.to_path_buf(),
        operation,
        message: error.to_string(),
    }
}

fn ignore_error_path(error: &ignore::Error) -> Option<PathBuf> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path.clone()),
        ignore::Error::Loop { child, .. } => Some(child.clone()),
        ignore::Error::WithLineNumber { err, .. } | ignore::Error::WithDepth { err, .. } => {
            ignore_error_path(err)
        }
        ignore::Error::Partial(errors) => errors.iter().find_map(ignore_error_path),
        _ => None,
    }
}

pub(crate) fn malformed_jsonl_warning(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("skipped 1 malformed JSONL record".to_string()),
        count => Some(format!("skipped {count} malformed JSONL records")),
    }
}
