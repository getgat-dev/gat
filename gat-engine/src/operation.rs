//! Operation owner.
//!
//! [`Operation`] is the single owner of one command's `&Repository`, its immutable
//! [`Snapshot`], and its mutable [`Session`]. It bundles the state captured
//! for one repository generation and performs no I/O during construction.
//!
//! Construction here performs no I/O of its own: it takes an
//! already-captured [`Snapshot`] and [`Session`] and simply
//! bundles them with the `&Repository` they were captured for.

use super::session::Session;
use super::snapshot::Snapshot;
use crate::repository::Repository;

/// One operation's repository reference, immutable snapshot, and mutable
/// execution session, owned together so a caller can never mix a snapshot
/// captured for one repository generation with a session (or `&Repository`) from
/// another.
///
/// # Structural guarantee: no desired-row access
///
/// `Operation` has no `desired`/`desired_view` field or method anywhere in
/// this module -- only the higher-level `DesiredOperation`,
/// which wraps an `Operation` plus an explicit `DesiredSnapshot`, exposes
/// desired-row reads. A desired-free command (`sync`, dry-run planning,
/// repair) therefore cannot reach desired rows by construction: there is no
/// method to call, not merely a runtime check that would reject one.
pub struct Operation<'repo> {
    repo: &'repo Repository,
    snapshot: Snapshot,
    session: Session,
}

/// A disjoint borrow of every service one bounded transfer window needs,
/// produced by [`Operation::window_services`].
pub(crate) struct WindowServices<'op> {
    pub(crate) cache_root: &'op gat_io::CacheRoot,
    pub(crate) policy: &'op super::path_policy::EffectivePathPolicy,
    pub(crate) remotes_catalog: &'op super::remote_catalog::RemoteCatalog,
    pub(crate) remotes: &'op mut super::remote_session::RemoteSession,
    pub(crate) remote_executor: &'op super::remote_executor::RemoteExecutor,
    pub(crate) limits: &'op super::limits::ExecutionLimits,
    pub(crate) cache_session: &'op mut super::cache_session::CacheSession,
}

impl<'repo> Operation<'repo> {
    /// Cancellation remains operation-scoped; callers may send it from another thread.
    pub fn transfer_cancellation(&self) -> crate::TransferCancellation {
        self.session.transfer_cancellation()
    }
    /// Builds an `Operation` from an already-captured `snapshot` and
    /// `session` -- the same values a coherent-snapshot-barrier call
    /// (the coherent repository snapshot acquisition path)
    /// already produces today. Side-effect free: this constructor performs
    /// no additional I/O beyond bundling the three references/values.
    pub(crate) const fn new(repo: &'repo Repository, snapshot: Snapshot, session: Session) -> Self {
        Self {
            repo,
            snapshot,
            session,
        }
    }

