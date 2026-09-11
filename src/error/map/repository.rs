//! `Failure` mapping for the engine repository's typed errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use super::filesystem_code;
use crate::presentation::UserLine;
use gat_core::config::ConfigScope;
use gat_engine::{
    RepoError, RepositoryAccessFailureKind, RepositoryAccessLockFailureKind as LockFailureKind,
    RepositoryError,
};

/// `ConfigScope`'s `Display` is already a closed set of static words
/// ("global"/"project"/"local"); expose that as `&'static str` so mapper
/// call sites can embed it in a [`UserLine`] fragment without going
/// through `format!`/`Display`.
const fn scope_word(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Global => "global",
        ConfigScope::Project => "project",
        ConfigScope::Local => "local",
    }
}

impl From<RepositoryError> for Failure {
    fn from(err: RepositoryError) -> Self {
        match &err {
            RepositoryError::Cancelled => Self::expected(Diagnostic::new(
                ErrorCode::Interrupted,
                "Operation cancelled",
            )),
            RepositoryError::PendingMountRecovery(source) => Self::infrastructure(
                super::mount::recovery_diagnostic(source.recovery_failure_kind()),
                err,
            ),
            RepositoryError::ConfigurationChanged { .. } => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::RepositoryUnavailable,
                    "Configuration changed during the operation; retry the command",
                ),
                err,
            ),
            RepositoryError::SettingLock { .. } => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::RepositoryUnavailable,
                    "Could not acquire configuration mutation authority",
                ),
                err,
            ),
            RepositoryError::CurrentDirectory(source) => Self::infrastructure(
                Diagnostic::new(
                    super::io_code(source),
                    "Gat could not determine the current directory",
                )
                .with_hint(
                    "Check that the directory you ran gat from still exists and is readable.",
                ),
                err,
            ),
            RepositoryError::NotRepository => Self::expected(
                Diagnostic::new(ErrorCode::NotRepository, "Not a gat/git repository")
                    .with_detail(
                        "No `.git` was found in the current directory or any parent directory.",
                    )
                    .with_hint(UserLine::compose([
                        UserLine::authored("Run this from inside a git checkout, or "),
                        UserLine::authored("`git init`").unbroken(),
                        UserLine::authored(" first."),
                    ])),
            ),
            RepositoryError::ConfigPathUnavailable => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    "Cannot resolve the global gat config path",
                )
                .with_detail(
                    "$HOME (or %USERPROFILE% on Windows) is not set, so `~/.gat/gat.yaml` has \
                     no location to resolve to.",
                )
                .with_hint("Set $HOME/%USERPROFILE%, or use a project/local config scope instead."),
            ),
            RepositoryError::ConfigLoad { scope, .. } => {
                let message = UserLine::compose([
                    UserLine::authored("Could not load the "),
                    UserLine::authored(scope_word(*scope)),
                    UserLine::authored(" gat.yaml"),
                ]);
                Self::infrastructure(config_diagnostic(&err, message), err)
            }
            RepositoryError::UndefinedResourceRemote { name } => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    "Configuration references an undefined remote",
                )
                .with_subject(UserLine::identifier(name.as_str()))
                .with_hint(UserLine::authored(
                    "Configure the remote or update the referring route or default choice.",
                )),
                err,
            ),
            RepositoryError::InvalidEffectiveMounts(_) => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    "The effective mount configuration is invalid",
                )
                .with_hint(UserLine::compose([
                    UserLine::authored("Run "),
                    UserLine::authored("`gat mount list`").unbroken(),
                    UserLine::authored(" to inspect every configured mount across all scopes."),
                ])),
                err,
            ),
            RepositoryError::InvalidEffectiveSelections(source) => {
                let mut diagnostic =
                    Diagnostic::new(ErrorCode::InvalidConfig, "Default selection does not exist");
                if let gat_core::config::ConfigError::UnknownSelection { name } = source {
                    diagnostic = diagnostic
                        .with_subject(crate::presentation::UserLine::identifier(name.as_str()));
                }
                Self::infrastructure(
                    diagnostic.with_hint(UserLine::compose([
                        UserLine::authored("Choose an existing selection with "),
                        UserLine::authored("`gat selection default`").unbroken(),
                        UserLine::authored(", or unset the dangling default."),
                    ])),
                    err,
                )
            }
            RepositoryError::InvalidEffectiveRoutes(_) => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    "The effective route configuration is invalid",
                )
                .with_hint(UserLine::compose([
                    UserLine::authored("Run "),
                    UserLine::authored("`gat route list`").unbroken(),
                    UserLine::authored(" to inspect every configured route across all scopes."),
                ])),
                err,
            ),
            RepositoryError::ConfigLoadScoped { scope, .. } => {
                let message = UserLine::compose([
                    UserLine::authored("Could not load the "),
                    UserLine::authored(scope_word(*scope)),
                    UserLine::authored(" gat.yaml"),
                ]);
                Self::infrastructure(config_diagnostic(&err, message), err)
            }
            RepositoryError::ConfigDirectoryCreate { scope, .. } => {
                let message = UserLine::compose([
                    UserLine::authored("Could not create the directory for the "),
                    UserLine::authored(scope_word(*scope)),
                    UserLine::authored(" gat.yaml"),
                ]);
                Self::infrastructure(
                    config_diagnostic(&err, message)
                        .with_hint("Check that the parent directory is writable."),
                    err,
                )
            }
            RepositoryError::ConfigSerialize { scope, .. } => {
                let message = UserLine::compose([
                    UserLine::authored("Could not serialize the "),
                    UserLine::authored(scope_word(*scope)),
                    UserLine::authored(" gat.yaml"),
                ]);
                Self::infrastructure(Diagnostic::new(ErrorCode::InvalidConfig, message), err)
            }
            RepositoryError::ConfigWrite { scope, kind, .. } => {
                let code = filesystem_code(*kind);
                let message = UserLine::compose([
                    UserLine::authored("Could not write the "),
                    UserLine::authored(scope_word(*scope)),
                    UserLine::authored(" gat.yaml"),
                ]);
                Self::infrastructure(
                    Diagnostic::new(code, message)
                        .with_hint("Check that the file is writable and the disk is not full."),
                    err,
                )
            }
        }
    }
}

