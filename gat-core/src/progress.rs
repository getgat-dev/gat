//! Transient progress protocol. A small abstraction
//! commands use to report *logical* progress ("hashing files",
//! "fetching objects") without knowing whether that turns into an
//! indicatif spinner/bar or nothing at all.
//!
//! This module owns the neutral *contract*: what a command may say
//! happened and to what degree. It never renders anything itself and
//! has no `indicatif`/terminal dependency -- rendering (styles, TTY
//! detection, terminal width, the concrete `indicatif`-backed
//! reporters, and any redaction of security-sensitive values) lives in
//! the root `gat` crate's `output::progress` module. `commands/**` must
//! only ever depend on this protocol, never on the root renderer.
//!
//! ## Architectural rules
//!
//! - **One logical progress task creates exactly one rendered progress
//!   entry.** There are no child progress tasks and no per-file/per-object
//!   rows. Concurrent workers contribute to the same logical task.
//! - **Only orchestration code can create progress tasks.**
//!   [`ProgressReporter::begin`] returns a [`ProgressTask`], the RAII
//!   owner of one logical task. Window/chunk/job/worker helpers receive
//!   a [`ProgressHandle`] (via [`ProgressTask::handle`]) instead: a
//!   restricted, cloneable capability that can update counters/activity
//!   but cannot begin another task, finish the task, change its total,
//!   allocate rows, or touch renderer state.
//! - **Progress follows the user's logical operation, not the
//!   implementation strategy.** Streaming windows, Rayon workers,
//!   remote-executor jobs, chunks, pages, and batches must not create
//!   progress lifecycles or define user-visible totals.
//! - **Totals always describe the whole logical task.** Use a
//!   determinate total only when the exact whole-task total is already
//!   naturally available *before* the corresponding bounded work
//!   materially begins.
//! - **Transient output remains separate from durable command output**:
//!   progress is cleared before final outcomes/errors are
//!   rendered.

use crate::git_location::GitLocationSpec;
use crate::lexical_path::GatPath;
use std::sync::atomic::{AtomicBool, Ordering};

/// One semantic operation a command can report progress for. Deliberately
/// closed, small, and free of count/byte payloads: add a variant only
/// when a command has a genuinely new, user-visible kind of work to
/// report, never an internal phase, window, worker, or transfer detail.
///
/// This type is a neutral protocol value -- it has no wording/label
/// method of its own. The root `output::progress` module alone owns the
/// mapping from a variant to its rendered presentation line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressOperation {
    /// Loading or refreshing tracked/materialized state (`gat.lock`,
    /// the materialized-state database, or both).
    LoadingState,
    /// Discovering candidate files or preparing a large file batch.
    DiscoveringFiles,
    /// Resolving a user-selected set of objects/history.
    ResolvingSelection,
    /// Hashing file(s) (e.g. `gat add` on a directory).
    Hashing,
    /// Downloading missing objects from a remote (e.g. `gat fetch`).
    Fetching,
    /// Publishing objects to a remote (e.g. `gat push`): checking remote
    /// presence and uploading are both represented as activity on this
    /// one logical task, never as separate task lifecycles.
    Pushing,
    /// Checking/reporting remote object status (e.g. `gat status
    /// --remote`).
    RemoteStatus,
    /// Reconciling the working tree against `gat.lock` (e.g. `gat sync`),
    /// including reshaping `gat.lock`'s on-disk shape when needed.
    Synchronizing,
    /// Applying a resolved mutation to tracked/desired state (e.g.
    /// `gat rm`, `gat mv`, `gat mount`'s desired-lock writes) --
    /// distinct from `Synchronizing`, which reconciles the working tree
    /// with already-published desired state.
    ApplyingChanges,
    /// Inspecting cached objects against tracked paths (e.g. `gat
    /// status`'s local-cache-state pass).
    InspectingCache,
    /// Repairing corrupted cache objects (`gat sync --repair`).
    Repairing,
    /// Scanning/deleting unreferenced cache objects (`gat gc`).
    GarbageCollecting,
    /// Computing the reachable-object keep-set before a `gat gc` sweep
    /// (walking selected repositories' histories) -- genuinely
    /// user-visible (it can be slow on a large history) and distinct
    /// from the sweep itself.
    ComputingReachability,
    /// Enumerating a remote's existing objects before a remote `gat gc`
    /// sweep -- the pass that naturally discovers the sweep's exact
    /// object count.
    ListingRemoteObjects,
    /// `gat system inspect`: resolved-domain-level inspection sweep.
    SystemInspect,
    /// `gat system repair`: resolved-domain-level repair sweep.
    SystemRepair,
    /// `gat system clean`: resolved-domain-level cleanup sweep.
    SystemClean,
    /// Cloning a remote source repository into a temporary directory
    /// (`gat mount add` with a remote `url`) -- an indeterminate,
    /// possibly long-running network operation with no byte/object count
    /// known up front. Remote-GC repository cloning is *not* an instance
    /// of this: it reports its activity onto the caller's already-open
    /// `ComputingReachability` task instead of beginning its own
    /// `CloningSource` task (see `gc/remote.rs::clone_bare_repo`).
    CloningSource,
}

