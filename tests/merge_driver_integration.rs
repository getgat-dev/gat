//! End-to-end integration tests driving the actual `gat` binary and a
//! real `git` binary together for the `gat.lock` semantic merge driver
//! -- the pure three-way merge rule itself is exhaustively
//! unit-tested in `gat-core/src/lock/merge.rs`, and the merge-driver
//! protocol service in `gat-engine/src/merge_driver.rs` -- what these
//! tests add is proof that real `git merge` actually invokes the
//! installed driver and gets the expected semantic result (or a real
//! conflict) rather than Git's own line-oriented text merge.

#[path = "common/mod.rs"]
mod common;
use common::{assert_ok, gat, gat_with_env, git, git_with_env, stdout};

fn read(dir: &std::path::Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap()
}

/// `gat init` on `main`, with an initial commit tracking `a.bin` and
/// `d.bin` via gat -- the shared ancestor every scenario branches from.
fn setup() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_ok(&git(dir, &["init", "-q", "-b", "main"]), "git init");
    assert_ok(&gat(dir, &["init"]), "gat init");
    std::fs::write(dir.join("a.bin"), b"a").unwrap();
    std::fs::write(dir.join("d.bin"), b"d").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin", "d.bin"]), "gat add");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "initial"]), "commit");
    tmp
}

fn checkout(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut full = vec!["checkout"];
    full.extend_from_slice(args);
    git(dir, &full)
}

fn branch(dir: &std::path::Path, name: &str) {
    assert_ok(&checkout(dir, &["-q", "-b", name]), "branch");
}

/// Scenario 1: two branches insert independent rows into the same
/// sorted textual gap of a flat `gat.lock` (both `b.bin`/`c.bin` sort
/// between `a.bin` and `d.bin`). Git's default line-oriented merge would
/// report a false conflict here; the installed `gat-lock` driver must
/// merge them cleanly into the union of all four paths.
#[test]
fn independent_insertions_into_the_same_sorted_gap_merge_cleanly() {
    let tmp = setup();
    let dir = tmp.path();

    branch(dir, "feature-b");
    std::fs::write(dir.join("b.bin"), b"b").unwrap();
    assert_ok(&gat(dir, &["add", "b.bin"]), "gat add b.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add b"]), "commit b");

    assert_ok(&checkout(dir, &["-q", "main"]), "back to main");
    branch(dir, "feature-c");
    std::fs::write(dir.join("c.bin"), b"c").unwrap();
    assert_ok(&gat(dir, &["add", "c.bin"]), "gat add c.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add c"]), "commit c");

    assert_ok(&checkout(dir, &["-q", "feature-b"]), "checkout b");
    let merge = git(dir, &["merge", "-q", "--no-ff", "feature-c"]);
    assert_ok(&merge, "merge feature-c into feature-b");

    let lock = read(dir, "gat.lock");
    for path in ["a.bin", "b.bin", "c.bin", "d.bin"] {
        assert!(
            lock.contains(path),
            "expected {path} in merged lock:\n{lock}"
        );
    }
    assert!(
        !lock.contains("<<<<<<<"),
        "no conflict markers expected:\n{lock}"
    );
}

/// Scenario 2: two branches modify two different existing tracked
/// paths -- an ordinary case that a plain text merge would already
/// handle, but must keep working through the semantic driver too.
#[test]
fn independent_modifications_to_distinct_existing_paths_merge_cleanly() {
    let tmp = setup();
    let dir = tmp.path();

    branch(dir, "feature-a");
    std::fs::write(dir.join("a.bin"), b"a-modified").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add a.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "modify a"]), "commit a");

    assert_ok(&checkout(dir, &["-q", "main"]), "back to main");
    branch(dir, "feature-d");
    std::fs::write(dir.join("d.bin"), b"d-modified").unwrap();
    assert_ok(&gat(dir, &["add", "d.bin"]), "gat add d.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "modify d"]), "commit d");

    assert_ok(&checkout(dir, &["-q", "feature-a"]), "checkout a");
    let merge = git(dir, &["merge", "-q", "--no-ff", "feature-d"]);
    assert_ok(&merge, "merge feature-d into feature-a");

    let lock = read(dir, "gat.lock");
    assert!(
        !lock.contains("<<<<<<<"),
        "no conflict markers expected:\n{lock}"
    );
}

