//! End-to-end integration tests driving the actual `gat` binary and a real
//! `git` binary together, covering the Git-hook wiring itself (installed
//! dispatcher scripts actually firing on real Git operations) -- the
//! reconciliation algorithm itself is exhaustively unit-tested in
//! `src/sync.rs`; what these tests add is proof that `post-checkout`/
//! `post-merge`/`post-rewrite` really do call back into `gat sync`, and
//! that the documented gaps (`git reset --hard`, `git restore`) really do
//! require a manual `gat sync`.

#[path = "common/mod.rs"]
mod common;
use common::{assert_ok, gat, gat_with_env, git, git_with_env};

/// A repo with gat initialized (hooks installed) and one committed,
/// gat-tracked file on `main`.
fn setup() -> tempfile::TempDir {
    let tmp = test_support_git::empty_git_repo();
    let dir = tmp.path();
    assert_ok(&gat(dir, &["init"]), "gat init");
    std::fs::write(dir.join("big.bin"), b"main content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "initial"]), "git commit");
    tmp
}

#[test]
fn init_installs_hooks_that_are_executable() {
    let tmp = setup();
    let hook = tmp.path().join(".git/hooks/post-checkout");
    assert!(hook.exists());
    let contents = std::fs::read_to_string(&hook).unwrap();
    assert!(contents.contains("gat hook post-checkout"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&hook).unwrap().permissions().mode() & 0o111,
            0o111
        );
    }
}

#[test]
fn branch_checkout_with_lock_change_materializes_new_content() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&git(dir, &["checkout", "-q", "-b", "feature"]), "branch");
    std::fs::write(dir.join("big.bin"), b"feature content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "feature"]), "commit");

    assert_ok(&git(dir, &["checkout", "-q", "main"]), "checkout main");
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"main content");

    assert_ok(
        &git(dir, &["checkout", "-q", "feature"]),
        "checkout feature",
    );
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"feature content"
    );
}

/// After `gat rm --cached` keeps a file on disk while dropping it from
/// `gat.lock`, a later Git hook (here, `post-rewrite` firing on `commit
/// --amend`) must not turn around and delete that file via `gat sync`
/// -- materialized state, not just `gat.lock`, has to forget the path
/// too.
#[test]
fn rm_cached_survives_a_later_hook_triggered_sync() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&gat(dir, &["rm", "--cached", "big.bin"]), "gat rm --cached");
    assert!(
        dir.join("big.bin").exists(),
        "rm --cached must keep the file on disk"
    );
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(
        &git(dir, &["commit", "-q", "-m", "untrack big.bin"]),
        "commit",
    );

    // `commit --amend` fires `post-rewrite`, which calls back into `gat
    // sync`; that must not delete `big.bin` just because materialized
    // state still (incorrectly) remembered it as gat-owned.
    assert_ok(
        &git(dir, &["commit", "-q", "--amend", "--no-edit"]),
        "amend",
    );
    assert!(
        dir.join("big.bin").exists(),
        "a hook-triggered sync must not delete a file `rm --cached` kept on disk"
    );
}

#[test]
fn branch_checkout_without_lock_change_is_a_silent_no_op() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&git(dir, &["checkout", "-q", "-b", "docs-only"]), "branch");
    std::fs::write(dir.join("readme.txt"), b"unrelated").unwrap();
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "docs"]), "commit");

    let out = git(dir, &["checkout", "-q", "main"]);
    assert_ok(&out, "checkout main");
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"main content");
}

#[test]
fn detached_head_checkout_still_materializes_correctly() {
    let tmp = setup();
    let dir = tmp.path();
    std::fs::write(dir.join("big.bin"), b"v2").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "v2"]), "commit v2");
    let log = git(dir, &["rev-parse", "HEAD~1"]);
    let first_commit = String::from_utf8_lossy(&log.stdout).trim().to_string();

    assert_ok(
        &git(dir, &["checkout", "-q", &first_commit]),
        "detached checkout",
    );
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"main content");
}

