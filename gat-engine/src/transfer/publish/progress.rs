use gat_core::progress::{ProgressActivity, ProgressHandle, PublicationProgress};
#[cfg(test)]
use tokio::time::Duration;
use tokio::time::Instant;

use crate::progress_reporting;
#[cfg(test)]
use crate::progress_reporting::REFRESH_INTERVAL;

/// Coordinator-owned reporting shared across all windows of a push.
///
/// Retain one instance for the complete operation so cumulative totals survive
/// window boundaries. Workers must not update its task's publication activity.
pub struct PublishProgress {
    task: ProgressHandle,
    counts: PublicationProgress,
    enabled: bool,
    last: Option<(PublicationProgress, Instant)>,
}

impl PublishProgress {
    #[must_use]
    pub fn new(task: ProgressHandle) -> Self {
        Self {
            enabled: task.is_enabled(),
            task,
            counts: PublicationProgress::default(),
            last: None,
        }
    }

    pub(super) const fn task(&self) -> &ProgressHandle {
        &self.task
    }

    pub(super) const fn checking_started(&mut self) {
        if self.enabled {
            self.counts.checking += 1;
        }
    }

    pub(super) const fn checking_finished(&mut self, success: bool) {
        if self.enabled {
            self.counts.checking -= 1;
            if success {
                self.counts.checked += 1;
            }
        }
    }

    pub(super) const fn uploading_started(&mut self) {
        if self.enabled {
            self.counts.uploading += 1;
        }
    }

    pub(super) const fn uploading_finished(&mut self) {
        if self.enabled {
            self.counts.uploading -= 1;
        }
    }

    pub(super) const fn verifying(&mut self, count: usize) {
        if self.enabled {
            self.counts.verifying = count as u64;
        }
    }

    pub(super) const fn complete(&mut self, status: super::PublishStatus) {
        if !self.enabled {
            return;
        }
        match status {
            super::PublishStatus::AlreadyPresent => self.counts.already_present += 1,
            super::PublishStatus::Uploaded => self.counts.uploaded += 1,
            super::PublishStatus::CacheMissing | super::PublishStatus::CacheCorrupt => {
                self.counts.rejected += 1;
            }
        }
    }

    pub(super) fn report(&mut self, force: bool) {
        if !self.enabled {
            return;
        }
        let active = |s: PublicationProgress| (s.checking != 0, s.verifying != 0, s.uploading != 0);
        // Count-only changes are published by the refresh timer. Phase changes
        // and final flushes publish immediately, without per-result clock reads.
        if self.last.is_some_and(|(previous, _)| {
            previous == self.counts || (!force && active(previous) == active(self.counts))
        }) {
            return;
        }
        let completed = |s: PublicationProgress| s.already_present + s.uploaded + s.rejected;
        let delta =
            completed(self.counts) - self.last.map_or(0, |(previous, _)| completed(previous));
        if delta != 0 {
            self.task.inc(delta);
        }
        self.task
            .set_activity(ProgressActivity::Publishing(self.counts));
        self.last = Some((self.counts, Instant::now()));
    }
}