/// Scenario 3: both branches change the *same* tracked path to
/// different content/OIDs -- a genuine conflict that must remain
/// unresolved, never silently picking one side.
#[test]
fn same_path_changed_differently_on_both_sides_stays_conflicted() {
    let tmp = setup();
    let dir = tmp.path();

    branch(dir, "feature-x");
    std::fs::write(dir.join("a.bin"), b"from-x").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add a.bin (x)");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(
        &git(dir, &["commit", "-q", "-m", "x changes a"]),
        "commit x",
    );

    assert_ok(&checkout(dir, &["-q", "main"]), "back to main");
    branch(dir, "feature-y");
    std::fs::write(dir.join("a.bin"), b"from-y").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add a.bin (y)");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(
        &git(dir, &["commit", "-q", "-m", "y changes a"]),
        "commit y",
    );

    assert_ok(&checkout(dir, &["-q", "feature-x"]), "checkout x");
    let merge = git(dir, &["merge", "--no-ff", "feature-y"]);
    assert!(
        !merge.status.success(),
        "merge of genuinely conflicting gat.lock changes must fail"
    );

    let status = git(dir, &["status", "--porcelain"]);
    assert!(
        stdout(&status).contains("gat.lock"),
        "gat.lock must be left as an unresolved conflict: {}",
        stdout(&status)
    );

    assert_ok(&git(dir, &["merge", "--abort"]), "merge --abort");
}

/// `gat init` installs a local `merge.gat-lock.*` Git config and
/// registers it via `info/attributes` -- both required for real `git
/// merge` to invoke the driver at all, and both checked directly here
/// rather than only inferred from a successful merge above.
#[test]
fn init_installs_local_merge_config_and_attributes() {
    let tmp = setup();
    let dir = tmp.path();

    let config = read(dir, ".git/config");
    assert!(config.contains("[merge \"gat-lock\"]"));
    assert!(config.contains("driver = gat merge-driver %O %A %B"));

    let attrs = read(dir, ".git/info/attributes");
    assert!(attrs.contains("/gat.lock merge=gat-lock"));
    assert!(attrs.contains("/gat.lock/** merge=gat-lock"));
}

/// `gat init --no-hooks` must still install the merge driver and
/// attributes -- only the actual Git hooks are skipped.
#[test]
fn init_no_hooks_still_installs_merge_driver_and_attributes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_ok(&git(dir, &["init", "-q", "-b", "main"]), "git init");
    assert_ok(&gat(dir, &["init", "--no-hooks"]), "gat init --no-hooks");

    assert!(!dir.join(".git/hooks/post-checkout").exists());
    let config = read(dir, ".git/config");
    assert!(config.contains("[merge \"gat-lock\"]"));
    let attrs = read(dir, ".git/info/attributes");
    assert!(attrs.contains("merge=gat-lock"));
}

/// `gat init --no-merge-driver` installs hooks but skips the merge
/// driver and attributes entirely.
#[test]
fn init_no_merge_driver_still_installs_hooks_but_skips_merge_integration() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_ok(&git(dir, &["init", "-q", "-b", "main"]), "git init");
    assert_ok(
        &gat(dir, &["init", "--no-merge-driver"]),
        "gat init --no-merge-driver",
    );

    assert!(dir.join(".git/hooks/post-checkout").exists());
    let config = read(dir, ".git/config");
    assert!(!config.contains("gat-lock"));
    assert!(!dir.join(".git/info/attributes").exists());
}

/// `gat hooks install` (removed) can install/repair the complete
/// integration (hooks, merge driver, attributes) on a repo that never
/// ran `gat init` at all -- now just plain `gat init`.
#[test]
fn init_installs_the_complete_integration_from_scratch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_ok(&git(dir, &["init", "-q", "-b", "main"]), "git init");

    assert_ok(&gat(dir, &["init"]), "gat init");

    assert!(dir.join(".git/hooks/post-checkout").exists());
    let config = read(dir, ".git/config");
    assert!(config.contains("[merge \"gat-lock\"]"));
    let attrs = read(dir, ".git/info/attributes");
    assert!(attrs.contains("merge=gat-lock"));
}

