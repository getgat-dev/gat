use crate::progress_reporting;
use crate::remote_executor::TransferObserver;
use gat_core::progress::{ProgressActivity, ProgressHandle, ReceiveProgress};
use tokio::time::Instant;

/// Reports fetch progress in successfully downloaded objects.
/// A repair reporter cannot be passed to a fetch window.
///
/// ```compile_fail
/// fn wrong_mode(op: &mut gat_engine::Operation<'_>, progress: &mut gat_engine::RepairProgress) {
///     gat_engine::download_window(op, Vec::new(), progress);
/// }
/// ```
pub type FetchProgress = ReceiveReporter<false>;

/// Reports repair progress in original path entries, including failed repairs.
/// A fetch reporter cannot be passed to a repair window.
///
/// ```compile_fail
/// fn wrong_mode(op: &mut gat_engine::Operation<'_>, progress: &mut gat_engine::FetchProgress) {
///     gat_engine::repair_window(op, Vec::new(), progress);
/// }
/// ```
pub type RepairProgress = ReceiveReporter<true>;

/// Shared storage for the two receive reporters. The mode is fixed by the
/// public aliases, so windows cannot mix position semantics. No runtime tag or
/// dynamic dispatch is needed; both modes retain totals across windows.
pub struct ReceiveReporter<const REPAIR: bool> {
    task: ProgressHandle,
    counts: ReceiveProgress,
    enabled: bool,
    completed: u64,
    last: Option<(ReceiveProgress, Instant)>,
}

impl<const REPAIR: bool> ReceiveReporter<REPAIR> {
    #[must_use]
    pub fn new(task: ProgressHandle) -> Self {
        Self {
            enabled: task.is_enabled(),
            task,
            counts: ReceiveProgress::default(),
            completed: 0,
            last: None,
        }
    }

    pub(super) const fn task(&self) -> &ProgressHandle {
        &self.task
    }

    /// Resolve an object rejected before a transfer could be admitted.
    fn reject(&mut self, entries: u64) {
        self.complete(false, entries);
        self.report(false);
    }

    const fn complete(&mut self, success: bool, entries: u64) {
        if !self.enabled {
            return;
        }
        if success {
            self.counts.received += 1;
        } else {
            self.counts.failed += 1;
        }
        if REPAIR || success {
            self.completed += entries;
        }
    }

    /// Publish the final snapshot after a window, including verification failure.
    pub fn flush(&mut self) {
        self.counts.verifying = 0;
        self.report(true);
    }

    fn report(&mut self, force: bool) {
        if !self.enabled {
            return;
        }
        if self.last.is_some_and(|(counts, _)| counts == self.counts) {
            return;
        }
        let active = |counts: ReceiveProgress| (counts.verifying != 0, counts.downloading != 0);
        // The executor's refresh deadline handles count-only changes. Avoid a
        // clock read and backend increment per result while downloads continue.
        if !force
            && self
                .last
                .is_some_and(|(counts, _)| active(counts) == active(self.counts))
        {
            return;
        }
        let now = Instant::now();
        if self.completed != 0 {
            self.task.inc(std::mem::take(&mut self.completed));
        }
        self.task.set_activity(if REPAIR {
            ProgressActivity::Repairing(self.counts)
        } else {
            ProgressActivity::Fetching(self.counts)
        });
        self.last = Some((self.counts, now));
    }

    fn deadline(&self) -> Option<Instant> {
        progress_reporting::deadline(self.last, self.counts)
    }
}

impl ReceiveReporter<false> {
    pub(super) fn verifying(&mut self, count: usize) {
        if !self.enabled {
            return;
        }
        self.counts.verifying = count as u64;
        self.report(false);
    }

    pub(super) fn verified(&mut self, checked: usize, cached: usize) {
        if !self.enabled {
            return;
        }
        self.counts.verifying = 0;
        self.counts.checked += checked as u64;
        self.counts.cached += cached as u64;
        self.report(false);
    }

    pub(super) fn rejected(&mut self) {
        self.reject(1);
    }

    pub(super) fn observe(&mut self) -> ReceiveObserver<'_, false, impl Fn(usize) -> u64> {
        ReceiveObserver {
            progress: self,
            entries: |_| 1,
        }
    }
}

impl ReceiveReporter<true> {
    /// Resolve an object rejected before a transfer could be admitted.
    pub fn rejected(&mut self, entries: u64) {
        self.reject(entries);
    }

