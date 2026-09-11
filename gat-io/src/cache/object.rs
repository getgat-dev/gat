//! Local object cache: content-addressed object paths, hashing,
//! ingest/finalization of new objects, and materialization of cache
//! objects into the working tree.
//!
//! `.gat/objects` (`objects_dir`) is the storage/cache *root*, touched
//! with `std::fs` directly -- no need to route local reads/writes through
//! an async storage layer. Finalized content-addressed objects live only
//! under its `blake3/` child namespace (`.gat/objects/blake3/xx/yy/oid`,
//! see [`crate::cache::layout::object_key`]); `objects_dir` itself also
//! holds sibling state that is *not* part of that namespace:
//! `cache.sqlite3` (the proof database) and `tmp-*` ingest scratch files.

use crate::cache::proof::{CacheObservation, CachePublication, CacheState, ObjectVerification};
#[cfg(any(test, feature = "test-support"))]
use crate::file_state::observe_regular_file_no_follow;
#[cfg(test)]
use crate::remote::STREAM_BUFFER_SIZE;
use gat_core::config::{MaterializationMode, MaterializationStrategy};
use gat_core::oid::Oid;
use rayon::prelude::*;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::NamedTempFile;

/// Local, filesystem-level failures for the object cache root
/// (`objects_dir`, its `blake3/` namespace, and materialized working-tree
/// copies) -- distinct from `crate::cache::proof::CacheStateError`
/// (the `cache.sqlite3` proof store).
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// Cache storage could not be prepared: the target directory, an object's
    /// fan-out parent, or the repository's required self-ignore file failed.
    #[error("could not prepare local cache storage at `{}`", path.display())]
    DirectoryUnavailable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A scratch file for an in-progress ingest could not be created
    /// under `objects_dir` -- also a "cache directory unavailable" case,
    /// kept as its own variant since it names no specific object path.
    #[error(
        "could not create a temporary file in the local object cache `{}`",
        path.display()
    )]
    TempFileUnavailable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A source path being ingested/materialized from could not be read
    /// (opened, stat'd, mmap'd, or copied) -- a local filesystem failure
    /// on the *input* side, kept distinct from any failure writing into
    /// the cache itself.
    #[error("could not read `{}`", path.display())]
    PathUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A streaming ingest source failed while being read. Unlike
    /// [`Self::PathUnreadable`], this source has no local filesystem path
    /// (for example, a remote object reader).
    #[error("could not read the object ingest source")]
    SourceUnreadable {
        #[source]
        source: std::io::Error,
    },
    /// A cache entry could not be written to (creating/finalizing an
    /// ingest's content, or toggling its read-only protection bit) --
    /// "cache entry unwritable".
    #[error("could not write to the local object cache entry `{}`", path.display())]
    EntryUnwritable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A cache entry named by an existing, trusted oid could not be read
    /// back (e.g. while materializing it into the working tree) --
    /// "cache entry unreadable"/"object missing", depending on whether
    /// the underlying I/O error is `NotFound` or something else.
    #[error("could not read the cached object `{}`", path.display())]
    EntryUnreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Every configured `cache.materialization_strategy` mode failed to
    /// materialize an object into the working tree. Retains each
    /// attempted mode's own typed [`CacheError`] (not a stringified
    /// summary) so the mapper can classify/log them without ever
    /// formatting a raw filesystem error into user-facing text.
    #[error(
        "could not materialize `{}` -- every configured cache.materialization_strategy mode failed ({})",
        dest.display(),
        attempts.iter().map(|(mode, _)| mode.to_string()).collect::<Vec<_>>().join(", ")
    )]
    MaterializationFailed {
        dest: PathBuf,
        attempts: Vec<(MaterializationMode, Box<Self>)>,
    },
    /// The cache's local proof-index database (`cache.sqlite3`) failed to
    /// open, read, or write while servicing an ingest/verify/materialize
    /// call. Boxed to break the mutual reference with
    /// `crate::cache::proof::CacheStateError`, which itself
    /// reuses a [`CacheError`] variant for its own filesystem-level
    /// failures.
    #[error(transparent)]
    State(crate::cache::proof::CacheProofError),
    /// A cache-relative oid string failed to parse -- always a
    /// corrupted/foreign `cache.sqlite3` row or `gat.lock` field, since
    /// every oid Gat itself writes is already validated.
    #[error(transparent)]
    Oid(#[from] gat_core::oid::OidFormatError),
    /// A `gat.lock`-adjacent atomic filesystem write (via
    /// `crate::atomic`) failed while this module updated on-disk state.
    #[error(transparent)]
    Atomic(#[from] crate::atomic::AtomicError),
}

impl From<crate::cache::proof::CacheStateError> for CacheError {
    fn from(err: crate::cache::proof::CacheStateError) -> Self {
        match err {
            crate::cache::proof::CacheStateError::Cache(source) => source,
            source => Self::State(crate::cache::proof::CacheProofError::from(source)),
        }
    }
}

/// Prefix for cache ingest scratch files. Plain and ownership-free by
/// design: an ordinary `add`/`fetch` ingest just needs `NamedTempFile`'s
/// own exclusive-creation/auto-cleanup-on-drop guarantees, with no extra
/// filesystem work (a lock, a host/pid-tagged name, ...) on the hot path.
///
/// Because a live ingest's temp file is indistinguishable from an
/// abandoned one by name alone, `gat system clean cache` never removes a
/// `tmp-*` file by default -- see `commands::system::cache`, which
/// requires the explicit `--purge-temporary` opt-in (with a clear warning
/// that doing so can disrupt another process's in-progress ingest) before
/// removing any of them.
pub const TEMP_PREFIX: &str = "tmp-";

/// This module's own typed `Result` alias -- every fallible function here
/// returns a typed [`CacheError`] directly, never `anyhow::Result`, so
/// callers never need to downcast an opaque error to recover cache-specific
/// classification.
pub type Result<T> = std::result::Result<T, CacheError>;

fn create_tmp_file(objects_dir: &Path) -> Result<NamedTempFile> {
    tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .tempfile_in(objects_dir)
        .map_err(|source| CacheError::TempFileUnavailable {
            path: objects_dir.to_path_buf(),
            source,
        })
}

/// Ensures `dir` exists, mapping any failure to
/// [`CacheError::DirectoryUnavailable`] rather than a bare `?`-propagated
/// [`std::io::Error`] -- shared by every `ingest*_to_tmp`/`publish_tmp`
/// call site that must prepare a cache directory before writing into it.
fn ensure_cache_directory(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| CacheError::DirectoryUnavailable {
        path: dir.to_path_buf(),
        source,
    })
}

/// Builds the complete on-disk path for an already-validated [`Oid`] via
/// [`crate::cache::layout::object_key_oid`], infallibly (an `Oid` is
/// always well-formed, so there is no [`gat_core::oid::OidFormatError`]
/// to propagate) and in one destination allocation.
pub fn cache_path_oid(objects_dir: &Path, oid: &Oid) -> PathBuf {
    objects_dir.join(crate::cache::layout::object_key_oid(oid))
}

/// The root of the finalized BLAKE3 object namespace under `objects_dir`
/// (`<objects_dir>/blake3`) -- the directory whose only well-formed
/// contents are `xx/yy/oid` fan-out object leaves. Directory-walking code
/// (local/remote GC, `gat system cache` maintenance) should enumerate
/// objects starting here rather than at `objects_dir` itself, so it never
/// has to reason about `objects_dir`'s other siblings (`cache.sqlite3`,
/// `tmp-*`) at all.
pub fn object_namespace_dir(objects_dir: &Path) -> PathBuf {
    objects_dir.join(crate::cache::layout::OBJECT_HASH_NAMESPACE)
}

/// Checks for an already-published
/// object directly from an already-validated [`Oid`], via
/// [`cache_path_oid`], with no [`gat_core::oid::OidFormatError`] to
/// propagate.
pub fn has_object_oid(objects_dir: &Path, oid: &Oid) -> bool {
    cache_path_oid(objects_dir, oid).exists()
}

/// How many oids one bounded verification window processes at a time in
/// [`CacheClient::verify_windows`]: large enough that the set-based proof
/// lookup/persist and the parallel filesystem step amortize their
/// per-batch overhead, small enough that a huge selection never
/// materializes every oid's status/proof at once. Independent
/// of the `SQLite` bind budget, which only bounds how many physical
/// statements a *single* logical lookup/mutation of one window maps to.
///
/// Also reused by DB-free publication callers (`fetch`/`add`) as the
/// bound on how many freshly-produced [`CachePublication`]s they accumulate
/// before draining them into one `CacheState::apply_many` persist, so a
/// very large operation never retains every proof delta it will ever
/// produce, matching the same bounded-window intent.
pub const VERIFY_WINDOW: usize = 4096;

/// Test-only override for [`VERIFY_WINDOW`]:
/// lets a test drive the exact same production
/// [`CacheClient::verify_windows`]/[`CacheClient::verify_windows_unmemoized`]
/// implementation with a deliberately small verification subwindow, so a
/// transfer window many times larger than the verification chunk can be
/// exercised without needing a many-thousand-object fixture.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static VERIFY_WINDOW_OVERRIDE: Cell<Option<usize>> = const { Cell::new(None) };
    }

    /// Scoped override: `verify_window()` reflects `size` until the
    /// returned guard drops, then reverts to the production default.
    #[must_use]
    pub fn with_verify_window(size: usize) -> VerifyWindowGuard {
        VERIFY_WINDOW_OVERRIDE.with(|c| c.set(Some(size)));
        VerifyWindowGuard
    }

    pub struct VerifyWindowGuard;

    impl Drop for VerifyWindowGuard {
        fn drop(&mut self) {
            VERIFY_WINDOW_OVERRIDE.with(|c| c.set(None));
        }
    }

    pub fn verify_window_override() -> Option<usize> {
        VERIFY_WINDOW_OVERRIDE.with(Cell::get)
    }

    thread_local! {
        static MEMO_HIGH_WATER: Cell<usize> = const { Cell::new(0) };
    }

    /// Record the current size of
    /// [`CacheClient`](super::CacheClient)'s operation-local verification
    /// memo, tracking the high-water mark so a test can assert the memo
    /// never silently grows to retain the whole operation's verification
    /// state (e.g. `verify_windows_unmemoized` purging per window).
    pub fn record_memo_size(size: usize) {
        MEMO_HIGH_WATER.with(|c| c.set(c.get().max(size)));
    }

    pub fn memo_high_water() -> usize {
        MEMO_HIGH_WATER.with(Cell::get)
    }

    /// Reset the high-water mark back to zero so a test that runs more
    /// than one sync in sequence (e.g. an initial ordinary sync to
    /// establish a clean starting state, followed by the `--rematerialize`
    /// run actually under test) can measure only the later run's memo
    /// growth, unaffected by an earlier run's retained memo size.
    pub fn reset_memo_high_water() {
        MEMO_HIGH_WATER.with(|c| c.set(0));
    }
}

/// The verification chunk size actually used by
/// [`CacheClient::verify_windows_impl`]: [`VERIFY_WINDOW`] in production,
/// or a test's injected override when one is active.
#[allow(
    clippy::missing_const_for_fn,
    reason = "Test builds read a thread-local override, so this shared definition cannot be const"
)]
fn verify_window_size() -> usize {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(size) = test_support::verify_window_override() {
        return size;
    }
    VERIFY_WINDOW
}

/// A lazy handle to one finalized cache object.
///
/// The physical cache path is intentionally opaque so higher layers can move
/// this capability into worker tasks without taking ownership of cache layout.
#[derive(Clone)]
pub struct CacheObject {
    path: Arc<PathBuf>,
    verified_size: Option<u64>,
}

impl CacheObject {
    /// Size from this operation's coherent verification, if one was performed.
    #[must_use]
    pub const fn verified_size(&self) -> Option<u64> {
        self.verified_size
    }

    /// Opens this object for streaming and captures its size for remote upload
    /// tuning.
    pub fn open(self) -> std::result::Result<CacheObjectReader, CacheObjectOpenError> {
        let file = File::open(self.path.as_ref()).map_err(|source| CacheObjectOpenError {
            stage: CacheObjectOpenStage::Open,
            path: Arc::clone(&self.path),
            source,
        })?;
        let size = if let Some(size) = self.verified_size {
            size
        } else {
            file.metadata()
                .map_err(|source| CacheObjectOpenError {
                    stage: CacheObjectOpenStage::Metadata,
                    path: Arc::clone(&self.path),
                    source,
                })?
                .len()
        };
        Ok(CacheObjectReader { file, size })
    }
}

/// Which local operation failed while opening an opaque cache object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheObjectOpenStage {
    Open,
    Metadata,
}

/// Failure to open or inspect an opaque cache object.
#[derive(Debug, thiserror::Error)]
#[error("could not {stage:?} the cached object `{}`", path.display())]
pub struct CacheObjectOpenError {
    stage: CacheObjectOpenStage,
    path: Arc<PathBuf>,
    #[source]
    source: std::io::Error,
}

impl CacheObjectOpenError {
    #[must_use]
    pub const fn stage(&self) -> CacheObjectOpenStage {
        self.stage
    }

    #[must_use]
    pub fn io_kind(&self) -> std::io::ErrorKind {
        self.source.kind()
    }
}

/// Owned, worker-safe reader for one cache object.
#[derive(Debug)]
pub struct CacheObjectReader {
    file: File,
    size: u64,
}

/// Proof-free local object-presence capability.
pub struct CachePresence {
    root: Arc<crate::cache::root::CacheRootInner>,
}

impl CachePresence {
    pub(crate) const fn new(root: Arc<crate::cache::root::CacheRootInner>) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn contains(&self, oid: &Oid) -> bool {
        has_object_oid(&self.root.objects_dir, oid)
    }

    /// Fallible, proof-free inspection for add. Only regular objects qualify.
    pub fn inspect_regular(&self, oid: &Oid) -> Result<bool> {
        let path = cache_path_oid(&self.root.objects_dir, oid);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => Ok(true),
            Ok(_) => Err(CacheError::EntryUnreadable {
                path,
                source: std::io::Error::from(std::io::ErrorKind::InvalidData),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(CacheError::EntryUnreadable { path, source }),
        }
    }
}

/// Cloneable, worker-safe cache writer that never opens the proof database.
#[derive(Clone)]
pub struct CacheWriter {
    root: Arc<crate::cache::root::CacheRootInner>,
}

impl CacheWriter {
    /// Starts an unpublished object. Dropping the handle discards its temporary file.
    pub fn begin_ingest(&self) -> Result<CacheIngest> {
        self.root.prepare_write()?;
        ensure_cache_directory(&self.root.objects_dir)?;
        Ok(CacheIngest {
            root: Arc::clone(&self.root),
            tmp: create_tmp_file(&self.root.objects_dir)?,
            hasher: blake3::Hasher::new(),
            size: 0,
        })
    }

    pub(crate) const fn new(root: Arc<crate::cache::root::CacheRootInner>) -> Self {
        Self { root }
    }

    pub fn ingest<R: std::io::Read>(
        &self,
        reader: R,
    ) -> Result<(Ingested, Option<CachePublication>)> {
        self.root.prepare_write()?;
        ingest_delta(&self.root.objects_dir, reader)
    }

    /// Same as [`Self::ingest`], but for a caller (e.g. `fetch`, downloading
    /// a remote object whose size the remote already reported via `stat`)
    /// that already knows the exact byte count up front: forwards
    /// `Some(size)` to the shared scratch-buffer helper so the very first
    /// read never allocates more than the object actually needs, instead
    /// of starting from the full [`crate::remote::STREAM_BUFFER_SIZE`]
    /// scratch buffer every unknown-size caller must assume.
    pub fn ingest_sized<R: std::io::Read>(
        &self,
        reader: R,
        size: u64,
    ) -> Result<(Ingested, Option<CachePublication>)> {
        self.root.prepare_write()?;
        ingest_sized_delta(&self.root.objects_dir, reader, size)
    }

