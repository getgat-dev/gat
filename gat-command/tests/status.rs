mod common;

use common::{RecordingProgress, add_paths, commit_all, repository};
use gat_command::{
    LsFilesError, LsFilesOutcome, LsFilesRequest, StatusError, StatusOutcome, StatusRequest,
    ls_files, status,
};
use gat_core::path_scope::normalize_path_scope;
use gat_core::progress::{NoopProgress, ProgressOperation, ProgressUnit};
use gat_core::selection::Selection;
use gat_engine::Repository;
use std::path::{Path, PathBuf};

fn scoped_selection(path: &Path) -> Selection {
    Selection::from_scope_patterns(normalize_path_scope(path).unwrap(), Vec::new(), Vec::new())
}

fn run_status(
    repo: &Repository,
    selection: Selection,
    progress: &dyn gat_core::progress::ProgressReporter,
) -> Result<StatusOutcome, StatusError> {
    status(
        repo,
        StatusRequest {
            selection: Some(selection),
        },
        progress,
    )
}

fn run_ls_files(
    repo: &Repository,
    selection: Selection,
    progress: &dyn gat_core::progress::ProgressReporter,
) -> Result<LsFilesOutcome, LsFilesError> {
    ls_files(
        repo,
        LsFilesRequest {
            selection: Some(selection),
        },
        progress,
    )
}

#[cfg(unix)]
#[test]
fn status_reports_a_symlinked_gat_lock_as_a_local_state_error() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"payload").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    commit_all(temp.path(), "add a.bin");

    let target = tempfile::tempdir().unwrap();
    std::fs::write(target.path().join("gat.lock"), "").unwrap();
    std::fs::remove_file(temp.path().join("gat.lock")).unwrap();
    std::os::unix::fs::symlink(target.path().join("gat.lock"), temp.path().join("gat.lock"))
        .unwrap();

    let error = run_status(&repo, Selection::root(), &NoopProgress).unwrap_err();
    assert!(matches!(
        error,
        StatusError::Compare(source)
            if matches!(source.kind(), gat_engine::CompareErrorKind::Lock(_))
    ));
}

#[test]
fn status_with_path_filter_restricts_output() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"a").unwrap();
    std::fs::write(temp.path().join("b.bin"), b"b").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin"), PathBuf::from("b.bin")]);

    let outcome = run_status(&repo, scoped_selection(Path::new("a.bin")), &NoopProgress).unwrap();
    let StatusOutcome::WorkingTree { rows, .. } = outcome else {
        panic!("expected working tree status");
    };
    assert_eq!(
        rows.into_iter()
            .map(|row| row.path.to_string())
            .collect::<Vec<_>>(),
        vec!["a.bin"]
    );
}

#[test]
fn ls_files_lists_uncommitted_tracked_files() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("big.bin"), b"payload").unwrap();
    add_paths(&repo, &[PathBuf::from("big.bin")]);

    let outcome = run_ls_files(&repo, Selection::root(), &NoopProgress).unwrap();
    assert_eq!(outcome.paths, vec!["big.bin"]);
}

#[test]
fn ls_files_path_filter_matches_a_tracked_file_missing_on_disk() {
    let (temp, repo) = repository();
    std::fs::create_dir_all(temp.path().join("data")).unwrap();
    std::fs::write(temp.path().join("data/nested.bin"), b"payload").unwrap();
    add_paths(&repo, &[PathBuf::from("data/nested.bin")]);
    std::fs::remove_file(temp.path().join("data/nested.bin")).unwrap();

    let outcome = run_ls_files(
        &repo,
        scoped_selection(Path::new("data\\nested.bin")),
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(outcome.paths, vec!["data/nested.bin"]);
}

#[test]
fn status_preserves_lexical_order_after_parallel_cache_annotation() {
    let (temp, repo) = repository();
    let mut names = (0..64)
        .map(|index| format!("file{index:02}.bin"))
        .collect::<Vec<_>>();
    for name in &names {
        std::fs::write(temp.path().join(name), name.as_bytes()).unwrap();
    }
    let paths = names.iter().map(PathBuf::from).collect::<Vec<_>>();
    add_paths(&repo, &paths);

    let outcome = run_status(&repo, Selection::root(), &NoopProgress).unwrap();
    let StatusOutcome::WorkingTree { rows, .. } = outcome else {
        panic!("expected working tree status");
    };
    let observed = rows
        .into_iter()
        .map(|row| row.path.to_string())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(observed, names);
}

#[test]
fn status_reports_sequential_loading_and_cache_progress() {
    let (temp, repo) = repository();
    std::fs::write(temp.path().join("a.bin"), b"a").unwrap();
    add_paths(&repo, &[PathBuf::from("a.bin")]);
    let progress = RecordingProgress::new();

    run_status(&repo, Selection::root(), &progress).unwrap();

    assert!(
        progress
            .operations()
            .contains(&ProgressOperation::LoadingState)
    );
    assert!(
        progress
            .operations()
            .contains(&ProgressOperation::InspectingCache)
    );
    assert_eq!(progress.max_active_tasks(), 1);
    let task = progress.only(ProgressOperation::InspectingCache);
    assert_eq!(task.unit, Some(ProgressUnit::Entries));
    assert_eq!(task.total, Some(1));
    assert_eq!(task.position, 1);
    assert!(task.finished);
}

#[test]
fn ls_files_advances_one_open_ended_progress_item_per_path() {
    let (temp, repo) = repository();
    for name in ["a.bin", "b.bin", "c.bin"] {
        std::fs::write(temp.path().join(name), name.as_bytes()).unwrap();
    }
    add_paths(
        &repo,
        &[
            PathBuf::from("a.bin"),
            PathBuf::from("b.bin"),
            PathBuf::from("c.bin"),
        ],
    );
    let progress = RecordingProgress::new();

    let outcome = run_ls_files(&repo, Selection::root(), &progress).unwrap();

    assert_eq!(outcome.paths.len(), 3);
    let task = progress.only(ProgressOperation::LoadingState);
    assert_eq!(task.unit, Some(ProgressUnit::Entries));
    assert_eq!(task.total, None);
    assert_eq!(task.position, 3);
    assert!(task.finished);
}
