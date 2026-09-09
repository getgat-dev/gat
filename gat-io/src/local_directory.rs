//! Lazy initialization of repository-owned, self-ignoring local storage.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Filesystem preparation failures retain their target at the owning boundary.
/// Callers translate the structured fields into their subsystem error type.
#[derive(Debug)]
pub(crate) struct PreparationError {
    pub(crate) path: PathBuf,
    pub(crate) source: std::io::Error,
}

impl PreparationError {
    pub(crate) fn at(path: &Path, source: std::io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Shared by the layout and its derived writing capabilities. Only successful
/// initialization is memoized; a fresh repository handle repairs a deleted
/// ignore file. No process-global path cache or per-object filesystem checks.
#[derive(Debug)]
pub(crate) struct LocalDirectory {
    path: PathBuf,
    initialized: AtomicBool,
    initialization: Mutex<()>,
    lock_identity: OnceLock<Arc<PathBuf>>,
    #[cfg(test)]
    inspections: std::sync::atomic::AtomicUsize,
}

impl LocalDirectory {
    #[cfg(test)]
    pub(crate) fn inspection_count(&self) -> usize {
        self.inspections.load(Ordering::Relaxed)
    }

    pub(crate) const fn new(path: PathBuf) -> Self {
        Self {
            path,
            initialized: AtomicBool::new(false),
            initialization: Mutex::new(()),
            lock_identity: OnceLock::new(),
            #[cfg(test)]
            inspections: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// A stable repository-lock key shared by layout clones. Resolving aliases
    /// once avoids filesystem work on nested mutation-lock acquisitions.
    pub(crate) fn lock_identity(&self) -> std::io::Result<Arc<PathBuf>> {
        if let Some(identity) = self.lock_identity.get() {
            return Ok(Arc::clone(identity));
        }
        let _guard = self
            .initialization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(identity) = self.lock_identity.get() {
            return Ok(Arc::clone(identity));
        }
        let identity = Arc::new(std::fs::canonicalize(&self.path)?);
        let _ = self.lock_identity.set(Arc::clone(&identity));
        Ok(identity)
    }

    pub(crate) fn ensure(&self) -> Result<(), PreparationError> {
        if self.is_initialized() {
            return Ok(());
        }
        // Serialize cold callers sharing this capability without locking the
        // warmed path. Failed attempts remain retryable. After a panic, inspect
        // the filesystem again rather than trusting a partially run initializer.
        let _guard = self
            .initialization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_initialized() {
            return Ok(());
        }
        #[cfg(test)]
        self.inspections.fetch_add(1, Ordering::Relaxed);
        ensure_directory(&self.path).map_err(|source| PreparationError::at(&self.path, source))?;
        let ignore = self.path.join(".gitignore");
        ensure_ignore(&self.path, &ignore)
            .map_err(|source| PreparationError::at(&ignore, source))?;
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }
}

fn ensure_directory(path: &Path) -> std::io::Result<()> {
    // A .gitignore inside a symlink target cannot hide the .gat symlink
    // entry in the worktree. Local storage must be an actual directory.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(std::io::ErrorKind::InvalidData.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(path),
        Err(error) => Err(error),
    }
}

fn ensure_ignore(directory: &Path, ignore: &Path) -> std::io::Result<()> {
    if !valid_ignore(ignore)? {
        // Publish complete bytes without replacing another initializer's
        // file. A failed write cannot leave an empty .gitignore behind.
        let mut tmp = tempfile::Builder::new()
            .prefix(".gitignore-")
            .tempfile_in(directory)?;
        tmp.write_all(b"*\n")?;
        tmp.as_file().sync_all()?;
        match tmp.persist_noclobber(ignore) {
            Ok(_) => crate::atomic::sync_dir(directory),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !valid_ignore(ignore)? {
                    return Err(std::io::ErrorKind::NotFound.into());
                }
            }
            Err(error) => return Err(error.error),
        }
    }
    Ok(())
}

fn valid_ignore(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.is_file() => {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        Ok(_) => {}
    }
    let mut bytes = [0; 4];
    let mut file = std::fs::File::open(path)?;
    let mut len = 0;
    while len < bytes.len() {
        match file.read(&mut bytes[len..]) {
            Ok(0) => break,
            Ok(count) => len += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    if !matches!(&bytes[..len], b"*" | b"*\n" | b"*\r\n") {
        // This file is the storage invariant, not a user-editable ignore list.
        // Refuse conflicting contents rather than silently overwriting them.
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_failures_distinguish_storage_from_its_ignore_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".gat");
        let directory = LocalDirectory::new(path.clone());
        std::fs::write(&path, b"obstruction").unwrap();
        let error = directory.ensure().unwrap_err();
        assert_eq!(error.path, path);
        assert_eq!(error.source.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let ignore = path.join(".gitignore");
        std::fs::write(&ignore, b"!keep\n").unwrap();
        let error = directory.ensure().unwrap_err();
        assert_eq!(error.path, ignore);
        assert_eq!(error.source.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn shared_cold_initialization_runs_once_even_with_concurrent_writers() {
        let temp = tempfile::tempdir().unwrap();
        let directory = LocalDirectory::new(temp.path().join(".gat"));
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    directory.ensure().unwrap();
                });
            }
        });
        assert_eq!(directory.inspection_count(), 1);
    }

    #[test]
    fn equivalent_single_rule_line_endings_are_preserved() {
        for contents in [b"*".as_slice(), b"*\n", b"*\r\n"] {
            let temp = tempfile::tempdir().unwrap();
            let directory = LocalDirectory::new(temp.path().join(".gat"));
            std::fs::create_dir(directory.path()).unwrap();
            let ignore = directory.path().join(".gitignore");
            std::fs::write(&ignore, contents).unwrap();
            directory.ensure().unwrap();
            assert_eq!(std::fs::read(ignore).unwrap(), contents);
        }
    }

    #[test]
    fn concurrent_initializers_publish_complete_ignore_and_leave_no_scratch_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".gat");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    let directory = LocalDirectory::new(path.clone());
                    barrier.wait();
                    directory.ensure().unwrap();
                    assert_eq!(std::fs::read(path.join(".gitignore")).unwrap(), b"*\n");
                });
            }
        });
        assert_eq!(std::fs::read_dir(path).unwrap().count(), 1);
    }

    #[test]
    fn failed_initialization_can_be_retried_without_overwriting_conflicting_contents() {
        let temp = tempfile::tempdir().unwrap();
        let directory = LocalDirectory::new(temp.path().join(".gat"));
        std::fs::create_dir(directory.path()).unwrap();
        let ignore = directory.path().join(".gitignore");
        std::fs::write(&ignore, b"!important\n").unwrap();
        assert_eq!(
            directory.ensure().unwrap_err().source.kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(&ignore).unwrap(), b"!important\n");
        std::fs::remove_file(&ignore).unwrap();
        directory.ensure().unwrap();
        assert_eq!(std::fs::read(ignore).unwrap(), b"*\n");
    }

    #[cfg(unix)]
    #[test]
    fn ignore_symlinks_are_rejected_without_touching_the_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::write(&target, b"*\n").unwrap();
        let directory = LocalDirectory::new(temp.path().join(".gat"));
        std::fs::create_dir(directory.path()).unwrap();
        std::os::unix::fs::symlink(&target, directory.path().join(".gitignore")).unwrap();
        assert_eq!(
            directory.ensure().unwrap_err().source.kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read(target).unwrap(), b"*\n");
    }

    #[cfg(unix)]
    #[test]
    fn local_storage_symlinks_are_rejected_without_writing_through_them() {
        let temp = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let directory = LocalDirectory::new(temp.path().join(".gat"));
        std::os::unix::fs::symlink(target.path(), directory.path()).unwrap();
        assert_eq!(
            directory.ensure().unwrap_err().source.kind(),
            std::io::ErrorKind::InvalidData
        );
        assert!(!target.path().join(".gitignore").exists());
    }
}