/// `gat init --no-merge-driver` installs only the hooks, leaving
/// the merge driver and attributes absent.
#[test]
fn init_no_merge_driver_only_installs_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_ok(&git(dir, &["init", "-q", "-b", "main"]), "git init");

    assert_ok(
        &gat(dir, &["init", "--no-merge-driver"]),
        "gat init --no-merge-driver",
    );

    assert!(dir.join(".git/hooks/post-checkout").exists());
    let config = read(dir, ".git/config");
    assert!(!config.contains("gat-lock"));
    assert!(!dir.join(".git/info/attributes").exists());
}

/// `gat init --no-hooks --no-merge-driver` on a repo where everything is
/// already installed removes only the hooks, leaving an
/// installed merge driver and attributes in place.
#[test]
fn init_no_hooks_leaves_merge_integration_installed() {
    let tmp = setup();
    let dir = tmp.path();

    assert_ok(&gat(dir, &["init", "--no-hooks"]), "gat init --no-hooks");

    assert!(!dir.join(".git/hooks/post-checkout").exists());
    let config = read(dir, ".git/config");
    assert!(config.contains("[merge \"gat-lock\"]"));
    let attrs = read(dir, ".git/info/attributes");
    assert!(attrs.contains("merge=gat-lock"));
}

/// `gat init --no-hooks --no-merge-driver` on a repo where everything is
/// already installed removes the complete integration it installed:
/// hooks, merge-driver config, and attributes, proving `gat init` truly
/// converges (removes, not merely skips) rather than just suppressing a
/// fresh install.
#[test]
fn init_no_hooks_no_merge_driver_removes_the_complete_integration() {
    let tmp = setup();
    let dir = tmp.path();

    assert_ok(
        &gat(dir, &["init", "--no-hooks", "--no-merge-driver"]),
        "gat init --no-hooks --no-merge-driver",
    );

    assert!(!dir.join(".git/hooks/post-checkout").exists());
    let config = read(dir, ".git/config");
    assert!(!config.contains("gat-lock"));
    // The whole file is removed since gat created it from scratch during
    // `setup`'s `gat init` and nothing else has touched it since.
    assert!(!dir.join(".git/info/attributes").exists());
}

/// Re-running `gat init` is idempotent: it doesn't duplicate the
/// `merge.gat-lock` config section or the `info/attributes` managed
/// block.
#[test]
fn reinstalling_does_not_duplicate_config_or_attributes() {
    let tmp = setup();
    let dir = tmp.path();

    assert_ok(&gat(dir, &["init"]), "gat init");

    let config = read(dir, ".git/config");
    assert_eq!(config.matches("[merge \"gat-lock\"]").count(), 1);
    let attrs = read(dir, ".git/info/attributes");
    assert_eq!(attrs.matches("merge=gat-lock").count(), 2);
}

/// Existing, unrelated Git config entries and `info/attributes` content
/// gat never owned survive both install and removal untouched.
#[test]
fn unrelated_config_and_attributes_content_is_preserved_across_install_and_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_ok(&git(dir, &["init", "-q", "-b", "main"]), "git init");
    assert_ok(
        &git(dir, &["config", "user.name", "Preexisting User"]),
        "seed unrelated config",
    );
    std::fs::create_dir_all(dir.join(".git/info")).unwrap();
    std::fs::write(dir.join(".git/info/attributes"), "*.psd -text\n").unwrap();

    assert_ok(&gat(dir, &["init"]), "gat init");
    let config = read(dir, ".git/config");
    assert!(config.contains("Preexisting User"));
    let attrs = read(dir, ".git/info/attributes");
    assert!(attrs.contains("*.psd -text"));

    assert_ok(
        &gat(dir, &["init", "--no-hooks", "--no-merge-driver"]),
        "gat init --no-hooks --no-merge-driver",
    );
    let config = read(dir, ".git/config");
    assert!(config.contains("Preexisting User"));
    assert!(!config.contains("gat-lock"));
    let attrs = read(dir, ".git/info/attributes");
    assert!(attrs.contains("*.psd -text"));
    assert!(!attrs.contains("gat-lock"));
}

