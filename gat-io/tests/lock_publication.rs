use gat_core::{
    lexical_path::GatPath,
    lock::{Entry, Lock, LockShardLevels},
    oid::Oid,
};
use gat_io::{LockStore, RepositoryLayout};

fn lock(paths: &[&str]) -> Lock {
    Lock {
        entries: paths
            .iter()
            .map(|p| Entry {
                path: GatPath::parse_canonical(p).unwrap(),
                oid: Oid::from_bytes([7; 32]),
            })
            .collect(),
    }
}

#[test]
fn invalid_publication_preserves_live_lock_across_shapes() {
    for before in [LockShardLevels::FLAT, LockShardLevels::new(1).unwrap()] {
        for after in [LockShardLevels::FLAT, LockShardLevels::new(1).unwrap()] {
            for paths in [
                &["a", "a/b"][..],
                &["a", "a-", "a/b"],
                &["a/b", "a-", "a"],
                &["a", "a"],
            ] {
                let temp = tempfile::tempdir().unwrap();
                let layout = RepositoryLayout::at(temp.path().to_path_buf());
                let valid = lock(&["original"]);
                LockStore::publish_repository(&layout, &valid, before).unwrap();
                let error =
                    LockStore::publish_repository(&layout, &lock(paths), after).unwrap_err();
                assert!(matches!(
                    error,
                    gat_io::LockError::Domain(
                        gat_io::LockDomainError::DirectoryPrefixConflict { .. }
                            | gat_io::LockDomainError::DuplicatePath { .. }
                    )
                ));
                assert_eq!(LockStore::load_repository(&layout).unwrap(), valid);
                assert_eq!(temp.path().join("gat.lock").is_file(), before.is_flat());
            }
        }
    }
}

#[test]
fn invalid_document_publication_preserves_existing_bytes_and_missing_destination() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("merge-result");
    let invalid = lock(&["a", "a-", "a/b"]);
    assert!(LockStore::publish_file_atomic(&invalid, &path).is_err());
    assert!(!path.exists());
    std::fs::write(&path, b"original bytes").unwrap();
    assert!(LockStore::publish_file_atomic(&invalid, &path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"original bytes");
}

#[test]
fn unordered_valid_paths_roundtrip_across_shapes() {
    for levels in [LockShardLevels::FLAT, LockShardLevels::new(1).unwrap()] {
        let temp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(temp.path().to_path_buf());
        let input = lock(&["a0", "a/b", "a-", "a.b", "ab", "a/child/deep"]);
        LockStore::publish_repository(&layout, &input, levels).unwrap();
        let mut expected = input;
        expected
            .entries
            .sort_unstable_by(|a, b| a.path.cmp(&b.path));
        let mut actual = LockStore::load_repository(&layout).unwrap();
        actual.entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(actual, expected);
    }
}

#[test]
fn invalid_repository_publication_does_not_create_a_lock() {
    for levels in [LockShardLevels::FLAT, LockShardLevels::new(1).unwrap()] {
        let temp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(temp.path().to_path_buf());
        assert!(LockStore::publish_repository(&layout, &lock(&["a", "a/b"]), levels).is_err());
        assert!(!temp.path().join("gat.lock").exists());
    }
}
