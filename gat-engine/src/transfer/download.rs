use crate::operation::Operation;
use crate::path_policy::{EffectivePathPolicy, ResolvedRemote};
use crate::remote_catalog::RemoteCatalog;
use crate::remote_executor::RemoteJob;
use crate::remote_session::RemoteSessionError;
use gat_core::lexical_path::GatPath;
use gat_core::name::RouteName;
use gat_core::oid::Oid;
use gat_core::progress::{ProgressActivity, ProgressHandle};
use gat_io::RemoteError;
use gat_io::{CacheError, CachePublication, ObjectVerification};
use std::collections::{BTreeMap, btree_map::Entry};
use std::error::Error;
use std::sync::Arc;

/// One route-resolved object in an already-bounded download window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadObject {
    oid: Oid,
    representative_path: GatPath,
    remote: ResolvedRemote,
}

impl DownloadObject {
    #[must_use]
    pub const fn new(oid: Oid, representative_path: GatPath, remote: ResolvedRemote) -> Self {
        Self {
            oid,
            representative_path,
            remote,
        }
    }
}

/// The number of objects actually downloaded by one bounded window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub downloaded: usize,
}

/// User-relevant classification of a remote object-read failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadRemoteFailureKind {
    PermissionDenied,
    NotFound,
    Unavailable,
    OperationFailed,
}

/// User-relevant classification of a local cache failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadCacheFailureKind {
    PermissionDenied,
    Unavailable,
}

/// Everything one bounded download window can fail with.
#[derive(Debug)]
pub enum DownloadError {
    Cancelled,
    RemoteOpen {
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        source: Box<RemoteSessionError>,
    },
    RemoteRead {
        kind: DownloadRemoteFailureKind,
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        source: Box<dyn Error + Send + Sync>,
    },
    Cache {
        kind: DownloadCacheFailureKind,
        path: GatPath,
        source: Box<dyn Error + Send + Sync>,
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

fn write_remote_context(
    f: &mut std::fmt::Formatter<'_>,
    remote_name: &str,
    route_name: Option<&RouteName>,
    route: Option<&GatPath>,
) -> std::fmt::Result {
    match (route_name, route) {
        (Some(route_name), Some(route)) => write!(
            f,
            "remote `{remote_name}` via route `{route_name}` (`{route}`)"
        ),
        (_, Some(route)) => write!(f, "remote `{remote_name}` via route `{route}`"),
        _ => write!(f, "remote `{remote_name}`"),
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("transfer cancelled"),
            Self::RemoteOpen {
                remote_name,
                route_name,
                route,
                path,
                ..
            } => {
                f.write_str("could not resolve/open ")?;
                write_remote_context(f, remote_name, route_name.as_ref(), route.as_ref())?;
                write!(f, " for `{path}`")
            }
            Self::RemoteRead {
                remote_name,
                route_name,
                route,
                path,
                ..
            } => {
                f.write_str("could not read from ")?;
                write_remote_context(f, remote_name, route_name.as_ref(), route.as_ref())?;
                write!(f, " selected by `{path}`")
            }
            Self::Cache { path, .. } => {
                write!(f, "could not store the downloaded object for `{path}`")
            }
            Self::HashMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "fetched object hash mismatch for `{path}`: expected {expected}, got {actual}"
            ),
            Self::TaskFailed { path, .. } => {
                write!(
                    f,
                    "the download task for `{path}` did not complete normally"
                )
            }
        }
    }
}

impl Error for DownloadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::RemoteOpen { source, .. } => Some(source.as_ref()),
            Self::RemoteRead { source, .. } | Self::Cache { source, .. } => Some(source.as_ref()),
            Self::HashMismatch { .. } => None,
            Self::TaskFailed { source, .. } => Some(source),
        }
    }
}

use super::receive::{ReceiveError as WorkerError, receive};

enum WindowError {
    Cache(CacheError),
    Download(DownloadError),
}

impl From<CacheError> for WindowError {
    fn from(source: CacheError) -> Self {
        Self::Cache(source)
    }
}

impl From<DownloadError> for WindowError {
    fn from(source: DownloadError) -> Self {
        Self::Download(source)
    }
}

fn diagnostic_remote(
    catalog: &RemoteCatalog,
    policy: &EffectivePathPolicy,
    object: &DownloadObject,
) -> (Arc<str>, Option<RouteName>, Option<GatPath>) {
    let remote_name = catalog.name(object.remote.id());
    let route = object.remote.route().map(|id| policy.route_descriptor(id));
    (
        remote_name,
        route.map(|descriptor| descriptor.name.clone()),
        route.map(|descriptor| descriptor.path.clone()),
    )
}

const fn remote_kind(source: &RemoteError) -> DownloadRemoteFailureKind {
    match source {
        RemoteError::PermissionDenied { .. } => DownloadRemoteFailureKind::PermissionDenied,
        RemoteError::NotFound { .. } => DownloadRemoteFailureKind::NotFound,
        RemoteError::Unavailable { .. } | RemoteError::ReadinessTimedOut { .. } => {
            DownloadRemoteFailureKind::Unavailable
        }
        RemoteError::MalformedUrl { .. }
        | RemoteError::UnsupportedScheme { .. }
        | RemoteError::InvalidFileRemotePath { .. }
        | RemoteError::DisallowedScheme
        | RemoteError::UnsupportedCapability { .. }
        | RemoteError::PayloadLimitExceeded { .. }
        | RemoteError::CleanupTimedOut
        | RemoteError::OperationFailed { .. } => DownloadRemoteFailureKind::OperationFailed,
    }
}

