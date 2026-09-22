use super::{cache_io_kind, diagnostic_remote};
use crate::operation::Operation;
use crate::path_policy::{EffectivePathPolicy, ResolvedRemote};
use crate::remote_catalog::{RemoteCatalog, RemoteId};
use crate::remote_executor::RemoteJob;
use crate::remote_session::RemoteSessionError;
use gat_core::lexical_path::GatPath;
use gat_core::oid::Oid;
use gat_io::RemoteError;
use gat_io::{CacheError, CachePublication};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::sync::Arc;

/// One route-resolved object in an already-bounded repair window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairObject {
    oid: Oid,
    representative_path: GatPath,
    remote: ResolvedRemote,
    entries: u64,
}

impl RepairObject {
    #[must_use]
    pub const fn new(
        oid: Oid,
        representative_path: GatPath,
        remote: ResolvedRemote,
        entries: u64,
    ) -> Self {
        Self {
            oid,
            representative_path,
            remote,
            entries,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairRemoteFailureKind {
    PermissionDenied,
    NotFound,
    Unavailable,
    OperationFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairCacheFailureKind {
    PermissionDenied,
    Unavailable,
}

/// One best-effort repair attempt's terminal failure.
#[derive(Debug)]
pub enum RepairError {
    Identity(crate::RemoteIdentityError),
    Cancelled,
    RemoteOpen {
        remote_name: Arc<str>,
        route: Option<super::TransferRoute>,
        path: GatPath,
        source: Arc<RemoteSessionError>,
    },
    RemoteRead {
        kind: RepairRemoteFailureKind,
        remote_name: Arc<str>,
        route: Option<super::TransferRoute>,
        path: GatPath,
        source: Box<dyn Error + Send + Sync>,
    },
    Cache {
        kind: RepairCacheFailureKind,
        path: GatPath,
        source: super::TransferCacheSource,
    },
    HashMismatch {
        path: GatPath,
        expected: Oid,
        actual: Oid,
    },
    TaskFailed {
        path: GatPath,
        source: tokio::task::JoinError,
    },
}

impl std::fmt::Display for RepairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity(source) => std::fmt::Display::fmt(source, f),
            Self::Cancelled => f.write_str("repair cancelled"),
            Self::RemoteOpen { path, .. } => {
                write!(f, "could not open the remote to repair `{path}` from")
            }
            Self::RemoteRead { path, .. } => {
                write!(f, "could not read the replacement object for `{path}`")
            }
            Self::Cache { path, .. } => {
                write!(f, "could not cache the replacement object for `{path}`")
            }
            Self::HashMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "repaired object hash mismatch for `{path}`: expected {expected}, got {actual}"
            ),
            Self::TaskFailed { path, .. } => {
                write!(f, "the repair task for `{path}` did not complete normally")
            }
        }
    }
}

impl Error for RepairError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Identity(source) => Some(source),
            Self::Cancelled => None,
            Self::RemoteOpen { source, .. } => Some(source.as_ref()),
            Self::RemoteRead { source, .. } => Some(source.as_ref()),
            Self::Cache { source, .. } => Some(source),
            Self::HashMismatch { .. } => None,
            Self::TaskFailed { source, .. } => Some(source),
        }
    }
}

/// A terminal repair result paired with its object identity.
#[derive(Debug)]
pub struct RepairResult {
    pub oid: Oid,
    pub result: Result<(), RepairError>,
}

use super::receive::{ReceiveError as WorkerError, receive};

const fn remote_kind(source: &RemoteError) -> RepairRemoteFailureKind {
    match source {
        RemoteError::PermissionDenied { .. } => RepairRemoteFailureKind::PermissionDenied,
        RemoteError::NotFound { .. } => RepairRemoteFailureKind::NotFound,
        RemoteError::Unavailable { .. } | RemoteError::ReadinessTimedOut { .. } => {
            RepairRemoteFailureKind::Unavailable
        }
        RemoteError::MalformedUrl { .. }
        | RemoteError::UnsupportedScheme { .. }
        | RemoteError::InvalidFileRemotePath { .. }
        | RemoteError::DisallowedScheme
        | RemoteError::UnsupportedCapability { .. }
        | RemoteError::PayloadLimitExceeded { .. }
        | RemoteError::CleanupTimedOut
        | RemoteError::OperationFailed { .. } => RepairRemoteFailureKind::OperationFailed,
    }
}

fn cache_kind(source: &CacheError) -> RepairCacheFailureKind {
    if cache_io_kind(source) == Some(std::io::ErrorKind::PermissionDenied) {
        RepairCacheFailureKind::PermissionDenied
    } else {
        RepairCacheFailureKind::Unavailable
    }
}

