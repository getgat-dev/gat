//! `Failure` mapping for typed transfer command and engine errors.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;

fn remote_resolution_context_for(
    remote_name: &str,
    route: Option<&gat_engine::TransferRoute>,
) -> UserLine {
    let mut parts = vec![
        UserLine::authored("remote `"),
        UserLine::identifier(remote_name),
        UserLine::authored("`"),
    ];
    if let Some(route) = route {
        parts.extend([
            UserLine::authored(" via route `"),
            UserLine::identifier(route.name.as_str()),
            UserLine::authored("` (`"),
            UserLine::path_text(route.path.as_str()),
            UserLine::authored("`)"),
        ]);
    }
    UserLine::compose(parts)
}

impl From<gat_command::PushError> for Failure {
    fn from(err: gat_command::PushError) -> Self {
        match err {
            gat_command::PushError::Repository(source) => (*source).into(),
            gat_command::PushError::Acquisition(source) => (*source).into(),
            gat_command::PushError::DesiredState(source) => source.into(),
            gat_command::PushError::History(source) => source.into(),
            gat_command::PushError::Lock(source) => source.into(),
            gat_command::PushError::RemoteCatalog(source) => source.into(),
            gat_command::PushError::RemoteSession(source) => source.into(),
            gat_command::PushError::UnknownOverride(source) => source.into(),
            gat_command::PushError::MissingRemoteConfig(source) => source.into(),
            gat_command::PushError::Presence(source) => source.into(),
            gat_command::PushError::Upload(source) => source.into(),
        }
    }
}

impl From<gat_command::FetchError> for Failure {
    fn from(err: gat_command::FetchError) -> Self {
        match err {
            gat_command::FetchError::Repository(source) => (*source).into(),
            gat_command::FetchError::Acquisition(source) => (*source).into(),
            gat_command::FetchError::DesiredState(source) => source.into(),
            gat_command::FetchError::History(source) => source.into(),
            gat_command::FetchError::Lock(source) => source.into(),
            gat_command::FetchError::RemoteCatalog(source) => source.into(),
            gat_command::FetchError::RemoteSession(source) => source.into(),
            gat_command::FetchError::UnknownOverride(source) => source.into(),
            gat_command::FetchError::MissingRemoteConfig(source) => source.into(),
            gat_command::FetchError::Download(source) => source.into(),
        }
    }
}

