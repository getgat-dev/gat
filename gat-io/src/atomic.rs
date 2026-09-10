//! Shared atomic-file-write helper for repo-local state that must never be
//! observed half-written, including `gat.lock` and `gat.yaml`. Also exposes [`persist_with_retry`] directly
//! for callers (cache publication) that build their own `NamedTempFile`
//! but still need the same Windows-safe atomic-rename publish. Writes go
//! to a same-directory, collision-safe temp file (so concurrent writers to
//! the same path never clobber each other's temp file the way a fixed
//! `.tmp` suffix could), are flushed to disk, then published via an atomic
//! rename -- a reader can never observe a partially-written file, and a
//! process killed mid-write leaves the previous (or no) file untouched at
//! `path`.
//!
//! This module owns atomic publication alongside [`RepoLock`], the
//! repository-wide advisory lock guarding this same
//! repo-local state against overlapping `gat` processes. Physical stage,
//! path, cleanup, and operating-system failure invariants stay here;
//! higher layers classify them into semantic operation failures.

use fs2::FileExt as _;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Local filesystem failures from atomically publishing a file
/// (`write_atomic`/`write_atomic_if_absent`/`write_atomic_with_proof`) or
/// acquiring the repo-wide advisory lock ([`RepoLock`]). Every write
/// through this module goes through the same create-temp-file /
/// write-contents / sync / rename-into-place sequence, so each stage gets
/// its own variant here so callers retain whether the failed action was
/// creating, writing, syncing, renaming, or locking.
#[derive(Debug, thiserror::Error)]
pub enum AtomicError {
    /// A parent directory or repository storage protection could not be
    /// prepared. `path` identifies the directory or required self-ignore file.
    #[error("could not prepare local storage at `{}`", path.display())]
    DirectoryUnavailable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The same-directory, collision-safe temp file itself could not be
    /// created.
    #[error("could not create a temporary file in `{}`", dir.display())]
    TempFileUnavailable {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The temp file's contents could not be written.
    #[error("could not write `{}`", path.display())]
    WriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The temp file (or, for `persist_finalized_with_proof`, reading
    /// its metadata to mint a proof) could not be synced/finalized before
    /// publishing.
    #[error("could not sync the temporary file publishing `{}`", path.display())]
    SyncFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The completed temp file could not be published (renamed, or -- for
    /// [`write_atomic_if_absent`] -- hard-linked) into place at `path`.
    #[error("could not publish `{}`", path.display())]
    PublishFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `RepoLock::acquire_repository`'s lock file itself (`.gat/state/sync.lock`)
    /// could not be created/opened.
    #[error("could not open lock file `{}`", path.display())]
    LockFileUnavailable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Acquiring the advisory lock on an already-open lock file failed for
    /// a reason other than another process actively holding it (e.g. an
    /// unsupported filesystem) -- distinct from [`AtomicError::LockTimedOut`],
    /// which is the expected "another live process holds it" case.
    #[error("could not acquire the lock at `{}`", path.display())]
    LockAcquireFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Another `gat` process demonstrably held the lock (the OS reported
    /// it as contended) for longer than `LOCK_WAIT`.
    #[error(
        "another gat process is modifying repository state (lock held at `{}`); \
         it will release automatically when it finishes or exits",
        path.display()
    )]
    LockTimedOut { path: PathBuf },
}

impl AtomicError {
    /// The OS category, when this is an I/O failure rather than lock contention.
    #[must_use]
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            Self::DirectoryUnavailable { source, .. }
            | Self::TempFileUnavailable { source, .. }
            | Self::WriteFailed { source, .. }
            | Self::SyncFailed { source, .. }
            | Self::PublishFailed { source, .. }
            | Self::LockFileUnavailable { source, .. }
            | Self::LockAcquireFailed { source, .. } => Some(source.kind()),
            Self::LockTimedOut { .. } => None,
        }
    }
}

/// Result of an atomic filesystem operation.
pub type Result<T> = std::result::Result<T, AtomicError>;

/// How many times to retry a transient publish failure before giving up.
/// Only ever exercised on Windows (see `persist_with_retry`); a plain
/// `rename(2)` on Unix never fails this way, so this is never hit there.
const PUBLISH_RETRIES: u32 = 10;

