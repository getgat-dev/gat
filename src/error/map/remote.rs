//! `Failure` mapping helpers for engine-owned remote-open errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;

pub(super) const fn remote_open_code(kind: &gat_engine::RemoteOpenFailureKind) -> ErrorCode {
    use gat_engine::RemoteOpenFailureKind;

    match kind {
        RemoteOpenFailureKind::NonUnicodeVariable { .. }
        | RemoteOpenFailureKind::InvalidInterpolation
        | RemoteOpenFailureKind::MissingVariable { .. } => ErrorCode::InvalidArgumentValue,
        RemoteOpenFailureKind::MalformedUrl
        | RemoteOpenFailureKind::UnsupportedBackend
        | RemoteOpenFailureKind::InvalidFileRemotePath { .. }
        | RemoteOpenFailureKind::DisallowedScheme
        | RemoteOpenFailureKind::UnsupportedCapability => ErrorCode::RemoteInvalid,
        RemoteOpenFailureKind::OperationFailed => ErrorCode::RemoteOperationFailed,
        RemoteOpenFailureKind::PermissionDenied => ErrorCode::RemotePermissionDenied,
        RemoteOpenFailureKind::NotFound => ErrorCode::ObjectMissing,
        RemoteOpenFailureKind::Unavailable | RemoteOpenFailureKind::ReadinessTimedOut { .. } => {
            ErrorCode::RemoteUnavailable
        }
    }
}

pub(super) fn semantic_remote_open_failure(
    kind: &gat_engine::RemoteOpenFailureKind,
    template: &gat_core::endpoint::RemoteUrlTemplate,
    source: impl std::error::Error + Send + Sync + 'static,
) -> Failure {
    use gat_engine::RemoteOpenFailureKind;

    let subject = UserLine::redacted_url(&crate::redaction::render_remote_template(template));
    let (diagnostic, expected) = match kind {
        RemoteOpenFailureKind::NonUnicodeVariable { name } => (non_unicode_variable_diagnostic(name), true),
        RemoteOpenFailureKind::InvalidInterpolation => {
            (super::interpolate::interpolation_syntax_diagnostic(), true)
        }
        RemoteOpenFailureKind::MissingVariable { name } => {
            (super::interpolate::missing_variable_diagnostic(name), true)
        }
        RemoteOpenFailureKind::MalformedUrl => (
            Diagnostic::new(ErrorCode::RemoteInvalid, "Invalid remote url")
                .with_subject(subject)
                .with_hint(
                    "Check the url scheme, host, and any `?query=params` against the \
                     backend's documented options.",
                ),
            false,
        ),
        RemoteOpenFailureKind::UnsupportedBackend => (
            Diagnostic::new(ErrorCode::RemoteInvalid, "Unsupported remote scheme")
                .with_subject(subject)
                .with_hint(
                    "Check that this gat build was compiled with support for this backend, \
                     and that the url scheme is spelled correctly.",
                ),
            false,
        ),
        RemoteOpenFailureKind::InvalidFileRemotePath { hint } => (
            Diagnostic::new(ErrorCode::RemoteInvalid, "Invalid file remote path")
                .with_subject(subject)
                .with_hint(*hint),
            true,
        ),
        RemoteOpenFailureKind::DisallowedScheme => (
            Diagnostic::new(ErrorCode::RemoteInvalid, "Unsupported remote scheme")
                .with_subject(subject)
                .with_hint(
                    "gat supports `file://`, `s3://`, `azblob://`, `gcs://`, and `oss://` \
                     remotes only.",
                ),
            true,
        ),
        RemoteOpenFailureKind::UnsupportedCapability => (
            Diagnostic::new(
                ErrorCode::RemoteInvalid,
                "Remote does not support a required operation",
            )
            .with_subject(subject)
            .with_hint(
                "gat requires stat/read/write/list/delete for every remote, plus multipart \
                 writes for non-file remotes; check this backend's own opendal capability docs.",
            ),
            false,
        ),
        RemoteOpenFailureKind::PermissionDenied => (
            Diagnostic::new(
                ErrorCode::RemotePermissionDenied,
                "Permission denied while checking remote",
            )
            .with_subject(subject)
            .with_hint(
                "Check the credentials resolved by this backend and ensure they permit listing the configured remote root.",
            ),
            false,
        ),
        RemoteOpenFailureKind::NotFound => (
            Diagnostic::new(ErrorCode::ObjectMissing, "Not found on remote").with_subject(
                UserLine::redacted_url(&crate::redaction::render_remote_template(template)),
            ),
            false,
        ),
        RemoteOpenFailureKind::ReadinessTimedOut { budget } => (
            Diagnostic::new(ErrorCode::RemoteUnavailable, UserLine::compose([UserLine::authored("Remote did not become ready within "), UserLine::number(i64::try_from(budget.as_secs()).unwrap_or(i64::MAX)), UserLine::authored(" seconds")]))
                .with_subject(subject)
                .with_hint("Credential discovery or the remote endpoint may be unreachable. Check the configured credential provider and network access."),
            false,
        ),
        RemoteOpenFailureKind::Unavailable => (
            Diagnostic::new(ErrorCode::RemoteUnavailable, "Remote is unavailable")
                .with_subject(subject)
                .with_hint("This may be transient; retrying later may succeed."),
            false,
        ),
        RemoteOpenFailureKind::OperationFailed => (
            Diagnostic::new(ErrorCode::RemoteOperationFailed, "Remote operation failed")
                .with_subject(subject),
            false,
        ),
    };

    if expected {
        Failure::expected_with_source(diagnostic, source)
    } else {
        Failure::infrastructure(diagnostic, source)
    }
}

