//! `Failure` mapping for `gat_command::SystemError`.

use super::super::super::{Diagnostic, ErrorCode};
use crate::error::Failure;
use crate::presentation::UserLine;
use gat_command::SystemError;
use gat_engine::MaintenanceErrorKind;

impl From<SystemError> for Failure {
    fn from(err: SystemError) -> Self {
        match &err {
            SystemError::TransactionChoiceRequired => Self::expected(Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                "`--transaction` requires either `--restore-backup` or `--promote-staged`",
            )),
            SystemError::RecoveryChoiceOnlyForLock => Self::expected(Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                UserLine::compose([
                    UserLine::authored(
                        "`--restore-backup` or `--promote-staged` are only valid with ",
                    ),
                    UserLine::authored("`gat system repair lock`").unbroken(),
                ]),
            )),
            SystemError::CachePurgeOnlyForCacheScope => Self::expected(Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                UserLine::compose([
                    UserLine::authored(
                        "`--purge-objects` or `--purge-temporary` are only valid with ",
                    ),
                    UserLine::authored("`gat system clean cache`").unbroken(),
                    UserLine::authored(" or "),
                    UserLine::authored("`gat system clean all`").unbroken(),
                ]),
            )),
            SystemError::Maintenance(source) => {
                if let MaintenanceErrorKind::UnsupportedCacheSchema { version } = source.kind() {
                    return Self::expected(
                        Diagnostic::new(
                            ErrorCode::StateIncompatible,
                            "Gat's local cache uses an unsupported format version",
                        )
                        .with_detail(UserLine::compose([
                            UserLine::authored("found format version "),
                            UserLine::number(version),
                            UserLine::authored("; refusing to purge objects or rewrite it"),
                        ]))
                        .with_hint("Use a compatible `gat` version to repair the cache first."),
                    );
                }
                let (code, summary) = match source.kind() {
                    MaintenanceErrorKind::Repository => (
                        ErrorCode::InvalidConfig,
                        "Could not load repository maintenance configuration",
                    ),
                    MaintenanceErrorKind::Lock => (
                        ErrorCode::StateUnavailable,
                        "Could not maintain Gat lock state",
                    ),
                    MaintenanceErrorKind::State => (
                        ErrorCode::StateUnavailable,
                        "Could not maintain repository state metadata",
                    ),
                    MaintenanceErrorKind::Cache => (
                        ErrorCode::CacheUnavailable,
                        "Could not maintain local cache state",
                    ),
                    MaintenanceErrorKind::Git => (
                        ErrorCode::GitOperationFailed,
                        "Could not maintain Git integration",
                    ),
                    MaintenanceErrorKind::UnsupportedCacheSchema { .. } => unreachable!(),
                };
                Self::infrastructure(Diagnostic::new(code, summary), err)
            }
        }
    }
}
