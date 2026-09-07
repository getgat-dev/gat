use crate::path_policy::{EffectivePathPolicy, ResolvedRemote};
use crate::remote_catalog::RemoteCatalog;
use crate::remote_executor::RemoteExecutor;
use crate::remote_executor::RemoteJob;
use crate::remote_session::{RemoteHandle, RemoteSessionError};
use gat_core::lexical_path::GatPath;
use gat_core::name::RouteName;
use gat_core::oid::Oid;
use gat_core::progress::{ProgressActivity, ProgressHandle};
use gat_io::RemoteError;
use gat_io::{CacheError, CacheObject, CacheObjectOpenError};
use std::error::Error;
use std::sync::Arc;

/// One route-resolved object in an already-bounded upload window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UploadObject {
    oid: Oid,
    representative_path: GatPath,
    remote: ResolvedRemote,
}

impl UploadObject {
    pub(crate) const fn new(
        oid: Oid,
        representative_path: GatPath,
        remote: ResolvedRemote,
    ) -> Self {
        Self {
            oid,
            representative_path,
            remote,
        }
    }
}

/// User-relevant classification of a local cache failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadCacheFailureKind {
    PermissionDenied,
    Missing,
    Unavailable,
}

/// User-relevant classification of a remote writer-open failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadRemoteFailureKind {
    PermissionDenied,
    Unavailable,
    OperationFailed,
}

/// User-relevant classification of a remote stream-write failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadWriteFailureKind {
    PermissionDenied,
    OperationFailed,
}

/// Everything one bounded upload window can fail with.
#[derive(Debug)]
pub enum UploadError {
    Cancelled,
    FileWrite {
        kind: UploadWriteFailureKind,
        published: bool,
        cancelled: bool,
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        source: Box<gat_io::FileWriteError>,
    },
    FileCleanup {
        primary: Box<Self>,
        cleanup: Box<gat_io::FileWriteError>,
    },
    Cleanup {
        primary: Box<Self>,
        cleanup: Box<RemoteError>,
    },
    CacheVerification {
        kind: UploadCacheFailureKind,
        path: GatPath,
        source: super::TransferCacheSource,
    },
    CacheOpen {
        kind: UploadCacheFailureKind,
        path: GatPath,
        source: CacheObjectOpenError,
    },
    CacheRead {
        kind: UploadCacheFailureKind,
        path: GatPath,
        source: std::io::Error,
    },
    RemoteOpen {
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        source: Box<RemoteSessionError>,
    },
    WriterOpen {
        kind: UploadRemoteFailureKind,
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        source: Box<RemoteError>,
    },
    WriterWrite {
        kind: UploadWriteFailureKind,
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        source: std::io::Error,
    },
    WriterFinalize {
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        source: std::io::Error,
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

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("transfer cancelled"),
            Self::Cleanup { primary, .. } | Self::FileCleanup { primary, .. } => primary.fmt(f),
            Self::FileWrite { .. } => f.write_str("file upload failed"),
            Self::CacheVerification { path, .. } => {
                write!(f, "could not verify the cached object for `{path}`")
            }
            Self::CacheOpen { path, .. } => {
                write!(f, "could not open the cached object for `{path}`")
            }
            Self::CacheRead { path, .. } => {
                write!(f, "could not read the cached object for `{path}`")
            }
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
            Self::WriterOpen {
                remote_name,
                route_name,
                route,
                path,
                ..
            } => {
                f.write_str("could not open an upload to ")?;
                write_remote_context(f, remote_name, route_name.as_ref(), route.as_ref())?;
                write!(f, " for `{path}`")
            }
            Self::WriterWrite {
                remote_name,
                route_name,
                route,
                path,
                ..
            } => {
                f.write_str("could not write the upload to ")?;
                write_remote_context(f, remote_name, route_name.as_ref(), route.as_ref())?;
                write!(f, " for `{path}`")
            }
            Self::WriterFinalize {
                remote_name,
                route_name,
                route,
                path,
                ..
            } => {
                f.write_str("could not finalize the upload to ")?;
                write_remote_context(f, remote_name, route_name.as_ref(), route.as_ref())?;
                write!(f, " for `{path}`")
            }
            Self::TaskFailed { path, .. } => {
                write!(f, "the upload task for `{path}` did not complete normally")
            }
        }
    }
}