/// One activity a command can be doing while a [`ProgressOperation`] is
/// underway -- the closed set of things worth telling the user about,
/// deliberately independent of [`ProgressOperation`] so the same
/// activity can appear under different operations without duplicating
/// activity wording -- commands only ever pick *which* activity is
/// happening, never how it reads.
///
/// This type is a neutral protocol value -- it has no wording method of
/// its own. The root `output::progress` module alone owns the mapping
/// from a variant to its rendered presentation line. Dynamic fields
/// carry only already-validated core semantic values (e.g. [`GatPath`],
/// [`GitLocationSpec`]) or plain display text (`pattern`) -- never a
/// presentation-layer/security-sensitive type such as a redacted URL:
/// that redaction happens only at the root renderer, from
/// [`ProgressActivity::CloningSource`]'s [`GitLocationSpec`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgressActivity {
    /// Reshaping `gat.lock`'s on-disk shard layout.
    ReshapingLock,
    /// Staging matched rows ahead of replaying them (`gat mount`).
    StagingRows,
    /// Replaying staged rows (`gat mount`).
    ReplayingRows,
    /// Observing the on-disk lock state before a mutation.
    ObservingLockState,
    /// Refreshing materialized/desired state from `gat.lock`.
    RefreshingDesiredState,
    /// Recovering an interrupted mount transaction.
    RecoveringInterruptedMount,
    /// Publishing an updated config.
    PublishingConfig,
    /// Deleting rows owned by a mount being removed.
    DeletingOwnedRows,
    /// Deleting proven-unreferenced remote objects in native batches.
    DeletingRemoteObjects,
    /// Opening the materialized-state store.
    OpeningMaterializedState,
    /// Publishing updated desired state.
    PublishingDesiredState,
    /// Recording newly materialized state.
    RecordingMaterializedState,
    /// Regenerating Git excludes.
    RegeneratingExcludes,
    /// Checking whether a candidate can reuse an existing OID.
    CheckingReuseStatus,
    /// Hashing one specific file (e.g. a large file whose byte-level
    /// progress is tracked separately via `percent`).
    HashingFile { path: GatPath, percent: Option<u8> },
    /// Walking a directory to discover candidate files.
    WalkingDirectory,
    /// Expanding a glob pattern to discover candidate files.
    ExpandingPattern { pattern: String },
    /// Connecting to a remote before checking/transferring anything.
    Connecting,
    /// Resolving a user-selected set of objects/history before
    /// streaming begins.
    ResolvingSelection,
    /// Loading already-published state (the non-history counterpart of
    /// [`ProgressActivity::ResolvingSelection`]).
    LoadingState,
    /// Checking remote object presence for the current window.
    CheckingRemote,
    /// The most recently completed remote-presence check.
    CheckedRemoteObject { path: GatPath },
    /// Classifying user-supplied selectors (literal path vs. glob).
    ClassifyingSelectors,
    /// Scanning desired state for matching rows.
    ScanningDesiredState,
    /// Scanning the full `gat.lock` (fallback path with no lexical
    /// locality to narrow with).
    ScanningLock,
    /// Matching a `gat mv` source path against tracked/desired rows.
    MatchingSourcePath,
    /// Checking a `gat mv` destination for collisions.
    CheckingDestination,
    /// Cloning a remote source repository, identified only by its
    /// exact, unvalidated, potentially credential-bearing configured
    /// location -- the same value the root `gat` crate already stores
    /// in `gat.yaml`. Only the root renderer, at the presentation
    /// boundary, is permitted to turn this into human-readable
    /// (redacted) text.
    CloningSource { location: GitLocationSpec },
    /// Streaming bytes for one specific file during an upload/download.
    TransferringFile { path: GatPath },
    /// Loading the materialized-state store (`gat sync`'s own phase,
    /// distinct from [`ProgressActivity::OpeningMaterializedState`]'s
    /// `gat add` wording).
    LoadingMaterializedState,
    /// Loading tracked (`gat.lock`) state.
    LoadingTrackedState,
    /// Validating the working tree against tracked/materialized state.
    ValidatingWorkingTree,
    /// Planning the set of changes a sync will apply.
    PlanningChanges,
    /// Applying planned changes to the working tree.
    ApplyingChanges,
}