    pub const fn repo(&self) -> &'repo Repository {
        self.repo
    }

    /// This operation's captured [`crate::repository_state::DesiredRevision`]
    /// -- delegates to [`Snapshot::desired_revision`]. Needed by
    /// [`super::mutation::MutationGuard::require_desired_identity`], which
    /// only holds a `&Operation` (not the test-only `Operation::snapshot`
    /// accessor), to read the revision it must compare `refresh()`'s
    /// result against.
    pub(crate) const fn desired_revision(&self) -> &crate::repository_state::DesiredRevision {
        self.snapshot.desired_revision()
    }

    /// This operation's compiled effective path policy (mount ownership,
    /// route resolution) -- delegates to `Snapshot::policy`. A thin
    /// pass-through so callers that only need `Operation` never need to
    /// reach back through a broader type merely to read
    /// this.
    pub const fn policy(&self) -> &super::path_policy::EffectivePathPolicy {
        self.snapshot.policy()
    }

    /// This operation's compiled remote catalog -- delegates to
    /// `Snapshot::remotes_catalog`.
    pub const fn remotes_catalog(&self) -> &super::remote_catalog::RemoteCatalog {
        self.snapshot.remotes_catalog()
    }

    /// This operation's typed execution-resource bounds -- delegates to
    /// `Session::limits`.
    pub const fn limits(&self) -> &super::limits::ExecutionLimits {
        self.session.limits()
    }

    /// Splits this operation into disjoint borrows of every service one
    /// bounded transfer window needs at once: `repo`/`policy`/`remotes_catalog`
    /// (read-only, from
    /// `Snapshot`), `remotes`/`remote_executor`/`limits` (read-only, from
    /// `Session`), and a mutable [`super::cache_session::CacheSession`]
    /// borrow -- so a window driver (`run_fetch_window`,
    /// `run_push_window`) can open a remote operator and dispatch through
    /// the executor from *inside* the closure passed to
    /// `cache_session.cache(...)`, instead of needing sequential
    /// `&self`/`&mut self` calls on the whole `Operation` that would
    /// conflict with each other.
    pub(crate) const fn window_services(&mut self) -> WindowServices<'_> {
        let (remotes, remote_executor, limits, cache_session) = self.session.split_mut();
        WindowServices {
            cache_root: self.snapshot.cache_root(),
            policy: self.snapshot.policy(),
            remotes_catalog: self.snapshot.remotes_catalog(),
            remotes,
            remote_executor,
            limits,
            cache_session,
        }
    }

    /// Checks one caller-bounded window of objects against their resolved
    /// remotes, calling `on_result` as soon as each individual check
    /// completes instead of buffering every result until the whole window
    /// finishes.
    pub fn check_remote_presence_streaming<T: crate::transfer::RemotePresenceObligation>(
        &mut self,
        obligations: &[T],
        on_result: impl FnMut(crate::transfer::RemotePresenceResult),
        progress: &gat_core::progress::ProgressHandle,
    ) -> std::result::Result<(), crate::transfer::RemotePresenceError> {
        crate::transfer::check_remote_presence_streaming(self, obligations, on_result, progress)
    }

    /// Validates one already-resolved remote without network I/O.
    /// Validation only borrows the captured configuration.
    ///
    /// ```
    /// use gat_engine::{Operation, RemoteId, RemoteSessionError};
    /// fn validate(operation: &Operation<'_>, id: RemoteId) -> Result<(), RemoteSessionError> {
    ///     operation.validate_remote(id)
    /// }
    /// ```
    pub fn validate_remote(
        &self,
        id: crate::remote_catalog::RemoteId,
    ) -> Result<(), crate::remote_session::RemoteSessionError> {
        crate::remote_session::RemoteSession::validate(self.snapshot.remotes_catalog(), id)
    }

    /// This operation's loaded effective [`gat_core::config::Config`]
    /// -- delegates to `Snapshot::config`.
    pub const fn config(&self) -> &gat_core::config::Config {
        self.snapshot.config()
    }

    /// Splits this operation into a disjoint immutable [`Snapshot`] borrow
    /// and mutable [`Session`] borrow, both from the same `&mut self`
    /// borrow: a
    /// reconciliation caller (`engine::workspace::sync::plan_dry_run`/
    /// `execute_mutating_sync`) that needs to read config/objects-dir/
    /// link-modes *and* later call into the mutable session (e.g.
    /// `Session::cache_session_mut`) cannot do so through sequential
    /// `&self`/`&mut self` calls on the whole `Operation` -- each would
    /// (re)borrow the whole value, and the borrow checker cannot see that
    /// `Snapshot` and `Session` never alias. This is a plain tuple, not a
    /// dedicated capability type: `Operation`/`MutationGuard` remain the
    /// one place command code assembles reconciliation-scoped state, so
    /// there is no separate "runtime" abstraction to keep in sync with
    /// them.
    pub(crate) const fn split_for_sync(&mut self) -> (&'repo Repository, &Snapshot, &mut Session) {
        (self.repo, &self.snapshot, &mut self.session)
    }

    /// Replaces this operation's captured desired revision in place -- used only
    /// by a caller that just performed its own legitimate, already-`RepoLock`-
    /// scoped desired-state mutation (sync's `gat.lock` reshape)
    /// and needs this operation's captured [`crate::repository_state::DesiredRevision`]
    /// to reflect that self-performed change, rather than treating it as a
    /// concurrent external race the next [`Self::mutate`] call would reject
    /// (see also the sync command coordinator). Every other field of
    /// [`Snapshot`] remains unchanged, avoiding redundant cache-path
    /// resolution and policy/catalog recompilation.
    pub(crate) const fn replace_desired_revision(
        &mut self,
        desired_revision: crate::repository_state::DesiredRevision,
    ) {
        self.snapshot.replace_desired_revision(desired_revision);
    }

    /// Applies the configured lock shape, if needed, and updates this
    /// operation's desired revision inside the same engine-owned workflow.
    pub fn reshape_lock_if_needed(
        &mut self,
        on_reshape: impl FnOnce(),
    ) -> Result<
        Option<gat_core::lock::LockShardLevels>,
        crate::repository_state::DesiredRevisionError,
    > {
        let reshaped = crate::repository_state::reshape_and_capture_revision(
            self.repo,
            self.snapshot.config(),
            on_reshape,
        )?;
        if let Some((levels, desired_revision)) = reshaped {
            self.replace_desired_revision(desired_revision);
            Ok(Some(levels))
        } else {
            Ok(None)
        }
    }

    /// The single mutation gate: acquires
    /// [`gat_io::RepoLock`], revalidates this operation's captured
    /// [`crate::repository_state::DesiredRevision`] against the
    /// repository's current canonical desired state, and -- only once that
    /// revalidation succeeds -- returns a [`super::mutation::MutationGuard`]
    /// borrowing this operation for the duration of the caller's mutating
    /// reconciliation. A stale revision (a concurrent `gat.lock` mutation
    /// that landed after this operation's snapshot was captured) is
    /// rejected here, before any worktree/materialized-state mutation
    /// occurs: the returned
    /// [`crate::repository_state::StaleDesiredRevisionError`] means no
    /// mutation-access capability or `MutationGuard` was handed to the
    /// caller.
    /// Engine reconciliation is the sole production caller; command
    /// orchestration cannot acquire or name the returned guard.
    pub(crate) fn mutate(
        &mut self,
    ) -> std::result::Result<
        super::mutation::MutationGuard<'_, 'repo>,
        crate::repository_state::DesiredRevisionError,
    > {
        let access = self
            .repo
            .acquire_mutation_access(self.snapshot.desired_revision())?;
        Ok(super::mutation::MutationGuard::new(self, access))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::Repository as Repo;

    /// Constructing an `Operation` from already-captured snapshot/session
    /// values must not perform any additional resource initialization: no
    /// remote operator, no `cache.sqlite3` open.
    #[test]
    fn construction_performs_no_additional_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let repo = Repo::at(tmp.path().to_path_buf());
        let config = repo.load_config().unwrap();
        let desired_revision = crate::repository_state::current_desired_revision(&repo).unwrap();
        let snapshot = Snapshot::new(repo.snapshot_input(config, desired_revision)).unwrap();
        let session = Session::new();

        let remote_opens_before = crate::remote_session::test_support::remote_opens();
        let op = Operation::new(&repo, snapshot, session);
        let remote_opens_after = crate::remote_session::test_support::remote_opens();

        assert_eq!(remote_opens_after, remote_opens_before);

        // The public semantic accessor still reaches the original repository.
        assert!(std::ptr::eq(op.repo(), &raw const repo));
    }

    // Revalidation, locking, and fail-closed race behavior are kept in the
    // adjacent private test module below.
}

