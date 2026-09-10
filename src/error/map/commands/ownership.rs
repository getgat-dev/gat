//! `Failure` mapping for `gat_command::OwnershipError`, shared by
//! `add`/`rm`/`mv`'s mount-ownership guard.

use super::super::super::{Diagnostic, ErrorCode, Failure};
use crate::presentation::UserLine;
use gat_command::OwnershipError;

impl From<OwnershipError> for Failure {
    fn from(err: OwnershipError) -> Self {
        Self::expected(
            Diagnostic::new(
                ErrorCode::MountOwnedPath,
                UserLine::compose([
                    UserLine::authored("This path is owned by mount `"),
                    UserLine::identifier(err.mount.as_str()),
                    UserLine::authored("`; only "),
                    UserLine::authored("`gat mount`").unbroken(),
                    UserLine::authored(" commands can change it"),
                ]),
            )
            .with_subject(UserLine::gat_path(&err.path))
            .with_hint(UserLine::compose([
                UserLine::authored("use "),
                UserLine::authored("`gat mount`").unbroken(),
                UserLine::authored(" commands to change paths under `"),
                UserLine::gat_path(&err.target),
                UserLine::authored("`"),
            ])),
        )
    }
}
