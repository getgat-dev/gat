use gat_core::progress::NoopProgress;
use gat_core::selection::Selection;
use gat_engine::Repository;
use std::path::PathBuf;

#[test]
fn repaired_state_forces_validation_before_trust_state_can_overwrite_local_edits() {
    let fixture = test_support::TestRepo::gat_repo();
    let repo = Repository::at(fixture.path().to_path_buf());
    fixture.write("tracked.bin", b"original");
    test_support::add(&repo, &[PathBuf::from("tracked.bin")], &NoopProgress).unwrap();
    fixture.commit_all("add tracked.bin");
    test_support::sync(&repo, &NoopProgress).unwrap();

    let layout = gat_io::RepositoryLayout::at(fixture.path().to_path_buf());
    gat_io::state_reset_database_for_test(&layout).unwrap();
    assert!(matches!(
        repo.maintenance().repair_state().unwrap(),
        gat_engine::StateRepair::Rebuilt
    ));
    fixture.write("tracked.bin", b"locally edited");

    let outcome = gat_command::recover_incomplete(gat_command::sync(
        &repo,
        gat_command::SyncRequest {
            selection: Some(Selection::root()),
            force: false,
            dry_run: false,
            trust_state: true,
            fetch: false,
            repair: false,
            remote: None,
            rematerialize: false,
        },
        &NoopProgress,
    ))
    .unwrap();

    assert_eq!(outcome.outcome.conflicts, vec!["tracked.bin".to_string()]);
    assert_eq!(
        std::fs::read(fixture.path().join("tracked.bin")).unwrap(),
        b"locally edited"
    );
}
