//! Process-boundary behavior: exit codes, stdout/stderr stream
//! separation, running outside a Git repo, worktree `.git` files, hook-
//! mode quietness, Unicode/space paths, `gat diff`, and CLI help/exit
//! status basics -- covers behavior that can only be characterized at
//! the real process boundary.

use crate::common::{
    assert_failure_code, assert_ok, assert_stdout_contains, assert_success, commit_all, gat, git,
    init_repo, stderr, stdout,
};

/// `Repo::discover()` is called directly from `main.rs` (not through any
/// not-yet-migrated `commands::*` module), so this failure goes through
/// `RepositoryError`'s own `From<RepositoryError> for Failure` (Section
/// 9), not the generic compatibility bridge: it must produce a
/// concise, specific "not a...repository" diagnostic (with an actionable
/// hint), not the generic "Gat hit an unexpected internal error".
#[test]
fn running_outside_a_git_repo_fails_with_nonzero_exit_and_stderr_message() {
    let tmp = tempfile::tempdir().unwrap();
    let args = ["status"];
    let out = gat(tmp.path(), &args);
    assert_failure_code(tmp.path(), &args, &out, 1);
    let err = stderr(&out);
    assert!(!err.is_empty(), "expected an error message on stderr");
    assert!(
        err.to_lowercase().contains("repository"),
        "expected a specific not-a-repository diagnostic, got: {err}"
    );
    assert!(
        !err.contains("unexpected internal error"),
        "expected the typed RepositoryError diagnostic, not the generic internal fallback: {err}"
    );
    assert!(
        stdout(&out).is_empty(),
        "no stdout expected on this failure path"
    );
}

#[test]
fn worktree_style_git_file_is_recognized_as_a_repo_marker() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("big.bin"), b"hello world").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    commit_all(dir, "initial");

    // Create a real linked worktree, whose `.git` is a file pointing back
    // at the main repo's git dir -- this is the shape `Repo::discover`
    // must accept.
    let wt = tempfile::tempdir().unwrap();
    let wt_path = wt.path().join("wt");
    assert_ok(
        &git(
            dir,
            &[
                "worktree",
                "add",
                "-q",
                wt_path.to_str().unwrap(),
                "-b",
                "wt-branch",
            ],
        ),
        "git worktree add",
    );
    assert!(
        wt_path.join(".git").is_file(),
        "worktree .git should be a file"
    );

    let out = gat(&wt_path, &["status"]);
    assert_success(&wt_path, &["status"], &out);
}

#[test]
fn hook_mode_sync_produces_no_progress_noise_on_stderr() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("big.bin"), b"content").unwrap();
    assert_ok(&gat(dir, &["add", "big.bin"]), "gat add");
    commit_all(dir, "add big.bin");

    let out = gat(dir, &["hook", "post-checkout", "", "", "1"]);
    assert_success(dir, &["hook", "post-checkout", "", "", "1"], &out);
    // Hook mode must stay silent (no spinner/progress artifacts) even
    // though it still exits 0 and may print nothing at all.
    assert!(
        !stderr(&out).contains('\r'),
        "hook mode must not emit carriage-return progress redraws: {}",
        stderr(&out)
    );
}

#[test]
fn tracks_files_with_unicode_and_space_in_path() {
    let tmp = init_repo();
    let dir = tmp.path();
    let name = "dir with space/héllo wörld 名前.bin";
    std::fs::create_dir_all(dir.join("dir with space")).unwrap();
    std::fs::write(dir.join(name), b"unicode content").unwrap();

    assert_ok(&gat(dir, &["add", name]), "gat add unicode path");
    let lock = std::fs::read_to_string(dir.join("gat.lock")).unwrap();
    assert!(lock.contains("héllo wörld 名前.bin"), "lock file: {lock}");

    let out = gat(dir, &["status"]);
    assert_ok(&out, "gat status");
}

