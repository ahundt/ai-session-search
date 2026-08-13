// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Application-owned execution resources for data-parallel work.

use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// RAII worker runtime owned by one database/application lifecycle.
///
/// Parallel iterators run on this pool only when entered through `ExecutionRuntime::install`.
/// Dropping the
/// runtime asks Rayon to finish outstanding work and terminate its worker threads.
pub struct ExecutionRuntime {
    worker_threads: NonZeroUsize,
    pool: OnceLock<rayon::ThreadPool>,
    initialization: Mutex<()>,
    #[cfg(test)]
    pool_builds: AtomicUsize,
}

impl ExecutionRuntime {
    /// Create a fixed-size worker pool. The explicit nonzero type prevents ambiguous `0` handling.
    pub const fn new(worker_threads: NonZeroUsize) -> Self {
        Self {
            worker_threads,
            pool: OnceLock::new(),
            initialization: Mutex::new(()),
            #[cfg(test)]
            pool_builds: AtomicUsize::new(0),
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
        let _initialization = self
            .initialization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.pool.get().is_none() {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(self.worker_threads.get())
                .thread_name(|index| format!("aise-worker-{index}"))
                .build()?;
            #[cfg(test)]
            self.pool_builds.fetch_add(1, Ordering::Relaxed);
            self.pool
                .set(pool)
                .expect("worker pool remains empty while initialization is locked");
        }
        Ok(self
            .pool
            .get()
            .expect("worker pool initialized above")
            .install(operation))
    }

    #[cfg(test)]
    pub(crate) fn pool_build_count(&self) -> usize {
        self.pool_builds.load(Ordering::Relaxed)
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

    #[test]
    fn concurrent_first_use_builds_exactly_one_worker_pool() {
        let runtime = std::sync::Arc::new(ExecutionRuntime::new(
            NonZeroUsize::new(2).expect("nonzero worker count"),
        ));
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let runtime = std::sync::Arc::clone(&runtime);
                let start = std::sync::Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    runtime.install(|| 42).unwrap()
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            assert_eq!(handle.join().unwrap(), 42);
        }
        assert_eq!(runtime.pool_build_count(), 1);
    }
}
