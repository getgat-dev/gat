//! Scoped accounting for parallel work; workers never publish activity text.

use std::sync::Mutex;

/// A coherent snapshot of started work. Completion is derived from disjoint
/// success/failure counts; callers cannot manufacture inconsistent snapshots.
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

/// Operation-wide work accounting. Disabled accounting takes no locks.
#[derive(Default)]
pub struct WorkProgress {
    counts: Option<Mutex<WorkCounts>>,
}

impl WorkProgress {
    #[must_use]
    pub fn enabled() -> Self {
        Self {
            counts: Some(Mutex::new(WorkCounts::default())),
        }
    }

    /// Read counters together.
    ///
    /// # Panics
    /// Panics if an earlier accounting update poisoned the counter lock.
    #[must_use]
    pub fn snapshot(&self) -> WorkCounts {
        self.counts
            .as_ref()
            .map_or_else(WorkCounts::default, |counts| *counts.lock().unwrap())
    }

    /// Start one item. Dropping it without completing it records failure,
    /// including early returns and unwinding. An item can complete only once.
    ///
    /// # Panics
    /// Panics if an earlier accounting update poisoned the counter lock.
    #[must_use]
    pub fn start(&self) -> WorkItem<'_> {
        if let Some(counts) = &self.counts {
            counts.lock().unwrap().active += 1;
        }
        WorkItem {
            counts: self.counts.as_ref(),
        }
    }
}

pub struct WorkItem<'a> {
    counts: Option<&'a Mutex<WorkCounts>>,
}

impl WorkItem<'_> {
    /// Record success and consume the item.
    ///
    /// # Panics
    /// Panics if an earlier accounting update poisoned the counter lock.
    pub fn complete(mut self) {
        if let Some(counts) = self.counts.take() {
            let mut counts = counts.lock().unwrap();
            counts.active -= 1;
            counts.succeeded += 1;
        }
    }
}

impl Drop for WorkItem<'_> {
    fn drop(&mut self) {
        if let Some(counts) = self.counts {
            let mut counts = counts.lock().unwrap();
            counts.active -= 1;
            counts.failed += 1;
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
}
