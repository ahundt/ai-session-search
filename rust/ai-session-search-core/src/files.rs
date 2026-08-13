// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! `files` command group: file-version recovery from session tool calls.
//!
//! File-mutating tool calls are extracted per provider and persisted to the `file_edits`
//! table, with per-provider fidelity:
//!   * claude — `Write`/`Edit`/`MultiEdit`/`NotebookEdit` (full content + `old`→`new` deltas);
//!   * pi     — `write`/`edit` (full content + `old`→`new` deltas);
//!   * codex  — `apply_patch` (`Add File` = full content; `Update`/`Delete` = path-only);
//!   * cursor — `ApplyPatch` unified diff (path-only);
//!   * antigravity — `write_to_file`/`replace_file_content`/`multi_replace_file_content`
//!     (path-only; the transcript's edit-arg content shape is unverified upstream).
//!
//! Path-only edits appear in `files search`/`history`/`cross-ref` but are not
//! reconstructable via `files extract` (a diff/hunk is not a replayable Write/Edit delta).
//!
//! This module turns the recorded edits into:
//!   * `files search`   — which files were edited, how often, across how many sessions;
//!   * `files history`   — the ordered versions of one file (with reconstructed line counts);
//!   * `files cross-ref` — the file ↔ session linkage;
//!   * `files extract`   — reconstruct (and optionally restore) a historical version.
//!
//! `files history` numbers versions by record order within the session (the stable, causal
//! ordering). That is normally — but not strictly — chronological: in a forked/resumed
//! session a later version's `ts` can predate an earlier one. This is expected, not a bug.
//!
//! Reconstruction replays deltas from the most recent full `Write` snapshot. `extract
//! --restore` never overwrites: it writes to a
//! collision-safe `<stem>.recovered[.ext]` sibling.

use std::collections::HashMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::dates::DateRange;
use crate::db::Db;
use crate::durable_fs::{entry_exists, StagedDirectory};
use crate::models::{
    EditOp, FileCrossRef, FileEdit, FileEditSummary, FileQuery, FileVersion, Provider,
};
use crate::render::{render, OutputFormat, Row};

pub(crate) type FileMutationPayload = (String, Option<String>, Vec<EditOp>);

#[derive(Default)]
pub(crate) struct PendingFileMutations {
    // One parse owns this O(P) state, where P is unresolved mutation calls. Stage/finish are
    // average O(1); dropping the tracker at EOF discards every outcome that was never proven.
    by_call_id: HashMap<String, PendingFileMutation>,
    next_seq: i64,
}

struct PendingFileMutation {
    ts: Option<chrono::DateTime<chrono::Utc>>,
    tool: String,
    payloads: Vec<FileMutationPayload>,
}

impl PendingFileMutations {
    pub(crate) fn stage(
        &mut self,
        call_id: Option<&str>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
        tool: &str,
        payload: Option<FileMutationPayload>,
    ) {
        let Some(payload) = payload else {
            return;
        };
        self.stage_many(call_id, ts, tool, vec![payload]);
    }

    pub(crate) fn stage_many(
        &mut self,
        call_id: Option<&str>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
        tool: &str,
        payloads: Vec<FileMutationPayload>,
    ) {
        let Some(call_id) = call_id.filter(|id| !id.is_empty()) else {
            return;
        };
        if payloads.is_empty() {
            return;
        }
        self.by_call_id.insert(
            call_id.to_string(),
            PendingFileMutation {
                ts,
                tool: tool.to_string(),
                payloads,
            },
        );
    }

    pub(crate) fn finish(
        &mut self,
        call_id: Option<&str>,
        succeeded: bool,
        out: &mut Vec<FileEdit>,
    ) {
        let Some(call_id) = call_id else {
            return;
        };
        let Some(pending) = self.by_call_id.remove(call_id) else {
            return;
        };
        if !succeeded {
            return;
        }
        for (file_path, new_content, edits) in pending.payloads {
            out.push(FileEdit {
                seq: self.next_seq,
                ts: pending.ts,
                tool: pending.tool.clone(),
                file_name: crate::util::file_basename(&file_path),
                file_path,
                new_content,
                edits,
            });
            self.next_seq += 1;
        }
    }
}

/// Reconstruct a file's content as of 1-based `version` by replaying edits forward
/// from the most recent full `Write` snapshot at or before the target.
///
/// Returns `None` when no complete replay path exists at `version` or when `version` is out of
/// range. A path-only edit or replay mismatch invalidates known content until a later full
/// snapshot; recovery never invents a successful file state from an unapplied mutation.
pub fn reconstruct(edits: &[FileEdit], version: usize) -> Option<String> {
    if version == 0 || version > edits.len() {
        return None;
    }
    let target = version - 1;
    // Latest full snapshot (a `Write`, which sets `new_content`) at or before target.
    let base = (0..=target)
        .rev()
        .find(|&i| edits[i].new_content.is_some())?;
    let mut content = edits[base].new_content.clone();
    for edit in &edits[base + 1..=target] {
        advance_reconstruction(&mut content, edit);
    }
    content
}

/// Apply replacements in order. Returns false when any requested replacement cannot be replayed.
fn apply_edits(content: &mut String, edits: &[EditOp]) -> bool {
    for op in edits {
        if op.old.is_empty() {
            return false;
        }
        if op.replace_all {
            if !content.contains(op.old.as_str()) {
                return false;
            }
            *content = content.replace(op.old.as_str(), &op.new);
        } else if let Some(pos) = content.find(op.old.as_str()) {
            content.replace_range(pos..pos + op.old.len(), &op.new);
        } else {
            return false;
        }
    }
    true
}

/// Advance one reconstruction state without inventing bytes for an unreplayable event.
/// A later full snapshot re-establishes a known state after any path-only gap.
fn advance_reconstruction(content: &mut Option<String>, edit: &FileEdit) {
    if let Some(full) = &edit.new_content {
        *content = Some(full.clone());
    } else if edit.edits.is_empty()
        || !content
            .as_mut()
            .is_some_and(|current| apply_edits(current, &edit.edits))
    {
        *content = None;
    }
}

