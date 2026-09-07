//! `Failure` mapping for engine-owned reconciliation failures.

use gat_engine::{
    SyncCacheFailureKind as CacheFailureKind, SyncError, SyncErrorKind,
    SyncExcludesFailureKind as ExcludesFailureKind,
    SyncFileStateFailureKind as FileStateFailureKind, SyncLockFailureKind as LockFailureKind,
    SyncMutationAuthorityFailureKind as MutationAuthorityFailureKind,
    SyncStateFailureKind as StateFailureKind,
    SyncWorktreePathFailureKind as WorktreePathFailureKind,
};

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;

impl From<SyncError> for Failure {
    fn from(err: SyncError) -> Self {
        let mut diagnostic = classify(&err);
        if err.flush_failed() {
            diagnostic = diagnostic.with_hint(
                "Additionally, Gat could not persist some already-applied state changes; \
                 run `gat sync` again to retry.",
            );
        }
        match err.kind() {
            SyncErrorKind::InvalidTrackedPath(_)
            | SyncErrorKind::WorktreePath {
                kind:
                    WorktreePathFailureKind::OutsideRepository
                    | WorktreePathFailureKind::InfrastructurePath
                    | WorktreePathFailureKind::UnsupportedFileType,
                ..
            }
            | SyncErrorKind::FileState(_)
            | SyncErrorKind::Excludes(
                ExcludesFailureKind::UnsupportedFileType | ExcludesFailureKind::Conflict,
            )
            | SyncErrorKind::Lock(
                LockFailureKind::Corrupt
                | LockFailureKind::Incompatible
                | LockFailureKind::InvalidPath
                | LockFailureKind::UnsupportedFileType
                | LockFailureKind::RepositoryLocked
                | LockFailureKind::InvalidArgument,
            )
            | SyncErrorKind::Cache(
                CacheFailureKind::StateIncompatible | CacheFailureKind::StateCorrupt,
            )
            | SyncErrorKind::MutationAuthority(
                MutationAuthorityFailureKind::RepositoryLocked
                | MutationAuthorityFailureKind::InvalidArgument
                | MutationAuthorityFailureKind::Conflict,
            ) => Self::expected_with_source(diagnostic, err),
            SyncErrorKind::Internal
            | SyncErrorKind::InvalidObjectId
            | SyncErrorKind::Filesystem(_)
            | SyncErrorKind::State(_)
            | SyncErrorKind::Cache(_)
            | SyncErrorKind::WorktreePath { .. }
            | SyncErrorKind::WorktreeMutation { .. }
            | SyncErrorKind::Excludes(_)
            | SyncErrorKind::Prune(_)
            | SyncErrorKind::MutationAuthority(_)
            | SyncErrorKind::Lock(_) => Self::infrastructure(diagnostic, err),
        }
    }
}

use super::{filesystem_code, state_code};

pub(super) fn classify(err: &SyncError) -> Diagnostic {
    classify_kind(err.kind())
}

