//! Typed errors for the `gat.lock` on-disk representation: the semantic
//! model in `super` (TSV row parsing/validation), the canonical
//! desired-state identity in [`super::identity`], and the on-disk flat/
//! sharded persistence and crash-safe reshape machinery in
//! `super::persistence`.

use std::path::PathBuf;

use gat_core::lexical_path::LexicalPathError;

/// Result of lock persistence or validation.
pub type Result<T> = std::result::Result<T, LockError>;

/// Lock validation and persistence failures, with structured row, path, and
/// shard context for the root diagnostic mapper. Underlying parser and I/O
/// error text is not intended for user-facing output.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// A pure lock-format/domain failure -- a structural fact about row
    /// syntax, path canonicality, OID format, or cross-row/cross-shard
    /// path invariants, independent of any filesystem or on-disk state.
    /// The pure validation portion is owned by [`LockDomainError`].
    #[error(transparent)]
    Domain(#[from] LockDomainError),

    /// A shard file (or the whole shard tree) is corrupt in a way not
    /// covered by a more specific variant above.
    #[error("`{}` is corrupt: {source}", path.display())]
    CorruptShard {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A discovered set of [`super::LockShardId`]s spans more than one
    /// on-disk shape -- the flat sentinel alongside sharded leaves, or
    /// sharded leaves at more than one fan-out depth. A completed
    /// `gat.lock` tree (Git-persisted or live on disk) is always written
    /// uniformly, so this can only mean external/manual
    /// corruption, never a state this build's own writers produce.
    #[error(
        "gat.lock is written at more than one shard depth: found depth {first_levels} and \
         depth {second_levels}"
    )]
    MixedShardTopology { first_levels: u8, second_levels: u8 },

    /// `gat.lock` (or a shard leaf) exists but is neither a regular file
    /// nor a directory -- a symlink, FIFO, socket, device, or other
    /// special object where only a regular file or directory is
    /// supported.
    #[error("`{}` is {detail}", path.display())]
    UnsupportedOnDiskKind { path: PathBuf, detail: String },

    /// A shard-relative path's bytes are not valid UTF-8.
    #[error("`{}` is not valid UTF-8", path.display())]
    NonUtf8Path { path: PathBuf },

    /// A filesystem operation on the lock/shard tree failed.
    #[error("could not {operation} `{}`", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A reshape/recovery-time consistency check failed, or an explicit
    /// recovery operation refused to proceed. See [`PersistenceError`]
    /// for the specific structural fact.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// A coherent read of `gat.lock` (or a shard file) detected the file
    /// changed out from under it, or was missing/not a regular file.
    #[error(transparent)]
    FileState(#[from] crate::file_state::FileStateError),

    /// A `gat.lock` write went through `crate::atomic` and failed
    /// there (temp-file creation, write, sync, or publish).
    #[error(transparent)]
    Atomic(#[from] crate::atomic::AtomicError),

    /// The caller-supplied row-producing closure (e.g.
    /// `publish_flat_shard_streaming`'s `next_row`) failed while
    /// producing the next row to publish. `LockError` deliberately has no
    /// direct knowledge of the row source's own error type (it may come
    /// from `storage::state` or elsewhere); the caller boxes its error
    /// explicitly at the point where its own error type and the lock
    /// codec meet, rather than `LockError` growing a `#[from]` conversion
    /// for every possible row source.
    #[error("reading the next lock row from its source failed")]
    RowSource(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// A malformed path was supplied to a codec operation without a
    /// persisted row/line number of its own.
    #[error(transparent)]
    LexicalPath(#[from] LexicalPathError),
}

/// The pure lock-format/domain failure taxonomy, along with its
/// [`MalformedRowReason`]/[`InvalidOidReason`] sub-reasons, lives in
/// `gat_core::lock::error` because none of it depends on a filesystem or
/// on-disk state; it is re-exported here for the I/O layer.
pub use gat_core::lock::{InvalidOidReason, LockDomainError, MalformedRowReason};

/// Every distinct structural fact the crash-safe reshape/recovery
/// machinery in `super::persistence` can fail on -- a semantic
/// classification of "what didn't match", never a preformatted narrative
/// string embedding another error's `Display`. Underlying parse/decode
/// failures are carried as `#[source]`, not interpolated into the
/// message.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// A reshape (or its test-only crash-simulation helper) was invoked
    /// with no on-disk representation at all to reshape.
    #[error("no on-disk gat.lock representation exists to reshape at `{}`", live_path.display())]
    NothingToReshape { live_path: PathBuf },

    /// The freshly staged destination for a reshape doesn't have the
    /// shape (flat/sharded depth) that was targeted.
    #[error(
        "staged reshape at `{}` is shape {actual:?}, expected {expected:?}",
        staging_path.display()
    )]
    StagedShapeMismatch {
        staging_path: PathBuf,
        actual: Option<super::LockShardLevels>,
        expected: super::LockShardLevels,
    },

    /// The freshly staged destination for a reshape failed to parse back
    /// as a valid lock.
    #[error("staged reshape at `{}` failed to parse back", staging_path.display())]
    StagedLockInvalid {
        staging_path: PathBuf,
        #[source]
        source: Box<LockError>,
    },

    /// The freshly staged destination for a reshape parsed back fine but
    /// its path -> oid map doesn't match the source lock's.
    #[error(
        "staged reshape at `{}` doesn't contain the same path -> oid map as the source lock",
        staging_path.display()
    )]
    StagedOidMapMismatch { staging_path: PathBuf },

    /// Encoding the durable reshape transaction record to JSON failed.
    #[error("encoding reshape transaction record failed")]
    TxnRecordEncode {
        #[source]
        source: serde_json::Error,
    },

    /// A durable reshape transaction record is malformed (fails to parse
    /// as JSON, or as the expected record shape).
    #[error("`{}` is a malformed reshape transaction record", record_path.display())]
    TxnRecordMalformed {
        record_path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// A durable reshape transaction record has an unrecognized phase --
    /// refusing to guess whether the reshape it describes is complete.
    #[error(
        "`{}` has unrecognized reshape transaction phase {phase:?}",
        record_path.display()
    )]
    TxnRecordUnrecognizedPhase { record_path: PathBuf, phase: String },

    /// A reshape transaction record proves a live representation existed
    /// when the transaction was prepared, but the live path is currently
    /// missing and not reflected on disk as complete -- an explicit
    /// recovery step is required.
    #[error(
        "`{}` records a reshape of `{}` that was interrupted; explicit recovery is required",
        record_path.display(), live_path.display()
    )]
    ReshapeInterruptedLiveMissing {
        record_path: PathBuf,
        live_path: PathBuf,
    },

    /// A reshape transaction was interrupted after the old on-disk
    /// representation was moved aside but before the new one was
    /// promoted into place -- an explicit recovery step is required.
    #[error(
        "`{}` was interrupted after moving `{}` aside to `{}`",
        record_path.display(), live_path.display(), backup_path.display()
    )]
    ReshapeInterruptedAfterBackup {
        record_path: PathBuf,
        live_path: PathBuf,
        backup_path: PathBuf,
    },

    /// An explicit recovery invocation named a transaction directory
    /// whose durable record disagrees about its own transaction id --
    /// refusing recovery.
    #[error(
        "`{}` does not match the transaction id {record_id:?} recorded in `{}`",
        txn_dir.display(), record_path.display()
    )]
    TxnIdMismatch {
        txn_dir: PathBuf,
        record_path: PathBuf,
        record_id: String,
    },

    /// A durable reshape transaction record's candidate paths (backup/
    /// staging) point outside their expected transaction-local
    /// locations -- refusing recovery.
    #[error(
        "`{}` records candidate paths outside their expected transaction-local locations",
        record_path.display()
    )]
    CandidatePathsOutsideTxnDir { record_path: PathBuf },

    /// An explicit recovery invocation found the live path already
    /// present and valid -- refusing to overwrite it.
    #[error("`{}` is already present and valid; refusing to overwrite it", live_path.display())]
    LiveAlreadyValid { live_path: PathBuf },

    /// A recovery candidate (backup or staged path) doesn't have the
    /// shape recorded for it -- refusing recovery.
    #[error(
        "`{}` is {actual:?}, expected {expected:?}; refusing recovery",
        candidate_path.display()
    )]
    CandidateShapeMismatch {
        candidate_path: PathBuf,
        actual: Option<super::LockShardLevels>,
        expected: super::LockShardLevels,
    },

    /// A recovery candidate (backup or staged path) failed to validate
    /// before being restored/promoted.
    #[error("validating `{}` before recovery failed", candidate_path.display())]
    CandidateInvalid {
        candidate_path: PathBuf,
        #[source]
        source: Box<LockError>,
    },

    /// An explicit recovery invocation would need to displace the
    /// current malformed live path, but the displacement target already
    /// exists -- refusing.
    #[error("`{}` already exists; refusing to displace the current live path there", displaced_live.display())]
    DisplacedLiveAlreadyExists { displaced_live: PathBuf },

    /// Placeholder used only to unwind out of
    /// [`gat_core::lock::validated::visit_filtered_matching`]'s closure
    /// signature (which
    /// requires a [`LockError`]) when the caller's own visitor callback
    /// failed with a different, non-lock error type; the caller always
    /// recovers its real error from its own captured `Option` immediately
    /// afterwards and this variant is never itself surfaced to a user.
    #[error("visit callback failed")]
    CallbackFailed,
}

