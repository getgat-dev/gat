//! `Failure` mapping for engine-owned Git history errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_engine::{HistoryError, HistoryErrorKind};

impl From<HistoryError> for Failure {
    fn from(err: HistoryError) -> Self {
        match err.kind() {
            HistoryErrorKind::InvalidDate => {
                let input = err.subject().to_string();
                Self::infrastructure(
                    Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        "That date could not be parsed",
                    )
                    .with_subject(UserLine::identifier(&input)),
                    err,
                )
            }
            HistoryErrorKind::OpenRepository => {
                let diagnostic = Diagnostic::new(
                    ErrorCode::RepositoryUnavailable,
                    "Could not open the git repository",
                );
                let diagnostic = if let Some(root) = err.root() {
                    diagnostic.with_subject(UserLine::path(root))
                } else {
                    diagnostic
                };
                Self::infrastructure(diagnostic, err)
            }
            HistoryErrorKind::RevisionResolution => {
                let revision = err.subject().to_string();
                Self::infrastructure(
                    Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        "Could not resolve that revision",
                    )
                    .with_subject(UserLine::identifier(&revision))
                    .with_hint("Check the revision/branch/tag name and try again."),
                    err,
                )
            }
            HistoryErrorKind::Traversal | HistoryErrorKind::UnsupportedHashKind => {
                Self::infrastructure(
                    Diagnostic::new(
                        ErrorCode::GitOperationFailed,
                        "Could not read the repository's commit history",
                    ),
                    err,
                )
            }
            HistoryErrorKind::InvalidLockSnapshot => {
                let label = err.subject().to_string();
                Self::expected_with_source(
                    Diagnostic::new(
                        ErrorCode::ObjectCorrupt,
                        "A persisted gat.lock is not valid",
                    )
                    .with_subject(UserLine::identifier(&label))
                    .with_hint(
                        "This revision's gat.lock was written by an incompatible tool or is \
                         corrupt.",
                    ),
                    err,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn invalid_date_retains_the_engine_error_as_its_source() {
        let input = "not-a-date-7f86916d";
        let err = gat_engine::parse_cli_date(input).unwrap_err();
        let failure = Failure::from(err);

        assert_eq!(failure.diagnostic().code(), ErrorCode::InvalidArgumentValue);
        assert_eq!(failure.diagnostic().subject(), Some(input));
        assert!(
            failure
                .technical_source()
                .is_some_and(|source| source.downcast_ref::<HistoryError>().is_some())
        );
    }

    #[test]
    fn repository_open_failure_uses_the_production_history_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let error = repo
            .visit_history_commits::<HistoryError>(
                &gat_core::history::HistorySelection::default(),
                |_| Ok(()),
            )
            .unwrap_err();
        let failure = Failure::from(error);

        assert_eq!(
            failure.diagnostic().code(),
            ErrorCode::RepositoryUnavailable
        );
        assert_eq!(
            failure.diagnostic().summary(),
            "Could not open the git repository"
        );
        assert_eq!(
            failure.diagnostic().subject(),
            Some(tmp.path().to_string_lossy().as_ref())
        );

        let history = failure
            .technical_source()
            .and_then(|source| source.downcast_ref::<HistoryError>())
            .expect("the root boundary must retain the engine history error");
        let io_history = history
            .source()
            .and_then(|source| source.downcast_ref::<gat_io::GitHistoryError>())
            .expect("the engine error must retain the I/O history error");
        assert!(
            io_history
                .source()
                .and_then(|source| source.downcast_ref::<gat_io::GitOpenError>())
                .is_some(),
            "the I/O history error must retain the physical Git open failure"
        );
    }
}
