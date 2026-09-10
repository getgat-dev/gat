//! `Failure` mapping for `gat_command::InitError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::InitError;
use gat_engine::InitializationErrorKind;

impl From<InitError> for Failure {
    fn from(err: InitError) -> Self {
        if let InitError::Repository(source) = err {
            return (*source).into();
        }
        let InitError::Engine(source) = &err else {
            unreachable!()
        };
        match source.kind() {
            InitializationErrorKind::OpenRepository => Self::infrastructure(
                Diagnostic::new(
                    filesystem_or(source, ErrorCode::RepositoryUnavailable),
                    "Could not open the git repository",
                )
                .with_subject(UserLine::path(
                    source.path().expect("git integration errors carry a path"),
                )),
                err,
            ),
            InitializationErrorKind::NonUtf8Hook => Self::expected(
                Diagnostic::new(
                    ErrorCode::GitOperationFailed,
                    "An existing git hook is not valid UTF-8",
                )
                .with_subject(UserLine::path(
                    source.path().expect("git integration errors carry a path"),
                ))
                .with_hint(
                    "gat only supports text-based managed-block editing; convert or remove it \
                     manually.",
                ),
            ),
            InitializationErrorKind::GitConfigLocked => Self::expected_with_source(
                Diagnostic::new(ErrorCode::Conflict, "Git's configuration lock is already present")
                    .with_hint("Wait for any Git process updating configuration to finish. If the lock remains, confirm that no process owns it before removing the stale lock."),
                err,
            ),
            InitializationErrorKind::GitConfig => Self::infrastructure(
                Diagnostic::new(
                    filesystem_or(source, ErrorCode::GitOperationFailed),
                    "Could not access the git repository's configuration",
                ),
                err,
            ),
            InitializationErrorKind::Read => Self::infrastructure(
                Diagnostic::new(filesystem_or(source, ErrorCode::FilesystemUnavailable),
                    "Could not read Git integration files"), err,
            ),
            InitializationErrorKind::Write => Self::infrastructure(
                Diagnostic::new(
                    filesystem_or(source, ErrorCode::FilesystemUnavailable),
                    "Could not update Git integration files",
                ),
                err,
            ),
            InitializationErrorKind::ConfigScaffold => Self::infrastructure(
                Diagnostic::new(filesystem_or(source, ErrorCode::FilesystemUnavailable), "Could not create gat.yaml"),
                err,
            ),
        }
    }
}

const fn filesystem_or(error: &gat_engine::InitializationError, fallback: ErrorCode) -> ErrorCode {
    match error.filesystem_failure() {
        Some(gat_engine::FilesystemFailureKind::PermissionDenied) => ErrorCode::PermissionDenied,
        Some(gat_engine::FilesystemFailureKind::StorageExhausted) => ErrorCode::StorageExhausted,
        Some(gat_engine::FilesystemFailureKind::Unavailable) | None => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_engine::InitializationError;

    #[test]
    fn scaffold_failure_preserves_os_category_without_rendering_sources() {
        for (kind, code) in [
            (
                std::io::ErrorKind::PermissionDenied,
                ErrorCode::PermissionDenied,
            ),
            (std::io::ErrorKind::StorageFull, ErrorCode::StorageExhausted),
            (std::io::ErrorKind::Other, ErrorCode::FilesystemUnavailable),
        ] {
            let error = InitializationError::from(gat_io::ConfigWriteError::Write(
                gat_io::AtomicError::WriteFailed {
                    path: "SENTINEL_PRIVATE_PATH".into(),
                    source: std::io::Error::new(kind, "SENTINEL_OS_MESSAGE"),
                },
            ));
            let failure: Failure = InitError::Engine(error).into();
            assert_eq!(failure.diagnostic().code(), code);
            assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
            assert!(failure.technical_source().is_some());
        }
    }

    #[test]
    fn existing_git_config_lock_is_a_conflict_and_is_not_deleted() {
        let tmp = test_support::git_repo_with_initial_commit();
        let lock = tmp.path().join(".git/config.lock");
        std::fs::write(&lock, b"owned by another process").unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let error = repo
            .initialization()
            .unwrap()
            .install_merge_driver()
            .unwrap_err();
        let failure: Failure = InitError::Engine(error).into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::Conflict);
        assert_eq!(std::fs::read(&lock).unwrap(), b"owned by another process");
        assert!(
            !failure
                .diagnostic()
                .hints()
                .join(" ")
                .contains("automatically")
        );
    }

    #[test]
    fn missing_repository_keeps_its_repository_classification() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().join("missing"));
        let Err(error) = repo.initialization() else {
            panic!("missing repository unexpectedly opened");
        };
        let failure: Failure = InitError::Engine(error).into();
        assert_eq!(
            failure.diagnostic().code(),
            ErrorCode::RepositoryUnavailable
        );
    }

    #[test]
    fn hook_write_failure_is_a_filesystem_failure() {
        let tmp = test_support::git_repo_with_initial_commit();
        let layout = gat_io::RepositoryLayout::at(tmp.path().to_path_buf());
        let integration = gat_io::GitIntegration::open(&layout).unwrap();
        let name = "post-checkout";
        let path = tmp.path().join(".git/hooks").join(name);
        std::fs::create_dir_all(&path).unwrap();
        let source = integration.write_hook(name, "payload").unwrap_err();
        assert_eq!(source.kind(), gat_io::GitIntegrationErrorKind::Write);
        assert!(source.io_kind().is_some());
        let failure: Failure = InitError::Engine(source.into()).into();
        // Windows reports writing over a directory as access denied.
        let expected = if cfg!(windows) {
            ErrorCode::PermissionDenied
        } else {
            ErrorCode::FilesystemUnavailable
        };
        assert_eq!(failure.diagnostic().code(), expected);
    }
}
