use crate::progress_reporting::{self, Refresh};
use gat_core::progress::{ProgressActivity, ProgressHandle};
use tokio::time::Instant;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    present: u64,
    missing: u64,
}

struct Enabled {
    task: ProgressHandle,
    counts: Counts,
    completed: u64,
    last: Option<(Counts, Instant)>,
}

/// Cumulative presence results across remote-status windows. The item position
/// counts successful checks; a failed request is neither present nor missing.
/// Disabled progress retains no task, counters, clock, or timer state.
#[derive(Default)]
pub struct PresenceProgress {
    enabled: Option<Enabled>,
}

impl PresenceProgress {
    #[must_use]
    pub fn new(task: ProgressHandle) -> Self {
        Self {
            enabled: task.is_enabled().then_some(Enabled {
                task,
                counts: Counts::default(),
                completed: 0,
                last: None,
            }),
        }
    }

    pub(super) fn task(&self) -> Option<&ProgressHandle> {
        self.enabled.as_ref().map(|state| &state.task)
    }

    pub(super) fn resume(&mut self) {
        if let Some(state) = &mut self.enabled {
            state.last = None;
        }
        self.flush();
    }

    pub(super) const fn completed(&mut self, present: bool) {
        if let Some(state) = &mut self.enabled {
            if present {
                state.counts.present += 1;
            } else {
                state.counts.missing += 1;
            }
            state.completed += 1;
        }
    }
}

impl Refresh for PresenceProgress {
    fn deadline(&self) -> Option<Instant> {
        self.enabled
            .as_ref()
            .and_then(|state| progress_reporting::deadline(state.last, state.counts))
    }

    fn flush(&mut self) {
        let Some(state) = &mut self.enabled else {
            return;
        };
        if state.last.is_some_and(|(counts, _)| counts == state.counts) {
            return;
        }
        if state.completed != 0 {
            state.task.inc(std::mem::take(&mut state.completed));
        }
        state.task.set_activity(ProgressActivity::RemotePresence {
            present: state.counts.present,
            missing: state.counts.missing,
        });
        state.last = Some((state.counts, Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use gat_core::progress::{ActivityBackend, ProgressTask};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recording {
        events: Mutex<Vec<ProgressActivity>>,
        position: Mutex<u64>,
    }
    impl ActivityBackend for Recording {
        fn inc(&self, delta: u64) {
            *self.position.lock().unwrap() += delta;
        }
        fn set_activity(&self, activity: &ProgressActivity) {
            self.events.lock().unwrap().push(activity.clone());
        }
        fn finish(&self) {}
    }

    #[tokio::test(start_paused = true)]
    async fn count_updates_flush_while_remaining_checks_stall_and_survive_windows() {
        let backend = Arc::new(Recording::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut progress = PresenceProgress::new(task.handle());
        progress.resume();
        progress.completed(true);
        progress.completed(false);
        assert_eq!(*backend.position.lock().unwrap(), 0);
        let mut pending = futures::stream::pending::<()>();
        let mut timer = None;
        {
            let next = progress.next(&mut pending, &mut timer);
            tokio::pin!(next);
            assert!(next.as_mut().now_or_never().is_none());
            tokio::time::advance(std::time::Duration::from_millis(100)).await;
            assert!(next.as_mut().now_or_never().is_none());
            assert_eq!(*backend.position.lock().unwrap(), 2);
        }
        assert!(timer.is_some());
        task.set_activity(ProgressActivity::Connecting);
        progress.resume();
        assert_eq!(
            backend.events.lock().unwrap().last(),
            Some(&ProgressActivity::RemotePresence {
                present: 1,
                missing: 1
            })
        );
        progress.completed(true);
        progress.flush();
        assert_eq!(*backend.position.lock().unwrap(), 3);
        assert_eq!(
            backend.events.lock().unwrap().last(),
            Some(&ProgressActivity::RemotePresence {
                present: 2,
                missing: 1
            })
        );
        assert!(progress.deadline().is_none());
    }

    #[test]
    fn disabled_presence_reporting_needs_no_timer_runtime() {
        let mut progress = PresenceProgress::default();
        progress.resume();
        progress.completed(true);
        let mut timer = None;
        assert_eq!(
            futures::executor::block_on(progress.next(&mut futures::stream::iter([7]), &mut timer)),
            Some(7)
        );
        assert!(timer.is_none());
        assert!(progress.deadline().is_none());
    }
}
