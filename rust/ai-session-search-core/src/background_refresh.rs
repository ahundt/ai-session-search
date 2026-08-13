// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Bounded, best-effort observability for implicit background index maintenance.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::durable_fs::{atomic_write_file, open_existing_file_lock, AtomicWriteMode};
use crate::indexer::BackgroundRefreshOutcome;
use crate::models::{
    IndexReadinessStatus, IndexRefreshState, IndexRefreshStatus, IndexRefreshTrigger,
    IndexSnapshotAvailability, IndexSnapshotStatus,
};
use crate::service::SessionSearch;

const REPORT_FILE_NAME: &str = "background-refresh-status.json";
const MAX_REPORT_BYTES: u64 = 64 * 1024;
const MAX_ERROR_CHARS: usize = 4_096;
const PROGRESS_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackgroundRefreshOrigin {
    IntegrationInstall,
    Cli,
    Mcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BackgroundRefreshState {
    Running,
    Updated,
    SkippedFresh,
    SkippedBusy,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BackgroundRefreshReport {
    #[serde(default)]
    database_path: Option<String>,
    origin: BackgroundRefreshOrigin,
    state: BackgroundRefreshState,
    started_at: chrono::DateTime<Utc>,
    finished_at: Option<chrono::DateTime<Utc>>,
    process_id: u32,
    schema_generation_before: Option<i64>,
    schema_generation_after: Option<i64>,
    #[serde(alias = "files_seen")]
    files_discovered: Option<usize>,
    files_processed: Option<usize>,
    sessions_updated: Option<usize>,
    error: Option<String>,
}

pub(crate) fn run(
    config: &Config,
    origin: BackgroundRefreshOrigin,
    should_cancel: &dyn Fn() -> bool,
) -> Result<BackgroundRefreshOutcome> {
    let started_at = Utc::now();
    let mut report = BackgroundRefreshReport {
        database_path: Some(crate::util::normalize_path(&config.db_path())),
        origin,
        state: BackgroundRefreshState::Running,
        started_at,
        finished_at: None,
        process_id: std::process::id(),
        schema_generation_before: None,
        schema_generation_after: None,
        files_discovered: None,
        files_processed: None,
        sessions_updated: None,
        error: None,
    };
    let mut elected = false;

    let result = (|| {
        let app = SessionSearch::open(config.clone()).context("could not open the index")?;
        report.schema_generation_before = Some(app.database().schema_version()?);
        let elected_report = report.clone();
        let mut record_election = || {
            elected = true;
            write_best_effort(config, &elected_report);
        };
        let mut last_progress_write = None;
        let mut record_progress = |files_processed, files_discovered, sessions_updated| {
            report.files_discovered = Some(files_discovered);
            report.files_processed = Some(files_processed);
            report.sessions_updated = Some(sessions_updated);
            let now = std::time::Instant::now();
            if last_progress_write
                .is_none_or(|last| now.duration_since(last) >= PROGRESS_REPORT_INTERVAL)
            {
                write_best_effort(config, &report);
                last_progress_write = Some(now);
            }
        };
        let outcome = crate::indexer::refresh_usable_index_nonblocking(
            config,
            app.database(),
            should_cancel,
            Some(&mut record_election),
            Some(&mut record_progress),
        )?;
        report.schema_generation_after = Some(app.database().schema_version()?);
        Ok(outcome)
    })();

    report.finished_at = Some(Utc::now());
    match &result {
        Ok(BackgroundRefreshOutcome::Updated {
            files_seen,
            sessions_updated,
        }) => {
            report.state = BackgroundRefreshState::Updated;
            report.files_discovered = Some(*files_seen);
            report.files_processed = Some(*files_seen);
            report.sessions_updated = Some(*sessions_updated);
        }
        Ok(BackgroundRefreshOutcome::SkippedFresh) => {
            report.state = BackgroundRefreshState::SkippedFresh;
        }
        Ok(BackgroundRefreshOutcome::SkippedBusy) => {
            report.state = BackgroundRefreshState::SkippedBusy;
        }
        Ok(BackgroundRefreshOutcome::Cancelled) => {
            report.state = BackgroundRefreshState::Cancelled;
        }
        Err(error) => {
            report.state = BackgroundRefreshState::Failed;
            report.error = Some(bounded_error(&format!("{error:#}")));
        }
    }
    // A contender that never acquired the update permit owns no durable receipt. Publishing its
    // transient lock loss could overwrite the elected writer's later progress or success.
    if elected || !matches!(result, Ok(BackgroundRefreshOutcome::SkippedBusy)) {
        write_best_effort(config, &report);
    }
    result
}

/// Return bounded durable readiness without discovery or indexing work.
///
/// Runtime is `O(1)` database metadata reads plus a status file capped by
/// [`MAX_REPORT_BYTES`]. Snapshot availability and refresh activity remain orthogonal so a stale
/// but compatible snapshot is never mistaken for either fresh data or no data.
pub(crate) fn readiness_status(
    config: &Config,
    db: &crate::db::Db,
) -> Result<IndexReadinessStatus> {
    let last_successful_refresh_at = db.auto_reindex_completed_at()?;
    let snapshot = IndexSnapshotStatus {
        availability: if db.has_sessions()? || last_successful_refresh_at.is_some() {
            IndexSnapshotAvailability::Usable
        } else {
            IndexSnapshotAvailability::Unavailable
        },
        last_successful_refresh_at,
    };
    let report = match load_from_path(&report_path(config)) {
        Ok(report) => report,
        Err(error) => {
            return Ok(IndexReadinessStatus {
                snapshot,
                refresh: IndexRefreshStatus {
                    state: IndexRefreshState::FailedWithRecovery,
                    started_by: None,
                    started_at: None,
                    finished_at: None,
                    files_discovered: None,
                    files_processed: None,
                    sessions_updated: None,
                    retry_after_ms: None,
                    message: Some(bounded_error(&format!(
                        "Cannot read automatic index-update status: {error:#}"
                    ))),
                    next_command: Some("aise reindex".to_string()),
                },
            });
        }
    };
    let expected_database_path = crate::util::normalize_path(&config.db_path());
    let refresh = match report
        .filter(|report| report.database_path.as_deref() == Some(expected_database_path.as_str()))
    {
        None if snapshot.availability == IndexSnapshotAvailability::Unavailable
            && update_lock_is_held(config)? =>
        {
            unreported_writer_status()
        }
        None => idle_refresh_status(snapshot.last_successful_refresh_at.is_some()),
        Some(report) => {
            let running_lock_is_held =
                report.state == BackgroundRefreshState::Running && update_lock_is_held(config)?;
            let report_event_at = report.finished_at.unwrap_or(report.started_at);
            // The database success marker and report file publish independently. A newer success
            // makes an older report historical unless its writer still owns the update lock.
            let superseded_by_success = snapshot
                .last_successful_refresh_at
                .is_some_and(|completed_at| completed_at > report_event_at)
                && !running_lock_is_held;
            if superseded_by_success {
                idle_refresh_status(true)
            } else {
                refresh_status_from_report(report, running_lock_is_held)
            }
        }
    };
    Ok(IndexReadinessStatus { snapshot, refresh })
}

fn unreported_writer_status() -> IndexRefreshStatus {
    IndexRefreshStatus {
        state: IndexRefreshState::Postponed,
        started_by: None,
        started_at: None,
        finished_at: None,
        files_discovered: None,
        files_processed: None,
        sessions_updated: None,
        retry_after_ms: Some(1_000),
        message: Some(
            "Another process owns the index writer; check readiness again after it completes."
                .to_string(),
        ),
        next_command: None,
    }
}

fn idle_refresh_status(has_successful_snapshot: bool) -> IndexRefreshStatus {
    IndexRefreshStatus {
        state: if has_successful_snapshot {
            IndexRefreshState::Fresh
        } else {
            IndexRefreshState::NotStarted
        },
        started_by: None,
        started_at: None,
        finished_at: None,
        files_discovered: None,
        files_processed: None,
        sessions_updated: None,
        retry_after_ms: None,
        message: None,
        next_command: None,
    }
}

fn refresh_status_from_report(
    report: BackgroundRefreshReport,
    running_lock_is_held: bool,
) -> IndexRefreshStatus {
    let started_by = Some(match report.origin {
        BackgroundRefreshOrigin::IntegrationInstall => IndexRefreshTrigger::IntegrationInstall,
        BackgroundRefreshOrigin::Cli => IndexRefreshTrigger::CommandLine,
        BackgroundRefreshOrigin::Mcp => IndexRefreshTrigger::Mcp,
    });
    let (state, retry_after_ms, message, next_command) = match report.state {
        BackgroundRefreshState::Running if running_lock_is_held => (
            IndexRefreshState::Indexing,
            Some(1_000),
            Some("Session history indexing is running.".to_string()),
            None,
        ),
        BackgroundRefreshState::Running => (
            IndexRefreshState::Postponed,
            Some(1_000),
            Some(
                "The recorded index update is no longer running; the next MCP start will retry."
                    .to_string(),
            ),
            Some("aise reindex".to_string()),
        ),
        BackgroundRefreshState::Updated | BackgroundRefreshState::SkippedFresh => {
            (IndexRefreshState::Fresh, None, None, None)
        }
        BackgroundRefreshState::SkippedBusy => (
            IndexRefreshState::Postponed,
            Some(1_000),
            Some(
                "Another process owns the index writer; automatic refresh will retry.".to_string(),
            ),
            None,
        ),
        BackgroundRefreshState::Cancelled => (
            IndexRefreshState::Postponed,
            Some(1_000),
            Some("The index update was cancelled before completion.".to_string()),
            Some("aise reindex".to_string()),
        ),
        BackgroundRefreshState::Failed => (
            IndexRefreshState::FailedWithRecovery,
            None,
            Some(failed_refresh_message(report.error.as_deref())),
            Some("aise reindex --full".to_string()),
        ),
    };
    IndexRefreshStatus {
        state,
        started_by,
        started_at: Some(report.started_at),
        finished_at: report.finished_at,
        files_discovered: report.files_discovered,
        files_processed: report.files_processed,
        sessions_updated: report.sessions_updated,
        retry_after_ms,
        message,
        next_command,
    }
}

pub(crate) fn report_path(config: &Config) -> PathBuf {
    config.cache_dir().join(REPORT_FILE_NAME)
}

fn load_from_path(path: &std::path::Path) -> Result<Option<BackgroundRefreshReport>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "report is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_REPORT_BYTES,
        "report is {} bytes; maximum is {MAX_REPORT_BYTES}",
        metadata.len()
    );
    let bytes = fs::read(path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn update_lock_is_held(config: &Config) -> Result<bool> {
    let path = crate::indexer::index_update_lock_path(&config.db_path());
    let Some(lock) = open_existing_file_lock(&path)? else {
        return Ok(false);
    };
    loop {
        match lock.try_read() {
            Ok(_guard) => return Ok(false),
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(true),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn write_best_effort(config: &Config, report: &BackgroundRefreshReport) {
    let result = preserve_active_writer_report(config, report).and_then(|preserve| {
        if preserve {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(report)
            .context("could not serialize background refresh report")?;
        atomic_write_file(&report_path(config), &bytes, AtomicWriteMode::Replace)
    });
    if let Err(error) = result {
        eprintln!("aise: background refresh status could not be recorded: {error:#}");
    }
}

/// Keep a losing contender from replacing the progress owned by the active database writer.
///
/// Work is `O(1)`: one report read capped by [`MAX_REPORT_BYTES`] and one update-lock probe.
/// Malformed reports are replaced, while failure to inspect a plausible active writer preserves
/// its last known progress instead of publishing a potentially false terminal state.
fn preserve_active_writer_report(
    config: &Config,
    candidate: &BackgroundRefreshReport,
) -> Result<bool> {
    let Ok(Some(existing)) = load_from_path(&report_path(config)) else {
        return Ok(false);
    };
    let expected_database_path = crate::util::normalize_path(&config.db_path());
    if existing.state != BackgroundRefreshState::Running
        || existing.process_id == candidate.process_id
        || existing.database_path.as_deref() != Some(expected_database_path.as_str())
    {
        return Ok(false);
    }
    update_lock_is_held(config)
}

fn bounded_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

fn failed_refresh_message(error: Option<&str>) -> String {
    const GUIDANCE: &str = "Fix the reported parser, configuration, or filesystem cause before retrying; reindexing unchanged code and inputs will repeat the failure.";
    let cause = error.unwrap_or("The automatic index update failed without recording an error.");
    let reserved = GUIDANCE.chars().count() + 1;
    let cause: String = cause
        .chars()
        .take(MAX_ERROR_CHARS.saturating_sub(reserved))
        .collect();
    format!("{cause}\n{GUIDANCE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_and_oversized_reports_are_bounded_failed_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        let path = report_path(&config);

        fs::write(&path, b"not json").unwrap();
        let malformed = readiness_status(&config, &db).unwrap().refresh;
        assert_eq!(malformed.state, IndexRefreshState::FailedWithRecovery);
        assert!(malformed.message.unwrap().contains("Cannot read"));
        assert_eq!(malformed.next_command.as_deref(), Some("aise reindex"));

        fs::write(&path, vec![b'x'; MAX_REPORT_BYTES as usize + 1]).unwrap();
        let oversized = readiness_status(&config, &db).unwrap().refresh;
        assert_eq!(oversized.state, IndexRefreshState::FailedWithRecovery);
        assert!(oversized.message.unwrap().contains("maximum"));
    }

    #[test]
    fn running_report_round_trips_and_write_failure_never_blocks_refresh_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let now = Utc::now();
        let report = BackgroundRefreshReport {
            database_path: Some(crate::util::normalize_path(&config.db_path())),
            origin: BackgroundRefreshOrigin::Mcp,
            state: BackgroundRefreshState::Running,
            started_at: now,
            finished_at: None,
            process_id: 42,
            schema_generation_before: Some(2),
            schema_generation_after: None,
            files_discovered: None,
            files_processed: None,
            sessions_updated: None,
            error: None,
        };
        write_best_effort(&config, &report);
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        assert_eq!(
            readiness_status(&config, &db).unwrap().refresh.state,
            IndexRefreshState::Postponed
        );

        let lock_path = crate::indexer::index_update_lock_path(&config.db_path());
        let mut lock = crate::indexer::open_index_update_lock(&lock_path).unwrap();
        let guard = lock.try_write().unwrap();
        let visible = readiness_status(&config, &db).unwrap().refresh;
        assert_eq!(visible.state, IndexRefreshState::Indexing);
        assert_eq!(visible.started_at, Some(report.started_at));
        assert_eq!(visible.next_command, None);
        drop(guard);
        assert_eq!(
            readiness_status(&config, &db).unwrap().refresh.state,
            IndexRefreshState::Postponed
        );

        let unusable_cache = dir.path().join("regular-file");
        fs::write(&unusable_cache, b"not a directory").unwrap();
        config.index.cache_dir = Some(unusable_cache.to_string_lossy().into_owned());
        write_best_effort(&config, &report);
    }

    #[test]
    fn losing_writer_election_preserves_the_active_writers_progress_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let active_report = BackgroundRefreshReport {
            database_path: Some(crate::util::normalize_path(&config.db_path())),
            origin: BackgroundRefreshOrigin::Cli,
            state: BackgroundRefreshState::Running,
            started_at: Utc::now(),
            finished_at: None,
            process_id: std::process::id().wrapping_add(1),
            schema_generation_before: Some(crate::db::SCHEMA_VERSION),
            schema_generation_after: None,
            files_discovered: Some(100),
            files_processed: Some(40),
            sessions_updated: Some(39),
            error: None,
        };
        write_best_effort(&config, &active_report);

        let lock_path = crate::indexer::index_update_lock_path(&config.db_path());
        let mut lock = crate::indexer::open_index_update_lock(&lock_path).unwrap();
        let guard = lock.try_write().unwrap();
        let outcome = run(&config, BackgroundRefreshOrigin::Mcp, &|| false).unwrap();

        assert_eq!(outcome, BackgroundRefreshOutcome::SkippedBusy);
        assert_eq!(
            load_from_path(&report_path(&config)).unwrap(),
            Some(active_report.clone()),
            "a losing contender must not replace the active writer's identity or progress"
        );
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        let readiness = readiness_status(&config, &db).unwrap().refresh;
        assert_eq!(readiness.state, IndexRefreshState::Indexing);
        assert_eq!(readiness.files_discovered, active_report.files_discovered);
        assert_eq!(readiness.files_processed, active_report.files_processed);
        assert_eq!(readiness.sessions_updated, active_report.sessions_updated);
        drop(guard);
    }

    #[test]
    fn losing_writer_without_an_owned_report_publishes_no_terminal_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        crate::db::Db::open(&config.db_path()).unwrap();
        let lock_path = crate::indexer::index_update_lock_path(&config.db_path());
        let mut lock = crate::indexer::open_index_update_lock(&lock_path).unwrap();
        let guard = lock.try_write().unwrap();

        let outcome = run(&config, BackgroundRefreshOrigin::Mcp, &|| false).unwrap();

        assert_eq!(outcome, BackgroundRefreshOutcome::SkippedBusy);
        assert_eq!(
            load_from_path(&report_path(&config)).unwrap(),
            None,
            "a contender that never owned the writer permit must not publish a receipt"
        );
        drop(guard);
    }

    #[test]
    fn terminal_outcomes_map_to_fresh_or_postponed_readiness() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        for (state, expected) in [
            (BackgroundRefreshState::Updated, IndexRefreshState::Fresh),
            (
                BackgroundRefreshState::SkippedFresh,
                IndexRefreshState::Fresh,
            ),
            (
                BackgroundRefreshState::SkippedBusy,
                IndexRefreshState::Postponed,
            ),
            (
                BackgroundRefreshState::Cancelled,
                IndexRefreshState::Postponed,
            ),
        ] {
            let report = BackgroundRefreshReport {
                database_path: Some(crate::util::normalize_path(&config.db_path())),
                origin: BackgroundRefreshOrigin::Cli,
                state,
                started_at: Utc::now(),
                finished_at: Some(Utc::now()),
                process_id: 42,
                schema_generation_before: Some(2),
                schema_generation_after: Some(2),
                files_discovered: None,
                files_processed: None,
                sessions_updated: None,
                error: None,
            };
            write_best_effort(&config, &report);
            assert_eq!(
                readiness_status(&config, &db).unwrap().refresh.state,
                expected
            );
        }
    }

    #[test]
    fn newer_successful_snapshot_supersedes_older_nonfresh_reports() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        let report_time = Utc::now() - chrono::Duration::seconds(1);

        for state in [
            BackgroundRefreshState::Cancelled,
            BackgroundRefreshState::Failed,
            BackgroundRefreshState::SkippedBusy,
            BackgroundRefreshState::Running,
        ] {
            let report = BackgroundRefreshReport {
                database_path: Some(crate::util::normalize_path(&config.db_path())),
                origin: BackgroundRefreshOrigin::Mcp,
                state,
                started_at: report_time,
                finished_at: (state != BackgroundRefreshState::Running).then_some(report_time),
                process_id: 42,
                schema_generation_before: Some(5),
                schema_generation_after: None,
                files_discovered: Some(10),
                files_processed: Some(3),
                sessions_updated: Some(2),
                error: (state == BackgroundRefreshState::Failed)
                    .then(|| "older parser failure".to_string()),
            };
            write_best_effort(&config, &report);
            db.mark_auto_reindex_complete().unwrap();

            let refresh = readiness_status(&config, &db).unwrap().refresh;
            assert_eq!(refresh.state, IndexRefreshState::Fresh, "{state:?}");
            assert_eq!(refresh.next_command, None, "{state:?}");
            assert_eq!(refresh.retry_after_ms, None, "{state:?}");
            assert_eq!(refresh.message, None, "{state:?}");
        }
    }

    #[test]
    fn newer_success_in_the_same_millisecond_supersedes_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        let millisecond =
            chrono::DateTime::from_timestamp_millis(Utc::now().timestamp_millis()).unwrap();
        let report_time = millisecond + chrono::Duration::nanoseconds(100_000);
        let completed_at = millisecond + chrono::Duration::nanoseconds(900_000);
        let report = BackgroundRefreshReport {
            database_path: Some(crate::util::normalize_path(&config.db_path())),
            origin: BackgroundRefreshOrigin::Mcp,
            state: BackgroundRefreshState::Failed,
            started_at: report_time,
            finished_at: Some(report_time),
            process_id: 42,
            schema_generation_before: Some(5),
            schema_generation_after: None,
            files_discovered: Some(10),
            files_processed: Some(3),
            sessions_updated: Some(2),
            error: Some("older parser failure".to_string()),
        };
        write_best_effort(&config, &report);
        db.mark_auto_reindex_complete_at(completed_at).unwrap();

        assert_eq!(
            readiness_status(&config, &db).unwrap().refresh.state,
            IndexRefreshState::Fresh
        );
    }

    #[test]
    fn newer_nonfresh_reports_are_not_hidden_by_an_older_successful_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        db.mark_auto_reindex_complete().unwrap();
        let report_time = Utc::now() + chrono::Duration::seconds(1);

        for (state, expected) in [
            (
                BackgroundRefreshState::Cancelled,
                IndexRefreshState::Postponed,
            ),
            (
                BackgroundRefreshState::Failed,
                IndexRefreshState::FailedWithRecovery,
            ),
            (
                BackgroundRefreshState::SkippedBusy,
                IndexRefreshState::Postponed,
            ),
            (
                BackgroundRefreshState::Running,
                IndexRefreshState::Postponed,
            ),
        ] {
            let report = BackgroundRefreshReport {
                database_path: Some(crate::util::normalize_path(&config.db_path())),
                origin: BackgroundRefreshOrigin::Mcp,
                state,
                started_at: report_time,
                finished_at: (state != BackgroundRefreshState::Running).then_some(report_time),
                process_id: 42,
                schema_generation_before: Some(5),
                schema_generation_after: None,
                files_discovered: Some(10),
                files_processed: Some(3),
                sessions_updated: Some(2),
                error: (state == BackgroundRefreshState::Failed)
                    .then(|| "newer parser failure".to_string()),
            };
            write_best_effort(&config, &report);

            assert_eq!(
                readiness_status(&config, &db).unwrap().refresh.state,
                expected,
                "{state:?}"
            );
        }
    }

    #[test]
    fn held_writer_keeps_an_older_running_report_visible_as_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        let report = BackgroundRefreshReport {
            database_path: Some(crate::util::normalize_path(&config.db_path())),
            origin: BackgroundRefreshOrigin::Mcp,
            state: BackgroundRefreshState::Running,
            started_at: Utc::now() - chrono::Duration::seconds(1),
            finished_at: None,
            process_id: 42,
            schema_generation_before: Some(5),
            schema_generation_after: None,
            files_discovered: Some(10),
            files_processed: Some(3),
            sessions_updated: Some(2),
            error: None,
        };
        write_best_effort(&config, &report);
        db.mark_auto_reindex_complete().unwrap();
        let lock_path = crate::indexer::index_update_lock_path(&config.db_path());
        let mut lock = crate::indexer::open_index_update_lock(&lock_path).unwrap();
        let _writer = lock.try_write().unwrap();

        let refresh = readiness_status(&config, &db).unwrap().refresh;
        assert_eq!(refresh.state, IndexRefreshState::Indexing);
        assert_eq!(refresh.started_at, Some(report.started_at));
        assert_eq!(refresh.next_command, None);
    }

    #[test]
    fn failed_refresh_requires_cause_correction_before_full_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let db = crate::db::Db::open(&config.db_path()).unwrap();
        let report = BackgroundRefreshReport {
            database_path: Some(crate::util::normalize_path(&config.db_path())),
            origin: BackgroundRefreshOrigin::Mcp,
            state: BackgroundRefreshState::Failed,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            process_id: 42,
            schema_generation_before: Some(4),
            schema_generation_after: None,
            files_discovered: Some(10),
            files_processed: Some(3),
            sessions_updated: None,
            error: Some("invalid provenance from the Claude parser".to_string()),
        };
        write_best_effort(&config, &report);

        let refresh = readiness_status(&config, &db).unwrap().refresh;
        assert_eq!(refresh.state, IndexRefreshState::FailedWithRecovery);
        let message = refresh.message.unwrap();
        assert!(message.contains("invalid provenance from the Claude parser"));
        assert!(
            message.contains("Fix the reported parser, configuration, or filesystem cause before"),
            "{message}"
        );
        assert_eq!(refresh.next_command.as_deref(), Some("aise reindex --full"));

        let bounded = failed_refresh_message(Some(&"x".repeat(MAX_ERROR_CHARS * 2)));
        assert!(bounded.chars().count() <= MAX_ERROR_CHARS);
        assert!(bounded.ends_with("unchanged code and inputs will repeat the failure."));
    }

    #[test]
    fn readiness_separates_snapshot_availability_from_refresh_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        let db = crate::db::Db::open(&config.db_path()).unwrap();

        let initial = readiness_status(&config, &db).unwrap();
        assert_eq!(
            initial.snapshot.availability,
            crate::models::IndexSnapshotAvailability::Unavailable
        );
        assert_eq!(
            initial.refresh.state,
            crate::models::IndexRefreshState::NotStarted
        );

        let mut report = BackgroundRefreshReport {
            database_path: Some(dir.path().join("other.db").to_string_lossy().into_owned()),
            origin: BackgroundRefreshOrigin::Mcp,
            state: BackgroundRefreshState::Running,
            started_at: Utc::now(),
            finished_at: None,
            process_id: 42,
            schema_generation_before: Some(crate::db::SCHEMA_VERSION),
            schema_generation_after: None,
            files_discovered: Some(12),
            files_processed: Some(9),
            sessions_updated: Some(3),
            error: None,
        };
        write_best_effort(&config, &report);
        assert_eq!(
            readiness_status(&config, &db).unwrap().refresh.state,
            IndexRefreshState::NotStarted,
            "a shared cache report for another configured database must not leak readiness"
        );
        report.database_path = Some(crate::util::normalize_path(&config.db_path()));
        write_best_effort(&config, &report);
        let mut lock = crate::indexer::open_index_update_lock(
            &crate::indexer::index_update_lock_path(&config.db_path()),
        )
        .unwrap();
        let _writer = lock.try_write().unwrap();

        let indexing = readiness_status(&config, &db).unwrap();
        assert_eq!(
            indexing.snapshot.availability,
            crate::models::IndexSnapshotAvailability::Unavailable
        );
        assert_eq!(
            indexing.refresh.state,
            crate::models::IndexRefreshState::Indexing
        );
        assert_eq!(indexing.refresh.files_processed, Some(9));
        assert_eq!(indexing.refresh.sessions_updated, Some(3));
        assert_eq!(indexing.refresh.retry_after_ms, Some(1_000));
    }

    #[test]
    fn completed_cold_refresh_records_database_identity_trigger_and_terminal_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.prime_agent.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;

        let outcome = run(
            &config,
            BackgroundRefreshOrigin::IntegrationInstall,
            &|| false,
        )
        .unwrap();
        assert_eq!(
            outcome,
            BackgroundRefreshOutcome::Updated {
                files_seen: 0,
                sessions_updated: 0
            }
        );

        let db = crate::db::Db::open(&config.db_path()).unwrap();
        let readiness = readiness_status(&config, &db).unwrap();
        assert_eq!(
            readiness.snapshot.availability,
            IndexSnapshotAvailability::Usable
        );
        assert_eq!(readiness.refresh.state, IndexRefreshState::Fresh);
        assert_eq!(
            readiness.refresh.started_by,
            Some(IndexRefreshTrigger::IntegrationInstall)
        );
        assert_eq!(readiness.refresh.files_discovered, Some(0));
        assert_eq!(readiness.refresh.files_processed, Some(0));
        assert_eq!(readiness.refresh.sessions_updated, Some(0));
    }
}
