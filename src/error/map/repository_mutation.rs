//! Root-owned presentation for engine repository-state workflow errors.

use crate::error::{Diagnostic, ErrorCode, Failure};
use gat_engine::{RepositoryMutationError, RepositoryStateError};

impl From<RepositoryStateError> for Failure {
    fn from(err: RepositoryStateError) -> Self {
        let diagnostic = match &err {
            RepositoryStateError::Open { source } => {
                stage_diagnostic(source, "Gat couldn't open this repository's local state")
            }
            RepositoryStateError::Refresh { source } => stage_diagnostic(
                source,
                "Gat couldn't refresh this repository's tracked state",
            ),
            RepositoryStateError::Read { source } => {
                stage_diagnostic(source, "Gat couldn't read this repository's tracked state")
            }
            RepositoryStateError::Verify { source } => {
                stage_diagnostic(source, "Gat couldn't verify the selected working files")
            }
            RepositoryStateError::Discover { .. } => Diagnostic::new(
                ErrorCode::GitOperationFailed,
                "Gat couldn't discover files in this repository",
            ),
            RepositoryStateError::GatIgnore { source } => Diagnostic::new(
                super::io_code(source),
                "Gat couldn't read this repository's .gatignore",
            ),
            RepositoryStateError::Ingest { source } => {
                stage_diagnostic(source, "Gat couldn't store the selected file content")
            }
        };
        Self::infrastructure(diagnostic, err)
    }
}

impl From<RepositoryMutationError> for Failure {
    fn from(err: RepositoryMutationError) -> Self {
        let diagnostic = match &err {
            RepositoryMutationError::Stale => Diagnostic::new(
                ErrorCode::Conflict,
                "Tracked state changed during preparation; run the command again",
            ),
            RepositoryMutationError::Acquire { source } => {
                stage_diagnostic(source, "Gat couldn't start the repository update")
            }
            RepositoryMutationError::Read { source } => stage_diagnostic(
                source,
                "Gat couldn't read the repository state being updated",
            ),
            RepositoryMutationError::Publish { source } => {
                stage_diagnostic(source, "Gat couldn't publish the updated tracked state")
            }
            RepositoryMutationError::RecordMaterialized { source } => {
                stage_diagnostic(source, "Gat couldn't record the updated working files")
            }
            RepositoryMutationError::ForgetMaterialized { source } => {
                stage_diagnostic(source, "Gat couldn't relinquish the removed working files")
            }
            RepositoryMutationError::MoveMaterialized { source } => {
                stage_diagnostic(source, "Gat couldn't record the moved working files")
            }
            RepositoryMutationError::RegenerateExcludes { source } => {
                stage_diagnostic(source, "Gat couldn't update Git's managed exclusions")
            }
        };
        Self::infrastructure(diagnostic, err)
    }
}