impl Error for UploadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::Cleanup { primary, .. } | Self::FileCleanup { primary, .. } => {
                Some(primary.as_ref())
            }
            Self::FileWrite { source, .. } => Some(source.as_ref()),
            Self::CacheVerification { source, .. } => Some(source),
            Self::CacheOpen { source, .. } => Some(source),
            Self::CacheRead { source, .. }
            | Self::WriterWrite { source, .. }
            | Self::WriterFinalize { source, .. } => Some(source),
            Self::RemoteOpen { source, .. } => Some(source.as_ref()),
            Self::WriterOpen { source, .. } => Some(source.as_ref()),
            Self::TaskFailed { source, .. } => Some(source),
        }
    }
}

fn diagnostic_remote(
    catalog: &RemoteCatalog,
    policy: &EffectivePathPolicy,
    object: &UploadObject,
) -> (Arc<str>, Option<RouteName>, Option<GatPath>) {
    let remote_name = catalog.name(object.remote.id());
    let route = object.remote.route().map(|id| policy.route_descriptor(id));
    (
        remote_name,
        route.map(|descriptor| descriptor.name.clone()),
        route.map(|descriptor| descriptor.path.clone()),
    )
}

pub(crate) fn cache_error_kind(source: &CacheError) -> UploadCacheFailureKind {
    let io_kind = match source {
        CacheError::DirectoryUnavailable { source, .. }
        | CacheError::TempFileUnavailable { source, .. }
        | CacheError::PathUnreadable { source, .. }
        | CacheError::SourceUnreadable { source }
        | CacheError::EntryUnwritable { source, .. }
        | CacheError::EntryUnreadable { source, .. } => Some(source.kind()),
        CacheError::MaterializationFailed { .. }
        | CacheError::State(_)
        | CacheError::Oid(_)
        | CacheError::Atomic(_) => None,
    };
    match io_kind {
        Some(std::io::ErrorKind::PermissionDenied) => UploadCacheFailureKind::PermissionDenied,
        Some(std::io::ErrorKind::NotFound) => UploadCacheFailureKind::Missing,
        _ => UploadCacheFailureKind::Unavailable,
    }
}

fn io_cache_kind(source: &std::io::Error) -> UploadCacheFailureKind {
    match source.kind() {
        std::io::ErrorKind::PermissionDenied => UploadCacheFailureKind::PermissionDenied,
        std::io::ErrorKind::NotFound => UploadCacheFailureKind::Missing,
        _ => UploadCacheFailureKind::Unavailable,
    }
}

const fn remote_kind(source: &RemoteError) -> UploadRemoteFailureKind {
    match source {
        RemoteError::PermissionDenied { .. } => UploadRemoteFailureKind::PermissionDenied,
        RemoteError::Unavailable { .. }
        | RemoteError::ReadinessTimedOut { .. }
        | RemoteError::NotFound { .. } => UploadRemoteFailureKind::Unavailable,
        RemoteError::InvalidConnectTimeout
        | RemoteError::MalformedUrl { .. }
        | RemoteError::UnsupportedScheme { .. }
        | RemoteError::InvalidFileRemotePath { .. }
        | RemoteError::DisallowedScheme
        | RemoteError::UnsupportedCapability { .. }
        | RemoteError::PayloadLimitExceeded { .. }
        | RemoteError::CleanupTimedOut
        | RemoteError::OperationFailed { .. } => UploadRemoteFailureKind::OperationFailed,
    }
}

