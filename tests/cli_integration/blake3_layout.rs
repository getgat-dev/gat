//! End-to-end assertions that the whole finalized content-addressed
//! object layout -- `.gat/objects/blake3/xx/yy/oid` locally,
//! `blake3/xx/yy/oid` on a remote -- is what every full user-facing
//! command flow (add/push/fetch/sync, repair, local `gc`, remote `gc`)
//! actually produces and consumes, driven purely through the compiled
//! `gat` binary rather than internal helpers.

use gat_core::oid::Oid;
use gat_io::{LockStore, OBJECT_HASH_NAMESPACE, RepositoryLayout, object_key_oid};

use crate::common::{assert_ok, commit_all, gat, git, init_repo};
use crate::support::remote_url;

#[test]
fn add_push_fetch_sync_only_ever_touch_the_blake3_namespaced_local_and_remote_paths() {
    let tmp = init_repo();
    let dir = tmp.path();
    let remote_dir = tempfile::tempdir().unwrap();

    assert_ok(
        &gat(
            dir,
            &["remote", "add", "origin", &remote_url(remote_dir.path())],
        ),
        "gat remote add",
    );
    assert_ok(
        &gat(dir, &["remote", "default", "origin"]),
        "choose default remote",
    );

    std::fs::write(dir.join("big.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");

    let oid = LockStore::load_repository(&gat_io::RepositoryLayout::at((dir).to_path_buf()))
        .unwrap()
        .entries[0]
        .oid;
    let cache_root = RepositoryLayout::at(dir.to_path_buf()).resolve_cache_root(None, None);
    let local_object_path = cache_root.object_path_for_test(&oid);
    assert!(
        local_object_path.is_file(),
        "expected `gat add` to have written the object at {}",
        local_object_path.display()
    );
    assert!(
        local_object_path.starts_with(cache_root.display_path().join(OBJECT_HASH_NAMESPACE)),
        "the added object must live under the blake3/ namespace"
    );

    commit_all(dir, "add big.bin");
    assert_ok(&gat(dir, &["push"]), "gat push");

    let remote_object_key = object_key_oid(&oid);
    assert!(
        remote_dir.path().join(&remote_object_key).is_file(),
        "expected `gat push` to have written the object at the blake3-namespaced remote key {remote_object_key}"
    );

    // Simulate a second clone with an empty local cache.
    let clone = tempfile::tempdir().unwrap();
    let clone_dir = clone.path();
    assert_ok(
        &git(
            dir,
            &[
                "clone",
                "-q",
                dir.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
        ),
        "git clone",
    );
    assert_ok(&gat(clone_dir, &["init"]), "gat init in clone");
    // `origin` was already committed to the tracked `gat.yaml` project
    // config before the clone, so it's inherited here -- re-adding it
    // would be rejected as a duplicate remote.
    assert_ok(&gat(clone_dir, &["fetch"]), "gat fetch");

    let clone_cache_root =
        RepositoryLayout::at(clone_dir.to_path_buf()).resolve_cache_root(None, None);
    let clone_local_object_path = clone_cache_root.object_path_for_test(&oid);
    assert!(
        clone_local_object_path.is_file(),
        "expected `gat fetch` to have written the object at {}",
        clone_local_object_path.display()
    );

    assert_ok(&gat(clone_dir, &["sync"]), "gat sync");
    let content = std::fs::read_to_string(clone_dir.join("big.bin")).unwrap();
    assert_eq!(content, "payload");
}

#[test]
fn repair_re_fetches_a_corrupted_object_back_to_its_blake3_namespaced_path() {
    let tmp = init_repo();
    let dir = tmp.path();
    let remote_dir = tempfile::tempdir().unwrap();

    assert_ok(
        &gat(
            dir,
            &["remote", "add", "origin", &remote_url(remote_dir.path())],
        ),
        "gat remote add",
    );
    assert_ok(
        &gat(dir, &["remote", "default", "origin"]),
        "choose default remote",
    );

    std::fs::write(dir.join("big.bin"), b"original content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    commit_all(dir, "add big.bin");
    assert_ok(&gat(dir, &["push"]), "gat push");

    let oid = LockStore::load_repository(&gat_io::RepositoryLayout::at((dir).to_path_buf()))
        .unwrap()
        .entries[0]
        .oid;
    let cache_root = RepositoryLayout::at(dir.to_path_buf()).resolve_cache_root(None, None);
    let obj_path = cache_root.object_path_for_test(&oid);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&obj_path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&obj_path, perms).unwrap();
    }
    std::fs::write(&obj_path, b"corrupted!!").unwrap();
    std::fs::remove_file(dir.join("big.bin")).unwrap();

    assert_ok(&gat(dir, &["sync", "--repair"]), "gat sync --repair");

    // Repaired content must land back at the exact same blake3-namespaced
    // path, verified against its content-addressed oid.
    assert!(obj_path.is_file());
    assert_eq!(std::fs::read(&obj_path).unwrap(), b"original content");
    assert_eq!(
        std::fs::read_to_string(dir.join("big.bin")).unwrap(),
        "original content"
    );
}

#[test]
fn local_gc_sweeps_an_orphan_from_and_keeps_a_reachable_object_under_the_blake3_namespace() {
    let tmp = init_repo();
    let dir = tmp.path();

    std::fs::write(dir.join("keep.bin"), b"keep-payload").unwrap();
    assert_ok(&gat(dir, &["add", "keep.bin"]), "gat add keep.bin");
    commit_all(dir, "add keep.bin");
    let kept_oid = LockStore::load_repository(&gat_io::RepositoryLayout::at((dir).to_path_buf()))
        .unwrap()
        .entries[0]
        .oid;

    // An orphaned object: an ordinary cache blob with no `gat.lock` row
    // referencing it in any commit's history or the current working
    // tree, exactly as `gc` must distinguish from a still-tracked one.
    let cache_root = RepositoryLayout::at(dir.to_path_buf()).resolve_cache_root(None, None);
    let orphan_oid = cache_root
        .writer()
        .ingest(std::io::Cursor::new(b"orphan-payload"))
        .unwrap()
        .0
        .oid;

    let kept_path = cache_root.object_path_for_test(&kept_oid);
    let orphan_path = cache_root.object_path_for_test(&orphan_oid);
    assert!(kept_path.is_file());
    assert!(orphan_path.is_file());

    assert_ok(&gat(dir, &["gc"]), "gat gc");

    assert!(
        kept_path.is_file(),
        "the still-reachable object must survive local gc"
    );
    assert!(
        !orphan_path.exists(),
        "the orphaned object under blake3/ must be swept by local gc"
    );
}

#[test]
fn remote_gc_sweeps_only_unreferenced_objects_under_the_remote_blake3_namespace() {
    let tmp = init_repo();
    let dir = tmp.path();
    let remote_dir = tempfile::tempdir().unwrap();

    assert_ok(
        &gat(
            dir,
            &["remote", "add", "origin", &remote_url(remote_dir.path())],
        ),
        "gat remote add",
    );
    assert_ok(
        &gat(dir, &["remote", "default", "origin"]),
        "choose default remote",
    );

    std::fs::write(dir.join("keep.bin"), b"keep-payload").unwrap();
    assert_ok(&gat(dir, &["add", "keep.bin"]), "gat add keep.bin");
    commit_all(dir, "add keep.bin");
    assert_ok(&gat(dir, &["push"]), "gat push");
    let kept_oid = LockStore::load_repository(&gat_io::RepositoryLayout::at((dir).to_path_buf()))
        .unwrap()
        .entries[0]
        .oid;

    // An orphaned remote object: written directly to the remote's
    // blake3-namespaced key, referenced by no lock at all.
    let orphan_oid = Oid::from_hex(&"9".repeat(64)).unwrap();
    let orphan_remote_key = object_key_oid(&orphan_oid);
    let orphan_remote_path = remote_dir.path().join(&orphan_remote_key);
    std::fs::create_dir_all(orphan_remote_path.parent().unwrap()).unwrap();
    std::fs::write(&orphan_remote_path, b"orphan").unwrap();

    let kept_remote_key = object_key_oid(&kept_oid);
    assert!(remote_dir.path().join(&kept_remote_key).is_file());
    assert!(orphan_remote_path.is_file());

    assert_ok(
        &gat(dir, &["gc", "--remote", "origin", "--unsafe"]),
        "gat gc --remote origin",
    );

    assert!(
        remote_dir.path().join(&kept_remote_key).is_file(),
        "the still-referenced remote object must survive remote gc"
    );
    assert!(
        !orphan_remote_path.exists(),
        "the orphaned remote object under blake3/ must be swept by remote gc"
    );
}
