//! The sync engine: idempotent, state-based reconciliation between the
//! currently checked-out `gat.lock` (desired state), the last successfully
//! materialized state (`.gat/state/state.sqlite3`, its own row per
//! working-tree path), and what's actually present on disk (working
//! state). Same algorithm regardless of what triggered it -- a Git hook,
//! `gat sync`, or `gat pull` -- so hook handling never needs its own
//! diffing logic.
//!
//! This module exposes only a read-only planning API publicly --
//! [`plan`], [`SyncPlan`], [`SyncOptions`], [`Validation`], and the
//! outcome/action types. The one *mutating* entry point,
//! [`sync_from_snapshot`], takes an already-acquired
//! [`crate::operation::Operation`] rather than
//! a bare `Repo`: the orchestration layer acquires an `Operation` through
//! its repository snapshot service, and this module only ever consumes
//! one it's handed -- it never acquires acquisition
//! authority (a `RepoLock`/session/snapshot) itself in production code.
//! This keeps the dependency direction
//! strictly one-way: `commands -> engine::workspace::sync`, never the reverse.
//!
//! The module is a small coordinator over two focused phases, kept in
//! their own submodules so the read-only decision procedure stays
//! independently understandable from the filesystem-mutating application
//! phase:
//!
//! - [`plan`] builds a [`SyncPlan`] by comparing `gat.lock`, the
//!   materialized state, and the working tree. It never touches disk
//!   beyond read-only stats/hashes, so `--dry-run` and tests can inspect an
//!   exact plan before anything is written.
//! - `execute` applies a `SyncPlan`: it's the only place that mutates the
//!   working tree, `.git/info/exclude`, and the materialized state file.

#[doc(hidden)]
pub(crate) mod desired_index;
mod error;
#[doc(hidden)]
pub(crate) mod execute;
#[doc(hidden)]
pub(crate) mod plan;

use crate::excludes;
use crate::repository::Repository as Repo;
use gat_core::lock::Entry;
use gat_core::oid::Oid;
use gat_core::progress::ProgressActivity;
use gat_core::progress::ProgressHandle;
use gat_io::LockStore;

#[cfg(any(test, feature = "test-support"))]
pub use desired_index::RefreshResult;
pub(crate) use error::Result;
pub use error::{
    CacheFailureKind, ExcludesFailureKind, FileStateFailureKind, FilesystemFailureKind,
    LockFailureKind, MutationAuthorityFailureKind, StateFailureKind, SyncError, SyncErrorKind,
    WorktreePathFailureKind,
};

use gat_io::StateStore;

#[doc(hidden)]
pub fn refresh_desired_index(
    repo: &Repo,
    store: &mut StateStore,
) -> Result<desired_index::RefreshResult> {
    desired_index::refresh(repo, store)
}

/// How hard `plan` checks a working-tree file against the materialized
/// state before deciding to replace/remove it.
///
/// Validation is an explicit trade-off between speed and how much of the
/// working tree `plan` is allowed to inspect before deciding a path is
/// safe to replace/remove:
///
/// - [`Validation::Validate`] (the default) validates with a Git-style
///   stat cache: it stats the file first, proving
///   matches/mismatches cheaply when possible, and only hashes file
///   content when stat metadata is ambiguous (no recorded proof, or the
///   current stat doesn't exactly match it).
/// - [`Validation::TrustState`] is the explicit performance opt-in:
///   it performs zero working-tree access, trusting `gat.lock` plus the
///   materialized state as ground truth and intentionally not detecting
///   or repairing a manually deleted/modified file when the desired row
///   still matches what gat last materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Validation {
    /// Validate the working tree with Git-style lazy stat-then-hash
    /// fallback logic.
    #[default]
    Validate,
    /// Trust `gat.lock` and the materialized state without touching the
    /// working tree at all.
    TrustState,
}

/// Bundles [`Validation`] with whether this reconciliation run should
/// rematerialize already-correct paths, so [`plan::plan_into_sink`]/
/// [`plan::plan_with_store`]/[`plan::merge_desired_with_prior`] receive
/// one reconciliation policy. Constructed once per sync run from
/// [`SyncOptions`]; the read-only [`plan`] API never builds one with
/// `rematerialize: true` -- only `gat sync --rematerialize`'s mutating
/// and dry-run paths do.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReconciliationPolicy {
    pub(crate) validation: Validation,
    pub(crate) rematerialize: bool,
}

/// Inputs that control one sync run's planning and execution.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// The shared path + include/exclude selection restricting which
    /// tracked paths this sync run reconciles (like every other
    /// selection-aware command). See [`gat_core::selection::Selection`].
    pub selection: gat_core::selection::Selection,
    /// Overwrite/remove locally modified gat-managed files instead of
    /// leaving them as conflicts.
    pub force: bool,
    /// Plan only; never touch disk, excludes, or the materialized state.
    pub dry_run: bool,
    /// How hard to check the working tree against the materialized state
    /// before replacing/removing a path. Default
    /// [`Validation::Validate`]: Git-style stat validation with hash
    /// fallback only on ambiguity. [`Validation::TrustState`] is the
    /// explicit opt-in that trusts `gat.lock` plus materialized state and
    /// performs zero working-tree access. See [`Validation`].
    pub validation: Validation,
    /// Recreate every selected already-correct managed file using the
    /// current `cache.materialization_strategy`, instead of leaving it
    /// untouched. Only `gat sync --rematerialize` sets this; `gat pull`
    /// and every installed Git hook stay at `false`, and the read-only
    /// [`plan`] API doesn't consult it at all. The sync engine itself
    /// (`effective_validation` in `engine::workspace::sync`, consulted by both
    /// [`sync_from_snapshot`]'s dry-run and mutating paths) forces
    /// [`Validation::Validate`] for the run whenever this is set,
    /// regardless of `validation`/`sync.trust_state` -- this is the
    /// authoritative enforcement point, not merely a CLI-level default
    /// (`crate::app`'s `resolve_sync_validation` computes the same thing
    /// early for documentation/UX purposes, but a `SyncOptions`
    /// constructed directly with `validation: Validation::TrustState,
    /// rematerialize: true` still gets the safe behavior here) -- so a
    /// locally modified file is reported as a conflict rather than
    /// silently overwritten.
    pub rematerialize: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// Path has no working-tree file yet (or it was deleted); materialize
    /// the desired object.
    Materialize(Entry),
    /// Path exists, safely matches what was last materialized; replace it
    /// with the new desired object.
    Replace(Entry),
    /// Path is materialized but absent from `gat.lock`; remove
    /// the working-tree file.
    Remove(gat_core::lexical_path::GatPath),
    /// The desired object is already what was last materialized at this
    /// path, but `SyncOptions::rematerialize` asked to recreate it anyway
    /// using the current `cache.materialization_strategy`. Never emitted
    /// unless `rematerialize` is set -- an ordinary sync leaves an
    /// already-correct file untouched (no action at all).
    Rematerialize(Entry),
    /// The working-tree file differs from what Gat last materialized
    /// there, so acting on it (`resolution`) would discard a local edit.
    /// Left untouched unless `SyncOptions::force` is set.
    Conflict {
        path: gat_core::lexical_path::GatPath,
        resolution: Box<Self>,
    },
    /// Desired object isn't in the local cache; the working-tree file (if
    /// any) is left exactly as-is.
    MissingObject {
        path: gat_core::lexical_path::GatPath,
        oid: Oid,
    },
    /// The working file differs from what was last materialized *and* the
    /// cache object Gat would replace it with does not match its own
    /// oid -- the cache object itself is corrupted (common with hardlink/
    /// symlink modes, where a write "through" the working file actually
    /// lands on the shared cache object). Neither `--force` nor a plain
    /// conflict resolution can fix this: the correct bytes have to come
    /// from a remote (`gat sync --repair`).
    Corrupted {
        path: gat_core::lexical_path::GatPath,
        oid: Oid,
    },
}