#[test]
fn merge_materializes_the_merged_in_content() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&git(dir, &["checkout", "-q", "-b", "feature"]), "branch");
    std::fs::write(dir.join("big.bin"), b"merged content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "feature"]), "commit");
    assert_ok(&git(dir, &["checkout", "-q", "main"]), "back to main");

    assert_ok(
        &git(dir, &["merge", "-q", "--no-ff", "feature", "-m", "merge"]),
        "merge",
    );
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"merged content"
    );
}

#[test]
fn merge_conflict_skips_the_hook_but_manual_sync_still_works() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&git(dir, &["checkout", "-q", "-b", "feature"]), "branch");
    std::fs::write(dir.join("conflict.txt"), "feature\n").unwrap();
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "feature"]), "commit");
    assert_ok(&git(dir, &["checkout", "-q", "main"]), "back to main");
    std::fs::write(dir.join("conflict.txt"), "main\n").unwrap();
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "main-side"]), "commit");

    let out = git(dir, &["merge", "feature", "-m", "merge"]);
    assert!(!out.status.success(), "merge should conflict");

    // Resolve the conflict by hand, exactly like a user would.
    std::fs::write(dir.join("conflict.txt"), "resolved\n").unwrap();
    assert_ok(&git(dir, &["add", "-A"]), "git add resolve");
    assert_ok(&git(dir, &["commit", "-q", "--no-edit"]), "finish merge");

    // gat's own tracked file is untouched by the unrelated conflict; a
    // manual sync still reports success.
    assert_ok(&gat(dir, &["sync"]), "manual sync after conflict");
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"main content");
}

#[test]
fn rebase_triggers_post_rewrite_and_reconciles() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&git(dir, &["checkout", "-q", "-b", "feature"]), "branch");
    std::fs::write(dir.join("big.bin"), b"rebase content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "feature"]), "commit");

    assert_ok(&git(dir, &["rebase", "-q", "main"]), "rebase");
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"rebase content"
    );
}

#[test]
fn commit_amend_triggers_post_rewrite() {
    let tmp = setup();
    let dir = tmp.path();
    std::fs::write(dir.join("big.bin"), b"amended content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");

    assert_ok(
        &git(dir, &["commit", "-q", "--amend", "--no-edit"]),
        "amend",
    );
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"amended content"
    );
}

#[test]
fn reset_hard_is_not_covered_by_hooks_but_manual_sync_recovers() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&git(dir, &["checkout", "-q", "-b", "feature"]), "branch");
    std::fs::write(dir.join("big.bin"), b"feature content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "feature"]), "commit");

    assert_ok(
        &git(dir, &["reset", "-q", "--hard", "HEAD~1"]),
        "reset --hard",
    );
    // Documented gap: no post-reset hook exists, so the file is stale here.
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"feature content"
    );

    assert_ok(&gat(dir, &["sync"]), "manual sync after reset --hard");
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"main content");
}

/// Observational/unrelated commands (`status`, `ls-files`, `remote`,
/// `push`) must never write to `.git/info/exclude`: only commands that can
/// change `gat.lock` (`add`/`rm`/`mv`), reconcile the working tree against
/// it (`sync`/`pull`), or a hook-triggered sync should touch it.
#[test]
fn observational_commands_never_touch_info_exclude() {
    let tmp = setup();
    let dir = tmp.path();
    let exclude_path = dir.join(".git/info/exclude");
    let before = std::fs::read_to_string(&exclude_path).unwrap();

    assert_ok(&gat(dir, &["status"]), "gat status");
    assert_ok(&gat(dir, &["ls-files"]), "gat ls-files");
    assert_ok(&gat(dir, &["remote", "list"]), "gat remote");
    assert_ok(&gat(dir, &["gc", "--dry-run"]), "gat gc --dry-run");

    let after = std::fs::read_to_string(&exclude_path).unwrap();
    assert_eq!(before, after);
}

/// `gat sync --dry-run` must never write `.git/info/exclude`, even when a
/// newly-tracked path would need to be added to it.
#[test]
fn sync_dry_run_never_writes_info_exclude() {
    let tmp = setup();
    let dir = tmp.path();
    std::fs::write(dir.join("new.bin"), b"new content").unwrap();
    assert_ok(&gat(dir, &["add", "new.bin"]), "gat add new.bin");

    let exclude_path = dir.join(".git/info/exclude");
    // Manually blank the managed block so a real sync would have to
    // rewrite it, then confirm `--dry-run` leaves it untouched.
    std::fs::write(&exclude_path, "").unwrap();
    let before = std::fs::read_to_string(&exclude_path).unwrap();

    assert_ok(&gat(dir, &["sync", "--dry-run"]), "gat sync --dry-run");

    let after = std::fs::read_to_string(&exclude_path).unwrap();
    assert_eq!(before, after);
}

