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
use crate::db::{Db, SchemaState, SourceFamilyStamp, SourceFileRefreshState, SCHEMA_VERSION};
use crate::durable_fs::open_file_lock;
use crate::models::{Provider, SourceFile};
use crate::source::ProviderSet;
use crate::util::normalize_path;
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReindexMode {
    Strict,
    Opportunistic { busy_timeout_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
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
                let writable = crate::db::index_write_compatible(&conn)?;
                match current_schema_layout_problem(&conn)? {
                    Some(reason) => {
                        crate::db::require_index_write_compatibility(&conn)?;
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
                            if writable {
                                SchemaState::Current
                            } else {
                                SchemaState::ReaderOnly
                            }
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
        // `WouldBlock` is the whole of "another process owns it". A signal is not: it says
        // nothing about who holds the lock, so retrying is what answers the question, exactly as
        // the blocking sibling above does. Treating the two alike reported contention that had
        // not happened and skipped the refresh on the strength of it.
        let guard = loop {
            match lock.try_write() {
                Ok(guard) => break guard,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(IndexUpdateLockError {
                        path: lock_path,
                        source,
                    }
                    .into());
                }
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

/// Validate recorded-input storage, including the base search layout and additive columns.
///
/// These columns were added after schema generation 5 was released. They are deliberately not
/// part of [`current_schema_layout_problem`]: an older schema-5 index still owns the released FTS
/// and base-message layout, so unrestricted reads can continue from a read-only connection. The
/// user-input scope calls this check and reports the exact writable upgrade action instead.
pub(crate) fn message_input_provenance_layout_problem(
    conn: &rusqlite::Connection,
) -> Result<Option<String>> {
    if let Some(problem) = current_schema_layout_problem(conn)? {
        return Ok(Some(problem));
    }
    let columns = conn
        .prepare("pragma table_info(messages)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    let missing_columns = ["input_delivery_state", "input_origin"]
        .into_iter()
        .filter(|column| !columns.contains(*column))
        .collect::<Vec<_>>();
    if missing_columns.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!(
            "messages is missing user-input provenance column(s): {}",
            missing_columns.join(", ")
        )))
    }
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

/// Run a coordinated legacy auto-refresh; cost follows the discovered files and changed transcript bytes.
#[deprecated(
    since = "1.1.0",
    note = "use IndexService::prepare_for_read or IndexService::refresh"
)]
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
                db.classify_injected_messages()?;
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
    Ok(db.write_compatible()?
        && (db.needs_backfill()? || !db.auto_reindex_is_fresh(interval_ms)?))
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
                    db.classify_injected_messages()?;
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

#[non_exhaustive]
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
    db.require_write_compatibility()?;
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
        db.classify_injected_messages()?;
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
    let mut sources = deduplicate_sources(discovery.sources);
    let mut discovery_warnings = discovery.warnings;
    let discovered_source_paths = sources
        .iter()
        .map(|source| (source.provider, normalize_path(&source.path)))
        .collect::<HashSet<_>>();
    let indexed_source_paths = db
        .indexed_source_identities()?
        .into_iter()
        .map(|source| (source.provider, source.source_path))
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
    let gemini_families =
        gemini_migration_families(db, &sources, full, &mut discovery_warnings, should_cancel)?;
    order_gemini_family_successors_first(&mut sources, &gemini_families);
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
    let mut joined_gemini_families = HashSet::new();
    let persisted_gemini_families = if sources
        .iter()
        .any(|source| source.provider == Provider::GeminiCli)
    {
        db.gemini_family_sessions_by_path()?
    } else {
        HashMap::new()
    };
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
        let family = gemini_families.get(&source_path).cloned();
        if family
            .as_ref()
            .is_some_and(|family| family.predecessor_path == source_path)
            && family
                .as_ref()
                .is_some_and(|family| joined_gemini_families.contains(&family.successor_path))
        {
            // The successor was already parsed and persisted with both physical receipts. Do not
            // index the predecessor a second time after a successful family join.
            continue;
        }
        let reconciliation = source_reconciliation.get(&(source.provider, source_path.clone()));
        let current_identity = reconciliation.and_then(|item| item.current_identity);
        // New sources carry an ID for freshness, but do not prove an existing archive.
        let reconciliation =
            reconciliation.filter(|item| item.session_id.is_some() || item.requires_reparse);
        let requires_reconciliation = reconciliation.is_some_and(|item| item.requires_reparse);
        let expected_session_id = reconciliation.and_then(|item| item.session_id.as_deref());
        let parser_version = crate::util::provider_parse_version(source.provider);
        let refresh_state = db.source_file_refresh_state(
            source.provider,
            &source_path,
            source.mtime_ns,
            source.size_bytes,
            parser_version,
            current_identity,
        )?;
        let requires_reconciliation =
            requires_reconciliation || refresh_state == SourceFileRefreshState::ChangedIdentity;
        let persisted_gemini_family = (source.provider == Provider::GeminiCli)
            .then(|| persisted_gemini_families.get(&source_path).cloned())
            .flatten();
        if family.is_none()
            && persisted_gemini_family.is_some()
            && refresh_state == SourceFileRefreshState::Current
        {
            // A removed family member is an archive receipt, not an instruction to replace the
            // canonical mixed session with the weaker surviving rendering.
            continue;
        }
        // A prior ID collision can leave a current files_seen row whose session still points at a
        // source that has since vanished. Parse that one candidate instead of prescribing an
        // incremental reindex that skips it forever.
        let family_current = family
            .as_ref()
            .is_some_and(|family| !family.requires_reparse);
        let current_without_session = !full
            && refresh_state == SourceFileRefreshState::Current
            && reconciliation.is_none()
            && !family_current;
        let family_requires_reparse = family
            .as_ref()
            .is_some_and(|family| family.requires_reparse);
        if !full
            && !requires_reconciliation
            && !current_without_session
            && !family_requires_reparse
            && refresh_state == SourceFileRefreshState::Current
        {
            continue;
        }
        // Incremental tail-parse fast path: when we hold a checkpoint for this file, it only
        // grew (offset within it → not truncated), and its head bytes are unchanged (not
        // rewritten/rotated), parse + append ONLY the appended bytes instead of re-reading the
        // whole (possibly multi-hundred-MB) file. Each provider reuses its own `parse_reader`
        // over the appended slice; on any doubt it returns `FullParse` and we re-read below.
        if !full && !requires_reconciliation && !current_without_session && family.is_none() {
            let outcome = match source.provider {
                Provider::Claude | Provider::ClaudeDesktop => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p, context| {
                        adapters
                            .claude
                            .parse_reader_with_context(r, p, Some(context))
                    },
                )?,
                Provider::Codex => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p, context| {
                        adapters
                            .codex
                            .parse_reader_with_context(r, p, Some(context))
                    },
                )?,
                Provider::Cursor => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p, context| {
                        adapters
                            .cursor
                            .parse_reader_with_context(r, p, Some(context))
                    },
                )?,
                Provider::Antigravity => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p, context| {
                        adapters
                            .antigravity
                            .parse_reader_with_context(r, p, Some(context))
                    },
                )?,
                Provider::Pi => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p, context| adapters.pi.parse_reader_with_context(r, p, Some(context)),
                )?,
                Provider::PrimeAgent => try_tail(
                    source,
                    &source_path,
                    expected_session_id,
                    db,
                    should_cancel,
                    |r, p, context| {
                        adapters
                            .prime_agent
                            .parse_reader_with_context(r, p, Some(context))
                    },
                )?,
                Provider::AiStudio | Provider::GeminiCli => TailOutcome::FullParse,
            };
            #[cfg(test)]
            TEST_TAIL_OUTCOMES.with(|outcomes| outcomes.borrow_mut().push(outcome));
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
        let (joined_family, parsed_from_family) = if let Some(family) = family
            .as_ref()
            .filter(|family| family.successor_path == source_path)
            .filter(|family| family.requires_reparse)
        {
            match parse_gemini_migration_family(&adapters, family, should_cancel) {
                Ok(GeminiFamilyParse::Joined(parsed)) => (true, Some(*parsed)),
                Ok(GeminiFamilyParse::NotJoined(reason)) => {
                    discovery_warnings.push(gemini_family_warning(
                        &source.path,
                        format!("Gemini JSON/JSONL family join skipped after parsing: {reason}"),
                    ));
                    (false, None)
                }
                Ok(GeminiFamilyParse::Cancelled) => {
                    return Ok(ReindexRun::Cancelled {
                        files_seen: i,
                        sessions_updated: updated,
                        discovery_warnings,
                    })
                }
                Err(error) => {
                    discovery_warnings.push(gemini_family_warning(
                        &source.path,
                        format!("Gemini JSON/JSONL family parse failed: {error:#}"),
                    ));
                    (false, None)
                }
            }
        } else {
            (false, None)
        };
        // Cursor's file-derived anchor survives a full refresh on filesystems that
        // have no birth time. This keyed session lookup reads no source bytes.
        let cursor_clock_seed = if source.provider == Provider::Cursor {
            expected_session_id
                .map(|id| db.resolve_session_record(id))
                .transpose()?
                .as_ref()
                .map(crate::providers::cursor::clock_seed)
        } else {
            None
        };
        let mut parsed = match parsed_from_family {
            Some(parsed) => parsed,
            None => match adapters.parse_until_with_clock_seed(
                source,
                should_cancel,
                cursor_clock_seed.as_deref(),
            ) {
                Ok(Some(parsed)) => parsed,
                Ok(None) => {
                    return Ok(ReindexRun::Cancelled {
                        files_seen: i,
                        sessions_updated: updated,
                        discovery_warnings,
                    })
                }
                Err(error)
                    if indexed_source_paths.contains(&(source.provider, source_path.clone()))
                        || reconciliation.is_some() =>
                {
                    // A failed refresh is not a new archive state. Keep the last successful rows and
                    // their old files_seen metadata so the next refresh retries this source, and
                    // report the failure. A source reconciliation also proves an existing row through
                    // a canonical-path alias.
                    discovery_warnings.push(
                        crate::source::ProviderDiscoveryWarning::refresh_failed(
                            source.provider,
                            &source.path,
                            &error,
                        ),
                    );
                    continue;
                }
                Err(error) => {
                    crate::util::minimal_record(source.provider, &source.path, format!("{error:#}"))
                }
            },
        };
        let persisted_gemini_family = match persisted_gemini_family {
            Some(identity) if parsed.session.provider_session_id == identity.native_session_id => {
                parsed.session.id = identity.canonical_session_id.clone();
                Some(identity.canonical_session_id)
            }
            Some(identity) => {
                discovery_warnings.push(gemini_family_warning(
                    &source.path,
                    "Gemini native session ID changed; the previous family binding is not reused"
                        .to_string(),
                ));
                // A path-based parser ID or source retirement would overwrite the old reference.
                // Preserve that archive and leave the conflicting readable source unstamped.
                if parsed.session.id == identity.canonical_session_id
                    || db
                        .exact_session_source_path(&identity.canonical_session_id)?
                        .as_deref()
                        == Some(&source_path)
                {
                    continue;
                }
                None
            }
            None => None,
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
        if should_cancel() {
            return Ok(ReindexRun::Cancelled {
                files_seen: i,
                sessions_updated: updated,
                discovery_warnings,
            });
        }
        let aliases = reconciliation.map_or(&[][..], |item| item.aliases.as_slice());
        // Constraint failures are not actionable without the session and source file being
        // applied; retain both in the error chain.
        if joined_family {
            let family = family
                .as_ref()
                .expect("joined Gemini family has a family descriptor");
            let family_members = [
                SourceFamilyStamp {
                    source_path: family.predecessor_path.clone(),
                    mtime_ns: family.predecessor.mtime_ns,
                    size_bytes: family.predecessor.size_bytes,
                },
                SourceFamilyStamp {
                    source_path: family.successor_path.clone(),
                    mtime_ns: family.successor.mtime_ns,
                    size_bytes: family.successor.size_bytes,
                },
            ];
            db.upsert_session_reconciling_family(
                &parsed,
                source.mtime_ns,
                source.size_bytes,
                &family_members,
                false,
            )
        } else if persisted_gemini_family.is_some() {
            db.upsert_session_reconciling_family(
                &parsed,
                source.mtime_ns,
                source.size_bytes,
                &[SourceFamilyStamp {
                    source_path: source_path.clone(),
                    mtime_ns: source.mtime_ns,
                    size_bytes: source.size_bytes,
                }],
                false,
            )
        } else {
            db.upsert_session_reconciling_sources(
                &parsed,
                source.mtime_ns,
                source.size_bytes,
                aliases,
                false,
            )
        }
        .with_context(|| {
            format!(
                "failed to index session '{}' from {}",
                parsed.session.id,
                source.path.display()
            )
        })?;
        if joined_family {
            if let Some(family) = family.as_ref() {
                // The DB transaction committed both receipts. Mark the predecessor consumed only
                // after that atomic publication; a later checkpoint failure merely causes a safe
                // retry, never an unrecorded half-family.
                joined_gemini_families.insert(family.successor_path.clone());
            }
        }
        // Record/refresh the tail checkpoint so the next reindex of this grown file can append
        // incrementally from the end of what we just parsed (instead of re-reading it all).
        let (offset, fingerprint) = full_parse_checkpoint(source)?;
        let digest = crate::tail::overlap_digest(&source.path, offset)?;
        db.set_file_checkpoint_with_digest(
            source.provider,
            &source_path,
            offset,
            &fingerprint,
            Some(&digest),
        )?;
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
        // unmerged segments and redundant pages. Merge them back, but only on a full
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
/// Time O(D + 1) for D trailing bytes through the last newline; O(1) retained memory.
/// The head fingerprint and sidecar stat add constant work.
fn full_parse_checkpoint(source: &SourceFile) -> Result<(i64, String)> {
    let current_size = std::fs::metadata(&source.path)?.len() as i64;
    if crate::source::is_compressed_jsonl(&source.path) {
        // No tail checkpoint into compressed bytes; any later change takes the full parse.
        return Ok((0, crate::tail::prefix_fingerprint(&source.path)?));
    }
    let transcript_size =
        source.size_bytes - source.sidecar.as_ref().map_or(0, |meta| meta.size_bytes);
    let unchanged =
        current_size == transcript_size && crate::providers::claude::sidecar_is_current(source)?;
    let complete_offset = if unchanged {
        crate::tail::complete_prefix_offset(&source.path)?
    } else {
        0
    };
    // Provider parsers accept a valid final JSONL record without a trailing newline. Such a
    // record is already present in the parsed session, but complete_prefix_offset points to its
    // beginning. Store zero so the next change takes a full-parse fallback and cannot duplicate it.
    let offset = if unchanged && complete_offset == transcript_size {
        complete_offset
    } else {
        0
    };
    Ok((
        offset,
        source_fingerprint(source, &crate::tail::prefix_fingerprint(&source.path)?),
    ))
}

/// Stamp only the fixed-size prefix and auxiliary metadata, not transcript contents. O(1).
fn source_fingerprint(source: &SourceFile, prefix: &str) -> String {
    match &source.sidecar {
        Some(meta) => format!("{prefix};{}:{}", meta.mtime_ns, meta.size_bytes),
        None => prefix.to_owned(),
    }
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
    current_identity: Option<file_id::FileId>,
    aliases: Vec<String>,
    requires_reparse: bool,
    session_id: Option<String>,
}

#[derive(Debug, Clone)]
struct GeminiMigrationFamily {
    predecessor: SourceFile,
    successor: SourceFile,
    predecessor_path: String,
    successor_path: String,
    native_session_id: String,
    logical_session_id: String,
    requires_reparse: bool,
}

#[derive(Debug)]
enum GeminiFamilyParse {
    Joined(Box<crate::models::ParsedSession>),
    NotJoined(String),
    Cancelled,
}

/// Prove only the exact Google migration pair: one sibling `foo.json` and one `foo.jsonl`, with
/// the same non-empty native session ID in both files. Failed proof leaves both sources on the
/// ordinary indexing path; the warning is diagnostic, never a reason to hide readable history.
/// For a changed/new pair the proof scans each member once and leaves parsing to the canonical
/// successor's turn in the normal indexing loop. This keeps discovery retention at `O(F_pair)`
/// descriptors; at most one pair's parsed messages and `O(B_pair)` payload bytes exist during its
/// transaction. An unchanged pair consults the two `files_seen` receipts and does no transcript
/// read.
fn gemini_migration_families(
    db: &Db,
    sources: &[SourceFile],
    full: bool,
    discovery_warnings: &mut Vec<crate::source::ProviderDiscoveryWarning>,
    should_cancel: &dyn Fn() -> bool,
) -> Result<HashMap<String, GeminiMigrationFamily>> {
    let source_by_path = sources
        .iter()
        .filter(|source| source.provider == Provider::GeminiCli)
        .map(|source| (normalize_path(&source.path), source.clone()))
        .collect::<HashMap<_, _>>();
    if source_by_path.is_empty() {
        return Ok(HashMap::new());
    }
    let indexed = db.indexed_source_identities()?;
    let mut indexed_by_path = HashMap::<String, Vec<(String, usize)>>::new();
    for source in indexed {
        if source.provider == Provider::GeminiCli {
            indexed_by_path
                .entry(source.source_path)
                .or_default()
                .push((source.session_id, source.sessions));
        }
    }
    let parser_version = crate::util::provider_parse_version(Provider::GeminiCli);
    let mut plans = HashMap::new();
    for predecessor in sources.iter().filter(|source| {
        source.provider == Provider::GeminiCli
            && source.path.extension().and_then(|value| value.to_str()) == Some("json")
    }) {
        if should_cancel() {
            break;
        }
        let successor_path = normalize_path(&predecessor.path.with_extension("jsonl"));
        let Some(successor) = source_by_path.get(&successor_path) else {
            continue;
        };
        let predecessor_path = normalize_path(&predecessor.path);
        let predecessor_refresh = db.source_file_refresh_state(
            Provider::GeminiCli,
            &predecessor_path,
            predecessor.mtime_ns,
            predecessor.size_bytes,
            parser_version,
            None,
        )?;
        let successor_refresh = db.source_file_refresh_state(
            Provider::GeminiCli,
            &successor_path,
            successor.mtime_ns,
            successor.size_bytes,
            parser_version,
            None,
        )?;
        // A previously joined family has two current files_seen receipts. Do not scan either
        // transcript again just to rediscover a relationship that is already represented by the
        // canonical session and receipts; a change or a new member re-enters proof below.
        if !full
            && predecessor_refresh == SourceFileRefreshState::Current
            && successor_refresh == SourceFileRefreshState::Current
        {
            let mut existing_ids = HashSet::new();
            for path in [&predecessor_path, &successor_path] {
                existing_ids.extend(
                    indexed_by_path
                        .get(path)
                        .into_iter()
                        .flatten()
                        .map(|(session_id, _)| session_id.clone()),
                );
            }
            if existing_ids.len() > 1 {
                discovery_warnings.push(gemini_family_warning(
                    &predecessor.path,
                    "Gemini JSON/JSONL family left independent because both session identities are already indexed".to_string(),
                ));
            } else if let Some(logical_session_id) = existing_ids.into_iter().next() {
                // Both receipts and exactly one logical session are the persisted narrow family
                // evidence. Reuse it without rereading either unchanged transcript; if a member
                // changes on a later refresh, the branch above is bypassed and native proof runs.
                let family = GeminiMigrationFamily {
                    predecessor: predecessor.clone(),
                    successor: successor.clone(),
                    predecessor_path: predecessor_path.clone(),
                    successor_path: successor_path.clone(),
                    native_session_id: String::new(),
                    logical_session_id,
                    requires_reparse: false,
                };
                plans.insert(predecessor_path, family.clone());
                plans.insert(successor_path, family);
            }
            continue;
        }
        let predecessor_native_id =
            crate::providers::gemini_cli::native_session_id_until(&predecessor.path, should_cancel);
        if should_cancel() {
            break;
        }
        let successor_native_id =
            crate::providers::gemini_cli::native_session_id_until(&successor.path, should_cancel);
        let native_id = match (predecessor_native_id, successor_native_id) {
            (Ok(Some(left)), Ok(Some(right))) if left == right => left,
            (Ok(left), Ok(right)) => {
                discovery_warnings.push(gemini_family_warning(
                    &predecessor.path,
                    format!(
                        "Gemini JSON/JSONL family proof rejected: native session IDs were {left:?} and {right:?}"
                    ),
                ));
                continue;
            }
            (Err(error), _) | (_, Err(error)) => {
                discovery_warnings.push(gemini_family_warning(
                    &predecessor.path,
                    format!("Gemini JSON/JSONL family proof rejected: {error:#}"),
                ));
                continue;
            }
        };
        let mut existing_ids = HashSet::new();
        let mut existing_conflict = false;
        for path in [&predecessor_path, &successor_path] {
            for (session_id, count) in indexed_by_path.get(path).into_iter().flatten() {
                if *count != 1 {
                    existing_conflict = true;
                }
                existing_ids.insert(session_id.clone());
            }
        }
        if existing_conflict || existing_ids.len() > 1 {
            discovery_warnings.push(gemini_family_warning(
                &predecessor.path,
                format!(
                    "Gemini JSON/JSONL family proof rejected: {} independently indexed session identities already exist",
                    existing_ids.len()
                ),
            ));
            continue;
        }
        let logical_session_id = existing_ids
            .into_iter()
            .next()
            .unwrap_or_else(|| format!("gemini-cli:{predecessor_path}"));
        let requires_reparse = full
            || predecessor_refresh != SourceFileRefreshState::Current
            || successor_refresh != SourceFileRefreshState::Current;
        let family = GeminiMigrationFamily {
            predecessor: predecessor.clone(),
            successor: successor.clone(),
            predecessor_path: predecessor_path.clone(),
            successor_path: successor_path.clone(),
            native_session_id: native_id,
            logical_session_id,
            requires_reparse,
        };
        plans.insert(predecessor_path, family.clone());
        plans.insert(successor_path, family);
    }
    Ok(plans)
}

/// The family merge must run at the successor's normal indexing turn so the predecessor is not
/// indexed first and then forced through a second replacement. Exact sibling families are disjoint,
/// so a stable pair swap is enough and keeps the ordinary discovery pipeline single-pass.
fn order_gemini_family_successors_first(
    sources: &mut [SourceFile],
    families: &HashMap<String, GeminiMigrationFamily>,
) {
    // Normalize each discovered path once. Families are already proven disjoint, so a local
    // position map preserves the existing pair-swap behavior while avoiding a full source scan
    // (and repeated path allocation) for every family.
    let mut positions = HashMap::with_capacity(sources.len());
    for (position, source) in sources.iter().enumerate() {
        positions
            .entry(normalize_path(&source.path))
            .or_insert(position);
    }
    let mut swaps = Vec::new();
    let mut seen_predecessors = HashSet::new();
    for family in families.values() {
        if !seen_predecessors.insert(family.predecessor_path.clone()) {
            continue;
        }
        let Some(&predecessor) = positions.get(&family.predecessor_path) else {
            continue;
        };
        let Some(&successor) = positions.get(&family.successor_path) else {
            continue;
        };
        if predecessor < successor {
            swaps.push((predecessor, successor));
        }
    }
    for (predecessor, successor) in swaps {
        sources.swap(predecessor, successor);
    }
}

fn gemini_family_warning(path: &Path, message: String) -> crate::source::ProviderDiscoveryWarning {
    crate::source::ProviderDiscoveryWarning {
        provider: Provider::GeminiCli,
        path: normalize_path(path),
        operation: "migration_family_proof".to_string(),
        message,
        readable_sources_preserved: true,
        verification_command: "aise doctor --format json".to_string(),
        guidance: "The source files remain readable; run `aise doctor --format json` to inspect the source pair and any conflicting archived reference.".to_string(),
    }
}

fn parse_gemini_migration_family(
    adapters: &ProviderSet,
    family: &GeminiMigrationFamily,
    should_cancel: &dyn Fn() -> bool,
) -> Result<GeminiFamilyParse> {
    let Some(predecessor) = adapters.parse_until(&family.predecessor, should_cancel)? else {
        return Ok(GeminiFamilyParse::Cancelled);
    };
    if should_cancel() {
        return Ok(GeminiFamilyParse::Cancelled);
    }
    let Some(successor) = adapters.parse_until(&family.successor, should_cancel)? else {
        return Ok(GeminiFamilyParse::Cancelled);
    };
    if predecessor.session.provider_session_id != family.native_session_id
        || successor.session.provider_session_id != family.native_session_id
    {
        return Ok(GeminiFamilyParse::NotJoined(
            "native session ID changed between family proof and parse".to_string(),
        ));
    }
    match crate::providers::gemini_cli::merge_migration_family(successor, predecessor) {
        Ok(mut parsed) => {
            parsed.session.id = family.logical_session_id.clone();
            let metadata = parsed
                .session
                .raw_metadata_json
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .filter(serde_json::Value::is_object)
                .unwrap_or_else(|| json!({}));
            let mut metadata = metadata;
            if let Some(object) = metadata.as_object_mut() {
                object.insert(
                    "gemini_migration_family".to_string(),
                    json!({
                        "predecessor_json": family.predecessor_path,
                        "successor_jsonl": family.successor_path,
                        "native_session_id": family.native_session_id,
                    }),
                );
            }
            parsed.session.raw_metadata_json = Some(metadata.to_string());
            Ok(GeminiFamilyParse::Joined(Box::new(parsed)))
        }
        Err(error) => Ok(GeminiFamilyParse::NotJoined(format!("{error:#}"))),
    }
}

fn source_reconciliation(
    db: &Db,
    sources: &[SourceFile],
) -> Result<HashMap<(Provider, String), SourceReconciliation>> {
    let stored = db.indexed_source_identities()?;
    let mut by_canonical = HashMap::<_, Vec<_>>::new();
    let mut by_file = HashMap::<_, Vec<_>>::new();
    let mut by_path = HashMap::new();
    for entry in &stored {
        by_path.insert((entry.provider, entry.source_path.clone()), entry);
        if let Ok(path) = std::fs::canonicalize(&entry.source_path) {
            by_canonical
                .entry((entry.provider, normalize_path(&path)))
                .or_default()
                .push(entry);
        }
        if let (Some(id), Some(mtime), Some(size)) = (
            entry
                .file_identity
                .as_deref()
                .and_then(crate::db::file_identity_key),
            entry.mtime_ns,
            entry.size_bytes,
        ) {
            by_file
                .entry((entry.provider, id, mtime, size))
                .or_default()
                .push(entry);
        }
    }
    Ok(sources
        .iter()
        .map(|source| {
            let path = normalize_path(&source.path);
            let mut matches = by_canonical
                .get(&(source.provider, path.clone()))
                .cloned()
                .unwrap_or_default();
            // Missing receipts and changed stamps cannot prove a historical move; preserve the archive.
            let current_identity = file_id::get_file_id(&source.path).ok();
            if let Some(id) = current_identity {
                matches.extend(
                    by_file
                        .get(&(source.provider, id, source.mtime_ns, source.size_bytes))
                        .into_iter()
                        .flatten()
                        .copied(),
                );
            }
            // Compressed and resumed Codex siblings represent the same source even after removal.
            if source.provider == Provider::Codex {
                matches.extend(
                    crate::providers::codex::rollout_sibling(&source.path)
                        .and_then(|sibling| {
                            by_path.get(&(source.provider, normalize_path(&sibling)))
                        })
                        .copied(),
                );
            }
            matches.sort_unstable_by_key(|entry| &entry.source_path);
            matches.dedup_by_key(|entry| &entry.source_path);
            let aliases = matches
                .iter()
                .filter(|entry| entry.source_path != path)
                .map(|entry| entry.source_path.clone())
                .collect::<Vec<_>>();
            let requires_reparse = !aliases.is_empty()
                || matches.iter().map(|entry| entry.sessions).sum::<usize>() > 1;
            let session_id = (matches.len() == 1 && matches[0].sessions == 1)
                .then(|| matches[0].session_id.clone());
            (
                (source.provider, path),
                SourceReconciliation {
                    current_identity,
                    aliases,
                    requires_reparse,
                    session_id,
                },
            )
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
        db.classify_injected_messages()?;
        db.mark_schema_current()?;
        if db.schema_version()? < SCHEMA_VERSION {
            db.migrate_message_search_schema_exclusive()?;
        }
        db.mark_auto_reindex_complete()?;
        Ok(true)
    })
}

/// Outcome of an incremental tail-parse attempt for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg(test)]
thread_local! {
    static TEST_TAIL_OUTCOMES: std::cell::RefCell<Vec<TailOutcome>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Read a parser's conservative marker when records depend on an earlier prefix.
fn tail_requires_full_parse(session: &crate::models::SessionRecord) -> bool {
    session
        .raw_metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|metadata| metadata.get("requires_full_parse_for_tail").cloned())
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// Try to incrementally append only the bytes appended to a session file since its last
/// checkpoint, reusing that provider's real parser (`parse_slice`) over the appended slice
/// ([`crate::tail`]). The preconditions (a stored checkpoint, no truncation, an unchanged file
/// head) make this a pure optimization: on any doubt it returns [`TailOutcome::FullParse`] and
/// the caller re-reads the whole file, so correctness never depends on the fast path.
/// Parsing A appended bytes plus the fixed overlap costs O(A + overlap) time/memory.
/// Stamp checks are O(1). Persistence still counts O(M) stored session messages and copies
/// O(B + A) transcript bytes in SQLite, including transient memory, for B stored bytes.
/// Returned messages retain O(A + overlap) bytes. Digest validation shares the tail buffer,
/// adding O(overlap) hashing and no file I/O, plus existing table/FTS maintenance costs.
/// Stored Codex spawn context also validates its header record with one bounded range
/// read, adding O(H) hashing/I/O and O(1) streaming memory for H header bytes.
fn try_tail<F>(
    source: &SourceFile,
    source_path: &str,
    expected_session_id: Option<&str>,
    db: &Db,
    should_cancel: &dyn Fn() -> bool,
    parse_slice: F,
) -> Result<TailOutcome>
where
    F: Fn(
        std::io::Cursor<Vec<u8>>,
        &std::path::Path,
        crate::tail::TailContext,
    ) -> Result<crate::models::ParsedSession>,
{
    // A compressed rollout is immutable once written and its byte offsets are not JSONL offsets,
    // so it is always read whole.
    if crate::source::is_compressed_jsonl(&source.path) {
        return Ok(TailOutcome::FullParse);
    }
    let Some((offset, stored_fingerprint)) = db.file_checkpoint(
        source.provider,
        source_path,
        crate::util::provider_parse_version(source.provider),
    )?
    else {
        return Ok(TailOutcome::FullParse);
    };
    // Truncation / copytruncate: the file is now shorter than where we parsed to → re-read whole.
    let transcript_size =
        source.size_bytes - source.sidecar.as_ref().map_or(0, |meta| meta.size_bytes);
    if offset <= 0 || transcript_size <= offset {
        return Ok(TailOutcome::FullParse);
    }
    // Rewrite / rotation: the head bytes changed → the stored offset is meaningless → re-read.
    let prefix = stored_fingerprint.split(';').next().unwrap_or_default();
    if source_fingerprint(source, prefix) != stored_fingerprint
        || !crate::providers::claude::sidecar_is_current(source)?
        || !crate::tail::fingerprint_matches(&source.path, prefix)?
    {
        return Ok(TailOutcome::FullParse);
    }
    let Some(stored_digest) = db.file_overlap_digest(source.provider, source_path)? else {
        return Ok(TailOutcome::FullParse);
    };
    // Reuse the persisted bounded inventory; the parser counts only new physical records.
    let stored_session = expected_session_id
        .map(|id| db.resolve_session_record(id))
        .transpose()?;
    let health_seed = stored_session
        .as_ref()
        .and_then(|session| session.raw_metadata_json.as_deref());
    // Carry only an unresolved dequeue across overlap eviction, never a lifetime guard.
    let mut pending_direct_delivery = false;
    if matches!(source.provider, Provider::Claude | Provider::ClaudeDesktop) {
        if let Some(stored) = stored_session.as_ref() {
            let metadata: serde_json::Value = stored
                .raw_metadata_json
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default();
            pending_direct_delivery = metadata["pending_direct_delivery"]
                .as_bool()
                .unwrap_or(false);
        }
    }
    let validated_session = stored_session.as_ref().filter(|session| {
        session.provider == source.provider
            && session.source_path == source_path
            && session.parse_version == crate::util::provider_parse_version(source.provider)
    });
    if source.provider == Provider::Codex
        && validated_session.is_some_and(|session| {
            (session.parent_session_id.is_some() || session.agent_label.is_some())
                && !crate::providers::codex::spawn_header_is_current(&source.path, session, offset)
        })
    {
        return Ok(TailOutcome::FullParse);
    }
    match crate::tail::tail_parse_until(
        &source.path,
        offset,
        Some(&stored_digest),
        should_cancel,
        |r, p, context| {
            parse_slice(
                r,
                p,
                crate::tail::TailContext {
                    pending_direct_delivery,
                    health_seed,
                    validated_session,
                    new_bytes_start: context.new_bytes_start,
                    prefix_truncated: context.prefix_truncated,
                },
            )
        },
    ) {
        Ok(crate::tail::TailParseRun::Complete(Some(mut tail))) => {
            // Some providers declare their immutable session ID only near the file head, outside
            // the bounded tail overlap. Never insert child rows under a fallback ID: re-read the
            // complete source so parent replacement and child publication stay one transaction.
            if expected_session_id != Some(tail.session.id.as_str()) {
                return Ok(TailOutcome::FullParse);
            }
            // If validated provenance could not seed the parser, retain the full-source
            // fallback rather than publishing root attribution for a spawned session.
            if source.provider == Provider::Codex {
                let stored_spawned = db
                    .session_metadata(std::slice::from_ref(&tail.session.id))?
                    .get(&tail.session.id)
                    .is_some_and(|session| {
                        session.parent_session_id.is_some() || session.agent_label.is_some()
                    });
                let tail_spawned =
                    tail.session.parent_session_id.is_some() || tail.session.agent_label.is_some();
                if stored_spawned
                    && (!tail_spawned
                        || stored_session.as_ref().is_some_and(|session| {
                            session.summary.is_none() && tail.session.summary.is_some()
                        }))
                {
                    // The first prompt also establishes immutable summary and creation metadata.
                    return Ok(TailOutcome::FullParse);
                }
            }
            if tail_requires_full_parse(&tail.session) {
                return Ok(TailOutcome::FullParse);
            }
            // Source bytes determine equality; parsed overlap rows can lack earlier context
            // and stored rows can retain archive order after a rewrite.
            if tail.overlap_digest != stored_digest {
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
            if !crate::providers::claude::sidecar_is_current(source)? {
                return Ok(TailOutcome::FullParse);
            }
            tail.new_fingerprint = source_fingerprint(source, &tail.new_fingerprint);
            db.append_tail_with_context(
                &tail,
                source.mtime_ns,
                source.size_bytes,
                validated_session,
            )?;
            Ok(TailOutcome::Appended)
        }
        Ok(crate::tail::TailParseRun::Complete(None)) => Ok(TailOutcome::NothingNew),
        Ok(crate::tail::TailParseRun::ChangedOverlap) => Ok(TailOutcome::FullParse),
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
    fn prefix_dependent_tail_marker_is_provider_neutral_and_typed() {
        for provider in [Provider::Claude, Provider::ClaudeDesktop, Provider::Codex] {
            let mut session = crate::util::minimal_record(
                provider,
                Path::new("/tmp/tail-marker.jsonl"),
                "fixture".into(),
            )
            .session;
            for (metadata, expected) in [
                (None, false),
                (Some("not json"), false),
                (Some(r#"{"requires_full_parse_for_tail":"true"}"#), false),
                (Some(r#"{"requires_full_parse_for_tail":false}"#), false),
                (Some(r#"{"requires_full_parse_for_tail":true}"#), true),
            ] {
                session.raw_metadata_json = metadata.map(str::to_owned);
                assert_eq!(tail_requires_full_parse(&session), expected);
            }
        }
    }

    #[test]
    fn hard_link_sources_remain_one_input_and_unchanged_on_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let first_root = dir.path().join("first");
        let second_root = dir.path().join("second");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        let first = first_root.join("a.jsonl");
        let second = second_root.join("b.jsonl");
        std::fs::write(&first, "{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"sessionId\":\"hard-link\",\"content\":\"one linked submission\"}\n").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        let mut config = config_with_single_claude_fixture(&db_path, dir.path());
        reindex(&config, &db, false, None).unwrap();
        assert_eq!(db.message_count().unwrap(), 1);
        assert_eq!(reindex(&config, &db, false, None).unwrap().1, 0);
        // Selecting another live name for the same file is a verified alias, not a copy.
        config.providers.claude.paths = vec![normalize_path(&second_root)];
        reindex(&config, &db, false, None).unwrap();
        assert!(first.exists());
        assert_eq!(
            db.resolve_session_record("claude:hard-link")
                .unwrap()
                .source_path,
            crate::util::canonical_source_path(&second)
        );
        assert_eq!(db.message_count().unwrap(), 1);
        assert_eq!(reindex(&config, &db, false, None).unwrap().1, 0);
    }

    #[test]
    fn renamed_claude_queue_source_reconciles_path_scoped_identity() {
        for explicit_id in [false, true] {
            for multiplicity in [1, 2] {
                let dir = tempfile::tempdir().unwrap();
                let db_path = dir.path().join("index.db");
                let root = dir.path().join("claude");
                std::fs::create_dir_all(&root).unwrap();
                let old_path = root.join("session.jsonl");
                let mut record = serde_json::json!({"type":"queue-operation","operation":"enqueue","timestamp":"2026-06-17T00:00:00Z","content":"moved queued input"});
                if explicit_id {
                    record["sessionId"] = serde_json::json!("queue-move");
                }
                std::fs::write(&old_path, format!("{record}\n").repeat(multiplicity)).unwrap();
                let config = config_with_single_claude_fixture(&db_path, &root);
                let db = Db::open(&db_path).unwrap();
                reindex(&config, &db, false, None).unwrap();
                let stored = rusqlite::Connection::open(&db_path).unwrap();
                let old_scope: String = stored
                    .query_row(
                        "select correlation_scope from messages where content='moved queued input'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert!(old_scope.contains("session.jsonl"), "{old_scope}");
                let new_path = root.join("renamed.jsonl");
                std::fs::rename(&old_path, &new_path).unwrap();
                for full in [false, false, true, false] {
                    reindex(&config, &db, full, None).unwrap();
                    let counts: (i64, i64, i64) = stored.query_row("select count(*), count(distinct correlation_scope), sum(correlation_scope like '%renamed.jsonl%') from messages where content='moved queued input'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
                    assert_eq!(
                        counts,
                        (multiplicity as i64, 1, multiplicity as i64),
                        "explicit id: {explicit_id}, full: {full}"
                    );
                    let new_scope: String = stored
                        .query_row("select correlation_scope from messages", [], |r| r.get(0))
                        .unwrap();
                    assert_ne!(old_scope, new_scope);
                    let session_count: i64 = stored
                        .query_row("select count(*) from sessions", [], |r| r.get(0))
                        .unwrap();
                    assert_eq!(session_count, 1);
                    let id = if explicit_id {
                        "claude:queue-move"
                    } else {
                        "claude:renamed"
                    };
                    assert_eq!(
                        db.resolve_session_record(id).unwrap().source_path,
                        crate::util::canonical_source_path(&new_path)
                    );
                    assert_eq!(
                        db.search_messages("moved queued input", &Default::default())
                            .unwrap()
                            .len(),
                        multiplicity
                    );
                }
            }
        }
    }

    #[test]
    fn same_stamp_atomic_replacement_is_reparsed_and_preserves_archive() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let line = |id: &str, text: &str| {
            format!(
            "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
        };
        let old = line("old-session", "old content");
        let new = line("new-session", "new content");
        assert_eq!(old.len(), new.len());
        std::fs::write(&path, &old).unwrap();
        let config = config_with_single_claude_fixture(&db_path, &root);
        let db = Db::open(&db_path).unwrap();
        assert_eq!(reindex(&config, &db, false, None).unwrap().1, 1);
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        let old_id = file_id::get_file_id(&path).unwrap();
        let replacement = root.join("replacement.tmp");
        std::fs::write(&replacement, &new).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert_ne!(file_id::get_file_id(&replacement).unwrap(), old_id);
        // Move the old file aside first: portable to Windows without relying on rename-overwrite.
        std::fs::rename(&path, root.join("archive.tmp")).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), stamp);
        assert_eq!(reindex(&config, &db, false, None).unwrap().1, 1);
        assert_eq!(
            db.search_messages("new content", &Default::default())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_messages("old content", &Default::default())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(reindex(&config, &db, false, None).unwrap().1, 0);
    }

    #[test]
    fn source_reconciliation_does_not_infer_missing_or_copied_file_moves() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        let old_path = root.join("session.jsonl");
        let record = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"queue-copy","timestamp":"2026-06-17T00:00:00Z","content":"copied queued input"}"#;
        std::fs::write(&old_path, format!("{record}\n")).unwrap();
        let config = config_with_single_claude_fixture(&db_path, &root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let copy_path = root.join("copy.jsonl");
        std::fs::copy(&old_path, &copy_path).unwrap();
        let adapter = ProviderSet::new(&config);
        let copied = adapter.discover_enabled(&config).sources;
        let reconciliation = source_reconciliation(&db, &copied).unwrap();
        assert!(
            reconciliation
                .get(&(Provider::Claude, normalize_path(&copy_path)))
                .is_none_or(|value| value.aliases.is_empty()),
            "a distinct physical copy is not an alias"
        );
        std::fs::remove_file(&copy_path).unwrap();
        let stored = rusqlite::Connection::open(&db_path).unwrap();
        stored
            .execute(
                "update files_seen set file_identity = null where provider = 'claude'",
                [],
            )
            .unwrap();
        drop(stored);
        let missing_path = root.join("missing-renamed.jsonl");
        std::fs::rename(&old_path, &missing_path).unwrap();
        let adapter = ProviderSet::new(&config);
        let moved = adapter.discover_enabled(&config).sources;
        let reconciliation = source_reconciliation(&db, &moved).unwrap();
        assert!(reconciliation
            .get(&(Provider::Claude, normalize_path(&missing_path)))
            .is_none_or(|value| value.aliases.is_empty()));

        std::fs::copy(&missing_path, &old_path).unwrap();
        let copied = adapter.discover_enabled(&config).sources;
        let reconciliation = source_reconciliation(&db, &copied).unwrap();
        assert!(reconciliation
            .get(&(Provider::Claude, normalize_path(&old_path)))
            .is_none_or(|value| value.aliases.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn renamed_claude_queue_source_through_symlink_root_remaps_canonical_scope() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let real_root = dir.path().join("real");
        let alias_root = dir.path().join("alias");
        std::fs::create_dir_all(&real_root).unwrap();
        std::os::unix::fs::symlink(&real_root, &alias_root).unwrap();
        let old_path = real_root.join("session.jsonl");
        let record = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"queue-symlink-move","timestamp":"2026-06-17T00:00:00Z","content":"symlink moved queued input"}"#;
        std::fs::write(&old_path, format!("{record}\n")).unwrap();
        let config = config_with_single_claude_fixture(&db_path, &alias_root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let new_path = real_root.join("renamed.jsonl");
        std::fs::rename(&old_path, &new_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let stored = rusqlite::Connection::open(&db_path).unwrap();
        let (count, scope): (i64, String) = stored
            .query_row(
                "select count(*), min(correlation_scope) from messages
                 where content = 'symlink moved queued input'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert!(scope.ends_with("real/renamed.jsonl"), "{scope}");
    }

    #[test]
    fn full_parse_checkpoint_never_advances_past_the_discovered_snapshot() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, b"first\n").unwrap();
        let source = SourceFile {
            sidecar: None,
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

    fn claude_overlap_padding(session_id: &str) -> String {
        let mut initial = format!(
            r#"{{"sessionId":"{session_id}","type":"user","message":{{"role":"user","content":"first prompt"}}}}
"#
        );
        let progress = format!(
            r#"{{"sessionId":"{session_id}","type":"assistant","message":{{"role":"assistant","content":"{}"}}}}
"#,
            "x".repeat(1024)
        );
        while initial.len() <= crate::tail::OVERLAP_BYTES as usize + crate::tail::FINGERPRINT_LEN {
            initial.push_str(&progress);
        }
        initial
    }

    #[test]
    fn claude_sidecar_checkpoint_uses_transcript_bytes_and_guards_metadata_changes() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project/parent/subagents");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("agent-demo.jsonl");
        let sidecar = path.with_extension("meta.json");
        let initial = claude_overlap_padding("parent").replace(
            "\"type\":",
            "\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":",
        );
        std::fs::write(&path, initial).unwrap();
        std::fs::write(
            &sidecar,
            r#"{"agentType":"Explore","description":"original title"}"#,
        )
        .unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_single_claude_fixture(&db_path, &root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let discover = || {
            ProviderSet::new(&config)
                .discover_enabled(&config)
                .sources
                .remove(0)
        };
        let source = discover();
        let source_path = normalize_path(&source.path);
        let (offset, _) = db
            .file_checkpoint(
                Provider::Claude,
                &source_path,
                crate::util::provider_parse_version(Provider::Claude),
            )
            .unwrap()
            .unwrap();
        assert_eq!(offset, std::fs::metadata(&path).unwrap().len() as i64);
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(
            b"{\"sessionId\":\"parent\",\"type\":\"assistant\",\"timestamp\":\"2026-01-01T00:01:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"sidecar reply\"}}\n",
        ).unwrap();
        let adapters = ProviderSet::new(&config);
        let source = discover();
        let parsed = adapters.claude.parse(&source);
        assert!(matches!(
            try_tail(
                &source,
                &source_path,
                Some(&parsed.session.id),
                &db,
                &|| false,
                |reader, path, context| adapters.claude.parse_reader_with_context(
                    reader,
                    path,
                    Some(context)
                )
            )
            .unwrap(),
            TailOutcome::Appended
        ));
        let fresh = Db::open(&dir.path().join("fresh.db")).unwrap();
        fresh
            .upsert_session(&parsed, source.mtime_ns, source.size_bytes)
            .unwrap();
        let filters = crate::models::MessageFilters {
            session_id: Some(parsed.session.id.clone()),
            ..Default::default()
        };
        let rows = |db: &Db| {
            serde_json::to_value(
                db.read_session_messages(&filters, crate::db::MessageOrder::OldestFirst)
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(rows(&db), rows(&fresh));
        std::fs::write(
            &sidecar,
            r#"{"agentType":"Explore","description":"changed metadata title"}"#,
        )
        .unwrap();
        let changed = discover();
        assert!(matches!(
            try_tail(
                &changed,
                &source_path,
                Some(&parsed.session.id),
                &db,
                &|| false,
                |reader, path, context| adapters.claude.parse_reader_with_context(
                    reader,
                    path,
                    Some(context)
                )
            )
            .unwrap(),
            TailOutcome::FullParse
        ));
        reindex(&config, &db, false, None).unwrap();
        assert_eq!(
            db.resolve_session_record(&parsed.session.id)
                .unwrap()
                .title
                .as_deref(),
            Some("changed metadata title")
        );
    }

    #[test]
    fn claude_turn_after_attachment_matches_fresh_index() {
        use std::io::Write as _;

        for variant in ["turns", "attachments", "interleaved"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let attachment = |n: usize| {
                serde_json::json!({"type":"attachment","sessionId":"attachment-turn",
                "attachment":{"type":"instructions","files":[{"path":"/synthetic/rules.txt","content":format!("attachment rule {n}")}]}})
            };
            let turn = |n: usize| {
                serde_json::json!({"type":"assistant","uuid":format!("turn-{n}"),
                "sessionId":"attachment-turn","message":{"role":"assistant","content":format!("appended response {n}")}})
            };
            std::fs::write(
                &path,
                format!(
                    "{}{}\n",
                    claude_overlap_padding("attachment-turn"),
                    attachment(0)
                ),
            )
            .unwrap();
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let rows = |path: &Path| {
                let conn = rusqlite::Connection::open(path).unwrap();
                let mut stmt = conn.prepare(
                    "select json_array(role,kind,content,correlation_authority,correlation_scope,correlation_id) from messages order by seq,id"
                ).unwrap();
                stmt.query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
            };
            for refresh in 1..=2 {
                let n = refresh * 2;
                let records = match variant {
                    "turns" => vec![turn(n), turn(n + 1)],
                    "attachments" => vec![attachment(n), attachment(0), attachment(n + 1)],
                    _ => vec![
                        turn(n),
                        attachment(n),
                        turn(n + 1),
                        attachment(0),
                        attachment(n + 1),
                    ],
                };
                let appended = records.iter().map(|r| format!("{r}\n")).collect::<String>();
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(appended.as_bytes())
                    .unwrap();
                reindex(&config, &db, false, None).unwrap();
                let fresh_path = dir.path().join(format!("fresh-{refresh}.db"));
                let fresh = Db::open(&fresh_path).unwrap();
                reindex(
                    &config_with_single_claude_fixture(&fresh_path, &root),
                    &fresh,
                    false,
                    None,
                )
                .unwrap();
                assert_eq!(
                    rows(&db_path),
                    rows(&fresh_path),
                    "{variant}, refresh {refresh}"
                );
                let stored = db.resolve_session_record("claude:attachment-turn").unwrap();
                let metadata: serde_json::Value =
                    serde_json::from_str(stored.raw_metadata_json.as_deref().unwrap()).unwrap();
                assert!(
                    metadata.get("requires_full_parse_for_tail").is_none(),
                    "{variant}, refresh {refresh}: {metadata}"
                );
            }
        }
    }

    #[test]
    fn claude_attachment_between_queue_and_delivery_keeps_one_input() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        let records = [
            serde_json::json!({"type":"attachment","uuid":"queued","sessionId":"queue-attachment",
                "attachment":{"type":"queued_command","prompt":"delivered prompt","commandMode":"prompt"}}),
            serde_json::json!({"type":"attachment","sessionId":"queue-attachment",
                "attachment":{"type":"instructions","content":"synthetic rule"}}),
            serde_json::json!({"type":"user","uuid":"delivered","sessionId":"queue-attachment",
                "message":{"role":"user","content":"delivered prompt"}}),
        ];
        std::fs::write(
            root.join("session.jsonl"),
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        reindex(
            &config_with_single_claude_fixture(&db_path, &root),
            &db,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            db.search_messages("delivered prompt", &Default::default())
                .unwrap()
                .len(),
            1
        );
        assert!(tail_requires_full_parse(
            &db.resolve_session_record("claude:queue-attachment")
                .unwrap()
        ));
    }

    #[test]
    fn ordinary_claude_append_keeps_incremental_tail_fast_path() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        let source = claude_root.join("session.jsonl");
        std::fs::write(
            &source,
            format!("{{\"sessionId\":\"ordinary-tail\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"first prompt\"}}}}\n{}", claude_overlap_padding("ordinary-tail")),
        )
        .unwrap();
        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let initial_count = db.message_count().unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(
                br#"{"sessionId":"ordinary-tail","type":"assistant","message":{"role":"assistant","content":"ordinary reply"}}
"#,
            )
            .unwrap();

        let adapters = ProviderSet::new(&config);
        let source_file = adapters
            .discover_enabled(&config)
            .sources
            .into_iter()
            .next()
            .unwrap();
        let (offset, fingerprint) = db
            .file_checkpoint(
                Provider::Claude,
                &normalize_path(&source_file.path),
                crate::util::provider_parse_version(Provider::Claude),
            )
            .unwrap()
            .expect("initial indexing writes a tail checkpoint");
        assert!(
            crate::tail::fingerprint_matches(&source, &fingerprint).unwrap(),
            "append preserves the source head"
        );
        let parsed_tail = crate::tail::tail_parse(&source, offset, |reader, path| {
            adapters.claude.parse_reader(reader, path)
        })
        .unwrap()
        .unwrap();
        assert_eq!(parsed_tail.session.id, "claude:ordinary-tail");
        let outcome = try_tail(
            &source_file,
            &normalize_path(&source_file.path),
            Some("claude:ordinary-tail"),
            &db,
            &|| false,
            |reader, path, context| {
                adapters
                    .claude
                    .parse_reader_with_context(reader, path, Some(context))
            },
        )
        .unwrap();
        assert!(matches!(outcome, TailOutcome::Appended));
        assert_eq!(db.message_count().unwrap(), initial_count + 1);
    }

    #[test]
    fn ask_user_answer_beyond_overlap_forces_full_parse_and_preserves_attribution() {
        use std::io::Write as _;

        for (answer_index, answer_record) in [
            concat!(
                r#"{"sessionId":"ask-tail","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-tail-1","content":"Your questions have been answered: \"choice\"=\"human answer\"."}]},"toolUseResult":{"answers":{"choice":"human answer"}}}"#,
                "\n"
            ),
            concat!(
                r#"{"sessionId":"ask-tail","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-tail-1","content":"The user responded: human answer"}]},"toolUseResult":{"response":"human answer"}}"#,
                "\n"
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("index.db");
            let claude_root = dir.path().join("claude");
            std::fs::create_dir_all(&claude_root).unwrap();
            let source = claude_root.join("session.jsonl");
            let initial = concat!(
            r#"{"sessionId":"ask-tail","type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask-tail-1","name":"AskUserQuestion","input":{}}]}}"#,
            "\n"
        ).to_owned() + &claude_overlap_padding("ask-tail");
            std::fs::write(&source, initial).unwrap();
            let config = config_with_single_claude_fixture(&db_path, &claude_root);
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            std::fs::OpenOptions::new()
                .append(true)
                .open(&source)
                .unwrap()
                .write_all(answer_record.as_bytes())
                .unwrap();

            let adapters = ProviderSet::new(&config);
            let source_file = adapters
                .discover_enabled(&config)
                .sources
                .into_iter()
                .next()
                .unwrap();
            let outcome = try_tail(
                &source_file,
                &normalize_path(&source_file.path),
                Some("claude:ask-tail"),
                &db,
                &|| false,
                |reader, path, context| adapters.claude.parse_reader_with_context(reader, path, Some(context)),
            )
            .unwrap();
            assert!(matches!(outcome, TailOutcome::FullParse));
            reindex(&config, &db, false, None).unwrap();
            let messages = db
                .search_messages(
                    "human answer",
                    &crate::models::MessageFilters {
                        role: Some(crate::models::Role::User),
                        session_id: Some("claude:ask-tail".to_string()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let answer = messages
                .iter()
                .find(|message| message.content.contains("human answer"))
                .expect("full parse preserved the structured answer");
            assert_eq!(
                answer.provenance.as_ref().unwrap().authorship,
                crate::models::MessageAuthorship::Mixed
            );
            assert_eq!(
                answer.provenance.as_ref().unwrap().input_origin,
                Some(crate::models::MessageInputOrigin::InteractiveAnswer)
            );

            if answer_index == 0 {
                // First move the dependent answer beyond the bounded overlap. The filler append
                // still overlaps the answer, so this reindex must take the conservative full
                // parse and advance the checkpoint past the filler.
                let filler_line = format!(
                    r#"{{"sessionId":"ask-tail","type":"assistant","message":{{"role":"assistant","content":"{}"}}}}
"#,
                    "x".repeat(1024)
                );
                let mut unrelated_append = String::new();
                while unrelated_append.len()
                    <= crate::tail::OVERLAP_BYTES as usize + crate::tail::FINGERPRINT_LEN
                {
                    unrelated_append.push_str(&filler_line);
                }
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&source)
                    .unwrap()
                    .write_all(unrelated_append.as_bytes())
                    .unwrap();
                reindex(&config, &db, false, None).unwrap();

                // With the answer now outside the overlap, a short unrelated append should use
                // the incremental path rather than permanently inheriting the old full parse.
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&source)
                    .unwrap()
                    .write_all(
                        br#"{"sessionId":"ask-tail","type":"assistant","message":{"role":"assistant","content":"post-overlap tail"}}
"#,
                    )
                    .unwrap();
                let source_after_append = adapters
                    .discover_enabled(&config)
                    .sources
                    .into_iter()
                    .next()
                    .unwrap();
                let resumed = try_tail(
                    &source_after_append,
                    &normalize_path(&source_after_append.path),
                    Some("claude:ask-tail"),
                    &db,
                    &|| false,
                    |reader, path, context| adapters.claude.parse_reader_with_context(reader, path, Some(context)),
                )
                .unwrap();
                assert!(
                    matches!(resumed, TailOutcome::Appended),
                    "dependent answer should stop forcing full parses after leaving overlap"
                );

                let tail_count = db.message_count().unwrap();
                let tail_hit = db
                    .search_messages("post-overlap tail", &Default::default())
                    .unwrap();
                assert_eq!(tail_hit.len(), 1);
                reindex(&config, &db, true, None).unwrap();
                assert_eq!(db.message_count().unwrap(), tail_count);
                let full_hit = db
                    .search_messages("post-overlap tail", &Default::default())
                    .unwrap();
                assert_eq!(full_hit.len(), 1);
                assert_eq!(full_hit[0].session_id, tail_hit[0].session_id);
                assert_eq!(full_hit[0].seq, tail_hit[0].seq);
                assert_eq!(full_hit[0].content, tail_hit[0].content);
            }
        }
    }

    #[test]
    fn codex_answer_beyond_overlap_reparses_the_call_prefix_once() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        let id = "019efd97-d602-7922-89dd-467272106505";
        let path = root.join(format!("rollout-2026-06-25T03-04-06-{id}.jsonl"));
        let mut initial =
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n");
        initial.push_str(concat!(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"outside-overlap","arguments":"{\"questions\":[{\"id\":\"q1\",\"question\":\"Choice?\"}]}"}}"#,
            "\n",
        ));
        let padding = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x".repeat(1024)}]}}).to_string() + "\n";
        while initial.len()
            <= crate::tail::OVERLAP_BYTES as usize + 2 * crate::tail::FINGERPRINT_LEN
        {
            initial.push_str(&padding);
        }
        std::fs::write(&path, initial).unwrap();
        let db_path = dir.path().join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.providers.codex.enabled = true;
        config.providers.codex.paths = vec![normalize_path(&root)];
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(concat!(
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"outside-overlap","output":{"answers":{"q1":{"answers":["tail answer α"]}}}}}"#,
            "\n",
        ).as_bytes()).unwrap();
        let adapters = ProviderSet::new(&config);
        let source = adapters.discover_enabled(&config).sources.remove(0);
        let session_id = format!("codex:{id}");
        let outcome = try_tail(
            &source,
            &normalize_path(&source.path),
            Some(&session_id),
            &db,
            &|| false,
            |reader, path, context| {
                adapters
                    .codex
                    .parse_reader_with_context(reader, path, Some(context))
            },
        )
        .unwrap();
        assert!(matches!(outcome, TailOutcome::FullParse));
        reindex(&config, &db, false, None).unwrap();
        let filters = crate::models::MessageFilters {
            role: Some(crate::models::Role::User),
            session_id: Some(session_id),
            ..Default::default()
        };
        assert_eq!(
            db.search_messages("tail answer α", &filters).unwrap().len(),
            1
        );
        assert_eq!(reindex(&config, &db, false, None).unwrap().1, 0);
        assert_eq!(
            db.search_messages("tail answer α", &filters).unwrap().len(),
            1
        );
        writeln!(std::fs::OpenOptions::new().append(true).open(&path).unwrap(), "{}", serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":"synthetic reply after answer"}})).unwrap();
        TEST_TAIL_OUTCOMES.with(|outcomes| outcomes.borrow_mut().clear());
        reindex(&config, &db, false, None).unwrap();
        assert_eq!(
            TEST_TAIL_OUTCOMES.with(|outcomes| std::mem::take(&mut *outcomes.borrow_mut())
                .into_iter()
                .map(|outcome| matches!(outcome, TailOutcome::FullParse))
                .collect::<Vec<_>>()),
            [false]
        );
        assert_eq!(
            db.search_messages("tail answer α", &filters).unwrap().len(),
            1
        );
    }

    #[test]
    fn codex_parent_publication_filters_many_same_path_edits() {
        use std::io::Write as _;
        for matching in [false, true] {
            for full in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let parent_root = dir.path().join("parent");
                let child_root = dir.path().join("child");
                std::fs::create_dir_all(&parent_root).unwrap();
                std::fs::create_dir_all(&child_root).unwrap();
                let parent = "00000000-0000-4000-8000-000000000091";
                let child = "00000000-0000-4000-8000-000000000092";
                let parent_path = parent_root.join(format!("rollout-{parent}.jsonl"));
                let child_path = child_root.join(format!("rollout-{child}.jsonl"));
                let mutation = |n: usize, content: &str| {
                    let path = format!("/synthetic/path-{}.rs", n % 50);
                    format!(
                        "{}\n{}\n",
                        serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":format!("edit-{n}"),"name":"apply_patch","input":format!("*** Begin Patch\n*** Add File: {path}\n+{content}\n*** End Patch")}}),
                        serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":format!("edit-{n}"),"output":format!("Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA {path}\n")}})
                    )
                };
                let mut body = format!(
                    "{}\n",
                    serde_json::json!({"type":"session_meta","payload":{"id":parent}})
                );
                for n in 0..50 {
                    body.push_str(&mutation(n, &format!("publisher initial {n}")));
                }
                std::fs::write(&parent_path, body).unwrap();
                let mut body = format!(
                    "{}\n",
                    serde_json::json!({"type":"session_meta","payload":{"id":child,"forked_from_id":parent}})
                );
                for n in 0..300 {
                    body.push_str(&mutation(n, &format!("child distinct {n}")));
                }
                if matching {
                    body.push_str(&mutation(300, "publisher later"));
                }
                body.push_str(
                    "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n",
                );
                std::fs::write(&child_path, body).unwrap();
                let db_path = dir.path().join("index.db");
                let db = Db::open(&db_path).unwrap();
                let mut config = config_with_single_claude_fixture(&db_path, &child_root);
                config.providers.claude.enabled = false;
                config.providers.codex.enabled = true;
                config.providers.codex.paths =
                    vec![normalize_path(&child_root), normalize_path(&parent_root)];
                reindex(&config, &db, false, None).unwrap();
                assert_eq!(db.file_edit_count().unwrap(), 350 + i64::from(matching));
                config.providers.codex.paths = vec![normalize_path(&parent_root)];
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&parent_path)
                    .unwrap()
                    .write_all(mutation(300, "publisher later").as_bytes())
                    .unwrap();
                crate::db::CODEX_FORK_AFTER_READ.with(|hook| hook.set(Some(|_| {})));
                if full {
                    reindex(&config, &db, true, None).unwrap();
                } else {
                    reindex_tail_outcome(&config, &db, TailOutcome::Appended);
                }
                assert_eq!(
                    crate::db::CODEX_FORK_AFTER_READ
                        .with(|hook| hook.take())
                        .is_none(),
                    matching
                );
                assert_eq!(db.file_edit_count().unwrap(), 351);
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                let child_edits: i64 = conn
                    .query_row(
                        "select count(*) from file_edits where session_id=?1",
                        [format!("codex:{child}")],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(child_edits, 300);
            }
        }
    }

    #[test]
    fn codex_parent_publication_skips_forks_without_matching_stored_edits() {
        codex_parent_publication_filters_forks(false);
    }

    #[test]
    fn codex_parent_tail_reconciles_matching_stored_fork_edit() {
        codex_parent_publication_filters_forks(true);
    }

    fn codex_parent_publication_filters_forks(matching: bool) {
        use std::io::Write as _;
        for full in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let parent_root = dir.path().join("parent");
            let child_root = dir.path().join("child");
            std::fs::create_dir_all(&parent_root).unwrap();
            std::fs::create_dir_all(&child_root).unwrap();
            let parent = "00000000-0000-4000-8000-000000000081";
            let child = "00000000-0000-4000-8000-000000000082";
            let parent_path = parent_root.join(format!("rollout-{parent}.jsonl"));
            let child_path = child_root.join(format!("rollout-{child}.jsonl"));
            let mutation = |path: &str, content: &str| {
                format!(
                    "{}\n{}\n",
                    serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":path,"name":"apply_patch","input":format!("*** Begin Patch\n*** Add File: /synthetic/{path}.rs\n+{content}\n*** End Patch")}}),
                    serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":path,"output":format!("Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA /synthetic/{path}.rs\n")}})
                )
            };
            let header = serde_json::json!({"type":"session_meta","payload":{"id":parent}});
            std::fs::write(
                &parent_path,
                format!("{header}\n{}", mutation("initial", "parent")),
            )
            .unwrap();
            let header = serde_json::json!({"type":"session_meta","payload":{"id":child,"forked_from_id":parent}});
            let settings =
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n";
            // An equal older edit must not make an unrelated tail append reparse this fork.
            let own_edit = if !matching && !full {
                mutation("initial", "parent")
            } else {
                mutation("child-only", "child")
            };
            std::fs::write(
                &child_path,
                format!(
                    "{header}\n{}{settings}{}",
                    mutation(
                        "later",
                        if matching {
                            "parent"
                        } else {
                            "different content"
                        }
                    ),
                    own_edit
                ),
            )
            .unwrap();
            let db_path = dir.path().join("index.db");
            let db = Db::open(&db_path).unwrap();
            let mut config = config_with_single_claude_fixture(&db_path, &child_root);
            config.providers.claude.enabled = false;
            config.providers.codex.enabled = true;
            config.providers.codex.paths =
                vec![normalize_path(&child_root), normalize_path(&parent_root)];
            reindex(&config, &db, false, None).unwrap();
            assert_eq!(db.file_edit_count().unwrap(), 3);
            config.providers.codex.paths = vec![normalize_path(&parent_root)];
            std::fs::OpenOptions::new()
                .append(true)
                .open(&parent_path)
                .unwrap()
                .write_all(mutation("later", "parent").as_bytes())
                .unwrap();
            crate::db::CODEX_FORK_AFTER_READ.with(|hook| {
                hook.set(Some(if matching {
                    |_| {}
                } else {
                    |_| panic!("parent publication parsed a fork without a matching stored edit")
                }))
            });
            crate::db::FORK_SOURCE_READS.with(|count| count.set(0));
            crate::db::FORK_FALLBACKS.with(|count| count.set(0));
            crate::db::FORK_LOCKED_READS.with(|count| count.set(0));
            crate::db::FORK_BEFORE_SOURCE_READ.with(|hook| {
                // The database sits two directories above the fork source in this layout.
                hook.set(Some(|path| {
                    crate::db::probe_fork_writer_lock(
                        &path.parent().unwrap().parent().unwrap().join("index.db"),
                    )
                }))
            });
            if full {
                reindex(&config, &db, true, None).unwrap();
            } else {
                reindex_tail_outcome(&config, &db, TailOutcome::Appended);
            }
            crate::db::FORK_BEFORE_SOURCE_READ.with(|hook| hook.set(None));
            // A matching fork is parsed once, before the write transaction; a publication with
            // no matching fork never reads one. Neither falls back to a read under the lock.
            assert_eq!(
                (
                    crate::db::FORK_SOURCE_READS.with(|count| count.get()),
                    crate::db::FORK_FALLBACKS.with(|count| count.get()),
                    crate::db::FORK_LOCKED_READS.with(|count| count.get()),
                ),
                (usize::from(matching), 0, 0),
                "matching {matching}, full {full}"
            );
            assert_eq!(
                crate::db::CODEX_FORK_AFTER_READ
                    .with(|hook| hook.take())
                    .is_none(),
                matching
            );
            assert_eq!(db.file_edit_count().unwrap(), if matching { 3 } else { 4 });
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let plan = conn
                .prepare(&format!(
                    "explain query plan {}",
                    crate::db::CODEX_FORK_CHILD_EDITS_SQL
                ))
                .unwrap()
                .query_map([format!("codex:{child}")], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|line| line
                        .contains("SEARCH file_edits USING INDEX idx_file_edits_session")),
                "child probe must read its own edits: {plan:?}"
            );
            assert_eq!(
                plan.iter()
                    .filter(|line| line.contains("file_edits"))
                    .count(),
                1,
                "a child probe must not join edit pairs: {plan:?}"
            );
            assert!(
                !plan.iter().any(|line| line.contains("idx_file_edits_path")),
                "{plan:?}"
            );
            let history = crate::files::history(&db, "later.rs", &Default::default()).unwrap();
            assert_eq!(history.len(), if matching { 1 } else { 2 });
            assert!(history
                .iter()
                .any(|edit| edit.session_id == format!("codex:{parent}")));
        }
    }

    #[test]
    fn codex_fork_edits_converge_across_root_order_and_parent_repair() {
        use std::io::Write as _;
        for order in [[0, 1, 2], [1, 0, 2], [2, 1, 0]] {
            for initial_parent in [
                "ready",
                "missing",
                "malformed",
                "metadata",
                "temporarily-unavailable",
            ] {
                let dir = tempfile::tempdir().unwrap();
                let parent_root = dir.path().join("parent");
                let fork_root = dir.path().join("fork");
                let descendant_root = dir.path().join("descendant");
                for root in [&parent_root, &fork_root, &descendant_root] {
                    std::fs::create_dir_all(root).unwrap();
                }
                let parent = "00000000-0000-4000-8000-000000000001";
                let fork = "00000000-0000-4000-8000-000000000002";
                let parent_path = parent_root.join(format!("rollout-{parent}.jsonl"));
                let fork_path = fork_root.join(format!("rollout-{fork}.jsonl"));
                let mutation = |call: &str, file: &str| {
                    let input = format!("*** Begin Patch\n*** Add File: /synthetic/{file}.rs\n+synthetic {file} content\n*** End Patch");
                    let output = format!("Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA /synthetic/{file}.rs\n");
                    format!(
                        "{}\n{}\n",
                        serde_json::json!({"type":"response_item","timestamp":"2026-11-01T00:00:01Z","payload":{"type":"custom_tool_call","call_id":call,"name":"apply_patch","input":input}}),
                        serde_json::json!({"type":"response_item","timestamp":"2026-11-01T00:00:01Z","payload":{"type":"custom_tool_call_output","call_id":call,"output":output}})
                    )
                };
                let header = serde_json::json!({"type":"session_meta","payload":{"id":parent,"timestamp":"2026-11-01T00:00:00Z"}});
                let parent_body = format!(
                    "{header}\n{}{}",
                    mutation("shared", "shared"),
                    mutation("parent-only", "parent-only")
                );
                let fork_header = serde_json::json!({"type":"session_meta","payload":{"id":fork,"forked_from_id":parent,"timestamp":"2026-11-01T00:00:00Z"}});
                let settings = serde_json::json!({"type":"event_msg","payload":{"type":"thread_settings_applied"}});
                std::fs::write(
                    &fork_path,
                    format!(
                        "{fork_header}\n{}{}{settings}\n{}",
                        mutation("shared", "shared"),
                        mutation("recovered", "fork-recovered"),
                        mutation("own", "fork-own")
                    ),
                )
                .unwrap();
                let expected = {
                    let descendant = "00000000-0000-4000-8000-000000000003";
                    let header = serde_json::json!({"type":"session_meta","payload":{"id":descendant,"forked_from_id":fork,"timestamp":"2026-11-01T00:00:00Z"}});
                    std::fs::write(
                        descendant_root.join(format!("rollout-{descendant}.jsonl")),
                        format!(
                            "{header}\n{}{settings}\n{}",
                            mutation("shared-copy", "shared"),
                            mutation("descendant-own", "descendant-own")
                        ),
                    )
                    .unwrap();
                    5
                };
                match initial_parent {
                    "ready" => std::fs::write(&parent_path, &parent_body).unwrap(),
                    "malformed" => std::fs::write(&parent_path, "{malformed\n").unwrap(),
                    "metadata" => std::fs::write(&parent_path, format!("{header}\n")).unwrap(),
                    _ => {}
                }
                let db_path = dir.path().join("index.db");
                let mut config = config_with_single_claude_fixture(&db_path, &fork_root);
                config.providers.claude.enabled = false;
                config.providers.codex.enabled = true;
                let roots = [&parent_root, &fork_root, &descendant_root];
                config.providers.codex.paths.clear();
                let db = Db::open(&db_path).unwrap();
                // Publish each root separately to prove the discovery order actually differs.
                for index in order {
                    config
                        .providers
                        .codex
                        .paths
                        .push(normalize_path(roots[index]));
                    reindex(&config, &db, false, None).unwrap();
                }
                if initial_parent != "ready" {
                    assert_eq!(db.file_edit_count().unwrap(), 4);
                    if initial_parent == "temporarily-unavailable" {
                        std::fs::rename(&fork_path, fork_path.with_extension("saved")).unwrap();
                    }
                    std::fs::write(&parent_path, &parent_body).unwrap();
                }
                // The fork's bytes, size and mtime stay unchanged while its parent appears.
                reindex(&config, &db, false, None).unwrap();
                if initial_parent == "temporarily-unavailable" {
                    // Restoring the same inode also restores the identical source stamp.
                    std::fs::rename(fork_path.with_extension("saved"), &fork_path).unwrap();
                    reindex(&config, &db, false, None).unwrap();
                }
                let history = crate::files::history(&db, "shared.rs", &Default::default()).unwrap();
                assert_eq!(history.len(), 1, "shared edit must have one indexed owner");
                assert_eq!(history[0].session_id, format!("codex:{parent}"));
                assert_eq!(
                    db.file_edit_count().unwrap(),
                    expected,
                    "order={order:?}, initial_parent={initial_parent}"
                );
                let rows = |path: &Path| {
                    let conn = rusqlite::Connection::open(path).unwrap();
                    let mut stmt = conn.prepare("select json_array(session_id,provider,seq,ts,tool,file_path,file_name,new_content,edits_json) from file_edits order by session_id,seq,id").unwrap();
                    stmt.query_map([], |r| r.get::<_, String>(0))
                        .unwrap()
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .unwrap()
                };
                let fresh_path = dir.path().join("fresh.db");
                let fresh = Db::open(&fresh_path).unwrap();
                reindex(&config, &fresh, false, None).unwrap();
                assert_eq!(rows(&db_path), rows(&fresh_path));
                let versions = crate::files::reconstruct_versions_query(
                    &db,
                    "fork-recovered.rs",
                    &Default::default(),
                )
                .unwrap()
                .collect::<Vec<_>>();
                assert_eq!(versions.len(), 1);
                assert_eq!(versions[0].content, "synthetic fork-recovered content\n");
                reindex(&config, &db, true, None).unwrap();
                assert_eq!(rows(&db_path), rows(&fresh_path));
                // Publishing an appended parent edit also reconciles an unchanged fork.
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&parent_path)
                    .unwrap()
                    .write_all(mutation("recovered-later", "fork-recovered").as_bytes())
                    .unwrap();
                let adapters = ProviderSet::new(&config);
                let source = adapters
                    .discover_enabled(&config)
                    .sources
                    .into_iter()
                    .find(|source| {
                        normalize_path(&source.path)
                            == crate::util::canonical_source_path(&parent_path)
                    })
                    .unwrap();
                assert!(matches!(
                    try_tail(
                        &source,
                        &normalize_path(&source.path),
                        Some(&format!("codex:{parent}")),
                        &db,
                        &|| false,
                        |r, p, context| adapters.codex.parse_reader_with_context(
                            r,
                            p,
                            Some(context)
                        )
                    )
                    .unwrap(),
                    TailOutcome::Appended
                ));
                let appended_path = dir.path().join("appended.db");
                let appended = Db::open(&appended_path).unwrap();
                reindex(&config, &appended, false, None).unwrap();
                assert_eq!(rows(&db_path), rows(&appended_path));
                assert_eq!(db.file_edit_count().unwrap(), expected);
                assert_eq!(reindex(&config, &db, false, None).unwrap().1, 0);
            }
        }
    }

    #[test]
    fn codex_fork_ancestry_counts_edits_and_repairs_empty_links() {
        for bridge in [false, true] {
            for initial_middle in ["ready", "missing", "metadata", "malformed", "cycle"] {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().join("codex");
                std::fs::create_dir_all(&root).unwrap();
                let ids = [
                    "00000000-0000-4000-8000-000000000021",
                    "00000000-0000-4000-8000-000000000022",
                    "00000000-0000-4000-8000-000000000023",
                ];
                let paths = ids.map(|id| root.join(format!("rollout-{id}.jsonl")));
                let body = |index: usize, parent: Option<&str>, replay: usize, own: usize| {
                    let header = serde_json::json!({"type":"session_meta","payload":{"id":ids[index],"forked_from_id":parent}});
                    let mut body = format!("{header}\n");
                    for n in 0..replay + own {
                        if n == replay {
                            body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n");
                        }
                        let call = format!("edit-{n}");
                        let input = "*** Begin Patch\n*** Add File: /synthetic/shared.rs\n+synthetic content\n*** End Patch";
                        body.push_str(&format!("{}\n{}\n",
                            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":call,"name":"apply_patch","input":input}}),
                            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":call,"output":"Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA /synthetic/shared.rs\n"}})));
                    }
                    if own == 0 {
                        body.push_str("{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"}}\n");
                    }
                    body
                };
                std::fs::write(&paths[0], body(0, None, 0, 2)).unwrap();
                let middle = body(1, Some(ids[0]), if bridge { 0 } else { 3 }, 0);
                match initial_middle {
                    "ready" => std::fs::write(&paths[1], &middle).unwrap(),
                    "metadata" => std::fs::write(&paths[1], body(1, None, 0, 0)).unwrap(),
                    "malformed" => std::fs::write(&paths[1], "{malformed\n").unwrap(),
                    "cycle" => std::fs::write(&paths[1], body(1, Some(ids[2]), 0, 0)).unwrap(),
                    _ => {}
                }
                // The extra equal replay must survive; a post-fork equal edit is also real.
                let replay = if bridge { 3 } else { 4 };
                std::fs::write(&paths[2], body(2, Some(ids[1]), replay, 1)).unwrap();
                let db_path = dir.path().join("index.db");
                let mut config = config_with_single_claude_fixture(&db_path, &root);
                config.providers.claude.enabled = false;
                config.providers.codex.enabled = true;
                config.providers.codex.paths = vec![normalize_path(&root)];
                let db = Db::open(&db_path).unwrap();
                reindex(&config, &db, false, None).unwrap();
                let history = |db: &Db| {
                    crate::files::history(db, "shared.rs", &Default::default())
                        .unwrap()
                        .into_iter()
                        .map(|v| (v.session_id, v.version))
                        .collect::<Vec<_>>()
                };
                if initial_middle != "ready" {
                    // Without an indexed link, the root's equal edits are not ancestry evidence.
                    assert_eq!(history(&db).len(), 2 + replay + 1);
                    std::fs::write(&paths[1], &middle).unwrap();
                    reindex(&config, &db, false, None).unwrap();
                }
                let versions = history(&db);
                assert_eq!(
                    versions.len(),
                    replay + 1,
                    "bridge={bridge}, initial_middle={initial_middle}"
                );
                for (id, count) in [(ids[0], 2), (ids[1], usize::from(!bridge)), (ids[2], 2)] {
                    assert_eq!(
                        versions
                            .iter()
                            .filter(|(s, _)| s == &format!("codex:{id}"))
                            .count(),
                        count
                    );
                }
                reindex(&config, &db, true, None).unwrap();
                assert_eq!(history(&db), versions);
                let fresh = Db::open(&dir.path().join("fresh.db")).unwrap();
                reindex(&config, &fresh, false, None).unwrap();
                assert_eq!(history(&fresh), versions);
                assert_eq!(reindex(&config, &db, false, None).unwrap().1, 0);
            }
        }
    }

    #[test]
    fn codex_parent_publication_commits_when_fork_reparse_fails_and_retries() {
        use std::io::Write as _;

        for full_parse in [false, true] {
            for failure in ["missing", "unreadable", "malformed", "changed-during-read"] {
                #[cfg(not(unix))]
                if failure == "unreadable" {
                    continue;
                }
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().canonicalize().unwrap();
                let parent_root = root.join("parent");
                let fork_root = root.join("fork");
                std::fs::create_dir_all(&parent_root).unwrap();
                std::fs::create_dir_all(&fork_root).unwrap();
                let parent = "00000000-0000-4000-8000-000000000011";
                let fork = "00000000-0000-4000-8000-000000000012";
                let parent_path = parent_root.join(format!("rollout-{parent}.jsonl"));
                let fork_path = fork_root.join(format!("rollout-{fork}.jsonl"));
                let mutation = |call: &str, file: &str| {
                    let input = format!(
                "*** Begin Patch\n*** Add File: /synthetic/{file}.rs\n+synthetic {file} content\n*** End Patch"
            );
                    let output = format!(
                "Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA /synthetic/{file}.rs\n"
            );
                    format!(
                        "{}\n{}\n",
                        serde_json::json!({"type":"response_item","timestamp":"2026-11-01T00:00:01Z","payload":{"type":"custom_tool_call","call_id":call,"name":"apply_patch","input":input}}),
                        serde_json::json!({"type":"response_item","timestamp":"2026-11-01T00:00:01Z","payload":{"type":"custom_tool_call_output","call_id":call,"output":output}})
                    )
                };
                let parent_header = serde_json::json!({
                    "type": "session_meta",
                    "payload": {"id": parent, "timestamp": "2026-11-01T00:00:00Z"}
                });
                let fork_header = serde_json::json!({
                    "type": "session_meta",
                    "payload": {"id": fork, "forked_from_id": parent, "timestamp": "2026-11-01T00:00:00Z"}
                });
                let settings = serde_json::json!({
                    "type": "event_msg",
                    "payload": {"type": "thread_settings_applied"}
                });
                std::fs::write(
                    &parent_path,
                    format!(
                        "{parent_header}\n{}{}",
                        mutation("shared", "shared"),
                        mutation("parent-only", "parent-only")
                    ),
                )
                .unwrap();
                std::fs::write(
                    &fork_path,
                    format!(
                        "{fork_header}\n{}{}{settings}\n{}",
                        mutation("shared", "shared"),
                        mutation("fork-only", "fork-only"),
                        mutation("fork-own", "fork-own")
                    ),
                )
                .unwrap();

                let db_path = dir.path().join("index.db");
                let mut config = config_with_single_claude_fixture(&db_path, &fork_root);
                config.providers.claude.enabled = false;
                config.providers.codex.enabled = true;
                config.providers.codex.paths =
                    vec![normalize_path(&parent_root), normalize_path(&fork_root)];
                let db = Db::open(&db_path).unwrap();
                reindex(&config, &db, false, None).unwrap();
                assert_eq!(db.file_edit_count().unwrap(), 4);

                // The parent publication must commit even when the dependent fork disappears while the
                // writer is reconciling it. Its receipt is invalidated so a later refresh retries it.
                let saved = fork_path.with_extension("saved");
                std::fs::copy(&fork_path, &saved).unwrap();
                match failure {
                    "missing" => {
                        std::fs::rename(&fork_path, fork_path.with_extension("unavailable"))
                            .unwrap()
                    }
                    "unreadable" => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(
                                &fork_path,
                                std::fs::Permissions::from_mode(0o000),
                            )
                            .unwrap();
                        }
                    }
                    "malformed" => {
                        std::fs::write(&fork_path, "{malformed\n").unwrap();
                        // Match the receipt so this exercises parsing, rather than the pre-read check.
                        let metadata = std::fs::metadata(&fork_path).unwrap();
                        let conn = rusqlite::Connection::open(&db_path).unwrap();
                        conn.execute(
                            "update files_seen set mtime_ns=?1, size_bytes=?2 where source_path=?3",
                            rusqlite::params![
                                metadata
                                    .modified()
                                    .unwrap()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_nanos() as i64,
                                metadata.len() as i64,
                                normalize_path(&fork_path)
                            ],
                        )
                        .unwrap();
                    }
                    "changed-during-read" => crate::db::CODEX_FORK_AFTER_READ.with(|hook| {
                        hook.set(Some(|path| {
                            std::fs::OpenOptions::new()
                                .append(true)
                                .open(path)
                                .unwrap()
                                .write_all(b"\n")
                                .unwrap();
                        }))
                    }),
                    _ => unreachable!(),
                }
                // The matching recovery edit keeps the fork eligible for the failed read.
                // Publish only the parent; retrying the fork is a separate refresh.
                config.providers.codex.paths = vec![normalize_path(&parent_root)];
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&parent_path)
                    .unwrap()
                    .write_all(
                        format!(
                            "{}{}",
                            mutation("parent-later", "parent-later"),
                            mutation("recovered-later", "fork-only")
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                reindex(&config, &db, full_parse, None).unwrap();
                assert!(crate::db::CODEX_FORK_AFTER_READ
                    .with(|hook| hook.take())
                    .is_none());
                let stored = rusqlite::Connection::open(&db_path).unwrap();
                assert_eq!(
            stored
                .query_row(
                    "select count(*) from file_edits where session_id = ?1 and file_name = 'parent-later.rs'",
                    rusqlite::params![format!("codex:{parent}")],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1,
            "parent edits commit despite a failed dependent fork read"
        );
                let invalidated: (String, Option<i64>, Option<String>) = stored.query_row(
            "select parse_version,tail_byte_offset,prefix_fingerprint from files_seen where source_path=?1",
            [normalize_path(&fork_path)], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
                assert_eq!(
                    invalidated,
                    (String::new(), None, None),
                    "{failure}, full={full_parse}"
                );
                #[cfg(unix)]
                if failure == "unreadable" {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&fork_path, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
                if failure != "unreadable" {
                    std::fs::copy(&saved, &fork_path).unwrap();
                }
                config
                    .providers
                    .codex
                    .paths
                    .push(normalize_path(&fork_root));
                reindex(&config, &db, false, None).unwrap();
                assert_eq!(db.file_edit_count().unwrap(), 5);
                let receipt: (String, Option<i64>, Option<String>) = stored.query_row(
            "select parse_version,tail_byte_offset,prefix_fingerprint from files_seen where source_path=?1",
            [normalize_path(&fork_path)], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
                assert!(!receipt.0.is_empty(), "retry succeeded for {failure}");
                assert_eq!(
                    stored
                        .query_row(
                            "select count(*) from file_edits where file_name = 'parent-later.rs'",
                            [],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap(),
                    1,
                    "the repaired fork keeps the parent publication"
                );
            }
        }
    }

    #[test]
    fn codex_fork_keeps_recoverable_edits_when_parent_lacks_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        let parent = "019f4a0e-d714-7cc3-8003-543a04a1a822";
        let own = "019f5ce8-afff-76a3-86f2-174307f3fa08";
        let path = root.join(format!("rollout-{own}.jsonl"));
        let parent_path = root.join(format!("rollout-{parent}.jsonl"));
        let records = [
            serde_json::json!({"type":"session_meta","payload":{"id":own,"forked_from_id":parent}}),
            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":"copied-edit","name":"apply_patch","input":"*** Begin Patch\n*** Add File: /synthetic/retained.rs\n+recoverable content\n*** End Patch"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"copied-edit","output":"Exit code: 0\nOutput:\nSuccess. Updated the following files:\nA /synthetic/retained.rs\n"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"thread_settings_applied"}}),
        ];
        std::fs::write(
            &path,
            records.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        let db_path = dir.path().join("index.db");
        let mut config = config_with_single_claude_fixture(&db_path, &root);
        config.providers.claude.enabled = false;
        config.providers.codex.enabled = true;
        config.providers.codex.paths = vec![normalize_path(&root)];
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        assert_eq!(db.file_edit_count().unwrap(), 1);
        // A parent filename without the copied mutation is not proof of a recoverable copy.
        for body in [
            serde_json::json!({"type":"session_meta","payload":{"id":parent}}).to_string(),
            "{malformed".into(),
        ] {
            std::fs::write(&parent_path, format!("{body}\n")).unwrap();
            reindex(&config, &db, true, None).unwrap();
            assert_eq!(db.file_edit_count().unwrap(), 1);
            let versions =
                crate::files::reconstruct_versions_query(&db, "retained.rs", &Default::default())
                    .unwrap()
                    .collect::<Vec<_>>();
            assert_eq!(versions.len(), 1);
            assert_eq!(versions[0].content, "recoverable content\n");
            let fresh_path = dir.path().join(format!("fresh-{}.db", body.len()));
            let fresh = Db::open(&fresh_path).unwrap();
            reindex(&config, &fresh, true, None).unwrap();
            assert_eq!(fresh.file_edit_count().unwrap(), 1);
        }
    }

    #[test]
    fn codex_fork_settings_reconcile_previously_indexed_replay() {
        use std::io::Write as _;
        for beyond_overlap in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("codex");
            std::fs::create_dir_all(&root).unwrap();
            let own = "019f5ce8-afff-76a3-86f2-174307f3fa09";
            let path = root.join(format!("rollout-{own}.jsonl"));
            let mut initial = format!(
                "{}\n{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":own,"forked_from_id":"parent"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"copied parent prompt"}]}})
            );
            if beyond_overlap {
                initial.push_str(&format!("{}\n", serde_json::json!({"type":"padding","text":"x".repeat(crate::tail::OVERLAP_BYTES as usize + crate::tail::FINGERPRINT_LEN)})));
            }
            std::fs::write(&path, initial).unwrap();
            let db_path = dir.path().join("index.db");
            let mut config = config_with_single_claude_fixture(&db_path, &root);
            config.providers.claude.enabled = false;
            config.providers.codex.enabled = true;
            config.providers.codex.paths = vec![normalize_path(&root)];
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let filters = crate::models::MessageFilters {
                role: Some(crate::models::Role::User),
                ..Default::default()
            };
            assert_eq!(
                db.search_messages("copied parent prompt", &filters)
                    .unwrap()
                    .len(),
                1
            );
            let appended = format!(
                "{}\n{}\n",
                serde_json::json!({"type":"event_msg","payload":{"type":"thread_settings_applied"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"new fork prompt"}]}})
            );
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(appended.as_bytes())
                .unwrap();
            reindex(&config, &db, false, None).unwrap();
            assert!(
                db.search_messages("copied parent prompt", &filters)
                    .unwrap()
                    .is_empty(),
                "header beyond overlap: {beyond_overlap}"
            );
            assert_eq!(
                db.search_messages("new fork prompt", &filters)
                    .unwrap()
                    .len(),
                1
            );
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let relation: String = conn
                .query_row(
                    "select record_relation from messages where content='copied parent prompt'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(relation, "mirror");
            let transcript: String = conn
                .query_row("select transcript_text from transcripts", [], |r| r.get(0))
                .unwrap();
            assert!(!transcript.contains("copied parent prompt"));
            let fresh_path = dir.path().join("fresh.db");
            let fresh = Db::open(&fresh_path).unwrap();
            reindex(&config, &fresh, true, None).unwrap();
            let fresh_conn = rusqlite::Connection::open(&fresh_path).unwrap();
            let fresh_transcript: String = fresh_conn
                .query_row("select transcript_text from transcripts", [], |r| r.get(0))
                .unwrap();
            assert_eq!(transcript, fresh_transcript);
            reindex(&config, &db, true, None).unwrap();
            assert_eq!(db.message_count().unwrap(), fresh.message_count().unwrap());
        }
    }

    #[test]
    fn codex_spawn_header_rewrite_beyond_fingerprint_matches_full_index() {
        use std::io::{Seek as _, Write as _};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        let id = "019efd97-d602-7922-89dd-467272106505";
        let path = root.join(format!("rollout-2026-06-25T03-04-06-{id}.jsonl"));
        // Explicit ordering places the changed provenance beyond the head fingerprint.
        let header = format!(
            r#"{{"type":"session_meta","padding":"{}","payload":{{"id":"{id}","timestamp":"2026-06-25T03:04:06Z","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"root-session","agent_nickname":"worker"}}}}}}}}}}"#,
            "x".repeat(8192)
        );
        let user = |text: &str| {
            format!(
                "{}\n",
                serde_json::json!({"type":"response_item","timestamp":"2026-06-25T03:04:07Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
            )
        };
        let mut initial = format!("{header}\n{}", user("initial prompt"));
        while initial.len() < header.len() + crate::tail::OVERLAP_BYTES as usize + 8192 {
            initial.push_str(&format!("{}\n", serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x".repeat(4096)}]}})));
        }
        std::fs::write(&path, &initial).unwrap();
        let db_path = dir.path().join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.providers.codex.enabled = true;
        config.providers.codex.paths = vec![normalize_path(&root)];
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let identity = file_id::get_file_id(&path).unwrap();
        let worker_offset = header.find("worker").unwrap();
        assert!(worker_offset > crate::tail::FINGERPRINT_LEN);
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(std::io::SeekFrom::Start(worker_offset as u64))
            .unwrap();
        file.write_all(b"helper").unwrap();
        assert_eq!(file.metadata().unwrap().len(), initial.len() as u64);
        drop(file);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(user("appended prompt").as_bytes())
            .unwrap();
        assert_eq!(file_id::get_file_id(&path).unwrap(), identity);
        assert_eq!(
            &std::fs::read(&path).unwrap()[..crate::tail::FINGERPRINT_LEN],
            &initial.as_bytes()[..crate::tail::FINGERPRINT_LEN]
        );
        TEST_TAIL_OUTCOMES.with(|counts| counts.borrow_mut().clear());
        crate::providers::codex::TEST_SPAWN_HEADER_VALIDATION_BYTES
            .with(|counts| counts.borrow_mut().clear());
        reindex(&config, &db, false, None).unwrap();
        crate::providers::codex::TEST_SPAWN_HEADER_VALIDATION_BYTES
            .with(|counts| assert_eq!(*counts.borrow(), vec![header.len() as u64]));
        let fresh = Db::open(&dir.path().join("fresh.db")).unwrap();
        reindex(&config, &fresh, true, None).unwrap();
        let session_id = format!("codex:{id}");
        let actual = db.resolve_session_record(&session_id).unwrap();
        let expected = fresh.resolve_session_record(&session_id).unwrap();
        assert_eq!(expected.agent_label.as_deref(), Some("helper"));
        assert_eq!(
            actual.agent_label, expected.agent_label,
            "spawn attribution differs from full index"
        );
        assert_eq!(actual.parent_session_id, expected.parent_session_id);
        TEST_TAIL_OUTCOMES
            .with(|counts| assert_eq!(*counts.borrow(), vec![TailOutcome::FullParse]));
        let attribution = |path: &Path| {
            rusqlite::Connection::open(path).unwrap()
                .prepare("select json_array(seq,role,content,authorship,input_origin,record_relation) from messages order by seq,id").unwrap()
                .query_map([], |row| row.get::<_, String>(0)).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(
            attribution(&db_path),
            attribution(&dir.path().join("fresh.db"))
        );

        // A pre-checkpoint row retains all existing fields and its parser version.
        rusqlite::Connection::open(&db_path).unwrap().execute(
            "update sessions set raw_metadata_json = json_remove(raw_metadata_json, '$.codex_spawn_header')", []
        ).unwrap();
        TEST_TAIL_OUTCOMES.with(|counts| counts.borrow_mut().clear());
        for turn in 0..2 {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(user(&format!("upgrade prompt {turn}")).as_bytes())
                .unwrap();
            reindex(&config, &db, false, None).unwrap();
            let stored = db.resolve_session_record(&session_id).unwrap();
            let metadata: serde_json::Value =
                serde_json::from_str(stored.raw_metadata_json.as_deref().unwrap()).unwrap();
            assert_eq!(
                metadata[crate::providers::codex::SPAWN_HEADER_KEY]["length"],
                header.len()
            );
            assert_eq!(stored.agent_label.as_deref(), Some("helper"));
        }
        TEST_TAIL_OUTCOMES.with(|counts| {
            assert_eq!(
                *counts.borrow(),
                vec![TailOutcome::FullParse, TailOutcome::Appended]
            )
        });
    }

    #[test]
    fn codex_spawn_metadata_beyond_overlap_reuses_validated_context() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        std::fs::create_dir_all(&root).unwrap();
        let id = "019efd97-d602-7922-89dd-467272106505";
        let path = root.join(format!("rollout-2026-06-25T03-04-06-{id}.jsonl"));
        let spawn_source = serde_json::json!({
            "subagent": {
                "thread_spawn": {
                    "parent_thread_id": "root-session",
                    "agent_nickname": "worker"
                }
            }
        });
        let mut initial = serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-06-25T03:04:06Z",
            "payload": {"id": id, "source": spawn_source, "timestamp": "2026-06-25T03:04:06Z"}
        })
        .to_string()
            + "\n";
        let header_length = initial.len() as u64 - 1;
        initial.push_str(concat!(
            r#"{"type":"response_item","timestamp":"2026-06-25T03:04:06Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"spawned tail prompt"}]}}"#,
            "\n"
        ));
        let padding = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-06-25T03:04:06Z",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "x".repeat(1024)}]
            }
        })
        .to_string()
            + "\n";
        while initial.len()
            <= crate::tail::OVERLAP_BYTES as usize + 2 * crate::tail::FINGERPRINT_LEN
        {
            initial.push_str(&padding);
        }
        std::fs::write(&path, initial).unwrap();

        let db_path = dir.path().join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.providers.codex.enabled = true;
        config.providers.codex.paths = vec![normalize_path(&root)];
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let session_id = format!("codex:{id}");
        let stored = db
            .session_metadata(std::slice::from_ref(&session_id))
            .unwrap();
        assert_eq!(
            stored[&session_id].parent_session_id.as_deref(),
            Some("codex:root-session")
        );

        let mut outcomes = Vec::new();
        crate::providers::codex::TEST_SPAWN_HEADER_VALIDATION_BYTES
            .with(|counts| counts.borrow_mut().clear());
        for turn in 0..10 {
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap(),
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-06-25T03:04:07Z",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text",
                            "text": format!("spawned tail prompt {turn}")}],
                        "internal_chat_message_metadata_passthrough": {
                            "content_item_metadata": [{"provenance": {"type": "user"}}]
                        }}
                })
            )
            .unwrap();
            if turn == 4 {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b"{\n")
                    .unwrap();
            }
            TEST_TAIL_OUTCOMES.with(|counts| counts.borrow_mut().clear());
            reindex(&config, &db, false, None).unwrap();
            TEST_TAIL_OUTCOMES.with(|counts| outcomes.extend(counts.borrow().iter().copied()));

            let fresh_path = dir.path().join(format!("fresh-{turn}.db"));
            let fresh = Db::open(&fresh_path).unwrap();
            reindex(&config, &fresh, true, None).unwrap();
            let snapshot = |path: &Path| {
                let conn = rusqlite::Connection::open(path).unwrap();
                [
                    "select json_array(seq,role,kind,ts,content,authorship,input_origin,input_delivery_state,record_relation,correlation_id) from messages order by seq,id",
                    "select json_array(parent_session_id,agent_label,title,summary,cwd,created_at,updated_at,last_message_at,preview_text,message_count,json(raw_metadata_json),parse_warning) from sessions order by id",
                    "select transcript_text from transcripts order by session_id",
                ].map(|sql| conn.prepare(sql).unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap())
            };
            assert_eq!(checkpoint_rows(&db_path), checkpoint_rows(&fresh_path));
            for (section, (actual, expected)) in snapshot(&db_path)
                .into_iter()
                .zip(snapshot(&fresh_path))
                .enumerate()
            {
                assert_eq!(actual.len(), expected.len(), "section {section}");
                for (row, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
                    let equal = if section == 1 {
                        serde_json::from_str::<serde_json::Value>(actual).unwrap()
                            == serde_json::from_str::<serde_json::Value>(expected).unwrap()
                    } else {
                        actual == expected
                    };
                    assert!(
                        equal,
                        "section {section}, row {row}: actual={} expected={}",
                        &actual[..actual.len().min(600)],
                        &expected[..expected.len().min(600)]
                    );
                }
            }
        }
        eprintln!("10 append outcomes: {outcomes:?}");
        assert_eq!(outcomes, vec![TailOutcome::Appended; 10]);
        crate::providers::codex::TEST_SPAWN_HEADER_VALIDATION_BYTES
            .with(|counts| assert_eq!(*counts.borrow(), vec![header_length; 10]));
        let filters = crate::models::MessageFilters {
            role: Some(crate::models::Role::User),
            session_id: Some(session_id),
            ..Default::default()
        };
        assert!(db
            .search_messages("spawned tail prompt", &filters)
            .unwrap()
            .is_empty());
        assert_eq!(
            db.search_messages("spawned tail prompt", &Default::default())
                .unwrap()
                .len(),
            11
        );

        let assert_full_fallback = |case| {
            let adapters = ProviderSet::new(&config);
            let source = adapters.discover_enabled(&config).sources.remove(0);
            let outcome = try_tail(
                &source,
                &normalize_path(&path),
                Some(&format!("codex:{id}")),
                &db,
                &|| false,
                |reader, path, context| {
                    adapters
                        .codex
                        .parse_reader_with_context(reader, path, Some(context))
                },
            )
            .unwrap();
            assert_eq!(outcome, TailOutcome::FullParse, "{case}");
            reindex(&config, &db, false, None).unwrap();
        };
        let append_newline = || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"\n")
                .unwrap();
        };
        let rewritten = std::fs::read_to_string(&path)
            .unwrap()
            .replace("worker", "helper");
        std::fs::write(&path, rewritten).unwrap();
        append_newline();
        assert_full_fallback("rewritten header");
        assert_eq!(
            db.session_metadata(std::slice::from_ref(&format!("codex:{id}")))
                .unwrap()[&format!("codex:{id}")]
                .agent_label
                .as_deref(),
            Some("helper")
        );

        let replacement = root.join("replacement.tmp");
        std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        append_newline();
        // File identity is checked by reindex before try_tail is called.
        // Removing the receipt also proves that this path actually reparses.
        rusqlite::Connection::open(&db_path).unwrap().execute(
            "update sessions set raw_metadata_json = json_remove(raw_metadata_json, '$.codex_spawn_header')", []
        ).unwrap();
        TEST_TAIL_OUTCOMES.with(|counts| counts.borrow_mut().clear());
        reindex(&config, &db, false, None).unwrap();
        TEST_TAIL_OUTCOMES.with(|counts| assert!(counts.borrow().is_empty()));
        let stored = db.resolve_session_record(&format!("codex:{id}")).unwrap();
        let metadata: serde_json::Value =
            serde_json::from_str(stored.raw_metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            metadata[crate::providers::codex::SPAWN_HEADER_KEY]["length"],
            header_length
        );

        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "update files_seen set parse_version = 'stale' where provider = 'codex'",
                [],
            )
            .unwrap();
        append_newline();
        assert_full_fallback("stale parser");
    }

    #[test]
    fn claude_rewritten_source_adds_tool_and_notice_rows_without_losing_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let turn = |i| serde_json::json!({"type":"assistant","uuid":format!("reply-{i}-{}", "x".repeat(80)),"sessionId":"rewrite","message":{"role":"assistant","content":format!("Synthetic response {i}")}});
        let initial: Vec<_> = (0..42).map(turn).collect();
        let write = |records: &[serde_json::Value]| {
            std::fs::write(
                &path,
                records.iter().map(|r| format!("{r}\n")).collect::<String>(),
            )
            .unwrap()
        };
        write(&initial);
        let db_path = dir.path().join("index.db");
        let config = config_with_single_claude_fixture(&db_path, &root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let retained = || {
            let mut stmt = conn
                .prepare(
                    "select id,seq,content from messages where kind='conversation' order by seq,id",
                )
                .unwrap();
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        let before = retained();
        let mut rewritten: Vec<_> = (0..40).map(turn).collect();
        rewritten.extend([
            serde_json::json!({"type":"assistant","uuid":"call-event","sessionId":"rewrite","message":{"role":"assistant","content":[{"type":"tool_use","id":"synthetic-call","name":"Bash","input":{"command":"echo synthetic"}}]}}),
            serde_json::json!({"type":"attachment","uuid":"denial","sessionId":"rewrite","attachment":{"type":"hook_blocking_error","content":"Synthetic operation denied by hook"}}),
            serde_json::json!({"type":"user","uuid":"result","sessionId":"rewrite","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"synthetic-call","content":"Synthetic tool result"}]}}),
            serde_json::json!({"type":"assistant","uuid":"final","sessionId":"rewrite","message":{"role":"assistant","content":"Synthetic response after denial"}}),
        ]);
        write(&rewritten);
        let check = |conn: &rusqlite::Connection, label: &str| {
            let mut stmt = conn.prepare("select kind,content from messages where kind in ('tool_call','harness_notice','tool_result') or content='Synthetic response after denial' order by kind,content").unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(rows.len(), 4, "{label} new source rows: {rows:?}");
            rows
        };
        let fresh_path = dir.path().join("fresh.db");
        let fresh = Db::open(&fresh_path).unwrap();
        reindex(
            &config_with_single_claude_fixture(&fresh_path, &root),
            &fresh,
            false,
            None,
        )
        .unwrap();
        let expected = check(&rusqlite::Connection::open(&fresh_path).unwrap(), "fresh");
        for full in [false, true, false] {
            reindex(&config, &db, full, None).unwrap();
            assert_eq!(check(&conn, "refreshed"), expected, "full={full}");
            assert_eq!(&retained()[..before.len()], before.as_slice());
            assert_eq!(db.message_count().unwrap(), 46);
        }
        use std::io::Write as _;
        for i in 0..2 {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(format!("{}\n", turn(100 + i)).as_bytes())
                .unwrap();
            reindex_tail_outcome(&config, &db, TailOutcome::Appended);
        }
    }

    #[test]
    fn claude_queue_delivery_orders_through_indexer() {
        use std::io::Write as _;
        for shape in [
            "u", "auR", "uR", "au", "aRu", "Ru", "auauR", "uRR", "auau", "auRR", "RRu", "Rau",
            "uau", "aPu", "aaauRR",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let mut records = vec![
                serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"delivery-orders","content":"synthetic queued request"}),
            ];
            for (i, ch) in shape.chars().enumerate() {
                records.push(match ch {
                    'a' | 'u' => serde_json::json!({"type":if ch == 'a' {"assistant"} else {"user"},"uuid":format!("turn-{i}"),"sessionId":"delivery-orders","message":{"role":if ch == 'a' {"assistant"} else {"user"},"content":format!("synthetic turn {i}")}}),
                    _ => serde_json::json!({"type":"queue-operation","operation":if ch == 'P' {"popAll"} else {"remove"},"sessionId":"delivery-orders"}),
                });
            }
            records.push(serde_json::json!({"type":"attachment","uuid":"delivery","sessionId":"delivery-orders","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"synthetic queued request","origin":{"kind":"human"}}}));
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            for record in &records {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(format!("{record}\n").as_bytes())
                    .unwrap();
                reindex(&config, &db, false, None).unwrap();
            }
            let check = |db_path: &Path| {
                let conn = rusqlite::Connection::open(db_path).unwrap();
                let counts: (i64, i64) = conn.query_row("select count(*),sum(authorship='human') from messages where content='synthetic queued request'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
                assert_eq!(counts, (1, 1), "{shape}");
            };
            check(&db_path);
            reindex(&config, &db, true, None).unwrap();
            check(&db_path);
            let fresh_path = dir.path().join("fresh.db");
            let fresh = Db::open(&fresh_path).unwrap();
            reindex(
                &config_with_single_claude_fixture(&fresh_path, &root),
                &fresh,
                false,
                None,
            )
            .unwrap();
            check(&fresh_path);
        }
    }

    // Exercise the same guarded tail attempt used by reindex, then compare persisted rows.
    fn assert_queue_refresh(config: &Config, db: &Db, root: &Path, full: bool) {
        TEST_TAIL_OUTCOMES.with(|outcomes| outcomes.borrow_mut().clear());
        reindex(config, db, false, None).unwrap();
        let outcomes = TEST_TAIL_OUTCOMES.with(|outcomes| {
            std::mem::take(&mut *outcomes.borrow_mut())
                .into_iter()
                .map(|outcome| matches!(outcome, TailOutcome::FullParse))
                .collect::<Vec<_>>()
        });
        assert_eq!(outcomes, [full], "expected full={full}");
        let fresh_path = root.parent().unwrap().join("fresh.db");
        let fresh = Db::open(&fresh_path).unwrap();
        reindex(
            &config_with_single_claude_fixture(&fresh_path, root),
            &fresh,
            true,
            None,
        )
        .unwrap();
        let rows = |path: &Path| {
            let conn = rusqlite::Connection::open(path).unwrap();
            let count: i64 = conn
                .query_row(
                    "select count(*) from messages where content='synthetic queued text'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
            let mut stmt = conn.prepare("select json_array(role,tool_name,kind,tool_call_id,is_compaction,content,authorship,correlation_authority,correlation_scope,correlation_id,input_delivery_state,input_origin,record_relation) from messages order by seq,id").unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let transcript: String = conn
                .query_row("select transcript_text from transcripts", [], |r| r.get(0))
                .unwrap();
            (rows, transcript)
        };
        assert_eq!(
            rows(&root.parent().unwrap().join("index.db")),
            rows(&fresh_path)
        );
    }

    fn health_refresh_sequence(mib: usize, benchmark: bool) {
        use std::io::Write as _;
        for provider in [
            Provider::Codex,
            Provider::Pi,
            Provider::GeminiCli,
            Provider::Claude,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("sources");
            let id = "cafe0000-cafe-cafe-cafe-000000000002";
            let relative = if provider == Provider::GeminiCli {
                "project/chats/session-synthetic.jsonl".to_owned()
            } else {
                format!("{id}.jsonl")
            };
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let header = match provider {
                Provider::Codex => json!({"type":"session_meta","payload":{"id":id}}),
                Provider::Pi => json!({"type":"session","id":id}),
                Provider::GeminiCli => json!({"sessionId":id,"kind":"main"}),
                _ => {
                    json!({"type":"user","sessionId":id,"message":{"role":"user","content":"synthetic start"}})
                }
            };
            let unknown =
                |n: usize| json!({"type":format!("future-{n}"),"padding":"x".repeat(65536)});
            let mut file = std::fs::File::create(&path).unwrap();
            writeln!(file, "{header}").unwrap();
            // Admit the first 16 names before they leave the actual overlap window.
            for n in 0..20 {
                writeln!(file, "{}", unknown(n)).unwrap();
            }
            let mut padding_records = 0;
            while file.metadata().unwrap().len() < (mib * 1024 * 1024) as u64 {
                let record = match provider {
                    Provider::Codex => match padding_records % 4 {
                        0 => {
                            json!({"type":"response_item","payload":{"type":"reasoning","encrypted_content":"synthetic opaque","padding":"x".repeat(65536)}})
                        }
                        1 => {
                            json!({"type":"world_state","payload":{"state":{},"padding":"x".repeat(65536)}})
                        }
                        2 => {
                            json!({"type":"event_msg","payload":{"type":"item_completed","padding":"x".repeat(65536)}})
                        }
                        _ => {
                            json!({"type":"response_item","payload":{"type":"additional_tools","tools":[],"padding":"x".repeat(65536)}})
                        }
                    },
                    _ => unknown(19),
                };
                writeln!(file, "{record}").unwrap();
                padding_records += 1;
            }
            file.flush().unwrap();
            let db_path = dir.path().join("index.db");
            let mut config = config_with_no_providers(&db_path);
            let selected = match provider {
                Provider::Codex => &mut config.providers.codex,
                Provider::Pi => &mut config.providers.pi,
                Provider::GeminiCli => &mut config.providers.gemini_cli,
                _ => &mut config.providers.claude,
            };
            selected.enabled = true;
            selected.paths = vec![normalize_path(&root)];
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let mut full_count = 0;
            let mut latencies = Vec::new();
            for turn in 0..16 {
                writeln!(file, "{}", json!({"type":format!("future-{}",turn % 21)})).unwrap();
                let message = match provider {
                    Provider::Codex => {
                        json!({"type":"response_item","payload":{"type":"message","role":"user","content":"synthetic append"}})
                    }
                    Provider::Pi => {
                        json!({"type":"message","id":format!("turn-{turn}"),"message":{"role":"user","content":"synthetic append"}})
                    }
                    Provider::GeminiCli => {
                        json!({"type":"user","id":format!("turn-{turn}"),"content":"synthetic append"})
                    }
                    _ => {
                        json!({"type":"user","uuid":format!("turn-{turn}"),"sessionId":id,"message":{"role":"user","content":"synthetic append"}})
                    }
                };
                writeln!(file, "{message}").unwrap();
                file.flush().unwrap();
                TEST_TAIL_OUTCOMES.with(|outcomes| outcomes.borrow_mut().clear());
                let start = std::time::Instant::now();
                reindex(&config, &db, false, None).unwrap();
                latencies.push(start.elapsed().as_secs_f64());
                let full = TEST_TAIL_OUTCOMES.with(|outcomes| {
                    let outcomes = std::mem::take(&mut *outcomes.borrow_mut());
                    assert_eq!(outcomes.len(), 1, "{provider:?}");
                    matches!(outcomes[0], TailOutcome::FullParse)
                });
                full_count += usize::from(full);
                if !benchmark {
                    if provider != Provider::GeminiCli {
                        assert!(!full, "{provider:?}: health forced full parse");
                    }
                    let fresh_path = dir.path().join(format!("fresh-{turn}.db"));
                    let fresh = Db::open(&fresh_path).unwrap();
                    let mut fresh_config = config.clone();
                    fresh_config.index.db_path = Some(normalize_path(&fresh_path));
                    reindex(&fresh_config, &fresh, true, None).unwrap();
                    assert_eq!(
                        serde_json::to_value(db.parser_health().unwrap().unrecognized_record_types)
                            .unwrap(),
                        serde_json::to_value(
                            fresh.parser_health().unwrap().unrecognized_record_types
                        )
                        .unwrap(),
                        "{provider:?} turn {turn}"
                    );
                }
            }
            if !benchmark {
                let before =
                    serde_json::to_value(db.parser_health().unwrap().unrecognized_record_types)
                        .unwrap();
                write!(file, "{{\"type\":\"partial").unwrap();
                file.flush().unwrap();
                reindex(&config, &db, false, None).unwrap();
                assert_eq!(
                    serde_json::to_value(db.parser_health().unwrap().unrecognized_record_types)
                        .unwrap(),
                    before
                );
                writeln!(file, "-unknown\"}}").unwrap();
                file.flush().unwrap();
                reindex(&config, &db, false, None).unwrap();
                let fresh_path = dir.path().join("fresh-partial.db");
                let fresh = Db::open(&fresh_path).unwrap();
                let mut fresh_config = config.clone();
                fresh_config.index.db_path = Some(normalize_path(&fresh_path));
                reindex(&fresh_config, &fresh, true, None).unwrap();
                assert_eq!(
                    serde_json::to_value(db.parser_health().unwrap().unrecognized_record_types)
                        .unwrap(),
                    serde_json::to_value(fresh.parser_health().unwrap().unrecognized_record_types)
                        .unwrap(),
                    "{provider:?}: completed partial record"
                );
            }
            eprintln!("HEALTH_REFRESH provider={provider:?} bytes={} full={full_count} tail={} seconds={latencies:?}",file.metadata().unwrap().len(),16-full_count);
        }
    }

    #[test]
    fn health_refresh_inventory_equals_full_beyond_overlap() {
        health_refresh_sequence(3, false);
    }

    #[test]
    #[ignore = "60 MiB whole-refresh timing workload"]
    fn health_refresh_benchmark() {
        health_refresh_sequence(60, true);
    }

    #[test]
    fn codex_overlap_markers_do_not_reparse_ordinary_appends() {
        use std::io::Write as _;
        let mut observed = Vec::new();
        for record in [
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","name":"request_user_input","call_id":"ask","arguments":"{\"questions\":[{\"id\":\"q\",\"question\":\"Proceed?\"}]}"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"ask","output":{"answers":{"q":{"answers":["yes"]}}}}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"thread_settings_applied"}}),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("codex");
            std::fs::create_dir_all(&root).unwrap();
            let id = "019efd97-d602-7922-89dd-467272106505";
            let path = root.join(format!("rollout-2026-06-25T03-04-06-{id}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{}\n{record}\n",
                    serde_json::json!({"type":"session_meta","payload":{"id":id}})
                ),
            )
            .unwrap();
            let db_path = dir.path().join("index.db");
            let mut config = config_with_no_providers(&db_path);
            config.providers.codex.enabled = true;
            config.providers.codex.paths = vec![normalize_path(&root)];
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            writeln!(std::fs::OpenOptions::new().append(true).open(&path).unwrap(),"{}",serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":"synthetic reply"}})).unwrap();
            TEST_TAIL_OUTCOMES.with(|outcomes| outcomes.borrow_mut().clear());
            let start = std::time::Instant::now();
            reindex(&config, &db, false, None).unwrap();
            let outcomes = TEST_TAIL_OUTCOMES.with(|outcomes| {
                std::mem::take(&mut *outcomes.borrow_mut())
                    .into_iter()
                    .map(|outcome| matches!(outcome, TailOutcome::FullParse))
                    .collect::<Vec<_>>()
            });
            eprintln!(
                "Codex overlap {record}: full={outcomes:?}, seconds={:.4}",
                start.elapsed().as_secs_f64()
            );
            observed.extend(outcomes);
        }
        assert_eq!(observed, [false, false, false]);
    }

    #[test]
    fn claude_queue_refresh_enqueue_replaces_attachment_identity() {
        queue_refresh_attachment_replacement(true);
    }

    #[test]
    fn claude_queue_refresh_user_replaces_attachment_identity() {
        queue_refresh_attachment_replacement(false);
    }

    fn queue_refresh_attachment_replacement(enqueue: bool) {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let attachment = serde_json::json!({"type":"attachment","uuid":"delivery","timestamp":"2026-11-01T00:00:00Z","sessionId":"queue-routing","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"synthetic queued text","origin":{"kind":"human"}}});
        std::fs::write(&path, format!("{attachment}\n")).unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_single_claude_fixture(&db_path, &root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let mut delivery = if enqueue {
            serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"queue-routing","uuid":"enqueue","content":"synthetic queued text"})
        } else {
            serde_json::json!({"type":"user","uuid":"user-copy","sessionId":"queue-routing","message":{"role":"user","content":"synthetic queued text"}})
        };
        delivery["timestamp"] = serde_json::json!("2026-11-01T00:00:01Z");
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "{delivery}"
        )
        .unwrap();
        assert_queue_refresh(&config, &db, &root, true);
    }

    #[test]
    fn claude_queue_refresh_retires_attachment_transcript_with_archive() {
        use std::io::Write as _;
        for enqueue in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let archive = serde_json::json!({"type":"assistant","uuid":"archived","sessionId":"queue-routing","message":{"role":"assistant","content":"synthetic archived reply"}});
            let attachment = serde_json::json!({"type":"attachment","uuid":"delivery","timestamp":"2026-11-01T00:00:00Z","sessionId":"queue-routing","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"synthetic queued text","origin":{"kind":"human"}}});
            std::fs::write(&path, format!("{archive}\n{attachment}\n")).unwrap();
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            std::fs::write(&path, format!("{attachment}\n")).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let mut delivery = if enqueue {
                serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"queue-routing","uuid":"enqueue","content":"synthetic queued text"})
            } else {
                serde_json::json!({"type":"user","uuid":"user-copy","sessionId":"queue-routing","message":{"role":"user","content":"synthetic queued text"}})
            };
            delivery["timestamp"] = serde_json::json!("2026-11-01T00:00:01Z");
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap(),
                "{delivery}"
            )
            .unwrap();
            reindex(&config, &db, false, None).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let count: i64 = conn
                .query_row(
                    "select count(*) from messages where content='synthetic queued text'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
            let transcript: String = conn
                .query_row("select transcript_text from transcripts", [], |r| r.get(0))
                .unwrap();
            assert!(transcript.contains("synthetic archived reply"));
            assert_eq!(
                transcript.matches("synthetic queued text").count(),
                1,
                "enqueue={enqueue}: {transcript}"
            );
        }
    }

    #[test]
    fn claude_queue_refresh_completed_delivery() {
        queue_refresh_delivery(true);
    }

    #[test]
    fn claude_queue_refresh_enqueue_then_delivery() {
        queue_refresh_delivery(false);
    }

    fn queue_refresh_delivery(already_delivered: bool) {
        use std::io::Write as _;
        {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let enqueue = serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"queue-routing","uuid":"enqueue","content":"synthetic queued text"});
            let attachment = serde_json::json!({"type":"attachment","uuid":"delivery","sessionId":"queue-routing","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"synthetic queued text","origin":{"kind":"human"}}});
            let remove = serde_json::json!({"type":"queue-operation","operation":"remove","sessionId":"queue-routing"});
            let ordinary = |i| serde_json::json!({"type":"assistant","uuid":format!("reply-{i}"),"sessionId":"queue-routing","message":{"role":"assistant","content":format!("synthetic reply {i}")}});
            std::fs::write(
                &path,
                if already_delivered {
                    format!("{enqueue}\n{}\n{attachment}\n{remove}\n", ordinary(0))
                } else {
                    format!("{}\n", ordinary(0))
                },
            )
            .unwrap();
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            if !already_delivered {
                writeln!(file, "{enqueue}").unwrap();
                assert_queue_refresh(&config, &db, &root, false);
                writeln!(file, "{attachment}").unwrap();
                assert_queue_refresh(&config, &db, &root, true);
                writeln!(file, "{remove}").unwrap();
                assert_queue_refresh(&config, &db, &root, false);
                writeln!(file, "{}", serde_json::json!({"type":"queue-operation","operation":"popAll","sessionId":"queue-routing"})).unwrap();
                assert_queue_refresh(&config, &db, &root, false);
            }
            for i in 1..=3 {
                writeln!(file, "{}", ordinary(i)).unwrap();
                assert_queue_refresh(&config, &db, &root, false);
            }
        }
    }

    #[test]
    fn claude_queue_refresh_distant_delivery() {
        queue_refresh_distant(false, false);
    }

    #[test]
    fn claude_queue_refresh_distant_split_delivery() {
        queue_refresh_distant(true, false);
    }

    #[test]
    fn claude_queue_refresh_distant_attachment_delivery() {
        queue_refresh_distant(false, true);
    }

    fn queue_refresh_distant(split: bool, attachment: bool) {
        use std::io::Write as _;
        {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let enqueue = serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"queue-routing","uuid":"enqueue","content":"synthetic queued text"});
            std::fs::write(
                &path,
                format!(
                    "{enqueue}\n{}",
                    claude_overlap_padding("queue-routing")
                        .lines()
                        .skip(1)
                        .map(|l| format!("{l}\n"))
                        .collect::<String>()
                ),
            )
            .unwrap();
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file,"{}",serde_json::json!({"type":"queue-operation","operation":"dequeue","sessionId":"queue-routing"})).unwrap();
            if split {
                assert_queue_refresh(&config, &db, &root, false);
                // Evict the dequeue itself from the overlap while it still awaits user text.
                writeln!(file, "{}", serde_json::json!({"type":"assistant","uuid":"padding-after-dequeue","sessionId":"queue-routing","message":{"role":"assistant","content":"z".repeat(2 * crate::tail::OVERLAP_BYTES as usize)}})).unwrap();
                assert_queue_refresh(&config, &db, &root, false);
            }
            let delivery = if attachment {
                serde_json::json!({"type":"attachment","uuid":"delivery","sessionId":"queue-routing","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"synthetic queued text","origin":{"kind":"human"}}})
            } else {
                serde_json::json!({"type":"user","uuid":"delivery","sessionId":"queue-routing","message":{"role":"user","content":"synthetic queued text"}})
            };
            writeln!(file, "{delivery}").unwrap();
            assert_queue_refresh(&config, &db, &root, true);
            if !attachment {
                let stored = db.resolve_session_record("claude:queue-routing").unwrap();
                let metadata: serde_json::Value =
                    serde_json::from_str(stored.raw_metadata_json.as_deref().unwrap()).unwrap();
                assert!(metadata.get("pending_direct_delivery").is_none());
            }
            writeln!(file,"{}",serde_json::json!({"type":"assistant","uuid":"after","sessionId":"queue-routing","message":{"role":"assistant","content":"synthetic following reply"}})).unwrap();
            assert_queue_refresh(&config, &db, &root, false);
        }
    }

    #[test]
    fn claude_direct_queue_delivery_orders_through_indexer() {
        use std::io::Write as _;
        // X/Y enqueue text, D dequeue, R remove, P popAll, A reply, U direct X,
        // T/W attachment X/Y, C contentless enqueue, O unrelated user turn.
        for (name, order, expected) in [
            ("dequeue-direct", "XDUA", (1, 0)),
            ("cancelled-then-dequeue", "YRAXDUA", (1, 1)),
            ("popall-then-dequeue", "YPAXDUA", (1, 1)),
            ("attach-remove", "XTRA", (1, 0)),
            ("a-attach-remove", "XAOTRA", (1, 0)),
            ("remove-before-attach", "XARTA", (1, 0)),
            ("attach-then-dequeue-same-text", "XATRAXDUA", (2, 0)),
            ("two-queued-fifo", "XYATWRRA", (1, 1)),
            ("contentless-dequeue-then-attach", "CDOAXATR", (1, 0)),
            ("cancelled-then-attach-other", "YRXATRXDU", (2, 1)),
            ("empty-dequeue", "DU", (1, 0)),
            ("undelivered-dequeue", "XD", (1, 0)),
            ("unmatched-direct", "YDU", (1, 1)),
            ("oldest-matching-enqueue", "XXDUT", (2, 0)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            for (i, ch) in order.chars().enumerate() {
                let text = if matches!(ch, 'Y' | 'W') {
                    "probe text yankee"
                } else {
                    "probe text xray"
                };
                let mut record = match ch {
                    'X' | 'Y' => {
                        serde_json::json!({"type":"queue-operation","operation":"enqueue","content":text})
                    }
                    'C' => serde_json::json!({"type":"queue-operation","operation":"enqueue"}),
                    'D' | 'R' | 'P' => {
                        serde_json::json!({"type":"queue-operation","operation":match ch { 'D' => "dequeue", 'R' => "remove", _ => "popAll" }})
                    }
                    'T' | 'W' => {
                        serde_json::json!({"type":"attachment","attachment":{"type":"queued_command","commandMode":"prompt","prompt":text,"origin":{"kind":"human"}}})
                    }
                    _ => {
                        serde_json::json!({"type":if ch == 'A' { "assistant" } else { "user" },"message":{"role":if ch == 'A' { "assistant" } else { "user" },"content":match ch { 'A' => "synthetic reply", 'O' => "other", _ => text }}})
                    }
                };
                record["sessionId"] = serde_json::json!("direct-orders");
                record["uuid"] = serde_json::json!(format!("event-{i}"));
                record["timestamp"] = serde_json::json!(format!("2026-11-01T00:00:{i:02}Z"));
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(format!("{record}\n").as_bytes())
                    .unwrap();
                reindex(&config, &db, false, None).unwrap();
            }
            let check = |db_path: &Path, mode: &str| {
                let conn = rusqlite::Connection::open(db_path).unwrap();
                let count = |text: &str| {
                    conn.query_row(
                    "select count(*) from messages where content=?1 and role='user' and kind='conversation'",
                    [text], |r| r.get::<_, i64>(0)).unwrap()
                };
                assert_eq!(
                    (count("probe text xray"), count("probe text yankee")),
                    expected,
                    "{name} {mode}"
                );
                if name == "oldest-matching-enqueue" {
                    let surviving: String = conn.query_row("select ts from messages where content='probe text xray' and input_origin='queued'", [], |r| r.get(0)).unwrap();
                    assert!(
                        surviving.contains("00:00:01"),
                        "oldest enqueue must close: {surviving}"
                    );
                }
            };
            check(&db_path, "stepwise");
            reindex(&config, &db, true, None).unwrap();
            check(&db_path, "full refresh");
            let fresh_path = dir.path().join("fresh.db");
            let fresh = Db::open(&fresh_path).unwrap();
            reindex(
                &config_with_single_claude_fixture(&fresh_path, &root),
                &fresh,
                false,
                None,
            )
            .unwrap();
            check(&fresh_path, "fresh");
        }
    }

    #[test]
    fn claude_direct_queue_delivery_reparses_v17_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let records = [
            serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"upgrade-direct","content":"cancelled synthetic input"}),
            serde_json::json!({"type":"queue-operation","operation":"remove","sessionId":"upgrade-direct"}),
            serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"upgrade-direct","content":"delivered synthetic input"}),
            serde_json::json!({"type":"queue-operation","operation":"dequeue","sessionId":"upgrade-direct"}),
            serde_json::json!({"type":"user","uuid":"direct","sessionId":"upgrade-direct","message":{"role":"user","content":"delivered synthetic input"}}),
        ];
        let render = |skip_dequeue: bool| {
            records
                .iter()
                .filter(|r| !skip_dequeue || r["operation"] != "dequeue")
                .map(|r| format!("{r}\n"))
                .collect::<String>()
        };
        std::fs::write(&path, render(false)).unwrap();
        let db_path = dir.path().join("index.db");
        let config = config_with_single_claude_fixture(&db_path, &root);
        let adapters = ProviderSet::new(&config);
        let source = adapters.discover_enabled(&config).sources.remove(0);
        // v17 retained both enqueue and direct user row after cancellation. Omitting dequeue
        // reproduces those same stored rows without keeping a copy of the old parser.
        let mut old = adapters
            .claude
            .parse_reader(std::io::Cursor::new(render(true)), &source.path)
            .unwrap();
        old.session.parse_version = "claude-v17".into();
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        db.upsert_session(&old, source.mtime_ns, source.size_bytes)
            .unwrap();
        let (offset, fingerprint) = full_parse_checkpoint(&source).unwrap();
        db.set_file_checkpoint(
            source.provider,
            &normalize_path(&path),
            offset,
            &fingerprint,
        )
        .unwrap();
        let count = || {
            db.search_messages("delivered synthetic input", &Default::default())
                .unwrap()
                .len()
        };
        assert_eq!(count(), 2);
        assert_eq!(reindex(&config, &db, false, None).unwrap().1, 1);
        assert_eq!(count(), 1);
        assert_eq!(reindex(&config, &db, false, None).unwrap().1, 0);
    }

    #[test]
    fn claude_direct_queue_delivery_beyond_overlap() {
        use std::io::Write as _;
        for dequeue_before_padding in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let enqueue = "{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"sessionId\":\"direct-tail\",\"content\":\"synthetic tail delivery\"}\n";
            let dequeue = "{\"type\":\"queue-operation\",\"operation\":\"dequeue\",\"sessionId\":\"direct-tail\"}\n";
            std::fs::write(
                &path,
                format!(
                    "{enqueue}{}{}",
                    if dequeue_before_padding { dequeue } else { "" },
                    claude_overlap_padding("direct-tail")
                        .lines()
                        .skip(1)
                        .map(|line| format!("{line}\n"))
                        .collect::<String>()
                ),
            )
            .unwrap();
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            if !dequeue_before_padding {
                file.write_all(dequeue.as_bytes()).unwrap();
            }
            reindex(&config, &db, false, None).unwrap();
            file.write_all(b"{\"type\":\"user\",\"uuid\":\"delivered\",\"sessionId\":\"direct-tail\",\"message\":{\"role\":\"user\",\"content\":\"synthetic tail delivery\"}}\n").unwrap();
            let adapters = ProviderSet::new(&config);
            let source = adapters.discover_enabled(&config).sources.remove(0);
            assert!(matches!(
                try_tail(
                    &source,
                    &normalize_path(&path),
                    Some("claude:direct-tail"),
                    &db,
                    &|| false,
                    |reader, path, context| adapters.claude.parse_reader_with_context(
                        reader,
                        path,
                        Some(context)
                    )
                )
                .unwrap(),
                TailOutcome::FullParse
            ));
            reindex(&config, &db, false, None).unwrap();
            assert_eq!(
                db.search_messages("synthetic tail delivery", &Default::default())
                    .unwrap()
                    .len(),
                1,
                "dequeue before padding={dequeue_before_padding}"
            );
            reindex(&config, &db, true, None).unwrap();
            assert_eq!(
                db.search_messages("synthetic tail delivery", &Default::default())
                    .unwrap()
                    .len(),
                1,
                "dequeue before padding={dequeue_before_padding}"
            );
        }
    }

    #[test]
    fn claude_queue_sender_evidence_is_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        let records = [
            serde_json::json!({"type":"queue-operation","operation":"enqueue","uuid":"first","sessionId":"fifo","content":"synthetic repeated request"}),
            serde_json::json!({"type":"queue-operation","operation":"enqueue","uuid":"second","sessionId":"fifo","content":"synthetic repeated request"}),
            serde_json::json!({"type":"assistant","uuid":"reply","sessionId":"fifo","message":{"role":"assistant","content":"synthetic reply"}}),
            serde_json::json!({"type":"attachment","uuid":"human-delivery","sessionId":"fifo","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"synthetic repeated request","origin":{"kind":"human"}}}),
            serde_json::json!({"type":"attachment","uuid":"agent-delivery","sessionId":"fifo","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"synthetic repeated request","origin":{"kind":"peer","agent_id":"synthetic-worker"}}}),
        ];
        let db_path = dir.path().join("index.db");
        let config = config_with_single_claude_fixture(&db_path, &root);
        let db = Db::open(&db_path).unwrap();
        let mut source = String::new();
        for record in records {
            source.push_str(&format!("{record}\n"));
            std::fs::write(root.join("session.jsonl"), &source).unwrap();
            reindex(&config, &db, false, None).unwrap();
        }
        for full in [false, true] {
            reindex(&config, &db, full, None).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let mut stmt = conn.prepare("select correlation_id,authorship from messages where content='synthetic repeated request' order by seq,id").unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                rows,
                vec![
                    ("first".into(), "human".into()),
                    ("second".into(), "agent".into())
                ]
            );
        }
    }

    #[test]
    fn claude_dequeue_closes_only_its_own_submission() {
        use std::io::Write as _;
        for later_content in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let mut later = serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"dequeue"});
            if later_content {
                later["content"] = serde_json::json!("synthetic repeat");
            }
            let records = [
                serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"dequeue","content":"synthetic repeat"}),
                serde_json::json!({"type":"queue-operation","operation":"dequeue","sessionId":"dequeue"}),
                serde_json::json!({"type":"user","uuid":"direct","sessionId":"dequeue","message":{"role":"user","content":"synthetic repeat"}}),
                serde_json::json!({"type":"assistant","uuid":"reply","sessionId":"dequeue","message":{"role":"assistant","content":"synthetic reply"}}),
                later,
                serde_json::json!({"type":"attachment","uuid":"delivery","sessionId":"dequeue","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"synthetic repeat","origin":{"kind":"human"}}}),
            ];
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            for record in &records {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(format!("{record}\n").as_bytes())
                    .unwrap();
                reindex(&config, &db, false, None).unwrap();
            }
            let check = |db_path: &Path| {
                let conn = rusqlite::Connection::open(db_path).unwrap();
                let rows: (i64, i64) = conn.query_row("select count(*),sum(authorship='human') from messages where content='synthetic repeat'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
                assert_eq!(rows, (2, 2), "later content={later_content}");
            };
            check(&db_path);
            reindex(&config, &db, true, None).unwrap();
            check(&db_path);
            let fresh_path = dir.path().join("fresh.db");
            let fresh = Db::open(&fresh_path).unwrap();
            reindex(
                &config_with_single_claude_fixture(&fresh_path, &root),
                &fresh,
                false,
                None,
            )
            .unwrap();
            check(&fresh_path);
        }
    }

    #[test]
    fn claude_queued_attachments_do_not_consume_other_submissions() {
        use std::io::Write as _;
        for arrangement in [
            "contentless",
            "attachment-first",
            "enqueue-first",
            "earlier-enqueue",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("session.jsonl");
            let attachment = serde_json::json!({"type":"attachment","uuid":"first-delivery","sessionId":"repeat-queue","timestamp":"2026-11-01T00:00:02Z","attachment":{"type":"queued_command","prompt":"repeat synthetic submission","commandMode":"prompt","origin":{"kind":"human"}}});
            let enqueue = serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"repeat-queue","timestamp":"2026-11-01T00:00:01Z","content":"repeat synthetic submission"});
            let records = if arrangement == "earlier-enqueue" {
                vec![enqueue.clone()]
            } else if arrangement == "contentless" {
                vec![
                    serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"repeat-queue"}),
                    attachment.clone(),
                ]
            } else if arrangement == "attachment-first" {
                vec![attachment.clone(), enqueue]
            } else {
                vec![enqueue, attachment.clone()]
            };
            std::fs::write(
                &path,
                records.iter().map(|r| format!("{r}\n")).collect::<String>(),
            )
            .unwrap();
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let reply = serde_json::json!({"type":"assistant","uuid":"reply","sessionId":"repeat-queue","message":{"role":"assistant","content":"synthetic response"}});
            let later = serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"repeat-queue","timestamp":"2026-11-01T00:00:03Z","content":"repeat synthetic submission"});
            let appended = if arrangement == "earlier-enqueue" {
                vec![
                    reply,
                    serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"repeat-queue"}),
                    attachment,
                ]
            } else {
                vec![reply, later]
            };
            for record in appended {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(format!("{record}\n").as_bytes())
                    .unwrap();
                reindex(&config, &db, false, None).unwrap();
            }
            let rows = |path: &Path| {
                let conn = rusqlite::Connection::open(path).unwrap();
                let mut stmt = conn.prepare("select json_array(role,kind,content,authorship,input_origin,input_delivery_state,correlation_authority,correlation_scope,correlation_id) from messages order by seq,id").unwrap();
                stmt.query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
            };
            reindex(&config, &db, false, None).unwrap();
            let fresh_path = dir.path().join("fresh.db");
            let fresh = Db::open(&fresh_path).unwrap();
            reindex(
                &config_with_single_claude_fixture(&fresh_path, &root),
                &fresh,
                false,
                None,
            )
            .unwrap();
            for indexed in [&db, &fresh] {
                assert_eq!(
                    indexed
                        .search_messages(
                            "repeat synthetic submission",
                            &crate::models::MessageFilters {
                                role: Some(crate::models::Role::User),
                                ..Default::default()
                            }
                        )
                        .unwrap()
                        .len(),
                    if arrangement == "earlier-enqueue" {
                        1
                    } else {
                        2
                    },
                    "{arrangement}"
                );
            }
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let authors: (i64,i64) = conn.query_row("select sum(authorship='human'),sum(authorship='unknown') from messages where content='repeat synthetic submission'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(
                authors,
                (
                    1,
                    if arrangement == "earlier-enqueue" {
                        0
                    } else {
                        1
                    }
                )
            );
            assert_eq!(rows(&db_path), rows(&fresh_path));
            reindex(&config, &db, true, None).unwrap();
            assert_eq!(rows(&db_path), rows(&fresh_path));
        }
    }

    #[test]
    fn claude_queued_sender_evidence_does_not_match_an_earlier_turn() {
        let mut observed = Vec::new();
        for with_enqueue in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("claude");
            std::fs::create_dir_all(&root).unwrap();
            let mut records = vec![
                serde_json::json!({"type":"user","uuid":"ordinary","sessionId":"queue-evidence","timestamp":"2026-11-01T00:00:00Z","message":{"role":"user","content":"repeat synthetic request"}}),
            ];
            if with_enqueue {
                records.push(serde_json::json!({"type":"queue-operation","operation":"enqueue","sessionId":"queue-evidence","timestamp":"2026-11-01T00:00:01Z","content":"repeat synthetic request"}));
            }
            records.push(serde_json::json!({"type":"attachment","uuid":"queued","sessionId":"queue-evidence","timestamp":"2026-11-01T00:00:02Z","attachment":{"type":"queued_command","prompt":"repeat synthetic request","commandMode":"prompt","origin":{"kind":"human"}}}));
            std::fs::write(
                root.join("session.jsonl"),
                records.iter().map(|r| format!("{r}\n")).collect::<String>(),
            )
            .unwrap();
            let db_path = dir.path().join("index.db");
            let config = config_with_single_claude_fixture(&db_path, &root);
            let db = Db::open(&db_path).unwrap();
            reindex(&config, &db, false, None).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let counts: (i64, i64, i64) = conn.query_row("select count(*), sum(authorship='human'), count(distinct correlation_id) from messages where content='repeat synthetic request'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
            let hits = db
                .search_messages(
                    "repeat synthetic request",
                    &crate::models::MessageFilters {
                        role: Some(crate::models::Role::User),
                        ..Default::default()
                    },
                )
                .unwrap()
                .len();
            observed.push((counts, hits));
            reindex(&config, &db, true, None).unwrap();
            assert_eq!(db.message_count().unwrap(), counts.0);
        }
        assert_eq!(observed, vec![((2, 2, 2), 2), ((2, 2, 2), 2)]);
    }

    #[test]
    fn queue_enqueue_beyond_overlap_appends_and_preserves_metadata() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let claude_root = dir.path().join("claude");
        std::fs::create_dir_all(&claude_root).unwrap();
        let source = claude_root.join("session.jsonl");
        std::fs::write(&source, claude_overlap_padding("queue-tail")).unwrap();
        let config = config_with_single_claude_fixture(&db_path, &claude_root);
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(
                br#"{"type":"queue-operation","operation":"enqueue","sessionId":"queue-tail","timestamp":"2026-06-17T00:00:00Z","content":"queued tail correction"}
"#,
            )
            .unwrap();

        let adapters = ProviderSet::new(&config);
        let source_file = adapters
            .discover_enabled(&config)
            .sources
            .into_iter()
            .next()
            .unwrap();
        let outcome = try_tail(
            &source_file,
            &normalize_path(&source_file.path),
            Some("claude:queue-tail"),
            &db,
            &|| false,
            |reader, path, context| {
                adapters
                    .claude
                    .parse_reader_with_context(reader, path, Some(context))
            },
        )
        .unwrap();
        assert!(matches!(outcome, TailOutcome::Appended));
        reindex(&config, &db, false, None).unwrap();
        let messages = db
            .search_messages(
                "queued tail correction",
                &crate::models::MessageFilters {
                    session_id: Some("claude:queue-tail".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        let queued = messages
            .iter()
            .find(|message| message.content == "queued tail correction")
            .expect("tail parse preserved queue input");
        let stored = rusqlite::Connection::open(&db_path).unwrap();
        let metadata: (String, String, String) = stored
            .query_row(
                "select authorship, input_origin, input_delivery_state from messages
             where session_id=?1 and content=?2",
                rusqlite::params![queued.session_id, queued.content],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            metadata,
            ("unknown".into(), "queued".into(), "submitted".into())
        );
        assert_eq!(
            db.search_messages(
                "queued tail correction",
                &crate::models::MessageFilters {
                    role: Some(crate::models::Role::User),
                    session_id: Some("claude:queue-tail".into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1
        );
        assert!(db
            .resolve_session("claude:queue-tail")
            .unwrap()
            .session
            .parse_warning
            .is_none());
    }

    #[test]
    fn a_failed_refresh_of_an_indexed_source_keeps_its_rows_and_reports_a_warning() {
        // A compressed rollout that cannot be decoded replaced the plain file the index holds.
        // The stored rows stay, and the run says which file failed instead of skipping it quietly.
        let dir = tempfile::tempdir().unwrap();
        let codex_root = dir.path().join("codex");
        std::fs::create_dir_all(&codex_root).unwrap();
        let plain = codex_root
            .join("rollout-2026-06-25T03-04-06-019efd97-d602-7922-89dd-467272106505.jsonl");
        std::fs::write(
            &plain,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/tmp/proj"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"do the thing"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let db_path = dir.path().join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.providers.codex.enabled = true;
        config.providers.codex.paths = vec![codex_root.to_string_lossy().to_string()];
        let db = Db::open(&db_path).unwrap();
        reindex_until(&config, &db, false, None, &|| false).unwrap();
        let stored = db.message_count().unwrap();
        assert!(stored > 0);

        let compressed = plain.with_extension("jsonl.zst");
        std::fs::write(&compressed, b"not a zstd stream").unwrap();
        std::fs::remove_file(&plain).unwrap();
        let ReindexRun::Completed {
            discovery_warnings, ..
        } = reindex_until(&config, &db, false, None, &|| false).unwrap()
        else {
            panic!("the default token never cancels");
        };

        assert_eq!(db.message_count().unwrap(), stored, "stored rows stay");
        let warning = discovery_warnings
            .iter()
            // Discovery canonicalizes roots (macOS reports /var as /private/var), so match the
            // file name rather than the spelling of the temporary directory.
            .find(|warning| warning.path.ends_with(".jsonl.zst"))
            .unwrap_or_else(|| panic!("no warning names the failed file: {discovery_warnings:?}"));
        assert_eq!(warning.operation, "refresh");
        assert!(warning.readable_sources_preserved);
        assert!(!warning.message.is_empty());
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
    fn released_schema_five_without_input_columns_stays_readable_for_unrestricted_reads() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        drop(Db::open(&db_path).unwrap());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        // This is the released v1.0.0 shape for the fields added after its schema-5 stamp. Keep
        // the derived FTS layout intact: an old index is readable content, not a broken search
        // index, and only the user-input scope needs the additive columns.
        conn.execute_batch(
            "drop index idx_messages_session_authorship_relation_seq;
             drop index idx_messages_authorship_relation_ts;
             alter table messages drop column input_delivery_state;
             alter table messages drop column input_origin;
             create index idx_messages_session_authorship_relation_seq
                 on messages(session_id, authorship, record_relation, seq);
             create index idx_messages_authorship_relation_ts
                 on messages(authorship, record_relation, ts desc, session_id, seq);
             pragma user_version = 5;",
        )
        .unwrap();
        let config = config_with_no_providers(&db_path);
        // Generation 5 is readable; a writable open promotes it to the current generation.
        assert_eq!(
            IndexCoordinator::new(&config).inspect_schema().unwrap(),
            SchemaState::Older {
                current: 5,
                required: SCHEMA_VERSION
            }
        );
        assert!(message_input_provenance_layout_problem(&conn)
            .unwrap()
            .is_some());
        assert!(current_schema_layout_problem(&conn).unwrap().is_none());
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
    fn gemini_json_jsonl_family_keeps_predecessor_history_and_two_receipts() {
        use crate::models::MessageFilters;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let root = dir.path().join("gemini");
        let chats = root.join("project-hash").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let legacy = chats.join("session-2026-07-12T10-30-family.json");
        let successor = chats.join("session-2026-07-12T10-30-family.jsonl");
        std::fs::write(
            &legacy,
            r#"{"sessionId":"g-family","kind":"main","messages":[
                {"id":"u1","type":"user","content":"original user input","timestamp":"2026-07-12T10:30:01Z"},
                {"id":"old-answer","type":"gemini","content":"legacy-only answer","timestamp":"2026-07-12T10:30:02Z"}
            ]}"#,
        )
        .unwrap();
        let mut config = config_with_no_providers(&db_path);
        config.providers.gemini_cli.enabled = true;
        config.providers.gemini_cli.paths = vec![root.to_string_lossy().to_string()];
        let db = Db::open(&db_path).unwrap();

        let (files_seen, _) = reindex(&config, &db, false, None).unwrap();
        assert_eq!(
            files_seen, 1,
            "legacy-only partial migration is indexed first"
        );
        assert_eq!(db.indexed_source_identities().unwrap().len(), 1);

        let old_reference = db
            .search_messages("legacy-only answer", &MessageFilters::default())
            .unwrap();
        let old_reference = (old_reference[0].session_id.clone(), old_reference[0].seq);

        std::fs::write(
            &successor,
            concat!(
                r#"{"sessionId":"g-family","kind":"main","startTime":"2026-07-12T10:30:00Z"}"#,
                "\n",
                r#"{"id":"u1","type":"user","content":"original user input","timestamp":"2026-07-12T10:30:01Z"}"#,
                "\n",
                r#"{"id":"echo","type":"user","content":[{"functionResponse":{"id":"q","name":"ask_user","response":{"output":"echoed"}}}]}"#,
                "\n",
                r#"{"id":"new-answer","type":"gemini","content":"successor-only answer","timestamp":"2026-07-12T10:30:03Z"}"#,
                "\n"
            ),
        )
        .unwrap();
        let (files_seen, _) = reindex(&config, &db, false, None).unwrap();
        assert_eq!(files_seen, 2);
        let identities = db.indexed_source_identities().unwrap();
        assert_eq!(identities.len(), 1, "family has one logical session");
        assert!(
            identities[0].source_path.ends_with(".jsonl"),
            "{identities:?}"
        );
        let session_id = identities[0].session_id.clone();
        let migrated_reference = db
            .search_messages("legacy-only answer", &MessageFilters::default())
            .unwrap();
        assert_eq!(
            (
                migrated_reference[0].session_id.clone(),
                migrated_reference[0].seq
            ),
            old_reference,
            "migration preserves previously published session/message references"
        );
        let user_filters = MessageFilters {
            role: Some(crate::models::Role::User),
            session_id: Some(session_id.clone()),
            ..Default::default()
        };
        assert_eq!(
            db.search_messages("original user input", &user_filters)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_messages(
                "legacy-only answer",
                &MessageFilters {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            db.search_messages(
                "successor-only answer",
                &MessageFilters {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1
        );
        assert!(db
            .search_messages(
                "functionResponse",
                &MessageFilters {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                }
            )
            .unwrap()
            .is_empty());

        let receipts = rusqlite::Connection::open(&db_path).unwrap();
        let receipt_count: i64 = receipts
            .query_row(
                "select count(*) from files_seen where provider='gemini-cli'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            receipt_count, 2,
            "both physical family members are freshened"
        );
        let predecessor_checkpoint: Option<i64> = receipts
            .query_row(
                "select tail_byte_offset from files_seen where provider='gemini-cli' and source_path like '%.json'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            predecessor_checkpoint, None,
            "predecessor never gets a tail checkpoint"
        );
        drop(receipts);

        let trigger_conn = rusqlite::Connection::open(&db_path).unwrap();
        trigger_conn
            .execute_batch(
                "create temp table family_message_updates(n integer);
                 insert into family_message_updates values (0);
                 create temp trigger count_family_message_updates after update on messages begin
                     update family_message_updates set n = n + 1; end;",
            )
            .unwrap();
        assert_eq!(reindex(&config, &db, false, None).unwrap(), (2, 0));
        let unchanged_updates: i64 = trigger_conn
            .query_row("select n from family_message_updates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            unchanged_updates, 0,
            "unchanged family refresh is write-free"
        );

        std::fs::write(
            &legacy,
            r#"{"sessionId":"g-family","kind":"main","messages":[
                {"id":"u1","type":"user","content":"original user input","timestamp":"2026-07-12T10:30:01Z"},
                {"id":"old-answer","type":"gemini","content":"legacy-only answer","timestamp":"2026-07-12T10:30:02Z"},
                {"id":"old-new","type":"user","content":"predecessor appended input","timestamp":"2026-07-12T10:30:04Z"}
            ]}"#,
        )
        .unwrap();
        reindex(&config, &db, false, None).unwrap();
        let appended_filters = MessageFilters {
            role: Some(crate::models::Role::User),
            session_id: Some(session_id.clone()),
            ..Default::default()
        };
        assert_eq!(
            db.search_messages("predecessor appended input", &appended_filters)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.search_messages("original user input", &appended_filters)
                .unwrap()
                .len(),
            1,
            "refresh does not duplicate the shared native input"
        );

        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&successor)
            .unwrap()
            .write_all(
                br#"{"id":"new-successor","type":"gemini","content":"successor appended answer","timestamp":"2026-07-12T10:30:05Z"}
"#,
            )
            .unwrap();
        reindex(&config, &db, false, None).unwrap();
        assert_eq!(
            db.search_messages(
                "successor appended answer",
                &MessageFilters {
                    session_id: Some(session_id.clone()),
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1
        );

        std::fs::remove_file(&successor).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let after_deletion = db.indexed_source_identities().unwrap();
        assert_eq!(after_deletion.len(), 1);
        assert_eq!(
            after_deletion[0].session_id, session_id,
            "logical ID remains stable"
        );
        assert_eq!(
            db.search_messages(
                "successor-only answer",
                &MessageFilters {
                    session_id: Some(after_deletion[0].session_id.clone()),
                    ..Default::default()
                }
            )
            .unwrap()
            .len(),
            1,
            "archived successor rows survive member deletion"
        );
    }

    #[test]
    fn gemini_family_accepts_retained_input_and_keeps_it_after_successor_removal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let root = dir.path().join("gemini");
        let chats = root.join("project").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let legacy = chats.join("session-family.json");
        let successor = chats.join("session-family.jsonl");
        std::fs::write(&legacy, r#"{"sessionId":"retained-family","kind":"main","messages":[{"id":"u1","type":"user","content":"family original α","timestamp":"2026-07-12T10:30:01Z"}]}"#).unwrap();
        std::fs::write(&successor, concat!(
            "{\"sessionId\":\"retained-family\",\"kind\":\"main\"}\n",
            "{\"id\":\"u1\",\"type\":\"user\",\"content\":\"family original α\",\"timestamp\":\"2026-07-12T10:30:01Z\"}\n",
            "{\"$patch\":{\"id\":\"u1\",\"content\":\"generated context β\"}}\n",
        )).unwrap();
        let original_successor = std::fs::read_to_string(&successor).unwrap();
        let mut config = config_with_no_providers(&db_path);
        config.providers.gemini_cli.enabled = true;
        config.providers.gemini_cli.paths = vec![normalize_path(&root)];
        let db = Db::open(&db_path).unwrap();
        let input = crate::models::MessageFilters {
            role: Some(crate::models::Role::User),
            ..Default::default()
        };
        for pass in [0, 1] {
            reindex(&config, &db, false, None).unwrap();
            assert_eq!(
                db.indexed_source_identities().unwrap().len(),
                1,
                "one native family at pass {pass}"
            );
            assert_eq!(
                db.search_messages("family original α", &input)
                    .unwrap()
                    .len(),
                1
            );
            assert!(db
                .search_messages("generated context β", &input)
                .unwrap()
                .is_empty());
            assert_eq!(
                db.search_messages("generated context β", &Default::default())
                    .unwrap()
                    .len(),
                1,
                "previously indexed context survives member removal"
            );
            if pass == 0 {
                std::fs::remove_file(&successor).unwrap();
            }
        }
        // A surviving legacy member may change after the successor disappeared. Its weaker
        // rendering must add new native events without downgrading the already indexed mixed row.
        std::fs::write(
            &legacy,
            r#"{"sessionId":"retained-family","kind":"main","messages":[
                {"id":"u1","type":"user","content":"family revised α","timestamp":"2026-07-12T10:30:01Z"},
                {"id":"u2","type":"user","content":"legacy-only later input","timestamp":"2026-07-12T10:30:02Z"}
            ]}"#,
        )
        .unwrap();
        reindex(&config, &db, false, None).unwrap();
        assert_eq!(
            db.search_messages("generated context β", &Default::default())
                .unwrap()
                .len(),
            1,
            "a changed surviving member cannot downgrade the joined mixed row"
        );
        assert_eq!(
            db.search_messages(
                "legacy-only later input",
                &crate::models::MessageFilters {
                    role: Some(crate::models::Role::User),
                    ..Default::default()
                },
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            db.search_messages(
                "family revised α",
                &crate::models::MessageFilters {
                    role: Some(crate::models::Role::User),
                    ..Default::default()
                },
            )
            .unwrap()
            .len(),
            1
        );
        for pass in 0..2 {
            let mut current = std::fs::read_to_string(&legacy).unwrap();
            current.push(' ');
            std::fs::write(&legacy, current).unwrap();
            reindex(&config, &db, true, None).unwrap();
            assert_eq!(
                db.search_messages("family revised α", &input)
                    .unwrap()
                    .len(),
                1,
                "repeated full refresh must not multiply revised native input at pass {pass}"
            );
        }
        std::fs::write(&successor, &original_successor).unwrap();
        for pass in 0..2 {
            reindex(&config, &db, true, None).unwrap();
            for query in [
                "family original α",
                "family revised α",
                "legacy-only later input",
            ] {
                assert_eq!(
                    db.search_messages(query, &input).unwrap().len(),
                    1,
                    "successor reappearance preserves each input once: {query}, pass {pass}"
                );
            }
            assert_eq!(
                db.search_messages("generated context β", &Default::default())
                    .unwrap()
                    .len(),
                1
            );
        }
        std::fs::write(&successor, format!("{original_successor}{{\"$patch\":{{\"id\":\"u1\",\"content\":\"generated context γ\"}}}}\n")).unwrap();
        reindex(&config, &db, true, None).unwrap();
        for query in [
            "family original α",
            "family revised α",
            "legacy-only later input",
        ] {
            assert_eq!(db.search_messages(query, &input).unwrap().len(), 1);
        }
        for query in ["generated context β", "generated context γ"] {
            assert_eq!(
                db.search_messages(query, &Default::default())
                    .unwrap()
                    .len(),
                1
            );
            assert!(db.search_messages(query, &input).unwrap().is_empty());
        }
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let duplicates: i64 = connection.query_row(
            "select count(*) from (select correlation_authority, correlation_scope, correlation_id
                from messages where correlation_id is not null
                group by correlation_authority, correlation_scope, correlation_id having count(*) > 1)",
            [], |row| row.get(0)).unwrap();
        assert_eq!(duplicates, 0, "each native event has one canonical row");
        connection.execute_batch("create trigger reject_unchanged_family_update before update on messages begin select raise(abort, 'unchanged family event rewritten'); end;").unwrap();
        reindex(&config, &db, true, None).unwrap();
    }

    #[test]
    fn gemini_family_receipt_cannot_rebind_a_changed_native_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let root = dir.path().join("gemini");
        let chats = root.join("project/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let legacy = chats.join("session-family.json");
        let successor = chats.join("session-family.jsonl");
        std::fs::write(&legacy, r#"{"sessionId":"old-native","messages":[{"id":"u1","type":"user","content":"original family input"}]}"#).unwrap();
        std::fs::write(&successor, "{\"sessionId\":\"old-native\"}\n{\"id\":\"u1\",\"type\":\"user\",\"content\":\"original family input\"}\n").unwrap();
        let mut config = config_with_no_providers(&db_path);
        config.providers.gemini_cli.enabled = true;
        config.providers.gemini_cli.paths = vec![normalize_path(&root)];
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        let old_id = db
            .search_messages("original family input", &Default::default())
            .unwrap()[0]
            .session_id
            .clone();
        std::fs::write(&successor, "{\"sessionId\":\"different-native\"}\n{\"id\":\"u1\",\"type\":\"user\",\"content\":\"replacement independent input\"}\n").unwrap();
        let ReindexRun::Completed {
            discovery_warnings, ..
        } = reindex_until(&config, &db, false, None, &|| false).unwrap()
        else {
            panic!("unexpected cancellation")
        };
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let native_id: String = connection
            .query_row(
                "select provider_session_id from sessions where id = ?1",
                [&old_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            native_id, "old-native",
            "a stale family receipt cannot rename the old native session"
        );
        let replacement = db
            .search_messages("replacement independent input", &Default::default())
            .unwrap();
        assert!(replacement.iter().all(|hit| hit.session_id != old_id));
        assert!(
            replacement.len() == 1
                || discovery_warnings
                    .iter()
                    .any(|warning| warning.message.contains("native session ID changed")),
            "a blocked path collision must be observable"
        );
    }

    #[test]
    fn gemini_family_orders_successor_before_forced_reverse_discovery() {
        let predecessor_path = PathBuf::from("/fixture/session.json");
        let successor_path = PathBuf::from("/fixture/session.jsonl");
        let unrelated_path = PathBuf::from("/fixture/other-session.jsonl");
        let source = |path: &Path| SourceFile {
            sidecar: None,
            provider: Provider::GeminiCli,
            path: path.to_path_buf(),
            mtime_ns: 1,
            size_bytes: 1,
        };
        let predecessor = source(&predecessor_path);
        let successor = source(&successor_path);
        let family = GeminiMigrationFamily {
            predecessor: predecessor.clone(),
            successor: successor.clone(),
            predecessor_path: normalize_path(&predecessor_path),
            successor_path: normalize_path(&successor_path),
            native_session_id: "native".into(),
            logical_session_id: "gemini-cli:fixture".into(),
            requires_reparse: true,
        };
        let mut families = HashMap::new();
        families.insert(family.predecessor_path.clone(), family.clone());
        families.insert(family.successor_path.clone(), family);
        let unrelated = source(&unrelated_path);
        for mut sources in [
            vec![predecessor.clone(), unrelated.clone(), successor.clone()],
            vec![successor.clone(), unrelated.clone(), predecessor.clone()],
        ] {
            order_gemini_family_successors_first(&mut sources, &families);
            assert_eq!(
                sources
                    .iter()
                    .map(|source| source.path.clone())
                    .collect::<Vec<_>>(),
                vec![
                    successor_path.clone(),
                    unrelated_path.clone(),
                    predecessor_path.clone()
                ]
            );
        }
    }

    #[test]
    fn gemini_family_receipts_roll_back_together_on_sql_failure() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let root = dir.path().join("gemini");
        let chats = root.join("project").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let legacy = chats.join("session-rollback.json");
        let successor = chats.join("session-rollback.jsonl");
        std::fs::write(
            &legacy,
            r#"{"sessionId":"rollback-family","kind":"main","messages":[{"id":"u1","type":"user","content":"legacy"}]}"#,
        )
        .unwrap();
        let mut config = config_with_no_providers(&db_path);
        config.providers.gemini_cli.enabled = true;
        config.providers.gemini_cli.paths = vec![root.to_string_lossy().to_string()];
        let db = Db::open(&db_path).unwrap();
        reindex(&config, &db, false, None).unwrap();
        std::fs::write(
            &successor,
            concat!(
                r#"{"sessionId":"rollback-family","kind":"main"}"#,
                "\n",
                r#"{"id":"u1","type":"user","content":"legacy"}"#,
                "\n",
            ),
        )
        .unwrap();
        let fixture = rusqlite::Connection::open(&db_path).unwrap();
        fixture
            .execute_batch(
                "create trigger fail_gemini_family_receipt before insert on files_seen
                 when new.source_path like '%.jsonl'
                 begin select raise(abort, 'fixture family receipt failure'); end;",
            )
            .unwrap();
        assert!(reindex(&config, &db, false, None).is_err());
        let identity = db.indexed_source_identities().unwrap();
        assert_eq!(identity.len(), 1);
        assert!(identity[0].source_path.ends_with(".json"), "{identity:?}");
        let receipt_count: i64 = fixture
            .query_row(
                "select count(*) from files_seen where provider = 'gemini-cli'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            receipt_count, 1,
            "failed family publish leaves no half receipt"
        );
        fixture
            .execute_batch("drop trigger fail_gemini_family_receipt")
            .unwrap();
        reindex(&config, &db, false, None).unwrap();
        assert_eq!(db.indexed_source_identities().unwrap().len(), 1);
    }

    #[test]
    fn gemini_family_cancellation_between_members_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let predecessor_path = dir.path().join("session-cancel.json");
        let successor_path = dir.path().join("session-cancel.jsonl");
        std::fs::write(
            &predecessor_path,
            r#"{"sessionId":"cancel-family","kind":"main","messages":[{"id":"u1","type":"user","content":"legacy"}]}"#,
        )
        .unwrap();
        std::fs::write(
            &successor_path,
            concat!(
                r#"{"sessionId":"cancel-family","kind":"main"}"#,
                "\n",
                r#"{"id":"u1","type":"user","content":"legacy"}"#,
                "\n",
            ),
        )
        .unwrap();
        let source = |path: &Path| {
            let metadata = std::fs::metadata(path).unwrap();
            SourceFile {
                sidecar: None,
                provider: Provider::GeminiCli,
                path: path.to_path_buf(),
                mtime_ns: 1,
                size_bytes: metadata.len() as i64,
            }
        };
        let family = GeminiMigrationFamily {
            predecessor: source(&predecessor_path),
            successor: source(&successor_path),
            predecessor_path: normalize_path(&predecessor_path),
            successor_path: normalize_path(&successor_path),
            native_session_id: "cancel-family".into(),
            logical_session_id: "gemini-cli:cancel-family".into(),
            requires_reparse: true,
        };
        let config = config_with_no_providers(&db_path);
        let adapters = ProviderSet::new(&config);
        let checks = std::cell::Cell::new(0usize);
        let cancelled = parse_gemini_migration_family(&adapters, &family, &|| {
            let next = checks.get() + 1;
            checks.set(next);
            next >= 4
        })
        .unwrap();
        assert!(matches!(cancelled, GeminiFamilyParse::Cancelled));
        assert!(checks.get() >= 4);
    }

    #[test]
    fn gemini_family_proof_failures_keep_files_independently_indexable() {
        for (label, legacy_json, successor_jsonl) in [
            (
                "conflicting",
                r#"{"sessionId":"g-one","messages":[{"id":"u1","type":"user","content":"legacy"}]}"#,
                "{\"sessionId\":\"g-two\"}\n{\"id\":\"u2\",\"type\":\"user\",\"content\":\"successor\"}\n",
            ),
            (
                "missing",
                r#"{"messages":[{"id":"u1","type":"user","content":"legacy"}]}"#,
                "{\"id\":\"u2\",\"type\":\"user\",\"content\":\"successor\"}\n",
            ),
            (
                "malformed",
                r#"{"sessionId":"g-one","messages":[{"id":"u1","type":"user","content":"legacy"}]}"#,
                "not json\n",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("index.db");
            let root = dir.path().join("gemini");
            let chats = root.join("project-hash").join("chats");
            std::fs::create_dir_all(&chats).unwrap();
            std::fs::write(
                chats.join(format!("session-{label}.json")),
                legacy_json,
            )
            .unwrap();
            std::fs::write(
                chats.join(format!("session-{label}.jsonl")),
                successor_jsonl,
            )
            .unwrap();
            let mut config = config_with_no_providers(&db_path);
            config.providers.gemini_cli.enabled = true;
            config.providers.gemini_cli.paths = vec![root.to_string_lossy().to_string()];
            let db = Db::open(&db_path).unwrap();
            let run = reindex_until(&config, &db, false, None, &|| false).unwrap();
            assert_eq!(
                db.indexed_source_identities().unwrap().len(),
                2,
                "{label} proof failure must not merge independent sources"
            );
            let discovery_warnings = match run {
                ReindexRun::Completed {
                    discovery_warnings, ..
                } => discovery_warnings,
                ReindexRun::Cancelled { .. } => panic!("test refresh unexpectedly cancelled"),
            };
            assert!(
                discovery_warnings
                    .iter()
                    .any(|warning| warning.operation == "migration_family_proof"),
                "{label} warning missing: {:?}",
                discovery_warnings
            );
        }
    }

    /// `reindex(full = true)` must not delete a session whose source file is gone.
    ///
    /// Agent harnesses clear old sessions on their own schedule, so the index is routinely the
    /// only surviving copy of that conversation. Rebuilding solely from the filesystem
    /// would destroy archived history when the harness has pruned its source transcripts.
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

    /// Codex `thread/revert` keeps the thread ID and starts a new rollout file named
    /// `rollout-<time>-<thread>_<rollout>.jsonl`, whose `history_base` inherits a prefix of the
    /// previous rollout. Every rollout names the thread in `session_meta.id`, so their parses are
    /// reconciled into one session, which keeps rows a later parse no longer holds. Doctor counted
    /// every rollout but the session's source path as unindexed, said its content was absent
    /// from search, and prescribed an `aise reindex` that could never clear the count.
    #[test]
    fn a_reverted_codex_thread_is_one_session_and_none_of_its_rollouts_is_unindexed() {
        use crate::models::MessageFilters;
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex/2026/10/14");
        std::fs::create_dir_all(&root).unwrap();
        let thread = "01a13899-5fe0-7651-a51f-0b42469588e6";
        let revert = "01a1389c-f5fc-76b3-ba16-9ae7e18559d6";
        // `aise` reads neither `history_mode` nor `history_base`; they make the fixture match the
        // rollouts Codex writes.
        let meta = |base: Option<&str>| {
            let mut meta = serde_json::json!({"id":thread,"session_id":thread,"cwd":"/tmp/reverted","history_mode":"paginated"});
            if let Some(rollout) = base {
                meta["history_base"] = serde_json::json!({"thread_id":rollout,"end_ordinal_exclusive":2,"end_byte_offset":100});
            }
            meta
        };
        let user_line = |prompt: &str| {
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":prompt}]}}).to_string() + "\n"
        };
        let rollout = |meta: serde_json::Value, prompts: &[&str]| {
            serde_json::json!({"type":"session_meta","payload":meta}).to_string()
                + "\n"
                + &prompts
                    .iter()
                    .map(|prompt| user_line(prompt))
                    .collect::<String>()
        };
        std::fs::write(
            root.join(format!("rollout-2026-10-14T12-08-58-{thread}.jsonl")),
            rollout(
                meta(None),
                &["original rollout prompt", "reverted away prompt"],
            ),
        )
        .unwrap();
        let second = root.join(format!(
            "rollout-2026-10-14T12-12-53-{thread}_{revert}.jsonl"
        ));

        let db_path = dir.path().join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.providers.codex.enabled = true;
        config.providers.codex.paths = vec![normalize_path(&dir.path().join("codex"))];
        let db = Db::open(&db_path).unwrap();
        let session_id = format!("codex:{thread}");
        let prompts_in_order = |db: &Db| {
            let mut hits = db
                .search_messages(
                    "prompt",
                    &MessageFilters {
                        session_id: Some(session_id.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
            hits.sort_by_key(|hit| hit.seq);
            hits.into_iter().map(|hit| hit.content).collect::<Vec<_>>()
        };
        let unindexed = |db: &Db| {
            crate::diagnostics::collect(&config, db)
                .unwrap()
                .index_status
                .unindexed_files
        };

        // Codex writes the revert's rollout after the original was indexed, and a later full
        // reindex reads both; either way the thread is one session in time order. A reverted
        // turn is still work the user did, so it stays searchable.
        reindex(&config, &db, false, None).unwrap();
        // Long enough for the incremental tail parse, which appends below exercise.
        let padding = serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x".repeat(2 * crate::tail::OVERLAP_BYTES as usize)}]}}).to_string() + "\n";
        std::fs::write(
            &second,
            rollout(meta(Some(thread)), &["second rollout prompt"]) + &padding,
        )
        .unwrap();
        assert_eq!(unindexed(&db), 1, "a rollout not yet read is pending");
        for full in [false, true] {
            reindex(&config, &db, full, None).unwrap();
            assert_eq!(
                prompts_in_order(&db),
                [
                    "original rollout prompt",
                    "reverted away prompt",
                    "second rollout prompt"
                ],
                "full={full}"
            );
            assert_eq!(unindexed(&db), 0, "full={full}");
            assert!(crate::diagnostics::explain_unindexed(&config, &db)
                .unwrap()
                .is_empty());
        }

        // Codex appends to the newest rollout, and a second revert adds a third file.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&second)
            .unwrap()
            .write_all(user_line("appended prompt").as_bytes())
            .unwrap();
        TEST_TAIL_OUTCOMES.with(|outcomes| outcomes.borrow_mut().clear());
        reindex(&config, &db, false, None).unwrap();
        assert!(
            TEST_TAIL_OUTCOMES
                .with(|outcomes| matches!(outcomes.borrow().as_slice(), [TailOutcome::Appended])),
            "an append to a revert rollout is read as a tail, not by rereading the file"
        );
        let third = "01a138a8-1dee-7c11-90fc-42dc004cffa2";
        std::fs::write(
            root.join(format!(
                "rollout-2026-10-14T12-25-04-{thread}_{third}.jsonl"
            )),
            rollout(meta(Some(revert)), &["third rollout prompt"]),
        )
        .unwrap();
        assert_eq!(unindexed(&db), 1, "a rollout not yet read is pending");
        reindex(&config, &db, false, None).unwrap();
        assert_eq!(
            prompts_in_order(&db),
            [
                "original rollout prompt",
                "reverted away prompt",
                "second rollout prompt",
                "appended prompt",
                "third rollout prompt"
            ]
        );
        assert_eq!(unindexed(&db), 0);
        assert_eq!(
            reindex(&config, &db, false, None).unwrap().1,
            0,
            "a current thread is skipped"
        );
    }

    /// One Codex rollout: a `session_meta` line naming `id`, then one user message per prompt.
    fn codex_rollout_text(id: &str, prompts: &[&str]) -> String {
        let mut text =
            serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":"/tmp/codex"}})
                .to_string()
                + "\n";
        for prompt in prompts {
            text += &serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":prompt}]}})
                .to_string();
            text += "\n";
        }
        text
    }

    /// A config reading only `<dir>/codex`, and the index it writes to `<dir>/index.db`.
    fn codex_config_and_db(dir: &std::path::Path) -> (Config, Db) {
        let db_path = dir.join("index.db");
        let mut config = config_with_no_providers(&db_path);
        config.providers.codex.enabled = true;
        config.providers.codex.paths = vec![normalize_path(&dir.join("codex"))];
        (config, Db::open(&db_path).unwrap())
    }

    /// A file named for no thread, or for a name whose trailing 36 bytes cut a multi-byte
    /// character, sits among rollouts because the Codex walk takes any `*.jsonl`. Reading a thread
    /// from its name must give such a file no thread, not panic the whole reindex.
    #[test]
    fn a_codex_file_with_a_non_ascii_name_does_not_stop_the_reindex() {
        use crate::models::MessageFilters;

        let thread = "01a13899-5fe0-7651-a51f-0b42469588e6";
        let stray = [
            // 36 bytes of 3-byte characters, so byte 8 of the trailing 36 is inside a character.
            "\u{4f1a}\u{8a71}\u{30ed}\u{30b0}\u{4f1a}\u{8a71}\u{30ed}\u{30b0}\u{4f1a}\u{8a71}\u{30ed}\u{30b0}.jsonl".to_string(),
            format!("rollout-abcdefg\u{e9}{}.jsonl", "a".repeat(27)),
        ];
        for (name, content) in stray.iter().zip([
            codex_rollout_text("stray-thread", &["stray prompt"]),
            "not json at all\n".to_string(),
        ]) {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("codex/2026/10/14");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join(format!("rollout-2026-10-14T12-08-58-{thread}.jsonl")),
                codex_rollout_text(thread, &["normal prompt"]),
            )
            .unwrap();
            std::fs::write(root.join(name), content).unwrap();
            let (config, db) = codex_config_and_db(dir.path());

            reindex(&config, &db, false, None).unwrap_or_else(|error| panic!("{name}: {error:#}"));
            let hits = db
                .search_messages(
                    "prompt",
                    &MessageFilters {
                        session_id: Some(format!("codex:{thread}")),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(hits.len(), 1, "{name}");
            crate::diagnostics::collect(&config, &db).unwrap();
            crate::diagnostics::explain_unindexed(&config, &db).unwrap();
        }
    }

    /// Two rollouts named for different threads whose `session_meta.id` is one ID are a real ID
    /// collision, not two rollouts of one thread: the loser's content is in no session.
    #[test]
    fn two_codex_files_of_different_threads_claiming_one_id_still_collide() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex/2026/10/14");
        std::fs::create_dir_all(&root).unwrap();
        let claimed = "01a13899-5fe0-7651-a51f-0b42469588e6";
        let first =
            root.join("rollout-2026-10-14T12-08-58-01a1389c-f5fc-76b3-ba16-9ae7e18559d6.jsonl");
        let second =
            root.join("rollout-2026-10-14T12-12-53-01a138a8-1dee-7c11-90fc-42dc004cffa2.jsonl");
        std::fs::write(&first, codex_rollout_text(claimed, &["first prompt"])).unwrap();
        std::fs::write(&second, codex_rollout_text(claimed, &["second prompt"])).unwrap();
        let (config, db) = codex_config_and_db(dir.path());

        reindex(&config, &db, false, None).unwrap();
        reindex(&config, &db, false, None).unwrap();

        let unindexed = crate::diagnostics::explain_unindexed(&config, &db).unwrap();
        assert_eq!(unindexed.len(), 1, "{unindexed:?}");
        assert_eq!(
            crate::diagnostics::collect(&config, &db)
                .unwrap()
                .index_status
                .unindexed_files,
            1
        );
        assert_eq!(unindexed[0].resolves_to, claimed);
        let holder = unindexed[0].id_already_held_by.as_deref().unwrap();
        assert_ne!(Path::new(holder), Path::new(&unindexed[0].path));
    }

    /// Prompts of `session_id` in sequence order.
    fn codex_prompts(db: &Db, session_id: &str) -> Vec<String> {
        use crate::models::MessageFilters;
        let mut hits = db
            .search_messages(
                "prompt",
                &MessageFilters {
                    session_id: Some(format!("codex:{session_id}")),
                    ..Default::default()
                },
            )
            .unwrap();
        hits.sort_by_key(|hit| hit.seq);
        hits.into_iter().map(|hit| hit.content).collect()
    }

    /// A rollout named `<T>_<R>` whose `session_meta.id` is Q is read as a rollout of T by name,
    /// and doctor hides a rollout of a held thread. Its content must still be searchable: it is
    /// indexed under Q, and stays there after another file of Q arrives and takes over the ID.
    #[test]
    fn a_codex_file_named_for_one_thread_and_holding_another_is_indexed_under_the_one_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex/2026/10/14");
        std::fs::create_dir_all(&root).unwrap();
        let named = "01a13899-5fe0-7651-a51f-0b42469588e6";
        let held = "01a138a8-1dee-7c11-90fc-42dc004cffa2";
        let revert = "01a1389c-f5fc-76b3-ba16-9ae7e18559d6";
        std::fs::write(
            root.join(format!("rollout-2026-10-14T12-08-58-{named}.jsonl")),
            codex_rollout_text(named, &["genuine prompt"]),
        )
        .unwrap();
        std::fs::write(
            root.join(format!(
                "rollout-2026-10-14T12-12-53-{named}_{revert}.jsonl"
            )),
            codex_rollout_text(held, &["misnamed prompt"]),
        )
        .unwrap();
        let (config, db) = codex_config_and_db(dir.path());
        let unindexed = |db: &Db| {
            crate::diagnostics::collect(&config, db)
                .unwrap()
                .index_status
                .unindexed_files
        };

        reindex(&config, &db, false, None).unwrap();
        assert_eq!(codex_prompts(&db, named), ["genuine prompt"]);
        assert_eq!(codex_prompts(&db, held), ["misnamed prompt"]);
        assert_eq!(unindexed(&db), 0);
        assert!(crate::diagnostics::explain_unindexed(&config, &db)
            .unwrap()
            .is_empty());

        // A file named for Q arrives and takes over Q. The misnamed file is no longer Q's source
        // path, so only its receipt marks it as read, and its content must still be in Q.
        std::fs::write(
            root.join(format!("rollout-2026-10-14T12-20-00-{held}.jsonl")),
            codex_rollout_text(held, &["holder prompt"]),
        )
        .unwrap();
        for full in [false, false, true] {
            reindex(&config, &db, full, None).unwrap();
            let mut prompts = codex_prompts(&db, held);
            prompts.sort();
            assert_eq!(prompts, ["holder prompt", "misnamed prompt"], "full={full}");
            assert_eq!(codex_prompts(&db, named), ["genuine prompt"], "full={full}");
            assert_eq!(unindexed(&db), 0, "full={full}");
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
            |reader, path, context| {
                adapters
                    .claude
                    .parse_reader_with_context(reader, path, Some(context))
            },
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
        std::fs::remove_file(index_update_lock_path(&db_path)).unwrap();
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
        std::fs::remove_file(index_update_lock_path(&db_path)).unwrap();
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
        std::fs::remove_file(index_update_lock_path(&db_path)).unwrap();
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
        std::fs::remove_file(index_update_lock_path(&db_path)).unwrap();
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
        std::fs::remove_file(index_update_lock_path(&db_path)).unwrap();
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
        std::fs::remove_file(index_update_lock_path(&db_path)).unwrap();
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
                sidecar: None,
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
        let line = |id: &str, role: &str, content: &str| {
            format!(
                "{{\"uuid\":\"{id}\",\"sessionId\":\"s1\",\"type\":\"{role}\",\
                 \"message\":{{\"role\":\"{role}\",\"content\":\"{content}\"}}}}\n"
            )
        };
        std::fs::write(&session_file, line("event-1", "user", "first prompt")).unwrap();

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
            "update messages set tool_name = 'STALE_PREFIX' where session_id = 'claude:s1'",
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
                None,
            )
            .unwrap(),
            SourceFileRefreshState::ChangedParser,
        );

        OpenOptions::new()
            .append(true)
            .open(&session_file)
            .unwrap()
            .write_all(line("event-2", "assistant", "second reply").as_bytes())
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
            |reader, path, context| {
                adapters
                    .claude
                    .parse_reader_with_context(reader, path, Some(context))
            },
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
                None,
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
        let tool: Option<String> = conn
            .query_row(
                "select tool_name from messages where session_id='claude:s1' and seq=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            tool, None,
            "the full parse must correct stale prefix metadata"
        );
    }

    fn reindex_tail_outcome(config: &Config, db: &Db, expected: TailOutcome) {
        TEST_TAIL_OUTCOMES.with(|outcomes| outcomes.borrow_mut().clear());
        reindex(config, db, false, None).unwrap();
        TEST_TAIL_OUTCOMES.with(|outcomes| assert_eq!(*outcomes.borrow(), vec![expected]));
    }

    fn checkpoint_record(role: &str, content: serde_json::Value) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type":role,"sessionId":"byte-window",
            "timestamp":"2026-01-01T00:00:00Z","message":{"role":role,"content":content}})
        )
    }

    fn checkpoint_rows(path: &std::path::Path) -> Vec<Vec<String>> {
        let conn = rusqlite::Connection::open(path).unwrap();
        ["select json_array(role,kind,ts,tool_name,content) from messages order by 1",
         "select json_array(m.content,p.ordinal,p.start_char,p.end_char,p.authorship,p.origin) from message_content_parts p join messages m on m.id=p.message_id order by 1",
         "select json_array(ts,tool,file_path,new_content,edits_json) from file_edits order by 1"]
        .iter().map(|sql| conn.prepare(sql).unwrap().query_map([], |r| r.get(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap()).collect()
    }

    fn assert_checkpoint_fresh(
        dir: &std::path::Path,
        root: &std::path::Path,
        db_path: &std::path::Path,
        n: usize,
    ) {
        let fresh_path = dir.join(format!("fresh-{n}.db"));
        let fresh = Db::open(&fresh_path).unwrap();
        reindex(
            &config_with_single_claude_fixture(&fresh_path, root),
            &fresh,
            false,
            None,
        )
        .unwrap();
        let actual = checkpoint_rows(db_path);
        let expected = checkpoint_rows(&fresh_path);
        for (table, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(actual.len(), expected.len(), "table {table}");
            for (actual, expected) in actual.iter().zip(expected) {
                assert_eq!(
                    crate::hashing::sha256(actual.as_bytes()),
                    crate::hashing::sha256(expected.as_bytes()),
                    "table {table}: {actual:.250} != {expected:.250}"
                );
            }
        }
    }

    #[test]
    fn overlap_start_between_tool_call_and_result_keeps_tail_path() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("session.jsonl");
        let call = checkpoint_record(
            "assistant",
            serde_json::json!([{"type":"tool_use","id":"read","name":"Read","input":{}}]),
        );
        let result = checkpoint_record(
            "user",
            serde_json::json!([{"type":"tool_result","tool_use_id":"read","content":"synthetic result"}]),
        );
        let mut body =
            checkpoint_record("assistant", serde_json::json!("x".repeat(5000))) + &call + &result;
        // Place the overlap start inside the call: the result's original tool name is unavailable.
        let padding_len = crate::tail::OVERLAP_BYTES as usize - result.len() - call.len() / 2;
        let padding = format!("{}\n", " ".repeat(padding_len - 1));
        body.push_str(&padding);
        std::fs::write(&path, body).unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        let config = config_with_single_claude_fixture(&db_path, &root);
        reindex(&config, &db, false, None).unwrap();
        for i in 0..3 {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(
                    checkpoint_record("assistant", serde_json::json!(format!("append {i}")))
                        .as_bytes(),
                )
                .unwrap();
            reindex_tail_outcome(&config, &db, TailOutcome::Appended);
            assert_checkpoint_fresh(dir.path(), &root, &db_path, i);
        }
    }

    #[test]
    fn overlap_timestamp_rewrite_matches_fresh_index() {
        checkpoint_rewrite_matches_fresh("timestamp");
    }
    #[test]
    fn overlap_content_parts_rewrite_matches_fresh_index() {
        checkpoint_rewrite_matches_fresh("parts");
    }
    #[test]
    fn overlap_result_error_rewrite_matches_fresh_index() {
        checkpoint_rewrite_matches_fresh("error");
    }

    fn checkpoint_rewrite_matches_fresh(kind: &str) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("session.jsonl");
        let head = checkpoint_record("assistant", serde_json::json!("x".repeat(5000)));
        let call = checkpoint_record(
            "assistant",
            serde_json::json!([{"type":"tool_use","id":"write","name":"Write","input":{"file_path":"/synthetic/file.rs","content":"synthetic"}}]),
        );
        let result = checkpoint_record(
            "user",
            serde_json::json!([{"type":"tool_result","tool_use_id":"write","is_error":true,"content":"output"}]),
        );
        let mixed = checkpoint_record(
            "user",
            serde_json::json!([{"type":"text","text":"left"},{"type":"tool_result","tool_use_id":"write","content":"output"},{"type":"text","text":"right"}]),
        );
        let original = head + &call + &result + &mixed;
        std::fs::write(&path, &original).unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        let config = config_with_single_claude_fixture(&db_path, &root);
        reindex(&config, &db, false, None).unwrap();
        let replacement = match kind {
            "timestamp" => original.replace(&mixed, &mixed.replace("00:00:00Z", "00:00:01Z")),
            "parts" => original.replace(&mixed, &checkpoint_record("user", serde_json::json!([{"type":"text","text":"left\noutput"},{"type":"tool_result","tool_use_id":"write","content":"right"}]))),
            "error" => original.replace("\"is_error\":true", "\"is_error\":false"),
            _ => unreachable!(),
        };
        std::fs::write(
            &path,
            replacement + &checkpoint_record("assistant", serde_json::json!("appended")),
        )
        .unwrap();
        reindex(&config, &db, false, None).unwrap();
        assert_checkpoint_fresh(dir.path(), &root, &db_path, 0);
    }

    #[test]
    fn missing_overlap_digest_full_parses_once_then_resumes_tail() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("session.jsonl");
        std::fs::write(
            &path,
            checkpoint_record("assistant", serde_json::json!("x".repeat(5000))),
        )
        .unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        let config = config_with_single_claude_fixture(&db_path, &root);
        reindex(&config, &db, false, None).unwrap();
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute("update files_seen set overlap_digest = null", [])
            .unwrap();
        for expected in [TailOutcome::FullParse, TailOutcome::Appended] {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(checkpoint_record("assistant", serde_json::json!("append")).as_bytes())
                .unwrap();
            reindex_tail_outcome(&config, &db, expected);
        }
    }

    #[test]
    fn rewrite_between_head_and_overlap_is_not_detected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("session.jsonl");
        let head = checkpoint_record("assistant", serde_json::json!("x".repeat(5000)));
        let original = head
            + &checkpoint_record("user", serde_json::json!("original middle"))
            + &checkpoint_record(
                "assistant",
                serde_json::json!("y".repeat(crate::tail::OVERLAP_BYTES as usize + 100)),
            );
        std::fs::write(&path, &original).unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        let config = config_with_single_claude_fixture(&db_path, &root);
        reindex(&config, &db, false, None).unwrap();
        std::fs::write(
            &path,
            original.replace("original middle", "rewritten middl")
                + &checkpoint_record("assistant", serde_json::json!("append")),
        )
        .unwrap();
        reindex_tail_outcome(&config, &db, TailOutcome::Appended);
        assert_eq!(
            db.search_messages("original middle", &Default::default())
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .search_messages("rewritten middl", &Default::default())
            .unwrap()
            .is_empty());
    }
}
