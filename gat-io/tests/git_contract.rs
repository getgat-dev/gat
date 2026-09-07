use gat_core::git::{GitCommitId, GitRevisionSpec};
use gat_core::lexical_path::GatPath;
use gat_core::lock::{Entry, Lock, LockShardLevels};
use gat_core::oid::Oid;
use gat_io::{
    GatIgnore, GitDiscovery, GitIntegration, GitReader, InfoExcludeMutation, LockStore,
    RepositoryLayout,
};

fn git(root: &std::path::Path, args: &[&str]) {
    test_support_git::run_git(root, args);
}

fn repository() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    git(temp.path(), &["init", "-q"]);
    temp
}

fn layout(root: &std::path::Path) -> RepositoryLayout {
    RepositoryLayout::at(root.to_path_buf())
}

#[test]
fn layout_binds_reusable_reader_revision_and_snapshot_access() {
    let temp = repository();
    let layout = layout(temp.path());
    let lock = Lock {
        entries: vec![Entry {
            path: GatPath::parse_canonical("data.bin").unwrap(),
            oid: Oid::from_hex(&"a".repeat(64)).unwrap(),
        }],
    };
    LockStore::publish_repository(&layout, &lock, LockShardLevels::FLAT).unwrap();
    git(temp.path(), &["add", "gat.lock"]);
    git(temp.path(), &["commit", "-q", "-m", "persist lock"]);

    let reader = GitReader::open(&layout).unwrap();
    assert_eq!(
        reader.staged_lock_snapshot().unwrap().to_lock().unwrap(),
        lock
    );
    assert_eq!(
        reader
            .lock_snapshot_at(&GitRevisionSpec::from("HEAD"))
            .unwrap()
            .to_lock()
            .unwrap(),
        lock
    );
    let expected = test_support_git::GitCommand::new(temp.path(), &["rev-parse", "HEAD"]).run();
    let expected =
        GitCommitId::parse_hex(String::from_utf8(expected.stdout).unwrap().trim()).unwrap();
    assert_eq!(
        gat_io::resolve_commit(&layout, &GitRevisionSpec::from("HEAD")).unwrap(),
        expected
    );
}

#[test]
fn committed_malformed_shard_identity_retains_the_semantic_source() {
    let temp = repository();
    let layout = layout(temp.path());
    let shard = temp.path().join("gat.lock").join("AB.tsv");
    std::fs::create_dir_all(shard.parent().unwrap()).unwrap();
    std::fs::write(&shard, b"").unwrap();
    git(temp.path(), &["add", "gat.lock"]);
    git(
        temp.path(),
        &["commit", "-q", "-m", "persist malformed lock"],
    );

    let Err(error) = GitReader::open(&layout)
        .unwrap()
        .lock_snapshot_at(&GitRevisionSpec::from("HEAD"))
    else {
        panic!("malformed shard identity must fail snapshot loading")
    };
    assert_eq!(error.kind(), gat_io::LockSnapshotErrorKind::InvalidSnapshot);
    assert!(
        std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<gat_core::lock::LockShardIdError>())
            .is_some()
    );
}

#[test]
fn layout_binds_discovery_ignore_integration_and_exclude_access() {
    let temp = repository();
    let layout = layout(temp.path());
    std::fs::write(temp.path().join("tracked.bin"), b"tracked").unwrap();
    std::fs::write(temp.path().join(".gatignore"), "ignored.bin\n").unwrap();
    git(temp.path(), &["add", "tracked.bin"]);

    let tracked = GatPath::parse_canonical("tracked.bin").unwrap();
    assert!(GitDiscovery::open(&layout).unwrap().is_tracked(&tracked));
    let ignore = GatIgnore::load(&layout).unwrap();
    assert!(ignore.is_ignored("ignored.bin"));
    assert!(!ignore.is_ignored("tracked.bin"));

    let integration = GitIntegration::open(&layout).unwrap();
    integration.install_merge_attributes().unwrap();
    assert!(
        std::fs::read_to_string(temp.path().join(".git/info/attributes"))
            .unwrap()
            .contains("/gat.lock merge=gat-lock")
    );

    let update = gat_io::mutate_info_exclude(&layout, false, |_| {
        InfoExcludeMutation::Replace("user-rule\n".to_string())
    })
    .unwrap();
    assert!(update.changed());
    assert_eq!(
        gat_io::read_info_exclude(&layout)
            .unwrap()
            .unwrap()
            .contents(),
        "user-rule\n"
    );
}

#[test]
fn linked_worktree_git_integration_uses_the_common_git_directory() {
    let temp = repository();
    std::fs::write(temp.path().join("tracked"), b"tracked").unwrap();
    git(temp.path(), &["add", "tracked"]);
    git(temp.path(), &["commit", "-q", "-m", "initial"]);
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    test_support_git::GitCommand::new(temp.path(), &["worktree", "add", "-q", "-b", "linked"])
        .arg(&linked)
        .run();

    let layout = layout(&linked);
    GitIntegration::open(&layout)
        .unwrap()
        .install_merge_attributes()
        .unwrap();

    assert!(
        std::fs::read_to_string(temp.path().join(".git/info/attributes"))
            .unwrap()
            .contains("/gat.lock merge=gat-lock")
    );
    assert!(!linked.join(".git/info/attributes").exists());
}

#[test]
fn invalid_repository_errors_retain_the_requested_root() {
    let temp = tempfile::tempdir().unwrap();
    let layout = layout(temp.path());

    let Err(reader_error) = GitReader::open(&layout) else {
        panic!("non-repository must not open")
    };
    assert_eq!(reader_error.path(), temp.path());

    let discovery_error = GitDiscovery::open(&layout).unwrap_err();
    assert_eq!(discovery_error.root(), temp.path());
}
