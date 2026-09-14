//! One publication owner samples parallel work, with explicit phase barriers.

use gat_core::progress::{ProgressActivity, ProgressHandle, WorkCounts, WorkProgress};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

#[derive(Clone, Copy)]
pub enum ParallelWork {
    Hashing,
    InspectingRepositories,
}

impl ParallelWork {
    const fn activity(self, counts: WorkCounts) -> ProgressActivity {
        match self {
            Self::Hashing => ProgressActivity::HashingFiles(counts),
            Self::InspectingRepositories => ProgressActivity::InspectingRepositories(counts),
        }
    }

    const fn position(self, counts: WorkCounts) -> u64 {
        match self {
            Self::Hashing => counts.succeeded(),
            Self::InspectingRepositories => counts.completed(),
        }
    }
}

enum Command {
    Resume,
    Pause,
}

enum Phase {
    Paused,
    Running { last: Option<WorkCounts> },
}

struct Sampler {
    work: Arc<WorkProgress>,
    task: ProgressHandle,
    kind: ParallelWork,
    commands: mpsc::Receiver<Command>,
    acknowledged: mpsc::SyncSender<()>,
}

impl Sampler {
    fn report(&self, phase: &mut Phase, position: &mut u64) {
        if let Phase::Running { last } = phase {
            let counts = self.work.snapshot();
            if *last != Some(counts) {
                let next = self.kind.position(counts);
                if next != *position {
                    self.task.inc(next - *position);
                    *position = next;
                }
                self.task.set_activity(self.kind.activity(counts));
                *last = Some(counts);
            }
        }
    }

    // This thread alone owns publication state. No backend callback runs under
    // a progress lock. An idle phase blocks indefinitely rather than polling.
    fn run(self) {
        let mut phase = Phase::Paused;
        let mut position = 0;
        loop {
            let command = match phase {
                Phase::Paused => match self.commands.recv() {
                    Ok(command) => command,
                    Err(_) => break,
                },
                Phase::Running { .. } => match self
                    .commands
                    .recv_timeout(crate::progress_reporting::REFRESH_INTERVAL)
                {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        self.report(&mut phase, &mut position);
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
            };
            match command {
                Command::Resume => {
                    phase = Phase::Running { last: None };
                    self.report(&mut phase, &mut position);
                }
                Command::Pause => {
                    self.report(&mut phase, &mut position);
                    phase = Phase::Paused;
                    if self.acknowledged.send(()).is_err() {
                        break;
                    }
                }
            }
        }
    }
}

struct Worker {
    commands: mpsc::SyncSender<Command>,
    acknowledged: mpsc::Receiver<()>,
    thread: JoinHandle<()>,
}

struct Enabled {
    work: Arc<WorkProgress>,
    worker: Option<Worker>,
}

/// One optional sampler for a logical operation, reused across windows. Worker
/// accounting is atomic. Disabled reporting and thread-creation failure use the
/// same no-op path; optional progress must not prevent useful work from running.
pub struct ParallelProgress {
    enabled: Option<Enabled>,
}

impl ParallelProgress {
    #[must_use]
    pub fn new(task: ProgressHandle, kind: ParallelWork) -> Self {
        Self::with_spawner(task, kind, |sampler| {
            std::thread::Builder::new()
                .name("gat-progress".into())
                .spawn(move || sampler.run())
        })
    }

    fn with_spawner(
        task: ProgressHandle,
        kind: ParallelWork,
        spawn: impl FnOnce(Sampler) -> std::io::Result<JoinHandle<()>>,
    ) -> Self {
        let enabled = task
            .is_enabled()
            .then(|| {
                let work = Arc::new(WorkProgress::enabled());
                // Each run sends at most Resume and Pause before waiting for the
                // acknowledgement. A fixed queue avoids per-window allocations.
                let (commands, receiver) = mpsc::sync_channel(2);
                let (acknowledge, acknowledged) = mpsc::sync_channel(0);
                let thread = spawn(Sampler {
                    work: Arc::clone(&work),
                    task,
                    kind,
                    commands: receiver,
                    acknowledged: acknowledge,
                })
                .ok()?;
                Some(Enabled {
                    work,
                    worker: Some(Worker {
                        commands,
                        acknowledged,
                        thread,
                    }),
                })
            })
            .flatten();
        Self { enabled }
    }

