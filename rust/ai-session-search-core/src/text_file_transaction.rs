// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Recoverable transactions over unrelated UTF-8 text files and managed discovery symlinks.
//!
//! Cross-directory rename atomicity does not exist. This module instead writes one durable
//! receipt before the first mutation, validates every preimage immediately before publication,
//! rolls handled failures back in reverse order, and retains evidence when an external edit makes
//! automatic recovery unsafe.

use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::durable_path::EncodedPath;

use crate::durable_fs::{
    atomic_write_file, entry_exists, is_link_entry, open_existing_file_lock, open_file_lock,
    remove_owned_link, sync_parent, AtomicWriteMode,
};

const RECEIPT_VERSION: u32 = 2;
const FIRST_SUPPORTED_RECEIPT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TextFileImage {
    text: String,
}

impl TextFileImage {
    pub(crate) fn new(text: String) -> Self {
        Self { text }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn with_text(&self, text: String) -> Self {
        Self { text }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextFileChange {
    pub(crate) path: PathBuf,
    pub(crate) before: Option<TextFileImage>,
    pub(crate) after: Option<TextFileImage>,
}

impl TextFileChange {
    pub(crate) fn write(path: PathBuf, before: Option<TextFileImage>, text: String) -> Self {
        let after = Some(match &before {
            Some(image) => image.with_text(text),
            None => TextFileImage::new(text),
        });
        Self {
            path,
            before,
            after,
        }
    }

    pub(crate) fn remove(path: PathBuf, before: TextFileImage) -> Self {
        Self {
            path,
            before: Some(before),
            after: None,
        }
    }
}

#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SymlinkImage {
    Absent,
    Directory,
    Link { target: EncodedPath },
}

impl PartialEq for SymlinkImage {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Absent, Self::Absent) | (Self::Directory, Self::Directory) => true,
            (Self::Link { target: left }, Self::Link { target: right }) => {
                match (left, right) {
                    (EncodedPath::WindowsWide(left), EncodedPath::WindowsWide(right)) => {
                        // Win32 accepts either separator, but verbatim/device paths do not.
                        // Keep case, relative targets, dot components, and every other unit exact.
                        let literal = |units: &[u16]| {
                            units.starts_with(&[92, 92, 63, 92])
                                || units.starts_with(&[92, 92, 46, 92])
                        };
                        if literal(left) || literal(right) {
                            return left == right;
                        }
                        let separator = |unit: &u16| if *unit == 47 { 92 } else { *unit };
                        left.iter().map(separator).eq(right.iter().map(separator))
                    }
                    _ => left == right,
                }
            }
            _ => false,
        }
    }
}

