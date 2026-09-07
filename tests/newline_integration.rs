//! End-to-end newline (LF/CRLF) regression coverage: proves
//! the repo-wide newline policy actually holds across the real, whole-
//! command boundaries that matter -- not just each parser in isolation.
//!
//! These tests spawn the real `git`/`gat` binaries against real
//! repositories, the same way
//! `cli_integration.rs`/`sync_hooks_integration.rs` do. Unlike those
//! files, several tests here *intentionally* opt into `core.autocrlf`
//! conversion (`git config core.autocrlf true` in the repo under test) to
//! actually exercise CRLF content on disk -- `tests/common::run`'s
//! isolation from the ambient/global git config only means ordinary
//! tests don't inherit a stray global setting; it does not, and must
//! not, prevent a test from opting a specific repo into CRLF behavior on
//! purpose.

#[path = "common/mod.rs"]
mod common;
use common::{assert_ok, commit_all, gat, git, init_repo};

fn read(dir: &std::path::Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap()
}

/// Delegates to the shared `test-support` crate's `file_remote_url` --
/// see `cli_integration.rs`'s identical wrapper for why each integration
/// test file still needs its own thin `fn remote_url` (they're separate
/// compiled crates that can't share a `use` of a private helper, but can
/// both depend on the same external `test-support` dev-dependency).
fn remote_url(path: &std::path::Path) -> String {
    test_support::file_remote_url(path)
}

/// A clone checked out with `core.autocrlf true` converts every LF in a
/// text-attributed tracked file to CRLF on checkout. `gat.lock` isn't
/// marked `-text`/binary by gat itself, so a real clone onto a
/// CRLF-converting checkout is a realistic way a working tree ends up
/// with a CRLF `gat.lock` even though gat always *writes* canonical LF.
/// `gat status`/`gat add` must still work against that CRLF file exactly
/// as they would against the canonical LF one -- this is the concrete
/// "mount/add path consuming committed CRLF lock text" scenario the
/// issue calls out.
#[test]
fn add_and_status_tolerate_a_crlf_gat_lock_from_an_autocrlf_checkout() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("a.bin"), b"a").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add a.bin");
    commit_all(dir, "add a.bin");

    // A second, autocrlf=true clone: git converts gat.lock's LF line
    // endings to CRLF on checkout, exactly as it would on a real Windows
    // checkout with the common `core.autocrlf=true` global default.
    // Clone without checking anything out yet, so `core.autocrlf` can be
    // set *before* the one and only checkout that follows -- setting it
    // after an ordinary checkout has already happened is a no-op, since
    // git only re-converts a file's line endings when it's actually
    // written to the working tree.
    let clone = tempfile::tempdir().unwrap();
    let clone_dir = clone.path();
    assert_ok(
        &git(
            dir,
            &[
                "clone",
                "-q",
                "--no-checkout",
                dir.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
        ),
        "git clone --no-checkout",
    );
    assert_ok(
        &git(clone_dir, &["config", "core.autocrlf", "true"]),
        "git config core.autocrlf true",
    );
    assert_ok(
        &git(clone_dir, &["checkout", "-q", "main"]),
        "git checkout main",
    );

    let lock_text = read(clone_dir, "gat.lock");
    assert!(
        lock_text.contains("\r\n"),
        "fixture must actually produce a CRLF gat.lock, got {lock_text:?}"
    );

    assert_ok(&gat(clone_dir, &["init"]), "gat init in clone");

    // `gat status`/`gat add` must succeed against the CRLF `gat.lock`
    // exactly as they would against the canonical LF one.
    assert_ok(&gat(clone_dir, &["status"]), "gat status on CRLF lock");
    std::fs::write(clone_dir.join("b.bin"), b"b").unwrap();
    assert_ok(&gat(clone_dir, &["add", "b.bin"]), "gat add b.bin in clone");

    // gat always re-serializes gat.lock in canonical LF, regardless of
    // what line endings the file it read had.
    let rewritten = read(clone_dir, "gat.lock");
    assert!(
        !rewritten.contains('\r'),
        "gat must always rewrite gat.lock in canonical LF, got {rewritten:?}"
    );
    assert!(rewritten.contains("a.bin"));
    assert!(rewritten.contains("b.bin"));
}

