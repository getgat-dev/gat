//! Semantic repository-maintenance capabilities for `gat system`.

use crate::excludes;
use crate::repository::Repository;
use gat_io::{
    CacheDatabaseHealth, CacheDatabaseUnreadable, LockStore, PreparedReshapeStatus,
    ReshapeRecoveryChoice, ReshapeTransactionKind,
};
use gat_io::{
    StateDatabaseHealth, StateDatabaseUnreadable, StateStore, count_stale_sidecars,
    inspect_database, rebuild_atomically, remove_stale_sidecars,
};

pub use gat_io::{
    CandidateInvalidReason, LiveLockInvalidReason, LiveLockState, LockMaintenanceState,
    PreparedReshapeStatus as PreparedTxnStatus, RecoveryCandidateOutcome as CandidateOutcome,
    ReshapeRecoveryChoice as RecoveryChoice, ReshapeTransactionKind as TransactionKind,
    ReshapeTransactionState as TransactionState, TransactionMalformedReason,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceErrorKind {
    Repository,
    Lock,
    State,
    Cache,
    Git,
    UnsupportedCacheSchema { version: i64 },
}

#[derive(Debug)]
pub struct MaintenanceError {
    kind: MaintenanceErrorKind,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl MaintenanceError {
    fn new(
        kind: MaintenanceErrorKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> MaintenanceErrorKind {
        self.kind
    }
}

impl std::fmt::Display for MaintenanceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let domain = match self.kind {
            MaintenanceErrorKind::Repository => "repository configuration",
            MaintenanceErrorKind::Lock => "lock state",
            MaintenanceErrorKind::State => "state metadata",
            MaintenanceErrorKind::Cache => "cache state",
            MaintenanceErrorKind::Git => "Git integration",
            MaintenanceErrorKind::UnsupportedCacheSchema { .. } => "cache state",
        };
        write!(formatter, "could not maintain {domain}")
    }
}

impl std::error::Error for MaintenanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

#[derive(Debug)]
pub struct DbErrorSource(Box<dyn std::error::Error + Send + Sync>);

impl DbErrorSource {
    fn boxed(source: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self(source)
    }
}

impl std::fmt::Display for DbErrorSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for DbErrorSource {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

/// `SQLite` integrity diagnostics retained only as a technical error source.
/// The raw text is not part of the command-facing report API.
///
/// ```compile_fail
/// use gat_engine::DbUnreadableReason;
/// fn detail(reason: DbUnreadableReason) {
///     if let DbUnreadableReason::IntegrityCheckFailed(detail) = reason {
///         let _: String = detail;
///     }
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DbIntegrityFailure(String);

impl DbIntegrityFailure {
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn for_test(detail: String) -> Self {
        Self(detail)
    }
}

#[derive(Debug)]
pub enum DbUnreadableReason {
    NotARegularFile,
    OpenFailed(DbErrorSource),
    SchemaVersionUnreadable(DbErrorSource),
    IntegrityCheckFailed(DbIntegrityFailure),
    IntegrityCheckUnrunnable(DbErrorSource),
    NotInitialized { version: i64 },
}

#[derive(Debug)]
pub enum StateDbState {
    Absent,
    Healthy,
    Outdated(i64),
    NewerVersion(i64),
    Unreadable(DbUnreadableReason),
}

#[derive(Debug)]
pub struct StateInspect {
    pub db: StateDbState,
    pub stale_sidecars: usize,
    pub validation_required: bool,
}

#[derive(Debug)]
pub enum StateRepair {
    NewerVersion { version: i64 },
    AlreadyValid { validation_required: bool },
    Rebuilt,
}

#[derive(Debug)]
pub struct StateClean {
    pub removed: usize,
}

#[derive(Debug)]
pub enum CacheDbState {
    Absent,
    Healthy,
    UnsupportedVersion(i64),
    Unreadable(DbUnreadableReason),
}

#[derive(Debug)]
pub struct CacheInspect {
    pub db: CacheDbState,
    pub temporary: usize,
}

#[derive(Debug)]
pub enum CacheRepair {
    UnsupportedVersion { version: i64 },
    AlreadyValid,
    Rebuilt,
}

#[derive(Debug)]
pub enum TemporaryCleanOutcome {
    NonePresent,
    Preserved { count: usize },
    NoneToPurge,
    Purged { count: usize },
}

#[derive(Debug)]
pub struct CacheClean {
    pub temporary: TemporaryCleanOutcome,
    pub objects_purged: Option<usize>,
}

#[derive(Debug)]
pub enum GitInspect {
    Current,
    Stale,
    PresentButUnvalidated,
    UnableToDeriveExpected,
}

#[derive(Debug)]
pub enum GitRepair {
    Rebuilt,
    AlreadyCurrent,
}

#[derive(Debug)]
pub enum GitClean {
    NoManagedArtifacts,
    RemovedStale,
    NoStaleArtifacts,
    PreservedUnvalidated,
}

#[derive(Debug)]
pub enum LockRepair {
    NothingToDo,
    CleanScratch {
        count: usize,
    },
    Recovered {
        txn_id: String,
        choice: RecoveryChoice,
        /// Post-recovery inspection, including unrelated transaction findings.
        state: LockMaintenanceState,
    },
    /// Inspection found state that automatic repair cannot resolve.
    Unresolved {
        state: LockMaintenanceState,
    },
    /// The request did not select exactly one eligible transaction.
    RecoveryNotSelected {
        state: LockMaintenanceState,
        reason: RecoverySelectionFailure,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverySelectionFailure {
    NoMatch,
    Ambiguous,
}

impl LockRepair {
    /// Whether repair resolved blocking lock state. Disposable scratch does not
    /// block later repairs; it remains the responsibility of explicit cleanup.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        match self {
            Self::NothingToDo | Self::CleanScratch { .. } => true,
            Self::Recovered { state, .. } => !lock_requires_repair(state),
            Self::Unresolved { .. } | Self::RecoveryNotSelected { .. } => false,
        }
    }
}

#[derive(Debug)]
pub struct LockClean {
    pub removed: usize,
    pub unresolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockRepairRequest {
    pub choice: Option<RecoveryChoice>,
    pub transaction: Option<String>,
}

pub struct MaintenanceService<'repo> {
    repo: &'repo Repository,
}

impl Repository {
    #[must_use]
    pub const fn maintenance(&self) -> MaintenanceService<'_> {
        MaintenanceService { repo: self }
    }
}

impl MaintenanceService<'_> {
    pub fn inspect_lock(&self) -> Result<LockMaintenanceState, MaintenanceError> {
        LockStore::inspect_repository(self.repo.layout())
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Lock, source))
    }

    pub fn repair_lock(&self, request: &LockRepairRequest) -> Result<LockRepair, MaintenanceError> {
        let _guard = self
            .repo
            .acquire_configuration_lock()
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Lock, source))?;
        let state = self.inspect_lock()?;
        match build_lock_repair_plan(state, request) {
            LockRepairPlan::Report(report) => Ok(report),
            LockRepairPlan::Recover { txn_id, choice } => {
                LockStore::recover_repository_reshape(self.repo.layout(), &txn_id, choice)
                    .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Lock, source))?;
                let state = self.inspect_lock()?;
                Ok(LockRepair::Recovered {
                    txn_id,
                    choice,
                    state,
                })
            }
        }
    }

    pub fn clean_lock(&self) -> Result<LockClean, MaintenanceError> {
        let _guard = self
            .repo
            .acquire_configuration_lock()
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Lock, source))?;
        let state = self.inspect_lock()?;
        let removable = removable_transaction_count(&state);
        let unresolved = has_preserved_transaction_state(&state);
        let removed = if removable == 0 {
            0
        } else {
            LockStore::clean_repository_reshape_scratch(self.repo.layout())
                .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Lock, source))?
        };
        Ok(LockClean {
            removed,
            unresolved,
        })
    }

    pub fn inspect_state(&self) -> Result<StateInspect, MaintenanceError> {
        let db = self.inspect_state_db()?;
        let stale_sidecars = count_stale_sidecars(self.repo.layout())
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::State, source))?;
        let validation_required = self.current_validation_required(&db)?;
        Ok(StateInspect {
            db,
            stale_sidecars,
            validation_required,
        })
    }

    pub fn repair_state(&self) -> Result<StateRepair, MaintenanceError> {
        let _guard = self
            .repo
            .acquire_configuration_lock()
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::State, source))?;
        let db = self.inspect_state_db()?;
        if let StateDbState::NewerVersion(version) = db {
            return Ok(StateRepair::NewerVersion { version });
        }
        if matches!(db, StateDbState::Healthy) {
            return Ok(StateRepair::AlreadyValid {
                validation_required: self.current_validation_required(&db)?,
            });
        }
        rebuild_atomically(self.repo.layout())
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::State, source))?;
        Ok(StateRepair::Rebuilt)
    }

    pub fn clean_state(&self) -> Result<StateClean, MaintenanceError> {
        let _guard = self
            .repo
            .acquire_configuration_lock()
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::State, source))?;
        let removed = remove_stale_sidecars(self.repo.layout())
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::State, source))?;
        Ok(StateClean { removed })
    }

    pub fn inspect_cache(&self) -> Result<CacheInspect, MaintenanceError> {
        let cache_root = self.cache_root()?;
        let cache = cache_root.maintenance();
        let db = map_cache_health(
            cache
                .inspect_database()
                .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?,
        );
        let temporary = cache
            .count_temporary_objects()
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?;
        Ok(CacheInspect { db, temporary })
    }

    pub fn repair_cache(&self) -> Result<CacheRepair, MaintenanceError> {
        let cache_root = self.cache_root()?;
        let cache = cache_root.maintenance();
        match map_cache_health(
            cache
                .inspect_database()
                .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?,
        ) {
            CacheDbState::UnsupportedVersion(version) => {
                Ok(CacheRepair::UnsupportedVersion { version })
            }
            CacheDbState::Healthy => Ok(CacheRepair::AlreadyValid),
            CacheDbState::Absent | CacheDbState::Unreadable(_) => {
                cache
                    .rebuild_database()
                    .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?;
                Ok(CacheRepair::Rebuilt)
            }
        }
    }

    pub fn clean_cache(
        &self,
        purge_temporary: bool,
        purge_objects: bool,
    ) -> Result<CacheClean, MaintenanceError> {
        let cache_root = self.cache_root()?;
        let cache = cache_root.maintenance();
        if purge_objects
            && let CacheDbState::UnsupportedVersion(version) = map_cache_health(
                cache
                    .inspect_database()
                    .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?,
            )
        {
            return Err(MaintenanceError::new(
                MaintenanceErrorKind::UnsupportedCacheSchema { version },
                UnsupportedCacheSchema { version },
            ));
        }

        let temporary = cache
            .count_temporary_objects()
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?;
        let temporary = if !purge_temporary {
            if temporary == 0 {
                TemporaryCleanOutcome::NonePresent
            } else {
                TemporaryCleanOutcome::Preserved { count: temporary }
            }
        } else if temporary == 0 {
            TemporaryCleanOutcome::NoneToPurge
        } else {
            let count = cache
                .purge_temporary_objects()
                .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?;
            TemporaryCleanOutcome::Purged { count }
        };
        let objects_purged = if purge_objects {
            let count = cache
                .purge_objects()
                .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?;
            cache
                .rebuild_database()
                .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Cache, source))?;
            Some(count)
        } else {
            None
        };
        Ok(CacheClean {
            temporary,
            objects_purged,
        })
    }

    pub fn inspect_git(&self) -> Result<GitInspect, MaintenanceError> {
        let present = excludes::managed_block_present(self.repo)
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Git, source))?;
        match excludes::sync(self.repo, true) {
            Ok(status) if status.changed => Ok(GitInspect::Stale),
            Ok(_) => Ok(GitInspect::Current),
            Err(_) if present => Ok(GitInspect::PresentButUnvalidated),
            Err(_) => Ok(GitInspect::UnableToDeriveExpected),
        }
    }

    pub fn repair_git(&self) -> Result<GitRepair, MaintenanceError> {
        let status = excludes::sync(self.repo, false)
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Git, source))?;
        Ok(if status.changed {
            GitRepair::Rebuilt
        } else {
            GitRepair::AlreadyCurrent
        })
    }

    pub fn clean_git(&self) -> Result<GitClean, MaintenanceError> {
        let present = excludes::managed_block_present(self.repo)
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Git, source))?;
        if !present {
            return Ok(GitClean::NoManagedArtifacts);
        }
        match excludes::sync(self.repo, true) {
            Ok(status) if status.changed => {
                excludes::remove_managed_block(self.repo)
                    .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Git, source))?;
                Ok(GitClean::RemovedStale)
            }
            Ok(_) => Ok(GitClean::NoStaleArtifacts),
            Err(_) => Ok(GitClean::PreservedUnvalidated),
        }
    }

    fn cache_root(&self) -> Result<gat_io::CacheRoot, MaintenanceError> {
        let config = self
            .repo
            .load_config()
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::Repository, source))?;
        Ok(self.repo.resolved_cache_root_from(&config))
    }

    fn inspect_state_db(&self) -> Result<StateDbState, MaintenanceError> {
        let health = inspect_database(self.repo.layout())
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::State, source))?;
        Ok(map_state_health(health))
    }

    fn current_validation_required(&self, db: &StateDbState) -> Result<bool, MaintenanceError> {
        if !matches!(db, StateDbState::Healthy) {
            return Ok(false);
        }
        StateStore::open_if_exists(self.repo.layout())
            .map(|store| store.is_some_and(|store| store.validation_required()))
            .map_err(|source| MaintenanceError::new(MaintenanceErrorKind::State, source))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("cache database has unsupported schema version {version}")]
