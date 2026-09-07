mod common;

use common::{
    RecordingProgress, add_paths, commit_all, git, remove_paths, repository, set_lock_shard_levels,
};
use gat_command::{DiffError, DiffOutcome, DiffRequest, DiffTarget, diff};
use gat_core::git::GitRevisionSpec;
use gat_core::path_scope::normalize_path_scope;
use gat_core::progress::{NoopProgress, ProgressOperation, ProgressReporter};
use gat_core::selection::Selection;
use gat_engine::{Repository, RowChange};
use std::path::{Path, PathBuf};

fn scoped_selection(path: &Path) -> Selection {
    Selection::from_scope_patterns(normalize_path_scope(path).unwrap(), Vec::new(), Vec::new())
}

fn revision(value: &str) -> GitRevisionSpec {
    GitRevisionSpec::from_string(value.to_string())
}

fn run_diff(
    repo: &Repository,
    from: GitRevisionSpec,
    to: DiffTarget,
    selection: Selection,
    progress: &dyn ProgressReporter,
) -> Result<DiffOutcome, DiffError> {
    diff(
        repo,
        DiffRequest {
            from,
            to,
            selection: Some(selection),
        },
        progress,
    )
}

fn working_diff(
    repo: &Repository,
    from: &str,
    selection: Selection,
) -> Result<DiffOutcome, DiffError> {
    run_diff(
        repo,
        revision(from),
        DiffTarget::WorkingTree,
        selection,
        &NoopProgress,
    )
}

#[derive(Debug, PartialEq, Eq)]
enum RowStatus {
    Added,
    Deleted,
    Modified,
}

fn row_status(change: &RowChange) -> RowStatus {
    match change {
        RowChange::Added { .. } => RowStatus::Added,
        RowChange::Removed => RowStatus::Deleted,
        RowChange::Modified { .. } => RowStatus::Modified,
        RowChange::Unchanged { .. } => unreachable!(),
    }
}

#[test]
fn diff_reports_an_unresolvable_revision_as_a_revision_error() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"payload").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "add a.bin");

    let error = working_diff(&repo, "this-revision-does-not-exist", Selection::root()).unwrap_err();
    assert!(matches!(
        error,
        DiffError::Compare(source)
            if matches!(
                source.kind(),
                gat_engine::CompareErrorKind::Revision(revision)
                    if revision.as_str() == "this-revision-does-not-exist"
            )
    ));
}

#[cfg(unix)]
#[test]
fn diff_against_the_working_tree_reports_a_symlinked_gat_lock_as_a_local_state_error() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"payload").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "add a.bin");

    let target = tempfile::tempdir().unwrap();
    std::fs::write(target.path().join("gat.lock"), "").unwrap();
    std::fs::remove_file(temp.path().join("gat.lock")).unwrap();
    std::os::unix::fs::symlink(target.path().join("gat.lock"), temp.path().join("gat.lock"))
        .unwrap();

    let error = working_diff(&repo, "HEAD", Selection::root()).unwrap_err();
    assert!(matches!(
        error,
        DiffError::Compare(source)
            if matches!(source.kind(), gat_engine::CompareErrorKind::Lock(_))
    ));
}

#[test]
fn revision_to_working_tree_diff_opens_exactly_one_loading_state_task() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"before").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "add a.bin");
    std::fs::write(temp.path().join("a.bin"), b"after-longer").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    let progress = RecordingProgress::new();

    run_diff(
        &repo,
        revision("HEAD"),
        DiffTarget::WorkingTree,
        Selection::root(),
        &progress,
    )
    .unwrap();

    let task = progress.only(ProgressOperation::LoadingState);
    assert!(task.finished);
    assert_eq!(progress.max_active_tasks(), 1);
}

#[test]
fn diff_with_no_args_compares_head_against_the_working_tree() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"before").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "add a.bin");
    std::fs::write(temp.path().join("a.bin"), b"after-longer").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    std::fs::write(temp.path().join("b.bin"), b"new").unwrap();
    add_paths(&repo, &[PathBuf::from("b.bin")]);

    let outcome = working_diff(&repo, "HEAD", Selection::root()).unwrap();
    let DiffOutcome::Changes {
        from,
        to,
        rows,
        changes,
        ..
    } = outcome
    else {
        panic!("expected changes");
    };
    assert_eq!(from.as_str(), "HEAD");
    assert_eq!(to, DiffTarget::WorkingTree);
    assert_eq!(changes, 2);
    let mut paths = rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>();
    paths.sort_unstable();
    assert_eq!(paths, vec!["a.bin", "b.bin"]);
}

