//! Shared diagnostics for mount mutation and snapshot-barrier recovery failures.

use super::super::{Diagnostic, ErrorCode, Failure};
use gat_engine::{MountRecoveryFailureKind, MountWorkflowError};

impl From<MountWorkflowError> for Failure {
    fn from(error: MountWorkflowError) -> Self {
        Self::infrastructure(recovery_diagnostic(error.recovery_failure_kind()), error)
    }
}

pub(super) fn recovery_diagnostic(kind: MountRecoveryFailureKind) -> Diagnostic {
    match kind {
        MountRecoveryFailureKind::InterruptedTransaction => Diagnostic::new(
            ErrorCode::RepairRequired,
            "Could not read the pending mount transaction journal",
        ).with_hint(
            "Check access to the pending transaction files and use a compatible gat version. Preserve the journal and staged data for recovery.",
        ),
        MountRecoveryFailureKind::Persistence => Diagnostic::new(
            ErrorCode::FilesystemUnavailable, "Could not persist mount transaction state",
        ).with_hint("Check filesystem access and available space before retrying the command."),
        MountRecoveryFailureKind::RepositoryState => Diagnostic::new(
            ErrorCode::FilesystemUnavailable, "Could not update repository mount state",
        ),
    }
}
