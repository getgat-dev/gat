//! Applying a computed [`super::SyncPlan`]: materializing/
//! replacing/removing worktree files through [`gat_io::WorktreeClient`]'s
//! confinement boundary, honoring `--force` conflict resolution, tallying
//! [`super::SyncOutcome`], updating materialized state, and refreshing
//! Gat-managed excludes.
//!
//! Preview and execution use separate sinks. They share decision resolution
//! and outcome accounting; only the execution sink owns worktree and state
//! persistence capabilities.

use super::{PlanSink, Result, SyncAction, SyncError, SyncOutcome, SyncPlan};
use crate::repository::Repository as Repo;
use gat_core::lexical_path::GatPath;
use gat_core::lock::Entry;
#[cfg(test)]
mod storage {
    pub use gat_io::{
        cache_hash_file_call_count as hash_file_call_count,
        cache_with_exclusive_hash_file_call_count as with_exclusive_hash_file_call_count,
    };
}
use gat_io::{
    CacheClient, MaterializeKind, RemovalReceipt, StateMutation, StateStore, WorktreeClient,
};

/// How many already-successful filesystem mutations accumulate before
/// their materialized-state changes are flushed in one durable
/// transaction (see [`PendingBatch`]). Chosen to keep at most a bounded
/// (not O(N)) amount of already-mutated-but-not-yet-durable filesystem
/// state exposed to a crash, while still turning a fresh/large sync's
/// state persistence from one commit per path into a small number of
/// commits overall.
const BATCH_SIZE: usize = 10000;

/// Filesystem mutations that have already succeeded, collected here
/// instead of being persisted one at a time, so their corresponding
/// materialized-state changes can be committed together in one
/// transaction via [`StateStore::apply_batch`]. `synchronous =
/// FULL` durability is unchanged -- every commit still fsyncs -- this
/// only reduces *how many* commits a sync needs, not how durable each
/// one is.
///
/// Invariant upheld by every caller: an entry is only ever pushed here
/// *after* its filesystem mutation has already succeeded, and the batch
/// is flushed before returning any error, so state can never claim a
/// filesystem mutation succeeded before it actually did. A crash can
/// leave at most one bounded batch of filesystem mutations ahead of
/// `SQLite`; those paths are still `dirty` and are safely reconciled again
/// on the next sync.
///
/// Mutations are stored in an ordered [`StateMutation`] list rather than
/// separate upsert/remove vectors, so [`Self::flush`] can preserve the
/// exact order actions were applied in (see [`StateMutation`]).
///
/// Opaque removal receipts piggyback on the same bounded batch/flush cadence.
/// A receipt exists only when this sync actually unlinked a file; after the
/// corresponding state mutations commit, the I/O capability consumes those
/// receipts to prune touched empty ancestors. A `NotFound` result still
/// reconciles materialized state but contributes no receipt. A plain `Vec`
/// remains sufficient because a plan never removes one path twice and the
/// private pruner deduplicates shared ancestors.
#[derive(Default)]
struct PendingBatch {
    ops: Vec<StateMutation>,
    removals: Vec<RemovalReceipt>,
}

impl PendingBatch {
    const fn len(&self) -> usize {
        self.ops.len()
    }

    const fn is_full(&self) -> bool {
        self.len() >= BATCH_SIZE
    }

    fn push_mutation(&mut self, mutation: StateMutation) {
        self.ops.push(mutation);
    }

    /// Records the logical state removal and, when a file was unlinked, the
    /// opaque receipt needed for later ancestor pruning.
    fn push_remove(&mut self, path: GatPath, resolved: Option<RemovalReceipt>) {
        if let Some(resolved) = resolved {
            self.removals.push(resolved);
        }
        self.ops.push(StateMutation::remove_exact(path));
    }

    /// Persist and clear everything accumulated so far, in one
    /// transaction, then prune any directories left empty by this
    /// batch's removes. A no-op (no transaction opened, no pruning) when
    /// empty. Pruning runs *after* the state commit: a pruning failure
    /// can only ever leave a harmless empty directory behind, never make
    /// materialized state claim a removed file is still present.
    fn flush(&mut self, store: &mut StateStore, worktree: WorktreeClient<'_>) -> Result<()> {
        if self.ops.is_empty() {
            return Ok(());
        }
        store.apply_batch(&self.ops)?;
        self.ops.clear();
        if !self.removals.is_empty() {
            let removed = std::mem::take(&mut self.removals);
            worktree.prune(&removed)?;
        }
        Ok(())
    }
}

/// Combines an action failure (`primary`, always returned to the caller as
/// the main error) with whatever a subsequent best-effort
/// [`PendingBatch::flush`] attempt produced. A flush failure here can only
/// ever mean an already-applied state mutation didn't get durably
/// committed, or a now-empty directory didn't get pruned -- strictly
/// secondary to the action failure that triggered the flush in the first
/// place, so it must never *replace* `primary`; it's folded in as extra
/// context instead, preserving the primary-error-plus-secondary-failure
/// wording used by command rollback reporting.
fn flush_after_failure(primary: SyncError, flush_result: Result<()>) -> SyncError {
    match flush_result {
        Ok(()) => primary,
        Err(flush_err) => SyncError::flush_after_failure(primary, flush_err),
    }
}

fn do_materialize(
    worktree: WorktreeClient<'_>,
    cache: &CacheClient,
    mode: &gat_core::config::MaterializationStrategy,
    entry: &Entry,
) -> Result<StateMutation> {
    Ok(worktree.materialize(cache, entry, mode, MaterializeKind::Create)?)
}

fn do_replace(
    worktree: WorktreeClient<'_>,
    cache: &CacheClient,
    mode: &gat_core::config::MaterializationStrategy,
    entry: &Entry,
) -> Result<StateMutation> {
    Ok(worktree.materialize(cache, entry, mode, MaterializeKind::Replace)?)
}

/// Recreates an already-correct working-tree file using the current
/// [`gat_core::config::MaterializationStrategy`] (`gat sync
/// --rematerialize`). Unlike [`do_replace`], the destination here starts
/// out *valid* -- its content already matches the desired OID -- so this
/// must never remove or truncate it before the new representation is
/// known to exist: the new representation is built inside a freshly,
/// atomically created, unique temp *directory* alongside `dest` (via the
/// ordinary [`storage::materialize`] fallback chain, so `--rematerialize`
/// gets exactly the same reflink/hardlink/symlink/copy fallback behavior
/// an ordinary materialize does -- a fresh directory always has room for
/// a not-yet-existing file, which hardlink/symlink require), and only
/// that already-built file is swapped into place, with one atomic
/// same-filesystem rename. The temp directory is owned by a `TempDir`
/// guard for its entire lifetime, so it (and, on any early return, its
/// still-unconsumed contents) is automatically removed even on failure --
/// this function itself never calls `remove_file`/`remove_dir` on a path
/// it did not itself just create. If materialization fails for every
/// configured mode, the guard's drop cleans up the temp directory and
/// `dest` is never touched, so a failure here can only ever leave the
/// prior, already-correct file exactly as it was.
fn do_rematerialize(
    worktree: WorktreeClient<'_>,
    cache: &CacheClient,
    mode: &gat_core::config::MaterializationStrategy,
    entry: &Entry,
) -> Result<StateMutation> {
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_do_rematerialize_call();
    Ok(worktree.materialize(cache, entry, mode, MaterializeKind::Rematerialize)?)
}

/// Removes the working-tree file at `path`. A successful unlink returns an
/// opaque pruning receipt; an already absent file returns `None`.
fn do_remove(worktree: WorktreeClient<'_>, path: &GatPath) -> Result<Option<RemovalReceipt>> {
    Ok(worktree.remove(path)?)
}

/// Resolve force-overridden conflicts without changing the decision or touching I/O.
fn resolve_action(action: SyncAction, force: bool) -> SyncAction {
    match action {
        SyncAction::Conflict(resolution) if force => resolution.into_action(),
        other => other,
    }
}

