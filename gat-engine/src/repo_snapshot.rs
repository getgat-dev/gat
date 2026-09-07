//! The coherent command-snapshot barrier: the
//! synchronization boundary a non-mount command crosses to obtain one
//! coherent repository generation of effective config, and -- for callers
//! that need it -- desired state, before doing any planning/execution work.
//!
//! This module owns only the *generic* barrier: acquire the repo lock,
//! recover any pending mount transaction, load effective config, and
//! (optionally) refresh/pin the desired-state mirror, all inside one lock
//! acquisition. Mount-specific durable journal mechanics stay in `gat-io`;
//! the engine-owned recovery workflow runs directly inside this barrier.

use crate::repository::{Repository as Repo, RepositoryError};
use gat_core::progress::{
    ProgressActivity, ProgressOperation, ProgressReporter, ProgressSpec, with_progress_typed,
};
use gat_io::{AtomicError, RepoLock};
use gat_io::{StateStore, StateStoreError};

use crate::desired_snapshot::DesiredSnapshot;
use crate::mount::MountWorkflowError;
use crate::repository_access::{
    RepositoryAccessFailureKind, classify_atomic as classify_repository_atomic, classify_io,
};
use crate::snapshot::Snapshot;
use crate::workspace::sync::{SyncError, refresh_desired_index};

use crate::repository_access::classify_state;
pub use crate::repository_access::{FilesystemFailureKind, LockFailureKind, StateFailureKind};

/// Semantic mount-recovery stage at which acquisition failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountRecoveryFailureKind {
    InterruptedTransaction,
    RepositoryState,
    Persistence,
}

/// Semantic snapshot-compilation stage at which acquisition failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotFailureKind {
    RemoteConfiguration,
    PathPolicy,
}

/// Application-facing category for a coherent repository-snapshot failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoSnapshotErrorKind {
    Repository,
    RepositoryLocked,
    RepositoryLock(FilesystemFailureKind),
    MountRecovery(MountRecoveryFailureKind),
    Configuration(
        Option<gat_core::config::ConfigScope>,
        crate::ConfigAccessFailureKind,
    ),
    State(StateFailureKind),
    Lock(LockFailureKind),
    Snapshot(SnapshotFailureKind),
    Filesystem(FilesystemFailureKind),
    Cache,
    InvalidPath,
    InvalidArgument,
    Conflict,
    Internal,
}

type BoxedSource = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Failure to acquire one coherent command snapshot.
///
/// Physical lock, state, cache, worktree, and repository errors remain
/// available through the technical source chain without exposing their
/// implementation variants or paths above the engine boundary.
#[derive(Debug)]
pub struct RepoSnapshotError {
    kind: RepoSnapshotErrorKind,
    source: BoxedSource,
}

impl RepoSnapshotError {
    fn new(
        kind: RepoSnapshotErrorKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RepoSnapshotErrorKind {
        self.kind
    }
}

impl std::fmt::Display for RepoSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stage = match self.kind {
            RepoSnapshotErrorKind::Repository => "open the repository",
            RepoSnapshotErrorKind::RepositoryLocked | RepoSnapshotErrorKind::RepositoryLock(_) => {
                "acquire repository state"
            }
            RepoSnapshotErrorKind::MountRecovery(_) => "recover an interrupted mount mutation",
            RepoSnapshotErrorKind::Configuration(_, _) => "load repository configuration",
            RepoSnapshotErrorKind::State(_) => "open repository state",
            RepoSnapshotErrorKind::Lock(_) => "read gat.lock",
            RepoSnapshotErrorKind::Snapshot(_) => "compile repository configuration",
            RepoSnapshotErrorKind::Filesystem(_) => "access repository files",
            RepoSnapshotErrorKind::Cache => "access the local object cache",
            RepoSnapshotErrorKind::InvalidPath => "validate a repository path",
            RepoSnapshotErrorKind::InvalidArgument => "validate repository selection",
            RepoSnapshotErrorKind::Conflict => "capture a stable desired state",
            RepoSnapshotErrorKind::Internal => "acquire repository state",
        };
        write!(formatter, "could not {stage}")
    }
}