fn worker_error(
    catalog: &RemoteCatalog,
    policy: &EffectivePathPolicy,
    object: &RepairObject,
    source: WorkerError,
) -> RepairError {
    match source {
        WorkerError::Cancelled => RepairError::Cancelled,
        WorkerError::Remote(source) => {
            let kind = remote_kind(&source);
            let (remote_name, route) = diagnostic_remote(catalog, policy, &object.remote);
            RepairError::RemoteRead {
                kind,
                remote_name,
                route,
                path: object.representative_path.clone(),
                source: Box::new(source),
            }
        }
        WorkerError::Cache(CacheError::SourceUnreadable { source }) => {
            let kind = match source.kind() {
                std::io::ErrorKind::PermissionDenied => RepairRemoteFailureKind::PermissionDenied,
                std::io::ErrorKind::NotFound => RepairRemoteFailureKind::NotFound,
                std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::UnexpectedEof => RepairRemoteFailureKind::Unavailable,
                _ => RepairRemoteFailureKind::OperationFailed,
            };
            let (remote_name, route) = diagnostic_remote(catalog, policy, &object.remote);
            RepairError::RemoteRead {
                kind,
                remote_name,
                route,
                path: object.representative_path.clone(),
                source: Box::new(CacheError::SourceUnreadable { source }),
            }
        }
        WorkerError::Cache(source) => RepairError::Cache {
            kind: cache_kind(&source),
            path: object.representative_path.clone(),
            source: super::TransferCacheSource::new(source),
        },
        WorkerError::HashMismatch { actual } => RepairError::HashMismatch {
            path: object.representative_path.clone(),
            expected: object.oid,
            actual,
        },
        WorkerError::Task(source) => RepairError::TaskFailed {
            path: object.representative_path.clone(),
            source,
        },
    }
}

/// Attempts every object in one bounded window and returns identified results.
/// Borrows the request buffer so callers can reuse its allocation between windows.
///
/// Failures are values rather than a fail-fast return so a bad object or
/// remote never prevents later repair candidates from being attempted.
#[allow(
    clippy::missing_panics_doc,
    reason = "The executor produces a terminal result for every admitted object"
)]
pub fn repair_window(
    operation: &mut Operation<'_>,
    objects: &[RepairObject],
    progress: &mut crate::ProgressUpdates,
) -> Vec<RepairResult> {
    if objects.is_empty() {
        return Vec::new();
    }
    debug_assert!(objects.len() <= operation.limits().transfer.window.get());

    #[cfg(any(test, feature = "test-support"))]
    test_support::record_window_size(objects.len());

    if let Some(error) = objects.iter().find_map(|object| {
        operation
            .remotes_catalog()
            .validate_id(object.remote.id())
            .and_then(|()| operation.policy().validate_remote(&object.remote))
            .err()
    }) {
        for object in objects {
            progress.inc(object.entries);
        }
        progress.flush();
        return objects
            .iter()
            .map(|object| RepairResult {
                oid: object.oid,
                result: Err(RepairError::Identity(error)),
            })
            .collect();
    }
    let services = operation.window_services();
    let cache_writer = services.cache_root.writer();
    let distinct_ids: BTreeSet<RemoteId> =
        objects.iter().map(|object| object.remote.id()).collect();
    let mut handles = BTreeMap::new();
    let mut unavailable = BTreeMap::new();
    for id in distinct_ids {
        match services
            .remotes
            .open_handle(services.remotes_catalog, id, Some(progress.task()))
        {
            Ok(handle) => {
                handles.insert(id, handle);
            }
            Err(source) => {
                unavailable.insert(id, Arc::new(source));
            }
        }
    }

    let mut results: Vec<Option<Result<(), RepairError>>> = std::iter::repeat_with(|| None)
        .take(objects.len())
        .collect();
    let mut jobs = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        if let Some(source) = unavailable.get(&object.remote.id()) {
            let (remote_name, route) =
                diagnostic_remote(services.remotes_catalog, services.policy, &object.remote);
            progress.inc(object.entries);
            results[index] = Some(Err(RepairError::RemoteOpen {
                remote_name,
                route,
                path: object.representative_path.clone(),
                source: Arc::clone(source),
            }));
        } else {
            jobs.push(RemoteJob::new(handles[&object.remote.id()].clone(), index));
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    test_support::record_attempts(jobs.len());

    progress.set_activity(gat_core::progress::ProgressActivity::Working);
    let worker_results = services.remote_executor.run_repair_window(
        &jobs,
        |handle, index| {
            let oid = objects[*index].oid;
            let client = handle.client().clone();
            let cache_writer = cache_writer.clone();
            async move { receive(services.remote_executor, client, cache_writer, oid).await }
        },
        || WorkerError::Cancelled,
        &mut progress
            .observe(|index, _: &Result<_, WorkerError>| objects[jobs[index].payload].entries),
    );

    let mut publications = Vec::<CachePublication>::new();
    for (job, result) in jobs.iter().zip(worker_results) {
        results[job.payload] = Some(match result {
            Ok(publication) => {
                publications.push(publication);
                Ok(())
            }
            Err(source) => Err(worker_error(
                services.remotes_catalog,
                services.policy,
                &objects[job.payload],
                source,
            )),
        });
    }
    if !publications.is_empty() {
        let _ = services
            .cache_session
            .apply_publications(services.cache_root, &publications);
    }

    progress.flush();
    objects
        .iter()
        .zip(results)
        .map(|(object, result)| RepairResult {
            oid: object.oid,
            result: result.expect("every repair object reaches a terminal result"),
        })
        .collect()
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static ATTEMPTS: Cell<usize> = const { Cell::new(0) };
        static WINDOW_HIGH_WATER: Cell<usize> = const { Cell::new(0) };
    }

    pub fn record_attempts(count: usize) {
        ATTEMPTS.with(|attempts| attempts.set(attempts.get() + count));
    }

    pub fn attempts() -> usize {
        ATTEMPTS.with(Cell::get)
    }

    pub fn record_window_size(size: usize) {
        WINDOW_HIGH_WATER.with(|high_water| high_water.set(high_water.get().max(size)));
    }

    pub fn window_high_water() -> usize {
        WINDOW_HIGH_WATER.with(Cell::get)
    }
}
