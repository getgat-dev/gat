//! `gat.lock` on-disk persistence: flat/sharded
//! shape detection, full and incremental writes, atomic file publication,
//! stat-first canonical desired-state identity observation, and
//! crash-safe reshape/recovery for the `gat.lock`/`gat.lock/` shard-
//! directory layout.
//!
//! `Entry`/`Lock`, semantic mutations, merge behavior, and pure errors live
//! in `gat_core::lock`. This module exposes opaque I/O capabilities and owns
//! physical persistence, reshape, observation, and errors with filesystem
//! context. Validated codec cooperation remains crate-private.

mod error;

pub use error::{
    InvalidOidReason, LockDomainError, LockError, MalformedRowReason, PersistenceError, Result,
};

#[cfg(test)]
pub(crate) use gat_core::lock::validated::visit_filtered_matching;
pub(crate) use gat_core::lock::validated::{
    check_ordered_row_conflict, entry_from_validated_parts, parse_row,
    validate_no_path_directory_conflicts, visit_rows_validated,
};
pub(crate) use gat_core::lock::{Entry, Lock, VERSION};

mod identity;
mod maintenance;
mod observation;
mod persistence;

pub use gat_core::lock::{
    CanonicalDesiredIdentity, LockShardId, LockShardLevels, ShardContentIdentity,
};
#[cfg(any(test, feature = "test-support"))]
pub use identity::hash_shard_bytes;
#[cfg(test)]
pub(crate) use identity::resolve_shard_identity;
pub(crate) use identity::{
    ShardIdentityResolution, current_desired_identity, current_desired_identity_with_prior,
};
pub use maintenance::{
    CandidateInvalidReason, LiveLockInvalidReason, LiveLockState, LockMaintenanceState,
    PreparedReshapeState, PreparedReshapeStatus, RecoveryCandidateOutcome, RecoveryCandidateState,
    ReshapeTransactionKind, ReshapeTransactionState, TransactionMalformedReason,
};
pub(crate) use observation::{ShardObservation, ShardObservationChange};
// Cross-crate test-only seam (gated by the `test-support` Cargo feature,
// activated only via the root `gat` crate's dev-dependency on `gat-io`):
// the sequential, reference-implementation XOR fold used both by
// `identity`'s own algebra tests and by `storage::state`'s
// incremental-vs-from-scratch regression tests to cross-check the
// production parallel reduction (`current_desired_identity_with_prior`)
// and the incremental sparse-toggle path -- never called from production
// code.
#[cfg(any(test, feature = "test-support"))]
pub use identity::desired_identity_from_shards;
#[cfg(any(test, feature = "test-support"))]
pub use identity::race_test_hooks;
#[cfg(any(test, feature = "test-support"))]
pub use identity::test_support as identity_test_support;

pub(crate) use persistence::ShardEvidence;
#[cfg(any(test, feature = "test-support"))]
pub use persistence::simulate_crash_after_first_rename;
#[cfg(any(test, feature = "test-support"))]
pub use persistence::test_support as flat_publish_test_support;
pub use persistence::{CompletedLockReshape, LockWriteGuard, PendingLockReshape};
pub use persistence::{
    FullLockEvidence, ReshapeRecoveryChoice, shard_id_for_path, shard_levels_for_ids,
    shard_levels_from_id,
};

/// The public lock-persistence façade: a
/// stateless, zero-sized type whose associated functions are the sole
/// entry points a caller outside this module needs, mirroring
/// [`crate::config::ConfigStore`]'s precedent. Every associated function
/// here is a thin, zero-cost forward to the (module-private) free
/// function of the same name in `persistence` -- kept as free functions
/// internally purely so this module's own extensive test suite can keep
/// calling them directly without an extra `LockStore::` qualifier on
/// every call.
#[derive(Debug, Clone, Copy)]
pub struct LockStore;

impl LockStore {
    /// Load and fully validate the logical lock, independent of its
    /// physical flat or sharded representation.
    pub fn load_repository(layout: &crate::RepositoryLayout) -> Result<Lock> {
        persistence::load(layout.root_path())
    }

    pub(crate) fn load_all(root: &std::path::Path) -> Result<Lock> {
        persistence::load(root)
    }

    /// Load and fully validate one self-contained lock document at an
    /// arbitrary path without repository shape discovery.
    pub fn load_file(path: &std::path::Path) -> Result<Lock> {
        persistence::load_file(path)
    }