/// Account for a resolved decision after its mutation succeeds, or immediately
/// for preview. Unresolved paths are retained; mutations only increment counters.
fn record_action(outcome: &mut SyncOutcome, action: &SyncAction) {
    match action {
        SyncAction::Materialize(_) => outcome.materialized += 1,
        SyncAction::Replace(_) => outcome.replaced += 1,
        SyncAction::Rematerialize(_) => outcome.rematerialized += 1,
        SyncAction::Remove(_) => outcome.removed += 1,
        SyncAction::MissingObject { path, oid } => outcome.missing.push((path.clone(), *oid)),
        SyncAction::Corrupted { path, oid } => outcome.corrupted.push((path.clone(), *oid)),
        SyncAction::Conflict(resolution) => outcome.conflicts.push(resolution.path().clone()),
    }
}

/// Preview sink with no worktree or persistence capabilities. Actions are tallied
/// as they stream past, retaining only the unresolved paths needed by the report.
pub(crate) struct DryRunPlanSink {
    force: bool,
    outcome: SyncOutcome,
}

impl DryRunPlanSink {
    pub(crate) fn new(force: bool) -> Self {
        Self {
            force,
            outcome: SyncOutcome {
                dry_run: true,
                ..Default::default()
            },
        }
    }

    /// The fully tallied dry-run [`SyncOutcome`], once every action/stat
    /// refresh the merge produced has been delivered via
    /// [`super::PlanSink`]. Excludes are reconciled separately by the
    /// caller (dry-run reporting needs the full desired [`Lock`], which
    /// this sink -- scoped to one bounded merge -- never holds).
    pub(crate) fn finish(self) -> SyncOutcome {
        self.outcome
    }
}

impl super::PlanSink for DryRunPlanSink {
    fn action(&mut self, action: SyncAction) -> Result<()> {
        record_action(&mut self.outcome, &resolve_action(action, self.force));
        Ok(())
    }

    fn state_mutation(&mut self, _mutation: StateMutation) -> Result<()> {
        // Preview does not persist validated stat proofs.
        Ok(())
    }
}

/// Execution boundary for both streamed decisions and collected plans. Successful
/// mutations and stat refreshes share one ordered, bounded persistence batch.
/// Drive the producer through [`Self::run`] so every exit flushes pending state.
pub(crate) struct ExecutePlanSink<'a> {
    worktree: WorktreeClient<'a>,
    cache: &'a CacheClient,
    mode: &'a gat_core::config::MaterializationStrategy,
    force: bool,
    store: &'a mut StateStore,
    pending: PendingBatch,
    flush_failed: bool,
    outcome: SyncOutcome,
}

impl<'a> ExecutePlanSink<'a> {
    pub(crate) fn new(
        repo: &'a Repo,
        cache: &'a CacheClient,
        mode: &'a gat_core::config::MaterializationStrategy,
        store: &'a mut StateStore,
        force: bool,
    ) -> Self {
        Self {
            worktree: repo.worktree_client(),
            cache,
            mode,
            force,
            store,
            pending: PendingBatch::default(),
            flush_failed: false,
            outcome: SyncOutcome::default(),
        }
    }

    /// Own the producer's completion boundary, including planner failures that
    /// occur between actions. An already-failed full-batch flush is never retried.
    pub(crate) fn run(
        mut self,
        produce: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<SyncOutcome> {
        let result = produce(&mut self);
        self.finish(result)
    }

    fn finish(mut self, result: Result<()>) -> Result<SyncOutcome> {
        match result {
            Ok(()) => {
                self.pending.flush(self.store, self.worktree)?;
                Ok(self.outcome)
            }
            Err(primary) if self.flush_failed => Err(primary),
            Err(primary) => Err(flush_after_failure(
                primary,
                self.pending.flush(self.store, self.worktree),
            )),
        }
    }
}

impl ExecutePlanSink<'_> {
    fn apply_action(&mut self, action: SyncAction) -> Result<()> {
        let action = resolve_action(action, self.force);
        match &action {
            SyncAction::Materialize(entry) => {
                let mutation = do_materialize(self.worktree, self.cache, self.mode, entry)?;
                self.pending.push_mutation(mutation);
            }
            SyncAction::Replace(entry) => {
                let mutation = do_replace(self.worktree, self.cache, self.mode, entry)?;
                self.pending.push_mutation(mutation);
            }
            SyncAction::Rematerialize(entry) => {
                let mutation = do_rematerialize(self.worktree, self.cache, self.mode, entry)?;
                self.pending.push_mutation(mutation);
            }
            SyncAction::Remove(path) => {
                let receipt = do_remove(self.worktree, path)?;
                self.pending.push_remove(path.clone(), receipt);
            }
            SyncAction::MissingObject { .. }
            | SyncAction::Corrupted { .. }
            | SyncAction::Conflict(_) => {}
        }
        record_action(&mut self.outcome, &action);
        Ok(())
    }

    fn flush_if_full(&mut self) -> Result<()> {
        if self.pending.is_full() {
            let result = self.pending.flush(self.store, self.worktree);
            self.flush_failed = result.is_err();
            return result;
        }
        Ok(())
    }
}

impl super::PlanSink for ExecutePlanSink<'_> {
    fn action(&mut self, action: SyncAction) -> Result<()> {
        self.apply_action(action)?;
        self.flush_if_full()
    }

    fn state_mutation(&mut self, mutation: StateMutation) -> Result<()> {
        self.pending.push_mutation(mutation);
        self.flush_if_full()
    }
}

/// Feed a collected plan through the same execution boundary as streaming plans.
/// Actions precede validated stat refreshes, preserving the collected plan's
/// mutation order. Excludes are reconciled separately by the caller.
pub(crate) fn apply_actions(
    repo: &Repo,
    cache: &CacheClient,
    mode: &gat_core::config::MaterializationStrategy,
    store: &mut StateStore,
    plan: SyncPlan,
    force: bool,
) -> Result<SyncOutcome> {
    ExecutePlanSink::new(repo, cache, mode, store, force).run(|sink| {
        for action in plan.actions {
            sink.action(action)?;
        }
        for mutation in plan.validated_state_mutations {
            sink.state_mutation(mutation)?;
        }
        Ok(())
    })
}

