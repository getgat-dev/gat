//! Shared cache capabilities and lazy local-storage ownership resolution.

use crate::local_directory::PreparationError;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// A resolved local object-cache capability.
///
/// The physical cache layout stays inside `gat-io`; higher layers retain
/// this cheap-to-clone handle and ask it for the specific cache capability
/// they need.
#[derive(Clone, Debug)]
pub struct CacheRoot {
    inner: Arc<CacheRootInner>,
}

#[derive(Debug)]
pub(crate) struct CacheRootInner {
    pub(crate) objects_dir: PathBuf,
    local_directory: Option<Arc<crate::local_directory::LocalDirectory>>,
    local_membership: OnceLock<bool>,
    membership_resolution: Mutex<()>,
    #[cfg(test)]
    resolutions: std::sync::atomic::AtomicUsize,
}

/// Evidence that this cache's repository protection has been prepared.
/// The object directory itself may still be absent.
/// Borrowed from the operation capability; constructing it is private to this module.
#[derive(Debug)]
pub(crate) struct PreparedCacheDirectory<'cache> {
    path: &'cache Path,
}

impl<'cache> PreparedCacheDirectory<'cache> {
    pub(crate) const fn path(&self) -> &'cache Path {
        self.path
    }
}

impl CacheRootInner {
    pub(crate) fn new(
        objects_dir: PathBuf,
        local_directory: Option<Arc<crate::local_directory::LocalDirectory>>,
    ) -> Self {
        // Classification belongs to the cache capability. Callers supply paths,
        // never an independently asserted protection flag.
        let local_membership = match &local_directory {
            None => OnceLock::from(false),
            Some(directory) if lexically_within(&objects_dir, directory.path()) => {
                OnceLock::from(true)
            }
            Some(_) => OnceLock::new(),
        };
        Self {
            objects_dir,
            local_directory,
            local_membership,
            membership_resolution: Mutex::new(()),
            #[cfg(test)]
            resolutions: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn prepare_write(&self) -> crate::CacheResult<()> {
        self.prepare_directory().map(|_| ()).map_err(|error| {
            crate::CacheError::DirectoryUnavailable {
                path: error.path,
                source: error.source,
            }
        })
    }

    /// Resolve custom locations only when needed, after creating directories
    /// but before creating any files. Canonical paths handle relative overrides,
    /// parent components and symlink aliases without changing display paths or
    /// creating repository-local storage for an external cache.
    pub(crate) fn prepare_directory(&self) -> Result<PreparedCacheDirectory<'_>, PreparationError> {
        self.ensure_local_directory(true)?;
        Ok(PreparedCacheDirectory {
            path: &self.objects_dir,
        })
    }

    /// Before ownership is established, an absent or inaccessible cache must not
    /// fall through to a writable `SQLite` open. Once prepared, handles assume
    /// stable directory ownership for their lifetime and skip repeated probes.
    pub(crate) fn prepare_existing_directory(
        &self,
    ) -> Result<Option<PreparedCacheDirectory<'_>>, PreparationError> {
        if self.local_membership.get().is_some_and(|local| {
            !local
                || self
                    .local_directory
                    .as_ref()
                    .is_some_and(|directory| directory.is_initialized())
        }) {
            return Ok(Some(PreparedCacheDirectory {
                path: &self.objects_dir,
            }));
        }
        if !self.objects_dir.is_dir() {
            return Ok(None);
        }
        self.ensure_local_directory(false)?;
        Ok(Some(PreparedCacheDirectory {
            path: &self.objects_dir,
        }))
    }

    fn ensure_local_directory(&self, create: bool) -> Result<(), PreparationError> {
        let is_local = self.resolve_membership(create)?;
        if is_local && let Some(directory) = &self.local_directory {
            directory.ensure()?;
        }
        Ok(())
    }