#[test]
fn diff_shows_a_semantic_summary_between_two_revisions() {
    let tmp = init_repo();
    let dir = tmp.path();

    std::fs::write(dir.join("a.bin"), b"v1").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add a.bin");
    commit_all(dir, "v1");
    assert_ok(&git(dir, &["tag", "v1"]), "git tag v1");

    std::fs::write(dir.join("a.bin"), b"v2-longer-payload").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add a.bin v2");
    std::fs::write(dir.join("b.bin"), b"brand new").unwrap();
    assert_ok(&gat(dir, &["add", "b.bin"]), "gat add b.bin");
    commit_all(dir, "v2");
    assert_ok(&git(dir, &["tag", "v2"]), "git tag v2");

    let out = gat(dir, &["diff", "v1", "v2"]);
    assert_success(dir, &["diff", "v1", "v2"], &out);
    assert_stdout_contains(dir, &["diff", "v1", "v2"], &out, "a.bin");
    assert_stdout_contains(dir, &["diff", "v1", "v2"], &out, "b.bin");
}

#[test]
fn diff_with_no_revisions_reports_no_changes_when_head_matches_the_working_tree() {
    let tmp = init_repo();
    let dir = tmp.path();
    std::fs::write(dir.join("a.bin"), b"payload").unwrap();
    assert_ok(&gat(dir, &["add", "a.bin"]), "gat add");
    commit_all(dir, "add a.bin");

    let out = gat(dir, &["diff"]);
    assert_success(dir, &["diff"], &out);
    assert!(
        stdout(&out).to_lowercase().contains("no changes"),
        "expected no changes: {}",
        stdout(&out)
    );
}

#[test]
fn help_exits_zero_and_lists_top_level_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let args = ["--help"];
    let out = gat(tmp.path(), &args);
    assert_success(tmp.path(), &args, &out);
    let text = stdout(&out);
    for cmd in [
        "init", "add", "rm", "status", "diff", "push", "fetch", "sync", "remote",
    ] {
        assert!(text.contains(cmd), "--help output missing `{cmd}`: {text}");
    }
}

#[test]
fn unknown_subcommand_fails_with_usage_on_stderr() {
    let tmp = tempfile::tempdir().unwrap();
    let out = gat(tmp.path(), &["this-is-not-a-command"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).to_lowercase().contains("unrecognized")
            || stderr(&out).to_lowercase().contains("error")
    );
}

/// `gat add` with no `PATHS` is a CLI syntax error: Clap
/// itself rejects it -- via `required = true` on `Command::Add.paths` --
/// before `app::run`/`Failure` ever runs, so it exits `2` (Clap's usage
/// exit code, distinct from every application-level `Failure`'s `1`) and
/// prints Clap's own "Usage:" block, not the `✗ error:`
/// `output::error::render` block.
#[test]
fn add_with_no_paths_is_rejected_by_clap_before_the_app_runs() {
    let tmp = init_repo();
    let args = ["add"];
    let out = gat(tmp.path(), &args);
    assert_failure_code(tmp.path(), &args, &out, 2);
    let err = stderr(&out);
    assert!(err.contains("Usage:"), "expected Clap usage text: {err}");
    assert!(
        !err.contains('✗'),
        "expected no app-level diagnostic rendering: {err}"
    );
}

/// Same as above for `gat rm`.
#[test]
fn rm_with_no_paths_is_rejected_by_clap_before_the_app_runs() {
    let tmp = init_repo();
    let args = ["rm"];
    let out = gat(tmp.path(), &args);
    assert_failure_code(tmp.path(), &args, &out, 2);
    let err = stderr(&out);
    assert!(err.contains("Usage:"), "expected Clap usage text: {err}");
    assert!(
        !err.contains('✗'),
        "expected no app-level diagnostic rendering: {err}"
    );
}

#[test]
fn version_flag_reports_a_version_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let args = ["--version"];
    let out = gat(tmp.path(), &args);
    assert_success(tmp.path(), &args, &out);
    assert_stdout_contains(tmp.path(), &args, &out, "gat");
}
