//! Pure `gat.lock` domain values and operations.
//!
//! Physical persistence, reshape, and coherent observation live in
//! `gat-io`. This module exposes semantic lock values directly and keeps
//! implementation modules private; storage readers that must validate
//! borrowed rows without materializing a whole lock use the deliberately
//! narrow [`validated`] facade.

mod codec;
/// Canonical byte serialization for resident and streaming destinations.
pub mod encoding;
mod error;
mod identity;
mod merge;
mod reader;
mod shard;

pub use codec::{Entry, EscapedPath, Lock, VERSION, path_matches_scope};
pub use error::{InvalidOidReason, LockDomainError, LockError, MalformedRowReason, Result};
pub use identity::{
    CanonicalDesiredIdentity, LOCK_VERSION, ShardContentIdentity, ShardContentIdentityDecodeError,
};
pub use merge::{Conflict, merge_three_way};
pub use shard::{LockShardId, LockShardIdError, LockShardLevels, LockShardLevelsError};

/// Validated, allocation-conscious row access for storage implementations.
///
/// These operations expose semantic rows and validation state only. The TSV
/// codec implementation remains private, while `gat-io` can stream or select
/// validated rows without reparsing OIDs or allocating paths it will discard.
///
/// Codec helpers are intentionally not flattened into [`crate::lock`]:
///
/// ```compile_fail
/// use gat_core::lock::parse_row;
/// ```
pub mod validated {
    pub use super::codec::{
        FilteredRowCursor, check_ordered_row_conflict, entry_from_validated_parts,
        exceeds_directory_upper_bound, is_directory_prefix, parse_row,
        validate_no_path_directory_conflicts, visit_filtered_matching, visit_rows_validated,
    };
    pub use super::reader::ValidatedLockFile;

    /// Compute the shard identity for row text whose canonical path form was
    /// already proven by this facade's parser/visitor contract.
    ///
    /// This hashes the borrowed row directly, avoiding a temporary
    /// [`crate::lexical_path::GatPath`] allocation in storage readers.
    #[must_use]
    pub fn shard_id_for_path(path: &str, levels: super::LockShardLevels) -> super::LockShardId {
        super::LockShardId::for_validated_path(path, levels)
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use reader::test_probes;
