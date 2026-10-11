//! Directory metadata durability, independently of mandatory file-content sync.

use std::{io, path::Path};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    #[cfg(any(unix, test))]
    Synced,
    Unsupported,
}

/// Unsupported directory sync permits publication, but cannot guarantee that
/// the new directory entry survives power loss. Never apply this policy to
/// regular files or infer support from the operating system or mount path.
pub(crate) fn sync(path: &Path) -> io::Result<Outcome> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // Validate the directory as part of open, avoiding a separate metadata
        // lookup (which may require a round trip on network filesystems).
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(path)?;
        classify(directory.sync_all())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(Outcome::Unsupported)
    }
}

#[cfg(any(unix, test))]
pub(crate) fn classify(result: io::Result<()>) -> io::Result<Outcome> {
    match result {
        Ok(()) => Ok(Outcome::Synced),
        // EINVAL is used for descriptors whose filesystem does not implement
        // directory fsync. Only classify the sync syscall, never open/metadata
        // errors. In particular, EIO from network-backed DrvFs is not evidence
        // of an unsupported operation and must remain an error.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) =>
        {
            Ok(Outcome::Unsupported)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unsupported_sync_is_tolerated() {
        assert_eq!(classify(Ok(())).unwrap(), Outcome::Synced);
        for kind in [io::ErrorKind::InvalidInput, io::ErrorKind::Unsupported] {
            assert_eq!(classify(Err(kind.into())).unwrap(), Outcome::Unsupported);
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::StorageFull,
            io::ErrorKind::NotFound,
            io::ErrorKind::Other,
        ] {
            assert_eq!(classify(Err(kind.into())).unwrap_err().kind(), kind);
        }
    }

    #[cfg(unix)]
    #[test]
    fn io_errors_retain_the_original_errno() {
        let error = classify(Err(io::Error::from_raw_os_error(5))).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5)); // EIO on Unix
    }

    #[cfg(unix)]
    #[test]
    fn rejects_missing_paths_and_regular_files() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            sync(&root.path().join("missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let file = root.path().join("file");
        std::fs::write(&file, b"data").unwrap();
        assert_eq!(
            sync(&file).unwrap_err().kind(),
            io::ErrorKind::NotADirectory
        );
        assert!(sync(root.path()).is_ok());
    }
}