/// Test-only instrumentation: counts actual [`do_rematerialize`]
/// executions, independent of [`SyncOutcome::rematerialized`] (which only
/// ever reflects the single sync pass that produced it). A
/// `--repair --rematerialize` run performs
/// exactly `N` real rematerializations, not `2N`, even though repair
/// orchestration may run
/// `sync_from_snapshot` more than once.
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support {
    use std::cell::Cell;

    thread_local! {
        static DO_REMATERIALIZE_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn record_do_rematerialize_call() {
        DO_REMATERIALIZE_CALLS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn do_rematerialize_calls() -> usize {
        DO_REMATERIALIZE_CALLS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_mutation::record_materialized_for_test as record_materialized;
    use crate::test_harness::git_repo;
    use crate::workspace::sync::error::FlushFailureComposite;
    use crate::workspace::sync::{
        StateFailureKind, SyncErrorKind, SyncOptions, Validation, WorktreePathFailureKind, sync,
    };
    use gat_core::lock::Lock;
    use gat_io::StateStore;

    /// Decision precedence must agree between preview and streaming execution.
    /// Removal never needs cache bytes, even if its former object is unusable.
    #[test]
    fn conflict_precedence_across_intents_cache_states_and_force() {
        for intent in ["replace", "rematerialize", "remove"] {
            let tmp = git_repo();
            let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            set_strategy(&repo, "copy");
            let cache = repo.resolved_cache_root().unwrap();
            let mut cases = Vec::new();
            let mut prior_entries = Vec::new();
            let mut desired = Lock::default();
            for cache_state in ["corrupt", "missing", "valid"] {
                let path = format!("{cache_state}.bin");
                // Distinct contents keep cache mutations local to each case.
                let prior_bytes = format!("prior-{cache_state}");
                let prior = Entry {
                    path: GatPath::parse_canonical(&path).unwrap(),
                    oid: ingest(&repo, prior_bytes.as_bytes()).oid,
                };
                std::fs::write(tmp.path().join(&path), prior_bytes.as_bytes()).unwrap();
                prior_entries.push(prior.clone());
                let expected_bytes = if intent == "replace" {
                    format!("desired-{cache_state}")
                } else {
                    prior_bytes
                };
                let entry = if intent == "replace" {
                    Entry {
                        path: prior.path,
                        oid: ingest(&repo, expected_bytes.as_bytes()).oid,
                    }
                } else {
                    prior
                };
                if intent != "remove" {
                    desired.upsert(entry.path.clone(), entry.oid);
                }
                let object = cache.object_path_for_test(&entry.oid);
                match cache_state {
                    "missing" => std::fs::remove_file(object).unwrap(),
                    "corrupt" => {
                        cache.make_object_writable_for_test(&entry.oid).unwrap();
                        std::fs::write(object, b"broken").unwrap();
                    }
                    _ => {}
                }
                cases.push((cache_state, entry, expected_bytes));
            }
            record_materialized(&repo, &prior_entries).unwrap();
            repo.save_lock(&desired).unwrap();
            for (_, entry, _) in &cases {
                std::fs::write(tmp.path().join(entry.path.as_str()), b"local").unwrap();
            }
            // Non-forced execution and preview preserve all files, allowing the
            // same three independent cases to feed each subsequent sync call.
            for force in [false, true] {
                for dry_run in [true, false] {
                    let outcome = sync(
                        &repo,
                        &SyncOptions {
                            policy: crate::ReconciliationPolicy::Validate {
                                rematerialize: intent == "rematerialize",
                            },
                            force,
                            dry_run,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                    let context = format!("{intent}/force={force}/dry_run={dry_run}");
                    let mut conflicts = Vec::new();
                    let mut missing = Vec::new();
                    let mut corrupted = Vec::new();
                    let mut changes = 0;
                    for (cache_state, entry, expected_bytes) in &cases {
                        let changed = match (*cache_state, intent, force) {
                            ("corrupt", "replace" | "rematerialize", _) => {
                                corrupted.push((entry.path.clone(), entry.oid));
                                false
                            }
                            (_, _, false) => {
                                conflicts.push(entry.path.clone());
                                false
                            }
                            ("missing", "replace" | "rematerialize", true) => {
                                missing.push((entry.path.clone(), entry.oid));
                                false
                            }
                            _ => true,
                        };
                        changes += usize::from(changed);
                        let path = tmp.path().join(entry.path.as_str());
                        if changed && !dry_run && intent == "remove" {
                            assert!(!path.exists(), "{context}/{cache_state}");
                        } else {
                            let expected = if changed && !dry_run {
                                expected_bytes.as_bytes()
                            } else {
                                b"local"
                            };
                            assert_eq!(
                                std::fs::read(path).unwrap(),
                                expected,
                                "{context}/{cache_state}"
                            );
                        }
                    }
                    assert_eq!(outcome.conflicts, conflicts, "{context}");
                    assert_eq!(outcome.missing, missing, "{context}");
                    assert_eq!(outcome.corrupted, corrupted, "{context}");
                    assert_eq!(outcome.materialized, 0, "{context}");
                    assert_eq!(
                        outcome.replaced,
                        if intent == "replace" { changes } else { 0 },
                        "{context}"
                    );
                    assert_eq!(
                        outcome.rematerialized,
                        if intent == "rematerialize" {
                            changes
                        } else {
                            0
                        },
                        "{context}"
                    );
                    assert_eq!(
                        outcome.removed,
                        if intent == "remove" { changes } else { 0 },
                        "{context}"
                    );
                }
            }
        }
    }

    #[test]
    fn preview_and_execution_account_for_every_action_with_and_without_force() {
        for force in [false, true] {
            let tmp = git_repo();
            let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            set_strategy(&repo, "copy");
            track(&repo, "replace.bin", b"old");
            let rematerialize = track(&repo, "rematerialize.bin", b"unchanged");
            let remove = track(&repo, "remove.bin", b"remove");
            track(&repo, "conflict.bin", b"local");
            sync(&repo, &SyncOptions::default()).unwrap();
            let create = track(&repo, "create.bin", b"created");
            let replace = track(&repo, "replace.bin", b"new");
            let conflict = track(&repo, "conflict.bin", b"desired");
            let actions = vec![
                SyncAction::Materialize(create),
                SyncAction::Replace(replace),
                SyncAction::Rematerialize(rematerialize),
                SyncAction::Remove(remove.path.clone()),
                SyncAction::MissingObject {
                    path: GatPath::parse_canonical("missing.bin").unwrap(),
                    oid: remove.oid,
                },
                SyncAction::Corrupted {
                    path: GatPath::parse_canonical("corrupted.bin").unwrap(),
                    oid: remove.oid,
                },
                SyncAction::Conflict(super::super::ConflictResolution::Replace(conflict)),
            ];
            let mut preview = DryRunPlanSink::new(force);
            for action in &actions {
                preview.action(action.clone()).unwrap();
            }
            preview
                .state_mutation(StateMutation::remove_exact(remove.path.clone()))
                .unwrap();
            let preview = preview.finish();
            assert!(preview.dry_run);
            assert!(!tmp.path().join("create.bin").exists());
            assert_eq!(
                std::fs::read(tmp.path().join("replace.bin")).unwrap(),
                b"old"
            );
            assert!(tmp.path().join("remove.bin").exists());
            assert!(materialized_row(&repo, "remove.bin").is_some());

            let mut store = StateStore::open(repo.layout()).unwrap();
            let executed = apply_actions(
                &repo,
                &repo.resolved_cache_root().unwrap().open_client(),
                &"copy".parse().unwrap(),
                &mut store,
                SyncPlan {
                    actions,
                    validated_state_mutations: Vec::new(),
                },
                force,
            )
            .unwrap();
            assert!(!executed.dry_run);
            assert_eq!(preview.materialized, executed.materialized);
            assert_eq!(preview.replaced, executed.replaced);
            assert_eq!(preview.rematerialized, executed.rematerialized);
            assert_eq!(preview.removed, executed.removed);
            assert_eq!(preview.conflicts, executed.conflicts);
            assert_eq!(preview.missing, executed.missing);
            assert_eq!(preview.corrupted, executed.corrupted);
            assert_eq!(executed.materialized, 1);
            assert_eq!(executed.replaced, 1 + usize::from(force));
            assert_eq!(executed.rematerialized, 1);
            assert_eq!(executed.removed, 1);
            assert_eq!(executed.conflicts.len(), usize::from(!force));
            assert_eq!(executed.missing.len(), 1);
            assert_eq!(executed.corrupted.len(), 1);
            assert_eq!(
                std::fs::read(tmp.path().join("conflict.bin")).unwrap(),
                if force {
                    b"desired".as_slice()
                } else {
                    b"local".as_slice()
                }
            );
            assert!(materialized_row(&repo, "remove.bin").is_none());
        }
    }

    #[test]
    fn actions_and_state_refreshes_share_the_batch_limit_and_finish_flushes_the_tail() {
        for last_is_action in [false, true] {
            let tmp = git_repo();
            let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            let first = track(&repo, "first.bin", b"first");
            let tail = track(&repo, "tail.bin", b"tail");
            let cache = repo.resolved_cache_root().unwrap().open_client();
            let mode = "copy".parse().unwrap();
            let mut store = StateStore::open(repo.layout()).unwrap();
            let mut sink = ExecutePlanSink::new(&repo, &cache, &mode, &mut store, false);
            // No-op state removals fill the batch without extra filesystem work.
            let refresh =
                StateMutation::remove_exact(GatPath::parse_canonical("absent.bin").unwrap());
            for _ in 0..BATCH_SIZE - 2 {
                sink.state_mutation(refresh.clone()).unwrap();
            }
            assert_eq!(sink.pending.len(), BATCH_SIZE - 2);
            if last_is_action {
                sink.state_mutation(refresh).unwrap();
                sink.action(SyncAction::Materialize(first)).unwrap();
            } else {
                sink.action(SyncAction::Materialize(first)).unwrap();
                assert!(materialized_row(&repo, "first.bin").is_none());
                sink.state_mutation(refresh).unwrap();
            }
            assert_eq!(sink.pending.len(), 0);
            assert!(materialized_row(&repo, "first.bin").is_some());
            sink.action(SyncAction::Materialize(tail)).unwrap();
            assert_eq!(sink.pending.len(), 1);
            assert!(materialized_row(&repo, "tail.bin").is_none());
            assert_eq!(sink.finish(Ok(())).unwrap().materialized, 2);
            assert!(materialized_row(&repo, "tail.bin").is_some());
        }
    }

    fn ingest(repo: &Repo, content: impl std::io::Read) -> gat_io::Ingested {
        repo.resolved_cache_root()
            .unwrap()
            .writer()
            .ingest(content)
            .unwrap()
            .0
    }

    fn track(repo: &Repo, path: &str, content: &[u8]) -> Entry {
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(repo, content);
        let oid = ingested.oid;
        lock.upsert(GatPath::parse_canonical(path).unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();
        Entry {
            path: gat_core::lexical_path::GatPath::parse_canonical(path).unwrap(),
            oid,
        }
    }

    fn materialized_row(repo: &Repo, path: &str) -> Option<gat_io::MaterializedRow> {
        StateStore::open(repo.layout())
            .unwrap()
            .load_all_raw()
            .unwrap()
            .into_iter()
            .find(|row| row.path() == path)
    }

    fn materialized_has_proof(repo: &Repo, path: &str) -> bool {
        materialized_row(repo, path)
            .as_ref()
            .is_some_and(gat_io::state_materialized_row_has_proof_for_test)
    }

    fn clear_materialized_stat(root: &std::path::Path, path: &str) {
        gat_io::state_test_support::clear_materialized_proof(
            &root.join(".gat/state/state.sqlite3"),
            path,
        );
    }

    #[test]
    fn initial_sync_materializes_every_tracked_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.materialized, 1);
        assert!(outcome.is_clean());
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
    }

    #[test]
    fn lock_change_replaces_materialized_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        track(&repo, "a.bin", b"world");
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.replaced, 1);
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"world");
    }

    #[test]
    fn removed_lock_entry_deletes_the_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        repo.save_lock(&Lock::default()).unwrap();
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(!tmp.path().join("a.bin").exists());
    }

    #[test]
    fn default_validation_restores_missing_working_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::remove_file(tmp.path().join("a.bin")).unwrap();

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.materialized, 1);
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
    }

    #[test]
    fn trust_state_does_not_restore_missing_matching_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::remove_file(tmp.path().join("a.bin")).unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.did_nothing());
        assert!(!tmp.path().join("a.bin").exists());
    }

    #[test]
    fn trust_state_does_not_report_local_modification_when_lock_matches_materialized() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.did_nothing());
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"locally edited"
        );
    }

    #[test]
    #[cfg(unix)]
    fn validate_uses_a_safe_cached_stat_without_hashing() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                ..Default::default()
            },
        )
        .unwrap();

        let path = tmp.path().join("a.bin");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&path, perms).unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                ..Default::default()
            },
        )
        .unwrap();

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();

        assert!(outcome.did_nothing());
    }

    /// Quantitative counterpart to
    /// `validate_uses_a_safe_cached_stat_without_hashing`: a warm, clean
    /// default (`Validation::Validate`) sync must call
    /// [`storage::hash_file`] exactly zero times (the definition
    /// of done: "Default clean validated sync ... hashes zero
    /// managed-file bytes after warm-up"), not merely happen to succeed
    /// despite the file being made unreadable.
    #[test]
    fn clean_validated_sync_hashes_zero_files() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                ..Default::default()
            },
        )
        .unwrap();

        storage::with_exclusive_hash_file_call_count(|| {
            let outcome = sync(
                &repo,
                &SyncOptions {
                    policy: crate::ReconciliationPolicy::default(),
                    ..Default::default()
                },
            )
            .unwrap();

            assert!(outcome.did_nothing());
            assert_eq!(storage::hash_file_call_count(), 0);
        });
    }

    #[test]
    #[cfg(unix)]
    fn validate_detects_size_changes_without_hashing() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                ..Default::default()
            },
        )
        .unwrap();

        let path = tmp.path().join("a.bin");
        std::fs::write(&path, b"hello, longer").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&path, perms).unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                ..Default::default()
            },
        )
        .unwrap();

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();

        assert_eq!(outcome.conflicts, vec!["a.bin".to_string()]);
    }

    #[test]
    fn validate_detects_same_size_content_changes_via_hash_fallback() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                ..Default::default()
            },
        )
        .unwrap();

        std::fs::write(tmp.path().join("a.bin"), b"HELLO").unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.conflicts, vec!["a.bin".to_string()]);
    }

    #[test]
    #[cfg(unix)]
    fn metadata_only_drift_hashes_once_then_reuses_the_refreshed_stat() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        assert!(materialized_has_proof(&repo, "a.bin"));

        let path = tmp.path().join("a.bin");
        std::fs::write(&path, b"hello").unwrap();

        storage::with_exclusive_hash_file_call_count(|| {
            let first = sync(
                &repo,
                &SyncOptions {
                    policy: crate::ReconciliationPolicy::default(),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(first.did_nothing());
            // Same-size content is ambiguous from stat alone -- exactly one
            // verifying hash is expected to prove it's actually unchanged
            // and refresh the stamp.
            assert_eq!(storage::hash_file_call_count(), 1);
        });

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&path, perms).unwrap();

        storage::with_exclusive_hash_file_call_count(|| {
            let second = sync(
                &repo,
                &SyncOptions {
                    policy: crate::ReconciliationPolicy::default(),
                    ..Default::default()
                },
            )
            .unwrap();

            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&path, perms).unwrap();

            assert!(second.did_nothing());
            // The refreshed stamp from `first` must now be reusable
            // stat-only.
            assert_eq!(storage::hash_file_call_count(), 0);
        });
    }

    #[test]
    fn dry_run_validate_does_not_persist_stat_refreshes() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        clear_materialized_stat(tmp.path(), "a.bin");
        assert!(!materialized_has_proof(&repo, "a.bin"));

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.did_nothing());
        assert!(!materialized_has_proof(&repo, "a.bin"));
    }

    #[test]
    fn path_scoped_sync_still_regenerates_excludes_from_the_full_lock() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "data/a.bin", b"a");
        track(&repo, "other.bin", b"b");

        sync(
            &repo,
            &SyncOptions {
                selection: gat_core::selection::Selection::from_scope_patterns(
                    gat_core::path_scope::normalize_path_scope(std::path::Path::new("data"))
                        .unwrap(),
                    Vec::new(),
                    Vec::new(),
                ),
                ..Default::default()
            },
        )
        .unwrap();

        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("data/a.bin"));
        assert!(exclude.contains("other.bin"));
    }

    #[test]
    fn force_overwrites_a_local_modification() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();
        track(&repo, "a.bin", b"world");

        let outcome = sync(
            &repo,
            &SyncOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.conflicts.is_empty());
        assert_eq!(outcome.replaced, 1);
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"world");
    }

    #[test]
    fn dry_run_reports_without_touching_disk() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");

        let outcome = sync(
            &repo,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.materialized, 1);
        assert!(!tmp.path().join("a.bin").exists());
    }

    #[test]
    fn hardlink_mode_shares_the_cache_object() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("hardlink".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        let entry = track(&repo, "a.bin", b"hello");

        sync(&repo, &SyncOptions::default()).unwrap();

        let obj = repo
            .resolved_cache_root()
            .unwrap()
            .object_path_for_test(&entry.oid);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                std::fs::metadata(tmp.path().join("a.bin")).unwrap().ino(),
                std::fs::metadata(&obj).unwrap().ino()
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn symlink_mode_materializes_a_symlink() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("symlink".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");

        sync(&repo, &SyncOptions::default()).unwrap();

        assert!(
            std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // A validated re-sync must recognize the correct symlink
        // materialization as a match, not a conflict: `plan::file_status`
        // validates a symlink leaf by checking that it points at the
        // exact cache object for the desired oid, rather than either
        // rejecting a non-regular leaf outright or dereferencing and
        // hashing the target as though the symlink were the managed
        // file.
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();
        assert_eq!(outcome.materialized, 0);
        assert_eq!(outcome.replaced, 0);
        assert!(
            std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "a correct symlink materialization must not be touched by revalidation"
        );
    }

    #[test]
    fn copy_mode_produces_a_writable_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");

        sync(&repo, &SyncOptions::default()).unwrap();

        let meta = std::fs::metadata(tmp.path().join("a.bin")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(meta.permissions().mode() & 0o200, 0);
        }
    }

    #[test]
    fn paths_with_spaces_and_non_ascii_are_synced() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "dir with space/héllo 世界.bin", b"hi");

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.materialized, 1);
        assert!(tmp.path().join("dir with space/héllo 世界.bin").exists());
    }

    #[test]
    fn force_does_not_fix_a_corrupted_cache_object() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        let world = track(&repo, "a.bin", b"world");
        let cache_root = repo.resolved_cache_root().unwrap();
        let obj = cache_root.object_path_for_test(&world.oid);
        cache_root
            .make_object_writable_for_test(&world.oid)
            .unwrap();
        std::fs::write(&obj, b"XXXXX").unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::default(),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            outcome.corrupted,
            vec![(
                gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                world.oid,
            )]
        );
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"locally edited"
        );
    }

    /// Confinement must also fire at the *deletion* boundary: a Remove action
    /// with a traversal path must fail before any filesystem call is made.
    ///
    /// In practice `Lock::parse` rejects traversal paths at load time, which
    /// is the first line of defence. This test bypasses that by inserting a
    /// traversal path directly into materialized state, so
    /// `confine_to_worktree` in `do_remove` is the only thing standing
    /// between this path and a real filesystem write, verifying the
    /// end-to-end invariant: any traversal path that might appear there
    /// causes `sync` to fail with an error that mentions path traversal, and
    /// that the path outside the worktree is never actually touched.
    #[test]
    fn confine_rejects_traversal_for_remove() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        // Insert a traversal path directly into materialized state
        // (bypassing the strict parser that `Lock::parse` would normally
        // apply, and the `GatPath` construction boundary that
        // `StateStore::upsert_many` would now enforce). Opening the
        // store first ensures the on-disk database/schema already exists,
        // matching every real caller's sequencing.
        drop(gat_io::StateStore::open(repo.layout()).unwrap());
        gat_io::state_test_support::insert_raw_materialized_row(
            &tmp.path().join(".gat/state/state.sqlite3"),
            "../evil",
            &[0xaau8; 32],
        );

        // The desired lock is empty, so sync wants to Remove the
        // materialized entry -- confine_to_worktree must reject it before
        // any filesystem call is made.
        let lock = gat_core::lock::Lock::default();
        repo.save_lock(&lock).unwrap();

        // Create the would-be victim outside the repo to prove it's never
        // touched even though materialized state says it exists. Its size
        // matches the fake entry's stated size (5) so plan() sees a status
        // of `Matches` -- an unconditional Remove action, not a Conflict.
        let victim = tmp.path().parent().unwrap().join("evil");
        std::fs::write(&victim, b"XXXXX").unwrap();

        let result = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        );
        assert!(
            result.is_err(),
            "sync with a traversal path in the materialized state must fail"
        );
        let err = result.unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::State(StateFailureKind::Corrupt),
            "invalid persisted traversal must fail closed as corrupt state"
        );
        // The file outside the worktree must be untouched.
        assert!(
            victim.exists(),
            "file outside the worktree must not be removed"
        );
    }

    #[test]
    #[cfg(unix)]
    fn materialize_rejects_symlinked_parent_path() {
        use std::os::unix::fs::symlink;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "link/out.bin", b"hello");

        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();

        let err = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::WorktreePath {
                kind: WorktreePathFailureKind::OutsideRepository,
                path: Some("link/out.bin".to_string()),
            },
            "expected symlink-ancestor rejection"
        );
        assert!(
            !outside.path().join("out.bin").exists(),
            "sync must not write outside the repository via symlinked parent"
        );
    }

    #[test]
    fn producer_cancellation_flushes_successful_actions_before_returning() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let entry = track(&repo, "first.bin", b"first");
        let cache = repo.resolved_cache_root().unwrap().open_client();
        let mut store = StateStore::open(repo.layout()).unwrap();
        let error =
            ExecutePlanSink::new(&repo, &cache, &"copy".parse().unwrap(), &mut store, false)
                .run(|sink| {
                    sink.action(SyncAction::Materialize(entry))?;
                    Err(crate::RepositoryError::Cancelled.into())
                })
                .unwrap_err();
        assert_eq!(
            error.kind(),
            &SyncErrorKind::MutationAuthority(
                super::super::MutationAuthorityFailureKind::Cancelled
            )
        );
        let materialized = crate::repository_mutation::load_materialized_for_test(&repo).unwrap();
        assert!(
            materialized
                .entries
                .iter()
                .any(|entry| entry.path == "first.bin")
        );
        assert_eq!(
            std::fs::read(tmp.path().join("first.bin")).unwrap(),
            b"first"
        );
    }

    /// `apply_actions` batches materialized-state persistence rather than
    /// committing after every single action (see `PendingBatch`), but a
    /// mid-plan failure must still flush whatever already succeeded
    /// before returning the error -- otherwise a retry after the failure
    /// would not see an accurate enough materialized-state view for the
    /// paths that were, in fact, already written to disk.
    #[test]
    #[cfg(unix)]
    fn a_failed_action_still_flushes_already_successful_mutations_in_its_pending_batch() {
        use std::os::unix::fs::symlink;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let ok_entry = track(&repo, "ok.bin", b"hello");

        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();
        let bad_entry = Entry {
            path: gat_core::lexical_path::GatPath::parse_canonical("link/out.bin").unwrap(),
            oid: ok_entry.oid,
        };

        let plan = SyncPlan {
            actions: vec![
                SyncAction::Materialize(ok_entry),
                SyncAction::Materialize(bad_entry),
            ],
            validated_state_mutations: Vec::new(),
        };
        let mut store = StateStore::open(repo.layout()).unwrap();
        let err = apply_actions(
            &repo,
            &repo.resolved_cache_root().unwrap().open_client(),
            &Default::default(),
            &mut store,
            plan,
            false,
        )
        .unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::WorktreePath {
                kind: WorktreePathFailureKind::OutsideRepository,
                path: Some("link/out.bin".to_string()),
            },
            "expected symlink-ancestor rejection"
        );
        drop(store);

        let materialized = crate::repository_mutation::load_materialized_for_test(&repo).unwrap();
        assert!(
            materialized.entries.iter().any(|e| e.path == "ok.bin"),
            "the successful action's state must be flushed even though the plan failed overall"
        );
    }

    /// A pruning failure surfaced by the post-error flush must never
    /// *replace* the original action error: it's strictly secondary (an
    /// already-applied removal's state is still durably committed either
    /// way; only the best-effort empty-directory cleanup didn't happen).
    #[cfg(unix)]
    #[test]
    fn a_pruning_failure_during_the_error_path_flush_does_not_mask_the_original_action_error() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::fs::symlink;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let leaf_entry = track(&repo, "a/leaf.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();
        let bad_entry = Entry {
            path: gat_core::lexical_path::GatPath::parse_canonical("link/out.bin").unwrap(),
            oid: leaf_entry.oid,
        };

        // Deny write on the repo root so `remove_dir("a")` -- attempted by
        // the post-error flush's pruning pass once "a/leaf.bin" is gone --
        // fails with a real (non-benign) permission error instead of just
        // finding "a" already empty.
        let mut perms = std::fs::metadata(tmp.path()).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(tmp.path(), perms).unwrap();

        let plan = SyncPlan {
            actions: vec![
                SyncAction::Remove(
                    gat_core::lexical_path::GatPath::parse_canonical("a/leaf.bin").unwrap(),
                ),
                SyncAction::Materialize(bad_entry),
            ],
            validated_state_mutations: Vec::new(),
        };
        let mut store = StateStore::open(repo.layout()).unwrap();
        let err = apply_actions(
            &repo,
            &repo.resolved_cache_root().unwrap().open_client(),
            &Default::default(),
            &mut store,
            plan,
            false,
        )
        .unwrap_err();
        drop(store);

        let mut perms = std::fs::metadata(tmp.path()).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(tmp.path(), perms).unwrap();

        let composite = std::error::Error::source(&err)
            .and_then(|source| source.downcast_ref::<FlushFailureComposite>())
            .expect("the flush composite should retain both failures");
        assert_eq!(
            composite.primary.kind(),
            &SyncErrorKind::WorktreePath {
                kind: WorktreePathFailureKind::OutsideRepository,
                path: Some("link/out.bin".to_string()),
            },
            "the original action failure must remain identifiable"
        );
    }

    /// Regression test for the executor batching correctness bug: `a`
    /// (an old materialized file) transitioning to `a/b` (a new desired
    /// path nested beneath `a`'s location) executes as `Remove("a")` then
    /// `Materialize("a/b")` on the filesystem. Both mutations succeed and
    /// land in the *same* pending batch/transaction (well under
    /// `BATCH_SIZE`), so [`StateStore::apply_batch`] must persist
    /// them in that same order -- upserting `a/b` and only then removing
    /// `a`'s now-stale prefix would incorrectly wipe out `a/b`'s just-
    /// written materialized row too, since `a/b` is nested under `a`.
    #[test]
    fn file_to_directory_transition_in_the_same_batch_persists_the_new_nested_path() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let a_entry = track(&repo, "a", b"old");
        let mut store = StateStore::open(repo.layout()).unwrap();
        let setup_plan = SyncPlan {
            actions: vec![SyncAction::Materialize(a_entry)],
            validated_state_mutations: Vec::new(),
        };
        apply_actions(
            &repo,
            &repo.resolved_cache_root().unwrap().open_client(),
            &Default::default(),
            &mut store,
            setup_plan,
            false,
        )
        .unwrap();
        drop(store);

        let ab_entry = track(&repo, "a/b", b"new");
        let plan = SyncPlan {
            actions: vec![
                SyncAction::Remove(gat_core::lexical_path::GatPath::parse_canonical("a").unwrap()),
                SyncAction::Materialize(ab_entry),
            ],
            validated_state_mutations: Vec::new(),
        };
        let mut store = StateStore::open(repo.layout()).unwrap();
        apply_actions(
            &repo,
            &repo.resolved_cache_root().unwrap().open_client(),
            &Default::default(),
            &mut store,
            plan,
            false,
        )
        .unwrap();
        drop(store);

        assert_eq!(std::fs::read(tmp.path().join("a/b")).unwrap(), b"new");
        let materialized = crate::repository_mutation::load_materialized_for_test(&repo).unwrap();
        assert!(
            materialized.entries.iter().any(|e| e.path == "a/b"),
            "a/b's materialized state must survive being queued in the same batch as a \
             prefix-removal of its former sibling file `a`"
        );
        assert!(
            !materialized.entries.iter().any(|e| e.path == "a"),
            "the old file `a` must no longer be materialized"
        );
    }

    /// The reverse transition: `a/b` (an old materialized nested file)
    /// gives way to `a` (a new desired path at its
    /// parent directory). The removal of the whole `a/` prefix must be
    /// applied before the new `a` file's upsert within the same batch,
    /// or the upsert could be clobbered by a removal ordered after it.
    #[test]
    fn directory_to_file_transition_in_the_same_batch_persists_the_new_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let ab_entry = track(&repo, "a/b", b"old");
        let mut store = StateStore::open(repo.layout()).unwrap();
        let setup_plan = SyncPlan {
            actions: vec![SyncAction::Materialize(ab_entry)],
            validated_state_mutations: Vec::new(),
        };
        apply_actions(
            &repo,
            &repo.resolved_cache_root().unwrap().open_client(),
            &Default::default(),
            &mut store,
            setup_plan,
            false,
        )
        .unwrap();
        drop(store);

        let a_entry = track(&repo, "a", b"new");
        let plan = SyncPlan {
            actions: vec![
                SyncAction::Remove(
                    gat_core::lexical_path::GatPath::parse_canonical("a/b").unwrap(),
                ),
                SyncAction::Materialize(a_entry),
            ],
            validated_state_mutations: Vec::new(),
        };
        let mut store = StateStore::open(repo.layout()).unwrap();
        apply_actions(
            &repo,
            &repo.resolved_cache_root().unwrap().open_client(),
            &Default::default(),
            &mut store,
            plan,
            false,
        )
        .unwrap();
        drop(store);

        assert_eq!(std::fs::read(tmp.path().join("a")).unwrap(), b"new");
        let materialized = crate::repository_mutation::load_materialized_for_test(&repo).unwrap();
        assert!(
            materialized.entries.iter().any(|e| e.path == "a"),
            "the new file `a` must be materialized"
        );
        assert!(
            !materialized.entries.iter().any(|e| e.path == "a/b"),
            "the old nested file `a/b` must no longer be materialized"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_rejects_symlinked_parent_path() {
        use std::os::unix::fs::symlink;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("victim.bin");
        std::fs::write(&outside_file, b"outside").unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();

        record_materialized(
            &repo,
            &[Entry {
                path: gat_core::lexical_path::GatPath::parse_canonical("link/victim.bin").unwrap(),
                oid: gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
            }],
        )
        .unwrap();
        repo.save_lock(&Lock::default()).unwrap();

        let err = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::WorktreePath {
                kind: WorktreePathFailureKind::OutsideRepository,
                path: Some("link/victim.bin".to_string()),
            },
            "expected symlink-ancestor rejection"
        );
        assert!(
            outside_file.exists(),
            "sync must not remove outside files via symlinked parent"
        );
    }

    #[test]
    #[cfg(unix)]
    fn dangling_symlink_leaf_is_replaced_with_force() {
        use std::os::unix::fs::symlink;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        // The target deliberately never exists (that's the "dangling"
        // under test), but it's still rooted under this test's own
        // owned `tmp`, not a shared `/tmp` path another process could
        // race to create.
        symlink(tmp.path().join("missing-target"), tmp.path().join("a.bin")).unwrap();

        // A symlink leaf is never followed and hashed as though its
        // target were the managed file: it is reported as a conflict and
        // may only be replaced through the explicit `--force` policy.
        let outcome = sync(
            &repo,
            &SyncOptions {
                force: true,
                ..SyncOptions::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.replaced, 1);
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
        assert!(
            !std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_leaf_is_replaced_safely_on_replace() {
        use std::os::unix::fs::symlink;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.bin");
        std::fs::write(&outside_file, b"hello").unwrap();
        std::fs::remove_file(tmp.path().join("a.bin")).unwrap();
        symlink(&outside_file, tmp.path().join("a.bin")).unwrap();

        track(&repo, "a.bin", b"world");
        // A symlink leaf is never followed: it is reported as a conflict
        // and may only be replaced through the explicit `--force` policy.
        let outcome = sync(
            &repo,
            &SyncOptions {
                force: true,
                ..SyncOptions::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.replaced, 1);
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"world");
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"hello");
        assert!(
            !std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn pre_existing_destination_is_restored_if_materialization_fails() {
        // Verify the backup/restore contract of do_materialize: when every
        // configured link mode fails after a pre-existing file was renamed
        // aside (because existence was unknown at planning time), the original
        // file must be restored so no user data is silently destroyed.
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        // Use hardlink-only so there is exactly one mode to fail.
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("hardlink".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(&repo, &b"desired content"[..]);
        lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"pre-existing content").unwrap();

        // Plan while the object is still in the cache: the plan emits Materialize.
        let plan = crate::workspace::sync::plan::plan(
            &repo,
            &gat_core::selection::Selection::root(),
            Validation::TrustState,
        )
        .unwrap();
        assert_eq!(plan.actions.len(), 1);

        // Remove the cached object so every materialize mode fails at execute time.
        let obj_path = repo
            .resolved_cache_root()
            .unwrap()
            .object_path_for_test(&ingested.oid);
        std::fs::remove_file(&obj_path).unwrap();

        let mut store = gat_io::StateStore::open(repo.layout()).unwrap();
        let result = apply_actions(
            &repo,
            &repo.resolved_cache_root().unwrap().open_client(),
            &Default::default(),
            &mut store,
            plan,
            false,
        );
        assert!(
            result.is_err(),
            "expected execute to fail when cache object is missing"
        );

        // The pre-existing file must have been restored.
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"pre-existing content",
            "pre-existing file must be restored after failed materialization"
        );
    }

    /// Removes `path` from `gat.lock` without touching the working tree or
    /// materialized state -- the deleted-path counterpart to `track`.
    fn untrack(repo: &Repo, path: &str) {
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        lock.remove_prefix(&gat_core::lexical_path::GatPath::parse_canonical(path).unwrap());
        repo.save_lock(&lock).unwrap();
    }

    #[test]
    fn deep_chain_removal_prunes_all_empty_ancestors_under_validate() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/c/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();

        untrack(&repo, "a/b/c/file.bin");
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(!tmp.path().join("a").exists());
    }

    #[test]
    fn deep_chain_removal_prunes_all_empty_ancestors_under_trust_state() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/c/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();

        untrack(&repo, "a/b/c/file.bin");
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(!tmp.path().join("a").exists());
    }

    #[test]
    fn untracked_sibling_file_blocks_pruning_above_it() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/c/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a/b/keep.txt"), b"keep").unwrap();

        untrack(&repo, "a/b/c/file.bin");
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(!tmp.path().join("a/b/c").exists());
        assert!(tmp.path().join("a/b").exists());
        assert!(tmp.path().join("a/b/keep.txt").exists());
    }

    #[test]
    fn two_removes_sharing_a_parent_prune_it_once() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/one.bin", b"1");
        track(&repo, "a/b/two.bin", b"2");
        sync(&repo, &SyncOptions::default()).unwrap();

        untrack(&repo, "a/b/one.bin");
        untrack(&repo, "a/b/two.bin");
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.removed, 2);
        assert!(!tmp.path().join("a").exists());
    }

    #[test]
    fn sibling_branches_are_both_pruned() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/left/file.bin", b"l");
        track(&repo, "a/right/file.bin", b"r");
        sync(&repo, &SyncOptions::default()).unwrap();

        untrack(&repo, "a/left/file.bin");
        untrack(&repo, "a/right/file.bin");
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.removed, 2);
        assert!(!tmp.path().join("a").exists());
    }

    #[test]
    fn unrelated_empty_directory_is_never_touched_by_sync_pruning() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::create_dir_all(tmp.path().join("unrelated")).unwrap();

        untrack(&repo, "a/file.bin");
        sync(&repo, &SyncOptions::default()).unwrap();

        assert!(!tmp.path().join("a").exists());
        assert!(tmp.path().join("unrelated").exists());
    }

    #[test]
    fn repository_root_is_never_removed_when_the_last_root_file_is_deleted() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "root.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();

        untrack(&repo, "root.bin");
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(tmp.path().exists());
    }

    #[test]
    fn dry_run_removal_performs_no_pruning() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();

        untrack(&repo, "a/b/file.bin");
        let outcome = sync(
            &repo,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(outcome.dry_run);
        assert!(tmp.path().join("a/b/file.bin").exists());
        assert!(tmp.path().join("a/b").exists());
    }

    #[test]
    fn remove_conflict_without_force_leaves_file_and_directories_untouched() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a/b/file.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a/b/file.bin"), b"locally edited").unwrap();

        untrack(&repo, "a/b/file.bin");
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.conflicts, vec!["a/b/file.bin".to_string()]);
        assert!(tmp.path().join("a/b/file.bin").exists());
        assert!(tmp.path().join("a/b").exists());
    }

    #[test]
    fn remove_conflict_with_force_deletes_the_leaf_and_prunes_the_empty_chain() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a/b/file.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a/b/file.bin"), b"locally edited").unwrap();

        untrack(&repo, "a/b/file.bin");
        let outcome = sync(
            &repo,
            &SyncOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.conflicts.is_empty());
        assert_eq!(outcome.removed, 1);
        assert!(!tmp.path().join("a").exists());
    }

    #[test]
    fn absent_before_sync_path_is_never_handed_to_do_remove_or_pruned() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();
        // Simulate the path already being gone from both `gat.lock` and
        // materialized state *before* this sync run -- e.g. reconciled by
        // some earlier process -- while its now-empty directory is left
        // over on disk. `FileStatus::Absent` means sync must not touch it.
        std::fs::remove_file(tmp.path().join("a/b/file.bin")).unwrap();
        untrack(&repo, "a/b/file.bin");
        StateStore::open(repo.layout())
            .unwrap()
            .remove_exact(&[
                gat_core::lexical_path::GatPath::parse_canonical("a/b/file.bin").unwrap(),
            ])
            .unwrap();

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert!(outcome.did_nothing());
        assert!(tmp.path().join("a/b").exists());
    }

    /// Distinct from the case above: here the path *is* still materialized
    /// (a real, still-tracked-until-now row) and `Validation::TrustState`
    /// schedules its removal without probing the filesystem first (per
    /// `Validation::TrustState`'s no-stat dirty-row fast path), but the
    /// leaf has already been deleted externally. `do_remove` legitimately
    /// hits `NotFound`: the materialized row is still reconciled away, but
    /// since *this sync* didn't unlink anything, its pre-existing empty
    /// parent directory must not be pruned.
    #[test]
    fn trust_state_reconciles_a_materialized_row_whose_leaf_is_already_gone_without_pruning() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::remove_file(tmp.path().join("a/b/file.bin")).unwrap();

        untrack(&repo, "a/b/file.bin");
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(tmp.path().join("a/b").exists());
    }

    #[test]
    fn mixed_batch_only_prunes_ancestors_of_the_actually_removed_leaf() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/file.bin", b"a");
        track(&repo, "b/file.bin", b"b");
        sync(&repo, &SyncOptions::default()).unwrap();
        // "a/file.bin" is externally deleted before sync ever gets to
        // reconcile it (NotFound), "b/file.bin" is left alone for sync to
        // actually unlink.
        std::fs::remove_file(tmp.path().join("a/file.bin")).unwrap();

        untrack(&repo, "a/file.bin");
        untrack(&repo, "b/file.bin");
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 2);
        // Only "b" (actually emptied by this sync) is pruned; "a" was
        // already empty before this sync ran and is left untouched.
        assert!(!tmp.path().join("b").exists());
        assert!(tmp.path().join("a").exists());
    }

    #[test]
    fn clean_repeated_sync_performs_zero_remove_dir_attempts() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();

        gat_io::worktree_test_support::reset_remove_dir_attempts();
        sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(gat_io::worktree_test_support::remove_dir_attempts(), 0);
    }

    #[test]
    fn materialize_only_sync_performs_zero_remove_dir_attempts() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/file.bin", b"x");

        gat_io::worktree_test_support::reset_remove_dir_attempts();
        sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(gat_io::worktree_test_support::remove_dir_attempts(), 0);
    }

    #[test]
    fn replace_only_sync_performs_zero_remove_dir_attempts() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let entry = track(&repo, "a/b/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();
        track(&repo, entry.path.as_str(), b"replaced");

        gat_io::worktree_test_support::reset_remove_dir_attempts();
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.replaced, 1);
        assert_eq!(gat_io::worktree_test_support::remove_dir_attempts(), 0);
    }

    #[test]
    fn dry_run_with_planned_removal_performs_zero_remove_dir_attempts() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();
        untrack(&repo, "a/b/file.bin");

        gat_io::worktree_test_support::reset_remove_dir_attempts();
        let outcome = sync(
            &repo,
            &SyncOptions {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(gat_io::worktree_test_support::remove_dir_attempts(), 0);
        assert!(tmp.path().join("a/b/file.bin").exists());
    }

    #[test]
    fn clean_repeated_sync_prunes_nothing() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a/b/file.bin", b"x");
        sync(&repo, &SyncOptions::default()).unwrap();

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert!(outcome.did_nothing());
        assert!(tmp.path().join("a/b/file.bin").exists());
    }

    fn set_strategy(repo: &Repo, strategy: &str) {
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some(strategy.parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    }

    #[test]
    fn plain_sync_after_a_strategy_change_leaves_an_already_correct_file_alone() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        set_strategy(&repo, "symlink");
        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert!(
            outcome.did_nothing(),
            "changing cache.materialization_strategy alone must not rewrite an \
             already-correct file: {outcome:?}"
        );
        assert!(
            !std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the existing copy representation must not have been touched"
        );
    }

    #[test]
    fn rematerialize_recreates_an_already_correct_file_copy_to_symlink() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        let entry = track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        assert!(
            !std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        set_strategy(&repo, "symlink");
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 1);
        assert_eq!(outcome.replaced, 0);
        // See `sync_repair_and_rematerialize_repairs_the_object_then_rematerializes`
        // for why the actual-symlink assertion is Unix-only: elsewhere
        // `MaterializationMode::Symlink` falls back to a copy.
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "--rematerialize must recreate the file using the new strategy"
        );
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
        let _ = entry;
    }

    #[test]
    #[cfg(unix)]
    fn rematerialize_recreates_an_already_correct_file_copy_to_hardlink() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        let entry = track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        set_strategy(&repo, "hardlink");
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 1);
        use std::os::unix::fs::MetadataExt;
        let obj = repo
            .resolved_cache_root()
            .unwrap()
            .object_path_for_test(&entry.oid);
        let dest_ino = std::fs::metadata(tmp.path().join("a.bin")).unwrap().ino();
        let obj_ino = std::fs::metadata(&obj).unwrap().ino();
        assert_eq!(
            dest_ino, obj_ino,
            "hardlink rematerialization must share the cache object's inode"
        );
    }

    #[test]
    fn repeated_rematerialize_stays_clean_and_idempotent() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "symlink");
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        let first = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();
        let second = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(first.rematerialized, 1);
        assert_eq!(second.rematerialized, 1);
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
    }

    #[test]
    fn rematerialize_reports_a_conflict_instead_of_overwriting_a_local_edit() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "a.bin", b"hello");
        sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        // Rematerialization uses a validating policy even when the ledger was
        // originally populated by a trust-state run.
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 0);
        assert_eq!(outcome.conflicts, vec!["a.bin".to_string()]);
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"locally edited",
            "a conflicting local edit must never be silently overwritten by --rematerialize"
        );
    }

    #[test]
    fn rematerialize_force_restores_a_locally_modified_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        // Validation exposes the local edit; force then resolves its conflict.
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                force: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 1);
        assert!(outcome.conflicts.is_empty());
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
    }

    #[test]
    fn rematerialize_leaves_the_file_untouched_when_the_cache_object_is_missing() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        let entry = track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        let obj = repo
            .resolved_cache_root()
            .unwrap()
            .object_path_for_test(&entry.oid);
        std::fs::remove_file(&obj).unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 0);
        assert_eq!(
            outcome.missing,
            vec![(
                gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                entry.oid,
            )]
        );
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
    }

    #[test]
    fn rematerialize_leaves_the_file_untouched_when_the_cache_object_is_corrupted() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        let entry = track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        let cache_root = repo.resolved_cache_root().unwrap();
        let obj = cache_root.object_path_for_test(&entry.oid);
        cache_root
            .make_object_writable_for_test(&entry.oid)
            .unwrap();
        std::fs::write(&obj, b"XXXXX").unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 0);
        assert_eq!(
            outcome.corrupted,
            vec![(
                gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                entry.oid,
            )]
        );
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
    }

    #[test]
    fn rematerialize_changed_desired_oid_uses_ordinary_replace() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        track(&repo, "a.bin", b"world");

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.replaced, 1);
        assert_eq!(outcome.rematerialized, 0);
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"world");
    }

    #[test]
    fn rematerialize_removed_desired_path_uses_ordinary_remove() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        repo.save_lock(&Lock::default()).unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.rematerialized, 0);
        assert!(!tmp.path().join("a.bin").exists());
    }

    #[test]
    fn dry_run_rematerialize_reports_but_never_touches_disk_or_state() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        let before_meta = std::fs::symlink_metadata(tmp.path().join("a.bin")).unwrap();
        let before_stat = materialized_row(&repo, "a.bin");

        set_strategy(&repo, "symlink");
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 1);
        assert!(
            !std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "--dry-run must never actually change the file's representation"
        );
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
        let after_meta = std::fs::symlink_metadata(tmp.path().join("a.bin")).unwrap();
        assert_eq!(
            before_meta.modified().unwrap(),
            after_meta.modified().unwrap()
        );
        assert_eq!(materialized_row(&repo, "a.bin"), before_stat);
    }

    /// Preview must validate local edits just as mutating rematerialization does.
    #[test]
    fn dry_run_rematerialize_reports_a_conflict_after_trust_state_sync() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "a.bin", b"hello");
        sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::TrustState,
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 0);
        assert_eq!(outcome.conflicts, vec!["a.bin".to_string()]);
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"locally edited"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_rematerialize_attempt_leaves_the_original_file_intact() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "dir/a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        let dir = tmp.path().join("dir");
        let original = std::fs::read(dir.join("a.bin")).unwrap();

        // Make the destination directory read-only so building the new
        // temp-sibling representation fails outright -- this must never
        // remove or truncate the pre-existing, still-valid a.bin.
        use std::os::unix::fs::PermissionsExt;
        let writable = std::fs::metadata(&dir).unwrap().permissions();
        let mut readonly = writable.clone();
        readonly.set_mode(0o500);
        std::fs::set_permissions(&dir, readonly).unwrap();

        let result = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        );

        std::fs::set_permissions(&dir, writable).unwrap();

        assert!(
            result.is_err(),
            "expected the rematerialize attempt to fail while the directory was read-only"
        );
        assert_eq!(
            std::fs::read(dir.join("a.bin")).unwrap(),
            original,
            "a failed rematerialize attempt must never destroy the original file"
        );
        assert_eq!(
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("a.bin")],
            "a failed rematerialize must leave no temp file/directory behind"
        );
    }

    /// A stale (or maliciously placed) file at the *old*, pre-collision-safe
    /// deterministic `<dest>.gat-rematerialize-tmp` sibling path must never be
    /// touched: `do_rematerialize` now builds its temp representation inside
    /// a freshly created, uniquely named temp *directory*, so that old fixed
    /// sibling name is just an ordinary, unrelated user path as far as
    /// `--rematerialize` is concerned.
    #[test]
    fn rematerialize_never_touches_a_preexisting_old_style_deterministic_tmp_sibling() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        set_strategy(&repo, "copy");
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        let sentinel_path = tmp.path().join("a.bin.gat-rematerialize-tmp");
        std::fs::write(&sentinel_path, b"do not touch me").unwrap();

        set_strategy(&repo, "symlink");
        let outcome = sync(
            &repo,
            &SyncOptions {
                policy: crate::ReconciliationPolicy::Validate {
                    rematerialize: true,
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.rematerialized, 1);
        assert_eq!(
            std::fs::read(&sentinel_path).unwrap(),
            b"do not touch me",
            "the old deterministic temp-sibling name must be treated as an \
             ordinary, unrelated user file"
        );
        // See the copy->symlink test above for why this is Unix-only.
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(tmp.path().join("a.bin"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
