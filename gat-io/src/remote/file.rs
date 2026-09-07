//! Synchronous file publication capabilities. Call only from admitted local work.
//!
//! The private staging inode is linked into place without replacing any existing
//! name. This is not a cache-to-remote hard link: staging owns a separate copy.
//! Filesystems without hard-link support fail closed. Parent directories must be
//! trusted; checking a leaf does not prevent concurrent parent substitution.

use super::RemoteClient;
use gat_core::oid::Oid;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return std::fs::File::open(path)?.sync_all();
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn ensure_directory(path: &Path) -> io::Result<()> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "object parent",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing root"))?;
    ensure_directory(parent)?;
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
        Err(error) => return Err(error),
    }
    sync_directory(parent)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilePublication {
    NotPublished,
    Published,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileWritePhase {
    Cancelled,
    Stage,
    Copy,
    Sync,
    Publish,
    Cleanup,
    DirectorySync,
}

/// Physical failure, including whether the final name was already published.
/// Sources must be mapped structurally, never rendered as user-facing text.
#[derive(Debug, thiserror::Error)]
#[error("file object write failed")]
pub struct FileWriteError {
    pub phase: FileWritePhase,
    pub publication: FilePublication,
    #[source]
    pub source: io::Error,
    pub cleanup: Option<io::Error>,
}

/// No I/O or payload allocation occurs during preparation.
pub struct PreparedFileWrite {
    destination: PathBuf,
    size: u64,
}

pub struct PreparedFilePresence(PathBuf);

/// Includes the sole copy buffer and inline incremental hash/ingest storage.
/// Path/handle metadata and allocator overhead are not payload allocations.
const RECEIVE_BUFFER_BYTES: usize =
    super::TRANSFER_CHUNK_SIZE + std::mem::size_of::<crate::CacheIngest>();

pub struct PreparedFileRead {
    source: PathBuf,
    oid: Oid,
}

#[derive(Debug, thiserror::Error)]
pub enum FileReceiveError {
    #[error("file receive cancelled")]
    Cancelled,
    #[error("file receive source failed")]
    Remote(#[source] super::RemoteError),
    #[error("file receive cache failed")]
    Cache(#[source] crate::CacheError),
}

impl PreparedFileRead {
    /// Open once, then read/hash/append to EOF using one reusable buffer. Type
    /// checks do not discover transfer ranges or choose a tiny-object pipeline.
    /// Parent directories are trusted; path checks cannot prevent substitution
    /// races. The opened handle is also checked before reading any content.
    pub fn receive(
        self,
        cache: crate::CacheWriter,
        cancelled: impl Fn() -> bool + Sync,
    ) -> Result<crate::ExpectedIngest, FileReceiveError> {
        if cancelled() {
            return Err(FileReceiveError::Cancelled);
        }
        let remote_error = |source| Self::remote_error(source);
        #[cfg(test)]
        tests::record(|counts| counts.metadata += 1);
        let metadata = std::fs::symlink_metadata(&self.source).map_err(&remote_error)?;
        if !metadata.is_file() {
            return Err(remote_error(io::Error::new(
                io::ErrorKind::InvalidData,
                "object is not a regular file",
            )));
        }
        if cancelled() {
            return Err(FileReceiveError::Cancelled);
        }
        #[cfg(test)]
        tests::record(|counts| counts.source_opens += 1);
        let source = std::fs::File::open(&self.source).map_err(&remote_error)?;
        #[cfg(test)]
        tests::record(|counts| counts.metadata += 1);
        let metadata = source.metadata().map_err(&remote_error)?;
        if !metadata.is_file() {
            return Err(remote_error(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened object is not a regular file",
            )));
        }
        if cancelled() {
            return Err(FileReceiveError::Cancelled);
        }
        let buffer_bytes = usize::try_from(metadata.len())
            .unwrap_or(usize::MAX)
            .clamp(1, super::TRANSFER_CHUNK_SIZE);
        self.receive_reader(source, cache, cancelled, buffer_bytes)
    }

    fn remote_error(source: io::Error) -> FileReceiveError {
        let kind = match source.kind() {
            io::ErrorKind::NotFound => opendal::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => opendal::ErrorKind::PermissionDenied,
            _ => opendal::ErrorKind::Unexpected,
        };
        FileReceiveError::Remote(super::classify_opendal_error(
            opendal::Error::new(kind, "file object read failed").set_source(source),
        ))
    }

    fn receive_reader(
        self,
        mut source: impl Read,
        cache: crate::CacheWriter,
        cancelled: impl Fn() -> bool,
        buffer_bytes: usize,
    ) -> Result<crate::ExpectedIngest, FileReceiveError> {
        let mut ingest = cache.begin_ingest().map_err(FileReceiveError::Cache)?;
        let mut buffer = vec![0; buffer_bytes];
        #[cfg(test)]
        tests::record(|counts| {
            counts.buffers += 1;
            counts.buffer_capacity += buffer.capacity();
        });
        loop {
            if cancelled() {
                return Err(FileReceiveError::Cancelled);
            }
            let count = match source.read(&mut buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result.map_err(Self::remote_error)?,
            };
            if count == 0 {
                break;
            }
            ingest
                .append(&buffer[..count])
                .map_err(FileReceiveError::Cache)?;
        }
        drop(source);
        drop(buffer);
        ingest
            .finish_unless_cancelled(self.oid, cancelled)
            .map_err(FileReceiveError::Cache)?
            .ok_or(FileReceiveError::Cancelled)
    }
}

impl PreparedFilePresence {
    /// Metadata only: no content opens, timestamps, or extended attributes.
    /// A symlink is not an object, including a dangling symlink.
    pub fn check(self) -> io::Result<bool> {
        self.check_with(|path| std::fs::symlink_metadata(path))
    }

    fn check_with(
        self,
        metadata: impl FnOnce(&Path) -> io::Result<std::fs::Metadata>,
    ) -> io::Result<bool> {
        #[cfg(test)]
        tests::record(|counts| counts.metadata += 1);
        match metadata(&self.0) {
            Ok(metadata) if metadata.is_file() => Ok(true),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "object is not a regular file",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FileUploadError {
    #[error("file upload cancelled")]
    Cancelled,
    #[error("cache source open failed")]
    CacheOpen(#[source] crate::CacheObjectOpenError),
    #[error("cache source read failed")]
    CacheRead {
        #[source]
        source: io::Error,
        cleanup: Option<FileWriteError>,
    },
    #[error("file publication failed")]
    Write(#[source] FileWriteError),
}

impl RemoteClient {
    /// Physical presence grouping, without changing logical per-object admission.
    #[must_use]
    pub fn presence_batch_limit(&self) -> usize {
        if self.operator.info().scheme() == "fs" {
            128
        } else {
            1
        }
    }

    #[must_use]
    pub fn download_buffer_bytes(&self) -> usize {
        if self.operator.info().scheme() == "fs" {
            RECEIVE_BUFFER_BYTES
        } else {
            super::DOWNLOAD_BUFFER_BYTES
        }
    }

    /// Pure preparation; physical work starts only after local admission.
    #[must_use]
    pub fn prepare_file_read(&self, oid: Oid) -> Option<PreparedFileRead> {
        let info = self.operator.info();
        (info.scheme() == "fs").then(|| PreparedFileRead {
            source: PathBuf::from(info.root()).join(crate::cache::object_key_oid(&oid)),
            oid,
        })
    }

    #[must_use]
    pub fn prepare_file_presence(&self, oid: &Oid) -> Option<PreparedFilePresence> {
        let info = self.operator.info();
        (info.scheme() == "fs").then(|| {
            PreparedFilePresence(PathBuf::from(info.root()).join(crate::cache::object_key_oid(oid)))
        })
    }

    /// Select the filesystem capability without exposing its resolved root.
    /// The operator's canonical root preserves query-root and platform handling.
    #[must_use]
    pub fn prepare_file_write(&self, oid: &Oid, size: u64) -> Option<PreparedFileWrite> {
        let info = self.operator.info();
        (info.scheme() == "fs").then(|| PreparedFileWrite {
            destination: PathBuf::from(info.root()).join(crate::cache::object_key_oid(oid)),
            size,
        })
    }
}

impl PreparedFileWrite {
    /// One reusable buffer, including a byte for detecting growth of tiny files.
    #[must_use]
    pub fn buffer_bytes(&self) -> usize {
        usize::try_from(self.size.saturating_add(1))
            .unwrap_or(usize::MAX)
            .min(super::TRANSFER_CHUNK_SIZE)
    }

    /// One admitted worker owns the source, staging, copy buffer and cleanup.
    /// No nested dispatch, whole-file buffering, or network work occurs here.
    pub fn upload(
        self,
        source: crate::CacheObject,
        cancelled: impl Fn() -> bool + Sync,
    ) -> Result<FilePublication, FileUploadError> {
        if cancelled() {
            return Err(FileUploadError::Cancelled);
        }
        #[cfg(test)]
        tests::record(|counts| counts.source_opens += 1);
        let mut source = source.open().map_err(FileUploadError::CacheOpen)?;
        if cancelled() {
            return Err(FileUploadError::Cancelled);
        }
        let buffer_bytes = self.buffer_bytes();
        let mut writer = self.begin().map_err(FileUploadError::Write)?;
        let mut buffer = vec![0; buffer_bytes];
        #[cfg(test)]
        tests::record(|counts| {
            counts.buffers += 1;
            counts.buffer_capacity += buffer.capacity();
        });
        loop {
            if cancelled() {
                return Err(FileUploadError::Write(writer.fail(
                    FileWritePhase::Cancelled,
                    io::Error::from(io::ErrorKind::Interrupted),
                )));
            }
            let count = match source.read_into(&mut buffer) {
                Ok(count) => count,
                Err(source) => {
                    return Err(FileUploadError::CacheRead {
                        source,
                        cleanup: writer.abort().err(),
                    });
                }
            };
            if count == 0 {
                break;
            }
            if count as u64 > writer.remaining {
                return Err(FileUploadError::CacheRead {
                    source: io::Error::new(io::ErrorKind::InvalidData, "cache source grew"),
                    cleanup: writer.abort().err(),
                });
            }
            if let Err(source) = writer.append(&buffer[..count]) {
                return Err(FileUploadError::Write(
                    writer.fail(FileWritePhase::Copy, source),
                ));
            }
        }
        drop(source);
        if writer.remaining != 0 {
            return Err(FileUploadError::CacheRead {
                source: io::Error::new(io::ErrorKind::UnexpectedEof, "cache source shrank"),
                cleanup: writer.abort().err(),
            });
        }
        writer
            .finish_checked(cancelled)
            .map_err(FileUploadError::Write)
    }

    /// Start after local admission. Staging is beside the final name, including
    /// when the object directory is itself a mount point.
    #[allow(
        clippy::missing_panics_doc,
        reason = "Object destinations are constructed with a parent directory"
    )]
    pub fn begin(self) -> Result<FileObjectWriter, FileWriteError> {
        let parent = self.destination.parent().expect("object key has a parent");
        let stage = || {
            ensure_directory(parent)?;
            #[cfg(test)]
            tests::record(|counts| counts.staging_opens += 1);
            tempfile::Builder::new()
                .prefix(".gat-upload-")
                .tempfile_in(parent)
        };
        let temporary = stage().map_err(|source| FileWriteError {
            phase: FileWritePhase::Stage,
            publication: FilePublication::NotPublished,
            source,
            cleanup: None,
        })?;
        Ok(FileObjectWriter {
            temporary,
            destination: self.destination,
            remaining: self.size,
            failed: false,
        })
    }
}

/// Owns a single stable destination handle. Explicit abort reports cleanup
/// errors; Drop is a best-effort fallback, not the normal cancellation path.
pub struct FileObjectWriter {
    temporary: tempfile::NamedTempFile,
    destination: PathBuf,
    remaining: u64,
    failed: bool,
}

impl FileObjectWriter {
    /// Source identity verification remains the caller's responsibility.
    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.failed || bytes.len() as u64 > self.remaining {
            self.failed = true;
            return Err(io::Error::new(io::ErrorKind::InvalidData, "source grew"));
        }
        if let Err(error) = self.temporary.write_all(bytes) {
            self.failed = true;
            return Err(error);
        }
        self.remaining -= bytes.len() as u64;
        Ok(())
    }

    /// Preserve a primary failure while explicitly disposing of staging.
    #[must_use]
    pub fn fail(self, phase: FileWritePhase, source: io::Error) -> FileWriteError {
        FileWriteError {
            phase,
            publication: FilePublication::NotPublished,
            source,
            cleanup: self.temporary.close().err(),
        }
    }

    pub fn abort(self) -> Result<(), FileWriteError> {
        self.temporary.close().map_err(|source| FileWriteError {
            phase: FileWritePhase::Cleanup,
            publication: FilePublication::NotPublished,
            source,
            cleanup: None,
        })
    }

    /// Check cancellation immediately before calling this method. Once started,
    /// publication and cleanup must drain. Publication atomically replaces existing
    /// regular files; source verification remains the caller's responsibility.
    pub fn finish(self) -> Result<FilePublication, FileWriteError> {
        self.finish_checked(|| false)
    }

    fn finish_checked(
        self,
        cancelled: impl Fn() -> bool,
    ) -> Result<FilePublication, FileWriteError> {
        self.finish_with(cancelled, std::fs::File::sync_all, sync_directory)
    }

    // Narrow operation seams keep durability/cleanup faults fixture-owned.
    fn finish_with(
        self,
        cancelled: impl Fn() -> bool,
        sync_file: impl FnOnce(&std::fs::File) -> io::Result<()>,
        sync_parent: impl FnOnce(&Path) -> io::Result<()>,
    ) -> Result<FilePublication, FileWriteError> {
        if cancelled() {
            return Err(self.fail(
                FileWritePhase::Cancelled,
                io::Error::from(io::ErrorKind::Interrupted),
            ));
        }
        if self.failed {
            return Err(self.fail(
                FileWritePhase::Copy,
                io::Error::other("copy previously failed"),
            ));
        }
        if self.remaining != 0 {
            return Err(self.fail(
                FileWritePhase::Copy,
                io::Error::new(io::ErrorKind::UnexpectedEof, "source shrank"),
            ));
        }
        if let Err(source) = sync_file(self.temporary.as_file()) {
            return Err(self.fail(FileWritePhase::Sync, source));
        }
        if cancelled() {
            return Err(self.fail(
                FileWritePhase::Cancelled,
                io::Error::from(io::ErrorKind::Interrupted),
            ));
        }
        // Preserve rejection of non-file destinations. Rename never follows a
        // leaf symlink, even if the entry changes after this check.
        match std::fs::symlink_metadata(&self.destination) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(self.fail(
                    FileWritePhase::Publish,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "destination is not a regular file",
                    ),
                ));
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(self.fail(FileWritePhase::Publish, source)),
        }
        match crate::atomic::persist_with_retry(self.temporary, &self.destination) {
            Ok(_) => sync_parent_after_publish(&self.destination, sync_parent),
            Err(error) => {
                let cleanup = error.file.close().err();
                Err(FileWriteError {
                    phase: FileWritePhase::Publish,
                    publication: FilePublication::NotPublished,
                    source: error.error,
                    cleanup,
                })
            }
        }
    }
}

fn sync_parent_after_publish(
    destination: &Path,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<FilePublication, FileWriteError> {
    sync_parent(destination.parent().expect("object parent")).map_err(|source| FileWriteError {
        phase: FileWritePhase::DirectorySync,
        publication: FilePublication::Published,
        source,
        cleanup: None,
    })?;
    Ok(FilePublication::Published)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    // Counts capability operations, not syscalls inside std/tempfile/cache or
    // directory creation. These synchronous fixtures stay on one test thread.
    pub(super) struct WorkCounts {
        pub source_opens: usize,
        pub staging_opens: usize,
        pub metadata: usize,
        pub buffers: usize,
        pub buffer_capacity: usize,
    }

    thread_local! {
        static WORK: std::cell::Cell<WorkCounts> = const { std::cell::Cell::new(WorkCounts {
            source_opens: 0, staging_opens: 0, metadata: 0, buffers: 0, buffer_capacity: 0,
        }) };
    }

    pub(super) fn record(update: impl FnOnce(&mut WorkCounts)) {
        WORK.with(|cell| {
            let mut counts = cell.get();
            update(&mut counts);
            cell.set(counts);
        });
    }

    fn count_work<T>(work: impl FnOnce() -> T) -> (T, WorkCounts) {
        WORK.set(WorkCounts::default());
        let result = work();
        (result, WORK.take())
    }

    #[test]
    fn presence_performs_one_metadata_check_and_preserves_io_errors() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("object");
        std::fs::write(&path, b"object").unwrap();
        let (result, counts) = count_work(|| PreparedFilePresence(path.clone()).check());
        assert!(result.unwrap());
        assert_eq!(
            counts,
            WorkCounts {
                metadata: 1,
                ..WorkCounts::default()
            }
        );
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
        ] {
            let (result, counts) = count_work(|| {
                PreparedFilePresence(path.clone()).check_with(|_| Err(io::Error::from(kind)))
            });
            if kind == io::ErrorKind::NotFound {
                assert!(!result.unwrap());
            } else {
                assert_eq!(result.unwrap_err().kind(), kind);
            }
            assert_eq!(
                counts,
                WorkCounts {
                    metadata: 1,
                    ..WorkCounts::default()
                }
            );
        }
    }

    #[test]
    fn file_capability_has_no_handle_clones_xattrs_or_nested_dispatch() {
        // Structural guard complements operation counters: do not add unused
        // zero counters for operations this capability deliberately cannot do.
        let production = include_str!("file.rs")
            .split("\nmod tests {")
            .next()
            .unwrap();
        for forbidden in ["try_clone(", "xattr::", "spawn_blocking(", "block_on("] {
            assert!(
                !production.contains(forbidden),
                "unexpected operation: {forbidden}"
            );
        }
    }

    #[test]
    fn fused_transfer_work_is_constant_across_buffer_boundaries() {
        let chunk = super::super::TRANSFER_CHUNK_SIZE;
        for size in [0, 7, chunk - 1, chunk, chunk + 1, 3 * chunk + 17] {
            let source_cache = tempfile::tempdir().unwrap();
            let remote = tempfile::tempdir().unwrap();
            let local = tempfile::tempdir().unwrap();
            let bytes = vec![42; size];
            let oid = Oid::from_bytes(*blake3::hash(&bytes).as_bytes());
            let source = cached(source_cache.path(), &bytes);
            let (result, counts) =
                count_work(|| prepare(remote.path(), size as u64).upload(source, || false));
            assert_eq!(result.unwrap(), FilePublication::Published);
            assert_eq!(
                counts,
                WorkCounts {
                    source_opens: 1,
                    staging_opens: 1,
                    buffers: 1,
                    buffer_capacity: (size + 1).min(chunk),
                    ..WorkCounts::default()
                }
            );
            let cache =
                crate::RepositoryLayout::at(local.path().to_owned()).resolve_cache_root(None, None);
            let read = PreparedFileRead {
                source: remote.path().join("object"),
                oid,
            };
            let (result, counts) = count_work(|| read.receive(cache.writer(), || false));
            assert!(matches!(
                result.unwrap(),
                crate::ExpectedIngest::Published { .. }
            ));
            assert_eq!(
                counts,
                WorkCounts {
                    source_opens: 1,
                    metadata: 2,
                    buffers: 1,
                    buffer_capacity: size.max(1).min(chunk),
                    ..WorkCounts::default()
                }
            );
            assert!(cache.presence().contains(&oid));
        }
    }

    fn read_fixture(root: &Path, bytes: &[u8], oid: Oid) -> PreparedFileRead {
        let source = root.join("source");
        std::fs::write(&source, bytes).unwrap();
        PreparedFileRead { source, oid }
    }

    #[test]
    fn receive_size_hint_never_limits_eof_or_expected_oid_validation() {
        let bytes = b"more bytes than the size hint";
        for expected_full_body in [false, true] {
            let remote = tempfile::tempdir().unwrap();
            let local = tempfile::tempdir().unwrap();
            let cache =
                crate::RepositoryLayout::at(local.path().to_owned()).resolve_cache_root(None, None);
            let expected = if expected_full_body {
                bytes.as_slice()
            } else {
                &bytes[..1]
            };
            let oid = Oid::from_bytes(*blake3::hash(expected).as_bytes());
            let read = read_fixture(remote.path(), bytes, oid);
            let result = read
                .receive_reader(std::io::Cursor::new(bytes), cache.writer(), || false, 1)
                .unwrap();
            if expected_full_body {
                assert!(matches!(result, crate::ExpectedIngest::Published { .. }));
                assert!(cache.presence().contains(&oid));
            } else {
                assert!(matches!(result, crate::ExpectedIngest::HashMismatch { .. }));
                assert!(!cache.presence().contains(&oid));
            }
        }
    }

    #[test]
    fn fused_receive_reuses_buffer_across_short_reads_and_preserves_read_failures() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Source {
            input: std::io::Cursor<Vec<u8>>,
            interrupted: bool,
            fail: bool,
            buffer: Option<usize>,
            closed: Arc<AtomicBool>,
        }
        impl Read for Source {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                assert_eq!(
                    *self.buffer.get_or_insert(bytes.as_ptr() as usize),
                    bytes.as_ptr() as usize
                );
                assert_eq!(bytes.len(), super::super::TRANSFER_CHUNK_SIZE);
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                if self.fail && self.input.position() > 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "READ-SECRET",
                    ));
                }
                self.input.read(&mut bytes[..8191])
            }
        }
        impl Drop for Source {
            fn drop(&mut self) {
                self.closed.store(true, Ordering::Relaxed);
            }
        }
        for fail in [false, true] {
            let remote = tempfile::tempdir().unwrap();
            let local = tempfile::tempdir().unwrap();
            let cache =
                crate::RepositoryLayout::at(local.path().to_owned()).resolve_cache_root(None, None);
            let bytes = vec![42; super::super::TRANSFER_CHUNK_SIZE + 17];
            let oid = Oid::from_bytes(*blake3::hash(&bytes).as_bytes());
            let read = read_fixture(remote.path(), &bytes, oid);
            let closed = Arc::new(AtomicBool::new(false));
            let source = Source {
                input: std::io::Cursor::new(bytes),
                interrupted: false,
                fail,
                buffer: None,
                closed: closed.clone(),
            };
            let result = read.receive_reader(
                source,
                cache.writer(),
                || {
                    if closed.load(Ordering::Relaxed) {
                        assert!(
                            !cache.presence().contains(&oid),
                            "source closes before publication"
                        );
                    }
                    false
                },
                super::super::TRANSFER_CHUNK_SIZE,
            );
            assert!(closed.load(Ordering::Relaxed));
            if fail {
                assert!(matches!(
                    result,
                    Err(FileReceiveError::Remote(
                        super::super::RemoteError::PermissionDenied { .. }
                    ))
                ));
                assert_eq!(std::fs::read_dir(cache.display_path()).unwrap().count(), 0);
            } else {
                assert!(matches!(
                    result.unwrap(),
                    crate::ExpectedIngest::Published { .. }
                ));
                assert_eq!(
                    cache.open_client().verify(&oid).unwrap(),
                    crate::ObjectVerification::Valid
                );
            }
        }
    }

    #[test]
    fn fused_receive_cancellation_between_chunks_discards_cache_staging() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let remote = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let cache =
            crate::RepositoryLayout::at(local.path().to_owned()).resolve_cache_root(None, None);
        let bytes = vec![42; 3 * super::super::TRANSFER_CHUNK_SIZE];
        let oid = Oid::from_bytes(*blake3::hash(&bytes).as_bytes());
        let read = read_fixture(remote.path(), &bytes, oid);
        let calls = AtomicUsize::new(0);
        let error = read
            .receive(cache.writer(), || {
                let call = calls.fetch_add(1, Ordering::Relaxed);
                if call == 4 {
                    assert_eq!(std::fs::read_dir(cache.display_path()).unwrap().count(), 1);
                }
                call >= 4
            })
            .unwrap_err();
        assert!(matches!(error, FileReceiveError::Cancelled));
        assert_eq!(std::fs::read_dir(cache.display_path()).unwrap().count(), 0);
        assert!(!cache.presence().contains(&oid));
    }

    #[test]
    fn fused_receive_mismatch_preserves_valid_cache_and_publishes_neither_oid() {
        let remote = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let cache =
            crate::RepositoryLayout::at(local.path().to_owned()).resolve_cache_root(None, None);
        let (valid, _) = cache
            .writer()
            .ingest(std::io::Cursor::new(b"valid"))
            .unwrap();
        let wrong = Oid::from_bytes(*blake3::hash(b"wrong").as_bytes());
        let result = read_fixture(remote.path(), b"wrong", valid.oid)
            .receive(cache.writer(), || false)
            .unwrap();
        assert!(
            matches!(result, crate::ExpectedIngest::HashMismatch { actual } if actual == wrong)
        );
        let mut cached = cache.open_client().object(&valid.oid).open().unwrap();
        assert_eq!(cached.read_small(32).unwrap(), b"valid");
        assert!(!cache.presence().contains(&wrong));
    }

    #[test]
    fn fused_receive_rejects_missing_directories_and_symlinks_before_cache_creation() {
        let remote = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let cache =
            crate::RepositoryLayout::at(local.path().to_owned()).resolve_cache_root(None, None);
        let oid = Oid::from_bytes([0; 32]);
        let source = remote.path().join("missing");
        let read = || PreparedFileRead {
            source: source.clone(),
            oid,
        };
        assert!(matches!(
            read().receive(cache.writer(), || false),
            Err(FileReceiveError::Remote(
                super::super::RemoteError::NotFound { .. }
            ))
        ));
        std::fs::create_dir(&source).unwrap();
        assert!(read().receive(cache.writer(), || false).is_err());
        std::fs::remove_dir(&source).unwrap();
        #[cfg(unix)]
        {
            let target = remote.path().join("target");
            std::fs::write(&target, b"valid").unwrap();
            std::os::unix::fs::symlink(&target, &source).unwrap();
            assert!(read().receive(cache.writer(), || false).is_err());
            std::fs::remove_file(&target).unwrap();
            assert!(read().receive(cache.writer(), || false).is_err());
        }
        assert!(!cache.display_path().exists());
    }

    fn cached(root: &Path, bytes: &[u8]) -> crate::CacheObject {
        let ingested = crate::cache::object::ingest(root, std::io::Cursor::new(bytes)).unwrap();
        let cache = crate::CacheClient::open(root.to_owned());
        cache.verify(&ingested.oid).unwrap();
        cache.object(&ingested.oid)
    }

    #[test]
    fn fused_upload_handles_empty_tiny_and_multiple_chunks() {
        for size in [
            0,
            7,
            super::super::TRANSFER_CHUNK_SIZE,
            3 * super::super::TRANSFER_CHUNK_SIZE + 17,
        ] {
            let cache = tempfile::tempdir().unwrap();
            let remote = tempfile::tempdir().unwrap();
            let bytes = vec![42; size];
            let source = cached(cache.path(), &bytes);
            assert_eq!(
                prepare(remote.path(), size as u64)
                    .upload(source, || false)
                    .unwrap(),
                FilePublication::Published
            );
            assert_eq!(std::fs::read(remote.path().join("object")).unwrap(), bytes);
            assert_eq!(std::fs::read_dir(remote.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn fused_upload_cancellation_between_chunks_owns_cleanup() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let bytes = vec![42; 3 * super::super::TRANSFER_CHUNK_SIZE];
        let source = cached(cache.path(), &bytes);
        let calls = AtomicUsize::new(0);
        let error = prepare(remote.path(), bytes.len() as u64)
            .upload(source, || {
                let call = calls.fetch_add(1, Ordering::Relaxed);
                if call == 3 {
                    assert!(!remote.path().join("object").exists());
                    assert_eq!(std::fs::read_dir(remote.path()).unwrap().count(), 1);
                }
                call >= 3
            })
            .unwrap_err();
        assert!(matches!(
            error,
            FileUploadError::Write(FileWriteError {
                phase: FileWritePhase::Cancelled,
                publication: FilePublication::NotPublished,
                cleanup: None,
                ..
            })
        ));
        assert_eq!(std::fs::read_dir(remote.path()).unwrap().count(), 0);
    }

    #[test]
    fn cancellation_after_file_sync_still_prevents_publication() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let writer = prepare(root.path(), 0).begin().unwrap();
        let calls = AtomicUsize::new(0);
        let error = writer
            .finish_checked(|| calls.fetch_add(1, Ordering::Relaxed) == 1)
            .unwrap_err();
        assert_eq!(error.phase, FileWritePhase::Cancelled);
        assert_eq!(error.publication, FilePublication::NotPublished);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn fused_upload_rejects_changed_sizes_and_cancellation_before_open() {
        let cache = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        let source = cached(cache.path(), b"data");
        for size in [3, 5] {
            assert!(
                prepare(remote.path(), size)
                    .upload(source.clone(), || false)
                    .is_err()
            );
            assert_eq!(std::fs::read_dir(remote.path()).unwrap().count(), 0);
        }
        assert!(matches!(
            prepare(remote.path(), 4).upload(source, || true),
            Err(FileUploadError::Cancelled)
        ));
        assert_eq!(std::fs::read_dir(remote.path()).unwrap().count(), 0);
    }

    #[test]
    fn metadata_presence_rejects_non_objects() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("object");
        assert!(!PreparedFilePresence(path.clone()).check().unwrap());
        std::fs::create_dir(&path).unwrap();
        assert!(PreparedFilePresence(path.clone()).check().is_err());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, b"data").unwrap();
        assert!(PreparedFilePresence(path.clone()).check().unwrap());
        #[cfg(unix)]
        {
            let link = root.path().join("link");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(PreparedFilePresence(link.clone()).check().is_err());
            std::fs::remove_file(&path).unwrap();
            assert!(PreparedFilePresence(link).check().is_err());
        }
    }

    #[test]
    fn query_root_is_shared_and_other_file_options_are_explicitly_rejected() {
        super::super::initialize_backends();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut url = url::Url::parse(&super::super::file_url(first.path())).unwrap();
        url.query_pairs_mut()
            .append_pair("root", second.path().to_str().unwrap());
        runtime.block_on(async {
            let client = RemoteClient::open(url.as_str()).unwrap();
            assert!(
                client
                    .prepare_write(0, super::super::TRANSFER_CHUNK_SIZE)
                    .is_err(),
                "file uploads cannot fall back to the async direct-write path"
            );
            let oid = Oid::from_bytes([5; 32]);
            client
                .prepare_file_write(&oid, 0)
                .unwrap()
                .begin()
                .unwrap()
                .finish()
                .unwrap();
            assert!(
                second
                    .path()
                    .join(crate::cache::object_key_oid(&oid))
                    .exists()
            );
            assert_eq!(std::fs::read_dir(first.path()).unwrap().count(), 0);
            assert!(client.contains_object(&oid).await.unwrap());
            assert!(client.prepare_file_presence(&oid).unwrap().check().unwrap());
            for option in ["atomic_write_dir", "unknown"] {
                let mut invalid = url.clone();
                invalid
                    .query_pairs_mut()
                    .append_pair(option, "SECRET-OPTION");
                let error = RemoteClient::open(invalid.as_str()).unwrap_err();
                assert!(!error.to_string().contains("SECRET-OPTION"));
            }
        });
        assert!(
            // hygiene-ok: parser-only Windows URI fixture; never opened.
            super::super::windows_file_root_uri("file:///C:/ignored?root=D%3A%2Fobjects").is_none()
        );
    }

    fn prepare(root: &std::path::Path, size: u64) -> PreparedFileWrite {
        PreparedFileWrite {
            destination: root.join("object"),
            size,
        }
    }

    #[test]
    fn partial_copy_is_invisible_and_abort_removes_staging() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = prepare(root.path(), 6).begin().unwrap();
        writer.append(b"abc").unwrap();
        assert!(!root.path().join("object").exists());
        writer.abort().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn competing_publishers_atomically_replace_complete_objects() {
        let root = tempfile::tempdir().unwrap();
        let mut first = prepare(root.path(), 3).begin().unwrap();
        let mut second = prepare(root.path(), 3).begin().unwrap();
        first.append(b"one").unwrap();
        second.append(b"two").unwrap();
        assert_eq!(first.finish().unwrap(), FilePublication::Published);
        assert_eq!(second.finish().unwrap(), FilePublication::Published);
        assert_eq!(std::fs::read(root.path().join("object")).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn rename_publication_replaces_an_existing_complete_object_atomically() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = prepare(root.path(), 3).begin().unwrap();
        std::fs::write(root.path().join("object"), b"old").unwrap();
        writer.append(b"new").unwrap();
        assert_eq!(writer.finish().unwrap(), FilePublication::Published);
        assert_eq!(std::fs::read(root.path().join("object")).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn simultaneous_publishers_leave_one_complete_object() {
        let root = tempfile::tempdir().unwrap();
        let mut first = prepare(root.path(), 3).begin().unwrap();
        let mut second = prepare(root.path(), 3).begin().unwrap();
        first.append(b"one").unwrap();
        second.append(b"two").unwrap();
        let gate = std::sync::Barrier::new(2);
        let outcomes = std::thread::scope(|scope| {
            let one = scope.spawn(|| {
                gate.wait();
                first.finish().unwrap()
            });
            let two = scope.spawn(|| {
                gate.wait();
                second.finish().unwrap()
            });
            [one.join().unwrap(), two.join().unwrap()]
        });
        assert_eq!(
            outcomes
                .iter()
                .filter(|&&outcome| outcome == FilePublication::Published)
                .count(),
            2
        );
        let winner = std::fs::read(root.path().join("object")).unwrap();
        assert!(winner == b"one" || winner == b"two");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn cleanup_failure_does_not_replace_the_primary_failure() {
        let root = tempfile::tempdir().unwrap();
        let writer = prepare(root.path(), 0).begin().unwrap();
        // Fixture-owned fault: remove staging before its explicit cleanup.
        std::fs::remove_file(writer.temporary.path()).unwrap();
        let error = writer.fail(
            FileWritePhase::Copy,
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.cleanup.unwrap().kind(), io::ErrorKind::NotFound);
        assert_eq!(error.publication, FilePublication::NotPublished);
    }

    #[test]
    fn short_and_growing_sources_never_publish() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = prepare(root.path(), 4).begin().unwrap();
        assert!(writer.append(b"excess").is_err());
        assert!(writer.append(b"abc").is_err());
        let error = writer.finish().unwrap_err();
        assert_eq!(error.phase, FileWritePhase::Copy);
        assert_eq!(error.publication, FilePublication::NotPublished);
        assert!(error.cleanup.is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn file_sync_failure_cleans_staging_without_publishing() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = prepare(root.path(), 3).begin().unwrap();
        writer.append(b"new").unwrap();
        let error = writer
            .finish_with(
                || false,
                |_| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                |_| panic!("directory sync must not start after failed sync"),
            )
            .unwrap_err();
        assert_eq!(error.phase, FileWritePhase::Sync);
        assert_eq!(error.publication, FilePublication::NotPublished);
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.cleanup.is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn directory_sync_failure_reports_completed_publication() {
        for existing in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let destination = root.path().join("object");
            if existing {
                std::fs::write(&destination, b"old").unwrap();
            }
            let mut writer = prepare(root.path(), 3).begin().unwrap();
            writer.append(b"new").unwrap();
            let error = writer
                .finish_with(
                    || false,
                    std::fs::File::sync_all,
                    |_| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                )
                .unwrap_err();
            assert_eq!(error.phase, FileWritePhase::DirectorySync);
            assert_eq!(error.publication, FilePublication::Published);
            assert!(error.cleanup.is_none());
            assert_eq!(std::fs::read(&destination).unwrap(), b"new");
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn directory_destination_is_not_a_successful_winner() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("object")).unwrap();
        let error = prepare(root.path(), 0)
            .begin()
            .unwrap()
            .finish()
            .unwrap_err();
        assert_eq!(error.phase, FileWritePhase::Publish);
        assert_eq!(error.publication, FilePublication::NotPublished);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn short_source_is_cleaned_up() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = prepare(root.path(), 4).begin().unwrap();
        writer.append(b"abc").unwrap();
        let error = writer.finish().unwrap_err();
        assert_eq!(error.source.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_winner_is_rejected_without_touching_its_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("object")).unwrap();
        let error = prepare(root.path(), 0)
            .begin()
            .unwrap()
            .finish()
            .unwrap_err();
        assert_eq!(error.publication, FilePublication::NotPublished);
        assert_eq!(std::fs::read(target).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn operator_root_and_inventory_agree_with_prepared_publication() {
        super::super::initialize_backends();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        runtime.block_on(async {
            let client = RemoteClient::open(&super::super::file_url(root.path())).unwrap();
            let oid = Oid::from_bytes([7; 32]);
            let mut writer = client.prepare_file_write(&oid, 3).unwrap().begin().unwrap();
            writer.append(b"abc").unwrap();
            assert!(
                client
                    .enumerate_objects()
                    .await
                    .unwrap()
                    .next()
                    .await
                    .is_none()
            );
            assert!(!client.contains_object(&oid).await.unwrap());
            writer.finish().unwrap();
            assert!(client.contains_object(&oid).await.unwrap());
            let mut objects = client.enumerate_objects().await.unwrap();
            assert_eq!(objects.next().await.unwrap().unwrap().oid, oid);
            assert!(objects.next().await.is_none());
            client.delete_objects(&[oid], |_| {}).await.unwrap();
            assert!(!client.contains_object(&oid).await.unwrap());
        });
    }
}
