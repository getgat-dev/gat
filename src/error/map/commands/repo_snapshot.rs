//! `Failure` mapping for coherent repository-snapshot acquisition.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use gat_engine::{LockFailureKind, RepoSnapshotError, RepoSnapshotErrorKind, SnapshotFailureKind};

use super::super::{filesystem_code, lock_code, state_code};

const fn config_scope_word(scope: gat_core::config::ConfigScope) -> &'static str {
    match scope {
        gat_core::config::ConfigScope::Global => "global",
        gat_core::config::ConfigScope::Project => "project",
        gat_core::config::ConfigScope::Local => "local",
    }
}

impl From<RepoSnapshotError> for Failure {
    fn from(err: RepoSnapshotError) -> Self {
        match err.kind() {
            RepoSnapshotErrorKind::Repository => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::RepositoryUnavailable,
                    "Could not prepare the repository",
                ),
                err,
            ),
            RepoSnapshotErrorKind::RepositoryLocked => Self::expected_with_source(
                Diagnostic::new(
                    ErrorCode::RepositoryLocked,
                    "Another gat process is modifying this repository",
                )
                .with_detail("It will release the lock automatically when it finishes or exits."),
                err,
            ),
            RepoSnapshotErrorKind::RepositoryLock(kind) => Self::infrastructure(
                Diagnostic::new(
                    filesystem_code(kind),
                    "Could not acquire the repository lock",
                ),
                err,
            ),
            RepoSnapshotErrorKind::MountRecovery(kind) => {
                Self::infrastructure(super::super::mount::recovery_diagnostic(kind), err)
            }
            RepoSnapshotErrorKind::Configuration(scope, kind) => {
                let summary = match scope {
                    Some(scope) => crate::presentation::UserLine::compose([
                        crate::presentation::UserLine::authored("Could not load the "),
                        crate::presentation::UserLine::authored(config_scope_word(scope)),
                        crate::presentation::UserLine::authored(" gat.yaml"),
                    ]),
                    None => crate::presentation::UserLine::authored(
                        "Could not load the repository configuration",
                    ),
                };
                Self::infrastructure(
                    super::super::repository::configuration_diagnostic(kind, summary),
                    err,
                )
            }
            RepoSnapshotErrorKind::State(kind) => Self::infrastructure(
                Diagnostic::new(state_code(kind), "Could not open Gat's local state"),
                err,
            ),
            RepoSnapshotErrorKind::Lock(kind) => {
                let diagnostic = Diagnostic::new(lock_code(kind), "Could not read gat.lock");
                match kind {
                    LockFailureKind::Corrupt
                    | LockFailureKind::Incompatible
                    | LockFailureKind::InvalidPath
                    | LockFailureKind::UnsupportedFileType
                    | LockFailureKind::RepositoryLocked
                    | LockFailureKind::InvalidArgument => {
                        Self::expected_with_source(diagnostic, err)
                    }
                    LockFailureKind::PermissionDenied
                    | LockFailureKind::StorageExhausted
                    | LockFailureKind::Unavailable
                    | LockFailureKind::RepairRequired => Self::infrastructure(diagnostic, err),
                }
            }
            RepoSnapshotErrorKind::Snapshot(kind) => {
                let summary = match kind {
                    SnapshotFailureKind::RemoteConfiguration => {
                        "The effective remote configuration is invalid"
                    }
                    SnapshotFailureKind::PathPolicy => {
                        "The effective path policy configuration is invalid"
                    }
                };
                Self::expected_with_source(Diagnostic::new(ErrorCode::InvalidConfig, summary), err)
            }
            RepoSnapshotErrorKind::Filesystem(kind) => Self::infrastructure(
                Diagnostic::new(
                    filesystem_code(kind),
                    "Could not access repository state files",
                ),
                err,
            ),
            RepoSnapshotErrorKind::Cache => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::CacheUnavailable,
                    "Could not access the local object cache",
                ),
                err,
            ),
            RepoSnapshotErrorKind::InvalidPath => Self::expected_with_source(
                Diagnostic::new(
                    ErrorCode::InvalidPath,
                    "Repository state contains an invalid path",
                ),
                err,
            ),
            RepoSnapshotErrorKind::InvalidArgument => Self::expected_with_source(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "Could not resolve the repository selection",
                ),
                err,
            ),
            RepoSnapshotErrorKind::Conflict => Self::expected_with_source(
                Diagnostic::new(
                    ErrorCode::Conflict,
                    "The desired state changed during snapshot acquisition",
                )
                .with_hint("Run the command again to observe the current state."),
                err,
            ),
            RepoSnapshotErrorKind::Internal => Self::internal(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use gat_io::AtomicError;
    use gat_io::StateStoreError;

    #[test]
    fn repository_lock_path_is_hidden_while_the_engine_error_is_retained() {
        let marker = "/private/repository/.gat/state/sync.lock";
        let failure: Failure = RepoSnapshotError::from(AtomicError::LockTimedOut {
            path: PathBuf::from(marker),
        })
        .into();
        let diagnostic = failure.diagnostic();

        assert_eq!(diagnostic.code(), ErrorCode::RepositoryLocked);
        assert!(!diagnostic.summary().contains(marker));
        assert!(
            diagnostic
                .detail()
                .is_none_or(|detail| !detail.contains(marker))
        );
        assert!(
            failure
                .technical_source()
                .and_then(|source| source.downcast_ref::<RepoSnapshotError>())
                .is_some()
        );
    }

    #[test]
    fn state_storage_details_are_hidden_but_semantic_classification_survives() {
        let marker = "private-state-row-marker";
        let failure: Failure = RepoSnapshotError::from(StateStoreError::InvalidRow {
            detail: marker.to_string(),
        })
        .into();
        let diagnostic = failure.diagnostic();

        assert_eq!(diagnostic.code(), ErrorCode::StateCorrupt);
        assert!(!diagnostic.summary().contains(marker));
        assert!(
            diagnostic
                .detail()
                .is_none_or(|detail| !detail.contains(marker))
        );
    }
}
