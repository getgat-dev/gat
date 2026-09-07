//! Release artifact acceptance subset.
//!
//! Every test in this module is prefixed `release_artifact_` so the
//! release workflow can select exactly this subset (e.g. via
//! `cargo nextest run -E 'test(release_artifact_)'`) and run it against
//! the binary extracted from a candidate release archive, by setting
//! `GAT_TEST_BIN` rather than
//! relying on `CARGO_BIN_EXE_gat`. Ordinary `cargo test`/nextest runs
//! execute these same tests unmodified against Cargo's own compiled
//! binary, so there is exactly one implementation of this behavior, not
//! a second shell/PowerShell copy.
//!
//! This subset intentionally reuses the same helpers and the same
//! `file://` remote round-trip pattern as the rest of this crate,
//! rather than re-implementing setup: the goal is to prove the
//! packaged binary exhibits the same behavior already characterized
//! elsewhere, not to add a new scenario.

use crate::common::{assert_ok, commit_all, gat, init_repo_spawned, stdout};
use crate::support::remote_url;

#[test]
fn release_artifact_version_flag_reports_package_version_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let out = gat(tmp.path(), &["--version"]);
    assert_ok(&out, "gat --version");
    assert_eq!(
        stdout(&out),
        concat!("gat ", env!("CARGO_PKG_VERSION"), "\n")
    );
}

#[test]
fn release_artifact_help_flag_exits_zero_and_lists_top_level_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let out = gat(tmp.path(), &["--help"]);
    assert_ok(&out, "gat --help");
    let text = stdout(&out);
    for cmd in [
        "init", "add", "rm", "status", "diff", "push", "fetch", "sync", "remote",
    ] {
        assert!(text.contains(cmd), "--help output missing `{cmd}`: {text}");
    }
}

#[test]
fn release_artifact_init_configure_remote_add_push_pull_round_trip() {
    // Real Git repo initialization, then `gat init` run through the
    // packaged binary under test itself ([`init_repo_spawned`]) -- not
    // the in-process `TestRepo::empty_gat_repo` fixture every other test
    // file uses -- so this round trip proves the packaged binary's own
    // `gat init` (not merely its later commands) actually works.
    let tmp = init_repo_spawned();
    let dir = tmp.path();

    // The packaged binary's `gat init` must have actually installed the
    // artifacts a real `gat init` produces: Git hooks (post-checkout/
    // post-merge/post-rewrite dispatcher lines) and the `gat.lock`
    // semantic merge driver (local config + `info/attributes`
    // selector). A broken packaged `gat init` that silently no-ops must
    // fail here, not be masked by falling through to in-process fixture
    // setup for the rest of this test.
    let hooks_dir = dir.join(".git").join("hooks");
    for hook in ["post-checkout", "post-merge", "post-rewrite"] {
        let contents = std::fs::read_to_string(hooks_dir.join(hook))
            .unwrap_or_else(|e| panic!("packaged `gat init` did not install the {hook} hook: {e}"));
        assert!(
            contents.contains(&format!("gat hook {hook}")),
            "installed {hook} hook is missing its gat dispatcher line: {contents}"
        );
    }
    let git_config = std::fs::read_to_string(dir.join(".git").join("config"))
        .expect("reading .git/config after packaged `gat init`");
    assert!(
        git_config.contains("gat-lock"),
        "packaged `gat init` did not register the gat.lock merge driver in .git/config: \
         {git_config}"
    );
    let attributes = std::fs::read_to_string(dir.join(".git").join("info").join("attributes"))
        .expect("reading .git/info/attributes after packaged `gat init`");
    assert!(
        attributes.contains("merge=gat-lock"),
        "packaged `gat init` did not select the gat-lock merge driver in \
         .git/info/attributes: {attributes}"
    );

    // `file://` remote configuration.
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

    // `add` + commit + `push`.
    std::fs::write(dir.join("tracked.bin"), b"hello world").unwrap();
    assert_ok(&gat(dir, &["add", "tracked.bin"]), "gat add");
    commit_all(dir, "add tracked.bin");
    assert_ok(&gat(dir, &["push"]), "gat push");

    // Removal of the local working-tree object.
    std::fs::remove_file(dir.join("tracked.bin")).unwrap();
    assert!(!dir.join("tracked.bin").exists());

    // `pull` restores the file with the expected content.
    assert_ok(&gat(dir, &["pull"]), "gat pull");
    let content = std::fs::read_to_string(dir.join("tracked.bin")).unwrap();
    assert_eq!(content, "hello world");
}