#[cfg(test)]
mod contract_tests {
    use crate::{DesiredOperation, Operation, Repository as Repo};
    use gat_core::lock::{Entry, Lock, LockShardLevels};
    use gat_core::{lexical_path::GatPath, oid::Oid};
    use gat_io::{LockStore, StateStore};

    fn entry(path: &str, byte: u8) -> Entry {
        Entry {
            path: GatPath::parse_canonical(path).unwrap(),
            oid: Oid::from_hex(&format!("{byte:02x}").repeat(32)).unwrap(),
        }
    }

    fn tracked_repo() -> (crate::test_harness::TestRepo, Repo) {
        let tmp = crate::test_harness::test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        repo.save_lock(&Lock {
            entries: vec![entry("a.bin", 1)],
        })
        .unwrap();
        (tmp, repo)
    }

    fn operation(repo: &Repo) -> Operation<'_> {
        crate::repo_snapshot::acquire_operation_without_desired_state(
            repo,
            &gat_core::progress::NoopProgress,
        )
        .unwrap()
    }

    #[test]
    fn mutate_succeeds_when_the_desired_revision_is_unchanged() {
        let (_tmp, repo) = tracked_repo();
        let mut operation = operation(&repo);

        let guard = operation.mutate().unwrap();
        assert!(std::ptr::eq(guard.operation().repo(), &raw const repo));
    }

    #[test]
    fn reshape_updates_the_operations_revision_under_the_same_lock() {
        let (_tmp, repo) = tracked_repo();
        let mut config = repo
            .load_config_scoped(gat_core::config::ConfigScope::Project)
            .unwrap();
        let target = LockShardLevels::new(1).unwrap();
        config.lock.shard_levels = Some(target);
        repo.save_config_scoped(&config, gat_core::config::ConfigScope::Project)
            .unwrap();
        let mut operation = operation(&repo);
        let before = *operation.desired_revision();
        let callback_calls = std::cell::Cell::new(0);

        let reshaped = operation
            .reshape_lock_if_needed(|| callback_calls.set(callback_calls.get() + 1))
            .unwrap();

        assert_eq!(reshaped, Some(target));
        assert_eq!(callback_calls.get(), 1);
        assert_ne!(*operation.desired_revision(), before);
        operation.mutate().unwrap();
    }

    #[test]
    fn matching_shape_skips_callback_lock_and_full_load() {
        let (_tmp, repo) = tracked_repo();
        let mut operation = operation(&repo);
        let callback_calls = std::cell::Cell::new(0);
        let loads_before = gat_io::lock_test_support::reshape_full_loads();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();

        let reshaped = gat_io::atomic_test_support::with_acquire_attempt_hook(
            std::thread::current().id(),
            attempted_tx,
            || operation.reshape_lock_if_needed(|| callback_calls.set(callback_calls.get() + 1)),
        )
        .unwrap();

        assert_eq!(reshaped, None);
        assert_eq!(callback_calls.get(), 0);
        assert!(attempted_rx.try_recv().is_err());
        assert_eq!(
            gat_io::lock_test_support::reshape_full_loads(),
            loads_before
        );
    }

    #[test]
    fn mutate_rejects_a_stale_desired_revision() {
        let (_tmp, repo) = tracked_repo();
        let mut operation = operation(&repo);

        repo.save_lock(&Lock {
            entries: vec![entry("a.bin", 1), entry("b.bin", 2)],
        })
        .unwrap();

        let error = operation.mutate().map(|_| ()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "desired state (gat.lock) changed during this operation; \
             re-run the command to observe the current state before mutating anything"
        );
    }

    #[test]
    fn mutate_after_a_long_remote_phase_with_concurrent_wal_churn_still_rejects_stale_state() {
        let (tmp, repo) = tracked_repo();
        let desired_operation =
            DesiredOperation::acquire(&repo, &gat_core::progress::NoopProgress).unwrap();
        let database_path = tmp.path().join(".gat/state/state.sqlite3");
        let wal_path = database_path.with_file_name(format!(
            "{}-wal",
            database_path.file_name().unwrap().to_string_lossy()
        ));
        let mut lock = Lock {
            entries: vec![entry("a.bin", 1)],
        };

        for index in 0..8u8 {
            let added = entry(&format!("noise-{index}.bin"), index + 2);
            lock.entries.push(added.clone());
            lock.entries
                .sort_by(|left, right| left.path.cmp(&right.path));
            repo.save_lock(&lock).unwrap();

            let mut writer = StateStore::open(repo.layout()).unwrap();
            writer
                .upsert_desired_for_test(&[added], LockShardLevels::FLAT)
                .unwrap();
            writer.set_busy_timeout_ms_for_test(0).unwrap();
            writer.checkpoint_and_truncate_wal().unwrap();
            assert!(
                std::fs::metadata(&wal_path).map_or(0, |meta| meta.len()) > 0,
                "round {index}: the live desired operation must retain its WAL pin"
            );
        }

        let mut operation = desired_operation.finish_selection();
        let writer = StateStore::open(repo.layout()).unwrap();
        writer.checkpoint_and_truncate_wal().unwrap();
        assert_eq!(
            std::fs::metadata(&wal_path).map_or(0, |meta| meta.len()),
            0,
            "finish_selection must immediately release the desired WAL pin"
        );

        let error = operation.mutate().map(|_| ()).unwrap_err();
        assert!(matches!(
            error,
            crate::DesiredRevisionError::Stale(crate::StaleDesiredRevisionError)
        ));
    }

    #[test]
    fn mutate_lock_blocks_a_concurrent_desired_state_write_until_the_guard_is_dropped() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;

        let (_tmp, repo) = tracked_repo();
        let mut operation = operation(&repo);
        let guard_released = Arc::new(AtomicBool::new(false));
        let writer_observed_release = Arc::new(AtomicBool::new(false));
        let layout = repo.layout().clone();
        let (start_tx, start_rx) = mpsc::channel::<()>();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let writer = {
            let guard_released = Arc::clone(&guard_released);
            let writer_observed_release = Arc::clone(&writer_observed_release);
            std::thread::spawn(move || {
                start_rx.recv().unwrap();
                let _lock = gat_io::RepoLock::acquire_repository(&layout).unwrap();
                LockStore::publish_repository(
                    &layout,
                    &Lock {
                        entries: vec![entry("a.bin", 1), entry("concurrent.bin", 2)],
                    },
                    LockShardLevels::FLAT,
                )
                .unwrap();
                writer_observed_release
                    .store(guard_released.load(Ordering::SeqCst), Ordering::SeqCst);
            })
        };

        let guard = operation.mutate().unwrap();
        gat_io::atomic_test_support::with_acquire_attempt_hook(
            writer.thread().id(),
            attempted_tx,
            || {
                start_tx.send(()).unwrap();
                attempted_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("writer must attempt to acquire the repository lock");
                guard_released.store(true, Ordering::SeqCst);
                drop(guard);
            },
        );

        writer.join().unwrap();
        assert!(
            writer_observed_release.load(Ordering::SeqCst),
            "the mutation guard must hold RepoLock until it is dropped"
        );
    }

    #[test]
    fn mutate_succeeds_despite_concurrent_config_edits() {
        let (_tmp, repo) = tracked_repo();
        let mut operation = operation(&repo);

        let mut project = repo
            .load_config_scoped(gat_core::config::ConfigScope::Project)
            .unwrap();
        project.sync.trust_state = Some(true);
        repo.save_config_scoped(&project, gat_core::config::ConfigScope::Project)
            .unwrap();

        let mut local = repo
            .load_config_scoped(gat_core::config::ConfigScope::Local)
            .unwrap();
        local.cache.materialization_strategy =
            Some(gat_core::config::MaterializationStrategy::from_values(&["copy"]).unwrap());
        repo.save_config_scoped(&local, gat_core::config::ConfigScope::Local)
            .unwrap();

        operation.mutate().unwrap();
    }

    #[test]
    fn require_desired_identity_rejects_a_lock_rewrite_that_bypassed_repo_lock() {
        let (_tmp, repo) = tracked_repo();
        let mut operation = operation(&repo);
        let expected = operation.desired_revision().identity();
        let guard = operation.mutate().unwrap();

        LockStore::publish_repository(
            repo.layout(),
            &Lock {
                entries: vec![entry("a.bin", 1), entry("external-bypass.bin", 2)],
            },
            LockShardLevels::FLAT,
        )
        .unwrap();
        let mut store = StateStore::open(repo.layout()).unwrap();
        let refreshed = crate::workspace::sync::refresh_desired_index(&repo, &mut store).unwrap();
        assert_ne!(refreshed.desired_identity, expected);

        let error = guard
            .require_desired_identity(refreshed.desired_identity)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "desired state (gat.lock) changed during this operation; \
             re-run the command to observe the current state before mutating anything"
        );
    }

    struct DesiredIdentityRaceHookGuard;

    impl Drop for DesiredIdentityRaceHookGuard {
        fn drop(&mut self) {
            gat_io::lock_race_test_hooks::clear();
        }
    }

    #[test]
    fn mutate_revalidation_fails_closed_on_a_shard_rewritten_mid_read() {
        let _race_guard = DesiredIdentityRaceHookGuard;
        let (tmp, repo) = tracked_repo();
        let mut store = StateStore::open(repo.layout()).unwrap();
        crate::workspace::sync::refresh_desired_index(&repo, &mut store).unwrap();
        let mut operation = operation(&repo);
        let target = tmp.path().join("gat.lock");

        let mut touched = std::fs::read(&target).unwrap();
        touched.push(b'\n');
        std::fs::write(&target, &touched).unwrap();

        gat_io::lock_race_test_hooks::set(move |path| {
            if path == target {
                std::fs::write(path, b"rewritten-mid-read-different-length-payload").unwrap();
            }
        });

        let error = operation.mutate().map(|_| ()).unwrap_err();
        let retained_changed_during_observation =
            std::iter::successors(Some(&error as &dyn std::error::Error), |source| {
                source.source()
            })
            .any(|source| source.to_string().contains("changed while being observed"));
        assert!(
            retained_changed_during_observation,
            "mid-read rewrites must fail closed and retain their physical source: {error:?}"
        );
    }
}
