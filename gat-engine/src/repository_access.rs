//! Semantic failures while reading or mutating repository-owned state.

use gat_io::AtomicError;
use gat_io::LockError;

/// Semantic filesystem failure classes needed above the engine boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesystemFailureKind {
    PermissionDenied,
    StorageExhausted,
    Unavailable,
}

/// Semantic local-state failure classes needed by root diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateFailureKind {
    Busy,
    Corrupt,
    Incompatible,
    PermissionDenied,
    StorageExhausted,
    Unavailable,
    InvalidObjectId,
}

/// Semantic `gat.lock` failure classes needed above the engine boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockFailureKind {
    Corrupt,
    Incompatible,
    InvalidPath,
    UnsupportedFileType,
    PermissionDenied,
    StorageExhausted,
    Unavailable,
    RepairRequired,
    RepositoryLocked,
    InvalidArgument,
}

/// Application-facing category for repository state access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepositoryAccessFailureKind {
    Lock(LockFailureKind),
    Filesystem(FilesystemFailureKind),
    RepositoryLocked,
}

type BoxedSource = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Repository state access failure with its physical cause kept private.
#[derive(Debug)]
pub struct RepositoryAccessError {
    kind: RepositoryAccessFailureKind,
    source: BoxedSource,
}

impl RepositoryAccessError {
    fn new(
        kind: RepositoryAccessFailureKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RepositoryAccessFailureKind {
        self.kind
    }

    pub(crate) fn from_lock(source: LockError) -> Self {
        Self::new(classify_lock(&source), source)
    }

    pub(crate) fn from_atomic(source: AtomicError) -> Self {
        Self::new(classify_atomic(&source), source)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        kind: RepositoryAccessFailureKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::new(kind, source)
    }
}

impl std::fmt::Display for RepositoryAccessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let action = match self.kind {
            RepositoryAccessFailureKind::Lock(_) => "access gat.lock",
            RepositoryAccessFailureKind::Filesystem(_)
            | RepositoryAccessFailureKind::RepositoryLocked => {
                "acquire repository mutation authority"
            }
        };
        write!(formatter, "could not {action}")
    }
}

impl std::error::Error for RepositoryAccessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

pub(crate) fn classify_io(source: &std::io::Error) -> FilesystemFailureKind {
    classify_io_kind(source.kind())
}

pub(crate) const fn classify_io_kind(kind: std::io::ErrorKind) -> FilesystemFailureKind {
    match kind {
        std::io::ErrorKind::PermissionDenied => FilesystemFailureKind::PermissionDenied,
        std::io::ErrorKind::StorageFull => FilesystemFailureKind::StorageExhausted,
        _ => FilesystemFailureKind::Unavailable,
    }
}

pub(crate) fn classify_atomic(source: &AtomicError) -> RepositoryAccessFailureKind {
    match source.io_kind() {
        Some(kind) => RepositoryAccessFailureKind::Filesystem(classify_io_kind(kind)),
        None => RepositoryAccessFailureKind::RepositoryLocked,
    }
}

pub(crate) fn classify_lock(source: &LockError) -> RepositoryAccessFailureKind {
    RepositoryAccessFailureKind::Lock(classify_lock_kind(source))
}

pub(crate) fn classify_lock_kind(source: &LockError) -> LockFailureKind {
    use gat_core::lock::LockDomainError;
    match source {
        LockError::Domain(LockDomainError::UnsupportedVersion { .. }) => {
            LockFailureKind::Incompatible
        }
        LockError::Domain(LockDomainError::InvalidRowPath { .. }) | LockError::LexicalPath(_) => {
            LockFailureKind::InvalidPath
        }
        LockError::Domain(_)
        | LockError::CorruptShard { .. }
        | LockError::MixedShardTopology { .. }
        | LockError::NonUtf8Path { .. }
        | LockError::Persistence(_) => LockFailureKind::Corrupt,
        LockError::UnsupportedOnDiskKind { .. } => LockFailureKind::UnsupportedFileType,
        LockError::Io { source, .. } => lock_filesystem_kind(classify_io(source)),
        LockError::FileState(_) => LockFailureKind::RepairRequired,
        LockError::Atomic(source) => match classify_atomic(source) {
            RepositoryAccessFailureKind::RepositoryLocked => LockFailureKind::RepositoryLocked,
            RepositoryAccessFailureKind::Filesystem(kind) => lock_filesystem_kind(kind),
            RepositoryAccessFailureKind::Lock(_) => unreachable!("atomic errors are not locks"),
        },
        LockError::RowSource(_) => LockFailureKind::Unavailable,
    }
}

const fn lock_filesystem_kind(kind: FilesystemFailureKind) -> LockFailureKind {
    match kind {
        FilesystemFailureKind::PermissionDenied => LockFailureKind::PermissionDenied,
        FilesystemFailureKind::StorageExhausted => LockFailureKind::StorageExhausted,
        FilesystemFailureKind::Unavailable => LockFailureKind::Unavailable,
    }
}

