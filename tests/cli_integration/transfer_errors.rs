//! CLI-level characterization of typed `push`/`fetch`/`repair`
//! remote-transfer failures. These tests require a
//! clean, single, nonzero-exit failure with no panic and, critically, no raw
//! `opendal`/backend-internal wording or object-storage key reaching stderr.
//! That is the user-visible guarantee of the command/engine transfer errors
//! and root diagnostic mapping, independent of the lower-layer technical
//! source retained internally.

use crate::common::{assert_ok, commit_all, gat, git, init_repo, stderr};
use crate::support::remote_url;

/// Substrings that would indicate a low-level `opendal`/backend internal
/// detail leaked into a user-facing diagnostic, rather than the
/// sanitized, semantic wording `RemoteError`'s variants produce. Kept
/// deliberately narrow (case-sensitive backend/crate identifiers) so this
/// doesn't false-positive on ordinary English words like "unexpected".
const FORBIDDEN_INTERNAL_WORDING: &[&str] = &["opendal", "ErrorKind", "backtrace", "panicked at"];

fn assert_no_internal_wording(err: &str) {
    for needle in FORBIDDEN_INTERNAL_WORDING {
        assert!(
            !err.contains(needle),
            "stderr must never contain raw backend/internal wording ({needle:?}): {err}"
        );
    }
}

/// Unix filesystem permission bits are silently ignored by the root user
/// (and sometimes inside certain sandboxes), which would make the
/// permission-denial scenarios below spuriously pass/fail rather than
/// exercise the intended failure path. Detected empirically instead of
/// checking effective UID (avoiding a new `libc`/`nix` dependency for
/// this one thing): chmod a scratch directory read-only and see whether
/// a write into it is actually rejected.
#[cfg(unix)]
fn permission_denial_is_enforced() -> bool {
    use std::os::unix::fs::PermissionsExt;

    let probe = tempfile::tempdir().unwrap();
    std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let blocked = std::fs::write(probe.path().join("probe.txt"), b"x").is_err();
    // Restore so the tempdir can be cleaned up by its `Drop` impl.
    std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    blocked
}

/// A remote root nested underneath what is, on disk, actually a *file*
/// (not a directory) can never be reached: the parent path segment itself
/// isn't a directory, so no amount of retrying makes it valid. This is
/// the "unreachable file remote" scenario, characterized here at `remote
/// add` time -- `build_remote`'s own validation already
/// rejects this eagerly, before any push/fetch is ever attempted.
#[test]
fn adding_an_unreachable_file_remote_fails_cleanly() {
    let tmp = init_repo();
    let dir = tmp.path();
    let remote_parent = tempfile::tempdir().unwrap();
    let blocking_file = remote_parent.path().join("not_a_directory");
    std::fs::write(&blocking_file, b"i am a file, not a directory").unwrap();
    let unreachable_root = blocking_file.join("remote_root");

    let out = gat(
        dir,
        &["remote", "add", "origin", &remote_url(&unreachable_root)],
    );
    assert!(
        !out.status.success(),
        "adding an unreachable file remote must fail"
    );
    let err = stderr(&out);
    assert!(!err.is_empty(), "expected an error message on stderr");
    assert!(
        !err.contains("panicked at"),
        "must fail cleanly, not panic: {err}"
    );
    assert_no_internal_wording(&err);
}

/// A remote that was configured and reachable, then had its entire
/// backing directory removed before the transfer runs, characterizes an
/// "unavailable remote": distinct from "unreachable" (which was never
/// valid to begin with) in that the remote genuinely stopped being usable
/// between configuration and use.
#[test]
fn push_to_since_removed_remote_directory_fails_cleanly() {
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

    // The remote becomes unavailable after configuration, before the
    // transfer that needs it: its directory is removed and replaced with
    // a plain file at the exact same path, so even a backend that
    // auto-vivifies missing directories on write cannot silently recover
    // -- that path segment can never become a directory again.
    std::fs::remove_dir_all(remote_dir.path()).unwrap();
    std::fs::write(remote_dir.path(), b"the remote is gone").unwrap();

    let out = gat(dir, &["push"]);
    assert!(
        !out.status.success(),
        "push to a since-removed remote directory must fail"
    );
    let err = stderr(&out);
    assert!(!err.is_empty(), "expected an error message on stderr");
    assert!(
        !err.contains("panicked at"),
        "must fail cleanly, not panic: {err}"
    );
    assert_no_internal_wording(&err);
}

