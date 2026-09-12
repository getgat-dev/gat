use std::path::Path;
use std::sync::Mutex;

use gat_command::{GcError, GcRequest, gc, gc_with_lifecycle_observer};
use gat_core::history::{HistoryRequest, HistorySelection};
use gat_core::lifecycle::Surface;
use gat_core::name::RemoteName;
use gat_core::progress::NoopProgress;
use gat_engine::Repository;

fn git(dir: &Path, args: &[&str]) {
    test_support_git::run_git(dir, args);
}

fn repository() -> (tempfile::TempDir, Repository) {
    let temp = test_support_git::empty_git_repo();
    git(
        temp.path(),
        &["commit", "-q", "--allow-empty", "-m", "initial"],
    );
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(temp.path().to_path_buf());
    (temp, repo)
}

#[test]
fn gc_maps_engine_report_into_command_outcome() {
    let (_temp, repo) = repository();
    let outcome = gc(
        &repo,
        GcRequest {
            repositories: Vec::new(),
            dry_run: true,
            unsafe_override: false,
            remote: None,
            history: HistoryRequest::CommandDefault,
        },
        &NoopProgress,
    )
    .unwrap();

    assert!(outcome.dry_run);
    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.uncertain, 0);
    assert_eq!(outcome.incomplete_repositories, 0);
    assert!(!outcome.forced_incomplete_keep_set);
}

#[test]
fn gc_observes_its_lifecycle_surface_once() {
    let (_temp, repo) = repository();
    let observed = Mutex::new(Vec::new());

    gc_with_lifecycle_observer(
        &repo,
        GcRequest {
            repositories: Vec::new(),
            dry_run: true,
            unsafe_override: true,
            remote: None,
            history: HistoryRequest::Selected(HistorySelection::conservative_default()),
        },
        &NoopProgress,
        &|surface| {
            let Surface::Command(command) = surface else {
                panic!("expected command lifecycle surface");
            };
            observed.lock().unwrap().push(command.to_string());
        },
    )
    .unwrap();

    assert_eq!(*observed.lock().unwrap(), vec!["gc"]);
}

#[test]
fn gc_preserves_typed_remote_errors() {
    let (_temp, repo) = repository();
    let error = gc(
        &repo,
        GcRequest {
            repositories: Vec::new(),
            dry_run: true,
            unsafe_override: false,
            remote: Some(RemoteName::from_string("missing".to_string())),
            history: HistoryRequest::CommandDefault,
        },
        &NoopProgress,
    )
    .unwrap_err();

    assert!(matches!(error, GcError::Engine(_)));
}