pub(crate) fn classify_state(source: &gat_io::StateStoreError) -> StateFailureKind {
    use gat_io::{StateSqlErrorKind, StateStoreError};

    let sql = |kind| match kind {
        StateSqlErrorKind::Busy => StateFailureKind::Busy,
        StateSqlErrorKind::Corrupt => StateFailureKind::Corrupt,
        StateSqlErrorKind::PermissionDenied => StateFailureKind::PermissionDenied,
        StateSqlErrorKind::StorageExhausted => StateFailureKind::StorageExhausted,
        StateSqlErrorKind::Unavailable => StateFailureKind::Unavailable,
    };
    match source {
        StateStoreError::OpenFailed { source, .. }
        | StateStoreError::QueryFailed { source, .. } => sql(source.kind()),
        StateStoreError::DirectoryUnavailable { source, .. } => match classify_io(source) {
            FilesystemFailureKind::PermissionDenied => StateFailureKind::PermissionDenied,
            FilesystemFailureKind::StorageExhausted => StateFailureKind::StorageExhausted,
            FilesystemFailureKind::Unavailable => StateFailureKind::Unavailable,
        },
        StateStoreError::WalUnsupported { .. } => StateFailureKind::Unavailable,
        StateStoreError::UnsupportedSchemaVersion { .. } => StateFailureKind::Incompatible,
        StateStoreError::InvalidOid { .. } => StateFailureKind::InvalidObjectId,
        StateStoreError::InvalidRow { .. } => StateFailureKind::Corrupt,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn lock_timeout_is_semantic_and_retains_the_physical_source() {
        const SENTINEL: &str = "sentinel-private-sync-lock";
        let error = RepositoryAccessError::from_atomic(AtomicError::LockTimedOut {
            path: PathBuf::from(SENTINEL),
        });

        assert_eq!(error.kind(), RepositoryAccessFailureKind::RepositoryLocked);
        assert!(!error.to_string().contains(SENTINEL));
        assert!(
            std::iter::successors(Some(&error as &dyn std::error::Error), |source| source
                .source())
            .any(|source| source.to_string().contains(SENTINEL))
        );
    }

    #[test]
    fn atomic_failures_are_classified_without_exposing_scratch_paths() {
        const SCRATCH: &str = ".tmp-SENTINEL_SCRATCH_7fa21c9e";
        let generic_io = || std::io::Error::other("boom");
        let permission_io =
            || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let storage_full_io =
            || std::io::Error::new(std::io::ErrorKind::StorageFull, "storage full");

        let cases = [
            (
                AtomicError::DirectoryUnavailable {
                    path: PathBuf::from("state"),
                    source: generic_io(),
                },
                RepositoryAccessFailureKind::Filesystem(FilesystemFailureKind::Unavailable),
            ),
            (
                AtomicError::TempFileUnavailable {
                    dir: PathBuf::from("state"),
                    source: permission_io(),
                },
                RepositoryAccessFailureKind::Filesystem(FilesystemFailureKind::PermissionDenied),
            ),
            (
                AtomicError::WriteFailed {
                    path: PathBuf::from(SCRATCH),
                    source: generic_io(),
                },
                RepositoryAccessFailureKind::Filesystem(FilesystemFailureKind::Unavailable),
            ),
            (
                AtomicError::SyncFailed {
                    path: PathBuf::from("state"),
                    source: storage_full_io(),
                },
                RepositoryAccessFailureKind::Filesystem(FilesystemFailureKind::StorageExhausted),
            ),
            (
                AtomicError::PublishFailed {
                    path: PathBuf::from("state"),
                    source: storage_full_io(),
                },
                RepositoryAccessFailureKind::Filesystem(FilesystemFailureKind::StorageExhausted),
            ),
            (
                AtomicError::LockFileUnavailable {
                    path: PathBuf::from("sync.lock"),
                    source: permission_io(),
                },
                RepositoryAccessFailureKind::Filesystem(FilesystemFailureKind::PermissionDenied),
            ),
            (
                AtomicError::LockAcquireFailed {
                    path: PathBuf::from("sync.lock"),
                    source: generic_io(),
                },
                RepositoryAccessFailureKind::Filesystem(FilesystemFailureKind::Unavailable),
            ),
            (
                AtomicError::LockTimedOut {
                    path: PathBuf::from("sync.lock"),
                },
                RepositoryAccessFailureKind::RepositoryLocked,
            ),
        ];

        for (source, expected) in cases {
            let error = RepositoryAccessError::from_atomic(source);
            assert_eq!(error.kind(), expected);
            assert!(!error.to_string().contains(SCRATCH));
        }
    }

    #[test]
    fn lock_io_classification_does_not_expose_its_path() {
        const SENTINEL: &str = "sentinel-private-lock-shard";
        let error = RepositoryAccessError::from_lock(LockError::Io {
            operation: "read",
            path: PathBuf::from(SENTINEL),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        });

        assert_eq!(
            error.kind(),
            RepositoryAccessFailureKind::Lock(LockFailureKind::PermissionDenied)
        );
        assert!(!error.to_string().contains(SENTINEL));
        assert!(
            std::iter::successors(Some(&error as &dyn std::error::Error), |source| source
                .source())
            .any(|source| source.to_string().contains(SENTINEL))
        );
    }
}