    fn resolve_membership(&self, create: bool) -> Result<bool, PreparationError> {
        let Some(directory) = &self.local_directory else {
            return Ok(false);
        };
        let is_local = if let Some(is_local) = self.local_membership.get() {
            *is_local
        } else {
            let _guard = self
                .membership_resolution
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(is_local) = self.local_membership.get() {
                *is_local
            } else {
                #[cfg(test)]
                self.resolutions
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let is_local = if lexically_within(
                    &std::path::absolute(&self.objects_dir)
                        .map_err(|source| PreparationError::at(&self.objects_dir, source))?,
                    &std::path::absolute(directory.path())
                        .map_err(|source| PreparationError::at(directory.path(), source))?,
                ) {
                    true
                } else {
                    if create {
                        std::fs::create_dir_all(&self.objects_dir)
                            .map_err(|source| PreparationError::at(&self.objects_dir, source))?;
                    }
                    let objects = std::fs::canonicalize(&self.objects_dir)
                        .map_err(|source| PreparationError::at(&self.objects_dir, source))?;
                    match std::fs::canonicalize(directory.path()) {
                        Ok(local) => objects.starts_with(local),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                        Err(error) => return Err(PreparationError::at(directory.path(), error)),
                    }
                };
                let _ = self.local_membership.set(is_local);
                is_local
            }
        };
        Ok(is_local)
    }
}

impl CacheRoot {
    pub(crate) fn new(
        objects_dir: PathBuf,
        directory: Arc<crate::local_directory::LocalDirectory>,
    ) -> Self {
        Self {
            inner: Arc::new(CacheRootInner::new(objects_dir, Some(directory))),
        }
    }

    #[must_use]
    pub fn open_client(&self) -> crate::CacheClient {
        crate::CacheClient::open_shared(Arc::clone(&self.inner))
    }

    #[must_use]
    pub fn writer(&self) -> crate::CacheWriter {
        crate::CacheWriter::new(Arc::clone(&self.inner))
    }

    #[must_use]
    pub fn presence(&self) -> crate::CachePresence {
        crate::CachePresence::new(Arc::clone(&self.inner))
    }

    #[must_use]
    pub fn maintenance(&self) -> crate::CacheMaintenance<'_> {
        crate::CacheMaintenance::new(&self.inner)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    #[must_use]
    pub fn object_path_for_test(&self, oid: &gat_core::oid::Oid) -> PathBuf {
        crate::cache::object::cache_path_oid(&self.inner.objects_dir, oid)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn make_object_writable_for_test(
        &self,
        oid: &gat_core::oid::Oid,
    ) -> crate::CacheResult<()> {
        crate::cache::object::unprotect(&self.object_path_for_test(oid))
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn break_database_for_test(&self) {
        let client = crate::cache::object::CacheClient::open(self.inner.objects_dir.clone());
        client.break_database_for_test();
    }

    /// The resolved host path for user-facing presentation only.
    #[must_use]
    pub fn display_path(&self) -> &Path {
        &self.inner.objects_dir
    }
}

/// A logical descendant still needs its parent ignored when its final cache
/// directory is a symlink pointing elsewhere. Parent components need physical
/// resolution; a textual prefix alone must not classify those paths as local.
fn lexically_within(path: &Path, directory: &Path) -> bool {
    path.strip_prefix(directory).is_ok_and(|suffix| {
        !suffix
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    })
}

#[cfg(test)]
mod tests {

    #[cfg(unix)]
    #[test]
    fn failed_local_path_resolution_preserves_its_path_and_can_be_retried() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        let local = temp.path().join(".gat");
        std::os::unix::fs::symlink(&local, &local).unwrap();
        let external = tempfile::tempdir().unwrap();
        let cache = layout.resolve_cache_root(Some(
            &gat_core::cache_location::CacheLocation::try_from_path(std::path::PathBuf::from(
                external.path().as_os_str(),
            ))
            .expect("nonempty fixture cache path"),
        ));
        let error = cache.inner.prepare_directory().unwrap_err();
        assert_eq!(error.path, local);
        assert!(cache.inner.local_membership.get().is_none());
        std::fs::remove_file(&local).unwrap();
        cache.writer().ingest(&b"content"[..]).unwrap();
        assert!(!local.exists());
    }

    #[test]
    fn preparation_errors_identify_external_cache_and_local_storage_separately() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().join("repository"));
        let external = temp.path().join("external");
        std::fs::write(&external, b"obstruction").unwrap();
        let root = layout.resolve_cache_root(Some(
            &gat_core::cache_location::CacheLocation::try_from_path(std::path::PathBuf::from(
                external.as_os_str(),
            ))
            .expect("nonempty fixture cache path"),
        ));
        let crate::CacheError::DirectoryUnavailable { path, .. } =
            root.writer().begin_ingest().err().unwrap()
        else {
            panic!("expected directory preparation failure");
        };
        assert_eq!(path, external);
        let crate::CacheMaintenanceError::Io { path, .. } =
            root.maintenance().rebuild_database().unwrap_err()
        else {
            panic!("expected directory preparation failure");
        };
        assert_eq!(path, external);
        assert!(!layout.cache_root_path().exists());

