//! `Failure` mapping for `gat_engine::RemoteCatalogError`.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_engine::RemoteCatalogError;

impl From<RemoteCatalogError> for Failure {
    fn from(err: RemoteCatalogError) -> Self {
        match err {
            RemoteCatalogError::UnknownOverride(source) => source.into(),
            RemoteCatalogError::UnknownDefault { name } => Self::expected(
                Diagnostic::new(ErrorCode::RemoteNotFound, "Unknown default remote")
                    .with_subject(UserLine::identifier(&name))
                    .with_hint(UserLine::compose([
                        UserLine::authored("add it with `gat remote add "),
                        UserLine::identifier(&name),
                        UserLine::authored(" <url>` or fix remotes.default"),
                    ])),
            ),
            RemoteCatalogError::NoRemoteConfigured => Self::expected(
                Diagnostic::new(ErrorCode::RemoteNotFound, "No remote configured")
                    .with_hint("run `gat remote add <name> <url>`"),
            ),
        }
    }
}