impl std::error::Error for RepoSnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

fn classify_atomic(source: &AtomicError) -> RepoSnapshotErrorKind {
    match classify_repository_atomic(source) {
        RepositoryAccessFailureKind::RepositoryLocked => RepoSnapshotErrorKind::RepositoryLocked,
        RepositoryAccessFailureKind::Filesystem(kind) => RepoSnapshotErrorKind::Filesystem(kind),
        RepositoryAccessFailureKind::Lock(_) => unreachable!("atomic errors are not locks"),
    }
}

const fn classify_repository_access(kind: RepositoryAccessFailureKind) -> RepoSnapshotErrorKind {
    match kind {
        RepositoryAccessFailureKind::Lock(kind) => RepoSnapshotErrorKind::Lock(kind),
        RepositoryAccessFailureKind::Filesystem(kind) => RepoSnapshotErrorKind::Filesystem(kind),
        RepositoryAccessFailureKind::RepositoryLocked => RepoSnapshotErrorKind::RepositoryLocked,
    }
}

fn classify_repository(source: &RepositoryError) -> RepoSnapshotErrorKind {
    match source {
        RepositoryError::CurrentDirectory(source) => {
            RepoSnapshotErrorKind::Filesystem(classify_io(source))
        }
        RepositoryError::NotRepository => RepoSnapshotErrorKind::Repository,
        RepositoryError::ConfigLoad { scope, .. }
        | RepositoryError::ConfigLoadScoped { scope, .. }
        | RepositoryError::ConfigDirectoryCreate { scope, .. }
        | RepositoryError::ConfigSerialize { scope, .. }
        | RepositoryError::ConfigWrite { scope, .. } => {
            RepoSnapshotErrorKind::Configuration(Some(*scope), source.config_failure_kind())
        }
        RepositoryError::ConfigPathUnavailable
        | RepositoryError::InvalidEffectiveMounts(_)
        | RepositoryError::InvalidEffectiveSelections(_)
        | RepositoryError::InvalidEffectiveRoutes(_) => {
            RepoSnapshotErrorKind::Configuration(None, crate::ConfigAccessFailureKind::Invalid)
        }
    }
}

impl MountWorkflowError {
    /// Classifies the failed workflow stage independently of test instrumentation.
    #[must_use]
    pub const fn recovery_failure_kind(&self) -> MountRecoveryFailureKind {
        classify_mount_recovery(self)
    }
}

const fn classify_mount_recovery(source: &MountWorkflowError) -> MountRecoveryFailureKind {
    match source {
        MountWorkflowError::ReadJournal { .. } => MountRecoveryFailureKind::InterruptedTransaction,
        MountWorkflowError::StageRows { .. }
        | MountWorkflowError::WriteJournal { .. }
        | MountWorkflowError::Cleanup { .. } => MountRecoveryFailureKind::Persistence,
        MountWorkflowError::Acquire { .. }
        | MountWorkflowError::OpenState { .. }
        | MountWorkflowError::ReadState { .. }
        | MountWorkflowError::DeleteRows { .. }
        | MountWorkflowError::PublishConfig { .. }
        | MountWorkflowError::ReplayRows { .. }
        | MountWorkflowError::SyncExcludes { .. } => MountRecoveryFailureKind::RepositoryState,
        #[cfg(any(test, feature = "test-support"))]
        MountWorkflowError::Fault(_) => MountRecoveryFailureKind::RepositoryState,
    }
}