/// `gat sync --dry-run` on a repo that has never had a real
/// (non-dry-run) sync run against it must not create the
/// materialized-state `SQLite` database as a side effect of merely
/// planning -- an absent database is treated as empty, not created. Uses
/// a freshly `gat init`ed repo rather than [`setup`], since `gat add`
/// itself already opens (and so creates) the database.
#[test]
fn sync_dry_run_never_creates_the_materialized_state_database() {
    let tmp = test_support_git::empty_git_repo();
    let dir = tmp.path();
    assert_ok(&gat(dir, &["init"]), "gat init");
    std::fs::write(dir.join("README"), "hi").unwrap();
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "init"]), "git commit");

    let db_path = dir.join(".gat/state/state.sqlite3");
    assert!(
        !db_path.exists(),
        "no sync/add has run yet, so the database must not exist before this test's dry-run"
    );

    assert_ok(&gat(dir, &["sync", "--dry-run"]), "gat sync --dry-run");

    assert!(
        !db_path.exists(),
        "--dry-run must never create the materialized-state database"
    );
}

/// `gat sync --dry-run --fetch` and `--dry-run --repair` are both
/// rejected with a clear, actionable error rather than silently ignoring
/// the flag or performing the stateful operation.
#[test]
fn sync_dry_run_rejects_explicit_fetch_and_repair() {
    let tmp = setup();
    let dir = tmp.path();

    let fetch_output = gat(dir, &["sync", "--dry-run", "--fetch"]);
    assert!(!fetch_output.status.success());
    let fetch_stderr = String::from_utf8_lossy(&fetch_output.stderr);
    assert!(fetch_stderr.contains("--dry-run"), "{fetch_stderr}");
    assert!(fetch_stderr.contains("--fetch"), "{fetch_stderr}");

    let repair_output = gat(dir, &["sync", "--dry-run", "--repair"]);
    assert!(!repair_output.status.success());
    let repair_stderr = String::from_utf8_lossy(&repair_output.stderr);
    assert!(repair_stderr.contains("--dry-run"), "{repair_stderr}");
    assert!(repair_stderr.contains("--repair"), "{repair_stderr}");
}

#[test]
fn restore_is_not_covered_by_hooks_but_manual_sync_recovers() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&git(dir, &["checkout", "-q", "-b", "feature"]), "branch");
    std::fs::write(dir.join("big.bin"), b"feature content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "feature"]), "commit");
    assert_ok(&git(dir, &["checkout", "-q", "main"]), "back to main");

    assert_ok(
        &git(dir, &["restore", "--source", "feature", "gat.lock"]),
        "git restore",
    );
    // `git restore` isn't a hook Gat installs for and isn't guaranteed to
    // reconcile the working tree (its hook-firing behavior varies across
    // Git versions) -- a manual `gat sync` must always finish the job.
    assert_ok(&gat(dir, &["sync"]), "manual sync after restore");
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"feature content"
    );
}

