//! Recoverable transactions over a small set of unrelated UTF-8 text files.
//!
//! Cross-directory rename atomicity does not exist. This module instead writes one durable
//! receipt before the first mutation, validates every preimage immediately before publication,
//! rolls handled failures back in reverse order, and retains evidence when an external edit makes
//! automatic recovery unsafe.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::durable_fs::{
    atomic_write_file, entry_exists, open_existing_file_lock, open_file_lock, sync_parent,
    AtomicWriteMode,
};

const RECEIPT_VERSION: u32 = 1;

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReceiptChange {
    path: EncodedPath,
    before: Option<TextFileImage>,
    after: Option<TextFileImage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "units", rename_all = "snake_case")]
enum EncodedPath {
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
    Utf8(String),
}

impl EncodedPath {
    fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            return Self::UnixBytes(path.as_os_str().as_bytes().to_vec());
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;
            return Self::WindowsWide(path.as_os_str().encode_wide().collect());
        }
        #[allow(unreachable_code)]
        Self::Utf8(path.to_string_lossy().into_owned())
    }

    fn to_path_buf(&self) -> Result<PathBuf> {
        match self {
            Self::UnixBytes(bytes) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt as _;
                    Ok(PathBuf::from(OsString::from_vec(bytes.clone())))
                }
                #[cfg(not(unix))]
                bail!("receipt contains a Unix path on a non-Unix host")
            }
            Self::WindowsWide(_units) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt as _;
                    return Ok(PathBuf::from(OsString::from_wide(_units)));
                }
                #[cfg(not(windows))]
                bail!("receipt contains a Windows path on a non-Windows host")
            }
            Self::Utf8(text) => Ok(PathBuf::from(text)),
        }
    }
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

pub(crate) fn execute_text_file_transaction(
    receipt_path: &Path,
    changes: &[TextFileChange],
) -> Result<()> {
    execute_text_file_transaction_with(receipt_path, changes, |_, change| {
        publish_text_change(change)
    })
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
            "failed to open MCP transaction lock {}",
            lock_path.display()
        )
    })?;
    if let Some(lock) = existing_lock {
        let _guard = lock.read().with_context(|| {
            format!(
                "failed to read-lock MCP transaction {}",
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
            "failed to recheck MCP transaction lock {}",
            lock_path.display()
        )
    })?;
    let Some(lock) = appeared_lock else {
        return Ok(unlocked_snapshot);
    };
    let _guard = lock.read().with_context(|| {
        format!(
            "failed to read-lock MCP transaction {} after it appeared during status",
            lock_path.display()
        )
    })?;
    read_snapshot()
}

