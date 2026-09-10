//! Adversarial CLI-level characterization of low-level error leakage. Every
//! scenario here deliberately
//! injects a distinctive, unique sentinel into a failure path and then
//! asserts, via `assert_no_low_level_error_leak`/`assert_no_secret_leak`
//! (`tests/common/mod.rs`), that neither the sentinel nor any baseline
//! low-level/backend vocabulary reaches stdout or stderr -- only Gat's
//! own typed, product-level wording should ever be user-visible.

use crate::common;
use crate::common::{
    assert_no_low_level_error_leak, assert_no_secret_leak, assert_ok, commit_all, gat, git,
    init_repo, stderr, stdout,
};
use crate::support::remote_url;

/// A corrupt materialized-state store (the file exists but is not a
/// valid state-store, distinguishable from `system_repair.rs`'s
/// `system repair state` happy path because here the corruption is
/// surfaced through an *ordinary* command, not `system inspect`/`repair`
/// themselves) must fail cleanly without leaking the store's own
/// on-disk format/implementation details.
#[test]
fn corrupt_state_store_failure_leaks_no_low_level_vocabulary() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");
    assert_ok(&gat(dir, &["sync"]), "gat sync");

    let state_db = dir.join(".gat/state/state.sqlite3");
    // A syntactically-plausible-looking but semantically bogus sentinel:
    // if this ever leaked verbatim into user output it would be
    // immediately recognizable as a raw file dump, not a real diagnostic.
    let sentinel = "SENTINEL_CORRUPT_STATE_STORE_c3f9a11d";
    std::fs::write(&state_db, sentinel).unwrap();

    let out = gat(dir, &["status"]);
    assert!(
        !out.status.success(),
        "status against a corrupt state store must fail"
    );
    assert_no_low_level_error_leak(&out, &[sentinel]);
}

/// An unreachable `file://` remote root (parent path segment is a file,
/// not a directory -- so no retry ever makes it valid) must fail cleanly
/// without leaking `opendal`/backend internals.
#[test]
fn unreachable_file_remote_failure_leaks_no_low_level_vocabulary() {
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
    assert_no_low_level_error_leak(&out, &[]);
}

/// Pushing to a remote URL carrying a credential-bearing query parameter
/// must never leak that credential verbatim into stdout/stderr, even
/// when the push itself fails (unreachable backend).
#[test]
fn push_failure_never_leaks_a_credential_bearing_remote_url() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");
    commit_all(dir, "add asset.bin");

    // A distinctive fake token that would only appear in output if the
    // raw configured remote URL (rather than a redacted form) leaked.
    let secret_token = "SECRET_TOKEN_a8f42e97";
    let unreachable = tempfile::tempdir().unwrap();
    let blocking_file = unreachable.path().join("not_a_directory");
    std::fs::write(&blocking_file, b"blocker").unwrap();
    let bogus_root = blocking_file.join("remote_root");
    let base_url = remote_url(&bogus_root);
    let mut configured = url::Url::parse(&base_url).unwrap();
    configured
        .query_pairs_mut()
        .append_pair("root", bogus_root.join(secret_token).to_str().unwrap());
    let url_with_secret = configured.to_string();

    let out_add = gat(dir, &["remote", "add", "origin", &url_with_secret]);
    // `remote add` may itself eagerly reject/validate the unreachable
    // file path (its URL is already redacted either way); assert
    // no-secret-leak on this step regardless of whether it succeeds.
    assert_no_secret_leak(&out_add, &[secret_token]);
    assert_no_low_level_error_leak(&out_add, &[]);
    if !out_add.status.success() {
        return;
    }
    let out = gat(dir, &["push"]);
    assert!(
        !out.status.success(),
        "push to an unreachable remote must fail"
    );
    assert_no_secret_leak(&out, &[secret_token]);
    assert_no_low_level_error_leak(&out, &[]);
}