    pub fn ingest_expected<R: std::io::Read>(
        &self,
        expected: Oid,
        reader: R,
    ) -> Result<ExpectedIngest> {
        self.root.prepare_write()?;
        ingest_expected_delta(&self.root.objects_dir, expected, reader)
    }
}

impl CacheObjectReader {
    pub(crate) fn read_into(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(bytes)
    }

    pub fn read_small(&mut self, limit: usize) -> std::io::Result<Vec<u8>> {
        let size = usize::try_from(self.size)
            .ok()
            .filter(|size| *size <= limit)
            .ok_or_else(|| std::io::Error::other("cache source exceeds small-object limit"))?;
        let mut bytes = vec![0; size];
        self.file.read_exact(&mut bytes)?;
        if self.file.read(&mut [0])? != 0 {
            return Err(std::io::Error::other(
                "cache source changed after verification",
            ));
        }
        Ok(bytes)
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Reads at most one local transfer chunk; never waits for remote I/O.
    pub fn read_chunk(&mut self, capacity: usize) -> std::io::Result<Vec<u8>> {
        let mut bytes = vec![0; capacity];
        let count = self.file.read(&mut bytes)?;
        bytes.truncate(count);
        Ok(bytes)
    }
}

/// Incremental, proof-database-free ingest. All methods perform local work only.
pub struct CacheIngest {
    root: Arc<crate::cache::root::CacheRootInner>,
    tmp: NamedTempFile,
    hasher: blake3::Hasher,
    size: u64,
}

impl CacheIngest {
    pub fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.tmp
            .write_all(bytes)
            .map_err(|source| CacheError::EntryUnwritable {
                path: self.tmp.path().to_path_buf(),
                source,
            })?;
        self.hasher.update(bytes);
        self.size += bytes.len() as u64;
        Ok(())
    }

    /// Verifies identity before any publication or durability work.
    #[allow(
        clippy::missing_panics_doc,
        reason = "Unconditional publication cannot produce a cancelled result"
    )]
    pub fn finish(self, expected: Oid) -> Result<ExpectedIngest> {
        self.finish_unless_cancelled(expected, || false)
            .map(|result| result.expect("unconditional publication is never cancelled"))
    }

    /// None means cancellation before publication; dropping self removes staging.
    /// Once publication begins it runs to completion, even if cancellation arrives.
    pub(crate) fn finish_unless_cancelled(
        mut self,
        expected: Oid,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<ExpectedIngest>> {
        if cancelled() {
            return Ok(None);
        }
        let actual = Oid::from_bytes(*self.hasher.finalize().as_bytes());
        if actual != expected {
            return Ok(Some(ExpectedIngest::HashMismatch { actual }));
        }
        let path = self.tmp.path().to_path_buf();
        self.tmp
            .flush()
            .and_then(|()| self.tmp.as_file().sync_all())
            .map_err(|source| CacheError::EntryUnwritable { path, source })?;
        #[cfg(any(test, feature = "test-support"))]
        record_sync_all();
        if cancelled() {
            return Ok(None);
        }
        let (_, publication) = publish_tmp(&self.root.objects_dir, self.tmp, expected, self.size)?;
        Ok(Some(ExpectedIngest::Published { publication }))
    }
}

impl Read for CacheObjectReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

/// The higher-level, operation-scoped verified-object cache API: callers
/// name the exact oids whose identity they need verified,
/// and this owns the `SQLite` lifecycle and access plan behind one
/// operation session -- a single `CacheState` proof index plus an
/// operation-local memo -- rather than opening `cache.sqlite3` inside
/// per-object or parallel loops.
///
/// - [`verify`](Self::verify) is the single-object convenience for use
///   *outside* loops; it memoizes, so composing helpers that re-ask for
///   the same oid never re-verify it.
/// - [`verify_windows`](Self::verify_windows) visits batch results in bounded windows:
///   duplicate-free, in bounded windows, with one set-based proof lookup
///   and one set-based proof persist per window and parallel filesystem
///   verification in between.
pub struct CacheClient {
    root: Arc<crate::cache::root::CacheRootInner>,
    index: CacheState,
    memo: std::cell::RefCell<std::collections::HashMap<Oid, CacheObservation>>,
}

/// Owned cache-verification work prepared by the coordinator.
///
/// The object paths and prior proofs remain opaque to higher layers. This
/// value is safe to move into a blocking worker because it contains no cache
/// client, proof database connection, or operation-scoped memo state.
pub struct PreparedCacheVerification {
    oids: Vec<Oid>,
    known: std::collections::HashMap<Oid, ObjectVerification>,
    pending: Vec<PreparedCacheObjectVerification>,
}

struct PreparedCacheObjectVerification {
    oid: Oid,
    path: PathBuf,
    prior: Option<crate::file_state::StatProof>,
}

/// Completed filesystem verification awaiting coordinator-side memo and
/// proof-database commit.
pub struct CompletedCacheVerification {
    oids: Vec<Oid>,
    known: std::collections::HashMap<Oid, ObjectVerification>,
    results: Vec<(Oid, CacheObservation, Option<CachePublication>)>,
}

/// A filesystem verification failure attributed to the OID whose cache
/// object could not be inspected.
#[derive(Debug, thiserror::Error)]
#[error("could not verify cached object {oid}")]
pub struct CacheVerificationFailure {
    oid: Oid,
    #[source]
    source: CacheError,
}

impl CacheVerificationFailure {
    #[must_use]
    pub const fn oid(&self) -> Oid {
        self.oid
    }

    #[must_use]
    pub fn into_source(self) -> CacheError {
        self.source
    }
}

impl PreparedCacheVerification {
    /// Runs only filesystem stat/proof-match/hash work.
    ///
    /// This deliberately consumes an owned preparation so callers can move
    /// it directly into `spawn_blocking` without capturing `CacheClient`.
    pub fn verify(
        self,
    ) -> std::result::Result<CompletedCacheVerification, CacheVerificationFailure> {
        let results = self
            .pending
            .into_par_iter()
            .map(
                |prepared| -> std::result::Result<
                    (Oid, CacheObservation, Option<CachePublication>),
                    CacheVerificationFailure,
                > {
                    let (observation, delta) = crate::cache::proof::verify_object_path_fs(
                        &prepared.path,
                        &prepared.oid,
                        prepared.prior.as_ref(),
                    )
                    .map_err(|source| CacheVerificationFailure {
                        oid: prepared.oid,
                        source: source.into(),
                    })?;
                    Ok((prepared.oid, observation, delta))
                },
            )
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(CompletedCacheVerification {
            oids: self.oids,
            known: self.known,
            results,
        })
    }
}

impl CacheClient {
    /// Open an isolated cache client for tests. Production operations reuse
    /// the client opened through their cache session. An unavailable proof
    /// index falls back to hashing.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn open(objects_dir: PathBuf) -> Self {
        Self::open_shared(Arc::new(crate::cache::root::CacheRootInner::new(
            objects_dir,
            None,
        )))
    }

    pub(crate) fn open_shared(root: Arc<crate::cache::root::CacheRootInner>) -> Self {
        let index = match root.prepare_existing_directory() {
            Ok(Some(directory)) => CacheState::open_prepared(&directory),
            Ok(None) | Err(_) => CacheState::disabled(),
        };
        Self {
            root,
            index,
            memo: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// The shared proof index, for test-only fault injection
    /// (`break_for_test`) that simulates the proof DB failing mid-session.
    /// Production callers publishing proof deltas go through
    /// [`Self::apply_publications`] instead, which also invalidates this
    /// session's verification memo for the affected oids.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn break_database_for_test(&self) {
        self.index.break_for_test();
    }

    pub(crate) fn prepare_write(&self) -> Result<()> {
        self.root.prepare_write()
    }

    /// The directory this cache's objects are fanned out under -- used by
    /// working-tree validation to compute the exact on-disk path a
    /// `cache.materialization_strategy = symlink` materialization must point at (see
    /// `engine::workspace::sync::plan::file_status`), without giving broader
    /// access to cache internals.
    pub(crate) fn objects_dir(&self) -> &Path {
        &self.root.objects_dir
    }

    /// Creates a lazy, opaque handle for reading one finalized cache object.
    ///
    /// This performs no filesystem or proof-index I/O. The physical path is
    /// retained privately and shared cheaply across upload jobs; opening and
    /// inspecting the file is deferred until [`CacheObject::open`].
    pub fn object(&self, oid: &Oid) -> CacheObject {
        CacheObject {
            path: Arc::new(cache_path_oid(&self.root.objects_dir, oid)),
            verified_size: self
                .memo
                .borrow()
                .get(oid)
                .copied()
                .and_then(CacheObservation::verified_size),
        }
    }

    /// Forget any memoized [`ObjectVerification`] this session holds for
    /// `oids`: a [`CachePublication`] fundamentally
    /// changes ground truth for that oid (its bytes were just replaced,
    /// e.g. by repair), so an operation-long memo entry recorded *before*
    /// that change would otherwise keep answering with the stale verdict
    /// for the rest of the operation. Called from
    /// [`Self::apply_publications`] rather than requiring every applying
    /// phase to remember to invalidate the memo itself.
    fn forget_memo(&self, oids: impl Iterator<Item = Oid>) {
        let mut memo = self.memo.borrow_mut();
        for oid in oids {
            memo.remove(&oid);
        }
    }

    /// Apply a batch of freshly produced [`CachePublication`]s to this
    /// session's shared proof index. First forget any memoized
    /// verification status for the affected oids so a later
    /// `verify`/`verify_windows` call in the same
    /// operation (e.g. a resync immediately following a repair) always
    /// re-derives its status from the just-updated ground truth instead
    /// of an earlier, now-stale memo entry. Invalidation must happen even if
    /// proof persistence fails: the filesystem mutation has already happened.
    pub fn apply_publications(&self, receipts: &[CachePublication]) -> Result<()> {
        self.forget_memo(receipts.iter().map(CachePublication::oid));
        self.index.apply_many(receipts)?;
        Ok(())
    }

    /// Remove proof rows for objects that have been swept from disk.
    pub fn remove_proofs(&self, oids: &[Oid]) -> Result<()> {
        self.forget_memo(oids.iter().copied());
        self.index.remove_many(oids)?;
        Ok(())
    }

    /// Verify one object outside a multi-object loop, reusing an
    /// operation-local memoized result if this oid was already verified
    /// in this session so composing helpers never re-verify the same oid.
    pub fn verify(&self, oid: &Oid) -> Result<ObjectVerification> {
        if let Some(observation) = self.memo.borrow().get(oid).copied() {
            #[cfg(any(test, feature = "test-support"))]
            crate::cache::proof::test_support::record_memo_hit();
            return Ok(observation.status());
        }
        #[cfg(any(test, feature = "test-support"))]
        crate::cache::proof::test_support::record_fs_verification();
        let prior = self.index.lookup(oid).ok().flatten();
        let (observation, delta) =
            crate::cache::proof::verify_object_fs(&self.root.objects_dir, oid, prior.as_ref())?;
        if let Some(delta) = delta {
            // Best-effort: `observation` above is already fully determined from
            // the just-completed filesystem verification: a failure to
            // persist this proof only costs a later call an extra hash,
            // never this call's correctness.
            let _ = self.index.apply_many(std::slice::from_ref(&delta));
        }
        let mut memo = self.memo.borrow_mut();
        memo.insert(*oid, observation);
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_memo_size(memo.len());
        Ok(observation.status())
    }

    /// Prepare one bounded verification batch on the coordinator thread.
    ///
    /// Memo inspection and the set-based proof lookup happen here. A proof
    /// database failure degrades to absent priors so the worker hashes the
    /// affected objects instead.
    pub fn prepare_verification(&self, oids: &[Oid]) -> PreparedCacheVerification {
        let mut known = std::collections::HashMap::new();
        let mut pending_oids = Vec::new();
        {
            let memo = self.memo.borrow();
            let mut seen = std::collections::HashSet::new();
            for oid in oids {
                if let Some(observation) = memo.get(oid).copied() {
                    known.insert(*oid, observation.status());
                } else if seen.insert(*oid) {
                    pending_oids.push(*oid);
                }
            }
        }

        let mut priors = if pending_oids.is_empty() {
            std::collections::HashMap::new()
        } else {
            self.index.exact_many(&pending_oids).unwrap_or_default()
        };
        #[cfg(any(test, feature = "test-support"))]
        for _ in &pending_oids {
            crate::cache::proof::test_support::record_fs_verification();
        }
        let pending = pending_oids
            .into_iter()
            .map(|oid| PreparedCacheObjectVerification {
                oid,
                // Pending OIDs have no memoized observation. Verification only
                // needs an owned path, not a shared reader handle or another lookup.
                path: cache_path_oid(&self.root.objects_dir, &oid),
                prior: priors.remove(&oid),
            })
            .collect();

        PreparedCacheVerification {
            oids: oids.to_vec(),
            known,
            pending,
        }
    }

    /// Commit a completed verification batch on the coordinator thread.
    ///
    /// Fresh statuses enter the operation memo before proof deltas are
    /// persisted. Proof persistence remains best-effort because it is only
    /// an accelerator for later verification.
    pub fn commit_verification(
        &self,
        completed: CompletedCacheVerification,
    ) -> Vec<ObjectVerification> {
        let CompletedCacheVerification {
            oids,
            known,
            results,
        } = completed;
        let mut deltas = Vec::new();
        let mut memo = self.memo.borrow_mut();
        for (oid, observation, delta) in results {
            memo.insert(oid, observation);
            if let Some(delta) = delta {
                deltas.push(delta);
            }
        }
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_memo_size(memo.len());
        // Fresh observations already live in the memo; do not build a second
        // OID-keyed map containing their projected statuses. `known` preserves
        // only the memo hits captured during preparation.
        let statuses = oids
            .iter()
            .map(|oid| {
                known
                    .get(oid)
                    .copied()
                    .or_else(|| memo.get(oid).copied().map(CacheObservation::status))
                    .unwrap_or(ObjectVerification::Missing)
            })
            .collect();
        drop(memo);
        let _ = self.index.apply_many(&deltas);
        statuses
    }

    /// Verify `oids` in bounded windows, invoking `on_window` with each
    /// window's oids and aligned statuses immediately after that window's
    /// set-based proof lookup, parallel filesystem verification, and
    /// set-based proof persist -- before the next window is touched. This
    /// bounds each status buffer, but the session memo retains results for
    /// previously verified objects. Callers with globally deduplicated OIDs
    /// can use [`Self::verify_windows_unmemoized`] to avoid that retained memo.
    pub fn verify_windows<E: From<CacheError>>(
        &self,
        oids: &[Oid],
        mut on_window: impl FnMut(&[Oid], &[ObjectVerification]) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        for window in oids.chunks(verify_window_size()) {
            let prepared = self.prepare_verification(window);
            let completed = prepared
                .verify()
                .map_err(CacheVerificationFailure::into_source)?;
            let statuses = self.commit_verification(completed);
            on_window(window, &statuses)?;
        }
        Ok(())
    }

    /// Like [`verify_windows`](Self::verify_windows), but for callers
    /// whose `oids` are already globally deduplicated for the whole
    /// operation (e.g. `push`/`fetch`'s selected-object set): each oid
    /// can only ever appear once across the whole `oids` slice, so this
    /// takes a genuinely unmemoized path rather than routing through the
    /// generic memoized implementation:
    ///
    /// - no per-window `HashSet` dedup pass (the contract already
    ///   guarantees one appearance per oid);
    /// - bulk proof lookup and parallel filesystem verification write
    ///   directly into a window-aligned status `Vec`, with no detour
    ///   through the session memo;
    /// - proof deltas are persisted once per window, exactly as before;
    /// - statuses go straight to `on_window`, never touching
    ///   `self.memo` at all (so there is nothing to insert-then-remove).
    ///
    /// Do not use this for callers that rely on memo reuse across
    /// separate verification calls on the same oid within one
    /// operation (e.g. sync's nested resolution helpers): those still
    /// need [`Self::verify_windows`] or [`Self::verify`].
    ///
    /// # Panics (debug only)
    ///
    /// Debug builds assert that `oids` contains no duplicates, since a
    /// duplicate would silently be verified twice (once per occurrence)
    /// instead of being caught by the caller's own dedup contract.
    pub fn verify_windows_unmemoized<E: From<CacheError>>(
        &self,
        oids: &[Oid],
        mut on_window: impl FnMut(&[Oid], &[ObjectVerification]) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        // Debug-only dedup check spans the *whole* `oids` slice, not just
        // one window at a time: a duplicate more than one window apart
        // (e.g. oid 0 and oid `VERIFY_WINDOW + 5`) would silently pass a
        // per-window-scoped check even though it still violates the
        // documented contract. Built once here so the check runs exactly
        // once, and compiled out entirely in release builds (`debug_assert!`
        // only evaluates its condition in debug/test builds), keeping the
        // production path allocation-free with respect to dedup checking.
        #[cfg(debug_assertions)]
        let mut seen: std::collections::HashSet<Oid> =
            std::collections::HashSet::with_capacity(oids.len());

        for window in oids.chunks(verify_window_size()) {
            if window.is_empty() {
                continue;
            }
            #[cfg(debug_assertions)]
            {
                let unique = window.iter().all(|oid| seen.insert(*oid));
                debug_assert!(
                    unique,
                    "verify_windows_unmemoized requires globally deduplicated oids"
                );
            }

            // A failed bulk proof lookup degrades to "no known priors"
            // for this window rather than aborting verification, same
            // reasoning as `verify_windows` above.
            let priors = self.index.exact_many(window).unwrap_or_default();
            #[cfg(any(test, feature = "test-support"))]
            for _ in window {
                crate::cache::proof::test_support::record_fs_verification();
            }
            // Filesystem verification runs in parallel and writes
            // directly into a window-aligned result vector -- never
            // through `self.memo`, since this caller's oids each appear
            // in at most one window and so gain nothing from memoizing.
            let objects_dir: &Path = &self.root.objects_dir;
            let priors = &priors;
            let results: Vec<(ObjectVerification, Option<CachePublication>)> = window
                .par_iter()
                .map(
                    |oid| -> Result<(ObjectVerification, Option<CachePublication>)> {
                        let (observation, delta) = crate::cache::proof::verify_object_fs(
                            objects_dir,
                            oid,
                            priors.get(oid),
                        )?;
                        Ok((observation.status(), delta))
                    },
                )
                .collect::<Result<Vec<_>>>()?;

            let mut statuses: Vec<ObjectVerification> = Vec::with_capacity(results.len());
            let mut deltas: Vec<CachePublication> = Vec::new();
            for (status, delta) in results {
                statuses.push(status);
                if let Some(delta) = delta {
                    deltas.push(delta);
                }
            }
            // Best-effort, same reasoning as `verify` above.
            let _ = self.index.apply_many(&deltas);

            on_window(window, &statuses)?;
        }
        Ok(())
    }

    /// Verify exactly `oids`, returning a status per input position (so a
    /// caller can zip results back against its own list). Duplicate oids
    /// in one request -- and any already verified earlier this session --
    /// are verified at most once; the rest are processed in bounded
    /// windows, each doing one set-based proof lookup, a parallel
    /// filesystem verification pass, and one set-based proof persist,
    /// before the next window is touched.
    ///
    /// This collects a whole-input result `Vec`, so it suits callers that
    /// already work over a small/bounded chunk (e.g. one dirty-row
    /// window). A caller that must stay bounded across a huge selection
    /// (e.g. `push`/`fetch`) should drive [`verify_windows`](Self::verify_windows)
    /// directly instead, consuming each window before the next is
    /// verified.
    #[cfg(any(test, feature = "test-support"))]
    pub fn verify_many(&self, oids: &[Oid]) -> Result<Vec<ObjectVerification>> {
        let mut result = Vec::with_capacity(oids.len());
        self.verify_windows(oids, |_window, statuses| -> Result<()> {
            result.extend_from_slice(statuses);
            Ok(())
        })?;
        Ok(result)
    }
}

