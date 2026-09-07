//! `Failure` mapping for process-bootstrap failures that occur before any
//! command even runs (`src/main.rs`), i.e. outside the `gat` library crate
//! proper. Exposes a small `pub` constructor (rather than widening
//! `Diagnostic`/`Failure`'s own visibility) since `main.rs` cannot use
//! `pub(in crate::error)` items across the crate boundary.

use super::super::{Diagnostic, ErrorCode, Failure};

/// The process-wide execution resources `main.rs` needs before it can enter
/// any command failed to start. The technical source is retained only as a
/// hidden source -- a user never needs (and must never see) the raw
/// OS/runtime message.
#[must_use]
pub fn runtime_start_failed(source: impl std::error::Error + Send + Sync + 'static) -> Failure {
    Failure::infrastructure(
        Diagnostic::new(
            ErrorCode::Internal,
            "Gat could not start its internal task runtime",
        ),
        source,
    )
}
