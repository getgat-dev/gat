use super::persistence::OnDiskShape;
use super::reshape_record::{
    RecordValidationError, read_record, reshape_root, transaction_directories,
};
use super::{LockError, LockShardLevels, ReshapeRecoveryChoice, Result};
use std::path::Path;

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

pub(super) fn inspect_live(root: &Path) -> Result<LiveLockState> {
    let live_path = root.join("gat.lock");
    match std::fs::symlink_metadata(&live_path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LiveLockState::Missing);
        }
        Err(source) => return Err(LockError::io("reading", &live_path, source)),
    }
    Ok(match super::persistence::on_disk_shape_at(&live_path) {
        Ok(Some(shape)) => match super::persistence::load_shape_at(&live_path, shape) {
            Ok(lock) => LiveLockState::Valid {
                shard_levels: shape.shard_levels(),
                entries: lock.entries.len(),
            },
            Err(error) => LiveLockState::Invalid {
                reason: LiveLockInvalidReason::LoadFailed(error),
            },
        },
        Ok(None) => LiveLockState::Invalid {
            reason: LiveLockInvalidReason::NeitherFileNorShardTree,
        },
        Err(error) => LiveLockState::Invalid {
            reason: LiveLockInvalidReason::LoadFailed(error),
        },
    })
}

pub(super) fn inspect(root: &Path) -> Result<LockMaintenanceState> {
    let live = inspect_live(root)?;
    let mut transactions = Vec::new();
    let live_intact = matches!(live, LiveLockState::Valid { .. });
    for txn_dir in transaction_directories(root)? {
        let txn_dir = txn_dir?;
        let id = txn_dir
            .file_name()
            .expect("directory entry has a name")
            .to_string_lossy();
        transactions.push(inspect_transaction(&txn_dir, &id, live_intact)?);
    }
    transactions.sort_by(|a, b| a.id.cmp(&b.id));

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

fn inspect_transaction(
    txn_dir: &Path,
    id: &str,
    live_intact: bool,
) -> Result<ReshapeTransactionState> {
    let malformed = |reason| ReshapeTransactionState {
        id: id.to_string(),
        kind: ReshapeTransactionKind::Malformed { reason },
    };
    let record = match read_record(txn_dir) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return Ok(ReshapeTransactionState {
                id: id.to_string(),
                kind: ReshapeTransactionKind::ScratchOnly,
            });
        }
        Err(LockError::Persistence(super::PersistenceError::TxnRecordMalformed {
            source, ..
        })) => {
            return Ok(malformed(TransactionMalformedReason::RecordDecodeFailed(
                source,
            )));
        }
        Err(error) => return Err(error),
    };
    let record = match record.validate(txn_dir) {
        Ok(record) => record,
        Err(error) => {
            return Ok(malformed(match error {
                RecordValidationError::Phase(_) => TransactionMalformedReason::UnrecognizedPhase,
                RecordValidationError::Id(_) => TransactionMalformedReason::IdMismatch,
                RecordValidationError::CandidatePaths => {
                    TransactionMalformedReason::MetadataOutsideScratchLayout
                }
            }));
        }
    };

    let backup = inspect_candidate(
        record.backup_path(),
        record.source_shape(),
        ReshapeRecoveryChoice::RestoreBackup,
    );
    let staged = inspect_candidate(
        record.staging_path(),
        record.target_shape(),
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