/// Content hash and size returned after ingesting one file into the local
/// cache. This describes only the cached object, not any `gat.lock` row or
/// other sidecar metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingested {
    pub oid: Oid,
    pub size: u64,
}

/// Result of ingesting bytes that must replace one specific cache object.
///
/// A mismatch never publishes the downloaded bytes under either the expected
/// or actual OID, so the existing destination remains available for a later
/// repair attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedIngest {
    Published {
        publication: Option<CachePublication>,
    },
    HashMismatch {
        actual: Oid,
    },
}

/// Stream `reader` into the local cache, hashing as it goes. Dedups by
/// content: if the object already exists the temp file is just discarded.
///
/// Hashing uses BLAKE3 (not SHA-256): dramatically faster, especially for
/// large files, without needing a separate large/small-file code path.
///
/// This single-pass streaming approach is the only option for non-local
/// sources (e.g. `fetch` reading from a remote), and is fine for small
/// local files too. For large *local* files, prefer `ingest_file`, whose
/// large-file-specific implementations reduce wall-clock time in practice;
/// This path is optimized for readers rather than large local files.
#[cfg(any(test, feature = "test-support"))]
pub fn ingest<R: std::io::Read>(objects_dir: &Path, reader: R) -> Result<Ingested> {
    let (tmp, oid, size) = ingest_to_tmp(objects_dir, reader, None)?;
    finalize_tmp(objects_dir, tmp, oid, size)
}

/// The DB-free counterpart of single-object `ingest` for parallel/multi-object
/// callers (e.g. `fetch`): streams `reader` into the cache and publishes
/// it, but instead of opening its own `CacheState` to persist a proof,
/// returns the [`CachePublication`] for the operation thread to apply later in
/// one bounded set-based batch.
pub fn ingest_delta<R: std::io::Read>(
    objects_dir: &Path,
    reader: R,
) -> Result<(Ingested, Option<CachePublication>)> {
    let (tmp, oid, size) = ingest_to_tmp(objects_dir, reader, None)?;
    publish_tmp(objects_dir, tmp, oid, size)
}

/// Same as [`ingest_delta`], but for a caller that already knows `reader`'s
/// exact byte count (e.g. a remote object whose size the remote already
/// reported via `stat`): forwards `Some(size)` through to [`ingest_to_tmp`]
/// so the scratch buffer never over-allocates for a small object.
pub fn ingest_sized_delta<R: std::io::Read>(
    objects_dir: &Path,
    reader: R,
    size_hint: u64,
) -> Result<(Ingested, Option<CachePublication>)> {
    let (tmp, oid, size) = ingest_to_tmp(objects_dir, reader, Some(size_hint))?;
    publish_tmp(objects_dir, tmp, oid, size)
}

/// DB-free repair ingest for an object whose expected identity is known.
///
/// The source is streamed and hashed once into a durable temp file. Matching
/// bytes atomically replace the expected object's existing destination;
/// mismatching bytes are discarded with the temp file and never published.
pub fn ingest_expected_delta<R: std::io::Read>(
    objects_dir: &Path,
    expected: Oid,
    reader: R,
) -> Result<ExpectedIngest> {
    let (tmp, actual, size) = ingest_to_tmp(objects_dir, reader, None)?;
    if actual != expected {
        return Ok(ExpectedIngest::HashMismatch { actual });
    }
    let (_, publication) = publish_tmp(objects_dir, tmp, expected, size)?;
    Ok(ExpectedIngest::Published { publication })
}

/// Stream `reader` into a fully-written, flushed cache temp file, hashing
/// as it goes, and return that temp file with its content oid and size --
/// the shared body of single-object `ingest` and [`ingest_delta`]/
/// [`ingest_sized_delta`], stopping just short of publishing so both the
/// DB-opening and DB-free publication paths can reuse it. `size_hint`, when
/// known up front, is forwarded to [`crate::remote::with_stream_buffer`] so
/// a small object never allocates a scratch buffer larger than itself;
/// unknown-size callers pass `None` and keep assuming the full
/// [`crate::remote::STREAM_BUFFER_SIZE`] scratch buffer.
fn ingest_to_tmp<R: std::io::Read>(
    objects_dir: &Path,
    mut reader: R,
    size_hint: Option<u64>,
) -> Result<(NamedTempFile, Oid, u64)> {
    ensure_cache_directory(objects_dir)?;
    // `NamedTempFile` opens with `O_EXCL`-equivalent exclusive creation and
    // retries on name collision, so concurrent ingests can never clobber
    // each other's temp file; its `Drop` also removes the file on any early
    // return (error, panic, or simply falling out of scope without an
    // explicit `persist`), so a crash or `?`-propagated failure never
    // leaves an orphaned temp file behind.
    let mut tmp = create_tmp_file(objects_dir)?;
    let tmp_path = tmp.path().to_path_buf();
    let entry_unwritable = |source: std::io::Error| CacheError::EntryUnwritable {
        path: tmp_path.clone(),
        source,
    };
    let mut hasher = blake3::Hasher::new();
    let mut size = 0u64;
    crate::remote::with_stream_buffer(size_hint, |buf| -> Result<()> {
        loop {
            let n = reader
                .read(buf)
                .map_err(|source| CacheError::SourceUnreadable { source })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            tmp.write_all(&buf[..n]).map_err(entry_unwritable)?;
            size += n as u64;
        }
        Ok(())
    })?;
    tmp.flush().map_err(entry_unwritable)?;
    tmp.as_file().sync_all().map_err(entry_unwritable)?;
    #[cfg(any(test, feature = "test-support"))]
    record_sync_all();
    let oid = Oid::from_bytes(*(hasher.finalize()).as_bytes());
    Ok((tmp, oid, size))
}

/// Which large-file `ingest_file` implementation to use. Re-exported from
/// `config` because `cache.ingest_strategy` in `gat.yaml` parses directly
/// into this type; the canonical definition stays next to the rest of the
/// persisted config schema so metadata consumers do not depend on this
/// module.
pub use gat_core::config::{DEFAULT_INGEST_STRATEGY, IngestStrategy};

/// Ingest a file that already exists on the local filesystem into the
/// cache, per `strategy` (see [`IngestStrategy`]).
#[cfg(any(test, feature = "test-support"))]
pub fn ingest_file(
    objects_dir: &Path,
    path: &Path,
    strategy: IngestStrategy,
    on_progress: impl Fn(u64) + Sync,
) -> Result<Ingested> {
    let (tmp, oid, size) = ingest_file_to_tmp(objects_dir, path, strategy, on_progress)?;
    finalize_tmp(objects_dir, tmp, oid, size)
}

/// The DB-free counterpart of single-object `ingest_file` for parallel/multi-object
/// callers (e.g. `add`'s parallel ingest): copies+hashes `path` into a
/// published cache object exactly like `ingest_file`, but returns the
/// [`CachePublication`] for the operation thread to persist later in one
/// bounded set-based batch instead of opening its own `CacheState` per
/// file.
pub fn ingest_file_delta(
    objects_dir: &Path,
    path: &Path,
    strategy: IngestStrategy,
    on_progress: impl Fn(u64) + Sync,
) -> Result<(Ingested, Option<CachePublication>)> {
    let (tmp, oid, size) = ingest_file_to_tmp(objects_dir, path, strategy, on_progress)?;
    publish_tmp(objects_dir, tmp, oid, size)
}

/// Copy+hash `path` into a fully-written cache temp file per `strategy`,
/// returning that temp file with its content oid and size, stopping just
/// short of publishing -- the shared body of
/// single-object `ingest_file` and [`ingest_file_delta`].
fn ingest_file_to_tmp(
    objects_dir: &Path,
    path: &Path,
    strategy: IngestStrategy,
    on_progress: impl Fn(u64) + Sync,
) -> Result<(NamedTempFile, Oid, u64)> {
    match strategy {
        IngestStrategy::Safe => ingest_file_safe_to_tmp(objects_dir, path, on_progress),
        IngestStrategy::Hybrid => ingest_file_hybrid_to_tmp(objects_dir, path, on_progress),
        IngestStrategy::Mmap => ingest_file_mmap_to_tmp(objects_dir, path, on_progress),
    }
}

/// [`IngestStrategy::Safe`] (the default): copy `path` into the cache and
/// then BLAKE3-hash the *copy* (not the source) via
/// [`update_mmap_rayon`](blake3::Hasher::update_mmap_rayon).
///
/// Hashing the freshly-written temp file rather than racing an independent
/// read of the source guarantees the invariant a content-addressed cache
/// depends on: a published object's bytes always match its own oid. If
/// the source were hashed concurrently with (rather than after) the copy,
/// a file that's modified while `gat add` is running could be copied and
/// hashed in different states, silently publishing an object under the
/// wrong oid. Hashing the copy instead means the oid always describes
/// exactly the bytes that get published, regardless of what happens to
/// the source afterwards. `Hybrid` and `Mmap` (both explicit opt-ins, not
/// the default) do not provide this same guarantee under concurrent
/// source modification -- see their own doc comments.
///
fn ingest_file_safe_to_tmp(
    objects_dir: &Path,
    path: &Path,
    on_progress: impl Fn(u64) + Sync,
) -> Result<(NamedTempFile, Oid, u64)> {
    ensure_cache_directory(objects_dir)?;
    let mut tmp = create_tmp_file(objects_dir)?;
    let tmp_path = tmp.path().to_path_buf();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Acquire) {
                let len = std::fs::metadata(&tmp_path).map_or(0, |m| m.len());
                on_progress(len);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        let result = std::fs::copy(path, &tmp_path);
        done.store(true, Ordering::Release);
        result
    })
    .map_err(|source| CacheError::PathUnreadable {
        path: path.to_path_buf(),
        source,
    })?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| CacheError::EntryUnwritable {
            path: tmp_path.clone(),
            source,
        })?;
    #[cfg(any(test, feature = "test-support"))]
    record_sync_all();
    let size = tmp
        .as_file()
        .metadata()
        .map_err(|source| CacheError::EntryUnwritable {
            path: tmp_path.clone(),
            source,
        })?
        .len();
    on_progress(size);
    let oid = hash_file_oid(tmp.path())?;
    Ok((tmp, oid, size))
}
/// Copies and hashes the source concurrently, then checks whether the
/// source's size/mtime changed during the operation. Unchanged means the
/// concurrent hash-of-source is trusted; changed (or unreadable) means it
/// isn't, so the copy just made is safely re-hashed instead, same as
/// `Safe`.
///
/// The mtime/size check is a heuristic, not a proof: a source rewritten
/// with identical size within one mtime tick can slip past it, in which
/// case the trusted "hash of source" and the published copy can disagree
/// -- this strategy does *not* guarantee `Safe`'s invariant that the
/// published bytes always match their own oid under concurrent source
/// modification. It catches the vastly common "someone edited/replaced
/// the file" case cheaply, but choosing `Hybrid` over `Safe` is an
/// explicit trade of that narrow correctness gap for speed.
fn ingest_file_hybrid_to_tmp(
    objects_dir: &Path,
    path: &Path,
    on_progress: impl Fn(u64) + Sync,
) -> Result<(NamedTempFile, Oid, u64)> {
    ensure_cache_directory(objects_dir)?;
    let mut tmp = create_tmp_file(objects_dir)?;
    let tmp_path = tmp.path().to_path_buf();
    let before = source_fingerprint(path);
    let done = AtomicBool::new(false);
    let (copy_result, hash_result): (std::io::Result<u64>, std::io::Result<blake3::Hash>) =
        std::thread::scope(|s| {
            s.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    let len = std::fs::metadata(&tmp_path).map_or(0, |m| m.len());
                    on_progress(len);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            });
            rayon::join(
                || {
                    let result = std::fs::copy(path, &tmp_path);
                    done.store(true, Ordering::Release);
                    result
                },
                || {
                    let mut hasher = blake3::Hasher::new();
                    hasher.update_mmap_rayon(path)?;
                    Ok(hasher.finalize())
                },
            )
        });
    let size = copy_result.map_err(|source| CacheError::PathUnreadable {
        path: path.to_path_buf(),
        source,
    })?;
    on_progress(size);
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| CacheError::EntryUnwritable {
            path: tmp_path.clone(),
            source,
        })?;
    #[cfg(any(test, feature = "test-support"))]
    record_sync_all();
    let after = source_fingerprint(path);
    let oid = if before.is_some() && before == after {
        Oid::from_bytes(
            *hash_result
                .map_err(|source| CacheError::PathUnreadable {
                    path: path.to_path_buf(),
                    source,
                })?
                .as_bytes(),
        )
    } else {
        hash_file_oid(tmp.path())?
    };
    Ok((tmp, oid, size))
}

