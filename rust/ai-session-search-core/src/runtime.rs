//! Application-owned execution resources for data-parallel work.

use std::num::NonZeroUsize;
use std::sync::OnceLock;

/// RAII worker runtime owned by one database/application lifecycle.
///
/// Parallel iterators run on this pool only when entered through `ExecutionRuntime::install`.
/// Dropping the
/// runtime asks Rayon to finish outstanding work and terminate its worker threads.
pub struct ExecutionRuntime {
    worker_threads: NonZeroUsize,
    pool: OnceLock<rayon::ThreadPool>,
}

impl ExecutionRuntime {
    /// Create a fixed-size worker pool. The explicit nonzero type prevents ambiguous `0` handling.
    pub const fn new(worker_threads: NonZeroUsize) -> Self {
        Self {
            worker_threads,
            pool: OnceLock::new(),
        }
    }

    /// Number of workers available to this application.
    pub fn worker_threads(&self) -> usize {
        self.worker_threads.get()
    }

    pub(crate) fn install<OP, R>(&self, operation: OP) -> Result<R, rayon::ThreadPoolBuildError>
    where
        OP: FnOnce() -> R + Send,
        R: Send,
    {
        if let Some(pool) = self.pool.get() {
            return Ok(pool.install(operation));
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.worker_threads.get())
            .thread_name(|index| format!("aise-worker-{index}"))
            .build()?;
        let _ = self.pool.set(pool);
        Ok(self
            .pool
            .get()
            .expect("worker pool initialized above")
            .install(operation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_pool_is_created_only_for_the_first_parallel_operation() {
        let runtime = ExecutionRuntime::new(NonZeroUsize::new(2).unwrap());
        assert_eq!(runtime.worker_threads(), 2);
        assert!(runtime.pool.get().is_none());

        assert_eq!(runtime.install(|| 6 * 7).unwrap(), 42);
        assert!(runtime.pool.get().is_some());
        assert_eq!(runtime.install(|| 7 * 8).unwrap(), 56);
    }
}
