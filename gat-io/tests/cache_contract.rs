use gat_core::cache_location::CacheLocation;
use gat_core::oid::Oid;
use gat_io::RepositoryLayout;

#[cfg(feature = "test-support")]
use gat_io::ObjectVerification;
#[cfg(feature = "test-support")]
use gat_io::{
    hash_file_call_count, object_key_oid, parse_object_key, with_exclusive_hash_file_call_count,
};

#[test]
fn resolution_is_pure_and_preserves_configured_location_semantics() {
    let repo = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(repo.path().to_path_buf());

    let default = layout.resolve_cache_root(None, None);
    assert_eq!(default.display_path(), repo.path().join(".gat/objects"));
    assert!(!repo.path().join(".gat").exists());

    let relative = CacheLocation::from_path(std::path::PathBuf::from("../shared-cache"));
    assert_eq!(
        layout
            .resolve_cache_root(None, Some(&relative))
            .display_path(),
        repo.path().join("../shared-cache")
    );

    let absolute = CacheLocation::from_path(shared.path().to_path_buf());
    assert_eq!(
        layout
            .resolve_cache_root(None, Some(&absolute))
            .display_path(),
        shared.path()
    );
}

#[test]
fn presence_is_proof_free_and_client_open_is_explicit() {
    let repo = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(repo.path().to_path_buf());
    let root = layout.resolve_cache_root(None, None);
    let database = root.display_path().join("cache.sqlite3");
    let oid = Oid::from_hex(&"0".repeat(64)).unwrap();

    assert!(!root.presence().contains(&oid));
    assert!(!database.exists());

    std::fs::create_dir_all(root.display_path()).unwrap();
    let _ = root.open_client();
    assert!(database.exists());
}

#[cfg(feature = "test-support")]
#[test]
fn typed_storage_keys_round_trip_and_malformed_keys_fail_closed() {
    let oid = Oid::from_bytes(std::array::from_fn(|index| (index).to_le_bytes()[0]));
    let key = object_key_oid(&oid);
    assert_eq!(parse_object_key(&key), Some(oid));

    let oid_hex = oid.to_hex();
    for malformed in [
        oid_hex.clone(),
        format!("sha256/00/01/{oid_hex}"),
        format!("blake3/ff/01/{oid_hex}"),
        format!("blake3/00/ff/{oid_hex}"),
        format!("blake3/00/01/{oid_hex}/"),
        format!("blake3/00/01/{oid_hex}/extra"),
        format!("blake3/00/01/{}", oid_hex.to_uppercase()),
        "blake3/00/01/é".to_string(),
        "cache.sqlite3".to_string(),
        "tmp-object".to_string(),
    ] {
        assert_eq!(parse_object_key(&malformed), None, "{malformed}");
    }
}

#[cfg(feature = "test-support")]
#[test]
fn cache_verification_uses_the_typed_client_and_reuses_warm_proofs() {
    let temp = tempfile::tempdir().unwrap();
    let objects_dir = temp.path().join("objects");
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let root = layout.resolve_cache_root(Some(objects_dir.as_os_str()), None);
    let (ingested, _) = root.writer().ingest(&b"payload"[..]).unwrap();
    let oid = ingested.oid;

    let cold_hashes = with_exclusive_hash_file_call_count(|| {
        assert_eq!(
            root.open_client().verify(&oid).unwrap(),
            ObjectVerification::Valid
        );
        hash_file_call_count()
    });
    assert_eq!(cold_hashes, 1);

    let warm_hashes = with_exclusive_hash_file_call_count(|| {
        assert_eq!(
            root.open_client().verify(&oid).unwrap(),
            ObjectVerification::Valid
        );
        hash_file_call_count()
    });
    assert_eq!(warm_hashes, 0);
}
