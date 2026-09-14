use futures::Stream;
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
    pub(super) task: ProgressHandle,
    counts: PublicationProgress,
    enabled: bool,
    completed: u64,
    last: Option<(PublicationProgress, Instant)>,
}

impl PublishProgress {
    #[must_use]
    pub fn new(task: ProgressHandle) -> Self {
        Self {
            enabled: task.is_enabled(),
            task,
            counts: PublicationProgress::default(),
            completed: 0,
            last: None,
        }
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
        self.completed += 1;
        match status {
            super::PublishStatus::AlreadyPresent => self.counts.already_present += 1,
            super::PublishStatus::Uploaded => self.counts.uploaded += 1,
            super::PublishStatus::CacheMissing | super::PublishStatus::CacheCorrupt => {
                self.counts.rejected += 1;
            }
        }
    }

    /// Wait for useful work, publishing a deferred snapshot if I/O stalls.
    ///
    /// The caller retains one lazily allocated timer for the window. Reusing
    /// its pinned allocation and registration avoids timer churn while draining
    /// fast completion streams. Disabled reporting never creates a timer.
    pub(super) async fn next_completion<S: Stream + Unpin>(
        &mut self,
        completions: &mut S,
        timer: &mut progress_reporting::RefreshTimer,
    ) -> Option<S::Item> {
        progress_reporting::next_with_refresh(self, completions, timer).await
    }

    pub(super) fn report(&mut self, force: bool) {
        if !self.enabled {
            self.completed = 0;
            return;
        }
        // Verification dispatch and remote refill can report the same snapshot
        // in one iteration. Skip the clock and backend when neither text nor
        // the separately batched entry position needs updating.
        if self.completed == 0 && self.last.is_some_and(|(counts, _)| counts == self.counts) {
            return;
        }
        self.report_at(force, Instant::now());
    }

    fn refresh_deadline(&self) -> Option<Instant> {
        progress_reporting::deadline(self.last, self.counts)
    }

    fn report_at(&mut self, force: bool, now: Instant) {
        if self.completed != 0 {
            self.task.inc(std::mem::take(&mut self.completed));
        }
        let active = |s: PublicationProgress| (s.checking != 0, s.verifying != 0, s.uploading != 0);
        let publish = progress_reporting::due(self.last, self.counts, active, force, now);
        if publish {
            self.task
                .set_activity(ProgressActivity::Publishing(self.counts));
            self.last = Some((self.counts, now));
        }
    }
}

impl progress_reporting::Refresh for PublishProgress {
    fn deadline(&self) -> Option<Instant> {
        self.refresh_deadline()
    }
    fn flush(&mut self) {
        self.report(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn progress_handle(backend: &Arc<RecordingBackend>) -> ProgressHandle {
        ProgressTask::from_backend(Arc::clone(backend) as Arc<dyn ActivityBackend>).handle()
    }

    #[test]
    fn reporting_throttles_counts_but_never_hides_activity_transitions() {
        let backend = Arc::new(RecordingBackend::default());
        let mut progress = PublishProgress::new(progress_handle(&backend));
        let now = Instant::now();
        progress.counts.checking = 100;
        progress.report_at(false, now);
        for checked in 1..50 {
            progress.counts.checked = checked;
            progress.counts.checking -= 1;
            progress.report_at(false, now);
        }
        assert_eq!(backend.activities.lock().unwrap().len(), 1);
        assert_eq!(
            progress.refresh_deadline(),
            Some(now + Duration::from_millis(100))
        );

        progress.counts.verifying = 12;
        progress.report_at(false, now);
        progress.counts.uploading = 8;
        progress.report_at(false, now);
        let snapshots = backend.activities.lock().unwrap();
        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[2].verifying, 12);
        assert_eq!(snapshots[2].uploading, 8);
        drop(snapshots);

        progress.counts.uploading = 7;
        progress.report_at(false, now);
        assert_eq!(backend.activities.lock().unwrap().len(), 3);
        progress.report_at(false, now + Duration::from_millis(100));
        assert_eq!(
            backend.activities.lock().unwrap().last().unwrap().uploading,
            7
        );
        assert_eq!(progress.refresh_deadline(), None);

        progress.counts.verifying = 0;
        progress.counts.uploading = 0;
        progress.counts.checking = 0;
        progress.report_at(true, now + Duration::from_millis(100));
        assert_eq!(
            backend.activities.lock().unwrap().last().unwrap().verifying,
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_reports_preserve_the_deadline_and_flush_entry_progress() {
        let backend = Arc::new(RecordingBackend::default());
        let mut progress = PublishProgress::new(progress_handle(&backend));
        progress.counts.checking = 2;
        progress.report(false);
        let first = progress.last;
        tokio::time::advance(Duration::from_millis(50)).await;
        progress.report(false);
        progress.report(true);
        assert_eq!(progress.last, first);
        assert_eq!(backend.activities.lock().unwrap().len(), 1);

        // A duplicate snapshot must not swallow separately batched increments.
        progress.completed = 2;
        progress.report(false);
        assert_eq!(*backend.increments.lock().unwrap(), [2]);
        assert_eq!(progress.completed, 0);
        assert_eq!(backend.activities.lock().unwrap().len(), 1);

        progress.counts.checked = 1;
        assert_eq!(
            progress.refresh_deadline(),
            first.map(|(_, when)| when + REFRESH_INTERVAL)
        );
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
        assert_eq!(progress.completed, 0);
        progress.report(false);
        assert!(progress.last.is_none());
        assert!(progress.refresh_deadline().is_none());
        let mut timer = None;
        let mut completions = futures::stream::iter([7]);
        assert_eq!(
            futures::executor::block_on(progress.next_completion(&mut completions, &mut timer)),
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
        let mut progress = PublishProgress::new(progress_handle(&backend));
        progress.counts.checking = 100;
        progress.report(false);
        let (sender, mut completions) = futures::channel::mpsc::unbounded();
        let mut timer = None;

        let mut timer_address = None;
        for checked in [1, 2] {
            progress.counts.checked = checked;
            progress.report(false);
            {
                let mut next =
                    std::pin::pin!(progress.next_completion(&mut completions, &mut timer));
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
        assert_eq!(
            progress.next_completion(&mut completions, &mut timer).await,
            None
        );
    }
}
