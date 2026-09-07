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
            .with_detail(
                "local `gat status` has no history concept; history selection only makes \
                 sense when comparing against a remote's history.",
            )
            .with_hint("Add `--remote <name>`, or remove the history flag(s)."),
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

/// Builds the `Failure` `crate::app::exit_result` reports when a
/// `sync`/`pull`/`hook` outcome finished but left conflicts/missing
/// objects/corruption behind: accepts the structured
/// [`gat_command::SyncCompletionStatus::Incomplete`] counts directly
/// (never a caller-composed `String`) and authors the one Gat-owned
/// summary text here, at this single rendering boundary.
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