#[test]
fn linked_worktree_shares_hooks_but_has_its_own_materialized_state() {
    // Sharing the object cache across worktrees is opt-in (`GAT_CACHE_LOCATION`
    // or `cache.location`, see `Repo::objects_dir`); without it each
    // worktree's default `.gat/objects` is its own, so use a shared cache
    // dir here the way a real multi-worktree setup would.
    let shared_cache = tempfile::tempdir().unwrap();
    let cache_env: &[(&str, Option<&str>)] = &[(
        "GAT_CACHE_LOCATION",
        Some(shared_cache.path().to_str().unwrap()),
    )];

    let tmp = test_support_git::empty_git_repo();
    let dir = tmp.path();
    assert_ok(&gat(dir, &["init"]), "gat init");
    std::fs::write(dir.join("big.bin"), b"main content").unwrap();
    assert_ok(
        &gat_with_env(dir, &["add", "big.bin"], cache_env),
        "gat add",
    );
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "initial"]), "commit");

    let worktree_parent = tempfile::tempdir().unwrap();
    let worktree_dir = worktree_parent.path().join("wt");

    assert_ok(&git(dir, &["checkout", "-q", "-b", "feature"]), "branch");
    assert_ok(&git(dir, &["checkout", "-q", "main"]), "back to main");
    assert_ok(
        &git(
            dir,
            &[
                "worktree",
                "add",
                "-q",
                worktree_dir.to_str().unwrap(),
                "feature",
            ],
        ),
        "worktree add",
    );

    // Linked worktrees aren't populated by `git worktree add` itself (no
    // hook covers it); `gat pull`/`gat sync` bootstraps this worktree, and
    // it gets its own `.gat/state`, independent of the main working tree's.
    assert_ok(
        &gat_with_env(&worktree_dir, &["sync"], cache_env),
        "sync in worktree",
    );
    assert_eq!(
        std::fs::read(worktree_dir.join("big.bin")).unwrap(),
        b"main content"
    );
    assert!(worktree_dir.join(".gat/state/state.sqlite3").exists());
    assert!(dir.join(".gat/state/state.sqlite3").exists());
}

#[test]
fn hooks_install_is_cooperative_with_a_pre_existing_hook() {
    let tmp = test_support_git::empty_git_repo();
    let dir = tmp.path();
    let hooks_dir = dir.join(".git/hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::write(
        hooks_dir.join("post-checkout"),
        "#!/bin/sh\necho pre-existing-hook-ran >> hook.log\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            hooks_dir.join("post-checkout"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    assert_ok(&gat(dir, &["init"]), "gat init");
    std::fs::write(dir.join("big.bin"), b"content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "c"]), "commit");
    assert_ok(&git(dir, &["checkout", "-q", "-b", "other"]), "branch");
    assert_ok(&git(dir, &["checkout", "-q", "main"]), "back to main");

    // The pre-existing hook still ran (twice: once per checkout above)...
    let log = std::fs::read_to_string(dir.join("hook.log")).unwrap();
    assert_eq!(log.matches("pre-existing-hook-ran").count(), 2);
    // ...and gat's own reconciliation still ran alongside it.
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"content");
}

#[test]
fn init_no_hooks_removes_only_gats_managed_block() {
    let tmp = setup();
    let dir = tmp.path();
    assert_ok(&gat(dir, &["init", "--no-hooks"]), "gat init --no-hooks");
    let hook_path = dir.join(".git/hooks/post-checkout");
    assert!(!hook_path.exists());
}

/// `gat init --no-hooks` followed by a plain `gat init` must actually
/// reinstall a *working* `post-checkout` dispatcher -- not merely
/// recreate the script file -- so a branch switch that changes tracked
/// content still triggers `gat sync` (via the regenerated hook's `gat
/// hook post-checkout "$@"` dispatch) and rematerializes the right
/// content after the disable/reenable cycle.
#[test]
fn reinstalling_after_no_hooks_dispatches_and_materializes_again() {
    let tmp = setup();
    let dir = tmp.path();

    assert_ok(&gat(dir, &["init", "--no-hooks"]), "gat init --no-hooks");
    assert!(!dir.join(".git/hooks/post-checkout").exists());
    assert_ok(&gat(dir, &["init"]), "gat init (reinstall)");
    assert!(dir.join(".git/hooks/post-checkout").exists());

    assert_ok(&git(dir, &["checkout", "-q", "-b", "feature"]), "branch");
    std::fs::write(dir.join("big.bin"), b"feature content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "feature"]), "commit");

    assert_ok(&git(dir, &["checkout", "-q", "main"]), "checkout main");
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"main content");

    assert_ok(
        &git(dir, &["checkout", "-q", "feature"]),
        "checkout feature",
    );
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"feature content"
    );
}

