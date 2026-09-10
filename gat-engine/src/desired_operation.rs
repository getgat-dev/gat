//! Explicit, short-lived desired-state capability: [`DesiredOperation`] is
//! the one type that owns both a
//! [`crate::operation::Operation`] and the `DesiredSnapshot`
//! pinned alongside it, so "this operation can read desired rows" is a
//! distinct, explicit type a caller either has or doesn't -- never an
//! `Option<DesiredSnapshot>` a desired-free command silently carries
//! around and might mistakenly query.
//!
//! Constructed only from the coherent snapshot/desired acquisition barrier
//! ([`super::repo_snapshot::recover_and_open_coherent_snapshot`]): every
//! `DesiredOperation` therefore carries a desired-state mirror refreshed in
//! the same repository-synchronization barrier that captured its
//! `Operation`'s config/policy, so the two can never observe different
//! repository generations.
//!
//! ## Desired-reader lifetime
//!
//! `DesiredOperation` is a *read capability*, not a mutation-phase-only
//! object: its underlying `SQLite` read transaction/WAL pin may legitimately
//! stay alive for the entire duration of a read-only streaming transfer.
//! Read-only `push`, `fetch`, and `status --remote` hold it (via
//! [`DesiredOperation::split_for_selection`]) across their whole bounded
//! remote-window execution -- selection, remote I/O, and completion all
//! interleave with the desired cursor still pinned -- rather than being
//! required to finish selection and discard the desired snapshot before
//! any network I/O starts.
//!
//! [`DesiredOperation::finish_selection`] is the one mandatory consuming boundary, but
//! it is required only immediately *before* a composite operation enters
//! mutating worktree/materialized-state reconciliation (`pull`'s
//! transition into the command's sync-with-operation boundary, `gat hook`'s
//! implicit-fetch transition into the same). A command that never mutates through this
//! `DesiredOperation` at all (plain `push`/`fetch`/`status --remote`) may
//! simply let it drop at the end of the command; there is no requirement
//! to call `finish_selection` first.

use crate::desired_snapshot::{DesiredSnapshot, DesiredView};
use crate::operation::Operation;
use crate::repo_snapshot::RepoSnapshotError;
use crate::repository::Repository as Repo;
use gat_core::progress::ProgressReporter;

type Result<T> = std::result::Result<T, RepoSnapshotError>;

/// An [`Operation`] plus the `DesiredSnapshot` captured alongside it.
/// Commands that need to read desired rows for selection/route/ownership
/// planning (`push`, `fetch`, `pull`, `status --remote`) hold this instead
/// of a desired-free `Operation`. A read-only command may keep it pinned
/// through its whole streaming transfer (see the module doc comment's
/// "Desired-reader lifetime" section); a composite command that later
/// mutates must call [`Self::finish_selection`] first.
pub struct DesiredOperation<'repo> {
    operation: Operation<'repo>,
    desired: DesiredSnapshot,
}

impl<'repo> DesiredOperation<'repo> {
    /// Builds a `DesiredOperation` from an already-captured `operation` and
    /// `desired` snapshot -- the same coherent-generation pair
    /// [`super::repo_snapshot::recover_and_open_coherent_snapshot`]
    /// already produces today. Side-effect free.
    pub(crate) const fn new(operation: Operation<'repo>, desired: DesiredSnapshot) -> Self {
        Self { operation, desired }
    }

    /// Crosses the repository synchronization barrier and builds a
    /// `DesiredOperation` directly: the one
    /// production acquisition path for a command that needs desired rows
    /// (`push`, `fetch`, `pull`, `status --remote`, `gat sync`'s implicit
    /// fetch, `gat hook`'s implicit fetch). Recovers any interrupted mount
    /// mutation, reads effective config, and refreshes the desired-state
    /// mirror, all inside one [`gat_io::RepoLock`] acquisition.
    pub fn acquire(repo: &'repo Repo, progress: &dyn ProgressReporter) -> Result<Self> {
        let (snapshot, desired) =
            crate::repo_snapshot::recover_and_open_coherent_snapshot(repo, progress)?;
        let session = crate::session::Session::new(repo, snapshot.config());
        let operation = Operation::new(repo, snapshot, session);
        Ok(Self::new(operation, desired))
    }