/// `gat init --no-merge-driver` followed by a plain `gat init` must
/// actually reinstall a *working* semantic merge driver -- not merely
/// restore the config strings -- so a merge that only the `gat-lock`
/// driver can resolve cleanly (independent insertions into the same
/// sorted gap) still resolves cleanly after the disable/reenable cycle.
#[test]
fn reinstalling_after_no_merge_driver_restores_working_merge_semantics() {
    let tmp = setup();
    let dir = tmp.path();

    assert_ok(
        &gat(dir, &["init", "--no-merge-driver"]),
        "gat init --no-merge-driver",
    );
    assert!(!read(dir, ".git/config").contains("gat-lock"));
    assert_ok(&gat(dir, &["init"]), "gat init (reinstall)");

    branch(dir, "feature-b");
    std::fs::write(dir.join("b.bin"), b"b").unwrap();
    assert_ok(&gat(dir, &["add", "b.bin"]), "gat add b.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add b"]), "commit b");

    assert_ok(&checkout(dir, &["-q", "main"]), "back to main");
    branch(dir, "feature-c");
    std::fs::write(dir.join("c.bin"), b"c").unwrap();
    assert_ok(&gat(dir, &["add", "c.bin"]), "gat add c.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add c"]), "commit c");

    assert_ok(&checkout(dir, &["-q", "feature-b"]), "checkout b");
    let merge = git(dir, &["merge", "-q", "--no-ff", "feature-c"]);
    assert_ok(&merge, "merge feature-c into feature-b after reinstall");

    let lock = read(dir, "gat.lock");
    for path in ["a.bin", "b.bin", "c.bin", "d.bin"] {
        assert!(
            lock.contains(path),
            "expected {path} in merged lock after reinstall:\n{lock}"
        );
    }
    assert!(
        !lock.contains("<<<<<<<"),
        "no conflict markers expected after reinstall:\n{lock}"
    );
}

/// Disabled-state counterpart to the reinstall test above: with the
/// merge driver actually removed (`gat init --no-merge-driver`), the
/// exact same independent-insertions scenario that the driver resolves
/// cleanly must instead surface as a real Git conflict, proving the
/// driver's absence is genuine (not merely a config string gat happens
/// not to check) rather than inferred only from `.git/config`/
/// `info/attributes` content.
#[test]
fn no_merge_driver_disabled_state_leaves_a_real_conflict_for_git_to_resolve() {
    let tmp = setup();
    let dir = tmp.path();

    assert_ok(
        &gat(dir, &["init", "--no-merge-driver"]),
        "gat init --no-merge-driver",
    );

    branch(dir, "feature-b");
    std::fs::write(dir.join("b.bin"), b"b").unwrap();
    assert_ok(&gat(dir, &["add", "b.bin"]), "gat add b.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add b"]), "commit b");

    assert_ok(&checkout(dir, &["-q", "main"]), "back to main");
    branch(dir, "feature-c");
    std::fs::write(dir.join("c.bin"), b"c").unwrap();
    assert_ok(&gat(dir, &["add", "c.bin"]), "gat add c.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add c"]), "commit c");

    assert_ok(&checkout(dir, &["-q", "feature-b"]), "checkout b");
    let merge = git(dir, &["merge", "-q", "--no-ff", "feature-c"]);
    assert!(
        !merge.status.success(),
        "merge should conflict without the gat-lock driver installed: {}",
        stdout(&merge)
    );
    let lock = read(dir, "gat.lock");
    assert!(
        lock.contains("<<<<<<<"),
        "expected raw Git conflict markers with the driver absent:\n{lock}"
    );

    // Clean up the conflicted merge state so the process-wide repo isn't
    // left half-merged for any later assertion in this test.
    assert_ok(&git(dir, &["merge", "--abort"]), "abort conflicted merge");
}

/// Sharded `gat.lock/` shard files must use the same driver -- each
/// shard file is itself a self-contained lock document, so independent
/// insertions landing in the same shard must merge exactly like the flat
/// case above.
#[test]
fn sharded_lock_shards_merge_through_the_same_driver() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    assert_ok(&git(dir, &["init", "-q", "-b", "main"]), "git init");
    assert_ok(&gat(dir, &["init"]), "gat init");
    assert_ok(
        &gat(dir, &["config", "lock.shard_levels", "1"]),
        "gat config lock.shard_levels",
    );
    std::fs::write(dir.join("a.bin"), b"a").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add a.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(
        &git(dir, &["commit", "-q", "-m", "initial sharded"]),
        "commit",
    );
    assert!(dir.join("gat.lock").is_dir(), "gat.lock must be sharded");

    branch(dir, "feature-b");
    std::fs::write(dir.join("b.bin"), b"b").unwrap();
    assert_ok(&gat(dir, &["add", "b.bin"]), "gat add b.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add b"]), "commit b");

    assert_ok(&checkout(dir, &["-q", "main"]), "back to main");
    branch(dir, "feature-c");
    std::fs::write(dir.join("c.bin"), b"c").unwrap();
    assert_ok(&gat(dir, &["add", "c.bin"]), "gat add c.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add c"]), "commit c");

    assert_ok(&checkout(dir, &["-q", "feature-b"]), "checkout b");
    let merge = git(dir, &["merge", "-q", "--no-ff", "feature-c"]);
    assert_ok(&merge, "merge feature-c into feature-b");

    let status = gat(dir, &["ls-files"]);
    assert_ok(&status, "gat ls-files");
    let files = stdout(&status);
    for path in ["a.bin", "b.bin", "c.bin"] {
        assert!(files.contains(path), "expected {path} in {files}");
    }
}