fn non_unicode_variable_diagnostic(name: &str) -> Diagnostic {
    Diagnostic::new(
        ErrorCode::InvalidArgumentValue,
        "Template variable is not Unicode",
    )
    .with_subject(UserLine::identifier(name))
}

/// Readiness wording shared by all workflows before workers receive a client.
pub(super) fn readiness_diagnostic(
    kind: &gat_engine::RemoteOpenFailureKind,
    remote_name: &str,
) -> Option<Diagnostic> {
    use gat_engine::RemoteOpenFailureKind;
    let remote = UserLine::compose([
        UserLine::authored("remote '"),
        UserLine::identifier(remote_name),
        UserLine::authored("'"),
    ]);
    match kind {
        RemoteOpenFailureKind::NonUnicodeVariable { name } => Some(non_unicode_variable_diagnostic(name)),
        RemoteOpenFailureKind::ReadinessTimedOut { budget } => Some(
            Diagnostic::new(ErrorCode::RemoteUnavailable, UserLine::compose([
                UserLine::authored("Readiness check for "), remote,
                UserLine::authored(" did not complete within "),
                UserLine::number(i64::try_from(budget.as_secs()).unwrap_or(i64::MAX)),
                UserLine::authored(" seconds"),
            ])).with_hint("Credential discovery or the remote endpoint may be unreachable. Check the configured credential provider and network access.")
        ),
        RemoteOpenFailureKind::PermissionDenied => Some(
            Diagnostic::new(ErrorCode::RemotePermissionDenied, UserLine::compose([
                UserLine::authored("Permission denied while checking "), remote,
            ])).with_hint("Check the credentials resolved by this backend and ensure they permit listing the configured remote root.")
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_timeout_is_unavailable_with_budget_and_credential_provider_hint() {
        let kind = gat_engine::RemoteOpenFailureKind::ReadinessTimedOut {
            budget: std::time::Duration::from_secs(10),
        };
        let diagnostic = readiness_diagnostic(&kind, "origin").unwrap();
        assert_eq!(diagnostic.code(), ErrorCode::RemoteUnavailable);
        assert!(diagnostic.summary().contains("origin"));
        assert!(diagnostic.summary().contains("10 seconds"));
        assert!(
            diagnostic
                .hints()
                .join(" ")
                .contains("Credential discovery")
        );
        assert!(!diagnostic.hints().join(" ").contains("missing credentials"));
    }

    #[test]
    fn readiness_mapping_never_renders_backend_secrets_or_unredacted_endpoint() {
        for kind in [
            gat_engine::RemoteOpenFailureKind::PermissionDenied,
            gat_engine::RemoteOpenFailureKind::ReadinessTimedOut {
                budget: std::time::Duration::from_secs(10),
            },
        ] {
            let template = gat_core::endpoint::RemoteUrlTemplate::from_string(
                "s3://bucket/root?access_key_id=SYNTHETIC-SECRET".to_owned(),
            );
            let failure = semantic_remote_open_failure(
                &kind,
                &template,
                std::io::Error::other("SYNTHETIC-SECRET"),
            );
            let diagnostic = failure.diagnostic();
            assert!(!diagnostic.summary().contains("SYNTHETIC-SECRET"));
            assert!(!diagnostic.subject().unwrap().contains("SYNTHETIC-SECRET"));
            assert!(!diagnostic.hints().join(" ").contains("SYNTHETIC-SECRET"));
            if kind == gat_engine::RemoteOpenFailureKind::PermissionDenied {
                assert!(diagnostic.hints().join(" ").contains("listing"));
            }
        }
    }
}
