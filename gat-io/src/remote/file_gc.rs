//! Bounded synchronous file inventory/deletion, driven by admitted GC workers.

use super::{RemoteClient, RemoteError, classify_opendal_error};
use gat_core::oid::Oid;
use std::{fs::ReadDir, io, path::PathBuf};

pub const FILE_GC_BATCH_SIZE: usize = 128;

pub struct FileObjectScan {
    root: PathBuf,
    started: bool,
    // Namespace plus two fan-out directories: at most three live handles.
    stack: Vec<(ReadDir, String)>,
}

pub struct FileDeleteBatch(Vec<PathBuf>);

pub struct FileDeleteOutcome {
    pub confirmed: usize,
    pub error: Option<RemoteError>,
}

fn remote_error(error: io::Error) -> RemoteError {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => opendal::ErrorKind::NotFound,
        io::ErrorKind::PermissionDenied => opendal::ErrorKind::PermissionDenied,
        _ => opendal::ErrorKind::Unexpected,
    };
    classify_opendal_error(opendal::Error::new(kind, "file GC failed").set_source(error))
}

impl RemoteClient {
    #[must_use]
    pub fn prepare_file_listing(&self) -> Option<FileObjectScan> {
        let info = self.operator.info();
        (info.scheme() == "fs").then(|| FileObjectScan {
            root: PathBuf::from(info.root()).join(crate::cache::OBJECT_HASH_NAMESPACE),
            started: false,
            stack: Vec::new(),
        })
    }

    /// # Panics
    /// Panics if `oids` exceeds [`FILE_GC_BATCH_SIZE`].
    #[must_use]
    pub fn prepare_file_delete(&self, oids: &[Oid]) -> Option<FileDeleteBatch> {
        let info = self.operator.info();
        (info.scheme() == "fs").then(|| {
            assert!(oids.len() <= FILE_GC_BATCH_SIZE);
            let root = PathBuf::from(info.root());
            FileDeleteBatch(
                oids.iter()
                    .map(|oid| root.join(crate::cache::object_key_oid(oid)))
                    .collect(),
            )
        })
    }
}

impl FileObjectScan {
    /// No object stats, timestamps or content opens. Only canonical fan-out
    /// directories are traversed; symlinks and non-object names are ignored.
    /// Parent substitution by another process remains outside the contract.
    /// An empty batch is not EOF: invalid entries also consume the work budget.
    pub fn next_batch(&mut self) -> Result<Option<Vec<Oid>>, RemoteError> {
        self.read_batch().map_err(remote_error)
    }