/// Reject a `--restore` destination derived from session data that would escape the
/// intended tree via a `..` (parent-dir) component. Reconstructed `file_path`s come
/// straight from (potentially untrusted) session JSON; an absolute path to the user's
/// own file is the normal case (we restore beside it, never overwriting), but a parent-dir
/// traversal is never a legitimate recorded edit path and must not steer a write elsewhere.
fn ensure_safe_restore_target(path: &Path) -> Result<()> {
    use std::path::Component;
    if path.components().any(|c| c == Component::ParentDir) {
        bail!(
            "refusing to restore to '{}': the session-recorded path contains '..'; \
             pass --output-dir to choose a safe destination",
            path.display()
        );
    }
    Ok(())
}

/// The bare, traversal-safe filename to write under `--output-dir`. The result is always
/// a single path component: [`Path::file_name`] strips any directory and returns `None`
/// for a path ending in `..`, so neither a session-recorded path nor a `--file` argument
/// containing `../` (or an absolute path) can escape the chosen output directory when it
/// is `dir.join`ed. Falls back to the literal `recovered` if neither source yields a name.
fn safe_output_name(original: &Path, file_arg: &str) -> PathBuf {
    original
        .file_name()
        .or_else(|| Path::new(file_arg).file_name())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("recovered"))
}

/// Pick a candidate recovery path: `<stem>.recovered[.ext]`, then `_2`, `_3`, …
/// until `exists` returns false. Publication must still use `create_new`; this helper
/// remains private so external callers cannot mistake candidate selection for a lock.
fn restore_target<F: Fn(&Path) -> bool>(original: &Path, exists: F) -> PathBuf {
    let dir = original.parent().unwrap_or_else(|| Path::new("."));
    let stem = original
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("recovered");
    let ext = original.extension().and_then(|s| s.to_str());
    let candidate = |n: usize| -> PathBuf {
        let marker = if n == 1 {
            "recovered".to_string()
        } else {
            format!("recovered_{n}")
        };
        let name = match ext {
            Some(ext) => format!("{stem}.{marker}.{ext}"),
            None => format!("{stem}.{marker}"),
        };
        dir.join(name)
    };
    let mut n = 1;
    loop {
        let path = candidate(n);
        if !exists(&path) {
            return path;
        }
        n += 1;
    }
}

struct PendingRestore {
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl PendingRestore {
    fn persist(mut self, content: &[u8]) -> Result<PathBuf> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| anyhow!("recovery file was already finalized"))?;
        file.write_all(content)
            .with_context(|| format!("failed to write recovery file '{}'", self.path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync recovery file '{}'", self.path.display()))?;
        self.file.take();
        Ok(self.path.clone())
    }
}

impl Drop for PendingRestore {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn create_recovery_file(base: &Path) -> Result<PendingRestore> {
    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create recovery directory '{}'", parent.display())
        })?;
    }
    loop {
        let path = restore_target(base, Path::exists);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                return Ok(PendingRestore {
                    path,
                    file: Some(file),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create recovery file '{}'", path.display())
                });
            }
        }
    }
}

/// Restore a reconstructed file without overwriting an existing path.
///
/// When `output_dir` is absent, the session-recorded path is validated and a
/// collision-safe `.recovered` sibling is created. When it is present, only the
/// recorded basename is used beneath that directory. The destination path is
/// atomically claimed with `create_new`, and a partial file is removed by an RAII
/// guard if writing or syncing fails.
pub fn restore_reconstructed(
    reconstructed: &ReconstructedFile,
    output_dir: Option<&Path>,
) -> Result<PathBuf> {
    let original = Path::new(&reconstructed.file_path);
    let base = match output_dir {
        Some(dir) => dir.join(safe_output_name(original, &reconstructed.file_path)),
        None => {
            if original.as_os_str().is_empty() {
                bail!("cannot restore beside an empty session-recorded path; provide output_dir");
            }
            ensure_safe_restore_target(original)?;
            original.to_path_buf()
        }
    };
    create_recovery_file(&base)?.persist(reconstructed.content.as_bytes())
}

/// Group `(session_id, provider, edit)` rows (already ordered by `(session_id, seq)`)
/// into per-session edit lists, preserving order. Each list's index+1 is its version.
fn group_by_session(
    rows: Vec<(String, Provider, FileEdit)>,
) -> Vec<(String, Provider, Vec<FileEdit>)> {
    let mut groups: Vec<(String, Provider, Vec<FileEdit>)> = Vec::new();
    for (session_id, provider, edit) in rows {
        match groups.last_mut() {
            Some((sid, _, list)) if *sid == session_id => list.push(edit),
            _ => groups.push((session_id, provider, vec![edit])),
        }
    }
    groups
}

fn count_lines(content: &str) -> i64 {
    content.lines().count() as i64
}

