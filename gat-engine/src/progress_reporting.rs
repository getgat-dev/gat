//! Coalesced item increments shared by every transfer coordinator.
use gat_core::progress::{ProgressActivity, ProgressHandle};
use tokio::time::{Duration, Instant};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

/// Pending increments for one logical task. The caller decides what counts as
/// an item; this publisher knows no operation-specific outcomes or activities.
/// No terminal position, total, or message is mirrored here.
#[derive(Default)]
pub struct ProgressUpdates {
    task: ProgressHandle,
    enabled: bool,
    pending: u64,
    deadline: Option<Instant>,
    timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl ProgressUpdates {
    #[must_use]
    pub fn new(task: ProgressHandle) -> Self {
        Self {
            enabled: task.is_enabled(),
            task,
            pending: 0,
            deadline: None,
            timer: None,
        }
    }

    pub(crate) const fn task(&self) -> &ProgressHandle {
        &self.task
    }

    /// Accumulate without a clock read or backend call per completion. Only the
    /// first increment after a flush arms the next publication deadline.
    pub fn inc(&mut self, delta: u64) {
        if delta == 0 || !self.enabled {
            return;
        }
        self.pending += delta;
        if self.deadline.is_none() {
            self.deadline = Some(Instant::now() + REFRESH_INTERVAL);
        }
    }

    /// Publish pending work before a sequential activity change.
    pub(crate) fn set_activity(&mut self, activity: ProgressActivity) {
        self.flush();
        self.task.set_activity(activity);
    }

    /// Flush at window/phase completion, including partial error outcomes.
    pub fn flush(&mut self) {
        if self.pending != 0 {
            self.task.inc(std::mem::take(&mut self.pending));
        }
        self.deadline = None;
    }

    /// Keep reporting while any coordinator wait is pending, including waits
    /// for another window to release capacity. Retain one timer across windows.
    pub(crate) async fn wait<F: std::future::Future>(&mut self, future: F) -> F::Output {
        tokio::pin!(future);
        if self.deadline.is_some() {
            tokio::select! {
                biased;
                () = self.refresh() => {},
                result = &mut future => return result,
            }
        }
        // No new increments can arrive while this wait borrows the publisher.
        future.await
    }

    pub(crate) async fn refresh(&mut self) {
        let Some(deadline) = self.deadline else {
            return std::future::pending().await;
        };
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        if timer.deadline() != deadline {
            timer.as_mut().reset(deadline);
        }
        timer.as_mut().await;
        self.flush();
    }

    /// The executor supplies results; the operation supplies their item weight.
    pub(crate) fn observe<R>(
        &mut self,
        weight: impl Fn(usize, &R) -> u64,
    ) -> impl crate::remote_executor::TransferObserver<R> {
        struct Observer<'a, F> {
            updates: &'a mut ProgressUpdates,
            weight: F,
        }
        impl<R, F: Fn(usize, &R) -> u64> crate::remote_executor::TransferObserver<R> for Observer<'_, F> {
            fn completed(&mut self, index: usize, result: &R) {
                if self.updates.enabled {
                    self.updates.inc((self.weight)(index, result));
                }
            }
            fn flush(&mut self) {
                self.updates.flush();
            }
            async fn refresh(&mut self) {
                self.updates.refresh().await;
            }
        }
        Observer {
            updates: self,
            weight,
        }
    }
}

impl Drop for ProgressUpdates {
    fn drop(&mut self) {
        // Optional reporting must not cause a second panic during cleanup.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.flush()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_executor::TransferObserver;
    use futures::{FutureExt, StreamExt};
    use gat_core::progress::{ActivityBackend, ProgressTask};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recording {
        increments: Mutex<Vec<u64>>,
        activities: Mutex<Vec<ProgressActivity>>,
    }
    impl ActivityBackend for Recording {
        fn inc(&self, delta: u64) {
            self.increments.lock().unwrap().push(delta);
        }
        fn set_activity(&self, activity: &ProgressActivity) {
            self.activities.lock().unwrap().push(activity.clone());
        }
        fn finish(&self) {}
    }

    #[tokio::test(start_paused = true)]
    async fn coalesces_completions_and_reuses_timer_while_other_work_stalls() {
        let backend = Arc::new(Recording::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut updates = ProgressUpdates::new(task.handle());
        let (sender, mut stream) = futures::channel::mpsc::unbounded();
        let mut address = None;
        for window in 0..2 {
            updates.inc(1);
            let deadline = updates.deadline;
            for _ in 0..99 {
                updates.inc(1);
            }
            assert_eq!(updates.deadline, deadline);
            assert_eq!(backend.increments.lock().unwrap().len(), window);
            {
                let mut next = std::pin::pin!(updates.wait(stream.next()));
                assert!(next.as_mut().now_or_never().is_none());
                tokio::time::advance(REFRESH_INTERVAL).await;
                assert!(next.as_mut().now_or_never().is_none());
                assert_eq!(backend.increments.lock().unwrap().len(), window + 1);
                sender.unbounded_send(window).unwrap();
                assert_eq!(next.await, Some(window));
            }
            let current = std::ptr::from_ref(updates.timer.as_ref().unwrap().as_ref().get_ref());
            assert_eq!(*address.get_or_insert(current), current);
            assert_eq!(updates.deadline, None);
        }
        assert_eq!(*backend.increments.lock().unwrap(), [100, 100]);
        updates.flush();
        assert_eq!(*backend.increments.lock().unwrap(), [100, 100]);
    }

    #[test]
    fn phases_and_early_exit_flush_pending_deltas_once() {
        let backend = Arc::new(Recording::default());
        let task = ProgressTask::from_backend(backend.clone());
        let run = || -> Result<(), ()> {
            let mut updates = ProgressUpdates::new(task.handle());
            updates.inc(3);
            updates.set_activity(ProgressActivity::Working);
            assert_eq!(*backend.increments.lock().unwrap(), [3]);
            updates.inc(2);
            Err(())
        };
        assert_eq!(run(), Err(()));
        assert_eq!(*backend.increments.lock().unwrap(), [3, 2]);
        assert_eq!(
            *backend.activities.lock().unwrap(),
            [ProgressActivity::Working]
        );
    }

    #[test]
    fn completion_weights_belong_to_the_operation() {
        let backend = Arc::new(Recording::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut updates = ProgressUpdates::new(task.handle());
        {
            let mut observer =
                updates.observe(|_, result: &Result<(), ()>| u64::from(result.is_ok()));
            observer.completed(0, &Ok(()));
            observer.completed(1, &Err(()));
        }
        updates.flush();
        assert_eq!(*backend.increments.lock().unwrap(), [1]);
        {
            let mut observer = updates.observe(|index, _: &Result<(), ()>| [3, 2][index]);
            observer.completed(0, &Ok(()));
            observer.completed(1, &Err(()));
            observer.flush();
        }
        assert_eq!(*backend.increments.lock().unwrap(), [1, 5]);
    }

    #[test]
    fn disabled_reporting_needs_no_runtime_timer_or_weights() {
        let mut updates = ProgressUpdates::default();
        updates.inc(10);
        updates
            .observe(|_, (): &()| panic!("disabled reporting evaluated a weight"))
            .completed(0, &());
        let mut stream = futures::stream::iter([7]);
        assert_eq!(
            futures::executor::block_on(updates.wait(stream.next())),
            Some(7)
        );
        assert!(updates.timer.is_none());
        assert!(updates.deadline.is_none());
        assert_eq!(updates.pending, 0);
    }
}
