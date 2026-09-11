//! Stateful repository workflows and execution coordination for `gat`.
//!
//! `gat-engine` owns operation snapshots, repository mutations, worktree
//! reconciliation, and stateful cache and remote sessions. It depends on
//! `gat-core` and the opaque physical capabilities exposed by `gat-io`;
//! command orchestration and presentation remain above this crate.
//!
//! Implementation namespaces are private; callers use the deliberate
//! crate-root operation and view capabilities.
//!
//! ```compile_fail
//! use gat_engine::operation::Operation;
//! ```
//!
//! ```compile_fail
//! use gat_engine::desired_snapshot::DesiredView;
//! ```
//!
//! ```compile_fail
//! use gat_engine::cache_session::CacheSession;
//! ```
//!
//! ```compile_fail
//! use gat_engine::mutation::MutationGuard;
//! ```
//!
//! ```compile_fail
//! use gat_engine::repo_snapshot::recover_and_open_coherent_snapshot;
//! ```
//!
//! ```compile_fail
//! use gat_engine::repository::Repository;
//! ```
//!
//! ```compile_fail
//! use gat_engine::Repo;
//! ```
//!
//! Managed Git exclusions are maintained through repository mutation and
//! maintenance capabilities; physical helpers and their errors stay internal.
//!
//! ```compile_fail
//! use gat_engine::sync_excludes_from_lock;
//! ```
//!
//! ```compile_fail
//! use gat_engine::ExcludesError;
//! ```
//!
//! Route identities stay inside the compiled policy; reports borrow the
//! selecting route's name through `EffectivePathPolicy::route_name`.
//!
//! ```compile_fail
//! use gat_engine::RouteId;
//! ```
//!
//! ```compile_fail
//! use gat_engine::ResolvedRemote;
//! fn route(remote: ResolvedRemote) {
//!     let _ = remote.route();
//! }
//! ```
//!
//! ```compile_fail
//! let repository = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap().repository_at(std::path::PathBuf::from("."));
//! let _ = gat_io::StateStore::open(&repository);
//! ```
//!
//! ```compile_fail
//! let repository = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap().repository_at(std::path::PathBuf::from("."));
//! let _: &gat_io::RepositoryLayout = &repository;
//! ```
//!
//! ```compile_fail
//! let repository = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap().repository_at(std::path::PathBuf::from("."));
//! let _ = repository.materialized_db_path();
//! ```
//!
//! ```compile_fail
//! let repository = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap().repository_at(std::path::PathBuf::from("."));
//! let _ = repository.sync_lock_path();
//! ```
//!
//! The command-facing repository façade does not expose its physical root.
//!
//! ```compile_fail
//! let repository = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap().repository_at(std::path::PathBuf::from("."));
//! let _ = repository.root();
//! ```
//!
//! ```compile_fail
//! let repository = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap().repository_at(std::path::PathBuf::from("."));
//! let _ = repository.config_path_for(gat_core::config::ConfigScope::Project);
//! ```
//!
//! Raw remote URLs are not a repository-facade result. Commands use typed
//! remote outcomes, while operations resolve typed catalogs and sessions.
//!
//! ```compile_fail
//! let repository = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap().repository_at(std::path::PathBuf::from("."));
//! let _ = repository.remote_url_named(None);
//! ```
//!
//! Standalone configuration documents are exposed only through the deliberate
//! engine boundary used by development tooling, not through an implementation
//! module.
//!
//! ```compile_fail
//! use gat_engine::config_file::load;
//! ```
//!
//! ```compile_fail
//! use gat_engine::session::Session;
//! ```
//!
//! ```compile_fail
//! use gat_engine::snapshot::Snapshot;
//! ```
//!
//! ```compile_fail
//! fn bypass_mutation_service(operation: &mut gat_engine::Operation<'_>) {
//!     let _ = operation.mutate();
//! }
//! ```

mod cache_presence;
mod cache_session;
mod compare;
mod config_file;
mod desired_operation;
mod desired_snapshot;
mod excludes;
mod gc;
mod git_location;
mod history;
mod initialization;
mod limits;
mod maintenance;
mod merge_driver;
mod mount;
mod mutation;
mod operation;
mod path_policy;
mod remote_catalog;
mod remote_executor;
pub use remote_executor::TransferCancellation;
mod remote_open;
mod remote_session;
mod repo_snapshot;
mod repository;
mod repository_access;
mod repository_mutation;
mod repository_state;
mod session;
mod snapshot;
mod transfer;
mod workspace;
mod worktree;

