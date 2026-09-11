//! Pull, sync, and Git-hook reconciliation orchestration.

use super::selection::{self, ResolvedSelection};
use gat_core::history::HistorySelection;
use gat_core::lock::LockShardLevels;
use gat_core::name::RemoteName;
use gat_core::progress::{
    ProgressActivity, ProgressOperation, ProgressReporter, ProgressSpec, ProgressUnit,
    with_progress_typed,
};
use gat_core::selection::Selection;
use gat_engine::{
    DesiredOperation, Operation, Repository, SyncError as EngineSyncError,
    SyncOptions as EngineSyncOptions, SyncOutcome as EngineSyncOutcome, Validation,
    sync_from_snapshot,
};
use std::fmt;

#[derive(Clone, Debug)]
pub struct SyncRequest {
    /// None uses configured defaults; Some replaces them completely.
    pub selection: Option<Selection>,
    pub force: bool,
    pub dry_run: bool,
    pub trust_state: bool,
    pub fetch: bool,
    pub repair: bool,
    pub remote: Option<RemoteName>,
    pub rematerialize: bool,
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    /// None uses configured defaults; Some replaces them completely.
    pub selection: Option<Selection>,
    pub remote: Option<RemoteName>,
    pub history: Option<HistorySelection>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HookRequest;

#[derive(Debug)]
pub struct SyncOutcome {
    pub scope: super::SelectionScope,
    pub outcome: EngineSyncOutcome,
    pub fetched: usize,
    pub repaired: usize,
    pub repair_failures: Vec<super::RepairFailure>,
    pub reshaped: Option<LockShardLevels>,
    pub shallow: bool,
    pub completion: SyncCompletionStatus,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SyncCompletionStatus {
    #[default]
    Clean,
    Incomplete {
        conflicts: usize,
        missing: usize,
        corrupted: usize,
    },
}

impl SyncCompletionStatus {
    const fn from_outcome(outcome: &EngineSyncOutcome, hook_mode: bool) -> Self {
        if hook_mode || outcome.is_clean() {
            Self::Clean
        } else {
            Self::Incomplete {
                conflicts: outcome.conflicts.len(),
                missing: outcome.missing.len(),
                corrupted: outcome.corrupted.len(),
            }
        }
    }

