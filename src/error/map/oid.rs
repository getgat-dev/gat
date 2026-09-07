//! `Failure` mapping for `gat_core::oid`'s typed errors.

use super::super::{Diagnostic, ErrorCode, Failure};
use gat_core::oid::OidFormatError;

impl From<OidFormatError> for Failure {
    fn from(err: OidFormatError) -> Self {
        Self::infrastructure(
            Diagnostic::new(ErrorCode::Internal, "An object identifier was malformed")
                .with_hint("This should not happen in normal use; it may indicate corrupted state"),
            err,
        )
    }
}
