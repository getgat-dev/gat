//! Pure `gat.lock` codec/domain errors: every
//! structural fact about row syntax, path canonicality, OID format, or a
//! cross-row/cross-shard path invariant that the `gat-core`-owned
//! [`super::codec`] and [`super::merge`] can raise without any filesystem,
//! `SQLite`, or other I/O dependency.
//!
//! Physical lock failures such as missing/corrupt shards, reshape recovery,
//! and atomic publication are composed around this error by `gat-io`.

use crate::lexical_path::LexicalPathError;

/// This module's own `Result` alias, used throughout `gat_core::lock`'s
/// pure codec/merge implementation.
pub type Result<T> = std::result::Result<T, LockError>;

/// Every way `gat-core`'s pure lock codec can fail: a structural lock-
/// format/domain failure ([`LockDomainError`]), a malformed lexical path
/// supplied to the codec, or an internal visitor callback sentinel.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// A pure lock-format/domain failure -- see [`LockDomainError`].
    #[error(transparent)]
    Domain(#[from] LockDomainError),

    /// A malformed path was supplied to a codec operation without a
    /// persisted row/line number of its own.
    #[error(transparent)]
    LexicalPath(#[from] LexicalPathError),

    /// Placeholder used only to unwind out of a `gat_core::lock` codec
    /// visitor closure (which requires this crate's own [`LockError`])
    /// when the caller's own visitor callback failed with a different,
    /// non-lock error type; the caller always recovers its real error
    /// from its own captured `Option` immediately afterwards and this
    /// variant is never itself surfaced to a user.
    #[error("visit callback failed")]
    CallbackFailed,
}

/// Every pure lock-format/domain failure: a structural fact about
/// `gat.lock` row syntax, path canonicality, OID format, or a
/// cross-row/cross-shard path invariant, that never depends on
/// filesystem or on-disk state.
#[derive(Debug, thiserror::Error)]
pub enum LockDomainError {
    /// A lock file/shard has no content at all -- not even a version
    /// header.
    #[error("lock file is empty")]
    Empty,

    /// The version header of a lock file/shard is missing or does not
    /// match the version this build writes.
    #[error("unrecognised lock-file version: expected {expected:?}, got {got:?}")]
    UnsupportedVersion { expected: String, got: String },

    /// A row failed structural TSV parsing (wrong field count, missing
    /// tab, an otherwise-malformed line).
    #[error("line {line}: {reason}")]
    MalformedRow {
        line: usize,
        reason: MalformedRowReason,
    },

    /// A row's path is not valid per the shared Gat-lexical path rules.
    #[error("line {line}: path {path:?} is invalid")]
    InvalidRowPath {
        line: usize,
        path: String,
        #[source]
        source: LexicalPathError,
    },

    /// A row's path is lexically valid but is not written in its exact
    /// canonical form.
    #[error("line {line}: path {path:?} is not written in its canonical form")]
    NonCanonicalPath { line: usize, path: String },

    /// A row's OID field is malformed (wrong length, not
    /// hex, ...).
    #[error("line {line}: {reason}")]
    InvalidOid {
        line: usize,
        path: String,
        reason: InvalidOidReason,
    },

    /// The same path is tracked by more than one row.
    #[error("path {path:?} is tracked more than once")]
    DuplicatePath { path: String, line: Option<usize> },

    /// The same path is tracked by more than one shard file.
    #[error("path {path:?} is tracked by more than one shard")]
    PathInMultipleShards { path: String },

    /// One tracked path is also a directory-prefix ancestor of another
    /// tracked path.
    #[error(
        "path {ancestor:?} is tracked directly and also as a directory prefix of {descendant:?}"
    )]
    DirectoryPrefixConflict {
        ancestor: String,
        descendant: String,
    },

    /// A row lives in a shard file other than the one its path hashes to
    /// -- corrupt or hand-edited shard placement.
    #[error("path {path:?} is stored in shard {actual:?}, but belongs in {expected:?}")]
    MisplacedShardRow {
        path: String,
        actual: String,
        expected: String,
    },
}

/// Why [`LockDomainError::MalformedRow`] was raised: a structural
/// TSV-parsing fact about a row, never a preformatted parser message.
#[derive(Debug, thiserror::Error)]
pub enum MalformedRowReason {
    #[error("expected a TAB after exactly 64 hexadecimal bytes")]
    InvalidSeparator,
    #[error("record must end with a newline (LF or CRLF)")]
    MissingLineFeed,
    #[error("path {path:?} is not in strictly increasing order")]
    UnorderedPath { path: String },
}

/// Why [`LockDomainError::InvalidOid`] was raised: a structural fact about
/// the row's oid field, never a preformatted parser message.
#[derive(Debug, thiserror::Error)]
pub enum InvalidOidReason {
    #[error("oid is not a valid 64-character lower-case hex blake3 hash")]
    NotHexBlake3,
}