pub(super) fn classify_kind(kind: &SyncErrorKind) -> Diagnostic {
    match kind {
        SyncErrorKind::Internal => {
            Diagnostic::new(ErrorCode::Internal, "Gat hit an unexpected internal error")
        }
        SyncErrorKind::InvalidTrackedPath(path) => {
            Diagnostic::new(ErrorCode::InvalidPath, "gat.lock tracks an invalid path")
                .with_subject(UserLine::path_text(path))
        }
        SyncErrorKind::Filesystem(kind) => Diagnostic::new(
            filesystem_code(*kind),
            "Could not synchronize the working tree",
        ),
        SyncErrorKind::Lock(kind) => super::repository::repository_access_diagnostic(
            gat_engine::RepositoryAccessFailureKind::Lock(*kind),
        ),
        SyncErrorKind::State(kind) => Diagnostic::new(state_code(*kind), state_summary(*kind)),
        SyncErrorKind::InvalidObjectId => {
            Diagnostic::new(ErrorCode::Internal, "An object identifier was malformed")
        }
        SyncErrorKind::Cache(kind) => classify_cache(kind),
        SyncErrorKind::WorktreePath { kind, path } => {
            let (code, summary, hint) = match kind {
                WorktreePathFailureKind::OutsideRepository => (
                    ErrorCode::PathOutsideRepository,
                    "Path escapes the repository root",
                    Some("Pass a path that stays inside the repository root."),
                ),
                WorktreePathFailureKind::InfrastructurePath => (
                    ErrorCode::InvalidPath,
                    "This path is gat/git infrastructure and can never be tracked",
                    Some("gat's own `.git`/`.gat` directories can never be tracked."),
                ),
                WorktreePathFailureKind::UnsupportedFileType => (
                    ErrorCode::UnsupportedFileType,
                    "This path is a symlink; gat does not track symlinks",
                    Some("Pass the symlink's real target instead, if you want gat to track it."),
                ),
                WorktreePathFailureKind::Filesystem(kind) => {
                    (filesystem_code(*kind), "Could not access this path", None)
                }
                WorktreePathFailureKind::Internal => (
                    ErrorCode::Internal,
                    "Gat hit an unexpected internal error",
                    None,
                ),
            };
            let mut diagnostic = Diagnostic::new(code, summary);
            if let Some(path) = path {
                diagnostic = diagnostic.with_subject(UserLine::path_text(path));
            }
            if let Some(hint) = hint {
                diagnostic = diagnostic.with_hint(hint);
            }
            diagnostic
        }
        SyncErrorKind::FileState(kind) => match kind {
            FileStateFailureKind::UnsupportedFileType => Diagnostic::new(
                ErrorCode::UnsupportedFileType,
                "Expected an existing regular file",
            ),
            FileStateFailureKind::Conflict => {
                Diagnostic::new(ErrorCode::Conflict, "A file was modified concurrently")
                    .with_hint("re-run the command; another process is writing to this file")
            }
        },
        SyncErrorKind::WorktreeMutation { kind, path } => {
            let mut diagnostic = Diagnostic::new(
                filesystem_code(*kind),
                "Could not synchronize the working tree",
            );
            if let Some(path) = path {
                diagnostic = diagnostic.with_subject(UserLine::path_text(path));
            }
            diagnostic
        }
        SyncErrorKind::Excludes(kind) => classify_excludes(kind),
        SyncErrorKind::Prune(kind) => Diagnostic::new(
            filesystem_code(*kind),
            "Could not remove an empty directory",
        ),
        SyncErrorKind::MutationAuthority(kind) => classify_mutation_authority(*kind),
    }
}

const fn state_summary(kind: StateFailureKind) -> &'static str {
    match kind {
        StateFailureKind::Busy
        | StateFailureKind::PermissionDenied
        | StateFailureKind::StorageExhausted
        | StateFailureKind::Unavailable => "Could not read or write Gat's local state",
        StateFailureKind::Corrupt => "Gat's local state is corrupted",
        StateFailureKind::Incompatible => "Gat's local state uses an unsupported format version",
        StateFailureKind::InvalidObjectId => "An object identifier was malformed",
    }
}