/// A `gat.lock` file that isn't a lock file at all (arbitrary garbage,
/// distinguishable from ordinary malformed-syntax cases by embedding a
/// unique sentinel) must fail cleanly without echoing the raw file
/// content or any parser-internal vocabulary.
#[test]
fn corrupt_lock_file_failure_leaks_no_low_level_vocabulary() {
    let tmp = init_repo();
    let dir = tmp.path();
    let sentinel = "SENTINEL_NOT_A_LOCK_FILE_9d21bb60";
    std::fs::write(
        dir.join("gat.lock"),
        format!("{sentinel}\nnot a real lock file\n"),
    )
    .unwrap();
    git(dir, &["add", "-A"]);
    commit_all(dir, "corrupt lock");

    let out = gat(dir, &["status"]);
    assert!(
        !out.status.success(),
        "status against a corrupt gat.lock must fail"
    );
    assert_no_low_level_error_leak(&out, &[sentinel]);
}

/// A repository path that Gat's own hierarchy treats as infrastructure
/// (`.git`) is refused with typed wording, not a raw filesystem/gix
/// internal error, when passed directly to `add`.
/// A `sync --repair` attempt whose remote object was itself deleted out
/// from under it (so the re-fetch genuinely fails, not just the initial
/// corruption detection) must report the repair-failure row without any
/// low-level opendal/backend vocabulary -- exercises
/// `commands::sync::repair_failure_rows`/
/// `error::map::problem::repair_problem`, the mapping this session moved
/// out of `RepairError::safe_summary()`.
#[test]
fn repair_failure_row_leaks_no_low_level_vocabulary_when_the_remote_object_is_missing() {
    use gat_io::{LockStore, RepositoryLayout};

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

    // Corrupt the local cache copy so a re-fetch is required...
    let oid = LockStore::load_repository(&gat_io::RepositoryLayout::at((dir).to_path_buf()))
        .unwrap()
        .entries[0]
        .oid;
    let cache_root = RepositoryLayout::at(dir.to_path_buf()).resolve_cache_root(None);
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

    // ...and remove everything the remote itself stored, so the
    // repair's re-fetch genuinely fails instead of succeeding.
    for entry in std::fs::read_dir(remote_dir.path()).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }

    let out = gat(dir, &["sync", "--repair"]);
    assert!(
        !out.status.success(),
        "repair should fail: the remote no longer has the object to re-fetch"
    );
    assert_no_low_level_error_leak(&out, &["opendal", "OpenDAL", "std::io::Error"]);
    // The safe, authored repair-failure summary should still be present.
    assert!(
        stdout(&out).contains("repair") || stderr(&out).contains("repair"),
        "expected the repair-failure row/summary to still be reported"
    );
}

/// Explicit peer clone failures must redact credentials.
#[test]
fn gc_repository_clone_failure_redacts_secrets() {
    let repo = init_repo();
    let secret = format!(
        "{}?token=SUPER-SECRET-VALUE",
        remote_url(&repo.path().join("gone"))
    );
    let out = gat(repo.path(), &["gc", "--repository", &secret]);
    assert_eq!(out.status.code(), Some(1));
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    // Long temporary paths can wrap the authored explanation on macOS.
    let words = combined.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(words.contains("could not be cloned"), "{combined}");
    assert!(combined.contains("REDACTED"), "{combined}");
    assert_no_secret_leak(&out, &["SUPER-SECRET-VALUE"]);
    assert_no_low_level_error_leak(&out, &[]);
}

