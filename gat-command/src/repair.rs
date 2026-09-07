//! Bounded best-effort repair orchestration.

use gat_core::lexical_path::GatPath;
use gat_core::name::RemoteName;
use gat_core::oid::Oid;
use gat_core::progress::ProgressHandle;
use gat_engine::{
    Operation, RepairError as EngineRepairError, RepairObject, ResolvedRemote, StreamingWindow,
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
    remote: Result<ResolvedRemote, RepairError>,
}

/// Repairs corrupted objects through an existing desired-free operation.
///
/// Work is deduplicated globally by OID, while route resolution, progress,
/// and final reporting remain aligned with every original path entry.
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
    let mut results = BTreeMap::<Oid, Result<(), Arc<RepairError>>>::new();

    for (path, oid) in request.corrupted {
        let remote = operation
            .policy()
            .resolved_remote_for_path(operation.remotes_catalog(), request.remote, path)
            .map_err(RepairError::from)
            .and_then(|resolved| {
                resolved.ok_or_else(|| {
                    RepairError::from(super::MissingRemoteConfigError { path: path.clone() })
                })
            });
        let _: Result<(), std::convert::Infallible> = window.record(
            *oid,
            || RepairCandidate {
                oid: *oid,
                path: path.clone(),
                remote,
            },
            |batch| {
                run_repair_window(operation, batch.drain(), &mut results, progress);
                Ok(())
            },
        );
    }
    let _: Result<(), std::convert::Infallible> = window.finish(|batch| {
        run_repair_window(operation, batch.drain(), &mut results, progress);
        Ok(())
    });

    let mut outcome = RepairOutcome::default();
    for (path, oid) in request.corrupted {
        match &results[oid] {
            Ok(()) => outcome.repaired += 1,
            Err(error) => outcome.failures.push(RepairFailure {
                path: path.clone(),
                oid: *oid,
                error: Arc::clone(error),
            }),
        }
        progress.inc(1);
    }
    outcome
}

fn run_repair_window(
    operation: &mut Operation<'_>,
    window: std::vec::Drain<'_, RepairCandidate>,
    results: &mut BTreeMap<Oid, Result<(), Arc<RepairError>>>,
    progress: &ProgressHandle,
) {
    let mut objects = Vec::with_capacity(window.len());
    for candidate in window {
        match candidate.remote {
            Ok(remote) => objects.push(RepairObject::new(candidate.oid, candidate.path, remote)),
            Err(error) => {
                results.insert(candidate.oid, Err(Arc::new(error)));
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
        results.insert(oid, result.map_err(RepairError::from).map_err(Arc::new));
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub use gat_engine::test_support::{
        repair_attempts as repair_oid_calls, repair_window_high_water,
    };
}
