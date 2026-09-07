//! Typed failures for engine-owned managed `.git/info/exclude`
//! regeneration.

use crate::repository::RepositoryError;
use gat_io::LockError;
use gat_io::StateStoreError;

pub(crate) type Result<T> = std::result::Result<T, ExcludesError>;

/// Everything that can go wrong regenerating or removing Gat's managed
/// `.git/info/exclude` block.
#[derive(Debug, thiserror::Error)]
pub enum ExcludesError {
    /// `gat.lock` failed to load/parse (only [`super::sync`]'s
    /// full-lock path reads it directly; the store-backed paths go
    /// through [`ExcludesError::StateStore`] instead).
    #[error(transparent)]
    Lock(#[from] LockError),

    /// Reading/writing `gat.yaml` (any scope) failed.
    #[error(transparent)]
    Config(#[from] Box<RepositoryError>),

    /// The materialized/desired-state database failed to open, read, or
    /// write while streaming desired paths or exclude-record metadata.
    #[error(transparent)]
    StateStore(#[from] StateStoreError),

    /// Physical observation/publication of `.git/info/exclude` failed.
    #[error(transparent)]
    Io(#[from] gat_io::InfoExcludeError),
}
