//! Git merge-driver execution over physical stage files and the pure lock merge.

use std::path::{Path, PathBuf};

use gat_core::lock::{Conflict, Lock, LockError as CoreLockError, merge_three_way};
use gat_io::LockStore;

use crate::RepositoryAccessError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStage {
    Ancestor,
    Ours,
    Theirs,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeDriverError {
    #[error("could not read the {stage:?} merge stage at `{}`", path.display())]
    Read {
        stage: MergeStage,
        path: PathBuf,
        #[source]
        source: Box<RepositoryAccessError>,
    },
    #[error("could not parse the {stage:?} merge stage")]
    Parse {
        stage: MergeStage,
        #[source]
        source: CoreLockError,
    },
    #[error("gat.lock semantic merge conflict ({} path(s))", .0.len())]
    SemanticConflict(Vec<Conflict>),
    #[error("could not publish the merged lock at `{}`", path.display())]
    Publish {
        path: PathBuf,
        #[source]
        source: Box<RepositoryAccessError>,
    },
}

fn read_stage(path: &Path, stage: MergeStage) -> Result<Lock, MergeDriverError> {
    let text = LockStore::read_file_if_present(path).map_err(|source| MergeDriverError::Read {
        stage,
        path: path.to_path_buf(),
        source: Box::new(RepositoryAccessError::from_lock(source)),
    })?;
    let Some(text) = text else {
        return Ok(Lock::default());
    };
    if text.trim().is_empty() {
        return Ok(Lock::default());
    }
    Lock::parse(&text).map_err(|source| MergeDriverError::Parse { stage, source })
}

pub fn merge_driver(ancestor: &Path, ours: &Path, theirs: &Path) -> Result<(), MergeDriverError> {
    let ancestor_lock = read_stage(ancestor, MergeStage::Ancestor)?;
    let ours_lock = read_stage(ours, MergeStage::Ours)?;
    let theirs_lock = read_stage(theirs, MergeStage::Theirs)?;

    let merged = merge_three_way(&ancestor_lock, &ours_lock, &theirs_lock)
        .map_err(MergeDriverError::SemanticConflict)?;
    LockStore::publish_file_atomic(&merged, ours).map_err(|source| MergeDriverError::Publish {
        path: ours.to_path_buf(),
        source: Box::new(RepositoryAccessError::from_lock(source)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::lexical_path::GatPath;
    use gat_core::lock::Entry;
    use gat_core::oid::Oid;

    const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn entry(path: &str, oid: &str) -> Entry {
        Entry {
            path: GatPath::parse_canonical(path).unwrap(),
            oid: Oid::from_hex(oid).unwrap(),
        }
    }

    fn write(path: &Path, lock: &Lock) {
        std::fs::write(path, lock.to_string()).unwrap();
    }

    #[test]
    fn clean_merge_publishes_the_union_to_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let ancestor = tmp.path().join("O");
        let ours = tmp.path().join("A");
        let theirs = tmp.path().join("B");
        write(
            &ancestor,
            &Lock {
                entries: vec![entry("a.bin", OID_A)],
            },
        );
        write(
            &ours,
            &Lock {
                entries: vec![entry("a.bin", OID_A), entry("b.bin", OID_B)],
            },
        );
        write(
            &theirs,
            &Lock {
                entries: vec![entry("a.bin", OID_A), entry("c.bin", &"c".repeat(64))],
            },
        );

        merge_driver(&ancestor, &ours, &theirs).unwrap();

        let merged = Lock::parse(&std::fs::read_to_string(ours).unwrap()).unwrap();
        let paths = merged
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["a.bin", "b.bin", "c.bin"]);
    }

    #[test]
    fn conflict_does_not_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let ancestor = tmp.path().join("O");
        let ours = tmp.path().join("A");
        let theirs = tmp.path().join("B");
        write(
            &ancestor,
            &Lock {
                entries: vec![entry("a.bin", OID_A)],
            },
        );
        write(
            &ours,
            &Lock {
                entries: vec![entry("a.bin", OID_B)],
            },
        );
        write(
            &theirs,
            &Lock {
                entries: vec![entry("a.bin", &"c".repeat(64))],
            },
        );
        let before = std::fs::read_to_string(&ours).unwrap();

        let error = merge_driver(&ancestor, &ours, &theirs).unwrap_err();

        assert!(matches!(error, MergeDriverError::SemanticConflict(_)));
        assert_eq!(std::fs::read_to_string(ours).unwrap(), before);
    }

    #[test]
    fn missing_and_empty_stages_are_empty_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing-O");
        let ours = tmp.path().join("A");
        let theirs = tmp.path().join("B");
        std::fs::write(&ours, "").unwrap();
        std::fs::write(&theirs, "").unwrap();

        merge_driver(&missing, &ours, &theirs).unwrap();

        assert_eq!(
            Lock::parse(&std::fs::read_to_string(ours).unwrap())
                .unwrap()
                .entries,
            []
        );
    }

    #[test]
    fn malformed_stage_fails_closed_with_stage_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let ancestor = tmp.path().join("O");
        let ours = tmp.path().join("A");
        let theirs = tmp.path().join("B");
        std::fs::write(&ancestor, "not a lock\n").unwrap();
        std::fs::write(&ours, "").unwrap();
        std::fs::write(&theirs, "").unwrap();

        let error = merge_driver(&ancestor, &ours, &theirs).unwrap_err();

        assert!(matches!(
            error,
            MergeDriverError::Parse {
                stage: MergeStage::Ancestor,
                ..
            }
        ));
    }
}