/// Windows can't atomically replace a file that another handle has open
/// without `FILE_SHARE_DELETE` (e.g. a concurrent writer's own rename of a
/// same-named temp file, or a scanning AV), and briefly fails the rename
/// with "Access is denied" (raw OS error 5) even though nothing is
/// actually wrong -- the destination is free again microseconds later.
/// Retry a few times with a short backoff before surfacing the error, so
/// concurrent `write_atomic` callers (`gat.lock`, `gat.yaml`, sync state)
/// don't spuriously fail under contention on Windows. Unix renames are
/// atomic and never return this error, so this only ever loops there.
///
/// Public so every direct `NamedTempFile::persist` caller in the crate
/// (not just `write_atomic_with_proof`) can share the same Windows-safe
/// retry -- cache publication publishes content-addressed objects
/// the same way and hits the identical transient failure under concurrent
/// ingests.
///
/// This is a low-level rename/retry primitive, not the API a caller that
/// needs reusable evidence should reach for directly: it hands back the
/// destination's post-rename `File`, but a proof minted from *that*
/// handle would depend on the rename having actually happened (and,
/// through Windows' retry loop, possibly having been retried) before any
/// evidence exists at all. Callers that need a
/// `crate::file_state::StatProof` describing the published bytes
/// should go through `persist_finalized_with_proof` instead, which
/// mints the proof from the temp file itself before ever calling this.
pub fn persist_with_retry(
    mut tmp: tempfile::NamedTempFile,
    path: &Path,
) -> std::result::Result<std::fs::File, tempfile::PersistError> {
    for attempt in 0..=PUBLISH_RETRIES {
        match tmp.persist(path) {
            Ok(file) => return Ok(file),
            Err(err) => {
                let transient =
                    cfg!(windows) && err.error.kind() == std::io::ErrorKind::PermissionDenied;
                if !transient || attempt == PUBLISH_RETRIES {
                    return Err(err);
                }
                tmp = err.file;
                std::thread::sleep(Duration::from_millis(10 * (u64::from(attempt) + 1)));
            }
        }
    }
    unreachable!()
}

/// Create a same-directory, collision-safe temp file next to `path` and
/// write `contents` into it -- the shared first step of both
/// [`write_atomic`] and `write_atomic_with_proof`. Performs neither a
/// durability `sync_all` nor any proof collection nor the publishing
/// rename itself; callers own exactly one of those next, depending on
/// whether they need a `crate::file_state::StatProof` describing the
/// bytes just written.
fn prepare_atomic_tmp(path: &Path, contents: &str) -> Result<tempfile::NamedTempFile> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            std::fs::create_dir_all(parent).map_err(|source| {
                AtomicError::DirectoryUnavailable {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
            parent
        }
        _ => Path::new("."),
    };
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(dir)
        .map_err(|source| AtomicError::TempFileUnavailable {
            dir: dir.to_path_buf(),
            source,
        })?;
    tmp.write_all(contents.as_bytes())
        .map_err(|source| AtomicError::WriteFailed {
            path: tmp.path().to_path_buf(),
            source,
        })?;
    Ok(tmp)
}