/// Line count of every 1-based version, reconstructed in a single forward pass:
/// O(total edits × content) instead of the O(versions² × content) you get from
/// calling [`reconstruct`] per version (which re-replays from the base each time).
/// A `Write` resets the running content; deltas replay on top; versions before the
/// first `Write` count 0 (no full-content base) — identical to `reconstruct`.
fn version_line_counts(edits: &[FileEdit]) -> Vec<i64> {
    let mut content: Option<String> = None;
    edits
        .iter()
        .map(|edit| {
            advance_reconstruction(&mut content, edit);
            content.as_deref().map(count_lines).unwrap_or(0)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconstructedFile {
    pub session_id: String,
    pub provider: Provider,
    pub version: usize,
    pub file_path: String,
    pub content: String,
}

/// Receipt for an atomically published, non-replacing directory of reconstructed versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveryPublicationReceipt {
    pub destination: PathBuf,
    pub files: Vec<PathBuf>,
}

fn versioned_output_name(reconstructed: &ReconstructedFile) -> PathBuf {
    let original = Path::new(&reconstructed.file_path);
    let base = safe_output_name(original, &reconstructed.file_path);
    let stem = base
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("recovered");
    match base.extension().and_then(|value| value.to_str()) {
        Some(extension) => {
            PathBuf::from(format!("{stem}_v{}.{}", reconstructed.version, extension))
        }
        None => PathBuf::from(format!("{stem}_v{}", reconstructed.version)),
    }
}

fn recovery_publication_parent(destination: &Path) -> Result<&Path> {
    if !destination.is_absolute() {
        bail!(
            "recovery publication destination must be absolute: {}",
            destination.display()
        );
    }
    if entry_exists(destination).with_context(|| {
        format!(
            "failed to inspect recovery publication destination {}",
            destination.display()
        )
    })? {
        bail!(
            "recovery publication destination already exists: {}",
            destination.display()
        );
    }
    let parent = destination.parent().ok_or_else(|| {
        anyhow!(
            "recovery publication destination has no parent: {}",
            destination.display()
        )
    })?;
    if !parent.is_dir() {
        bail!(
            "recovery publication parent is not a directory: {}",
            parent.display()
        );
    }
    Ok(parent)
}

/// Publish reconstructed versions as one complete directory transaction.
///
/// The destination must be absolute, its parent must already exist, and no entry may already
/// occupy it. Each version is written and synced in a same-parent staging directory before an
/// atomic no-replace rename makes the complete set visible. Dropping after any earlier failure
/// removes the staging directory.
pub fn publish_reconstructed_versions<I>(
    versions: I,
    destination: &Path,
) -> Result<RecoveryPublicationReceipt>
where
    I: IntoIterator<Item = ReconstructedFile>,
{
    let parent = recovery_publication_parent(destination)?;
    let mut versions = versions.into_iter();
    let first = versions
        .next()
        .ok_or_else(|| anyhow!("cannot publish an empty reconstructed-version directory"))?;

    let staging = StagedDirectory::begin(parent, "recovery")?;
    let mut files = Vec::new();
    for reconstructed in std::iter::once(first).chain(versions) {
        let name = versioned_output_name(&reconstructed);
        staging.write(&name, reconstructed.content.as_bytes())?;
        files.push(destination.join(name));
    }
    staging.publish(destination)?;
    Ok(RecoveryPublicationReceipt {
        destination: destination.to_path_buf(),
        files,
    })
}

/// Reconstruct one selected version without performing filesystem I/O.
pub fn reconstruct_query(
    db: &Db,
    file: &str,
    query: &FileQuery,
    version: Option<usize>,
) -> Result<ReconstructedFile> {
    let (session_id, provider, edits) = selected_edit_group(db, file, query)?;
    let selected = version.unwrap_or(edits.len());
    if selected == 0 || selected > edits.len() {
        bail!(
            "version {selected} is out of range for '{file}'; expected 1..={}",
            edits.len()
        );
    }
    let content = reconstruct(&edits, selected).ok_or_else(|| {
        anyhow!(
            "cannot reconstruct version {selected} of '{file}': no complete replay path exists; a full-content base may be missing or an intervening edit may be path-only"
        )
    })?;
    Ok(ReconstructedFile {
        session_id,
        provider,
        version: selected,
        file_path: edits[selected - 1].file_path.clone(),
        content,
    })
}

fn selected_edit_group(
    db: &Db,
    file: &str,
    query: &FileQuery,
) -> Result<(String, Provider, Vec<FileEdit>)> {
    let mut groups = group_by_session(db.file_edits_for_query(file, query)?);
    if groups.is_empty() {
        bail!("no file edits found for '{file}'");
    }
    if groups.len() > 1 {
        bail!(
            "file '{file}' matched {} sessions; set an exact session_id before reconstruction",
            groups.len()
        );
    }
    groups
        .pop()
        .ok_or_else(|| anyhow!("reconstruction selection unexpectedly became empty"))
}

/// A lazy, causal reconstruction of every version with a complete replay path.
///
/// The iterator owns the selected edit rows, so it does not retain a database borrow or lock.
/// Versions before the first full snapshot and versions invalidated by path-only edits are
/// omitted; their original version numbers remain visible as gaps. Replay work is linear in the
/// edit sequence and live content memory is proportional to one recovered version.
#[derive(Debug)]
pub struct ReconstructedFileVersions {
    session_id: String,
    provider: Provider,
    edits: std::vec::IntoIter<FileEdit>,
    content: Option<String>,
    version: usize,
}

impl Iterator for ReconstructedFileVersions {
    type Item = ReconstructedFile;

    fn next(&mut self) -> Option<Self::Item> {
        for edit in self.edits.by_ref() {
            self.version += 1;
            advance_reconstruction(&mut self.content, &edit);
            if let Some(content) = &self.content {
                return Some(ReconstructedFile {
                    session_id: self.session_id.clone(),
                    provider: self.provider,
                    version: self.version,
                    file_path: edit.file_path,
                    content: content.clone(),
                });
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.edits.len()))
    }
}

impl std::iter::FusedIterator for ReconstructedFileVersions {}

/// Prepare lazy reconstruction without performing filesystem I/O.
pub fn reconstruct_versions_query(
    db: &Db,
    file: &str,
    query: &FileQuery,
) -> Result<ReconstructedFileVersions> {
    let (session_id, provider, edits) = selected_edit_group(db, file, query)?;
    if !edits.iter().any(|edit| edit.new_content.is_some()) {
        bail!(
            "cannot reconstruct any version of '{file}': no full-content base exists in the selected edit history"
        );
    }
    Ok(ReconstructedFileVersions {
        session_id,
        provider,
        edits: edits.into_iter(),
        content: None,
        version: 0,
    })
}

/// Return every reconstructable or path-only file version grouped by session.
/// Version numbers are 1-based and preserve provider event order.
pub fn history(db: &Db, file: &str, query: &FileQuery) -> Result<Vec<FileVersion>> {
    let groups = group_by_session(db.file_edits_for_query(file, query)?);
    let mut versions = Vec::new();
    for (session_id, provider, edits) in &groups {
        let line_counts = version_line_counts(edits);
        for (index, edit) in edits.iter().enumerate() {
            versions.push(FileVersion {
                session_id: session_id.clone(),
                provider: *provider,
                version: (index + 1) as i64,
                tool: edit.tool.clone(),
                ts: edit.ts,
                lines: line_counts[index],
                file_path: edit.file_path.clone(),
            });
        }
    }
    if versions.is_empty() {
        bail!("no file edits found for '{file}'");
    }
    let page = versions
        .into_iter()
        .skip(query.offset)
        .take(if query.limit == 0 {
            usize::MAX
        } else {
            query.limit
        })
        .collect();
    Ok(page)
}

impl Row for FileEditSummary {
    fn headers() -> &'static [&'static str] {
        &["file", "edits", "sessions", "last_edited"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.file_path.clone(),
            self.edits.to_string(),
            self.sessions.to_string(),
            self.last_edited
                .map(|ts| ts.to_rfc3339())
                .unwrap_or_default(),
        ]
    }
}

impl Row for FileVersion {
    fn headers() -> &'static [&'static str] {
        &["session", "version", "tool", "ts", "lines", "file"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.session_id.clone(),
            self.version.to_string(),
            self.tool.clone(),
            self.ts.map(|ts| ts.to_rfc3339()).unwrap_or_default(),
            self.lines.to_string(),
            self.file_path.clone(),
        ]
    }
}