/// Ordered actions needed to reconcile the working tree with `gat.lock`.
#[derive(Debug, Clone, Default)]
pub struct SyncPlan {
    pub actions: Vec<SyncAction>,
    pub(crate) validated_state_mutations: Vec<gat_io::StateMutation>,
}

/// Receives one classified reconciliation decision at a time, in the
/// desired/materialized merge's deterministic path order. The merge
/// itself (`plan::plan_into_sink`) never has to know whether its output is
/// being collected into a complete [`SyncPlan`] or applied immediately --
/// only which [`PlanSink`] implementation it was handed.
///
/// - [`plan::CollectPlanSink`] reconstructs a complete [`SyncPlan`], for
///   the read-only planning API (`plan()`/`--dry-run`) and any caller that
///   genuinely needs the whole plan for inspection.
/// - [`execute::ExecutePlanSink`] applies each action (and persists each
///   stat refresh) as it arrives, in bounded batches, for the mutating
///   `Validation::Validate` sync path -- the same bounded plan/apply shape
///   `Validation::TrustState`'s dirty-row streaming fast path already uses.
pub(crate) trait PlanSink {
    /// Consume one classified action, in merge order.
    fn action(&mut self, action: SyncAction) -> Result<()>;
    /// Consume one validated stat-proof refresh discovered while planning
    /// (see [`SyncPlan::validated_state_mutations`]).
    fn state_mutation(&mut self, mutation: gat_io::StateMutation) -> Result<()>;
}

/// Summary of what a sync run did, or would do under `--dry-run`.
#[derive(Debug, Clone, Default)]
pub struct SyncOutcome {
    pub materialized: usize,
    pub replaced: usize,
    pub removed: usize,
    /// How many already-correct managed files were (or, under
    /// `--dry-run`, would be) recreated by `--rematerialize`. Distinct
    /// from `replaced`: the desired content/OID never changed here, only
    /// the on-disk representation (copy/reflink/hardlink/symlink).
    pub rematerialized: usize,
    pub conflicts: Vec<gat_core::lexical_path::GatPath>,
    pub missing: Vec<(gat_core::lexical_path::GatPath, Oid)>,
    /// Paths where the working file diverged *and* the cache object gat
    /// would have replaced it with is itself corrupted -- not fixable by
    /// `--force` (which would just write back the same bad bytes); needs
    /// `gat sync --repair`.
    pub corrupted: Vec<(gat_core::lexical_path::GatPath, Oid)>,
    pub dry_run: bool,
    /// Whether the gat-managed block in `.git/info/exclude` changed (or,
    /// under `--dry-run`, would change) to match `gat.lock`. Computed from
    /// `gat.lock` alone, independent of `conflicts`/`missing`/`corrupted`
    /// above -- excludes describe what gat manages, not whether every file
    /// was actually materialized.
    pub excludes_changed: bool,
    /// How many paths are (or, under `--dry-run`, would be) in the
    /// gat-managed exclude block.
    pub excludes_count: usize,
}

#[cfg(test)]
impl SyncOutcome {
    #[must_use]
    pub fn did_nothing(&self) -> bool {
        self.materialized == 0
            && self.replaced == 0
            && self.removed == 0
            && self.rematerialized == 0
            && self.is_clean()
    }
}

impl SyncOutcome {
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.conflicts.is_empty() && self.missing.is_empty() && self.corrupted.is_empty()
    }

    /// Fold `other` (one chunk's outcome, from the dirty-row streaming
    /// fast path in [`sync`]) into `self`. Counters add and per-path
    /// `Vec`s concatenate; `dry_run`/`excludes_changed`/`excludes_count`
    /// are set by the caller afterward and are untouched here.
    fn merge(&mut self, other: Self) {
        self.materialized += other.materialized;
        self.replaced += other.replaced;
        self.removed += other.removed;
        self.rematerialized += other.rematerialized;
        self.conflicts.extend(other.conflicts);
        self.missing.extend(other.missing);
        self.corrupted.extend(other.corrupted);
    }
}

/// Read-only sync planning: compares `gat.lock`, the materialized state,
/// and the working tree and returns the resulting [`SyncPlan`] without
/// touching disk beyond read-only stats/hashes. Used directly by
/// `--dry-run` and by any caller that needs to inspect a plan before
/// deciding whether to apply it; the mutating path
/// ([`sync_from_snapshot`]) drives the same underlying merge/decision
/// procedure through a bounded execution sink instead of collecting it.
pub fn plan(
    repo: &Repo,
    selection: &gat_core::selection::Selection,
    validation: Validation,
) -> Result<SyncPlan> {
    plan::plan(repo, selection, validation)
}

/// Reconcile a fixture repository using a fresh operation snapshot.
#[cfg(test)]
pub(crate) fn sync(repo: &Repo, opts: &SyncOptions) -> Result<SyncOutcome> {
    let cfg = repo.load_config().expect("load test repository config");
    let desired_revision = crate::repository_state::current_desired_revision(repo)
        .expect("observe test desired revision");
    let snapshot = crate::snapshot::Snapshot::new(repo.snapshot_input(cfg, desired_revision))
        .expect("build test operation snapshot");
    let session = crate::session::Session::for_test();
    let mut operation = crate::operation::Operation::new(repo, snapshot, session);
    sync_from_snapshot(&mut operation, opts, None)
}

fn set_phase(progress: Option<&ProgressHandle>, activity: ProgressActivity) {
    if let Some(progress) = progress {
        progress.set_activity(activity);
    }
}

