//! `Failure` mapping for command and engine mount workflows.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::MountError;
use gat_engine::MountSourceErrorKind;
#[cfg(test)]
use gat_engine::MountWorkflowError;

impl From<MountError> for Failure {
    fn from(err: MountError) -> Self {
        match &err {
            MountError::RootOwnedAssets { .. } => Self::expected(
                Diagnostic::new(
                    ErrorCode::MountOwnedPath,
                    "That target already holds root-owned assets",
                )
                .with_hint(
                    "a mount cannot claim a target that already holds root-owned gat.lock rows",
                ),
            ),
            MountError::NotFound { .. } => Self::expected(Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                "No mount is defined with that name",
            )),
            MountError::AlreadyExists { name } => Self::expected(
                Diagnostic::new(ErrorCode::Conflict, "That mount already exists").with_hint(
                    UserLine::compose([
                        UserLine::authored("run `"),
                        UserLine::compose([
                            UserLine::authored("gat mount remove "),
                            UserLine::identifier(name.as_str()),
                            UserLine::authored(""),
                        ])
                        .unbroken(),
                        UserLine::authored("` first to change it"),
                    ]),
                ),
            ),
            MountError::ShadowedOnAdd { .. }
            | MountError::WouldShadowExisting { .. }
            | MountError::ShadowedOnMutate { .. }
            | MountError::RemovalWouldRevealOwnershipChange { .. } => {
                Self::expected(Diagnostic::new(
                    ErrorCode::Conflict,
                    "That mount conflicts with another mount definition",
                ))
            }
            MountError::WrongScope { actual_scope, .. } => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "No mount is defined at that scope",
                )
                .with_hint(UserLine::compose([
                    UserLine::authored("pass `"),
                    UserLine::authored(config_scope_flag(*actual_scope)),
                    UserLine::authored("` to target it there"),
                ])),
            ),
            MountError::ExplicitRemoteNotFound { .. } => Self::expected(Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                "That remote is not configured in either repository",
            )),
            MountError::RouteScopeConflict { defining_scope, .. } => Self::expected(
                Diagnostic::new(
                    ErrorCode::Conflict,
                    "That route is defined at a higher-precedence scope",
                )
                .with_hint(UserLine::compose([
                    UserLine::authored("update the route with `"),
                    UserLine::authored(config_scope_flag(*defining_scope)),
                    UserLine::authored("` instead"),
                ])),
            ),
            MountError::Source { kind, location, .. } => match kind {
                MountSourceErrorKind::InvalidLocation => Self::expected(
                    Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        "Not a recognized Git repository location",
                    )
                    .with_hint(
                        "Provide a local path, an `https://`/`http://`/`ssh://`/`git://`/`file://` \
                             URL, or an scp-like `[user@]host:path`.",
                    ),
                ),
                MountSourceErrorKind::MissingRepositoryName => Self::expected(
                    Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        "Could not infer a mount target name from that location",
                    )
                    .with_hint("Specify TARGET explicitly."),
                ),
                MountSourceErrorKind::LocalSourceMissing => Self::expected(Diagnostic::new(
                    ErrorCode::InvalidArgumentValue,
                    "That local source does not exist",
                )),
                MountSourceErrorKind::ResolveRevision => Self::expected_with_source(
                    Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        "Could not resolve that revision in the source repository",
                    ),
                    err,
                ),
                MountSourceErrorKind::ClonePrepare
                | MountSourceErrorKind::CloneFetch
                | MountSourceErrorKind::CloneCheckout => {
                    let url = crate::redaction::RedactedUrl::render(location.as_location_str());
                    Self::infrastructure(
                        Diagnostic::new(
                            ErrorCode::RemoteUnavailable,
                            "Could not clone the source repository",
                        )
                        .with_subject(UserLine::redacted_url(&url)),
                        err,
                    )
                }
                MountSourceErrorKind::UnsupportedHashKind => Self::expected_with_source(
                    Diagnostic::new(
                        ErrorCode::GitOperationFailed,
                        "The source repository uses an unsupported Git object hash format",
                    )
                    .with_hint("Use a source repository with a supported Git object format."),
                    err,
                ),
                MountSourceErrorKind::InferredTarget => Self::expected_with_source(
                    Diagnostic::new(
                        ErrorCode::InvalidArgumentValue,
                        "Could not infer a valid mount target from that location",
                    ),
                    err,
                ),
                MountSourceErrorKind::ResolvePath
                | MountSourceErrorKind::CreateTemporary
                | MountSourceErrorKind::OpenRepository
                | MountSourceErrorKind::LoadConfig => Self::infrastructure(
                    Diagnostic::new(
                        ErrorCode::FilesystemUnavailable,
                        "Could not prepare the mount source repository",
                    ),
                    err,
                ),
            },
            MountError::Repository(_) => {
                let MountError::Repository(source) = err else {
                    unreachable!()
                };
                (*source).into()
            }
            MountError::Workflow(_) => {
                let MountError::Workflow(source) = err else {
                    unreachable!()
                };
                (*source).into()
            }
        }
    }
}

