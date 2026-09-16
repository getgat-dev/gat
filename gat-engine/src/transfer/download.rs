use super::{cache_io_kind, diagnostic_remote};
use crate::ProgressUpdates;
use crate::operation::Operation;
use crate::path_policy::{EffectivePathPolicy, ResolvedRemote};
use crate::remote_catalog::RemoteCatalog;
use crate::remote_executor::RemoteJob;
use crate::remote_session::RemoteSessionError;
use gat_core::lexical_path::GatPath;
use gat_core::oid::Oid;
use gat_core::progress::ProgressActivity;
use gat_io::RemoteError;
use gat_io::{CacheError, CachePublication, ObjectVerification};
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
    Identity(crate::RemoteIdentityError),
    Cancelled,
    RemoteOpen {
        remote_name: Arc<str>,
        route: Option<super::TransferRoute>,
        path: GatPath,
        source: Box<RemoteSessionError>,
    },
    RemoteRead {
        kind: DownloadRemoteFailureKind,
        remote_name: Arc<str>,
        route: Option<super::TransferRoute>,
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

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity(source) => std::fmt::Display::fmt(source, f),
            Self::Cancelled => f.write_str("transfer cancelled"),
            Self::RemoteOpen {
                remote_name,
                route,
                path,
                ..
            } => {
                f.write_str("could not resolve/open ")?;
                super::write_remote_context(f, remote_name, route.as_ref())?;
                write!(f, " for `{path}`")
            }
            Self::RemoteRead {
                remote_name,
                route,
                path,
                ..
            } => {
                f.write_str("could not read from ")?;
                super::write_remote_context(f, remote_name, route.as_ref())?;
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
            Self::Identity(source) => Some(source),
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
    if cache_io_kind(source) == Some(std::io::ErrorKind::PermissionDenied) {
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
    let (remote_name, route) = diagnostic_remote(catalog, policy, &object.remote);
    DownloadError::RemoteOpen {
        remote_name,
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
            let (remote_name, route) = diagnostic_remote(catalog, policy, &object.remote);
            DownloadError::RemoteRead {
                kind,
                remote_name,
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
            let (remote_name, route) = diagnostic_remote(catalog, policy, &object.remote);
            DownloadError::RemoteRead {
                kind,
                remote_name,
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
    progress: &mut ProgressUpdates,
) -> Result<DownloadOutcome, DownloadError> {
    if objects.is_empty() {
        return Ok(DownloadOutcome::default());
    }
    for object in &objects {
        operation
            .remotes_catalog()
            .validate_id(object.remote.id())
            .and_then(|()| operation.policy().validate_remote(&object.remote))
            .map_err(DownloadError::Identity)?;
    }
    progress.set_activity(ProgressActivity::Working);
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

            let missing = status_window
                .iter()
                .filter(|status| **status != ObjectVerification::Valid)
                .count();
            let mut jobs = Vec::with_capacity(missing);
            for (index, status) in status_window.iter().enumerate() {
                if *status == ObjectVerification::Valid {
                    continue;
                }
                let object = &object_window[index];
                let handle = services
                    .remotes
                    .open_handle(
                        services.remotes_catalog,
                        object.remote.id(),
                        Some(progress.task()),
                    )
                    .map_err(|source| {
                        remote_open(services.remotes_catalog, services.policy, object, source)
                    })?;
                jobs.push(RemoteJob::new(handle, index));
            }
            progress.set_activity(ProgressActivity::Working);
            let mut observer =
                progress.observe(|_, result: &Result<_, WorkerError>| u64::from(result.is_ok()));
            let results = services.remote_executor.run_download_window(
                &jobs,
                |handle, index| {
                    let object = &object_window[*index];
                    let oid = object.oid;
                    let client = handle.client().clone();
                    let cache_writer = cache_writer.clone();
                    async move {
                        receive(services.remote_executor, client, cache_writer, oid).await
                    }
                },
                || WorkerError::Cancelled,
                &mut observer,
            );

            let mut publications = Vec::<CachePublication>::new();
            for (job, result) in jobs.iter().zip(results) {
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
                            &object_window[job.payload],
                            source,
                        )
                        .into());
                    }
                }
            }
            Ok(publications)
        },
    );

    progress.flush();
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
