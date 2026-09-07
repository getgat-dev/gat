//! Explicit process-wide execution-resource bootstrap.
//!
//! The CLI owns one Tokio runtime and Rayon global pool for the lifetime of
//! one invocation. Operation-scoped limits remain below this boundary:
//! `RemoteExecutor` controls remote jobs and physical requests, while this
//! module only owns process-wide worker capacity.

use std::num::NonZeroUsize;

// Cache work is admitted eight tasks at a time; leave room for backend-local
// filesystem tasks and unrelated short process work.
const TOKIO_BLOCKING_THREADS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessResourcePolicy {
    cpu: NonZeroUsize,
    tokio_worker: NonZeroUsize,
    tokio_blocking: NonZeroUsize,
}

impl ProcessResourcePolicy {
    fn from_parallelism(parallelism: Option<NonZeroUsize>) -> Self {
        let cpu_threads = parallelism.unwrap_or_else(|| NonZeroUsize::new(1).unwrap());
        Self {
            cpu: cpu_threads,
            tokio_worker: cpu_threads,
            tokio_blocking: NonZeroUsize::new(TOKIO_BLOCKING_THREADS)
                .expect("Tokio blocking thread policy must be positive"),
        }
    }

    fn detect() -> Self {
        Self::from_parallelism(std::thread::available_parallelism().ok())
    }
}

/// Technical failure while creating process-wide execution resources.
#[derive(Debug, thiserror::Error)]
pub enum ProcessResourcesError {
    #[error("could not initialize the process CPU pool")]
    Rayon(#[source] rayon::ThreadPoolBuildError),
    #[error("could not initialize the process task runtime")]
    Tokio(#[source] std::io::Error),
}

/// Initializes Gat's process-wide Rayon pool and Tokio runtime.
///
/// The returned runtime must remain alive until all command work and output
/// rendering complete. Rayon is initialized before the runtime because its
/// global pool can only be configured once per process.
pub fn initialize() -> Result<tokio::runtime::Runtime, ProcessResourcesError> {
    let policy = ProcessResourcePolicy::detect();
    rayon::ThreadPoolBuilder::new()
        .num_threads(policy.cpu.get())
        .build_global()
        .map_err(ProcessResourcesError::Rayon)?;

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(policy.tokio_worker.get())
        .max_blocking_threads(policy.tokio_blocking.get())
        .enable_all()
        .build()
        .map_err(ProcessResourcesError::Tokio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_uses_one_thread_when_parallelism_detection_fails() {
        let policy = ProcessResourcePolicy::from_parallelism(None);
        assert_eq!(policy.cpu.get(), 1);
        assert_eq!(policy.tokio_worker.get(), 1);
        assert_eq!(policy.tokio_blocking.get(), TOKIO_BLOCKING_THREADS);
    }

    #[test]
    fn policy_reuses_detected_parallelism_for_cpu_and_async_workers() {
        let policy = ProcessResourcePolicy::from_parallelism(Some(NonZeroUsize::new(7).unwrap()));
        assert_eq!(policy.cpu.get(), 7);
        assert_eq!(policy.tokio_worker.get(), 7);
        assert_eq!(policy.tokio_blocking.get(), TOKIO_BLOCKING_THREADS);
    }
}