/// A source file's size + mtime, cheap enough to compare before and after
/// a concurrent copy+hash to detect (heuristically) whether it changed
/// mid-operation. `None` if the metadata couldn't be read at all (treated
/// as "changed", i.e. not safe to trust).
pub fn source_fingerprint(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let m = std::fs::metadata(path).ok()?;
    let modified = m.modified().ok()?;
    Some((m.len(), modified))
}

/// [`IngestStrategy::Mmap`] (explicit opt-in, not the default): map the
/// source once and have the copy (a plain `write_all`) and the hash
/// (`update_rayon` over the same slice) both read from that single
/// mapping, instead of two independent re-reads of `path` (`Safe`'s
/// sequential re-read, or `Hybrid`'s concurrent one).
///
/// This strategy's contract is stricter than "the bytes might be
/// surprising": the source file must remain stable for the lifetime of the
/// mapping/read. A file-backed mmap is not an immutable snapshot, so
/// concurrent writers, truncation, or replacing `path` while the mapping is
/// alive are outside this strategy's safety assumptions and must be
/// avoided. Choose `Safe` instead when source-file stability cannot be
/// guaranteed.
fn ingest_file_mmap_to_tmp(
    objects_dir: &Path,
    path: &Path,
    on_progress: impl Fn(u64) + Sync,
) -> Result<(NamedTempFile, Oid, u64)> {
    ensure_cache_directory(objects_dir)?;
    let mut tmp = create_tmp_file(objects_dir)?;
    let tmp_path = tmp.path().to_path_buf();
    let src = std::fs::File::open(path).map_err(|source| CacheError::PathUnreadable {
        path: path.to_path_buf(),
        source,
    })?;
    let size = src
        .metadata()
        .map_err(|source| CacheError::PathUnreadable {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if size == 0 {
        // mmap-ing a zero-length file is invalid; nothing to copy or hash.
        tmp.as_file()
            .sync_all()
            .map_err(|source| CacheError::EntryUnwritable {
                path: tmp_path.clone(),
                source,
            })?;
        #[cfg(any(test, feature = "test-support"))]
        record_sync_all();
        on_progress(0);
        let oid = Oid::from_bytes(*(blake3::hash(&[])).as_bytes());
        return Ok((tmp, oid, 0));
    }
    // SAFETY: file-backed mmap requires the mapped file to remain stable for
    // the lifetime of the mapping. Callers only reach this path by
    // explicitly choosing `IngestStrategy::Mmap`, and doing so means taking
    // responsibility for ensuring `path` is not concurrently modified,
    // truncated, or replaced while `mmap` is alive.
    let mmap =
        unsafe { memmap2::Mmap::map(&src) }.map_err(|source| CacheError::PathUnreadable {
            path: path.to_path_buf(),
            source,
        })?;
    let done = AtomicBool::new(false);
    let (write_result, hash): (std::io::Result<()>, blake3::Hash) = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Acquire) {
                let len = std::fs::metadata(&tmp_path).map_or(0, |m| m.len());
                on_progress(len);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        let joined = rayon::join(
            || tmp.as_file_mut().write_all(&mmap),
            || {
                let mut hasher = blake3::Hasher::new();
                hasher.update_rayon(&mmap);
                hasher.finalize()
            },
        );
        done.store(true, Ordering::Release);
        joined
    });
    write_result.map_err(|source| CacheError::EntryUnwritable {
        path: tmp_path.clone(),
        source,
    })?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| CacheError::EntryUnwritable {
            path: tmp_path.clone(),
            source,
        })?;
    #[cfg(any(test, feature = "test-support"))]
    record_sync_all();
    on_progress(size);
    let oid = Oid::from_bytes(*(hash).as_bytes());
    Ok((tmp, oid, size))
}

/// Publish a fully-written, flushed temp file into its content-addressed
/// home, verifying integrity before anything becomes visible under `oid`.
/// Shared by `ingest` and `ingest_file`.
///
/// `tmp` was already hashed to `oid` by the caller as part of producing
/// it, so if an object already exists at the destination this never
/// re-reads/re-hashes *that* file's bytes to decide what to do with it
/// (the cache-dedup goal is now made proof-based rather than
/// permission-based):
///
/// - if the existing object has a [`crate::cache::proof::CacheState`]
///   proof for `oid` whose stat still matches the destination's current
///   stat, it is trusted as-is -- `tmp`'s
///   `Drop` removes the temp file and the destination is never read or
///   hashed;
/// - otherwise (no pre-existing object, or one with no reusable proof --
///   e.g. bit rot repair, manual tampering, a foreign write, or simply
///   never having been proof-stamped) it is *not* trusted by permission
///   bits or filename alone, but since `tmp` already carries a
///   proven-correct `oid`, the fix is to publish `tmp` over it rather
///   than re-hash the untrusted bytes just to reach the same conclusion.
///
/// [`protect`] still runs on the published object -- it remains mutation
/// prevention, not identity proof -- and a fresh
/// proof is persisted afterward so a later `finalize_tmp`
/// or [`CacheClient::verify`] call for the
/// same `oid` can trust it without hashing. A failure to persist that
/// proof (a disabled/errored `CacheState`) never fails this call or
/// makes the just-published bytes untrusted for the *caller* who already
/// verified them -- it only costs a later redundant hash.
///
/// Publishing happens via an atomic rename
/// ([`crate::atomic::persist_with_retry`], which retries the rare
/// transient "Access is denied" Windows can return under concurrent
/// publishes to the same path), so a reader can never observe a
/// partially-written object at the final path.
#[cfg(any(test, feature = "test-support"))]
pub fn finalize_tmp(
    objects_dir: &Path,
    tmp: NamedTempFile,
    oid: Oid,
    size: u64,
) -> Result<Ingested> {
    let session = CacheState::open_for_test(objects_dir);
    finalize_tmp_in(objects_dir, tmp, oid, size, &session)
}

/// [`finalize_tmp`] over an already-open operation session instead of
/// opening its own `CacheState`. This is what lets a genuinely
/// single-object standalone `ingest`/`finalize_tmp` keep the
/// trust-existing-destination optimization (a proof lookup that avoids
/// re-publishing already-known-good bytes) without every parallel/
/// multi-object caller paying a fresh DB open per file -- those instead
/// go through the DB-free [`publish_tmp`] and batch their deltas.
#[cfg(any(test, feature = "test-support"))]
pub fn finalize_tmp_in(
    objects_dir: &Path,
    tmp: NamedTempFile,
    oid: Oid,
    size: u64,
    session: &CacheState,
) -> Result<Ingested> {
    let dest = cache_path_oid(objects_dir, &oid);

    let prior = session.lookup(&oid).ok().flatten();
    let current = observe_regular_file_no_follow(&dest);
    if let (Some(prior), Some(current)) = (prior.as_ref(), current.as_ref())
        && current.matches(prior)
    {
        // Proven: the existing destination's bytes are already known-good
        // for this oid, so `tmp`'s `Drop` removes the temp file and
        // nothing else happens -- zero hash of the existing destination.
        return Ok(Ingested { oid, size });
    }

    let (ingested, delta) = publish_tmp(objects_dir, tmp, oid, size)?;
    if let Some(delta) = delta {
        // Best-effort: a failure here never turns into a failure to
        // access these now-verified/published bytes.
        let _ = session.apply_many(std::slice::from_ref(&delta));
    }
    Ok(ingested)
}

/// The DB-free core of publication: atomically publish a fully-written,
/// flushed temp file into its content-addressed home and return the
/// [`CachePublication`] the operation thread should later persist for it, if
/// any -- performing no `SQLite` I/O itself.
///
/// `tmp` was already hashed to `oid` by the caller as part of producing
/// it, and this path does not read any prior proof (a parallel worker
/// must never touch `SQLite`), so it always publishes `tmp` over whatever
/// is at the destination rather than trying to trust it. Since `tmp`
/// already carries a proven-correct `oid`, that is a cheap atomic rename
/// of already-verified bytes, never a re-hash of the existing
/// destination -- so this remains hash-free even where the single-object
/// `finalize_tmp_in` would have trusted an existing proof and skipped
/// the rename entirely.
///
/// [`protect`] still runs on the published object -- it remains mutation
/// prevention, not identity proof. The returned delta is always an
/// `Upsert` of the proof [`crate::atomic::persist_finalized_with_proof`] minted
/// from `tmp` itself before the rename; `protect`'s mode-only change
/// afterward cannot invalidate it, so no destination stat or content
/// re-read is needed to establish it.
///
/// Publishing happens via an atomic rename
/// ([`crate::atomic::persist_finalized_with_proof`], which retries the rare
/// transient "Access is denied" Windows can return under concurrent
/// publishes to the same path), so a reader can never observe a
/// partially-written object at the final path.
pub fn publish_tmp(
    objects_dir: &Path,
    tmp: NamedTempFile,
    oid: Oid,
    size: u64,
) -> Result<(Ingested, Option<CachePublication>)> {
    let dest = cache_path_oid(objects_dir, &oid);

    ensure_cache_directory(dest.parent().unwrap())?;
    // The existing file (if any) is untrusted -- clear its read-only bit
    // first so the overwrite below can't be blocked by `protect`'s
    // permissions on platforms that check them on replace. Deliberately
    // best-effort: if `dest` doesn't exist yet there's nothing to
    // unprotect, and if it does but this fails, the persist below will
    // itself fail and surface a real error -- this is never the last
    // line of defense.
    let _ = unprotect(&dest);
    let published = crate::atomic::persist_finalized_with_proof(tmp, &dest)?;
    protect(&dest)?;

    // Any stale proof for this oid is invalid once the bytes are
    // replaced the bytes, so the delta either replaces it with a fresh
    // proof describing exactly the bytes just published, or clears it
    // outright if a proof cannot be observed. The proof was
    // already minted from the temp file itself, before the rename
    // (`persist_finalized_with_proof`), so `protect`'s mode-only change afterward
    // cannot invalidate it -- no destination stat or content re-read is
    // needed to establish it.
    let delta = Some(CachePublication::upsert(oid, published.proof));

    Ok((Ingested { oid, size }, delta))
}

/// Test-only measurement of `hash_file_oid` calls, so tests can assert a
/// stat-first code path (`gat add` reuse, validated `sync`'s hash
/// fallback) actually avoided hashing rather than merely happening to
/// produce the right answer.
///
/// A plain process-wide counter would be racy under cargo test's default
/// multi-threaded runner: unrelated, concurrently-running tests also call
/// `hash_file_oid` (via `add`/`sync`), and `rayon`'s *global* thread pool is
/// shared across every test in the process, so a naive reset-run-assert
/// window (or even a lock held for that window) can be polluted, or can
/// deadlock against the measured operation's own legitimate internal
/// parallel hashing.
///
/// Instead, [`with_exclusive_hash_file_call_count`] runs the measured
/// closure inside a *dedicated* `rayon` thread pool (never the shared
/// global one) and records exactly which threads belong to it (the
/// calling thread plus that pool's workers, discovered via
/// [`rayon::ThreadPool::broadcast`]). `hash_file_oid` only counts a call
/// when it runs on one of those registered threads, so calls made by
/// unrelated tests on other threads are provably excluded, without ever
/// blocking the measured operation's own hashing.
#[cfg(any(test, feature = "test-support"))]
static HASH_FILE_MEASUREMENT: std::sync::Mutex<
    Option<(std::collections::HashSet<std::thread::ThreadId>, u64)>,
> = std::sync::Mutex::new(None);

/// Serializes concurrent uses of [`with_exclusive_hash_file_call_count`]
/// against each other (the single `HASH_FILE_MEASUREMENT` slot only
/// supports one active measurement at a time); does not affect unrelated
/// tests' `hash_file_oid` calls at all.
#[cfg(any(test, feature = "test-support"))]
static HASH_FILE_MEASUREMENT_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Test-only measurement of durability `sync_all` calls made while
// Preparing a cache temp file for publication:
// `ingest*_to_tmp` helper above calls `sync_all` exactly once, always on
// its own calling thread (never a shared `rayon` worker -- the
// concurrent copy/hash halves of `Hybrid`/`Mmap` never touch `sync_all`
// themselves), so a plain thread-local counter is sufficient here,
// unlike `HASH_FILE_MEASUREMENT`'s cross-thread isolation.
#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static SYNC_ALL_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "test-support"))]
fn record_sync_all() {
    SYNC_ALL_CALLS.with(|c| c.set(c.get() + 1));
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_sync_all_call_count() {
    SYNC_ALL_CALLS.with(|c| c.set(0));
}

/// Durability `sync_all` calls made on the current thread by any
/// `ingest*_to_tmp` helper since the last [`reset_sync_all_call_count`].
#[cfg(any(test, feature = "test-support"))]
pub fn sync_all_call_count() -> u64 {
    SYNC_ALL_CALLS.with(std::cell::Cell::get)
}

/// Current call count for the active [`with_exclusive_hash_file_call_count`]
/// measurement. Test-only; only meaningful while called from inside that
/// closure.
#[cfg(any(test, feature = "test-support"))]
///
/// # Panics
/// Panics if the measurement mutex is poisoned.
pub fn hash_file_call_count() -> u64 {
    HASH_FILE_MEASUREMENT
        .lock()
        .unwrap()
        .as_ref()
        .map_or(0, |(_, count)| *count)
}

/// Runs `f` inside a freshly built, private `rayon` thread pool, counting
/// `hash_file_oid` calls made by `f`'s own thread(s) only. See
/// `HASH_FILE_MEASUREMENT` for why this is immune both to unrelated
/// concurrently-running tests' hashing and to deadlocking against `f`'s
/// own internal parallel hashing. `f` must perform its `rayon` parallel
/// work (if any) as normal `rayon` calls (`par_iter`, `join`, ...) -- they
/// pick up the dedicated pool automatically because `f` runs via
/// [`rayon::ThreadPool::install`]. Test-only.
#[cfg(any(test, feature = "test-support"))]
///
/// # Panics
/// Panics if the measurement mutex is poisoned or the worker pool cannot be created.
pub fn with_exclusive_hash_file_call_count<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    let pool = rayon::ThreadPoolBuilder::new().build().unwrap();
    let mut threads: std::collections::HashSet<std::thread::ThreadId> = pool
        .broadcast(|_| std::thread::current().id())
        .into_iter()
        .collect();
    threads.insert(std::thread::current().id());

    let _gate = HASH_FILE_MEASUREMENT_GATE.lock().unwrap();
    *HASH_FILE_MEASUREMENT.lock().unwrap() = Some((threads, 0));
    let result = pool.install(f);
    *HASH_FILE_MEASUREMENT.lock().unwrap() = None;
    result
}