fn config_diagnostic(error: &RepositoryError, summary: UserLine) -> Diagnostic {
    configuration_diagnostic(error.config_failure_kind(), summary)
}

pub(super) fn configuration_diagnostic(
    kind: gat_engine::ConfigAccessFailureKind,
    summary: UserLine,
) -> Diagnostic {
    match kind {
        gat_engine::ConfigAccessFailureKind::Invalid => {
            Diagnostic::new(ErrorCode::InvalidConfig, summary)
        }
        gat_engine::ConfigAccessFailureKind::Filesystem(kind) => {
            Diagnostic::new(filesystem_code(kind), summary)
        }
        gat_engine::ConfigAccessFailureKind::UnsupportedVersion { found, expected } => {
            Diagnostic::new(ErrorCode::UnsupportedConfigVersion, summary)
                .with_detail(UserLine::compose([
                    UserLine::authored("Found configuration version "),
                    UserLine::number(i64::from(found)),
                    UserLine::authored("; this gat supports version "),
                    UserLine::number(i64::from(expected)),
                    UserLine::authored("."),
                ]))
                .with_hint("Use a gat version compatible with this configuration.")
        }
    }
}

impl From<RepoError> for Failure {
    fn from(err: RepoError) -> Self {
        match err {
            RepoError::Config(source) => (*source).into(),
            RepoError::Lock(source) => {
                let kind = source.kind();
                repository_access_failure(kind, RepoError::Lock(source))
            }
            RepoError::Atomic(source) => {
                let kind = source.kind();
                repository_access_failure(kind, RepoError::Atomic(source))
            }
            RepoError::RemoteConfig(source) => source.into(),
        }
    }
}

pub(super) fn repository_access_failure(
    kind: RepositoryAccessFailureKind,
    source: impl std::error::Error + Send + Sync + 'static,
) -> Failure {
    let diagnostic = repository_access_diagnostic(kind);
    match kind {
        RepositoryAccessFailureKind::Lock(
            LockFailureKind::Corrupt
            | LockFailureKind::Incompatible
            | LockFailureKind::InvalidPath
            | LockFailureKind::UnsupportedFileType
            | LockFailureKind::RepositoryLocked
            | LockFailureKind::InvalidArgument,
        )
        | RepositoryAccessFailureKind::RepositoryLocked => {
            Failure::expected_with_source(diagnostic, source)
        }
        RepositoryAccessFailureKind::Lock(
            LockFailureKind::PermissionDenied
            | LockFailureKind::StorageExhausted
            | LockFailureKind::Unavailable
            | LockFailureKind::RepairRequired,
        )
        | RepositoryAccessFailureKind::Filesystem(_) => Failure::infrastructure(diagnostic, source),
    }
}

