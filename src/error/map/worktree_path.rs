//! `Failure` mapping for worktree path errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;

impl From<gat_engine::WorktreePathError> for Failure {
    fn from(err: gat_engine::WorktreePathError) -> Self {
        use gat_engine::WorktreePathError as EngineError;

        match &err {
            EngineError::NotRelative { path } => Self::expected(
                Diagnostic::new(ErrorCode::PathOutsideRepository, "Path must be relative")
                    .with_subject(UserLine::path_text(path))
                    .with_hint("Pass a path that stays inside the repository root."),
            ),
            EngineError::ParentTraversal { path } => Self::expected(
                Diagnostic::new(
                    ErrorCode::PathOutsideRepository,
                    "Path escapes the repository (contains `..`)",
                )
                .with_subject(UserLine::path_text(path))
                .with_hint("Pass a path that stays inside the repository root."),
            ),
            EngineError::NotMaterializable { path } => Self::expected(
                Diagnostic::new(
                    ErrorCode::PathOutsideRepository,
                    "Path cannot be represented on this host",
                )
                .with_subject(UserLine::path_text(path))
                .with_hint("Pass a path that stays inside the repository root."),
            ),
            EngineError::NonUtf8Component { path } => Self::expected(
                Diagnostic::new(
                    ErrorCode::PathOutsideRepository,
                    "Path contains non-UTF-8 characters",
                )
                .with_subject(UserLine::path_text(path))
                .with_hint("Pass a path that stays inside the repository root."),
            ),
            EngineError::EscapesWorktree { path } => Self::expected(
                Diagnostic::new(
                    ErrorCode::PathOutsideRepository,
                    "Path escapes the repository root",
                )
                .with_subject(UserLine::path_text(path))
                .with_hint("Pass a path that stays inside the repository root."),
            ),
            EngineError::SymlinkAncestor { path, .. } => Self::expected(
                Diagnostic::new(
                    ErrorCode::PathOutsideRepository,
                    "Path traverses a symlinked ancestor directory",
                )
                .with_subject(UserLine::path_text(path))
                .with_hint("Remove or replace the symlinked ancestor directory before retrying."),
            ),
            EngineError::ForbiddenInfrastructurePath { path } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidPath,
                    "This path is gat/git infrastructure and can never be tracked",
                )
                .with_subject(UserLine::path_text(path))
                .with_hint(
                    "Root `.git`, `.gat`, `gat.lock`, and `gat.yaml` paths can never be tracked.",
                ),
            ),
            EngineError::UnsupportedLeafSymlink { path } => Self::expected(
                Diagnostic::new(
                    ErrorCode::UnsupportedFileType,
                    "This path is a symlink; gat does not track symlinks",
                )
                .with_subject(UserLine::path_text(path))
                .with_hint("Pass the symlink's real target instead, if you want gat to track it."),
            ),
            EngineError::Io { path, source, .. } => Self::infrastructure(
                Diagnostic::new(super::io_code(source), "Could not access this path")
                    .with_subject(UserLine::path(path)),
                err,
            ),
            EngineError::Internal { .. } => Self::internal(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_categories_and_safe_path_context_survive_without_source_text() {
        for (kind, expected) in [
            (
                std::io::ErrorKind::PermissionDenied,
                ErrorCode::PermissionDenied,
            ),
            (std::io::ErrorKind::StorageFull, ErrorCode::StorageExhausted),
            (
                std::io::ErrorKind::NotFound,
                ErrorCode::FilesystemUnavailable,
            ),
        ] {
            let failure: Failure = gat_engine::WorktreePathError::Io {
                operation: "SENTINEL_OPERATION",
                path: "unsafe\n\u{1b}[31m.bin".into(),
                source: std::io::Error::new(kind, "SENTINEL_SOURCE"),
            }
            .into();
            let diagnostic = failure.diagnostic();
            assert_eq!(diagnostic.code(), expected);
            let subject = diagnostic.subject().unwrap();
            assert!(!subject.contains('\n'));
            assert!(!subject.contains('\u{1b}'));
            assert!(!diagnostic.summary().contains("SENTINEL"));
            assert!(failure.technical_source().is_some());
        }
    }
}
