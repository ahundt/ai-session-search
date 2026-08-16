// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Adam Zhao
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context as _, Result};
use fd_lock::RwLock;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::{Config, IndexRefresh};
use crate::db::{Db, SchemaState, SourceFileRefreshState, SCHEMA_VERSION};
use crate::durable_fs::open_file_lock;
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
pub(crate) enum ReindexRun {
    Completed {
        files_seen: usize,
        sessions_updated: usize,
        discovery_warnings: Vec<crate::source::ProviderDiscoveryWarning>,
    },
    Cancelled {
        files_seen: usize,
        sessions_updated: usize,
        discovery_warnings: Vec<crate::source::ProviderDiscoveryWarning>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundRefreshOutcome {
    Updated {
        files_seen: usize,
        sessions_updated: usize,
    },
    SkippedFresh,
    SkippedBusy,
    Cancelled,
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

/// Inspects schema state and elects the single application writer before maintenance begins.
/// Schema inspection is intentionally read-only and does not create an absent database or lock.
pub(crate) struct IndexCoordinator<'a> {
    config: &'a Config,
}

/// Unforgeable proof that this call owns the cross-process index writer lock.
pub(crate) struct MaintenancePermit<'lock> {
    _guard: &'lock fd_lock::RwLockWriteGuard<'lock, File>,
}

impl<'a> IndexCoordinator<'a> {
    pub(crate) const fn new(config: &'a Config) -> Self {
        Self { config }
    }

    pub(crate) fn inspect_schema(&self) -> Result<SchemaState> {
        let path = self.config.db_path();
        if !path
            .try_exists()
            .with_context(|| format!("failed to inspect index path {}", path.display()))?
        {
            return Ok(SchemaState::Missing);
        }

        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = rusqlite::Connection::open_with_flags(&path, flags)
            .with_context(|| format!("failed to open index {} read-only", path.display()))?;
        match conn.query_row("pragma user_version", [], |row| row.get::<_, i64>(0)) {
            Ok(version) if version == SCHEMA_VERSION => {
                match current_schema_layout_problem(&conn)? {
                    Some(reason) => {
                        let detail = format!(
                            "{} has a current version stamp but {reason}",
                            path.display()
                        );
                        // When every base (non-derived) table is intact, the only broken objects are
                        // the derived FTS5 message-search tables/triggers, which rebuild online from
                        // the base rows (no transcript re-read, no data loss). Classify that as
                        // repairable so a writable open self-heals in place. Only genuine base-data
                        // loss still demands an offline `reindex --full`.
                        if base_data_intact(&conn)? {
                            Ok(SchemaState::RepairableLayout { reason: detail })
                        } else {
                            Ok(SchemaState::RecoveryRequired { reason: detail })
                        }
                    }
                    None => {
                        let providers = crate::db::unknown_index_providers(&conn)?;
                        Ok(if providers.is_empty() {
                            SchemaState::Current
                        } else {
                            SchemaState::UnknownProviders { providers }
                        })
                    }
                }
            }
            Ok(version) => Ok(SchemaState::from_version(version)),
            Err(error)
                if matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
                ) =>
            {
                Ok(SchemaState::RecoveryRequired {
                    reason: format!("SQLite could not read {}: {error}", path.display()),
                })
            }
            Err(error) => Err(error).with_context(|| {
                format!("failed to inspect schema generation in {}", path.display())
            }),
        }
    }

    pub(crate) fn with_elected_writer<T>(
        &self,
        operation: impl FnOnce(&MaintenancePermit<'_>) -> Result<T>,
    ) -> Result<T> {
        let lock_path = index_update_lock_path(&self.config.db_path());
        let mut lock = open_index_update_lock(&lock_path)?;
        let guard = loop {
            match lock.write() {
                Ok(guard) => break guard,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(IndexUpdateLockError {
                        path: lock_path.clone(),
                        source,
                    }
                    .into());
                }
            }
        };
        let permit = MaintenancePermit { _guard: &guard };
        operation(&permit)
    }

    /// Attempt writer election without making an implicit read wait for another process.
    ///
    /// Lock acquisition is `O(1)` excluding operating-system contention. `None` means another
    /// process owns or is contending for the writer; explicit maintenance keeps using the blocking
    /// [`Self::with_elected_writer`] contract.
    pub(crate) fn try_with_elected_writer<T>(
        &self,
        operation: impl FnOnce(&MaintenancePermit<'_>) -> Result<T>,
    ) -> Result<Option<T>> {
        let lock_path = index_update_lock_path(&self.config.db_path());
        let mut lock = open_index_update_lock(&lock_path)?;
        let guard = match lock.try_write() {
            Ok(guard) => guard,
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                return Ok(None);
            }
            Err(source) => {
                return Err(IndexUpdateLockError {
                    path: lock_path,
                    source,
                }
                .into());
            }
        };
        let permit = MaintenancePermit { _guard: &guard };
        operation(&permit).map(Some)
    }
}

/// Return a concrete reason when a database stamped with the current generation does not own the
/// current search layout. This inspection is read-only and deliberately checks semantic ownership,
/// not FTS shadow-table implementation details that SQLite may change.
pub(crate) fn current_schema_layout_problem(conn: &rusqlite::Connection) -> Result<Option<String>> {
    if let Some(problem) = message_search_layout_problem(conn)? {
        return Ok(Some(problem));
    }
    let parts_table_exists: bool = conn.query_row(
        "select exists(
            select 1 from sqlite_schema
            where type = 'table' and name = 'message_content_parts'
        )",
        [],
        |row| row.get(0),
    )?;
    if !parts_table_exists {
        return Ok(Some(
            "is missing required table(s): message_content_parts".to_string(),
        ));
    }
    let columns = conn
        .prepare("pragma table_info(messages)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    let missing_columns = [
        "authorship",
        "record_relation",
        "correlation_authority",
        "correlation_scope",
        "correlation_id",
    ]
    .into_iter()
    .filter(|column| !columns.contains(*column))
    .collect::<Vec<_>>();
    if !missing_columns.is_empty() {
        return Ok(Some(format!(
            "messages is missing required column(s): {}",
            missing_columns.join(", ")
        )));
    }
    Ok(None)
}

/// Validate the schema-4 derived message-search objects independently of newer parser-derived
/// columns. Database-file migration uses this to heal a v4 hybrid without falsely claiming that
/// unavailable transcript rows have schema-5 provenance.
pub(crate) fn message_search_layout_problem(conn: &rusqlite::Connection) -> Result<Option<String>> {
    let mut stmt = conn.prepare("select type, name, coalesce(sql, '') from sqlite_schema")?;
    let objects: HashMap<String, (String, String)> = stmt
        .query_map([], |row| Ok((row.get(1)?, (row.get(0)?, row.get(2)?))))?
        .collect::<rusqlite::Result<_>>()?;

    let required_tables = [
        "sessions",
        "transcripts",
        "files_seen",
        "index_metadata",
        "messages",
        "file_edits",
        "sessions_fts",
        "messages_fts",
        "messages_vocab",
        "messages_trigram",
        "messages_trigram_vocab",
        "messages_trigram_terms",
    ];
    let missing: Vec<_> = required_tables
        .into_iter()
        .filter(|name| !matches!(objects.get(*name), Some((kind, _)) if kind == "table"))
        .collect();
    if !missing.is_empty() {
        return Ok(Some(format!(
            "is missing required table(s): {}",
            missing.join(", ")
        )));
    }

    let obsolete: Vec<_> = ["trigram_postings", "trigram_meta"]
        .into_iter()
        .filter(|name| objects.contains_key(*name))
        .collect();
    if !obsolete.is_empty() {
        return Ok(Some(format!(
            "contains obsolete pre-v4 table(s): {}",
            obsolete.join(", ")
        )));
    }

    let invalid_triggers: Vec<_> = ["messages_ai", "messages_ad", "messages_au"]
        .into_iter()
        .filter(|name| {
            !matches!(objects.get(*name), Some((kind, sql)) if kind == "trigger" && sql.contains("messages_fts") && sql.contains("messages_trigram"))
        })
        .collect();
    if !invalid_triggers.is_empty() {
        return Ok(Some(format!(
            "has missing or incompatible message-index trigger(s): {}",
            invalid_triggers.join(", ")
        )));
    }

    Ok(None)
}