struct UnsupportedCacheSchema {
    version: i64,
}

enum LockRepairPlan {
    Report(LockRepair),
    Recover {
        txn_id: String,
        choice: RecoveryChoice,
    },
}

fn build_lock_repair_plan(
    state: LockMaintenanceState,
    request: &LockRepairRequest,
) -> LockRepairPlan {
    if let Some(choice) = request.choice {
        return match select_explicit_transaction(&state, request.transaction.as_deref(), choice) {
            Ok(index) => LockRepairPlan::Recover {
                txn_id: state.transactions[index].id.clone(),
                choice,
            },
            Err(reason) => {
                LockRepairPlan::Report(LockRepair::RecoveryNotSelected { state, reason })
            }
        };
    }
    if lock_requires_repair(&state) {
        return LockRepairPlan::Report(LockRepair::Unresolved { state });
    }
    let count = removable_transaction_count(&state);
    if count > 0 {
        LockRepairPlan::Report(LockRepair::CleanScratch { count })
    } else {
        LockRepairPlan::Report(LockRepair::NothingToDo)
    }
}

fn select_explicit_transaction(
    state: &LockMaintenanceState,
    transaction: Option<&str>,
    choice: RecoveryChoice,
) -> Result<usize, RecoverySelectionFailure> {
    let mut candidates = state
        .transactions
        .iter()
        .enumerate()
        .filter(|(_, transaction)| match &transaction.kind {
            ReshapeTransactionKind::Prepared(prepared) if needs_explicit_recovery(transaction) => {
                match choice {
                    ReshapeRecoveryChoice::RestoreBackup => prepared.backup.is_valid(),
                    ReshapeRecoveryChoice::PromoteStaged => prepared.staged.is_valid(),
                }
            }
            _ => false,
        });
    if let Some(id) = transaction {
        candidates
            .find(|(_, transaction)| transaction.id == id)
            .map(|(index, _)| index)
            .ok_or(RecoverySelectionFailure::NoMatch)
    } else {
        let (index, _) = candidates.next().ok_or(RecoverySelectionFailure::NoMatch)?;
        if candidates.next().is_some() {
            Err(RecoverySelectionFailure::Ambiguous)
        } else {
            Ok(index)
        }
    }
}