/// Bridges `gat_core::lock`'s own composite pure [`LockError`]
/// (`gat_core::lock::LockError`, returned by every function
/// `super`/`super::persistence` now calls into `gat-core` for --
/// `Lock::parse` and the operations under
/// `gat_core::lock::validated`, ...) into this module's own flat `LockError`
/// variants, so every existing `?`/`.map_err(...)` call site here that
/// mixes gat-core's pure functions with this module's I/O-owned ones
/// keeps compiling unchanged.
impl From<gat_core::lock::LockError> for LockError {
    fn from(err: gat_core::lock::LockError) -> Self {
        match err {
            gat_core::lock::LockError::Domain(domain) => Self::Domain(domain),
            gat_core::lock::LockError::LexicalPath(lexical) => Self::LexicalPath(lexical),
            gat_core::lock::LockError::CallbackFailed => {
                Self::Persistence(PersistenceError::CallbackFailed)
            }
        }
    }
}

impl LockError {
    /// Build an [`LockError::Io`] variant for a failed filesystem
    /// operation, mirroring [`crate::atomic::AtomicError`]'s own
    /// per-stage variants.
    pub fn io(
        operation: &'static str,
        path: impl AsRef<std::path::Path>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            operation,
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    /// Whether this failure is (transitively) a missing-file race --
    /// used by lock persistence's retry loop to
    /// distinguish "the live path vanished between our check and our
    /// read because a concurrent writer just replaced it" (retry once)
    /// from every other, terminal failure.
    #[must_use]
    pub fn is_not_found_race(&self) -> bool {
        matches!(self, Self::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`LockError::is_not_found_race`] must recognize a missing-file
    /// `Io` failure (the case the lock-load retry loop cares about) and
    /// reject every other variant/kind.
    #[test]
    fn is_not_found_race_detects_only_a_not_found_io_failure() {
        let not_found = LockError::io(
            "reading",
            PathBuf::from("gat.lock"),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        assert!(not_found.is_not_found_race());

        let permission_denied = LockError::io(
            "reading",
            PathBuf::from("gat.lock"),
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(!permission_denied.is_not_found_race());

        let empty = LockError::from(LockDomainError::Empty);
        assert!(!empty.is_not_found_race());
    }

    #[test]
    fn corrupt_shard_retains_its_typed_source_and_physical_path() {
        use std::error::Error as _;

        let path = PathBuf::from("gat.lock/ab.tsv");
        let err = LockError::CorruptShard {
            path: path.clone(),
            source: Box::new(LockDomainError::Empty),
        };
        let LockError::CorruptShard {
            path: error_path, ..
        } = &err
        else {
            unreachable!();
        };
        assert_eq!(error_path, &path);
        assert!(
            err.source()
                .and_then(|source| source.downcast_ref::<LockDomainError>())
                .is_some()
        );
    }
}
