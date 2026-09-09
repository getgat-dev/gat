//! `gat system` maintenance namespace: inspecting and repairing a
//! corrupt state database, cache metadata database, or a stale managed
//! Git exclude block.

use crate::common::{assert_no_low_level_error_leak, assert_ok, gat, init_repo, stdout};

#[test]
fn system_inspect_reports_a_healthy_repo() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");

    let out = gat(dir, &["system", "inspect"]);
    assert_ok(&out, "gat system inspect");
    let report = stdout(&out);
    assert!(report.contains("System state: healthy"), "{report}");
    assert!(report.contains("Lock"), "{report}");
    assert!(report.contains("State"), "{report}");
    assert!(report.contains("Cache"), "{report}");
    assert!(report.contains("Git"), "{report}");
}

#[test]
fn system_repair_preserves_and_reports_an_invalid_lock_before_other_domains() {
    let tmp = init_repo();
    let dir = tmp.path();
    let invalid = b"not a gat lock\n";
    std::fs::write(dir.join("gat.lock"), invalid).unwrap();
    for scope in ["lock", "all"] {
        for full in [false, true] {
            let mut args = vec!["system", "repair", scope];
            if full {
                args.push("-o");
            }
            let out = gat(dir, &args);
            assert_ok(&out, "gat system repair reports unresolved state");
            let report = stdout(&out);
            assert!(report.contains("System repair: incomplete"), "{report}");
            assert!(report.contains("✗ Lock"), "{report}");
            assert!(report.contains("gat.lock:"), "{report}");
            assert!(!report.contains("no repair needed"), "{report}");
            assert!(!report.contains("✓ State"), "{report}");
            assert!(!report.contains("not a gat lock"), "{report}");
            assert_eq!(std::fs::read(dir.join("gat.lock")).unwrap(), invalid);
            assert_no_low_level_error_leak(&out, &["not a gat lock"]);
        }
    }
}

#[test]
fn system_repair_state_rebuilds_a_corrupt_database() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");
    let state_db = dir.join(".gat/state/state.sqlite3");
    std::fs::write(&state_db, b"not a sqlite database").unwrap();

    let inspect = gat(dir, &["system", "inspect", "state"]);
    assert_ok(&inspect, "gat system inspect state");
    assert!(stdout(&inspect).contains("unreadable"));

    assert_ok(
        &gat(dir, &["system", "repair", "state"]),
        "gat system repair state",
    );
    let reinspect = gat(dir, &["system", "inspect", "state"]);
    assert_ok(&reinspect, "gat system inspect state (after repair)");
    assert!(stdout(&reinspect).contains("desired"));
    assert!(stdout(&reinspect).contains("current"));
}

#[test]
fn system_repair_cache_rebuilds_a_corrupt_metadata_database() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");
    let cache_db = dir.join(".gat/objects/cache.sqlite3");
    std::fs::write(&cache_db, b"not a sqlite database").unwrap();

    let inspect = gat(dir, &["system", "inspect", "cache"]);
    assert_ok(&inspect, "gat system inspect cache");
    assert!(stdout(&inspect).contains("disabled"));

    assert_ok(
        &gat(dir, &["system", "repair", "cache"]),
        "gat system repair cache",
    );
    let reinspect = gat(dir, &["system", "inspect", "cache"]);
    assert_ok(&reinspect, "gat system inspect cache (after repair)");
    assert!(stdout(&reinspect).contains("cache metadata"));
    assert!(stdout(&reinspect).contains("healthy"));
}

#[test]
fn system_repair_cache_leaves_healthy_metadata_untouched() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");
    let cache_db = dir.join(".gat/objects/cache.sqlite3");
    assert_ok(
        &gat(dir, &["system", "repair", "cache"]),
        "initial gat system repair cache",
    );
    let before = std::fs::read(&cache_db).unwrap();

    for (args, state) in [
        (vec!["system", "repair", "cache"], "valid"),
        (vec!["system", "repair", "cache", "-o"], "already valid"),
    ] {
        let repair = gat(dir, &args);
        assert_ok(&repair, "gat system repair cache");
        assert!(
            stdout(&repair)
                .lines()
                .any(|line| line.starts_with("✓  cache metadata") && line.ends_with(state))
        );
        assert_eq!(
            std::fs::read(&cache_db).unwrap(),
            before,
            "repair must not rewrite healthy shared cache metadata"
        );
    }
}

