use anyhow::Result;
use fd_lock::RwLock;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::db::Db;
use crate::models::{Provider, SourceFile};
use crate::source::ProviderSet;
use crate::util::normalize_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexMode {
    Strict,
    Opportunistic { busy_timeout_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexOutcome {
    Updated {
        files_seen: usize,
        sessions_updated: usize,
    },
    SkippedBusy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoReindexOutcome {
    Updated {
        files_seen: usize,
        sessions_updated: usize,
    },
    SkippedBusy,
    SkippedFresh,
    SkippedLockUnavailable {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("index update lock {path} is unavailable: {source}")]
struct IndexUpdateLockError {
    path: PathBuf,
    #[source]
    source: std::io::Error,
}

pub fn index_update_lock_path(db_path: &Path) -> PathBuf {
    let mut filename = db_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("index.db"))
        .to_os_string();
    filename.push(".update.lock");
    db_path.with_file_name(filename)
}

pub(crate) fn open_index_update_lock(path: &Path) -> Result<RwLock<File>> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| IndexUpdateLockError {
            path: path.to_path_buf(),
            source,
        })?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(IndexUpdateLockError {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "lock path exists and is not a regular file",
                ),
            }
            .into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(source) => {
            return Err(IndexUpdateLockError {
                path: path.to_path_buf(),
                source,
            }
            .into());
        }
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|source| IndexUpdateLockError {
        path: path.to_path_buf(),
        source,
    })?;
    if !file
        .metadata()
        .map_err(|source| IndexUpdateLockError {
            path: path.to_path_buf(),
            source,
        })?
        .is_file()
    {
        return Err(IndexUpdateLockError {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                ErrorKind::InvalidInput,
                "opened lock path is not a regular file",
            ),
        }
        .into());
    }
    Ok(RwLock::new(file))
}

pub fn with_index_update_lock<T>(config: &Config, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = index_update_lock_path(&config.db_path());
    let mut lock = open_index_update_lock(&lock_path)?;
    let _guard = loop {
        match lock.write() {
            Ok(guard) => break guard,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                return Err(IndexUpdateLockError {
                    path: lock_path.clone(),
                    source: err,
                }
                .into());
            }
        }
    };
    f()
}

