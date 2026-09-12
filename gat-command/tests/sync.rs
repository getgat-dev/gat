use gat_command::{HookRequest, SyncRequest};
use gat_core::progress::NoopProgress;
use gat_core::selection::Selection;
use gat_engine::Repository;

fn repository() -> (tempfile::TempDir, Repository) {
    let tmp = test_support_git::empty_git_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    (tmp, repo)
}

#[test]
fn dry_run_stays_desired_free_and_does_not_create_state() {
    let (tmp, repo) = repository();
    let outcome = gat_command::sync(
        &repo,
        SyncRequest {
            selection: Some(Selection::root()),
            force: false,
            dry_run: true,
            trust_state: false,
            fetch: false,
            repair: false,
            remote: None,
            rematerialize: false,
        },
        &NoopProgress,
    )
    .unwrap();

    assert!(outcome.outcome.dry_run);
    assert!(!tmp.path().join(".gat/state/state.sqlite3").exists());
}

#[test]
fn hook_reconciliation_is_clean_for_an_empty_repository() {
    let (_tmp, repo) = repository();
    let outcome = gat_command::hook(&repo, HookRequest, &NoopProgress).unwrap();

    assert!(outcome.is_clean());
    assert_eq!(outcome.fetched, 0);
}
