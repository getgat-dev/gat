//! Shared decoding and validation of durable reshape transaction metadata.

use super::persistence::OnDiskShape;
use super::{LockError, PersistenceError, Result};
use std::path::{Path, PathBuf};

pub(super) const PREPARED_PHASE: &str = "prepared";

pub(super) fn reshape_root(root: &Path) -> PathBuf {
    root.join(".gat").join("lock-reshape")
}

/// Enumerate real transaction directories without following a symlinked
/// namespace. Child symlinks and unrelated files remain untouched.
pub(super) fn transaction_directories(
    root: &Path,
) -> Result<impl Iterator<Item = Result<PathBuf>>> {
    let namespace = reshape_root(root);
    let entries = crate::local_directory::read_directory_if_present(&namespace)
        .map_err(|source| LockError::io("reading", &namespace, source))?;
    Ok(entries.into_iter().flatten().filter_map(move |entry| {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => return Some(Err(LockError::io("reading", &namespace, source))),
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => Some(Ok(entry.path())),
            Ok(_) => None,
            Err(source) => Some(Err(LockError::io("reading", entry.path(), source))),
        }
    }))
}

/// Untrusted wire representation. Consumers must validate its identity and
/// candidate paths before using it to recover or discard a transaction.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct TxnRecord {
    pub id: String,
    pub source_shape: OnDiskShape,
    pub target_shape: OnDiskShape,
    pub staging_path: PathBuf,
    pub backup_path: PathBuf,
    pub phase: String,
}

pub(super) struct PreparedRecord(TxnRecord);

pub(super) enum RecordValidationError {
    Phase(String),
    Id(String),
    CandidatePaths,
}

impl RecordValidationError {
    pub fn into_lock_error(self, txn_dir: &Path) -> LockError {
        let record_path = txn_dir.join("txn.json");
        match self {
            Self::Phase(phase) => {
                PersistenceError::TxnRecordUnrecognizedPhase { record_path, phase }
            }
            Self::Id(record_id) => PersistenceError::TxnIdMismatch {
                txn_dir: txn_dir.to_path_buf(),
                record_path,
                record_id,
            },
            Self::CandidatePaths => PersistenceError::CandidatePathsOutsideTxnDir { record_path },
        }
        .into()
    }
}

impl TxnRecord {
    pub fn validate(
        self,
        txn_dir: &Path,
    ) -> std::result::Result<PreparedRecord, RecordValidationError> {
        if self.phase != PREPARED_PHASE {
            return Err(RecordValidationError::Phase(self.phase));
        }
        if txn_dir.file_name() != Some(std::ffi::OsStr::new(&self.id)) {
            return Err(RecordValidationError::Id(self.id));
        }
        if self.backup_path != txn_dir.join("backup") || self.staging_path != txn_dir.join("new") {
            return Err(RecordValidationError::CandidatePaths);
        }
        Ok(PreparedRecord(self))
    }
}

impl PreparedRecord {
    pub const fn source_shape(&self) -> OnDiskShape {
        self.0.source_shape
    }
    pub const fn target_shape(&self) -> OnDiskShape {
        self.0.target_shape
    }
    pub fn backup_path(&self) -> &Path {
        &self.0.backup_path
    }
    pub fn staging_path(&self) -> &Path {
        &self.0.staging_path
    }
}

/// Absence means pre-record scratch. A symlink or other non-file entry must
/// never be mistaken for absent scratch, including a dangling symlink.
pub(super) fn read_record(txn_dir: &Path) -> Result<Option<TxnRecord>> {
    let record_path = txn_dir.join("txn.json");
    match std::fs::symlink_metadata(&record_path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(LockError::UnsupportedOnDiskKind {
                path: record_path,
                detail: "not a regular transaction record".into(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(LockError::io("reading", &record_path, source)),
    }
    let text = std::fs::read_to_string(&record_path)
        .map_err(|source| LockError::io("reading", &record_path, source))?;
    serde_json::from_str(&text).map(Some).map_err(|source| {
        PersistenceError::TxnRecordMalformed {
            record_path,
            source,
        }
        .into()
    })
}