fn classify_cache(kind: &CacheFailureKind) -> Diagnostic {
    match kind {
        CacheFailureKind::PermissionDenied => Diagnostic::new(
            ErrorCode::PermissionDenied,
            "Could not access the local object cache",
        ),
        CacheFailureKind::ObjectMissing => {
            Diagnostic::new(ErrorCode::ObjectMissing, "Could not read a cached object")
        }
        CacheFailureKind::StateUnavailable => Diagnostic::new(
            ErrorCode::StateUnavailable,
            "Could not read or write Gat's local cache metadata",
        ),
        CacheFailureKind::StateIncompatible => Diagnostic::new(
            ErrorCode::StateIncompatible,
            "Gat's local cache uses an unsupported format version",
        ),
        CacheFailureKind::StateCorrupt => Diagnostic::new(
            ErrorCode::StateCorrupt,
            "Gat's local cache metadata is corrupt",
        )
        .with_hint("Run `gat system repair cache` to rebuild it."),
        CacheFailureKind::StorageExhausted => Diagnostic::new(
            ErrorCode::StorageExhausted,
            "Could not access the local object cache",
        ),
        CacheFailureKind::Unavailable => Diagnostic::new(
            ErrorCode::CacheUnavailable,
            "Could not access the local object cache",
        ),
        CacheFailureKind::InvalidObjectId => {
            Diagnostic::new(ErrorCode::Internal, "An object identifier was malformed")
        }
        CacheFailureKind::Materialization(modes) => {
            let modes = modes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            Diagnostic::new(
                ErrorCode::CacheUnavailable,
                "Could not materialize a cached object",
            )
            .with_hint(UserLine::compose([
                UserLine::authored("Every configured cache.materialization_strategy mode failed ("),
                UserLine::identifier(&modes),
                UserLine::authored(
                    "); run `gat config cache.materialization_strategy <mode> [<mode>...]` \
                     to change the fallback chain (e.g. `copy` works on the widest range of \
                     filesystems)",
                ),
            ]))
        }
    }
}

fn classify_excludes(kind: &ExcludesFailureKind) -> Diagnostic {
    match kind {
        ExcludesFailureKind::Repository => Diagnostic::new(
            ErrorCode::NotRepository,
            "Could not open the Git repository",
        ),
        ExcludesFailureKind::Lock(kind) => super::repository::repository_access_diagnostic(
            gat_engine::RepositoryAccessFailureKind::Lock(*kind),
        ),
        ExcludesFailureKind::Configuration(scope, kind) => {
            let summary = match scope {
                Some(gat_core::config::ConfigScope::Global) => {
                    "Could not update global configuration"
                }
                Some(gat_core::config::ConfigScope::Project) => {
                    "Could not update project configuration"
                }
                Some(gat_core::config::ConfigScope::Local) => {
                    "Could not update local configuration"
                }
                None => "Could not load repository configuration",
            };
            super::repository::configuration_diagnostic(*kind, summary.into())
        }
        ExcludesFailureKind::State(kind) => {
            Diagnostic::new(state_code(*kind), state_summary(*kind))
        }
        ExcludesFailureKind::Filesystem(kind) => Diagnostic::new(
            filesystem_code(*kind),
            "Could not regenerate .git/info/exclude",
        ),
        ExcludesFailureKind::UnsupportedFileType => Diagnostic::new(
            ErrorCode::UnsupportedFileType,
            ".git/info/exclude is not a regular file",
        )
        .with_hint("gat refuses to manage a symlink or other special file here"),
        ExcludesFailureKind::Conflict => Diagnostic::new(
            ErrorCode::Conflict,
            ".git/info/exclude was modified concurrently",
        )
        .with_hint("re-run the command; another process is writing to this file"),
    }
}