pub fn auto_reindex(
    config: &Config,
    db: &Db,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<AutoReindexOutcome> {
    if db.auto_reindex_is_fresh(config.index.auto_reindex_interval_ms)? {
        return Ok(AutoReindexOutcome::SkippedFresh);
    }

    with_index_update_lock(config, || {
        if db.auto_reindex_is_fresh(config.index.auto_reindex_interval_ms)? {
            return Ok(AutoReindexOutcome::SkippedFresh);
        }

        match reindex_with_mode(
            config,
            db,
            false,
            progress,
            ReindexMode::Opportunistic {
                busy_timeout_ms: config.index.auto_reindex_busy_timeout_ms,
            },
        ) {
            Ok(ReindexOutcome::Updated {
                files_seen,
                sessions_updated,
            }) => {
                db.mark_auto_reindex_complete()?;
                Ok(AutoReindexOutcome::Updated {
                    files_seen,
                    sessions_updated,
                })
            }
            Ok(ReindexOutcome::SkippedBusy) => Ok(AutoReindexOutcome::SkippedBusy),
            Err(err) => Err(err),
        }
    })
}

/// Refresh implicit read paths without making index-lock infrastructure a read dependency.
/// Explicit reindex commands continue to use the strict lock API and fail on the same error.
pub fn refresh_index_opportunistically(
    config: &Config,
    db: &Db,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<AutoReindexOutcome> {
    if db.needs_backfill()? {
        return ensure_schema_backfilled(config, db, progress).map(|_| {
            AutoReindexOutcome::Updated {
                files_seen: 0,
                sessions_updated: 0,
            }
        });
    }
    match auto_reindex(config, db, progress) {
        Err(err) if err.downcast_ref::<IndexUpdateLockError>().is_some() => {
            Ok(AutoReindexOutcome::SkippedLockUnavailable {
                reason: err.to_string(),
            })
        }
        other => other,
    }
}

pub fn reindex_with_mode(
    config: &Config,
    db: &Db,
    full: bool,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
    mode: ReindexMode,
) -> Result<ReindexOutcome> {
    match mode {
        ReindexMode::Strict => {
            let (files_seen, sessions_updated) = reindex(config, db, full, progress)?;
            Ok(ReindexOutcome::Updated {
                files_seen,
                sessions_updated,
            })
        }
        ReindexMode::Opportunistic { busy_timeout_ms } => {
            match db.with_busy_timeout_ms(busy_timeout_ms, || reindex(config, db, full, progress)) {
                Ok((files_seen, sessions_updated)) => Ok(ReindexOutcome::Updated {
                    files_seen,
                    sessions_updated,
                }),
                Err(err) if Db::is_sqlite_busy_error(&err) => Ok(ReindexOutcome::SkippedBusy),
                Err(err) => Err(err),
            }
        }
    }
}

/// Incrementally (or fully) reindex all enabled providers into `db`.
///
/// Returns `(files_seen, sessions_updated)`. When `full` is true every discovered file
/// is re-parsed (bypassing the `(mtime_ns, size_bytes)` skip); otherwise a file is
/// skipped when it already matches what's recorded in `files_seen`, making repeated
/// calls cheap.
///
/// DURABLE ARCHIVE: a session whose source file has been removed (e.g. a CLI harness clearing
/// old sessions) is not re-visited, so its indexed history is retained. Re-parsing a live source
/// reconciles superseded session IDs and verified filesystem aliases for that same physical file;
/// those are replacements, not independent archives. An explicit full wipe is [`Db::clear_all`]
/// (not part of reindex) or deleting the index file.
///
/// When `progress` is provided it's invoked with `(index, total, updated)` after
/// each updated file so callers can render progress; the CLI uses this and the
/// MCP server passes `None`.
pub fn reindex(
    config: &Config,
    db: &Db,
    full: bool,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<(usize, usize)> {
    let adapters = ProviderSet::new(config);
    let sources = adapters.discover_enabled(config);
    let source_reconciliation = source_reconciliation(db, &sources)?;

    let total = sources.len();
    let mut updated = 0usize;
    let mut progress = progress;
    if full {
        db.clear_trigram_base()?;
    }
    for (i, source) in sources.iter().enumerate() {
        let source_path = normalize_path(&source.path);
        let reconciliation = source_reconciliation.get(&(source.provider, source_path.clone()));
        let requires_reconciliation = reconciliation.is_some_and(|item| item.requires_reparse);
        if !full
            && !requires_reconciliation
            && db.is_file_current(
                source.provider,
                &source_path,
                source.mtime_ns,
                source.size_bytes,
            )?
            && db.source_parse_version_is_current(
                source.provider,
                &source_path,
                crate::util::provider_parse_version(source.provider),
            )?
        {
            continue;
        }
        // Incremental tail-parse fast path: when we hold a checkpoint for this file, it only
        // grew (offset within it → not truncated), and its head bytes are unchanged (not
        // rewritten/rotated), parse + append ONLY the appended bytes instead of re-reading the
        // whole (possibly multi-hundred-MB) file. Each provider reuses its own `parse_reader`
        // over the appended slice; on any doubt it returns `FullParse` and we re-read below.
        if !full && !requires_reconciliation {
            let outcome = match source.provider {
                Provider::Claude | Provider::ClaudeDesktop => {
                    try_tail(source, &source_path, db, |r, p| {
                        adapters.claude.parse_reader(r, p)
                    })?
                }
                Provider::Codex => try_tail(source, &source_path, db, |r, p| {
                    adapters.codex.parse_reader(r, p)
                })?,
                Provider::Cursor => try_tail(source, &source_path, db, |r, p| {
                    adapters.cursor.parse_reader(r, p)
                })?,
                Provider::Antigravity => try_tail(source, &source_path, db, |r, p| {
                    adapters.antigravity.parse_reader(r, p)
                })?,
                Provider::Pi => try_tail(source, &source_path, db, |r, p| {
                    adapters.pi.parse_reader(r, p)
                })?,
                Provider::AiStudio | Provider::GeminiCli => TailOutcome::FullParse,
            };
            match outcome {
                TailOutcome::Appended => {
                    updated += 1;
                    if let Some(cb) = progress.as_deref_mut() {
                        cb(i + 1, total, updated);
                    }
                    continue;
                }
                TailOutcome::NothingNew => continue,
                TailOutcome::FullParse => {}
            }
        }
        let mut parsed = match source.provider {
            Provider::Claude | Provider::ClaudeDesktop => adapters.claude.parse(source),
            Provider::Codex => adapters.codex.parse(source),
            Provider::Cursor => adapters.cursor.parse(source),
            Provider::Antigravity => adapters.antigravity.parse(source),
            Provider::Pi => adapters.pi.parse(source),
            Provider::AiStudio => adapters.aistudio.parse(source),
            Provider::GeminiCli => adapters.gemini_cli.parse(source),
        };
        // Guarantee every indexed row has a date fallback: providers that lack per-message
        // timestamps still need strict date filters to find their rows by file/session time.
        crate::util::backfill_parsed_dates(&mut parsed, source.mtime_ns);
        let aliases = reconciliation.map_or(&[][..], |item| item.aliases.as_slice());
        db.upsert_session_reconciling_sources(
            &parsed,
            source.mtime_ns,
            source.size_bytes,
            aliases,
            !full,
        )?;
        // Record/refresh the tail checkpoint so the next reindex of this grown file can append
        // incrementally from the end of what we just parsed (instead of re-reading it all).
        let offset = crate::tail::complete_prefix_offset(&source.path)?;
        let fingerprint = crate::tail::prefix_fingerprint(&source.path)?;
        db.set_file_checkpoint(source.provider, &source_path, offset, &fingerprint)?;
        updated += 1;
        if let Some(cb) = progress.as_deref_mut() {
            cb(i + 1, total, updated);
        }
    }

    // Fold the WAL back into the main DB after writing, so the `-wal` file does not accumulate
    // across the per-command auto-reindex. Cheap when nothing was written (skip then).
    if updated > 0 {
        // A full reindex deletes+reinserts every row, fragmenting the FTS5 index into many
        // unmerged segments (≈2x on-disk bloat, measured). Merge them back — but ONLY on a full
        // reindex, never on the per-command incremental path, since `optimize` rewrites the whole
        // index. Incremental appends rely on FTS5's own automerge to stay reasonably compact.
        if full {
            db.optimize_fts()?;
        }
        db.checkpoint_truncate()?;
    }

    Ok((total, updated))
}

#[derive(Debug, Default)]
struct SourceReconciliation {
    aliases: Vec<String>,
    requires_reparse: bool,
}

fn source_reconciliation(
    db: &Db,
    sources: &[SourceFile],
) -> Result<HashMap<(Provider, String), SourceReconciliation>> {
    let mut indexed_by_identity = HashMap::<(Provider, String), Vec<(String, usize)>>::new();
    for (provider, stored_path, sessions) in db.indexed_source_paths()? {
        let Ok(canonical) = std::fs::canonicalize(&stored_path) else {
            continue;
        };
        indexed_by_identity
            .entry((provider, normalize_path(&canonical)))
            .or_default()
            .push((stored_path, sessions));
    }
    Ok(sources
        .iter()
        .filter_map(|source| {
            let path = normalize_path(&source.path);
            let indexed = indexed_by_identity.get(&(source.provider, path.clone()))?;
            let aliases = indexed
                .iter()
                .filter(|(stored, _)| stored != &path)
                .map(|(stored, _)| stored.clone())
                .collect::<Vec<_>>();
            let requires_reparse = !aliases.is_empty()
                || indexed.iter().map(|(_, sessions)| sessions).sum::<usize>() > 1;
            requires_reparse.then_some((
                (source.provider, path),
                SourceReconciliation {
                    aliases,
                    requires_reparse,
                },
            ))
        })
        .collect())
}

/// Ensure the current on-disk schema has been fully backfilled. Returns `true`
/// when it performed the one-time full reindex, `false` when the index was
/// already current.
///
/// This is shared by CLI and MCP startup: schema repair is a data invariant, not
/// a frontend concern. The full path calls [`Db::replace_session`] through
/// [`reindex`] so parser/schema fixes rewrite stale message metadata instead of
/// preserving an old prefix for incremental speed.
pub fn ensure_schema_backfilled(
    config: &Config,
    db: &Db,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<bool> {
    if !db.needs_backfill()? {
        return Ok(false);
    }
    with_index_update_lock(config, || {
        if !db.needs_backfill()? {
            return Ok(false);
        }
        reindex(config, db, true, progress)?;
        db.purge_injected_messages()?;
        db.mark_schema_current()?;
        db.mark_auto_reindex_complete()?;
        Ok(true)
    })
}

/// Outcome of an incremental tail-parse attempt for one file.
enum TailOutcome {
    /// New rows were parsed from the appended bytes and appended to the index.
    Appended,
    /// The file grew only by a partially-written (unterminated) line; nothing complete to index
    /// yet — skip it (the next reindex re-checks cheaply once the line is flushed).
    NothingNew,
    /// The fast path is not safe (no checkpoint, truncation, or a rewritten head); the caller
    /// must perform a full parse.
    FullParse,
}

/// Try to incrementally append only the bytes appended to a session file since its last
/// checkpoint, reusing that provider's real parser (`parse_slice`) over the appended slice
/// ([`crate::tail`]). The preconditions (a stored checkpoint, no truncation, an unchanged file
/// head) make this a pure optimization: on any doubt it returns [`TailOutcome::FullParse`] and
/// the caller re-reads the whole file, so correctness never depends on the fast path.
fn try_tail<F>(
    source: &SourceFile,
    source_path: &str,
    db: &Db,
    parse_slice: F,
) -> Result<TailOutcome>
where
    F: Fn(std::io::Cursor<Vec<u8>>, &std::path::Path) -> Result<crate::models::ParsedSession>,
{
    let Some((offset, stored_fingerprint)) = db.file_checkpoint(source.provider, source_path)?
    else {
        return Ok(TailOutcome::FullParse);
    };
    if source.provider == Provider::ClaudeDesktop {
        return Ok(TailOutcome::FullParse);
    }
    // Truncation / copytruncate: the file is now shorter than where we parsed to → re-read whole.
    if offset <= 0 || source.size_bytes < offset {
        return Ok(TailOutcome::FullParse);
    }
    // Rewrite / rotation: the head bytes changed → the stored offset is meaningless → re-read.
    if !crate::tail::fingerprint_matches(&source.path, &stored_fingerprint)? {
        return Ok(TailOutcome::FullParse);
    }
    match crate::tail::tail_parse(&source.path, offset, parse_slice) {
        Ok(Some(mut tail)) => {
            crate::util::backfill_session_dates(&mut tail.session, source.mtime_ns);
            crate::util::backfill_event_dates(
                &tail.session,
                &mut tail.new_messages,
                &mut tail.new_file_edits,
                source.mtime_ns,
            );
            db.append_tail(&tail, source.mtime_ns, source.size_bytes)?;
            Ok(TailOutcome::Appended)
        }
        Ok(None) => Ok(TailOutcome::NothingNew),
        // The tail fast path is a pure optimization, so ANY failure parsing the appended slice
        // degrades to a full re-read rather than aborting the reindex (this module's "on any doubt
        // → FullParse" contract). The error is usually a `parse_slice` UTF-8 failure: a tool-output
        // line with embedded binary, or the bounded overlap window beginning mid-character. The
        // full parse re-reads from byte 0 (a clean char boundary) and is itself panic/error-safe
        // via `minimal_record`, so a single bad file never breaks the whole reindex — and thus
        // every read command, which auto-reindexes first.
        Err(_) => Ok(TailOutcome::FullParse),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    const TEST_RESTORED_BUSY_TIMEOUT_MS: u64 = 1_000;
    const TEST_OPPORTUNISTIC_NO_WAIT_MS: u64 = 0;
    const TEST_AUTO_REINDEX_INTERVAL_MS: u64 = 60_000;

    fn config_with_single_claude_fixture(
        path: &std::path::Path,
        claude_root: &std::path::Path,
    ) -> Config {
        let mut config = Config::default();
        config.index.db_path = Some(path.to_string_lossy().to_string());
        config.providers.claude.enabled = true;
        config.providers.claude.paths = vec![claude_root.to_string_lossy().to_string()];
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;
        config
    }

    fn config_with_no_providers(path: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.index.db_path = Some(path.to_string_lossy().to_string());
        config.index.auto_reindex_interval_ms = TEST_AUTO_REINDEX_INTERVAL_MS;
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;
        config
    }

    #[test]
    fn reindex_discovers_and_searches_snapshot_providers() {
        use crate::models::MessageFilters;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let aistudio_root = dir.path().join("aistudio");
        let gemini_root = dir.path().join("gemini");
        let gemini_chats = gemini_root.join("project-hash").join("chats");
        std::fs::create_dir_all(&aistudio_root).unwrap();
        std::fs::create_dir_all(&gemini_chats).unwrap();
        std::fs::write(
            aistudio_root.join("studio.json"),
            r#"{"chunkedPrompt":{"chunks":[{"role":"user","text":"studio-needle"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            gemini_chats.join("session-2026-07-12T10-30-id.json"),
            r#"{"sessionId":"g1","messages":[{"type":"gemini","content":"gemini-needle"}]}"#,
        )
        .unwrap();

        let mut config = config_with_no_providers(&db_path);
        config.providers.aistudio.enabled = true;
        config.providers.aistudio.paths = vec![aistudio_root.to_string_lossy().to_string()];
        config.providers.gemini_cli.enabled = true;
        config.providers.gemini_cli.paths = vec![gemini_root.to_string_lossy().to_string()];
        let db = Db::open(&db_path).unwrap();

        let (updated, total) = reindex(&config, &db, false, None).unwrap();

        assert_eq!((updated, total), (2, 2));
        assert_eq!(
            db.search_messages("studio-needle", &MessageFilters::default())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_messages("gemini-needle", &MessageFilters::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn opportunistic_reindex_skips_on_writer_contention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        std::fs::write(
            claude_root.join("session.jsonl"),
            r#"{"sessionId":"s1","cwd":"/tmp/project","type":"user","message":{"role":"user","content":"hello"}}"#,
        )
        .unwrap();
        let writer = rusqlite::Connection::open(&path).unwrap();
        let contender = Db::open_with_busy_timeout(&path, TEST_RESTORED_BUSY_TIMEOUT_MS).unwrap();
        let config = config_with_single_claude_fixture(&path, &claude_root);

        writer.execute_batch("begin immediate").unwrap();
        let outcome = reindex_with_mode(
            &config,
            &contender,
            false,
            None,
            ReindexMode::Opportunistic {
                busy_timeout_ms: TEST_OPPORTUNISTIC_NO_WAIT_MS,
            },
        )
        .unwrap();
        assert_eq!(outcome, ReindexOutcome::SkippedBusy);
        assert_eq!(
            contender.busy_timeout_ms().unwrap(),
            TEST_RESTORED_BUSY_TIMEOUT_MS,
            "temporary opportunistic timeout must be restored"
        );
        writer.execute_batch("rollback").unwrap();
    }

    #[test]
    fn auto_reindex_uses_shared_freshness_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let db = Db::open(&path).unwrap();
        let config = config_with_no_providers(&path);

        let first = auto_reindex(&config, &db, None).unwrap();
        assert_eq!(
            first,
            AutoReindexOutcome::Updated {
                files_seen: 0,
                sessions_updated: 0
            }
        );
        let second = auto_reindex(&config, &db, None).unwrap();
        assert_eq!(second, AutoReindexOutcome::SkippedFresh);
    }

    #[test]
    fn auto_reindex_busy_sqlite_writer_serves_existing_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        std::fs::write(
            claude_root.join("session.jsonl"),
            r#"{"sessionId":"s1","cwd":"/tmp/project","type":"user","message":{"role":"user","content":"hello"}}"#,
        )
        .unwrap();
        let writer = rusqlite::Connection::open(&path).unwrap();
        let contender = Db::open_with_busy_timeout(&path, TEST_OPPORTUNISTIC_NO_WAIT_MS).unwrap();
        let mut config = config_with_single_claude_fixture(&path, &claude_root);
        config.index.auto_reindex_busy_timeout_ms = TEST_OPPORTUNISTIC_NO_WAIT_MS;

        writer.execute_batch("begin immediate").unwrap();
        let outcome = auto_reindex(&config, &contender, None).unwrap();
        writer.execute_batch("rollback").unwrap();

        assert_eq!(outcome, AutoReindexOutcome::SkippedBusy);
    }

    #[test]
    fn auto_reindex_waits_for_update_lock_then_uses_fresh_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let db = Db::open(&path).unwrap();
        let config = config_with_no_providers(&path);
        let lock_path = index_update_lock_path(&config.db_path());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        let mut lock = RwLock::new(file);
        let guard = lock.write().unwrap();
        let (tx, rx) = mpsc::channel();
        let thread_config = config.clone();
        let thread_path = path.clone();

        std::thread::spawn(move || {
            let reader = Db::open(&thread_path).unwrap();
            tx.send(auto_reindex(&thread_config, &reader, None).unwrap())
                .unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(30)).is_err(),
            "auto_reindex returned before the update lock was released"
        );
        db.mark_auto_reindex_complete().unwrap();
        drop(guard);

        let outcome = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(outcome, AutoReindexOutcome::SkippedFresh);
    }

    #[test]
    fn opportunistic_refresh_serves_existing_index_when_lock_path_is_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        let db = Db::open(&db_path).unwrap();
        db.mark_schema_current().unwrap();
        std::fs::create_dir(index_update_lock_path(&db_path)).unwrap();

        let outcome = refresh_index_opportunistically(&config, &db, None).unwrap();

        assert!(matches!(
            outcome,
            AutoReindexOutcome::SkippedLockUnavailable { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn opportunistic_refresh_never_follows_a_symbolic_link_lock() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        let db = Db::open(&db_path).unwrap();
        db.mark_schema_current().unwrap();
        let target = dir.path().join("unrelated");
        std::fs::write(&target, b"preserve me").unwrap();
        symlink(&target, index_update_lock_path(&db_path)).unwrap();

        let outcome = refresh_index_opportunistically(&config, &db, None).unwrap();

        assert!(matches!(
            outcome,
            AutoReindexOutcome::SkippedLockUnavailable { .. }
        ));
        assert_eq!(std::fs::read(target).unwrap(), b"preserve me");
    }

    #[test]
    fn opportunistic_refresh_requires_lock_for_schema_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        let db = Db::open(&db_path).unwrap();
        std::fs::create_dir(index_update_lock_path(&db_path)).unwrap();

        let err = refresh_index_opportunistically(&config, &db, None).unwrap_err();

        assert!(err.downcast_ref::<IndexUpdateLockError>().is_some());
    }

    #[test]
    fn incremental_reindex_reparses_current_file_when_parse_version_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        let session_file = claude_root.join("session.jsonl");
        std::fs::write(
            &session_file,
            r#"{"sessionId":"s1","cwd":"/tmp/project","type":"user","message":{"role":"user","content":"hello"}}"#,
        )
        .unwrap();
        let config = config_with_single_claude_fixture(&path, &claude_root);
        let db = Db::open(&path).unwrap();
        let metadata = std::fs::metadata(&session_file).unwrap();
        let mtime_ns = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        let size_bytes = metadata.len() as i64;
        let source_path = normalize_path(&session_file);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "insert into sessions (id, provider, provider_session_id, preview_text, \
                 source_path, parse_version, discovery_source) \
                 values ('claude:s1','claude','s1','','/old','claude-v1','test')",
            [],
        )
        .unwrap();
        conn
            .execute(
                "insert into files_seen (provider, source_path, mtime_ns, size_bytes, last_indexed_at) \
                 values ('claude', ?1, ?2, ?3, '2026-01-01T00:00:00Z')",
                rusqlite::params![source_path, mtime_ns, size_bytes],
            )
            .unwrap();

        let (_seen, updated) = reindex(&config, &db, false, None).unwrap();
        assert_eq!(updated, 1, "old parse_version must force reparse");
        let version: String = conn
            .query_row(
                "select parse_version from sessions where id='claude:s1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            version,
            crate::util::provider_parse_version(Provider::Claude)
        );
    }
}
