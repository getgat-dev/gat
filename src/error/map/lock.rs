//! `Failure` mapping for core lock-domain errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use super::lexical_path::lexical_path_diagnostic;
use crate::presentation::UserLine;
use gat_core::lock::{
    InvalidOidReason, LockDomainError, LockError as CoreLockError, MalformedRowReason,
};

/// Author safe, static CLI wording for [`MalformedRowReason`] instead of
/// reusing the reason's own (developer-facing) `Display`.
const fn malformed_row_problem(reason: &MalformedRowReason) -> &'static str {
    match reason {
        MalformedRowReason::InvalidSeparator => {
            "expected a tab after a 64-character lowercase hexadecimal object id"
        }
        MalformedRowReason::MissingLineFeed => "this record must end with a newline (LF or CRLF)",
        MalformedRowReason::UnorderedPath { .. } => {
            "lock paths must be in strictly increasing order"
        }
    }
}

/// Author safe, static CLI wording for [`InvalidOidReason`] instead of
/// reusing the reason's own (developer-facing) `Display`.
const fn invalid_oid_problem(reason: &InvalidOidReason) -> &'static str {
    match reason {
        InvalidOidReason::NotHexBlake3 => "this object id is not a valid hash",
    }
}

impl From<CoreLockError> for Failure {
    fn from(err: CoreLockError) -> Self {
        let diagnostic = match &err {
            CoreLockError::Domain(source) => lock_domain_diagnostic(source),
            CoreLockError::LexicalPath(source) => lexical_path_diagnostic(source),
            CoreLockError::CallbackFailed => return Self::internal(err),
        };
        match &err {
            CoreLockError::Domain(_) | CoreLockError::LexicalPath(_) => {
                Self::expected_with_source(diagnostic, err)
            }
            CoreLockError::CallbackFailed => unreachable!("handled above"),
        }
    }
}

fn lock_domain_diagnostic(err: &LockDomainError) -> Diagnostic {
    match err {
        LockDomainError::Empty => Diagnostic::new(ErrorCode::StateCorrupt, "gat.lock is empty")
            .with_hint(
                "gat.lock is missing its version header; restore it from version \
                     control before retrying.",
            ),
        LockDomainError::UnsupportedVersion { expected, got } => {
            Diagnostic::new(ErrorCode::StateIncompatible, "Unsupported gat.lock format")
                .with_detail(UserLine::compose([
                    UserLine::authored("expected version "),
                    UserLine::identifier(expected),
                    UserLine::authored(", got "),
                    UserLine::identifier(got),
                ]))
                .with_hint(
                    "This gat.lock was written by an incompatible version of gat; \
                 upgrade gat or restore gat.lock from a compatible version.",
                )
        }
        LockDomainError::MalformedRow { line, reason } => {
            Diagnostic::new(ErrorCode::StateCorrupt, "gat.lock is malformed")
                .with_detail(UserLine::compose([
                    UserLine::authored("line "),
                    UserLine::number(*line as i64),
                    UserLine::authored(": "),
                    UserLine::authored(malformed_row_problem(reason)),
                ]))
                .with_hint(UserLine::compose([
                    UserLine::authored(
                        "gat.lock has been hand-edited or corrupted; restore it from version \
                         control, or run ",
                    ),
                    UserLine::authored("`gat system repair`").unbroken(),
                    UserLine::authored(" if one is available."),
                ]))
        }
        LockDomainError::InvalidRowPath { line, path, .. } => {
            Diagnostic::new(ErrorCode::InvalidPath, "gat.lock contains an invalid path")
                .with_subject(UserLine::path_text(path))
                .with_detail(UserLine::compose([
                    UserLine::authored("line "),
                    UserLine::number(*line as i64),
                    UserLine::authored(": this path is not valid"),
                ]))
                .with_hint("Restore gat.lock from version control, or remove the offending row.")
        }
        LockDomainError::NonCanonicalPath { line, path } => Diagnostic::new(
            ErrorCode::StateCorrupt,
            "gat.lock contains a path that is not in canonical form",
        )
        .with_subject(UserLine::path_text(path))
        .with_detail(UserLine::compose([
            UserLine::authored("line "),
            UserLine::number(*line as i64),
            UserLine::authored(": not written in canonical form"),
        ]))
        .with_hint("Restore gat.lock from version control, or remove the offending row."),
        LockDomainError::InvalidOid { line, path, reason } => Diagnostic::new(
            ErrorCode::StateCorrupt,
            "gat.lock contains an invalid object id",
        )
        .with_subject(UserLine::path_text(path))
        .with_detail(UserLine::compose([
            UserLine::authored("line "),
            UserLine::number(*line as i64),
            UserLine::authored(": "),
            UserLine::authored(invalid_oid_problem(reason)),
        ]))
        .with_hint("Restore gat.lock from version control, or remove the offending row."),
        LockDomainError::DuplicatePath { path, line } => Diagnostic::new(
            ErrorCode::StateCorrupt,
            "gat.lock tracks the same path more than once",
        )
        .with_subject(UserLine::path_text(path))
        .with_detail(match line {
            Some(line) => {
                UserLine::compose([UserLine::authored("line "), UserLine::number(*line as i64)])
            }
            None => UserLine::authored("found while cross-checking shards"),
        })
        .with_hint("Restore gat.lock from version control, or remove the duplicate row."),
        LockDomainError::PathInMultipleShards { path } => Diagnostic::new(
            ErrorCode::StateCorrupt,
            "gat.lock tracks the same path more than once",
        )
        .with_subject(UserLine::path_text(path))
        .with_hint("Restore gat.lock from version control, or remove the duplicate row."),
        LockDomainError::DirectoryPrefixConflict {
            ancestor,
            descendant,
        } => Diagnostic::new(
            ErrorCode::StateCorrupt,
            "gat.lock tracks a path both directly and as a directory",
        )
        .with_subject(UserLine::path_text(ancestor))
        .with_detail(UserLine::compose([
            UserLine::authored("also tracked as a prefix of "),
            UserLine::identifier(descendant),
        ]))
        .with_hint("Restore gat.lock from version control, or remove the conflicting row."),
        LockDomainError::MisplacedShardRow { path, .. } => Diagnostic::new(
            ErrorCode::StateCorrupt,
            "gat.lock's sharded storage is corrupt",
        )
        .with_subject(UserLine::path_text(path))
        .with_detail("a tracked path is stored in the wrong shard file")
        .with_hint(
            "gat.lock/ has been hand-edited or corrupted; restore it from version \
                 control before retrying.",
        ),
    }
}