pub use cache_presence::CachePresenceSession;
pub use compare::{
    ChangedRow, CompareError, CompareErrorKind, ComparisonService, CurrentComparison, RowChange,
    Unchanged,
};
pub use config_file::{
    ConfigFileLoadError, ConfigFileSaveError, load as load_config_file, save as save_config_file,
};
pub use desired_operation::DesiredOperation;
pub use desired_snapshot::DesiredView;
pub use excludes::SyncStatus as ExcludesSyncStatus;
pub use gc::{
    GcError, GcFailure, GcFailureKind, GcOptions, GcReport, GcRepositoryFailureKind,
    GcRepositoryIssue,
};
pub use git_location::{GitLocationValidationError, validate_git_location};
pub use history::{
    HistoryError, HistoryErrorKind, HistoryVisitStats, ResolveCommitError, ResolveCommitErrorKind,
    parse_cli_date,
};
pub use initialization::{
    HookChanges, InitializationError, InitializationErrorKind, InitializationService,
    IntegrationStatus, ManagedHook, ResolvedCacheLocation,
};
pub use limits::{
    ExecutionLimits, GcLimits, RemoteConcurrency, RemoteGcLimits, RemoteLimits, SyncLimits,
    TransferLimits,
};
pub use maintenance::{
    CacheClean, CacheDbState, CacheInspect, CacheRepair, CandidateInvalidReason, CandidateOutcome,
    DbIntegrityFailure, DbUnreadableReason, GitClean, GitInspect, GitRepair, LiveLockInvalidReason,
    LiveLockState, LockClean, LockMaintenanceState, LockRepair, LockRepairRequest,
    MaintenanceError, MaintenanceErrorKind, MaintenanceService, PreparedTxnStatus, RecoveryChoice,
    RecoverySelectionFailure, StateClean, StateDbState, StateInspect, StateRepair,
    TemporaryCleanOutcome, TransactionKind, TransactionMalformedReason, TransactionState,
};
pub use merge_driver::{MergeDriverError, MergeStage, merge_driver};
pub use mount::{
    MountMutationOutcome, MountRowSource, MountService, MountSourceError, MountSourceErrorKind,
    MountSourceLocation, MountWorkflowError, PreparedMountSource,
};
pub use operation::Operation;
pub use path_policy::{
    EffectivePathPolicy, MountOwnership, PathPolicyError, ResolvedRemote,
    UnknownRemoteOverrideError,
};
pub use remote_catalog::{RemoteCatalog, RemoteCatalogError, RemoteId, RemoteUrlValidationError};
pub use remote_open::{RemoteOpenError, RemoteOpenFailureKind};
pub use remote_session::RemoteSessionError;
pub use repo_snapshot::{
    FilesystemFailureKind, LockFailureKind, MountRecoveryFailureKind, RepoSnapshotError,
    RepoSnapshotErrorKind, SnapshotFailureKind, StateFailureKind,
    acquire_operation_without_desired_state,
};
pub use repository::{
    CacheLocationOrigin, ConfigAccessFailureKind, ConfigLayers, RepoError, Repository,
    RepositoryError,
};
pub use repository_access::{
    FilesystemFailureKind as RepositoryAccessFilesystemFailureKind,
    LockFailureKind as RepositoryAccessLockFailureKind, RepositoryAccessError,
    RepositoryAccessFailureKind,
};
pub use repository_mutation::{
    AddCandidate, DesiredMutation, DesiredScope, DesiredState, GitPathStatus,
    MaterializationSession, MaterializedEntry, RepositoryMutationError, RepositoryStateError,
};
pub use repository_state::{
    DesiredRevision, DesiredRevisionError, StaleDesiredRevisionError, current_desired_revision,
};
pub use snapshot::SnapshotError;
pub use transfer::{
    DownloadCacheFailureKind, DownloadError, DownloadObject, DownloadOutcome,
    DownloadRemoteFailureKind, PublishError, PublishObject, PublishOutcome, PublishStatus,
    RemotePresenceError, RemotePresenceObligation, RemotePresenceResult, RepairCacheFailureKind,
    RepairError, RepairObject, RepairOutcome, RepairRemoteFailureKind, SelectedObject,
    StreamingWindow, TransferCacheSource, UploadCacheFailureKind, UploadError,
    UploadRemoteFailureKind, UploadWriteFailureKind, WindowBatch, download_window, publish_window,
    repair_window, visit_current_state_objects, visit_history_objects,
};
pub use workspace::sync::{
    CacheFailureKind as SyncCacheFailureKind, ExcludesFailureKind as SyncExcludesFailureKind,
    FileStateFailureKind as SyncFileStateFailureKind,
    FilesystemFailureKind as SyncFilesystemFailureKind, LockFailureKind as SyncLockFailureKind,
    MutationAuthorityFailureKind as SyncMutationAuthorityFailureKind, ReconciliationPolicy,
    StateFailureKind as SyncStateFailureKind, SyncAction, SyncError, SyncErrorKind, SyncOptions,
    SyncOutcome, SyncPlan, Validation, WorktreePathFailureKind as SyncWorktreePathFailureKind,
    plan as plan_sync, sync_from_snapshot,
};
pub use worktree::{
    DestinationKind, EntryKind, MoveError as WorktreeMoveError, RemoveError as WorktreeRemoveError,
    RollbackError as WorktreeRollbackError, WorktreePathError, inspect_move_destination,
    inspect_read_path, move_path as move_worktree_path, reject_infrastructure_path,
    remove_and_prune, rollback_move as rollback_worktree_move, validate_mutation_path,
};

