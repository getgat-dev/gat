//! `Failure` mapping for `gat_core::lexical_path` errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_core::lexical_path::LexicalPathError;

pub(super) fn lexical_path_diagnostic(err: &LexicalPathError) -> Diagnostic {
    match err {
        LexicalPathError::EmptyPath { input } => Diagnostic::new(
            ErrorCode::InvalidPath,
            "This path must not be the repository root",
        )
        .with_subject(UserLine::path_text(input))
        .with_hint("Name a specific path under the repository root, not the root itself."),
        LexicalPathError::NonUtf8 { display } => Diagnostic::new(
            ErrorCode::InvalidPath,
            "This path contains non-UTF-8 characters",
        )
        .with_subject(UserLine::path_text(display))
        .with_hint("Gat paths must be valid UTF-8."),
        LexicalPathError::NotRelative { input } => {
            Diagnostic::new(ErrorCode::InvalidPath, "This path must be relative")
                .with_subject(UserLine::path_text(input))
                .with_hint(
                    "Pass a path relative to the repository root, not an absolute or UNC path.",
                )
        }
        LexicalPathError::ParentTraversal { input } => Diagnostic::new(
            ErrorCode::PathOutsideRepository,
            "This path escapes the repository root (contains `..`)",
        )
        .with_subject(UserLine::path_text(input))
        .with_hint("Remove the `..` component; Gat paths must stay inside the repository."),
    }
}

impl From<LexicalPathError> for Failure {
    fn from(err: LexicalPathError) -> Self {
        Self::expected(lexical_path_diagnostic(&err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_is_rejected_by_callers_that_require_a_concrete_path() {
        // `normalize_relative_path` itself treats `.`/`./`/"" as a
        // successful `LexicalPath::Empty`, not an error -- `EmptyPath` is
        // for a caller (like `normalize_route_path`) that additionally
        // requires a non-root path.
        let err = LexicalPathError::EmptyPath {
            input: ".".to_string(),
        };
        let failure: Failure = err.into();
        assert_eq!(failure.diagnostic().subject(), Some("."));
    }
}
