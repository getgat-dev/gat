use super::persistence::OnDiskShape;
use super::{LockError, LockShardLevels, LockStore, ReshapeRecoveryChoice, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

const PREPARED_PHASE: &str = "prepared";

#[derive(Debug)]
pub struct LockMaintenanceState {
    pub live: LiveLockState,
    pub transactions: Vec<ReshapeTransactionState>,
}

#[derive(Debug)]
pub enum LiveLockState {
    Missing,
    Valid {
        shard_levels: LockShardLevels,
        entries: usize,
    },
    Invalid {
        reason: LiveLockInvalidReason,
    },
}

#[derive(Debug)]
pub enum LiveLockInvalidReason {
    LoadFailed(LockError),
    NeitherFileNorShardTree,
}

#[derive(Debug)]
pub struct ReshapeTransactionState {
    pub id: String,
    pub kind: ReshapeTransactionKind,
}

#[derive(Debug)]
pub enum ReshapeTransactionKind {
    ScratchOnly,
    Malformed { reason: TransactionMalformedReason },
    Prepared(Box<PreparedReshapeState>),
}

#[derive(Debug)]
pub enum TransactionMalformedReason {
    RecordDecodeFailed(serde_json::Error),
    UnrecognizedPhase,
    IdMismatch,
    MetadataOutsideScratchLayout,
}

#[derive(Debug)]
pub struct PreparedReshapeState {
    pub backup: RecoveryCandidateState,
    pub staged: RecoveryCandidateState,
    pub status: PreparedReshapeStatus,
}

#[derive(Debug)]
pub struct RecoveryCandidateState {
    pub choice: ReshapeRecoveryChoice,
    pub outcome: RecoveryCandidateOutcome,
}

impl RecoveryCandidateState {
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        matches!(self.outcome, RecoveryCandidateOutcome::Valid { .. })
    }
}

#[derive(Debug)]
pub enum RecoveryCandidateOutcome {
    Valid {
        shard_levels: LockShardLevels,
        entries: usize,
    },
    Invalid(CandidateInvalidReason),
}

#[derive(Debug)]
pub enum CandidateInvalidReason {
    Missing,
    ShapeMismatch,
    LoadFailed(LockError),
    ShapeUnreadable(LockError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedReshapeStatus {
    CleanablePrepared,
    CompletedNotCleaned,
    RecoveryRequired,
    AmbiguousRecovery,
    CorruptRecoveryState,
}

#[derive(Deserialize)]
struct TxnRecord {
    id: String,
    source_shape: OnDiskShape,
    target_shape: OnDiskShape,
    staging_path: PathBuf,
    backup_path: PathBuf,
    phase: String,
}

pub(super) fn inspect(root: &Path) -> Result<LockMaintenanceState> {
    let live_path = root.join("gat.lock");
    let live_present = match std::fs::metadata(&live_path) {
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => return Err(LockError::io("reading", &live_path, source)),
    };
    let live = if live_present {
        match (
            super::persistence::on_disk_shape_at(&live_path),
            LockStore::load_all(root),
        ) {
            (Ok(Some(shape)), Ok(lock)) => LiveLockState::Valid {
                shard_levels: shape.shard_levels(),
                entries: lock.entries.len(),
            },
            (_, Err(err)) => LiveLockState::Invalid {
                reason: LiveLockInvalidReason::LoadFailed(err),
            },
            (Ok(None), Ok(_)) => LiveLockState::Invalid {
                reason: LiveLockInvalidReason::NeitherFileNorShardTree,
            },
            (Err(err), Ok(_)) => LiveLockState::Invalid {
                reason: LiveLockInvalidReason::LoadFailed(err),
            },
        }
    } else {
        LiveLockState::Missing
    };

    let reshape_root = reshape_root(root);
    let entries = match std::fs::read_dir(&reshape_root) {
        Ok(entries) => Some(entries),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(LockError::io("reading", &reshape_root, source)),
    };
    let mut transactions = Vec::new();
    if let Some(entries) = entries {
        let live_intact = matches!(live, LiveLockState::Valid { .. });
        for entry in entries {
            let entry = entry.map_err(|source| LockError::io("reading", &reshape_root, source))?;
            let file_type = entry
                .file_type()
                .map_err(|source| LockError::io("reading", entry.path(), source))?;
            if !file_type.is_dir() {
                continue;
            }
            let txn_dir = entry.path();
            let id = entry.file_name().to_string_lossy().into_owned();
            transactions.push(inspect_transaction(&txn_dir, &id, live_intact)?);
        }
        transactions.sort_by(|a, b| a.id.cmp(&b.id));
    }

    Ok(LockMaintenanceState { live, transactions })
}

pub(super) fn recover(
    root: &Path,
    transaction_id: &str,
    choice: ReshapeRecoveryChoice,
) -> Result<()> {
    super::persistence::recover_prepared_reshape(
        root,
        &reshape_root(root).join(transaction_id),
        choice,
    )
}

pub(super) fn clean(root: &Path) -> Result<usize> {
    super::persistence::clean_disposable_reshape_scratch(root)
}

fn reshape_root(root: &Path) -> PathBuf {
    root.join(".gat").join("lock-reshape")
}

fn inspect_transaction(
    txn_dir: &Path,
    id: &str,
    live_intact: bool,
) -> Result<ReshapeTransactionState> {
    let record_path = txn_dir.join("txn.json");
    let record_text = match std::fs::read_to_string(&record_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReshapeTransactionState {
                id: id.to_string(),
                kind: ReshapeTransactionKind::ScratchOnly,
            });
        }
        Err(source) => return Err(LockError::io("reading", &record_path, source)),
    };
    let record: TxnRecord = match serde_json::from_str(&record_text) {
        Ok(record) => record,
        Err(err) => {
            return Ok(ReshapeTransactionState {
                id: id.to_string(),
                kind: ReshapeTransactionKind::Malformed {
                    reason: TransactionMalformedReason::RecordDecodeFailed(err),
                },
            });
        }
    };
    if record.phase != PREPARED_PHASE {
        return Ok(ReshapeTransactionState {
            id: id.to_string(),
            kind: ReshapeTransactionKind::Malformed {
                reason: TransactionMalformedReason::UnrecognizedPhase,
            },
        });
    }
    if record.id != id {
        return Ok(ReshapeTransactionState {
            id: id.to_string(),
            kind: ReshapeTransactionKind::Malformed {
                reason: TransactionMalformedReason::IdMismatch,
            },
        });
    }
    if record.backup_path != txn_dir.join("backup") || record.staging_path != txn_dir.join("new") {
        return Ok(ReshapeTransactionState {
            id: id.to_string(),
            kind: ReshapeTransactionKind::Malformed {
                reason: TransactionMalformedReason::MetadataOutsideScratchLayout,
            },
        });
    }