    /// As [`Self::acquire`], but with deliberately tiny [`crate::limits::ExecutionLimits`]
    /// instead of production defaults -- for structural tests that need to
    /// force multiple bounded windows without huge fixtures.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn acquire_with_limits(
        repo: &'repo Repo,
        progress: &dyn ProgressReporter,
        limits: crate::limits::ExecutionLimits,
    ) -> Result<Self> {
        let (snapshot, desired) =
            crate::repo_snapshot::recover_and_open_coherent_snapshot(repo, progress)?;
        let session = crate::session::Session::configured(
            limits,
            repo.inputs.templates(),
            snapshot.config().network.resolve(),
        );
        let operation = Operation::new(repo, snapshot, session);
        Ok(Self::new(operation, desired))
    }

    /// Borrows the base [`Operation`] without consuming this
    /// `DesiredOperation` -- for reading operation/snapshot/session-level
    /// state (policy, limits, remotes, ...) while selection is still in
    /// progress. See [`Self::finish_selection`] for the explicit,
    /// consuming end of that lifetime.
    pub const fn operation(&self) -> &Operation<'repo> {
        &self.operation
    }

    /// Read-only desired rows for selection and route planning.
    /// This [`DesiredView`] exposes neither the materialized ledger nor
    /// state mutation.
    pub const fn desired_view(&self) -> DesiredView<'_> {
        self.desired.view()
    }

    /// Splits this `DesiredOperation` into disjoint mutable/read-only
    /// borrows of its two fields at once: a
    /// caller that needs to both stream downloads through the mutable
    /// `Operation` (opening the shared
    /// `CacheClient`) *and* read rows through the still-alive
    /// [`DesiredView`] in the same call cannot do so through two
    /// sequential `&self`/`&mut self` method calls -- each would borrow
    /// the whole `DesiredOperation`. Implemented here, inside this type's
    /// own `impl` block, where the two fields are visibly disjoint to the
    /// borrow checker.
    pub const fn split_for_selection(&mut self) -> (&mut Operation<'repo>, DesiredView<'_>) {
        (&mut self.operation, self.desired.view())
    }

    /// Ends desired-state selection explicitly: drops `self.desired` --
    /// and with it the desired-state `SQLite` read handle/WAL pin it holds
    /// -- *before* returning the base `Operation`. Required only
    /// immediately before a composite operation enters mutating
    /// worktree/materialized-state reconciliation (`pull`'s transition
    /// into the command's sync-with-operation boundary, `gat hook`'s
    /// implicit-fetch transition into the same) -- **not** before remote I/O in general. A
    /// read-only command (`push`, `fetch`, `status --remote`) that never
    /// mutates through this `DesiredOperation` may keep it alive through
    /// its whole streaming transfer and simply let it drop at the end of
    /// the command instead of calling this.
    pub fn finish_selection(self) -> Operation<'repo> {
        drop(self.desired);
        self.operation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructing a `DesiredOperation` from already-captured
    /// operation/desired values must not perform any additional resource
    /// initialization: no remote operator, no
    /// second `state.sqlite3`/`cache.sqlite3` open.
    #[test]
    fn construction_performs_no_additional_side_effects() {
        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let remote_opens_before = crate::remote_session::test_support::remote_opens();
        let (snapshot, desired) = super::super::repo_snapshot::recover_and_open_coherent_snapshot(
            &repo,
            &gat_core::progress::NoopProgress,
        )
        .unwrap();
        let session = crate::session::Session::for_test();
        let operation = Operation::new(&repo, snapshot, session);
        let remote_opens_after_operation = crate::remote_session::test_support::remote_opens();

        let desired_op = DesiredOperation::new(operation, desired);
        let remote_opens_after = crate::remote_session::test_support::remote_opens();

        assert_eq!(remote_opens_after, remote_opens_after_operation);
        assert_eq!(remote_opens_after, remote_opens_before);

        assert!(std::ptr::eq(desired_op.operation().repo(), &raw const repo));
        let _ = desired_op.desired_view();
    }

    /// [`DesiredOperation::desired_view`] must
    /// observe the same rows the underlying [`DesiredSnapshot::view`] does
    /// -- it is a narrowing accessor, not a different read path.
    #[test]
    fn desired_view_observes_the_same_rows_as_the_underlying_snapshot() {
        use gat_core::lock::Entry;
        use gat_io::StateStore;

        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let mut store = StateStore::open(repo.layout()).unwrap();
        store
            .upsert_desired_for_test(
                &[Entry {
                    path: gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                    oid: gat_core::oid::Oid::from_hex(&"0".repeat(64)).unwrap(),
                }],
                gat_core::lock::LockShardLevels::FLAT,
            )
            .unwrap();
        let desired = DesiredSnapshot::new(store);

        let (snapshot, _) = super::super::repo_snapshot::recover_and_open_coherent_snapshot(
            &repo,
            &gat_core::progress::NoopProgress,
        )
        .unwrap();
        let session = crate::session::Session::for_test();
        let operation = Operation::new(&repo, snapshot, session);
        let desired_op = DesiredOperation::new(operation, desired);

        let mut seen = Vec::new();
        desired_op
            .desired_view()
            .visit_entries(&gat_core::selection::Selection::default(), |row| {
                seen.push(row.path.to_string());
                Ok::<(), crate::RepositoryStateError>(())
            })
            .unwrap();
        assert_eq!(seen, vec!["a.bin".to_string()]);
    }

    /// `finish_selection` must release the
    /// desired-state `SQLite` read transaction/WAL pin -- not merely make
    /// the `DesiredSnapshot` value inaccessible -- immediately once it is
    /// called, and the fact that the base `Operation` returned by
    /// `finish_selection` continues to live must not keep that pin alive.
    /// Reproduce the exact concurrent-writer/WAL-checkpoint probe
    /// `storage::state::tests::pinned_desired_snapshot_blocks_wal_checkpoint_until_dropped`
    /// uses at the `StateStore` layer, but drive it through the
    /// `DesiredOperation`/`finish_selection` API surface push/fetch/pull
    /// actually use, and assert the checkpoint succeeds *immediately after
    /// `finish_selection` returns* -- while the returned `Operation` (this
    /// test's stand-in for "the top-level operation continues" into
    /// mutating reconciliation) is still very much alive.
    ///
    /// This does *not* claim read-only remote I/O must happen only after
    /// this boundary: read-only
    /// `push`/`fetch`/`status` may retain the `DesiredOperation` across
    /// bounded remote I/O (see
    /// the command integration test covering a long remote phase with
    /// concurrent WAL churn).
    /// `finish_selection` is only *mandatory* at the transition into
    /// mutating reconciliation; what this test proves is narrower and
    /// unconditional: whenever it *is* called, the pin is gone
    /// immediately, regardless of what the caller does before or after.
    #[test]
    fn finish_selection_releases_the_desired_wal_pin_before_mutating_reconciliation() {
        use gat_core::lock::Entry;
        use gat_io::StateStore;

        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        // Seed desired state so the pinned reader has something to pin
        // against, exactly as the lower-level state-store test does.
        let mut writer = StateStore::open(repo.layout()).unwrap();
        writer
            .upsert_desired_for_test(
                &[Entry {
                    path: gat_core::lexical_path::GatPath::parse_canonical("seed.bin").unwrap(),
                    oid: gat_core::oid::Oid::from_hex(&"1".repeat(64)).unwrap(),
                }],
                gat_core::lock::LockShardLevels::FLAT,
            )
            .unwrap();

        // Acquire a `DesiredOperation` exactly as push/fetch/pull do: this
        // is the pinned-reader connection under test.
        let (snapshot, desired) = super::super::repo_snapshot::recover_and_open_coherent_snapshot(
            &repo,
            &gat_core::progress::NoopProgress,
        )
        .unwrap();
        let session = crate::session::Session::for_test();
        let operation = Operation::new(&repo, snapshot, session);
        let desired_op = DesiredOperation::new(operation, desired);

        // Simulate concurrent desired-state writers (a second process
        // running `gat add`/`gat mv`) landing commits while selection is
        // still in progress, so there is real WAL growth for a checkpoint
        // to (attempt to) reclaim.
        for i in 0..50u32 {
            let byte = (i % 250) as u8;
            writer
                .upsert_desired_for_test(
                    &[Entry {
                        path: gat_core::lexical_path::GatPath::parse_canonical(&format!(
                            "concurrent-{i}.bin"
                        ))
                        .unwrap(),
                        oid: gat_core::oid::Oid::from_hex(&format!("{byte:02x}").repeat(32))
                            .unwrap(),
                    }],
                    gat_core::lock::LockShardLevels::FLAT,
                )
                .unwrap();
        }

        // Selection is complete: call `finish_selection` at the
        // transition into mutating reconciliation (the only point it's
        // mandatory at, per the adopted contract).
        let op = desired_op.finish_selection();

        // The WAL pin must already be gone -- a checkpoint attempted right
        // now (with `op`, the base `Operation`, still alive, standing in
        // for "the top-level operation continues" into mutating
        // reconciliation) must be able to truncate the WAL. If
        // `finish_selection` merely hid the `DesiredSnapshot` value without
        // actually dropping its `StateStore`/read transaction, this
        // checkpoint would still be blocked exactly like the lower-level
        // test proves for a live pin.
        writer.checkpoint_and_truncate_wal().unwrap();
        let db_path = tmp.path().join(".gat/state/state.sqlite3");
        let wal_path = db_path.with_file_name(format!(
            "{}-wal",
            db_path.file_name().unwrap().to_string_lossy()
        ));
        let wal_size_after_finish_selection = std::fs::metadata(&wal_path).map_or(0, |m| m.len());
        assert_eq!(
            wal_size_after_finish_selection, 0,
            "finish_selection must release the desired-state WAL pin immediately \
             once called -- a checkpoint right afterwards must be able to truncate \
             the WAL even though the base Operation is still alive"
        );

        // The base Operation returned by finish_selection is still usable
        // (this is what a real push/fetch/pull continues to hold on its way
        // into mutating reconciliation) -- its continued liveness is
        // exactly what must not keep the desired pin alive.
        assert!(std::ptr::eq(op.repo(), &raw const repo));
    }

    /// `finish_selection` must return the same
    /// base `Operation` that was passed into `DesiredOperation::new` (by
    /// value, not a fresh one), while consuming `self` so the
    /// `DesiredSnapshot` cannot be reached afterwards -- selection is over.
    /// (The stronger claim that the underlying desired-state `SQLite`
    /// read/WAL pin is actually released at this point, not merely at
    /// command end, is exercised by
    /// `finish_selection_releases_the_desired_wal_pin_before_mutating_reconciliation`
    /// above.)
    #[test]
    fn finish_selection_returns_the_same_operation_and_drops_the_desired_snapshot() {
        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let (snapshot, desired) = super::super::repo_snapshot::recover_and_open_coherent_snapshot(
            &repo,
            &gat_core::progress::NoopProgress,
        )
        .unwrap();
        let session = crate::session::Session::for_test();
        let operation = Operation::new(&repo, snapshot, session);
        let desired_op = DesiredOperation::new(operation, desired);

        let finished = desired_op.finish_selection();
        assert!(std::ptr::eq(finished.repo(), &raw const repo));
    }

    /// Config is snapshot-isolated for the whole
    /// lifetime of one already-acquired `DesiredOperation`/`Operation`. A
    /// `gat config` edit landing after acquisition must not retroactively
    /// rewrite the already-captured config -- only the *next*, newly
    /// acquired operation observes it.
    #[test]
    fn config_edit_after_acquisition_does_not_rewrite_the_captured_config() {
        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let desired_op =
            DesiredOperation::acquire(&repo, &gat_core::progress::NoopProgress).unwrap();
        assert_eq!(desired_op.operation().config().sync.trust_state, None);

        let mut config = repo
            .load_config_scoped(gat_core::config::ConfigScope::Project)
            .unwrap();
        config.sync.trust_state = Some(true);
        repo.save_config_scoped(&config, gat_core::config::ConfigScope::Project)
            .unwrap();

        // The already-acquired operation's captured config is untouched.
        assert_eq!(desired_op.operation().config().sync.trust_state, None);

        // A newly acquired operation observes the edit.
        let desired_op2 =
            DesiredOperation::acquire(&repo, &gat_core::progress::NoopProgress).unwrap();
        assert_eq!(
            desired_op2.operation().config().sync.trust_state,
            Some(true)
        );
    }

    /// The same snapshot-isolation contract
    /// `config_edit_after_acquisition_does_not_rewrite_the_captured_config`
    /// proves for `Project`-scope edits must hold for `Local`-scope edits
    /// too (`<repo_root>/.gat/gat.yaml`) -- `Operation`/`Snapshot` capture
    /// one already-merged `Config` value with no retained notion of which
    /// scope contributed which field, so isolation cannot depend on which
    /// scope changed.
    ///
    /// `Global` scope (`~/.gat/gat.yaml`) is not separately exercised
    /// in-process here: it resolves through the ambient `$HOME`/
    /// `%USERPROFILE%` environment variable
    /// ([`crate::repository::Repository::global_config_dir`]), and mutating that
    /// process-wide environment variable from an in-process unit test
    /// would race every other test in this crate's multi-threaded test
    /// binary. The crate's `Global`-scope isolation coverage instead lives
    /// at the CLI/subprocess level, where each test process gets its own
    /// real `HOME` (see
    /// `cli_integration::release_artifact::release_artifact_ordinary_commands_ignore_a_conflicting_global_config_and_cache_dir_elsewhere`).
    /// The isolation mechanism under test here is scope-agnostic by
    /// construction -- `Repository::load_config` merges all three scopes into
    /// one `Config` value before `Operation`/`Snapshot` ever see it, and
    /// neither retains a per-scope fingerprint -- so this Project+Local
    /// coverage already exercises the same code path a concurrent
    /// `Global`-scope edit would.
    #[test]
    fn config_edit_at_local_scope_after_acquisition_does_not_rewrite_the_captured_config() {
        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let desired_op =
            DesiredOperation::acquire(&repo, &gat_core::progress::NoopProgress).unwrap();
        assert_eq!(desired_op.operation().config().sync.trust_state, None);

        let mut config = repo
            .load_config_scoped(gat_core::config::ConfigScope::Local)
            .unwrap();
        config.sync.trust_state = Some(true);
        repo.save_config_scoped(&config, gat_core::config::ConfigScope::Local)
            .unwrap();

        // The already-acquired operation's captured config is untouched.
        assert_eq!(desired_op.operation().config().sync.trust_state, None);

        // A newly acquired operation observes the edit.
        let desired_op2 =
            DesiredOperation::acquire(&repo, &gat_core::progress::NoopProgress).unwrap();
        assert_eq!(
            desired_op2.operation().config().sync.trust_state,
            Some(true)
        );
    }
}
