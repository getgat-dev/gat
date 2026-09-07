//! `Failure` mapping for `gat_command::GcError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::{
    GcEngineError, GcError, GcFailureKind, GcRepositoryFailureKind, GcRepositoryIssue,
};

/// Authors one safe, single-line CLI problem statement for a
/// repository's failure reason -- explicitly matched so that adding a new
/// [`GcRepositoryFailureKind`] variant forces a decision here rather than
/// silently falling back to the typed reason's own `Display` (which is
/// developer-facing abstraction text, not presentation).
const fn repository_problem(kind: GcRepositoryFailureKind) -> &'static str {
    match kind {
        GcRepositoryFailureKind::InvalidLocation => "it is not a valid Git location",
        GcRepositoryFailureKind::CloneFailed => "it could not be cloned",
        GcRepositoryFailureKind::ShallowHistory => {
            "its shallow history could not provide a complete keep set"
        }
        GcRepositoryFailureKind::Inspection => "it could not be inspected for reachability",
    }
}

/// Renders one repository issue as exactly one safe detail line:
/// `repository` is converted from its domain identity (a path or redacted
/// URL) to a display-safe single-line `UserLine`, `reason` contributes
/// only its authored problem text (never its own `Display`/source
/// chain), so a repository identity containing a literal newline or
/// forged `hint:`/`error:` prefix can never manufacture an extra
/// rendered line.
fn repository_detail_line(issue: &GcRepositoryIssue) -> UserLine {
    UserLine::compose([
        UserLine::redacted_url(&crate::redaction::RedactedUrl::render(
            issue.location().as_location_str(),
        )),
        UserLine::authored(": "),
        UserLine::authored(repository_problem(issue.kind())),
    ])
}

