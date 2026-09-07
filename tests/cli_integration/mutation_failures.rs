//! `add`/`rm`/`mv` partial-failure and roundtrip behavior: what happens
//! when one path among several does not exist, when a destination
//! already exists, and how the lock file/exclude list are kept in sync
//! across these mutations.

use crate::common::{assert_ok, commit_all, gat, init_repo};

#[test]
fn add_rm_roundtrip_updates_lock_and_exclude() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("a.bin"), b"aaaa").unwrap();
    std::fs::write(dir.join("b.bin"), b"bbbb").unwrap();

    assert_ok(&gat(dir, &["add", "a.bin", "b.bin"]), "gat add");
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap();
    assert!(lock.contains("a.bin"));
    assert!(lock.contains("b.bin"));

    let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
    assert!(exclude.contains("a.bin"));

    assert_ok(&gat(dir, &["rm", "a.bin"]), "gat rm");
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap();
    assert!(!lock.contains("a.bin"), "a.bin should be removed from lock");
    assert!(lock.contains("b.bin"), "b.bin should remain");
    assert!(
        !dir.join("a.bin").exists(),
        "gat rm deletes the working-tree file"
    );
}

/// `gat rm --cached` must leave the file on disk *and* make sure `gat
/// sync` never turns around and deletes it afterward -- which requires
/// forgetting materialized state, not just `gat.lock`, for the removed
/// path.
#[test]
fn rm_cached_then_sync_keeps_the_file_on_disk() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("big.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&gat(dir, &["sync"]), "gat sync");

    assert_ok(&gat(dir, &["rm", "--cached", "big.bin"]), "gat rm --cached");
    // A flat `gat.lock` is a degenerate single shard, so once it
    // has no entries left it's removed entirely (same as an emptied
    // sharded shard file), rather than left behind as an empty file.
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap_or_default();
    assert!(
        !lock.contains("big.bin"),
        "big.bin should be removed from lock"
    );
    assert!(
        dir.join("big.bin").exists(),
        "rm --cached must keep the file on disk"
    );

    assert_ok(&gat(dir, &["sync"]), "gat sync after rm --cached");
    assert!(
        dir.join("big.bin").exists(),
        "a later sync must not delete a file `rm --cached` deliberately kept on disk"
    );
}

#[test]
fn add_partially_fails_when_one_path_does_not_exist() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("real.bin"), b"data").unwrap();

    let args = ["add", "real.bin", "missing.bin"];
    let out = gat(dir, &args);
    assert!(
        !out.status.success(),
        "add should fail overall when a path is missing"
    );
    // The detailed failure contract this exercises (no `gat.lock`
    // written, real.bin untouched on disk, an orphaned but harmless
    // cache object, no materialized-state row) is covered at the command
    // level. This spawned-binary test keeps only the end-to-end assertion
    // that the real
    // process actually surfaces the failure (nonzero exit) and that a
    // corrected retry still works through the real CLI.
    assert_ok(&gat(dir, &["add", "real.bin"]), "gat add real.bin (retry)");
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap();
    assert!(lock.contains("real.bin"));
}

/// `gat add` refuses a directly named symlink rather than following it and
/// tracking whatever it points at.
#[test]
#[cfg(unix)]
fn add_rejects_a_directly_named_symlink() {
    use std::os::unix::fs::symlink;

    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("target.bin"), b"real content").unwrap();
    symlink(dir.join("target.bin"), dir.join("link.bin")).unwrap();

    let out = gat(dir, &["add", "link.bin"]);
    assert!(
        !out.status.success(),
        "add should refuse a directly-named symlink"
    );
    // The command-level test checks the specific typed rejection. This
    // process test verifies that the rejected add does not create lock state.
    assert!(
        !dir.join("gat.lock").exists(),
        "a rejected add must not create gat.lock"
    );

    assert_ok(
        &gat(dir, &["add", "target.bin"]),
        "gat add target.bin (the real file, not the symlink)",
    );
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap();
    assert!(lock.contains("target.bin"));
    assert!(!lock.contains("link.bin"));
}

/// `gat add` refuses to ingest through a symlinked *ancestor* directory
/// through a symlink ancestor, even though the leaf
/// component named on the command line is an ordinary file and the Gat
/// path itself is lexically root-relative.
#[test]
#[cfg(unix)]
fn add_rejects_a_path_traversing_a_symlinked_ancestor_directory() {
    use std::os::unix::fs::symlink;

    let tmp = init_repo();
    let dir = tmp.path();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.bin"), b"outside content").unwrap();
    symlink(outside.path(), dir.join("external")).unwrap();

    let out = gat(dir, &["add", "external/secret.bin"]);
    assert!(
        !out.status.success(),
        "add should refuse a path traversing a symlinked ancestor directory"
    );
    assert!(
        !dir.join("gat.lock").exists(),
        "a rejected add must not create gat.lock"
    );
}

#[test]
fn mv_renames_tracked_file_and_updates_lock() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("old.bin"), b"content").unwrap();
    assert_ok(&gat(dir, &["add", "old.bin"]), "gat add");
    commit_all(dir, "add old.bin");

    let out = gat(dir, &["mv", "old.bin", "new.bin"]);
    assert_ok(&out, "gat mv");
    assert!(!dir.join("old.bin").exists());
    assert!(dir.join("new.bin").exists());
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap();
    assert!(lock.contains("new.bin"));
    assert!(!lock.contains("old.bin"));
}

#[test]
fn mv_without_force_fails_when_destination_already_exists() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("old.bin"), b"content").unwrap();
    assert_ok(&gat(dir, &["add", "old.bin"]), "gat add");
    std::fs::write(dir.join("new.bin"), b"already here").unwrap();
    commit_all(dir, "add old.bin");

    let out = gat(dir, &["mv", "old.bin", "new.bin"]);
    assert!(
        !out.status.success(),
        "gat mv without --force must fail when the destination already exists"
    );
    // The command-level error test checks the exact diagnostic and `--force`
    // guidance. This process test verifies that state remains unchanged.
    assert!(dir.join("old.bin").exists());
    assert_eq!(std::fs::read(dir.join("new.bin")).unwrap(), b"already here");
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap();
    assert!(lock.contains("old.bin"));
    assert!(!lock.contains("new.bin"));
}

#[test]
fn mv_with_force_replaces_an_existing_destination() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("old.bin"), b"content").unwrap();
    assert_ok(&gat(dir, &["add", "old.bin"]), "gat add");
    std::fs::write(dir.join("new.bin"), b"already here").unwrap();
    commit_all(dir, "add old.bin");

    let out = gat(dir, &["mv", "--force", "old.bin", "new.bin"]);
    assert_ok(&out, "gat mv --force");
    assert!(!dir.join("old.bin").exists());
    assert_eq!(std::fs::read(dir.join("new.bin")).unwrap(), b"content");
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap();
    assert!(lock.contains("new.bin"));
    assert!(!lock.contains("old.bin"));
}