/// BLAKE3 hash of a working-tree file, used by `sync` to check whether a
/// materialized file was modified locally before replacing/removing it.
/// Uses the same `update_mmap_rayon` hashing that `ingest_file_safe` and
/// `ingest_file_hybrid` rely on internally, instead of a separate
/// streaming pass.
/// BLAKE3 hash of a working-tree file, used by `sync` to check whether a
/// materialized file was modified locally before replacing/removing it.
/// Uses the same `update_mmap_rayon` hashing that `ingest_file_safe` and
/// `ingest_file_hybrid` rely on internally, instead of a separate
/// streaming pass. Constructs the [`Oid`] directly from the BLAKE3
/// digest, with zero hex-encoding on this path -- callers needing hex
/// text (a genuine textual boundary) use `hash_file_oid` instead.
pub fn hash_file_oid(path: &Path) -> Result<Oid> {
    #[cfg(any(test, feature = "test-support"))]
    {
        race_test_hooks::fire_before_hash(path);
        let mut guard = HASH_FILE_MEASUREMENT.lock().unwrap();
        if let Some((threads, count)) = guard.as_mut()
            && threads.contains(&std::thread::current().id())
        {
            *count += 1;
        }
    }
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap_rayon(path)
        .map_err(|source| CacheError::PathUnreadable {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(Oid::from_bytes(*hasher.finalize().as_bytes()))
}

/// Rewrite a path immediately before hashing to test coherent-observation
/// failures when an external writer races the read.
#[cfg(any(test, feature = "test-support"))]
pub mod race_test_hooks {
    use std::path::Path;
    use std::sync::Mutex;

    type Hook = Box<dyn FnMut(&Path) + Send>;

    // Process-wide (not thread-local): `hash_file` may run on any Rayon
    // worker thread, not necessarily the test's own thread.
    static BEFORE_HASH: Mutex<Option<Hook>> = Mutex::new(None);

    /// Install a hook that runs immediately before `hash_file` reads
    /// `path`. Tests must pair this with [`clear`] (a guard is
    /// recommended) so the hook never leaks into an unrelated test
    /// running later in the same process. Because this hook is
    /// process-wide, callers must filter on the exact path they expect
    /// inside their closure.
    ///
    /// # Panics
    /// Panics if the test hook mutex is poisoned.
    pub fn set(hook: impl FnMut(&Path) + Send + 'static) {
        *BEFORE_HASH.lock().unwrap() = Some(Box::new(hook));
    }

    /// Remove any installed hook.
    ///
    /// # Panics
    /// Panics if the test hook mutex is poisoned.
    pub fn clear() {
        *BEFORE_HASH.lock().unwrap() = None;
    }

    pub(crate) fn fire_before_hash(path: &Path) {
        if let Some(hook) = BEFORE_HASH.lock().unwrap().as_mut() {
            hook(path);
        }
    }
}

/// Make a cache object read-only. Cache entries are shared by content and may
/// also be materialized through sharing modes like hardlinks, so nothing
/// should ever write through them in place.
#[cfg(unix)]
pub fn protect(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444)).map_err(|source| {
        CacheError::EntryUnwritable {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

#[cfg(not(unix))]
pub fn protect(_path: &Path) -> Result<()> {
    Ok(())
}

/// Undo `protect`'s read-only bit on a materialized working-tree copy.
/// Only the cache entry should stay locked; files checked out into the
/// working tree are ordinary, writable files.
#[cfg(unix)]
pub fn unprotect(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).map_err(|source| {
        CacheError::EntryUnwritable {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(())
}

#[cfg(not(unix))]
pub fn unprotect(_path: &Path) -> Result<()> {
    Ok(())
}

/// Materialize a cache object into the working tree by trying each mode in
/// `modes`, in order, until one succeeds: [`MaterializationMode::Reflink`]
/// (copy-on-write clone — fast, space-efficient, and safe from cache
/// corruption since the destination is its own copy-on-write inode),
/// [`MaterializationMode::Hardlink`] (shares inode/bytes with the cache — free, but
/// requires the cache and working tree on the same filesystem, and the
/// file stays read-only since the cache entry is protected),
/// [`MaterializationMode::Symlink`] (works across filesystems, but exposes the cache
/// object path through the link), or [`MaterializationMode::Copy`] (always works,
/// but duplicates the data). A mode that isn't supported here (wrong
/// filesystem, no `CoW` support, ...) falls through to the next one in the
/// list; if every mode fails, returns one error combining each mode's
/// failure so the user can see exactly why (and how to fix `cache.materialization_strategy`)
/// instead of only the last attempt's message.
pub fn materialize(obj: &Path, dest: &Path, modes: &MaterializationStrategy) -> Result<()> {
    let modes = modes.modes();
    let mut attempts = Vec::new();
    for mode in modes {
        match materialize_one(obj, dest, *mode) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // A partially created file from a failed attempt would make
                // the next mode's link/copy call fail with "already exists"
                // instead of actually being tried.
                let _ = std::fs::remove_file(dest);
                attempts.push((*mode, Box::new(e)));
            }
        }
    }
    Err(CacheError::MaterializationFailed {
        dest: dest.to_path_buf(),
        attempts,
    })
}

pub fn materialize_one(obj: &Path, dest: &Path, mode: MaterializationMode) -> Result<()> {
    match mode {
        MaterializationMode::Reflink => reflink_writable(obj, dest),
        MaterializationMode::Hardlink => {
            std::fs::hard_link(obj, dest).map_err(|source| CacheError::EntryUnwritable {
                path: dest.to_path_buf(),
                source,
            })
        }
        MaterializationMode::Symlink => symlink(obj, dest),
        MaterializationMode::Copy => copy_writable(obj, dest),
    }
}

pub fn copy_writable(obj: &Path, dest: &Path) -> Result<()> {
    std::fs::copy(obj, dest).map_err(|source| CacheError::EntryUnreadable {
        path: obj.to_path_buf(),
        source,
    })?;
    unprotect(dest)
}

/// A copy-on-write clone: same bytes as `copy_writable` but shares storage
/// with `obj` until either side is written, so it's as cheap as a hardlink
/// without the same-filesystem requirement or read-only sharing risk. The
/// clone is its own inode, so (unlike a hardlink) it needs the same
/// unprotect as a plain copy.
pub fn reflink_writable(obj: &Path, dest: &Path) -> Result<()> {
    reflink_copy::reflink(obj, dest).map_err(|source| CacheError::EntryUnreadable {
        path: obj.to_path_buf(),
        source,
    })?;
    unprotect(dest)
}

#[cfg(unix)]
pub fn symlink(obj: &Path, dest: &Path) -> Result<()> {
    std::os::unix::fs::symlink(obj, dest).map_err(|source| CacheError::EntryUnreadable {
        path: obj.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(not(unix))]
pub fn symlink(obj: &Path, dest: &Path) -> Result<()> {
    copy_writable(obj, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    #[test]
    fn opaque_cache_object_opens_with_exact_size_and_bytes() {
        fn assert_send_static<T: Send + 'static>() {}

        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let payload = b"opaque upload source";
        let ingested = ingest(&objects_dir, Cursor::new(payload)).unwrap();
        let cache = CacheClient::open(objects_dir);

        let mut reader = cache.object(&ingested.oid).open().unwrap();
        assert_send_static::<CacheObject>();
        assert_send_static::<CacheObjectReader>();
        assert_eq!(reader.size(), payload.len() as u64);
        let mut actual = Vec::new();
        reader.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, payload);
    }

    #[test]
    fn opaque_cache_object_reports_missing_open_without_exposing_its_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = CacheClient::open(tmp.path().join("objects"));
        let oid = Oid::from_bytes([7; 32]);

        let error = cache.object(&oid).open().unwrap_err();
        assert_eq!(error.stage(), CacheObjectOpenStage::Open);
        assert_eq!(error.io_kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn cache_writer_is_worker_safe_and_never_opens_the_proof_database() {
        fn assert_send_sync_static<T: Send + Sync + 'static>() {}

        let tmp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let objects_dir = tmp.path().join("objects");
        let root = layout.resolve_cache_root(Some(
            &gat_core::cache_location::CacheLocation::try_from_path(std::path::PathBuf::from(
                objects_dir.as_os_str(),
            ))
            .expect("nonempty fixture cache path"),
        ));
        let before = crate::cache::proof::test_support::snapshot().cache_db_opens;
        let writer = root.writer();

        let (_, publication) = writer.ingest(Cursor::new(b"worker payload")).unwrap();

        assert_send_sync_static::<CacheWriter>();
        assert_eq!(
            crate::cache::proof::test_support::snapshot().cache_db_opens,
            before
        );
        assert_eq!(
            format!("{:?}", publication.expect("new object has a proof receipt")),
            "CachePublication(..)"
        );
    }

    #[test]
    fn create_tmp_file_uses_the_plain_tmp_prefix_with_no_ownership_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let file = create_tmp_file(tmp.path()).unwrap();
        let name = file.path().file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(TEMP_PREFIX));
        assert!(
            !name.contains(&std::process::id().to_string()),
            "ordinary ingest temp names must not encode the pid: {name}"
        );
    }

    #[test]
    fn cache_path_is_objects_dir_joined_with_the_blake3_namespaced_fan_out() {
        // hygiene-ok: pure path-joining logic exercised with a stand-in string; no filesystem access happens at this path.
        let dir = Path::new("/tmp/objects");
        let hex = "0123456789abcdef".repeat(4);
        let oid = Oid::from_hex(&hex).unwrap();
        gat_core::oid::test_support::take_to_hex_calls();
        let path = cache_path_oid(dir, &oid);
        assert_eq!(path, dir.join(format!("blake3/01/23/{hex}")));
        assert_eq!(
            gat_core::oid::test_support::take_to_hex_calls(),
            0,
            "typed cache paths must encode directly without allocating an OID hex string"
        );
    }

    #[test]
    fn object_namespace_dir_is_objects_dir_joined_with_blake3() {
        // hygiene-ok: pure path-joining logic exercised with a stand-in string; no filesystem access happens at this path.
        let dir = Path::new("/tmp/objects");
        assert_eq!(object_namespace_dir(dir), dir.join("blake3"));
    }

    #[test]
    fn ingested_objects_never_land_at_the_old_root_level_fan_out_path() {
        // Before the blake3/ namespace, a finalized object lived directly
        // at `<objects_dir>/xx/yy/oid`. That bare path must never be used
        // any more -- only `<objects_dir>/blake3/xx/yy/oid`.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let pointer = ingest(&objects_dir, Cursor::new(b"hello world")).unwrap();
        let key = crate::cache::layout::object_key_oid(&pointer.oid);
        let legacy_path = objects_dir.join(
            key.strip_prefix(crate::cache::layout::OBJECT_HASH_NAMESPACE)
                .and_then(|path| path.strip_prefix('/'))
                .unwrap(),
        );
        assert!(
            !legacy_path.exists(),
            "no object should ever be written to the pre-namespace root-level fan-out path"
        );
        assert!(cache_path_oid(&objects_dir, &pointer.oid).is_file());
    }

    #[test]
    fn cache_sqlite3_lives_at_the_objects_dir_root_not_under_blake3() {
        use crate::cache::proof::CacheState;

        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        // Opening the proof database creates cache.sqlite3 lazily; force
        // that by opening it directly.
        let _cache_state = CacheState::open_for_test(&objects_dir);
        assert!(
            objects_dir.join("cache.sqlite3").is_file(),
            "cache.sqlite3 must live at the objects_dir root"
        );
        assert!(
            !object_namespace_dir(&objects_dir)
                .join("cache.sqlite3")
                .exists(),
            "cache.sqlite3 must never be created under the blake3/ object namespace"
        );
    }

    #[test]
    fn has_object_false_then_true_after_ingest() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let pointer = ingest(&objects_dir, Cursor::new(b"hello world")).unwrap();
        assert!(has_object_oid(&objects_dir, &pointer.oid));
        assert!(!has_object_oid(
            &objects_dir,
            &Oid::from_hex(&"0".repeat(64)).unwrap()
        ));
    }

    #[test]
    fn ingest_hashes_and_sizes_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = b"hello world";
        let pointer = ingest(&objects_dir, Cursor::new(content)).unwrap();
        let expected_oid = Oid::from_bytes(*(blake3::hash(content)).as_bytes());
        assert_eq!(pointer.oid, expected_oid);
        assert_eq!(pointer.size, content.len() as u64);
        let stored = std::fs::read(cache_path_oid(&objects_dir, &pointer.oid)).unwrap();
        assert_eq!(stored, content);
    }

    #[test]
    fn ingest_streams_correctly_across_the_shared_stream_buffer_boundary() {
        // Content size deliberately straddles STREAM_BUFFER_SIZE (the
        // shared 1 MiB buffer `ingest_to_tmp` now uses instead of the old
        // 64 KiB one) by a non-round amount, so a wrong buffer size or an
        // off-by-one in the read loop would still surface as a hash/size
        // mismatch here.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let size = STREAM_BUFFER_SIZE * 2 + 137;
        let content = vec![0xABu8; size];
        let pointer = ingest(&objects_dir, Cursor::new(&content)).unwrap();
        let expected_oid = Oid::from_bytes(*(blake3::hash(&content)).as_bytes());
        assert_eq!(pointer.oid, expected_oid);
        assert_eq!(pointer.size, size as u64);
        let stored = std::fs::read(cache_path_oid(&objects_dir, &pointer.oid)).unwrap();
        assert_eq!(stored, content);
    }

    #[test]
    fn incremental_ingest_checks_identity_before_publication_and_discards_temps() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let root = layout.resolve_cache_root(None);
        let writer = root.writer();
        let expected = Oid::from_bytes(*blake3::hash(b"expected").as_bytes());
        let wrong = Oid::from_bytes(*blake3::hash(b"wrong").as_bytes());
        writer
            .ingest_expected(expected, Cursor::new(b"expected"))
            .unwrap();
        let mut ingest = writer.begin_ingest().unwrap();
        let scratch = ingest.tmp.path().to_path_buf();
        ingest.append(b"wrong").unwrap();
        assert!(
            matches!(ingest.finish(expected).unwrap(), ExpectedIngest::HashMismatch { actual } if actual == wrong)
        );
        assert!(!scratch.exists());
        assert!(!cache_path_oid(&writer.root.objects_dir, &wrong).exists());
        assert_eq!(
            std::fs::read(cache_path_oid(&writer.root.objects_dir, &expected)).unwrap(),
            b"expected"
        );
        let ingest = writer.begin_ingest().unwrap();
        let scratch = ingest.tmp.path().to_path_buf();
        drop(ingest);
        assert!(!scratch.exists());
    }

    #[test]
    fn incremental_ingest_cancellation_after_sync_preserves_existing_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::RepositoryLayout::at(tmp.path().to_owned()).resolve_cache_root(None);
        let writer = root.writer();
        let oid = Oid::from_bytes(*blake3::hash(b"valid").as_bytes());
        writer.ingest_expected(oid, Cursor::new(b"valid")).unwrap();
        let mut ingest = writer.begin_ingest().unwrap();
        ingest.append(b"valid").unwrap();
        let staging = ingest.tmp.path().to_owned();
        let calls = AtomicUsize::new(0);
        assert!(
            ingest
                .finish_unless_cancelled(oid, || calls.fetch_add(1, Ordering::Relaxed) == 1)
                .unwrap()
                .is_none()
        );
        assert!(!staging.exists());
        let mut existing = root.open_client().object(&oid).open().unwrap();
        assert_eq!(existing.read_small(32).unwrap(), b"valid");
    }

    #[test]
    fn verified_source_carries_size_for_cold_and_warm_proofs() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = tmp.path().join("objects");
        let ingested = ingest(&objects, Cursor::new(b"source bytes")).unwrap();
        for _ in 0..2 {
            let cache = CacheClient::open(objects.clone());
            assert_eq!(
                cache.verify(&ingested.oid).unwrap(),
                ObjectVerification::Valid
            );
            assert_eq!(cache.object(&ingested.oid).verified_size(), Some(12));
            assert_eq!(cache.object(&ingested.oid).open().unwrap().size(), 12);
        }
    }

    #[test]
    fn ingest_sized_with_a_small_known_size_matches_the_unknown_size_ingest() {
        // A small known-size download must produce exactly the same
        // stored bytes/oid as the unknown-size path -- only the scratch
        // buffer's initial allocation differs (right-sized instead of a
        // full STREAM_BUFFER_SIZE), never the observable ingest result.
        let tmp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let objects_dir = tmp.path().join("objects");
        let root = layout.resolve_cache_root(Some(
            &gat_core::cache_location::CacheLocation::try_from_path(std::path::PathBuf::from(
                objects_dir.as_os_str(),
            ))
            .expect("nonempty fixture cache path"),
        ));
        let writer = root.writer();
        let content = b"a small known-size remote object";

        let (ingested, publication) = writer
            .ingest_sized(Cursor::new(content), content.len() as u64)
            .unwrap();

        let expected_oid = Oid::from_bytes(*(blake3::hash(content)).as_bytes());
        assert_eq!(ingested.oid, expected_oid);
        assert_eq!(ingested.size, content.len() as u64);
        assert!(
            publication.is_some(),
            "a freshly ingested object must have a proof receipt to publish"
        );
        let stored = std::fs::read(cache_path_oid(&objects_dir, &ingested.oid)).unwrap();
        assert_eq!(stored, content);
    }

    #[test]
    fn ingest_with_unknown_size_still_uses_the_full_stream_buffer() {
        // The plain unknown-size `ingest`/`ingest_delta` path must keep
        // existing behavior: no size hint means the full
        // STREAM_BUFFER_SIZE scratch buffer, exactly as before this
        // change.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = b"unknown-size ingest still works exactly as before";
        let pointer = ingest(&objects_dir, Cursor::new(content)).unwrap();
        let expected_oid = Oid::from_bytes(*(blake3::hash(content)).as_bytes());
        assert_eq!(pointer.oid, expected_oid);
        assert_eq!(pointer.size, content.len() as u64);
    }

    #[test]
    fn ingest_classifies_source_read_failures_separately_from_cache_writes() {
        struct FailingReader;

        impl std::io::Read for FailingReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("source-read-sentinel"))
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let err = ingest_delta(&tmp.path().join("objects"), FailingReader).unwrap_err();
        assert!(matches!(
            err,
            CacheError::SourceUnreadable { ref source }
                if source.to_string() == "source-read-sentinel"
        ));
    }

    #[test]
    fn ingest_file_matches_streaming_ingest_and_dedups() {
        for strategy in IngestStrategy::ALL {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let content = vec![9u8; 200_000]; // big enough to exercise both copy and mmap-hash paths
            let src = tmp.path().join("src.bin");
            std::fs::write(&src, &content).unwrap();

            let via_file = ingest_file(&objects_dir, &src, strategy, |_| {}).unwrap();
            let expected_oid = Oid::from_bytes(*(blake3::hash(&content)).as_bytes());
            assert_eq!(via_file.oid, expected_oid, "strategy {strategy:?}");
            assert_eq!(via_file.size, content.len() as u64, "strategy {strategy:?}");
            let stored = std::fs::read(cache_path_oid(&objects_dir, &via_file.oid)).unwrap();
            assert_eq!(stored, content, "strategy {strategy:?}");

            // same content via the streaming path produces the identical oid
            let via_stream = ingest(&objects_dir, Cursor::new(&content)).unwrap();
            assert_eq!(via_file, via_stream, "strategy {strategy:?}");

            // re-ingesting the same file dedups (no leftover temp files)
            ingest_file(&objects_dir, &src, strategy, |_| {}).unwrap();
            let leftovers: Vec<_> = std::fs::read_dir(&objects_dir)
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("tmp-"))
                .collect();
            assert!(leftovers.is_empty(), "strategy {strategy:?}");
        }
    }

    #[test]
    fn ingest_dedups_identical_content() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let p1 = ingest(&objects_dir, Cursor::new(b"same content")).unwrap();
        let p2 = ingest(&objects_dir, Cursor::new(b"same content")).unwrap();
        assert_eq!(p1, p2);
        // no leftover temp files after dedup
        let leftovers: Vec<_> = std::fs::read_dir(&objects_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn ingest_handles_empty_content() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let pointer = ingest(&objects_dir, Cursor::new(b"")).unwrap();
        assert_eq!(pointer.size, 0);
        assert!(has_object_oid(&objects_dir, &pointer.oid));
    }

    #[test]
    fn ingest_from_concurrent_threads_is_content_addressed_correctly() {
        // Each thread ingests distinct content concurrently; every object
        // must land in the cache under its own correct oid with no
        // corruption from colliding temp filenames.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::thread::scope(|s| {
            for i in 0..16 {
                let objects_dir = &objects_dir;
                s.spawn(move || {
                    let content = format!("distinct content #{i}").into_bytes();
                    let ingested = ingest(objects_dir, Cursor::new(&content)).unwrap();
                    let stored = std::fs::read(cache_path_oid(objects_dir, &ingested.oid)).unwrap();
                    assert_eq!(stored, content);
                });
            }
        });
    }

    #[test]
    fn ingest_from_concurrent_threads_dedups_identical_content_safely() {
        // Many threads race to ingest the *same* content concurrently.
        // Whichever one wins the publish race, every thread must observe
        // the same correct oid and the final on-disk bytes must match it
        // -- no torn/partial object from two publishes overlapping.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = vec![7u8; 300_000];
        let oids: Vec<Oid> = std::thread::scope(|s| {
            #[allow(
                clippy::needless_collect,
                reason = "Spawn every worker before joining any of them so the test exercises concurrent execution"
            )]
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let objects_dir = &objects_dir;
                    let content = &content;
                    s.spawn(move || ingest(objects_dir, Cursor::new(content)).unwrap().oid)
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let expected_oid = Oid::from_bytes(*(blake3::hash(&content)).as_bytes());
        assert!(oids.iter().all(|oid| *oid == expected_oid));
        let stored = std::fs::read(cache_path_oid(&objects_dir, &expected_oid)).unwrap();
        assert_eq!(stored, content);
        // no leftover temp files after the race resolves
        let leftovers: Vec<_> = std::fs::read_dir(&objects_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn ingest_file_from_concurrent_threads_is_content_addressed_correctly() {
        // Same invariant as `ingest_from_concurrent_threads_is_content_addressed_correctly`,
        // but through the large-file `ingest_file` path (safe/hybrid/mmap),
        // whose temp-file handling and finalization differ from the
        // streaming `ingest`.
        let tmp = tempfile::tempdir().unwrap();
        let srcs: Vec<(PathBuf, Vec<u8>)> = (0..8)
            .map(|i| {
                let content: Vec<u8> = (0..150_000usize)
                    .map(|b| ((b + i * 37) % 251).to_le_bytes()[0])
                    .collect();
                let src = tmp.path().join(format!("src-{i}.bin"));
                std::fs::write(&src, &content).unwrap();
                (src, content)
            })
            .collect();
        // Strategies share only immutable sources; each starts with an empty
        // cache so all workers still exercise publication and deduplication.
        for strategy in IngestStrategy::ALL {
            let objects_dir = tmp.path().join(format!("objects-{strategy:?}"));
            std::thread::scope(|s| {
                for (src, content) in &srcs {
                    let objects_dir = &objects_dir;
                    s.spawn(move || {
                        let ingested = ingest_file(objects_dir, src, strategy, |_| {}).unwrap();
                        let expected_oid = Oid::from_bytes(*(blake3::hash(content)).as_bytes());
                        assert_eq!(ingested.oid, expected_oid, "strategy {strategy:?}");
                        let stored =
                            std::fs::read(cache_path_oid(objects_dir, &ingested.oid)).unwrap();
                        assert_eq!(stored, *content, "strategy {strategy:?}");
                    });
                }
            });
        }
    }

    #[test]
    fn ingest_file_from_concurrent_threads_dedups_identical_content_safely() {
        // Many threads race to `ingest_file` the *same* source content
        // concurrently, each through its own independently-created temp
        // file. Whichever wins the publish race, every thread must observe
        // the same correct oid and the final on-disk bytes must match it.
        let tmp = tempfile::tempdir().unwrap();
        let content: Vec<u8> = (0..250_000usize)
            .map(|i| (i % 197).to_le_bytes()[0])
            .collect();
        let src = tmp.path().join("src.bin");
        std::fs::write(&src, &content).unwrap();
        let expected_oid = Oid::from_bytes(*(blake3::hash(&content)).as_bytes());
        // Strategies share only immutable sources; each starts with an empty
        // cache so all workers still exercise publication and deduplication.
        for strategy in IngestStrategy::ALL {
            let objects_dir = tmp.path().join(format!("objects-{strategy:?}"));
            let oids: Vec<Oid> = std::thread::scope(|s| {
                #[allow(
                    clippy::needless_collect,
                    reason = "Spawn every worker before joining any of them so the test exercises concurrent execution"
                )]
                let handles: Vec<_> = (0..8)
                    .map(|_| {
                        let objects_dir = &objects_dir;
                        let src = &src;
                        s.spawn(move || {
                            ingest_file(objects_dir, src, strategy, |_| {}).unwrap().oid
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert!(
                oids.iter().all(|oid| *oid == expected_oid),
                "strategy {strategy:?}"
            );
            let stored = std::fs::read(cache_path_oid(&objects_dir, &expected_oid)).unwrap();
            assert_eq!(stored, content, "strategy {strategy:?}");
            // no leftover temp files after the race resolves
            let leftovers: Vec<_> = std::fs::read_dir(&objects_dir)
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("tmp-"))
                .collect();
            assert!(leftovers.is_empty(), "strategy {strategy:?}");
        }
    }

    #[test]
    fn ingest_repairs_corrupted_existing_object_instead_of_trusting_it() {
        // A pre-existing object at the destination path with the wrong
        // bytes (bit rot, manual tampering, ...) must never be trusted by
        // filename alone: since it isn't `protect`ed (plain `std::fs::write`
        // leaves ordinary, writable permissions), it's replaced by the
        // freshly verified temp file rather than re-hashed and kept.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = b"the real content";
        let oid = Oid::from_bytes(*(blake3::hash(content)).as_bytes());
        let dest = cache_path_oid(&objects_dir, &oid);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"corrupted garbage bytes").unwrap();

        let ingested = ingest(&objects_dir, Cursor::new(content)).unwrap();
        assert_eq!(ingested.oid, oid);
        let stored = std::fs::read(&dest).unwrap();
        assert_eq!(stored, content);
    }

    /// Proof-based dedup replaces the old read-only-permission trust
    /// heuristic: `protect`'s read-only bit is
    /// mutation prevention, never identity proof on its own. An existing
    /// object that is `protect`ed but has no reusable
    /// [`crate::cache::proof::CacheState`] proof on record for
    /// its oid (e.g. never stamped by a prior `finalize_tmp`, or bit
    /// rot/tampering after protection) is *not* trusted by permission
    /// bits alone -- it is replaced by the already-verified `tmp` without
    /// ever being hashed.
    #[test]
    fn finalize_tmp_replaces_a_protected_but_unproven_existing_object_without_rehashing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = b"the real content";
        let oid = Oid::from_bytes(*(blake3::hash(content)).as_bytes());
        let dest = cache_path_oid(&objects_dir, &oid);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"corrupted but protected bytes").unwrap();
        protect(&dest).unwrap();

        let ingested = ingest(&objects_dir, Cursor::new(content)).unwrap();
        assert_eq!(ingested.oid, oid);
        // Replaced -- `protect`'s permission bit alone proves nothing
        // about this destination's identity without a recorded proof.
        assert_eq!(std::fs::read(&dest).unwrap(), content);
    }

    /// The proof-based counterpart of the test above: an existing
    /// destination with a reusable [`crate::cache::proof::CacheState`]
    /// proof whose stat still matches is trusted as-is -- `finalize_tmp`
    /// never re-reads/re-hashes its bytes to decide, even though `tmp`
    /// was already independently verified to hash to the same `oid`.
    /// Characterized by corrupting the destination's bytes *in place*
    /// after recording its proof, while explicitly restoring its
    /// original size and mtime (so the stat proof still matches): a
    /// rehash-based repair would still catch that and overwrite it, but
    /// the proof-trust path must leave it alone.
    #[test]
    fn finalize_tmp_trusts_a_reusable_cache_proof_without_rehashing_the_destination() {
        use crate::cache::proof::CacheState;
        use crate::file_state::coherent_observation;
        use gat_core::oid::Oid;

        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = b"the real content";
        let oid = Oid::from_bytes(*(blake3::hash(content)).as_bytes());
        let dest = cache_path_oid(&objects_dir, &oid);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, content).unwrap();
        protect(&dest).unwrap();

        let observed =
            coherent_observation(&dest, || Ok::<(), crate::file_state::FileStateError>(()))
                .unwrap();
        let proof = observed.proof;
        let state = CacheState::open_for_test(&objects_dir);
        state.upsert(&oid, &proof).unwrap();

        // Corrupt the destination's bytes without changing its size or
        // mtime, so the recorded proof still matches: if `finalize_tmp`
        // ever read/hashed this file, it would detect the mismatch and
        // repair it, but the proof-based path must trust it untouched.
        let corrupted = b"corrupted bytes!";
        assert_eq!(corrupted.len(), content.len());
        unprotect(&dest).unwrap();
        let mtime = std::fs::metadata(&dest).unwrap().modified().unwrap();
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&dest).unwrap();
            f.write_all(corrupted).unwrap();
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&dest)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        protect(&dest).unwrap();

        let ingested = ingest(&objects_dir, Cursor::new(content)).unwrap();
        assert_eq!(ingested.oid, oid);
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            corrupted,
            "trusted purely from the reusable proof -- never re-read/re-hashed"
        );
    }

    /// Publishing a freshly ingested cache object performs
    /// exactly one durability `sync_all` (of the temp file, before
    /// publication). [`finalize_tmp_seeds_a_reusable_proof_with_no_extra_hash`]
    /// also proves that proof
    /// creation adds no destination content read/hash --
    /// [`crate::atomic::persist_finalized_with_proof`] mints the proof
    /// from that same already-synced temp file, so proof creation is
    /// pure metadata already in hand.
    #[test]
    fn ingest_performs_exactly_one_durability_sync() {
        use crate::cache::proof::CacheState;

        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        reset_sync_all_call_count();

        let ingested = ingest(&objects_dir, Cursor::new(b"exactly once".as_slice())).unwrap();

        assert_eq!(
            sync_all_call_count(),
            1,
            "publishing a fresh object must durably sync its temp file exactly once"
        );

        let dest = cache_path_oid(&objects_dir, &ingested.oid);
        let observed = observe_regular_file_no_follow(&dest).unwrap();
        let state = CacheState::open_for_test(&objects_dir);
        let proof = state.lookup(&ingested.oid).unwrap();
        assert_eq!(proof, Some(observed));
    }

    /// A brand-new object, published for the first time, is seeded with a
    /// reusable proof immediately -- the proof describes exactly the
    /// bytes just written, and the coherent-observation model does not
    /// needs to wait out any racy window before trusting it.
    #[test]
    fn finalize_tmp_seeds_a_reusable_proof_for_a_freshly_published_object() {
        use crate::cache::proof::CacheState;

        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = b"fresh content";
        let ingested = ingest(&objects_dir, Cursor::new(content)).unwrap();
        let oid = ingested.oid;

        let dest = cache_path_oid(&objects_dir, &ingested.oid);
        let observed = observe_regular_file_no_follow(&dest)
            .expect("the freshly published destination must still be a regular file");

        let state = CacheState::open_for_test(&objects_dir);
        let proof = state.lookup(&oid).unwrap();
        assert_eq!(
            proof,
            Some(observed),
            "a freshly published object's proof must be seeded immediately, matching \
             the destination's actual stat"
        );
    }

    /// Publication seeds a reusable proof without ever calling
    /// `hash_file_oid` on the destination.
    #[test]
    fn finalize_tmp_seeds_a_reusable_proof_with_no_extra_hash() {
        use crate::cache::proof::CacheState;
        use gat_core::oid::Oid;

        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let content = b"old enough content";
        let oid = Oid::from_bytes(*(blake3::hash(content)).as_bytes());

        let mut named = tempfile::Builder::new()
            .prefix("tmp-")
            .tempfile_in(&objects_dir)
            .unwrap();
        named.write_all(content).unwrap();
        named.as_file_mut().sync_all().unwrap();

        let ingested = with_exclusive_hash_file_call_count(|| {
            finalize_tmp(&objects_dir, named, oid, content.len() as u64)
        })
        .unwrap();
        assert_eq!(ingested.oid, oid);
        assert_eq!(hash_file_call_count(), 0);

        let state = CacheState::open_for_test(&objects_dir);
        let proof = state
            .lookup(&oid)
            .unwrap()
            .expect("a fresh publish should seed a reusable proof");
        let current =
            crate::file_state::observe_regular_file_no_follow(&cache_path_oid(&objects_dir, &oid))
                .unwrap();
        assert!(current.matches(&proof));
    }

    /// A `cache.sqlite3` that can't be opened/understood (here, something
    /// else occupying that filename) must never fail publication or make
    /// otherwise-valid, already-verified object bytes inaccessible -- it
    /// only costs a later redundant hash (matching the failure
    /// contract for `CacheState`/`CacheClient::verify`).
    #[test]
    fn finalize_tmp_survives_an_unusable_cache_database() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        // A directory where `cache.sqlite3` should be makes it unopenable
        // as a database, so `CacheState::open` degrades to disabled.
        std::fs::create_dir_all(objects_dir.join("cache.sqlite3")).unwrap();

        let content = b"still published correctly";
        let ingested = ingest(&objects_dir, Cursor::new(content)).unwrap();
        let dest = cache_path_oid(&objects_dir, &ingested.oid);
        assert_eq!(std::fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn ingest_file_repairs_corrupted_existing_object() {
        for strategy in IngestStrategy::ALL {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let content = vec![3u8; 100_000];
            let src = tmp.path().join("src.bin");
            std::fs::write(&src, &content).unwrap();
            let oid = Oid::from_bytes(*(blake3::hash(&content)).as_bytes());
            let dest = cache_path_oid(&objects_dir, &oid);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(&dest, b"not the right bytes at all").unwrap();

            let ingested = ingest_file(&objects_dir, &src, strategy, |_| {}).unwrap();
            assert_eq!(ingested.oid, oid, "strategy {strategy:?}");
            assert_eq!(
                std::fs::read(&dest).unwrap(),
                content,
                "strategy {strategy:?}"
            );
        }
    }

    #[test]
    fn expected_ingest_atomically_replaces_the_corrupted_object() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = b"repaired content";
        let expected = Oid::from_bytes(*(blake3::hash(content)).as_bytes());
        let dest = cache_path_oid(&objects_dir, &expected);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"corrupted").unwrap();

        let result = ingest_expected_delta(&objects_dir, expected, Cursor::new(content)).unwrap();

        assert!(matches!(result, ExpectedIngest::Published { .. }));
        assert_eq!(std::fs::read(dest).unwrap(), content);
    }

    #[test]
    fn expected_ingest_mismatch_preserves_the_corrupted_object() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let expected = Oid::from_bytes(*(blake3::hash(b"expected")).as_bytes());
        let actual = Oid::from_bytes(*(blake3::hash(b"unexpected")).as_bytes());
        let expected_dest = cache_path_oid(&objects_dir, &expected);
        let actual_dest = cache_path_oid(&objects_dir, &actual);
        std::fs::create_dir_all(expected_dest.parent().unwrap()).unwrap();
        std::fs::write(&expected_dest, b"corrupted").unwrap();

        let result =
            ingest_expected_delta(&objects_dir, expected, Cursor::new(b"unexpected")).unwrap();

        assert_eq!(result, ExpectedIngest::HashMismatch { actual });
        assert_eq!(std::fs::read(expected_dest).unwrap(), b"corrupted");
        assert!(!actual_dest.exists());
    }

    #[test]
    fn ingest_leaves_no_orphaned_temp_file_when_reader_errors() {
        // A reader that fails partway through must not leave a leftover
        // temp file behind -- `NamedTempFile`'s `Drop` cleans it up even
        // though `ingest` returns early via `?` before ever reaching
        // `finalize_tmp`.
        struct FailingReader {
            emitted: usize,
        }
        impl std::io::Read for FailingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.emitted > 0 {
                    return Err(std::io::Error::other("simulated crash mid-read"));
                }
                let n = 4.min(buf.len());
                buf[..n].copy_from_slice(&b"part"[..n]);
                self.emitted += 1;
                Ok(n)
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let err = ingest(&objects_dir, FailingReader { emitted: 0 });
        assert!(err.is_err());
        let leftovers: Vec<_> = std::fs::read_dir(&objects_dir)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(leftovers.is_empty(), "leftover entries: {leftovers:?}");
    }

    #[test]
    fn ingest_large_file_hashes_and_stores_correctly() {
        // Large enough to meaningfully exercise the mmap/rayon hashing
        // path in `ingest_file`, not just the small in-memory buffer path.
        // Deterministic pseudo-random-ish content (not all-same-byte) so a
        // hasher bug that only shows up on varied input would be caught.
        let content: Vec<u8> = (0..8_000_000usize)
            .map(|i| (i % 251).to_le_bytes()[0])
            .collect();
        let expected_oid = Oid::from_bytes(*(blake3::hash(&content)).as_bytes());
        for strategy in IngestStrategy::ALL {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let src = tmp.path().join("large.bin");
            std::fs::write(&src, &content).unwrap();

            let ingested = ingest_file(&objects_dir, &src, strategy, |_| {}).unwrap();
            assert_eq!(ingested.oid, expected_oid, "strategy {strategy:?}");
            assert_eq!(ingested.size, content.len() as u64, "strategy {strategy:?}");
            let stored = std::fs::read(cache_path_oid(&objects_dir, &ingested.oid)).unwrap();
            assert_eq!(stored, content, "strategy {strategy:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn ingest_leaves_cache_object_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let pointer = ingest(&objects_dir, Cursor::new(b"payload")).unwrap();
        let path = cache_path_oid(&objects_dir, &pointer.oid);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444);
    }

    #[test]
    pub fn ingest_file_hybrid_matches_hash_of_unmodified_source() {
        // The common case: nothing touches the source during the
        // concurrent copy+hash, so the optimistic hash-of-source result
        // must be trusted and match the safe hash-of-copy result exactly.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content: Vec<u8> = (0..500_000usize)
            .map(|i| (i % 199).to_le_bytes()[0])
            .collect();
        let src = tmp.path().join("src.bin");
        std::fs::write(&src, &content).unwrap();

        let ingested = ingest_file(&objects_dir, &src, IngestStrategy::Hybrid, |_| {}).unwrap();
        let expected_oid = Oid::from_bytes(*(blake3::hash(&content)).as_bytes());
        assert_eq!(ingested.oid, expected_oid);
        assert_eq!(ingested.size, content.len() as u64);
    }

    #[test]
    pub fn ingest_file_hybrid_falls_back_to_safe_hash_when_source_changes_mid_copy() {
        // If the source is rewritten between the before/after fingerprint
        // checks, the optimistic hash-of-source can't be trusted; the oid
        // published must still describe exactly what got copied, not
        // whatever the source became. Simulated here by mutating the
        // source's mtime after the fact (standing in for a real
        // concurrent writer, which can't be reliably scheduled mid-copy
        // in a deterministic test) and asserting the invariant that
        // matters: the published bytes always match their own oid.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content = vec![5u8; 400_000];
        let src = tmp.path().join("src.bin");
        std::fs::write(&src, &content).unwrap();

        let ingested = ingest_file(&objects_dir, &src, IngestStrategy::Hybrid, |_| {}).unwrap();
        let dest = cache_path_oid(&objects_dir, &ingested.oid);
        let stored = std::fs::read(&dest).unwrap();
        let actual_hash = Oid::from_bytes(*(blake3::hash(&stored)).as_bytes());
        // The published object's own bytes always hash to its own oid --
        // this must hold regardless of strategy or timing.
        assert_eq!(actual_hash, ingested.oid);
        assert_eq!(stored, content);
    }

    #[test]
    pub fn source_fingerprint_detects_size_and_mtime_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.bin");
        std::fs::write(&path, b"one").unwrap();
        let before = source_fingerprint(&path);
        assert!(before.is_some());

        // identical re-read is stable
        assert_eq!(before, source_fingerprint(&path));

        // a size change is always caught, regardless of mtime resolution
        std::fs::write(&path, b"a different length").unwrap();
        let after = source_fingerprint(&path);
        assert_ne!(before, after, "size change must be detected");

        // a missing source can never be trusted
        std::fs::remove_file(&path).unwrap();
        assert_eq!(source_fingerprint(&path), None);
    }

    #[test]
    pub fn ingest_file_mmap_matches_streaming_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let content: Vec<u8> = (0..1_000_000usize)
            .map(|i| (i % 253).to_le_bytes()[0])
            .collect();
        let src = tmp.path().join("src.bin");
        std::fs::write(&src, &content).unwrap();

        let ingested = ingest_file(&objects_dir, &src, IngestStrategy::Mmap, |_| {}).unwrap();
        let expected_oid = Oid::from_bytes(*(blake3::hash(&content)).as_bytes());
        assert_eq!(ingested.oid, expected_oid);
        assert_eq!(ingested.size, content.len() as u64);
        let stored = std::fs::read(cache_path_oid(&objects_dir, &ingested.oid)).unwrap();
        assert_eq!(stored, content);
    }

    #[test]
    pub fn ingest_file_mmap_handles_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let src = tmp.path().join("empty.bin");
        std::fs::write(&src, b"").unwrap();

        let ingested = ingest_file(&objects_dir, &src, IngestStrategy::Mmap, |_| {}).unwrap();
        assert_eq!(ingested.size, 0);
        assert_eq!(ingested.oid, Oid::from_bytes(*blake3::hash(b"").as_bytes()));
    }

    #[test]
    #[cfg(unix)]
    fn unprotect_restores_writable_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        protect(&path).unwrap();
        unprotect(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn materialize_copy_mode_produces_writable_independent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let obj = tmp.path().join("obj");
        std::fs::write(&obj, b"payload").unwrap();
        protect(&obj).unwrap();
        let dest = tmp.path().join("dest");
        materialize(&obj, &dest, &"copy".parse().unwrap()).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        std::fs::write(&dest, b"edited").unwrap(); // writable, doesn't error
        assert_eq!(std::fs::read(&obj).unwrap(), b"payload"); // cache untouched
    }

    #[test]
    #[cfg(unix)]
    fn materialize_hardlink_mode_shares_inode() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let obj = tmp.path().join("obj");
        std::fs::write(&obj, b"payload").unwrap();
        protect(&obj).unwrap();
        let dest = tmp.path().join("dest");
        materialize(&obj, &dest, &"hardlink".parse().unwrap()).unwrap();
        assert_eq!(
            std::fs::metadata(&dest).unwrap().ino(),
            std::fs::metadata(&obj).unwrap().ino()
        );
    }

    #[test]
    #[cfg(unix)]
    fn materialize_symlink_mode_links_to_object() {
        let tmp = tempfile::tempdir().unwrap();
        let obj = tmp.path().join("obj");
        std::fs::write(&obj, b"payload").unwrap();
        protect(&obj).unwrap();
        let dest = tmp.path().join("dest");
        materialize(&obj, &dest, &"symlink".parse().unwrap()).unwrap();
        assert_eq!(std::fs::read_link(&dest).unwrap(), obj);
    }

    #[test]
    fn materialize_falls_through_a_mode_list_to_the_first_mode_that_works() {
        // A multi-mode preference list falls through to `copy`, the
        // universal fallback, once earlier modes don't apply.
        let tmp = tempfile::tempdir().unwrap();
        let obj = tmp.path().join("obj");
        std::fs::write(&obj, b"payload").unwrap();
        protect(&obj).unwrap();
        let dest = tmp.path().join("dest");
        materialize(&obj, &dest, &"copy".parse().unwrap()).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");

        // Exercise the actual fallback path: a hardlink across devices
        // isn't reproducible in a unit test, but an empty/garbage list
        // still has to land on `copy` since it's always tried last.
        let dest2 = tmp.path().join("dest2");
        materialize(
            &obj,
            &dest2,
            &gat_core::config::MaterializationStrategy::from_values(&[
                "reflink", "hardlink", "symlink", "copy",
            ])
            .unwrap(),
        )
        .unwrap();
        assert!(dest2.exists());
    }

    #[test]
    fn materialize_reports_every_mode_failure_when_all_modes_fail() {
        let tmp = tempfile::tempdir().unwrap();
        // Object doesn't exist, so every mode ("reflink"/"hardlink"/
        // "symlink"/"copy") fails the same way -- proving the combined
        // error names each attempted mode instead of only the last one.
        let obj = tmp.path().join("missing-obj");
        let dest = tmp.path().join("dest");
        let err = materialize(
            &obj,
            &dest,
            &gat_core::config::MaterializationStrategy::from_values(&[
                "reflink", "hardlink", "copy",
            ])
            .unwrap(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("cache.materialization_strategy"),
            "message was: {msg}"
        );
        assert!(msg.contains("reflink"), "message was: {msg}");
        assert!(msg.contains("hardlink"), "message was: {msg}");
        assert!(msg.contains("copy"), "message was: {msg}");
        assert!(!dest.exists());
    }

    /// Structural (not wall-clock) assertions for the operation-scoped
    /// [`CacheClient`] proof access plan: each proves a shape
    /// property -- one DB open, set-based lookups, duplicate-free/memoized
    /// verification, presence-only stays hash/proof-free -- that a plain
    /// input/output equivalence test cannot catch regressing.
    mod object_cache_structural {
        use super::*;
        use crate::cache::proof::fixture::CacheFixture;
        use crate::cache::proof::test_support;
        use gat_core::oid::Oid;

        /// Publishes `bytes` at their content-addressed cache path
        /// directly, bypassing [`ingest`] entirely -- delegates to the
        /// shared [`CacheFixture::publish_cold`] rather than
        /// duplicating its bypass-`ingest` rationale here; see that
        /// method's doc comment for exactly why going through the real
        /// `ingest` would race.
        fn write_object(objects_dir: &Path, bytes: &[u8]) -> Oid {
            CacheFixture::new(objects_dir).publish_cold(bytes)
        }

        /// Writes `count` distinct objects in parallel (each ingest
        /// durably fsyncs its own temp file, the dominant cost of
        /// seeding a large object set) and returns their oids in index
        /// order.
        fn write_objects(objects_dir: &Path, count: usize) -> Vec<Oid> {
            (0..count)
                .into_par_iter()
                .map(|i| write_object(objects_dir, format!("obj-{i}").as_bytes()))
                .collect()
        }

        #[test]
        fn valid_size_survives_cold_warm_and_unavailable_proofs_in_both_verifiers() {
            for staged in [false, true] {
                for proof_state in ["cold", "warm", "unavailable"] {
                    let tmp = tempfile::tempdir().unwrap();
                    let objects_dir = tmp.path().join("objects");
                    let oid = write_object(&objects_dir, b"payload");
                    if proof_state == "warm" {
                        CacheClient::open(objects_dir.clone()).verify(&oid).unwrap();
                    }
                    let cache = CacheClient::open(objects_dir);
                    if proof_state == "unavailable" {
                        cache.break_database_for_test();
                    }
                    assert_eq!(cache.object(&oid).verified_size(), None);
                    let status = if staged {
                        let completed = cache.prepare_verification(&[oid, oid]).verify().unwrap();
                        assert_eq!(cache.object(&oid).verified_size(), None);
                        let statuses = cache.commit_verification(completed);
                        assert_eq!(statuses.len(), 2);
                        assert_eq!(statuses[0], statuses[1]);
                        statuses[0]
                    } else {
                        cache.verify(&oid).unwrap()
                    };
                    assert_eq!(status, ObjectVerification::Valid);
                    assert_eq!(cache.object(&oid).verified_size(), Some(7));
                    let before = test_support::snapshot();
                    assert_eq!(cache.verify(&oid).unwrap(), ObjectVerification::Valid);
                    assert_eq!(cache.object(&oid).verified_size(), Some(7));
                    assert_eq!(
                        test_support::snapshot().fs_verifications,
                        before.fs_verifications
                    );
                }
            }
        }

        #[test]
        fn proof_database_failure_cannot_keep_a_pre_mutation_observation() {
            for removed in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let objects_dir = tmp.path().join("objects");
                let oid = write_object(&objects_dir, b"payload");
                let cache = CacheClient::open(objects_dir.clone());
                assert_eq!(cache.verify(&oid).unwrap(), ObjectVerification::Valid);
                assert_eq!(cache.object(&oid).verified_size(), Some(7));
                cache.break_database_for_test();
                let object = cache_path_oid(&objects_dir, &oid);
                std::fs::remove_file(&object).unwrap();
                let error = if removed {
                    cache.remove_proofs(&[oid])
                } else {
                    std::fs::write(&object, b"broken").unwrap();
                    cache.apply_publications(&[CachePublication::remove(oid)])
                };
                assert!(error.is_err());
                assert_eq!(cache.object(&oid).verified_size(), None);
                assert_eq!(
                    cache.verify(&oid).unwrap(),
                    if removed {
                        ObjectVerification::Missing
                    } else {
                        ObjectVerification::Corrupt
                    }
                );
            }
        }

        #[test]
        fn invalidation_discards_status_and_size_before_observing_missing_or_corrupt() {
            for staged in [false, true] {
                for missing in [false, true] {
                    let tmp = tempfile::tempdir().unwrap();
                    let objects_dir = tmp.path().join("objects");
                    let oid = write_object(&objects_dir, b"payload");
                    let cache = CacheClient::open(objects_dir.clone());
                    cache.verify(&oid).unwrap();
                    assert_eq!(cache.object(&oid).verified_size(), Some(7));
                    let path = cache_path_oid(&objects_dir, &oid);
                    std::fs::remove_file(&path).unwrap();
                    if missing {
                        cache.remove_proofs(&[oid]).unwrap();
                    } else {
                        std::fs::write(path, b"broken").unwrap();
                        cache
                            .apply_publications(&[CachePublication::remove(oid)])
                            .unwrap();
                    }
                    assert_eq!(cache.object(&oid).verified_size(), None);
                    let status = if staged {
                        let completed = cache.prepare_verification(&[oid]).verify().unwrap();
                        cache.commit_verification(completed)[0]
                    } else {
                        cache.verify(&oid).unwrap()
                    };
                    assert_eq!(
                        status,
                        if missing {
                            ObjectVerification::Missing
                        } else {
                            ObjectVerification::Corrupt
                        }
                    );
                    assert_eq!(cache.object(&oid).verified_size(), None);
                    assert_eq!(cache.verify(&oid).unwrap(), status);
                }
            }
        }

        #[test]
        fn unmemoized_verification_preserves_existing_memo_without_retaining_new_sizes() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let retained = write_object(&objects_dir, b"retained");
            let fresh = write_object(&objects_dir, b"fresh");
            let cache = CacheClient::open(objects_dir);
            cache.verify(&retained).unwrap();
            cache
                .verify_windows_unmemoized(&[retained, fresh], |_, statuses| -> Result<()> {
                    assert_eq!(
                        statuses,
                        &[ObjectVerification::Valid, ObjectVerification::Valid]
                    );
                    Ok(())
                })
                .unwrap();
            assert_eq!(cache.memo.borrow().len(), 1);
            assert_eq!(cache.object(&retained).verified_size(), Some(8));
            assert_eq!(cache.object(&fresh).verified_size(), None);
        }

        #[test]
        fn verify_many_deduplicates_repeated_oids_to_one_verification() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"payload");
            let cache = CacheClient::open(objects_dir);

            let before = test_support::snapshot();
            let statuses = cache.verify_many(&[oid, oid, oid]).unwrap();
            let after = test_support::snapshot();

            assert_eq!(statuses.len(), 3);
            assert!(
                statuses
                    .iter()
                    .all(|s| matches!(s, ObjectVerification::Valid))
            );
            // Three requested positions collapse to one unique oid, so
            // exactly one filesystem verification runs -- not three.
            assert_eq!(after.fs_verifications - before.fs_verifications, 1);
        }

        #[test]
        fn verify_many_uses_one_set_based_lookup_not_n_point_queries() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oids: Vec<Oid> = (0..64)
                .map(|i| write_object(&objects_dir, format!("obj-{i}").as_bytes()))
                .collect();
            let cache = CacheClient::open(objects_dir);

            let before = test_support::snapshot();
            cache.verify_many(&oids).unwrap();
            let after = test_support::snapshot();

            // 64 oids, one logical request, and -- since 64 is far below the
            // SQLite bind budget -- a single physical `SELECT ... IN (...)`
            // statement, never 64 point queries.
            assert_eq!(
                after.proof_lookup_requests - before.proof_lookup_requests,
                1
            );
            assert_eq!(
                after.proof_lookup_statements - before.proof_lookup_statements,
                1
            );
        }

        #[test]
        fn verify_windows_yields_more_than_one_window_past_the_verify_window_bound() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            // More than one `window_size`, so `verify_windows` must call
            // back more than once rather than internally collecting the
            // whole input before yielding a single combined result: each
            // window is fully verified/persisted and handed to
            // the caller before the next window is even looked up.
            // Exercise the production windowing logic with small real-file fixtures.
            let window_size = 8;
            let _window = crate::cache::object::test_support::with_verify_window(window_size);
            let count = window_size + 1;
            let oids = write_objects(&objects_dir, count);
            let cache = CacheClient::open(objects_dir);

            let mut window_calls = 0usize;
            let mut seen = 0usize;
            cache
                .verify_windows(&oids, |window_oids, statuses| -> Result<()> {
                    window_calls += 1;
                    assert_eq!(window_oids.len(), statuses.len());
                    assert!(window_oids.len() <= window_size);
                    seen += window_oids.len();
                    assert!(
                        statuses
                            .iter()
                            .all(|s| matches!(s, ObjectVerification::Valid))
                    );
                    Ok(())
                })
                .unwrap();

            assert_eq!(window_calls, 2, "expected one callback per bounded window");
            assert_eq!(seen, count);
        }

        #[test]
        fn verify_windows_unmemoized_does_not_retain_verification_state_past_its_own_window() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            // More than one `window_size`, and every oid is distinct
            // (already globally deduplicated, matching how `push`/`fetch`
            // call this), so nothing here can ever benefit from
            // cross-window memoization.
            // Exercise the production windowing logic with small real-file fixtures.
            let window_size = 8;
            let _window = crate::cache::object::test_support::with_verify_window(window_size);
            let count = window_size + 1;
            let oids = write_objects(&objects_dir, count);
            let cache = CacheClient::open(objects_dir);

            let mut max_memo_len = 0usize;
            cache
                .verify_windows_unmemoized(&oids, |window_oids, statuses| -> Result<()> {
                    assert_eq!(window_oids.len(), statuses.len());
                    // The memo must never hold more than the current
                    // window's worth of entries: earlier windows' statuses
                    // are dropped once yielded, so retained verification
                    // state stays bounded rather than growing with the
                    // full selection.
                    max_memo_len = max_memo_len.max(cache.memo.borrow().len());
                    Ok(())
                })
                .unwrap();

            assert!(
                max_memo_len <= window_size,
                "expected retained memo state to stay bounded by one window, got {max_memo_len}"
            );
            assert_eq!(
                cache.memo.borrow().len(),
                0,
                "expected no verification state to be retained once every window has been \
                 consumed"
            );
        }

        #[test]
        fn memo_high_water_counter_stays_bounded_by_one_window_across_a_large_unmemoized_run() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            // Exercise the production windowing logic with small real-file fixtures.
            let window_size = 8;
            let _window = crate::cache::object::test_support::with_verify_window(window_size);
            let count = window_size + 1;
            let oids = write_objects(&objects_dir, count);
            let cache = CacheClient::open(objects_dir);

            crate::cache::object::test_support::reset_memo_high_water();
            cache
                .verify_windows_unmemoized(&oids, |_, _| -> Result<()> { Ok(()) })
                .unwrap();

            assert!(
                crate::cache::object::test_support::memo_high_water() <= window_size,
                "expected the item-68 memo high-water counter to stay bounded by one window, \
                 got {}",
                crate::cache::object::test_support::memo_high_water()
            );
        }

        #[test]
        #[cfg(debug_assertions)]
        #[should_panic(expected = "verify_windows_unmemoized requires globally deduplicated oids")]
        fn verify_windows_unmemoized_panics_on_a_duplicate_more_than_one_window_apart() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            // Enough distinct objects to span more than one `window_size`,
            // then repeat the very first oid at the very end -- more than
            // `window_size` elements separate the two occurrences, so a
            // dedup check scoped to only the *current* window would never
            // observe both appearances together and would miss this
            // violation entirely.
            // Exercise the production windowing logic with small real-file fixtures.
            let window_size = 8;
            let _window = crate::cache::object::test_support::with_verify_window(window_size);
            let count = window_size + 1;
            let mut oids = write_objects(&objects_dir, count);
            oids.push(oids[0]);
            let cache = CacheClient::open(objects_dir);

            let _ = cache.verify_windows_unmemoized(&oids, |_, _| -> Result<()> { Ok(()) });
        }

        #[test]
        fn a_second_verify_reuses_the_memoized_status_without_reverifying() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"payload");
            let cache = CacheClient::open(objects_dir);

            cache.verify(&oid).unwrap();
            let before = test_support::snapshot();
            cache.verify(&oid).unwrap();
            let after = test_support::snapshot();

            // The second verify is a pure memo hit: no filesystem
            // verification, no proof lookup.
            assert_eq!(after.fs_verifications - before.fs_verifications, 0);
            assert_eq!(after.memo_hits - before.memo_hits, 1);
        }

        #[test]
        fn staged_results_preserve_input_order_across_memo_hits_and_fresh_duplicates() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let known = write_object(&objects_dir, b"known");
            let fresh = write_object(&objects_dir, b"fresh bytes");
            let missing = Oid::from_bytes([0xaa; 32]);
            let cache = CacheClient::open(objects_dir);
            cache.verify(&known).unwrap();
            let completed = cache
                .prepare_verification(&[fresh, known, missing, fresh, known])
                .verify()
                .unwrap();
            assert_eq!(
                cache.commit_verification(completed),
                vec![
                    ObjectVerification::Valid,
                    ObjectVerification::Valid,
                    ObjectVerification::Missing,
                    ObjectVerification::Valid,
                    ObjectVerification::Valid
                ]
            );
            assert_eq!(cache.object(&known).verified_size(), Some(5));
            assert_eq!(cache.object(&fresh).verified_size(), Some(11));
            assert_eq!(cache.object(&missing).verified_size(), None);
        }

        #[test]
        fn staged_verification_is_worker_safe_and_reuses_committed_memo_state() {
            fn assert_send_static<T: Send + 'static>() {}

            assert_send_static::<PreparedCacheVerification>();
            assert_send_static::<CompletedCacheVerification>();

            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"staged payload");
            let cache = CacheClient::open(objects_dir);

            let before = test_support::snapshot();
            let completed = cache.prepare_verification(&[oid, oid]).verify().unwrap();
            assert_eq!(
                cache.commit_verification(completed),
                vec![ObjectVerification::Valid; 2]
            );
            let after_first = test_support::snapshot();
            assert_eq!(
                after_first.fs_verifications - before.fs_verifications,
                1,
                "duplicate oids in one preparation must share one filesystem verification"
            );

            let completed = cache.prepare_verification(&[oid]).verify().unwrap();
            assert_eq!(
                cache.commit_verification(completed),
                vec![ObjectVerification::Valid]
            );
            let after_second = test_support::snapshot();
            assert_eq!(
                after_second.fs_verifications - after_first.fs_verifications,
                0,
                "a committed staged result must be reused from the operation memo"
            );
        }

        #[test]
        fn staged_verification_falls_back_to_hashing_after_proof_database_failure() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"fallback payload");

            let hashes = with_exclusive_hash_file_call_count(move || {
                let cache = CacheClient::open(objects_dir);
                cache.break_database_for_test();
                let completed = cache.prepare_verification(&[oid]).verify().unwrap();
                assert_eq!(
                    cache.commit_verification(completed),
                    vec![ObjectVerification::Valid]
                );
                hash_file_call_count()
            });
            assert_eq!(hashes, 1);
        }

        #[test]
        fn a_cold_valid_object_is_hashed_at_most_once_per_verification() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"cold payload");

            let hashes = with_exclusive_hash_file_call_count(|| {
                let cache = CacheClient::open(objects_dir);
                cache.verify(&oid).unwrap();
                hash_file_call_count()
            });
            assert_eq!(hashes, 1);
        }

        #[test]
        fn a_warm_proof_avoids_hashing_entirely() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"warm payload");
            // Seed a reusable proof through a first, throwaway operation.
            CacheClient::open(objects_dir.clone()).verify(&oid).unwrap();

            // A fresh operation (empty memo) now trusts the persisted stat
            // proof and never hashes.
            let hashes = with_exclusive_hash_file_call_count(|| {
                let cache = CacheClient::open(objects_dir);
                cache.verify(&oid).unwrap();
                hash_file_call_count()
            });
            assert_eq!(hashes, 0);
        }

        #[test]
        fn cache_presence_touches_no_proof_database_or_hash() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"payload");
            let missing = Oid::from_hex(&"a".repeat(64)).unwrap();
            let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
            let root = layout.resolve_cache_root(Some(
                &gat_core::cache_location::CacheLocation::try_from_path(std::path::PathBuf::from(
                    objects_dir.as_os_str(),
                ))
                .expect("nonempty fixture cache path"),
            ));
            let presence = root.presence();

            let hashes = with_exclusive_hash_file_call_count(|| {
                let before = test_support::snapshot();
                assert!(presence.contains(&oid));
                assert!(!presence.contains(&missing));
                let after = test_support::snapshot();
                assert_eq!(
                    after.proof_lookup_requests - before.proof_lookup_requests,
                    0
                );
                assert_eq!(after.cache_db_opens - before.cache_db_opens, 0);
                hash_file_call_count()
            });
            assert_eq!(hashes, 0);
        }

        #[test]
        fn a_disabled_proof_db_still_verifies_by_falling_back_to_hashing() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"payload");
            // A directory where `cache.sqlite3` should be makes the proof DB
            // unopenable, so the accelerator is disabled -- verification must
            // still succeed by hashing, never error. Replace the DB file that
            // `write_object` just created with such a directory.
            let db = objects_dir.join("cache.sqlite3");
            let _ = std::fs::remove_file(&db);
            let _ = std::fs::remove_file(objects_dir.join("cache.sqlite3-wal"));
            let _ = std::fs::remove_file(objects_dir.join("cache.sqlite3-shm"));
            std::fs::create_dir_all(&db).unwrap();

            let cache = CacheClient::open(objects_dir);
            assert!(matches!(
                cache.verify(&oid).unwrap(),
                ObjectVerification::Valid
            ));
        }

        #[test]
        fn verify_many_degrades_to_hashing_when_the_proof_db_fails_after_opening_successfully() {
            // Unlike a DB that never opens (see above), this DB opens fine
            // -- `CacheClient::open` succeeds -- but then fails on a real
            // query once broken. `verify_many` must still succeed by
            // treating the failed bulk lookup as "no known priors" and
            // hashing, never propagating the raw SQLite error and failing
            // an otherwise-valid verification.
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"payload");
            let cache = CacheClient::open(objects_dir);
            cache.break_database_for_test();

            let statuses = cache.verify_many(&[oid]).unwrap();
            assert_eq!(statuses, vec![ObjectVerification::Valid]);
        }

        #[test]
        fn verify_degrades_when_the_proof_db_fails_after_opening_successfully() {
            let tmp = tempfile::tempdir().unwrap();
            let objects_dir = tmp.path().join("objects");
            let oid = write_object(&objects_dir, b"payload");
            let cache = CacheClient::open(objects_dir);
            cache.break_database_for_test();

            let status = cache.verify(&oid).unwrap();
            assert_eq!(status, ObjectVerification::Valid);
        }
    }
}
