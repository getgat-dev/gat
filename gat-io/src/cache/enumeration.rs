use super::layout::{OBJECT_HASH_NAMESPACE, parse_object_key};
use super::object::object_namespace_dir;
use super::proof::{CacheProofError, CacheState};
use gat_core::oid::Oid;
use std::path::{Path, PathBuf};

const PROOF_REMOVAL_BATCH: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSweepDecision {
    Keep,
    Delete,
    Uncertain,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheSweepStats {
    pub deleted: usize,
    pub uncertain: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheEnumerationError {
    #[error("could not {operation} `{}`", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Proof(#[from] CacheProofError),
}

impl CacheEnumerationError {
    fn io(operation: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

fn directory_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>, CacheEnumerationError> {
    // Complete each directory listing before mutating its entries. This also
    // avoids depending on directory-iterator behavior during deletion.
    std::fs::read_dir(dir)
        .map_err(|source| CacheEnumerationError::io("read", dir, source))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|source| CacheEnumerationError::io("read", dir, source))
}

/// Enumerate the typed local object namespace in unspecified order and optionally
/// delete objects selected by the caller. Physical fan-out paths, malformed
/// leaves, directory cleanup, and proof-row removal remain internal.
pub fn sweep_objects<E>(
    objects_dir: &Path,
    dry_run: bool,
    mut decide: impl FnMut(Oid) -> Result<CacheSweepDecision, E>,
) -> Result<Result<CacheSweepStats, E>, CacheEnumerationError> {
    let namespace = object_namespace_dir(objects_dir);
    if !namespace.exists() {
        return Ok(Ok(CacheSweepStats::default()));
    }
    let state = (!dry_run).then(|| CacheState::open(objects_dir));
    let mut removed = Vec::with_capacity(PROOF_REMOVAL_BATCH);
    let mut stats = CacheSweepStats::default();

    for l1 in directory_entries(&namespace)? {
        let l1_path = l1.path();
        if !l1
            .file_type()
            .map_err(|source| CacheEnumerationError::io("stat", &l1_path, source))?
            .is_dir()
        {
            continue;
        }
        let mut l1_nonempty = false;
        for l2 in directory_entries(&l1_path)? {
            let l2_path = l2.path();
            if !l2
                .file_type()
                .map_err(|source| CacheEnumerationError::io("stat", &l2_path, source))?
                .is_dir()
            {
                l1_nonempty = true;
                continue;
            }
            let mut l2_nonempty = false;
            for leaf in directory_entries(&l2_path)? {
                let key = format!(
                    "{OBJECT_HASH_NAMESPACE}/{}/{}/{}",
                    l1.file_name().to_string_lossy(),
                    l2.file_name().to_string_lossy(),
                    leaf.file_name().to_string_lossy()
                );
                let Some(oid) = parse_object_key(&key) else {
                    l2_nonempty = true;
                    continue;
                };
                let decision = match decide(oid) {
                    Ok(decision) => decision,
                    Err(error) => return Ok(Err(error)),
                };
                match decision {
                    CacheSweepDecision::Keep => l2_nonempty = true,
                    CacheSweepDecision::Uncertain => {
                        stats.uncertain += 1;
                        l2_nonempty = true;
                    }
                    CacheSweepDecision::Delete => {
                        stats.deleted += 1;
                        if dry_run {
                            l2_nonempty = true;
                        } else {
                            let path = leaf.path();
                            std::fs::remove_file(&path).map_err(|source| {
                                CacheEnumerationError::io("remove", path, source)
                            })?;
                            removed.push(oid);
                            if removed.len() == PROOF_REMOVAL_BATCH {
                                remove_proofs(state.as_ref(), &mut removed)?;
                            }
                        }
                    }
                }
            }
            if !dry_run {
                if l2_nonempty {
                    l1_nonempty = true;
                } else {
                    remove_dir_if_empty(&l2_path)?;
                }
            }
        }
        if !dry_run && !l1_nonempty {
            remove_dir_if_empty(&l1_path)?;
        }
    }
    remove_proofs(state.as_ref(), &mut removed)?;
    Ok(Ok(stats))
}

fn remove_proofs(
    state: Option<&CacheState>,
    removed: &mut Vec<Oid>,
) -> Result<(), CacheEnumerationError> {
    if let Some(state) = state {
        state.remove_many(removed).map_err(CacheProofError::from)?;
    }
    removed.clear();
    Ok(())
}

fn remove_dir_if_empty(dir: &Path) -> Result<(), CacheEnumerationError> {
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_remove_dir_attempt();
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(source) => Err(CacheEnumerationError::io("remove", dir, source)),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static REMOVE_DIR_ATTEMPT_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset_remove_dir_attempt_count() {
        REMOVE_DIR_ATTEMPT_COUNT.with(|count| count.set(0));
    }

    pub fn remove_dir_attempt_count() -> usize {
        REMOVE_DIR_ATTEMPT_COUNT.with(Cell::get)
    }

    pub(super) fn record_remove_dir_attempt() {
        REMOVE_DIR_ATTEMPT_COUNT.with(|count| count.set(count.get() + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::object::{cache_path_oid, has_object_oid, ingest, object_namespace_dir};
    use crate::cache::{cache_has_proof_for_test, seed_cache_proof_for_test};
    use crate::file_state;

    #[test]
    fn sweep_removes_only_deleted_objects_and_empty_fanout_directories() {
        let temp = tempfile::tempdir().unwrap();
        let kept = ingest(temp.path(), std::io::Cursor::new(b"kept"))
            .unwrap()
            .oid;
        let deleted = ingest(temp.path(), std::io::Cursor::new(b"deleted"))
            .unwrap()
            .oid;
        let deleted_path = cache_path_oid(temp.path(), &deleted);
        let l2 = deleted_path.parent().unwrap().to_path_buf();
        let l1 = l2.parent().unwrap().to_path_buf();
        test_support::reset_remove_dir_attempt_count();

        let stats = sweep_objects::<std::convert::Infallible>(temp.path(), false, |oid| {
            Ok(if oid == kept {
                CacheSweepDecision::Keep
            } else {
                CacheSweepDecision::Delete
            })
        })
        .unwrap()
        .unwrap();

        assert_eq!(stats.deleted, 1);
        assert!(has_object_oid(temp.path(), &kept));
        assert!(!has_object_oid(temp.path(), &deleted));
        assert!(!l2.exists());
        assert!(!l1.exists());
        assert_eq!(test_support::remove_dir_attempt_count(), 2);
    }

    #[test]
    fn keep_only_and_dry_run_sweeps_attempt_no_directory_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        ingest(temp.path(), std::io::Cursor::new(b"kept")).unwrap();
        test_support::reset_remove_dir_attempt_count();
        sweep_objects::<std::convert::Infallible>(temp.path(), false, |_| {
            Ok(CacheSweepDecision::Keep)
        })
        .unwrap()
        .unwrap();
        assert_eq!(test_support::remove_dir_attempt_count(), 0);

        sweep_objects::<std::convert::Infallible>(temp.path(), true, |_| {
            Ok(CacheSweepDecision::Delete)
        })
        .unwrap()
        .unwrap();
        assert_eq!(test_support::remove_dir_attempt_count(), 0);
    }

    #[test]
    fn malformed_fanout_leaf_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let kept = ingest(temp.path(), std::io::Cursor::new(b"kept"))
            .unwrap()
            .oid;
        let malformed_dir = object_namespace_dir(temp.path()).join("00").join("00");
        std::fs::create_dir_all(&malformed_dir).unwrap();
        let malformed = malformed_dir.join("ff".repeat(32));
        std::fs::write(&malformed, b"not a real object").unwrap();
        let mut visited = Vec::new();

        sweep_objects::<std::convert::Infallible>(temp.path(), false, |oid| {
            visited.push(oid);
            Ok(CacheSweepDecision::Keep)
        })
        .unwrap()
        .unwrap();

        assert_eq!(visited, vec![kept]);
        assert!(malformed.is_file());
        assert!(has_object_oid(temp.path(), &kept));
    }

    #[test]
    fn sweep_removes_deleted_proofs_and_preserves_kept_proofs() {
        let temp = tempfile::tempdir().unwrap();
        let kept = ingest(temp.path(), std::io::Cursor::new(b"kept"))
            .unwrap()
            .oid;
        let deleted = ingest(temp.path(), std::io::Cursor::new(b"deleted"))
            .unwrap()
            .oid;
        for oid in [kept, deleted] {
            let proof =
                file_state::observe_regular_file_no_follow(&cache_path_oid(temp.path(), &oid))
                    .unwrap();
            seed_cache_proof_for_test(temp.path(), &oid, &proof);
        }

        sweep_objects::<std::convert::Infallible>(temp.path(), false, |oid| {
            Ok(if oid == kept {
                CacheSweepDecision::Keep
            } else {
                CacheSweepDecision::Delete
            })
        })
        .unwrap()
        .unwrap();

        assert!(cache_has_proof_for_test(temp.path(), &kept));
        assert!(!cache_has_proof_for_test(temp.path(), &deleted));
    }

    #[cfg(unix)]
    #[test]
    fn sweep_ignores_database_sidecars_outside_the_object_namespace() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let deleted = ingest(temp.path(), std::io::Cursor::new(b"deleted"))
            .unwrap()
            .oid;
        let database = temp.path().join("cache.sqlite3");
        std::fs::write(&database, "sentinel:cache.sqlite3").unwrap();
        let original_mode = std::fs::metadata(&database).unwrap().permissions().mode();
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o000)).unwrap();
        for name in ["cache.sqlite3", "cache.sqlite3-wal", "cache.sqlite3-shm"] {
            if name != "cache.sqlite3" {
                std::fs::write(temp.path().join(name), format!("sentinel:{name}")).unwrap();
            }
        }

        sweep_objects::<std::convert::Infallible>(temp.path(), false, |_| {
            Ok(CacheSweepDecision::Delete)
        })
        .unwrap()
        .unwrap();

        assert!(!has_object_oid(temp.path(), &deleted));
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(original_mode))
            .unwrap();
        for name in ["cache.sqlite3", "cache.sqlite3-wal", "cache.sqlite3-shm"] {
            assert_eq!(
                std::fs::read_to_string(temp.path().join(name)).unwrap(),
                format!("sentinel:{name}")
            );
        }
    }
}