/// Reconcile the working tree with `gat.lock` on an already-acquired
/// [`crate::operation::Operation`]: the sole *mutating* entry point
/// `engine::workspace::sync` exposes, and the one every real command
/// (`gat sync`, `gat pull`, every installed Git hook) ultimately calls,
/// dispatching to a strictly read-only planning path for `--dry-run` or
/// the single-mutation-gate path otherwise. The orchestration layer
/// acquires the `Operation` via its repository snapshot service; this function only ever
/// consumes one it's handed -- it never acquires acquisition authority
/// itself.
///
/// `--dry-run` never acquires [`gat_io::RepoLock`]/mutation
/// authority: it reads directly through
/// [`crate::operation::Operation`]. A real
/// (non-dry-run) reconciliation instead calls
/// `crate::operation::Operation::mutate` -- the one mutation
/// gate, and only proceeds with the
/// returned, already-revision-validated
/// `crate::mutation::MutationGuard` if that succeeds. One
/// top-level sync/pull/repair operation reuses the same `Operation`, so it
/// observes one coherent generation and one coherent session end to end,
/// including across the second
/// `sync_from_snapshot` call a repair-capable sync makes for post-repair
/// reconciliation.
pub fn sync_from_snapshot(
    operation: &mut crate::operation::Operation<'_>,
    opts: &SyncOptions,
    progress: Option<&gat_core::progress::ProgressHandle>,
) -> Result<SyncOutcome> {
    if opts.dry_run {
        return plan_dry_run(operation, opts, progress);
    }
    let mut guard = operation.mutate()?;
    execute_mutating_sync(&mut guard, opts, progress)
}

/// Resolves the validation mode a reconciliation run actually uses,
/// applying both downgrades to `Validation::Validate` that must never
/// depend solely on the CLI layer having already applied them:
///
/// - `opts.rematerialize` unconditionally forces `Validate`. This is the
///   authoritative enforcement point for that invariant: a
///   `--rematerialize` run must establish that the working-tree file
///   still matches materialized state before replacing it, so a locally
///   modified file becomes a conflict instead of being silently
///   overwritten). `crate::app`'s `resolve_sync_validation` also computes
///   this for documentation/early-CLI-error purposes, but correctness
///   must not depend on that -- any caller that builds a `SyncOptions`
///   directly gets the same guarantee here,
///   inside the engine itself.
/// - an outstanding `reconciliation_meta.validation_required` flag:
///   `gat system repair state` destructively rebuilt the
///   materialized ledger without being able to prove it still reflects
///   the working tree) downgrades an explicit `Validation::TrustState`
///   the same way, since trusting an admittedly-unproven ledger would be
///   unsafe regardless of why `TrustState` was requested.
fn effective_validation(opts: &SyncOptions, validation_required: bool) -> Validation {
    if opts.rematerialize {
        return Validation::Validate;
    }
    if opts.validation == Validation::TrustState && validation_required {
        Validation::Validate
    } else {
        opts.validation
    }
}

/// `--dry-run`'s strict read-only planning path: never
/// registers cache usage, never creates/opens-for-write the
/// materialized-state `SQLite` database, and never persists an incremental
/// refresh of the desired-lock mirror. A materialized-state database that
/// doesn't exist yet (or is on an outdated schema that a normal open would
/// discard and rebuild anyway) is simply treated as empty rather than
/// created -- see `StateStore::open_if_exists` and
/// `plan::plan_with_store`'s `Option<&StateStore>`. Takes only an
/// [`crate::operation::Operation`] -- no
/// [`gat_io::RepoLock`], no `crate::mutation::MutationGuard`
/// -- because it is read-only by construction and must never acquire
/// mutation authority it doesn't need.
fn plan_dry_run(
    operation: &mut crate::operation::Operation<'_>,
    opts: &SyncOptions,
    progress: Option<&gat_core::progress::ProgressHandle>,
) -> Result<SyncOutcome> {
    let (repo, snapshot, session) = operation.split_for_sync();
    let cfg = snapshot.config();
    let cache_root = snapshot.cache_root();
    let merge_window = session.limits().sync.merge_window.get();

    set_phase(progress, ProgressActivity::LoadingMaterializedState);
    let store = StateStore::open_if_exists(repo.layout())?;
    // `gat system repair state` can destructively reset the
    // materialized ledger without being able to prove it still
    // reflects the working tree, recording that
    // condition directly in the database it just rebuilt
    // (`reconciliation_meta.validation_required`) rather than a
    // separate marker file; while it's set, `Validation::TrustState`
    // must not bypass working-tree validation, so it's transparently
    // downgraded to `Validation::Validate` here rather than trusting a
    // ledger gat itself just admitted it can't vouch for. Consulting
    // the already-open store's cached flag costs nothing beyond the
    // open this read-only path already does. `opts.rematerialize` forces
    // the same downgrade unconditionally -- see `effective_validation`.
    let validation = effective_validation(
        opts,
        store.as_ref().is_some_and(StateStore::validation_required),
    );
    set_phase(progress, ProgressActivity::LoadingTrackedState);
    let desired_lock = LockStore::load_repository(repo.layout())?;
    set_phase(progress, ProgressActivity::ValidatingWorkingTree);
    // Tally directly through `DryRunPlanSink`
    // instead of collecting a complete `SyncPlan` (`plan::plan_with_store`
    // + `execute::execute_with_store`) purely to throw it away after one
    // pass -- `--rematerialize --dry-run` on a large, entirely clean
    // repository would otherwise retain one `SyncAction::Rematerialize`
    // per selected path just to count them.
    let mut sink = execute::DryRunPlanSink::new(opts.force);
    session
        .cache_session_mut()
        .sync_scoped_cache(cache_root, |cache| {
            plan::plan_into_sink(
                repo,
                cache,
                plan::DesiredSource {
                    store: store.as_ref(),
                    desired_lock: Some(&desired_lock),
                },
                &opts.selection,
                ReconciliationPolicy {
                    validation,
                    rematerialize: opts.rematerialize,
                },
                merge_window,
                &mut sink,
            )
        })?;
    set_phase(progress, ProgressActivity::PlanningChanges);
    let mut outcome = sink.finish();
    let excludes_status =
        excludes::sync_from_lock_with_config(repo, &desired_lock, true, cfg).map_err(Box::new)?;
    outcome.excludes_changed = excludes_status.changed;
    outcome.excludes_count = excludes_status.count;
    Ok(outcome)
}