impl Row for FileCrossRef {
    fn headers() -> &'static [&'static str] {
        &["file", "session", "provider", "edits"]
    }
    fn cells(&self) -> Vec<String> {
        vec![
            self.file_path.clone(),
            self.session_id.clone(),
            self.provider.as_str().to_string(),
            self.edits.to_string(),
        ]
    }
}

#[derive(Debug, Subcommand)]
pub enum FilesCmd {
    /// List files edited via tool calls, with edit/session counts.
    Search(FilesSearchArgs),
    /// Show the ordered versions of one file (per session).
    ///
    /// Use `aise files extract` to reconstruct one of these versions, or `aise files search` to find the path.
    History(FilesHistoryArgs),
    /// Show which sessions edited which files.
    CrossRef(FilesCrossRefArgs),
    /// Reconstruct (and optionally restore) a historical version of a file.
    ///
    /// Use `aise files history` first to pick the version you want.
    Extract(FilesExtractArgs),
}

#[derive(Debug, Args, Clone, Default)]
pub struct FileScopeArgs {
    /// Restrict to one indexed session source. Omit to include all nine.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,
    /// Exact session id or unique prefix. Use this when chaining from session/message output.
    /// Omit to include every session.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Restrict to sessions whose cwd, repo root, or transcript path starts with this path prefix.
    /// Omit to search every allowed root.
    #[arg(long)]
    pub path: Option<String>,
}

impl FileScopeArgs {
    fn resolved_query(&self, db: &Db) -> Result<FileQuery> {
        Ok(FileQuery {
            provider: self.provider,
            session_id: resolve_session_id(db, self.session_id.as_deref())?,
            path_prefix: self.path.as_deref().map(crate::util::normalize_path_prefix),
            ..Default::default()
        })
    }
}