#[cfg(feature = "test-support")]
#[test]
fn diff_against_working_tree_never_hex_encodes_the_current_oid() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"before").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "add a.bin");
    std::fs::write(temp.path().join("a.bin"), b"after-longer").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    std::fs::write(temp.path().join("b.bin"), b"new").unwrap();
    add_paths(&repo, &[PathBuf::from("b.bin")]);

    gat_core::oid::test_support::take_to_hex_calls();
    let outcome = working_diff(&repo, "HEAD", Selection::root()).unwrap();
    let to_hex_calls = gat_core::oid::test_support::take_to_hex_calls();

    assert!(matches!(outcome, DiffOutcome::Changes { .. }));
    assert_eq!(
        to_hex_calls, 0,
        "gat diff never displays an oid, so it must not hex-encode one"
    );
}

#[test]
fn diff_reports_no_changes_when_revision_matches_the_working_tree() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"payload").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "add a.bin");

    let outcome = working_diff(&repo, "HEAD", Selection::root()).unwrap();
    assert_eq!(
        outcome,
        DiffOutcome::NoChanges {
            scope: gat_command::SelectionScope::Unrestricted,
            from: revision("HEAD"),
            to: DiffTarget::WorkingTree,
        }
    );
}

#[test]
fn diff_compares_two_explicit_revisions() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"v1").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "v1");
    git(temp.path(), &["tag", "v1"]);
    std::fs::write(temp.path().join("a.bin"), b"v2-longer").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "v2");
    git(temp.path(), &["tag", "v2"]);

    let outcome = run_diff(
        &repo,
        revision("v1"),
        DiffTarget::Revision(revision("v2")),
        Selection::root(),
        &NoopProgress,
    )
    .unwrap();
    let DiffOutcome::Changes { from, to, rows, .. } = outcome else {
        panic!("expected changes");
    };
    assert_eq!(from.as_str(), "v1");
    assert_eq!(to, DiffTarget::Revision(revision("v2")));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path.as_str(), "a.bin");
    assert!(matches!(rows[0].change, RowChange::Modified { .. }));
}

#[test]
fn diff_with_one_revision_compares_it_against_the_working_tree() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"v1").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "v1");
    git(temp.path(), &["tag", "v1"]);
    std::fs::write(temp.path().join("a.bin"), b"v2-longer").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);

    let outcome = working_diff(&repo, "v1", Selection::root()).unwrap();
    let DiffOutcome::Changes { from, to, rows, .. } = outcome else {
        panic!("expected changes");
    };
    assert_eq!(from.as_str(), "v1");
    assert_eq!(to, DiffTarget::WorkingTree);
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0].change, RowChange::Modified { .. }));
}

#[test]
fn diff_restricts_output_to_the_given_path() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"a").unwrap();
    std::fs::create_dir_all(temp.path().join("dir")).unwrap();
    std::fs::write(temp.path().join("dir/b.bin"), b"b").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin"), PathBuf::from("dir")]);
    commit_all(temp.path(), "add both");
    std::fs::write(temp.path().join("a.bin"), b"a-changed").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    std::fs::write(temp.path().join("dir/b.bin"), b"b-changed").unwrap();
    add_paths(&repo, &[PathBuf::from("dir")]);

    let outcome = working_diff(&repo, "HEAD", scoped_selection(Path::new("a.bin"))).unwrap();
    let DiffOutcome::Changes { rows, changes, .. } = outcome else {
        panic!("expected changes");
    };
    assert_eq!(changes, 1);
    assert_eq!(rows[0].path.as_str(), "a.bin");
}

