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
fn local_storage_writers_ignore_their_first_files_without_exclude_sync() {
    for writer in [
        "state",
        "lock",
        "config",
        "journal",
        "cache",
        "cache-repair",
        "state-repair",
        "cache-proof",
        "cache-sweep",
    ] {
        let temp = repository();
        let layout = layout(temp.path());
        let exclude_path = temp.path().join(".git/info/exclude");
        let before = std::fs::read(&exclude_path).unwrap();
        match writer {
            "state" => {
                gat_io::StateStore::open(&layout).unwrap();
            }
            "lock" => {
                gat_io::RepoLock::acquire_repository(&layout).unwrap();
            }
            "config" => gat_io::ConfigStore::save_scope(
                &layout,
                gat_core::config::ConfigScope::Local,
                None,
                &gat_core::config::Config::default(),
            )
            .unwrap(),
            "journal" => gat_io::MountJournal::open(&layout)
                .reset_staged_rows()
                .unwrap(),
            "cache" => {
                layout
                    .resolve_cache_root(None, None)
                    .writer()
                    .ingest(&b"content"[..])
                    .unwrap();
            }
            "cache-repair" => layout
                .resolve_cache_root(None, None)
                .maintenance()
                .rebuild_database()
                .unwrap(),
            "state-repair" => gat_io::rebuild_atomically(&layout).unwrap(),
            "cache-sweep" => {
                seed_unprotected_cache_object(&layout);
                layout
                    .resolve_cache_root(None, None)
                    .maintenance()
                    .sweep(false, |_| {
                        Ok::<_, std::convert::Infallible>(gat_io::CacheSweepDecision::Delete)
                    })
                    .unwrap()
                    .unwrap();
                assert!(temp.path().join(".gat/objects/cache.sqlite3").is_file());
            }
            "cache-proof" => {
                std::fs::create_dir_all(temp.path().join(".gat/objects")).unwrap();
                let _client = layout.resolve_cache_root(None, None).open_client();
                assert!(temp.path().join(".gat/objects/cache.sqlite3").is_file());
            }
            _ => unreachable!(),
        }
        assert_eq!(
            std::fs::read(temp.path().join(".gat/.gitignore")).unwrap(),
            b"*\n",
            "{writer}"
        );
        assert_eq!(std::fs::read(exclude_path).unwrap(), before, "{writer}");
        std::fs::write(temp.path().join("visible"), b"visible").unwrap();
        let status = test_support_git::run_git(
            temp.path(),
            &["status", "--porcelain", "--untracked-files=all"],
        );
        assert_eq!(
            String::from_utf8(status.stdout).unwrap(),
            "?? visible\n",
            "{writer}"
        );
        git(temp.path(), &["add", "-A"]);
        let staged = test_support_git::run_git(temp.path(), &["ls-files"]);
        assert_eq!(
            String::from_utf8(staged.stdout).unwrap(),
            "visible\n",
            "{writer}"
        );
    }
}

#[test]
fn read_only_access_and_external_cache_writes_do_not_create_local_storage() {
    let temp = repository();
    let layout = layout(temp.path());
    assert!(
        gat_io::StateStore::open_if_exists(&layout)
            .unwrap()
            .is_none()
    );
    assert!(
        gat_io::MountJournal::open(&layout)
            .read()
            .unwrap()
            .is_none()
    );
    let _client = layout.resolve_cache_root(None, None).open_client();
    assert!(matches!(
        gat_io::inspect_database(&layout).unwrap(),
        gat_io::StateDatabaseHealth::Absent
    ));
    assert!(matches!(
        layout
            .resolve_cache_root(None, None)
            .maintenance()
            .inspect_database()
            .unwrap(),
        gat_io::CacheDatabaseHealth::Absent
    ));
    let external = tempfile::tempdir().unwrap();
    layout
        .resolve_cache_root(Some(external.path().as_os_str()), None)
        .writer()
        .ingest(&b"content"[..])
        .unwrap();
    assert!(!temp.path().join(".gat").exists());
    assert!(!external.path().join(".gitignore").exists());
}

fn seed_unprotected_cache_object(layout: &RepositoryLayout) -> std::path::PathBuf {
    let cache = layout.resolve_cache_root(None, None);
    let oid = Oid::from_hex(&"aa".repeat(32)).unwrap();
    let object = cache.object_path_for_test(&oid);
    std::fs::create_dir_all(object.parent().unwrap()).unwrap();
    std::fs::write(&object, b"legacy object").unwrap();
    object
}

