//! `Failure` mapping for `gat_engine::CompareError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_engine::{CompareError, CompareErrorKind};

impl From<CompareError> for Failure {
    fn from(err: CompareError) -> Self {
        match err.kind() {
            CompareErrorKind::Acquisition(kind) => {
                super::repo_snapshot::acquisition_failure(*kind, err)
            }
            CompareErrorKind::Repository => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::RepositoryUnavailable,
                    "Could not open the git repository",
                ),
                err,
            ),
            CompareErrorKind::GitIndex => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::GitOperationFailed,
                    "Could not read the git index",
                ),
                err,
            ),
            CompareErrorKind::Revision(revision) => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "Could not resolve that revision",
                )
                .with_subject(UserLine::identifier(revision.as_str()))
                .with_hint("Check the revision/branch/tag name and try again."),
                err,
            ),
            CompareErrorKind::GitObject => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::GitOperationFailed,
                    "Could not read a persisted gat.lock from git",
                ),
                err,
            ),
            CompareErrorKind::InvalidSnapshot { label } => Self::expected_with_source(
                Diagnostic::new(
                    ErrorCode::ObjectCorrupt,
                    "A persisted gat.lock is not valid",
                )
                .with_subject(UserLine::identifier(label))
                .with_hint(
                    "This revision's gat.lock was written by an incompatible tool or is corrupt.",
                ),
                err,
            ),
            CompareErrorKind::State(kind) => Self::infrastructure(
                comparison_diagnostic(&gat_engine::SyncErrorKind::State(*kind)),
                err,
            ),
            CompareErrorKind::Lock(kind) => Self::infrastructure(
                comparison_diagnostic(&gat_engine::SyncErrorKind::Lock(*kind)),
                err,
            ),
            CompareErrorKind::DesiredState(kind) => {
                Self::infrastructure(comparison_diagnostic(kind), err)
            }
        }
    }
}

fn comparison_diagnostic(kind: &gat_engine::SyncErrorKind) -> Diagnostic {
    super::super::sync::classify_kind(kind)
        .with_detail("Could not prepare repository state for comparison.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_and_lock_causes_survive_comparison_without_private_details() {
        let state: CompareError = gat_io::StateStoreError::InvalidRow {
            detail: "SENTINEL_STATE".into(),
        }
        .into();
        let failure: Failure = state.into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::StateCorrupt);
        assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
        let lock: CompareError = gat_engine::SyncError::from(gat_io::AtomicError::LockTimedOut {
            path: "SENTINEL_LOCK".into(),
        })
        .into();
        let failure: Failure = lock.into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::RepositoryLocked);
        assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
        assert!(failure.technical_source().is_some());
    }
}
