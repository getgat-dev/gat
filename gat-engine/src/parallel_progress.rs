//! One publication owner samples parallel work, with explicit phase barriers.

use gat_core::progress::{ProgressHandle, WorkCounter, WorkProgress};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

enum Phase {
    Paused,
    Running,
}

struct Sampler {
    work: Arc<WorkCounter>,
    task: ProgressHandle,
    commands: mpsc::Receiver<Phase>,
    acknowledged: mpsc::SyncSender<()>,
}

impl Sampler {
    fn report(&self) {
        let delta = self.work.take_pending();
        if delta != 0 {
            self.task.inc(delta);
        }
    }

    // This thread alone owns publication state. No backend callback runs under
    // a progress lock. An idle phase blocks indefinitely rather than polling.
    fn run(self) {
        let mut phase = Phase::Paused;
        loop {
            phase = match phase {
                Phase::Paused => match self.commands.recv() {
                    Ok(command) => command,
                    Err(_) => break,
                },
                Phase::Running => match self
                    .commands
                    .recv_timeout(crate::progress_reporting::REFRESH_INTERVAL)
                {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.report();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
            };
            match phase {
                // Earlier work was flushed on pause; accumulate new work
                // until the refresh deadline or the next pause.
                Phase::Running => {}
                Phase::Paused => {
                    self.report();
                    if self.acknowledged.send(()).is_err() {
                        break;
                    }
                }
            }
        }
    }
}

struct Enabled {
    work: Arc<WorkCounter>,
    commands: mpsc::SyncSender<Phase>,
    acknowledged: mpsc::Receiver<()>,
    thread: JoinHandle<()>,
}

/// One optional sampler for a logical operation, reused across windows. Worker
/// accounting is atomic. Disabled reporting and thread-creation failure use the
/// same no-op path; optional progress must not prevent useful work from running.
pub struct ParallelProgress {
    enabled: Option<Enabled>,
}

impl ParallelProgress {
    #[must_use]
    pub fn new(task: ProgressHandle) -> Self {
        Self::with_spawner(task, |sampler| {
            std::thread::Builder::new()
                .name("gat-progress".into())
                .spawn(move || sampler.run())
        })
    }

    fn with_spawner(
        task: ProgressHandle,
        spawn: impl FnOnce(Sampler) -> std::io::Result<JoinHandle<()>>,
    ) -> Self {
        let enabled = task
            .is_enabled()
            .then(|| {
                let work = Arc::new(WorkCounter::default());
                // Each run sends two phase transitions before waiting for the
                // acknowledgement. A fixed queue avoids per-window allocations.
                let (commands, receiver) = mpsc::sync_channel(2);
                let (acknowledge, acknowledged) = mpsc::sync_channel(0);
                let thread = spawn(Sampler {
                    work: Arc::clone(&work),
                    task,
                    commands: receiver,
                    acknowledged: acknowledge,
                })
                .ok()?;
                Some(Enabled {
                    work,
                    commands,
                    acknowledged,
                    thread,
                })
            })
            .flatten();
        Self { enabled }
    }

    /// Flush and acknowledge completion before the next phase, including on
    /// errors and unwinding. Workers must finish within the closure.
    ///
    /// ```compile_fail
    /// let mut progress = gat_engine::ParallelProgress::new(Default::default());
    /// let escaped = progress.run(|work| *work);
    /// escaped.inc(1); // Workers cannot outlive the final flush barrier.
    /// ```
    pub fn run<T>(&mut self, run: impl FnOnce(&WorkProgress<'_>) -> T) -> T {
        let Some(enabled) = &self.enabled else {
            return run(&WorkProgress::default());
        };
        if enabled.commands.send(Phase::Running).is_err() {
            return run(&WorkProgress::default());
        }
        struct Pause<'a>(&'a Enabled);
        impl Drop for Pause<'_> {
            fn drop(&mut self) {
                if self.0.commands.send(Phase::Paused).is_ok() {
                    // A failed backend closes the acknowledgement channel.
                    // Do not turn its failure into a second panic during unwind.
                    let _ = self.0.acknowledged.recv();
                }
            }
        }
        let _pause = Pause(enabled);
        run(&enabled.work.handle())
    }
}

impl Drop for ParallelProgress {
    fn drop(&mut self) {
        if let Some(Enabled {
            commands,
            acknowledged,
            thread,
            ..
        }) = self.enabled.take()
        {
            drop(commands);
            drop(acknowledged);
            // Backend panics must not replace an operation error during unwind.
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::progress::{ActivityBackend, ProgressActivity, ProgressTask};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    #[derive(Default)]
    struct Recording {
        position: AtomicU64,
        finished: AtomicBool,
        activity: Mutex<Option<ProgressActivity>>,
    }
    impl ActivityBackend for Recording {
        fn inc(&self, delta: u64) {
            assert!(!self.finished.load(Ordering::Relaxed));
            self.position.fetch_add(delta, Ordering::Relaxed);
        }
        fn set_activity(&self, activity: &ProgressActivity) {
            *self.activity.lock().unwrap() = Some(activity.clone());
        }
        fn finish(&self) {
            self.finished.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn windows_flush_before_next_phase_and_preserve_partial_error_counts() {
        let backend = Arc::new(Recording::default());
        let task = ProgressTask::from_backend(backend.clone());
        let mut progress = ParallelProgress::new(task.handle());
        for expected in [3, 6] {
            let result: Result<(), ()> = progress.run(|work| {
                std::thread::scope(|scope| {
                    scope.spawn(|| work.inc(1));
                    scope.spawn(|| work.inc(2));
                });
                Err(())
            });
            assert_eq!(result, Err(()));
            assert_eq!(backend.position.load(Ordering::Relaxed), expected);
            task.set_activity(ProgressActivity::CheckingReuseStatus);
            assert_eq!(
                *backend.activity.lock().unwrap(),
                Some(ProgressActivity::CheckingReuseStatus)
            );
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            progress.run(|work| {
                work.inc(1);
                panic!("original operation panic");
            })
        }));
        assert_eq!(backend.position.load(Ordering::Relaxed), 7);
        drop(progress);
        task.finish();
    }

    #[test]
    fn disabled_and_failed_startup_run_without_worker_accounting() {
        let mut disabled = ParallelProgress::with_spawner(ProgressHandle::default(), |_| {
            panic!("disabled reporting started a sampler");
        });
        disabled.run(|work| {
            work.inc(10);
        });
        let task = ProgressTask::from_backend(Arc::new(Recording::default()));
        let mut failed = ParallelProgress::with_spawner(task.handle(), |_| {
            Err(std::io::Error::other("unavailable"))
        });
        failed.run(|work| {
            work.inc(10);
        });
    }

    #[test]
    fn backend_panic_does_not_replace_operation_failure() {
        struct Broken;
        impl ActivityBackend for Broken {
            fn inc(&self, _: u64) {
                panic!("broken backend");
            }
            fn set_activity(&self, _: &ProgressActivity) {}
            fn finish(&self) {}
        }
        let task = ProgressTask::from_backend(Arc::new(Broken));
        let mut progress = ParallelProgress::new(task.handle());
        let result: Result<(), &str> = progress.run(|work| {
            work.inc(1);
            Err("original error")
        });
        assert_eq!(result, Err("original error"));
        drop(progress);
    }
}