const fn classify_sync(source: &SyncError) -> RepoSnapshotErrorKind {
    use crate::workspace::sync::{
        CacheFailureKind, ExcludesFailureKind, FileStateFailureKind, MutationAuthorityFailureKind,
        SyncErrorKind, WorktreePathFailureKind,
    };

    match source.kind() {
        SyncErrorKind::Internal | SyncErrorKind::InvalidObjectId => RepoSnapshotErrorKind::Internal,
        SyncErrorKind::InvalidTrackedPath(_) => RepoSnapshotErrorKind::InvalidPath,
        SyncErrorKind::Filesystem(kind)
        | SyncErrorKind::WorktreeMutation { kind, .. }
        | SyncErrorKind::Prune(kind) => RepoSnapshotErrorKind::Filesystem(*kind),
        SyncErrorKind::Lock(kind) => RepoSnapshotErrorKind::Lock(*kind),
        SyncErrorKind::State(kind) => RepoSnapshotErrorKind::State(*kind),
        SyncErrorKind::Cache(kind) => match kind {
            CacheFailureKind::PermissionDenied => {
                RepoSnapshotErrorKind::Filesystem(FilesystemFailureKind::PermissionDenied)
            }
            CacheFailureKind::StorageExhausted => {
                RepoSnapshotErrorKind::Filesystem(FilesystemFailureKind::StorageExhausted)
            }
            _ => RepoSnapshotErrorKind::Cache,
        },
        SyncErrorKind::WorktreePath { kind, .. } => match kind {
            WorktreePathFailureKind::Internal => RepoSnapshotErrorKind::Internal,
            WorktreePathFailureKind::Filesystem(kind) => RepoSnapshotErrorKind::Filesystem(*kind),
            _ => RepoSnapshotErrorKind::InvalidPath,
        },
        SyncErrorKind::FileState(kind) => match kind {
            FileStateFailureKind::UnsupportedFileType => RepoSnapshotErrorKind::InvalidPath,
            FileStateFailureKind::Conflict => RepoSnapshotErrorKind::Conflict,
        },
        SyncErrorKind::Excludes(kind) => match kind {
            ExcludesFailureKind::Repository => RepoSnapshotErrorKind::Repository,
            ExcludesFailureKind::Lock(kind) => RepoSnapshotErrorKind::Lock(*kind),
            ExcludesFailureKind::Configuration(scope, kind) => {
                RepoSnapshotErrorKind::Configuration(*scope, *kind)
            }
            ExcludesFailureKind::State(kind) => RepoSnapshotErrorKind::State(*kind),
            ExcludesFailureKind::Filesystem(kind) => RepoSnapshotErrorKind::Filesystem(*kind),
            ExcludesFailureKind::UnsupportedFileType => RepoSnapshotErrorKind::InvalidPath,
            ExcludesFailureKind::Conflict => RepoSnapshotErrorKind::Conflict,
        },
        SyncErrorKind::MutationAuthority(kind) => match kind {
            MutationAuthorityFailureKind::Lock(kind) => RepoSnapshotErrorKind::Lock(*kind),
            MutationAuthorityFailureKind::RepositoryLocked => {
                RepoSnapshotErrorKind::RepositoryLocked
            }
            MutationAuthorityFailureKind::Filesystem(kind) => {
                RepoSnapshotErrorKind::Filesystem(*kind)
            }
            MutationAuthorityFailureKind::Configuration(scope, kind) => {
                RepoSnapshotErrorKind::Configuration(*scope, *kind)
            }
            MutationAuthorityFailureKind::InvalidArgument => RepoSnapshotErrorKind::InvalidArgument,
            MutationAuthorityFailureKind::Conflict => RepoSnapshotErrorKind::Conflict,
        },
    }
}

fn classify_desired_revision(
    source: &crate::repository_state::DesiredRevisionError,
) -> RepoSnapshotErrorKind {
    use crate::repository::RepoError;
    use crate::repository_state::DesiredRevisionError;

    match source {
        DesiredRevisionError::Lock(source) | DesiredRevisionError::Atomic(source) => {
            classify_repository_access(source.kind())
        }
        DesiredRevisionError::Repository(RepoError::Config(source)) => classify_repository(source),
        DesiredRevisionError::Repository(RepoError::Lock(source) | RepoError::Atomic(source)) => {
            classify_repository_access(source.kind())
        }
        DesiredRevisionError::Repository(RepoError::RemoteConfig(_)) => {
            RepoSnapshotErrorKind::Configuration(None, crate::ConfigAccessFailureKind::Invalid)
        }
        DesiredRevisionError::Stale(_) => RepoSnapshotErrorKind::Conflict,
    }
}

