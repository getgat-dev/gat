//! `Failure` mapping for `gat_command::RemoteStatusError`.

use super::super::super::Failure;
use gat_command::RemoteStatusError;

impl From<RemoteStatusError> for Failure {
    fn from(err: RemoteStatusError) -> Self {
        match err {
            RemoteStatusError::Repository(source) => (*source).into(),
            RemoteStatusError::Acquisition(source) => (*source).into(),
            RemoteStatusError::DesiredState(source) => source.into(),
            RemoteStatusError::History(source) => source.into(),
            RemoteStatusError::Lock(source) => source.into(),
            RemoteStatusError::RemoteCatalog(source) => source.into(),
            RemoteStatusError::RemoteSession(source) => source.into(),
            RemoteStatusError::UnknownOverride(source) => source.into(),
            RemoteStatusError::MissingRemoteConfig(source) => source.into(),
            RemoteStatusError::Presence(source) => source.into(),
        }
    }
}

impl From<gat_command::MissingRemoteConfigError> for Failure {
    fn from(err: gat_command::MissingRemoteConfigError) -> Self {
        use super::super::super::{Diagnostic, ErrorCode};
        use crate::presentation::UserLine;

        Self::expected(
            Diagnostic::new(ErrorCode::RemoteNotConfigured, "No remote configured")
                .with_subject(UserLine::path_text(err.path.as_str()))
                .with_hint(
                    "Run `gat remote add <name> <url>`, or set `remotes.default` to an \
                     already-configured remote.",
                ),
        )
    }
}
