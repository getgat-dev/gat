use gat_core::config::ConfigScope;
use gat_core::lexical_path::GatPath;
use gat_core::lock::Lock;
use gat_core::oid::Oid;
use gat_core::progress::NoopProgress;
use gat_core::selection::Selection;
use gat_engine::{
    DesiredOperation, DesiredRevisionError, Repository, RepositoryStateError,
    acquire_operation_without_desired_state,
};

fn repository() -> (tempfile::TempDir, Repository) {
    let temp = tempfile::tempdir().expect("create temporary repository");
    test_support_git::run_git(temp.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(temp.path().join("README"), "fixture\n").expect("write initial file");
    test_support_git::commit_all(temp.path(), "initial");
    let repo = Repository::at(temp.path().to_path_buf());
    (temp, repo)
}

fn lock_with(path: &str, byte: char) -> Lock {
    let mut lock = Lock::default();
    lock.upsert(
        GatPath::parse_canonical(path).expect("canonical fixture path"),
        Oid::from_hex(&byte.to_string().repeat(64)).expect("valid fixture object id"),
    );
    lock
}

#[test]
fn desired_operation_exposes_one_coherent_view_and_finishes_into_its_operation() {
    let (_temp, repo) = repository();
    repo.save_lock(&lock_with("data.bin", '1'))
        .expect("publish desired state");

    let desired = DesiredOperation::acquire(&repo, &NoopProgress).expect("acquire operation");
    let mut seen = Vec::new();
    desired
        .desired_view()
        .visit_entries(&Selection::root(), |entry| {
            seen.push(entry.path);
            Ok::<(), RepositoryStateError>(())
        })
        .expect("visit desired entries");
    assert_eq!(seen, vec![GatPath::parse_canonical("data.bin").unwrap()]);

    let operation = desired.finish_selection();
    assert!(std::ptr::eq(operation.repo(), &raw const repo));
    assert_eq!(operation.config().sync.trust_state, None);
}

#[test]
fn repository_revision_and_desired_view_reject_stale_state() {
    let (_temp, repo) = repository();
    repo.save_lock(&lock_with("first.bin", '1'))
        .expect("publish initial desired state");

    let revision = repo
        .current_desired_revision()
        .expect("observe desired revision");
    repo.revalidate_desired_revision(&revision)
        .expect("unchanged revision remains valid");

    let mut seen = Vec::new();
    repo.visit_current_desired_entries(&Selection::root(), |entry| seen.push(entry.path))
        .expect("visit current desired state");
    assert_eq!(seen, vec![GatPath::parse_canonical("first.bin").unwrap()]);

    repo.save_lock(&lock_with("second.bin", '2'))
        .expect("publish changed desired state");
    assert!(matches!(
        repo.revalidate_desired_revision(&revision),
        Err(DesiredRevisionError::Stale(_))
    ));
}

#[test]
fn repository_revision_treats_missing_or_corrupt_state_as_an_optional_accelerator() {
    let (temp, repo) = repository();
    repo.save_lock(&lock_with("data.bin", '1'))
        .expect("publish desired state");
    let database = temp.path().join(".gat/state/state.sqlite3");

    let expected = repo
        .current_desired_revision()
        .expect("observe without state database");
    assert!(
        !database.exists(),
        "revision observation must not create accelerator state"
    );

    std::fs::create_dir_all(database.parent().unwrap()).expect("create state directory");
    let corrupt = b"not a sqlite database";
    std::fs::write(&database, corrupt).expect("write corrupt accelerator");

    assert_eq!(
        repo.current_desired_revision()
            .expect("fall back from corrupt accelerator"),
        expected
    );
    assert_eq!(
        std::fs::read(database).expect("read untouched accelerator"),
        corrupt
    );
}

#[cfg(feature = "test-support")]
#[test]
fn repository_revision_reuses_warm_state_without_reading_or_hashing_lock_content() {
    let (_temp, repo) = repository();
    repo.save_lock(&lock_with("data.bin", '1'))
        .expect("publish desired state");
    DesiredOperation::acquire(&repo, &NoopProgress).expect("seed desired accelerator");

    let (reads, hashes) = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap()
        .install(|| {
            let reads_before =
                gat_io::lock_identity_test_support::current_identity_content_read_call_count();
            let hashes_before = gat_io::lock_identity_test_support::hash_shard_bytes_call_count();
            repo.current_desired_revision()
                .expect("observe warm desired revision");
            (
                gat_io::lock_identity_test_support::current_identity_content_read_call_count()
                    - reads_before,
                gat_io::lock_identity_test_support::hash_shard_bytes_call_count() - hashes_before,
            )
        });

    assert_eq!(reads, 0);
    assert_eq!(hashes, 0);
}

#[test]
fn acquired_operations_keep_their_config_snapshot() {
    let (_temp, repo) = repository();
    let desired = DesiredOperation::acquire(&repo, &NoopProgress).expect("acquire operation");
    assert_eq!(desired.operation().config().sync.trust_state, None);

    let mut config = repo
        .load_config_scoped(ConfigScope::Project)
        .expect("load project config");
    config.sync.trust_state = Some(true);
    repo.save_config_scoped(&config, ConfigScope::Project)
        .expect("save project config");

    assert_eq!(desired.operation().config().sync.trust_state, None);
    let next = DesiredOperation::acquire(&repo, &NoopProgress).expect("reacquire operation");
    assert_eq!(next.operation().config().sync.trust_state, Some(true));
}

#[cfg(feature = "test-support")]
#[test]
fn public_acquisition_paths_load_config_once_without_reobserving_warm_desired_state() {
    let (_temp, repo) = repository();
    repo.save_lock(&lock_with("data.bin", '3'))
        .expect("publish desired state");

    DesiredOperation::acquire(&repo, &NoopProgress).expect("warm desired state");
    gat_engine::test_support::reset_canonical_observations();
    let before = gat_engine::test_support::config_loads();
    DesiredOperation::acquire(&repo, &NoopProgress).expect("acquire desired operation");
    assert_eq!(gat_engine::test_support::config_loads() - before, 1);
    assert_eq!(gat_engine::test_support::canonical_observations(), 0);

    let before = gat_engine::test_support::config_loads();
    let operation = acquire_operation_without_desired_state(&repo, &NoopProgress)
        .expect("acquire desired-free operation");
    assert_eq!(gat_engine::test_support::config_loads() - before, 1);
    assert!(std::ptr::eq(operation.repo(), &raw const repo));
}
