use crate::operation::Operation;
use crate::path_policy::{EffectivePathPolicy, ResolvedRemote};
use crate::remote_catalog::{RemoteCatalog, RemoteId};
use crate::remote_executor::RemoteJob;
use crate::remote_session::RemoteSessionError;
use gat_core::lexical_path::GatPath;
use gat_core::name::RouteName;
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
}

impl RepairObject {
    #[must_use]
    pub const fn new(oid: Oid, representative_path: GatPath, remote: ResolvedRemote) -> Self {
        Self {
            oid,
            representative_path,
            remote,
        }
    }

    #[must_use]
    pub const fn oid(&self) -> Oid {
        self.oid
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
    Cancelled,
    RemoteOpen {
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        source: Arc<RemoteSessionError>,
    },
    RemoteRead {
        kind: RepairRemoteFailureKind,
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
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
            Self::Cancelled => None,
            Self::RemoteOpen { source, .. } => Some(source.as_ref()),
            Self::RemoteRead { source, .. } => Some(source.as_ref()),
            Self::Cache { source, .. } => Some(source),
            Self::HashMismatch { .. } => None,
            Self::TaskFailed { source, .. } => Some(source),
        }
    }
}

/// Results aligned with the input repair objects.
#[derive(Debug, Default)]
pub struct RepairOutcome {
    pub results: Vec<Result<(), RepairError>>,
}

use super::receive::{ReceiveError as WorkerError, receive};

fn diagnostic_remote(
    catalog: &RemoteCatalog,
    policy: &EffectivePathPolicy,
    object: &RepairObject,
) -> (Arc<str>, Option<RouteName>, Option<GatPath>) {
    let remote_name = catalog.name(object.remote.id());
    let route = object.remote.route().map(|id| policy.route_descriptor(id));
    (
        remote_name,
        route.map(|descriptor| descriptor.name.clone()),
        route.map(|descriptor| descriptor.path.clone()),
    )
}

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
    let permission_denied = match source {
        CacheError::DirectoryUnavailable { source, .. }
        | CacheError::TempFileUnavailable { source, .. }
        | CacheError::PathUnreadable { source, .. }
        | CacheError::SourceUnreadable { source }
        | CacheError::EntryUnwritable { source, .. }
        | CacheError::EntryUnreadable { source, .. } => {
            source.kind() == std::io::ErrorKind::PermissionDenied
        }
        CacheError::MaterializationFailed { .. }
        | CacheError::State(_)
        | CacheError::Oid(_)
        | CacheError::Atomic(_) => false,
    };
    if permission_denied {
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
            let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
            RepairError::RemoteRead {
                kind,
                remote_name,
                route_name,
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
            let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
            RepairError::RemoteRead {
                kind,
                remote_name,
                route_name,
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

/// Attempts every object in one bounded window and returns aligned results.
///
/// Failures are values rather than a fail-fast return so a bad object or
/// remote never prevents later repair candidates from being attempted.
#[allow(
    clippy::missing_panics_doc,
    reason = "The executor produces a terminal result for every admitted object"
)]
pub fn repair_window(
    operation: &mut Operation<'_>,
    objects: Vec<RepairObject>,
    progress: &gat_core::progress::ProgressHandle,
) -> RepairOutcome {
    if objects.is_empty() {
        return RepairOutcome::default();
    }
    debug_assert!(objects.len() <= operation.limits().transfer.window.get());

    #[cfg(any(test, feature = "test-support"))]
    test_support::record_window_size(objects.len());

    let services = operation.window_services();
    let cache_writer = services.cache_root.writer();
    let distinct_ids: BTreeSet<RemoteId> =
        objects.iter().map(|object| object.remote.id()).collect();
    let mut handles = BTreeMap::new();
    let mut unavailable = BTreeMap::new();
    for id in distinct_ids {
        match services
            .remotes
            .open_handle(services.remotes_catalog, id, Some(progress))
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
            let (remote_name, route_name, route) =
                diagnostic_remote(services.remotes_catalog, services.policy, object);
            results[index] = Some(Err(RepairError::RemoteOpen {
                remote_name,
                route_name,
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

    let worker_results = services.remote_executor.run_repair_window(
        &jobs,
        |handle, index| {
            let object = objects[*index].clone();
            let client = handle.client().clone();
            let cache_writer = cache_writer.clone();
            async move { receive(services.remote_executor, client, cache_writer, object.oid).await }
        },
        || WorkerError::Cancelled,
    );

    let mut publications = Vec::<CachePublication>::new();
    for (job, result) in jobs.iter().zip(worker_results) {
        results[job.payload] = Some(match result {
            Ok(publication) => {
                if let Some(publication) = publication {
                    publications.push(publication);
                }
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

    RepairOutcome {
        results: results
            .into_iter()
            .map(|result| result.expect("every repair object reaches a terminal result"))
            .collect(),
    }
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