impl SymlinkImage {
    fn link(target: &Path) -> Self {
        Self::Link {
            target: EncodedPath::from_path(target),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymlinkChange {
    pub(crate) path: PathBuf,
    pub(crate) before: SymlinkImage,
    pub(crate) after: SymlinkImage,
}

impl SymlinkChange {
    pub(crate) fn install(path: PathBuf, target: PathBuf) -> Self {
        Self {
            path,
            before: SymlinkImage::Absent,
            after: SymlinkImage::link(&target),
        }
    }

    pub(crate) fn replace_directory(path: PathBuf, target: PathBuf) -> Self {
        Self {
            path,
            before: SymlinkImage::Directory,
            after: SymlinkImage::link(&target),
        }
    }

    pub(crate) fn remove(path: PathBuf, target: PathBuf) -> Self {
        Self {
            path,
            before: SymlinkImage::link(&target),
            after: SymlinkImage::Absent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryOutcome {
    RolledBack { paths: usize },
    Finalized { paths: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionPhase {
    Prepared,
    Published,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TransactionReceipt {
    version: u32,
    phase: TransactionPhase,
    changes: Vec<ReceiptChange>,
    #[serde(default)]
    symlink_changes: Vec<ReceiptSymlinkChange>,
    /// Absent copy directories to prune after restoring file preimages, deepest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rollback_directories: Vec<EncodedPath>,
    /// Resolved directory locations at planning time. Empty only for older receipts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parent_locations: Vec<ParentLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ParentLocation {
    path: EncodedPath,
    resolved: EncodedPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReceiptChange {
    /// Absolute native mutation path, retaining the user's linked layout.
    path: EncodedPath,
    before: Option<TextFileImage>,
    after: Option<TextFileImage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReceiptSymlinkChange {
    path: EncodedPath,
    before: SymlinkImage,
    after: SymlinkImage,
}

pub(crate) fn snapshot_utf8_regular_file(path: &Path) -> Result<Option<TextFileImage>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()))
        }
    };
    if !metadata.file_type().is_file() {
        bail!(
            "expected an absent path or UTF-8 regular file, but {} is not a regular file",
            path.display()
        );
    }
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    Ok(Some(TextFileImage::new(text)))
}

pub(crate) fn publish_text_change(change: &TextFileChange) -> Result<()> {
    verify_image(&change.path, &change.before)?;
    apply_image(&change.path, &change.after)
}

pub(crate) fn publish_symlink_change(change: &SymlinkChange) -> Result<()> {
    verify_symlink_image(&change.path, &change.before)?;
    apply_symlink_change_after(change)
}

pub(crate) fn apply_symlink_change_after(change: &SymlinkChange) -> Result<()> {
    apply_symlink_image(&change.path, &change.after, &change.before)
}

#[cfg(test)]
pub(crate) fn execute_text_file_transaction(
    receipt_path: &Path,
    changes: &[TextFileChange],
) -> Result<()> {
    execute_text_file_transaction_with(receipt_path, changes, |_, change| {
        publish_text_change(change)
    })
}

pub(crate) fn execute_text_file_and_symlink_transaction_with<F>(
    receipt_path: &Path,
    changes: &[TextFileChange],
    symlink_changes: &[SymlinkChange],
    publish_symlink: F,
) -> Result<()>
where
    F: FnMut(usize, &SymlinkChange) -> Result<()>,
{
    execute_transaction_with(
        receipt_path,
        changes,
        symlink_changes,
        |_, change| publish_text_change(change),
        publish_symlink,
    )
}

/// Runs a read-only snapshot while excluding transaction writers for the same receipt.
///
/// The shared guard remains alive for the entire callback so callers cannot combine a receipt
/// from one transaction generation with target files from another.
pub(crate) fn with_text_file_transaction_read_lock<T>(
    receipt_path: &Path,
    mut read_snapshot: impl FnMut() -> Result<T>,
) -> Result<T> {
    let lock_path = lock_path(receipt_path);
    let existing_lock = open_existing_file_lock(&lock_path).with_context(|| {
        format!(
            "failed to open integration transaction lock {}",
            lock_path.display()
        )
    })?;
    if let Some(lock) = existing_lock {
        let _guard = lock.read().with_context(|| {
            format!(
                "failed to read-lock integration transaction {}",
                lock_path.display()
            )
        })?;
        return read_snapshot();
    }

    // No transaction has created the durable lock yet. Read without mutating the directory, then
    // recheck: if a writer started during the snapshot it has created the lock before changing any
    // target, so waiting and repeating under that lock produces one consistent generation.
    let unlocked_snapshot = read_snapshot()?;
    let appeared_lock = open_existing_file_lock(&lock_path).with_context(|| {
        format!(
            "failed to recheck integration transaction lock {}",
            lock_path.display()
        )
    })?;
    let Some(lock) = appeared_lock else {
        return Ok(unlocked_snapshot);
    };
    let _guard = lock.read().with_context(|| {
        format!(
            "failed to read-lock integration transaction {} after it appeared during status",
            lock_path.display()
        )
    })?;
    read_snapshot()
}

#[cfg(test)]
fn execute_text_file_transaction_with<F>(
    receipt_path: &Path,
    changes: &[TextFileChange],
    publish: F,
) -> Result<()>
where
    F: FnMut(usize, &TextFileChange) -> Result<()>,
{
    execute_transaction_with(receipt_path, changes, &[], publish, |_, _| {
        unreachable!("a text-only transaction has no symlink changes")
    })
}

fn execute_transaction_with<F, G>(
    receipt_path: &Path,
    changes: &[TextFileChange],
    symlink_changes: &[SymlinkChange],
    publish_text: F,
    publish_symlink: G,
) -> Result<()>
where
    F: FnMut(usize, &TextFileChange) -> Result<()>,
    G: FnMut(usize, &SymlinkChange) -> Result<()>,
{
    execute_transaction_with_directories(
        receipt_path,
        changes,
        symlink_changes,
        &[],
        publish_text,
        publish_symlink,
    )
}

/// Record parent locations for every transaction; copy-directory pruning evidence is optional.
pub(crate) fn execute_transaction_with_directories<F, G>(
    receipt_path: &Path,
    changes: &[TextFileChange],
    symlink_changes: &[SymlinkChange],
    rollback_directories: &[PathBuf],
    mut publish_text: F,
    mut publish_symlink: G,
) -> Result<()>
where
    F: FnMut(usize, &TextFileChange) -> Result<()>,
    G: FnMut(usize, &SymlinkChange) -> Result<()>,
{
    let changes = changes
        .iter()
        .cloned()
        .map(absolutize_change)
        .collect::<Result<Vec<_>>>()?;
    let symlink_changes = symlink_changes
        .iter()
        .cloned()
        .map(absolutize_symlink_change)
        .collect::<Result<Vec<_>>>()?;
    if changes.is_empty() && symlink_changes.is_empty() {
        return Ok(());
    }
    validate_changes(&changes, &symlink_changes)?;
    validate_control_paths(receipt_path, &changes, &symlink_changes)?;
    let parent_locations = record_parent_locations(
        receipt_path,
        &changes,
        &symlink_changes,
        rollback_directories,
    )?;
    let lock_path = lock_path(receipt_path);
    let mut lock = open_file_lock(&lock_path).with_context(|| {
        format!(
            "failed to open integration transaction lock {}",
            lock_path.display()
        )
    })?;
    let _guard = lock
        .try_write()
        .map_err(|error| integration_lock_error(error, &lock_path))?;
    if entry_exists(receipt_path)? {
        bail!(
            "pending integration receipt requires recovery: {}; {}",
            receipt_path.display(),
            recovery_guidance(receipt_path)
        );
    }
    for change in &changes {
        verify_image(&change.path, &change.before)?;
    }
    for change in &symlink_changes {
        verify_symlink_image(&change.path, &change.before)?;
    }

    let mut directories = Vec::new();
    for path in rollback_directories {
        let path = std::path::absolute(path)?;
        if !entry_exists(&path)? {
            if !changes.iter().any(|change| change.path.starts_with(&path)) {
                bail!(
                    "rollback directory {} contains no transaction file; refusing it",
                    path.display()
                );
            }
            directories.push(path);
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories.dedup();
    let mut receipt = TransactionReceipt {
        parent_locations,
        rollback_directories: directories
            .iter()
            .map(|path| EncodedPath::from_path(path))
            .collect(),
        version: RECEIPT_VERSION,
        phase: TransactionPhase::Prepared,
        changes: changes.iter().map(ReceiptChange::from).collect(),
        symlink_changes: symlink_changes
            .iter()
            .map(ReceiptSymlinkChange::from)
            .collect(),
    };
    write_receipt(receipt_path, &receipt, AtomicWriteMode::CreateNew)?;
    verify_receipt_ancestors(&receipt)?;

    for (index, change) in changes.iter().enumerate() {
        verify_target_parent(&receipt, &change.path)?;
        if let Err(publish_error) = publish_text(index, change) {
            return rollback_after_failure(receipt_path, &receipt, publish_error);
        }
    }
    for (index, change) in symlink_changes.iter().enumerate() {
        verify_target_parent(&receipt, &change.path)?;
        if let Err(publish_error) = publish_symlink(index, change) {
            return rollback_after_failure(receipt_path, &receipt, publish_error);
        }
    }

    verify_receipt_ancestors(&receipt)?;
    receipt.phase = TransactionPhase::Published;
    if let Err(phase_error) = write_receipt(receipt_path, &receipt, AtomicWriteMode::Replace) {
        return match load_receipt(receipt_path) {
            Ok(on_disk) if on_disk.phase == TransactionPhase::Published => Err(anyhow!(
                "integration changes and the published receipt are complete, but durability confirmation failed: {phase_error:#}; {} to verify and finalize",
                recovery_guidance(receipt_path)
            )),
            Ok(on_disk) if on_disk.phase == TransactionPhase::Prepared => {
                rollback_after_failure(receipt_path, &on_disk, phase_error)
            }
            Ok(_) => Err(anyhow!(
                "integration receipt has an unknown phase after publication failure; preserved evidence at {}",
                receipt_path.display()
            )),
            Err(load_error) => Err(anyhow!(
                "failed to publish the final integration receipt: {phase_error:#}; the receipt is unreadable: {load_error:#}; current files were not changed again and evidence was preserved at {}",
                receipt_path.display()
            )),
        };
    }
    match remove_receipt(receipt_path) {
        Ok(()) => Ok(()),
        Err(error) if entry_exists(receipt_path).unwrap_or(true) => Err(error).context(format!(
            "integration changes are complete; {} to verify and finalize",
            recovery_guidance(receipt_path)
        )),
        Err(error) => Err(error).context(
            "integration changes are complete and the receipt is absent, but its parent-directory sync failed",
        ),
    }
}

pub(crate) fn transaction_recovery_required(receipt_path: &Path) -> Result<bool> {
    entry_exists(receipt_path).with_context(|| {
        format!(
            "failed to inspect integration receipt {}",
            receipt_path.display()
        )
    })
}

pub(crate) fn recovery_guidance(receipt_path: &Path) -> String {
    format!(
        "invoke `aise` with argv [`integrations`, `recover`, `--transaction-receipt`, `<RECEIPT_PATH>`], where `<RECEIPT_PATH>` is {}",
        receipt_path.display()
    )
}

pub(crate) fn recover_text_file_transaction(receipt_path: &Path) -> Result<RecoveryOutcome> {
    if !transaction_recovery_required(receipt_path)? {
        bail!(
            "no integration recovery receipt exists at {}",
            receipt_path.display()
        );
    }
    verify_receipt_ancestors(&load_receipt(receipt_path)?)?;
    let lock_path = lock_path(receipt_path);
    let mut lock = open_file_lock(&lock_path).with_context(|| {
        format!(
            "failed to open integration transaction lock {}",
            lock_path.display()
        )
    })?;
    let _guard = lock
        .try_write()
        .map_err(|error| integration_lock_error(error, &lock_path))?;
    let receipt = load_receipt(receipt_path)?;
    verify_receipt_ancestors(&receipt)?;
    let outcome = match receipt.phase {
        TransactionPhase::Prepared => RecoveryOutcome::RolledBack {
            paths: rollback_prepared(&receipt)?,
        },
        TransactionPhase::Published => {
            for change in &receipt.changes {
                let path = change.path.to_path_buf()?;
                verify_image(&path, &change.after).with_context(|| {
                    format!(
                        "published integration transaction conflicts with current file {}; refusing to discard recovery evidence",
                        path.display()
                    )
                })?;
            }
            for change in &receipt.symlink_changes {
                let path = change.path.to_path_buf()?;
                verify_symlink_image(&path, &change.after).with_context(|| {
                    format!(
                        "published integration transaction conflicts with current symlink {}; refusing to discard recovery evidence",
                        path.display()
                    )
                })?;
            }
            RecoveryOutcome::Finalized {
                paths: receipt.changes.len() + receipt.symlink_changes.len(),
            }
        }
    };
    remove_receipt(receipt_path)?;
    Ok(outcome)
}

fn validate_changes(changes: &[TextFileChange], symlink_changes: &[SymlinkChange]) -> Result<()> {
    let mut paths = std::collections::HashSet::new();
    for change in changes {
        if change.before == change.after {
            bail!(
                "text file transaction contains a no-op for {}",
                change.path.display()
            );
        }
        if !paths.insert(change.path.clone()) {
            bail!(
                "text file transaction contains duplicate path {}",
                change.path.display()
            );
        }
    }
    for change in symlink_changes {
        if change.before == change.after {
            bail!(
                "symlink transaction contains a no-op for {}",
                change.path.display()
            );
        }
        if !paths.insert(change.path.clone()) {
            bail!(
                "integration transaction contains duplicate path {}",
                change.path.display()
            );
        }
    }
    Ok(())
}

fn validate_control_paths(
    receipt_path: &Path,
    changes: &[TextFileChange],
    symlink_changes: &[SymlinkChange],
) -> Result<()> {
    let receipt_path = normalize_transaction_path(receipt_path)?;
    let control_paths = [
        ("transaction receipt", receipt_path.clone()),
        ("adjacent transaction lock", lock_path(&receipt_path)),
    ];
    for (control_role, control_path) in control_paths {
        let target = changes
            .iter()
            .map(|change| &change.path)
            .chain(symlink_changes.iter().map(|change| &change.path))
            .find(|path| normalize_transaction_path(path).is_ok_and(|path| path == control_path));
        if let Some(target) = target {
            bail!(
                "integration transaction control-path conflict: {control_role} path {} overlaps mutation target path {}; choose a transaction receipt whose receipt and adjacent lock paths are outside every mutation target",
                control_path.display(),
                target.display()
            );
        }
    }
    Ok(())
}

fn absolutize_transaction_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("failed to resolve current directory for integration transaction paths")?
            .join(path))
    }
}

fn normalize_transaction_path(path: &Path) -> Result<PathBuf> {
    let absolute = absolutize_transaction_path(path)?;
    Ok(crate::util::normalize_absolute_path(&absolute))
}

fn absolutize_change(mut change: TextFileChange) -> Result<TextFileChange> {
    change.path = absolutize_transaction_path(&change.path)?;
    Ok(change)
}

fn absolutize_symlink_change(mut change: SymlinkChange) -> Result<SymlinkChange> {
    change.path = absolutize_transaction_path(&change.path)?;
    Ok(change)
}

/// Canonicalize the deepest existing directory and retain any not-yet-created components.
/// The same resolver is used for planning and validation, including Windows verbatim paths.
fn resolve_directory_location_with(
    path: &Path,
    mut canonicalize: impl FnMut(&Path) -> std::io::Result<PathBuf>,
) -> Result<PathBuf> {
    let mut existing = absolutize_transaction_path(path)?;
    let mut missing = Vec::new();
    loop {
        match canonicalize(&existing) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(crate::util::normalize_absolute_path(&resolved));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let component = existing
                    .components()
                    .next_back()
                    .ok_or_else(|| anyhow!("cannot resolve directory location {}", path.display()))?
                    .as_os_str()
                    .to_os_string();
                if !existing.pop() {
                    return Err(error)
                        .with_context(|| format!("resolve directory location {}", path.display()));
                }
                missing.push(component);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("resolve directory location {}", path.display()))
            }
        }
    }
}

fn resolve_directory_location(path: &Path) -> Result<PathBuf> {
    resolve_directory_location_with(path, |path| fs::canonicalize(path))
}

fn record_parent_locations(
    receipt: &Path,
    changes: &[TextFileChange],
    links: &[SymlinkChange],
    rollback_directories: &[PathBuf],
) -> Result<Vec<ParentLocation>> {
    let mut directories = std::collections::BTreeSet::new();
    for path in std::iter::once(receipt)
        .chain(changes.iter().map(|change| change.path.as_path()))
        .chain(links.iter().map(|change| change.path.as_path()))
    {
        let absolute = absolutize_transaction_path(path)?;
        if let Some(parent) = absolute.parent() {
            directories.insert(parent.to_path_buf());
        }
    }
    for path in rollback_directories {
        directories.insert(absolutize_transaction_path(path)?);
    }
    directories
        .into_iter()
        .map(|path| {
            Ok(ParentLocation {
                resolved: EncodedPath::from_path(&resolve_directory_location(&path)?),
                path: EncodedPath::from_path(&path),
            })
        })
        .collect()
}

fn verify_parent_location(receipt: &TransactionReceipt, location: &ParentLocation) -> Result<()> {
    let path = location.path.to_path_buf()?;
    // A receipt-authorized legacy copy migration is reversed before its files are restored.
    if receipt.symlink_changes.iter().any(|change| {
        matches!(change.before, SymlinkImage::Directory)
            && matches!(change.after, SymlinkImage::Link { .. })
            && change
                .path
                .to_path_buf()
                .is_ok_and(|link| path.starts_with(link))
            && change.path.to_path_buf().is_ok_and(|link| {
                snapshot_symlink_image(&link).is_ok_and(|current| current == change.after)
            })
    }) {
        return Ok(());
    }
    let expected = location.resolved.to_path_buf()?;
    let current = resolve_directory_location(&path)
        .with_context(|| format!("cannot verify recorded parent {}; restore the original directory or link target; if a receipt exists, run `aise integrations recover` with the same --transaction-receipt, otherwise retry the original command", path.display()))?;
    if current != expected {
        bail!("transaction parent {} changed location: recorded {}, now {}. Stopped before further file changes; kept any existing receipt. Restore the original directory or link target; if a receipt exists, run `aise integrations recover` with the same --transaction-receipt, otherwise retry the original command", path.display(), expected.display(), current.display());
    }
    Ok(())
}

fn verify_receipt_ancestors(receipt: &TransactionReceipt) -> Result<()> {
    // Older receipts lack location evidence and retain their original image-only recovery.
    for location in &receipt.parent_locations {
        verify_parent_location(receipt, location)?;
    }
    Ok(())
}

fn verify_target_parent(receipt: &TransactionReceipt, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        for location in &receipt.parent_locations {
            if location.path.to_path_buf()? == parent {
                verify_parent_location(receipt, location)?;
            }
        }
    }
    Ok(())
}

fn rollback_prepared(receipt: &TransactionReceipt) -> Result<usize> {
    // Validate the entire plan before restoring even one unrelated file or link.
    verify_receipt_ancestors(receipt)?;
    let mut restored = 0;
    let mut conflicts = Vec::new();
    for change in receipt.symlink_changes.iter().rev() {
        let path = change.path.to_path_buf()?;
        let current = snapshot_symlink_image(&path)?;
        if current == change.before {
            continue;
        }
        let removed_directory_before_link =
            matches!(&change.before, SymlinkImage::Directory) && current == SymlinkImage::Absent;
        if current == change.after || removed_directory_before_link {
            verify_target_parent(receipt, &path)?;
            if let Err(error) = apply_symlink_image(&path, &change.before, &current) {
                conflicts.push(format!("{}: {error:#}", path.display()));
            } else {
                restored += 1;
            }
        } else {
            conflicts.push(format!(
                "{}: entry differs from both receipt images",
                path.display()
            ));
        }
    }
    for change in receipt.changes.iter().rev() {
        let path = change.path.to_path_buf()?;
        let current = snapshot_utf8_regular_file(&path)?;
        if current == change.before {
            continue;
        }
        if current == change.after {
            verify_target_parent(receipt, &path)?;
            if let Err(error) = apply_image(&path, &change.before) {
                conflicts.push(format!("{}: {error:#}", path.display()));
            } else {
                restored += 1;
            }
        } else {
            conflicts.push(format!(
                "{}: content differs from both receipt images",
                path.display()
            ));
        }
    }
    if conflicts.is_empty() {
        for directory in &receipt.rollback_directories {
            let path = directory.to_path_buf()?;
            if let Some(location) = receipt.parent_locations.iter().find(|location| {
                location
                    .path
                    .to_path_buf()
                    .is_ok_and(|recorded| recorded == path)
            }) {
                verify_parent_location(receipt, location)?;
            }
            // Never follow a replacement link, and never remove a directory containing user data.
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Ok(metadata) if metadata.is_dir() && !is_link_entry(&metadata) => {}
                _ => continue,
            }
            match fs::remove_dir(&path) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("prune rolled-back copy directory {}", path.display())
                    })
                }
            }
        }
        Ok(restored)
    } else {
        bail!("{}", conflicts.join("; "))
    }
}

