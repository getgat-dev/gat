//! Shared typed state, cache, and worktree failures used by reconciliation,
//! repository mutation, and comparison workflows. Expected per-path outcomes remain data in [`super::SyncOutcome`].

use gat_core::config::ConfigScope;
use gat_io::AtomicError;
use gat_io::CacheError;
use gat_io::FileStateError;
use gat_io::LockError;
use gat_io::StateStoreError;
use gat_io::{PruneError, WorktreeMutationError, WorktreePathError};

pub type Result<T> = std::result::Result<T, SyncError>;

pub use crate::repository_access::{FilesystemFailureKind, LockFailureKind, StateFailureKind};
use crate::repository_access::{classify_io, classify_state};

/// Semantic local-cache failure classes needed by root diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheFailureKind {
    PermissionDenied,
    ObjectMissing,
    StateUnavailable,
    StateIncompatible,
    StateCorrupt,
    StorageExhausted,
    Unavailable,
    InvalidObjectId,
    Materialization(Vec<gat_core::config::MaterializationMode>),
}

/// Semantic working-tree path failure classes needed by root diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreePathFailureKind {
    OutsideRepository,
    InfrastructurePath,
    UnsupportedFileType,
    Filesystem(FilesystemFailureKind),
    Internal,
}

/// Semantic coherent-file-observation failure classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStateFailureKind {
    UnsupportedFileType,
    Conflict,
}

/// Semantic `.git/info/exclude` regeneration failure classes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExcludesFailureKind {
    Repository,
    Lock(LockFailureKind),
    Configuration(Option<ConfigScope>, crate::ConfigAccessFailureKind),
    State(StateFailureKind),
    Filesystem(FilesystemFailureKind),
    UnsupportedFileType,
    Conflict,
}

/// Semantic mutation-authority failure classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationAuthorityFailureKind {
    Lock(LockFailureKind),
    RepositoryLocked,
    Filesystem(FilesystemFailureKind),
    Configuration(Option<ConfigScope>, crate::ConfigAccessFailureKind),
    InvalidArgument,
    Conflict,
}

/// Application-facing category for a reconciliation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncErrorKind {
    Internal,
    InvalidTrackedPath(String),
    Filesystem(FilesystemFailureKind),
    Lock(LockFailureKind),
    State(StateFailureKind),
    InvalidObjectId,
    Cache(CacheFailureKind),
    WorktreePath {
        kind: WorktreePathFailureKind,
        path: Option<String>,
    },
    FileState(FileStateFailureKind),
    WorktreeMutation {
        kind: FilesystemFailureKind,
        path: Option<String>,
    },
    Excludes(ExcludesFailureKind),
    Prune(FilesystemFailureKind),
    MutationAuthority(MutationAuthorityFailureKind),
}

type BoxedSource = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Failure that prevents reconciliation from completing.
///
/// Physical lock, state, cache, and worktree errors remain available through
/// the technical source chain without exposing their variants or paths above
/// the engine boundary.
#[derive(Debug)]
pub struct SyncError {
    kind: SyncErrorKind,
    source: BoxedSource,
}

