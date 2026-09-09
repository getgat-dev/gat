//! Typed failures for `.gat/state/state.sqlite3` (materialized state,
//! desired state, and reconciliation metadata). Distinct from
//! [`crate::cache::proof::CacheStateError`]
//! (`cache.sqlite3`, the shared object-cache proof index), which is its
//! own sibling SQLite-backed state store with its own classification.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateSqlErrorKind {
    Busy,
    Corrupt,
    PermissionDenied,
    StorageExhausted,
    Unavailable,
}

#[derive(Debug)]
pub struct StateSqlError {
    kind: StateSqlErrorKind,
    source: rusqlite::Error,
}

impl StateSqlError {
    pub(crate) const fn from_sqlite(source: rusqlite::Error) -> Self {
        let kind = match &source {
            rusqlite::Error::SqliteFailure(error, _) => match error.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    StateSqlErrorKind::Busy
                }
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                    StateSqlErrorKind::Corrupt
                }
                rusqlite::ErrorCode::PermissionDenied => StateSqlErrorKind::PermissionDenied,
                rusqlite::ErrorCode::DiskFull => StateSqlErrorKind::StorageExhausted,
                _ => StateSqlErrorKind::Unavailable,
            },
            _ => StateSqlErrorKind::Unavailable,
        };
        Self { kind, source }
    }

    #[must_use]
    pub const fn kind(&self) -> StateSqlErrorKind {
        self.kind
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn for_test(kind: StateSqlErrorKind) -> Self {
        Self {
            kind,
            source: rusqlite::Error::InvalidQuery,
        }
    }
}

impl std::fmt::Display for StateSqlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for StateSqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// This module's own `Result` alias -- every fallible function under
/// `state_store` returns a typed [`StateStoreError`], not
/// `anyhow::Result`. `?` in a caller that still returns `anyhow::Result`
/// keeps working unchanged (`StateStoreError` implements
/// `std::error::Error + Send + Sync + 'static`, which `anyhow::Error`
/// converts from automatically).
pub(crate) type Result<T> = std::result::Result<T, StateStoreError>;

/// Local SQLite-backed state-store failures. The vast majority of this
/// module's fallible `SQLite` calls (preparing a statement, executing a
/// query, reading a row back, beginning/committing a transaction) share
/// one [`StateStoreError::QueryFailed`] variant carrying a short
/// `operation` description rather than one dedicated Rust variant per SQL
/// statement --
/// there are simply too many distinct statements in this module for a
/// per-statement variant to be practical, and the *classification* that
/// actually matters (busy/locked, corrupt, permission-denied, disk-full,
/// or none of the above) is derived uniformly from the underlying
/// [`rusqlite::Error`] by the mapping layer's rusqlite classifier, not
/// from which statement failed.
#[derive(Debug, thiserror::Error)]
pub enum StateStoreError {
    /// The database file itself could not be opened (or opened
    /// read-only, for [`super::StateStore::open_if_exists`]).
    #[error("could not open the state database `{}`", path.display())]
    OpenFailed {
        path: PathBuf,
        #[source]
        source: StateSqlError,
    },
    /// Local storage could not be prepared before opening the database.
    /// `path` identifies the directory or its required self-ignore file.
    #[error("could not prepare local state at `{}`", path.display())]
    DirectoryUnavailable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The database file lives on a filesystem that cannot support the
    /// WAL journal mode Gat requires (e.g. some network mounts, which
    /// don't support WAL's shared-memory file).
    #[error(
        "{} could not be switched to WAL journal mode (got {mode}); this can happen on \
         filesystems that do not support WAL's shared-memory file (e.g. some network mounts) \
         and is not currently supported",
        path.display()
    )]
    WalUnsupported { path: PathBuf, mode: String },
    /// The database declares a schema version this build does not
    /// support. Unsupported versions fail closed rather than being migrated,
    /// discarded, rebuilt, or silently reinterpreted. See
    /// `schema::SCHEMA_VERSION`.
    #[error(
        "{} was written by a schema version this build of gat does not support (found \
         {found}, expected {expected})",
        path.display()
    )]
    UnsupportedSchemaVersion {
        path: PathBuf,
        found: i64,
        expected: i64,
    },
    /// A hex object id (from an in-memory [`gat_core::lock::Entry`])
    /// could not be parsed while encoding a row for storage -- a
    /// malformed in-memory value, not database corruption.
    #[error("invalid object id {context}")]
    InvalidOid {
        context: String,
        #[source]
        source: gat_core::oid::OidFormatError,
    },
    /// A row (or fingerprint/identity blob) read back from the database
    /// does not have the shape this build expects -- e.g. a wrong-length
    /// oid/fingerprint blob, or a desired row with no `desired_shard_id`.
    /// Never repeats the offending bytes/lengths in the user-facing
    /// root diagnostics summary (only in this variant's own
    /// `Display`, which is never rendered to a user).
    #[error("the state database contains a malformed row: {detail}")]
    InvalidRow { detail: String },
    /// Any other rusqlite call -- preparing a statement, executing a
    /// query, reading a row, beginning/committing a transaction, or
    /// updating a row -- failed. See this enum's own doc comment for why
    /// these share one variant.
    #[error("{operation} failed")]
    QueryFailed {
        operation: String,
        #[source]
        source: StateSqlError,
    },
}