fn rollback_after_failure(
    receipt_path: &Path,
    receipt: &TransactionReceipt,
    failure: anyhow::Error,
) -> Result<()> {
    match rollback_prepared(receipt) {
        Ok(paths) => {
            remove_receipt(receipt_path)?;
            Err(failure).context(format!(
                "integration publication failed; restored {paths} changed path(s)"
            ))
        }
        Err(rollback_error) => Err(anyhow!(
            "integration publication failed: {failure:#}; automatic rollback was incomplete: {rollback_error:#}; recovery receipt preserved at {}; {} after resolving concurrent edits",
            receipt_path.display(),
            recovery_guidance(receipt_path)
        )),
    }
}

fn verify_image(path: &Path, expected: &Option<TextFileImage>) -> Result<()> {
    let current = snapshot_utf8_regular_file(path)?;
    if &current == expected {
        Ok(())
    } else {
        bail!(
            "{} changed after MCP preflight; refusing to overwrite concurrent edits",
            path.display()
        )
    }
}

pub(crate) fn snapshot_symlink_image(path: &Path) -> Result<SymlinkImage> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(SymlinkImage::Absent),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()))
        }
    };
    if is_link_entry(&metadata) {
        let target = fs::read_link(path)
            .with_context(|| format!("failed to read symlink {}", path.display()))?;
        return Ok(SymlinkImage::link(&target));
    }
    if metadata.is_dir() {
        return Ok(SymlinkImage::Directory);
    }
    bail!(
        "expected an absent path, directory, or symbolic link, but {} is another entry type",
        path.display()
    )
}

