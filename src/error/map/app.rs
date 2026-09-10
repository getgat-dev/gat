//! `Failure` mapping for `crate::app`'s app-level policy errors and
//! synchronization-completion diagnostics.

use super::super::{Diagnostic, ErrorCode, Failure};
use crate::app::AppError;
use crate::presentation::UserLine;

impl From<AppError> for Failure {
    fn from(err: AppError) -> Self {
        let diagnostic = match err {
            AppError::HistoryRequiresRemote => Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                "History flags require `--remote`",
            )
            .with_detail(UserLine::compose([
                UserLine::authored("local "),
                UserLine::authored("`gat status`").unbroken(),
                UserLine::authored(
                    " has no history concept; history selection only makes \
                 sense when comparing against a remote's history.",
                ),
            ]))
            .with_hint(UserLine::compose([
                UserLine::authored("Add "),
                UserLine::authored("`--remote <name>`").unbroken(),
                UserLine::authored(", or remove the history flag(s)."),
            ])),
            AppError::DryRunConflictsWithFetch => Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                "`--dry-run` cannot be combined with `--fetch`",
            )
            .with_hint("`--dry-run` never fetches; remove one of the two flags."),
            AppError::DryRunConflictsWithRepair => Diagnostic::new(
                ErrorCode::InvalidArgumentValue,
                "`--dry-run` cannot be combined with `--repair`",
            )
            .with_hint("`--dry-run` never repairs; remove one of the two flags."),
        };
        Self::expected(diagnostic)
    }
}

/// Summarizes an incomplete sync returned as an error before a report exists.
/// Completed sync outcomes retain their own presentation and exit status.
pub fn sync_completion_conflict(conflicts: usize, missing: usize, corrupted: usize) -> Failure {
    Failure::expected(Diagnostic::new(
        ErrorCode::Conflict,
        UserLine::compose([
            UserLine::authored("sync incomplete: "),
            UserLine::number(conflicts as i64),
            UserLine::authored(" conflict(s), "),
            UserLine::number(missing as i64),
            UserLine::authored(" missing object(s), "),
            UserLine::number(corrupted as i64),
            UserLine::authored(" corrupted object(s)"),
        ]),
    ))
}
