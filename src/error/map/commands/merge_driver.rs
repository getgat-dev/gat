//! `Failure` mapping for `gat_command::MergeDriverError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::MergeDriverError;
use gat_core::oid::Oid;
use gat_engine::MergeDriverError as EngineError;

/// Renders one side of a conflicting entry for the diagnostic detail --
/// `Some(oid)` as its content hash, `None` as an explicit "deleted"
/// marker, mirroring how the merge itself distinguishes an edit from a
/// removal.
fn describe(value: Option<&Oid>) -> String {
    match value {
        Some(oid) => format!("blake3:{oid}"),
        None => "deleted".to_string(),
    }
}

impl From<MergeDriverError> for Failure {
    fn from(err: MergeDriverError) -> Self {
        let MergeDriverError::Engine(err) = err;
        match err {
            EngineError::Read {
                ref path,
                ref source,
                ..
            } => Self::infrastructure(
                Diagnostic::new(
                    super::super::repository_access_code(source.kind()),
                    "Could not read a merge input file",
                )
                .with_subject(UserLine::path(path)),
                err,
            ),
            EngineError::Parse { source, .. } => source.into(),
            EngineError::SemanticConflict(ref conflicts) => {
                let lines = conflicts.iter().map(|c| {
                    UserLine::compose([
                        UserLine::identifier(c.path.as_str()),
                        UserLine::authored(": ours="),
                        UserLine::identifier(&describe(Option::from(&c.ours))),
                        UserLine::authored(", theirs="),
                        UserLine::identifier(&describe(Option::from(&c.theirs))),
                    ])
                });
                Self::expected(
                    lines
                        .fold(
                            Diagnostic::new(
                                ErrorCode::Conflict,
                                UserLine::compose([
                                    UserLine::authored("gat.lock semantic merge conflict ("),
                                    UserLine::number(conflicts.len() as i64),
                                    UserLine::authored(
                                        " path(s) changed incompatibly on both sides)",
                                    ),
                                ]),
                            ),
                            Diagnostic::with_detail,
                        )
                        .with_hint("resolve the conflicting paths manually in the merged gat.lock, then stage it"),
                )
            }
            EngineError::Publish {
                ref path,
                ref source,
                ..
            } => Self::infrastructure(
                Diagnostic::new(
                    super::super::repository_access_code(source.kind()),
                    "Could not write the merged gat.lock",
                )
                .with_subject(UserLine::path(path)),
                err,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::lexical_path::GatPath;
    use gat_core::lock::Conflict;
    use gat_engine::MergeStage;
    use std::path::PathBuf;

    fn access_source(sentinel: &'static str) -> gat_engine::RepositoryAccessError {
        gat_engine::RepositoryAccessError::for_test(
            gat_engine::RepositoryAccessFailureKind::Filesystem(
                gat_engine::FilesystemFailureKind::Unavailable,
            ),
            std::io::Error::other(sentinel),
        )
    }

    fn gp(path: &str) -> GatPath {
        GatPath::parse_canonical(path).unwrap()
    }

    /// A clean, multi-path semantic conflict still renders exactly one
    /// composed line per conflict in the diagnostic detail.
    #[test]
    fn semantic_conflict_detail_has_one_line_per_conflict() {
        let conflicts = vec![
            Conflict {
                path: gp("src/lib.rs"),
                ancestor: Some(gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap()),
                ours: Some(gat_core::oid::Oid::from_hex(&"b".repeat(64)).unwrap()),
                theirs: Some(gat_core::oid::Oid::from_hex(&"c".repeat(64)).unwrap()),
            },
            Conflict {
                path: gp("src/main.rs"),
                ancestor: Some(gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap()),
                ours: Some(gat_core::oid::Oid::from_hex(&"d".repeat(64)).unwrap()),
                theirs: Some(gat_core::oid::Oid::from_hex(&"e".repeat(64)).unwrap()),
            },
        ];
        let err = MergeDriverError::Engine(EngineError::SemanticConflict(conflicts));
        let failure: Failure = err.into();
        let detail = failure
            .diagnostic()
            .detail()
            .expect("SemanticConflict attaches a detail");
        assert_eq!(detail.lines().count(), 2);
        assert!(detail.contains("src/lib.rs"));
        assert!(detail.contains("src/main.rs"));
    }

    fn rendered_diagnostic(error: EngineError) -> String {
        let failure: Failure = MergeDriverError::Engine(error).into();
        let diagnostic = failure.diagnostic();
        format!(
            "{}{}",
            diagnostic.summary(),
            diagnostic.detail().unwrap_or_default()
        )
    }

    #[test]
    fn physical_sources_never_reach_rendered_diagnostics() {
        let read_sentinel = "SENTINEL_MERGE_STAGE_READ_4be00be1";
        let read = rendered_diagnostic(EngineError::Read {
            stage: MergeStage::Ancestor,
            path: PathBuf::from("O"),
            source: Box::new(access_source(read_sentinel)),
        });
        assert!(!read.contains(read_sentinel));

        let publish_sentinel = "SENTINEL_MERGE_PUBLISH_e3c6ce3b";
        let publish = rendered_diagnostic(EngineError::Publish {
            path: PathBuf::from("A"),
            source: Box::new(access_source(publish_sentinel)),
        });
        assert!(!publish.contains(publish_sentinel));
    }

    #[test]
    fn parse_failure_retains_the_core_lock_error_without_io_rewrapping() {
        let source = gat_core::lock::Lock::parse("not-a-lock").unwrap_err();
        let failure: Failure = MergeDriverError::Engine(EngineError::Parse {
            stage: MergeStage::Ours,
            source,
        })
        .into();

        assert_eq!(failure.diagnostic().code(), ErrorCode::StateIncompatible);
        assert!(
            failure
                .technical_source()
                .is_some_and(|source| source.downcast_ref::<gat_core::lock::LockError>().is_some())
        );
    }
    #[test]
    fn physical_stage_failures_preserve_permission_and_capacity_classifications() {
        for (kind, code) in [
            (
                gat_engine::FilesystemFailureKind::PermissionDenied,
                ErrorCode::PermissionDenied,
            ),
            (
                gat_engine::FilesystemFailureKind::StorageExhausted,
                ErrorCode::StorageExhausted,
            ),
        ] {
            for publish in [false, true] {
                let source = Box::new(gat_engine::RepositoryAccessError::for_test(
                    gat_engine::RepositoryAccessFailureKind::Filesystem(kind),
                    std::io::Error::other("SENTINEL"),
                ));
                let error = if publish {
                    EngineError::Publish {
                        path: "A".into(),
                        source,
                    }
                } else {
                    EngineError::Read {
                        path: "O".into(),
                        stage: MergeStage::Ancestor,
                        source,
                    }
                };
                let failure: Failure = MergeDriverError::Engine(error).into();
                assert_eq!(failure.diagnostic().code(), code);
                assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
                assert!(failure.technical_source().is_some());
            }
        }
    }
}
