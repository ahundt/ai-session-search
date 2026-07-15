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
use crate::models::{IndexUpdateState, IndexUpdateStatus};
use crate::service::SessionSearch;

const REPORT_FILE_NAME: &str = "background-refresh-status.json";
const MAX_REPORT_BYTES: u64 = 64 * 1024;
const MAX_ERROR_CHARS: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackgroundRefreshOrigin {
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
    origin: BackgroundRefreshOrigin,
    state: BackgroundRefreshState,
    started_at: chrono::DateTime<Utc>,
    finished_at: Option<chrono::DateTime<Utc>>,
    process_id: u32,
    schema_generation_before: Option<i64>,
    schema_generation_after: Option<i64>,
    files_seen: Option<usize>,
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
        origin,
        state: BackgroundRefreshState::Running,
        started_at,
        finished_at: None,
        process_id: std::process::id(),
        schema_generation_before: None,
        schema_generation_after: None,
        files_seen: None,
        sessions_updated: None,
        error: None,
    };
    write_best_effort(config, &report);

    let result = (|| {
        let app = SessionSearch::open(config.clone()).context("could not open the index")?;
        report.schema_generation_before = Some(app.database().schema_version()?);
        let outcome = crate::indexer::refresh_usable_index_nonblocking(
            config,
            app.database(),
            should_cancel,
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
            report.files_seen = Some(*files_seen);
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
    write_best_effort(config, &report);
    result
}

pub(crate) fn public_status(config: &Config) -> Option<IndexUpdateStatus> {
    let path = report_path(config);
    let report = match load_from_path(&path) {
        Ok(report) => report?,
        Err(error) => {
            return Some(IndexUpdateStatus {
                state: IndexUpdateState::AttentionRequired,
                started_at: Utc::now(),
                message: bounded_error(&format!(
                    "Cannot read automatic index-update status at {}: {error:#}",
                    path.display()
                )),
                next_command: None,
            });
        }
    };

    match report.state {
        BackgroundRefreshState::Running => match update_lock_is_held(config) {
            Ok(true) => Some(IndexUpdateStatus {
                state: IndexUpdateState::InProgress,
                started_at: report.started_at,
                message: "An automatic index update is running; searches continue using the compatible existing index.".to_string(),
                next_command: None,
            }),
            Ok(false) => None,
            Err(error) => Some(IndexUpdateStatus {
                state: IndexUpdateState::AttentionRequired,
                started_at: report.started_at,
                message: bounded_error(&format!(
                    "Cannot determine whether the automatic index update is still running: {error:#}"
                )),
                next_command: None,
            }),
        },
        BackgroundRefreshState::Failed => Some(IndexUpdateStatus {
            state: IndexUpdateState::AttentionRequired,
            started_at: report.started_at,
            message: report.error.unwrap_or_else(|| {
                "The automatic index update failed without recording an error.".to_string()
            }),
            next_command: Some("aise reindex".to_string()),
        }),
        BackgroundRefreshState::Updated
        | BackgroundRefreshState::SkippedFresh
        | BackgroundRefreshState::SkippedBusy
        | BackgroundRefreshState::Cancelled => None,
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
    anyhow::ensure!(metadata.file_type().is_file(), "report is not a regular file");
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
    let result = serde_json::to_vec_pretty(report)
        .context("could not serialize background refresh report")
        .and_then(|bytes| {
            atomic_write_file(&report_path(config), &bytes, AtomicWriteMode::Replace)
        });
    if let Err(error) = result {
        eprintln!("aise: background refresh status could not be recorded: {error:#}");
    }
}

fn bounded_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_and_oversized_reports_are_bounded_failed_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        let path = report_path(&config);

        fs::write(&path, b"not json").unwrap();
        let malformed = public_status(&config).unwrap();
        assert_eq!(malformed.state, IndexUpdateState::AttentionRequired);
        assert!(malformed.message.contains("Cannot read"));
        assert_eq!(malformed.next_command, None);

        fs::write(&path, vec![b'x'; MAX_REPORT_BYTES as usize + 1]).unwrap();
        let oversized = public_status(&config).unwrap();
        assert_eq!(oversized.state, IndexUpdateState::AttentionRequired);
        assert!(oversized.message.contains("maximum"));
    }

    #[test]
    fn running_report_round_trips_and_write_failure_never_blocks_refresh_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        let now = Utc::now();
        let report = BackgroundRefreshReport {
            origin: BackgroundRefreshOrigin::Mcp,
            state: BackgroundRefreshState::Running,
            started_at: now,
            finished_at: None,
            process_id: 42,
            schema_generation_before: Some(2),
            schema_generation_after: None,
            files_seen: None,
            sessions_updated: None,
            error: None,
        };
        write_best_effort(&config, &report);
        assert_eq!(public_status(&config), None);

        let lock_path = crate::indexer::index_update_lock_path(&config.db_path());
        let mut lock = crate::indexer::open_index_update_lock(&lock_path).unwrap();
        let guard = lock.try_write().unwrap();
        let visible = public_status(&config).unwrap();
        assert_eq!(visible.state, IndexUpdateState::InProgress);
        assert_eq!(visible.started_at, report.started_at);
        assert_eq!(visible.next_command, None);
        drop(guard);
        assert_eq!(public_status(&config), None);

        let unusable_cache = dir.path().join("regular-file");
        fs::write(&unusable_cache, b"not a directory").unwrap();
        config.index.cache_dir = Some(unusable_cache.to_string_lossy().into_owned());
        write_best_effort(&config, &report);
    }

    #[test]
    fn normal_terminal_outcomes_stay_out_of_public_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.cache_dir = Some(dir.path().to_string_lossy().into_owned());
        for state in [
            BackgroundRefreshState::Updated,
            BackgroundRefreshState::SkippedFresh,
            BackgroundRefreshState::SkippedBusy,
            BackgroundRefreshState::Cancelled,
        ] {
            let report = BackgroundRefreshReport {
                origin: BackgroundRefreshOrigin::Cli,
                state,
                started_at: Utc::now(),
                finished_at: Some(Utc::now()),
                process_id: 42,
                schema_generation_before: Some(2),
                schema_generation_after: Some(2),
                files_seen: None,
                sessions_updated: None,
                error: None,
            };
            write_best_effort(&config, &report);
            assert_eq!(public_status(&config), None);
        }
    }
}