pub(crate) fn worker_error(
    catalog: &RemoteCatalog,
    policy: &EffectivePathPolicy,
    object: &UploadObject,
    source: WorkerError,
) -> UploadError {
    match source {
        WorkerError::Cancelled => UploadError::Cancelled,
        WorkerError::FileWrite(source) => {
            let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
            UploadError::FileWrite {
                kind: if source.source.kind() == std::io::ErrorKind::PermissionDenied {
                    UploadWriteFailureKind::PermissionDenied
                } else {
                    UploadWriteFailureKind::OperationFailed
                },
                published: source.publication != gat_io::FilePublication::NotPublished,
                cancelled: source.phase == gat_io::FileWritePhase::Cancelled,
                remote_name,
                route_name,
                route,
                path: object.representative_path.clone(),
                source: Box::new(source),
            }
        }
        WorkerError::FileCleanup { primary, cleanup } => UploadError::FileCleanup {
            primary: Box::new(worker_error(catalog, policy, object, *primary)),
            cleanup,
        },
        WorkerError::Cleanup { primary, cleanup } => UploadError::Cleanup {
            primary: Box::new(worker_error(catalog, policy, object, *primary)),
            cleanup,
        },
        WorkerError::CacheOpen(source) => UploadError::CacheOpen {
            kind: match source.io_kind() {
                std::io::ErrorKind::PermissionDenied => UploadCacheFailureKind::PermissionDenied,
                std::io::ErrorKind::NotFound => UploadCacheFailureKind::Missing,
                _ => UploadCacheFailureKind::Unavailable,
            },
            path: object.representative_path.clone(),
            source,
        },
        WorkerError::CacheRead(source) => UploadError::CacheRead {
            kind: io_cache_kind(&source),
            path: object.representative_path.clone(),
            source,
        },
        WorkerError::WriterOpen(source) => {
            let kind = remote_kind(&source);
            let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
            UploadError::WriterOpen {
                kind,
                remote_name,
                route_name,
                route,
                path: object.representative_path.clone(),
                source: Box::new(source),
            }
        }
        WorkerError::WriterWrite(source) => {
            let kind = if source.kind() == std::io::ErrorKind::PermissionDenied {
                UploadWriteFailureKind::PermissionDenied
            } else {
                UploadWriteFailureKind::OperationFailed
            };
            let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
            UploadError::WriterWrite {
                kind,
                remote_name,
                route_name,
                route,
                path: object.representative_path.clone(),
                source,
            }
        }
        WorkerError::WriterFinalize(source) => {
            let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, object);
            UploadError::WriterFinalize {
                remote_name,
                route_name,
                route,
                path: object.representative_path.clone(),
                source,
            }
        }
        WorkerError::Task(source) => UploadError::TaskFailed {
            path: object.representative_path.clone(),
            source,
        },
    }
}

pub(crate) enum WorkerError {
    Cancelled,
    FileWrite(gat_io::FileWriteError),
    FileCleanup {
        primary: Box<Self>,
        cleanup: Box<gat_io::FileWriteError>,
    },
    Cleanup {
        primary: Box<Self>,
        cleanup: Box<RemoteError>,
    },
    CacheOpen(CacheObjectOpenError),
    CacheRead(std::io::Error),
    WriterOpen(RemoteError),
    WriterWrite(std::io::Error),
    WriterFinalize(std::io::Error),
    Task(tokio::task::JoinError),
}

impl From<gat_io::FileUploadError> for WorkerError {
    fn from(error: gat_io::FileUploadError) -> Self {
        match error {
            gat_io::FileUploadError::Cancelled => Self::Cancelled,
            gat_io::FileUploadError::CacheOpen(source) => Self::CacheOpen(source),
            gat_io::FileUploadError::CacheRead { source, cleanup } => {
                let primary = Self::CacheRead(source);
                match cleanup {
                    Some(cleanup) => Self::FileCleanup {
                        primary: Box::new(primary),
                        cleanup: Box::new(cleanup),
                    },
                    None => primary,
                }
            }
            gat_io::FileUploadError::Write(source) => Self::FileWrite(source),
        }
    }
}

enum PreparedWrite {
    File(gat_io::PreparedFileWrite),
    Network(Box<gat_io::PreparedRemoteWrite>),
}

impl PreparedWrite {
    fn buffer_bytes(&self) -> usize {
        match self {
            Self::File(write) => write.buffer_bytes(),
            Self::Network(write) => write.buffer_bytes(),
        }
    }
}

impl From<crate::remote_executor::LocalTransferError> for WorkerError {
    fn from(error: crate::remote_executor::LocalTransferError) -> Self {
        match error {
            crate::remote_executor::LocalTransferError::Cancelled => Self::Cancelled,
            crate::remote_executor::LocalTransferError::Task(source) => Self::Task(source),
        }
    }
}

/// One fully prepared upload, containing only owned capabilities that can
/// cross the async and blocking-worker boundaries.
pub(crate) struct PreparedUpload {
    object: UploadObject,
    handle: RemoteHandle,
    source: CacheObject,
    result_index: usize,
    prepared: Result<PreparedWrite, RemoteError>,
}

