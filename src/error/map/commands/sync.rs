//! `Failure` mapping for sync command errors.

use super::super::super::Failure;

impl From<gat_command::SyncError> for Failure {
    fn from(err: gat_command::SyncError) -> Self {
        match err {
            gat_command::SyncError::Acquisition(source) => (*source).into(),
            gat_command::SyncError::Repository(source) => (*source).into(),
            gat_command::SyncError::Repo(source) => (*source).into(),
            gat_command::SyncError::DesiredRevision(source) => source.into(),
            gat_command::SyncError::Reconciliation(source) => source.into(),
            gat_command::SyncError::Fetch(source) => (*source).into(),
            gat_command::SyncError::Incomplete(source) => {
                let outcome = source.into_outcome();
                super::super::app::sync_completion_conflict(
                    outcome.outcome.conflicts.len(),
                    outcome.outcome.missing.len(),
                    outcome.outcome.corrupted.len(),
                )
            }
        }
    }
}
