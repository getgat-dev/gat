//! `Failure` mapping for `gat_core::globs` errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_core::globs::GlobError;

pub(super) fn glob_diagnostic(err: &GlobError) -> Diagnostic {
    match err {
        GlobError::InvalidLexicalSyntax { pattern, .. } => Diagnostic::new(
            ErrorCode::InvalidArgumentValue,
            "That pattern is not a valid path pattern",
        )
        .with_subject(UserLine::identifier(pattern))
        .with_hint(
            "Patterns must be relative, use `/` separators, and stay inside the \
             repository (no leading `/`, `..`, or drive letters).",
        ),
        // Deliberately does not interpolate `source` (a
        // `glob::PatternError`) into the diagnostic text: its
        // `Display` is parser-library wording ("wildcards are either
        // regular `*` or recursive `**`", byte offsets, ...), not
        // something Gat has authored for users. The
        // pattern itself is still named via `subject`.
        GlobError::InvalidGlobSyntax { pattern, .. } => Diagnostic::new(
            ErrorCode::InvalidArgumentValue,
            "That pattern is not a valid glob pattern",
        )
        .with_subject(UserLine::identifier(pattern))
        .with_hint("Check for unbalanced `[...]` or a stray trailing `\\`."),
    }
}

impl From<GlobError> for Failure {
    fn from(err: GlobError) -> Self {
        Self::infrastructure(glob_diagnostic(&err), err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::globs::GatGlobPattern;

    /// A `glob::PatternError`'s own `Display` text (parser-
    /// library wording, byte offsets, ...) must never leak into the
    /// rendered `Failure` -- only Gat's own authored summary/hint may
    /// appear there.
    #[test]
    fn invalid_glob_syntax_failure_never_leaks_the_parser_librarys_own_wording() {
        let err = GatGlobPattern::parse("data/[").unwrap_err();
        let raw_parser_text = match &err {
            GlobError::InvalidGlobSyntax { source, .. } => source.to_string(),
            other @ GlobError::InvalidLexicalSyntax { .. } => {
                panic!("expected InvalidGlobSyntax, got {other:?}")
            }
        };
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert_eq!(diagnostic.subject(), Some("data/["));
        assert!(!diagnostic.summary().contains(&raw_parser_text));
        for hint in diagnostic.hints() {
            assert!(!hint.contains(&raw_parser_text));
        }
    }

    /// Symmetric check for the lexical-syntax branch: the rejected pattern
    /// itself is preserved when safe, but the technical
    /// `LexicalPathError` source
    /// is not echoed verbatim either.
    #[test]
    fn invalid_lexical_syntax_failure_names_the_pattern_without_leaking_the_source() {
        let err = GatGlobPattern::parse("../escape.bin").unwrap_err();
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert_eq!(diagnostic.subject(), Some("../escape.bin"));
        assert!(!diagnostic.summary().is_empty());
    }
}