fn stage_diagnostic(source: &gat_engine::SyncError, summary: &'static str) -> Diagnostic {
    let mut diagnostic = super::sync::classify(source);
    let cause = std::mem::replace(&mut diagnostic.summary, summary.into());
    diagnostic.with_detail(cause)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("SENTINEL_LOW_LEVEL_MUTATION_DETAIL")]
    struct Sentinel;

    #[test]
    fn engine_mutation_sources_are_retained_but_never_rendered() {
        let failure: Failure = RepositoryMutationError::Publish {
            source: Box::new(
                gat_io::AtomicError::PublishFailed {
                    path: "gat.lock".into(),
                    source: std::io::Error::other(Sentinel),
                }
                .into(),
            ),
        }
        .into();

        assert!(
            !failure
                .diagnostic()
                .summary()
                .to_string()
                .contains("SENTINEL_LOW_LEVEL_MUTATION_DETAIL")
        );
        assert!(failure.technical_source().is_some());
    }

    #[test]
    fn engine_preparation_sources_are_retained_but_never_rendered() {
        let failure: Failure = RepositoryStateError::Discover {
            source: Box::new(Sentinel),
        }
        .into();

        assert!(
            !failure
                .diagnostic()
                .summary()
                .to_string()
                .contains("SENTINEL_LOW_LEVEL_MUTATION_DETAIL")
        );
        assert!(failure.technical_source().is_some());
    }

    #[test]
    fn gatignore_physical_path_is_retained_only_as_a_technical_source() {
        const SENTINEL_PATH: &str = "/SENTINEL/physical/repository/.gatignore";

        let failure: Failure = RepositoryStateError::GatIgnore {
            source: std::io::Error::other(SENTINEL_PATH),
        }
        .into();

        assert_eq!(
            failure.diagnostic().summary(),
            "Gat couldn't read this repository's .gatignore"
        );
        assert_eq!(failure.diagnostic().subject(), None);
        assert!(
            !failure
                .diagnostic()
                .summary()
                .to_string()
                .contains(SENTINEL_PATH)
        );
        assert!(failure.technical_source().is_some_and(|source| {
            source
                .source()
                .is_some_and(|source| source.to_string().contains(SENTINEL_PATH))
        }));
    }
    #[test]
    fn mutation_stages_preserve_state_lock_and_filesystem_classifications() {
        fn causes() -> Vec<(gat_engine::SyncError, ErrorCode)> {
            vec![
                (
                    gat_io::StateStoreError::OpenFailed {
                        path: "SENTINEL".into(),
                        source: gat_io::StateSqlError::for_test(gat_io::StateSqlErrorKind::Busy),
                    }
                    .into(),
                    ErrorCode::StateBusy,
                ),
                (
                    gat_io::StateStoreError::InvalidRow {
                        detail: "SENTINEL".into(),
                    }
                    .into(),
                    ErrorCode::StateCorrupt,
                ),
                (
                    gat_io::StateStoreError::UnsupportedSchemaVersion {
                        path: "SENTINEL".into(),
                        found: 99,
                        expected: 1,
                    }
                    .into(),
                    ErrorCode::StateIncompatible,
                ),
                (
                    gat_io::AtomicError::LockTimedOut {
                        path: "SENTINEL".into(),
                    }
                    .into(),
                    ErrorCode::RepositoryLocked,
                ),
                (
                    gat_io::AtomicError::PublishFailed {
                        path: "SENTINEL".into(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "SENTINEL",
                        ),
                    }
                    .into(),
                    ErrorCode::PermissionDenied,
                ),
                (
                    gat_io::AtomicError::WriteFailed {
                        path: "SENTINEL".into(),
                        source: std::io::Error::new(std::io::ErrorKind::StorageFull, "SENTINEL"),
                    }
                    .into(),
                    ErrorCode::StorageExhausted,
                ),
            ]
        }
        for stage in 0..7 {
            for (cause, expected) in causes() {
                let source = Box::new(cause);
                let error = match stage {
                    0 => RepositoryMutationError::Acquire { source },
                    1 => RepositoryMutationError::Read { source },
                    2 => RepositoryMutationError::Publish { source },
                    3 => RepositoryMutationError::RecordMaterialized { source },
                    4 => RepositoryMutationError::ForgetMaterialized { source },
                    5 => RepositoryMutationError::MoveMaterialized { source },
                    _ => RepositoryMutationError::RegenerateExcludes { source },
                };
                let failure: Failure = error.into();
                let diagnostic = failure.diagnostic();
                assert_eq!(diagnostic.code(), expected);
                let rendered = format!(
                    "{} {:?} {:?} {:?}",
                    diagnostic.summary(),
                    diagnostic.detail(),
                    diagnostic.subject(),
                    diagnostic.hints()
                );
                assert!(!rendered.contains("SENTINEL"));
                assert!(failure.technical_source().is_some());
            }
        }
        assert_eq!(
            Failure::from(RepositoryMutationError::Stale)
                .diagnostic()
                .code(),
            ErrorCode::Conflict
        );
    }

    #[test]
    fn preparation_preserves_underlying_state_and_cache_failures() {
        for stage in 0..5 {
            let source = Box::new(
                gat_io::StateStoreError::InvalidRow {
                    detail: "SENTINEL".into(),
                }
                .into(),
            );
            let error = match stage {
                0 => RepositoryStateError::Open { source },
                1 => RepositoryStateError::Refresh { source },
                2 => RepositoryStateError::Read { source },
                3 => RepositoryStateError::Verify { source },
                _ => RepositoryStateError::Ingest { source },
            };
            let failure: Failure = error.into();
            assert_eq!(failure.diagnostic().code(), ErrorCode::StateCorrupt);
            assert!(!failure.diagnostic().detail().unwrap().contains("SENTINEL"));
        }
    }
}
