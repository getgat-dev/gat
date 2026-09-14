//! Scoped accounting for parallel work; workers never publish activity text.

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic sampled totals for started work. Active work is derived from
/// starts and disjoint completions, so sampled completions cannot exceed starts.
/// Workers may finish between reads; quiescent snapshots are exact.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkCounts {
    active: u64,
    succeeded: u64,
    failed: u64,
}

impl WorkCounts {
    #[must_use]
    pub const fn active(self) -> u64 {
        self.active
    }
    #[must_use]
    pub const fn succeeded(self) -> u64 {
        self.succeeded
    }
    #[must_use]
    pub const fn failed(self) -> u64 {
        self.failed
    }
    #[must_use]
    pub const fn completed(self) -> u64 {
        self.succeeded + self.failed
    }
}

#[derive(Default)]
struct Counters {
    started: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
}

/// Operation-wide work accounting. Workers perform two atomic increments per
/// item, without locks, allocations, clocks, or reporting callbacks. Disabled
/// accounting does not touch any atomics.
#[derive(Default)]
pub struct WorkProgress {
    counts: Option<Counters>,
}

impl WorkProgress {
    #[must_use]
    pub fn enabled() -> Self {
        Self {
            counts: Some(Counters::default()),
        }
    }

    /// Sample completions before starts. Acquiring each completion observes
    /// its preceding start (including across a release sequence of workers),
    /// so the final start load cannot lag the completions used in this sample.
    #[must_use]
    pub fn snapshot(&self) -> WorkCounts {
        self.counts
            .as_ref()
            .map_or_else(WorkCounts::default, |counts| {
                let succeeded = counts.succeeded.load(Ordering::Acquire);
                let failed = counts.failed.load(Ordering::Acquire);
                let started = counts.started.load(Ordering::Relaxed);
                WorkCounts {
                    active: started - succeeded - failed,
                    succeeded,
                    failed,
                }
            })
    }

    /// Start one item. Dropping it without completing it records failure,
    /// including early returns and unwinding. An item can complete only once.
    #[must_use]
    pub fn start(&self) -> WorkItem<'_> {
        if let Some(counts) = &self.counts {
            counts.started.fetch_add(1, Ordering::Relaxed);
        }
        WorkItem {
            counts: self.counts.as_ref(),
        }
    }
}

/// An unfinished work item. Success consumes the item; every other exit records
/// failure. Its borrow prevents work from outliving its operation's counters.
pub struct WorkItem<'a> {
    counts: Option<&'a Counters>,
}

impl WorkItem<'_> {
    /// Record success and consume the item.
    ///
    /// ```compile_fail
    /// let work = gat_core::progress::WorkProgress::enabled();
    /// let item = work.start();
    /// item.complete();
    /// item.complete(); // a completion cannot be counted twice
    /// ```
    pub fn complete(mut self) {
        if let Some(counts) = self.counts.take() {
            counts.succeeded.fetch_add(1, Ordering::Release);
        }
    }
}

impl Drop for WorkItem<'_> {
    fn drop(&mut self) {
        if let Some(counts) = self.counts {
            counts.failed.fetch_add(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_work_and_early_exit_preserve_accounting() {
        let progress = WorkProgress::enabled();
        let first = progress.start();
        let failed = progress.start();
        assert_eq!(progress.snapshot().active(), 2);
        first.complete();
        let counts = progress.snapshot();
        assert_eq!(
            (counts.active(), counts.succeeded(), counts.failed()),
            (1, 1, 0)
        );
        drop(failed);
        let counts = progress.snapshot();
        assert_eq!(
            (counts.active(), counts.succeeded(), counts.failed()),
            (0, 1, 1)
        );
        assert_eq!(counts.completed(), 2);
    }

    #[test]
    fn unwind_records_failure_and_disabled_accounting_stays_empty() {
        let progress = WorkProgress::enabled();
        let _ = std::panic::catch_unwind(|| {
            let _item = progress.start();
            panic!("interrupted work");
        });
        assert_eq!(progress.snapshot().failed(), 1);
        assert_eq!(progress.snapshot().active(), 0);
        let disabled = WorkProgress::default();
        disabled.start().complete();
        drop(disabled.start());
        assert_eq!(disabled.snapshot(), WorkCounts::default());
    }
    #[test]
    fn parallel_completions_preserve_sampled_invariants_and_exact_final_totals() {
        let progress = WorkProgress::enabled();
        let finished = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..10_000 {
                        progress.start().complete();
                        drop(progress.start());
                    }
                    finished.fetch_add(1, Ordering::Release);
                });
            }
            let mut previous = WorkCounts::default();
            while finished.load(Ordering::Acquire) != 8 {
                let counts = progress.snapshot();
                assert!(counts.succeeded() >= previous.succeeded());
                assert!(counts.failed() >= previous.failed());
                assert!(counts.active() <= 160_000);
                previous = counts;
                std::thread::yield_now();
            }
        });
        let counts = progress.snapshot();
        assert_eq!(
            (counts.active(), counts.succeeded(), counts.failed()),
            (0, 80_000, 80_000)
        );
    }
}