/// Converts a `rusqlite::Result` directly into a typed
/// [`StateStoreError::QueryFailed`] while preserving the operation
/// description.
pub(crate) trait StateResultExt<T> {
    fn state_context(self, operation: impl Into<String>) -> Result<T>;

    /// Lazy counterpart to [`Self::state_context`]: `operation` is only
    /// invoked on the error path, so a per-chunk `format!(...)` call site
    /// avoids allocating a description string on every successful
    /// mutation.
    fn with_state_context<F, S>(self, operation: F) -> Result<T>
    where
        F: FnOnce() -> S,
        S: Into<String>;
}

impl<T> StateResultExt<T> for rusqlite::Result<T> {
    fn state_context(self, operation: impl Into<String>) -> Result<T> {
        self.map_err(|source| StateStoreError::QueryFailed {
            operation: operation.into(),
            source: StateSqlError::from_sqlite(source),
        })
    }

    fn with_state_context<F, S>(self, operation: F) -> Result<T>
    where
        F: FnOnce() -> S,
        S: Into<String>,
    {
        self.map_err(|source| StateStoreError::QueryFailed {
            operation: operation().into(),
            source: StateSqlError::from_sqlite(source),
        })
    }
}

/// Lets a bare `?` on a `rusqlite::Error` (e.g. `row.get(0)?` inside a
/// `query_map`/`query_row` callback, where there is no already-known
/// `operation` description to attach) convert straight into a
/// [`StateStoreError::QueryFailed`] with a generic operation description.
/// Prefer `StateResultExt::state_context` wherever a more specific
/// description is available.
impl From<rusqlite::Error> for StateStoreError {
    fn from(source: rusqlite::Error) -> Self {
        Self::QueryFailed {
            operation: "reading a row".to_string(),
            source: StateSqlError::from_sqlite(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_failure(code: rusqlite::ErrorCode) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code: 0,
            },
            Some("internal sqlite detail".to_string()),
        )
    }

    #[test]
    fn sqlite_result_codes_map_to_semantic_state_kinds() {
        let cases = [
            (rusqlite::ErrorCode::DatabaseBusy, StateSqlErrorKind::Busy),
            (rusqlite::ErrorCode::DatabaseLocked, StateSqlErrorKind::Busy),
            (
                rusqlite::ErrorCode::DatabaseCorrupt,
                StateSqlErrorKind::Corrupt,
            ),
            (
                rusqlite::ErrorCode::NotADatabase,
                StateSqlErrorKind::Corrupt,
            ),
            (
                rusqlite::ErrorCode::PermissionDenied,
                StateSqlErrorKind::PermissionDenied,
            ),
            (
                rusqlite::ErrorCode::DiskFull,
                StateSqlErrorKind::StorageExhausted,
            ),
            (rusqlite::ErrorCode::Unknown, StateSqlErrorKind::Unavailable),
        ];

        for (code, expected) in cases {
            assert_eq!(
                StateSqlError::from_sqlite(sqlite_failure(code)).kind(),
                expected
            );
        }
        assert_eq!(
            StateSqlError::from_sqlite(rusqlite::Error::QueryReturnedNoRows).kind(),
            StateSqlErrorKind::Unavailable
        );
    }
}