#[test]
fn diff_with_nonexistent_path_scope_reports_no_changes() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"a").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "add a.bin");

    let outcome = working_diff(&repo, "HEAD", scoped_selection(Path::new("missing"))).unwrap();
    assert_eq!(
        outcome,
        DiffOutcome::NoChanges {
            scope: gat_command::SelectionScope::Explicit,
            from: revision("HEAD"),
            to: DiffTarget::WorkingTree,
        }
    );
}

#[test]
fn diff_compares_across_a_flat_to_sharded_reshape() {
    let (temp, repo) = repository();
    for name in ["a.bin", "b.bin", "c.bin"] {
        std::fs::write(temp.path().join(name), format!("payload-{name}")).unwrap();
    }
    add_paths(
        &repo,
        &[
            PathBuf::from("a.bin"),
            PathBuf::from("b.bin"),
            PathBuf::from("c.bin"),
        ],
    );
    assert!(temp.path().join("gat.lock").is_file());
    commit_all(temp.path(), "flat lock");
    git(temp.path(), &["tag", "flat"]);

    set_lock_shard_levels(&repo, 2);
    std::fs::write(temp.path().join("a.bin"), b"payload-a-changed").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    assert!(temp.path().join("gat.lock").is_dir());
    remove_paths(&repo, &[PathBuf::from("c.bin")]);

    let outcome = working_diff(&repo, "flat", Selection::root()).unwrap();
    let DiffOutcome::Changes { rows, changes, .. } = outcome else {
        panic!("expected changes");
    };
    assert_eq!(changes, 2);
    assert_eq!(
        rows.iter()
            .map(|row| (row.path.as_str(), row_status(&row.change)))
            .collect::<Vec<_>>(),
        vec![
            ("a.bin", RowStatus::Modified),
            ("c.bin", RowStatus::Deleted)
        ]
    );

    let outcome = working_diff(&repo, "flat", scoped_selection(Path::new("a.bin"))).unwrap();
    let DiffOutcome::Changes { rows, changes, .. } = outcome else {
        panic!("expected changes");
    };
    assert_eq!(changes, 1);
    assert_eq!(rows[0].path.as_str(), "a.bin");
}

#[test]
fn diff_compares_two_revisions_with_different_lock_shapes() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"v1").unwrap();
    std::fs::write(temp.path().join("keep.bin"), b"keep").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin"), PathBuf::from("keep.bin")]);
    commit_all(temp.path(), "flat");
    git(temp.path(), &["tag", "flat"]);

    set_lock_shard_levels(&repo, 1);
    std::fs::write(temp.path().join("a.bin"), b"v2-longer").unwrap();
    std::fs::write(temp.path().join("new.bin"), b"new").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin"), PathBuf::from("new.bin")]);
    assert!(temp.path().join("gat.lock").is_dir());
    commit_all(temp.path(), "sharded");
    git(temp.path(), &["tag", "sharded"]);

    let outcome = run_diff(
        &repo,
        revision("flat"),
        DiffTarget::Revision(revision("sharded")),
        Selection::root(),
        &NoopProgress,
    )
    .unwrap();
    let DiffOutcome::Changes { rows, changes, .. } = outcome else {
        panic!("expected changes");
    };
    assert_eq!(changes, 2);
    assert_eq!(
        rows.iter()
            .map(|row| (row.path.as_str(), row_status(&row.change)))
            .collect::<Vec<_>>(),
        vec![
            ("a.bin", RowStatus::Modified),
            ("new.bin", RowStatus::Added)
        ]
    );
}

#[test]
fn diff_reads_a_sharded_gat_lock_at_a_revision() {
    let (temp, repo) = repository();
    set_lock_shard_levels(&repo, 2);
    std::fs::write(temp.path().join("a.bin"), b"payload-a").unwrap();
    std::fs::write(temp.path().join("b.bin"), b"payload-b").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin"), PathBuf::from("b.bin")]);
    assert!(temp.path().join("gat.lock").is_dir());
    commit_all(temp.path(), "sharded lock");
    std::fs::write(temp.path().join("a.bin"), b"payload-a-changed").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);

    let outcome = working_diff(&repo, "HEAD", Selection::root()).unwrap();
    let DiffOutcome::Changes { rows, changes, .. } = outcome else {
        panic!("expected changes");
    };
    assert_eq!(changes, 1);
    assert_eq!(rows[0].path.as_str(), "a.bin");
}