        std::fs::create_dir_all(layout.cache_root_path()).unwrap();
        std::fs::write(layout.cache_root_path().join(".gitignore"), b"!keep\n").unwrap();
        let root = layout.resolve_cache_root(None);
        let crate::CacheError::DirectoryUnavailable { path, source } =
            root.writer().begin_ingest().err().unwrap()
        else {
            panic!("expected local storage preparation failure");
        };
        assert_eq!(path, layout.cache_root_path().join(".gitignore"));
        assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
        let crate::CacheMaintenanceError::Io { path, .. } =
            root.maintenance().rebuild_database().unwrap_err()
        else {
            panic!("expected local storage preparation failure");
        };
        assert_eq!(path, layout.cache_root_path().join(".gitignore"));
    }

    #[test]
    fn concurrent_custom_cache_preparation_resolves_and_initializes_once() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        let objects = temp.path().join("intermediate/../.gat/objects");
        let root = layout.resolve_cache_root(Some(
            &gat_core::cache_location::CacheLocation::try_from_path(std::path::PathBuf::from(
                objects.as_os_str(),
            ))
            .expect("nonempty fixture cache path"),
        ));
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let inner = std::sync::Arc::clone(&root.inner);
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    inner.prepare_directory().unwrap();
                });
            }
        });
        assert_eq!(
            root.inner
                .resolutions
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(layout.local_directory().inspection_count(), 1);
        assert_eq!(
            std::fs::read(temp.path().join(".gat/.gitignore")).unwrap(),
            b"*\n"
        );
    }

    #[test]
    fn custom_cache_ownership_is_resolved_once_before_any_files_are_written() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        let objects = temp.path().join("intermediate/../.gat/objects");
        let root = layout.resolve_cache_root(Some(
            &gat_core::cache_location::CacheLocation::try_from_path(std::path::PathBuf::from(
                objects.as_os_str(),
            ))
            .expect("nonempty fixture cache path"),
        ));
        assert!(!temp.path().join(".gat").exists());
        for _ in 0..32 {
            root.writer().ingest(&b"content"[..]).unwrap();
        }
        assert_eq!(
            root.inner
                .resolutions
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(layout.local_directory().inspection_count(), 1);
        assert_eq!(
            std::fs::read(temp.path().join(".gat/.gitignore")).unwrap(),
            b"*\n"
        );
    }

    #[test]
    fn absent_cache_gate_never_authorizes_a_later_writable_open() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        let root = layout.resolve_cache_root(None);
        let ready = root.inner.prepare_existing_directory().unwrap();
        // Model a directory appearing after the probe without a self-ignore
        // file: the captured decision must still prevent a SQLite open.
        std::fs::create_dir_all(root.display_path()).unwrap();
        assert!(ready.is_none());
        assert!(!root.display_path().join("cache.sqlite3").exists());
        let _ = root.open_client();
        assert!(temp.path().join(".gat/.gitignore").is_file());
        assert!(root.display_path().join("cache.sqlite3").is_file());
    }

    #[test]
    fn failed_ignore_validation_disables_proof_writes_and_remains_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        std::fs::create_dir_all(temp.path().join(".gat/objects")).unwrap();
        let ignore = temp.path().join(".gat/.gitignore");
        std::fs::write(&ignore, b"!unignore\n").unwrap();
        let root = layout.resolve_cache_root(None);
        let _ = root.open_client();
        assert!(!root.display_path().join("cache.sqlite3").exists());
        assert!(root.writer().begin_ingest().is_err());
        std::fs::remove_file(ignore).unwrap();
        root.writer().begin_ingest().unwrap();
        let _ = root.open_client();
        assert!(root.display_path().join("cache.sqlite3").is_file());
    }
}