const fn config_scope_flag(scope: gat_core::config::ConfigScope) -> &'static str {
    match scope {
        gat_core::config::ConfigScope::Global => "--global",
        gat_core::config::ConfigScope::Project => "--project",
        gat_core::config::ConfigScope::Local => "--local",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_workflow_fault_maps_without_panicking_or_leaking_its_label() {
        let label = "SENTINEL_MOUNT_FAULT";
        let _guard = gat_core::fault::armed(label);
        let fault = gat_core::fault::hit(label).unwrap_err();
        let failure = Failure::from(MountWorkflowError::Fault(fault));
        assert_eq!(
            failure.diagnostic().code(),
            ErrorCode::FilesystemUnavailable
        );
        assert!(!failure.diagnostic().summary().contains(label));
        assert!(failure.diagnostic().subject().is_none());
        assert!(failure.technical_source().is_some());
    }

    /// A failed mount clone must never render the raw gix failure text
    /// or an unredacted credential-bearing URL; only the authored
    /// summary and the redacted subject may appear.
    #[test]
    fn source_clone_failure_never_renders_raw_gix_text_or_credentials() {
        let sentinel = "SENTINEL_GIX_CLONE_7c21fe08";
        let secret = "s3cr3t-token-9f0a";
        // hygiene-ok: synthetic credential-bearing test URL exercising redaction on a mount-clone failure; never dialed.
        let url = format!("https://user:{secret}@example.invalid/repo.git");
        let io_error = std::io::Error::other(format!("{sentinel}: connection reset"));
        let source =
            gat_engine::MountSourceError::test_error(MountSourceErrorKind::CloneFetch, io_error);
        let err = MountError::Source {
            kind: MountSourceErrorKind::CloneFetch,
            location: gat_core::git_location::GitLocationSpec::from_string(url),
            revision: None,
            source: Box::new(source),
        };
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        let rendered = format!(
            "{}{}{}",
            diagnostic.summary(),
            diagnostic.subject().unwrap_or_default(),
            diagnostic.detail().unwrap_or_default()
        );
        assert!(
            !rendered.contains(sentinel),
            "rendered diagnostic must never contain the raw gix error text: {rendered}"
        );
        assert!(
            !rendered.contains(secret),
            "rendered diagnostic must never contain the unredacted credential: {rendered}"
        );
    }

    /// Malformed mount-transaction journal JSON
    /// must never render the raw `serde_json` parser message, only the
    /// static authored summary/hint.
    #[test]
    fn malformed_transaction_journal_json_never_renders_the_raw_serde_error_text() {
        let sentinel = "SENTINEL_SERDE_JSON_2b9f14ac";
        let json_err = serde_json::from_str::<serde_json::Value>(&format!("{{{sentinel}"))
            .expect_err("deliberately malformed JSON");
        let err = MountWorkflowError::ReadJournal {
            source: Box::new(json_err),
        };
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        let rendered = format!(
            "{}{}{}",
            diagnostic.summary(),
            diagnostic.subject().unwrap_or_default(),
            diagnostic.detail().unwrap_or_default()
        );
        assert!(
            !rendered.contains(sentinel),
            "rendered diagnostic must never contain the raw serde_json error text: {rendered}"
        );
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn unreadable_recovery_journal_requires_repair_without_rendering_its_source() {
        for snapshot in [false, true] {
            let error = MountWorkflowError::ReadJournal {
                source: Box::new(std::io::Error::other("SENTINEL_JOURNAL")),
            };
            let failure: Failure = if snapshot {
                gat_engine::RepoSnapshotError::from(error).into()
            } else {
                error.into()
            };
            assert_eq!(failure.diagnostic().code(), ErrorCode::RepairRequired);
            assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
            assert!(failure.technical_source().is_some());
        }
    }
}
