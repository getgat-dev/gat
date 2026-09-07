//! `Failure` mapping for `gat_command::RemoveError`.

use super::super::super::{Diagnostic, Failure};
use crate::presentation::UserLine;
use gat_command::RemoveError;

impl From<RemoveError> for Failure {
    fn from(err: RemoveError) -> Self {
        match err {
            RemoveError::MountOwned(source) => source.into(),
            RemoveError::Path(source) => source.into(),
            RemoveError::Lexical(source) => source.into(),
            RemoveError::Glob(source) => source.into(),
            RemoveError::Acquisition(source) => (*source).into(),
            RemoveError::PathPolicy(source) => source.into(),
            RemoveError::RemoteCatalog(source) => source.into(),
            RemoveError::RepositoryMutation(source) => (*source).into(),
            RemoveError::Cleanup(source) => {
                let failure = match *source {
                    gat_engine::WorktreeRemoveError::Path(source) => source.into(),
                    error @ (gat_engine::WorktreeRemoveError::Delete { .. }
                    | gat_engine::WorktreeRemoveError::Prune { .. }) => {
                        let (path, source, summary) = match &error {
                            gat_engine::WorktreeRemoveError::Delete { path, source } => {
                                (path, source, "Could not delete a working-tree file")
                            }
                            gat_engine::WorktreeRemoveError::Prune { path, source } => (
                                path,
                                source,
                                "Could not prune an empty working-tree directory",
                            ),
                            gat_engine::WorktreeRemoveError::Path(_) => unreachable!(),
                        };
                        let code = super::super::io_code(source);
                        Self::infrastructure(
                            Diagnostic::new(code, summary).with_subject(UserLine::path_text(path)),
                            error,
                        )
                    }
                };
                after_publication(
                    failure,
                    "Working-tree cleanup failed; some selected files or empty directories may remain. Materialized ownership and Git exclusions have not been updated.",
                )
            }
            RemoveError::ForgetMaterialized { cached, source } => {
                let mut failure = after_publication(
                    (*source).into(),
                    "Materialized ownership cleanup failed; Git exclusions have not been updated.",
                );
                if cached {
                    *failure.diagnostic = failure.diagnostic.with_hint(
                        "Preserve the retained files before running gat sync: stale ownership may cause sync to delete them.");
                }
                failure
            }
            RemoveError::SyncExcludes(source) => after_publication(
                (*source).into(),
                "Git exclusion regeneration failed; materialized ownership cleanup completed.",
            ),
        }
    }
}

fn after_publication(mut failure: Failure, stage: &'static str) -> Failure {
    // Earlier path diagnostics may suggest retrying; that cannot resume rm.
    failure.diagnostic.hints.clear();
    *failure.diagnostic = failure.diagnostic
        .with_detail("Removals were already published to gat.lock; the selected paths are no longer tracked.")
        .with_detail(stage)
        .with_hint("Repeating the same gat rm does not resume cleanup for these paths. Inspect the remaining files and metadata before further cleanup.");
    failure
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use gat_engine::{RepositoryMutationError, WorktreeRemoveError};

    const SENTINEL: &str = "SENTINEL_LOW_LEVEL_REMOVE_DETAIL";

    fn assert_partial(failure: &Failure, code: ErrorCode) {
        let diagnostic = failure.diagnostic();
        assert_eq!(diagnostic.code(), code);
        assert!(diagnostic.detail().unwrap().contains("already published"));
        assert!(diagnostic.hints().join(" ").contains("does not resume"));
        let rendered = format!(
            "{} {:?} {:?} {:?}",
            diagnostic.summary(),
            diagnostic.subject(),
            diagnostic.detail(),
            diagnostic.hints()
        );
        assert!(!rendered.contains(SENTINEL));
        assert!(!rendered.contains('\u{1b}'));
        assert!(failure.technical_source().is_some());
    }

    #[test]
    fn cleanup_maps_io_kinds_and_sanitizes_paths_without_rendering_sources() {
        for (kind, code) in [
            (
                std::io::ErrorKind::PermissionDenied,
                ErrorCode::PermissionDenied,
            ),
            (std::io::ErrorKind::StorageFull, ErrorCode::StorageExhausted),
            (std::io::ErrorKind::Other, ErrorCode::FilesystemUnavailable),
        ] {
            for prune in [false, true] {
                let path = "unsafe\n\u{1b}[31m.bin".to_owned();
                let source = std::io::Error::new(kind, SENTINEL);
                let error = if prune {
                    WorktreeRemoveError::Prune { path, source }
                } else {
                    WorktreeRemoveError::Delete { path, source }
                };
                let failure: Failure = RemoveError::Cleanup(Box::new(error)).into();
                assert_partial(&failure, code);
                assert!(!failure.diagnostic().subject().unwrap().contains('\n'));
                assert!(failure.diagnostic().summary().contains(if prune {
                    "prune"
                } else {
                    "delete"
                }));
            }
        }
    }

    #[test]
    fn metadata_stages_preserve_classification_and_cached_recovery_context() {
        for cached in [false, true] {
            let failure: Failure = RemoveError::ForgetMaterialized {
                cached,
                source: Box::new(RepositoryMutationError::ForgetMaterialized {
                    source: Box::new(
                        gat_io::StateStoreError::InvalidRow {
                            detail: SENTINEL.into(),
                        }
                        .into(),
                    ),
                }),
            }
            .into();
            assert_partial(&failure, ErrorCode::StateCorrupt);
            assert_eq!(
                failure
                    .diagnostic()
                    .hints()
                    .join(" ")
                    .contains("stale ownership"),
                cached
            );
        }
        let failure: Failure =
            RemoveError::SyncExcludes(Box::new(RepositoryMutationError::RegenerateExcludes {
                source: Box::new(
                    gat_io::AtomicError::PublishFailed {
                        path: "exclude".into(),
                        source: std::io::Error::other(SENTINEL),
                    }
                    .into(),
                ),
            }))
            .into();
        assert_partial(&failure, ErrorCode::FilesystemUnavailable);
        assert!(
            failure
                .diagnostic()
                .detail()
                .unwrap()
                .contains("ownership cleanup completed")
        );
    }
}