impl From<AtomicError> for RepoSnapshotError {
    fn from(source: AtomicError) -> Self {
        let kind = match classify_atomic(&source) {
            RepoSnapshotErrorKind::Filesystem(kind) => RepoSnapshotErrorKind::RepositoryLock(kind),
            kind => kind,
        };
        Self::new(kind, source)
    }
}

impl From<StateStoreError> for RepoSnapshotError {
    fn from(source: StateStoreError) -> Self {
        Self::new(
            RepoSnapshotErrorKind::State(classify_state(&source)),
            source,
        )
    }
}

impl From<crate::snapshot::SnapshotError> for RepoSnapshotError {
    fn from(source: crate::snapshot::SnapshotError) -> Self {
        let kind = match source {
            crate::snapshot::SnapshotError::RemoteCatalog(_) => {
                SnapshotFailureKind::RemoteConfiguration
            }
            crate::snapshot::SnapshotError::PathPolicy(_) => SnapshotFailureKind::PathPolicy,
        };
        Self::new(RepoSnapshotErrorKind::Snapshot(kind), source)
    }
}

impl From<crate::repository_state::DesiredRevisionError> for RepoSnapshotError {
    fn from(source: crate::repository_state::DesiredRevisionError) -> Self {
        let kind = classify_desired_revision(&source);
        Self::new(kind, source)
    }
}

impl From<MountWorkflowError> for RepoSnapshotError {
    fn from(source: MountWorkflowError) -> Self {
        let kind = RepoSnapshotErrorKind::MountRecovery(classify_mount_recovery(&source));
        Self::new(kind, source)
    }
}

impl From<RepositoryError> for RepoSnapshotError {
    fn from(source: RepositoryError) -> Self {
        let kind = classify_repository(&source);
        Self::new(kind, source)
    }
}

impl From<SyncError> for RepoSnapshotError {
    fn from(source: SyncError) -> Self {
        let kind = classify_sync(&source);
        Self::new(kind, source)
    }
}

type Result<T> = std::result::Result<T, RepoSnapshotError>;

fn lock_and_load_config(
    repo: &Repo,
    progress: &dyn ProgressReporter,
) -> Result<(RepoLock, gat_core::config::Config)> {
    let guard = RepoLock::acquire_repository(repo.layout())?;
    repo.mounts().recover_pending_locked(&guard, progress)?;
    let config = repo.load_config()?;
    Ok((guard, config))
}