fn removable_transaction_count(state: &LockMaintenanceState) -> usize {
    state
        .transactions
        .iter()
        .filter(|transaction| match &transaction.kind {
            ReshapeTransactionKind::ScratchOnly => true,
            ReshapeTransactionKind::Malformed { .. } => false,
            ReshapeTransactionKind::Prepared(prepared) => matches!(
                prepared.status,
                PreparedReshapeStatus::CleanablePrepared
                    | PreparedReshapeStatus::CompletedNotCleaned
            ),
        })
        .count()
}

fn lock_requires_repair(state: &LockMaintenanceState) -> bool {
    matches!(state.live, LiveLockState::Invalid { .. }) || has_preserved_transaction_state(state)
}

fn has_preserved_transaction_state(state: &LockMaintenanceState) -> bool {
    state
        .transactions
        .iter()
        .any(|transaction| match &transaction.kind {
            ReshapeTransactionKind::Malformed { .. } => true,
            ReshapeTransactionKind::Prepared(prepared) => matches!(
                prepared.status,
                PreparedReshapeStatus::RecoveryRequired
                    | PreparedReshapeStatus::AmbiguousRecovery
                    | PreparedReshapeStatus::CorruptRecoveryState
            ),
            ReshapeTransactionKind::ScratchOnly => false,
        })
}