fn execute_text_file_transaction_with<F>(
    receipt_path: &Path,
    changes: &[TextFileChange],
    mut publish: F,
) -> Result<()>
where
    F: FnMut(usize, &TextFileChange) -> Result<()>,
{
    let changes = changes
        .iter()
        .cloned()
        .map(absolutize_change)
        .collect::<Result<Vec<_>>>()?;
    if changes.is_empty() {
        return Ok(());
    }
    validate_changes(&changes)?;
    validate_control_paths(receipt_path, &changes)?;
    let lock_path = lock_path(receipt_path);
    let mut lock = open_file_lock(&lock_path).with_context(|| {
        format!(
            "failed to open MCP transaction lock {}",
            lock_path.display()
        )
    })?;
    let _guard = lock.try_write().with_context(|| {
        format!(
            "another MCP configuration transaction holds {}",
            lock_path.display()
        )
    })?;
    if entry_exists(receipt_path)? {
        bail!(
            "pending MCP configuration receipt requires recovery: {}; {}",
            receipt_path.display(),
            recovery_guidance(receipt_path)
        );
    }
    for change in &changes {
        verify_image(&change.path, &change.before)?;
    }

    let mut receipt = TransactionReceipt {
        version: RECEIPT_VERSION,
        phase: TransactionPhase::Prepared,
        changes: changes.iter().map(ReceiptChange::from).collect(),
    };
    write_receipt(receipt_path, &receipt, AtomicWriteMode::CreateNew)?;

    for (index, change) in changes.iter().enumerate() {
        if let Err(publish_error) = publish(index, change) {
            return rollback_after_failure(receipt_path, &receipt, publish_error);
        }
    }

    receipt.phase = TransactionPhase::Published;
    if let Err(phase_error) = write_receipt(receipt_path, &receipt, AtomicWriteMode::Replace) {
        return match load_receipt(receipt_path) {
            Ok(on_disk) if on_disk.phase == TransactionPhase::Published => Err(anyhow!(
                "MCP changes and the published receipt are complete, but durability confirmation failed: {phase_error:#}; {} to verify and finalize",
                recovery_guidance(receipt_path)
            )),
            Ok(on_disk) if on_disk.phase == TransactionPhase::Prepared => {
                rollback_after_failure(receipt_path, &on_disk, phase_error)
            }
            Ok(_) => Err(anyhow!(
                "MCP receipt has an unknown phase after publication failure; preserved evidence at {}",
                receipt_path.display()
            )),
            Err(load_error) => Err(anyhow!(
                "failed to publish the final MCP receipt: {phase_error:#}; the receipt is unreadable: {load_error:#}; current files were not changed again and evidence was preserved at {}",
                receipt_path.display()
            )),
        };
    }
    match remove_receipt(receipt_path) {
        Ok(()) => Ok(()),
        Err(error) if entry_exists(receipt_path).unwrap_or(true) => Err(error).context(format!(
            "MCP changes are complete; {} to verify and finalize",
            recovery_guidance(receipt_path)
        )),
        Err(error) => Err(error).context(
            "MCP changes are complete and the receipt is absent, but its parent-directory sync failed",
        ),
    }
}

pub(crate) fn transaction_recovery_required(receipt_path: &Path) -> Result<bool> {
    entry_exists(receipt_path)
        .with_context(|| format!("failed to inspect MCP receipt {}", receipt_path.display()))
}

pub(crate) fn recovery_guidance(receipt_path: &Path) -> String {
    format!(
        "invoke `aise` with argv [`mcp`, `recover`, `--transaction-receipt`, `<RECEIPT_PATH>`], where `<RECEIPT_PATH>` is {}",
        receipt_path.display()
    )
}

pub(crate) fn recover_text_file_transaction(receipt_path: &Path) -> Result<RecoveryOutcome> {
    if !transaction_recovery_required(receipt_path)? {
        bail!(
            "no MCP configuration recovery receipt exists at {}",
            receipt_path.display()
        );
    }
    let lock_path = lock_path(receipt_path);
    let mut lock = open_file_lock(&lock_path).with_context(|| {
        format!(
            "failed to open MCP transaction lock {}",
            lock_path.display()
        )
    })?;
    let _guard = lock.try_write().with_context(|| {
        format!(
            "another MCP configuration transaction holds {}",
            lock_path.display()
        )
    })?;
    let receipt = load_receipt(receipt_path)?;
    let outcome = match receipt.phase {
        TransactionPhase::Prepared => RecoveryOutcome::RolledBack {
            paths: rollback_prepared(&receipt)?,
        },
        TransactionPhase::Published => {
            for change in &receipt.changes {
                let path = change.path.to_path_buf()?;
                verify_image(&path, &change.after).with_context(|| {
                    format!(
                        "published MCP transaction conflicts with current file {}; refusing to discard recovery evidence",
                        path.display()
                    )
                })?;
            }
            RecoveryOutcome::Finalized {
                paths: receipt.changes.len(),
            }
        }
    };
    remove_receipt(receipt_path)?;
    Ok(outcome)
}

fn validate_changes(changes: &[TextFileChange]) -> Result<()> {
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
    Ok(())
}

