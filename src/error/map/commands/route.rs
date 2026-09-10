//! `Failure` mapping for `gat_command::RouteError`.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::RouteError;

impl From<RouteError> for Failure {
    fn from(err: RouteError) -> Self {
        match err {
            RouteError::Scope(source) => source.into(),
            RouteError::UnknownRemote { remote } => Self::expected(
                Diagnostic::new(ErrorCode::RemoteNotFound, "No remote with that name")
                    .with_subject(UserLine::identifier(remote.as_str()))
                    .with_hint(UserLine::compose([
                        UserLine::authored("Run `"),
                        UserLine::compose([
                            UserLine::authored("gat remote add "),
                            UserLine::identifier(remote.as_str()),
                            UserLine::authored(" <url>"),
                        ])
                        .unbroken(),
                        UserLine::authored("` to configure it."),
                    ])),
            ),
            RouteError::ReservedName => Self::expected(
                Diagnostic::new(
                    ErrorCode::InvalidConfig,
                    UserLine::compose([
                        UserLine::authored("Route name `*` is reserved for "),
                        UserLine::authored("`gat route list`").unbroken(),
                        UserLine::authored("'s synthetic default-remote row"),
                    ]),
                )
                .with_hint("Choose a different name."),
            ),
            RouteError::AlreadyExists { name } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidConfig, "That route already exists")
                    .with_subject(UserLine::identifier(name.as_str()))
                    .with_hint(UserLine::compose([
                        UserLine::authored("Run `"),
                        UserLine::compose([
                            UserLine::authored("gat route update "),
                            UserLine::identifier(name.as_str()),
                            UserLine::authored(""),
                        ])
                        .unbroken(),
                        UserLine::authored("` instead."),
                    ])),
            ),
            RouteError::NotFound { name } => Self::expected(
                Diagnostic::new(ErrorCode::InvalidConfig, "No route with that name")
                    .with_subject(UserLine::identifier(name.as_str())),
            ),
            RouteError::Repository(source) => (*source).into(),
        }
    }
}