    pub(super) fn observe(
        &mut self,
        entries: impl Fn(usize) -> u64,
    ) -> ReceiveObserver<'_, true, impl Fn(usize) -> u64> {
        ReceiveObserver {
            progress: self,
            entries,
        }
    }
}

pub(super) struct ReceiveObserver<'a, const REPAIR: bool, W> {
    progress: &'a mut ReceiveReporter<REPAIR>,
    entries: W,
}

impl<T, E, const REPAIR: bool, W: Fn(usize) -> u64> TransferObserver<Result<T, E>>
    for ReceiveObserver<'_, REPAIR, W>
{
    fn completed(&mut self, index: usize, result: &Result<T, E>) {
        if self.progress.enabled {
            self.progress
                .complete(result.is_ok(), (self.entries)(index));
        }
    }
    fn report(&mut self, active: usize, force: bool) {
        if !self.progress.enabled {
            return;
        }
        self.progress.counts.downloading = active as u64;
        self.progress.report(force);
    }
    fn deadline(&self) -> Option<Instant> {
        self.progress.deadline()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::progress::{ActivityBackend, ProgressTask};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recording {
        activities: Mutex<Vec<ProgressActivity>>,
        increments: Mutex<Vec<u64>>,
    }
    impl ActivityBackend for Recording {
        fn inc(&self, delta: u64) {
            self.increments.lock().unwrap().push(delta);
        }
        fn set_activity(&self, value: &ProgressActivity) {
            self.activities.lock().unwrap().push(value.clone());
        }
        fn finish(&self) {}
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_throttles_counts_but_reports_verification_and_download_transitions() {
        let backend = Arc::new(Recording::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut progress = FetchProgress::new(task.handle());
        progress.verifying(100);
        progress.verified(100, 90);
        {
            let mut observer = progress.observe();
            <_ as TransferObserver<Result<(), ()>>>::report(&mut observer, 10, false);
            for index in 0..8 {
                observer.completed(index, &Ok::<_, ()>(()));
                <_ as TransferObserver<Result<(), ()>>>::report(&mut observer, 9 - index, false);
            }
        }
        let before = backend.activities.lock().unwrap().len();
        assert_eq!(before, 3, "count-only updates should be coalesced");
        assert!(backend.increments.lock().unwrap().is_empty());
        tokio::time::advance(crate::progress_reporting::REFRESH_INTERVAL).await;
        assert!(
            progress
                .deadline()
                .is_some_and(|deadline| deadline <= Instant::now())
        );
        progress.report(true);
        assert_eq!(backend.activities.lock().unwrap().len(), before + 1);
        assert_eq!(backend.increments.lock().unwrap().iter().sum::<u64>(), 8);
        assert!(matches!(backend.activities.lock().unwrap().last(),
            Some(ProgressActivity::Fetching(counts)) if counts.downloading == 2
                && counts.checked == 100 && counts.cached == 90 && counts.received == 8));
    }

    #[test]
    fn repair_counts_objects_in_summary_and_paths_in_position() {
        let backend = Arc::new(Recording::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut progress = RepairProgress::new(task.handle());
        {
            let mut observer = progress.observe(|index| [3, 2][index]);
            observer.completed(0, &Ok::<_, ()>(()));
            observer.completed(1, &Err::<(), _>(()));
            <_ as TransferObserver<Result<(), ()>>>::report(&mut observer, 0, true);
        }
        assert_eq!(*backend.increments.lock().unwrap(), [5]);
        assert!(matches!(backend.activities.lock().unwrap().last(),
            Some(ProgressActivity::Repairing(counts)) if counts.received == 1 && counts.failed == 1));
    }

    #[test]
    fn disabled_reports_need_no_runtime_or_refresh_timer() {
        use gat_core::progress::{NoopProgress, ProgressOperation, ProgressReporter, ProgressSpec};
        let task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Fetching));
        let mut progress = FetchProgress::new(task.handle());
        progress.verifying(128);
        progress.verified(128, 127);
        progress.observe().completed(0, &Ok::<_, ()>(()));
        let mut repair = RepairProgress::new(task.handle());
        repair
            .observe(|_| panic!("disabled reporting must not calculate weights"))
            .completed(0, &Ok::<_, ()>(()));
        repair.flush();
        assert_eq!(repair.counts, ReceiveProgress::default());
        progress.flush();
        assert!(progress.deadline().is_none());
        assert!(progress.last.is_none());
        assert_eq!(progress.counts, ReceiveProgress::default());
    }
}