#[test]
fn system_clean_cache_refuses_before_purging_temporary_files_for_unsupported_schema() {
    let tmp = init_repo();
    let dir = tmp.path();
    let objects = dir.join(".gat/objects");
    std::fs::create_dir_all(&objects).unwrap();
    let cache_db = objects.join("cache.sqlite3");
    {
        let conn = rusqlite::Connection::open(&cache_db).unwrap();
        conn.pragma_update(None, "user_version", 999_i64).unwrap();
    }
    let temporary = objects.join("tmp-in-progress");
    std::fs::write(&temporary, b"partial").unwrap();

    let clean = gat(
        dir,
        &[
            "system",
            "clean",
            "cache",
            "--purge-temporary",
            "--purge-objects",
        ],
    );

    assert!(!clean.status.success());
    assert!(
        temporary.exists(),
        "unsupported cache metadata must be rejected before any destructive cleanup"
    );
}

#[test]
fn system_repair_git_regenerates_the_managed_exclude_block() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");

    std::fs::write(dir.join(".git/info/exclude"), "# user-managed\n").unwrap();
    let inspect = gat(dir, &["system", "inspect", "git"]);
    assert_ok(&inspect, "gat system inspect git");
    assert!(stdout(&inspect).contains("stale"));

    assert_ok(
        &gat(dir, &["system", "repair", "git"]),
        "gat system repair git",
    );
    let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
    assert!(exclude.contains("# >>> gat >>>"));
    assert!(exclude.contains("asset.bin"));
    assert!(exclude.contains("# user-managed"));
}

/// A cache metadata database file that opens as `SQLite` but fails its
/// `PRAGMA integrity_check` (as opposed to being unopenable garbage
/// bytes, covered by `system_repair_cache_rebuilds_a_corrupt_metadata_database`
/// above) must be reported without leaking raw `SQLite` vocabulary.
#[test]
fn system_inspect_cache_integrity_check_failure_leaks_no_low_level_vocabulary() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");

    let cache_db = dir.join(".gat/objects/cache.sqlite3");
    std::fs::create_dir_all(cache_db.parent().unwrap()).unwrap();
    corrupt_sqlite_page_data(&cache_db);

    let inspect = gat(dir, &["system", "inspect", "cache"]);
    assert_no_low_level_error_leak(&inspect, &[]);
}

/// A state database file that opens as `SQLite` but fails its integrity
/// check must be reported without leaking raw `SQLite` vocabulary.
#[test]
fn system_inspect_state_integrity_check_failure_leaks_no_low_level_vocabulary() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("asset.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "asset.bin"]), "gat add");

    let state_db = dir.join(".gat/state/state.sqlite3");
    corrupt_sqlite_page_data(&state_db);

    let inspect = gat(dir, &["system", "inspect", "state"]);
    assert_no_low_level_error_leak(&inspect, &[]);
}

/// Overwrites the tail of a valid `SQLite` file with garbage, leaving the
/// header intact so the database *opens* successfully but fails its
/// `PRAGMA integrity_check` -- distinct from the "garbage bytes,
/// unopenable" case exercised elsewhere in this file.
fn corrupt_sqlite_page_data(path: &std::path::Path) {
    use std::io::{Seek, SeekFrom, Write};
    // A repo's default cache is disabled; create a minimal, genuinely
    // valid SQLite file at this path first (so opening it and failing
    // its own integrity check both actually engage the SQLite/rusqlite
    // codepath rather than short-circuiting into the "unopenable
    // garbage" branch already covered by the tests above).
    {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute("CREATE TABLE t (x INTEGER)", []).unwrap();
        conn.execute("INSERT INTO t (x) VALUES (1)", []).unwrap();
    }
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let len = file.metadata().unwrap().len();
    // SQLite's header occupies the first 100 bytes; corrupt everything
    // past it so the file is still recognized as SQLite but its actual
    // page data is garbage.
    let corrupt_from = 100.min(len);
    file.seek(SeekFrom::Start(corrupt_from)).unwrap();
    let garbage = vec![
        0xFFu8;
        usize::try_from(len - corrupt_from)
            .expect("corruption range must fit in usize")
    ];
    file.write_all(&garbage).unwrap();
}

/// A `gat.lock` that Gat cannot parse (see
/// `error_leak_adversarial::corrupt_lock_file_failure_leaks_no_low_level_vocabulary`)
/// also makes `gat system inspect git`'s exclude-validation step take
/// its error path (it depends on resolving Gat's lock state to compute
/// the expected managed exclude block); it must report this without
/// leaking parser-internal vocabulary.
#[test]
fn system_inspect_git_exclude_validation_failure_leaks_no_low_level_vocabulary() {
    let tmp = init_repo();
    let dir = tmp.path();
    let sentinel = "SENTINEL_GIT_INSPECT_LOCK_3fa08e21";
    std::fs::write(
        dir.join("gat.lock"),
        format!("{sentinel}\nnot a real lock file\n"),
    )
    .unwrap();
    crate::common::git(dir, &["add", "-A"]);
    crate::common::commit_all(dir, "corrupt lock");

    let inspect = gat(dir, &["system", "inspect", "git"]);
    assert_no_low_level_error_leak(&inspect, &[sentinel]);
}
