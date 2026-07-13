//! Application-owned execution resources for data-parallel work.

use std::num::NonZeroUsize;

/// RAII worker runtime owned by one database/application lifecycle.
///
/// Parallel iterators run on this pool only when entered through [`Self::install`]. Dropping the
/// runtime asks Rayon to finish outstanding work and terminate its worker threads.
pub struct ExecutionRuntime {
    pool: rayon::ThreadPool,
}

impl ExecutionRuntime {
    /// Create a fixed-size worker pool. The explicit nonzero type prevents ambiguous `0` handling.
    pub fn new(worker_threads: NonZeroUsize) -> Result<Self, rayon::ThreadPoolBuildError> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(worker_threads.get())
            .thread_name(|index| format!("aise-worker-{index}"))
            .build()?;
        Ok(Self { pool })
    }

    /// Number of workers available to this application.
    pub fn worker_threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    pub(crate) fn install<OP, R>(&self, operation: OP) -> R
    where
        OP: FnOnce() -> R + Send,
        R: Send,
    {
        self.pool.install(operation)
    }
}