fn verify_symlink_image(path: &Path, expected: &SymlinkImage) -> Result<()> {
    let current = snapshot_symlink_image(path)?;
    if &current == expected {
        Ok(())
    } else {
        bail!(
            "{} changed after discovery-link preflight; refusing to overwrite concurrent edits",
            path.display()
        )
    }
}

fn apply_symlink_image(path: &Path, image: &SymlinkImage, previous: &SymlinkImage) -> Result<()> {
    let remove_link = || -> Result<()> {
        let SymlinkImage::Link { target } = previous else {
            bail!(
                "no link ownership evidence for {}; preserved entry",
                path.display()
            );
        };
        remove_owned_link(path, &target.to_path_buf()?)
            .with_context(|| format!("failed to remove owned link {}", path.display()))
    };
    match image {
        SymlinkImage::Absent => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect {}", path.display()))
            }
            Ok(metadata) if is_link_entry(&metadata) => {
                remove_link()?;
                Ok(sync_parent(path)?)
            }
            Ok(_) => bail!("refusing to remove non-symlink entry {}", path.display()),
        },
        SymlinkImage::Directory => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                fs::create_dir_all(path)
                    .with_context(|| format!("failed to restore directory {}", path.display()))?;
                Ok(sync_parent(path)?)
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect {}", path.display()))
            }
            Ok(metadata) if is_link_entry(&metadata) => {
                remove_link()?;
                fs::create_dir_all(path)
                    .with_context(|| format!("failed to restore directory {}", path.display()))?;
                Ok(sync_parent(path)?)
            }
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => bail!("refusing to replace non-directory entry {}", path.display()),
        },
        SymlinkImage::Link { target } => {
            let target = target.to_path_buf()?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create symlink directory {}", parent.display())
                })?;
            }
            create_directory_symlink(&target, path).with_context(|| {
                format!(
                    "failed to link discovery entry {} to {}",
                    path.display(),
                    target.display()
                )
            })?;
            Ok(sync_parent(path)?)
        }
    }
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

fn apply_image(path: &Path, image: &Option<TextFileImage>) -> Result<()> {
    match image {
        Some(image) => atomic_write_file(
            path,
            image.text.as_bytes(),
            if entry_exists(path)? {
                AtomicWriteMode::Replace
            } else {
                AtomicWriteMode::CreateNew
            },
        ),
        None => {
            fs::remove_file(path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            sync_parent(path).with_context(|| {
                format!(
                    "removed {} but failed to sync its parent directory",
                    path.display()
                )
            })
        }
    }
}

fn write_receipt(path: &Path, receipt: &TransactionReceipt, mode: AtomicWriteMode) -> Result<()> {
    verify_receipt_ancestors(receipt)?;
    let content = serde_json::to_vec_pretty(receipt)?;
    atomic_write_file(path, &content, mode).with_context(|| {
        format!(
            "failed to publish integration transaction receipt {}",
            path.display()
        )
    })
}

fn load_receipt(path: &Path) -> Result<TransactionReceipt> {
    let image = snapshot_utf8_regular_file(path)?.ok_or_else(|| {
        anyhow!(
            "no integration recovery receipt exists at {}",
            path.display()
        )
    })?;
    let receipt: TransactionReceipt = serde_json::from_str(image.text()).with_context(|| {
        format!(
            "failed to parse integration transaction receipt {}",
            path.display()
        )
    })?;
    if !(FIRST_SUPPORTED_RECEIPT_VERSION..=RECEIPT_VERSION).contains(&receipt.version) {
        bail!(
            "unsupported integration transaction receipt version {} in {} (expected {} through {})",
            receipt.version,
            path.display(),
            FIRST_SUPPORTED_RECEIPT_VERSION,
            RECEIPT_VERSION
        );
    }
    Ok(receipt)
}

fn remove_receipt(path: &Path) -> Result<()> {
    fs::remove_file(path)
        .with_context(|| format!("failed to remove receipt {}", path.display()))?;
    sync_parent(path).with_context(|| {
        format!(
            "removed receipt {} but failed to sync its parent",
            path.display()
        )
    })
}

/// Another process holds the integration transaction lock. Nothing was changed, and the same
/// transaction can simply be tried again later, so callers that run unasked, such as the
/// post-upgrade refresh, defer on exactly this error rather than reporting it.
#[derive(Debug, thiserror::Error)]
#[error("another integration transaction holds {}", .0.display())]
pub(crate) struct IntegrationLockHeld(PathBuf);

fn integration_lock_error(error: std::io::Error, lock_path: &Path) -> anyhow::Error {
    if error.kind() == ErrorKind::WouldBlock {
        IntegrationLockHeld(lock_path.to_path_buf()).into()
    } else {
        anyhow::Error::new(error).context(format!(
            "failed to lock integration transactions at {}",
            lock_path.display()
        ))
    }
}

pub(crate) fn lock_path(receipt_path: &Path) -> PathBuf {
    let mut name = receipt_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("mcp-transaction"))
        .to_os_string();
    name.push(".lock");
    receipt_path.with_file_name(name)
}

impl From<&TextFileChange> for ReceiptChange {
    fn from(change: &TextFileChange) -> Self {
        Self {
            path: EncodedPath::from_path(&change.path),
            before: change.before.clone(),
            after: change.after.clone(),
        }
    }
}