/// Whether every base (non-derived) table needed to rebuild the message-search layout is present.
/// The derived FTS5 objects (`messages_fts`, `messages_trigram`, the fts5vocab shadows) and their
/// triggers can be rebuilt from these rows; the base tables cannot. Read-only. Used to distinguish a
/// repairable hybrid layout (base intact → self-heal) from genuine base-data loss (→ offline
/// recovery).
pub(crate) fn base_data_intact(conn: &rusqlite::Connection) -> Result<bool> {
    const BASE_TABLES: [&str; 6] = [
        "sessions",
        "transcripts",
        "files_seen",
        "index_metadata",
        "messages",
        "file_edits",
    ];
    for table in BASE_TABLES {
        let present: bool = conn.query_row(
            "select exists(select 1 from sqlite_schema where type = 'table' and name = ?1)",
            [table],
            |row| row.get(0),
        )?;
        if !present {
            return Ok(false);
        }
    }
    Ok(true)
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
    open_file_lock(path).map_err(|source| {
        IndexUpdateLockError {
            path: path.to_path_buf(),
            source,
        }
        .into()
    })
}

pub fn with_index_update_lock<T>(config: &Config, f: impl FnOnce() -> Result<T>) -> Result<T> {
    IndexCoordinator::new(config).with_elected_writer(|_permit| f())
}

