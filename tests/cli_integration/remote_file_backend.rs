//! Push/fetch against a real (`file://`) remote, and the error surfaced
//! when a remote object is missing. `file://` remotes are the default
//! integration backend: fast, hermetic, no network service
//! required.

use gat_io::LockStore;
use gat_io::{OBJECT_HASH_NAMESPACE, object_key_oid};

use crate::common::{assert_ok, commit_all, gat, git, init_repo, stderr};
use crate::support::remote_url;

#[test]
fn push_then_fetch_roundtrips_object_via_file_remote() {
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
    commit_all(dir, "add big.bin");

    assert_ok(&gat(dir, &["push"]), "gat push");

    // Objects use the hash namespace.
    let oid = LockStore::load_repository(&gat_io::RepositoryLayout::at((dir).to_path_buf()))
        .unwrap()
        .entries[0]
        .oid;
    let remote_object_key = object_key_oid(&oid);
    assert!(
        remote_dir.path().join(&remote_object_key).is_file(),
        "expected the pushed object at the blake3-namespaced remote key {remote_object_key}"
    );
    let namespaced_key = object_key_oid(&oid);
    let legacy_remote_key = namespaced_key
        .strip_prefix(OBJECT_HASH_NAMESPACE)
        .and_then(|path| path.strip_prefix('/'))
        .unwrap();
    assert!(
        !remote_dir.path().join(legacy_remote_key).exists(),
        "the pre-namespace bare remote key must never be used: {legacy_remote_key}"
    );
    assert!(
        remote_dir.path().join("blake3").is_dir(),
        "the blake3/ namespace directory must exist as a sibling of any other remote root state"
    );

    // Simulate a second clone: fresh repo without the cached object.
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

    // Object isn't cached locally yet in the clone.
    assert!(
        !clone_dir.join(".gat/objects").exists()
            || std::fs::read_dir(clone_dir.join(".gat/objects"))
                .map_or(true, |mut d| d.next().is_none())
    );

    assert_ok(&gat(clone_dir, &["fetch"]), "gat fetch");
    assert_ok(&gat(clone_dir, &["sync"]), "gat sync");
    let content = std::fs::read_to_string(clone_dir.join("big.bin")).unwrap();
    assert_eq!(content, "payload");
}

#[test]
fn push_reports_skipped_items_missing_from_local_cache_on_stderr() {
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
    commit_all(dir, "add big.bin");

    // Simulate a fresh clone: gat.lock is tracked but the local object
    // cache is empty, so push must skip (not error on) the missing object.
    std::fs::remove_dir_all(dir.join(".gat/objects")).unwrap();
    std::fs::create_dir_all(dir.join(".gat/objects")).unwrap();

    let out = gat(dir, &["push"]);
    assert_ok(&out, "gat push");
    let err = stderr(&out);
    assert!(
        err.contains("big.bin") && err.contains("not in cache"),
        "expected skipped-item diagnostic naming big.bin on stderr: {err}"
    );
}

#[test]
fn fetch_fails_clearly_when_remote_object_is_missing() {
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
    commit_all(dir, "add big.bin");
    // Deliberately do NOT push, so the remote has no objects.

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

    let out = gat(clone_dir, &["fetch"]);
    assert!(
        !out.status.success(),
        "fetch should fail when remote has no matching object"
    );
    assert!(
        !stderr(&out).is_empty(),
        "expected an error message on stderr"
    );
}