/// Map pure lock-domain failures to CLI diagnostics.
impl From<LockDomainError> for Failure {
    fn from(err: LockDomainError) -> Self {
        let diagnostic = lock_domain_diagnostic(&err);
        if matches!(err, LockDomainError::InvalidRowPath { .. }) {
            Self::expected_with_source(diagnostic, err)
        } else {
            Self::expected(diagnostic)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::lexical_path::LexicalPathError;

    #[test]
    fn core_lock_error_retains_the_complete_type_as_its_source() {
        let err = gat_core::lock::LockError::from(
            gat_core::lexical_path::GatPath::normalize("../escape").unwrap_err(),
        );
        let failure: Failure = err.into();

        assert_eq!(
            failure.diagnostic().code(),
            ErrorCode::PathOutsideRepository
        );
        assert_eq!(failure.diagnostic().subject(), Some("../escape"));
        assert!(
            failure
                .technical_source()
                .is_some_and(|source| source.downcast_ref::<CoreLockError>().is_some()),
            "the root boundary must retain the complete gat-core lock error"
        );
    }

    #[test]
    fn callback_placeholder_is_classified_as_internal() {
        let failure: Failure = CoreLockError::CallbackFailed.into();

        assert_eq!(failure.diagnostic().code(), ErrorCode::Internal);
        assert!(
            failure
                .technical_source()
                .is_some_and(|source| source.downcast_ref::<CoreLockError>().is_some())
        );
    }

    /// [`LockDomainError::DirectoryPrefixConflict`]'s `descendant` field is a
    /// path taken straight from `gat.lock`; a hand-edited/corrupted
    /// entry containing an embedded newline must never fabricate an
    /// extra rendered diagnostic line.
    #[test]
    fn directory_prefix_conflict_detail_never_splits_on_an_embedded_newline() {
        let err = LockDomainError::DirectoryPrefixConflict {
            ancestor: "src".to_string(),
            descendant: "src/lib.rs\nSENTINEL_INJECTED_LINE".to_string(),
        };
        let failure: Failure = err.into();
        let detail = failure
            .diagnostic()
            .detail()
            .expect("DirectoryPrefixConflict attaches a detail");
        assert_eq!(detail.lines().count(), 1);
        assert!(!detail.contains('\n'));
    }

    /// Symmetric check for [`LockDomainError::MalformedRow`]: it now carries a
    /// structured [`MalformedRowReason`] (Gat-authored text) rather than
    /// a raw TSV-parser message, so the rendered detail may safely
    /// include the reason's own `Display` text alongside the line number.
    #[test]
    fn malformed_row_failure_surfaces_the_line_and_reason() {
        let err = LockDomainError::MalformedRow {
            line: 7,
            reason: MalformedRowReason::UnorderedPath {
                path: "some/path".to_string(),
            },
        };
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert!(diagnostic.detail().is_some_and(|d| d.contains("line 7")));
    }

    /// [`LockDomainError::InvalidRowPath`] preserves the offending path and
    /// line number, but must not echo [`LexicalPathError`]'s own
    /// technical `Display` wording into the rendered diagnostic.
    #[test]
    fn invalid_row_path_failure_names_the_path_without_leaking_the_lexical_source_wording() {
        let source = LexicalPathError::EmptyPath {
            input: "../escape".to_string(),
        };
        let raw_source_text = source.to_string();
        let err = LockDomainError::InvalidRowPath {
            line: 4,
            path: "../escape".to_string(),
            source,
        };
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert_eq!(diagnostic.subject(), Some("../escape"));
        assert!(!diagnostic.summary().contains(&raw_source_text));
        assert!(
            !diagnostic
                .detail()
                .is_some_and(|d| d.contains(&raw_source_text))
        );
        for hint in diagnostic.hints() {
            assert!(!hint.contains(&raw_source_text));
        }
    }
}
