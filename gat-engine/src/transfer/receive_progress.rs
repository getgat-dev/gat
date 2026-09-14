use crate::progress_reporting;
use crate::remote_executor::TransferObserver;
use gat_core::progress::{ProgressActivity, ProgressHandle, ReceiveProgress};
use tokio::time::Instant;

/// Operation-scoped receive reporting, retained across bounded windows.
///
/// Fetch positions count successful downloads. Repair positions count original
/// path entries; the engine receives their multiplicity with each unique object.
pub struct DownloadProgress {
    pub(crate) task: ProgressHandle,
    counts: ReceiveProgress,
    repair: bool,
    enabled: bool,
    completed: u64,
    last: Option<(ReceiveProgress, Instant)>,
}

impl DownloadProgress {
    #[must_use]
    pub fn fetch(task: ProgressHandle) -> Self {
        Self::new(task, false)
    }

    #[must_use]
    pub fn repair(task: ProgressHandle) -> Self {
        Self::new(task, true)
    }

    fn new(task: ProgressHandle, repair: bool) -> Self {
        Self {
            enabled: task.is_enabled(),
            task,
            repair,
            counts: ReceiveProgress::default(),
            completed: 0,
            last: None,
        }
    }

    pub(super) fn verifying(&mut self, count: usize) {
        self.counts.verifying = count as u64;
        self.report(false);
    }

    pub(super) fn verified(&mut self, checked: usize, cached: usize) {
        self.counts.verifying = 0;
        self.counts.checked += checked as u64;
        self.counts.cached += cached as u64;
        self.report(false);
    }

    /// Resolve an object rejected before a transfer could be admitted.
    pub fn rejected(&mut self, entries: u64) {
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
        if self.repair || success {
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
        if progress_reporting::due(self.last, self.counts, active, force, now) {
            if self.completed != 0 {
                self.task.inc(std::mem::take(&mut self.completed));
            }
            self.task.set_activity(if self.repair {
                ProgressActivity::Repairing(self.counts)
            } else {
                ProgressActivity::Fetching(self.counts)
            });
            self.last = Some((self.counts, now));
        }
    }

    fn deadline(&self) -> Option<Instant> {
        progress_reporting::deadline(self.last, self.counts)
    }

    pub(super) fn observe(
        &mut self,
        entries: impl Fn(usize) -> u64,
    ) -> ReceiveObserver<'_, impl Fn(usize) -> u64> {
        ReceiveObserver {
            progress: self,
            entries,
        }
    }
}

pub(super) struct ReceiveObserver<'a, W> {
    progress: &'a mut DownloadProgress,
    entries: W,
}

impl<T, E, W: Fn(usize) -> u64> TransferObserver<Result<T, E>> for ReceiveObserver<'_, W> {
    fn completed(&mut self, index: usize, result: &Result<T, E>) {
        if self.progress.enabled {
            self.progress
                .complete(result.is_ok(), (self.entries)(index));
        }
    }
    fn report(&mut self, active: usize, force: bool) {
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
        let mut progress = DownloadProgress::fetch(task.handle());
        progress.verifying(100);
        progress.verified(100, 90);
        {
            let mut observer = progress.observe(|_| 1);
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
        let mut progress = DownloadProgress::repair(task.handle());
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
        let mut progress = DownloadProgress::fetch(task.handle());
        progress.verifying(128);
        progress.verified(128, 127);
        progress
            .observe(|_| panic!("disabled reporting must not calculate weights"))
            .completed(0, &Ok::<_, ()>(()));
        progress.flush();
        assert!(progress.deadline().is_none());
        assert!(progress.last.is_none());
    }
}