/// The repository synchronization barrier a command crosses to obtain its
/// one coherent generation of effective config *and* desired state:
/// recovery, effective-config capture, and the desired-state mirror's
/// refresh against the current on-disk
/// `gat.lock` all happen inside a single [`gat_io::RepoLock`]
/// acquisition, so a mount mutation can never advance the repository
/// generation in the gap between "recovery observed nothing pending" and
/// "this command's config/desired-state snapshot is captured" -- the exact
/// TOCTOU window this barrier closes. Every step here is already cheap when
/// uncontended (recovery is a single `stat`, refresh is a fast up-to-date
/// check when nothing changed): this does not hold the lock around any
/// remote/network work, only around these three local, bounded steps.
///
/// A caller that needs config+rows from one generation (a route/ownership
/// planner like `push`/`fetch`, or
/// [`crate::desired_operation::DesiredOperation::acquire`])
/// should call this instead of separately recovering a pending mount and
/// then loading config and refreshing desired state:
/// those two steps, called separately, are exactly the gap a concurrent
/// mount mutation could complete inside of, combining an old config
/// generation with new desired-state rows (or vice versa). Reentrant: a
/// caller that already holds the repo lock (e.g. `mount()` itself) can call
/// this without blocking on itself.
pub(crate) fn recover_and_open_coherent_snapshot(
    repo: &Repo,
    progress: &dyn ProgressReporter,
) -> Result<(Snapshot, DesiredSnapshot)> {
    let (_guard, config) = lock_and_load_config(repo, progress)?;
    // A single `desired_index::refresh()` both materializes the mirror
    // *and* returns the exact `CanonicalDesiredIdentity` of the desired
    // generation it just materialized: that returned identity, not a second,
    // independent canonical observation, is the proof of what this
    // mirror now contains. There is no before/after retry loop here
    // because there is nothing left for a second observation to
    // rediscover -- refresh already proved its own generation while
    // producing it.
    let (store, refreshed) = with_progress_typed(
        progress,
        ProgressSpec::indeterminate(ProgressOperation::LoadingState),
        |_| -> Result<_> {
            let mut store = StateStore::open(repo.layout())?;
            let refreshed = refresh_desired_index(repo, &mut store)?;
            Ok((store, refreshed))
        },
    )?;
    // Pin the store's SQLite snapshot *before* releasing `_guard` below,
    // so `config` and every later read through `store` are provably the
    // same repository generation even if a concurrent mount mutation
    // commits immediately after this function returns (see
    // `StateStore::pin_snapshot`) -- without this, `store` was
    // just a plain connection whose later reads could observe a newer
    // generation than the identity refresh already returned above.
    store.pin_snapshot()?;
    let desired_revision =
        crate::repository_state::DesiredRevision::from_identity(refreshed.desired_identity);
    let snapshot = Snapshot::new(repo.snapshot_input(config, desired_revision))?;
    Ok((snapshot, DesiredSnapshot::new(store)))
}