pub(super) fn repository_access_diagnostic(kind: RepositoryAccessFailureKind) -> Diagnostic {
    match kind {
        RepositoryAccessFailureKind::Filesystem(kind) => Diagnostic::new(
            filesystem_code(kind),
            "Could not acquire the repository lock",
        ),
        RepositoryAccessFailureKind::RepositoryLocked
        | RepositoryAccessFailureKind::Lock(LockFailureKind::RepositoryLocked) => Diagnostic::new(
            ErrorCode::RepositoryLocked,
            "Another gat process is modifying this repository",
        )
        .with_detail("It will release the lock automatically when it finishes or exits."),
        RepositoryAccessFailureKind::Lock(kind) => {
            let (summary, hint) = match kind {
                LockFailureKind::Corrupt => (
                    "gat.lock is corrupted",
                    Some(UserLine::authored(
                        "Restore gat.lock from a trusted version-control revision before retrying.",
                    )),
                ),
                LockFailureKind::Incompatible => (
                    "Unsupported gat.lock format",
                    Some(UserLine::authored(
                        "Upgrade gat or restore gat.lock from a compatible version.",
                    )),
                ),
                LockFailureKind::InvalidPath => (
                    "gat.lock contains an invalid path",
                    Some(UserLine::authored(
                        "Restore gat.lock from version control, or remove the offending row.",
                    )),
                ),
                LockFailureKind::UnsupportedFileType => (
                    "gat.lock is not a regular file or directory",
                    Some(UserLine::authored(
                        "gat.lock must be a regular file or directory, not a symlink, FIFO, \
                         socket, or device.",
                    )),
                ),
                LockFailureKind::PermissionDenied => ("Could not access gat.lock", None),
                LockFailureKind::StorageExhausted => ("Could not access gat.lock", None),
                LockFailureKind::Unavailable => ("Could not access gat.lock", None),
                LockFailureKind::RepairRequired => (
                    "gat.lock was modified while being read",
                    Some(UserLine::compose([
                        UserLine::authored("Run the command again; if this recurs, run "),
                        UserLine::authored("`gat system repair lock`").unbroken(),
                        UserLine::authored("."),
                    ])),
                ),
                LockFailureKind::InvalidArgument => ("Could not resolve a gat.lock pattern", None),
                LockFailureKind::RepositoryLocked => unreachable!("handled above"),
            };
            let diagnostic = Diagnostic::new(super::lock_code(kind), summary);
            match hint {
                Some(hint) => diagnostic.with_hint(hint),
                None => diagnostic,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_engine::{
        RepositoryAccessError, RepositoryAccessFailureKind,
        RepositoryAccessLockFailureKind as LockFailureKind,
    };

    #[test]
    fn not_repository_maps_to_a_specific_diagnostic_with_an_actionable_hint() {
        let failure = Failure::from(RepositoryError::NotRepository);
        let diagnostic = failure.diagnostic();
        assert_eq!(diagnostic.code(), ErrorCode::NotRepository);
        assert!(!diagnostic.summary().is_empty());
        assert!(!diagnostic.hints().is_empty());
    }

    #[test]
    fn current_directory_failure_hides_the_os_message_but_retains_its_source() {
        const SENTINEL: &str = "gat-repository-test-sentinel: stale NFS file handle";
        let failure = Failure::from(RepositoryError::CurrentDirectory(std::io::Error::other(
            SENTINEL,
        )));
        let diagnostic = failure.diagnostic();
        assert!(!diagnostic.summary().contains(SENTINEL));
        assert!(
            diagnostic
                .detail()
                .is_none_or(|detail| !detail.contains(SENTINEL))
        );
        assert!(
            diagnostic
                .hints()
                .iter()
                .all(|hint| !hint.contains(SENTINEL))
        );

        let technical: &dyn std::error::Error = failure
            .technical_source()
            .expect("CurrentDirectory carries a technical source");
        assert!(
            std::iter::successors(Some(technical), |error| error.source())
                .any(|error| error.to_string().contains(SENTINEL))
        );
    }

    #[test]
    fn repository_config_failure_exposes_scope_but_keeps_path_and_parser_text_technical() {
        const PATH_SENTINEL: &str = "gat-config-path-sentinel";
        const PARSER_SENTINEL: &str = "gat-config-parser-sentinel";
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join(PATH_SENTINEL);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("gat.yaml"),
            format!("git:\n  ignore_patterns: {PARSER_SENTINEL}\n"),
        )
        .unwrap();

        let error = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(root)
            .load_config()
            .unwrap_err();
        let failure = Failure::from(error);
        let diagnostic = failure.diagnostic();
        assert_eq!(diagnostic.code(), ErrorCode::InvalidConfig);
        assert!(diagnostic.summary().contains("project"));
        assert_eq!(diagnostic.subject(), None);
        let rendered = format!(
            "{}{}{:?}",
            diagnostic.summary(),
            diagnostic.detail().unwrap_or_default(),
            diagnostic.hints()
        );
        assert!(!rendered.contains(PATH_SENTINEL));
        assert!(!rendered.contains(PARSER_SENTINEL));

        let technical = failure
            .technical_source()
            .expect("repository config failure retains its source");
        let chain = std::iter::successors(
            Some(technical as &(dyn std::error::Error + 'static)),
            |error| error.source(),
        )
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        assert!(chain.contains(PATH_SENTINEL));
        assert!(chain.contains(PARSER_SENTINEL));
    }

    #[test]
    fn repo_lock_failure_uses_semantic_diagnostics_and_retains_the_outer_error() {
        const SENTINEL: &str = "sentinel-private-gat-lock-shard";
        let err = RepoError::Lock(RepositoryAccessError::for_test(
            RepositoryAccessFailureKind::Lock(LockFailureKind::PermissionDenied),
            std::io::Error::other(SENTINEL),
        ));

        let failure = Failure::from(err);
        assert_eq!(failure.diagnostic().code(), ErrorCode::PermissionDenied);
        let rendered = format!(
            "{}{}{:?}",
            failure.diagnostic().summary(),
            failure.diagnostic().detail().unwrap_or_default(),
            failure.diagnostic().hints()
        );
        assert!(!rendered.contains(SENTINEL));

        let technical = failure
            .technical_source()
            .expect("repository access retains its complete source");
        assert!(technical.downcast_ref::<RepoError>().is_some());
        assert!(
            std::iter::successors(Some(technical as &dyn std::error::Error), |error| error
                .source())
            .any(|error| error.to_string().contains(SENTINEL))
        );
    }
}

#[cfg(test)]
mod access_classification_tests {
    use super::*;

    #[test]
    fn repository_and_snapshot_preserve_config_io_and_version_failures() {
        for scoped in [false, true] {
            for (kind, expected) in [
                (
                    std::io::ErrorKind::PermissionDenied,
                    ErrorCode::PermissionDenied,
                ),
                (std::io::ErrorKind::Other, ErrorCode::FilesystemUnavailable),
            ] {
                for snapshot in [false, true] {
                    let source = gat_io::ConfigError::Unreadable {
                        path: "SENTINEL_PRIVATE_PATH".into(),
                        source: std::io::Error::new(kind, "SENTINEL_OS"),
                    };
                    let error = if scoped {
                        RepositoryError::ConfigLoadScoped {
                            scope: ConfigScope::Project,
                            source,
                        }
                    } else {
                        RepositoryError::ConfigLoad {
                            scope: ConfigScope::Project,
                            source,
                        }
                    };
                    let failure: Failure = if snapshot {
                        gat_engine::RepoSnapshotError::from(error).into()
                    } else {
                        error.into()
                    };
                    assert_eq!(failure.diagnostic().code(), expected);
                    assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
                    assert!(failure.technical_source().is_some());
                }
            }
        }
        for snapshot in [false, true] {
            let error = RepositoryError::ConfigLoad {
                scope: ConfigScope::Local,
                source: gat_io::ConfigError::UnsupportedVersion {
                    path: "SENTINEL".into(),
                    found: 99,
                    expected: 1,
                },
            };
            let failure: Failure = if snapshot {
                gat_engine::RepoSnapshotError::from(error).into()
            } else {
                error.into()
            };
            assert_eq!(
                failure.diagnostic().code(),
                ErrorCode::UnsupportedConfigVersion
            );
            assert!(failure.diagnostic().detail().unwrap().contains("99"));
            assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
        }
    }

    #[test]
    fn current_directory_and_config_directory_failures_are_filesystem_errors() {
        let failure: Failure = RepositoryError::CurrentDirectory(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "SENTINEL",
        ))
        .into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::PermissionDenied);
        let failure: Failure = RepositoryError::ConfigDirectoryCreate {
            scope: ConfigScope::Global,
            source: gat_io::ConfigWriteError::Write(gat_io::AtomicError::DirectoryUnavailable {
                path: "SENTINEL".into(),
                source: std::io::Error::new(std::io::ErrorKind::StorageFull, "SENTINEL"),
            }),
        }
        .into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::StorageExhausted);
        assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
    }
}
