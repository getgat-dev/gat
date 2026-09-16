use gat_core::config::{Config, ConfigScope};
use gat_core::lexical_path::GatPath;
use gat_core::lock::{Lock, LockShardLevels};
use gat_core::name::MountName;
use gat_io::{
    ConfigError, ConfigStore, LockStore, MountJournal, MountTxnChange, MountTxnRecord,
    RepositoryLayout, ScopedConfigError, StateDatabaseHealth, count_stale_sidecars,
    inspect_database, rebuild_atomically, remove_stale_sidecars,
};

fn layout(root: &std::path::Path) -> RepositoryLayout {
    RepositoryLayout::at(root.to_path_buf())
}

#[test]
fn repository_config_scopes_are_layout_bound_and_keep_paths_technical() {
    let repository = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let layout = layout(repository.path());

    let mut project = Config::default();
    project.cache.location = Some(
        gat_core::cache_location::CacheLocation::try_from_path("project-cache".into())
            .expect("nonempty cache location"),
    );
    ConfigStore::save_scope(&layout, ConfigScope::Project, None, &project).unwrap();
    assert_eq!(
        ConfigStore::load_scope(&layout, ConfigScope::Project, None)
            .unwrap()
            .cache
            .location,
        project.cache.location
    );

    ConfigStore::save_scope(&layout, ConfigScope::Local, None, &Config::default()).unwrap();
    assert!(repository.path().join(".gat/gat.yaml").is_file());
    assert_eq!(
        ConfigStore::load_scope(&layout, ConfigScope::Local, None).unwrap(),
        Config::default()
    );

    ConfigStore::save_scope(
        &layout,
        ConfigScope::Global,
        Some(global.path()),
        &Config::default(),
    )
    .unwrap();
    assert!(global.path().join("gat.yaml").is_file());
    assert_eq!(
        ConfigStore::load_scope(&layout, ConfigScope::Global, Some(global.path())).unwrap(),
        Config::default()
    );
    assert!(matches!(
        ConfigStore::load_scope(&layout, ConfigScope::Global, None),
        Err(ScopedConfigError::Layout(
            gat_io::LayoutError::ConfigPathUnavailable
        ))
    ));

    let sentinel = repository.path().join("gat.yaml");
    std::fs::write(&sentinel, "git:\n  ignore_patterns: not-a-sequence\n").unwrap();
    let error = ConfigStore::load_scope(&layout, ConfigScope::Project, None).unwrap_err();
    let ScopedConfigError::Read(ConfigError::InvalidSyntax { path, .. }) = error else {
        panic!("expected scoped syntax error");
    };
    assert_eq!(path, sentinel);
}