pub fn auto_reindex(
    config: &Config,
    db: &Db,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<AutoReindexOutcome> {
    if !auto_refresh_is_due(db, config.index.auto_reindex_interval_ms)? {
        return Ok(AutoReindexOutcome::SkippedFresh);
    }

    IndexCoordinator::new(config)
        .with_elected_writer(|_permit| auto_reindex_after_election(config, db, progress))
}

fn auto_reindex_after_election(
    config: &Config,
    db: &Db,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<AutoReindexOutcome> {
    let schema_backfill_required = db.needs_backfill()?;
    if !auto_refresh_is_due(db, config.index.auto_reindex_interval_ms)? {
        return Ok(AutoReindexOutcome::SkippedFresh);
    }

    match reindex_with_mode(
        config,
        db,
        schema_backfill_required,
        progress,
        ReindexMode::Opportunistic {
            busy_timeout_ms: config.index.auto_reindex_busy_timeout_ms,
        },
    ) {
        Ok(ReindexOutcome::Updated {
            files_seen,
            sessions_updated,
        }) => {
            if schema_backfill_required {
                db.purge_injected_messages()?;
                db.mark_schema_current()?;
                if db.schema_version()? < SCHEMA_VERSION {
                    db.migrate_message_search_schema_exclusive()?;
                }
            }
            db.mark_auto_reindex_complete()?;
            Ok(AutoReindexOutcome::Updated {
                files_seen,
                sessions_updated,
            })
        }
        Ok(ReindexOutcome::SkippedBusy) => Ok(AutoReindexOutcome::SkippedBusy),
        Err(err) => Err(err),
    }
}

/// True when parser/schema backfill is required or the ordinary content-refresh interval elapsed.
/// Schema work always wins over a recent content timestamp.
pub(crate) fn auto_refresh_is_due(db: &Db, interval_ms: u64) -> Result<bool> {
    Ok(db.needs_backfill()? || !db.auto_reindex_is_fresh(interval_ms)?)
}

/// Refresh implicit read paths without making index-lock infrastructure a read dependency.
/// Explicit reindex commands continue to use the strict lock API and fail on the same error.
pub fn refresh_index_opportunistically(
    config: &Config,
    db: &Db,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<AutoReindexOutcome> {
    if !db.schema_is_readable()? {
        return ensure_schema_backfilled(config, db, progress).map(|_| {
            AutoReindexOutcome::Updated {
                files_seen: 0,
                sessions_updated: 0,
            }
        });
    }
    if !auto_refresh_is_due(db, config.index.auto_reindex_interval_ms)? {
        return Ok(AutoReindexOutcome::SkippedFresh);
    }
    let outcome = IndexCoordinator::new(config)
        .try_with_elected_writer(|_permit| auto_reindex_after_election(config, db, progress));
    match outcome {
        Ok(Some(outcome)) => Ok(outcome),
        Ok(None) => Ok(AutoReindexOutcome::SkippedBusy),
        Err(err) if err.downcast_ref::<IndexUpdateLockError>().is_some() => {
            Ok(AutoReindexOutcome::SkippedLockUnavailable {
                reason: err.to_string(),
            })
        }
        Err(err) => Err(err),
    }
}

pub(crate) fn prepare_index_for_read(
    config: &Config,
    db: &Db,
) -> Result<Option<AutoReindexOutcome>> {
    match config.index.refresh {
        IndexRefresh::Auto if db.schema_is_readable()? => Ok(None),
        IndexRefresh::Auto => refresh_index_opportunistically(config, db, None).map(Some),
        IndexRefresh::BeforeQuery => {
            if db.needs_backfill()? {
                ensure_schema_backfilled(config, db, None)?;
                return Ok(Some(AutoReindexOutcome::Updated {
                    files_seen: 0,
                    sessions_updated: 0,
                }));
            }
            with_index_update_lock(config, || {
                let (files_seen, sessions_updated) = reindex(config, db, false, None)?;
                db.mark_auto_reindex_complete()?;
                Ok(Some(AutoReindexOutcome::Updated {
                    files_seen,
                    sessions_updated,
                }))
            })
        }
        IndexRefresh::ExistingOnly => {
            if !db.schema_is_readable()? {
                anyhow::bail!(
                    "existing index {} has an unreadable schema generation; run `aise reindex` without `--index-refresh existing-only`",
                    config.db_path().display()
                );
            }
            Ok(None)
        }
    }
}

/// Prepare an index for an immediate read without performing an optional `auto` refresh.
///
/// An absent or outdated schema cannot serve a read and is repaired synchronously. A due, empty
/// `auto` index is populated synchronously so the first command does not report an empty history
/// while discoverable sources wait on a detached refresh. An established usable index is returned
/// unchanged so transports can refresh it after delivering the response. Deterministic
/// `before-query` and `existing-only` policies retain their normal behavior.
pub(crate) fn prepare_index_for_read_now(
    config: &Config,
    db: &Db,
) -> Result<Option<AutoReindexOutcome>> {
    if config.index.refresh != IndexRefresh::Auto {
        return prepare_index_for_read(config, db);
    }
    if !db.schema_is_readable()? {
        return ensure_schema_backfilled(config, db, None).map(|_| {
            Some(AutoReindexOutcome::Updated {
                files_seen: 0,
                sessions_updated: 0,
            })
        });
    }
    if !db.needs_backfill()?
        && !db.has_sessions()?
        && auto_refresh_is_due(db, config.index.auto_reindex_interval_ms)?
    {
        return refresh_index_opportunistically(config, db, None).map(Some);
    }
    Ok(None)
}

/// Refresh a readable index without waiting for another updater.
///
/// Callers must synchronously prepare an unreadable schema before invoking this helper. A readable
/// older generation is upgraded fully under the update lock. Lock contention is an expected no-op;
/// `on_elected` runs exactly once after acquiring that lock and before refresh work. Cancellation
/// is observed at transaction boundaries. Completion stamps are written only after the reindex and
/// any archive cleanup both succeed.
pub(crate) fn refresh_usable_index_nonblocking(
    config: &Config,
    db: &Db,
    should_cancel: &dyn Fn() -> bool,
    on_elected: Option<&mut dyn FnMut()>,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<BackgroundRefreshOutcome> {
    if should_cancel() {
        return Ok(BackgroundRefreshOutcome::Cancelled);
    }
    if !db.schema_is_readable()? {
        anyhow::bail!(
            "background refresh requires a readable index schema; prepare {} synchronously first",
            config.db_path().display()
        );
    }
    if !auto_refresh_is_due(db, config.index.auto_reindex_interval_ms)? {
        return Ok(BackgroundRefreshOutcome::SkippedFresh);
    }

    let outcome = IndexCoordinator::new(config).try_with_elected_writer(|_permit| {
        if let Some(callback) = on_elected {
            callback();
        }
        if should_cancel() {
            return Ok(BackgroundRefreshOutcome::Cancelled);
        }
        let schema_backfill_required = db.needs_backfill()?;
        if !auto_refresh_is_due(db, config.index.auto_reindex_interval_ms)? {
            return Ok(BackgroundRefreshOutcome::SkippedFresh);
        }

        let run = db.with_busy_timeout_ms(config.index.auto_reindex_busy_timeout_ms, || {
            reindex_until(
                config,
                db,
                schema_backfill_required,
                progress,
                should_cancel,
            )
        });
        match run {
            Ok(ReindexRun::Completed {
                files_seen,
                sessions_updated,
                ..
            }) => {
                if schema_backfill_required {
                    db.purge_injected_messages()?;
                    db.mark_schema_current()?;
                    if db.schema_version()? < SCHEMA_VERSION {
                        db.migrate_message_search_schema_exclusive()?;
                    }
                }
                db.mark_auto_reindex_complete()?;
                Ok(BackgroundRefreshOutcome::Updated {
                    files_seen,
                    sessions_updated,
                })
            }
            Ok(ReindexRun::Cancelled { .. }) => Ok(BackgroundRefreshOutcome::Cancelled),
            Err(error) if Db::is_sqlite_busy_error(&error) => {
                Ok(BackgroundRefreshOutcome::SkippedBusy)
            }
            Err(error) => Err(error),
        }
    })?;
    Ok(outcome.unwrap_or(BackgroundRefreshOutcome::SkippedBusy))
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

pub struct ExplicitReindexOutcome {
    pub files_seen: usize,
    pub sessions_updated: usize,
    pub effective_full: bool,
    pub discovery_warnings: Vec<crate::source::ProviderDiscoveryWarning>,
}

/// Full public reindex contract: parser backfill and incompatible message-search migration share
/// one elected-writer interval and one database connection. This prevents adapters from racing a
/// second writer between the two stages or implementing their own close/reopen choreography.
pub(crate) fn explicit_reindex_and_migrate(
    config: &Config,
    db: &Db,
    requested_full: bool,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<ExplicitReindexOutcome> {
    with_index_update_lock(config, || {
        let outcome = explicit_reindex_with_writer_permit(config, db, requested_full, progress)?;
        if outcome.effective_full && db.schema_version()? < SCHEMA_VERSION {
            db.migrate_message_search_schema_exclusive()
                .with_context(|| {
                    format!(
                        "failed to migrate {} to message-search schema v{SCHEMA_VERSION}; \
                     stop other AI Session Search processes, verify free disk space and write \
                     access, then retry `aise reindex --full`",
                        config.db_path().display()
                    )
                })?;
        }
        Ok(outcome)
    })
}

fn explicit_reindex_with_writer_permit(
    config: &Config,
    db: &Db,
    requested_full: bool,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
) -> Result<ExplicitReindexOutcome> {
    let effective_full = requested_full || db.needs_backfill()?;
    let (files_seen, sessions_updated, discovery_warnings) =
        match reindex_until(config, db, effective_full, progress, &|| false)? {
            ReindexRun::Completed {
                files_seen,
                sessions_updated,
                discovery_warnings,
            } => (files_seen, sessions_updated, discovery_warnings),
            ReindexRun::Cancelled { .. } => {
                unreachable!("the default reindex token never cancels")
            }
        };
    if effective_full {
        db.purge_injected_messages()?;
        db.mark_schema_current()?;
    }
    db.mark_auto_reindex_complete()?;
    Ok(ExplicitReindexOutcome {
        files_seen,
        sessions_updated,
        effective_full,
        discovery_warnings,
    })
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
    match reindex_until(config, db, full, progress, &|| false)? {
        ReindexRun::Completed {
            files_seen,
            sessions_updated,
            ..
        } => Ok((files_seen, sessions_updated)),
        ReindexRun::Cancelled { .. } => unreachable!("the default reindex token never cancels"),
    }
}

pub(crate) fn reindex_until(
    config: &Config,
    db: &Db,
    full: bool,
    progress: Option<&mut dyn FnMut(usize, usize, usize)>,
    should_cancel: &dyn Fn() -> bool,
) -> Result<ReindexRun> {
    if should_cancel() {
        return Ok(ReindexRun::Cancelled {
            files_seen: 0,
            sessions_updated: 0,
            discovery_warnings: Vec::new(),
        });
    }
    // Derived query indexes an older aise did not create are built here, first, because this is
    // the one place that already holds the cross-process writer election and expects long
    // writes; every query process's `open` deliberately skips the build on a populated index.
    db.ensure_message_kind_index()?;
    let adapters = ProviderSet::new(config);
    let discovery = adapters.discover_enabled(config);
    let sources = deduplicate_sources(discovery.sources);
    let discovery_warnings = discovery.warnings;
    let discovered_source_paths = sources
        .iter()
        .map(|source| (source.provider, normalize_path(&source.path)))
        .collect::<HashSet<_>>();
    let indexed_source_paths = db
        .indexed_source_identities()?
        .into_iter()
        .map(|(provider, source_path, _, _)| (provider, source_path))
        .collect::<HashSet<_>>();
    if should_cancel() {
        return Ok(ReindexRun::Cancelled {
            files_seen: 0,
            sessions_updated: 0,
            discovery_warnings,
        });
    }
    let source_reconciliation = source_reconciliation(db, &sources)?;
    if should_cancel() {
        return Ok(ReindexRun::Cancelled {
            files_seen: 0,
            sessions_updated: 0,
            discovery_warnings,
        });
    }

    let total = sources.len();
    let mut updated = 0usize;
    let mut progress = progress;
    if full {
        db.clear_trigram_base()?;
    }
    for (i, source) in sources.iter().enumerate() {
        if should_cancel() {
            return Ok(ReindexRun::Cancelled {
                files_seen: i,
                sessions_updated: updated,
                discovery_warnings,
            });
        }
        let source_path = normalize_path(&source.path);
        let reconciliation = source_reconciliation.get(&(source.provider, source_path.clone()));
        let requires_reconciliation = reconciliation.is_some_and(|item| item.requires_reparse);
        let expected_session_id = reconciliation.and_then(|item| item.session_id.as_deref());
        let parser_version = crate::util::provider_parse_version(source.provider);
        let refresh_state = db.source_file_refresh_state(
            source.provider,
            &source_path,
            source.mtime_ns,
            source.size_bytes,
            parser_version,
        )?;
        // A prior ID collision can leave a current files_seen row whose session still points at a
        // source that has since vanished. Parse that one candidate instead of prescribing an
        // incremental reindex that skips it forever.
        let current_without_session =
            !full && refresh_state == SourceFileRefreshState::Current && reconciliation.is_none();
        if !full
            && !requires_reconciliation
            && !current_without_session
            && refresh_state == SourceFileRefreshState::Current
        {
            continue;
        }
        // Incremental tail-parse fast path: when we hold a checkpoint for this file, it only
        // grew (offset within it → not truncated), and its head bytes are unchanged (not
        // rewritten/rotated), parse + append ONLY the appended bytes instead of re-reading the
        // whole (possibly multi-hundred-MB) file. Each provider reuses its own `parse_reader`
        // over the appended slice; on any doubt it returns `FullParse` and we re-read below.
        if !full && !requires_reconciliation && !current_without_session {
            let outcome = match source.provider {
                Provider::Claude | Provider::ClaudeDesktop => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p| adapters.claude.parse_reader(r, p),
                )?,
                Provider::Codex => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p| adapters.codex.parse_reader(r, p),
                )?,
                Provider::Cursor => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p| adapters.cursor.parse_reader(r, p),
                )?,
                Provider::Antigravity => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p| adapters.antigravity.parse_reader(r, p),
                )?,
                Provider::Pi => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p| adapters.pi.parse_reader(r, p),
                )?,
                Provider::PrimeAgent => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p| adapters.prime_agent.parse_reader(r, p),
                )?,
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
                TailOutcome::Cancelled => {
                    return Ok(ReindexRun::Cancelled {
                        files_seen: i,
                        sessions_updated: updated,
                        discovery_warnings,
                    })
                }
            }
        }
        // Shared with diagnostics::explain_unindexed so a diagnosis parses a file exactly as
        // indexing does; two copies of this dispatch could disagree.
        let mut parsed = match adapters.parse_until(source, should_cancel) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => {
                return Ok(ReindexRun::Cancelled {
                    files_seen: i,
                    sessions_updated: updated,
                    discovery_warnings,
                })
            }
            Err(_)
                if indexed_source_paths.contains(&(source.provider, source_path.clone()))
                    || reconciliation.is_some() =>
            {
                // A failed refresh is not a new archive state. Keep the last successful rows and
                // their old files_seen metadata so the next refresh retries this source. A source
                // reconciliation also proves an existing row through a canonical-path alias.
                continue;
            }
            Err(error) => {
                crate::util::minimal_record(source.provider, &source.path, format!("{error:#}"))
            }
        };
        // Guarantee every indexed row has a date fallback: providers that lack per-message
        // timestamps still need strict date filters to find their rows by file/session time.
        crate::util::backfill_parsed_dates(&mut parsed, source.mtime_ns);
        // `backfill_parsed_dates` leaves the span ordered; the only way to arrive here without a
        // complete span is a source whose mtime does not convert. That is this file's problem:
        // index it as a warned stub, like any other parse failure, so the sources after it are
        // still indexed and every read command that refreshes first still answers.
        if let Err(error) = crate::util::validate_session_date_order(&parsed.session) {
            parsed = crate::util::minimal_record(
                source.provider,
                &source.path,
                format!("malformed session timestamps: {error:#}"),
            );
            crate::util::backfill_parsed_dates(&mut parsed, source.mtime_ns);
        }
        // Do not oscillate between two live files that claim one provider ID. The existing holder
        // remains canonical until it leaves the configured discovery set.
        if current_without_session
            && db
                .exact_session_source_path(&parsed.session.id)?
                .is_some_and(|holder| {
                    discovered_source_paths
                        .contains(&(source.provider, normalize_path(Path::new(&holder))))
                })
        {
            continue;
        }
        let aliases = reconciliation.map_or(&[][..], |item| item.aliases.as_slice());
        // Constraint failures are not actionable without the session and source file being
        // applied; retain both in the error chain.
        db.upsert_session_reconciling_sources(
            &parsed,
            source.mtime_ns,
            source.size_bytes,
            aliases,
            false,
        )
        .with_context(|| {
            format!(
                "failed to index session '{}' from {}",
                parsed.session.id,
                source.path.display()
            )
        })?;
        // Record/refresh the tail checkpoint so the next reindex of this grown file can append
        // incrementally from the end of what we just parsed (instead of re-reading it all).
        let (offset, fingerprint) = full_parse_checkpoint(source)?;
        db.set_file_checkpoint(source.provider, &source_path, offset, &fingerprint)?;
        updated += 1;
        if let Some(cb) = progress.as_deref_mut() {
            cb(i + 1, total, updated);
        }
    }

    if should_cancel() {
        return Ok(ReindexRun::Cancelled {
            files_seen: total,
            sessions_updated: updated,
            discovery_warnings,
        });
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

    Ok(ReindexRun::Completed {
        files_seen: total,
        sessions_updated: updated,
        discovery_warnings,
    })
}