fn classify_mutation_authority(kind: MutationAuthorityFailureKind) -> Diagnostic {
    match kind {
        MutationAuthorityFailureKind::Lock(kind) => {
            super::repository::repository_access_diagnostic(
                gat_engine::RepositoryAccessFailureKind::Lock(kind),
            )
        }

        MutationAuthorityFailureKind::RepositoryLocked => Diagnostic::new(
            ErrorCode::RepositoryLocked,
            "Another gat process is modifying this repository",
        )
        .with_detail("It will release the lock automatically when it finishes or exits."),
        MutationAuthorityFailureKind::Filesystem(kind) => Diagnostic::new(
            filesystem_code(kind),
            "Could not confirm exclusive access to gat.lock for this sync",
        ),
        MutationAuthorityFailureKind::Configuration(scope, kind) => {
            let summary = match scope {
                Some(gat_core::config::ConfigScope::Global) => {
                    "Could not load global configuration"
                }
                Some(gat_core::config::ConfigScope::Project) => {
                    "Could not load project configuration"
                }
                Some(gat_core::config::ConfigScope::Local) => "Could not load local configuration",
                None => "Could not load repository configuration",
            };
            super::repository::configuration_diagnostic(kind, summary.into())
        }
        MutationAuthorityFailureKind::InvalidArgument => Diagnostic::new(
            ErrorCode::InvalidArgumentValue,
            "Repository configuration is invalid",
        ),
        MutationAuthorityFailureKind::Conflict => Diagnostic::new(
            ErrorCode::Conflict,
            "The desired state (gat.lock) changed during this operation",
        )
        .with_hint("Run the command again to observe the current state."),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::PathBuf;

    use gat_io::AtomicError;
    use gat_io::LockError;
    use gat_io::StateStoreError;

    use super::*;

    #[test]
    fn lock_diagnostic_hides_physical_path_and_retains_sync_error() {
        let marker = "private-lock-path-marker";
        let error = SyncError::from(LockError::Atomic(AtomicError::LockTimedOut {
            path: PathBuf::from(marker),
        }));
        let failure: Failure = error.into();
        let diagnostic = failure.diagnostic();

        assert_eq!(diagnostic.code(), ErrorCode::RepositoryLocked);
        assert_eq!(diagnostic.subject(), None);
        assert!(!diagnostic.summary().contains(marker));
        assert!(
            failure
                .technical_source()
                .and_then(|source| source.downcast_ref::<SyncError>())
                .is_some()
        );
    }

    #[test]
    fn state_diagnostic_hides_raw_persisted_text_and_retains_sync_error() {
        let marker = "private-state-detail-marker";
        let error = SyncError::from(StateStoreError::InvalidRow {
            detail: marker.to_string(),
        });
        let failure: Failure = error.into();
        let diagnostic = failure.diagnostic();

        assert_eq!(diagnostic.code(), ErrorCode::StateCorrupt);
        assert!(!diagnostic.summary().contains(marker));
        assert!(!diagnostic.detail().unwrap_or_default().contains(marker));
        let source = failure
            .technical_source()
            .and_then(|source| source.downcast_ref::<SyncError>())
            .expect("the complete sync error should be retained");
        assert!(
            source
                .source()
                .unwrap()
                .downcast_ref::<StateStoreError>()
                .is_some()
        );
    }
    #[test]
    fn internal_failure_keeps_partial_state_flush_context() {
        let primary: gat_engine::SyncError = gat_io::WorktreePathError::Internal {
            detail: "SENTINEL_PRIMARY".into(),
        }
        .into();
        let flush: gat_engine::SyncError = gat_io::StateStoreError::InvalidRow {
            detail: "SENTINEL_FLUSH".into(),
        }
        .into();
        let failure: Failure = gat_engine::test_support::sync_flush_failure(primary, flush).into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::Internal);
        assert!(
            failure
                .diagnostic()
                .hints()
                .join(" ")
                .contains("already-applied")
        );
        assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
    }

    #[test]
    fn path_io_and_configuration_causes_survive_reconciliation() {
        let failure: Failure = gat_engine::SyncError::from(gat_io::WorktreePathError::Io {
            operation: "SENTINEL_OPERATION",
            path: "SENTINEL_PATH".into(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "SENTINEL_SOURCE"),
        })
        .into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::PermissionDenied);
        assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));

        let failure: Failure =
            gat_engine::SyncError::from(gat_engine::DesiredRevisionError::Repository(
                gat_engine::RepoError::Config(Box::new(gat_engine::RepositoryError::ConfigLoad {
                    scope: gat_core::config::ConfigScope::Project,
                    source: gat_io::ConfigError::UnsupportedVersion {
                        path: "SENTINEL_PATH".into(),
                        found: 99,
                        expected: 1,
                    },
                })),
            ))
            .into();
        assert_eq!(
            failure.diagnostic().code(),
            ErrorCode::UnsupportedConfigVersion
        );
        assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
    }
}
