//! Bounded best-effort repair orchestration.

use gat_core::lexical_path::GatPath;
use gat_core::name::RemoteName;
use gat_core::oid::Oid;
use gat_core::progress::ProgressHandle;
use gat_engine::{
    Operation, RepairError as EngineRepairError, RepairObject, RepairProgress, StreamingWindow,
    UnknownRemoteOverrideError, repair_window,
};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct RepairRequest<'a> {
    pub corrupted: &'a [(GatPath, Oid)],
    pub remote: Option<&'a RemoteName>,
}

#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    #[error(transparent)]
    UnknownRemoteOverride(#[from] UnknownRemoteOverrideError),
    #[error(transparent)]
    MissingRemoteConfig(#[from] super::MissingRemoteConfigError),
    #[error(transparent)]
    DataPlane(#[from] EngineRepairError),
}

#[derive(Debug)]
pub struct RepairFailure {
    pub path: GatPath,
    pub oid: Oid,
    pub error: Arc<RepairError>,
}

#[derive(Debug, Default)]
pub struct RepairOutcome {
    pub repaired: usize,
    pub failures: Vec<RepairFailure>,
}

struct RepairCandidate {
    oid: Oid,
    path: GatPath,
}

#[derive(Default)]
struct RepairResult {
    entries: u64,
    result: Option<Result<(), Arc<RepairError>>>,
}

/// Repairs corrupted objects through an existing desired-free operation.
///
/// Work and route resolution are deduplicated globally by OID using the first
/// path. Progress and final outcomes account for every original path entry.
#[allow(
    clippy::missing_panics_doc,
    reason = "Every deduplicated repair candidate resolves before final path association"
)]
pub fn repair_with_operation(
    operation: &mut Operation<'_>,
    request: RepairRequest<'_>,
    progress: &ProgressHandle,
) -> RepairOutcome {
    if request.corrupted.is_empty() {
        return RepairOutcome::default();
    }

    let mut window =
        StreamingWindow::<Oid, RepairCandidate>::new(operation.limits().transfer.window);
    let mut results = BTreeMap::<Oid, RepairResult>::new();
    let reporting_enabled = progress.is_enabled();
    if reporting_enabled {
        for (_, oid) in request.corrupted {
            results.entry(*oid).or_default().entries += 1;
        }
    }
    let mut reporting = RepairProgress::new(progress.clone());

    for (path, oid) in request.corrupted {
        let _: Result<(), std::convert::Infallible> = window.record(
            *oid,
            || RepairCandidate {
                oid: *oid,
                path: path.clone(),
            },
            |batch| {
                run_repair_window(
                    operation,
                    batch.drain(),
                    request.remote,
                    reporting_enabled,
                    &mut results,
                    &mut reporting,
                );
                Ok(())
            },
        );
    }
    let _: Result<(), std::convert::Infallible> = window.finish(|batch| {
        run_repair_window(
            operation,
            batch.drain(),
            request.remote,
            reporting_enabled,
            &mut results,
            &mut reporting,
        );
        Ok(())
    });

    reporting.flush();
    let mut outcome = RepairOutcome::default();
    for (path, oid) in request.corrupted {
        match results[oid]
            .result
            .as_ref()
            .expect("every repair candidate resolves")
        {
            Ok(()) => outcome.repaired += 1,
            Err(error) => outcome.failures.push(RepairFailure {
                path: path.clone(),
                oid: *oid,
                error: Arc::clone(error),
            }),
        }
    }
    outcome
}

fn run_repair_window(
    operation: &mut Operation<'_>,
    window: std::vec::Drain<'_, RepairCandidate>,
    remote_override: Option<&RemoteName>,
    reporting_enabled: bool,
    results: &mut BTreeMap<Oid, RepairResult>,
    progress: &mut RepairProgress,
) {
    let mut objects = Vec::with_capacity(window.len());
    for candidate in window {
        let entries = if reporting_enabled {
            results[&candidate.oid].entries
        } else {
            1
        };
        let remote = operation
            .policy()
            .resolved_remote_for_path(
                operation.remotes_catalog(),
                remote_override,
                &candidate.path,
            )
            .map_err(RepairError::from)
            .and_then(|resolved| {
                resolved.ok_or_else(|| {
                    RepairError::from(super::MissingRemoteConfigError {
                        path: candidate.path.clone(),
                    })
                })
            });
        match remote {
            Ok(remote) => objects.push(RepairObject::new(
                candidate.oid,
                candidate.path,
                remote,
                entries,
            )),
            Err(error) => {
                progress.rejected(entries);
                results.entry(candidate.oid).or_default().result = Some(Err(Arc::new(error)));
            }
        }
    }

    #[allow(
        clippy::needless_collect,
        reason = "Keep OIDs for result association before repair_window consumes the objects"
    )]
    let oids: Vec<Oid> = objects.iter().map(gat_engine::RepairObject::oid).collect();
    let outcome = repair_window(operation, objects, progress);
    for (oid, result) in oids.into_iter().zip(outcome.results) {
        results.entry(oid).or_default().result =
            Some(result.map_err(RepairError::from).map_err(Arc::new));
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub use gat_engine::test_support::{
        repair_attempts as repair_oid_calls, repair_window_high_water,
    };
}
