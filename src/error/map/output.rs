//! Safe wording for durable output failures.
use crate::error::{Diagnostic, Failure};
use crate::output::{Stream, WriteFailure};

impl From<WriteFailure> for Failure {
    fn from(failure: WriteFailure) -> Self {
        let summary = match failure.stream {
            Stream::Stdout => "Could not write command output to stdout",
            Stream::Stderr => "Could not write command output to stderr",
        };
        Self::infrastructure(
            Diagnostic::new(super::io_code(&failure.source), summary),
            failure,
        )
    }
}