/// Release-artifact counterpart of
/// `environment::spawned_ordinary_commands_ignore_a_conflicting_global_config_and_cache_dir_elsewhere`:
/// proves the *packaged* binary's explicit child environment (the same
/// [`common::gat_with_env`]-based isolation every other acceptance test
/// here relies on) -- not any ambient host state -- controls cache/
/// global-config resolution, including for `init_repo_spawned()`'s own
/// packaged `gat init`. First proves the packaged binary *does* honor a
/// deliberately conflicting `GAT_CACHE_DIR` override when a test
/// explicitly supplies one via `extra_env`, then proves an entirely
/// ordinary packaged `gat init`/`gat add` -- run with no `extra_env`
/// override -- resolves to the deterministic repo-local default
/// regardless of a conflicting global Gat config and cache-dir
/// relocation sitting on disk throughout.
#[test]
fn release_artifact_ordinary_commands_ignore_a_conflicting_global_config_and_cache_dir_elsewhere() {
    let conflicting_home = tempfile::tempdir().unwrap();
    let conflicting_global_config = conflicting_home.path().join(".gat");
    std::fs::create_dir_all(&conflicting_global_config).unwrap();
    let conflicting_cache_location = conflicting_home.path().join("conflicting-global-cache");
    std::fs::write(
        conflicting_global_config.join("gat.yaml"),
        format!(
            "cache:\n  location: {}\n",
            conflicting_cache_location.display()
        ),
    )
    .unwrap();
    let conflicting_cache_dir = tempfile::tempdir().unwrap();

    // Sanity: the packaged binary must actually honor an explicit
    // conflicting `GAT_CACHE_DIR` override, so the isolation proven
    // below is meaningful, not vacuous.
    let honoring_repo = init_repo_spawned();
    std::fs::write(honoring_repo.path().join("big.bin"), b"payload").unwrap();
    let honoring_out = crate::common::gat_with_env(
        honoring_repo.path(),
        &["add", "big.bin"],
        &[(
            "GAT_CACHE_DIR",
            Some(conflicting_cache_dir.path().to_str().unwrap()),
        )],
    );
    assert_ok(
        &honoring_out,
        "packaged gat add with an explicit conflicting override",
    );
    assert!(
        walk_has_any_file(conflicting_cache_dir.path()),
        "expected the explicit conflicting GAT_CACHE_DIR override to actually be honored by the \
         packaged binary"
    );
    let conflicting_cache_dir_count_before = count_files(conflicting_cache_dir.path());

    // Now prove an entirely ordinary `init_repo_spawned()` fixture plus
    // an ordinary spawned `gat add`, with no `extra_env` override at
    // all, still resolve to the deterministic repo-local default --
    // never the conflicting global config/cache relocation created
    // above, which remains on disk throughout.
    let ordinary_repo = init_repo_spawned();
    std::fs::write(ordinary_repo.path().join("big.bin"), b"payload").unwrap();
    assert_ok(
        &gat(ordinary_repo.path(), &["add", "big.bin"]),
        "ordinary packaged gat add",
    );

    let default_cache = ordinary_repo.path().join(".gat").join("objects");
    assert!(
        walk_has_any_file(&default_cache),
        "expected the ordinary packaged command to use the repo-local default cache: {}",
        default_cache.display()
    );
    assert!(
        !walk_has_any_file(&conflicting_cache_location),
        "ordinary packaged command must not have used the conflicting global cache.location"
    );
    assert_eq!(
        count_files(conflicting_cache_dir.path()),
        conflicting_cache_dir_count_before,
        "ordinary packaged command must not have reused the earlier explicit GAT_CACHE_DIR \
         override"
    );
}

fn walk_has_any_file(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if walk_has_any_file(&path) {
                return true;
            }
        } else if path.is_file() {
            return true;
        }
    }
    false
}

/// Recursively counts regular files under `dir` (see
/// `environment::count_files`'s identical helper, kept as a separate
/// copy since this is a distinct test binary/module).
fn count_files(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            count += count_files(&path);
        } else if path.is_file() {
            count += 1;
        }
    }
    count
}
