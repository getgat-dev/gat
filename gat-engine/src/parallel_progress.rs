//! Sample parallel work once per operation, independently of worker completion.

use gat_core::progress::{ProgressActivity, ProgressHandle, WorkCounts, WorkProgress};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

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

enum Phase {
    Paused,
    Running { last: Option<WorkCounts> },
}

struct Reporting {
    phase: Phase,
    position: u64,
}

struct Shared {
    work: WorkProgress,
    task: ProgressHandle,
    kind: ParallelWork,
    reporting: Mutex<Reporting>,
}

impl Shared {
    // Keep the phase lock through publication: after pause returns no sampler
    // can overwrite a subsequent discovery, publication, or other activity.
    fn report(&self, reporting: &mut Reporting) {
        if let Phase::Running { last } = &mut reporting.phase {
            let counts = self.work.snapshot();
            if *last != Some(counts) {
                let position = self.kind.position(counts);
                let delta = position - reporting.position;
                if delta != 0 {
                    self.task.inc(delta);
                    reporting.position = position;
                }
                self.task.set_activity(self.kind.activity(counts));
                *last = Some(counts);
            }
        }
    }
}

/// One sampler for the whole logical operation, reused across windows. Only
/// enabled reporting allocates shared state or starts a thread. Workers take a
/// short accounting lock at start/end, never format text or read the clock.
/// The sampler wakes during stalled work and exits immediately on drop.
pub struct ParallelProgress {
    enabled: Option<Enabled>,
}

struct Enabled {
    shared: Arc<Shared>,
    worker: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}

impl ParallelProgress {
    /// # Panics
    /// Panics if the system cannot start the enabled progress sampler.
    #[must_use]
    pub fn new(task: ProgressHandle, kind: ParallelWork) -> Self {
        let enabled = task.is_enabled().then(|| {
            let shared = Arc::new(Shared {
                work: WorkProgress::enabled(),
                task,
                kind,
                reporting: Mutex::new(Reporting {
                    phase: Phase::Paused,
                    position: 0,
                }),
            });
            let (shutdown, receiver) = mpsc::channel();
            let observer = Arc::clone(&shared);
            let thread = std::thread::spawn(move || {
                while matches!(
                    receiver.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    observer.report(&mut observer.reporting.lock().unwrap());
                }
            });
            Enabled {
                shared,
                worker: Some((shutdown, thread)),
            }
        });
        Self { enabled }
    }

    /// Report only while this work runs; flush before the next command phase,
    /// including on error or unwind. Counts survive calls to this method.
    ///
    /// # Panics
    /// Propagates panics from the closure or a poisoned reporting backend.
    pub fn run<T>(&mut self, run: impl FnOnce(&WorkProgress) -> T) -> T {
        let Some(enabled) = &self.enabled else {
            return run(&WorkProgress::default());
        };
        let shared = &enabled.shared;
        {
            let mut reporting = shared.reporting.lock().unwrap();
            reporting.phase = Phase::Running { last: None };
            shared.report(&mut reporting);
        }
        struct Pause<'a>(&'a Shared);
        impl Drop for Pause<'_> {
            fn drop(&mut self) {
                let mut reporting = self.0.reporting.lock().unwrap();
                self.0.report(&mut reporting);
                reporting.phase = Phase::Paused;
            }
        }
        let _pause = Pause(shared);
        run(&shared.work)
    }
}

impl Drop for Enabled {
    fn drop(&mut self) {
        if let Some((shutdown, thread)) = self.worker.take() {
            drop(shutdown);
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
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Recording {
        position: AtomicU64,
        activities: Mutex<Vec<ProgressActivity>>,
        observed: mpsc::Sender<WorkCounts>,
    }

    impl ActivityBackend for Recording {
        fn inc(&self, delta: u64) {
            self.position.fetch_add(delta, Ordering::Relaxed);
        }
        fn set_activity(&self, activity: &ProgressActivity) {
            self.activities.lock().unwrap().push(activity.clone());
            if let ProgressActivity::HashingFiles(counts)
            | ProgressActivity::InspectingRepositories(counts) = activity
            {
                self.observed.send(*counts).unwrap();
            }
        }
        fn finish(&self) {}
    }

    fn fixture(
        kind: ParallelWork,
    ) -> (ParallelProgress, Arc<Recording>, mpsc::Receiver<WorkCounts>) {
        let (observed, receiver) = mpsc::channel();
        let backend = Arc::new(Recording {
            position: AtomicU64::new(0),
            activities: Mutex::new(Vec::new()),
            observed,
        });
        let task = ProgressTask::from_backend(backend.clone());
        (
            ParallelProgress::new(task.handle(), kind),
            backend,
            receiver,
        )
    }

    #[test]
    fn stalled_workers_are_sampled_and_windows_keep_exact_totals() {
        let (mut progress, backend, received) = fixture(ParallelWork::Hashing);
        progress.run(|work| {
            let first = work.start();
            let failed = work.start();
            // The workers make no more callbacks until the sampler observes
            // both. This tests liveness without sleeps or timing assertions.
            while received.recv().unwrap().active() != 2 {}
            assert_eq!(backend.position.load(Ordering::Relaxed), 0);
            first.complete();
            drop(failed);
        });
        assert_eq!(backend.position.load(Ordering::Relaxed), 1);
        progress.run(|work| work.start().complete());
        assert_eq!(backend.position.load(Ordering::Relaxed), 2);
        let last = backend.activities.lock().unwrap().last().cloned().unwrap();
        let ProgressActivity::HashingFiles(counts) = last else {
            panic!("hashing snapshot");
        };
        assert_eq!(
            (counts.active(), counts.succeeded(), counts.failed()),
            (0, 2, 1)
        );

        backend.set_activity(&ProgressActivity::CheckingReuseStatus);
        let shared = &progress.enabled.as_ref().unwrap().shared;
        shared.report(&mut shared.reporting.lock().unwrap());
        assert_eq!(
            backend.activities.lock().unwrap().last(),
            Some(&ProgressActivity::CheckingReuseStatus)
        );
    }

    #[test]
    fn unwind_flushes_and_pauses_repository_inspection() {
        let (mut progress, backend, _received) = fixture(ParallelWork::InspectingRepositories);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            progress.run(|work| {
                work.start().complete();
                let _unfinished = work.start();
                panic!("inspection failed");
            })
        }));
        assert!(result.is_err());
        assert_eq!(backend.position.load(Ordering::Relaxed), 2);
        let shared = &progress.enabled.as_ref().unwrap().shared;
        assert!(matches!(
            shared.reporting.lock().unwrap().phase,
            Phase::Paused
        ));
        assert_eq!(shared.work.snapshot().active(), 0);
        assert_eq!(shared.work.snapshot().failed(), 1);
    }

    #[test]
    fn disabled_reporting_has_no_sampler_or_accounting() {
        let task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
        let mut progress = ParallelProgress::new(task.handle(), ParallelWork::Hashing);
        assert!(progress.enabled.is_none());
        progress.run(|work| {
            work.start().complete();
            assert_eq!(work.snapshot(), WorkCounts::default());
        });
    }
}
