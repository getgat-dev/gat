//! Use-case orchestration for `gat system`.

use gat_core::lifecycle;
use gat_core::progress::{ProgressOperation, ProgressReporter, ProgressSpec, ProgressUnit};
use gat_engine::{LockRepairRequest, MaintenanceError, Repository};

pub use gat_engine::{
    CacheClean, CacheDbState, CacheInspect, CacheRepair, CandidateInvalidReason, CandidateOutcome,
    DbUnreadableReason, GitClean, GitInspect, GitRepair, LiveLockInvalidReason, LiveLockState,
    LockClean, LockMaintenanceState as LockState, LockRepair, PreparedTxnStatus, RecoveryChoice,
    RecoverySelectionFailure, StateClean, StateDbState, StateInspect, StateRepair,
    TemporaryCleanOutcome, TransactionKind, TransactionMalformedReason, TransactionState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemScope {
    Lock,
    State,
    Cache,
    Git,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemVerb {
    Inspect,
    Repair,
    Clean,
}

/// Raw maintenance input. Execution accepts only the private validated form,
/// constructed after lifecycle observation and before repository work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemRequest {
    Inspect {
        scope: SystemScope,
    },
    Repair {
        scope: SystemScope,
        transaction: Option<String>,
        recovery: Option<RecoveryChoice>,
    },
    Clean {
        scope: SystemScope,
        purge_temporary: bool,
        purge_objects: bool,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    #[error("a transaction id requires an explicit recovery choice")]
    TransactionChoiceRequired,
    #[error("an explicit recovery choice is only valid for lock repair")]
    RecoveryChoiceOnlyForLock,
    #[error("cache purge options are only valid for cache or all scope")]
    CachePurgeOnlyForCacheScope,
    #[error(transparent)]
    Maintenance(#[from] MaintenanceError),
}

#[derive(Debug)]
pub enum DomainFact {
    Lock(LockFact),
    State(StateFact),
    Cache(CacheFact),
    Git(GitFact),
}

#[derive(Debug)]
pub enum LockFact {
    Inspect(LockState),
    Repair(LockRepair),
    Clean(LockClean),
}

#[derive(Debug)]
pub enum StateFact {
    Inspect(StateInspect),
    Repair(StateRepair),
    Clean(StateClean),
}

#[derive(Debug)]
pub enum CacheFact {
    Inspect(CacheInspect),
    Repair(CacheRepair),
    Clean(CacheClean),
}

#[derive(Debug)]
pub enum GitFact {
    Inspect(GitInspect),
    Repair(GitRepair),
    Clean(GitClean),
}

#[derive(Debug)]
pub struct SystemOutcome {
    pub verb: SystemVerb,
    pub facts: Vec<DomainFact>,
}

#[derive(Clone, Copy)]
enum Domain {
    Lock,
    State,
    Cache,
    Git,
}

pub fn system(
    repo: &Repository,
    request: SystemRequest,
    progress: &dyn ProgressReporter,
) -> Result<SystemOutcome, SystemError> {
    system_with_lifecycle_observer(repo, request, progress, &|_| {})
}

pub fn system_with_lifecycle_observer(
    repo: &Repository,
    request: SystemRequest,
    progress: &dyn ProgressReporter,
    observe: &dyn Fn(lifecycle::Surface<'_>),
) -> Result<SystemOutcome, SystemError> {
    observe(lifecycle::Surface::Command("system"));
    match ValidatedSystemRequest::try_from(request)? {
        ValidatedSystemRequest::Inspect(scope) => inspect(repo, scope, progress),
        ValidatedSystemRequest::Repair(request) => repair(repo, request, progress),
        ValidatedSystemRequest::Clean(request) => clean(repo, request, progress),
    }
}

fn inspect(
    repo: &Repository,
    scope: SystemScope,
    progress: &dyn ProgressReporter,
) -> Result<SystemOutcome, SystemError> {
    let domains = resolve_domains(scope);
    let task = progress.begin(ProgressSpec::items(
        ProgressOperation::SystemInspect,
        ProgressUnit::Domains,
        Some(domains.len() as u64),
    ));
    let maintenance = repo.maintenance();
    let facts = run_domains(domains, |domain| {
        let fact = match domain {
            Domain::Lock => DomainFact::Lock(LockFact::Inspect(maintenance.inspect_lock()?)),
            Domain::State => DomainFact::State(StateFact::Inspect(maintenance.inspect_state()?)),
            Domain::Cache => DomainFact::Cache(CacheFact::Inspect(maintenance.inspect_cache()?)),
            Domain::Git => DomainFact::Git(GitFact::Inspect(maintenance.inspect_git()?)),
        };
        task.inc(1);
        Ok(fact)
    })?;
    Ok(SystemOutcome {
        verb: SystemVerb::Inspect,
        facts,
    })
}

fn repair(
    repo: &Repository,
    request: ValidatedRepair,
    progress: &dyn ProgressReporter,
) -> Result<SystemOutcome, SystemError> {
    let ValidatedRepair {
        scope,
        lock_request,
    } = request;
    let domains = resolve_domains(scope);
    let task = progress.begin(ProgressSpec::items(
        ProgressOperation::SystemRepair,
        ProgressUnit::Domains,
        Some(domains.len() as u64),
    ));
    let maintenance = repo.maintenance();
    if scope == SystemScope::All {
        let lock = maintenance.repair_lock(&lock_request)?;
        task.inc(1);
        if !lock.is_complete() {
            return Ok(SystemOutcome {
                verb: SystemVerb::Repair,
                facts: vec![DomainFact::Lock(LockFact::Repair(lock))],
            });
        }
        let mut facts = vec![DomainFact::Lock(LockFact::Repair(lock))];
        facts.push(DomainFact::State(StateFact::Repair(
            maintenance.repair_state()?,
        )));
        task.inc(1);
        facts.push(DomainFact::Cache(CacheFact::Repair(
            maintenance.repair_cache()?,
        )));
        task.inc(1);
        facts.push(DomainFact::Git(GitFact::Repair(maintenance.repair_git()?)));
        task.inc(1);
        return Ok(SystemOutcome {
            verb: SystemVerb::Repair,
            facts,
        });
    }
    let facts = run_domains(domains, |domain| {
        let fact = match domain {
            Domain::Lock => {
                DomainFact::Lock(LockFact::Repair(maintenance.repair_lock(&lock_request)?))
            }
            Domain::State => DomainFact::State(StateFact::Repair(maintenance.repair_state()?)),
            Domain::Cache => DomainFact::Cache(CacheFact::Repair(maintenance.repair_cache()?)),
            Domain::Git => DomainFact::Git(GitFact::Repair(maintenance.repair_git()?)),
        };
        task.inc(1);
        Ok(fact)
    })?;
    Ok(SystemOutcome {
        verb: SystemVerb::Repair,
        facts,
    })
}

fn clean(
    repo: &Repository,
    request: ValidatedClean,
    progress: &dyn ProgressReporter,
) -> Result<SystemOutcome, SystemError> {
    let ValidatedClean {
        scope,
        purge_temporary,
        purge_objects,
    } = request;
    let domains = resolve_domains(scope);
    let task = progress.begin(ProgressSpec::items(
        ProgressOperation::SystemClean,
        ProgressUnit::Domains,
        Some(domains.len() as u64),
    ));
    let maintenance = repo.maintenance();
    let facts = run_domains(domains, |domain| {
        let fact = match domain {
            Domain::Lock => DomainFact::Lock(LockFact::Clean(maintenance.clean_lock()?)),
            Domain::State => DomainFact::State(StateFact::Clean(maintenance.clean_state()?)),
            Domain::Cache => DomainFact::Cache(CacheFact::Clean(
                maintenance.clean_cache(purge_temporary, purge_objects)?,
            )),
            Domain::Git => DomainFact::Git(GitFact::Clean(maintenance.clean_git()?)),
        };
        task.inc(1);
        Ok(fact)
    })?;
    Ok(SystemOutcome {
        verb: SystemVerb::Clean,
        facts,
    })
}

// These private types are the executable boundary. Only this conversion creates
// them; maintenance dispatch never accepts raw dependent option combinations.
enum ValidatedSystemRequest {
    Inspect(SystemScope),
    Repair(ValidatedRepair),
    Clean(ValidatedClean),
}

struct ValidatedRepair {
    scope: SystemScope,
    lock_request: LockRepairRequest,
}

struct ValidatedClean {
    scope: SystemScope,
    purge_temporary: bool,
    purge_objects: bool,
}

impl TryFrom<SystemRequest> for ValidatedSystemRequest {
    type Error = SystemError;

    fn try_from(request: SystemRequest) -> Result<Self, Self::Error> {
        match request {
            SystemRequest::Inspect { scope } => Ok(Self::Inspect(scope)),
            SystemRequest::Repair {
                scope,
                transaction,
                recovery,
            } => {
                if transaction.is_some() && recovery.is_none() {
                    return Err(SystemError::TransactionChoiceRequired);
                }
                if recovery.is_some() && scope != SystemScope::Lock {
                    return Err(SystemError::RecoveryChoiceOnlyForLock);
                }
                Ok(Self::Repair(ValidatedRepair {
                    scope,
                    lock_request: LockRepairRequest {
                        choice: recovery,
                        transaction,
                    },
                }))
            }
            SystemRequest::Clean {
                scope,
                purge_temporary,
                purge_objects,
            } => {
                if (purge_temporary || purge_objects)
                    && !matches!(scope, SystemScope::Cache | SystemScope::All)
                {
                    return Err(SystemError::CachePurgeOnlyForCacheScope);
                }
                Ok(Self::Clean(ValidatedClean {
                    scope,
                    purge_temporary,
                    purge_objects,
                }))
            }
        }
    }
}

fn resolve_domains(scope: SystemScope) -> Vec<Domain> {
    match scope {
        SystemScope::Lock => vec![Domain::Lock],
        SystemScope::State => vec![Domain::State],
        SystemScope::Cache => vec![Domain::Cache],
        SystemScope::Git => vec![Domain::Git],
        SystemScope::All => vec![Domain::Lock, Domain::State, Domain::Cache, Domain::Git],
    }
}

fn run_domains(
    domains: Vec<Domain>,
    mut run: impl FnMut(Domain) -> Result<DomainFact, SystemError>,
) -> Result<Vec<DomainFact>, SystemError> {
    domains.into_iter().map(&mut run).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::progress::{
        ActivityBackend, ProgressActivity, ProgressOperation, ProgressReporter, ProgressSpec,
        ProgressTask,
    };
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct RecordingBackend {
        position: AtomicU64,
        finishes: AtomicU64,
    }

    impl ActivityBackend for RecordingBackend {
        fn inc(&self, delta: u64) {
            self.position.fetch_add(delta, Ordering::SeqCst);
        }

        fn set_activity(&self, _activity: &ProgressActivity) {}

        fn finish(&self) {
            self.finishes.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct RecordingProgress {
        specs: Mutex<Vec<(ProgressOperation, Option<u64>)>>,
        backend: Arc<RecordingBackend>,
    }

    impl RecordingProgress {
        fn only(&self, operation: ProgressOperation) -> (Option<u64>, u64, u64) {
            let specs = self.specs.lock().unwrap();
            let matching = specs
                .iter()
                .filter(|(candidate, _)| *candidate == operation)
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), 1);
            (
                matching[0].1,
                self.backend.position.load(Ordering::SeqCst),
                self.backend.finishes.load(Ordering::SeqCst),
            )
        }
    }

    impl ProgressReporter for RecordingProgress {
        fn begin(&self, spec: ProgressSpec) -> ProgressTask {
            self.specs
                .lock()
                .unwrap()
                .push((spec.operation(), spec.total()));
            ProgressTask::from_backend(Arc::clone(&self.backend) as Arc<dyn ActivityBackend>)
        }
    }

    #[test]
    fn repair_validation_preserves_scope_rules_and_error_precedence() {
        for scope in [
            SystemScope::Lock,
            SystemScope::State,
            SystemScope::Cache,
            SystemScope::Git,
            SystemScope::All,
        ] {
            for transaction in [None, Some("txn".to_string())] {
                for recovery in [
                    None,
                    Some(RecoveryChoice::RestoreBackup),
                    Some(RecoveryChoice::PromoteStaged),
                ] {
                    let result = ValidatedSystemRequest::try_from(SystemRequest::Repair {
                        scope,
                        transaction: transaction.clone(),
                        recovery,
                    });
                    if transaction.is_some() && recovery.is_none() {
                        assert!(matches!(
                            result,
                            Err(SystemError::TransactionChoiceRequired)
                        ));
                    } else if recovery.is_some() && scope != SystemScope::Lock {
                        assert!(matches!(
                            result,
                            Err(SystemError::RecoveryChoiceOnlyForLock)
                        ));
                    } else {
                        let Ok(ValidatedSystemRequest::Repair(request)) = result else {
                            panic!("valid repair request rejected")
                        };
                        assert_eq!(request.scope, scope);
                        assert_eq!(request.lock_request.choice, recovery);
                        assert_eq!(request.lock_request.transaction, transaction);
                    }
                }
            }
        }
    }

    #[test]
    fn clean_validation_keeps_purge_choices_only_for_cache_and_all() {
        for scope in [
            SystemScope::Lock,
            SystemScope::State,
            SystemScope::Cache,
            SystemScope::Git,
            SystemScope::All,
        ] {
            for purge_temporary in [false, true] {
                for purge_objects in [false, true] {
                    let result = ValidatedSystemRequest::try_from(SystemRequest::Clean {
                        scope,
                        purge_temporary,
                        purge_objects,
                    });
                    if (purge_temporary || purge_objects)
                        && !matches!(scope, SystemScope::Cache | SystemScope::All)
                    {
                        assert!(matches!(
                            result,
                            Err(SystemError::CachePurgeOnlyForCacheScope)
                        ));
                    } else {
                        let Ok(ValidatedSystemRequest::Clean(request)) = result else {
                            panic!("valid clean request rejected")
                        };
                        assert_eq!(request.scope, scope);
                        assert_eq!(request.purge_temporary, purge_temporary);
                        assert_eq!(request.purge_objects, purge_objects);
                    }
                }
            }
        }
    }

    #[test]
    fn all_scope_resolves_domains_in_fixed_order() {
        assert!(matches!(
            resolve_domains(SystemScope::All).as_slice(),
            [Domain::Lock, Domain::State, Domain::Cache, Domain::Git]
        ));
    }

    #[test]
    fn lifecycle_is_observed_before_validation_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let observed = std::cell::Cell::new(false);

        let error = system_with_lifecycle_observer(
            &repo,
            SystemRequest::Repair {
                scope: SystemScope::All,
                transaction: Some("txn".to_string()),
                recovery: None,
            },
            &gat_core::progress::NoopProgress,
            &|surface| {
                observed.set(matches!(surface, lifecycle::Surface::Command("system")));
            },
        )
        .unwrap_err();

        assert!(matches!(error, SystemError::TransactionChoiceRequired));
        assert!(observed.get());
    }

    #[test]
    fn cache_inspection_loads_config_once_without_registering_cache_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let before_loads = gat_engine::test_support::config_loads();
        let before_resolutions = gat_engine::test_support::cache_location_resolutions();

        system(
            &repo,
            SystemRequest::Inspect {
                scope: SystemScope::Cache,
            },
            &gat_core::progress::NoopProgress,
        )
        .unwrap();

        assert_eq!(gat_engine::test_support::config_loads() - before_loads, 1);
        assert_eq!(
            gat_engine::test_support::cache_location_resolutions() - before_resolutions,
            1
        );
    }

    #[test]
    fn all_scope_reports_the_exact_domain_total() {
        let tmp = tempfile::tempdir().unwrap();
        test_support_git::run_git(tmp.path(), &["init", "--quiet"]);
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let progress = RecordingProgress::default();

        system(
            &repo,
            SystemRequest::Inspect {
                scope: SystemScope::All,
            },
            &progress,
        )
        .unwrap();

        assert_eq!(
            progress.only(ProgressOperation::SystemInspect),
            (Some(4), 4, 1)
        );
    }

    #[test]
    fn one_domain_scope_reports_a_total_of_one() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let progress = RecordingProgress::default();

        system(
            &repo,
            SystemRequest::Inspect {
                scope: SystemScope::Cache,
            },
            &progress,
        )
        .unwrap();

        assert_eq!(
            progress.only(ProgressOperation::SystemInspect),
            (Some(1), 1, 1)
        );
    }

    #[test]
    fn inspect_stops_progress_before_the_first_failing_domain() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let progress = RecordingProgress::default();

        system(
            &repo,
            SystemRequest::Inspect {
                scope: SystemScope::All,
            },
            &progress,
        )
        .unwrap_err();

        assert_eq!(
            progress.only(ProgressOperation::SystemInspect),
            (Some(4), 3, 1)
        );
    }

    #[test]
    fn repair_stops_progress_before_the_first_failing_domain() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let progress = RecordingProgress::default();

        system(
            &repo,
            SystemRequest::Repair {
                scope: SystemScope::All,
                transaction: None,
                recovery: None,
            },
            &progress,
        )
        .unwrap_err();

        assert_eq!(
            progress.only(ProgressOperation::SystemRepair),
            (Some(4), 3, 1)
        );
    }
}
