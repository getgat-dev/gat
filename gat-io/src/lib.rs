//! Physical I/O capabilities for `gat`.
//!
//! `gat-io` owns everything that touches the filesystem, a real Git
//! repository, `SQLite`, or a remote object store: atomic file publication,
//! the repository-wide advisory lock, repository discovery/layout, and
//! (as later increments land) config-file persistence, `gat.lock`
//! persistence, local content-addressed storage, Git plumbing, and remote
//! transfer. It depends only on `gat-core` among workspace crates -- no
//! `gat-engine`, `gat-command`, or root `gat` types ever appear here.
//!
//! Implementation namespaces stay private; callers use crate-root
//! capabilities such as [`LockStore`], [`ConfigStore`], and [`StateStore`].
//!
//! Atomic publication, environment discovery, and transaction journals follow
//! the same boundary.
//!
//! ```
//! fn accepts_atomic(_: &gat_io::AtomicError) {}
//! fn accepts_journal(_: &gat_io::MountJournal) {}
//! let inputs = gat_io::InvocationInputs::from_pairs([] as [(&str, &str); 0]).unwrap();
//! assert!(inputs.home().is_none());
//! ```
//!
//! ```compile_fail
//! use gat_io::atomic::AtomicError;
//! ```
//!
//! ```compile_fail
//! use gat_io::env::InvocationInputs;
//! ```
//!
//! ```compile_fail
//! use gat_io::journal::mount::MountJournal;
//! ```
//!
//! ```compile_fail
//! use gat_io::lock::LockStore;
//! ```
//!
//! Repository lock and desired-state persistence is bound to an opaque
//! [`RepositoryLayout`]; callers cannot substitute unrelated root, database,
//! or synchronization-lock paths.
//!
//! ```compile_fail
//! let layout = gat_io::RepositoryLayout::at(std::path::PathBuf::from("."));
//! let _ = &layout.root;
//! ```
//!
//! ```compile_fail
//! let layout = gat_io::RepositoryLayout::at(std::path::PathBuf::from("."));
//! let _ = layout.root();
//! ```
//!
//! ```compile_fail
//! let layout = gat_io::RepositoryLayout::at(std::path::PathBuf::from("."));
//! let _ = layout.cache_root();
//! ```
//!
//! ```compile_fail
//! let layout = gat_io::RepositoryLayout::at(std::path::PathBuf::from("."));
//! let _ = layout.materialized_db_path();
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::LockStore::load_repository(std::path::Path::new("."));
//! ```
//!
//! Raw lock-shape selection is an I/O/state-store cooperation detail rather
//! than a public persistence protocol.
//!
//! ```compile_fail
//! let levels = gat_core::lock::LockShardLevels::new(0).unwrap();
//! let _ = gat_io::LockStore::acquire_matching_shape(
//!     std::path::Path::new("."),
//!     std::path::Path::new(".gat/state/sync.lock"),
//!     levels,
//! );
//! ```
//!
//! ```compile_fail
//! let levels = gat_core::lock::LockShardLevels::new(0).unwrap();
//! let _ = gat_io::LockStore::acquire_current_or_target_shape(
//!     std::path::Path::new("."),
//!     std::path::Path::new(".gat/state/sync.lock"),
//!     levels,
//! );
//! ```
//!
//! Test instrumentation is repository-layout-bound too:
//!
//! ```compile_fail
//! let _ = gat_io::lock_test_support::shard_inodes(std::path::Path::new("."));
//! ```
//!
//! ```compile_fail
//! let levels = gat_core::lock::LockShardLevels::new(0).unwrap();
//! let _ = gat_io::lock_test_support::acquire_matching_shape(
//!     std::path::Path::new("."),
//!     levels,
//! );
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::state_reset_database_for_test(
//!     std::path::Path::new(".gat/state/state.sqlite3"),
//! );
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::RepoLock::acquire_repository(std::path::Path::new("."));
//! ```
//!
//! ```compile_fail
//! let layout = gat_io::RepositoryLayout::at(std::path::PathBuf::from("."));
//! let _ = layout.sync_lock_path();
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::RepoLock::acquire(std::path::Path::new("sync.lock"));
//! ```
//!
//! Physical lock publication evidence is not part of the public API.
//!
//! ```compile_fail
//! use gat_io::lock::FullLockEvidence;
//! ```
//!
//! Configuration persistence follows the same boundary: callers use the
//! crate-root capability, not its implementation module.
//!
//! ```
//! let _store = gat_io::ConfigStore;
//! ```
//!
//! Repository-owned config paths are resolved only inside `gat-io`.
//!
//! ```compile_fail
//! let layout = gat_io::RepositoryLayout::at(std::path::PathBuf::from("."));
//! let _ = layout.config_path();
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::ConfigStore::load_scope(
//!     std::path::Path::new("."),
//!     gat_core::config::ConfigScope::Project,
//!     None,
//! );
//! ```
//!
//! ```compile_fail
//! use gat_io::config::ConfigStore;
//! ```
//!
//! `SQLite` persistence follows the same boundary. Its cursors, statements,
//! and transactions stay behind [`StateStore`] methods.
//!
//! ```compile_fail
//! use gat_io::state::MaterializedStore;
//! ```
//!
//! ```
//! fn accepts_state_store(_: &gat_io::StateStore) {}
//! ```
//!
//! The historical implementation name is not a second public state-store
//! surface.
//!
//! ```compile_fail
//! let _ = gat_io::MaterializedStore::open;
//! ```
//!
//! State stores accept the I/O-owned repository layout directly; the retired
//! generic path-provider bridge is not part of the public API.
//!
//! ```compile_fail
//! use gat_io::StateDatabasePath;
//! ```
//!
//! The SQLite-backed pull cursors and write transaction are implementation
//! details and cannot be named by callers.
//!
//! ```compile_fail
//! use gat_io::state::{DesiredRows, DesiredStateWrite, MaterializedRows};
//! ```
//!
//! Local object persistence follows the same pattern. Ordinary use,
//! proof-free presence checks, worker-side ingestion, and destructive
//! maintenance are separate capabilities.
//!
//! ```
//! fn accepts_cache_root(_: &gat_io::CacheRoot) {}
//! fn accepts_cache(_: &gat_io::CacheClient) {}
//! fn accepts_writer(_: &gat_io::CacheWriter) {}
//! fn accepts_maintenance(_: &gat_io::CacheMaintenance) {}
//! ```
//!
//! ```compile_fail
//! use gat_io::cache::CacheClient;
//! ```
//!
//! Resolved cache fields and raw-path constructors are not public.
//!
//! ```compile_fail
//! fn inspect(root: &gat_io::CacheRoot) {
//!     let _ = &root.objects_dir;
//! }
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::CacheClient::open(std::path::PathBuf::from("objects"));
//! ```
//!
//! Test fixtures also resolve the capability from a repository layout rather
//! than reconstructing a cache root or importing raw storage helpers.
//!
//! ```compile_fail
//! let _ = gat_io::cache_root_for_test(
//!     std::path::PathBuf::from("objects"),
//!     std::path::PathBuf::from("repository"),
//! );
//! ```
//!
//! ```compile_fail
//! use gat_io::cache_test_support;
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::cache_path_oid_for_test(
//!     std::path::Path::new("objects"),
//!     &gat_core::oid::Oid::from_bytes([0; 32]),
//! );
//! ```
//!
//! Presence-only callers use [`CacheRoot::presence`] rather than activating a
//! proof-index client.
//!
//! ```compile_fail
//! fn inspect(client: &gat_io::CacheClient, oid: &gat_core::oid::Oid) {
//!     let _ = client.contains(oid);
//! }
//! ```
//!
//! Cache layout, proof databases, and `SQLite` ownership remain
//! implementation details.
//!
//! ```compile_fail
//! use gat_io::cache::maintenance::inspect_database;
//! use gat_io::cache::proof::CacheState;
//! ```
//!
//! Repository-local maintenance and journal capabilities derive their
//! physical paths from one repository layout.
//!
//! ```compile_fail
//! let _ = gat_io::MountJournal::new(std::path::PathBuf::from(".gat"));
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::inspect_database(std::path::Path::new("state.sqlite3"));
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::StateStore::open_at(std::path::Path::new("state.sqlite3"));
//! ```
//!
//! Remote storage follows the same opaque-capability boundary. `OpenDAL`
//! operators, backend configuration, and object-store implementation modules
//! cannot be named by callers.
//!
//! ```
//! fn accepts_remote(_: &gat_io::RemoteClient) {}
//! ```
//!
//! Remote construction is coordinated by `gat-engine` through its remote session;
//! workspace boundary checks enforce that production call-site policy.
//!
//! ```compile_fail
//! use gat_io::remote::RemoteClient;
//! ```
//!
//! ```compile_fail
//! use gat_io::remote::{RemoteError, RemoteObjectLister};
//! ```
//!
//! Git access is likewise exposed through opaque crate-root capabilities;
//! Gix-backed implementation namespaces and conversion helpers stay private.
//!
//! ```
//! fn accepts_git_reader(_: &gat_io::GitReader) {}
//! fn accepts_git_snapshot(_: &gat_io::LockSnapshot) {}
//! ```
//!
//! Repository-local Git operations are bound to one [`RepositoryLayout`].
//! Arbitrary paths remain valid only for protocols that genuinely operate on
//! external documents, such as merge-driver stages.
//!
//! ```compile_fail
//! let _ = gat_io::GitReader::open(std::path::Path::new("."));
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::GitDiscovery::open(std::path::Path::new("."));
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::GitIntegration::open(std::path::Path::new("."));
//! ```
//!
//! ```compile_fail
//! fn expose_path(integration: &gat_io::GitIntegration) {
//!     let _ = integration.info_exclude_path();
//! }
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::GatIgnore::load(std::path::Path::new("."));
//! ```
//!
//! ```compile_fail
//! let revision = gat_core::git::GitRevisionSpec::from("HEAD");
//! let _ = gat_io::resolve_commit(std::path::Path::new("."), &revision);
//! ```
//!
//! ```compile_fail
//! let _ = gat_io::LockSnapshot::staged(std::path::Path::new("."));
//! ```
//!
//! ```compile_fail
//! use gat_io::git::GitReader;
//! ```
//!
//! ```compile_fail
//! use gat_io::git::history::ResolvedHistory;
//! ```
//!
//! Prepared Git worktrees expose semantic operations without revealing their
//! physical roots.
//!
//! ```compile_fail
//! fn inspect(prepared: &gat_io::PreparedGitWorktree) {
//!     let _ = prepared.root();
//! }
//! ```
//!
//! Temporary bare repositories similarly expose history semantics without
//! their clone directory or Gix reader.
//!
//! ```
//! fn accepts_bare(_: &gat_io::PreparedBareGitRepository) {}
//! ```
//!
//! ```compile_fail
//! fn inspect(prepared: &gat_io::PreparedBareGitRepository) {
//!     let _ = prepared._temporary;
//!     let _ = prepared.reader;
//! }
//! ```
//!
//! Working-tree filesystem access uses a repository-bound capability. Path
//! resolution, stat proofs, cache paths, and deletion receipts remain private.
//!
//! ```
//! fn accepts_worktree(_: gat_io::WorktreeClient<'_>) {}
//! ```
//!
//! Raw-path construction is private; repository layouts create worktree
//! capabilities without exposing their physical root through the capability.
//!
//! ```compile_fail
//! let _ = gat_io::WorktreeClient::new(std::path::Path::new("."));
//! ```
//!
//! ```
//! let layout = gat_io::RepositoryLayout::at(std::path::PathBuf::from("."));
//! let _worktree = layout.worktree_client();
//! ```
//!
//! ```compile_fail
//! use gat_io::worktree::WorktreeClient;
//! ```
//!
//! ```compile_fail
//! use gat_io::worktree::{WorktreeFileStatus, WorktreeIngested};
//! ```
//!
//! Physical proof and removal evidence cannot be inspected by callers.
//!
//! ```compile_fail
//! fn inspect(receipt: gat_io::RemovalReceipt, status: gat_io::WorktreeFileStatus) {
//!     let _ = receipt.path;
//!     let _ = status.proof_refresh;
//! }
//! ```
//!
//! File observations and exclude publication receipts are opaque as well.
//!
//! ```compile_fail
//! fn inspect(proof: &gat_io::file_state::StatProof) {
//!     let _ = proof.size;
//! }
//! ```
//!
//! ```compile_fail
//! fn inspect(published: gat_io::atomic::PublishedFile) {
//!     let _ = published.proof;
//! }
//! ```
//!
//! ```compile_fail
//! fn inspect(snapshot: gat_io::InfoExcludeSnapshot, update: gat_io::InfoExcludeUpdate) {
//!     let _ = snapshot.proof();
//!     let _ = update.proof();
//! }
//! ```
//!
//! ```compile_fail
//! fn inspect(store: &gat_io::StateStore) {
//!     let record = store.exclude_record().unwrap();
//!     let _ = record.proof;
//! }
//! ```
//!
//! Proof-bearing materialized rows and ordered state mutations are opaque
//! transport values. Callers may inspect semantic row identity, but cannot
//! construct or decompose physical proof evidence.
//!
//! ```compile_fail
//! fn inspect(row: gat_io::MaterializedRow) {
//!     let _ = row.proof;
//! }
//! ```
//!
//! ```compile_fail
//! fn construct(path: gat_core::lexical_path::GatPath, oid: gat_core::oid::Oid) {
//!     let _ = gat_io::MaterializedRow { path, oid, proof: None };
//! }
//! ```
//!
//! ```compile_fail
//! fn decompose(mutation: gat_io::StateMutation) {
//!     let gat_io::StateMutation::RefreshStat { path: _, proof: _ } = mutation;
//! }
//! ```
//!
//! ```compile_fail
//! use gat_io::StoredShard;
//! ```
//!
//! Lock/state accelerator cooperation is exposed only through the state
//! capability; callers cannot invoke the retired lock-store coordinator.
//!
//! ```compile_fail
//! fn observe(root: &std::path::Path, layout: &gat_io::RepositoryLayout) {
//!     let _ = gat_io::LockStore::observe_canonical_identity(root, layout);
//! }
//! ```

mod atomic;
mod cache;
mod config;
mod env;
mod file_state;
mod git;
mod journal;
mod local_directory;
mod lock;
mod remote;
mod repository_layout;
mod state;
mod worktree;

pub use atomic::{AtomicError, RepoLock, write_atomic, write_atomic_if_absent};
pub use cache::CacheRoot;
pub use cache::{
    CacheClient, CacheDatabaseHealth, CacheDatabaseUnreadable, CacheEnumerationError, CacheError,
    CacheIngest, CacheMaintenance, CacheMaintenanceError, CacheObject, CacheObjectOpenError,
    CacheObjectOpenStage, CacheObjectReader, CachePresence, CacheProofError, CacheProofErrorKind,
    CachePublication, CacheSweepDecision, CacheSweepStats, CacheVerificationFailure, CacheWriter,
    CompletedCacheVerification, DEFAULT_INGEST_STRATEGY, ExpectedIngest, IngestStrategy, Ingested,
    OBJECT_HASH_NAMESPACE, ObjectVerification, PreparedCacheVerification, VERIFY_WINDOW,
    object_key_oid, parse_object_key,
};
pub use config::{
    CONFIG_VERSION, ConfigError, ConfigStore, ConfigWriteError, ScopedConfigError,
    ScopedConfigWriteError,
};
pub use env::{
    EnvironmentName, InputValueReason, InvocationInputError, InvocationInputs, TemplateResolver,
};
pub use file_state::FileStateError;
pub use git::{
    AddExclusion, AddExclusionReason, GatIgnore, GatIgnoreError, GitCloneError, GitCloneErrorKind,
    GitDiscovery, GitDiscoveryError, GitDiscoveryErrorKind, GitHistoryError, GitHistoryErrorKind,
    GitIgnoreMatcher, GitIntegration, GitIntegrationError, GitIntegrationErrorKind,
    GitIntegrationStatus, GitLocation, GitLocationError, GitLocationKind, GitOpenError,
    GitPathStatus, GitReader, HistoryStats, InfoExcludeError, InfoExcludeMutation,
    InfoExcludeSnapshot, InfoExcludeUpdate, InfoExcludeVerification, LockSnapshot,
    LockSnapshotError, LockSnapshotErrorKind, PrepareBareGitRepositoryError,
    PrepareGitWorktreeError, PreparedBareGitRepository, PreparedGitWorktree, ResolveCommitError,
    ResolveCommitErrorKind, SnapshotShard, mutate_info_exclude, parse_cli_date, parse_location,
    prepare_bare_repository, prepare_worktree, read_info_exclude, resolve_commit,
};
pub use journal::mount::{
    MOUNT_TXN_VERSION, MountJournal, MountJournalError, MountJournalValidationError,
    MountTxnChange, MountTxnPhase, MountTxnRecord, StagedRow, StagedWindows,
};
pub use lock::{
    CandidateInvalidReason, CompletedLockReshape, InvalidOidReason, LiveLockInvalidReason,
    LiveLockState, LockDomainError, LockError, LockMaintenanceState, LockStore, LockWriteGuard,
    MalformedRowReason, PendingLockReshape, PersistenceError, PreparedReshapeState,
    PreparedReshapeStatus, RecoveryCandidateOutcome, RecoveryCandidateState, ReshapeRecoveryChoice,
    ReshapeTransactionKind, ReshapeTransactionState, TransactionMalformedReason,
};
pub use remote::{
    AsyncRemoteWriter, DOWNLOAD_BUFFER_BYTES, FileDeleteBatch, FileDeleteOutcome, FileGc,
    FileObjectScan, FileObjectWriter, FilePublication, FileReceiveError, FileUploadError,
    FileWriteError, FileWritePhase, InterpolateError, OpenRemoteError, PreparedFilePresence,
    PreparedFileRead, PreparedFileWrite, PreparedRemoteWrite, RemoteBackendError, RemoteClient,
    RemoteError, RemoteObject, RemoteObjectLister, RemoteRead, RemoteRequestBudget,
    STREAM_BUFFER_SIZE, TRANSFER_CHUNK_SIZE, initialize_backends, with_stream_buffer,
};
pub use repository_layout::{LayoutError, RepositoryLayout};

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use git::{
    info_exclude_test_support as git_info_exclude_test_support,
    lock_snapshot_test_support as git_lock_snapshot_test_support, test_support as git_test_support,
};
pub use state::{
    AddCandidateDiscoveryError, DesiredCandidateScope, DesiredMutationOpenError,
    DesiredMutationSession, DesiredPathExclusions, DesiredPublicationError, DesiredQuery,
    DesiredRefresh, DesiredRefreshError, DesiredRemoval, DesiredRow, DesiredStateOpenError,
    DesiredStateSession, DirtyRow, MaterializationPreparationError, MaterializedRow,
    MountMutationSession, MountReplayResult, PreparedMaterialization, StateDatabaseHealth,
    StateDatabaseUnreadable, StateMaintenanceError, StateMutation, StateSqlError,
    StateSqlErrorKind, StateStore, StateStoreError, count_stale_sidecars, inspect_database,
    rebuild_atomically, remove_stale_sidecars,
};
pub use worktree::{
    DestinationKind as WorktreeDestinationKind, EntryKind as WorktreeEntryKind, MaterializeKind,
    MovePathError, PruneError, RemovalReceipt, RemovePathError, RollbackMoveError, WorktreeClient,
    WorktreeFileStatus, WorktreeMutationError, WorktreePathError, WorktreeStatusKind,
};

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use worktree::test_support as worktree_test_support;

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use remote::file_url as remote_file_url_for_test;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use remote::test_support as remote_test_support;

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use state::{
    create_stale_sidecar_for_test as state_create_stale_sidecar_for_test,
    current_schema_version_for_test as state_current_schema_version_for_test,
    load_materialized_for_test as state_load_materialized_for_test,
    materialized_row_has_proof_for_test as state_materialized_row_has_proof_for_test,
    materialized_row_proof_matches_path_for_test as state_materialized_row_proof_matches_path_for_test,
    record_materialized_for_test as state_record_materialized_for_test,
    record_materialized_unlocked_for_test as state_record_materialized_unlocked_for_test,
    repair_temp_count_for_test as state_repair_temp_count_for_test,
    reset_database_for_test as state_reset_database_for_test,
    set_schema_version_for_test as state_set_schema_version_for_test,
    shard_ids_for_test as state_shard_ids_for_test,
    shard_observation_for_test as state_shard_observation_for_test,
    test_support as state_test_support,
};

/// Result of an ordinary local cache operation.
pub type CacheResult<T> = cache::Result<T>;

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use lock::{
    desired_identity_from_shards as lock_desired_identity_from_shards,
    flat_publish_test_support as lock_flat_publish_test_support,
    hash_shard_bytes as lock_hash_shard_bytes, identity_test_support as lock_identity_test_support,
    race_test_hooks as lock_race_test_hooks,
    simulate_crash_after_first_rename as simulate_lock_crash_after_first_rename,
    test_support as lock_test_support,
};

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use cache::{
    enumeration_test_support as cache_enumeration_test_support, hash_file_call_count,
    hash_file_call_count as cache_hash_file_call_count,
    object_test_support as cache_object_test_support,
    proof_test_support as cache_proof_test_support, race_test_hooks as cache_race_test_hooks,
    reset_sync_all_call_count as cache_reset_sync_all_call_count,
    sync_all_call_count as cache_sync_all_call_count, with_exclusive_hash_file_call_count,
    with_exclusive_hash_file_call_count as cache_with_exclusive_hash_file_call_count,
};

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod cache_benchmark_support {
    pub use crate::cache::object::{ingest, ingest_file};
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod file_state_test_support {
    pub use crate::file_state::test_support::*;
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod atomic_test_support {
    pub use crate::atomic::test_support::*;
    pub use crate::atomic::{RepoLock, write_atomic};
}

pub use config::ConfigRevision;