#[test]
fn cache_sweep_prepares_only_existing_storage_and_never_writes_in_dry_run() {
    let temp = repository();
    let layout = layout(temp.path());
    let cache = layout.resolve_cache_root(None, None);
    let keep = |_| Ok::<_, std::convert::Infallible>(gat_io::CacheSweepDecision::Keep);
    cache.maintenance().sweep(false, keep).unwrap().unwrap();
    assert!(!temp.path().join(".gat").exists());
    let object = seed_unprotected_cache_object(&layout);
    cache.maintenance().sweep(false, keep).unwrap().unwrap();
    let uncertain = cache
        .maintenance()
        .sweep(false, |_| {
            Ok::<_, std::convert::Infallible>(gat_io::CacheSweepDecision::Uncertain)
        })
        .unwrap()
        .unwrap();
    assert_eq!(uncertain.uncertain, 1);
    assert!(
        cache
            .maintenance()
            .sweep(false, |_| { Err::<gat_io::CacheSweepDecision, _>(()) })
            .unwrap()
            .is_err()
    );
    cache
        .maintenance()
        .sweep(true, |_| {
            Ok::<_, std::convert::Infallible>(gat_io::CacheSweepDecision::Delete)
        })
        .unwrap()
        .unwrap();
    assert!(!temp.path().join(".gat/.gitignore").exists());
    assert!(!temp.path().join(".gat/objects/cache.sqlite3").exists());
    std::fs::write(temp.path().join(".gat/.gitignore"), b"!keep\n").unwrap();
    assert!(
        cache
            .maintenance()
            .sweep(false, |_| {
                Ok::<_, std::convert::Infallible>(gat_io::CacheSweepDecision::Delete)
            })
            .is_err()
    );
    assert!(object.is_file());
    assert!(!temp.path().join(".gat/objects/cache.sqlite3").exists());
}

#[test]
fn database_readers_fail_before_creating_sidecars_when_protection_is_invalid() {
    let temp = repository();
    let initial = layout(temp.path());
    drop(gat_io::StateStore::open(&initial).unwrap());
    initial
        .resolve_cache_root(None, None)
        .maintenance()
        .rebuild_database()
        .unwrap();
    let ignore = temp.path().join(".gat/.gitignore");
    std::fs::write(&ignore, b"!keep\n").unwrap();
    let layout = layout(temp.path());
    assert!(gat_io::StateStore::open_if_exists(&layout).is_err());
    assert!(gat_io::inspect_database(&layout).is_err());
    assert!(
        layout
            .resolve_cache_root(None, None)
            .maintenance()
            .inspect_database()
            .is_err()
    );
    for database in ["state/state.sqlite3", "objects/cache.sqlite3"] {
        for suffix in ["-wal", "-shm"] {
            assert!(
                !temp
                    .path()
                    .join(".gat")
                    .join(format!("{database}{suffix}"))
                    .exists()
            );
        }
    }
    assert_eq!(std::fs::read(ignore).unwrap(), b"!keep\n");
}

#[test]
fn existing_database_readers_protect_sqlite_sidecars_before_opening() {
    for reader in ["state", "state-health", "cache-health"] {
        let temp = repository();
        let initial = layout(temp.path());
        drop(gat_io::StateStore::open(&initial).unwrap());
        initial
            .resolve_cache_root(None, None)
            .maintenance()
            .rebuild_database()
            .unwrap();
        // Model storage from an older Gat version without the self-ignore file.
        std::fs::remove_file(temp.path().join(".gat/.gitignore")).unwrap();
        let layout = layout(temp.path());
        match reader {
            "state" => {
                let store = gat_io::StateStore::open_if_exists(&layout)
                    .unwrap()
                    .unwrap();
                assert!(temp.path().join(".gat/state/state.sqlite3-wal").exists());
                drop(store);
            }
            "state-health" => assert!(matches!(
                gat_io::inspect_database(&layout).unwrap(),
                gat_io::StateDatabaseHealth::Healthy
            )),
            "cache-health" => assert!(matches!(
                layout
                    .resolve_cache_root(None, None)
                    .maintenance()
                    .inspect_database()
                    .unwrap(),
                gat_io::CacheDatabaseHealth::Healthy
            )),
            _ => unreachable!(),
        }
        assert_eq!(
            std::fs::read(temp.path().join(".gat/.gitignore")).unwrap(),
            b"*\n",
            "{reader}"
        );
        let status = test_support_git::run_git(
            temp.path(),
            &["status", "--porcelain", "--untracked-files=all"],
        );
        assert!(status.stdout.is_empty(), "{reader}");
    }
}

#[test]
fn failed_ignore_initialization_prevents_state_and_cache_publication() {
    let temp = repository();
    let layout = layout(temp.path());
    std::fs::create_dir_all(temp.path().join(".gat/.gitignore")).unwrap();
    assert!(gat_io::StateStore::open(&layout).is_err());
    assert!(gat_io::RepoLock::acquire_repository(&layout).is_err());
    assert!(
        layout
            .resolve_cache_root(None, None)
            .writer()
            .begin_ingest()
            .is_err()
    );
    assert!(!temp.path().join(".gat/state").exists());
    assert!(!temp.path().join(".gat/objects").exists());
}

#[test]
fn linked_worktrees_ignore_local_storage_independently() {
    let temp = repository();
    git(
        temp.path(),
        &["commit", "--allow-empty", "-q", "-m", "initial"],
    );
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    test_support_git::GitCommand::new(temp.path(), &["worktree", "add", "-q", "-b", "linked"])
        .arg(&linked)
        .run();
    gat_io::StateStore::open(&layout(&linked)).unwrap();
    assert!(!temp.path().join(".gat").exists());
    let status =
        test_support_git::run_git(&linked, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(status.stdout.is_empty());
    gat_io::StateStore::open(&layout(temp.path())).unwrap();
    assert_eq!(
        std::fs::read(temp.path().join(".gat/.gitignore")).unwrap(),
        b"*\n"
    );
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