impl ProgressActivity {
    /// The activity for resolving a selection when historical rows are
    /// involved, vs. loading already-published state otherwise --
    /// shared wording decision used by `fetch`/`push`/`remote_status`.
    #[must_use]
    pub const fn selection_or_state(history_selected: bool) -> Self {
        if history_selected {
            Self::ResolvingSelection
        } else {
            Self::LoadingState
        }
    }
}

/// The unit a [`ProgressSpec`] item count is measured in -- deliberately
/// closed and small, same spirit as [`ProgressOperation`]. Neutral
/// protocol value with no wording method of its own -- the root
/// `output::progress` module alone maps a variant to its rendered prefix
/// text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressUnit {
    Files,
    Objects,
    Entries,
    Domains,
}

/// The full specification of a logical progress task's identity and
/// initial measurement, passed once to [`ProgressReporter::begin`].
///
/// `unit`/`total` are `None` when no count is meaningful (indeterminate:
/// rendered as a plain spinner). When `unit` is set, `total` is `Some`
/// only if the exact whole-task total is already naturally known before
/// the corresponding bounded work materially begins -- never a
/// pre-counted/pre-materialized/internal-window value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressSpec {
    pub(crate) operation: ProgressOperation,
    pub(crate) unit: Option<ProgressUnit>,
    pub(crate) total: Option<u64>,
}

impl ProgressSpec {
    /// No item count is meaningful for this task.
    #[must_use]
    pub const fn indeterminate(operation: ProgressOperation) -> Self {
        Self {
            operation,
            unit: None,
            total: None,
        }
    }

    /// An item count in `unit`, with `total` known up front (`Some`,
    /// determinate) or not (`None`, open-ended).
    #[must_use]
    pub const fn items(
        operation: ProgressOperation,
        unit: ProgressUnit,
        total: Option<u64>,
    ) -> Self {
        Self {
            operation,
            unit: Some(unit),
            total,
        }
    }

    /// Which [`ProgressOperation`] this spec begins -- exposed read-only
    /// so a [`ProgressReporter`] impl can distinguish operations (e.g. a
    /// test double that only intercepts one specific operation) without
    /// being able to construct or mutate a spec's fields directly.
    #[must_use]
    pub const fn operation(&self) -> ProgressOperation {
        self.operation
    }

    /// Which [`ProgressUnit`] (if any) this spec's item count is
    /// measured in -- exposed read-only for the same reason as
    /// [`ProgressSpec::operation`]; a concrete renderer (necessarily in
    /// a separate crate) needs this to classify a task as indeterminate,
    /// open-ended, or determinate.
    #[must_use]
    pub const fn unit(&self) -> Option<ProgressUnit> {
        self.unit
    }

    /// The whole-task total (if already known up front) -- exposed
    /// read-only for the same reason as [`ProgressSpec::operation`].
    #[must_use]
    pub const fn total(&self) -> Option<u64> {
        self.total
    }
}

/// The narrow capability a concrete renderer supplies for one running
/// logical task: item increments, activity updates, and a finish
/// signal. Implemented by the root crate's indicatif-backed task, its
/// own test doubles, and this module's no-op backend -- never named
/// directly by command code, only reached through [`ProgressHandle`]/
/// [`ProgressTask`]. Kept `pub` (rather than `pub(crate)`) because
/// concrete implementations necessarily live in the root `gat` crate,
/// a separate workspace member.
pub trait ActivityBackend: Send + Sync {
    fn inc(&self, delta: u64);
    fn set_activity(&self, activity: &ProgressActivity);
    fn finish(&self);
}

/// A no-op backend: every method does nothing. Used by [`NoopProgress`]-
/// style reporters and as the "not the targeted occurrence" branch of
/// test doubles that only instrument one specific task.
struct NoopBackend;

impl ActivityBackend for NoopBackend {
    fn inc(&self, _delta: u64) {}
    fn set_activity(&self, _activity: &ProgressActivity) {}
    fn finish(&self) {}
}