/// `gat sync` must materialize the correct working-tree content from a
/// committed lock/config that a `core.autocrlf=true` checkout delivered
/// as CRLF -- the shallow-clone/GC-relevant "clone consuming committed
/// CRLF metadata" scenario.
#[test]
fn sync_materializes_correctly_from_a_crlf_checkout_of_gat_lock() {
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
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add big.bin");
    commit_all(dir, "add big.bin");
    assert_ok(&gat(dir, &["push"]), "gat push");

    let clone = tempfile::tempdir().unwrap();
    let clone_dir = clone.path();
    assert_ok(
        &git(
            dir,
            &[
                "clone",
                "-q",
                "--no-checkout",
                dir.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
        ),
        "git clone --no-checkout",
    );
    assert_ok(
        &git(clone_dir, &["config", "core.autocrlf", "true"]),
        "git config core.autocrlf true",
    );
    assert_ok(
        &git(clone_dir, &["checkout", "-q", "main"]),
        "git checkout main",
    );
    assert!(read(clone_dir, "gat.lock").contains("\r\n"));

    assert_ok(&gat(clone_dir, &["init"]), "gat init in clone");
    // `gat remote add origin ...` from before the clone was already
    // committed inside the tracked `gat.yaml` project config, so the
    // clone inherits it -- re-adding here would now be rejected as a
    // duplicate remote; use `remote update` to change an existing name.
    assert_ok(&gat(clone_dir, &["fetch"]), "gat fetch");
    assert_ok(&gat(clone_dir, &["sync"]), "gat sync");

    assert_eq!(read(clone_dir, "big.bin"), "payload");
}

/// `gat mount add` against a source whose committed `gat.lock` is itself
/// CRLF (a real `core.autocrlf=true` checkout on the source side) must
/// import the same rows as against an LF source, and this repo's own
/// `gat.lock` (which mount add writes into) must stay canonical LF.
#[test]
fn mount_add_tolerates_a_crlf_gat_lock_from_the_source_repository() {
    let source = init_repo();
    let source_dir = source.path();
    std::fs::write(source_dir.join("weights.bin"), b"weights").unwrap();
    assert_ok(&gat(source_dir, &["add", "weights.bin"]), "gat add");
    commit_all(source_dir, "add weights.bin");

    // Rewrite the source's own committed gat.lock blob to CRLF, so the
    // object `gat mount add` reads is itself CRLF, not merely
    // checkout-converted.
    let crlf_lock = read(source_dir, "gat.lock").replace('\n', "\r\n");
    std::fs::write(source_dir.join("gat.lock"), &crlf_lock).unwrap();
    assert_ok(&git(source_dir, &["add", "gat.lock"]), "git add gat.lock");
    assert_ok(
        &git(source_dir, &["commit", "-q", "-m", "crlf lock"]),
        "git commit",
    );

    let dest = init_repo();
    let dest_dir = dest.path();
    assert_ok(
        &gat(
            dest_dir,
            &[
                "mount",
                "add",
                "vendor",
                source_dir.to_str().unwrap(),
                "vendor",
            ],
        ),
        "gat mount add",
    );

    let lock_text = read(dest_dir, "gat.lock");
    assert!(
        !lock_text.contains('\r'),
        "gat must always write gat.lock in canonical LF, got {lock_text:?}"
    );
    assert!(lock_text.contains("vendor/weights.bin"));
}

/// `gat diff rev1 rev2` merge-walks both revisions' persisted `gat.lock`
/// state ([`gat_io::LockSnapshot::with_shard_rows_pull`]
/// under the hood) to report changed rows. This must produce the same
/// result whether the compared commits' `gat.lock` blobs are canonical
/// LF or CRLF.
#[test]
fn diff_merge_walks_two_crlf_committed_lock_revisions_correctly() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("a.bin"), b"a").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add a.bin");
    commit_all(dir, "base");
    let crlf_base = read(dir, "gat.lock").replace('\n', "\r\n");
    std::fs::write(dir.join("gat.lock"), &crlf_base).unwrap();
    assert_ok(&git(dir, &["add", "gat.lock"]), "git add gat.lock");
    assert_ok(
        &git(dir, &["commit", "-q", "--amend", "-m", "base (crlf lock)"]),
        "git commit --amend",
    );

    std::fs::write(dir.join("b.bin"), b"b").unwrap();
    assert_ok(&gat(dir, &["add", "b.bin"]), "gat add b.bin");
    commit_all(dir, "add b.bin");
    let crlf_head = read(dir, "gat.lock").replace('\n', "\r\n");
    std::fs::write(dir.join("gat.lock"), &crlf_head).unwrap();
    assert_ok(&git(dir, &["add", "gat.lock"]), "git add gat.lock");
    assert_ok(
        &git(
            dir,
            &["commit", "-q", "--amend", "-m", "add b.bin (crlf lock)"],
        ),
        "git commit --amend",
    );

    let out = gat(dir, &["diff", "HEAD~1", "HEAD"]);
    assert_ok(&out, "gat diff HEAD~1 HEAD");
    let text = common::stdout(&out);
    assert!(text.contains("b.bin"), "diff output: {text}");
    assert!(!text.contains("a.bin"), "diff output: {text}");
}