    /// Flush and acknowledge the end of this phase before the caller proceeds,
    /// including on error or unwind. Counts survive windows. Work items cannot
    /// escape the closure and outlive the final snapshot.
    ///
    /// ```compile_fail
    /// use gat_core::progress::{NoopProgress, ProgressOperation, ProgressReporter, ProgressSpec};
    /// use gat_engine::{ParallelProgress, ParallelWork};
    /// let task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
    /// let mut progress = ParallelProgress::new(task.handle(), ParallelWork::Hashing);
    /// let unfinished = progress.run(|work| work.start());
    /// ```
    pub fn run<T>(&mut self, run: impl FnOnce(&WorkProgress) -> T) -> T {
        let Some(enabled) = &self.enabled else {
            return run(&WorkProgress::default());
        };
        let Some(worker) = &enabled.worker else {
            return run(&WorkProgress::default());
        };
        if worker.commands.send(Command::Resume).is_err() {
            return run(&WorkProgress::default());
        }
        struct Pause<'a>(&'a Worker);
        impl Drop for Pause<'_> {
            fn drop(&mut self) {
                if self.0.commands.send(Command::Pause).is_ok() {
                    // A failed backend closes the acknowledgement channel.
                    // Do not turn its failure into a second panic during unwind.
                    let _ = self.0.acknowledged.recv();
                }
            }
        }
        let _pause = Pause(worker);
        run(&enabled.work)
    }
}

