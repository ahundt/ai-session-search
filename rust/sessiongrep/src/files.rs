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

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::dates::DateRange;
use crate::db::Db;
use crate::models::{
    EditOp, FileCrossRef, FileEdit, FileEditSummary, FileQuery, FileVersion, Provider,
};
use crate::render::{render, OutputFormat, Row};

/// Reconstruct a file's content as of 1-based `version` by replaying edits forward
/// from the most recent full `Write` snapshot at or before the target.
///
/// Returns `None` when no `Write` exists at or before `version` (deltas alone cannot
/// rebuild full content) or when `version` is out of range. Missing `old_string`s are
/// skipped best-effort (tolerant replay).
pub fn reconstruct(edits: &[FileEdit], version: usize) -> Option<String> {
    if version == 0 || version > edits.len() {
        return None;
    }
    let target = version - 1;
    // Latest full snapshot (a `Write`, which sets `new_content`) at or before target.
    let base = (0..=target)
        .rev()
        .find(|&i| edits[i].new_content.is_some())?;
    let mut content = edits[base].new_content.clone().unwrap_or_default();
    for edit in &edits[base + 1..=target] {
        apply_edits(&mut content, &edit.edits);
    }
    Some(content)
}

/// Apply replacements in order. A `replace_all` op replaces every occurrence; otherwise
/// only the first (== only, for a unique non-`replace_all` Edit). Empty and not-found
/// `old` strings are skipped (best-effort).
fn apply_edits(content: &mut String, edits: &[EditOp]) {
    for op in edits {
        if op.old.is_empty() {
            continue;
        }
        if op.replace_all {
            if content.contains(op.old.as_str()) {
                *content = content.replace(op.old.as_str(), &op.new);
            }
        } else if let Some(pos) = content.find(op.old.as_str()) {
            content.replace_range(pos..pos + op.old.len(), &op.new);
        }
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

/// Pick a collision-free recovery path: `<stem>.recovered[.ext]`, then `_2`, `_3`, …
/// until `exists` returns false. The filesystem check is injected so this is pure and
/// unit-testable. Callers must still avoid TOCTOU races on the returned path.
pub fn restore_target<F: Fn(&Path) -> bool>(original: &Path, exists: F) -> PathBuf {
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
            match &edit.new_content {
                Some(full) => content = Some(full.clone()),
                None => {
                    if let Some(current) = content.as_mut() {
                        apply_edits(current, &edit.edits);
                    }
                }
            }
            content.as_deref().map(count_lines).unwrap_or(0)
        })
        .collect()
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
    History(FilesHistoryArgs),
    /// Show which sessions edited which files.
    CrossRef(FilesCrossRefArgs),
    /// Reconstruct (and optionally restore) a historical version of a file.
    Extract(FilesExtractArgs),
}

#[derive(Debug, Args)]
pub struct FilesSearchArgs {
    /// Glob over the basename (`*.rs`), or the full path when it contains `/`. Omit to list
    /// every edited file.
    #[arg(value_name = "PATTERN")]
    pub pattern: Option<String>,
    /// Scope to one session id (substring match).
    #[arg(long)]
    pub session: Option<String>,
    /// Only files with at least this many edits.
    #[arg(long)]
    pub min_edits: Option<i64>,
    /// Only files with at most this many edits.
    #[arg(long)]
    pub max_edits: Option<i64>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct FilesHistoryArgs {
    /// File basename or path (e.g. `db.rs` or `src/db.rs`).
    pub file: String,
    /// Scope to one session id (substring match).
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct FilesCrossRefArgs {
    /// Glob over the basename, or the full path when it contains `/`.
    #[arg(value_name = "PATTERN")]
    pub pattern_arg: Option<String>,
    /// Glob over the basename, or the full path when it contains `/`.
    #[arg(long)]
    pub pattern: Option<String>,
    /// Scope to one session id (substring match).
    #[arg(long)]
    pub session: Option<String>,
    #[command(flatten)]
    pub dates: DateRange,
    /// Max results. 0 = unlimited.
    #[arg(long, default_value_t = 0)]
    pub limit: usize,
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

#[derive(Debug, Args)]
pub struct FilesExtractArgs {
    /// File basename or path to reconstruct.
    pub file: String,
    /// 1-based version to reconstruct. Default = latest.
    #[arg(long, short)]
    pub version: Option<usize>,
    /// Required when the file was edited in more than one session.
    #[arg(long, short)]
    pub session: Option<String>,
    /// Write the reconstructed content to a collision-safe `.recovered` sibling
    /// (never overwrites) instead of printing to stdout.
    #[arg(long)]
    pub restore: bool,
    /// Directory to write the recovered file into (implies a write; default = beside original).
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
                session: args.session.clone(),
                since,
                until,
                min_edits: args.min_edits,
                max_edits: args.max_edits,
                limit: args.limit,
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
                session: args.session.clone(),
                since,
                until,
                limit: args.limit,
                ..Default::default()
            };
            emit(&db.file_cross_ref(&query)?, args.format)
        }
        FilesCmd::History(args) => {
            let groups = group_by_session(db.file_edits_for(&args.file, args.session.as_deref())?);
            let mut versions = Vec::new();
            for (session_id, provider, edits) in &groups {
                // One forward pass for all version line counts (avoids O(n^2) replay).
                let line_counts = version_line_counts(edits);
                for (i, edit) in edits.iter().enumerate() {
                    versions.push(FileVersion {
                        session_id: session_id.clone(),
                        provider: *provider,
                        version: (i + 1) as i64,
                        tool: edit.tool.clone(),
                        ts: edit.ts,
                        lines: line_counts[i],
                        file_path: edit.file_path.clone(),
                    });
                }
            }
            if versions.is_empty() {
                bail!("no file edits found for '{}'", args.file);
            }
            emit(&versions, args.format)
        }
        FilesCmd::Extract(args) => run_extract(db, args),
    }
}

fn run_extract(db: &Db, args: &FilesExtractArgs) -> Result<()> {
    let mut groups = group_by_session(db.file_edits_for(&args.file, args.session.as_deref())?);
    let (session_id, _provider, edits) = match groups.len() {
        0 => bail!("no file edits found for '{}'", args.file),
        1 => groups.remove(0),
        n => {
            let ids: Vec<String> = groups.into_iter().map(|(sid, _, _)| sid).collect();
            bail!(
                "'{}' was edited in {n} sessions ({}); pass --session to choose one",
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
            "cannot reconstruct '{}' v{version}: no Write snapshot at or before it (only deltas)",
            args.file
        )
    })?;
    let original = PathBuf::from(&edits[version - 1].file_path);
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
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, &content)?;
    println!(
        "restored '{}' v{version}/{} ({lines} lines) -> {}",
        args.file,
        edits.len(),
        target.display()
    );
    Ok(())
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
            session: None,
            dates: DateRange::default(),
            limit: 0,
            format: OutputFormat::Json,
        };
        assert!(run(&db, &FilesCmd::CrossRef(args)).is_err());
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
    fn reconstruct_missing_old_string_is_skipped() {
        let edits = vec![write(0, "a\nb"), edit(1, &[("zzz", "Z")])];
        // 'zzz' not present → edit is a no-op, content unchanged.
        assert_eq!(reconstruct(&edits, 2).as_deref(), Some("a\nb"));
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