impl From<GcError> for Failure {
    fn from(err: GcError) -> Self {
        match &err {
            GcError::Engine(engine) => match engine {
                GcEngineError::RemoteOpen { remote_name, source } => {
                    if let Some(diagnostic) = super::super::remote::readiness_diagnostic(source.kind(), remote_name.as_str()) {
                        Self::infrastructure(diagnostic, err)
                    } else {
                        let kind = source.kind().clone();
                        let template = source.template().clone();
                        super::super::remote::semantic_remote_open_failure(&kind, &template, err)
                    }
                }
                GcEngineError::RemoteDeletionRequiresUnsafe => Self::expected(
                    Diagnostic::new(ErrorCode::InvalidArgumentValue,
                        "Remote garbage collection requires --unsafe to delete objects")
                        .with_hint("Use --dry-run to inspect candidates. Supply --repository for every other repository to protect; unlisted repositories are not protected.")),
                GcEngineError::Failure(failure) => {
                    let (code, summary) = match failure.kind() {
                        GcFailureKind::Repository => (
                            ErrorCode::RepositoryUnavailable,
                            "Could not read this repository's configuration during gc",
                        ),
                        GcFailureKind::Cache => (
                            ErrorCode::CacheUnavailable,
                            "Could not update the local object cache during gc",
                        ),
                        GcFailureKind::Remote => (
                            ErrorCode::RemoteUnavailable,
                            "Could not reach the configured remote during gc",
                        ),
                        GcFailureKind::KeepSet => (
                            ErrorCode::CacheUnavailable,
                            "Could not compute or read a gc keep-set",
                        ),
                    };
                    let mut diagnostic = Diagnostic::new(code, summary);
                    if let Some(count) = failure.confirmed_deletions() {
                        diagnostic = diagnostic
                            .with_detail(UserLine::compose([
                                UserLine::number(count as i64),
                                UserLine::authored(
                                    " remote object deletion(s) were confirmed before the failure.",
                                ),
                            ]))
                            .with_detail(if failure.kind() == GcFailureKind::Remote {
                                "Additional deletions may have completed in an unconfirmed batch."
                            } else {
                                "Pending deletion candidates were not submitted."
                            });
                    }
                    Self::infrastructure(diagnostic, err)
                }
                GcEngineError::IncompleteKeepSet { scope, issues } => {
                    let mut diagnostic = Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        UserLine::compose([
                            UserLine::authored("Refusing destructive "),
                            UserLine::authored(scope),
                            UserLine::authored(" gc: "),
                            UserLine::number(issues.len() as i64),
                            UserLine::authored(" additional repository/repositories could not be inspected"),
                        ]),
                    )
                    .with_hint(
                        "Re-run with --dry-run to report uncertain objects, correct the supplied repository locations, or pass --unsafe to override this safety check.",
                    );
                    for issue in issues {
                        diagnostic = diagnostic.with_detail(repository_detail_line(issue));
                    }
                    Self::expected_with_source(diagnostic, err)
                }
                GcEngineError::ShallowHistory { scope } => Self::expected(
                    Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        UserLine::compose([
                            UserLine::authored("Refusing destructive "),
                            UserLine::authored(scope),
                            UserLine::authored(" gc: this repository is a shallow clone"),
                        ]),
                    )
                    .with_detail(
                        "History beyond the shallow boundary is unavailable, so the keep set may \
                         be missing objects still referenced by a historical `gat.lock` there.",
                    )
                    .with_hint(
                        "Re-run with --dry-run to report uncertain objects, unshallow the \
                         repository (`git fetch --unshallow`), or pass --unsafe to override this \
                         safety check.",
                    ),
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn partial_sweep_reports_confirmed_count_without_leaking_the_source() {
        let error =
            gat_engine::test_support::gc_sweep_failure(17, io::Error::other("SENTINEL_SECRET"));
        let failure: Failure = GcError::Engine(error).into();
        let diagnostic = failure.diagnostic();
        let detail = diagnostic.detail().unwrap();
        assert!(detail.contains("17 remote object deletion(s) were confirmed"));
        assert!(detail.contains("Additional deletions may have completed"));
        assert!(!detail.contains("SENTINEL_SECRET"));
        assert!(!diagnostic.summary().contains("SENTINEL_SECRET"));
        assert!(failure.technical_source().is_some());
    }

    /// A synthetic `IncompleteKeepSet` containing a source-bearing
    /// repository issue must survive intact as `Failure::technical_source`
    /// -- not just its inner reason/source -- so the retained source
    /// chain can still be walked internally (tests/support tooling) all
    /// the way down to the original nested technical source.
    #[test]
    fn incomplete_keep_set_retains_the_complete_gc_error_as_its_technical_source() {
        let sentinel = "SENTINEL_REPOSITORY_INSPECTION_FAILURE";
        let issue = gat_engine::test_support::repository_issue(
            "peer".into(),
            GcRepositoryFailureKind::Inspection,
            io::Error::other(sentinel),
        );
        let err = GcError::Engine(GcEngineError::IncompleteKeepSet {
            scope: "local shared-cache",
            issues: vec![issue],
        });
        let failure: Failure = err.into();

        let source = failure
            .technical_source()
            .expect("IncompleteKeepSet must retain a technical source");
        let gc_err = source
            .downcast_ref::<GcError>()
            .expect("retained source must downcast to the complete GcError");
        let GcError::Engine(GcEngineError::IncompleteKeepSet { issues, .. }) = gc_err else {
            panic!("expected IncompleteKeepSet, got {gc_err:?}");
        };

        // Walk the retained source chain: GcError -> GcRepositoryIssue ->
        // technical source, confirming the
        // original nested technical source is still reachable.
        let mut chain: Vec<String> = Vec::new();
        let mut current: &(dyn std::error::Error + 'static) = &issues[0];
        loop {
            chain.push(current.to_string());
            match current.source() {
                Some(next) => current = next,
                None => break,
            }
        }
        assert!(
            chain.iter().any(|s| s.contains(sentinel)),
            "retained source chain must still reach the original nested technical source: \
             {chain:?}"
        );

        // None of the retained source text may leak into the Diagnostic.
        let diagnostic = failure.diagnostic();
        let rendered = format!(
            "{}{}",
            diagnostic.summary(),
            diagnostic.detail().unwrap_or_default()
        );
        assert!(
            !rendered.contains(sentinel),
            "diagnostic must never contain the retained technical source's text: {rendered}"
        );
        assert!(rendered.contains("peer"));
    }
}