/// Initializes the remote backends once during process bootstrap.
///
/// Keeping this at the engine boundary prevents the application shell from
/// depending on the physical remote implementation. Callers should invoke it
/// once before opening any remotes, not per operation.
pub fn initialize_backends() {
    gat_io::initialize_backends();
}

#[cfg(test)]
pub(crate) mod test_harness {
    use std::path::Path;

    pub struct TestRepo(tempfile::TempDir);

    impl TestRepo {
        pub fn path(&self) -> &Path {
            self.0.path()
        }
    }

    pub fn git_repo() -> TestRepo {
        let temp = tempfile::tempdir().expect("create temporary repository");
        test_support_git::run_git(temp.path(), &["init", "--quiet"]);
        TestRepo(temp)
    }

    pub fn git(dir: &Path, args: &[&str]) {
        test_support_git::run_git(dir, args);
    }

    pub fn commit_all(dir: &Path, message: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "--quiet", "--allow-empty", "-m", message]);
    }

    pub fn test_repo() -> TestRepo {
        let repo = git_repo();
        commit_all(repo.path(), "initial");
        repo
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    #[must_use]
    pub fn sync_flush_failure(
        primary: crate::SyncError,
        flush: crate::SyncError,
    ) -> crate::SyncError {
        crate::SyncError::flush_after_failure(primary, flush)
    }

    pub use crate::excludes::{
        managed_block_present as managed_block_present_for_test,
        remove_managed_block as remove_managed_block_for_test, sync as sync_excludes_for_test,
        sync_from_lock as sync_excludes_from_lock_for_test,
    };
    pub use crate::gc::test_support::{gc_sweep_failure, repository_issue};
    pub use crate::limits::ExecutionLimits;
    pub use crate::path_policy::test_support::{
        policy_comparisons, policy_compilations, remote_route_resolutions,
    };
    pub use crate::remote_session::test_support::{remote_open_count_for, remote_opens};
    pub use crate::repository_mutation::{
        load_materialized_for_test, record_materialized_for_test,
    };
    pub use crate::transfer::test_support::{
        remote_check_window_sizes, repair_attempts, repair_window_high_water,
        reset_remote_check_window_sizes,
    };
    pub use crate::workspace::sync::plan::test_support::merge_buffer_high_water;
    pub use crate::workspace::sync::test_support::{dirty_rows_high_water, do_rematerialize_calls};
    pub use crate::workspace::sync::{RefreshResult, refresh_desired_index};

    pub const LARGE_FILE_PROGRESS_THRESHOLD: u64 =
        crate::repository_mutation::LARGE_FILE_PROGRESS_THRESHOLD;

    #[must_use]
    pub fn remote_url_validation_error_for_test(
        template: &str,
        message: &'static str,
    ) -> crate::RemoteUrlValidationError {
        crate::remote_catalog::RemoteUrlValidationError::from_io(
            &template.into(),
            gat_io::remote_test_support::backend_open_error(message),
        )
    }

    use gat_core::config::RemotesConfig;
    use gat_core::name::RemoteName;
    use std::cell::Cell;
    use std::collections::BTreeMap;

    thread_local! {
        static CACHE_LOCATION_RESOLUTIONS: Cell<usize> = const { Cell::new(0) };
        static CONFIG_LOADS: Cell<usize> = const { Cell::new(0) };
        static SCOPED_CONFIG_LOADS: Cell<usize> = const { Cell::new(0) };
        static MATERIALIZATION_STRATEGY_RESOLUTIONS: Cell<usize> = const { Cell::new(0) };
        static CANONICAL_OBSERVATIONS: Cell<usize> = const { Cell::new(0) };
    }

    /// Opens a configured remote through the authoritative operation-scoped
    /// engine service and returns its semantic engine error.
    ///
    /// # Panics
    /// Panics if the test catalog cannot be built or the remote unexpectedly opens successfully.
    #[must_use]
    pub fn remote_session_error_for_test(
        template: &str,
    ) -> crate::remote_session::RemoteSessionError {
        let name = RemoteName::from_string("origin".to_string());
        let remotes = RemotesConfig {
            by_name: BTreeMap::from([(
                name.clone(),
                gat_core::endpoint::RemoteUrlTemplate::from_string(template.to_string()).into(),
            )]),
            default: Some(name.clone()),
        };
        let catalog = crate::remote_catalog::RemoteCatalog::from_config(&remotes)
            .expect("the test remote catalog is valid");
        let id = catalog
            .id_of(&name)
            .expect("the test remote is present in the catalog");
        crate::remote_session::RemoteSession::new()
            .open(&catalog, id, None)
            .expect_err("the test remote must fail to open")
    }

    #[must_use]
    pub fn cache_db_opens() -> usize {
        gat_io::cache_proof_test_support::snapshot().cache_db_opens
    }

    /// Resolves the repository's operational cache as an opaque capability.
    ///
    /// Cross-layer tests use this instead of turning the presentation-only
    /// cache location back into an operational path.
    #[must_use]
    /// # Panics
    /// Panics when the fixture configuration cannot be loaded.
    pub fn cache_root(repo: &crate::Repository) -> gat_io::CacheRoot {
        repo.resolved_cache_root().unwrap()
    }

    pub fn record_cache_location_resolution() {
        CACHE_LOCATION_RESOLUTIONS.with(|count| count.set(count.get() + 1));
    }

    pub fn cache_location_resolutions() -> usize {
        CACHE_LOCATION_RESOLUTIONS.with(Cell::get)
    }

    pub fn record_config_load() {
        CONFIG_LOADS.with(|count| count.set(count.get() + 1));
    }

    pub fn config_loads() -> usize {
        CONFIG_LOADS.with(Cell::get)
    }

    pub fn record_scoped_config_load() {
        SCOPED_CONFIG_LOADS.with(|count| count.set(count.get() + 1));
    }

    pub fn scoped_config_loads() -> usize {
        SCOPED_CONFIG_LOADS.with(Cell::get)
    }

    pub fn record_materialization_strategy_resolution() {
        MATERIALIZATION_STRATEGY_RESOLUTIONS.with(|count| count.set(count.get() + 1));
    }

    pub fn materialization_strategy_resolutions() -> usize {
        MATERIALIZATION_STRATEGY_RESOLUTIONS.with(Cell::get)
    }

    pub fn record_canonical_observation() {
        CANONICAL_OBSERVATIONS.with(|count| count.set(count.get() + 1));
    }

    pub fn canonical_observations() -> usize {
        CANONICAL_OBSERVATIONS.with(Cell::get)
    }

    pub fn reset_canonical_observations() {
        CANONICAL_OBSERVATIONS.with(|count| count.set(0));
    }
}

pub use gat_io::{AddExclusion, AddExclusionReason};

mod invocation;
pub use invocation::{EnvironmentName, InputValueReason, Invocation, InvocationInputError};

mod resources;
pub use resources::*;

pub(crate) use mount::{LockedMount, MountAdd, MountRemove, MountUpdate};