/// way whether the two branches' `gat.lock` blobs use `LF` or `CRLF` --
/// Git hands the driver whatever bytes are in the repository (not
/// necessarily checkout-converted), so the driver's own parsing (via
/// `Lock::parse`) must tolerate both.
#[test]
fn merge_driver_resolves_a_crlf_committed_lock_cleanly() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("a.bin"), b"a").unwrap();
    std::fs::write(dir.join("d.bin"), b"d").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin", "d.bin"]), "gat add");
    commit_all(dir, "base");

    // Rewrite the just-committed gat.lock blob to CRLF and amend, so the
    // shared ancestor blob itself is CRLF (not merely checkout-converted).
    let crlf_lock = read(dir, "gat.lock").replace('\n', "\r\n");
    std::fs::write(dir.join("gat.lock"), &crlf_lock).unwrap();
    assert_ok(&git(dir, &["add", "gat.lock"]), "git add gat.lock");
    assert_ok(
        &git(dir, &["commit", "-q", "--amend", "-m", "base (crlf lock)"]),
        "git commit --amend",
    );

    assert_ok(
        &git(dir, &["switch", "-c", "feature"]),
        "git switch -c feature",
    );
    std::fs::write(dir.join("b.bin"), b"b").unwrap();
    assert_ok(&gat(dir, &["add", "b.bin"]), "gat add b.bin on feature");
    commit_all(dir, "feature adds b.bin");

    assert_ok(&git(dir, &["switch", "main"]), "git switch main");
    std::fs::write(dir.join("c.bin"), b"c").unwrap();
    assert_ok(&gat(dir, &["add", "c.bin"]), "gat add c.bin on main");
    commit_all(dir, "main adds c.bin");

    let merge = git(dir, &["merge", "-q", "--no-edit", "feature"]);
    assert_ok(&merge, "git merge feature");

    let merged = read(dir, "gat.lock");
    assert!(
        !merged.contains('\r'),
        "merged gat.lock must be canonical LF"
    );
    for path in ["a.bin", "b.bin", "c.bin", "d.bin"] {
        assert!(
            merged.contains(path),
            "merged lock missing {path}: {merged}"
        );
    }
}