    /// Read one arbitrary lock document without parsing it. Returns `None`
    /// when the file is absent and performs no separate existence probe.
    pub fn read_file_if_present(path: &std::path::Path) -> Result<Option<String>> {
        persistence::read_file_if_present(path)
    }

    /// Atomically publish one self-contained lock document at an arbitrary
    /// path, such as the `%A` file supplied by Git's merge-driver protocol.
    pub fn publish_file_atomic(lock: &Lock, path: &std::path::Path) -> Result<()> {
        persistence::save_file_atomic(lock, path)
    }

    /// Stream selected typed rows while preserving whole-lock validation.
    pub fn visit_repository(
        layout: &crate::RepositoryLayout,
        exact_path: Option<&gat_core::lexical_path::GatPath>,
        keep: impl Fn(&str) -> bool,
        visit: impl FnMut(&Entry) -> Result<()>,
    ) -> Result<()> {
        persistence::visit_lock_rows_validated(
            layout.root_path(),
            exact_path.map(gat_core::lexical_path::GatPath::as_str),
            keep,
            visit,
        )
    }

    pub(crate) fn visit_selected(
        root: &std::path::Path,
        exact_path: Option<&gat_core::lexical_path::GatPath>,
        keep: impl Fn(&str) -> bool,
        visit: impl FnMut(&Entry) -> Result<()>,
    ) -> Result<()> {
        persistence::visit_lock_rows_validated(
            root,
            exact_path.map(gat_core::lexical_path::GatPath::as_str),
            keep,
            visit,
        )
    }

    /// Publish the complete logical lock at the configured shard depth.
    /// Physical shape selection and any required reshape remain internal.
    pub fn publish_repository(
        layout: &crate::RepositoryLayout,
        lock: &Lock,
        shard_levels: LockShardLevels,
    ) -> Result<()> {
        persistence::publish_complete(layout.root_path(), lock, shard_levels)
    }

    /// As [`Self::publish_repository`], returning an opaque receipt for the
    /// I/O-owned desired-state mirror to consume without rereading files.
    pub(crate) fn publish_complete_with_evidence(
        root: &std::path::Path,
        lock: &Lock,
        shard_levels: LockShardLevels,
    ) -> Result<FullLockEvidence> {
        persistence::publish_complete_with_evidence(root, lock, shard_levels)
    }

    /// Prepare a reshape when the current physical representation differs
    /// from `target`, without exposing the observed shape or transaction
    /// paths. The common missing/matching path acquires no repository lock
    /// and performs no complete lock load.
    pub fn begin_repository_reshape(
        layout: &crate::RepositoryLayout,
        target: LockShardLevels,
    ) -> Result<Option<PendingLockReshape>> {
        persistence::begin_reshape(layout.root_path(), target)
    }