impl From<&SymlinkChange> for ReceiptSymlinkChange {
    fn from(change: &SymlinkChange) -> Self {
        Self {
            path: EncodedPath::from_path(&change.path),
            before: change.before.clone(),
            after: change.after.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    const CRASH_HELPER_ROOT: &str = "AI_SESSION_SEARCH_TRANSACTION_CRASH_HELPER_ROOT";

    fn change(path: &Path, before: Option<&str>, after: Option<&str>) -> TextFileChange {
        TextFileChange {
            path: path.to_path_buf(),
            before: before.map(|text| TextFileImage::new(text.to_string())),
            after: after.map(|text| TextFileImage::new(text.to_string())),
        }
    }

    const COPY_REPLACEMENT_ROOT: &str = "AI_SESSION_SEARCH_COPY_REPLACEMENT_TEST_ROOT";

    fn copy_replacement_fixture(
        root: &Path,
        existing: bool,
        nested: bool,
    ) -> (Vec<TextFileChange>, PathBuf) {
        let copy = root.join("copy");
        let replaced = if nested {
            copy.join("references")
        } else {
            copy
        };
        fs::create_dir(root.join("external")).unwrap();
        fs::write(root.join("external/SKILL.md"), "installed bytes").unwrap();
        fs::write(root.join("other.txt"), "before").unwrap();
        if existing {
            fs::create_dir_all(&replaced).unwrap();
        }
        (
            vec![
                change(&root.join("other.txt"), Some("before"), Some("after")),
                change(&replaced.join("SKILL.md"), None, Some("installed bytes")),
            ],
            replaced,
        )
    }

    fn create_fixture_directory_link(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let output = Command::new("cmd.exe")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
    }

    fn replace_copy_ancestor(root: &Path, replaced: &Path) {
        fs::rename(replaced, root.join("moved-copy")).unwrap();
        create_fixture_directory_link(&root.join("external"), replaced);
    }

    fn assert_copy_replacement_preserved(root: &Path, error: &anyhow::Error, replaced: &Path) {
        assert_eq!(
            fs::read_to_string(root.join("external/SKILL.md")).unwrap(),
            "installed bytes"
        );
        assert_eq!(
            fs::read_to_string(root.join("moved-copy/SKILL.md")).unwrap(),
            "installed bytes"
        );
        assert_eq!(fs::read_to_string(root.join("other.txt")).unwrap(), "after");
        assert!(root.join("receipt.json").is_file());
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains(&replaced.display().to_string()),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("changed location") && diagnostic.contains("recover"),
            "{diagnostic}"
        );
        // Follow the diagnostic: restore the original directory, then recover the same receipt.
        crate::durable_fs::remove_owned_link(replaced, &fs::read_link(replaced).unwrap()).unwrap();
        fs::rename(root.join("moved-copy"), replaced).unwrap();
        assert_eq!(
            recover_text_file_transaction(&root.join("receipt.json")).unwrap(),
            RecoveryOutcome::RolledBack { paths: 2 }
        );
        assert_eq!(
            fs::read_to_string(root.join("external/SKILL.md")).unwrap(),
            "installed bytes"
        );
        assert_eq!(
            fs::read_to_string(root.join("other.txt")).unwrap(),
            "before"
        );
        assert!(!replaced.join("SKILL.md").exists());
        assert!(!root.join("receipt.json").exists());
    }

    #[test]
    fn copy_ancestor_replacement_failure_preserves_foreign_files_and_receipt() {
        for existing in [false, true] {
            for nested in [false, true] {
                let dir = tempdir().unwrap();
                let (changes, replaced) = copy_replacement_fixture(dir.path(), existing, nested);
                let mut prepared = Vec::new();
                let error = execute_transaction_with_directories(
                    &dir.path().join("receipt.json"),
                    &changes,
                    &[],
                    &[dir.path().join("copy"), replaced.clone()],
                    |index, change| {
                        publish_text_change(change)?;
                        if index == 1 {
                            prepared = fs::read(dir.path().join("receipt.json")).unwrap();
                            replace_copy_ancestor(dir.path(), &replaced);
                            bail!("injected publication failure after ancestor replacement");
                        }
                        Ok(())
                    },
                    |_, _| unreachable!(),
                )
                .unwrap_err();
                assert_eq!(fs::read(dir.path().join("receipt.json")).unwrap(), prepared);
                assert_copy_replacement_preserved(dir.path(), &error, &replaced);
            }
        }
    }

    #[test]
    fn copy_ancestor_replacement_process_exit_recovery_preserves_foreign_files_and_receipt() {
        for existing in [false, true] {
            for nested in [false, true] {
                let dir = tempdir().unwrap();
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "text_file_transaction::tests::copy_ancestor_replacement_crash_helper",
                        "--ignored",
                    ])
                    .env(COPY_REPLACEMENT_ROOT, dir.path())
                    .env(
                        "AI_SESSION_SEARCH_COPY_TEST_EXISTING",
                        if existing { "1" } else { "0" },
                    )
                    .env(
                        "AI_SESSION_SEARCH_COPY_TEST_NESTED",
                        if nested { "1" } else { "0" },
                    )
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(73), "{output:?}");
                let replaced = if nested {
                    dir.path().join("copy/references")
                } else {
                    dir.path().join("copy")
                };
                let prepared = fs::read(dir.path().join("receipt.json")).unwrap();
                let error =
                    recover_text_file_transaction(&dir.path().join("receipt.json")).unwrap_err();
                assert_eq!(fs::read(dir.path().join("receipt.json")).unwrap(), prepared);
                assert_copy_replacement_preserved(dir.path(), &error, &replaced);
            }
        }
    }

    #[test]
    fn retargeted_preexisting_link_preserves_foreign_files_and_receipt() {
        let dir = tempdir().unwrap();
        let (changes, copy) = copy_replacement_fixture(dir.path(), true, false);
        fs::rename(&copy, dir.path().join("owned")).unwrap();
        create_fixture_directory_link(&dir.path().join("owned"), &copy);
        let error = execute_text_file_transaction_with(
            &dir.path().join("receipt.json"),
            &changes,
            |index, change| {
                publish_text_change(change)?;
                if index == 1 {
                    replace_copy_ancestor(dir.path(), &copy);
                    bail!("injected failure after retargeting an existing link");
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_copy_replacement_preserved(dir.path(), &error, &copy);
    }

    #[test]
    fn existing_linked_parent_supports_rollback_and_new_and_legacy_receipt_recovery() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        let linked = dir.path().join("linked");
        fs::create_dir(&real).unwrap();
        create_fixture_directory_link(&real, &linked);
        let target = linked.join("config.json");
        let receipt_path = dir.path().join("receipt.json");
        fs::write(&target, "before").unwrap();
        let planned = change(&target, Some("before"), Some("after"));
        let error = execute_text_file_transaction_with(
            &receipt_path,
            std::slice::from_ref(&planned),
            |_, change| {
                publish_text_change(change)?;
                bail!("injected failure with unchanged linked layout");
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("restored 1 changed path"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "before");
        assert!(!receipt_path.exists());
        for (version, record_locations) in [
            (1, false),
            (RECEIPT_VERSION, false),
            (RECEIPT_VERSION, true),
        ] {
            let receipt = TransactionReceipt {
                version,
                phase: TransactionPhase::Prepared,
                changes: vec![ReceiptChange::from(&planned)],
                symlink_changes: Vec::new(),
                rollback_directories: Vec::new(),
                parent_locations: if record_locations {
                    record_parent_locations(&receipt_path, std::slice::from_ref(&planned), &[], &[])
                        .unwrap()
                } else {
                    Vec::new()
                },
            };
            write_receipt(&receipt_path, &receipt, AtomicWriteMode::CreateNew).unwrap();
            let json: serde_json::Value =
                serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
            assert_eq!(json.get("parent_locations").is_some(), record_locations);
            publish_text_change(&planned).unwrap();
            recover_text_file_transaction(&receipt_path).unwrap();
            assert_eq!(fs::read_to_string(&target).unwrap(), "before");
            assert!(is_link_entry(&fs::symlink_metadata(&linked).unwrap()));
            assert!(!receipt_path.exists());
        }
    }

    #[test]
    fn directory_location_resolution_uses_the_same_canonical_reading_before_and_after_creation() {
        let dir = tempdir().unwrap();
        let short = dir.path().join("SHORTN~1");
        let long = dir.path().join("Long Name");
        let canonical = dir.path().join("canonical");
        let planned = resolve_directory_location_with(&short.join("new/child"), |path| {
            if path == short {
                Ok(canonical.clone())
            } else {
                Err(std::io::Error::from(ErrorKind::NotFound))
            }
        })
        .unwrap();
        let current = resolve_directory_location_with(&long.join("new/child"), |_| {
            Ok(canonical.join("new/child"))
        })
        .unwrap();
        assert_eq!(planned, current);
        let retargeted = resolve_directory_location_with(&short.join("new/child"), |_| {
            Ok(dir.path().join("foreign/new/child"))
        })
        .unwrap();
        assert_ne!(planned, retargeted);
    }

    #[test]
    fn prepared_legacy_directory_to_link_migration_restores_real_directory_before_files() {
        let dir = tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        fs::create_dir(&legacy).unwrap();
        fs::create_dir(dir.path().join("external")).unwrap();
        fs::write(dir.path().join("external/SKILL.md"), "canonical bytes").unwrap();
        let parent_locations = vec![ParentLocation {
            path: EncodedPath::from_path(&legacy),
            resolved: EncodedPath::from_path(&fs::canonicalize(&legacy).unwrap()),
        }];
        replace_copy_ancestor(dir.path(), &legacy);
        let receipt = TransactionReceipt {
            parent_locations,
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Prepared,
            changes: vec![ReceiptChange::from(&change(
                &legacy.join("SKILL.md"),
                Some("legacy bytes"),
                None,
            ))],
            symlink_changes: vec![ReceiptSymlinkChange {
                path: EncodedPath::from_path(&legacy),
                before: SymlinkImage::Directory,
                after: snapshot_symlink_image(&legacy).unwrap(),
            }],
            rollback_directories: Vec::new(),
        };
        let path = dir.path().join("receipt.json");
        write_receipt(&path, &receipt, AtomicWriteMode::CreateNew).unwrap();
        recover_text_file_transaction(&path).unwrap();
        assert!(!is_link_entry(&fs::symlink_metadata(&legacy).unwrap()));
        assert_eq!(
            fs::read_to_string(legacy.join("SKILL.md")).unwrap(),
            "legacy bytes"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("external/SKILL.md")).unwrap(),
            "canonical bytes"
        );
        assert!(!path.exists());
    }

    #[test]
    fn publication_stops_before_entering_a_replaced_ancestor() {
        let dir = tempdir().unwrap();
        let (changes, replaced) = copy_replacement_fixture(dir.path(), true, true);
        let mut calls = 0;
        let error = execute_text_file_transaction_with(
            &dir.path().join("receipt.json"),
            &changes,
            |index, change| {
                calls += 1;
                publish_text_change(change)?;
                if index == 0 {
                    replace_copy_ancestor(dir.path(), &replaced);
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("external/SKILL.md")).unwrap(),
            "installed bytes"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("other.txt")).unwrap(),
            "after"
        );
        assert!(dir.path().join("receipt.json").is_file());
        assert!(format!("{error:#}").contains("changed location"));
    }

    #[test]
    fn ordinary_text_transaction_rollback_rejects_replaced_package_config_and_instruction_parents()
    {
        for name in ["SKILL.md", "config.json", "AGENTS.md"] {
            let dir = tempdir().unwrap();
            let managed = dir.path().join("managed");
            let external = dir.path().join("external");
            fs::create_dir(&managed).unwrap();
            fs::create_dir(&external).unwrap();
            fs::write(external.join(name), "installed bytes").unwrap();
            let planned = change(&managed.join(name), None, Some("installed bytes"));
            let error = execute_text_file_transaction_with(
                &dir.path().join("receipt.json"),
                &[planned],
                |_, change| {
                    publish_text_change(change)?;
                    replace_copy_ancestor(dir.path(), &managed);
                    bail!("injected ordinary-file publication failure");
                },
            )
            .unwrap_err();
            assert_eq!(
                fs::read_to_string(external.join(name)).unwrap(),
                "installed bytes"
            );
            assert_eq!(
                fs::read_to_string(dir.path().join("moved-copy").join(name)).unwrap(),
                "installed bytes"
            );
            assert!(dir.path().join("receipt.json").exists());
            assert!(format!("{error:#}").contains("changed location"));
        }
    }

    #[test]
    #[ignore = "subprocess helper that exits after replacing a published file ancestor"]
    fn copy_ancestor_replacement_crash_helper() {
        let Some(root) = std::env::var_os(COPY_REPLACEMENT_ROOT).map(PathBuf::from) else {
            return;
        };
        let (changes, replaced) = copy_replacement_fixture(
            &root,
            std::env::var("AI_SESSION_SEARCH_COPY_TEST_EXISTING").unwrap() == "1",
            std::env::var("AI_SESSION_SEARCH_COPY_TEST_NESTED").unwrap() == "1",
        );
        execute_transaction_with_directories(
            &root.join("receipt.json"),
            &changes,
            &[],
            &[root.join("copy"), replaced.clone()],
            |index, change| {
                publish_text_change(change)?;
                if index == 1 {
                    replace_copy_ancestor(&root, &replaced);
                    std::process::exit(73);
                }
                Ok(())
            },
            |_, _| unreachable!(),
        )
        .unwrap();
        panic!("copy replacement helper did not exit after publication");
    }

    #[test]
    fn prepared_copy_recovery_prunes_only_empty_created_directories() {
        for added_user_file in [false, true] {
            let dir = tempdir().unwrap();
            let root = dir.path().join("copy");
            let child = root.join("references");
            let file = child.join("guide.md");
            let receipt_path = dir.path().join("receipt.json");
            let created = change(&file, None, Some("installed bytes"));
            let receipt = TransactionReceipt {
                parent_locations: Vec::new(),
                version: RECEIPT_VERSION,
                phase: TransactionPhase::Prepared,
                changes: vec![ReceiptChange::from(&created)],
                symlink_changes: Vec::new(),
                rollback_directories: vec![
                    EncodedPath::from_path(&child),
                    EncodedPath::from_path(&root),
                ],
            };
            write_receipt(&receipt_path, &receipt, AtomicWriteMode::CreateNew).unwrap();
            publish_text_change(&created).unwrap();
            if added_user_file {
                fs::write(child.join("personal.txt"), "preserved").unwrap();
            }
            recover_text_file_transaction(&receipt_path).unwrap();
            assert!(!file.exists());
            assert!(!receipt_path.exists());
            if added_user_file {
                assert_eq!(
                    fs::read_to_string(child.join("personal.txt")).unwrap(),
                    "preserved"
                );
            } else {
                assert!(!root.exists());
            }
        }
    }

    #[test]
    fn successful_transaction_updates_all_paths_and_removes_receipt() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let receipt = dir.path().join("receipt.json");
        fs::write(&first, "before").unwrap();

        execute_text_file_transaction(
            &receipt,
            &[
                change(&first, Some("before"), Some("after")),
                change(&second, None, Some("created")),
            ],
        )
        .unwrap();

        assert_eq!(fs::read_to_string(first).unwrap(), "after");
        assert_eq!(fs::read_to_string(second).unwrap(), "created");
        assert!(!receipt.exists());
    }

    #[test]
    fn handled_later_failure_rolls_back_earlier_publication_and_removes_receipt() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let receipt = dir.path().join("receipt.json");
        fs::write(&first, "before-1").unwrap();
        fs::write(&second, "before-2").unwrap();
        let changes = [
            change(&first, Some("before-1"), Some("after-1")),
            change(&second, Some("before-2"), Some("after-2")),
        ];

        let error = execute_text_file_transaction_with(&receipt, &changes, |index, change| {
            if index == 1 {
                bail!("injected second publication failure");
            }
            publish_text_change(change)
        })
        .unwrap_err();

        assert!(error.to_string().contains("restored 1 changed path"));
        assert_eq!(fs::read_to_string(first).unwrap(), "before-1");
        assert_eq!(fs::read_to_string(second).unwrap(), "before-2");
        assert!(!receipt.exists());
    }

    #[test]
    fn windows_link_images_compare_injected_readings_by_native_separators() {
        let image = |path: &str| SymlinkImage::Link {
            target: EncodedPath::WindowsWide(path.encode_utf16().collect()),
        };
        // Model symlink_dir receiving a joined path and read_link returning backslashes.
        let planned = image(r"C:\work/package\ai-session-search");
        let read_back = image(r"C:\work\package\ai-session-search");
        assert_eq!(planned, read_back);
        assert_ne!(planned, image(r"C:\work\other\ai-session-search"));
        assert_ne!(planned, image(r"c:\work\package\ai-session-search"));
        assert_ne!(planned, image(r"work\package\ai-session-search"));
        assert_ne!(image(r"\\?\C:\work/package"), image(r"\\?\C:\work\package"));
        assert_ne!(image(r"\\.\C:\work/package"), image(r"\\.\C:\work\package"));
        assert_ne!(
            SymlinkImage::Link {
                target: EncodedPath::UnixBytes(b"/work/a\\b".to_vec())
            },
            SymlinkImage::Link {
                target: EncodedPath::UnixBytes(b"/work/a/b".to_vec())
            }
        );
    }

    #[test]
    fn link_removal_preserves_a_retargeted_entry_and_both_targets() {
        let dir = tempdir().unwrap();
        let expected = dir.path().join("expected");
        let foreign = dir.path().join("foreign");
        let link = dir.path().join("link");
        for target in [&expected, &foreign] {
            fs::create_dir(target).unwrap();
            fs::write(target.join("data"), "preserved").unwrap();
        }
        create_directory_symlink(&foreign, &link).unwrap();
        assert!(
            apply_symlink_change_after(&SymlinkChange::remove(link.clone(), expected.clone()))
                .is_err()
        );
        assert_eq!(fs::read_link(&link).unwrap(), foreign);
        for target in [&expected, &foreign] {
            assert_eq!(
                fs::read_to_string(target.join("data")).unwrap(),
                "preserved"
            );
        }
    }

    #[test]
    fn directory_link_removal_and_directory_restoration_preserve_target_contents() {
        for restore_directory in [false, true] {
            let dir = tempdir().unwrap();
            let target = dir.path().join("target");
            let link = dir.path().join("link");
            fs::create_dir(&target).unwrap();
            fs::write(target.join("data"), "preserved").unwrap();
            create_directory_symlink(&target, &link).unwrap();
            let mut change = SymlinkChange::remove(link.clone(), target.clone());
            if restore_directory {
                change.after = SymlinkImage::Directory;
            }
            publish_symlink_change(&change).unwrap();
            assert_eq!(
                fs::read_to_string(target.join("data")).unwrap(),
                "preserved"
            );
            if restore_directory {
                assert!(!fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink());
                assert_eq!(fs::read_dir(&link).unwrap().count(), 0);
            } else {
                assert!(fs::symlink_metadata(&link).is_err());
            }
        }
    }

    #[test]
    fn prepared_receipt_recovery_restores_symlink_and_text_preimages() {
        let dir = tempdir().unwrap();
        let text = dir.path().join("config.txt");
        let canonical = dir.path().join("skills/ai-session-search");
        let link = dir.path().join("harness/skills/ai-session-search");
        let receipt_path = dir.path().join("receipt.json");
        fs::write(&text, "after").unwrap();
        fs::create_dir_all(&canonical).unwrap();
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        create_directory_symlink(&canonical, &link).unwrap();
        let receipt = TransactionReceipt {
            parent_locations: Vec::new(),
            rollback_directories: Vec::new(),
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Prepared,
            changes: vec![ReceiptChange::from(&change(
                &text,
                Some("before"),
                Some("after"),
            ))],
            symlink_changes: vec![ReceiptSymlinkChange::from(&SymlinkChange::install(
                link.clone(),
                canonical,
            ))],
        };
        write_receipt(&receipt_path, &receipt, AtomicWriteMode::CreateNew).unwrap();

        assert_eq!(
            recover_text_file_transaction(&receipt_path).unwrap(),
            RecoveryOutcome::RolledBack { paths: 2 }
        );
        assert_eq!(fs::read_to_string(text).unwrap(), "before");
        assert!(!link.exists());
        assert!(!receipt_path.exists());
    }

    #[test]
    fn version_one_text_only_receipt_remains_recoverable() {
        let dir = tempdir().unwrap();
        let text = dir.path().join("config.txt");
        let receipt_path = dir.path().join("receipt.json");
        fs::write(&text, "after").unwrap();
        let mut value = serde_json::to_value(TransactionReceipt {
            parent_locations: Vec::new(),
            rollback_directories: Vec::new(),
            version: 1,
            phase: TransactionPhase::Prepared,
            changes: vec![ReceiptChange::from(&change(
                &text,
                Some("before"),
                Some("after"),
            ))],
            symlink_changes: Vec::new(),
        })
        .unwrap();
        value.as_object_mut().unwrap().remove("symlink_changes");
        fs::write(&receipt_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        assert_eq!(
            recover_text_file_transaction(&receipt_path).unwrap(),
            RecoveryOutcome::RolledBack { paths: 1 }
        );
        assert_eq!(fs::read_to_string(text).unwrap(), "before");
        assert!(!receipt_path.exists());
    }

    #[test]
    fn pending_receipt_blocks_new_transaction_before_any_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.txt");
        let receipt_path = dir.path().join("receipt.json");
        fs::write(&path, "before").unwrap();
        let receipt = TransactionReceipt {
            parent_locations: Vec::new(),
            rollback_directories: Vec::new(),
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Prepared,
            changes: vec![ReceiptChange::from(&change(
                &path,
                Some("before"),
                Some("after"),
            ))],
            symlink_changes: Vec::new(),
        };
        write_receipt(&receipt_path, &receipt, AtomicWriteMode::CreateNew).unwrap();

        let error = execute_text_file_transaction(
            &receipt_path,
            &[change(&path, Some("before"), Some("other"))],
        )
        .unwrap_err();

        assert!(error.to_string().contains("requires recovery"));
        assert_eq!(fs::read_to_string(path).unwrap(), "before");
        assert!(receipt_path.exists());
    }

    #[test]
    fn control_path_validation_rejects_receipt_target_without_filesystem_mutation() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("receipt.json");
        let adjacent_lock = lock_path(&target);
        fs::write(&target, "before").unwrap();

        let error = execute_text_file_transaction(
            &target,
            &[change(&target, Some("before"), Some("after"))],
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("transaction receipt path"), "{error}");
        assert!(error.contains("mutation target path"), "{error}");
        assert!(error.contains(&target.display().to_string()), "{error}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "before");
        assert!(!adjacent_lock.exists());
    }

    #[test]
    fn control_path_validation_rejects_lock_target_without_filesystem_mutation() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let target = lock_path(&receipt);
        fs::write(&target, "before").unwrap();

        let error = execute_text_file_transaction(
            &receipt,
            &[change(&target, Some("before"), Some("after"))],
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("adjacent transaction lock path"), "{error}");
        assert!(error.contains("mutation target path"), "{error}");
        assert!(error.contains(&target.display().to_string()), "{error}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "before");
        assert!(!receipt.exists());
    }

    #[test]
    fn control_path_validation_rejects_lexically_aliased_target() {
        let dir = tempdir().unwrap();
        let receipt = dir.path().join("state").join("..").join("receipt.json");
        let target = dir.path().join("receipt.json");
        fs::write(&target, "before").unwrap();

        let error = execute_text_file_transaction(
            &receipt,
            &[change(&target, Some("before"), Some("after"))],
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("control-path conflict"), "{error}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "before");
    }

    #[test]
    fn control_path_validation_allows_distinct_transaction_paths() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("config.txt");
        let receipt = dir.path().join("receipt.json");
        fs::write(&target, "before").unwrap();

        execute_text_file_transaction(&receipt, &[change(&target, Some("before"), Some("after"))])
            .unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "after");
        assert!(!receipt.exists());
    }

    #[test]
    fn recovery_guidance_is_shell_independent_and_preserves_the_display_path() {
        let receipt = Path::new("/tmp/receipt with 'quotes'.json");
        let guidance = recovery_guidance(receipt);

        assert!(guidance.contains(
            "argv [`integrations`, `recover`, `--transaction-receipt`, `<RECEIPT_PATH>`]"
        ));
        assert!(guidance.contains(&receipt.display().to_string()));
        assert!(!guidance.contains("'\"'\"'"));
    }

    #[test]
    fn read_snapshot_waits_for_writer_and_then_runs_once() {
        use std::sync::{mpsc, Arc, Barrier};
        use std::thread;
        use std::time::Duration;

        let dir = tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let mut lock = open_file_lock(&lock_path(&receipt)).unwrap();
        let writer_ready = Arc::new(Barrier::new(2));
        let release_writer = Arc::new(Barrier::new(2));

        let writer_ready_thread = Arc::clone(&writer_ready);
        let release_writer_thread = Arc::clone(&release_writer);
        let writer = thread::spawn(move || {
            let _guard = lock.write().unwrap();
            writer_ready_thread.wait();
            release_writer_thread.wait();
        });
        writer_ready.wait();

        let (entered_tx, entered_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            with_text_file_transaction_read_lock(&receipt, || {
                entered_tx.send(()).unwrap();
                Ok(())
            })
        });

        assert!(entered_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release_writer.wait();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reader should enter after writer releases the lock");

        writer.join().unwrap();
        reader.join().unwrap().unwrap();
    }

    #[test]
    fn read_snapshot_without_prior_transaction_creates_no_lock_or_parent() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("read-only-status");
        let receipt = parent.join("receipt.json");
        let calls = std::cell::Cell::new(0);

        let value = with_text_file_transaction_read_lock(&receipt, || {
            calls.set(calls.get() + 1);
            Ok("snapshot")
        })
        .unwrap();

        assert_eq!(value, "snapshot");
        assert_eq!(calls.get(), 1);
        assert!(!parent.exists());
    }

    #[test]
    fn edit_after_preflight_is_preserved_without_creating_receipt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.txt");
        let receipt = dir.path().join("receipt.json");
        fs::write(&path, "before").unwrap();
        let planned = change(&path, Some("before"), Some("after"));
        fs::write(&path, "external edit").unwrap();

        let error = execute_text_file_transaction(&receipt, &[planned]).unwrap_err();

        assert!(error
            .to_string()
            .contains("refusing to overwrite concurrent edits"));
        assert_eq!(fs::read_to_string(path).unwrap(), "external edit");
        assert!(!receipt.exists());
    }

    #[test]
    fn edit_during_failure_is_preserved_while_safe_paths_are_rolled_back() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let receipt = dir.path().join("receipt.json");
        fs::write(&first, "before-1").unwrap();
        fs::write(&second, "before-2").unwrap();
        let changes = [
            change(&first, Some("before-1"), Some("after-1")),
            change(&second, Some("before-2"), Some("after-2")),
        ];

        let error = execute_text_file_transaction_with(&receipt, &changes, |index, change| {
            if index == 1 {
                fs::write(&change.path, "external edit").unwrap();
            }
            publish_text_change(change)
        })
        .unwrap_err();

        assert!(error.to_string().contains("rollback was incomplete"));
        assert_eq!(fs::read_to_string(first).unwrap(), "before-1");
        assert_eq!(fs::read_to_string(second).unwrap(), "external edit");
        assert!(receipt.exists());
    }

    #[test]
    fn held_transaction_lock_rejects_competing_writer_before_receipt() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.txt");
        let receipt = dir.path().join("receipt.json");
        fs::write(&path, "before").unwrap();
        let lock_path = lock_path(&receipt);
        let mut lock = open_file_lock(&lock_path).unwrap();
        let _guard = lock.try_write().unwrap();

        let error = execute_text_file_transaction(
            &receipt,
            &[change(&path, Some("before"), Some("after"))],
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("another integration transaction"));
        assert_eq!(fs::read_to_string(path).unwrap(), "before");
        assert!(!receipt.exists());
    }

    #[test]
    fn recovery_without_receipt_creates_no_lock_or_parent() {
        let dir = tempdir().unwrap();
        let parent = dir.path().join("absent");
        let receipt = parent.join("receipt.json");

        let error = recover_text_file_transaction(&receipt).unwrap_err();

        assert!(error
            .to_string()
            .contains("no integration recovery receipt"));
        assert!(!parent.exists());
    }

    #[test]
    fn process_exit_after_first_publish_is_recovered_from_durable_receipt() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let receipt = dir.path().join("receipt.json");
        fs::write(&first, "before-1").unwrap();
        fs::write(&second, "before-2").unwrap();

        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "text_file_transaction::tests::crash_after_first_publish_helper",
                "--ignored",
            ])
            .env(CRASH_HELPER_ROOT, dir.path())
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(86), "{output:?}");
        assert_eq!(fs::read_to_string(&first).unwrap(), "after-1");
        assert_eq!(fs::read_to_string(&second).unwrap(), "before-2");
        assert!(receipt.exists());

        assert_eq!(
            recover_text_file_transaction(&receipt).unwrap(),
            RecoveryOutcome::RolledBack { paths: 1 }
        );
        assert_eq!(fs::read_to_string(first).unwrap(), "before-1");
        assert_eq!(fs::read_to_string(second).unwrap(), "before-2");
        assert!(!receipt.exists());
    }

    #[test]
    #[ignore = "subprocess helper that intentionally exits without running destructors"]
    fn crash_after_first_publish_helper() {
        let Some(root) = std::env::var_os(CRASH_HELPER_ROOT).map(PathBuf::from) else {
            return;
        };
        let changes = [
            change(&root.join("first.txt"), Some("before-1"), Some("after-1")),
            change(&root.join("second.txt"), Some("before-2"), Some("after-2")),
        ];

        let _ = execute_text_file_transaction_with(
            &root.join("receipt.json"),
            &changes,
            |index, change| {
                publish_text_change(change)?;
                if index == 0 {
                    std::process::exit(86);
                }
                Ok(())
            },
        );
        panic!("crash helper reached the second publication");
    }

    #[test]
    fn prepared_recovery_restores_published_paths_and_preserves_untouched_paths() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let receipt_path = dir.path().join("receipt.json");
        fs::write(&first, "after").unwrap();
        fs::write(&second, "before-2").unwrap();
        let receipt = TransactionReceipt {
            parent_locations: Vec::new(),
            rollback_directories: Vec::new(),
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Prepared,
            changes: vec![
                ReceiptChange::from(&change(&first, Some("before"), Some("after"))),
                ReceiptChange::from(&change(&second, Some("before-2"), Some("after-2"))),
            ],
            symlink_changes: Vec::new(),
        };
        write_receipt(&receipt_path, &receipt, AtomicWriteMode::CreateNew).unwrap();

        assert_eq!(
            recover_text_file_transaction(&receipt_path).unwrap(),
            RecoveryOutcome::RolledBack { paths: 1 }
        );
        assert_eq!(fs::read_to_string(first).unwrap(), "before");
        assert_eq!(fs::read_to_string(second).unwrap(), "before-2");
        assert!(!receipt_path.exists());
    }

    #[test]
    fn recovery_preserves_receipt_and_external_edit_on_conflict() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.txt");
        let receipt_path = dir.path().join("receipt.json");
        fs::write(&path, "external edit").unwrap();
        let receipt = TransactionReceipt {
            parent_locations: Vec::new(),
            rollback_directories: Vec::new(),
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Prepared,
            changes: vec![ReceiptChange::from(&change(
                &path,
                Some("before"),
                Some("after"),
            ))],
            symlink_changes: Vec::new(),
        };
        write_receipt(&receipt_path, &receipt, AtomicWriteMode::CreateNew).unwrap();

        let error = recover_text_file_transaction(&receipt_path).unwrap_err();

        assert!(error.to_string().contains("differs from both"));
        assert_eq!(fs::read_to_string(path).unwrap(), "external edit");
        assert!(receipt_path.exists());
    }

    #[test]
    fn published_recovery_only_finalizes_matching_outputs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.txt");
        let receipt_path = dir.path().join("receipt.json");
        fs::write(&path, "after").unwrap();
        let receipt = TransactionReceipt {
            parent_locations: Vec::new(),
            rollback_directories: Vec::new(),
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Published,
            changes: vec![ReceiptChange::from(&change(
                &path,
                Some("before"),
                Some("after"),
            ))],
            symlink_changes: Vec::new(),
        };
        write_receipt(&receipt_path, &receipt, AtomicWriteMode::CreateNew).unwrap();

        assert_eq!(
            recover_text_file_transaction(&receipt_path).unwrap(),
            RecoveryOutcome::Finalized { paths: 1 }
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "after");
        assert!(!receipt_path.exists());
    }

    // Darwin's renamex_np(RENAME_EXCL) rejects invalid UTF-8 path bytes with EILSEQ; Linux
    // permits arbitrary non-NUL filename bytes and therefore exercises the receipt encoding.
    #[cfg(target_os = "linux")]
    #[test]
    fn receipt_round_trips_non_utf8_destination_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let dir = tempdir().unwrap();
        let path = dir.path().join(OsString::from_vec(vec![b'f', 0x80]));
        let receipt = dir.path().join("receipt.json");

        execute_text_file_transaction(&receipt, &[change(&path, None, Some("content"))]).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "content");
    }
}