fn validate_control_paths(receipt_path: &Path, changes: &[TextFileChange]) -> Result<()> {
    let receipt_path = normalize_transaction_path(receipt_path)?;
    let control_paths = [
        ("transaction receipt", receipt_path.clone()),
        ("adjacent transaction lock", lock_path(&receipt_path)),
    ];
    for (control_role, control_path) in control_paths {
        if let Some(change) = changes.iter().find(|change| {
            normalize_transaction_path(&change.path).is_ok_and(|path| path == control_path)
        }) {
            bail!(
                "MCP transaction control-path conflict: {control_role} path {} overlaps mutation target path {}; choose a transaction receipt whose receipt and adjacent lock paths are outside every mutation target",
                control_path.display(),
                change.path.display()
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
            .context("failed to resolve current directory for MCP transaction paths")?
            .join(path))
    }
}

fn normalize_transaction_path(path: &Path) -> Result<PathBuf> {
    use std::path::Component;

    let absolute = absolutize_transaction_path(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    Ok(normalized)
}

fn absolutize_change(mut change: TextFileChange) -> Result<TextFileChange> {
    change.path = absolutize_transaction_path(&change.path)?;
    Ok(change)
}

fn rollback_prepared(receipt: &TransactionReceipt) -> Result<usize> {
    let mut restored = 0;
    let mut conflicts = Vec::new();
    for change in receipt.changes.iter().rev() {
        let path = change.path.to_path_buf()?;
        let current = snapshot_utf8_regular_file(&path)?;
        if current == change.before {
            continue;
        }
        if current == change.after {
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
                "MCP configuration publication failed; restored {paths} changed path(s)"
            ))
        }
        Err(rollback_error) => Err(anyhow!(
            "MCP configuration publication failed: {failure:#}; automatic rollback was incomplete: {rollback_error:#}; recovery receipt preserved at {}; {} after resolving concurrent edits",
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
    let content = serde_json::to_vec_pretty(receipt)?;
    atomic_write_file(path, &content, mode).with_context(|| {
        format!(
            "failed to publish MCP transaction receipt {}",
            path.display()
        )
    })
}

fn load_receipt(path: &Path) -> Result<TransactionReceipt> {
    let image = snapshot_utf8_regular_file(path)?.ok_or_else(|| {
        anyhow!(
            "no MCP configuration recovery receipt exists at {}",
            path.display()
        )
    })?;
    let receipt: TransactionReceipt = serde_json::from_str(image.text())
        .with_context(|| format!("failed to parse MCP transaction receipt {}", path.display()))?;
    if receipt.version != RECEIPT_VERSION {
        bail!(
            "unsupported MCP transaction receipt version {} in {} (expected {})",
            receipt.version,
            path.display(),
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

fn lock_path(receipt_path: &Path) -> PathBuf {
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
    fn pending_receipt_blocks_new_transaction_before_any_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.txt");
        let receipt_path = dir.path().join("receipt.json");
        fs::write(&path, "before").unwrap();
        let receipt = TransactionReceipt {
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Prepared,
            changes: vec![ReceiptChange::from(&change(
                &path,
                Some("before"),
                Some("after"),
            ))],
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

        assert!(
            guidance.contains("argv [`mcp`, `recover`, `--transaction-receipt`, `<RECEIPT_PATH>`]")
        );
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
            .contains("another MCP configuration transaction"));
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
            .contains("no MCP configuration recovery receipt"));
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
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Prepared,
            changes: vec![
                ReceiptChange::from(&change(&first, Some("before"), Some("after"))),
                ReceiptChange::from(&change(&second, Some("before-2"), Some("after-2"))),
            ],
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
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Prepared,
            changes: vec![ReceiptChange::from(&change(
                &path,
                Some("before"),
                Some("after"),
            ))],
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
            version: RECEIPT_VERSION,
            phase: TransactionPhase::Published,
            changes: vec![ReceiptChange::from(&change(
                &path,
                Some("before"),
                Some("after"),
            ))],
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
        use std::os::unix::ffi::OsStringExt as _;

        let dir = tempdir().unwrap();
        let path = dir.path().join(OsString::from_vec(vec![b'f', 0x80]));
        let receipt = dir.path().join("receipt.json");

        execute_text_file_transaction(&receipt, &[change(&path, None, Some("content"))]).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "content");
    }
}