/// Atomically write `contents` to `path` (temp file in the same directory,
/// then rename into place), so a process interrupted mid-write never
/// leaves a truncated file at `path`. Proof-agnostic: no
/// `crate::file_state::StatProof` is collected, so a caller that needs
/// one describing the bytes just published should use
/// `write_atomic_with_proof` instead rather than discard this one's
/// result -- this path never stats the published file at all.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let mut tmp = prepare_atomic_tmp(path, contents)?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| AtomicError::SyncFailed {
            path: path.to_path_buf(),
            source,
        })?;
    persist_with_retry(tmp, path).map_err(|e| AtomicError::PublishFailed {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

/// Create `path` with `contents` only if nothing exists there yet, with
/// no race window between an existence check and the write -- unlike
/// [`write_atomic`], this never overwrites a file that another process
/// (or a concurrent call in the same process) publishes at `path` after
/// this call starts. Returns `true` if this call created the file,
/// `false` if `path` already existed (left completely untouched -- not
/// even read, so its content is irrelevant).
///
/// Implemented as a same-directory temp file (written and synced exactly
/// like [`write_atomic`]'s) published via [`std::fs::hard_link`] instead
/// of a rename: `link(2)`/`CreateHardLink` atomically fail with
/// `AlreadyExists` rather than replacing an existing destination, which
/// is exactly the create-if-absent semantics a rename can't provide (a
/// rename always succeeds by silently replacing whatever was there).
/// The temp file itself is then discarded -- the hard link at `path`
/// keeps the published bytes alive independently of it.
pub fn write_atomic_if_absent(path: &Path, contents: &str) -> Result<bool> {
    let tmp = prepare_atomic_tmp(path, contents)?;
    tmp.as_file()
        .sync_all()
        .map_err(|source| AtomicError::SyncFailed {
            path: path.to_path_buf(),
            source,
        })?;
    match std::fs::hard_link(tmp.path(), path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(source) => Err(AtomicError::PublishFailed {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// A successful atomic publish, together with a
/// `crate::file_state::StatProof` describing exactly the bytes just
/// published -- minted from the completed temp file itself, immediately
/// before the rename that publishes it, rather than from a second,
/// separate stat of the destination path afterward. POSIX `rename(2)`
/// never changes a file's size or mtime, so this pre-rename proof is
/// exactly as valid as a post-rename one would be, without requiring the
/// rename itself to hand back a live file handle.
pub struct PublishedFile {
    pub(crate) proof: crate::file_state::StatProof,
}

/// As [`write_atomic`], but also returns a [`PublishedFile`] carrying the
/// proof of the bytes just written -- the atomic-publish counterpart of
/// [`crate::file_state::coherent_observation`] on the read side.
pub fn write_atomic_with_proof(path: &Path, contents: &str) -> Result<PublishedFile> {
    let mut tmp = prepare_atomic_tmp(path, contents)?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| AtomicError::SyncFailed {
            path: path.to_path_buf(),
            source,
        })?;
    persist_finalized_with_proof(tmp, path)
}

/// Own only the final steps of publication for a temp file a caller has
/// already fully written *and durably synced* (`sync_all`'d): mint a
/// [`PublishedFile`]'s `crate::file_state::StatProof` from `tmp`'s own
/// metadata, then hand it to [`persist_with_retry`] for the Windows-safe
/// atomic rename. `tmp` is taken by value specifically so no caller can
/// go on writing to it (or otherwise changing its metadata) after this
/// point -- the proof and the published bytes are guaranteed to describe
/// the same generation.
///
/// Precondition: the caller has already completed every write/flush and
/// any durability sync `tmp` needs -- this function performs no write,
/// flush, hash, or `sync_all` of its own, and it never stats or reads the
/// destination path either before or after the rename.
///
/// The proof is minted *before* the rename, not from a second, separate
/// stat of `path` afterward: POSIX `rename(2)` never changes a file's
/// size or mtime, so a pre-rename proof is exactly as valid as a
/// post-rename one, and `tmp`'s content/metadata never change across
/// [`persist_with_retry`]'s transient-failure retries (the same temp file
/// is simply retried), so minting the proof once, before the rename loop
/// even starts, remains accurate through a (possibly retried) success.
pub fn persist_finalized_with_proof(
    tmp: tempfile::NamedTempFile,
    path: &Path,
) -> Result<PublishedFile> {
    let metadata = tmp
        .as_file()
        .metadata()
        .map_err(|source| AtomicError::SyncFailed {
            path: path.to_path_buf(),
            source,
        })?;
    let proof = crate::file_state::stat_proof_from_metadata(&metadata).ok_or_else(|| {
        AtomicError::SyncFailed {
            path: path.to_path_buf(),
            source: std::io::Error::other("temp file metadata does not describe a regular file"),
        }
    })?;
    persist_with_retry(tmp, path).map_err(|e| AtomicError::PublishFailed {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    #[cfg(test)]
    tests::after_publish(path);
    Ok(PublishedFile { proof })
}

/// Best-effort `fsync` of a directory's own metadata (as opposed to a
/// file's contents), so a rename into/out of `dir` -- e.g. `gat.lock`
/// reshape's transaction-record publish and its two commit renames (see
/// `crate::LockStore::publish_repository`) -- survives a power
/// loss, not just an ordinary process kill (which a completed `rename(2)`
/// already survives on its own). Silently a no-op wherever directory
/// `fsync` either isn't meaningful (Windows) or fails (e.g. `dir` no
/// longer exists) -- callers rely on the transaction record plus an intact
/// backup for recoverability, not on this being infallible.
pub fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    {
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// How long to wait for another process's advisory lock before giving up.
/// This timeout only activates when
/// another process is *demonstrably alive* (the OS reports the lock as
/// held). A crashed or killed process's lock is released automatically by
/// the OS when its file descriptor closes, so there is no stale-lock
/// stealing here.
const LOCK_WAIT: Duration = Duration::from_secs(10);

thread_local! {
    // Weak handles make the registry non-owning. Each repository retains its
    // own OS lock, and only the final guard releases that lock.
    static HELD_LOCKS: RefCell<HashMap<Arc<PathBuf>, Weak<std::fs::File>>> = RefCell::new(HashMap::new());
}

/// A cooperative, OS advisory-lock-based file lock (`.gat/state/sync.lock`)
/// held across an entire plan+execute, a single materialized-state
/// mutation, or a `gat.lock` shape reshape/entry write, so two overlapping
/// `gat sync`/hook invocations (e.g. a rebase that fires `post-rewrite` per
/// commit), a `gat add`/`rm`/`mv` racing a sync, or any of those racing a
/// `lock.shard_levels` reshape, never interleave their reads and writes of
/// repo-local state.
///
/// Uses `flock(2)` (Unix) or `LockFileEx` (Windows) for true ownership
/// semantics: a live process's lock cannot be stolen by an age heuristic,
/// and a crashed/killed process's lock is released automatically by the OS
/// when its file descriptors close. RAII: dropping this struct closes the
/// final shared file handle, which releases the advisory lock. Reentrant
/// guards share ownership only within the same thread and canonical repository.
/// Guards cannot move to another thread because the registry is thread-local.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<gat_io::RepoLock>();
/// ```
pub struct RepoLock {
    file: Rc<std::fs::File>,
    identity: Arc<PathBuf>,
    global: Option<Box<Self>>,
}

impl RepoLock {
    /// Serialize configuration-dependent mutations across repositories sharing a
    /// global scope. Always acquire global authority before repository authority.
    pub fn acquire_configuration(
        layout: &crate::RepositoryLayout,
        global: Option<&Path>,
    ) -> Result<Self> {
        let global = global
            .map(|directory| {
                std::fs::create_dir_all(directory).map_err(|source| {
                    AtomicError::DirectoryUnavailable {
                        path: directory.to_path_buf(),
                        source,
                    }
                })?;
                let identity = std::fs::canonicalize(directory)
                    .map_err(|source| AtomicError::DirectoryUnavailable {
                        path: directory.to_path_buf(),
                        source,
                    })?
                    .join("configuration.lock");
                Self::acquire_at(identity.clone(), Arc::new(identity))
            })
            .transpose()?;
        let mut repository = Self::acquire_repository(layout)?;
        repository.global = global.map(Box::new);
        Ok(repository)
    }

    /// Acquire this repository's mutation lock without exposing its path.
    pub fn acquire_repository(layout: &crate::RepositoryLayout) -> Result<Self> {
        layout
            .local_directory()
            .ensure()
            .map_err(|error| AtomicError::DirectoryUnavailable {
                path: error.path,
                source: error.source,
            })?;
        let identity = layout.local_directory().lock_identity().map_err(|source| {
            AtomicError::DirectoryUnavailable {
                path: layout.cache_root_path().to_path_buf(),
                source,
            }
        })?;
        Self::acquire_at(layout.sync_lock_path(), identity)
    }

    fn acquire_at(path: PathBuf, identity: Arc<PathBuf>) -> Result<Self> {
        if let Some(file) =
            HELD_LOCKS.with(|locks| locks.borrow().get(&identity).and_then(Weak::upgrade))
        {
            return Ok(Self {
                file,
                identity,
                global: None,
            });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| {
                AtomicError::DirectoryUnavailable {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| AtomicError::LockFileUnavailable {
                path: path.clone(),
                source,
            })?;

        #[cfg(any(test, feature = "test-support"))]
        test_support::notify_acquire_attempt();

        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    let file = Rc::new(file);
                    HELD_LOCKS.with(|locks| {
                        locks
                            .borrow_mut()
                            .insert(Arc::clone(&identity), Rc::downgrade(&file))
                    });
                    return Ok(Self {
                        file,
                        identity,
                        global: None,
                    });
                }
                Err(e) if e.kind() == fs2::lock_contended_error().kind() => {
                    // The OS confirms another process actively holds the lock.
                    // Poll until the deadline; the lock is released automatically
                    // when that process finishes or its file descriptor closes.
                    if Instant::now() >= deadline {
                        return Err(AtomicError::LockTimedOut { path });
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(source) => {
                    return Err(AtomicError::LockAcquireFailed { path, source });
                }
            }
        }
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if Rc::strong_count(&self.file) == 1 {
            // A guard may outlive the registry during thread-local teardown.
            // Dropping its file still releases the OS lock in that case.
            let _ = HELD_LOCKS.try_with(|locks| locks.borrow_mut().remove(&self.identity));
        }
    }
}

/// Test-only synchronization hook for deterministically proving that a
/// concurrent `RepoLock::acquire_repository` call on another thread has actually
/// reached the OS-level lock boundary (i.e. is about to make, or is
/// retrying, its `try_lock_exclusive` call) rather than guessing with a
/// fixed sleep. Registrations are keyed per-`ThreadId` in a shared map, so
/// two `#[test]` functions running concurrently (as `cargo test` does by
/// default) can each install their own hook for their own target thread
/// without one overwriting the other's -- unlike a single global slot,
/// which would let one test's registration clobber another's and cause the
/// unrelated test to hang waiting on a notification that never arrives.
/// Gated on the `test-support` feature (in addition to `cfg(test)`) so
/// the root `gat` crate's own tests -- which link `gat-io` as an ordinary
/// (non-test) dependency and so cannot see another crate's `#[cfg(test)]`
/// items directly -- can still install and observe this instrumentation
/// via `gat-io = { path = "gat-io", features = ["test-support"] }` in
/// `[dev-dependencies]`; Cargo's feature unification then activates it
/// for every build of `gat-io` in that same `cargo test` invocation.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::mpsc::Sender;
    use std::thread::ThreadId;

    static ACQUIRE_ATTEMPT_HOOKS: Mutex<Option<HashMap<ThreadId, Sender<()>>>> = Mutex::new(None);

    /// RAII guard that removes this test's own hook registration when
    /// dropped -- including on panic/unwind -- so a failing assertion in
    /// one test can never leave a stale entry behind for a later test that
    /// happens to reuse the same `ThreadId`.
    struct AcquireAttemptHookGuard(ThreadId);

    impl Drop for AcquireAttemptHookGuard {
        fn drop(&mut self) {
            if let Some(map) = ACQUIRE_ATTEMPT_HOOKS.lock().unwrap().as_mut() {
                map.remove(&self.0);
            }
        }
    }

    /// Install `sender` so `super::RepoLock::acquire_repository` notifies it the
    /// first time it's called on `thread_id`, for the duration of `f`.
    /// Only this thread's own registration is ever installed or removed,
    /// so this is safe to call from multiple tests running in parallel.
    ///
    /// # Panics
    /// Panics if the hook registry mutex is poisoned.
    pub fn with_acquire_attempt_hook<T>(
        thread_id: ThreadId,
        sender: Sender<()>,
        f: impl FnOnce() -> T,
    ) -> T {
        ACQUIRE_ATTEMPT_HOOKS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(thread_id, sender);
        let _guard = AcquireAttemptHookGuard(thread_id);
        f()
    }

    /// # Panics
    /// Panics if the hook registry mutex is poisoned.
    pub fn notify_acquire_attempt() {
        if let Some(sender) = ACQUIRE_ATTEMPT_HOOKS
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|map| map.get(&std::thread::current().id()))
        {
            let _ = sender.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_local_guard_releases_lock_after_registry_teardown() {
        thread_local! {
            static GUARD: RefCell<Option<RepoLock>> = const { RefCell::new(None) };
        }
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        let worker_layout = layout.clone();
        std::thread::spawn(move || {
            // Initialize the guard slot first so its destructor runs after
            // the registry's destructor on thread exit.
            GUARD.with(|slot| {
                *slot.borrow_mut() = Some(RepoLock::acquire_repository(&worker_layout).unwrap());
            });
        })
        .join()
        .unwrap();
        let probe = std::fs::OpenOptions::new()
            .write(true)
            .open(layout.sync_lock_path())
            .unwrap();
        probe.try_lock_exclusive().unwrap();
    }

    #[test]
    fn nested_locks_for_different_repositories_initialize_and_lock_each_repository() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let a = crate::RepositoryLayout::at(a.path().to_path_buf());
        let b = crate::RepositoryLayout::at(b.path().to_path_buf());
        let _a = RepoLock::acquire_repository(&a).unwrap();
        let _b = RepoLock::acquire_repository(&b).unwrap();
        assert_eq!(
            std::fs::read(b.cache_root_path().join(".gitignore")).unwrap(),
            b"*\n"
        );
        let probe = std::fs::OpenOptions::new()
            .write(true)
            .open(b.sync_lock_path())
            .unwrap();
        assert_eq!(
            probe.try_lock_exclusive().unwrap_err().kind(),
            fs2::lock_contended_error().kind()
        );
    }

    #[test]
    fn dropping_outer_guard_keeps_the_os_lock_until_the_last_nested_guard() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        let outer = RepoLock::acquire_repository(&layout).unwrap();
        let inner = RepoLock::acquire_repository(&layout).unwrap();
        drop(outer);
        let probe = std::fs::OpenOptions::new()
            .write(true)
            .open(layout.sync_lock_path())
            .unwrap();
        assert_eq!(
            probe.try_lock_exclusive().unwrap_err().kind(),
            fs2::lock_contended_error().kind()
        );
        drop(inner);
        probe.try_lock_exclusive().unwrap();
        assert!(HELD_LOCKS.with(|locks| locks.borrow().is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn repository_aliases_share_the_same_reentrant_lock() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&repository, &alias).unwrap();
        let layout = crate::RepositoryLayout::at(repository);
        let alias = crate::RepositoryLayout::at(alias);
        let first = RepoLock::acquire_repository(&layout).unwrap();
        let second = RepoLock::acquire_repository(&alias).unwrap();
        assert!(Rc::ptr_eq(&first.file, &second.file));
    }

    #[test]
    #[cfg(unix)]
    fn write_atomic_reports_permission_denied_for_an_unwritable_parent_directory() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("locked-down");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let err = write_atomic(&dir.join("f.txt"), "hello").unwrap_err();
        assert!(matches!(
            err,
            AtomicError::TempFileUnavailable {
                source,
                ..
            } if source.kind() == std::io::ErrorKind::PermissionDenied
        ));

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `write_atomic_with_proof`'s returned proof describes
    /// exactly the bytes published -- correct size, and a live no-follow
    /// stat of the destination still exactly matches it (POSIX
    /// `rename(2)` never changes size/mtime, so a proof minted from the
    /// pre-rename temp file is exactly as valid as a post-rename stat
    /// would be).
    #[test]
    fn write_atomic_with_proof_returns_a_proof_describing_the_published_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        let published = write_atomic_with_proof(&path, "hello, proof").unwrap();

        assert_eq!(published.proof.size, "hello, proof".len() as u64);
        assert_eq!(
            crate::file_state::observe_regular_file_no_follow(&path),
            Some(published.proof)
        );
    }

    /// The proof-agnostic [`write_atomic`] publishes identically
    /// to `write_atomic_with_proof` (same content ends up on disk) while
    /// exposing no proof at all -- it is `()`, not merely an unused value
    /// a caller happens to discard.
    #[test]
    fn write_atomic_publishes_the_same_bytes_with_no_proof_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        let result: Result<()> = write_atomic(&path, "same content");
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "same content");
    }

    type AfterPublish = Box<dyn FnOnce(&Path)>;

    thread_local! {
        // Each test owns its callback; taking it before invocation also allows
        // the replacement writer to use the same publication helper.
        static AFTER_PUBLISH: std::cell::RefCell<Option<AfterPublish>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn after_publish(path: &Path) {
        let callback = AFTER_PUBLISH.with(|slot| slot.borrow_mut().take());
        if let Some(callback) = callback {
            callback(path);
        }
    }

    /// Force a replacement after the original rename but before its proof
    /// returns. This tests the metadata race without saturating Windows rename
    /// retries with unrelated publication contention.
    #[test]
    fn write_atomic_with_proof_never_reports_a_racing_writers_metadata() {
        struct ClearHook;
        impl Drop for ClearHook {
            fn drop(&mut self) {
                AFTER_PUBLISH.with(|slot| slot.borrow_mut().take());
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        let original = "original";
        let replacement = "a different writer's longer replacement";
        let _clear_hook = ClearHook;
        AFTER_PUBLISH.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |path| {
                write_atomic_with_proof(path, replacement).unwrap();
            }));
        });

        let published = write_atomic_with_proof(&path, original).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        let current = crate::file_state::observe_regular_file_no_follow(&path).unwrap();
        assert_eq!(current.size, replacement.len() as u64);
        assert_eq!(published.proof.size, original.len() as u64);
        assert_ne!(published.proof, current);
    }

    #[test]
    fn write_atomic_creates_file_with_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        write_atomic(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn write_atomic_creates_missing_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/c/f.txt");
        write_atomic(&path, "nested").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "nested");
    }

    #[test]
    fn write_atomic_overwrites_existing_file_completely() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        write_atomic(&path, "first, much longer content").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn write_atomic_leaves_no_leftover_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        write_atomic(&path, "content").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover entries: {leftovers:?}");
    }

    #[test]
    fn write_atomic_from_concurrent_threads_never_corrupts_the_file() {
        // Many threads race to write the same path concurrently with
        // distinct, easily-distinguished contents. Whichever writer wins,
        // the file must always be one writer's complete, uncorrupted
        // content -- never a torn mix of two, and never left as a stray
        // temp file due to a fixed/colliding temp name.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        let candidates: Vec<String> = (0..16).map(|i| "x".repeat(1000 + i)).collect();
        std::thread::scope(|s| {
            for content in &candidates {
                let path = &path;
                s.spawn(move || write_atomic(path, content).unwrap());
            }
        });
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(
            candidates.iter().any(|c| c == &stored),
            "final content wasn't any single writer's complete output"
        );
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover entries: {leftovers:?}");
    }

    #[test]
    fn write_atomic_if_absent_creates_a_missing_file_and_reports_true() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        let created = write_atomic_if_absent(&path, "hello").unwrap();
        assert!(created);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn write_atomic_if_absent_never_overwrites_an_existing_file_and_reports_false() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "original").unwrap();
        let created = write_atomic_if_absent(&path, "would-be-overwrite").unwrap();
        assert!(!created);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn write_atomic_if_absent_creates_missing_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/c/f.txt");
        let created = write_atomic_if_absent(&path, "nested").unwrap();
        assert!(created);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "nested");
    }

    #[test]
    fn write_atomic_if_absent_leaves_no_leftover_temp_files_whether_created_or_not() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        write_atomic_if_absent(&path, "first").unwrap();
        write_atomic_if_absent(&path, "second").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "leftover entries: {leftovers:?}");
    }

    /// A deterministic creation race: many threads race to
    /// `write_atomic_if_absent` the same path with distinct,
    /// easily-distinguished contents. Exactly one call across the whole
    /// race must report `created == true`, and the file's final content
    /// must be exactly that one winner's content -- proving the
    /// create-if-absent semantics hold under real concurrency, not just
    /// when called strictly before-or-after an existence check.
    #[test]
    fn write_atomic_if_absent_under_concurrent_racing_writers_exactly_one_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.txt");
        let candidates: Vec<String> = (0..16).map(|i| format!("winner-{i}")).collect();

        let results: Vec<(String, bool)> = std::thread::scope(|s| {
            #[allow(
                clippy::needless_collect,
                reason = "Spawn every worker before joining any of them so the test exercises concurrent execution"
            )]
            let handles: Vec<_> = candidates
                .iter()
                .map(|content| {
                    let path = &path;
                    s.spawn(move || {
                        (
                            content.clone(),
                            write_atomic_if_absent(path, content).unwrap(),
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let winners: Vec<&(String, bool)> =
            results.iter().filter(|(_, created)| *created).collect();
        assert_eq!(
            winners.len(),
            1,
            "exactly one racing writer must create the file, got: {results:?}"
        );
        let stored = std::fs::read_to_string(&path).unwrap();
        assert_eq!(stored, winners[0].0);
    }
}