impl PreparedUpload {
    pub(crate) fn buffer_bytes(&self) -> usize {
        self.prepared
            .as_ref()
            .map_or(0, PreparedWrite::buffer_bytes)
    }

    pub(crate) const fn index(&self) -> usize {
        self.result_index
    }

    pub(crate) fn new(
        object: UploadObject,
        handle: RemoteHandle,
        source: CacheObject,
        result_index: usize,
    ) -> Self {
        let size = source
            .verified_size()
            .expect("publication source is verified");
        let prepared = match handle.client().prepare_file_write(&object.oid, size) {
            Some(write) => Ok(PreparedWrite::File(write)),
            None => handle
                .client()
                .prepare_write(size, RemoteExecutor::transfer_buffer_limit())
                .map(|write| PreparedWrite::Network(Box::new(write))),
        };
        Self {
            object,
            handle,
            source,
            result_index,
            prepared,
        }
    }
}

pub(crate) struct ExecutedUpload {
    object: UploadObject,
    result_index: usize,
    result: Result<(), WorkerError>,
}

impl ExecutedUpload {
    pub(crate) fn into_parts(self) -> (UploadObject, usize, Result<(), WorkerError>) {
        (self.object, self.result_index, self.result)
    }
}

/// Executes one prepared upload under the operation-wide remote concurrency
/// budget. The executor permit covers the complete logical upload, including
/// cache open/read, remote writer creation, streaming, and finalization.
pub(crate) async fn execute_upload(
    executor: &RemoteExecutor,
    upload: PreparedUpload,
    task: ProgressHandle,
) -> ExecutedUpload {
    let PreparedUpload {
        object,
        handle,
        source,
        result_index,
        prepared,
    } = upload;
    let job = RemoteJob::new(handle, (object, source));
    let result = match prepared {
        Ok(prepared) => {
            upload_bytes(
                executor,
                job.handle.client().clone(),
                job.payload.1.clone(),
                job.payload.0.clone(),
                prepared,
                task,
            )
            .await
        }
        Err(error) => Err(WorkerError::WriterOpen(error)),
    };
    let RemoteJob {
        payload: (object, _),
        ..
    } = job;
    ExecutedUpload {
        object,
        result_index,
        result,
    }
}

async fn upload_bytes(
    executor: &RemoteExecutor,
    client: gat_io::RemoteClient,
    source: CacheObject,
    object: UploadObject,
    prepared: PreparedWrite,
    task: ProgressHandle,
) -> Result<(), WorkerError> {
    if executor.is_cancelled() {
        return Err(WorkerError::Cancelled);
    }
    task.set_activity(ProgressActivity::TransferringFile {
        path: object.representative_path,
    });
    let prepared = match prepared {
        PreparedWrite::File(write) => {
            let cancellation = executor.cancellation();
            return executor
                .local_transfer(move || {
                    write
                        .upload(source, || cancellation.is_cancelled())
                        .map(|_| ())
                        .map_err(WorkerError::from)
                })
                .await
                .map_err(WorkerError::from)?;
        }
        PreparedWrite::Network(write) => *write,
    };
    if prepared.whole_object() {
        let bytes = executor
            .local_transfer(move || {
                let mut reader = source.open().map_err(WorkerError::CacheOpen)?;
                reader
                    .read_small(gat_io::TRANSFER_CHUNK_SIZE)
                    .map_err(WorkerError::CacheRead)
            })
            .await
            .map_err(WorkerError::from)??;
        return executor
            .cancellable(client.write_object(&object.oid, bytes, prepared))
            .await
            .map_err(|()| WorkerError::Cancelled)?
            .map_err(WorkerError::WriterOpen);
    }
    let mut reader = executor
        .local_transfer(move || source.open())
        .await
        .map_err(WorkerError::from)?
        .map_err(WorkerError::CacheOpen)?;
    if executor.is_cancelled() {
        return Err(WorkerError::Cancelled);
    }
    let expected_size = reader.size();
    let mut writer = executor
        .cancellable(client.open_writer(&object.oid, prepared))
        .await
        .map_err(|()| WorkerError::Cancelled)?
        .map_err(WorkerError::WriterOpen)?;
    let result = async {
        let mut sent = 0u64;
        let (returned, first) = executor
            .local_transfer(move || {
                let bytes = reader.read_chunk(gat_io::TRANSFER_CHUNK_SIZE);
                (reader, bytes)
            })
            .await
            .map_err(WorkerError::from)?;
        reader = returned;
        let mut bytes = first.map_err(WorkerError::CacheRead)?;
        while !bytes.is_empty() {
            if executor.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            sent += bytes.len() as u64;
            if sent > expected_size {
                return Err(WorkerError::CacheRead(std::io::Error::other(
                    "cache source changed after verification",
                )));
            }
            let ((), (returned, next)) = RemoteExecutor::overlap_transfer(
                async {
                    executor
                        .cancellable(writer.write(bytes))
                        .await
                        .map_err(|()| WorkerError::Cancelled)?
                        .map_err(|error| WorkerError::WriterWrite(remote_io(error)))
                },
                async {
                    executor
                        .local_transfer(move || {
                            let bytes = reader
                                .read_chunk(gat_io::TRANSFER_CHUNK_SIZE)
                                .map_err(WorkerError::CacheRead)?;
                            Ok::<_, WorkerError>((reader, bytes))
                        })
                        .await
                        .map_err(WorkerError::from)?
                },
            )
            .await?;
            reader = returned;
            bytes = next;
        }
        if sent != expected_size {
            return Err(WorkerError::CacheRead(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "cache source changed after verification",
            )));
        }
        executor
            .cancellable(writer.close())
            .await
            .map_err(|()| WorkerError::Cancelled)?
            .map_err(|error| WorkerError::WriterFinalize(remote_io(error)))
    }
    .await;
    if let Err(primary) = result {
        // The coordinator drains this future; the writer remains owned until cleanup finishes.
        return match writer.abort().await {
            Ok(()) => Err(primary),
            Err(cleanup) => Err(WorkerError::Cleanup {
                primary: Box::new(primary),
                cleanup: Box::new(cleanup),
            }),
        };
    }
    Ok(())
}

