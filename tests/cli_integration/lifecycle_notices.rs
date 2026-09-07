//! Lifecycle notices: process-level proof that
//! notices actually land on stderr (never stdout), that a successful
//! experimental command's exit code/stdout are unaffected, and that a
//! *failing* experimental command still emits its notice.

use crate::common::{assert_ok, gat, init_repo, stderr, stdout};

/// Experimental resource and maintenance commands print exactly one "Experimental"
/// notice to stderr on a successful run; the notice never leaks into
/// stdout (scripts parsing stdout must never see it) and the exit code
/// stays zero.
#[test]
fn experimental_commands_emit_one_stderr_notice_with_unchanged_success_semantics() {
    for args in [
        vec!["gc", "--dry-run"],
        vec!["mount", "list"],
        vec!["route", "list"],
        vec!["selection", "list"],
    ] {
        let tmp = init_repo();
        let out = gat(tmp.path(), &args);
        assert_ok(&out, &format!("gat {}", args.join(" ")));
        let err = stderr(&out);
        assert_eq!(
            err.matches("Experimental").count(),
            1,
            "gat {}: expected exactly one Experimental notice on stderr, got: {err}",
            args.join(" ")
        );
        assert!(
            !stdout(&out).contains("Experimental"),
            "gat {}: notice must never appear on stdout, got: {}",
            args.join(" "),
            stdout(&out)
        );
    }
}

/// A failing experimental command must still emit its lifecycle notice on
/// stderr (alongside the error), not just a successful one.
#[test]
fn failing_experimental_command_still_emits_its_stderr_notice() {
    let tmp = init_repo();
    let out = gat(tmp.path(), &["gc", "--remote", "does-not-exist"]);
    assert!(
        !out.status.success(),
        "expected `gat gc --remote does-not-exist` to fail"
    );
    let err = stderr(&out);
    assert!(
        err.contains("Experimental") && err.contains("`gat gc`"),
        "expected an Experimental notice for `gat gc` on stderr even on failure, got: {err}"
    );
    assert!(
        stdout(&out).is_empty(),
        "no stdout expected on this failure path"
    );
}

/// `cache.ingest_strategy=hybrid` prints an Experimental notice naming
/// the value; `=safe` (the stable default) prints no lifecycle notice at
/// all.
#[test]
fn config_ingest_strategy_hybrid_notice_and_safe_no_notice() {
    let tmp = init_repo();
    let dir = tmp.path();

    let hybrid = gat(dir, &["config", "cache.ingest_strategy", "hybrid"]);
    assert_ok(&hybrid, "gat config cache.ingest_strategy hybrid");
    let hybrid_err = stderr(&hybrid);
    assert!(
        hybrid_err.contains("Experimental") && hybrid_err.contains("hybrid"),
        "expected an Experimental notice naming hybrid, got: {hybrid_err}"
    );

    let safe = gat(dir, &["config", "cache.ingest_strategy", "safe"]);
    assert_ok(&safe, "gat config cache.ingest_strategy safe");
    let safe_err = stderr(&safe);
    assert!(
        !safe_err.contains("Experimental") && !safe_err.contains("Deprecated"),
        "expected no lifecycle notice for the stable `safe` value, got: {safe_err}"
    );
}

/// `cache.ingest_strategy=mmap` still works (parses and persists) and
/// prints a factual Deprecated notice that does not claim `safe` is a
/// direct replacement.
#[test]
fn config_ingest_strategy_mmap_still_works_and_emits_deprecation_notice() {
    let tmp = init_repo();
    let dir = tmp.path();

    let out = gat(dir, &["config", "cache.ingest_strategy", "mmap"]);
    assert_ok(&out, "gat config cache.ingest_strategy mmap");
    let err = stderr(&out);
    assert!(
        err.contains("Deprecated"),
        "expected a Deprecated notice, got: {err}"
    );
    assert!(
        !err.contains("Use `"),
        "mmap must not claim a direct replacement, got: {err}"
    );
    assert!(
        err.contains("stable default"),
        "expected factual wording naming the stable default, got: {err}"
    );

    let get = gat(dir, &["config", "cache.ingest_strategy"]);
    assert_ok(&get, "gat config cache.ingest_strategy (read)");
    assert!(stdout(&get).contains("mmap"));
}

/// `gat config git.exclude_patterns <value>` (write) and a bare read both
/// still work and emit a Deprecated notice naming `git.ignore_patterns`;
/// the write persists under the canonical key.
#[test]
fn config_git_exclude_patterns_alias_read_and_write_emit_deprecation_notice() {
    let tmp = init_repo();
    let dir = tmp.path();

    let write = gat(dir, &["config", "git.exclude_patterns", "/data/"]);
    assert_ok(&write, "gat config git.exclude_patterns /data/");
    let write_err = stderr(&write);
    assert!(
        write_err.contains("Deprecated") && write_err.contains("git.ignore_patterns"),
        "expected a Deprecated notice naming git.ignore_patterns, got: {write_err}"
    );

    // Persists under the canonical key, not the deprecated one.
    let yaml = std::fs::read_to_string(dir.join("gat.yaml")).unwrap();
    assert!(yaml.contains("ignore_patterns"));
    assert!(!yaml.contains("exclude_patterns"));

    let read = gat(dir, &["config", "git.exclude_patterns"]);
    assert_ok(&read, "gat config git.exclude_patterns (read)");
    assert!(
        stdout(&read).contains("/data/"),
        "expected the read to still return the persisted value: {}",
        stdout(&read)
    );
    let read_err = stderr(&read);
    assert!(
        read_err.contains("Deprecated") && read_err.contains("git.ignore_patterns"),
        "expected a Deprecated notice on read too, got: {read_err}"
    );
}
