//! `Failure` mapping for [`gat_engine::DesiredRevisionError`].

use super::super::{Diagnostic, ErrorCode, Failure};
use gat_engine::DesiredRevisionError;

impl From<DesiredRevisionError> for Failure {
    fn from(err: DesiredRevisionError) -> Self {
        match err {
            DesiredRevisionError::Lock(source) => {
                let kind = source.kind();
                super::repository::repository_access_failure(
                    kind,
                    DesiredRevisionError::Lock(source),
                )
            }
            DesiredRevisionError::Atomic(source) => {
                let kind = source.kind();
                super::repository::repository_access_failure(
                    kind,
                    DesiredRevisionError::Atomic(source),
                )
            }
            DesiredRevisionError::Repository(source) => match source {
                gat_engine::RepoError::Lock(source) => {
                    let kind = source.kind();
                    super::repository::repository_access_failure(
                        kind,
                        DesiredRevisionError::Repository(gat_engine::RepoError::Lock(source)),
                    )
                }
                gat_engine::RepoError::Atomic(source) => {
                    let kind = source.kind();
                    super::repository::repository_access_failure(
                        kind,
                        DesiredRevisionError::Repository(gat_engine::RepoError::Atomic(source)),
                    )
                }
                source => source.into(),
            },
            DesiredRevisionError::Stale(_) => Self::expected(
                Diagnostic::new(
                    ErrorCode::Conflict,
                    "The desired state (gat.lock) changed during this operation",
                )
                .with_hint("Run the command again to observe the current state."),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_engine::StaleDesiredRevisionError;
    use gat_engine::{RepositoryAccessError, RepositoryAccessFailureKind};

    #[test]
    fn desired_revision_lock_failure_hides_physical_paths_and_retains_the_outer_error() {
        const SENTINEL: &str = "sentinel-private-sync-lock";
        let err = DesiredRevisionError::Atomic(RepositoryAccessError::for_test(
            RepositoryAccessFailureKind::RepositoryLocked,
            std::io::Error::other(SENTINEL),
        ));

        let failure = Failure::from(err);
        assert_eq!(failure.diagnostic().code(), ErrorCode::RepositoryLocked);
        let rendered = format!(
            "{}{}{:?}",
            failure.diagnostic().summary(),
            failure.diagnostic().detail().unwrap_or_default(),
            failure.diagnostic().hints()
        );
        assert!(!rendered.contains(SENTINEL));

        let technical = failure
            .technical_source()
            .expect("desired revision access retains its complete source");
        assert!(technical.downcast_ref::<DesiredRevisionError>().is_some());
        assert!(
            std::iter::successors(Some(technical as &dyn std::error::Error), |error| error
                .source())
            .any(|error| error.to_string().contains(SENTINEL))
        );
    }

    #[test]
    fn stale_desired_revision_keeps_its_conflict_diagnostic() {
        let failure = Failure::from(DesiredRevisionError::Stale(StaleDesiredRevisionError));

        assert_eq!(failure.diagnostic().code(), ErrorCode::Conflict);
        assert_eq!(
            failure.diagnostic().summary(),
            "The desired state (gat.lock) changed during this operation"
        );
        assert_eq!(
            failure.diagnostic().hints(),
            ["Run the command again to observe the current state."]
        );
        assert!(failure.technical_source().is_none());
    }
}
