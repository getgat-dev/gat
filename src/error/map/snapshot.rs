//! `Failure` mapping for `gat_engine::SnapshotError`.

use super::super::{Diagnostic, ErrorCode, Failure};
use gat_engine::SnapshotError;

impl From<SnapshotError> for Failure {
    fn from(err: SnapshotError) -> Self {
        match &err {
            SnapshotError::RemoteCatalog(_) => Self::expected_with_source(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    "The effective remote configuration is invalid",
                ),
                err,
            ),
            SnapshotError::PathPolicy(_) => Self::expected_with_source(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    "The effective path policy configuration is invalid",
                ),
                err,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that expected snapshot failures retain their typed source:
    /// `SnapshotError` wraps a real typed `#[source]`-bearing inner error
    /// (`RemoteCatalogError`/`PathPolicyError`) -- it must not be silently
    /// dropped just because the classification is "expected".
    #[test]
    fn snapshot_remote_catalog_failure_retains_the_complete_typed_error_as_its_source() {
        let err = SnapshotError::RemoteCatalog(gat_engine::RemoteCatalogError::UnknownDefault {
            name: "origin".to_string(),
        });
        let failure: Failure = err.into();

        let source = failure
            .technical_source()
            .expect("expected_with_source must retain a technical source");
        let retained = source
            .downcast_ref::<SnapshotError>()
            .expect("the retained source must be the complete typed SnapshotError");
        assert!(matches!(retained, SnapshotError::RemoteCatalog(_)));
    }

    #[test]
    fn snapshot_path_policy_failure_retains_the_complete_typed_error_as_its_source() {
        let err = SnapshotError::PathPolicy(gat_engine::PathPolicyError::UnknownDefault {
            name: "assets".to_string(),
        });
        let failure: Failure = err.into();

        let source = failure
            .technical_source()
            .expect("expected_with_source must retain a technical source");
        let retained = source
            .downcast_ref::<SnapshotError>()
            .expect("the retained source must be the complete typed SnapshotError");
        assert!(matches!(retained, SnapshotError::PathPolicy(_)));
    }
}