    #[must_use]
    pub const fn is_clean(self) -> bool {
        matches!(self, Self::Clean)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Acquisition(#[from] Box<gat_engine::RepoSnapshotError>),
    #[error(transparent)]
    Repository(Box<gat_engine::RepositoryError>),
    #[error(transparent)]
    Repo(Box<gat_engine::RepoError>),
    #[error(transparent)]
    DesiredRevision(#[from] gat_engine::DesiredRevisionError),
    #[error(transparent)]
    Reconciliation(#[from] EngineSyncError),
    #[error(transparent)]
    Fetch(#[from] Box<super::FetchError>),
    #[error(transparent)]
    Incomplete(#[from] Box<SyncIncompleteError>),
}

impl From<gat_engine::RepoSnapshotError> for SyncError {
    fn from(error: gat_engine::RepoSnapshotError) -> Self {
        Self::Acquisition(Box::new(error))
    }
}

impl From<gat_engine::RepositoryError> for SyncError {
    fn from(error: gat_engine::RepositoryError) -> Self {
        Self::Repository(Box::new(error))
    }
}

impl From<gat_engine::RepoError> for SyncError {
    fn from(error: gat_engine::RepoError) -> Self {
        Self::Repo(Box::new(error))
    }
}

impl From<super::FetchError> for SyncError {
    fn from(error: super::FetchError) -> Self {
        Self::Fetch(Box::new(error))
    }
}

#[derive(Debug)]
pub struct SyncIncompleteError {
    outcome: SyncOutcome,
}

impl SyncIncompleteError {
    const fn new(outcome: SyncOutcome) -> Self {
        Self { outcome }
    }

    #[must_use]
    pub fn into_outcome(self) -> SyncOutcome {
        self.outcome
    }
}

impl fmt::Display for SyncIncompleteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sync outcome left unreconciled paths behind")
    }
}

impl std::error::Error for SyncIncompleteError {}

pub fn recover_incomplete(
    result: Result<SyncOutcome, SyncError>,
) -> Result<SyncOutcome, SyncError> {
    match result {
        Ok(outcome) => Ok(outcome),
        Err(SyncError::Incomplete(incomplete)) => Ok(incomplete.into_outcome()),
        Err(error) => Err(error),
    }
}

pub fn sync(
    repo: &Repository,
    request: SyncRequest,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    let entry = SyncEntry::acquire(repo, request.dry_run, progress)?;
    let ResolvedSelection { selection, scope } =
        selection::resolve(request.selection.as_ref(), entry.config())?;
    let validation = resolve_validation(entry.config(), request.trust_state, request.rematerialize);
    let repair = !request.dry_run && (request.repair || entry.config().sync.auto_repair());
    let (fetched, mut operation) = entry.into_operation_after_fetch(
        request.fetch,
        &selection,
        request.remote.as_ref(),
        progress,
    )?;
    sync_with_operation_impl(
        &mut operation,
        ReconciliationRequest {
            scope,
            selection: selection.into_owned(),
            force: request.force,
            dry_run: request.dry_run,
            validation,
            repair,
            remote: request.remote.as_ref(),
            rematerialize: request.rematerialize,
            fetched,
            shallow: false,
            hook_mode: false,
        },
        progress,
    )
}

pub fn pull(
    repo: &Repository,
    request: PullRequest,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    let desired = DesiredOperation::acquire(repo, progress)?;
    pull_with_desired_operation(desired, request, progress)
}

#[doc(hidden)]
pub fn pull_with_desired_operation(
    mut desired: DesiredOperation<'_>,
    request: PullRequest,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    let ResolvedSelection { selection, scope } =
        selection::resolve(request.selection.as_ref(), desired.operation().config())?;
    let fetched = super::fetch_with_desired_operation(
        &mut desired,
        super::FetchRequest {
            selection: Some(&selection),
            remote: request.remote.as_ref(),
            source: super::FetchSource::CurrentAndHistory(request.history.as_ref()),
        },
        progress,
    )?;
    let mut operation = desired.finish_selection();
    let validation = resolve_validation(operation.config(), false, false);
    let repair = operation.config().sync.auto_repair();
    sync_with_operation_impl(
        &mut operation,
        ReconciliationRequest {
            scope,
            selection: selection.into_owned(),
            force: false,
            dry_run: false,
            validation,
            repair,
            remote: request.remote.as_ref(),
            rematerialize: false,
            fetched: fetched.fetched,
            shallow: fetched.shallow,
            hook_mode: false,
        },
        progress,
    )
}

#[doc(hidden)]
pub fn sync_with_operation(
    operation: &mut Operation<'_>,
    request: SyncRequest,
    fetched: usize,
    shallow: bool,
    hook_mode: bool,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    let ResolvedSelection { selection, scope } =
        selection::resolve(request.selection.as_ref(), operation.config())?;
    let validation = resolve_validation(
        operation.config(),
        request.trust_state,
        request.rematerialize,
    );
    let repair = !request.dry_run && (request.repair || operation.config().sync.auto_repair());
    sync_with_operation_impl(
        operation,
        ReconciliationRequest {
            scope,
            selection: selection.into_owned(),
            force: request.force,
            dry_run: request.dry_run,
            validation,
            repair,
            remote: request.remote.as_ref(),
            rematerialize: request.rematerialize,
            fetched,
            shallow,
            hook_mode,
        },
        progress,
    )
}

pub fn hook(
    repo: &Repository,
    _request: HookRequest,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    let mut desired = DesiredOperation::acquire(repo, progress)?;
    let ResolvedSelection { selection, scope } =
        selection::resolve(None, desired.operation().config())?;
    let fetched = if desired.operation().config().sync.auto_fetch() {
        super::fetch_with_desired_operation(
            &mut desired,
            super::FetchRequest {
                selection: Some(&selection),
                remote: None,
                source: super::FetchSource::Current,
            },
            progress,
        )
        .map_or(0, |outcome| outcome.fetched)
    } else {
        0
    };
    let mut operation = desired.finish_selection();
    let validation = resolve_validation(operation.config(), false, false);
    let repair = operation.config().sync.auto_repair();
    sync_with_operation_impl(
        &mut operation,
        ReconciliationRequest {
            scope,
            selection: selection.into_owned(),
            force: false,
            dry_run: false,
            validation,
            repair,
            remote: None,
            rematerialize: false,
            fetched,
            shallow: false,
            hook_mode: true,
        },
        progress,
    )
}

fn resolve_validation(
    config: &gat_core::config::Config,
    trust_state: bool,
    rematerialize: bool,
) -> Validation {
    if rematerialize {
        Validation::Validate
    } else if trust_state || config.sync.trust_state == Some(true) {
        Validation::TrustState
    } else {
        Validation::Validate
    }
}

enum SyncEntry<'repo> {
    Desired(DesiredOperation<'repo>),
    Bare(Operation<'repo>),
}

impl<'repo> SyncEntry<'repo> {
    fn acquire(
        repo: &'repo Repository,
        dry_run: bool,
        progress: &dyn ProgressReporter,
    ) -> Result<Self, SyncError> {
        if dry_run {
            Ok(Self::Bare(
                gat_engine::acquire_operation_without_desired_state(repo, progress)?,
            ))
        } else {
            Ok(Self::Desired(DesiredOperation::acquire(repo, progress)?))
        }
    }

    const fn config(&self) -> &gat_core::config::Config {
        match self {
            Self::Desired(desired) => desired.operation().config(),
            Self::Bare(operation) => operation.config(),
        }
    }

    fn into_operation_after_fetch(
        self,
        fetch: bool,
        selection: &Selection,
        remote: Option<&RemoteName>,
        progress: &dyn ProgressReporter,
    ) -> Result<(usize, Operation<'repo>), SyncError> {
        match self {
            Self::Bare(operation) => Ok((0, operation)),
            Self::Desired(mut desired) => {
                let fetched = if fetch || desired.operation().config().sync.auto_fetch() {
                    super::fetch_with_desired_operation(
                        &mut desired,
                        super::FetchRequest {
                            selection: Some(selection),
                            remote,
                            source: super::FetchSource::Current,
                        },
                        progress,
                    )?
                    .fetched
                } else {
                    0
                };
                Ok((fetched, desired.finish_selection()))
            }
        }
    }
}

struct ReconciliationRequest<'a> {
    scope: super::SelectionScope,
    selection: Selection,
    force: bool,
    dry_run: bool,
    validation: Validation,
    repair: bool,
    remote: Option<&'a RemoteName>,
    rematerialize: bool,
    fetched: usize,
    shallow: bool,
    hook_mode: bool,
}

fn sync_with_operation_impl(
    operation: &mut Operation<'_>,
    request: ReconciliationRequest<'_>,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    operation.check_cancelled()?;
    let options = EngineSyncOptions {
        selection: request.selection,
        force: request.force,
        dry_run: request.dry_run,
        validation: request.validation,
        rematerialize: request.rematerialize,
    };
    let sync_task = progress.begin(ProgressSpec::indeterminate(
        ProgressOperation::Synchronizing,
    ));
    let reshaped = if options.dry_run {
        None
    } else {
        operation.reshape_lock_if_needed(|| {
            sync_task
                .handle()
                .set_activity(ProgressActivity::ReshapingLock);
        })?
    };

    let repair_then_rematerialize = request.repair && options.rematerialize;
    let first_pass_options = if repair_then_rematerialize {
        EngineSyncOptions {
            dry_run: true,
            ..options.clone()
        }
    } else {
        options.clone()
    };
    let mut outcome =
        sync_from_snapshot(operation, &first_pass_options, Some(&sync_task.handle()))?;
    sync_task.finish();
    operation.check_cancelled()?;

    let mut repaired = 0;
    let mut repair_failures = Vec::new();
    if request.repair && !options.dry_run && !outcome.corrupted.is_empty() {
        with_progress_typed(
            progress,
            ProgressSpec::items(
                ProgressOperation::Repairing,
                ProgressUnit::Entries,
                Some(outcome.corrupted.len() as u64),
            ),
            |task| -> Result<(), SyncError> {
                let repair = super::repair_with_operation(
                    operation,
                    super::RepairRequest {
                        corrupted: &outcome.corrupted,
                        remote: request.remote,
                    },
                    &task.handle(),
                );
                repaired = repair.repaired;
                repair_failures = repair.failures;
                Ok(())
            },
        )?;
        if !repair_then_rematerialize {
            outcome = run_sync_pass(operation, &options, progress)?;
        }
    }
    if repair_then_rematerialize {
        outcome = run_sync_pass(operation, &options, progress)?;
    }

    let completion = SyncCompletionStatus::from_outcome(&outcome, request.hook_mode);
    let result = SyncOutcome {
        scope: request.scope,
        outcome,
        fetched: request.fetched,
        repaired,
        repair_failures,
        reshaped,
        shallow: request.shallow,
        completion,
    };
    if result.completion.is_clean() {
        Ok(result)
    } else {
        Err(Box::new(SyncIncompleteError::new(result)).into())
    }
}

fn run_sync_pass(
    operation: &mut Operation<'_>,
    options: &EngineSyncOptions,
    progress: &dyn ProgressReporter,
) -> Result<EngineSyncOutcome, SyncError> {
    with_progress_typed(
        progress,
        ProgressSpec::indeterminate(ProgressOperation::Synchronizing),
        |task| {
            Ok(sync_from_snapshot(
                operation,
                options,
                Some(&task.handle()),
            )?)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::config::{Config, SyncConfig};

    #[test]
    fn rematerialize_forces_validation_over_trust_state() {
        let config = Config {
            sync: SyncConfig {
                trust_state: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            resolve_validation(&config, false, true),
            Validation::Validate
        );
        assert_eq!(
            resolve_validation(&config, false, false),
            Validation::TrustState
        );
    }

    #[test]
    fn hook_completion_never_escalates_recoverable_path_conditions() {
        let outcome = EngineSyncOutcome {
            conflicts: vec![gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap()],
            ..Default::default()
        };
        assert_eq!(
            SyncCompletionStatus::from_outcome(&outcome, true),
            SyncCompletionStatus::Clean
        );
    }
}