/// Capture a tail checkpoint only when the path still has the exact size discovery supplied to
/// the parser. A later reopen may observe a concurrent append, but those bytes were not proven to
/// have crossed the parser's EOF and therefore must force one conservative full parse.
fn full_parse_checkpoint(source: &SourceFile) -> Result<(i64, String)> {
    let current_size = std::fs::metadata(&source.path)?.len() as i64;
    let complete_offset = if current_size == source.size_bytes {
        crate::tail::complete_prefix_offset(&source.path)?
    } else {
        0
    };
    // Provider parsers accept a valid final JSONL record without a trailing newline. Such a
    // record is already present in the parsed session, but complete_prefix_offset points to its
    // beginning. Store zero so the next change takes a full-parse fallback and cannot duplicate it.
    let offset = if current_size == source.size_bytes && complete_offset == source.size_bytes {
        complete_offset
    } else {
        0
    };
    Ok((offset, crate::tail::prefix_fingerprint(&source.path)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PhysicalSourceIdentity {
    File(file_id::FileId),
    CanonicalPath(PathBuf),
}

fn deduplicate_sources(sources: Vec<SourceFile>) -> Vec<SourceFile> {
    let mut seen = HashSet::with_capacity(sources.len());
    sources
        .into_iter()
        .filter(|source| {
            let identity = file_id::get_file_id(&source.path)
                .map(PhysicalSourceIdentity::File)
                .unwrap_or_else(|_| {
                    PhysicalSourceIdentity::CanonicalPath(
                        std::fs::canonicalize(&source.path).unwrap_or_else(|_| source.path.clone()),
                    )
                });
            seen.insert((source.provider, identity))
        })
        .collect()
}

#[derive(Debug, Default)]
struct SourceReconciliation {
    aliases: Vec<String>,
    requires_reparse: bool,
    session_id: Option<String>,
}

fn source_reconciliation(
    db: &Db,
    sources: &[SourceFile],
) -> Result<HashMap<(Provider, String), SourceReconciliation>> {
    let mut indexed_by_identity =
        HashMap::<(Provider, String), Vec<(String, usize, String)>>::new();
    for (provider, stored_path, sessions, session_id) in db.indexed_source_identities()? {
        let Ok(canonical) = std::fs::canonicalize(&stored_path) else {
            continue;
        };
        indexed_by_identity
            .entry((provider, normalize_path(&canonical)))
            .or_default()
            .push((stored_path, sessions, session_id));
    }
    Ok(sources
        .iter()
        .filter_map(|source| {
            let path = normalize_path(&source.path);
            let indexed = indexed_by_identity.get(&(source.provider, path.clone()))?;
            let aliases = indexed
                .iter()
                .filter(|(stored, _, _)| stored != &path)
                .map(|(stored, _, _)| stored.clone())
                .collect::<Vec<_>>();
            let requires_reparse = !aliases.is_empty()
                || indexed
                    .iter()
                    .map(|(_, sessions, _)| sessions)
                    .sum::<usize>()
                    > 1;
            let session_id =
                (indexed.len() == 1 && indexed[0].1 == 1).then(|| indexed[0].2.clone());
            Some((
                (source.provider, path),
                SourceReconciliation {
                    aliases,
                    requires_reparse,
                    session_id,
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
        if db.schema_is_readable()? {
            return Ok(false);
        }
        if let SchemaState::UnknownProviders { providers } = db.schema_state()? {
            anyhow::bail!(
                "{}",
                SchemaState::unknown_providers_message(&config.db_path(), &providers)
            );
        }
        anyhow::bail!(
            "index schema generation {} is newer than this aise build supports (maximum {}); upgrade aise before opening {}",
            db.schema_version()?,
            crate::db::SCHEMA_VERSION,
            config.db_path().display()
        );
    }
    with_index_update_lock(config, || {
        if !db.needs_backfill()? {
            return Ok(false);
        }
        reindex(config, db, true, progress)?;
        db.purge_injected_messages()?;
        db.mark_schema_current()?;
        if db.schema_version()? < SCHEMA_VERSION {
            db.migrate_message_search_schema_exclusive()?;
        }
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
    Cancelled,
}

/// Try to incrementally append only the bytes appended to a session file since its last
/// checkpoint, reusing that provider's real parser (`parse_slice`) over the appended slice
/// ([`crate::tail`]). The preconditions (a stored checkpoint, no truncation, an unchanged file
/// head) make this a pure optimization: on any doubt it returns [`TailOutcome::FullParse`] and
/// the caller re-reads the whole file, so correctness never depends on the fast path.
fn try_tail<F>(
    source: &SourceFile,
    source_path: &str,
    expected_session_id: Option<&str>,
    db: &Db,
    should_cancel: &dyn Fn() -> bool,
    parse_slice: F,
) -> Result<TailOutcome>
where
    F: Fn(std::io::Cursor<Vec<u8>>, &std::path::Path) -> Result<crate::models::ParsedSession>,
{
    let Some((offset, stored_fingerprint)) = db.file_checkpoint(
        source.provider,
        source_path,
        crate::util::provider_parse_version(source.provider),
    )?
    else {
        return Ok(TailOutcome::FullParse);
    };
    if source.provider == Provider::ClaudeDesktop {
        return Ok(TailOutcome::FullParse);
    }
    // Truncation / copytruncate: the file is now shorter than where we parsed to → re-read whole.
    if offset <= 0 || source.size_bytes <= offset {
        return Ok(TailOutcome::FullParse);
    }
    // Rewrite / rotation: the head bytes changed → the stored offset is meaningless → re-read.
    if !crate::tail::fingerprint_matches(&source.path, &stored_fingerprint)? {
        return Ok(TailOutcome::FullParse);
    }
    match crate::tail::tail_parse_until(&source.path, offset, should_cancel, parse_slice) {
        Ok(crate::tail::TailParseRun::Complete(Some(mut tail))) => {
            // Some providers declare their immutable session ID only near the file head, outside
            // the bounded tail overlap. Never insert child rows under a fallback ID: re-read the
            // complete source so parent replacement and child publication stay one transaction.
            if expected_session_id != Some(tail.session.id.as_str()) {
                return Ok(TailOutcome::FullParse);
            }
            // Fills the tail's volatile end for `append_tail`; the tail's `created_at` is never
            // persisted (`append_tail` advances `updated_at`/`last_message_at` only), so nothing
            // here validates it.
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
        Ok(crate::tail::TailParseRun::Complete(None)) => Ok(TailOutcome::NothingNew),
        Ok(crate::tail::TailParseRun::Cancelled) => Ok(TailOutcome::Cancelled),
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

    #[test]
    fn full_parse_checkpoint_never_advances_past_the_discovered_snapshot() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, b"first\n").unwrap();
        let source = SourceFile {
            provider: Provider::Claude,
            path: path.clone(),
            mtime_ns: 0,
            size_bytes: std::fs::metadata(&path).unwrap().len() as i64,
        };
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"concurrent append\n")
            .unwrap();

        let (offset, _) = full_parse_checkpoint(&source).unwrap();
        assert_eq!(
            offset, 0,
            "a later reopen must not checkpoint concurrently appended bytes the parser may not have read"
        );
    }
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
        config.providers.prime_agent.enabled = false;
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
        config.providers.prime_agent.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;
        config
    }

    fn force_legacy_parser_layout(path: &std::path::Path, version: i64) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "drop trigger if exists messages_ai;
             drop trigger if exists messages_ad;
             drop trigger if exists messages_au;
             drop table if exists messages_trigram_terms;
             drop table if exists messages_trigram_vocab;
             drop table if exists messages_trigram;
             pragma user_version=0;",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        crate::fts::install_released_message_word_index(&conn).unwrap();
        crate::trigram_index::ensure_schema(&conn).unwrap();
    }

    #[test]
    fn schema_inspection_is_read_only_total_and_does_not_create_missing_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        let coordinator = IndexCoordinator::new(&config);

        assert_eq!(coordinator.inspect_schema().unwrap(), SchemaState::Missing);
        assert!(!db_path.exists());

        let db = Db::open(&db_path).unwrap();
        db.mark_schema_current().unwrap();
        drop(db);
        assert_eq!(coordinator.inspect_schema().unwrap(), SchemaState::Current);

        for version in 1..crate::db::SCHEMA_VERSION {
            let legacy_path = dir.path().join(format!("v{version}.db"));
            drop(Db::open(&legacy_path).unwrap());
            force_legacy_parser_layout(&legacy_path, version);
            let conn = rusqlite::Connection::open(&legacy_path).unwrap();
            let schema_before: String = conn
                .query_row(
                    "select group_concat(coalesce(sql, ''), char(10))
                       from (select sql from sqlite_schema order by type, name)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            drop(conn);

            let legacy_config = config_with_no_providers(&legacy_path);
            assert_eq!(
                IndexCoordinator::new(&legacy_config)
                    .inspect_schema()
                    .unwrap(),
                SchemaState::Older {
                    current: version,
                    required: crate::db::SCHEMA_VERSION,
                }
            );

            let conn = rusqlite::Connection::open_with_flags(
                &legacy_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let schema_after: String = conn
                .query_row(
                    "select group_concat(coalesce(sql, ''), char(10))
                       from (select sql from sqlite_schema order by type, name)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                schema_after, schema_before,
                "v{version} inspection mutated schema"
            );
        }
    }

    #[test]
    fn explicit_migration_error_names_database_and_safe_retry_action() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        drop(Db::open(&db_path).unwrap());
        force_legacy_parser_layout(&db_path, 3);
        let db = Db::open(&db_path).unwrap();
        // Force a genuine migration failure that survives the idempotent pre-drop of the trigram
        // objects and lets the reindex stage (no providers) complete: a stray VIEW squatting the
        // `messages_trigram` name. `install_target_message_search_indexes` runs
        // `drop table if exists messages_trigram` (a no-op against a view), then `create virtual
        // table messages_trigram` fails "there is already an object named messages_trigram". The
        // wrapper must still name the database and the safe retry.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("create view messages_trigram as select 1 as conflict")
            .unwrap();
        drop(conn);
        let config = config_with_no_providers(&db_path);

        let error = match explicit_reindex_and_migrate(&config, &db, true, None) {
            Ok(_) => panic!("migration over a name-squatting view unexpectedly succeeded"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains(&db_path.display().to_string()), "{error}");
        assert!(error.contains("free disk space"), "{error}");
        assert!(error.contains("write access"), "{error}");
        assert!(error.contains("aise reindex --full"), "{error}");
    }

    #[test]
    fn schema_inspection_classifies_invalid_database_as_offline_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        std::fs::write(&db_path, b"not a sqlite database").unwrap();
        let config = config_with_no_providers(&db_path);

        let state = IndexCoordinator::new(&config).inspect_schema().unwrap();

        assert!(matches!(state, SchemaState::RecoveryRequired { .. }));
    }

    #[test]
    fn schema_inspection_classifies_hybrid_layout_repairable_and_base_loss_recovery() {
        // A v4 stamp with a broken derived layout is REPAIRABLE when every base table is intact
        // (self-heal rebuilds the FTS5 objects online), but still requires offline RECOVERY when a
        // base table itself is missing (the base rows a rebuild would read are gone).
        for (case, mutation, expected_reason, repairable) in [
            (
                "obsolete-custom-index",
                "create table trigram_postings(tg text primary key, ids blob not null, df integer not null);",
                "obsolete",
                true,
            ),
            (
                "missing-trigram-vocabulary",
                "drop table messages_trigram_vocab;",
                "missing",
                true,
            ),
            (
                "missing-base-table",
                "drop table file_edits;",
                "missing",
                false,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join(format!("{case}.db"));
            drop(Db::open(&db_path).unwrap());
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(mutation).unwrap();
            drop(conn);

            let config = config_with_no_providers(&db_path);
            let state = IndexCoordinator::new(&config).inspect_schema().unwrap();
            match (state, repairable) {
                (SchemaState::RepairableLayout { reason }, true) => {
                    assert!(reason.contains(expected_reason), "{case}: {reason}");
                }
                (SchemaState::RecoveryRequired { reason }, false) => {
                    assert!(reason.contains(expected_reason), "{case}: {reason}");
                }
                (other, _) => panic!("{case}: unexpected schema state {other:?}"),
            }
        }
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

    /// `reindex(full = true)` must not delete a session whose source file is gone.
    ///
    /// Agent harnesses clear old sessions on their own schedule, so the index is routinely the
    /// only surviving copy of that conversation. Measured on live data: 549 subagent runs in one
    /// project directory named 69 distinct parent sessions and NOT ONE of those parent
    /// transcripts was still on disk. A full reindex that rebuilt from the filesystem would
    /// destroy exactly the history this tool exists to keep.
    ///
    /// The property was documented on `reindex` and enforced by nothing. It is asserted here at
    /// the level that matters — the row, its messages, and its transcript text all survive —
    /// because a row retained without its content is still a loss. `Db::clear_all` remains the
    /// only total wipe, and reindex must never call it.
    #[test]
    fn full_reindex_retains_sessions_whose_source_file_was_deleted() {
        use crate::models::MessageFilters;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        let parent = "5137d2d7-7297-4bac-9995-87347d0b6f5f";
        let runs = claude_root.join(parent).join("subagents");
        std::fs::create_dir_all(&runs).unwrap();

        let parent_file = claude_root.join(format!("{parent}.jsonl"));
        std::fs::write(
            &parent_file,
            format!(
                r#"{{"sessionId":"{parent}","cwd":"/tmp/project","type":"user","message":{{"role":"user","content":"parent-needle"}}}}
"#
            ),
        )
        .unwrap();
        // A spawned run, which outlives its spawner more often than not.
        let run_file = runs.join("agent-a47504e0af44bd955.jsonl");
        std::fs::write(
            &run_file,
            format!(
                r#"{{"sessionId":"{parent}","isSidechain":true,"agentId":"a47504e0af44bd955","cwd":"/tmp/project","type":"user","message":{{"role":"user","content":"run-needle"}}}}
"#
            ),
        )
        .unwrap();

        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, true, None).unwrap();

        let ids = |db: &Db| {
            let mut ids: Vec<String> = db
                .list_recent(&crate::models::SearchFilters::default())
                .unwrap()
                .into_iter()
                .map(|session| session.id)
                .collect();
            ids.sort();
            ids
        };
        let before = ids(&db);
        assert_eq!(
            before,
            vec![
                format!("claude:{parent}"),
                format!("claude:{parent}/agent-a47504e0af44bd955"),
            ],
            "both the spawner and its run are indexed to begin with"
        );

        // The harness clears both files.
        std::fs::remove_file(&parent_file).unwrap();
        std::fs::remove_file(&run_file).unwrap();
        let (files_seen, _) = reindex(&config, &db, true, None).unwrap();
        assert_eq!(files_seen, 0, "nothing is discoverable to re-parse");

        assert_eq!(
            ids(&db),
            before,
            "a full reindex is an archive, not a rebuild"
        );
        // Rows without content would be a loss dressed as a retention.
        for needle in ["parent-needle", "run-needle"] {
            assert_eq!(
                db.search_messages(needle, &MessageFilters::default())
                    .unwrap()
                    .len(),
                1,
                "{needle} must still be searchable after its source file is gone"
            );
        }
    }

    #[test]
    fn incremental_reindex_reclaims_an_id_from_an_unavailable_holder() {
        use crate::models::MessageFilters;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        let project = claude_root.join("-tmp-project");
        std::fs::create_dir_all(&project).unwrap();
        let shared = "3f260ba5-b3b0-4e79-9488-b9832ded0835";
        for name in [shared, "aaaaaaaa-1111-2222-3333-444444444444"] {
            std::fs::write(
                project.join(format!("{name}.jsonl")),
                format!(
                    r#"{{"type":"user","sessionId":"{shared}","cwd":"/tmp/project","message":{{"role":"user","content":"turn in {name}"}}}}
"#
                ),
            )
            .unwrap();
        }

        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, true, None).unwrap();
        let explanation = crate::diagnostics::explain_unindexed(&config, &db).unwrap();
        assert_eq!(explanation.len(), 1);
        let candidate = std::path::PathBuf::from(&explanation[0].path);
        let holder = std::path::PathBuf::from(
            explanation[0]
                .id_already_held_by
                .as_deref()
                .expect("the other live file must hold the shared id"),
        );
        let (_, updated) = reindex(&config, &db, false, None).unwrap();
        assert_eq!(
            updated, 0,
            "two live files claiming one provider id remain an explained collision"
        );
        std::fs::remove_file(holder).unwrap();

        let (_, updated) = reindex(&config, &db, false, None).unwrap();
        assert_eq!(updated, 1, "the surviving source must be reclaimed");
        assert_eq!(
            crate::diagnostics::collect(&config, &db)
                .unwrap()
                .index_status
                .unindexed_files,
            0,
            "incremental repair must not keep prescribing itself after the old holder vanished"
        );
        let candidate_name = candidate.file_stem().unwrap().to_string_lossy();
        assert_eq!(
            db.search_messages(
                &format!("turn in {candidate_name}"),
                &MessageFilters::default()
            )
            .unwrap()
            .len(),
            1,
            "the surviving transcript, not merely its source path, must be searchable"
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

    /// An index written before the message-class index existed gains it on the next refresh:
    /// `open` skips the build on a populated table (it runs in every query process), the elected
    /// refresh writer builds it once, and it stays put afterwards.
    #[test]
    fn refresh_builds_the_message_kind_index_an_older_aise_did_not_create() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        drop(Db::open(&path).unwrap());
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(&format!(
                "insert into sessions(id, provider, provider_session_id, preview_text, source_path, \
                   parse_version, discovery_source) \
                 values('claude:s1', 'claude', 's1', '', '/x', 'v1', 'jsonl');
                 insert into messages(session_id, provider, seq, role, content) \
                 values('claude:s1', 'claude', 0, 'user', 'hello');
                 drop index {};",
                crate::db::MESSAGE_KIND_INDEX
            ))
            .unwrap();
        let reopened = Db::open(&path).unwrap();
        assert!(
            !reopened.message_kind_index_exists().unwrap(),
            "open leaves the build to the refresh writer on a populated index"
        );
        let config = config_with_no_providers(&path);
        auto_reindex(&config, &reopened, None).unwrap();
        assert!(
            reopened.message_kind_index_exists().unwrap(),
            "the elected refresh writer builds the missing class index"
        );
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
    fn parser_contract_change_overrides_recent_refresh_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let db = Db::open(&path).unwrap();
        let config = config_with_no_providers(&path);
        db.mark_schema_current().unwrap();
        db.mark_auto_reindex_complete().unwrap();

        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "delete from index_metadata
                 where key like 'auto_reindex_parser_contract_%'",
                [],
            )
            .unwrap();

        assert!(
            auto_refresh_is_due(&db, config.index.auto_reindex_interval_ms).unwrap(),
            "a newer parser contract must not inherit an older build's freshness interval"
        );
        assert_eq!(
            auto_reindex(&config, &db, None).unwrap(),
            AutoReindexOutcome::Updated {
                files_seen: 0,
                sessions_updated: 0,
            }
        );
        assert_eq!(
            auto_reindex(&config, &db, None).unwrap(),
            AutoReindexOutcome::SkippedFresh,
            "recording the current parser contract must restore the ordinary freshness interval"
        );
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
    fn tail_identity_mismatch_falls_back_to_full_parse() {
        use crate::models::MessageFilters;
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        let source = claude_root.join("session.jsonl");
        let mut initial = concat!(
            r#"{"sessionId":"current-id","type":"user","message":{"role":"user","content":"first prompt"}}"#,
            "\n",
        )
        .to_string();
        let mut ignored = format!(r#"{{"type":"progress","padding":"{}"}}"#, "x".repeat(1024));
        ignored.push('\n');
        while initial.len() <= crate::tail::OVERLAP_BYTES as usize + crate::tail::FINGERPRINT_LEN {
            initial.push_str(&ignored);
        }
        std::fs::write(&source, initial).unwrap();

        let source_path = normalize_path(&source);
        let db = Db::open(&db_path).unwrap();
        let mut migrated = crate::util::minimal_record(Provider::Claude, &source, String::new());
        migrated.session.id = "claude:migrated-id".into();
        migrated.session.provider_session_id = "migrated-id".into();
        migrated.session.source_path = source_path.clone();
        migrated.session.parse_version =
            crate::util::provider_parse_version(Provider::Claude).into();
        db.upsert_session(
            &migrated,
            1,
            std::fs::metadata(&source).unwrap().len() as i64,
        )
        .unwrap();
        db.set_file_checkpoint(
            Provider::Claude,
            &source_path,
            crate::tail::complete_prefix_offset(&source).unwrap(),
            &crate::tail::prefix_fingerprint(&source).unwrap(),
        )
        .unwrap();

        OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(
                concat!(
                    r#"{"sessionId":"current-id","type":"user","message":{"role":"user","content":"second prompt"}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();

        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let adapters = ProviderSet::new(&config);
        let source_file = adapters
            .discover_enabled(&config)
            .sources
            .into_iter()
            .next()
            .unwrap();
        let outcome = try_tail(
            &source_file,
            &source_path,
            Some("claude:migrated-id"),
            &db,
            &|| false,
            |reader, path| adapters.claude.parse_reader(reader, path),
        )
        .unwrap();
        assert!(
            matches!(outcome, TailOutcome::FullParse),
            "identity drift must bypass incremental child inserts"
        );
        let (_seen, updated) = reindex(&config, &db, false, None).unwrap();

        assert_eq!(updated, 1);
        assert!(db.resolve_session_record("claude:migrated-id").is_err());
        assert_eq!(
            db.search_messages("prompt", &MessageFilters::default())
                .unwrap()
                .len(),
            2
        );
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
        db.mark_schema_current().unwrap();
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

    #[test]
    fn existing_only_never_touches_the_update_lock_and_requires_a_usable_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.index.refresh = IndexRefresh::ExistingOnly;
        drop(Db::open(&db_path).unwrap());
        force_legacy_parser_layout(&db_path, 1);
        let db = Db::open(&db_path).unwrap();
        let lock_path = index_update_lock_path(&db_path);

        let error = prepare_index_for_read(&config, &db).unwrap_err();
        assert!(error.to_string().contains("run `aise reindex`"));
        assert!(!lock_path.exists());

        db.mark_schema_current().unwrap();
        std::fs::create_dir(&lock_path).unwrap();
        assert!(prepare_index_for_read(&config, &db).unwrap().is_none());
        assert!(lock_path.is_dir());
    }

    #[test]
    fn before_query_finishes_schema_preparation_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.index.refresh = IndexRefresh::BeforeQuery;
        let db = Db::open(&db_path).unwrap();

        let outcome = prepare_index_for_read(&config, &db).unwrap();

        assert!(matches!(outcome, Some(AutoReindexOutcome::Updated { .. })));
        assert!(!db.needs_backfill().unwrap());
        assert!(index_update_lock_path(&db_path).is_file());
    }

    #[test]
    fn immediate_auto_read_repairs_only_an_unusable_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        drop(Db::open(&db_path).unwrap());
        force_legacy_parser_layout(&db_path, 1);
        let db = Db::open(&db_path).unwrap();

        let first = prepare_index_for_read_now(&config, &db).unwrap();
        assert!(matches!(first, Some(AutoReindexOutcome::Updated { .. })));
        assert!(!db.needs_backfill().unwrap());

        std::fs::create_dir(index_update_lock_path(&db_path)).unwrap_err();
        let second = prepare_index_for_read_now(&config, &db).unwrap();
        assert!(
            second.is_none(),
            "a usable auto index must be served immediately"
        );
    }

    #[test]
    fn immediate_auto_read_populates_a_fresh_empty_index_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        std::fs::write(
            claude_root.join("session.jsonl"),
            concat!(
                r#"{"sessionId":"first-use","type":"user","message":{"role":"user","content":"first prompt"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let db = Db::open(&db_path).unwrap();

        assert!(!db.has_sessions().unwrap());
        let outcome = prepare_index_for_read_now(&config, &db).unwrap();

        assert!(matches!(
            outcome,
            Some(AutoReindexOutcome::Updated {
                files_seen: 1,
                sessions_updated: 1,
            })
        ));
        assert!(db.has_sessions().unwrap());
    }

    #[test]
    fn compatible_schema_is_served_immediately_then_upgraded_in_background() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        drop(Db::open(&db_path).unwrap());
        force_legacy_parser_layout(&db_path, 2);
        let db = Db::open(&db_path).unwrap();
        db.mark_auto_reindex_complete().unwrap();

        assert!(db.needs_backfill().unwrap());
        assert!(db.schema_is_readable().unwrap());
        assert!(auto_refresh_is_due(&db, config.index.auto_reindex_interval_ms).unwrap());
        assert!(prepare_index_for_read_now(&config, &db).unwrap().is_none());
        assert!(
            db.needs_backfill().unwrap(),
            "the foreground read is nonmutating"
        );

        let outcome =
            refresh_usable_index_nonblocking(&config, &db, &|| false, None, None).unwrap();
        assert!(matches!(outcome, BackgroundRefreshOutcome::Updated { .. }));
        assert!(!db.needs_backfill().unwrap());
    }

    #[test]
    fn existing_only_serves_compatible_schema_without_creating_update_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.index.refresh = IndexRefresh::ExistingOnly;
        let db = Db::open(&db_path).unwrap();
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .pragma_update(None, "user_version", 2)
            .unwrap();

        assert!(prepare_index_for_read_now(&config, &db).unwrap().is_none());
        assert!(!index_update_lock_path(&db_path).exists());
        assert!(db.needs_backfill().unwrap());
    }

    #[test]
    fn future_schema_generation_fails_closed_with_upgrade_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        let db = Db::open(&db_path).unwrap();
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .pragma_update(None, "user_version", crate::db::SCHEMA_VERSION + 1)
            .unwrap();

        let error = prepare_index_for_read_now(&config, &db).unwrap_err();
        assert!(error.to_string().contains("newer than this aise build"));
        assert!(error.to_string().contains("upgrade aise"));
        assert!(!index_update_lock_path(&db_path).exists());
    }

    #[test]
    fn background_refresh_returns_immediately_when_an_updater_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        let db = Db::open(&db_path).unwrap();
        db.mark_schema_current().unwrap();
        let mut lock = open_index_update_lock(&index_update_lock_path(&db_path)).unwrap();
        let _guard = lock.write().unwrap();

        let outcome =
            refresh_usable_index_nonblocking(&config, &db, &|| false, None, None).unwrap();

        assert_eq!(outcome, BackgroundRefreshOutcome::SkippedBusy);
    }

    #[test]
    fn fresh_empty_immediate_read_never_waits_for_another_elected_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_no_providers(&db_path);
        let _db = Db::open(&db_path).unwrap();
        let mut lock = open_index_update_lock(&index_update_lock_path(&db_path)).unwrap();
        let guard = lock.write().unwrap();
        let (finished, finished_rx) = std::sync::mpsc::sync_channel(1);
        let reader_config = config.clone();
        let reader_path = db_path.clone();
        let reader = std::thread::spawn(move || {
            let reader_db = Db::open(&reader_path).unwrap();
            finished
                .send(prepare_index_for_read_now(&reader_config, &reader_db))
                .unwrap();
        });

        let before_writer_release = finished_rx.recv_timeout(std::time::Duration::from_millis(100));
        let finished_without_waiting = before_writer_release.is_ok();
        drop(guard);
        let outcome = match before_writer_release {
            Ok(outcome) => outcome,
            Err(_) => finished_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("reader finishes after the test releases the writer"),
        };
        reader.join().unwrap();

        assert!(
            finished_without_waiting,
            "a fresh-empty implicit read must not block behind another process's writer"
        );
        assert_eq!(outcome.unwrap(), Some(AutoReindexOutcome::SkippedBusy));
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
        drop(Db::open(&db_path).unwrap());
        force_legacy_parser_layout(&db_path, 1);
        let db = Db::open(&db_path).unwrap();
        std::fs::create_dir(index_update_lock_path(&db_path)).unwrap();

        let err = refresh_index_opportunistically(&config, &db, None).unwrap_err();

        assert!(err.downcast_ref::<IndexUpdateLockError>().is_some());
    }

    #[test]
    fn discovered_sources_deduplicate_physical_aliases_per_provider() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("session.jsonl");
        let alias = dir.path().join("alias.jsonl");
        std::fs::write(&original, "{}\n").unwrap();
        std::fs::hard_link(&original, &alias).unwrap();
        let source = |provider, path: &std::path::Path| {
            let metadata = std::fs::metadata(path).unwrap();
            SourceFile {
                provider,
                path: path.to_path_buf(),
                mtime_ns: 1,
                size_bytes: metadata.len() as i64,
            }
        };

        let sources = deduplicate_sources(vec![
            source(Provider::Claude, &original),
            source(Provider::Claude, &alias),
            source(Provider::Codex, &alias),
        ]);

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].provider, Provider::Claude);
        assert_eq!(sources[0].path, original);
        assert_eq!(sources[1].provider, Provider::Codex);
    }

    #[test]
    fn cancellable_reindex_stops_between_source_transactions() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        for session_id in ["one", "two"] {
            std::fs::write(
                claude_root.join(format!("{session_id}.jsonl")),
                format!(
                    "{{\"sessionId\":\"{session_id}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n"
                ),
            )
            .unwrap();
        }
        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let db = Db::open(&db_path).unwrap();
        let run = reindex_until(&config, &db, false, None, &|| {
            db.message_count().unwrap() >= 1
        })
        .unwrap();

        assert_eq!(
            run,
            ReindexRun::Cancelled {
                files_seen: 1,
                sessions_updated: 1,
                discovery_warnings: Vec::new(),
            }
        );
    }

    #[test]
    fn cancellable_reindex_stops_during_one_large_provider_scan() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        std::fs::write(
            claude_root.join("large.jsonl"),
            format!(
                "{{\"sessionId\":\"large\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n",
                serde_json::to_string(&"x".repeat(256 * 1024)).unwrap()
            ),
        )
        .unwrap();
        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let db = Db::open(&db_path).unwrap();
        let checks = std::cell::Cell::new(0usize);

        let run = reindex_until(&config, &db, false, None, &|| {
            let next = checks.get() + 1;
            checks.set(next);
            next >= 8
        })
        .unwrap();

        assert_eq!(
            run,
            ReindexRun::Cancelled {
                files_seen: 0,
                sessions_updated: 0,
                discovery_warnings: Vec::new(),
            }
        );
        assert_eq!(db.message_count().unwrap(), 0);
        assert!(checks.get() >= 8);
    }

    #[test]
    fn incremental_reindex_reparses_once_when_source_checkpoint_version_changes() {
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
        let (authorship, record_relation): (String, String) = conn
            .query_row(
                "select authorship, record_relation
                 from messages
                 where session_id = 'claude:s1' and seq = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(authorship, "human");
        assert_eq!(record_relation, "original");
        let (_seen, updated) = reindex(&config, &db, false, None).unwrap();
        assert_eq!(
            updated, 0,
            "the source checkpoint must converge after reparse"
        );
    }

    #[test]
    fn grown_file_with_stale_parser_checkpoint_replaces_prefix_instead_of_tail_appending() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        let session_file = claude_root.join("session.jsonl");
        let line = |role: &str, content: &str| {
            format!(
                "{{\"sessionId\":\"s1\",\"type\":\"{role}\",\
                 \"message\":{{\"role\":\"{role}\",\"content\":\"{content}\"}}}}\n"
            )
        };
        std::fs::write(&session_file, line("user", "first prompt")).unwrap();

        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let db = Db::open(&db_path).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (_seen, updated) = reindex(&config, &db, false, None).unwrap();
        assert_eq!(updated, 1);
        let source_path: String = conn
            .query_row(
                "select source_path from files_seen where provider = 'claude'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "update messages set content = 'STALE_PREFIX' where session_id = 'claude:s1'",
            [],
        )
        .unwrap();
        let changed = conn
            .execute(
                "update files_seen set parse_version = 'claude-v1'
                 where provider = 'claude'",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1, "the fixture must stale exactly one source row");
        assert_eq!(
            db.source_file_refresh_state(
                Provider::Claude,
                &source_path,
                2,
                std::fs::metadata(&session_file).unwrap().len() as i64,
                crate::util::provider_parse_version(Provider::Claude),
            )
            .unwrap(),
            SourceFileRefreshState::ChangedParser,
        );

        OpenOptions::new()
            .append(true)
            .open(&session_file)
            .unwrap()
            .write_all(line("assistant", "second reply").as_bytes())
            .unwrap();
        let adapters = ProviderSet::new(&config);
        let source_file = adapters
            .discover_enabled(&config)
            .sources
            .into_iter()
            .next()
            .unwrap();
        let tail_outcome = try_tail(
            &source_file,
            &source_path,
            Some("claude:s1"),
            &db,
            &|| false,
            |reader, path| adapters.claude.parse_reader(reader, path),
        )
        .unwrap();
        assert!(matches!(tail_outcome, TailOutcome::FullParse));
        assert_eq!(
            db.source_file_refresh_state(
                Provider::Claude,
                &source_path,
                2,
                std::fs::metadata(&session_file).unwrap().len() as i64,
                crate::util::provider_parse_version(Provider::Claude),
            )
            .unwrap(),
            SourceFileRefreshState::ChangedParser,
            "checking a stale tail checkpoint must not mutate its parser version"
        );
        let (_seen, updated) = reindex(&config, &db, false, None).unwrap();
        assert_eq!(updated, 1);
        let contents = db
            .read_session_messages(
                &crate::models::MessageFilters {
                    session_id: Some("claude:s1".to_string()),
                    ..Default::default()
                },
                crate::db::MessageOrder::OldestFirst,
            )
            .unwrap()
            .into_iter()
            .map(|message| message.content)
            .collect::<Vec<_>>();
        assert_eq!(contents, ["first prompt", "second reply"]);
    }
}