fn needs_explicit_recovery(transaction: &TransactionState) -> bool {
    matches!(
        &transaction.kind,
        ReshapeTransactionKind::Prepared(prepared)
            if matches!(
                prepared.status,
                PreparedReshapeStatus::RecoveryRequired
                    | PreparedReshapeStatus::AmbiguousRecovery
                    | PreparedReshapeStatus::CorruptRecoveryState
            )
    )
}

fn map_state_health(health: StateDatabaseHealth) -> StateDbState {
    match health {
        StateDatabaseHealth::Absent => StateDbState::Absent,
        StateDatabaseHealth::Healthy => StateDbState::Healthy,
        StateDatabaseHealth::Outdated(version) => StateDbState::Outdated(version),
        StateDatabaseHealth::NewerVersion(version) => StateDbState::NewerVersion(version),
        StateDatabaseHealth::Unreadable(reason) => StateDbState::Unreadable(match reason {
            StateDatabaseUnreadable::NotARegularFile => DbUnreadableReason::NotARegularFile,
            StateDatabaseUnreadable::OpenFailed(source) => {
                DbUnreadableReason::OpenFailed(DbErrorSource::boxed(source))
            }
            StateDatabaseUnreadable::SchemaVersionUnreadable(source) => {
                DbUnreadableReason::SchemaVersionUnreadable(DbErrorSource::boxed(source))
            }
            StateDatabaseUnreadable::IntegrityCheckFailed(detail) => {
                DbUnreadableReason::IntegrityCheckFailed(DbIntegrityFailure(detail))
            }
            StateDatabaseUnreadable::IntegrityCheckUnrunnable(source) => {
                DbUnreadableReason::IntegrityCheckUnrunnable(DbErrorSource::boxed(source))
            }
        }),
    }
}