/// An explicitly supplied shared-cache peer whose repository root has vanished
/// (deleted before `gc` runs) triggers
/// `GcError::IncompleteKeepSet`: the refusal must
/// surface only Gat's own product wording -- the explicit peer's own
/// path plus one authored problem phrase -- never a raw `std::io::Error`
/// message, and must exit `1`.
#[test]
fn gc_fails_closed_and_leaks_nothing_when_an_explicit_peer_is_missing() {
    let shared_cache = tempfile::tempdir().unwrap();

    let peer = init_repo();
    std::fs::write(peer.path().join("peer.bin"), b"peer-payload").unwrap();
    let add_out = common::gat_with_env(
        peer.path(),
        &["add", "peer.bin"],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert_ok(&add_out, "gat add (peer, shared cache)");

    let main = init_repo();
    let main_dir = main.path();
    std::fs::write(main_dir.join("main.bin"), b"main-payload").unwrap();
    let add_main = common::gat_with_env(
        main_dir,
        &["add", "main.bin"],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert_ok(&add_main, "gat add (main, shared cache)");
    commit_all(main_dir, "add main.bin");

    // The explicit peer's repository root vanishes entirely: its path
    // cannot be canonicalized or resolved.
    let peer_path = peer.path().to_path_buf();
    drop(peer);
    assert!(
        !peer_path.exists(),
        "peer repo root must actually be gone before gc runs"
    );

    let out = common::gat_with_env(
        main_dir,
        &["gc", "--repository", peer_path.to_str().unwrap()],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert!(
        !out.status.success(),
        "gc must refuse to proceed with an unresolvable explicit peer"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a fail-closed gc refusal is an application failure, not a Clap usage error"
    );
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        combined
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("could not be inspected"),
        "expected the usual fail-closed refusal wording, got: {combined}"
    );
    assert!(
        combined.contains("could not be cloned"),
        "expected the authored repository problem phrase, got: {combined}"
    );
    assert_no_low_level_error_leak(&out, &[]);
}

/// Malformed explicit locations must not expose parser diagnostics.
#[test]
fn gc_repository_invalid_location_is_redacted() {
    let repo = init_repo();
    // hygiene-ok: synthetic malformed URL; never dialed.
    let out = gat(
        repo.path(),
        // hygiene-ok: malformed parser input, never dialed.
        &["gc", "--repository", "https://SENTINEL bogus host/repo.git"],
    );
    assert_eq!(out.status.code(), Some(1));
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    assert!(combined.contains("not a valid Git location"), "{combined}");
    assert_no_low_level_error_leak(&out, &["SENTINEL", "gix"]);
}

/// An explicitly supplied local repository whose repository root path contains a
/// literal newline plus text resembling a forged `hint:`/`error:` line
/// must still surface `gc`'s fail-closed refusal as exactly one rendered
/// line per repository issue: the embedded newline must never manufacture
/// an extra line, and the forged `hint:`/`error:` prefix must never be
/// interpreted as a real diagnostic line by anything reading the output.
#[test]
#[cfg(unix)]
fn gc_repository_path_containing_a_literal_newline_never_forges_an_extra_rendered_line() {
    let shared_cache = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();

    // A directory name containing a literal newline plus forged
    // `hint:`/`error:`-shaped text: valid on Unix filesystems (a path
    // component may contain any byte except NUL and `/`), but must never
    // be treated as literal terminal structure once rendered.
    let peer_name = "peer\nhint: forged\nerror: forged repo";
    let peer_dir = scratch.path().join(peer_name);
    std::fs::create_dir(&peer_dir).expect("creating a directory whose name embeds a newline");
    assert_ok(
        &git(&peer_dir, &["init", "-q", "-b", "main"]),
        "git init (peer)",
    );
    assert_ok(&gat(&peer_dir, &["init"]), "gat init (peer)");
    std::fs::write(peer_dir.join("peer.bin"), b"peer-payload").unwrap();
    let add_out = common::gat_with_env(
        &peer_dir,
        &["add", "peer.bin"],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert_ok(&add_out, "gat add (peer, shared cache)");
    // Corrupt the peer's own `gat.lock` so this repository fails
    // *inspection* (as opposed to failing to resolve at all) once `gc`
    // walks it -- proving the newline/forged-line handling holds for the
    // `Inspection` reason too, not just `CloneFailed`.
    std::fs::write(peer_dir.join("gat.lock"), b"not a real lock file\n").unwrap();
    assert_ok(&git(&peer_dir, &["add", "-A"]), "git add -A (peer)");
    assert_ok(
        &git(&peer_dir, &["commit", "-q", "-m", "corrupt lock"]),
        "git commit (peer)",
    );

    let main = init_repo();
    let main_dir = main.path();
    std::fs::write(main_dir.join("main.bin"), b"main-payload").unwrap();
    let add_main = common::gat_with_env(
        main_dir,
        &["add", "main.bin"],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert_ok(&add_main, "gat add (main, shared cache)");
    commit_all(main_dir, "add main.bin");

    let out = common::gat_with_env(
        main_dir,
        &["gc", "--repository", peer_dir.to_str().unwrap()],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert!(
        !out.status.success(),
        "gc must refuse to proceed while an explicit peer fails inspection"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a fail-closed gc refusal is an application failure, not a Clap usage error"
    );
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    // The embedded newline must be escaped as printable text (visible as
    // its escape sequence), never split across multiple raw lines.
    assert!(
        !combined.contains("peer\nhint:"),
        "the repository path's embedded newline must never appear as a raw, unescaped \
         line break in the rendered output, got: {combined}"
    );
    // No line may begin with a forged `hint:`/`error:` prefix that did
    // not originate from Gat's own authored diagnostic structure.
    for line in combined.lines() {
        let trimmed = line.trim_start();
        assert!(
            !trimmed.starts_with("hint: forged") && !trimmed.starts_with("error: forged"),
            "a forged hint:/error: line must never appear as its own rendered line, got: {combined}"
        );
    }
    assert_no_low_level_error_leak(&out, &[]);
}

/// Every raw control character a local repository's own root path could
/// plausibly embed (ESC, CR, TAB, BEL, DEL) must be escaped as printable
/// text in `gc`'s repository-issue rendering -- never left raw, which
/// could otherwise manipulate the user's terminal (cursor movement,
/// bell, etc.) or masquerade as additional structure.
#[test]
#[cfg(unix)]
fn gc_repository_path_containing_raw_control_characters_is_always_escaped() {
    let shared_cache = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();

    // ESC, CR, TAB, BEL, DEL -- NUL is deliberately excluded, since no
    // Unix filesystem path component can contain it at all.
    let peer_name = "peer_\u{1b}_\r_\t_\u{7}_\u{7f}_ctrl";
    let peer_dir = scratch.path().join(peer_name);
    std::fs::create_dir(&peer_dir)
        .expect("creating a directory whose name embeds raw control characters");
    assert_ok(
        &git(&peer_dir, &["init", "-q", "-b", "main"]),
        "git init (peer)",
    );
    assert_ok(&gat(&peer_dir, &["init"]), "gat init (peer)");
    std::fs::write(peer_dir.join("peer.bin"), b"peer-payload").unwrap();
    let add_out = common::gat_with_env(
        &peer_dir,
        &["add", "peer.bin"],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert_ok(&add_out, "gat add (peer, shared cache)");

    let main = init_repo();
    let main_dir = main.path();
    std::fs::write(main_dir.join("main.bin"), b"main-payload").unwrap();
    let add_main = common::gat_with_env(
        main_dir,
        &["add", "main.bin"],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert_ok(&add_main, "gat add (main, shared cache)");
    commit_all(main_dir, "add main.bin");

    // The explicit peer's repository root vanishes entirely, exactly as
    // in the unresolvable-path test, so the control-character-laden path
    // itself is what gets rendered as the repository's identity.
    drop(std::fs::remove_dir_all(&peer_dir));
    assert!(
        !peer_dir.exists(),
        "peer repo root must actually be gone before gc runs"
    );

    let out = common::gat_with_env(
        main_dir,
        &["gc", "--repository", peer_dir.to_str().unwrap()],
        &[(
            "GAT_CACHE_LOCATION",
            Some(shared_cache.path().to_str().unwrap()),
        )],
    );
    assert!(
        !out.status.success(),
        "gc must refuse to proceed with an unresolvable explicit peer"
    );
    let raw_bytes = [out.stdout.as_slice(), out.stderr.as_slice()].concat();
    for raw_control in [0x1b_u8, b'\r', b'\t', 0x07, 0x7f] {
        assert!(
            !raw_bytes.contains(&raw_control),
            "raw control byte {raw_control:#x} must never appear unescaped in gc's output"
        );
    }
    assert_no_low_level_error_leak(&out, &[]);
}

#[test]
fn adding_git_infrastructure_path_leaks_no_low_level_vocabulary() {
    let tmp = init_repo();
    let dir = tmp.path();

    let out = gat(dir, &["add", ".git/config"]);
    assert!(!out.status.success(), "adding .git/config must fail");
    assert_no_low_level_error_leak(&out, &[]);
}

/// A local cache directory that denies write access must fail cleanly
/// with only Gat product wording -- no raw filesystem/`os error`
/// vocabulary -- and, like every application failure, exit `1`
/// specifically (not some other nonzero code).
#[test]
#[cfg(unix)]
fn permission_denied_cache_directory_fails_cleanly_and_exits_one() {
    use std::os::unix::fs::PermissionsExt;

    let probe = tempfile::tempdir().unwrap();
    std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let write_blocked = std::fs::write(probe.path().join("probe.txt"), b"x").is_err();
    std::fs::set_permissions(probe.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    if !write_blocked {
        eprintln!("skipping: Unix permission bits are not enforced in this environment");
        return;
    }

    let tmp = init_repo();
    let dir = tmp.path();
    let objects_dir = dir.join(".gat/objects");
    std::fs::create_dir_all(&objects_dir).unwrap();
    std::fs::set_permissions(&objects_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    let out = gat(dir, &["add", "asset.bin"]);

    // Restore permissions unconditionally so the tempdir can be cleaned
    // up regardless of the assertions below.
    std::fs::set_permissions(&objects_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        !out.status.success(),
        "add against a permission-denied cache directory must fail"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "application failures must exit 1"
    );
    assert_no_low_level_error_leak(&out, &[]);
}

/// `gat remote add`/`gat remote update`'s *success* confirmation must
/// route the configured URL through `RedactedUrl`/`UserLine::redacted_url`
/// rather than a generic string conversion. Unlike the
/// adversarial tests above, which only exercise *failure* paths, this
/// proves the happy-path wiring: a credential-bearing URL survives all
/// the way from the raw CLI argument through `RemoteOutcome::Added`/
/// `Updated` into the rendered confirmation line without the credential
/// ever appearing verbatim.
#[test]
fn remote_add_and_update_success_message_never_leaks_the_configured_credential() {
    let tmp = init_repo();
    let dir = tmp.path();

    let remote_root = tempfile::tempdir().unwrap();
    let base_url = remote_url(remote_root.path());
    // hygiene-ok: synthetic never-dialed test secret in a query string.
    let secret_token = "s3cr3t-token-b91cf2";
    let mut configured = url::Url::parse(&base_url).unwrap();
    configured.query_pairs_mut().append_pair(
        "root",
        remote_root.path().join(secret_token).to_str().unwrap(),
    );
    let url_with_secret = configured.to_string();

    let out_add = gat(dir, &["remote", "add", "origin", &url_with_secret]);
    assert_ok(&out_add, "gat remote add");
    assert_no_secret_leak(&out_add, &[secret_token]);
    assert!(
        stderr(&out_add).contains("root=REDACTED"),
        "the confirmation should show a redacted query key: {}",
        stderr(&out_add)
    );

    let shown = gat(dir, &["remote", "show", "origin"]);
    assert_ok(&shown, "gat remote show");
    assert_no_secret_leak(&shown, &[secret_token]);
    let output = stdout(&shown);
    for detail in [
        "origin",
        "root=REDACTED",
        "Default",
        "no",
        "Defined in",
        "project",
    ] {
        assert!(output.contains(detail), "missing {detail}: {output}");
    }

    let other_root = tempfile::tempdir().unwrap();
    let other_base_url = remote_url(other_root.path());
    // hygiene-ok: synthetic never-dialed test secret in a query string.
    let secret_token_2 = "h4x0r-token-7ad310";
    let mut configured = url::Url::parse(&other_base_url).unwrap();
    configured.query_pairs_mut().append_pair(
        "root",
        other_root.path().join(secret_token_2).to_str().unwrap(),
    );
    let url_with_secret_2 = configured.to_string();

    let out_update = gat(
        dir,
        &["remote", "update", "origin", "--url", &url_with_secret_2],
    );
    assert_ok(&out_update, "gat remote update");
    assert_no_secret_leak(&out_update, &[secret_token_2]);
    assert!(
        stderr(&out_update).contains("root=REDACTED"),
        "the confirmation should show a redacted query key: {}",
        stderr(&out_update)
    );
}