/// The mutating reconciliation phase requires
/// an already lock-held, revision-validated
/// `crate::mutation::MutationGuard` rather than independently
/// accepting a `Repo`, config/snapshot pieces, session/cache handles, a
/// revision value, or an optional lock -- the guard is the *only* way to
/// reach this function's config/objects-dir/link-modes/limits/cache, and
/// constructing one (`Operation::mutate`) already proved this operation's
/// captured desired revision still matches the repository's current
/// canonical desired state before this function ever runs.
fn execute_mutating_sync(
    guard: &mut crate::mutation::MutationGuard<'_, '_>,
    opts: &SyncOptions,
    progress: Option<&gat_core::progress::ProgressHandle>,
) -> Result<SyncOutcome> {
    // `repo` alone (unlike `snapshot`/`session`) does not borrow `guard`
    // itself -- `Operation::repo` returns the `&'repo Repo` the operation
    // was built from, independent of `guard`'s own borrow -- so it can be
    // read before `split_for_sync()`'s mutable borrow of `guard` begins,
    // leaving `guard` free for the single `require_desired_identity` call
    // below.
    let repo = guard.operation().repo();
    set_phase(progress, ProgressActivity::LoadingMaterializedState);
    let mut store = StateStore::open(repo.layout())?;

    // Single refresh point:
    // both reconciliation branches below need the desired mirror refreshed
    // before they can stream from it, but unlike before, that refresh now
    // happens exactly once, here, before either branch begins mutating
    // anything -- and its returned `CanonicalDesiredIdentity` is checked
    // against this guard's already-lock-revalidated `DesiredRevision`
    // immediately afterward. A mismatch (a concurrent `gat.lock` write
    // this guard's own lock-acquisition somehow didn't observe, or a
    // `refresh()` bug) is rejected right here, before any
    // worktree/materialized mutation, rather than only after one branch
    // already started applying actions from a view of desired state that
    // was never actually proven to match.
    set_phase(progress, ProgressActivity::LoadingTrackedState);
    let refreshed = desired_index::refresh(repo, &mut store)?;
    guard
        .require_desired_identity(refreshed.desired_identity)
        .map_err(crate::repository_state::DesiredRevisionError::from)?;

    let (repo, snapshot, session) = guard.operation_mut().split_for_sync();
    let cfg = snapshot.config();
    let cache_root = snapshot.cache_root();
    let mode = snapshot.materialization_strategy();
    let sync_window = session.limits().sync.dirty_window.get();
    let merge_window = session.limits().sync.merge_window.get();

    // Same validation-required downgrade as the dry-run path above, but
    // reading the flag this store already loaded when it opened, so this
    // adds no extra query/filesystem operation to the normal sync path.
    // `opts.rematerialize` forces the same downgrade unconditionally, and
    // is the authoritative enforcement point for that invariant -- see
    // `effective_validation`; this is what actually keeps a
    // `SyncOptions { validation: TrustState, rematerialize: true, .. }`
    // out of the dirty-row `TrustState` fast path just below, regardless
    // of whether a caller already resolved `Validate` beforehand.
    let validation = effective_validation(opts, store.validation_required());

    // Reconciliation fast path: any unfiltered *or* path/glob-scoped
    // `Validation::TrustState` sync is exactly the case the single-state-row
    // `state` mirror (see `desired_index::refresh`) was built to
    // accelerate. Path scope is applied as a lexical range directly
    // against the dirty partial index (`dirty_rows_in_scope_after`);
    // include/exclude globs are applied in Rust (`GlobFilter`, kept
    // authoritative -- no SQL glob translation) against just the dirty
    // candidates in scope, never against the full repository.
    //
    // Dirty rows are streamed in bounded, keyset-paginated chunks (the
    // configured `sync_dirty_window`, `crate::limits::SyncLimits::dirty_window`)
    // rather than loaded/planned/applied all at once: a
    // fresh sync of a huge repo would otherwise hold every dirty row plus
    // every corresponding `SyncAction` in memory simultaneously. Each
    // chunk's actions are applied (and their materialized-state changes
    // batched/flushed -- see `execute::apply_actions`) before the next
    // chunk is even fetched, so peak memory and the SQLite batch size are
    // both bounded independent of how many paths are dirty overall.
    if validation == Validation::TrustState {
        let scope = opts.selection.scope_path();
        let mut outcome = SyncOutcome::default();
        session
            .cache_session_mut()
            .sync_scoped_cache(cache_root, |cache| -> Result<()> {
                let mut after: Option<gat_core::lexical_path::GatPath> = None;
                loop {
                    let rows =
                        store.dirty_rows_in_scope_after(scope, after.as_ref(), sync_window)?;
                    if rows.is_empty() {
                        break;
                    }
                    #[cfg(any(test, feature = "test-support"))]
                    test_support::record_dirty_rows(rows.len());
                    after = Some(rows[rows.len() - 1].path.clone());
                    let actions = plan::plan_from_dirty_rows(cache, rows, &opts.selection)?;
                    let chunk_plan = SyncPlan {
                        actions,
                        validated_state_mutations: Vec::new(),
                    };
                    set_phase(progress, ProgressActivity::ApplyingChanges);
                    let chunk_outcome = execute::apply_actions(
                        repo, cache, mode, &mut store, chunk_plan, opts.force,
                    )?;
                    outcome.merge(chunk_outcome);
                }
                Ok(())
            })?;

        // Exclude fast path:
        // `.git/info/exclude`'s gat-managed block depends only on the
        // desired lock set's content identity
        // (`store.desired_fingerprint()`, maintained incrementally by
        // `desired_index::refresh`) and `git.ignore_patterns` -- never on
        // `opts.path`/`include`/`exclude`, which only scope which paths
        // *this* sync touches on disk, not what gat manages overall. The
        // shared coordinator proves this without ever constructing the
        // full desired `Lock`, and without a full desired-path stream
        // unless its own stat/content proof tiers both miss.
        let status =
            excludes::sync_from_store_fast_path(repo, &mut store, cfg).map_err(Box::new)?;
        outcome.excludes_changed = status.changed;
        outcome.excludes_count = status.count;
        return Ok(outcome);
    }

    // `Validation::Validate`'s full desired/materialized merge (unlike the
    // `TrustState` dirty-row fast path above) still walks every managed
    // path, but it avoids doing so from a freshly parsed
    // `gat.lock`: once the desired mirror is refreshed, planning streams
    // desired rows straight from SQLite, so a warm validated sync reads
    // zero `gat.lock` bytes on the planning hot path.
    set_phase(progress, ProgressActivity::ValidatingWorkingTree);
    set_phase(progress, ProgressActivity::ApplyingChanges);
    // Bounded plan/apply: the merge streams each classified
    // action straight into `ExecutePlanSink`, which applies it to the
    // working tree and persists its materialized-state delta in the same
    // bounded batches used for collected plans -- unlike the
    // dry-run path above, this never collects a complete `SyncPlan` in
    // memory first. The sink writes through its own, separately opened
    // `StateStore` connection (SQLite WAL already lets one writer
    // and one reader share the same database file) rather than `store`
    // itself, since `store` stays borrowed read-only for the whole merge
    // below (streaming desired/materialized rows) and Rust can't hand out
    // a `&mut` to the same value at the same time.
    let mut write_store = StateStore::open(repo.layout())?;
    let mut outcome = session
        .cache_session_mut()
        .sync_scoped_cache(cache_root, |cache| {
            let mut sink =
                execute::ExecutePlanSink::new(repo, cache, mode, &mut write_store, opts.force);
            plan::plan_into_sink(
                repo,
                cache,
                plan::DesiredSource {
                    store: Some(&store),
                    desired_lock: None,
                },
                &opts.selection,
                ReconciliationPolicy {
                    validation,
                    rematerialize: opts.rematerialize,
                },
                merge_window,
                &mut sink,
            )?;
            sink.finish()
        })?;
    let excludes_status =
        excludes::sync_from_store_fast_path(repo, &mut store, cfg).map_err(Box::new)?;
    outcome.excludes_changed = excludes_status.changed;
    outcome.excludes_count = excludes_status.count;
    // A full, unrestricted, *clean* `Validation::Validate` sync has just
    // reconciled every managed path against the working tree with nothing
    // left unresolved, so it re-establishes the materialized ledger as
    // trustworthy even if an earlier `gat system repair state` reset it
    // without proof -- clear the persisted flag so `Validation::TrustState`
    // may resume trusting it. A path/glob-scoped sync only reconciles part
    // of the tree, and an outstanding conflict/missing/corrupted path means
    // reconciliation didn't actually finish -- both deliberately leave the
    // flag in place.
    if validation == Validation::Validate
        && opts.selection.is_unrestricted()
        && outcome.is_clean()
        && store.validation_required()
    {
        store.set_validation_required(false)?;
    }
    Ok(outcome)
}