impl Drop for Enabled {
    fn drop(&mut self) {
        if let Some(Worker {
            commands,
            acknowledged,
            thread,
        }) = self.worker.take()
        {
            drop(commands);
            drop(acknowledged);
            // Backend panics are confined to optional reporting, never promoted
            // to operation failures (especially while another error unwinds).
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::progress::{
        ActivityBackend, NoopProgress, ProgressOperation, ProgressReporter, ProgressSpec,
        ProgressTask,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct Recording {
        position: AtomicU64,
        activities: Mutex<Vec<ProgressActivity>>,
        observed: mpsc::Sender<WorkCounts>,
        finished: AtomicBool,
    }
    impl ActivityBackend for Recording {
        fn inc(&self, delta: u64) {
            assert!(!self.finished.load(Ordering::Relaxed));
            self.position.fetch_add(delta, Ordering::Relaxed);
        }
        fn set_activity(&self, activity: &ProgressActivity) {
            assert!(!self.finished.load(Ordering::Relaxed));
            self.activities.lock().unwrap().push(activity.clone());
            if let ProgressActivity::HashingFiles(counts)
            | ProgressActivity::InspectingRepositories(counts) = activity
            {
                self.observed.send(*counts).unwrap();
            }
        }
        fn finish(&self) {
            self.finished.store(true, Ordering::Relaxed);
        }
    }

    struct Fixture {
        progress: ParallelProgress,
        task: ProgressTask,
        backend: Arc<Recording>,
        received: mpsc::Receiver<WorkCounts>,
    }
    fn fixture(kind: ParallelWork) -> Fixture {
        let (observed, received) = mpsc::channel();
        let backend = Arc::new(Recording {
            position: AtomicU64::new(0),
            activities: Mutex::new(Vec::new()),
            observed,
            finished: AtomicBool::new(false),
        });
        let task = ProgressTask::from_backend(backend.clone());
        Fixture {
            progress: ParallelProgress::new(task.handle(), kind),
            task,
            backend,
            received,
        }
    }

    #[test]
    fn stalled_workers_are_sampled_and_windows_keep_exact_totals() {
        let mut fixture = fixture(ParallelWork::Hashing);
        fixture.progress.run(|work| {
            let first = work.start();
            let failed = work.start();
            while fixture.received.recv().unwrap().active() != 2 {}
            assert_eq!(fixture.backend.position.load(Ordering::Relaxed), 0);
            first.complete();
            drop(failed);
        });
        assert_eq!(fixture.backend.position.load(Ordering::Relaxed), 1);
        fixture.progress.run(|work| work.start().complete());
        assert_eq!(fixture.backend.position.load(Ordering::Relaxed), 2);
        let last = fixture
            .backend
            .activities
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap();
        let ProgressActivity::HashingFiles(counts) = last else {
            panic!("hashing snapshot");
        };
        assert_eq!(
            (counts.active(), counts.succeeded(), counts.failed()),
            (0, 2, 1)
        );
    }

    #[test]
    fn acknowledged_pause_prevents_late_publication_and_idle_polling() {
        let mut fixture = fixture(ParallelWork::Hashing);
        fixture.progress.run(|work| work.start().complete());
        fixture
            .backend
            .set_activity(&ProgressActivity::CheckingReuseStatus);
        // A second pause is an explicit barrier through the sampler's idle
        // receive path. No sleeps or assumptions about scheduling are needed.
        let worker = fixture
            .progress
            .enabled
            .as_ref()
            .unwrap()
            .worker
            .as_ref()
            .unwrap();
        worker.commands.send(Command::Pause).unwrap();
        worker.acknowledged.recv().unwrap();
        assert_eq!(
            fixture.backend.activities.lock().unwrap().last(),
            Some(&ProgressActivity::CheckingReuseStatus)
        );
    }

    #[test]
    fn unwind_flushes_repository_inspection() {
        let mut fixture = fixture(ParallelWork::InspectingRepositories);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture.progress.run(|work| {
                work.start().complete();
                let _unfinished = work.start();
                panic!("inspection failed");
            })
        }));
        assert!(result.is_err());
        assert_eq!(fixture.backend.position.load(Ordering::Relaxed), 2);
        let work = &fixture.progress.enabled.as_ref().unwrap().work;
        assert_eq!(work.snapshot().active(), 0);
        assert_eq!(work.snapshot().failed(), 1);
    }

    #[test]
    fn disabled_reporting_does_not_attempt_thread_creation() {
        let task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
        let mut progress =
            ParallelProgress::with_spawner(task.handle(), ParallelWork::Hashing, |_| {
                panic!("disabled sampler")
            });
        assert!(progress.enabled.is_none());
        progress.run(|work| {
            work.start().complete();
            assert_eq!(work.snapshot(), WorkCounts::default());
        });
    }

    #[test]
    fn unavailable_sampler_falls_back_to_disabled_accounting() {
        let fixture = fixture(ParallelWork::Hashing);
        let mut progress =
            ParallelProgress::with_spawner(fixture.task.handle(), ParallelWork::Hashing, |_| {
                Err(std::io::Error::other("thread unavailable"))
            });
        assert!(progress.enabled.is_none());
        assert_eq!(
            progress.run(|work| {
                work.start().complete();
                work.snapshot()
            }),
            WorkCounts::default()
        );
    }

    #[test]
    fn backend_panic_does_not_poison_cleanup_or_replace_operation_errors() {
        struct Panicking;
        impl ActivityBackend for Panicking {
            fn inc(&self, _: u64) {}
            fn set_activity(&self, _: &ProgressActivity) {
                panic!("broken reporting backend");
            }
            fn finish(&self) {}
        }
        let task = ProgressTask::from_backend(Arc::new(Panicking));
        let mut progress = ParallelProgress::new(task.handle(), ParallelWork::Hashing);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            progress.run(|work| {
                let _unfinished = work.start();
                panic!("original operation failure");
            })
        }));
        assert_eq!(
            *result.unwrap_err().downcast::<&str>().unwrap(),
            "original operation failure"
        );
        assert_eq!(progress.run(|_| 42), 42);
    }
}
