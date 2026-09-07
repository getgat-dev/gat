use gat_core::lexical_path::GatPath;
use gat_core::lock::{Entry, Lock, LockShardLevels};
use gat_core::oid::Oid;
use gat_io::{LiveLockState, LockStore, RepositoryLayout};

fn entry(path: &str, byte: u8) -> Entry {
    Entry {
        path: GatPath::parse_canonical(path).unwrap(),
        oid: Oid::from_bytes([byte; 32]),
    }
}

fn sample_lock() -> Lock {
    Lock {
        entries: vec![
            entry("alpha/a.bin", 1),
            entry("alpha/b.bin", 2),
            entry("beta/c.bin", 3),
        ],
    }
}

fn shapes() -> [LockShardLevels; 2] {
    [LockShardLevels::FLAT, LockShardLevels::new(1).unwrap()]
}

fn semantic_entries(lock: &Lock) -> std::collections::BTreeMap<&str, Oid> {
    lock.entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.oid))
        .collect()
}

fn assert_same_lock(actual: &Lock, expected: &Lock) {
    assert_eq!(semantic_entries(actual), semantic_entries(expected));
}

fn assert_shape(layout: &RepositoryLayout, expected: Option<LockShardLevels>) {
    let live = LockStore::inspect_repository(layout).unwrap().live;
    match (live, expected) {
        (LiveLockState::Missing, None) => {}
        (LiveLockState::Valid { shard_levels, .. }, Some(expected)) => {
            assert_eq!(shard_levels, expected);
        }
        (actual, expected) => panic!("expected shape {expected:?}, got {actual:?}"),
    }
}

fn reshape(
    layout: &RepositoryLayout,
    target: LockShardLevels,
) -> Option<gat_io::CompletedLockReshape> {
    LockStore::begin_repository_reshape(layout, target)
        .unwrap()
        .map(|reshape| reshape.apply().unwrap())
}

#[test]
fn complete_publish_and_load_are_representation_independent() {
    for levels in shapes() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(temp.path().to_path_buf());
        let lock = sample_lock();

        LockStore::publish_repository(&layout, &lock, levels).unwrap();

        assert_same_lock(&LockStore::load_repository(&layout).unwrap(), &lock);
        assert_shape(&layout, Some(levels));
    }
}

#[test]
fn selected_visits_have_identical_semantics_across_representations() {
    let expected = Lock {
        entries: vec![entry("alpha/a.bin", 1), entry("alpha/b.bin", 2)],
    };

    for levels in shapes() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(temp.path().to_path_buf());
        LockStore::publish_repository(&layout, &sample_lock(), levels).unwrap();
        let mut visited = Vec::new();

        LockStore::visit_repository(
            &layout,
            None,
            |path| path.starts_with("alpha/"),
            |entry| {
                visited.push(entry.clone());
                Ok(())
            },
        )
        .unwrap();

        assert_same_lock(&Lock { entries: visited }, &expected);
    }
}

#[test]
fn reshape_in_both_directions_preserves_the_logical_lock() {
    for (source, target) in [
        (LockShardLevels::FLAT, LockShardLevels::new(1).unwrap()),
        (LockShardLevels::new(1).unwrap(), LockShardLevels::FLAT),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(temp.path().to_path_buf());
        let lock = sample_lock();
        LockStore::publish_repository(&layout, &lock, source).unwrap();

        let completed = reshape(&layout, target).expect("shape differs");
        drop(completed);
        assert_same_lock(&LockStore::load_repository(&layout).unwrap(), &lock);
        assert_shape(&layout, Some(target));
        assert!(reshape(&layout, target).is_none());
    }
}

#[test]
fn ensuring_the_current_or_missing_shape_is_a_no_op() {
    let empty = tempfile::tempdir().unwrap();
    let empty_layout = RepositoryLayout::at(empty.path().to_path_buf());
    assert!(reshape(&empty_layout, LockShardLevels::new(1).unwrap()).is_none());
    assert_shape(&empty_layout, None);

    for levels in shapes() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(temp.path().to_path_buf());
        let lock = sample_lock();
        LockStore::publish_repository(&layout, &lock, levels).unwrap();

        assert!(reshape(&layout, levels).is_none());
        assert_same_lock(&LockStore::load_repository(&layout).unwrap(), &lock);
    }
}

#[test]
fn later_complete_publication_preserves_requested_shape() {
    for levels in shapes() {
        let temp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(temp.path().to_path_buf());
        let mut lock = sample_lock();
        LockStore::publish_repository(&layout, &lock, levels).unwrap();
        lock.entries.push(entry("gamma/d.bin", 4));

        LockStore::publish_repository(&layout, &lock, levels).unwrap();

        assert_same_lock(&LockStore::load_repository(&layout).unwrap(), &lock);
        assert_shape(&layout, Some(levels));
    }
}

#[test]
fn malformed_content_fails_closed_at_the_same_public_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let path = temp.path().join("gat.lock");
    let malformed = b"not a gat lock\n";
    std::fs::write(&path, malformed).unwrap();

    assert!(LockStore::load_repository(&layout).is_err());
    assert!(LockStore::visit_repository(&layout, None, |_| true, |_| Ok(())).is_err());
    let reshape = LockStore::begin_repository_reshape(&layout, LockShardLevels::new(1).unwrap())
        .unwrap()
        .expect("the flat representation differs from the requested target");
    assert!(reshape.apply().is_err());
    assert_eq!(std::fs::read(path).unwrap(), malformed);
}

#[test]
fn malformed_shard_identity_retains_the_physical_path_and_semantic_source() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let shard = temp.path().join("gat.lock").join("AB.tsv");
    std::fs::create_dir_all(shard.parent().unwrap()).unwrap();
    std::fs::write(&shard, b"").unwrap();

    let error = LockStore::load_repository(&layout).unwrap_err();
    let gat_io::LockError::CorruptShard { path, source } = error else {
        panic!("expected malformed shard identity to be reported as CorruptShard");
    };
    assert_eq!(path, shard);
    assert!(
        source
            .downcast_ref::<gat_core::lock::LockShardIdError>()
            .is_some()
    );
}

#[test]
fn escaped_paths_survive_publication_selection_and_reshape() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let lock = Lock {
        entries: vec![
            entry("a\tfile", 1),
            entry("a\nfile", 2),
            entry("a\rfile", 3),
            entry("a\"file", 4),
            entry("aZfile", 5),
        ],
    };
    LockStore::publish_repository(&layout, &lock, LockShardLevels::FLAT).unwrap();
    for levels in [LockShardLevels::new(1).unwrap(), LockShardLevels::FLAT] {
        reshape(&layout, levels);
        assert_same_lock(&LockStore::load_repository(&layout).unwrap(), &lock);
        let mut selected = Vec::new();
        LockStore::visit_repository(
            &layout,
            None,
            |path| path.contains('\n'),
            |entry| {
                selected.push(entry.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(selected, vec![entry("a\nfile", 2)]);
    }
}