    let backup = inspect_candidate(
        &record.backup_path,
        record.source_shape,
        ReshapeRecoveryChoice::RestoreBackup,
    );
    let staged = inspect_candidate(
        &record.staging_path,
        record.target_shape,
        ReshapeRecoveryChoice::PromoteStaged,
    );
    let status = classify_prepared(live_intact, &backup, &staged);
    Ok(ReshapeTransactionState {
        id: id.to_string(),
        kind: ReshapeTransactionKind::Prepared(Box::new(PreparedReshapeState {
            backup,
            staged,
            status,
        })),
    })
}

fn inspect_candidate(
    path: &Path,
    expected_shape: OnDiskShape,
    choice: ReshapeRecoveryChoice,
) -> RecoveryCandidateState {
    let outcome = match super::persistence::on_disk_shape_at(path) {
        Ok(None) => RecoveryCandidateOutcome::Invalid(CandidateInvalidReason::Missing),
        Ok(Some(found)) if found != expected_shape => {
            RecoveryCandidateOutcome::Invalid(CandidateInvalidReason::ShapeMismatch)
        }
        Ok(Some(_)) => match super::persistence::load_shape_at(path, expected_shape) {
            Ok(lock) => RecoveryCandidateOutcome::Valid {
                shard_levels: expected_shape.shard_levels(),
                entries: lock.entries.len(),
            },
            Err(err) => RecoveryCandidateOutcome::Invalid(CandidateInvalidReason::LoadFailed(err)),
        },
        Err(err) => RecoveryCandidateOutcome::Invalid(CandidateInvalidReason::ShapeUnreadable(err)),
    };
    RecoveryCandidateState { choice, outcome }
}

const fn classify_prepared(
    live_intact: bool,
    backup: &RecoveryCandidateState,
    staged: &RecoveryCandidateState,
) -> PreparedReshapeStatus {
    if live_intact {
        if backup.is_valid() {
            PreparedReshapeStatus::CompletedNotCleaned
        } else {
            PreparedReshapeStatus::CleanablePrepared
        }
    } else {
        match (backup.is_valid(), staged.is_valid()) {
            (true, true) => PreparedReshapeStatus::AmbiguousRecovery,
            (true, false) | (false, true) => PreparedReshapeStatus::RecoveryRequired,
            (false, false) => PreparedReshapeStatus::CorruptRecoveryState,
        }
    }
}
