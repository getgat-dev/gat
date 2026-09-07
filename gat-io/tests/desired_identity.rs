use gat_core::lexical_path::GatPath;
use gat_core::lock::{
    CanonicalDesiredIdentity, Entry, Lock, LockShardId, LockShardLevels, ShardContentIdentity,
};
use gat_core::oid::Oid;
use gat_io::{LockStore, RepositoryLayout, StateStore};

fn entry(path: &str, byte: u8) -> Entry {
    Entry {
        path: GatPath::parse_canonical(path).unwrap(),
        oid: Oid::from_bytes([byte; 32]),
    }
}

fn flat_identity(root: &std::path::Path) -> CanonicalDesiredIdentity {
    let bytes = std::fs::read(root.join("gat.lock")).unwrap();
    CanonicalDesiredIdentity::empty()
        .toggle_shard(LockShardId::flat(), &ShardContentIdentity::hash(&bytes))
}

fn publish_flat(layout: &RepositoryLayout, entries: Vec<Entry>) {
    LockStore::publish_repository(layout, &Lock { entries }, LockShardLevels::FLAT).unwrap();
}

#[test]
fn missing_accelerator_falls_back_without_creating_state() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    publish_flat(&layout, vec![entry("a.bin", 1)]);
    let database = temp.path().join(".gat/state/state.sqlite3");
    assert!(!database.exists());

    let observed = StateStore::observe_canonical_identity(&layout).unwrap();

    assert_eq!(observed, flat_identity(temp.path()));
    assert!(!database.exists());
}

#[test]
fn corrupt_accelerator_falls_back_without_modifying_it() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    publish_flat(&layout, vec![entry("a.bin", 1)]);
    let database = temp.path().join(".gat/state/state.sqlite3");
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let corrupt = b"not a sqlite database";
    std::fs::write(&database, corrupt).unwrap();

    let observed = StateStore::observe_canonical_identity(&layout).unwrap();

    assert_eq!(observed, flat_identity(temp.path()));
    assert_eq!(std::fs::read(database).unwrap(), corrupt);
}

#[test]
fn invalid_utf8_live_lock_fails_before_desired_state_changes() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let mut bytes = format!("{}\n", gat_core::lock::VERSION).into_bytes();
    bytes.extend_from_slice(b"invalid-");
    bytes.push(0xff);
    bytes.extend_from_slice(format!("\t{}\n", "0".repeat(64)).as_bytes());
    std::fs::write(temp.path().join("gat.lock"), bytes).unwrap();

    assert!(LockStore::load_repository(&layout).is_err());

    let mut store = StateStore::open(&layout).unwrap();
    assert!(store.refresh_desired_state(&layout).is_err());
    assert_eq!(store.load_desired_as_lock().unwrap(), Lock::default());
}

#[test]
fn stale_accelerator_falls_back_to_authoritative_lock_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    publish_flat(&layout, vec![entry("a.bin", 1)]);
    let mut store = StateStore::open(&layout).unwrap();
    store.refresh_desired_state(&layout).unwrap();

    publish_flat(&layout, vec![entry("a.bin", 1), entry("changed.bin", 2)]);
    let observed = StateStore::observe_canonical_identity(&layout).unwrap();

    assert_eq!(observed, flat_identity(temp.path()));
}

#[cfg(feature = "test-support")]
fn observation_counts(layout: &RepositoryLayout) -> (CanonicalDesiredIdentity, usize, usize) {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap()
        .install(|| {
            let reads_before =
                gat_io::lock_identity_test_support::current_identity_content_read_call_count();
            let hashes_before = gat_io::lock_identity_test_support::hash_shard_bytes_call_count();
            let identity = StateStore::observe_canonical_identity(layout).unwrap();
            (
                identity,
                gat_io::lock_identity_test_support::current_identity_content_read_call_count()
                    - reads_before,
                gat_io::lock_identity_test_support::hash_shard_bytes_call_count() - hashes_before,
            )
        })
}