/// Linked worktrees resolve the common Git dir (shared `config` and
/// `info/attributes`), not the per-worktree one -- installing from the
/// linked worktree must still land in the main repo's common dir.
#[test]
fn linked_worktrees_use_the_common_git_dir_for_merge_integration() {
    let tmp = setup();
    let dir = tmp.path();
    let worktree_parent = tempfile::tempdir().unwrap();
    let worktree_dir = worktree_parent.path().join("linked-worktree");

    assert_ok(
        &git(
            dir,
            &[
                "worktree",
                "add",
                "-q",
                worktree_dir.to_str().unwrap(),
                "-b",
                "wt-branch",
            ],
        ),
        "git worktree add",
    );

    assert_ok(&gat(&worktree_dir, &["init"]), "init from worktree");

    // The common `.git/config`/`.git/info/attributes` (in the main
    // worktree) picked up the integration -- not some per-worktree copy.
    let config = read(dir, ".git/config");
    assert!(config.contains("[merge \"gat-lock\"]"));
    let attrs = read(dir, ".git/info/attributes");
    assert!(attrs.contains("merge=gat-lock"));

    // The hook dispatcher (installed via the same `hooks::hooks_dir` ->
    // `gat_io::common_dir()` path) also lands in
    // the main worktree's `.git/hooks`, not the linked worktree's own
    // `.git` (which is just a file pointing back at the main repo, so a
    // per-worktree hooks dir doesn't even exist).
    let hook = read(dir, ".git/hooks/post-checkout");
    assert!(hook.contains("gat hook post-checkout"));
    assert!(
        !worktree_dir.join(".git").is_dir(),
        "a linked worktree's .git is a file, not a directory"
    );
}

/// The `gix::open::Options::isolated()` gix-open helper
/// (`gat_io::common_dir`) behind `hooks::hooks_dir` and
/// `git_integration::common_dir` disables `GIT_COMMON_DIR`/
/// `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` environment-variable
/// overrides, not just `HOME`/`USERPROFILE`: prove merge-driver,
/// attributes, and hook installation all still resolve the *real*
/// common Git dir -- from a linked worktree, the harder case -- even
/// when the child process's environment carries a conflicting
/// `GIT_COMMON_DIR`/`GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` that would
/// otherwise redirect a non-isolated `gix::open` (or a naive `git`
/// invocation) elsewhere.
#[test]
fn linked_worktree_integration_ignores_a_conflicting_ambient_git_common_dir_and_config() {
    let tmp = setup();
    let dir = tmp.path();
    let worktree_parent = tempfile::tempdir().unwrap();
    let worktree_dir = worktree_parent.path().join("linked-worktree");

    assert_ok(
        &git(
            dir,
            &[
                "worktree",
                "add",
                "-q",
                worktree_dir.to_str().unwrap(),
                "-b",
                "wt-branch-conflicting-env",
            ],
        ),
        "git worktree add",
    );

    // A conflicting `GIT_COMMON_DIR` (pointing nowhere real) and
    // conflicting `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` (pointing at an
    // otherwise-empty decoy config) that a non-isolated `gix::open` would
    // otherwise honor.
    let decoy_common_dir = worktree_parent.path().join("decoy-common-dir");
    std::fs::create_dir_all(&decoy_common_dir).unwrap();
    let decoy_config = worktree_parent.path().join("decoy-gitconfig");
    std::fs::write(&decoy_config, "[user]\nname = decoy\n").unwrap();
    let conflicting_env: &[(&str, Option<&str>)] = &[
        ("GIT_COMMON_DIR", Some(decoy_common_dir.to_str().unwrap())),
        ("GIT_CONFIG_GLOBAL", Some(decoy_config.to_str().unwrap())),
        ("GIT_CONFIG_SYSTEM", Some(decoy_config.to_str().unwrap())),
    ];

    assert_ok(
        &gat_with_env(&worktree_dir, &["init"], conflicting_env),
        "init from worktree with conflicting ambient env",
    );

    // Everything still landed in the main worktree's real common dir --
    // not the decoy `GIT_COMMON_DIR`, and unaffected by the decoy global
    // config.
    let config = read(dir, ".git/config");
    assert!(config.contains("[merge \"gat-lock\"]"));
    let attrs = read(dir, ".git/info/attributes");
    assert!(attrs.contains("merge=gat-lock"));
    let hook = read(dir, ".git/hooks/post-checkout");
    assert!(hook.contains("gat hook post-checkout"));
    assert!(
        !decoy_common_dir.join("hooks").exists(),
        "must not have written hooks into the decoy GIT_COMMON_DIR"
    );
}

