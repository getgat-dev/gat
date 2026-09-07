//! `Failure` mapping for `gat-engine` path-policy errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_engine::{PathPolicyError, UnknownRemoteOverrideError};

impl From<UnknownRemoteOverrideError> for Failure {
    fn from(err: UnknownRemoteOverrideError) -> Self {
        let hint = UserLine::compose([
            UserLine::authored("add it with `gat remote add "),
            UserLine::identifier(&err.name),
            UserLine::authored(" <url>` or choose a configured remote"),
        ]);
        Self::expected(
            Diagnostic::new(ErrorCode::RemoteNotFound, "Unknown remote")
                .with_subject(UserLine::identifier(&err.name))
                .with_hint(hint),
        )
    }
}

impl From<PathPolicyError> for Failure {
    fn from(err: PathPolicyError) -> Self {
        match &err {
            PathPolicyError::UnknownRouteRemote {
                route_name, remote, ..
            } => Self::expected(
                Diagnostic::new(ErrorCode::RemoteNotFound, "Route names an unknown remote")
                    .with_subject(UserLine::identifier(route_name))
                    .with_hint(UserLine::compose([
                        UserLine::authored("add `"),
                        UserLine::identifier(&remote.clone()),
                        UserLine::authored("` with `gat remote add "),
                        UserLine::identifier(&remote.clone()),
                        UserLine::authored(" <url>` or fix the route"),
                    ])),
            ),
            PathPolicyError::UnknownDefault { name } => Self::expected(
                Diagnostic::new(ErrorCode::RemoteNotFound, "Unknown default remote")
                    .with_subject(UserLine::identifier(name))
                    .with_hint(UserLine::compose([
                        UserLine::authored("add it with `gat remote add "),
                        UserLine::identifier(&name.clone()),
                        UserLine::authored(" <url>` or fix remotes.default"),
                    ])),
            ),
        }
    }
}
