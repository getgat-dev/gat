//! Pending increments with separate sampling and worker capabilities.
use std::sync::atomic::{AtomicU64, Ordering};

/// Owns pending increments. Only the sampling owner receives this capability;
/// workers borrow an increment-only [`WorkProgress`] instead.
#[derive(Default)]
pub struct WorkCounter {
    pending: AtomicU64,
}

impl WorkCounter {
    #[must_use]
    pub const fn handle(&self) -> WorkProgress<'_> {
        WorkProgress {
            pending: Some(&self.pending),
        }
    }

    /// Atomically drain unpublished work without mirroring terminal state.
    #[must_use]
    pub fn take_pending(&self) -> u64 {
        self.pending.swap(0, Ordering::Relaxed)
    }
}

/// Increment-only worker capability. One relaxed atomic per completed item;
/// disabled handles touch no atomics and allocate no shared state.
///
/// ```compile_fail
/// fn worker(progress: &gat_core::progress::WorkProgress<'_>) {
///     progress.take_pending(); // Only the sampling owner can drain counters.
/// }
/// ```
#[derive(Clone, Copy, Default)]
pub struct WorkProgress<'a> {
    pending: Option<&'a AtomicU64>,
}

impl WorkProgress<'_> {
    pub fn inc(&self, delta: u64) {
        if let Some(pending) = self.pending {
            pending.fetch_add(delta, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_increments_are_drained_exactly_once() {
        let counter = WorkCounter::default();
        let work = counter.handle();
        let total = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        for _ in 0..10_000 {
                            work.inc(1);
                        }
                    })
                })
                .collect();
            let during = counter.take_pending();
            for worker in workers {
                worker.join().unwrap();
            }
            during + counter.take_pending()
        });
        assert_eq!(total, 80_000);
        assert_eq!(counter.take_pending(), 0);
        let disabled = WorkProgress::default();
        disabled.inc(10);
        assert!(disabled.pending.is_none());
    }
}
