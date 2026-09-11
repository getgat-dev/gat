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
    DesiredOperation, Operation, ReconciliationPolicy, Repository, SyncError as EngineSyncError,
    SyncOptions as EngineSyncOptions, SyncOutcome as EngineSyncOutcome, sync_from_snapshot,
};

/// Raw invocation preferences. Configuration precedence is resolved before any
/// fetch, repair, or reconciliation is dispatched. Preview suppresses transfers.
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

/// Report of a completed reconciliation attempt, including any unresolved paths.
/// Execution failures are returned separately as [`SyncError`].
#[derive(Debug)]
pub struct SyncOutcome {
    pub scope: super::SelectionScope,
    pub outcome: EngineSyncOutcome,
    pub fetched: usize,
    pub repaired: usize,
    pub repair_failures: Vec<super::RepairFailure>,
    pub reshaped: Option<LockShardLevels>,
    pub shallow: bool,
}

impl SyncOutcome {
    /// Whether reconciliation left no conflicts, missing objects, or corrupted objects.
    /// This describes filesystem facts independently of the invocation's exit policy.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.outcome.is_clean()
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

pub fn sync(
    repo: &Repository,
    request: SyncRequest,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    if request.dry_run {
        let mut operation = gat_engine::acquire_operation_without_desired_state(repo, progress)?;
        let request = resolve_request(request, operation.config())?;
        return sync_with_operation_impl(&mut operation, request, progress);
    }
    let mut desired = DesiredOperation::acquire(repo, progress)?;
    let mut request = resolve_request(request, desired.operation().config())?;
    if let ReconciliationMode::Execute {
        fetch: true,
        remote,
        ..
    } = &request.mode
    {
        request.fetched = super::fetch_with_desired_operation(
            &mut desired,
            super::FetchRequest {
                selection: Some(&request.selection),
                remote: remote.as_ref(),
                source: super::FetchSource::Current,
            },
            progress,
        )?
        .fetched;
    }
    sync_with_operation_impl(&mut desired.finish_selection(), request, progress)
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
    let policy = resolve_policy(operation.config(), false, false);
    let repair = operation.config().sync.auto_repair();
    sync_with_operation_impl(
        &mut operation,
        ReconciliationRequest {
            scope,
            selection: selection.into_owned(),
            force: false,
            policy,
            mode: ReconciliationMode::Execute {
                fetch: false,
                repair,
                remote: request.remote,
            },
            fetched: fetched.fetched,
            shallow: fetched.shallow,
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
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    let mut request = resolve_request(request, operation.config())?;
    request.fetched = fetched;
    request.shallow = shallow;
    sync_with_operation_impl(operation, request, progress)
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
    let policy = resolve_policy(operation.config(), false, false);
    let repair = operation.config().sync.auto_repair();
    sync_with_operation_impl(
        &mut operation,
        ReconciliationRequest {
            scope,
            selection: selection.into_owned(),
            force: false,
            policy,
            mode: ReconciliationMode::Execute {
                fetch: false,
                repair,
                remote: None,
            },
            fetched,
            shallow: false,
        },
        progress,
    )
}

fn resolve_policy(
    config: &gat_core::config::Config,
    trust_state: bool,
    rematerialize: bool,
) -> ReconciliationPolicy {
    if !rematerialize && (trust_state || config.sync.trust_state == Some(true)) {
        ReconciliationPolicy::TrustState
    } else {
        ReconciliationPolicy::Validate { rematerialize }
    }
}

/// Preview carries no active transfer or repair work, even when configuration
/// enables automatic maintenance for mutating invocations.
#[derive(Debug, PartialEq, Eq)]
enum ReconciliationMode {
    Preview,
    Execute {
        fetch: bool,
        repair: bool,
        remote: Option<RemoteName>,
    },
}

impl ReconciliationMode {
    const fn is_preview(&self) -> bool {
        matches!(self, Self::Preview)
    }
}

struct ReconciliationRequest {
    scope: super::SelectionScope,
    selection: Selection,
    force: bool,
    policy: ReconciliationPolicy,
    mode: ReconciliationMode,
    fetched: usize,
    shallow: bool,
}

fn resolve_request(
    request: SyncRequest,
    config: &gat_core::config::Config,
) -> Result<ReconciliationRequest, SyncError> {
    let ResolvedSelection { selection, scope } =
        selection::resolve(request.selection.as_ref(), config)?;
    Ok(ReconciliationRequest {
        scope,
        selection: selection.into_owned(),
        force: request.force,
        policy: resolve_policy(config, request.trust_state, request.rematerialize),
        mode: if request.dry_run {
            ReconciliationMode::Preview
        } else {
            ReconciliationMode::Execute {
                fetch: request.fetch || config.sync.auto_fetch(),
                repair: request.repair || config.sync.auto_repair(),
                remote: request.remote,
            }
        },
        fetched: 0,
        shallow: false,
    })
}

fn sync_with_operation_impl(
    operation: &mut Operation<'_>,
    request: ReconciliationRequest,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    operation.check_cancelled()?;
    let options = EngineSyncOptions {
        selection: request.selection,
        force: request.force,
        dry_run: request.mode.is_preview(),
        policy: request.policy,
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

    let repair_then_rematerialize = matches!(
        request.mode,
        ReconciliationMode::Execute { repair: true, .. }
    ) && options.policy.rematerialize();
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
    if let ReconciliationMode::Execute {
        repair: true,
        remote,
        ..
    } = &request.mode
        && !outcome.corrupted.is_empty()
    {
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
                        remote: remote.as_ref(),
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

    Ok(SyncOutcome {
        scope: request.scope,
        outcome,
        fetched: request.fetched,
        repaired,
        repair_failures,
        reshaped,
        shallow: request.shallow,
    })
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
    fn rematerialization_and_trust_preferences_resolve_to_one_policy() {
        for configured_trust in [None, Some(false), Some(true)] {
            for trust_state in [false, true] {
                for rematerialize in [false, true] {
                    let config = Config {
                        sync: SyncConfig {
                            trust_state: configured_trust,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let policy = resolve_policy(&config, trust_state, rematerialize);
                    if rematerialize {
                        assert_eq!(
                            policy,
                            ReconciliationPolicy::Validate {
                                rematerialize: true
                            }
                        );
                    } else if trust_state || configured_trust == Some(true) {
                        assert_eq!(policy, ReconciliationPolicy::TrustState);
                    } else {
                        assert_eq!(
                            policy,
                            ReconciliationPolicy::Validate {
                                rematerialize: false
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn preview_requests_discard_transfers_while_execution_resolves_config_defaults() {
        for dry_run in [false, true] {
            for auto_fetch in [false, true] {
                for auto_repair in [false, true] {
                    for fetch in [false, true] {
                        for repair in [false, true] {
                            let config = Config {
                                sync: SyncConfig {
                                    auto_fetch: Some(auto_fetch),
                                    auto_repair: Some(auto_repair),
                                    ..Default::default()
                                },
                                ..Default::default()
                            };
                            let remote = RemoteName::from_string("origin".to_string());
                            let resolved = resolve_request(
                                SyncRequest {
                                    selection: Some(Selection::root()),
                                    force: false,
                                    dry_run,
                                    trust_state: false,
                                    fetch,
                                    repair,
                                    remote: Some(remote.clone()),
                                    rematerialize: false,
                                },
                                &config,
                            )
                            .unwrap();
                            assert_eq!(
                                resolved.mode,
                                if dry_run {
                                    ReconciliationMode::Preview
                                } else {
                                    ReconciliationMode::Execute {
                                        fetch: fetch || auto_fetch,
                                        repair: repair || auto_repair,
                                        remote: Some(remote),
                                    }
                                }
                            );
                        }
                    }
                }
            }
        }
    }
}
