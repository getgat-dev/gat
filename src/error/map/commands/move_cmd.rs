//! `Failure` mapping for `gat_command::MoveError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::MoveError;

impl From<MoveError> for Failure {
    fn from(err: MoveError) -> Self {
        match err {
            MoveError::MountOwned(source) => source.into(),
            MoveError::Path(source) => source.into(),
            MoveError::Acquisition(source) => (*source).into(),
            MoveError::PathPolicy(source) => source.into(),
            MoveError::RemoteCatalog(source) => source.into(),
            MoveError::RepositoryMutation(source) => (*source).into(),
            MoveError::SourceNotTracked { ref path } => Self::expected(
                Diagnostic::new(ErrorCode::Conflict, "This path is not tracked by gat")
                    .with_subject(UserLine::gat_path(path)),
            ),
            MoveError::DestinationTracked { ref dst, .. } => Self::expected(
                Diagnostic::new(
                    ErrorCode::Conflict,
                    "The destination is already tracked by gat",
                )
                .with_subject(UserLine::gat_path(dst))
                .with_hint("Pass --force to replace the tracked destination."),
            ),
            MoveError::DestinationIsDirectory { ref path } => Self::expected(
                Diagnostic::new(
                    ErrorCode::Conflict,
                    "The destination is an existing directory",
                )
                .with_subject(UserLine::gat_path(path))
                .with_hint(
                    "Name the destination path explicitly; moving into a directory is unsupported.",
                ),
            ),
            MoveError::DestinationExists { ref dst, .. } => Self::expected(
                Diagnostic::new(ErrorCode::Conflict, "The destination already exists")
                    .with_subject(UserLine::gat_path(dst))
                    .with_hint("Pass --force to replace the destination."),
            ),
            MoveError::CheckDestination { source, .. } => source.into(),
            MoveError::Worktree(source) => match *source {
                gat_engine::WorktreeMoveError::Path(source) => source.into(),
                error => {
                    let (path, source, summary) = match &error {
                        gat_engine::WorktreeMoveError::CreateParent { path, source } => (
                            path,
                            source,
                            "Could not create the move destination's parent directory",
                        ),
                        gat_engine::WorktreeMoveError::Rename { dst, source, .. } => {
                            (dst, source, "Could not rename the working-tree path")
                        }
                        gat_engine::WorktreeMoveError::Path(_) => unreachable!(),
                    };
                    Self::infrastructure(
                        Diagnostic::new(super::super::io_code(source), summary)
                            .with_subject(UserLine::path_text(path)),
                        error,
                    )
                }
            },
            MoveError::RolledBack { src, source, .. } => {
                let mut failure = Self::from(*source);
                *failure.diagnostic = failure.diagnostic
                    .with_detail("The working-tree rename was rolled back after tracking-state publication failed.")
                    .with_subject(UserLine::gat_path(&src))
                    .with_hint("Inspect gat.lock and the restored source before retrying the move.");
                failure
            }
            MoveError::RollbackFailed {
                ref src,
                ref dst,
                ref rollback_source,
                ..
            } => {
                let code = match &**rollback_source {
                    gat_engine::WorktreeRollbackError::Rename { source, .. }
                    | gat_engine::WorktreeRollbackError::Path(
                        gat_engine::WorktreePathError::Io { source, .. },
                    ) => super::super::io_code(source),
                    gat_engine::WorktreeRollbackError::Path(source) => match source {
                        gat_engine::WorktreePathError::Internal { .. } => ErrorCode::Internal,
                        gat_engine::WorktreePathError::ForbiddenInfrastructurePath { .. } => {
                            ErrorCode::InvalidPath
                        }
                        gat_engine::WorktreePathError::UnsupportedLeafSymlink { .. } => {
                            ErrorCode::UnsupportedFileType
                        }
                        _ => ErrorCode::PathOutsideRepository,
                    },
                };
                Self::infrastructure(
                    Diagnostic::new(code, "Tracking-state publication and the move rollback both failed")
                        .with_subject(UserLine::gat_path(dst))
                        .with_detail("The rename completed; the moved path was not restored to its original location.")
                        .with_detail(UserLine::compose([
                            UserLine::authored("Original path: "), UserLine::gat_path(src),
                        ]))
                        .with_hint("Preserve the moved files and inspect gat.lock. If the original path is still tracked, use gat rm --cached for that path, then gat add for the destination to reconcile tracking."),
                    err,
                )
            }
            MoveError::Published { src, dst, source } => {
                let mut failure = Self::from(*source);
                failure.diagnostic.hints.clear();
                *failure.diagnostic = failure.diagnostic
                    .with_subject(UserLine::gat_path(&dst))
                    .with_detail(UserLine::compose([
                        UserLine::authored("The working-tree move and gat.lock publication completed from "),
                        UserLine::gat_path(&src), UserLine::authored(" to "), UserLine::gat_path(&dst),
                        UserLine::authored("; metadata cleanup is incomplete."),
                    ]))
                    .with_hint("Repeating the original move does not resume metadata cleanup. Inspect the moved files and tracking state before running gat sync.");
                failure
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::lexical_path::GatPath;
    use gat_engine::{RepositoryMutationError, WorktreeMoveError, WorktreeRollbackError};

    fn path(value: &str) -> GatPath {
        GatPath::parse_canonical(value).unwrap()
    }

    fn publication_failure() -> MoveError {
        MoveError::RepositoryMutation(Box::new(RepositoryMutationError::Publish {
            source: Box::new(
                gat_io::AtomicError::PublishFailed {
                    path: "SENTINEL_PHYSICAL_PATH".into(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "SENTINEL_SAVE",
                    ),
                }
                .into(),
            ),
        }))
    }

    fn assert_safe(failure: &Failure) {
        assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
        assert!(failure.technical_source().is_some());
    }

    #[test]
    fn rollback_outcomes_explain_where_the_file_is_and_preserve_causes() {
        let failure: Failure = MoveError::RolledBack {
            src: path("original"),
            dst: path("destination"),
            source: Box::new(publication_failure()),
        }
        .into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::PermissionDenied);
        assert_eq!(failure.diagnostic().subject(), Some("original"));
        assert!(
            failure
                .diagnostic()
                .detail()
                .unwrap()
                .contains("rolled back")
        );
        assert_safe(&failure);

        let failure: Failure = MoveError::RollbackFailed {
            src: path("original"),
            dst: path("destination"),
            save_source: Box::new(publication_failure()),
            rollback_source: Box::new(WorktreeRollbackError::Rename {
                src: "original".into(),
                dst: "destination".into(),
                source: std::io::Error::new(std::io::ErrorKind::StorageFull, "SENTINEL_ROLLBACK"),
            }),
        }
        .into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::StorageExhausted);
        assert_eq!(failure.diagnostic().subject(), Some("destination"));
        assert!(
            failure
                .diagnostic()
                .detail()
                .unwrap()
                .contains("not restored")
        );
        assert!(
            failure
                .diagnostic()
                .hints()
                .join(" ")
                .contains("rm --cached")
        );
        assert_safe(&failure);
        assert!(
            failure
                .technical_source()
                .unwrap()
                .downcast_ref::<MoveError>()
                .is_some()
        );
    }

    #[test]
    fn rename_and_parent_creation_classify_io_and_escape_path_context() {
        for create in [false, true] {
            let source = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "SENTINEL_IO");
            let unsafe_path = "destination
\u{1b}[31m"
                .to_owned();
            let error = if create {
                WorktreeMoveError::CreateParent {
                    path: unsafe_path,
                    source,
                }
            } else {
                WorktreeMoveError::Rename {
                    src: "original".into(),
                    dst: unsafe_path,
                    source,
                }
            };
            let failure: Failure = MoveError::Worktree(Box::new(error)).into();
            assert_eq!(failure.diagnostic().code(), ErrorCode::PermissionDenied);
            assert!(!failure.diagnostic().subject().unwrap().contains('\n'));
            assert!(!failure.diagnostic().subject().unwrap().contains('\u{1b}'));
            assert_safe(&failure);
        }
    }

    #[test]
    fn published_move_reports_metadata_failure_without_suggesting_the_original_retry() {
        let failure: Failure = MoveError::Published {
            src: path("original"),
            dst: path("destination"),
            source: Box::new(RepositoryMutationError::MoveMaterialized {
                source: Box::new(
                    gat_io::StateStoreError::InvalidRow {
                        detail: "SENTINEL_STATE".into(),
                    }
                    .into(),
                ),
            }),
        }
        .into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::StateCorrupt);
        assert!(
            failure
                .diagnostic()
                .detail()
                .unwrap()
                .contains("publication completed")
        );
        assert!(
            failure
                .diagnostic()
                .hints()
                .join(" ")
                .contains("does not resume")
        );
        assert_safe(&failure);
    }
}