fn map_cache_health(health: CacheDatabaseHealth) -> CacheDbState {
    match health {
        CacheDatabaseHealth::Absent => CacheDbState::Absent,
        CacheDatabaseHealth::Healthy => CacheDbState::Healthy,
        CacheDatabaseHealth::UnsupportedVersion(version) => {
            CacheDbState::UnsupportedVersion(version)
        }
        CacheDatabaseHealth::Unreadable(reason) => CacheDbState::Unreadable(match reason {
            CacheDatabaseUnreadable::NotARegularFile => DbUnreadableReason::NotARegularFile,
            CacheDatabaseUnreadable::OpenFailed(source) => {
                DbUnreadableReason::OpenFailed(DbErrorSource::boxed(source))
            }
            CacheDatabaseUnreadable::SchemaVersionUnreadable(source) => {
                DbUnreadableReason::SchemaVersionUnreadable(DbErrorSource::boxed(source))
            }
            CacheDatabaseUnreadable::IntegrityCheckFailed(detail) => {
                DbUnreadableReason::IntegrityCheckFailed(DbIntegrityFailure(detail))
            }
            CacheDatabaseUnreadable::IntegrityCheckUnrunnable(source) => {
                DbUnreadableReason::IntegrityCheckUnrunnable(DbErrorSource::boxed(source))
            }
            CacheDatabaseUnreadable::NotInitialized { version } => {
                DbUnreadableReason::NotInitialized { version }
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::lexical_path::GatPath;
    use gat_core::lock::{Entry, Lock, LockShardLevels};
    use gat_core::oid::Oid;
    use gat_io::LockStore;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn integrity_reports_retain_diagnostics_without_copying_the_text() {
        for cache in [false, true] {
            let detail = String::from("sqlite diagnostic sentinel");
            let allocation = detail.as_ptr();
            let reason = if cache {
                let CacheDbState::Unreadable(reason) =
                    map_cache_health(CacheDatabaseHealth::Unreadable(
                        CacheDatabaseUnreadable::IntegrityCheckFailed(detail),
                    ))
                else {
                    panic!("expected an unreadable cache report");
                };
                reason
            } else {
                let StateDbState::Unreadable(reason) =
                    map_state_health(StateDatabaseHealth::Unreadable(
                        StateDatabaseUnreadable::IntegrityCheckFailed(detail),
                    ))
                else {
                    panic!("expected an unreadable state report");
                };
                reason
            };
            let DbUnreadableReason::IntegrityCheckFailed(source) = reason else {
                panic!("expected an integrity failure");
            };
            assert_eq!(source.0.as_ptr(), allocation);
            assert_eq!(source.0, "sqlite diagnostic sentinel");
        }
    }

    fn tracked_repo() -> (tempfile::TempDir, Repository) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let lock = Lock {
            entries: vec![
                Entry {
                    path: GatPath::parse_canonical("a.bin").unwrap(),
                    oid: Oid::from_hex(&"1".repeat(64)).unwrap(),
                },
                Entry {
                    path: GatPath::parse_canonical("b.bin").unwrap(),
                    oid: Oid::from_hex(&"2".repeat(64)).unwrap(),
                },
            ],
        };
        LockStore::publish_repository(repo.layout(), &lock, LockShardLevels::FLAT).unwrap();
        (tmp, repo)
    }

    fn simulated_txn(root: &Path, repo: &Repository, target_levels: LockShardLevels) -> PathBuf {
        let lock = LockStore::load_repository(repo.layout()).unwrap();
        gat_io::simulate_lock_crash_after_first_rename(root, &lock, target_levels).unwrap();
        let reshape_root = root.join(".gat").join("lock-reshape");
        let mut transactions = fs::read_dir(&reshape_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        transactions.sort();
        assert_eq!(transactions.len(), 1);
        transactions.pop().unwrap()
    }

    fn txn_id(transaction: &Path) -> String {
        transaction
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn inspect_lock_is_read_only_for_an_interrupted_reshape() {
        let (tmp, repo) = tracked_repo();
        let transaction = simulated_txn(tmp.path(), &repo, LockShardLevels::new(2).unwrap());
        let before_backup = fs::metadata(transaction.join("backup")).unwrap().len();
        let before_staged_entries = fs::read_dir(transaction.join("new")).unwrap().count();

        let state = repo.maintenance().inspect_lock().unwrap();

        assert!(state.transactions.iter().any(needs_explicit_recovery));
        assert!(!tmp.path().join("gat.lock").exists());
        assert_eq!(
            fs::metadata(transaction.join("backup")).unwrap().len(),
            before_backup
        );
        assert_eq!(
            fs::read_dir(transaction.join("new")).unwrap().count(),
            before_staged_entries
        );
    }

    #[test]
    fn repair_plan_retains_invalid_state_alongside_disposable_scratch() {
        for invalid_live in [false, true] {
            for malformed in [false, true] {
                let live = if invalid_live {
                    LiveLockState::Invalid {
                        reason: LiveLockInvalidReason::NeitherFileNorShardTree,
                    }
                } else {
                    LiveLockState::Valid {
                        shard_levels: LockShardLevels::FLAT,
                        entries: 0,
                    }
                };
                let mut transactions = vec![TransactionState {
                    id: "scratch".into(),
                    kind: TransactionKind::ScratchOnly,
                }];
                if malformed {
                    transactions.push(TransactionState {
                        id: "malformed".into(),
                        kind: TransactionKind::Malformed {
                            reason: TransactionMalformedReason::UnrecognizedPhase,
                        },
                    });
                }
                let plan = build_lock_repair_plan(
                    LockMaintenanceState { live, transactions },
                    &LockRepairRequest {
                        choice: None,
                        transaction: None,
                    },
                );
                if invalid_live || malformed {
                    let LockRepairPlan::Report(LockRepair::Unresolved { state }) = plan else {
                        panic!("invalid state must remain in an unresolved report");
                    };
                    assert_eq!(
                        matches!(state.live, LiveLockState::Invalid { .. }),
                        invalid_live
                    );
                    assert_eq!(state.transactions.len(), 1 + usize::from(malformed));
                    assert_eq!(state.transactions[0].id, "scratch");
                } else {
                    assert!(matches!(
                        plan,
                        LockRepairPlan::Report(LockRepair::CleanScratch { count: 1 })
                    ));
                }
            }
        }
    }

    #[test]
    fn recovery_selection_distinguishes_ambiguity_from_no_match() {
        let candidate = |choice| gat_io::RecoveryCandidateState {
            choice,
            outcome: CandidateOutcome::Valid {
                shard_levels: LockShardLevels::FLAT,
                entries: 1,
            },
        };
        let transaction = |id: &str| TransactionState {
            id: id.into(),
            kind: TransactionKind::Prepared(Box::new(gat_io::PreparedReshapeState {
                backup: candidate(RecoveryChoice::RestoreBackup),
                staged: candidate(RecoveryChoice::PromoteStaged),
                status: PreparedTxnStatus::AmbiguousRecovery,
            })),
        };
        let mut state = LockMaintenanceState {
            live: LiveLockState::Missing,
            transactions: vec![transaction("first"), transaction("second")],
        };
        for choice in [RecoveryChoice::RestoreBackup, RecoveryChoice::PromoteStaged] {
            assert_eq!(
                select_explicit_transaction(&state, None, choice),
                Err(RecoverySelectionFailure::Ambiguous)
            );
            assert_eq!(
                select_explicit_transaction(&state, Some("second"), choice),
                Ok(1)
            );
            assert_eq!(
                select_explicit_transaction(&state, Some("unknown"), choice),
                Err(RecoverySelectionFailure::NoMatch)
            );
        }
        state.transactions.pop();
        assert_eq!(
            select_explicit_transaction(&state, None, RecoveryChoice::RestoreBackup),
            Ok(0)
        );
        state.transactions.clear();
        assert_eq!(
            select_explicit_transaction(&state, None, RecoveryChoice::RestoreBackup),
            Err(RecoverySelectionFailure::NoMatch)
        );
    }

    #[test]
    fn unmatched_recovery_preserves_the_inspected_transactions() {
        let plan = build_lock_repair_plan(
            LockMaintenanceState {
                live: LiveLockState::Missing,
                transactions: vec![TransactionState {
                    id: "scratch".into(),
                    kind: TransactionKind::ScratchOnly,
                }],
            },
            &LockRepairRequest {
                choice: Some(RecoveryChoice::RestoreBackup),
                transaction: Some("missing".into()),
            },
        );
        let LockRepairPlan::Report(LockRepair::RecoveryNotSelected { state, reason }) = plan else {
            panic!("expected an unmatched recovery report");
        };
        assert_eq!(reason, RecoverySelectionFailure::NoMatch);
        assert_eq!(state.transactions.len(), 1);
        assert_eq!(state.transactions[0].id, "scratch");
    }

    #[test]
    fn repair_lock_requires_an_explicit_choice_for_ambiguous_recovery() {
        let (tmp, repo) = tracked_repo();
        let transaction = simulated_txn(tmp.path(), &repo, LockShardLevels::new(2).unwrap());

        let report = repo
            .maintenance()
            .repair_lock(&LockRepairRequest {
                choice: None,
                transaction: None,
            })
            .unwrap();

        let LockRepair::Unresolved { state } = report else {
            panic!("expected explicit recovery choice");
        };
        assert!(
            state
                .transactions
                .iter()
                .any(|candidate| candidate.id == txn_id(&transaction))
        );
        assert!(!tmp.path().join("gat.lock").exists());
    }

    #[test]
    fn repair_lock_can_restore_the_validated_backup_explicitly() {
        let (tmp, repo) = tracked_repo();
        let transaction = simulated_txn(tmp.path(), &repo, LockShardLevels::new(2).unwrap());

        let report = repo
            .maintenance()
            .repair_lock(&LockRepairRequest {
                choice: Some(RecoveryChoice::RestoreBackup),
                transaction: Some(txn_id(&transaction)),
            })
            .unwrap();

        assert!(report.is_complete());
        assert!(matches!(
            report,
            LockRepair::Recovered {
                choice: RecoveryChoice::RestoreBackup,
                ..
            }
        ));
        assert_eq!(
            LockStore::current_repository_shard_levels(repo.layout()).unwrap(),
            Some(LockShardLevels::FLAT)
        );
        assert!(transaction.exists());
    }

    #[test]
    fn explicit_recovery_retains_unrelated_malformed_transactions() {
        let (tmp, repo) = tracked_repo();
        let transaction = simulated_txn(tmp.path(), &repo, LockShardLevels::new(2).unwrap());
        let other = transaction.parent().unwrap().join("unrelated");
        fs::create_dir(&other).unwrap();
        let malformed = b"not a transaction record";
        fs::write(other.join("txn.json"), malformed).unwrap();

        let report = repo
            .maintenance()
            .repair_lock(&LockRepairRequest {
                choice: Some(RecoveryChoice::RestoreBackup),
                transaction: Some(txn_id(&transaction)),
            })
            .unwrap();

        assert!(!report.is_complete());
        let LockRepair::Recovered { state, .. } = report else {
            panic!("the requested transaction should still be recovered");
        };
        assert!(matches!(state.live, LiveLockState::Valid { .. }));
        assert!(
            state.transactions.iter().any(|txn| txn.id == "unrelated"
                && matches!(txn.kind, TransactionKind::Malformed { .. }))
        );
        assert_eq!(fs::read(other.join("txn.json")).unwrap(), malformed);
    }

    #[test]
    fn repair_lock_can_promote_the_validated_staged_lock_explicitly() {
        let (tmp, repo) = tracked_repo();
        let target = LockShardLevels::new(2).unwrap();
        let transaction = simulated_txn(tmp.path(), &repo, target);

        let report = repo
            .maintenance()
            .repair_lock(&LockRepairRequest {
                choice: Some(RecoveryChoice::PromoteStaged),
                transaction: Some(txn_id(&transaction)),
            })
            .unwrap();

        assert!(report.is_complete());
        assert!(matches!(
            report,
            LockRepair::Recovered {
                choice: RecoveryChoice::PromoteStaged,
                ..
            }
        ));
        assert_eq!(
            LockStore::current_repository_shard_levels(repo.layout()).unwrap(),
            Some(target)
        );
        assert!(transaction.exists());
    }

    #[test]
    fn clean_lock_preserves_interrupted_recovery_state() {
        let (tmp, repo) = tracked_repo();
        let transaction = simulated_txn(tmp.path(), &repo, LockShardLevels::new(2).unwrap());

        let report = repo.maintenance().clean_lock().unwrap();

        assert!(report.unresolved);
        assert!(transaction.exists());
        assert!(!tmp.path().join("gat.lock").exists());
    }

    #[test]
    fn clean_lock_removes_completed_transaction_scratch() {
        let (tmp, repo) = tracked_repo();
        let transaction = simulated_txn(tmp.path(), &repo, LockShardLevels::new(2).unwrap());
        fs::rename(transaction.join("new"), tmp.path().join("gat.lock")).unwrap();

        let report = repo.maintenance().clean_lock().unwrap();

        assert!(!report.unresolved);
        assert_eq!(report.removed, 1);
        assert!(!transaction.exists());
    }

    #[test]
    fn repair_lock_reports_completed_scratch_without_removing_it() {
        let (tmp, repo) = tracked_repo();
        let transaction = simulated_txn(tmp.path(), &repo, LockShardLevels::new(2).unwrap());
        fs::rename(transaction.join("new"), tmp.path().join("gat.lock")).unwrap();

        let report = repo
            .maintenance()
            .repair_lock(&LockRepairRequest {
                choice: None,
                transaction: None,
            })
            .unwrap();

        assert!(matches!(report, LockRepair::CleanScratch { count: 1 }));
        assert!(transaction.exists());
    }

    #[test]
    fn clean_lock_serializes_with_a_concurrent_repo_lock_holder() {
        let (tmp, repo) = tracked_repo();
        let transaction = simulated_txn(tmp.path(), &repo, LockShardLevels::new(2).unwrap());
        fs::rename(transaction.join("new"), tmp.path().join("gat.lock")).unwrap();
        let guard = repo.acquire_configuration_lock().unwrap();
        let root = tmp.path().to_path_buf();
        let progressed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progressed_writer = progressed.clone();
        let (start_tx, start_rx) = std::sync::mpsc::channel::<()>();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            start_rx.recv().unwrap();
            let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(root);
            let report = repo.maintenance().clean_lock().unwrap();
            progressed_writer.store(true, std::sync::atomic::Ordering::SeqCst);
            report
        });

        gat_io::atomic_test_support::with_acquire_attempt_hook(
            handle.thread().id(),
            attempted_tx,
            || {
                start_tx.send(()).unwrap();
                attempted_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("maintenance must reach the repository lock boundary");
            },
        );
        assert!(!progressed.load(std::sync::atomic::Ordering::SeqCst));
        drop(guard);

        let report = handle.join().unwrap();
        assert!(progressed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!report.unresolved);
        assert!(!transaction.exists());
    }

    #[test]
    fn repair_state_refuses_to_rewrite_a_newer_schema_database() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let db_path = tmp.path().join(".gat/state/state.sqlite3");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let version = gat_io::state_current_schema_version_for_test() + 1;
        gat_io::state_set_schema_version_for_test(repo.layout(), version).unwrap();
        let before = fs::read(&db_path).unwrap();

        let report = repo.maintenance().repair_state().unwrap();

        assert!(matches!(report, StateRepair::NewerVersion { version: found } if found == version));
        assert_eq!(fs::read(&db_path).unwrap(), before);
    }

    #[test]
    fn failed_state_rebuild_preserves_the_live_database_and_removes_its_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let db_path = tmp.path().join(".gat/state/state.sqlite3");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let version = gat_io::state_current_schema_version_for_test() - 1;
        gat_io::state_set_schema_version_for_test(repo.layout(), version).unwrap();
        let before = fs::read(&db_path).unwrap();
        fs::write(tmp.path().join("gat.lock"), "not a gat lock\n").unwrap();

        assert!(repo.maintenance().repair_state().is_err());
        assert_eq!(fs::read(&db_path).unwrap(), before);
        assert_eq!(
            gat_io::state_repair_temp_count_for_test(repo.layout()).unwrap(),
            0
        );
    }

    #[test]
    fn inspect_state_reports_a_wrong_filesystem_type_as_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        fs::create_dir_all(tmp.path().join(".gat/state/state.sqlite3")).unwrap();

        let report = repo.maintenance().inspect_state().unwrap();

        assert!(matches!(report.db, StateDbState::Unreadable(_)));
    }

    #[test]
    fn repair_state_leaves_an_already_healthy_database_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut store = StateStore::open(repo.layout()).unwrap();
        crate::workspace::sync::refresh_desired_index(&repo, &mut store).unwrap();
        drop(store);
        let db_path = tmp.path().join(".gat/state/state.sqlite3");
        let before = fs::read(&db_path).unwrap();

        let report = repo.maintenance().repair_state().unwrap();

        assert!(matches!(
            report,
            StateRepair::AlreadyValid {
                validation_required: false
            }
        ));
        assert_eq!(fs::read(&db_path).unwrap(), before);
    }

    #[test]
    fn destructive_state_rebuild_marks_provenance_for_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let report = repo.maintenance().repair_state().unwrap();

        assert!(matches!(report, StateRepair::Rebuilt));
        assert!(
            StateStore::open(repo.layout())
                .unwrap()
                .validation_required()
        );
        let inspect = repo.maintenance().inspect_state().unwrap();
        assert!(matches!(inspect.db, StateDbState::Healthy));
        assert!(inspect.validation_required);
    }

    #[test]
    fn clean_state_serializes_with_a_concurrent_repo_lock_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let db_path = tmp.path().join(".gat/state/state.sqlite3");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        gat_io::state_create_stale_sidecar_for_test(repo.layout()).unwrap();
        let guard = repo.acquire_configuration_lock().unwrap();
        let root = tmp.path().to_path_buf();
        let progressed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progressed_writer = progressed.clone();
        let (start_tx, start_rx) = std::sync::mpsc::channel::<()>();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            start_rx.recv().unwrap();
            let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(root);
            let report = repo.maintenance().clean_state().unwrap();
            progressed_writer.store(true, std::sync::atomic::Ordering::SeqCst);
            report
        });

        gat_io::atomic_test_support::with_acquire_attempt_hook(
            handle.thread().id(),
            attempted_tx,
            || {
                start_tx.send(()).unwrap();
                attempted_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("maintenance must reach the repository lock boundary");
            },
        );
        assert!(!progressed.load(std::sync::atomic::Ordering::SeqCst));
        drop(guard);

        let report = handle.join().unwrap();
        assert!(progressed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(report.removed, 1);
    }
}
