//! Corrupt cache entries: a cached object's content-addressed hash no
//! longer matches its bytes, and `gat` must detect and report the
//! corruption rather than materializing bad content into the worktree.

use gat_io::{LockStore, RepositoryLayout};

use crate::common::{
    assert_no_low_level_error_leak, assert_ok, commit_all, gat, init_repo, stderr,
};

#[test]
fn checkout_reports_error_when_cached_object_content_is_corrupted() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("big.bin"), b"original content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    commit_all(dir, "add big.bin");

    // Corrupt the single cached object on disk: resolve the tracked
    // object's canonical on-disk path (`<objects_dir>/blake3/xx/yy/oid`,
    // from the committed lock's oid, and
    // overwrite its content there, invalidating the content-addressed
    // hash. Locating it this way (rather than scanning `.gat/objects` and
    // excluding known bookkeeping files by name) keeps this test coupled
    // to the one real canonical-path helper instead of re-deriving the
    // storage layout by hand.
    let oid = LockStore::load_repository(&gat_io::RepositoryLayout::at((dir).to_path_buf()))
        .unwrap()
        .entries[0]
        .oid;
    let cache_root = RepositoryLayout::at(dir.to_path_buf()).resolve_cache_root(None);
    let obj_path = cache_root.object_path_for_test(&oid);
    assert!(
        obj_path.is_file(),
        "expected a cached object at {}",
        obj_path.display()
    );
    // Cached objects are made read-only on write;
    // restore write permission before corrupting the content.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&obj_path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&obj_path, perms).unwrap();
    }
    std::fs::write(&obj_path, b"corrupted!!").unwrap();

    // Force a re-materialization to surface the corruption.
    std::fs::remove_file(dir.join("big.bin")).unwrap();
    let out = gat(dir, &["sync", "--repair"]);
    // Cached object content is verified against its content-addressed oid
    // before materialization; corruption introduced directly in the cache
    // (bypassing gat entirely, as done above) is detected. `--repair`
    // attempts to re-fetch the corrupted object, which fails here because
    // no remote is configured, so the overall sync reports an error and
    // the corrupted content is never materialized into the worktree.
    assert!(
        !out.status.success(),
        "sync --repair should fail: no remote is configured to repair the corrupted object"
    );
    assert!(
        stderr(&out).contains("corrupted"),
        "expected the corrupted object to be reported on stderr, got: {}",
        stderr(&out)
    );
    assert!(
        !dir.join("big.bin").exists(),
        "corrupted cache content must not be materialized into the worktree"
    );
    // This exercises `gat_command::RepairError`'s
    // per-object failure path end-to-end (via its
    // `MissingRemoteConfig` variant, since no remote is configured);
    // its reported outcome must never leak low-level vocabulary either.
    assert_no_low_level_error_leak(&out, &[]);
}