impl From<gat_engine::DownloadError> for Failure {
    fn from(err: gat_engine::DownloadError) -> Self {
        if let gat_engine::DownloadError::RemoteOpen {
            remote_name,
            source,
            path,
            ..
        } = &err
            && let Some(diagnostic) = source
                .kind()
                .and_then(|kind| super::super::remote::readiness_diagnostic(kind, remote_name))
        {
            return Self::infrastructure(
                diagnostic.with_subject(UserLine::path_text(path.as_str())),
                err,
            );
        }
        use gat_engine::{DownloadCacheFailureKind, DownloadError, DownloadRemoteFailureKind};

        match &err {
            DownloadError::Identity(source) => (*source).into(),
            DownloadError::Cancelled => Self::expected(Diagnostic::new(
                ErrorCode::Interrupted,
                "Transfer cancelled",
            )),
            DownloadError::RemoteOpen {
                remote_name,
                route,
                path,
                source,
            } => {
                let code = source
                    .kind()
                    .map_or(ErrorCode::Internal, super::super::remote::remote_open_code);
                let description = remote_resolution_context_for(remote_name, route.as_ref());
                Self::infrastructure(
                    Diagnostic::new(
                        code,
                        UserLine::compose([UserLine::authored("Could not open "), description]),
                    )
                    .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }

            DownloadError::RemoteRead {
                kind,
                remote_name,
                route,
                path,
                ..
            } => {
                let code = match kind {
                    DownloadRemoteFailureKind::PermissionDenied => ErrorCode::PermissionDenied,
                    DownloadRemoteFailureKind::NotFound => ErrorCode::ObjectMissing,
                    DownloadRemoteFailureKind::Unavailable => ErrorCode::RemoteUnavailable,
                    DownloadRemoteFailureKind::OperationFailed => ErrorCode::RemoteOperationFailed,
                };
                let description = remote_resolution_context_for(remote_name, route.as_ref());
                Self::infrastructure(
                    Diagnostic::new(
                        code,
                        UserLine::compose([
                            UserLine::authored("Could not read the object from "),
                            description,
                        ]),
                    )
                    .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }
            DownloadError::Cache { kind, path, .. } => {
                let code = match kind {
                    DownloadCacheFailureKind::PermissionDenied => ErrorCode::PermissionDenied,
                    DownloadCacheFailureKind::Unavailable => ErrorCode::CacheUnavailable,
                };
                Self::infrastructure(
                    Diagnostic::new(code, "Could not store the downloaded object")
                        .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }
            DownloadError::HashMismatch { path, .. } => Self::expected(
                Diagnostic::new(
                    ErrorCode::ObjectCorrupt,
                    "The downloaded object's content did not match its expected hash",
                )
                .with_subject(UserLine::path_text(path.as_str()))
                .with_hint("This may indicate a corrupted or tampered remote; try fetching again"),
            ),
            DownloadError::TaskFailed { path, .. } => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::Internal,
                    "The download task for this object did not complete normally",
                )
                .with_subject(UserLine::path_text(path.as_str())),
                err,
            ),
        }
    }
}

impl From<gat_engine::UploadError> for Failure {
    fn from(err: gat_engine::UploadError) -> Self {
        if let gat_engine::UploadError::RemoteOpen {
            remote_name,
            source,
            path,
            ..
        } = &err
            && let Some(diagnostic) = source
                .kind()
                .and_then(|kind| super::super::remote::readiness_diagnostic(kind, remote_name))
        {
            return Self::infrastructure(
                diagnostic.with_subject(UserLine::path_text(path.as_str())),
                err,
            );
        }
        use gat_engine::{
            UploadCacheFailureKind, UploadError, UploadRemoteFailureKind, UploadWriteFailureKind,
        };

        let cleanup_failed = matches!(
            err,
            UploadError::Cleanup { .. } | UploadError::FileCleanup { .. }
        );
        let mut primary = &err;
        while let UploadError::Cleanup { primary: cause, .. }
        | UploadError::FileCleanup { primary: cause, .. } = primary
        {
            primary = cause;
        }
        match primary {
            UploadError::Identity(source) => (*source).into(),
            UploadError::Cancelled => Self::infrastructure(
                if cleanup_failed {
                    Diagnostic::new(
                        ErrorCode::RemoteOperationFailed,
                        "Transfer cancelled, but upload cleanup failed",
                    )
                } else {
                    Diagnostic::new(ErrorCode::Interrupted, "Transfer cancelled")
                },
                err,
            ),
            UploadError::Cleanup { .. } | UploadError::FileCleanup { .. } => {
                unreachable!("cleanup wrappers were unwrapped")
            }
            UploadError::FileWrite {
                kind,
                state,
                remote_name,
                route,
                path,
                source,
            } => {
                let code = match kind {
                    _ if *state == gat_engine::FileUploadFailure::Cancelled
                        && !cleanup_failed
                        && source.cleanup.is_none() =>
                    {
                        ErrorCode::Interrupted
                    }
                    UploadWriteFailureKind::PermissionDenied => ErrorCode::RemotePermissionDenied,
                    UploadWriteFailureKind::OperationFailed => ErrorCode::RemoteOperationFailed,
                };
                let description = remote_resolution_context_for(remote_name, route.as_ref());
                let message = match state {
                    gat_engine::FileUploadFailure::Published => {
                        "The object is published, but upload cleanup or durability failed on "
                    }
                    gat_engine::FileUploadFailure::Cancelled => "File upload cancelled on ",
                    gat_engine::FileUploadFailure::NotPublished => {
                        "Could not publish the file upload to "
                    }
                };
                Self::infrastructure(
                    Diagnostic::new(
                        code,
                        UserLine::compose([UserLine::authored(message), description]),
                    )
                    .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }
            UploadError::CacheVerification { kind, path, .. }
            | UploadError::CacheOpen { kind, path, .. }
            | UploadError::CacheRead { kind, path, .. } => {
                let code = match kind {
                    UploadCacheFailureKind::PermissionDenied => ErrorCode::PermissionDenied,
                    UploadCacheFailureKind::Missing => ErrorCode::ObjectMissing,
                    UploadCacheFailureKind::Unavailable => ErrorCode::CacheUnavailable,
                };
                Self::infrastructure(
                    Diagnostic::new(code, "Could not read the cached object")
                        .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }
            UploadError::RemoteOpen {
                remote_name,
                route,
                path,
                source,
            } => {
                let code = source
                    .kind()
                    .map_or(ErrorCode::Internal, super::super::remote::remote_open_code);
                let description = remote_resolution_context_for(remote_name, route.as_ref());
                Self::infrastructure(
                    Diagnostic::new(
                        code,
                        UserLine::compose([UserLine::authored("Could not open "), description]),
                    )
                    .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }
            UploadError::WriterOpen {
                kind,
                remote_name,
                route,
                path,
                ..
            } => {
                let code = match kind {
                    UploadRemoteFailureKind::PermissionDenied => ErrorCode::RemotePermissionDenied,
                    UploadRemoteFailureKind::Unavailable => ErrorCode::RemoteUnavailable,
                    UploadRemoteFailureKind::OperationFailed => ErrorCode::RemoteOperationFailed,
                };
                let description = remote_resolution_context_for(remote_name, route.as_ref());
                Self::infrastructure(
                    Diagnostic::new(
                        code,
                        UserLine::compose([
                            UserLine::authored("Could not open an upload to "),
                            description,
                        ]),
                    )
                    .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }
            UploadError::WriterWrite {
                kind,
                remote_name,
                route,
                path,
                ..
            } => {
                let code = match kind {
                    UploadWriteFailureKind::PermissionDenied => ErrorCode::PermissionDenied,
                    UploadWriteFailureKind::OperationFailed => ErrorCode::RemoteOperationFailed,
                };
                let description = remote_resolution_context_for(remote_name, route.as_ref());
                Self::infrastructure(
                    Diagnostic::new(
                        code,
                        UserLine::compose([
                            UserLine::authored("Could not write the object to "),
                            description,
                        ]),
                    )
                    .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }
            UploadError::WriterFinalize {
                remote_name,
                route,
                path,
                ..
            } => {
                let description = remote_resolution_context_for(remote_name, route.as_ref());
                Self::infrastructure(
                    Diagnostic::new(
                        ErrorCode::RemoteOperationFailed,
                        UserLine::compose([
                            UserLine::authored("Could not finalize the upload to "),
                            description,
                        ]),
                    )
                    .with_subject(UserLine::path_text(path.as_str())),
                    err,
                )
            }
            UploadError::TaskFailed { path, .. } => Self::infrastructure(
                Diagnostic::new(
                    ErrorCode::Internal,
                    "The upload task for this object did not complete normally",
                )
                .with_subject(UserLine::path_text(path.as_str())),
                err,
            ),
        }
    }
}

impl From<gat_engine::RemotePresenceError> for Failure {
    fn from(err: gat_engine::RemotePresenceError) -> Self {
        if let gat_engine::RemotePresenceError::RemoteOpen {
            remote_name,
            source,
            path,
            ..
        } = &err
            && let Some(diagnostic) = source
                .kind()
                .and_then(|kind| super::super::remote::readiness_diagnostic(kind, remote_name))
        {
            return Self::infrastructure(
                diagnostic.with_subject(UserLine::path_text(path.as_str())),
                err,
            );
        }
        let (code, action, remote_name, route, path) = match &err {
            gat_engine::RemotePresenceError::Identity(source) => return (*source).into(),
            gat_engine::RemotePresenceError::Cancelled => {
                return Self::expected(Diagnostic::new(
                    ErrorCode::Interrupted,
                    "Transfer cancelled",
                ));
            }
            gat_engine::RemotePresenceError::RemoteOpen {
                remote_name,
                route,
                path,
                source,
            } => (
                source
                    .kind()
                    .map_or(ErrorCode::Internal, super::super::remote::remote_open_code),
                "Could not open ",
                remote_name,
                route.as_ref(),
                path,
            ),
            gat_engine::RemotePresenceError::PresenceCheck {
                remote_name,
                route,
                path,
                ..
            } => (
                ErrorCode::RemoteOperationFailed,
                "Could not check ",
                remote_name,
                route.as_ref(),
                path,
            ),
        };
        let description = remote_resolution_context_for(remote_name, route);
        let suffix = if matches!(err, gat_engine::RemotePresenceError::PresenceCheck { .. }) {
            " for this object"
        } else {
            ""
        };
        let summary = UserLine::compose([
            UserLine::authored(action),
            description,
            UserLine::authored(suffix),
        ]);
        Self::infrastructure(
            Diagnostic::new(code, summary).with_subject(UserLine::path_text(path.as_str())),
            err,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_receive_source_failure_keeps_context_without_rendering_physical_paths() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        gat_io::initialize_backends();
        let remote = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let secret_path = remote.path().join("FILE-READ-SECRET");
        std::fs::create_dir(&secret_path).unwrap();
        let client = gat_io::RemoteClient::open(
            &gat_io::remote_file_url_for_test(&secret_path),
            &gat_io::InvocationInputs::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .templates(),
            gat_core::settings::NetworkOptions::default(),
        )
        .unwrap();
        let cache = gat_io::RepositoryLayout::at(local.path().to_owned()).resolve_cache_root(None);
        let error = client
            .prepare_file_read(gat_core::oid::Oid::from_bytes([0; 32]))
            .unwrap()
            .receive(cache.writer(), || false)
            .unwrap_err();
        let gat_io::FileReceiveError::Remote(source) = error else {
            panic!("expected remote source failure")
        };
        let failure: Failure = gat_engine::DownloadError::RemoteRead {
            kind: gat_engine::DownloadRemoteFailureKind::NotFound,
            remote_name: "origin".into(),
            route: Some(gat_engine::TransferRoute {
                name: gat_core::name::RouteName::from_string("assets".into()),
                path: gat_core::lexical_path::GatPath::parse_canonical("data files").unwrap(),
            }),
            path: gat_core::lexical_path::GatPath::parse_canonical("file.bin").unwrap(),
            source: Box::new(source),
        }
        .into();
        assert!(!failure.diagnostic().summary().contains("FILE-READ-SECRET"));
        assert_eq!(failure.diagnostic().subject(), Some("file.bin"));
        for width in [20, 39, 40, 60, 100, 120] {
            let mut bytes = Vec::new();
            let mut stdout = Vec::new();
            let mut output = crate::output::Output::new(&mut stdout, &mut bytes);
            output.set_layouts(
                crate::output::OutputLayout::default(),
                crate::output::OutputLayout::bounded(width),
            );
            crate::output::error::render(&mut output, failure.diagnostic()).unwrap();
            let plain = crate::output::strip_ansi(&String::from_utf8(bytes).unwrap());
            assert!(plain.contains("`data files`"));
            assert!(!plain.contains("FILE-READ-SECRET"));
            for line in plain.lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= width.min(100)
                        || line.trim() == "(`data files`)",
                    "{width}: {line}"
                );
            }
        }

        assert!(
            failure
                .technical_source()
                .unwrap()
                .downcast_ref::<gat_engine::DownloadError>()
                .is_some()
        );
    }

    #[test]
    fn push_lock_failure_retains_the_core_error_without_io_rewrapping() {
        let source = gat_core::lock::LockError::from(
            gat_core::lexical_path::GatPath::normalize("../escape").unwrap_err(),
        );
        let failure: Failure = gat_command::PushError::Lock(source).into();

        assert_eq!(
            failure.diagnostic().code(),
            ErrorCode::PathOutsideRepository
        );
        assert!(
            failure
                .technical_source()
                .is_some_and(|source| source.downcast_ref::<gat_core::lock::LockError>().is_some())
        );
    }

    #[test]
    fn remote_presence_diagnostic_keeps_semantic_context_and_redacts_source() {
        const SENTINEL: &str = "SECRET-BACKEND-SENTINEL";
        let err = gat_engine::RemotePresenceError::PresenceCheck {
            remote_name: "origin".into(),
            route: Some(gat_engine::TransferRoute {
                name: gat_core::name::RouteName::from_string("assets".to_string()),
                path: gat_core::lexical_path::GatPath::parse_canonical("vendor/assets").unwrap(),
            }),
            path: gat_core::lexical_path::GatPath::parse_canonical("vendor/assets/a.bin").unwrap(),
            source: Box::new(std::io::Error::other(SENTINEL)),
        };

        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert!(diagnostic.summary().contains("origin"));
        assert!(diagnostic.summary().contains("assets"));
        assert!(diagnostic.summary().contains("vendor/assets"));
        assert_eq!(diagnostic.subject(), Some("vendor/assets/a.bin"));
        assert!(!diagnostic.summary().contains(SENTINEL));
        assert!(!diagnostic.subject().unwrap().contains(SENTINEL));
    }

    #[test]
    fn remote_open_uses_the_engine_classification_and_retains_transfer_context() {
        let source = gat_engine::test_support::remote_session_error_for_test(
            "unsupported://host/path?token=SYNTHETIC-SECRET",
        );
        let err = gat_engine::DownloadError::RemoteOpen {
            remote_name: "origin".into(),
            route: None,
            path: gat_core::lexical_path::GatPath::parse_canonical("assets/a.bin").unwrap(),
            source: Box::new(source),
        };

        let failure: Failure = err.into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::RemoteInvalid);
        assert!(failure.diagnostic().summary().contains("origin"));
        assert_eq!(failure.diagnostic().subject(), Some("assets/a.bin"));
        assert!(!failure.diagnostic().summary().contains("SYNTHETIC-SECRET"));
        assert!(failure.technical_source().is_some_and(|source| {
            source.downcast_ref::<gat_engine::DownloadError>().is_some()
        }));
    }

    #[test]
    fn download_diagnostic_keeps_semantic_context_and_redacts_source() {
        const SENTINEL: &str = "SECRET-DOWNLOAD-SENTINEL";
        let err = gat_engine::DownloadError::RemoteRead {
            kind: gat_engine::DownloadRemoteFailureKind::OperationFailed,
            remote_name: "origin".into(),
            route: Some(gat_engine::TransferRoute {
                name: gat_core::name::RouteName::from_string("assets".to_string()),
                path: gat_core::lexical_path::GatPath::parse_canonical("vendor/assets").unwrap(),
            }),
            path: gat_core::lexical_path::GatPath::parse_canonical("vendor/assets/a.bin").unwrap(),
            source: Box::new(std::io::Error::other(SENTINEL)),
        };

        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert!(diagnostic.summary().contains("origin"));
        assert!(diagnostic.summary().contains("assets"));
        assert!(diagnostic.summary().contains("vendor/assets"));
        assert_eq!(diagnostic.subject(), Some("vendor/assets/a.bin"));
        assert!(!diagnostic.summary().contains(SENTINEL));
        assert!(!diagnostic.subject().unwrap().contains(SENTINEL));
    }

    #[test]
    fn upload_writer_diagnostic_keeps_semantic_context_and_redacts_source() {
        const SENTINEL: &str = "SECRET-UPLOAD-SENTINEL";
        let err = gat_engine::UploadError::WriterWrite {
            kind: gat_engine::UploadWriteFailureKind::OperationFailed,
            remote_name: "origin".into(),
            route: Some(gat_engine::TransferRoute {
                name: gat_core::name::RouteName::from_string("assets".to_string()),
                path: gat_core::lexical_path::GatPath::parse_canonical("vendor/assets").unwrap(),
            }),
            path: gat_core::lexical_path::GatPath::parse_canonical("vendor/assets/a.bin").unwrap(),
            source: std::io::Error::other(SENTINEL),
        };
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert!(diagnostic.summary().contains("origin"));
        assert!(diagnostic.summary().contains("assets"));
        assert!(diagnostic.summary().contains("vendor/assets"));
        assert_eq!(diagnostic.subject(), Some("vendor/assets/a.bin"));
        assert!(!diagnostic.summary().contains(SENTINEL));
        let source = failure
            .technical_source()
            .expect("infrastructure() must retain a technical source");
        assert!(source.downcast_ref::<gat_engine::UploadError>().is_some());
    }

    #[test]
    fn cancellation_does_not_hide_a_failed_cleanup() {
        let cancelled = Failure::from(gat_engine::UploadError::Cancelled);
        assert_eq!(cancelled.diagnostic().code(), ErrorCode::Interrupted);
        let failure = Failure::from(gat_engine::UploadError::FileCleanup {
            primary: Box::new(gat_engine::UploadError::Cancelled),
            cleanup: Box::new(gat_io::FileWriteError {
                phase: gat_io::FileWritePhase::Cleanup,
                source: std::io::Error::other("CLEANUP-SECRET"),
                cleanup: None,
            }),
        });
        assert_eq!(
            failure.diagnostic().code(),
            ErrorCode::RemoteOperationFailed
        );
        assert!(failure.diagnostic().summary().contains("cleanup failed"));
        assert!(!failure.diagnostic().summary().contains("CLEANUP-SECRET"));
    }

    #[test]
    fn file_cancellation_without_cleanup_failure_remains_interrupted() {
        let failure = Failure::from(gat_engine::UploadError::FileWrite {
            kind: gat_engine::UploadWriteFailureKind::OperationFailed,
            state: gat_engine::FileUploadFailure::Cancelled,
            remote_name: "origin".into(),
            route: None,
            path: gat_core::lexical_path::GatPath::parse_canonical("file.bin").unwrap(),
            source: Box::new(gat_io::FileWriteError {
                phase: gat_io::FileWritePhase::Cancelled,
                source: std::io::Error::other("CANCELLATION-SECRET"),
                cleanup: None,
            }),
        });
        assert_eq!(failure.diagnostic().code(), ErrorCode::Interrupted);
        assert!(!failure.diagnostic().summary().contains("SECRET"));
    }

    #[test]
    fn file_publication_diagnostic_is_honest_and_redacts_both_sources() {
        for (state, phase) in [
            (
                gat_engine::FileUploadFailure::NotPublished,
                gat_io::FileWritePhase::Copy,
            ),
            (
                gat_engine::FileUploadFailure::Published,
                gat_io::FileWritePhase::DirectorySync,
            ),
            (
                gat_engine::FileUploadFailure::Cancelled,
                gat_io::FileWritePhase::Cancelled,
            ),
        ] {
            let published = state == gat_engine::FileUploadFailure::Published;
            let cancelled = state == gat_engine::FileUploadFailure::Cancelled;
            let failure: Failure = gat_engine::UploadError::FileWrite {
                kind: gat_engine::UploadWriteFailureKind::OperationFailed,
                state,
                remote_name: "origin".into(),
                route: None,
                path: gat_core::lexical_path::GatPath::parse_canonical("file.bin").unwrap(),
                source: Box::new(gat_io::FileWriteError {
                    phase,
                    source: std::io::Error::other("PRIMARY-SECRET"),
                    cleanup: (!published).then(|| std::io::Error::other("CLEANUP-SECRET")),
                }),
            }
            .into();
            let diagnostic = failure.diagnostic();
            assert!(!diagnostic.summary().contains("SECRET"));
            assert_eq!(diagnostic.subject(), Some("file.bin"));
            assert_eq!(diagnostic.summary().contains("is published"), published);
            assert_eq!(diagnostic.summary().contains("cancelled"), cancelled);
            let retained = failure
                .technical_source()
                .unwrap()
                .downcast_ref::<gat_engine::UploadError>()
                .unwrap();
            assert!(
                matches!(retained, gat_engine::UploadError::FileWrite { source, .. } if source.cleanup.is_some() != published)
            );
        }
    }

    #[test]
    fn upload_payload_limit_retains_counts_without_rendering_low_level_context() {
        let failure: Failure = gat_engine::UploadError::WriterOpen {
            kind: gat_engine::UploadRemoteFailureKind::OperationFailed,
            remote_name: "origin".into(),
            route: None,
            path: gat_core::lexical_path::GatPath::parse_canonical("file.bin").unwrap(),
            source: Box::new(gat_io::RemoteError::PayloadLimitExceeded {
                required_bytes: 1024,
                limit_bytes: 512,
            }),
        }
        .into();
        assert!(failure.diagnostic().summary().contains("origin"));
        assert_eq!(failure.diagnostic().subject(), Some("file.bin"));
        let source = failure
            .technical_source()
            .unwrap()
            .downcast_ref::<gat_engine::UploadError>()
            .unwrap();
        assert!(
            matches!(source, gat_engine::UploadError::WriterOpen { source, .. }
            if matches!(source.as_ref(), gat_io::RemoteError::PayloadLimitExceeded {
                required_bytes: 1024, limit_bytes: 512, ..
            }))
        );
    }

    #[test]
    fn upload_cleanup_failure_retains_both_errors_without_rendering_sources() {
        let primary = gat_engine::UploadError::CacheRead {
            kind: gat_engine::UploadCacheFailureKind::Unavailable,
            path: gat_core::lexical_path::GatPath::parse_canonical("file.bin").unwrap(),
            source: std::io::Error::other("PRIMARY-SECRET"),
        };
        let cleanup = gat_io::RemoteError::CleanupTimedOut;
        let failure: Failure = gat_engine::UploadError::Cleanup {
            primary: Box::new(primary),
            cleanup: Box::new(cleanup),
        }
        .into();
        assert_eq!(failure.diagnostic().subject(), Some("file.bin"));
        assert!(!failure.diagnostic().summary().contains("SECRET"));
        let source = failure
            .technical_source()
            .unwrap()
            .downcast_ref::<gat_engine::UploadError>()
            .unwrap();
        assert!(matches!(source, gat_engine::UploadError::Cleanup { .. }));
    }

    /// A cancelled (never-panicked) `JoinError` is the easiest one to
    /// construct synchronously in a unit test; used below to exercise
    /// both task-failure mappers without spinning up a runtime.
    fn cancelled_join_error() -> tokio::task::JoinError {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            let handle = tokio::spawn(std::future::pending::<()>());
            handle.abort();
            handle.await.expect_err("aborted task yields a JoinError")
        })
    }

    #[test]
    fn upload_task_failed_retains_the_whole_typed_error_as_the_technical_source() {
        let err = gat_engine::UploadError::TaskFailed {
            path: gat_core::lexical_path::GatPath::parse_canonical("objects/up/loaded").unwrap(),
            source: cancelled_join_error(),
        };
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert!(!diagnostic.summary().contains("JoinError"));
        assert!(!diagnostic.summary().contains("cancelled"));
        let source = failure
            .technical_source()
            .expect("infrastructure() must retain a technical source");
        let typed = source
            .downcast_ref::<gat_engine::UploadError>()
            .expect("the whole UploadError must be retained, not just its JoinError");
        assert!(matches!(
            typed,
            gat_engine::UploadError::TaskFailed { path, .. }
                if path.as_str() == "objects/up/loaded"
        ));
    }

    /// Download task-failure retention mirrors the upload-side test above.
    #[test]
    fn download_task_failed_retains_the_whole_typed_error_as_the_technical_source() {
        let err = gat_engine::DownloadError::TaskFailed {
            path: gat_core::lexical_path::GatPath::parse_canonical("objects/down/loaded").unwrap(),
            source: cancelled_join_error(),
        };
        let failure: Failure = err.into();
        let diagnostic = failure.diagnostic();
        assert!(!diagnostic.summary().contains("JoinError"));
        assert!(!diagnostic.summary().contains("cancelled"));
        let source = failure
            .technical_source()
            .expect("infrastructure() must retain a technical source");
        let typed = source
            .downcast_ref::<gat_engine::DownloadError>()
            .expect("the whole DownloadError must be retained, not just its JoinError");
        assert!(matches!(
            typed,
            gat_engine::DownloadError::TaskFailed { path, .. }
                if path.as_str() == "objects/down/loaded"
        ));
    }
}