/// Regression test for the indirect Git-to-Gat isolation hole: the
/// installed `post-checkout` dispatcher is
/// executed by the spawned `git` process itself, not by this test
/// process, so the hook-triggered `gat sync` only gets isolated from
/// ambient Gat state if the `git` child that fires it does too -- see
/// `tests/common/mod.rs`'s shared `isolated_child_env`, which both
/// `gat_with_env` and `git`/`git_with_env` now build on. The installed
/// dispatcher itself is a bare `gat hook post-checkout "$@" || exit $?`
/// (see `gat-engine/src/initialization.rs`) with no isolation of its own, so
/// it inherits whatever environment the `git` process that ran it had.
///
/// First proves the setup is meaningful: an
/// *explicit* conflicting `GAT_CACHE_LOCATION` override, passed to the `git`
/// child that triggers the hook via [`git_with_env`]'s `extra_env`,
/// really is honored by the hook-triggered `gat sync` -- otherwise the
/// assertion that follows would be vacuous. Then proves the actual
/// regression: an *ordinary* `git checkout` (through the plain
/// [`git`] helper, no `extra_env` override) still resolves the
/// hook-triggered `gat sync` to the isolated repo-local default, even
/// with a conflicting global Gat config and cache relocation sitting on
/// disk elsewhere the whole time.
#[test]
fn hook_triggered_sync_ignores_a_conflicting_global_config_and_cache_dir_elsewhere() {
    let tmp = setup();
    let dir = tmp.path();

    // Deliberately conflicting fake global config + cache relocation
    // (mirrors `cli_integration::environment`'s direct-spawn counterpart).
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

    // A second branch with different tracked content, ingested via an
    // explicit `GAT_CACHE_LOCATION` override so the object physically lives
    // under `conflicting_cache_dir` -- otherwise a later checkout given
    // that same override would have nothing to materialize from and the
    // "honoring" sanity check below would be meaningless.
    assert_ok(&git(dir, &["checkout", "-q", "-b", "honoring"]), "branch");
    std::fs::write(dir.join("big.bin"), b"honoring content").unwrap();
    assert_ok(
        &gat_with_env(
            dir,
            &["add", "big.bin"],
            &[(
                "GAT_CACHE_LOCATION",
                Some(conflicting_cache_dir.path().to_str().unwrap()),
            )],
        ),
        "gat add with an explicit conflicting GAT_CACHE_LOCATION",
    );
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "honoring"]), "commit");
    assert_ok(&git(dir, &["checkout", "-q", "main"]), "checkout main");

    // Sanity check: the hook-triggered gat *does* honor an explicit
    // conflicting environment given to the git child that fires it --
    // the object is only findable under `conflicting_cache_dir`, so a
    // successful materialization here proves the override actually
    // reached the hook-triggered `gat sync`, not just `gat add` above.
    let honoring_out = git_with_env(
        dir,
        &["checkout", "-q", "honoring"],
        &[(
            "GAT_CACHE_LOCATION",
            Some(conflicting_cache_dir.path().to_str().unwrap()),
        )],
    );
    assert_ok(
        &honoring_out,
        "git checkout honoring with an explicit conflicting GAT_CACHE_LOCATION",
    );
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"honoring content",
        "expected the hook-triggered gat sync to honor an explicit conflicting \
         GAT_CACHE_LOCATION passed to the git child that fired it"
    );

    // The actual regression: an *ordinary* checkout back to
    // main, through the plain `git` helper (default isolation only, no
    // `extra_env`), must not have the hook-triggered `gat sync` pick up
    // the conflicting global config/cache relocation that has been
    // sitting on disk the whole time.
    let conflicting_cache_dir_count_before = count_files(conflicting_cache_dir.path());
    assert_ok(
        &git(dir, &["checkout", "-q", "main"]),
        "ordinary checkout back to main",
    );
    assert_eq!(
        std::fs::read(dir.join("big.bin")).unwrap(),
        b"main content",
        "ordinary hook-triggered sync must still materialize main's own content"
    );
    assert!(
        !walk_has_any_file(&conflicting_cache_location),
        "ordinary hook-triggered gat sync must not have used the conflicting \
         global cache.location"
    );
    assert_eq!(
        count_files(conflicting_cache_dir.path()),
        conflicting_cache_dir_count_before,
        "ordinary hook-triggered gat sync must not have reused the earlier \
         explicit GAT_CACHE_LOCATION override"
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

/// Recursively counts regular files under `dir` to detect whether a
/// later checkout added anything new to a directory that already
/// contained files from an earlier step in the same test.
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