    /// Test-support observation of the live lock representation.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn current_repository_shard_levels(
        layout: &crate::RepositoryLayout,
    ) -> Result<Option<LockShardLevels>> {
        persistence::on_disk_shape(layout.root_path())
            .map(|shape| shape.map(persistence::OnDiskShape::shard_levels))
    }

    /// Inspect the live lock and interrupted-reshape state without exposing
    /// physical transaction paths or the on-disk shape codec.
    pub fn inspect_repository(layout: &crate::RepositoryLayout) -> Result<LockMaintenanceState> {
        maintenance::inspect(layout.root_path())
    }

    /// Recover an inspected reshape transaction by semantic ID.
    pub fn recover_repository_reshape(
        layout: &crate::RepositoryLayout,
        transaction_id: &str,
        choice: ReshapeRecoveryChoice,
    ) -> Result<()> {
        maintenance::recover(layout.root_path(), transaction_id, choice)
    }

    /// Remove reshape scratch proven safe to discard.
    pub fn clean_repository_reshape_scratch(layout: &crate::RepositoryLayout) -> Result<usize> {
        maintenance::clean(layout.root_path())
    }

    /// Observe all live lock shards against a prior semantic catalog,
    /// retaining physical shard discovery and paths inside `gat-io`.
    pub(crate) fn observe_shards(
        root: &std::path::Path,
        priors: &std::collections::HashMap<
            LockShardId,
            (ShardContentIdentity, Option<crate::file_state::StatProof>),
        >,
    ) -> Result<Vec<ShardObservation>> {
        observation::observe_shards(root, priors)
    }

    /// Observe and validate the complete live lock once, retaining the
    /// resulting physical evidence inside the I/O crate.
    pub(crate) fn observe_full_with_evidence(root: &std::path::Path) -> Result<FullLockEvidence> {
        persistence::observe_full_lock_with_evidence(root)
    }

    /// Acquire the repo-wide lock and resolve whether
    /// the sparse, touched-shard-scoped publication pipeline applies --
    /// [`LockWriteGuard::can_publish_incrementally`] is `true` only when the
    /// current on-disk shape already matches `target`. `target` is
    /// caller-supplied rather than loaded here: `gat-io` owns filesystem
    /// state, the repo-wide lock, and shape observation, while config/policy
    /// resolution stays the engine's responsibility.
    /// See [`LockWriteGuard`].
    pub(crate) fn acquire_matching_shape(
        layout: &crate::RepositoryLayout,
        target: LockShardLevels,
    ) -> Result<LockWriteGuard> {
        persistence::acquire_matching_shape(layout.root_path(), &layout.sync_lock_path(), target)
    }

    /// As [`Self::acquire_matching_shape`], but treats "nothing tracked on
    /// disk yet" as eligible for the sparse pipeline too, using the shape
    /// `target` calls for. See [`LockWriteGuard`].
    pub(crate) fn acquire_current_or_target_shape(
        layout: &crate::RepositoryLayout,
        target: LockShardLevels,
    ) -> Result<LockWriteGuard> {
        persistence::acquire_current_or_target_shape(
            layout.root_path(),
            &layout.sync_lock_path(),
            target,
        )
    }

    /// Publish the complete post-mutation rows for exactly the logical
    /// shards in `touched_shard_ids`, dispatching flat versus sharded
    /// persistence from the locked shape. Kept crate-private because the
    /// state store is the sole consumer of the physical rows, prior proofs,
    /// and publication evidence.
    pub(crate) fn publish_touched(
        root: &std::path::Path,
        shape_lock: &LockWriteGuard,
        touched_shard_ids: &std::collections::BTreeSet<LockShardId>,
        rows_by_shard: &std::collections::BTreeMap<LockShardId, Vec<Entry>>,
        priors: &std::collections::BTreeMap<
            LockShardId,
            (ShardContentIdentity, crate::file_state::StatProof),
        >,
    ) -> Result<persistence::SparseShardPublish> {
        if shape_lock.is_flat() {
            let entries = rows_by_shard
                .get(&LockShardId::flat())
                .map_or(&[][..], Vec::as_slice);
            let published = persistence::publish_flat_shard(
                root,
                entries,
                priors.get(&LockShardId::flat()).copied(),
            )?;
            Ok(match published {
                Some(shard) => (vec![shard], Vec::new()),
                None => (Vec::new(), vec![LockShardId::flat()]),
            })
        } else {
            persistence::save_sparse_shards(root, touched_shard_ids, rows_by_shard, priors)
        }
    }

    /// Stream the complete flat desired state into one publication. This is
    /// crate-private so only the `SQLite` state store can bridge its ordered
    /// cursor into the physical writer.
    pub(crate) fn publish_flat_streaming(
        root: &std::path::Path,
        next_row: impl FnMut() -> Result<Option<Entry>>,
    ) -> Result<persistence::SparseShardPublish> {
        Ok(
            match persistence::publish_flat_shard_streaming(root, next_row)? {
                Some(shard) => (vec![shard], Vec::new()),
                None => (Vec::new(), vec![LockShardId::flat()]),
            },
        )
    }
}