#[test]
fn project_config_scaffold_is_atomic_and_idempotent() {
    let repository = tempfile::tempdir().unwrap();
    let layout = layout(repository.path());

    let mut config = gat_core::config::Config::default();
    assert!(ConfigStore::create_project_if_absent(&layout, &config).unwrap());
    let path = repository.path().join("gat.yaml");
    let original = std::fs::read_to_string(&path).unwrap();
    assert!(original.contains(&format!("# version: {}\n", gat_io::CONFIG_VERSION)));

    config.sync.auto_fetch = Some(true);
    assert!(!ConfigStore::create_project_if_absent(&layout, &config).unwrap());
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn state_maintenance_derives_database_and_sidecar_paths_from_the_layout() {
    let repository = tempfile::tempdir().unwrap();
    let layout = layout(repository.path());

    assert!(matches!(
        inspect_database(&layout).unwrap(),
        StateDatabaseHealth::Absent
    ));

    let state_dir = repository.path().join(".gat/state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("state.sqlite3-wal"), []).unwrap();
    assert_eq!(count_stale_sidecars(&layout).unwrap(), 1);
    assert_eq!(remove_stale_sidecars(&layout).unwrap(), 1);
    assert_eq!(count_stale_sidecars(&layout).unwrap(), 0);

    LockStore::publish_repository(&layout, &Lock::default(), LockShardLevels::FLAT).unwrap();
    rebuild_atomically(&layout).unwrap();
    assert!(matches!(
        inspect_database(&layout).unwrap(),
        StateDatabaseHealth::Healthy
    ));
}

#[cfg(unix)]
#[test]
fn dangling_database_link_prevents_stale_sidecar_cleanup() {
    let repository = tempfile::tempdir().unwrap();
    let layout = layout(repository.path());
    let state = repository.path().join(".gat/state");
    std::fs::create_dir_all(&state).unwrap();
    let database = state.join("state.sqlite3");
    std::os::unix::fs::symlink(state.join("missing.sqlite3"), &database).unwrap();
    for name in ["state.sqlite3-wal", "state.sqlite3-shm"] {
        std::fs::write(state.join(name), b"preserve sidecar").unwrap();
    }
    assert_eq!(count_stale_sidecars(&layout).unwrap(), 0);
    assert_eq!(remove_stale_sidecars(&layout).unwrap(), 0);
    for name in ["state.sqlite3-wal", "state.sqlite3-shm"] {
        assert_eq!(
            std::fs::read(state.join(name)).unwrap(),
            b"preserve sidecar"
        );
    }
    assert!(std::fs::symlink_metadata(database).unwrap().is_symlink());
}

#[cfg(unix)]
#[test]
fn orphaned_sidecar_cleanup_unlinks_live_and_dangling_links_without_following_them() {
    let repository = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let layout = layout(repository.path());
    let state = repository.path().join(".gat/state");
    std::fs::create_dir_all(&state).unwrap();
    let target = external.path().join("user-file");
    std::fs::write(&target, b"preserve target").unwrap();
    std::os::unix::fs::symlink(&target, state.join("state.sqlite3-wal")).unwrap();
    std::os::unix::fs::symlink(
        external.path().join("missing"),
        state.join("state.sqlite3-shm"),
    )
    .unwrap();
    assert_eq!(count_stale_sidecars(&layout).unwrap(), 2);
    assert_eq!(remove_stale_sidecars(&layout).unwrap(), 2);
    assert_eq!(count_stale_sidecars(&layout).unwrap(), 0);
    assert_eq!(remove_stale_sidecars(&layout).unwrap(), 0);
    assert_eq!(std::fs::read(target).unwrap(), b"preserve target");
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(external.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn database_inspection_rejects_live_and_dangling_symlinks_before_preparation() {
    for exists in [false, true] {
        let repository = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let layout = layout(repository.path());
        let target = external.path().join("external.sqlite3");
        if exists {
            let connection = rusqlite::Connection::open(&target).unwrap();
            connection
                .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE sentinel(value);")
                .unwrap();
        }
        let before = std::fs::read(&target).ok();
        for relative in [".gat/state/state.sqlite3", ".gat/objects/cache.sqlite3"] {
            let link = repository.path().join(relative);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
        }

        assert!(matches!(
            inspect_database(&layout).unwrap(),
            StateDatabaseHealth::Unreadable(gat_io::StateDatabaseUnreadable::NotARegularFile)
        ));
        assert!(matches!(
            layout
                .resolve_cache_root(None)
                .maintenance()
                .inspect_database()
                .unwrap(),
            gat_io::CacheDatabaseHealth::Unreadable(
                gat_io::CacheDatabaseUnreadable::NotARegularFile
            )
        ));
        assert_eq!(std::fs::read(&target).ok(), before);
        assert_eq!(
            std::fs::read_dir(external.path()).unwrap().count(),
            usize::from(exists)
        );
        assert!(!repository.path().join(".gat/.gitignore").exists());
        for relative in [".gat/state/state.sqlite3", ".gat/objects/cache.sqlite3"] {
            assert!(
                std::fs::symlink_metadata(repository.path().join(relative))
                    .unwrap()
                    .is_symlink()
            );
        }
    }
}

#[test]
fn mount_journal_is_repository_bound_and_preserves_typed_records() {
    let repository = tempfile::tempdir().unwrap();
    let layout = layout(repository.path());
    let journal = MountJournal::open(&layout);
    let record = MountTxnRecord {
        phase: gat_io::MountTxnPhase::Publish,
        shard_levels: gat_core::lock::LockShardLevels::FLAT,
        change: MountTxnChange::Add {
            target: GatPath::parse_canonical("data").unwrap(),
            row_windows: 0,
        },
        scope: ConfigScope::Project,
        name: MountName::from_string("data".to_string()),
        post_config: Config::default(),
        pre_config: Config::default(),
    };

    journal.write(&record).unwrap();
    let restored = journal.read().unwrap().unwrap();

    assert!(matches!(restored.change, MountTxnChange::Add { .. }));
    assert_eq!(restored.scope, ConfigScope::Project);
    assert_eq!(restored.name.as_str(), "data");
    assert_eq!(restored.change.new_target().unwrap().as_str(), "data");
    journal.remove_journal().unwrap();
    assert!(journal.read().unwrap().is_none());
}