impl SyncError {
    fn new(kind: SyncErrorKind, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &SyncErrorKind {
        &self.kind
    }

    #[must_use]
    pub fn flush_failed(&self) -> bool {
        self.source
            .downcast_ref::<FlushFailureComposite>()
            .is_some()
    }

    pub(crate) fn missing_materialized_store() -> Self {
        Self::new(SyncErrorKind::Internal, MissingStateStore)
    }

    pub(crate) fn flush_after_failure(primary: Self, flush_error: Self) -> Self {
        let kind = primary.kind.clone();
        Self::new(
            kind,
            FlushFailureComposite {
                primary: Box::new(primary),
                flush_error: Box::new(flush_error),
            },
        )
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stage = match self.kind {
            SyncErrorKind::Internal => "prepare reconciliation",
            SyncErrorKind::InvalidTrackedPath(_) | SyncErrorKind::WorktreePath { .. } => {
                "validate a working-tree path"
            }
            SyncErrorKind::Filesystem(_)
            | SyncErrorKind::FileState(_)
            | SyncErrorKind::WorktreeMutation { .. }
            | SyncErrorKind::Prune(_) => "synchronize the working tree",
            SyncErrorKind::Lock(_) => "read or write gat.lock",
            SyncErrorKind::State(_) => "read or write local state",
            SyncErrorKind::InvalidObjectId => "validate an object identifier",
            SyncErrorKind::Cache(_) => "access the local object cache",
            SyncErrorKind::Excludes(_) => "regenerate .git/info/exclude",
            SyncErrorKind::MutationAuthority(_) => "confirm synchronization authority",
        };
        write!(formatter, "could not {stage}")
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("streaming desired planning requires a materialized-state store")]
struct MissingStateStore;

/// Retains both typed halves of a reconciliation failure whose recovery flush
/// also failed.
#[derive(Debug, thiserror::Error)]
#[error("a sync operation failed; a subsequent flush also failed")]
pub struct FlushFailureComposite {
    #[source]
    pub primary: Box<SyncError>,
    pub flush_error: Box<SyncError>,
}

fn classify_atomic(source: &AtomicError) -> MutationAuthorityFailureKind {
    use crate::repository_access::{RepositoryAccessFailureKind, classify_atomic};
    match classify_atomic(source) {
        RepositoryAccessFailureKind::RepositoryLocked => {
            MutationAuthorityFailureKind::RepositoryLocked
        }
        RepositoryAccessFailureKind::Filesystem(kind) => {
            MutationAuthorityFailureKind::Filesystem(kind)
        }
        RepositoryAccessFailureKind::Lock(_) => unreachable!("atomic errors are not locks"),
    }
}

use crate::repository_access::classify_lock_kind as classify_lock;

fn classify_cache(source: &CacheError) -> CacheFailureKind {
    use gat_io::CacheProofErrorKind;

    match source {
        CacheError::DirectoryUnavailable { source, .. }
        | CacheError::TempFileUnavailable { source, .. }
        | CacheError::EntryUnwritable { source, .. } => match classify_io(source) {
            FilesystemFailureKind::PermissionDenied => CacheFailureKind::PermissionDenied,
            FilesystemFailureKind::StorageExhausted => CacheFailureKind::StorageExhausted,
            FilesystemFailureKind::Unavailable => CacheFailureKind::Unavailable,
        },
        CacheError::PathUnreadable { source, .. } | CacheError::SourceUnreadable { source } => {
            if source.kind() == std::io::ErrorKind::PermissionDenied {
                CacheFailureKind::PermissionDenied
            } else {
                CacheFailureKind::Unavailable
            }
        }
        CacheError::EntryUnreadable { source, .. } => {
            if source.kind() == std::io::ErrorKind::NotFound {
                CacheFailureKind::ObjectMissing
            } else {
                CacheFailureKind::Unavailable
            }
        }
        CacheError::MaterializationFailed { attempts, .. } => {
            CacheFailureKind::Materialization(attempts.iter().map(|(mode, _)| *mode).collect())
        }
        CacheError::State(source) => match source.kind() {
            CacheProofErrorKind::Unavailable => CacheFailureKind::StateUnavailable,
            CacheProofErrorKind::Incompatible => CacheFailureKind::StateIncompatible,
            CacheProofErrorKind::Corrupt => CacheFailureKind::StateCorrupt,
            CacheProofErrorKind::InvalidObjectIdentifier => CacheFailureKind::InvalidObjectId,
        },
        CacheError::Oid(_) => CacheFailureKind::InvalidObjectId,
        CacheError::Atomic(source) => match classify_atomic(source) {
            MutationAuthorityFailureKind::Filesystem(FilesystemFailureKind::PermissionDenied) => {
                CacheFailureKind::PermissionDenied
            }
            MutationAuthorityFailureKind::Filesystem(FilesystemFailureKind::StorageExhausted) => {
                CacheFailureKind::StorageExhausted
            }
            _ => CacheFailureKind::Unavailable,
        },
    }
}

fn classify_worktree_path(source: &WorktreePathError) -> (WorktreePathFailureKind, Option<String>) {
    match source {
        WorktreePathError::NotRelative { path }
        | WorktreePathError::ParentTraversal { path }
        | WorktreePathError::NotMaterializable { path }
        | WorktreePathError::NonUtf8Component { path }
        | WorktreePathError::EscapesWorktree { path }
        | WorktreePathError::SymlinkAncestor { path, .. } => (
            WorktreePathFailureKind::OutsideRepository,
            Some(path.clone()),
        ),
        WorktreePathError::ForbiddenInfrastructurePath { path } => (
            WorktreePathFailureKind::InfrastructurePath,
            Some(path.clone()),
        ),
        WorktreePathError::UnsupportedLeafSymlink { path } => (
            WorktreePathFailureKind::UnsupportedFileType,
            Some(path.clone()),
        ),
        WorktreePathError::Io { source, .. } => (
            WorktreePathFailureKind::Filesystem(classify_io(source)),
            None,
        ),
        WorktreePathError::Internal { .. } => (WorktreePathFailureKind::Internal, None),
    }
}

const fn classify_file_state(source: &FileStateError) -> FileStateFailureKind {
    match source {
        FileStateError::NotRegularFile { .. } => FileStateFailureKind::UnsupportedFileType,
        FileStateError::Observed { .. } => FileStateFailureKind::Conflict,
    }
}

fn classify_worktree_mutation(source: &WorktreeMutationError) -> SyncErrorKind {
    match source {
        WorktreeMutationError::Path(source) => {
            let (kind, path) = classify_worktree_path(source);
            SyncErrorKind::WorktreePath { kind, path }
        }
        WorktreeMutationError::Cache(source) => SyncErrorKind::Cache(classify_cache(source)),
        WorktreeMutationError::FileState(source) => {
            SyncErrorKind::FileState(classify_file_state(source))
        }
        WorktreeMutationError::NoFileName { path } => {
            SyncErrorKind::InvalidTrackedPath(path.clone())
        }
        WorktreeMutationError::Io { path, source, .. } => SyncErrorKind::WorktreeMutation {
            kind: classify_io(source),
            path: Some(path.clone()),
        },
        WorktreeMutationError::Prune(source) => SyncErrorKind::Prune(classify_io(&source.source)),
    }
}

fn classify_repository(
    source: &crate::repository::RepositoryError,
) -> MutationAuthorityFailureKind {
    use crate::repository::RepositoryError;

    match source {
        RepositoryError::ConfigLoad { scope, .. }
        | RepositoryError::ConfigLoadScoped { scope, .. }
        | RepositoryError::ConfigDirectoryCreate { scope, .. }
        | RepositoryError::ConfigSerialize { scope, .. }
        | RepositoryError::ConfigWrite { scope, .. } => {
            MutationAuthorityFailureKind::Configuration(Some(*scope), source.config_failure_kind())
        }
        RepositoryError::CurrentDirectory(source) => {
            MutationAuthorityFailureKind::Filesystem(classify_io(source))
        }
        RepositoryError::NotRepository
        | RepositoryError::ConfigPathUnavailable
        | RepositoryError::InvalidEffectiveMounts(_)
        | RepositoryError::InvalidEffectiveSelections(_)
        | RepositoryError::InvalidEffectiveRoutes(_) => {
            MutationAuthorityFailureKind::Configuration(
                None,
                crate::ConfigAccessFailureKind::Invalid,
            )
        }
    }
}

fn classify_excludes(source: &crate::excludes::ExcludesError) -> ExcludesFailureKind {
    use crate::excludes::ExcludesError;
    use gat_io::InfoExcludeError;

    match source {
        ExcludesError::Lock(source) => ExcludesFailureKind::Lock(classify_lock(source)),
        ExcludesError::Config(source) => {
            let kind = match classify_repository(source) {
                MutationAuthorityFailureKind::Configuration(scope, _) => scope,
                _ => None,
            };
            ExcludesFailureKind::Configuration(kind, source.config_failure_kind())
        }
        ExcludesError::StateStore(source) => ExcludesFailureKind::State(classify_state(source)),
        ExcludesError::Io(source) => match source {
            InfoExcludeError::OpenRepository(_) => ExcludesFailureKind::Repository,
            InfoExcludeError::Read { source, .. } | InfoExcludeError::Remove { source, .. } => {
                ExcludesFailureKind::Filesystem(classify_io(source))
            }
            InfoExcludeError::NotRegularFile { .. } => ExcludesFailureKind::UnsupportedFileType,
            InfoExcludeError::Write(source) => match classify_atomic(source) {
                MutationAuthorityFailureKind::Filesystem(kind) => {
                    ExcludesFailureKind::Filesystem(kind)
                }
                _ => ExcludesFailureKind::Filesystem(FilesystemFailureKind::Unavailable),
            },
            InfoExcludeError::ConcurrentModification { .. } => ExcludesFailureKind::Conflict,
            InfoExcludeError::FileState(_) => {
                ExcludesFailureKind::Filesystem(FilesystemFailureKind::Unavailable)
            }
        },
    }
}

fn classify_mutation_authority(
    source: &crate::repository_state::DesiredRevisionError,
) -> MutationAuthorityFailureKind {
    use crate::repository::RepoError;
    use crate::repository_access::RepositoryAccessFailureKind;
    use crate::repository_state::DesiredRevisionError;

    let access = |kind| match kind {
        RepositoryAccessFailureKind::Lock(kind) => {
            let kind = match kind {
                crate::repository_access::LockFailureKind::Corrupt => LockFailureKind::Corrupt,
                crate::repository_access::LockFailureKind::Incompatible => {
                    LockFailureKind::Incompatible
                }
                crate::repository_access::LockFailureKind::InvalidPath => {
                    LockFailureKind::InvalidPath
                }
                crate::repository_access::LockFailureKind::UnsupportedFileType => {
                    LockFailureKind::UnsupportedFileType
                }
                crate::repository_access::LockFailureKind::PermissionDenied => {
                    LockFailureKind::PermissionDenied
                }
                crate::repository_access::LockFailureKind::StorageExhausted => {
                    LockFailureKind::StorageExhausted
                }
                crate::repository_access::LockFailureKind::Unavailable => {
                    LockFailureKind::Unavailable
                }
                crate::repository_access::LockFailureKind::RepairRequired => {
                    LockFailureKind::RepairRequired
                }
                crate::repository_access::LockFailureKind::RepositoryLocked => {
                    LockFailureKind::RepositoryLocked
                }
                crate::repository_access::LockFailureKind::InvalidArgument => {
                    LockFailureKind::InvalidArgument
                }
            };
            MutationAuthorityFailureKind::Lock(kind)
        }
        RepositoryAccessFailureKind::Filesystem(kind) => {
            let kind = match kind {
                crate::repository_access::FilesystemFailureKind::PermissionDenied => {
                    FilesystemFailureKind::PermissionDenied
                }
                crate::repository_access::FilesystemFailureKind::StorageExhausted => {
                    FilesystemFailureKind::StorageExhausted
                }
                crate::repository_access::FilesystemFailureKind::Unavailable => {
                    FilesystemFailureKind::Unavailable
                }
            };
            MutationAuthorityFailureKind::Filesystem(kind)
        }
        RepositoryAccessFailureKind::RepositoryLocked => {
            MutationAuthorityFailureKind::RepositoryLocked
        }
    };

    match source {
        DesiredRevisionError::Lock(source) | DesiredRevisionError::Atomic(source) => {
            access(source.kind())
        }
        DesiredRevisionError::Repository(source) => match source {
            RepoError::Config(source) => classify_repository(source),
            RepoError::Lock(source) | RepoError::Atomic(source) => access(source.kind()),
            RepoError::RemoteConfig(_) => MutationAuthorityFailureKind::InvalidArgument,
        },
        DesiredRevisionError::Stale(_) => MutationAuthorityFailureKind::Conflict,
    }
}

impl From<LockError> for SyncError {
    fn from(source: LockError) -> Self {
        Self::new(SyncErrorKind::Lock(classify_lock(&source)), source)
    }
}

impl From<StateStoreError> for SyncError {
    fn from(source: StateStoreError) -> Self {
        Self::new(SyncErrorKind::State(classify_state(&source)), source)
    }
}

impl From<gat_core::oid::OidFormatError> for SyncError {
    fn from(source: gat_core::oid::OidFormatError) -> Self {
        Self::new(SyncErrorKind::InvalidObjectId, source)
    }
}

impl From<AtomicError> for SyncError {
    fn from(source: AtomicError) -> Self {
        let kind = SyncErrorKind::MutationAuthority(classify_atomic(&source));
        Self::new(kind, source)
    }
}

impl From<CacheError> for SyncError {
    fn from(source: CacheError) -> Self {
        Self::new(SyncErrorKind::Cache(classify_cache(&source)), source)
    }
}

impl From<WorktreePathError> for SyncError {
    fn from(source: WorktreePathError) -> Self {
        let (kind, path) = classify_worktree_path(&source);
        Self::new(SyncErrorKind::WorktreePath { kind, path }, source)
    }
}

impl From<FileStateError> for SyncError {
    fn from(source: FileStateError) -> Self {
        Self::new(
            SyncErrorKind::FileState(classify_file_state(&source)),
            source,
        )
    }
}

impl From<WorktreeMutationError> for SyncError {
    fn from(source: WorktreeMutationError) -> Self {
        Self::new(classify_worktree_mutation(&source), source)
    }
}

impl From<Box<crate::excludes::ExcludesError>> for SyncError {
    fn from(source: Box<crate::excludes::ExcludesError>) -> Self {
        let kind = SyncErrorKind::Excludes(classify_excludes(&source));
        Self::new(kind, source)
    }
}

impl From<PruneError> for SyncError {
    fn from(source: PruneError) -> Self {
        let kind = SyncErrorKind::Prune(classify_io(&source.source));
        Self::new(kind, source)
    }
}

impl From<crate::repository_state::DesiredRevisionError> for SyncError {
    fn from(source: crate::repository_state::DesiredRevisionError) -> Self {
        let kind = SyncErrorKind::MutationAuthority(classify_mutation_authority(&source));
        Self::new(kind, source)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn lock_path_is_hidden_but_typed_source_is_retained() {
        let marker = "private-lock-path-marker";
        let error = SyncError::from(LockError::Atomic(AtomicError::LockTimedOut {
            path: PathBuf::from(marker),
        }));

        assert_eq!(
            error.kind(),
            &SyncErrorKind::Lock(LockFailureKind::RepositoryLocked)
        );
        assert!(!error.to_string().contains(marker));
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<LockError>()
                .is_some()
        );
    }

    #[test]
    fn corrupt_state_detail_is_hidden_but_typed_source_is_retained() {
        let marker = "private-state-detail-marker";
        let error = SyncError::from(StateStoreError::InvalidRow {
            detail: marker.to_string(),
        });

        assert_eq!(
            error.kind(),
            &SyncErrorKind::State(StateFailureKind::Corrupt)
        );
        assert!(!error.to_string().contains(marker));
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<StateStoreError>()
                .is_some()
        );
    }

    #[test]
    fn flush_failure_retains_both_complete_sync_errors() {
        let error = SyncError::flush_after_failure(
            SyncError::missing_materialized_store(),
            SyncError::missing_materialized_store(),
        );
        let composite = error
            .source()
            .unwrap()
            .downcast_ref::<FlushFailureComposite>()
            .expect("flush composite should remain the direct source");

        assert_eq!(composite.primary.kind(), &SyncErrorKind::Internal);
        assert_eq!(composite.flush_error.kind(), &SyncErrorKind::Internal);
        assert!(error.flush_failed());
    }
}