#[cfg(feature = "test-support")]
#[test]
fn warm_accelerator_reuses_the_stat_proof_without_content_work() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    publish_flat(&layout, vec![entry("a.bin", 1)]);
    let mut store = StateStore::open(&layout).unwrap();
    let refreshed = store.refresh_desired_state(&layout).unwrap();

    let (observed, reads, hashes) = observation_counts(&layout);

    assert_eq!(observed, refreshed.identity());
    assert_eq!(reads, 0);
    assert_eq!(hashes, 0);
}

#[cfg(feature = "test-support")]
#[test]
fn stale_or_absent_prior_rows_cost_one_read_and_hash_per_affected_shard() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    publish_flat(&layout, vec![entry("a.bin", 1)]);
    let mut store = StateStore::open(&layout).unwrap();
    store.refresh_desired_state(&layout).unwrap();
    publish_flat(&layout, vec![entry("a.bin", 1), entry("changed.bin", 2)]);

    let (changed, changed_reads, changed_hashes) = observation_counts(&layout);
    assert_eq!(changed, flat_identity(temp.path()));
    assert_eq!(changed_reads, 1);
    assert_eq!(changed_hashes, 1);

    let fresh_temp = tempfile::tempdir().unwrap();
    let fresh_layout = RepositoryLayout::at(fresh_temp.path().to_path_buf());
    let _empty_catalog = StateStore::open(&fresh_layout).unwrap();
    publish_flat(&fresh_layout, vec![entry("new.bin", 3)]);

    let (absent, absent_reads, absent_hashes) = observation_counts(&fresh_layout);
    assert_eq!(absent, flat_identity(fresh_temp.path()));
    assert_eq!(absent_reads, 1);
    assert_eq!(absent_hashes, 1);
}

#[cfg(feature = "test-support")]
#[test]
fn malformed_prior_proof_is_a_cache_miss_not_a_correctness_failure() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    publish_flat(&layout, vec![entry("a.bin", 1)]);
    let mut store = StateStore::open(&layout).unwrap();
    store.refresh_desired_state(&layout).unwrap();
    drop(store);
    rusqlite::Connection::open(temp.path().join(".gat/state/state.sqlite3"))
        .unwrap()
        .execute("UPDATE lock_shards SET proof = X'00'", [])
        .unwrap();

    let (observed, reads, hashes) = observation_counts(&layout);

    assert_eq!(observed, flat_identity(temp.path()));
    assert_eq!(reads, 1);
    assert_eq!(hashes, 1);
}

#[cfg(feature = "test-support")]
#[test]
fn refresh_removes_missing_shards_from_rows_catalog_and_identity() {
    let temp = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let levels = LockShardLevels::new(2).unwrap();
    let complete = Lock {
        entries: (0..256u16)
            .map(|i| entry(&format!("file-{i:03}.bin"), (i).to_le_bytes()[0]))
            .collect(),
    };
    let retained = Lock {
        entries: vec![complete.entries[0].clone()],
    };

    LockStore::publish_repository(&layout, &complete, levels).unwrap();
    let mut store = StateStore::open(&layout).unwrap();
    store.refresh_desired_state(&layout).unwrap();
    let before_ids = gat_io::state_shard_ids_for_test(&store).unwrap();
    assert!(before_ids.len() > 1);
    LockStore::publish_repository(&layout, &retained, levels).unwrap();

    let refreshed = store.refresh_desired_state(&layout).unwrap();
    let after_ids = gat_io::state_shard_ids_for_test(&store).unwrap();

    assert_eq!(store.load_desired_as_lock().unwrap(), retained);
    assert_eq!(after_ids.len(), 1);
    assert!(after_ids.iter().all(|id| before_ids.contains(id)));
    assert_eq!(
        store.desired_fingerprint().unwrap(),
        *refreshed.identity().as_bytes()
    );
}