/// Regression test for the indirect Git-to-Gat isolation hole, semantic
/// merge-driver counterpart of `sync_hooks_integration`'s
/// `hook_triggered_sync_ignores_a_conflicting_global_config_and_cache_dir_elsewhere`:
/// `git merge` invokes the
/// installed `gat-lock` merge driver (`gat merge-driver %O %A %B`) as its
/// own child process, inheriting whatever environment the `git merge`
/// process itself had -- so this proves the driver still runs correctly
/// when that `git` process is given the same default isolated child
/// environment [`common::git`] now applies (via `isolated_child_env`,
/// shared with [`common::gat_with_env`]), and remains correct even when
/// the `git` child is additionally handed an explicit, deliberately
/// conflicting `HOME`/`GAT_CACHE_LOCATION`/global-Gat-config environment via
/// [`git_with_env`]. Unlike the hook path, `gat merge-driver` itself never
/// touches ambient Gat/Git state at all (see `gat-engine/src/merge_driver.rs`
/// -- it only reads the three temp files Git hands it and writes the
/// merged result back) -- this test exists to prove the isolation
/// refactor didn't accidentally break the driver invocation itself, not
/// to prove the driver ignores ambient state (it never reads any).
#[test]
fn merge_driver_still_merges_cleanly_under_an_explicit_conflicting_child_environment() {
    let tmp = setup();
    let dir = tmp.path();

    branch(dir, "feature-b");
    std::fs::write(dir.join("b.bin"), b"b").unwrap();
    assert_ok(&gat(dir, &["add", "b.bin"]), "gat add b.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add b"]), "commit b");

    assert_ok(&checkout(dir, &["-q", "main"]), "back to main");
    branch(dir, "feature-c");
    std::fs::write(dir.join("c.bin"), b"c").unwrap();
    assert_ok(&gat(dir, &["add", "c.bin"]), "gat add c.bin");
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", "add c"]), "commit c");

    assert_ok(&checkout(dir, &["-q", "feature-b"]), "checkout b");

    // A deliberately conflicting home/global-config/cache-relocation
    // environment, handed to the `git merge` child that in turn invokes
    // `gat merge-driver` -- mirroring the conflicting setups used
    // elsewhere in this test suite (a fake home with its own `.gat`
    // directory, plus a redirected `GAT_CACHE_LOCATION`).
    let conflicting_home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(conflicting_home.path().join(".gat")).unwrap();
    let conflicting_cache_dir = tempfile::tempdir().unwrap();
    let conflicting_env: &[(&str, Option<&str>)] = &[
        (
            common::HOME_ENV_VAR,
            Some(conflicting_home.path().to_str().unwrap()),
        ),
        (
            "GAT_CACHE_LOCATION",
            Some(conflicting_cache_dir.path().to_str().unwrap()),
        ),
    ];

    let merge = git_with_env(
        dir,
        &["merge", "-q", "--no-ff", "feature-c"],
        conflicting_env,
    );
    assert_ok(
        &merge,
        "merge feature-c into feature-b under a conflicting child environment",
    );

    let lock = read(dir, "gat.lock");
    for path in ["a.bin", "b.bin", "c.bin", "d.bin"] {
        assert!(
            lock.contains(path),
            "expected {path} in merged lock:\n{lock}"
        );
    }
    assert!(
        !lock.contains("<<<<<<<"),
        "no conflict markers expected:\n{lock}"
    );
}