impl Repo {
    /// Capture add's config and pinned desired state under the coherent recovery
    /// barrier, then release repository synchronization before local preparation.
    /// The callback keeps the captured config alive through mutation publication.
    pub fn with_add_state<T, E>(
        &self,
        progress: &dyn ProgressReporter,
        run: impl FnOnce(
            &gat_core::config::Config,
            crate::DesiredState<'_, '_>,
        ) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<RepoSnapshotError> + From<crate::RepositoryStateError>,
    {
        let (guard, config) = lock_and_load_config(self, progress)?;
        let loading = progress.begin(ProgressSpec::indeterminate(ProgressOperation::LoadingState));
        loading
            .handle()
            .set_activity(ProgressActivity::OpeningMaterializedState);
        loading
            .handle()
            .set_activity(ProgressActivity::RefreshingDesiredState);
        let desired = self.desired_state(&config)?;
        drop(guard);
        loading.finish();
        run(&config, desired)
    }

    /// Capture config and mutation state coherently, retaining repository
    /// authority through selection, worktree changes, and publication.
    pub fn with_desired_mutation<T, E>(
        &self,
        progress: &dyn ProgressReporter,
        run: impl FnOnce(
            &gat_core::config::Config,
            crate::DesiredMutation<'_, '_>,
        ) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<RepoSnapshotError> + From<crate::RepositoryMutationError>,
    {
        // The nested mutation guard is reentrant; this outer guard owns the
        // OS lock and must remain alive until the callback returns.
        let (_guard, config) = lock_and_load_config(self, progress)?;
        let desired = with_progress_typed(
            progress,
            ProgressSpec::indeterminate(ProgressOperation::LoadingState),
            |_| self.desired_mutation(&config),
        )?;
        run(&config, desired)
    }
}

/// As [`recover_and_open_coherent_snapshot`], but for a caller that never
/// reads desired rows through a [`crate::desired_operation::DesiredOperation`]
/// at all -- `gat sync` (`--dry-run` or not) plans/executes through its own
/// `sync_from_snapshot`-scoped `StateStore`, which already applies
/// the read-only, never-creates-the-database `--dry-run` handling
/// independently of any desired-state capability. Opening (and so unconditionally
/// creating) a *second*, unused materialized-state store here for every
/// `gat sync` invocation -- including `--dry-run` ones -- would silently
/// violate that guarantee, since opening a refreshed desired store always
/// creates the database if it doesn't exist yet. Recovers any
/// interrupted mount mutation and reads effective config inside the same
/// lock acquisition as [`recover_and_open_coherent_snapshot`], just without
/// the store step neither this function's callers nor its config need.
pub(crate) fn recover_and_load_config(
    repo: &Repo,
    progress: &dyn ProgressReporter,
) -> Result<Snapshot> {
    let (_guard, config) = lock_and_load_config(repo, progress)?;
    // `sync`/`sync --dry-run` never open a desired-state capability's
    // `DesiredSnapshot` (see the module doc comment above), but still need
    // a desired-state revision identity to detect a concurrent
    // `gat.lock` mutation between this barrier and their later mutating
    // reconciliation phase. The canonical
    // revision is read directly from the Git-visible `gat.lock`
    // representation, never from a
    // derived SQLite mirror, so computing it here is always strictly
    // read-only regardless of whether the caller is a `--dry-run` or a
    // real mutating sync.
    //
    // This can be a genuine cold read (a hash of every shard, if the
    // materialized mirror's stat-cache accelerator is absent or stale),
    // not merely a fast local check, so it must never happen silently:
    // wrapped in its own `LoadingState` task here (recovery above, by
    // this point, has already finished its own task if it ran one, so
    // this never nests inside it) rather than left uncovered before the
    // caller's own task begins later.
    let loading = progress.begin(ProgressSpec::indeterminate(ProgressOperation::LoadingState));
    loading
        .handle()
        .set_activity(ProgressActivity::RefreshingDesiredState);
    let desired_revision = crate::repository_state::current_desired_revision(repo)?;
    loading.finish();
    Ok(Snapshot::new(
        repo.snapshot_input(config, desired_revision),
    )?)
}

/// Crosses the same barrier as `recover_and_load_config` and builds a
/// bare [`crate::operation::Operation`] directly from the result
/// -- the acquisition path for a command that never reads desired rows
/// through the runtime at all (`gat sync`, which plans/executes entirely
/// through its own `sync_from_snapshot`-scoped materialized-state mirror).
///
/// This composition step keeps barrier crossing and the pure
/// `crate::operation::Operation::new` constructor inside the engine layer.
pub fn acquire_operation_without_desired_state<'repo>(
    repo: &'repo Repo,
    progress: &dyn ProgressReporter,
) -> Result<crate::operation::Operation<'repo>> {
    let snapshot = recover_and_load_config(repo, progress)?;
    let session = crate::session::Session::new();
    Ok(crate::operation::Operation::new(repo, snapshot, session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::lexical_path::GatPath;
    use std::error::Error as _;
    use std::path::PathBuf;

    fn tracked_repo() -> (crate::test_harness::TestRepo, Repo) {
        let tmp = crate::test_harness::test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let (ingested, _) = repo
            .resolved_cache_root()
            .writer()
            .ingest(&b"payload"[..])
            .unwrap();
        lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();
        (tmp, repo)
    }

    /// The `DesiredRevision` stored
    /// in the returned `Snapshot` must exactly equal the
    /// `CanonicalDesiredIdentity` the paired `DesiredSnapshot`'s refresh
    /// actually persisted -- with no second, independent filesystem
    /// identity observation involved in producing either half of the pair.
    #[test]
    fn coherent_snapshot_desired_revision_matches_the_refresh_that_produced_it() {
        let (tmp, repo) = tracked_repo();

        let (snapshot, desired) =
            recover_and_open_coherent_snapshot(&repo, &gat_core::progress::NoopProgress).unwrap();

        let persisted = desired.desired_fingerprint().unwrap();
        let persisted = gat_core::lock::CanonicalDesiredIdentity::from_bytes(persisted);

        assert_eq!(
            snapshot.desired_revision().identity(),
            persisted,
            "Snapshot's DesiredRevision must be exactly the identity the paired \
             DesiredSnapshot's refresh persisted -- not a second, independently \
             observed value"
        );
        drop(tmp);
    }

    /// A warm `recover_and_open_coherent_snapshot`
    /// call must perform zero full canonical (live-filesystem) desired-state
    /// observations -- it derives its `DesiredRevision` entirely from the
    /// one `desired_index::refresh()` it already runs, never from a
    /// separate `current_desired_revision()` call the way the old
    /// before/refresh/after retry loop did.
    #[test]
    fn warm_coherent_snapshot_performs_no_full_canonical_observation() {
        let (tmp, repo) = tracked_repo();

        // Warm up first (both the mirror and any incidental counter
        // activity from setup) so only the call under test is measured.
        recover_and_open_coherent_snapshot(&repo, &gat_core::progress::NoopProgress).unwrap();

        crate::test_support::reset_canonical_observations();
        recover_and_open_coherent_snapshot(&repo, &gat_core::progress::NoopProgress).unwrap();

        assert_eq!(
            crate::test_support::canonical_observations(),
            0,
            "recover_and_open_coherent_snapshot must never call the full canonical \
             current_desired_revision() observation -- it should build its \
             DesiredRevision directly from desired_index::refresh()'s own result"
        );
        drop(tmp);
    }

    #[derive(Debug, thiserror::Error)]
    enum MutationTestError {
        #[error(transparent)]
        Snapshot(#[from] RepoSnapshotError),
        #[error(transparent)]
        Mutation(#[from] crate::RepositoryMutationError),
    }

    #[test]
    fn coherent_mutation_keeps_the_os_lock_through_the_callback() {
        let (tmp, repo) = tracked_repo();
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path().join(".gat/state/sync.lock"))
            .unwrap();
        repo.with_desired_mutation(
            &gat_core::progress::NoopProgress,
            |_, desired| -> std::result::Result<(), MutationTestError> {
                assert!(matches!(
                    probe.try_lock(),
                    Err(std::fs::TryLockError::WouldBlock)
                ));
                assert!(desired.desired_any_subtree(&GatPath::parse_canonical("a.bin").unwrap())?);
                Ok(())
            },
        )
        .unwrap();
        probe.try_lock().unwrap();
    }

    #[test]
    fn repository_lock_timeout_is_semantic_and_retains_its_physical_source() {
        let marker = PathBuf::from("/private/repository/.gat/state/sync.lock");
        let err = RepoSnapshotError::from(AtomicError::LockTimedOut {
            path: marker.clone(),
        });

        assert_eq!(err.kind(), RepoSnapshotErrorKind::RepositoryLocked);
        assert!(!err.to_string().contains(marker.to_string_lossy().as_ref()));
        assert!(
            err.source()
                .and_then(|source| source.downcast_ref::<AtomicError>())
                .is_some_and(|source| matches!(
                    source,
                    AtomicError::LockTimedOut { path } if path == &marker
                ))
        );
    }

    #[test]
    fn corrupt_state_is_classified_without_exposing_storage_details() {
        let marker = "private-state-row-marker";
        let err = RepoSnapshotError::from(StateStoreError::InvalidRow {
            detail: marker.to_string(),
        });

        assert_eq!(
            err.kind(),
            RepoSnapshotErrorKind::State(StateFailureKind::Corrupt)
        );
        assert!(!err.to_string().contains(marker));
        assert!(
            err.source()
                .and_then(|source| source.downcast_ref::<StateStoreError>())
                .is_some_and(|source| matches!(source, StateStoreError::InvalidRow { detail } if detail == marker))
        );
    }

    #[test]
    fn invalid_snapshot_configuration_retains_the_typed_snapshot_error() {
        let err = RepoSnapshotError::from(crate::snapshot::SnapshotError::RemoteCatalog(
            crate::remote_catalog::RemoteCatalogError::UnknownDefault {
                name: "missing".to_string(),
            },
        ));

        assert_eq!(
            err.kind(),
            RepoSnapshotErrorKind::Snapshot(SnapshotFailureKind::RemoteConfiguration)
        );
        assert!(
            err.source()
                .and_then(|source| source.downcast_ref::<crate::snapshot::SnapshotError>())
                .is_some()
        );
    }
}
