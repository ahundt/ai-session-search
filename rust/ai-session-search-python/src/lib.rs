use std::path::PathBuf;
use std::sync::Mutex;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use ai_session_search::config::Config;
use ai_session_search::indexer::AutoReindexOutcome;
use ai_session_search::models::{MessageFilters, MessageHit};
use ai_session_search::service::SessionSearch as CoreSessionSearch;

fn runtime_error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

#[pyclass(module = "ai_session_search._native", frozen)]
struct NativeMessageHit {
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    provider: String,
    #[pyo3(get)]
    seq: i64,
    #[pyo3(get)]
    role: String,
    #[pyo3(get)]
    kind: String,
    #[pyo3(get)]
    timestamp: Option<String>,
    #[pyo3(get)]
    tool_name: Option<String>,
    #[pyo3(get)]
    tool_call_id: Option<String>,
    #[pyo3(get)]
    fuzzy_score: Option<u32>,
    #[pyo3(get)]
    content: String,
}

impl From<MessageHit> for NativeMessageHit {
    fn from(hit: MessageHit) -> Self {
        Self {
            session_id: hit.session_id,
            provider: hit.provider.as_str().to_string(),
            seq: hit.seq,
            role: hit.role.as_str().to_string(),
            kind: hit.kind.as_str().to_string(),
            timestamp: hit.ts.map(|value| value.to_rfc3339()),
            tool_name: hit.tool_name,
            tool_call_id: hit.tool_call_id,
            fuzzy_score: hit.fuzzy_score,
            content: hit.content,
        }
    }
}

#[pyclass(module = "ai_session_search._native", frozen)]
struct RefreshOutcome {
    #[pyo3(get)]
    status: &'static str,
    #[pyo3(get)]
    files_seen: Option<usize>,
    #[pyo3(get)]
    sessions_updated: Option<usize>,
    #[pyo3(get)]
    reason: Option<String>,
}

impl From<AutoReindexOutcome> for RefreshOutcome {
    fn from(outcome: AutoReindexOutcome) -> Self {
        match outcome {
            AutoReindexOutcome::Updated {
                files_seen,
                sessions_updated,
            } => Self {
                status: "updated",
                files_seen: Some(files_seen),
                sessions_updated: Some(sessions_updated),
                reason: None,
            },
            AutoReindexOutcome::SkippedBusy => Self {
                status: "skipped_busy",
                files_seen: None,
                sessions_updated: None,
                reason: None,
            },
            AutoReindexOutcome::SkippedFresh => Self {
                status: "skipped_fresh",
                files_seen: None,
                sessions_updated: None,
                reason: None,
            },
            AutoReindexOutcome::SkippedLockUnavailable { reason } => Self {
                status: "skipped_lock_unavailable",
                files_seen: None,
                sessions_updated: None,
                reason: Some(reason),
            },
        }
    }
}

#[pyclass(module = "ai_session_search._native")]
struct SessionSearch {
    inner: Mutex<CoreSessionSearch>,
}

#[pymethods]
impl SessionSearch {
    #[new]
    #[pyo3(signature = (db_path=None))]
    fn new(db_path: Option<PathBuf>) -> PyResult<Self> {
        let mut config = Config::load().map_err(runtime_error)?;
        if let Some(path) = db_path {
            if path.as_os_str().is_empty() {
                return Err(PyValueError::new_err("db_path must not be empty"));
            }
            config.index.db_path = Some(path.to_string_lossy().into_owned());
        }
        let inner = CoreSessionSearch::open(config).map_err(runtime_error)?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    #[getter]
    fn db_path(&self) -> PyResult<PathBuf> {
        let app = self.inner.lock().map_err(runtime_error)?;
        Ok(app.config().db_path())
    }

    #[pyo3(signature = (query, *, limit=50, offset=0))]
    fn search_messages(
        &self,
        py: Python<'_>,
        query: String,
        limit: usize,
        offset: usize,
    ) -> PyResult<Vec<NativeMessageHit>> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            let filters = MessageFilters {
                limit,
                offset,
                ..MessageFilters::default()
            };
            app.messages()
                .search(&query, &filters)
                .map(|hits| hits.into_iter().map(NativeMessageHit::from).collect())
                .map_err(runtime_error)
        })
    }

    fn refresh(&self, py: Python<'_>) -> PyResult<RefreshOutcome> {
        py.detach(|| {
            let app = self.inner.lock().map_err(runtime_error)?;
            app.index()
                .refresh()
                .map(RefreshOutcome::from)
                .map_err(runtime_error)
        })
    }
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<SessionSearch>()?;
    module.add_class::<NativeMessageHit>()?;
    module.add_class::<RefreshOutcome>()?;
    Ok(())
}