/// `Validation::TrustState` dirty-row cursor instrumentation, colocated
/// with the loop it measures -- mirrors
/// the merge-buffer high-water counter.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static DIRTY_ROWS_HIGH_WATER: Cell<usize> = const { Cell::new(0) };
    }

    /// Records one `dirty_rows_in_scope_after` chunk's row count, for a
    /// structural test to assert it never exceeds the configured
    /// `sync_dirty_window`.
    pub(crate) fn record_dirty_rows(size: usize) {
        DIRTY_ROWS_HIGH_WATER.with(|c| c.set(c.get().max(size)));
    }

    pub fn dirty_rows_high_water() -> usize {
        DIRTY_ROWS_HIGH_WATER.with(Cell::get)
    }

    #[must_use]
    pub fn do_rematerialize_calls() -> usize {
        super::execute::test_support::do_rematerialize_calls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_mutation::load_materialized_for_test;
    use crate::test_harness::git_repo;
    use gat_core::lexical_path::GatPath;
    use gat_core::oid::Oid;
    use std::path::Path;

    fn trust_state_opts() -> SyncOptions {
        SyncOptions {
            validation: Validation::TrustState,
            ..Default::default()
        }
    }

    fn track(repo: &Repo, path: &str, content: &[u8]) -> Entry {
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let (ingested, _) = repo
            .resolved_cache_root()
            .unwrap()
            .writer()
            .ingest(content)
            .unwrap();
        let oid = ingested.oid;
        lock.upsert(GatPath::parse_canonical(path).unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();
        Entry {
            path: gat_core::lexical_path::GatPath::parse_canonical(path).unwrap(),
            oid,
        }
    }

    /// `gat sync --dry-run` on a repo that has never been
    /// synced yet must never create the materialized-state `SQLite`
    /// database as a side effect of merely
    /// planning. A materialize/replace/remove/conflict-capable plan is
    /// still produced correctly against the implicit "nothing
    /// materialized yet" state.
    #[test]
    fn sync_dry_run_never_creates_the_materialized_state_database() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");

        let outcome = sync(
            &repo,
            &SyncOptions {
                dry_run: true,
                validation: Validation::TrustState,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.dry_run);
        assert!(
            !tmp.path().join(".gat/state/state.sqlite3").exists(),
            "--dry-run must never create the materialized-state database"
        );
        assert!(
            !tmp.path().join("a.bin").exists(),
            "--dry-run must never materialize a file"
        );
    }

    /// A pre-existing materialized-state database (from an earlier
    /// non-dry-run sync) is read but never mutated by a later `--dry-run`
    /// sync: reconciling a second change under `--dry-run` reports it
    /// without persisting anything.
    #[test]
    fn sync_dry_run_reads_but_never_mutates_an_existing_materialized_state_database() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &trust_state_opts()).unwrap();
        let database = tmp.path().join(".gat/state/state.sqlite3");
        assert!(database.exists());
        let db_bytes_before = std::fs::read(&database).unwrap();

        track(&repo, "b.bin", b"world");
        let outcome = sync(
            &repo,
            &SyncOptions {
                dry_run: true,
                validation: Validation::TrustState,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.dry_run);
        assert!(
            !tmp.path().join("b.bin").exists(),
            "--dry-run must never materialize a new file"
        );
        let db_bytes_after = std::fs::read(database).unwrap();
        assert_eq!(
            db_bytes_before, db_bytes_after,
            "--dry-run must never write to an existing materialized-state database"
        );
    }

    /// Characterization test: `sync()` (the actual cache-materializing
    /// entry point used by `gat sync`/`gat pull`/hooks) does register this
    /// repo, at that explicit boundary rather than as a side effect of
    /// resolving `objects_dir()`.
    #[test]
    fn repeated_sync_is_a_no_op() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &trust_state_opts()).unwrap();

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert!(outcome.did_nothing());
    }

    /// A warm, unscoped `Validation::Validate` (the default) sync must not
    /// *read* `gat.lock`'s content at all: once the desired mirror is
    /// refreshed, the equivalent `Lock` used for planning/excludes is
    /// read back out of `SQLite` (`StateStore::load_desired_as_lock`),
    /// never re-parsed from the authoritative file. Stripping all
    /// permissions from `gat.lock` after warm-up -- so `stat` still
    /// succeeds (proving the mirror is still refreshed) but `open`/`read`
    /// would fail -- and asserting a clean, unscoped default sync still
    /// succeeds as a no-op is a strong proxy for "zero `gat.lock` bytes
    /// read" without instrumenting file I/O directly.
    #[cfg(unix)]
    #[test]
    fn warm_validated_sync_does_not_read_gat_lock_content() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        // The flat shard's post-write proof describes exactly the bytes
        // just written, so it's immediately reusable -- no separate
        // settling sync is needed before this stat-only check.
        let lock_path = tmp.path().join("gat.lock");

        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let restore = scopeguard(&lock_path);

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();
        drop(restore);

        assert!(outcome.did_nothing());
    }

    #[cfg(unix)]
    fn scopeguard(path: &Path) -> impl Drop + '_ {
        struct Guard<'a>(&'a Path);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o644));
            }
        }
        Guard(path)
    }

    /// Both `Validation::
    /// Validate` (exercised here, and the default per
    /// `validation_defaults_to_validate` below) and `Validation::
    /// TrustState` call the exact same shared
    /// `excludes::sync_from_store_fast_path` coordinator (see its call
    /// sites above), so a warm default sync must take the identical
    /// zero-content-read tier-1 fast path for `.git/info/exclude` that
    /// `TrustState` does -- not merely skip `gat.lock`, which the sibling
    /// test above already covers for the desired-shard side. Revokes
    /// read permission on `.git/info/exclude` itself (its stat still
    /// succeeds under `chmod 0`, only a real `open`+`read` would fail) and
    /// asserts a further default sync still succeeds as a no-op.
    #[cfg(unix)]
    #[test]
    fn warm_validated_sync_does_not_read_git_info_exclude_content() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        let exclude_path = tmp.path().join(".git").join("info").join("exclude");
        assert!(
            exclude_path.is_file(),
            "the first sync must have materialized a gat-managed exclude block"
        );

        // The first sync's post-write proof describes exactly the bytes
        // just written, so it's immediately reusable -- no separate
        // settling sync is needed before a truly stat-only tier-1 hit.
        std::fs::set_permissions(&exclude_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let restore = scopeguard(&exclude_path);

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();
        drop(restore);

        assert!(outcome.did_nothing());
        assert!(
            !outcome.excludes_changed,
            "a warm default sync must confirm the exclude block unchanged via tier 1, \
             the same zero-read fast path Validation::TrustState uses"
        );
    }

    #[test]
    fn validation_defaults_to_validate() {
        assert_eq!(Validation::default(), Validation::Validate);
        assert_eq!(SyncOptions::default().validation, Validation::Validate);
    }

    /// While `gat system repair state`'s validation-required flag is set
    /// in the materialized-state database, `Validation::TrustState` must
    /// be transparently downgraded to `Validation::Validate` (see
    /// `sync_from_snapshot`) rather than trusting a materialized ledger
    /// gat itself just admitted it can't vouch for -- a locally modified
    /// file must surface as a conflict, not be silently left
    /// alone/overwritten by the trust-state fast path.
    #[test]
    fn trust_state_is_downgraded_to_validate_while_validation_required_is_set() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        std::fs::write(tmp.path().join("a.bin"), b"local edit").unwrap();
        StateStore::open(repo.layout())
            .unwrap()
            .set_validation_required(true)
            .unwrap();

        let outcome = sync(&repo, &trust_state_opts()).unwrap();

        assert_eq!(
            outcome.conflicts,
            vec!["a.bin".to_string()],
            "a locally modified file must surface as a conflict once trust-state is downgraded"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"local edit",
            "the trust-state fast path must not silently keep/overwrite the file while unvalidated"
        );
        assert!(
            StateStore::open(repo.layout())
                .unwrap()
                .validation_required(),
            "a scoped/incomplete sync must not clear the validation-required flag"
        );
    }

    /// A full, unrestricted `Validation::Validate` sync re-establishes
    /// trust in the materialized ledger and clears the validation-required
    /// flag, so a later `Validation::TrustState` sync may resume trusting
    /// it.
    #[test]
    fn a_full_validated_sync_clears_the_validation_required_flag() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        StateStore::open(repo.layout())
            .unwrap()
            .set_validation_required(true)
            .unwrap();

        sync(
            &repo,
            &SyncOptions {
                validation: Validation::Validate,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(
            !StateStore::open(repo.layout())
                .unwrap()
                .validation_required(),
            "a full validated sync must clear the validation-required flag"
        );
    }

    /// A clean, unscoped `Validation::TrustState` sync (the fast path) must
    /// record an exclude fingerprint that matches the current desired
    /// lock set/`git.ignore_patterns`, so the *next* clean sync can
    /// skip exclude regeneration -- and, crucially, skip loading the
    /// full desired lock entirely.
    #[test]
    fn clean_sync_records_an_exclude_fingerprint_matching_current_desired_state() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");

        sync(&repo, &trust_state_opts()).unwrap();

        let mut store = StateStore::open(repo.layout()).unwrap();
        let cfg = repo.load_config().unwrap();
        let expected = crate::excludes::fingerprint(
            &store.desired_fingerprint().unwrap(),
            cfg.git.effective_ignore_patterns(),
        );
        // The write just happened, and its post-write proof describes
        // exactly the bytes just written, so it is recorded immediately
        // -- either a stat-proof match or a content-confirmed match is
        // acceptable, but the fast path must never fall back to a full
        // rebuild.
        let record = store.exclude_record().unwrap();
        assert_eq!(record.fingerprint(), Some(expected));
        assert_eq!(record.count(), 1);
        let outcome = crate::excludes::sync_from_store_fast_path(&repo, &mut store, &cfg).unwrap();
        assert!(
            !outcome.changed,
            "expected a fast-path match, not a rebuild"
        );
        assert_eq!(outcome.count, 1);
    }

    /// A warmed clean sync's exclude fast path must not read the
    /// contents of `.git/info/exclude` at all once a stat match has been
    /// recorded -- reading/hashing the whole file on every sync would
    /// make it O(N) again, since the default managed block contains one
    /// exact rule per tracked path. Proven here by making the file
    /// unreadable (but still stat-able) and asserting the clean sync
    /// still succeeds and reports no change.
    #[test]
    #[cfg(unix)]
    fn clean_sync_does_not_read_the_exclude_file_once_its_stat_is_safely_unchanged() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");

        sync(&repo, &trust_state_opts()).unwrap();
        // The first sync's post-write proof describes exactly the bytes
        // just written, so it's immediately trusted outright for the
        // next sync's stat-only check.
        let exclude_path = tmp.path().join(".git/info/exclude");

        let mut perms = std::fs::metadata(&exclude_path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&exclude_path, perms).unwrap();

        let outcome = sync(&repo, &trust_state_opts()).unwrap();

        // Restore permissions regardless of the assertion outcome so
        // cleanup doesn't fail.
        let mut perms = std::fs::metadata(&exclude_path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&exclude_path, perms).unwrap();

        assert!(!outcome.excludes_changed);
    }

    /// Changing `git.ignore_patterns` between syncs must still trigger
    /// a full exclude rebuild on the next clean sync, even though
    /// nothing in `gat.lock` itself changed -- the fingerprint has to
    /// fold in exclude inputs, not just the desired lock set.
    #[test]
    fn sync_regenerates_excludes_after_ignore_patterns_change_alone() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "data/a.bin", b"hello");
        sync(&repo, &trust_state_opts()).unwrap();
        let before = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(before.contains("data/a.bin"));

        let mut cfg = repo.load_config().unwrap();
        cfg.git.ignore_patterns = Some(vec![
            gat_core::git_ignore::GitIgnorePattern::parse("/data/").unwrap(),
        ]);
        repo.save_config(&cfg).unwrap();

        sync(&repo, &trust_state_opts()).unwrap();

        let after = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(after.contains("/data/"));
        assert!(
            !after.contains("data/a.bin"),
            "the exact rule should be suppressed once the pattern covers it"
        );
    }

    /// The exclude fingerprint alone only proves the *inputs* that would
    /// regenerate `.git/info/exclude` are unchanged; deleting the file
    /// between two otherwise-clean syncs must still trigger a rebuild,
    /// not be masked by an unchanged fingerprint.
    #[test]
    fn sync_rewrites_info_exclude_after_it_was_deleted_between_clean_syncs() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &trust_state_opts()).unwrap();
        let exclude_path = tmp.path().join(".git/info/exclude");
        assert!(exclude_path.exists());

        std::fs::remove_file(&exclude_path).unwrap();

        sync(&repo, &trust_state_opts()).unwrap();

        let text = std::fs::read_to_string(&exclude_path).unwrap();
        assert!(text.contains("a.bin"));
    }

    /// Externally editing/removing gat's managed block (while leaving the
    /// file itself in place) must also be detected and repaired, not
    /// masked by an unchanged desired-lock/`git.ignore_patterns`
    /// fingerprint.
    #[test]
    fn sync_repairs_an_externally_modified_managed_block() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &trust_state_opts()).unwrap();
        let exclude_path = tmp.path().join(".git/info/exclude");

        // Simulate external tampering: drop the whole file's content down
        // to something without Gat's managed block.
        std::fs::write(&exclude_path, "# hand-edited, no gat block\n").unwrap();

        sync(&repo, &trust_state_opts()).unwrap();

        let text = std::fs::read_to_string(&exclude_path).unwrap();
        assert!(text.contains("a.bin"));
        assert!(text.contains("# hand-edited, no gat block"));
    }

    #[test]
    fn concurrent_sync_attempts_do_not_corrupt_state() {
        let tmp = git_repo();
        let repo = std::sync::Arc::new(
            crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf()),
        );
        for i in 0..20 {
            track(
                &repo,
                &format!("f{i}.bin"),
                format!("content-{i}").as_bytes(),
            );
        }

        let start = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let repo = repo.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    sync(&repo, &SyncOptions::default())
                })
            })
            .collect();
        let mut succeeded = 0;
        for handle in handles {
            match handle.join().unwrap() {
                Ok(_) => succeeded += 1,
                // Contention may exceed the product's bounded lock wait. A
                // rejected attempt must still leave the successful sync intact.
                Err(error) => assert!(
                    matches!(
                        error.kind(),
                        error::SyncErrorKind::MutationAuthority(
                            error::MutationAuthorityFailureKind::RepositoryLocked
                        )
                    ),
                    "unexpected sync failure: {error:?}"
                ),
            }
        }
        assert!(
            succeeded > 0,
            "at least one contender must acquire the lock"
        );

        let state = load_materialized_for_test(&repo).unwrap();
        assert_eq!(state.entries.len(), 20);
        for i in 0..20 {
            assert_eq!(
                std::fs::read(tmp.path().join(format!("f{i}.bin"))).unwrap(),
                format!("content-{i}").as_bytes()
            );
        }
    }

    #[test]
    fn is_clean_and_did_nothing_account_for_corrupted() {
        let mut outcome = SyncOutcome::default();
        assert!(outcome.is_clean());
        assert!(outcome.did_nothing());
        outcome.corrupted.push((
            gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
            gat_core::oid::Oid::from_hex(&"0".repeat(64)).unwrap(),
        ));
        assert!(!outcome.is_clean());
        assert!(!outcome.did_nothing());
    }

    /// Ingests `count` distinct, content-addressed test files in
    /// parallel (each call is independently fsync'd durability I/O, the
    /// dominant cost of seeding a large fixture) and returns each file's
    /// index alongside its resulting oid, in index order.
    fn ingest_many_test_files(cache_root: &gat_io::CacheRoot, count: usize) -> Vec<(usize, Oid)> {
        use rayon::prelude::*;
        let writer = cache_root.writer();
        let mut ingested: Vec<(usize, Oid)> = (0..count)
            .into_par_iter()
            .map(|i| {
                let content = format!("data-{i}").into_bytes();
                let ingested = writer.ingest(content.as_slice()).unwrap().0;
                (i, ingested.oid)
            })
            .collect();
        ingested.sort_unstable_by_key(|(i, _)| *i);
        ingested
    }

    /// A fresh sync with more dirty paths than one configured
    /// `sync_dirty_window` must still materialize/persist every path
    /// correctly -- the streaming fast path in `sync()` fetches, plans,
    /// and applies dirty rows in several bounded chunks rather than one
    /// `Vec` covering everything, and each chunk's materialized-state
    /// changes are batched/flushed independently
    /// (`execute::apply_actions`/`PendingBatch`). This exercises the
    /// keyset pagination across a chunk boundary and the batch flush
    /// boundary together, end to end. Uses a small, explicit
    /// `dirty_window` (rather than the production default) and roughly 7
    /// paths sharing one ingested object/oid, crossing multiple dirty
    /// chunks without thousands of ingests/materializations.
    #[test]
    fn fresh_sync_beyond_one_dirty_chunk_materializes_every_path() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut limits = crate::limits::ExecutionLimits::tiny();
        let dirty_window = 3;
        limits.sync.dirty_window = std::num::NonZeroUsize::new(dirty_window).unwrap();
        let count = dirty_window * 2 + 1;
        let ingested = repo
            .resolved_cache_root()
            .unwrap()
            .writer()
            .ingest(&b"data"[..])
            .unwrap()
            .0;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();

        let outcome = sync_with_limits(&repo, &trust_state_opts(), limits);
        assert_eq!(outcome.materialized, count);
        assert!(outcome.is_clean());

        for i in 0..count {
            assert_eq!(
                std::fs::read(tmp.path().join(format!("file-{i:06}.bin"))).unwrap(),
                b"data"
            );
        }

        // The reconciliation mirror must agree there is nothing left
        // dirty -- confirms every chunk's batch was actually persisted,
        // not just written to disk.
        let store = StateStore::open(repo.layout()).unwrap();
        assert!(!store.has_dirty().unwrap());

        // A second sync must be a true no-op.
        let outcome = sync_with_limits(&repo, &trust_state_opts(), limits);
        assert!(outcome.did_nothing());
    }

    /// Builds an [`crate::operation::Operation`] for `repo`
    /// using explicit `limits` instead of production defaults, mirroring
    /// the same coherent snapshot/session acquisition
    /// [`crate::repo_snapshot::acquire_operation_without_desired_state`]
    /// performs (but with test-chosen `limits` instead of
    /// `ExecutionLimits`'s production default), so tests can exercise
    /// independently-tuned resource bounds through the real
    /// `sync_from_snapshot` entry point rather than a parallel test-only
    /// sync path.
    fn sync_with_limits(
        repo: &Repo,
        opts: &SyncOptions,
        limits: crate::limits::ExecutionLimits,
    ) -> SyncOutcome {
        let cfg = repo.load_config().unwrap();
        let desired_revision = {
            let _guard = repo.acquire_configuration_lock().unwrap();
            crate::repository_state::current_desired_revision(repo).unwrap()
        };
        let snapshot =
            crate::snapshot::Snapshot::new(repo.snapshot_input(cfg, desired_revision)).unwrap();
        let session = crate::session::Session::with_limits(limits);
        let mut operation = crate::operation::Operation::new(repo, snapshot, session);
        sync_from_snapshot(&mut operation, opts, None).unwrap()
    }

    /// `sync_dirty_window` and
    /// `sync_merge_window` are independently configurable resources --
    /// retuning one must never silently retune the other, or the number
    /// of dirty/merge chunks a sync performs. Uses deliberately different
    /// tiny values for each window (distinct from every other
    /// [`crate::limits::ExecutionLimits`] field) and asserts a
    /// `Validation::TrustState` sync, which only exercises the dirty-row
    /// cursor and never the desired/materialized merge, still correctly
    /// materializes a fixture sized against `sync_merge_window` rather
    /// than `sync_dirty_window` -- proving the merge window has no
    /// influence on this path at all, and vice versa in the paired
    /// `Validation::Validate` test below.
    #[test]
    fn sync_dirty_window_and_merge_window_tune_independently_under_trust_state() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.dirty_window = std::num::NonZeroUsize::new(3).unwrap();
        limits.sync.merge_window = std::num::NonZeroUsize::new(11).unwrap();
        // Sized against the *larger* of the two so a dirty-window-only
        // path (`TrustState`) is forced across several dirty chunks while
        // staying far below the merge window -- if the merge window were
        // silently governing this path instead, this count would still
        // fit in a single merge-window-sized batch and the test would not
        // distinguish the two resources.
        let count = limits.sync.dirty_window.get() * 4 + 1;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for (i, oid) in ingest_many_test_files(&repo.resolved_cache_root().unwrap(), count) {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                oid,
            );
        }
        repo.save_lock(&lock).unwrap();

        let outcome = sync_with_limits(&repo, &trust_state_opts(), limits);
        assert_eq!(outcome.materialized, count);
        assert!(outcome.is_clean());
        for i in 0..count {
            assert_eq!(
                std::fs::read(tmp.path().join(format!("file-{i:06}.bin"))).unwrap(),
                format!("data-{i}").as_bytes()
            );
        }
    }

    /// `Validation::TrustState`'s dirty-row
    /// cursor must never retain more than one configured
    /// `sync_dirty_window` of rows at a time, even for a fixture spanning
    /// several dirty chunks -- proving the memory-instrumentation
    /// dimension for this resource, mirroring the existing merge-buffer
    /// high-water assertion.
    #[test]
    fn sync_dirty_window_high_water_never_exceeds_the_configured_window() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.dirty_window = std::num::NonZeroUsize::new(4).unwrap();
        let count = limits.sync.dirty_window.get() * 5 + 1;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for (i, oid) in ingest_many_test_files(&repo.resolved_cache_root().unwrap(), count) {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                oid,
            );
        }
        repo.save_lock(&lock).unwrap();

        let outcome = sync_with_limits(&repo, &trust_state_opts(), limits);
        assert_eq!(outcome.materialized, count);
        assert!(
            test_support::dirty_rows_high_water() <= limits.sync.dirty_window.get(),
            "dirty-row high-water {} exceeded sync_dirty_window {}",
            test_support::dirty_rows_high_water(),
            limits.sync.dirty_window
        );
    }

    /// the desired/materialized merge (governed by `sync_merge_window`)
    /// with a fixture sized against the *dirty* window instead, proving
    /// `sync_dirty_window` has no bearing on the merge path either --
    /// together the two tests show changing one resource cannot silently
    /// retune the other's effective chunking.
    #[test]
    fn sync_dirty_window_and_merge_window_tune_independently_under_validate() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.dirty_window = std::num::NonZeroUsize::new(11).unwrap();
        limits.sync.merge_window = std::num::NonZeroUsize::new(3).unwrap();
        let count = limits.sync.merge_window.get() * 4 + 1;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for (i, oid) in ingest_many_test_files(&repo.resolved_cache_root().unwrap(), count) {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                oid,
            );
        }
        repo.save_lock(&lock).unwrap();

        let outcome = sync_with_limits(&repo, &SyncOptions::default(), limits);
        assert_eq!(outcome.materialized, count);
        assert!(outcome.is_clean());
        for i in 0..count {
            assert_eq!(
                std::fs::read(tmp.path().join(format!("file-{i:06}.bin"))).unwrap(),
                format!("data-{i}").as_bytes()
            );
        }
    }

    /// The mutating `Validation::Validate` path streams the full
    /// desired/materialized merge into `ExecutePlanSink` (see
    /// `plan::MergeBuffer`), flushing whenever its buffered rows reach a
    /// configured `sync_merge_window` (`plan::MergeBuffer::merge_window`)
    /// -- this must still materialize and durably persist every path
    /// across more than one such boundary, exactly as the `TrustState`
    /// dirty-chunk path does above. Uses a small, explicit `merge_window`
    /// (rather than the production default) and roughly 7 paths sharing
    /// one ingested object/oid, crossing multiple merge batches without
    /// thousands of ingests/materializations.
    #[test]
    fn fresh_validated_sync_beyond_one_merge_batch_materializes_every_path() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut limits = crate::limits::ExecutionLimits::tiny();
        let merge_window = 3;
        limits.sync.merge_window = std::num::NonZeroUsize::new(merge_window).unwrap();
        let count = merge_window * 2 + 1;
        let ingested = repo
            .resolved_cache_root()
            .unwrap()
            .writer()
            .ingest(&b"data"[..])
            .unwrap()
            .0;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();

        let outcome = sync_with_limits(&repo, &SyncOptions::default(), limits);
        assert_eq!(outcome.materialized, count);
        assert!(outcome.is_clean());

        for i in 0..count {
            assert_eq!(
                std::fs::read(tmp.path().join(format!("file-{i:06}.bin"))).unwrap(),
                b"data"
            );
        }

        // The materialized-state DB (written through `ExecutePlanSink`'s
        // own connection, separate from the one the merge streamed reads
        // from) must agree there is nothing left dirty -- confirms every
        // merge batch was actually persisted, not just written to disk.
        let store = StateStore::open(repo.layout()).unwrap();
        assert!(!store.has_dirty().unwrap());

        // A second sync must be a true no-op.
        let outcome = sync_with_limits(&repo, &SyncOptions::default(), limits);
        assert!(outcome.did_nothing());
    }
}