fn cache_kind(source: &CacheError) -> DownloadCacheFailureKind {
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
        DownloadCacheFailureKind::PermissionDenied
    } else {
        DownloadCacheFailureKind::Unavailable
    }
}

fn remote_open(
    catalog: &RemoteCatalog,
    policy: &EffectivePathPolicy,
    object: &DownloadObject,
    source: RemoteSessionError,
) -> DownloadError {
    let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
    DownloadError::RemoteOpen {
        remote_name,
        route_name,
        route,
        path: object.representative_path.clone(),
        source: Box::new(source),
    }
}

fn worker_error(
    catalog: &RemoteCatalog,
    policy: &EffectivePathPolicy,
    object: &DownloadObject,
    source: WorkerError,
) -> DownloadError {
    match source {
        WorkerError::Cancelled => DownloadError::Cancelled,
        WorkerError::Remote(source) => {
            let kind = remote_kind(&source);
            let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
            DownloadError::RemoteRead {
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
                std::io::ErrorKind::PermissionDenied => DownloadRemoteFailureKind::PermissionDenied,
                std::io::ErrorKind::NotFound => DownloadRemoteFailureKind::NotFound,
                std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::UnexpectedEof => DownloadRemoteFailureKind::Unavailable,
                _ => DownloadRemoteFailureKind::OperationFailed,
            };
            let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
            DownloadError::RemoteRead {
                kind,
                remote_name,
                route_name,
                route,
                path: object.representative_path.clone(),
                source: Box::new(CacheError::SourceUnreadable { source }),
            }
        }
        WorkerError::Cache(source) => DownloadError::Cache {
            kind: cache_kind(&source),
            path: object.representative_path.clone(),
            source: Box::new(source),
        },
        WorkerError::HashMismatch { actual } => DownloadError::HashMismatch {
            path: object.representative_path.clone(),
            expected: object.oid,
            actual,
        },
        WorkerError::Task(source) => DownloadError::TaskFailed {
            path: object.representative_path.clone(),
            source,
        },
    }
}

/// Downloads one already-bounded window through an operation's shared cache,
/// remote session, and remote executor.
pub fn download_window(
    operation: &mut Operation<'_>,
    objects: Vec<DownloadObject>,
    task: &ProgressHandle,
) -> Result<DownloadOutcome, DownloadError> {
    if objects.is_empty() {
        return Ok(DownloadOutcome::default());
    }
    let services = operation.window_services();
    let cache_writer = services.cache_root.writer();
    let window_oids: Vec<Oid> = objects.iter().map(|object| object.oid).collect();
    debug_assert!(window_oids.len() <= services.limits.transfer.window.get());

    let mut verified_offset = 0;
    let mut downloaded = 0;
    let result: Result<(), WindowError> = services.cache_session.verify_windows_unmemoized(
        services.cache_root,
        &window_oids,
        |verify_oids, status_window| {
            let object_window = &objects[verified_offset..verified_offset + verify_oids.len()];
            verified_offset += verify_oids.len();

            let job_indices: Vec<usize> = status_window
                .iter()
                .enumerate()
                .filter_map(|(index, status)| match status {
                    ObjectVerification::Valid => None,
                    ObjectVerification::Missing | ObjectVerification::Corrupt => Some(index),
                })
                .collect();

            let mut handles = BTreeMap::new();
            for &index in &job_indices {
                let object = &object_window[index];
                if let Entry::Vacant(entry) = handles.entry(object.remote.id()) {
                    let handle = services
                        .remotes
                        .open_handle(services.remotes_catalog, object.remote.id(), Some(task))
                        .map_err(|source| {
                            remote_open(services.remotes_catalog, services.policy, object, source)
                        })?;
                    entry.insert(handle);
                }
            }

            let jobs: Vec<RemoteJob<usize>> = job_indices
                .iter()
                .map(|&index| {
                    RemoteJob::new(handles[&object_window[index].remote.id()].clone(), index)
                })
                .collect();
            let results = services.remote_executor.run_download_window(
                &jobs,
                |handle, index| {
                    let object = object_window[*index].clone();
                    let client = handle.client().clone();
                    let cache_writer = cache_writer.clone();
                    let task = task.clone();
                    task.set_activity(ProgressActivity::TransferringFile {
                        path: object.representative_path.clone(),
                    });
                    async move {
                        let publication =
                            receive(services.remote_executor, client, cache_writer, object.oid)
                                .await?;
                        task.inc(1);
                        Ok(publication)
                    }
                },
                || WorkerError::Cancelled,
            );

            let mut publications = Vec::<CachePublication>::new();
            for (&index, result) in job_indices.iter().zip(results) {
                let Some(result) = result else { continue };
                match result {
                    Ok(publication) => {
                        downloaded += 1;
                        if let Some(publication) = publication {
                            publications.push(publication);
                        }
                    }
                    Err(source) => {
                        return Err(worker_error(
                            services.remotes_catalog,
                            services.policy,
                            &object_window[index],
                            source,
                        )
                        .into());
                    }
                }
            }
            Ok(publications)
        },
    );

    match result {
        Ok(()) => Ok(DownloadOutcome { downloaded }),
        Err(WindowError::Download(source)) => Err(source),
        Err(WindowError::Cache(source)) => Err(DownloadError::Cache {
            kind: cache_kind(&source),
            path: objects[0].representative_path.clone(),
            source: Box::new(source),
        }),
    }
}