fn remote_io(error: RemoteError) -> std::io::Error {
    let kind = match &error {
        RemoteError::PermissionDenied { .. } => std::io::ErrorKind::PermissionDenied,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Silent;
    impl gat_core::progress::ActivityBackend for Silent {
        fn inc(&self, _: u64) {}
        fn set_activity(&self, _: &ProgressActivity) {}
        fn finish(&self) {}
    }

    #[test]
    fn file_upload_uses_one_worker_and_a_bounded_reservation_at_every_size() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["origin"]);
        let cache_dir = tempfile::tempdir().unwrap();
        let cache_root = gat_io::RepositoryLayout::at(cache_dir.path().to_owned())
            .resolve_cache_root(None, None);
        let cache = cache_root.open_client();
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::default().remote);
        for size in [
            0,
            7,
            gat_io::TRANSFER_CHUNK_SIZE + 1,
            5 * gat_io::TRANSFER_CHUNK_SIZE + 17,
        ] {
            let (ingested, _) = cache_root
                .writer()
                .ingest(std::io::Cursor::new(vec![42; size]))
                .unwrap();
            cache.verify(&ingested.oid).unwrap();
            let object = UploadObject::new(
                ingested.oid,
                GatPath::parse_canonical("file.bin").unwrap(),
                ResolvedRemote::for_test(handles[0].id()),
            );
            let prepared =
                PreparedUpload::new(object, handles[0].clone(), cache.object(&ingested.oid), 0);
            assert_eq!(
                prepared.buffer_bytes(),
                (size + 1).min(gat_io::TRANSFER_CHUNK_SIZE)
            );
            assert!(matches!(&prepared.prepared, Ok(PreparedWrite::File(_))));
            let before = executor.local_submissions();
            let task = gat_core::progress::ProgressTask::from_backend(Arc::new(Silent));
            let lease = executor
                .try_transfer(handles[0].id(), prepared.buffer_bytes())
                .unwrap();
            let result = runtime.block_on(execute_upload(&executor, prepared, task.handle()));
            drop(lease);
            assert!(result.result.is_ok());
            assert_eq!(executor.local_submissions() - before, 1);
            assert!(
                handles[0]
                    .client()
                    .prepare_file_presence(&ingested.oid)
                    .unwrap()
                    .check()
                    .unwrap()
            );
        }
    }
}