/// A cheap, cloneable capability for worker/window/chunk code: item
/// increments and activity-message updates only. Cannot begin/finish a
/// task, change its total, allocate rows, or otherwise touch renderer
/// state -- the restriction is structural (this type simply has no such
/// methods), not just documented.
#[derive(Clone)]
pub struct ProgressHandle {
    pub(crate) backend: std::sync::Arc<dyn ActivityBackend>,
}

impl ProgressHandle {
    /// Advance the logical item position by `delta` (e.g. one more file
    /// hashed, one more object fetched). Cheap: a thread-safe counter
    /// update only, never a direct terminal redraw.
    pub fn inc(&self, delta: u64) {
        self.backend.inc(delta);
    }

    /// Update the task's current activity (e.g. the path currently
    /// being hashed/uploaded/fetched, or a status like "checking
    /// remote"). Only a closed [`ProgressActivity`] variant is
    /// accepted -- never an arbitrary string -- so wording stays owned
    /// entirely by the renderer.
    pub fn set_activity(&self, activity: ProgressActivity) {
        self.backend.set_activity(&activity);
    }
}

/// A running logical progress task, returned by
/// [`ProgressReporter::begin`]. The RAII owner of the task: guarantees
/// the underlying transient UI (if any) is cleared no matter how the
/// caller's scope ends (normal return, an early `return`, or a
/// propagated error via `?`), and is the only way to create a
/// [`ProgressHandle`] or explicitly finish the task early. Its
/// [`ProgressSpec`] (operation, unit, total) is fixed at [`begin`] time
/// and immutable for the task's whole lifetime -- there is no
/// `set_total`/transition API, since no command needs a whole-task
/// total to become known only partway through. Never construct this
/// directly; only [`ProgressReporter`] implementations do (via
/// [`ProgressTask::from_backend`]).
///
/// [`begin`]: ProgressReporter::begin
pub struct ProgressTask {
    backend: std::sync::Arc<dyn ActivityBackend>,
    finished: AtomicBool,
}

impl ProgressTask {
    /// Construct a task from a concrete backend -- only
    /// [`ProgressReporter`] implementations (in the root crate's
    /// `output::progress` and its own test doubles) call this. Kept
    /// `pub` (rather than `pub(crate)`) for the same cross-crate reason
    /// as [`ActivityBackend`].
    pub fn from_backend(backend: std::sync::Arc<dyn ActivityBackend>) -> Self {
        Self {
            backend,
            finished: AtomicBool::new(false),
        }
    }

    /// Advance the logical item position by `delta`. Equivalent to
    /// calling the same method on a [`ProgressHandle`] obtained via
    /// [`ProgressTask::handle`].
    pub fn inc(&self, delta: u64) {
        self.backend.inc(delta);
    }

    /// Update the task's current activity.
    pub fn set_activity(&self, activity: ProgressActivity) {
        self.backend.set_activity(&activity);
    }

    /// Return a cloneable, restricted capability for worker/window/chunk
    /// code: item increments and message updates only. The only
    /// sanctioned way for non-orchestration code to touch this task at
    /// all.
    pub fn handle(&self) -> ProgressHandle {
        ProgressHandle {
            backend: std::sync::Arc::clone(&self.backend),
        }
    }

    /// Finish and clear immediately, rather than waiting for `Drop`. Safe
    /// to call and then let the task drop -- finishing twice is a no-op.
    pub fn finish(&self) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        self.backend.finish();
    }
}

impl Drop for ProgressTask {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The narrow abstraction commands depend on: report that a semantic
/// operation started (with its initial [`ProgressSpec`]), get back a
/// [`ProgressTask`], drop it (implicitly or explicitly) when done.
/// Commands must not depend on the concrete terminal/no-op
/// implementations directly -- only ever on `&dyn ProgressReporter`,
/// constructed once at the application boundary
/// (`output::progress::for_environment`).
///
/// `begin` belongs at the orchestration boundary that owns the whole
/// user-visible operation's lifetime -- a window/chunk/job helper
/// receives [`ProgressTask::handle`]'s result and updates it; it never
/// calls `begin` again for the same logical task.
pub trait ProgressReporter: Send + Sync {
    fn begin(&self, spec: ProgressSpec) -> ProgressTask;

