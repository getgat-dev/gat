//! `Failure` mapping for `gat_command::AddError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::AddError;

impl From<AddError> for Failure {
    fn from(err: AddError) -> Self {
        match err {
            AddError::Snapshot(source) => (*source).into(),
            AddError::AlreadyGitTracked { ref path } => Self::expected(
                Diagnostic::new(ErrorCode::Conflict, "This path is already tracked by git")
                    .with_subject(UserLine::gat_path(path))
                    .with_hint(UserLine::compose([
                        UserLine::authored("run `"),
                        UserLine::compose([UserLine::authored("git rm --cached "), UserLine::gat_path(path)]).unbroken(),
                        UserLine::authored("` first if you want gat to manage it"),
                    ])),
            ),
            AddError::IgnoredByGit { ref path } => Self::expected(
                Diagnostic::new(ErrorCode::Conflict, "This path is ignored by git")
                    .with_subject(UserLine::gat_path(path))
                    .with_hint(
                        "adjust the ignore rule (or its precedence), or pass --force, if you want gat to track it",
                    ),
            ),
            AddError::IgnoredByGatignore { ref path } => Self::expected(
                Diagnostic::new(ErrorCode::Conflict, "This path is ignored by .gatignore")
                    .with_subject(UserLine::gat_path(path))
                    .with_hint("edit .gatignore first if you want gat to track it (or pass --force)"),
            ),
            AddError::MountOwned(source) => source.into(),
            AddError::Path(source) => source.into(),
            AddError::Lexical(source) => source.into(),
            AddError::UnsupportedFileType { ref path } => Self::expected(
                Diagnostic::new(ErrorCode::UnsupportedFileType, "This file type is not supported")
                    .with_subject(UserLine::gat_path(path))
                    .with_hint("gat only tracks regular files and directories of them"),
            ),
            AddError::NoMatch { ref pattern } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidArgumentValue, "This pattern matched no files")
                    .with_subject(UserLine::identifier(pattern.as_str())),
            ),
            AddError::Glob(source) => source.into(),
            AddError::Config(source) => (*source).into(),
            AddError::PathPolicy(source) => source.into(),
            AddError::RemoteCatalog(source) => source.into(),
            AddError::RepositoryState(source) => (*source).into(),
            AddError::RepositoryMutation(source) => {
                let published = matches!(*source,
                    gat_engine::RepositoryMutationError::RecordMaterialized { .. }
                    | gat_engine::RepositoryMutationError::RegenerateExcludes { .. });
                let mut failure = Self::from(*source);
                if published {
                    *failure.diagnostic = failure.diagnostic.with_detail(
                        "The added paths were already published to gat.lock; tracking completed but metadata cleanup is incomplete.");
                }
                failure
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_failure_reports_completed_tracking_and_preserves_state_classification() {
        let failure: Failure = AddError::RepositoryMutation(Box::new(
            gat_engine::RepositoryMutationError::RecordMaterialized {
                source: Box::new(
                    gat_io::StateStoreError::InvalidRow {
                        detail: "SENTINEL".into(),
                    }
                    .into(),
                ),
            },
        ))
        .into();
        assert_eq!(failure.diagnostic().code(), ErrorCode::StateCorrupt);
        assert!(
            failure
                .diagnostic()
                .detail()
                .unwrap()
                .contains("already published")
        );
        assert!(!format!("{:?}", failure.diagnostic()).contains("SENTINEL"));
        assert!(failure.technical_source().is_some());
    }
}