/// Test-only structural instrumentation: not a timing benchmark, but a
/// counter a test can assert against to catch a regression back
/// into the whole-shard `BTreeSet`-based validator for a scoped read that
/// should be taking the bounded, ordered `FilteredRowCursor` fast path
/// instead.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    /// Acquire the production shape-selection guard for cross-layer
    /// synchronization characterization without publishing the raw-path
    /// constructor as a normal I/O capability.
    pub fn acquire_matching_shape(
        layout: &crate::RepositoryLayout,
        target: super::LockShardLevels,
    ) -> super::Result<super::LockWriteGuard> {
        super::LockStore::acquire_matching_shape(layout, target)
    }

    #[cfg(unix)]
    pub fn shard_inodes(
        layout: &crate::RepositoryLayout,
    ) -> super::Result<std::collections::HashMap<super::LockShardId, u64>> {
        use std::os::unix::fs::MetadataExt;

        super::persistence::list_shard_files(layout.root_path())?
            .into_iter()
            .map(|shard| {
                std::fs::metadata(&shard.full_path)
                    .map(|metadata| (shard.shard_id, metadata.ino()))
                    .map_err(|source| super::LockError::io("reading", &shard.full_path, source))
            })
            .collect()
    }

    thread_local! {
        static SELECTED_SHARD_PARSES: Cell<usize> = const { Cell::new(0) };
        static SHARD_TEXT_READS: Cell<usize> = const { Cell::new(0) };
        static MERGE_HEAD_COMPARISONS: Cell<usize> = const { Cell::new(0) };
        static FULL_SHARD_TEXT_READS: Cell<usize> = const { Cell::new(0) };
        static MAX_RETAINED_ORDERED_ROWS: Cell<usize> = const { Cell::new(0) };
        static RENDER_ENTRIES_CALLS: Cell<usize> = const { Cell::new(0) };
        static FULL_LOCK_EVIDENCE_SHARD_READS: Cell<usize> = const { Cell::new(0) };
        static RESHAPE_FULL_LOADS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn record_render_entries_call() {
        RENDER_ENTRIES_CALLS.with(|c| c.set(c.get() + 1));
    }

    pub fn render_entries_calls() -> usize {
        RENDER_ENTRIES_CALLS.with(Cell::get)
    }

    pub fn record_full_lock_evidence_shard_read() {
        FULL_LOCK_EVIDENCE_SHARD_READS.with(|c| c.set(c.get() + 1));
    }

    pub fn full_lock_evidence_shard_reads() -> usize {
        FULL_LOCK_EVIDENCE_SHARD_READS.with(Cell::get)
    }

    pub fn record_reshape_full_load() {
        RESHAPE_FULL_LOADS.with(|c| c.set(c.get() + 1));
    }

    pub fn reshape_full_loads() -> usize {
        RESHAPE_FULL_LOADS.with(Cell::get)
    }

    #[must_use]
    pub fn btree_validation_parses() -> usize {
        gat_core::lock::test_probes::btree_validation_parses()
    }

    pub fn record_selected_shard_parse() {
        SELECTED_SHARD_PARSES.with(|c| c.set(c.get() + 1));
    }

    pub fn selected_shard_parses() -> usize {
        SELECTED_SHARD_PARSES.with(Cell::get)
    }

    pub fn record_shard_text_read() {
        SHARD_TEXT_READS.with(|c| c.set(c.get() + 1));
    }

    pub fn shard_text_reads() -> usize {
        SHARD_TEXT_READS.with(Cell::get)
    }

    pub fn record_merge_head_comparison() {
        MERGE_HEAD_COMPARISONS.with(|c| c.set(c.get() + 1));
    }

    pub fn merge_head_comparisons() -> usize {
        MERGE_HEAD_COMPARISONS.with(Cell::get)
    }

    pub fn record_full_shard_text_read() {
        FULL_SHARD_TEXT_READS.with(|c| c.set(c.get() + 1));
    }

    pub fn full_shard_text_reads() -> usize {
        FULL_SHARD_TEXT_READS.with(Cell::get)
    }

    pub fn reset_max_retained_ordered_rows() {
        MAX_RETAINED_ORDERED_ROWS.with(|c| c.set(0));
    }

    pub fn observe_retained_ordered_rows(count: usize) {
        MAX_RETAINED_ORDERED_ROWS.with(|c| {
            if count > c.get() {
                c.set(count);
            }
        });
    }

    pub fn max_retained_ordered_rows() -> usize {
        MAX_RETAINED_ORDERED_ROWS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path.trim_end_matches('/')).unwrap()
    }

    fn oid(hex: &str) -> gat_core::oid::Oid {
        gat_core::oid::Oid::from_hex(hex).unwrap()
    }

    #[test]
    fn save_file_atomic_roundtrips_escaped_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        lock.entries.push(Entry {
            path: gp("data/fi\t\n\r\"le.bin"),
            oid: oid(&"a".repeat(64)),
        });
        let path = tmp.path().join("gat.lock");
        LockStore::publish_file_atomic(&lock, &path).unwrap();
        assert_eq!(
            Lock::parse(&std::fs::read_to_string(path).unwrap()).unwrap(),
            lock
        );
    }
}