#[derive(Debug, Args)]
pub struct FilesSearchArgs {
    /// Glob over the basename (`*.rs`), or the full path when it contains `/`. Omit to list
    /// every edited file.
    #[arg(value_name = "PATTERN")]
    pub pattern: Option<String>,
    #[command(flatten)]
    pub scope: FileScopeArgs,
    /// Only files with at least this many edits. Omit for no lower bound on edit count.
    #[arg(long)]
    pub min_edits: Option<i64>,
    /// Only files with at most this many edits. Omit for no upper bound on edit count.
    #[arg(long)]
    pub max_edits: Option<i64>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Skip this many rows in deterministic result order.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct FilesHistoryArgs {
    /// File basename or path (e.g. `db.rs` or `src/db.rs`).
    pub file: String,
    #[command(flatten)]
    pub scope: FileScopeArgs,
    /// Max versions to return. 0 = unlimited.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Skip this many versions in deterministic session/version order.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct FilesCrossRefArgs {
    /// Glob over the basename, or the full path when it contains `/`. Omit to cross-reference
    /// every edited file.
    #[arg(value_name = "PATTERN")]
    pub pattern_arg: Option<String>,
    /// Glob over the basename, or the full path when it contains `/`. Omit to cross-reference
    /// every edited file.
    #[arg(long)]
    pub pattern: Option<String>,
    #[command(flatten)]
    pub scope: FileScopeArgs,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    /// Skip this many rows in deterministic result order.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct FilesExtractArgs {
    /// File basename or path to reconstruct.
    pub file: String,
    /// 1-based version to reconstruct. Default = latest.
    #[arg(long, short, conflicts_with = "all")]
    pub version: Option<usize>,
    /// Reconstruct every version with a complete replay path. Streams versions to stdout, or
    /// atomically publishes a new non-replacing directory when --output-dir is present.
    #[arg(long, conflicts_with = "restore")]
    pub all: bool,
    /// Multi-version stdout format. Valid only with --all. Default: framed.
    #[arg(long, value_enum, requires = "all")]
    pub format: Option<ReconstructedVersionsFormat>,
    #[command(flatten)]
    pub scope: FileScopeArgs,
    /// Write the reconstructed content to a collision-safe `.recovered` sibling
    /// (never overwrites) instead of printing to stdout.
    #[arg(long, conflicts_with = "output_dir")]
    pub restore: bool,
    /// For one version, a directory to write into. With --all, the new directory to atomically
    /// publish; it must not already exist. Omit to write to the current directory.
    #[arg(long, short)]
    pub output_dir: Option<PathBuf>,
    /// Report what would happen without printing content or writing files.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(db: &Db, cmd: &FilesCmd) -> Result<()> {
    match cmd {
        FilesCmd::Search(args) => {
            let (since, until) = args.dates.resolve_now()?;
            let query = FileQuery {
                pattern: args.pattern.clone(),
                since,
                until,
                min_edits: args.min_edits,
                max_edits: args.max_edits,
                limit: args.limit,
                offset: args.offset,
                ..args.scope.resolved_query(db)?
            };
            emit(&db.file_search(&query)?, args.format)
        }
        FilesCmd::CrossRef(args) => {
            let (since, until) = args.dates.resolve_now()?;
            let pattern = match (&args.pattern_arg, &args.pattern) {
                (Some(positional), Some(flag)) if positional != flag => {
                    anyhow::bail!(
                        "positional PATTERN and --pattern disagree: {positional:?} != {flag:?}"
                    );
                }
                (Some(positional), _) => Some(positional.clone()),
                (_, Some(flag)) => Some(flag.clone()),
                (None, None) => None,
            };
            let query = FileQuery {
                pattern,
                since,
                until,
                limit: args.limit,
                offset: args.offset,
                ..args.scope.resolved_query(db)?
            };
            emit(&db.file_cross_ref(&query)?, args.format)
        }
        FilesCmd::History(args) => {
            let query = FileQuery {
                limit: args.limit,
                offset: args.offset,
                ..args.scope.resolved_query(db)?
            };
            let versions = history(db, &args.file, &query)?;
            emit(&versions, args.format)
        }
        FilesCmd::Extract(args) => run_extract(db, args),
    }
}

fn resolve_session_id(db: &Db, session_id: Option<&str>) -> Result<Option<String>> {
    session_id
        .map(|id| db.resolve_session_record(id).map(|session| session.id))
        .transpose()
}

fn file_edits_for_scope(
    db: &Db,
    file: &str,
    scope: &FileScopeArgs,
) -> Result<Vec<(String, Provider, FileEdit)>> {
    let query = scope.resolved_query(db)?;
    db.file_edits_for_query(file, &query)
}

fn run_extract(db: &Db, args: &FilesExtractArgs) -> Result<()> {
    if args.all {
        return run_extract_all(db, args);
    }
    let mut groups = group_by_session(file_edits_for_scope(db, &args.file, &args.scope)?);
    let (session_id, provider, edits) = match groups.len() {
        0 => bail!("no file edits found for '{}'", args.file),
        1 => groups.remove(0),
        n => {
            let ids: Vec<String> = groups.into_iter().map(|(sid, _, _)| sid).collect();
            bail!(
                "'{}' was edited in {n} sessions ({}); pass --session-id with an exact id or unique prefix",
                args.file,
                ids.join(", ")
            );
        }
    };

    let version = args.version.unwrap_or(edits.len());
    if version == 0 || version > edits.len() {
        bail!(
            "version {version} out of range for '{}' (1..={})",
            args.file,
            edits.len()
        );
    }
    let content = reconstruct(&edits, version).ok_or_else(|| {
        anyhow!(
            "cannot reconstruct '{}' v{version}: no complete replay path exists (missing full snapshot or intervening path-only edit)",
            args.file
        )
    })?;
    let original_path = edits[version - 1].file_path.clone();
    let original = PathBuf::from(&original_path);
    let lines = count_lines(&content);

    // Decide whether we are writing a file or printing to stdout.
    let writing = args.restore || args.output_dir.is_some();
    if !writing {
        if args.dry_run {
            println!(
                "session {session_id}: '{}' v{version}/{} ({lines} lines) — dry run, not printed",
                args.file,
                edits.len()
            );
            return Ok(());
        }
        let stdout = io::stdout();
        let mut out = stdout.lock();
        out.write_all(content.as_bytes())?;
        if !content.ends_with('\n') {
            writeln!(out)?;
        }
        return Ok(());
    }

    // Build a collision-safe target (never overwrites an existing file).
    let base = match &args.output_dir {
        Some(dir) => dir.join(safe_output_name(&original, &args.file)),
        None => {
            // In-place restore beside the session-recorded original: reject `..` escapes.
            ensure_safe_restore_target(&original)?;
            original.clone()
        }
    };
    let target = restore_target(&base, |path| path.exists());

    if args.dry_run {
        println!(
            "session {session_id}: would restore '{}' v{version}/{} ({lines} lines) -> {}",
            args.file,
            edits.len(),
            target.display()
        );
        return Ok(());
    }
    let reconstructed = ReconstructedFile {
        session_id,
        provider,
        version,
        file_path: original_path,
        content,
    };
    let target = restore_reconstructed(&reconstructed, args.output_dir.as_deref())?;
    println!(
        "{}",
        restored_file_receipt(
            &args.file,
            version,
            edits.len(),
            lines,
            &target,
            reconstructed.content.as_bytes(),
        )
    );
    Ok(())
}

/// Render one successful recovery receipt with a digest of the exact published bytes.
/// For `B` content bytes this adds O(B) SHA-256 work, O(1) hashing state, and one serial in-memory
/// pass after reconstruction/publication; filesystem write complexity and collision safety do not
/// change.
fn restored_file_receipt(
    requested_file: &str,
    version: usize,
    version_count: usize,
    lines: i64,
    destination: &Path,
    content: &[u8],
) -> String {
    let digest = crate::hashing::sha256(content);
    format!(
        "restored '{requested_file}' v{version}/{version_count} ({lines} lines) -> {} (sha256:{digest})",
        destination.display()
    )
}

fn run_extract_all(db: &Db, args: &FilesExtractArgs) -> Result<()> {
    let query = args.scope.resolved_query(db)?;
    let versions = reconstruct_versions_query(db, &args.file, &query)?;

    if let Some(output_dir) = &args.output_dir {
        let destination = if output_dir.is_absolute() {
            output_dir.clone()
        } else {
            std::env::current_dir()
                .context("failed to resolve current directory for --output-dir")?
                .join(output_dir)
        };
        recovery_publication_parent(&destination)?;
        if args.dry_run {
            let count = versions.count();
            eprintln!(
                "would atomically publish {count} reconstructable versions of '{}' to {}",
                args.file,
                destination.display()
            );
            return Ok(());
        }
        let receipt = publish_reconstructed_versions(versions, &destination)?;
        eprintln!(
            "published {} reconstructable versions of '{}' to {}",
            receipt.files.len(),
            args.file,
            receipt.destination.display()
        );
        return Ok(());
    }

    if args.dry_run {
        let count = versions.count();
        eprintln!(
            "would stream {count} reconstructable versions of '{}' to stdout",
            args.file
        );
        return Ok(());
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    write_reconstructed_versions(
        versions,
        &args.file,
        args.format.unwrap_or_default(),
        &mut out,
    )?;
    out.flush()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ReconstructedVersionsFormat {
    #[default]
    Framed,
    Jsonl,
}

fn write_reconstructed_versions<I, W>(
    versions: I,
    requested_file: &str,
    format: ReconstructedVersionsFormat,
    out: &mut W,
) -> Result<usize>
where
    I: IntoIterator<Item = ReconstructedFile>,
    W: io::Write,
{
    let escaped_file = requested_file.escape_default().to_string();
    let mut count = 0;
    for reconstructed in versions {
        match format {
            ReconstructedVersionsFormat::Framed => {
                if count > 0 {
                    writeln!(out)?;
                }
                writeln!(
                    out,
                    "=== {escaped_file} v{} session={} provider={} ===",
                    reconstructed.version,
                    reconstructed.session_id.escape_default(),
                    reconstructed.provider.as_str()
                )?;
                out.write_all(reconstructed.content.as_bytes())?;
                if !reconstructed.content.ends_with('\n') {
                    writeln!(out)?;
                }
            }
            ReconstructedVersionsFormat::Jsonl => {
                serde_json::to_writer(&mut *out, &reconstructed)
                    .context("failed to serialize a reconstructed version as JSONL")?;
                writeln!(out)?;
            }
        }
        count += 1;
    }
    Ok(count)
}

fn emit<T: Serialize + Row>(rows: &[T], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render(rows, format, &mut out)?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        cmd: FilesCmd,
    }

    fn write(seq: i64, content: &str) -> FileEdit {
        FileEdit {
            seq,
            ts: None,
            tool: "Write".into(),
            file_path: "/repo/src/db.rs".into(),
            file_name: "db.rs".into(),
            new_content: Some(content.into()),
            edits: Vec::new(),
        }
    }

    fn recon(version: usize, content: &str) -> ReconstructedFile {
        ReconstructedFile {
            session_id: "claude:s1".to_string(),
            provider: Provider::Claude,
            version,
            file_path: "src/a.rs".to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn write_reconstructed_versions_framed_and_jsonl_serialize_each_version() {
        let versions = || vec![recon(1, "first content"), recon(2, "second content")];

        // Framed: one header line per version, a blank line between entries, and
        // each content block terminated by a newline.
        let mut buf = Vec::new();
        let n = write_reconstructed_versions(
            versions(),
            "src/a.rs",
            ReconstructedVersionsFormat::Framed,
            &mut buf,
        )
        .unwrap();
        assert_eq!(n, 2);
        let framed = String::from_utf8(buf).unwrap();
        assert!(framed.contains("=== src/a.rs v1 session=claude:s1 provider=claude ==="));
        assert!(framed.contains("=== src/a.rs v2 session=claude:s1 provider=claude ==="));
        assert!(framed.contains("first content\n"));
        assert!(framed.contains("\n\n=== src/a.rs v2")); // blank line separates entries

        // Jsonl: one JSON object per line carrying the version and content.
        let mut buf = Vec::new();
        let n = write_reconstructed_versions(
            versions(),
            "src/a.rs",
            ReconstructedVersionsFormat::Jsonl,
            &mut buf,
        )
        .unwrap();
        assert_eq!(n, 2);
        let jsonl = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["session_id"], "claude:s1");
            assert!(v["version"].is_number());
            assert!(v["content"].is_string());
        }
    }

    #[test]
    fn create_recovery_file_persists_content_and_drop_removes_unpersisted() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("recovered.txt");

        // restore_target never overwrites the original: the first recovery file is
        // "<stem>.recovered.<ext>", so it persists content beside the original.
        let pending = create_recovery_file(&base).unwrap();
        assert_eq!(
            pending.path.file_name().unwrap().to_str().unwrap(),
            "recovered.recovered.txt"
        );
        let path = pending.persist(b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");

        // The next recovery file for the same base avoids the collision with a
        // numbered marker, and dropping it without persisting removes the reserved file.
        let pending2 = create_recovery_file(&base).unwrap();
        assert_eq!(
            pending2.path.file_name().unwrap().to_str().unwrap(),
            "recovered.recovered_2.txt"
        );
        let reserved = pending2.path.clone();
        assert!(reserved.exists());
        drop(pending2);
        assert!(!reserved.exists());
    }

    fn edit(seq: i64, pairs: &[(&str, &str)]) -> FileEdit {
        FileEdit {
            seq,
            ts: None,
            tool: if pairs.len() > 1 { "MultiEdit" } else { "Edit" }.into(),
            file_path: "/repo/src/db.rs".into(),
            file_name: "db.rs".into(),
            new_content: None,
            edits: pairs.iter().map(|(o, n)| EditOp::new(*o, *n)).collect(),
        }
    }

    fn path_only(seq: i64) -> FileEdit {
        FileEdit {
            seq,
            ts: None,
            tool: "apply_patch".into(),
            file_path: "/repo/src/db.rs".into(),
            file_name: "db.rs".into(),
            new_content: None,
            edits: Vec::new(),
        }
    }

    fn edit_all(seq: i64, pairs: &[(&str, &str)]) -> FileEdit {
        FileEdit {
            seq,
            ts: None,
            tool: if pairs.len() > 1 { "MultiEdit" } else { "Edit" }.into(),
            file_path: "/repo/src/db.rs".into(),
            file_name: "db.rs".into(),
            new_content: None,
            edits: pairs
                .iter()
                .map(|(o, n)| EditOp {
                    old: (*o).into(),
                    new: (*n).into(),
                    replace_all: true,
                })
                .collect(),
        }
    }

    #[test]
    fn cross_ref_accepts_positional_pattern_and_pattern_option() {
        assert!(
            TestCli::try_parse_from(["sg", "cross-ref", "Glossary-and-Definitions.md"]).is_ok()
        );
        assert!(TestCli::try_parse_from([
            "sg",
            "cross-ref",
            "--pattern",
            "Glossary-and-Definitions.md",
        ])
        .is_ok());
    }

    #[test]
    fn cross_ref_rejects_disagreeing_positional_and_pattern_option() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let args = FilesCrossRefArgs {
            pattern_arg: Some("a.rs".into()),
            pattern: Some("b.rs".into()),
            scope: FileScopeArgs::default(),
            dates: DateRange::default(),
            limit: 0,
            offset: 0,
            format: OutputFormat::Json,
        };
        assert!(run(&db, &FilesCmd::CrossRef(args)).is_err());
    }

    #[test]
    fn file_commands_accept_exact_session_id_scope() {
        for args in [
            vec!["sg", "search", "--session-id", "claude:abc"],
            vec!["sg", "cross-ref", "--session-id", "claude:abc"],
            vec!["sg", "history", "db.rs", "--session-id", "claude:abc"],
            vec!["sg", "extract", "db.rs", "--session-id", "claude:abc"],
        ] {
            assert!(TestCli::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn file_commands_accept_provider_and_path_scope() {
        for args in [
            vec!["sg", "search", "--provider", "claude", "--path", "/repo"],
            vec!["sg", "cross-ref", "--provider", "codex", "--path", "/repo"],
            vec![
                "sg",
                "history",
                "db.rs",
                "--provider",
                "cursor",
                "--path",
                "/repo",
            ],
            vec![
                "sg",
                "extract",
                "db.rs",
                "--provider",
                "pi",
                "--path",
                "/repo",
            ],
        ] {
            assert!(TestCli::try_parse_from(args).is_ok());
        }
    }

    #[test]
    fn file_commands_reject_removed_substring_session_scope() {
        assert!(TestCli::try_parse_from(["sg", "search", "--session", "abc"]).is_err());
        assert!(TestCli::try_parse_from(["sg", "extract", "db.rs", "--session", "abc",]).is_err());
    }

    #[test]
    fn extract_all_has_unambiguous_destination_and_version_options() {
        assert!(TestCli::try_parse_from(["sg", "extract", "db.rs", "--all"]).is_ok());
        assert!(TestCli::try_parse_from([
            "sg",
            "extract",
            "db.rs",
            "--all",
            "--output-dir",
            "versions",
        ])
        .is_ok());
        assert!(
            TestCli::try_parse_from(["sg", "extract", "db.rs", "--all", "--format", "jsonl",])
                .is_ok()
        );
        for args in [
            vec!["sg", "extract", "db.rs", "--all", "--version", "2"],
            vec!["sg", "extract", "db.rs", "--all", "--restore"],
            vec!["sg", "extract", "db.rs", "--restore", "--output-dir", "out"],
            vec!["sg", "extract", "db.rs", "--format", "jsonl"],
        ] {
            assert!(TestCli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn framed_version_stream_preserves_content_and_original_version_gaps() {
        let versions = vec![
            ReconstructedFile {
                session_id: "claude:one".into(),
                provider: Provider::Claude,
                version: 1,
                file_path: "/repo/db.rs".into(),
                content: "one\n".into(),
            },
            ReconstructedFile {
                session_id: "claude:one".into(),
                provider: Provider::Claude,
                version: 3,
                file_path: "/repo/db.rs".into(),
                content: "three".into(),
            },
        ];
        let mut output = Vec::new();
        assert_eq!(
            write_reconstructed_versions(
                versions,
                "db\n.rs",
                ReconstructedVersionsFormat::Framed,
                &mut output,
            )
            .unwrap(),
            2
        );
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "=== db\\n.rs v1 session=claude:one provider=claude ===\none\n\n=== db\\n.rs v3 session=claude:one provider=claude ===\nthree\n"
        );
    }

    #[test]
    fn jsonl_version_stream_is_unambiguous_and_round_trips_content() {
        let version = ReconstructedFile {
            session_id: "claude:one".into(),
            provider: Provider::Claude,
            version: 2,
            file_path: "/repo/db.rs".into(),
            content: "=== db.rs v9 ===\nembedded header\n".into(),
        };
        let mut output = Vec::new();
        assert_eq!(
            write_reconstructed_versions(
                [version.clone()],
                "ignored.rs",
                ReconstructedVersionsFormat::Jsonl,
                &mut output,
            )
            .unwrap(),
            1
        );
        assert_eq!(
            serde_json::from_slice::<ReconstructedFile>(&output).unwrap(),
            version
        );
    }

    #[test]
    fn recovery_publication_rejects_empty_and_duplicate_versions_without_residue() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("versions");
        let empty_error =
            publish_reconstructed_versions(std::iter::empty::<ReconstructedFile>(), &destination)
                .unwrap_err();
        assert!(empty_error.to_string().contains("empty"));
        assert_eq!(dir.path().read_dir().unwrap().count(), 0);

        let duplicate = ReconstructedFile {
            session_id: "claude:one".into(),
            provider: Provider::Claude,
            version: 1,
            file_path: "/repo/db.rs".into(),
            content: "complete".into(),
        };
        let duplicate_error =
            publish_reconstructed_versions([duplicate.clone(), duplicate], &destination)
                .unwrap_err();
        assert!(duplicate_error.to_string().contains("staged artifact"));
        assert_eq!(dir.path().read_dir().unwrap().count(), 0);
    }

    #[test]
    fn reconstruct_replace_all_replaces_every_occurrence() {
        // Edit with replace_all=true must replace ALL occurrences, not just the first.
        let edits = vec![
            write(0, "foo bar foo baz foo"),
            edit_all(1, &[("foo", "X")]),
        ];
        assert_eq!(reconstruct(&edits, 2).as_deref(), Some("X bar X baz X"));
    }

    #[test]
    fn reconstruct_edit_without_replace_all_replaces_first_only() {
        // Default (replace_all=false) replaces only the first occurrence (Edit requires
        // old_string be unique; first == only).
        let edits = vec![write(0, "foo bar foo"), edit(1, &[("foo", "X")])];
        assert_eq!(reconstruct(&edits, 2).as_deref(), Some("X bar foo"));
    }

    #[test]
    fn reconstruct_replays_write_then_edit() {
        let edits = vec![write(0, "a\nb\nc"), edit(1, &[("b", "B")])];
        assert_eq!(reconstruct(&edits, 1).as_deref(), Some("a\nb\nc"));
        assert_eq!(reconstruct(&edits, 2).as_deref(), Some("a\nB\nc"));
    }

    #[test]
    fn reconstruct_all_versions_skips_pre_snapshot_deltas_and_replays_once() {
        let edits = vec![
            edit(0, &[("missing", "ignored")]),
            write(1, "alpha"),
            edit(2, &[("alpha", "beta")]),
            write(3, "reset"),
        ];
        let versions = ReconstructedFileVersions {
            session_id: "claude:a".into(),
            provider: Provider::Claude,
            edits: edits.into_iter(),
            content: None,
            version: 0,
        }
        .collect::<Vec<_>>();
        assert_eq!(
            versions
                .iter()
                .map(|version| (version.version, version.content.as_str()))
                .collect::<Vec<_>>(),
            [(2, "alpha"), (3, "beta"), (4, "reset")]
        );
    }

    #[test]
    fn path_only_edit_invalidates_reconstruction_until_next_snapshot() {
        let edits = vec![
            write(0, "known"),
            path_only(1),
            edit(2, &[("known", "cannot be trusted")]),
            write(3, "reset"),
        ];

        assert_eq!(reconstruct(&edits, 1).as_deref(), Some("known"));
        assert_eq!(reconstruct(&edits, 2), None);
        assert_eq!(reconstruct(&edits, 3), None);
        assert_eq!(reconstruct(&edits, 4).as_deref(), Some("reset"));
        assert_eq!(version_line_counts(&edits), [1, 0, 0, 1]);
        let versions = ReconstructedFileVersions {
            session_id: "codex:a".into(),
            provider: Provider::Codex,
            edits: edits.into_iter(),
            content: None,
            version: 0,
        }
        .collect::<Vec<_>>();
        assert_eq!(
            versions
                .iter()
                .map(|version| (version.version, version.content.as_str()))
                .collect::<Vec<_>>(),
            [(1, "known"), (4, "reset")]
        );
    }

    #[test]
    fn reconstruct_replays_multiedit() {
        let edits = vec![write(0, "x y z"), edit(1, &[("x", "1"), ("z", "9")])];
        assert_eq!(reconstruct(&edits, 2).as_deref(), Some("1 y 9"));
    }

    #[test]
    fn reconstruct_uses_latest_write_as_base() {
        // A second Write overwrites; later edits apply on top of it, not the first.
        let edits = vec![
            write(0, "old content"),
            write(1, "fresh\ncontent"),
            edit(2, &[("fresh", "FRESH")]),
        ];
        assert_eq!(reconstruct(&edits, 3).as_deref(), Some("FRESH\ncontent"));
    }

    #[test]
    fn reconstruct_without_write_base_is_none() {
        // Deltas alone cannot rebuild full content.
        let edits = vec![edit(0, &[("a", "b")])];
        assert_eq!(reconstruct(&edits, 1), None);
    }

    #[test]
    fn reconstruct_out_of_range_is_none() {
        let edits = vec![write(0, "x")];
        assert_eq!(reconstruct(&edits, 0), None);
        assert_eq!(reconstruct(&edits, 2), None);
    }

    #[test]
    fn reconstruct_missing_old_string_invalidates_until_the_next_full_snapshot() {
        let edits = vec![write(0, "a\nb"), edit(1, &[("zzz", "Z")])];
        assert_eq!(
            reconstruct(&edits, 2),
            None,
            "a replay mismatch must not publish bytes that were never proven to exist"
        );
    }

    #[test]
    fn restore_target_avoids_collisions() {
        let original = Path::new("/repo/src/db.rs");
        // Nothing exists → first candidate.
        let first = restore_target(original, |_| false);
        assert_eq!(first, Path::new("/repo/src/db.recovered.rs"));
        // `.recovered.rs` taken → bump to `_2`.
        let taken = Path::new("/repo/src/db.recovered.rs");
        let second = restore_target(original, |p| p == taken);
        assert_eq!(second, Path::new("/repo/src/db.recovered_2.rs"));
    }

    #[test]
    fn restore_rejects_parent_dir_traversal() {
        // `..` in a session-recorded path must be refused (could escape the tree).
        assert!(ensure_safe_restore_target(Path::new("../../etc/cron.d/evil")).is_err());
        assert!(ensure_safe_restore_target(Path::new("/repo/../../etc/x")).is_err());
        // Normal cases: absolute path to the user's own file, or a clean relative path.
        assert!(ensure_safe_restore_target(Path::new("/Users/me/proj/src/db.rs")).is_ok());
        assert!(ensure_safe_restore_target(Path::new("src/db.rs")).is_ok());
    }

    #[test]
    fn restored_receipt_names_destination_and_content_checksum() {
        let receipt = restored_file_receipt(
            "src/lib.rs",
            2,
            4,
            3,
            Path::new("/repo/src/lib.recovered.rs"),
            b"one\ntwo\nthree\n",
        );
        assert_eq!(
            receipt,
            format!(
                "restored 'src/lib.rs' v2/4 (3 lines) -> /repo/src/lib.recovered.rs (sha256:{})",
                crate::hashing::sha256(b"one\ntwo\nthree\n")
            )
        );
    }

    #[test]
    fn safe_output_name_cannot_escape_output_dir() {
        // Normal case: the basename of the session-recorded path.
        assert_eq!(
            safe_output_name(Path::new("/home/u/proj/src/main.rs"), "main.rs"),
            PathBuf::from("main.rs")
        );
        // Recorded path ends in `..` (file_name None) → fall back to the --file basename.
        assert_eq!(
            safe_output_name(Path::new("foo/.."), "evil.rs"),
            PathBuf::from("evil.rs")
        );
        // A traversal/absolute --file arg is reduced to its bare last component.
        assert_eq!(
            safe_output_name(Path::new(".."), "../../etc/passwd"),
            PathBuf::from("passwd")
        );
        // Neither source yields a name → literal fallback (still a single component).
        assert_eq!(
            safe_output_name(Path::new(".."), ".."),
            PathBuf::from("recovered")
        );
        // The joined result always stays inside the chosen output dir.
        let joined = Path::new("/out").join(safe_output_name(Path::new(".."), "../../etc/passwd"));
        assert_eq!(joined, PathBuf::from("/out/passwd"));
    }

    #[test]
    fn restore_target_handles_no_extension() {
        let original = Path::new("/repo/Makefile");
        assert_eq!(
            restore_target(original, |_| false),
            Path::new("/repo/Makefile.recovered")
        );
    }

    #[test]
    fn incremental_line_counts_match_per_version_reconstruct() {
        // The O(n) one-pass counts must equal the O(n^2) per-version reconstruct, including
        // a re-Write mid-history and a delta that changes the line count.
        let edits = vec![
            write(0, "a\nb\nc"),
            edit(1, &[("b", "B")]),
            write(2, "x"),
            edit(3, &[("x", "X\nY\nZ")]),
        ];
        let incremental = version_line_counts(&edits);
        let reference: Vec<i64> = (1..=edits.len())
            .map(|v| reconstruct(&edits, v).map(|c| count_lines(&c)).unwrap_or(0))
            .collect();
        assert_eq!(incremental, reference);
        assert_eq!(incremental, vec![3, 3, 1, 3]);
    }

    #[test]
    fn version_line_counts_zero_before_first_write() {
        // Edit-only prefix has no full-content base → 0 until a Write appears.
        let edits = vec![
            edit(0, &[("a", "b")]),
            write(1, "one\ntwo"),
            edit(2, &[("one", "1")]),
        ];
        assert_eq!(version_line_counts(&edits), vec![0, 2, 2]);
    }

    #[test]
    fn group_by_session_numbers_versions_in_order() {
        let rows = vec![
            ("claude:a".into(), Provider::Claude, write(0, "v1")),
            (
                "claude:a".into(),
                Provider::Claude,
                edit(1, &[("v1", "v2")]),
            ),
            ("claude:b".into(), Provider::Claude, write(0, "other")),
        ];
        let groups = group_by_session(rows);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "claude:a");
        assert_eq!(groups[0].2.len(), 2, "session a has two ordered versions");
        assert_eq!(groups[1].0, "claude:b");
        assert_eq!(groups[1].2.len(), 1);
    }
}