    fn read_batch(&mut self) -> io::Result<Option<Vec<Oid>>> {
        if !self.started {
            self.started = true;
            match std::fs::symlink_metadata(&self.root) {
                Ok(metadata) if metadata.is_dir() => {
                    self.stack.push((
                        std::fs::read_dir(&self.root)?,
                        crate::cache::OBJECT_HASH_NAMESPACE.to_owned(),
                    ));
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid object namespace",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        let mut objects = Vec::with_capacity(FILE_GC_BATCH_SIZE);
        let mut visited = 0;
        while let Some((directory, prefix)) = self.stack.last_mut() {
            if visited == FILE_GC_BATCH_SIZE {
                return Ok(Some(objects));
            }
            let Some(entry) = directory.next() else {
                self.stack.pop();
                continue;
            };
            let entry = entry?;
            visited += 1;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let key = format!("{prefix}/{name}");
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if self.stack.len() < 3 {
                if kind.is_dir()
                    && name.len() == 2
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    match std::fs::read_dir(entry.path()) {
                        Ok(directory) => self.stack.push((directory, key)),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
            } else if kind.is_file()
                && let Some(oid) = crate::cache::parse_object_key(&key)
            {
                objects.push(oid);
                if objects.len() == FILE_GC_BATCH_SIZE {
                    break;
                }
            }
        }
        Ok((visited != 0).then_some(objects))
    }
}

impl FileDeleteBatch {
    /// Each successful unlink (or already-missing object) is confirmed. The
    /// first failure stops the batch; later paths are untouched. `remove_file`
    /// cannot recursively remove a directory and never follows a leaf symlink.
    #[must_use]
    pub fn delete(self) -> FileDeleteOutcome {
        let mut confirmed = 0;
        for path in self.0 {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return FileDeleteOutcome {
                        confirmed,
                        error: Some(remote_error(error)),
                    };
                }
            }
            confirmed += 1;
        }
        FileDeleteOutcome {
            confirmed,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, RemoteClient) {
        super::super::initialize_backends();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        let client = runtime
            .block_on(async { RemoteClient::open(&super::super::file_url(root.path())).unwrap() });
        (root, client)
    }

    fn oid(index: u16) -> Oid {
        let mut bytes = [0; 32];
        bytes[30..].copy_from_slice(&index.to_be_bytes());
        Oid::from_bytes(bytes)
    }

    fn object(root: &std::path::Path, index: u16) -> PathBuf {
        let path = root.join(crate::cache::object_key_oid(&oid(index)));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"object").unwrap();
        path
    }

    #[test]
    fn listing_bounds_pages_and_handles_and_ignores_invalid_names() {
        let (root, client) = fixture();
        let mut expected = Vec::new();
        let first = object(root.path(), 0);
        for index in 0..260 {
            object(root.path(), index);
            expected.push(oid(index));
            std::fs::write(
                first.parent().unwrap().join(format!(".gat-upload-{index}")),
                b"stage",
            )
            .unwrap();
        }
        let invalid = root.path().join("blake3/zz/00");
        std::fs::create_dir_all(&invalid).unwrap();
        std::fs::write(invalid.join(oid(999).to_string()), b"invalid").unwrap();
        let mut scan = client.prepare_file_listing().unwrap();
        let mut seen = Vec::new();
        let mut pages = 0;
        while let Some(batch) = scan.next_batch().unwrap() {
            assert!(batch.len() <= FILE_GC_BATCH_SIZE);
            assert!(scan.stack.len() <= 3);
            seen.extend(batch);
            pages += 1;
        }
        assert!(
            pages >= 5,
            "invalid entries also consume the batch work budget"
        );
        seen.sort();
        expected.sort();
        assert_eq!(seen, expected);
        assert!(scan.stack.is_empty());
        assert!(scan.next_batch().unwrap().is_none());
    }

    #[test]
    fn missing_namespace_is_empty_but_invalid_namespace_fails_closed() {
        let (root, client) = fixture();
        assert!(
            client
                .prepare_file_listing()
                .unwrap()
                .next_batch()
                .unwrap()
                .is_none()
        );
        std::fs::write(root.path().join("blake3"), b"invalid").unwrap();
        assert!(client.prepare_file_listing().unwrap().next_batch().is_err());
    }

    #[test]
    fn deletion_stops_at_invalid_target_and_confirms_prior_unlinks_and_absence() {
        let (root, client) = fixture();
        let first = object(root.path(), 0);
        let invalid = object(root.path(), 2);
        std::fs::remove_file(&invalid).unwrap();
        std::fs::create_dir(&invalid).unwrap();
        let later = object(root.path(), 3);
        let result = client
            .prepare_file_delete(&[oid(0), oid(1), oid(2), oid(3)])
            .unwrap()
            .delete();
        assert_eq!(result.confirmed, 2);
        assert!(result.error.is_some());
        assert!(!first.exists());
        assert!(invalid.is_dir());
        assert!(later.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn listing_never_traverses_symlinks_and_deletion_never_follows_them() {
        use std::os::unix::fs::symlink;
        let (root, client) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let target = object(outside.path(), 1);
        let leaf = object(root.path(), 1);
        std::fs::remove_file(&leaf).unwrap();
        symlink(&target, &leaf).unwrap();
        symlink(
            outside.path().join("blake3/00"),
            root.path().join("blake3/aa"),
        )
        .unwrap();
        let mut scan = client.prepare_file_listing().unwrap();
        while let Some(batch) = scan.next_batch().unwrap() {
            assert!(batch.is_empty());
        }
        let result = client.prepare_file_delete(&[oid(1)]).unwrap().delete();
        assert_eq!(result.confirmed, 1);
        assert!(result.error.is_none());
        assert!(target.is_file());
        assert!(std::fs::symlink_metadata(&leaf).is_err());
    }
}
