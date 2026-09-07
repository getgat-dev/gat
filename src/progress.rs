//! Transient progress protocol.
//!
//! The neutral protocol itself -- [`ProgressOperation`],
//! [`ProgressActivity`], [`ProgressUnit`], [`ProgressSpec`], the
//! reporter/task/handle capability types, and [`NoopProgress`] -- lives
//! in [`gat_core::progress`] and is re-exported here unchanged.
//! `commands/**` must only ever depend on this protocol, never on
//! [`crate::output`]. This module additionally owns:
//!
//! - [`with_progress_typed`], a thin convenience wrapper commands use
//!   directly (re-exported from core);
//! - `test_support`, this crate's recording/probing test doubles, which
//!   render activity wording through [`crate::output::progress`] purely
//!   to make recorded messages human-readable in assertions -- a root-
//!   only concern that cannot live in the renderer-independent core
//!   protocol.
//!
//! [`gat_core::progress`] defines the protocol rules: one logical task per
//! rendered entry, orchestration-only `begin`, totals describing the whole
//! logical task, and separation between transient and durable output.

pub use gat_core::progress::{
    ActivityBackend, NoopProgress, ProgressActivity, ProgressHandle, ProgressOperation,
    ProgressReporter, ProgressSpec, ProgressTask, ProgressUnit, with_progress_typed,
};

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64 as TestAtomicU64;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    /// A stable identity for one `begin`/finish lifecycle, distinct from
    /// [`ProgressOperation`] equality: lets a test distinguish between
    /// two concurrent/sequential instances of the *same* operation.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct TaskId(pub u64);

    /// Final, normalized state of one logical task: no total-order event
    /// log is needed to prove semantic
    /// contracts -- just the task's identity and its final measured
    /// state (plus every message it was ever given, so a test can still
    /// assert on activity-text changes over the task's lifetime).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RecordedTask {
        pub id: TaskId,
        pub operation: ProgressOperation,
        pub unit: Option<ProgressUnit>,
        pub initial_total: Option<u64>,
        pub total: Option<u64>,
        pub position: u64,
        pub messages: Vec<String>,
        pub finished: bool,
    }

    #[derive(Default)]
    struct RecordedTaskState {
        operation: Option<ProgressOperation>,
        unit: Option<ProgressUnit>,
        initial_total: Option<u64>,
        total: Option<u64>,
        position: u64,
        messages: Vec<String>,
        finished: bool,
    }

    struct RecordingBackend {
        state: Arc<Mutex<RecordedTaskState>>,
        finished_flag: AtomicBool,
        finishes: Arc<TestAtomicU64>,
        active_tasks: Arc<AtomicUsize>,
    }

    impl ActivityBackend for RecordingBackend {
        fn inc(&self, delta: u64) {
            let mut state = self.state.lock().unwrap();
            debug_assert!(
                state.unit.is_some(),
                "ProgressHandle::inc() called on an indeterminate task (no unit) -- \
                 a position with no unit is meaningless; report sub-phases via \
                 set_activity() instead"
            );
            state.position += delta;
        }

        fn set_activity(&self, activity: &ProgressActivity) {
            // The protocol layer never renders wording itself (that is
            // `output::progress`'s job); tests borrow its rendering
            // purely to make recorded activity readable in assertions.
            let line = crate::output::progress::activity_line(activity);
            self.state
                .lock()
                .unwrap()
                .messages
                .push(line.as_str().to_string());
        }

        fn finish(&self) {
            if self.finished_flag.swap(true, Ordering::SeqCst) {
                return;
            }
            self.state.lock().unwrap().finished = true;
            self.finishes.fetch_add(1, Ordering::SeqCst);
            self.active_tasks.fetch_sub(1, Ordering::SeqCst);
        }
    }

    type TaskRegistry = Arc<Mutex<Vec<(TaskId, Arc<Mutex<RecordedTaskState>>)>>>;

    #[derive(Clone, Default)]
    pub struct RecordingProgress {
        tasks: TaskRegistry,
        next_task_id: Arc<TestAtomicU64>,
        finishes: Arc<TestAtomicU64>,
        /// How many logical tasks are currently begun-but-not-finished.
        /// Incremented at `begin` and decremented exactly once at finish.
        active_tasks: Arc<AtomicUsize>,
    }

    impl RecordingProgress {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Every task ever begun, in begin order, as normalized
        /// [`RecordedTask`] snapshots of their *current* (possibly still
        /// in-progress) state.
        pub(crate) fn tasks(&self) -> Vec<RecordedTask> {
            self.tasks
                .lock()
                .unwrap()
                .iter()
                .map(|(id, state)| {
                    let state = state.lock().unwrap();
                    RecordedTask {
                        id: *id,
                        operation: state.operation.expect("operation always set at begin"),
                        unit: state.unit,
                        initial_total: state.initial_total,
                        total: state.total,
                        position: state.position,
                        messages: state.messages.clone(),
                        finished: state.finished,
                    }
                })
                .collect()
        }

        /// How many logical tasks have finished so far.
        pub(crate) fn finish_count(&self) -> usize {
            usize::try_from(self.finishes.load(Ordering::SeqCst))
                .expect("finish count must fit in usize")
        }

        /// Whether at least one begun-but-not-yet-finished task for
        /// `operation` currently exists -- lets a caller-supplied
        /// per-row visitor prove real streaming work happens strictly
        /// while the covering task is still open, not in a silent gap
        /// after it already closed.
        pub(crate) fn is_active(&self, operation: ProgressOperation) -> bool {
            self.tasks.lock().unwrap().iter().any(|(_, state)| {
                let state = state.lock().unwrap();
                state.operation == Some(operation) && !state.finished
            })
        }

        /// The single recorded task for `operation`, panicking if there
        /// isn't exactly one -- a convenience for tests that already
        /// know (and want to assert) there's only one.
        pub(crate) fn only(&self, operation: ProgressOperation) -> RecordedTask {
            let mut matches: Vec<_> = self
                .tasks()
                .into_iter()
                .filter(|t| t.operation == operation)
                .collect();
            assert_eq!(
                matches.len(),
                1,
                "expected exactly one {operation:?} task, found {}",
                matches.len()
            );
            matches.remove(0)
        }
    }

    impl ProgressReporter for RecordingProgress {
        fn begin(&self, spec: ProgressSpec) -> ProgressTask {
            let id = TaskId(self.next_task_id.fetch_add(1, Ordering::SeqCst));
            let state = Arc::new(Mutex::new(RecordedTaskState {
                operation: Some(spec.operation()),
                unit: spec.unit(),
                initial_total: spec.total(),
                total: spec.total(),
                ..Default::default()
            }));
            self.tasks.lock().unwrap().push((id, Arc::clone(&state)));
            self.active_tasks.fetch_add(1, Ordering::SeqCst);
            let backend = RecordingBackend {
                state,
                finished_flag: AtomicBool::new(false),
                finishes: Arc::clone(&self.finishes),
                active_tasks: Arc::clone(&self.active_tasks),
            };
            ProgressTask::from_backend(Arc::new(backend))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::RecordingProgress;
    use super::*;

    #[test]
    fn progress_task_finish_is_idempotent() {
        let progress = RecordingProgress::new();
        let task = progress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
        task.finish();
        task.finish(); // must not panic or double-fire finish semantics
        drop(task); // Drop after an explicit finish must also be a no-op
        assert!(progress.only(ProgressOperation::Hashing).finished);
    }

    #[test]
    fn recording_progress_tracks_finish_count_across_multiple_tasks() {
        let progress = RecordingProgress::new();
        let first = progress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
        let second = progress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
        assert_eq!(progress.finish_count(), 0);
        drop(first);
        assert_eq!(progress.finish_count(), 1);
        drop(second);
        assert_eq!(progress.finish_count(), 2);
    }

    #[test]
    fn progress_task_drop_finishes_the_task_without_an_explicit_call() {
        let progress = RecordingProgress::new();
        {
            let _task = progress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
            assert!(progress.is_active(ProgressOperation::Hashing));
        }
        assert!(!progress.is_active(ProgressOperation::Hashing));
        assert!(progress.only(ProgressOperation::Hashing).finished);
    }

    #[test]
    fn noop_reporter_preserves_full_api_surface_without_panicking() {
        let task = NoopProgress.begin(ProgressSpec::items(
            ProgressOperation::Hashing,
            ProgressUnit::Files,
            Some(3),
        ));
        task.inc(1);
        task.set_activity(ProgressActivity::HashingFile {
            path: gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
            percent: None,
        });
        let handle = task.handle();
        handle.inc(1);
        handle.set_activity(ProgressActivity::HashingFile {
            path: gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
            percent: Some(50),
        });
        task.finish();
        NoopProgress.finish_all();
    }

    #[test]
    fn recording_progress_never_reports_a_position_past_its_declared_total() {
        let progress = RecordingProgress::new();
        let task = progress.begin(ProgressSpec::items(
            ProgressOperation::Hashing,
            ProgressUnit::Files,
            Some(3),
        ));
        for _ in 0..3 {
            task.inc(1);
        }
        drop(task);
        let recorded = progress.only(ProgressOperation::Hashing);
        assert_eq!(recorded.total, Some(3));
        assert!(recorded.position <= recorded.total.unwrap());
        assert_eq!(recorded.position, 3);
    }
}