impl progress_reporting::Refresh for PublishProgress {
    fn deadline(&self) -> Option<Instant> {
        progress_reporting::deadline(self.last, self.counts)
    }
    fn flush(&mut self) {
        self.report(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress_reporting::Refresh;
    use futures::FutureExt;
    use gat_core::progress::{ActivityBackend, ProgressTask};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingBackend {
        activities: Mutex<Vec<PublicationProgress>>,
        increments: Mutex<Vec<u64>>,
    }
    impl ActivityBackend for RecordingBackend {
        fn inc(&self, delta: u64) {
            self.increments.lock().unwrap().push(delta);
        }
        fn set_activity(&self, activity: &ProgressActivity) {
            if let ProgressActivity::Publishing(counts) = activity {
                self.activities.lock().unwrap().push(*counts);
            }
        }
        fn finish(&self) {}
    }

    #[tokio::test(start_paused = true)]
    async fn reporting_throttles_counts_but_never_hides_activity_transitions() {
        let backend = Arc::new(RecordingBackend::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut progress = PublishProgress::new(task.handle());
        let now = Instant::now();
        progress.counts.checking = 100;
        progress.report(false);
        for checked in 1..50 {
            progress.counts.checked = checked;
            progress.counts.checking -= 1;
            progress.report(false);
        }
        assert_eq!(backend.activities.lock().unwrap().len(), 1);
        assert_eq!(progress.deadline(), Some(now + Duration::from_millis(100)));

        progress.counts.verifying = 12;
        progress.report(false);
        progress.counts.uploading = 8;
        progress.report(false);
        {
            let snapshots = backend.activities.lock().unwrap();
            assert_eq!(snapshots.len(), 3);
            assert_eq!(snapshots[2].verifying, 12);
            assert_eq!(snapshots[2].uploading, 8);
        }

        progress.counts.uploading = 7;
        progress.report(false);
        assert_eq!(backend.activities.lock().unwrap().len(), 3);
        tokio::time::advance(REFRESH_INTERVAL).await;
        progress.flush();
        assert_eq!(
            backend.activities.lock().unwrap().last().unwrap().uploading,
            7
        );
        assert_eq!(progress.deadline(), None);

        progress.counts.verifying = 0;
        progress.counts.uploading = 0;
        progress.counts.checking = 0;
        progress.flush();
        assert_eq!(
            backend.activities.lock().unwrap().last().unwrap().verifying,
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_reports_preserve_the_deadline_and_flush_entry_progress() {
        let backend = Arc::new(RecordingBackend::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut progress = PublishProgress::new(task.handle());
        progress.counts.checking = 2;
        progress.report(false);
        let first = progress.last;
        tokio::time::advance(Duration::from_millis(50)).await;
        progress.report(false);
        progress.report(true);
        assert_eq!(progress.last, first);
        assert_eq!(backend.activities.lock().unwrap().len(), 1);

        // Successful outcomes determine both summary and position. Repeated
        // count-only reports must not invoke the backend per completion.
        progress.counts.checking = 200;
        for _ in 0..100 {
            progress.checking_finished(true);
            progress.complete(super::super::PublishStatus::AlreadyPresent);
            progress.report(false);
        }
        assert!(backend.increments.lock().unwrap().is_empty());
        assert_eq!(progress.last, first);
        assert_eq!(
            progress.deadline(),
            first.map(|(_, when)| when + REFRESH_INTERVAL)
        );
        progress.flush();
        assert_eq!(*backend.increments.lock().unwrap(), [100]);
        progress.flush();
        assert_eq!(*backend.increments.lock().unwrap(), [100]);

        progress.complete(super::super::PublishStatus::Uploaded);
        progress.complete(super::super::PublishStatus::CacheMissing);
        progress.complete(super::super::PublishStatus::CacheCorrupt);
        progress.flush();
        assert_eq!(*backend.increments.lock().unwrap(), [100, 3]);
    }

    #[test]
    fn disabled_reporting_has_no_refresh_deadline() {
        use gat_core::progress::{NoopProgress, ProgressOperation, ProgressReporter, ProgressSpec};
        let task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Pushing));
        let mut progress = PublishProgress::new(task.handle());
        progress.checking_started();
        progress.checking_finished(true);
        progress.uploading_started();
        progress.uploading_finished();
        progress.verifying(12);
        progress.complete(super::super::PublishStatus::Uploaded);
        assert_eq!(progress.counts, PublicationProgress::default());
        progress.report(false);
        assert!(progress.last.is_none());
        assert!(progress.deadline().is_none());
        let mut timer = None;
        let mut completions = futures::stream::iter([7]);
        assert_eq!(
            futures::executor::block_on(progress.next(&mut completions, &mut timer)),
            Some(7)
        );
        assert!(
            timer.is_none(),
            "disabled reporting must work without a Tokio timer runtime"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_completion_refreshes_and_reuses_the_timer() {
        let backend = Arc::new(RecordingBackend::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut progress = PublishProgress::new(task.handle());
        progress.counts.checking = 100;
        progress.report(false);
        let (sender, mut completions) = futures::channel::mpsc::unbounded();
        let mut timer = None;

        let mut timer_address = None;
        for checked in [1, 2] {
            progress.counts.checked = checked;
            progress.report(false);
            {
                let mut next = std::pin::pin!(progress.next(&mut completions, &mut timer));
                assert!(next.as_mut().now_or_never().is_none());
                tokio::time::advance(REFRESH_INTERVAL).await;
                assert!(next.as_mut().now_or_never().is_none());
                assert_eq!(
                    backend.activities.lock().unwrap().last().unwrap().checked,
                    checked
                );
                sender.unbounded_send(checked).unwrap();
                assert_eq!(next.await, Some(checked));
            }
            let address = std::ptr::from_ref(timer.as_ref().unwrap().as_ref().get_ref());
            assert_eq!(*timer_address.get_or_insert(address), address);
        }
        drop(sender);
        assert_eq!(progress.next(&mut completions, &mut timer).await, None);
    }
}