/// A remote directory that exists but denies write access characterizes
/// "remote permission failure where testable" -- skipped when running as
/// root (`euid == 0`), since root bypasses Unix permission bits entirely
/// and the scenario simply cannot be exercised in that environment.
#[test]
#[cfg(unix)]
fn push_to_permission_denied_remote_fails_cleanly() {
    use std::os::unix::fs::PermissionsExt;

    if !permission_denial_is_enforced() {
        eprintln!("skipping: Unix permission bits are not enforced in this environment");
        return;
    }

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

    // Deny write (and traversal-for-write) access to the remote root.
    std::fs::set_permissions(remote_dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    let out = gat(dir, &["push"]);

    // Restore permissions unconditionally so the tempdir can still be
    // cleaned up by its `Drop` impl regardless of the assertions below.
    std::fs::set_permissions(remote_dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        !out.status.success(),
        "push to a permission-denied remote must fail"
    );
    let err = stderr(&out);
    assert!(!err.is_empty(), "expected an error message on stderr");
    assert!(
        !err.contains("panicked at"),
        "must fail cleanly, not panic: {err}"
    );
    assert_no_internal_wording(&err);
}

/// An object the lockfile references but that is not
/// present in the local cache is the "missing local object" scenario --
/// exercised here on `push`, where it is a semantic, expected condition
/// (the object simply hasn't been fetched into this working copy's cache
/// yet), not a corruption or backend failure, so `push` reports it as a
/// clean per-item skip rather than a hard failure.
#[test]
fn push_of_missing_local_object_is_reported_cleanly_not_as_a_backend_failure() {
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

    // Simulate the object never having been cached locally.
    std::fs::remove_dir_all(dir.join(".gat/objects")).unwrap();
    std::fs::create_dir_all(dir.join(".gat/objects")).unwrap();

    let out = gat(dir, &["push"]);
    assert_ok(&out, "gat push");
    let err = stderr(&out);
    assert!(
        err.contains("big.bin"),
        "expected the skipped item's path named on stderr: {err}"
    );
    assert_no_internal_wording(&err);
}

/// A local cache directory that cannot be written to characterizes
/// "failed local cache write": `fetch` must report this as a clean,
/// local-filesystem failure -- distinct from any remote-side failure --
/// rather than a raw `std::io::Error`/panic. Skipped when running as
/// root, for the same reason as the permission-denied-remote scenario.
#[test]
#[cfg(unix)]
fn fetch_with_unwritable_local_cache_fails_cleanly() {
    use std::os::unix::fs::PermissionsExt;

    if !permission_denial_is_enforced() {
        eprintln!("skipping: Unix permission bits are not enforced in this environment");
        return;
    }

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

    let objects_dir = clone_dir.join(".gat/objects");
    std::fs::create_dir_all(&objects_dir).unwrap();
    std::fs::set_permissions(&objects_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let out = gat(clone_dir, &["fetch"]);

    // Restore permissions unconditionally so cleanup can proceed.
    std::fs::set_permissions(&objects_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        !out.status.success(),
        "fetch with an unwritable local cache directory must fail"
    );
    let err = stderr(&out);
    assert!(!err.is_empty(), "expected an error message on stderr");
    assert!(
        !err.contains("panicked at"),
        "must fail cleanly, not panic: {err}"
    );
    assert_no_internal_wording(&err);
}

/// A remote URL with an embedded secret (here, a query-string token) must
/// never have that secret echoed back on stderr, even when the URL itself
/// is invalid/unreachable -- `remote add`'s own `build_remote` validation
/// is what's exercised here, since it's the earliest point
/// such a URL is rejected. Uses an unsupported scheme (`ftp://`, no
/// network I/O ever attempted) rather than `http(s)://`, matching the
/// existing unit-level secret-redaction tests in `gat-io/src/remote.rs`.
#[test]
fn remote_add_with_embedded_secret_never_leaks_it_on_failure() {
    let tmp = init_repo();
    let dir = tmp.path();

    let secret = "s3kr1t-p4ssw0rd";
    let url = format!("ftp://host/path?token={secret}");

    let out = gat(dir, &["remote", "add", "origin", &url]);
    // Whether this specific URL is accepted or rejected at `add` time is
    // not the point of this test (that's `build_remote`'s own concern,
    // already unit-tested in `gat-io/src/remote.rs`); what matters here
    // is that *if* it fails, the secret is never in the failure text.
    let err = stderr(&out);
    assert!(
        !err.contains(secret),
        "a remote URL's embedded secret must never appear on stderr: {err}"
    );
}

/// Sanity check that `push`/`fetch`'s successful path (via the file
/// backend) never mentions `opendal` either
/// -- the absence-of-internal-wording assertion should hold on the happy
/// path's own (empty) stderr too, not just on failures.
#[test]
fn successful_push_and_fetch_never_mention_backend_internals_on_stderr() {
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

    let push_out = gat(dir, &["push"]);
    assert_ok(&push_out, "gat push");
    assert_no_internal_wording(&stderr(&push_out));

    std::fs::remove_dir_all(dir.join(".gat/objects")).unwrap();
    let fetch_out = gat(dir, &["fetch"]);
    assert_ok(&fetch_out, "gat fetch");
    assert_no_internal_wording(&stderr(&fetch_out));
}

#[test]
fn invalid_connect_timeout_is_rejected_only_when_network_io_is_needed() {
    use crate::common::gat_with_env;
    let tmp = init_repo();
    let dir = tmp.path();
    let env = [("GAT_CONNECT_TIMEOUT", Some("SYNTHETIC-SECRET"))];
    // hygiene-ok: construction-only fixture; the invalid timeout prevents network I/O.
    let url = "s3://fixture/root?region=fixture";
    assert_ok(
        &gat_with_env(dir, &["remote", "add", "origin", url], &env),
        "network-free remote validation",
    );
    assert_ok(
        &gat_with_env(dir, &["status", "--remote", "origin"], &env),
        "empty selection does not need readiness",
    );
    std::fs::write(dir.join("data.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "data.bin"]), "gat add");
    let out = gat_with_env(dir, &["push", "--remote", "origin"], &env);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("Invalid GAT_CONNECT_TIMEOUT"), "{err}");
    assert!(err.contains("positive whole number of seconds"), "{err}");
    assert!(!err.contains("SYNTHETIC-SECRET"), "{err}");
    assert_no_internal_wording(&err);
}

#[test]
fn file_remote_ignores_connect_timeout_override() {
    use crate::common::gat_with_env;
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
    std::fs::write(dir.join("data.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "data.bin"]), "gat add");
    assert_ok(
        &gat_with_env(
            dir,
            &["push", "--remote", "origin"],
            &[("GAT_CONNECT_TIMEOUT", Some("invalid"))],
        ),
        "file push ignores network timeout override",
    );
}