    /// Clear every currently-active progress UI immediately, ahead of
    /// rendering the final `CommandOutcome` -- so, even if a caller held
    /// onto a task longer than necessary, transient output is guaranteed
    /// gone before durable output is written.
    fn finish_all(&self) {}
}

/// A progress-tracked step for a closure that already returns a concrete
/// typed `Result<T, E>` rather than `anyhow::Result<T>` -- lets a fully
/// migrated command or engine capability keep its own typed error
/// end-to-end through the step, instead of round-tripping through
/// `anyhow::Error` and recovering it afterwards via a downcast bridge.
pub fn with_progress_typed<T, E>(
    progress: &dyn ProgressReporter,
    spec: ProgressSpec,
    f: impl FnOnce(&ProgressTask) -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    let task = progress.begin(spec);
    let value = f(&task)?;
    task.finish();
    Ok(value)
}

/// The no-op implementation: every method does nothing, used whenever
/// progress must not be displayed (non-tty stderr, quiet/hook contexts,
/// unit tests exercising command logic). Touches no terminal state and
/// allocates nothing beyond the shared no-op backend.
#[derive(Default)]
pub struct NoopProgress;

impl ProgressReporter for NoopProgress {
    fn begin(&self, _spec: ProgressSpec) -> ProgressTask {
        ProgressTask::from_backend(std::sync::Arc::new(NoopBackend))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    /// A minimal, purely local recording backend used only to prove the
    /// task/handle lifecycle contract (finish idempotency, `Drop`
    /// semantics, finish counting) without depending on any renderer --
    /// unlike the root crate's richer `RecordingProgress` test support,
    /// which additionally renders activity wording for human-readable
    /// test assertions and therefore must stay in root.
    #[derive(Default)]
    struct CountingBackend {
        increments: AtomicU64,
        activities: Mutex<Vec<ProgressActivity>>,
        finishes: AtomicU64,
    }

    impl ActivityBackend for CountingBackend {
        fn inc(&self, delta: u64) {
            self.increments.fetch_add(delta, Ordering::SeqCst);
        }
        fn set_activity(&self, activity: &ProgressActivity) {
            self.activities.lock().unwrap().push(activity.clone());
        }
        fn finish(&self) {
            self.finishes.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct CountingProgress {
        backend: Arc<CountingBackend>,
    }

    impl ProgressReporter for CountingProgress {
        fn begin(&self, _spec: ProgressSpec) -> ProgressTask {
            ProgressTask::from_backend(Arc::clone(&self.backend) as Arc<dyn ActivityBackend>)
        }
    }

    #[test]
    fn progress_task_finish_is_idempotent() {
        let backend = Arc::new(CountingBackend::default());
        let progress = CountingProgress {
            backend: Arc::clone(&backend),
        };
        let task = progress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
        task.finish();
        task.finish(); // must not panic or double-fire finish semantics
        drop(task); // Drop after an explicit finish must also be a no-op
        assert_eq!(backend.finishes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn progress_task_drop_finishes_the_task_without_an_explicit_call() {
        let backend = Arc::new(CountingBackend::default());
        let progress = CountingProgress {
            backend: Arc::clone(&backend),
        };
        {
            let _task = progress.begin(ProgressSpec::indeterminate(ProgressOperation::Hashing));
            assert_eq!(backend.finishes.load(Ordering::SeqCst), 0);
        }
        assert_eq!(backend.finishes.load(Ordering::SeqCst), 1);
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
            path: GatPath::parse_canonical("a.bin").unwrap(),
            percent: None,
        });
        let handle = task.handle();
        handle.inc(1);
        handle.set_activity(ProgressActivity::HashingFile {
            path: GatPath::parse_canonical("a.bin").unwrap(),
            percent: Some(50),
        });
        task.finish();
        NoopProgress.finish_all();
    }

    #[test]
    fn handle_and_task_increments_and_activity_reach_the_backend() {
        let backend = Arc::new(CountingBackend::default());
        let progress = CountingProgress {
            backend: Arc::clone(&backend),
        };
        let task = progress.begin(ProgressSpec::items(
            ProgressOperation::Hashing,
            ProgressUnit::Files,
            Some(3),
        ));
        task.inc(1);
        let handle = task.handle();
        handle.inc(1);
        handle.set_activity(ProgressActivity::ExpandingPattern {
            pattern: "*.bin".to_string(),
        });
        assert_eq!(backend.increments.load(Ordering::SeqCst), 2);
        assert_eq!(backend.activities.lock().unwrap().len(), 1);
    }
}
