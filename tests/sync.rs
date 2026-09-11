//! Root cross-layer characterization for the command-owned sync use case.

#[cfg(test)]
mod common;

use ::test_support::add;
use common::RecordingProgress;
use gat_command::{SyncError, SyncOutcome, SyncRequest};
use gat_core::config::{Config, ConfigScope};
use gat_core::lock::{Lock, LockShardLevels};
use gat_core::progress::{NoopProgress, ProgressReporter};
use gat_core::selection::Selection;
use gat_engine::Repository as Repo;
use gat_io::CacheRoot;
use std::path::{Path, PathBuf};
use test_support::{git_repo_with_initial_commit as test_repo, remote_add_with_default};
use test_support_git::commit_all;

fn layout(root: &Path) -> gat_io::RepositoryLayout {
    gat_io::RepositoryLayout::at(root.to_path_buf())
}

fn cache_root(repo: &Repo) -> CacheRoot {
    gat_engine::test_support::cache_root(repo)
}

fn request() -> SyncRequest {
    SyncRequest {
        selection: Some(Selection::root()),
        force: false,
        dry_run: false,
        trust_state: false,
        fetch: false,
        repair: false,
        remote: None,
        rematerialize: false,
    }
}

fn run(
    repo: &Repo,
    request: SyncRequest,
    progress: &dyn ProgressReporter,
) -> Result<SyncOutcome, SyncError> {
    gat_command::sync(repo, request, progress)
}

fn push(repo: &Repo, progress: &dyn ProgressReporter) {
    let selection = Selection::root();
    gat_command::push(
        repo,
        gat_command::PushRequest {
            selection: Some(&selection),
            remote: None,
            source: gat_command::PushSource::Current,
        },
        progress,
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn sync_finishes_progress_when_reconciliation_fails_mid_run() {
    use std::os::unix::fs::symlink;

    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), tmp.path().join("link")).unwrap();
    let ingested = cache_root(&repo)
        .writer()
        .ingest(&b"payload"[..])
        .unwrap()
        .0;
    let mut lock = Lock::default();
    lock.upsert(
        gat_core::lexical_path::GatPath::parse_canonical("link/out.bin").unwrap(),
        ingested.oid,
    );
    repo.save_lock(&lock).unwrap();

    let progress = RecordingProgress::new();
    let error = run(
        &repo,
        SyncRequest {
            trust_state: true,
            ..request()
        },
        &progress,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        SyncError::Reconciliation(ref source)
            if matches!(
                source.kind(),
                gat_engine::SyncErrorKind::WorktreePath {
                    kind: gat_engine::SyncWorktreePathFailureKind::OutsideRepository,
                    ..
                }
            )
    ));
    assert!(progress.tasks().iter().all(|task| task.finished));
}

#[test]
fn repair_then_rematerialize_reuses_one_snapshot_and_rematerializes_once() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    repo.write_config_fixture(&Config {
        cache: gat_core::config::CacheConfig {
            materialization_strategy: Some("copy".parse().unwrap()),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    let paths = ["a.bin", "b.bin", "c.bin", "d.bin"];
    for (index, path) in paths.iter().enumerate() {
        std::fs::write(tmp.path().join(path), format!("content-{index}")).unwrap();
    }
    add(
        &repo,
        &paths.iter().map(PathBuf::from).collect::<Vec<_>>(),
        &NoopProgress,
    )
    .unwrap();
    commit_all(tmp.path(), "add files");
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();
    push(&repo, &NoopProgress);

    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries
        .iter()
        .find(|entry| entry.path == "a.bin")
        .unwrap()
        .oid;
    let root = cache_root(&repo);
    let object = root.object_path_for_test(&oid);
    root.make_object_writable_for_test(&oid).unwrap();
    std::fs::write(&object, b"corrupted").unwrap();
    let mut config = repo.load_config_scoped(ConfigScope::Project).unwrap();
    config.cache.materialization_strategy = Some("symlink".parse().unwrap());
    repo.write_scoped_config_fixture(&config, ConfigScope::Project)
        .unwrap();

    let config_loads_before = gat_engine::test_support::config_loads();
    let remote_opens_before = gat_engine::test_support::remote_opens();
    let rematerialize_before = gat_engine::test_support::do_rematerialize_calls();
    let outcome = run(
        &repo,
        SyncRequest {
            rematerialize: true,
            repair: true,
            ..request()
        },
        &NoopProgress,
    )
    .unwrap();
    let rematerialize_after = gat_engine::test_support::do_rematerialize_calls();

    assert_eq!(outcome.outcome.rematerialized, paths.len());
    assert_eq!(rematerialize_after - rematerialize_before, paths.len());
    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1
    );
    assert_eq!(
        gat_engine::test_support::remote_opens() - remote_opens_before,
        1
    );
}

#[test]
fn many_paths_resolve_snapshot_policy_once() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let paths: Vec<PathBuf> = (0..50)
        .map(|index| {
            let path = PathBuf::from(format!("file{index}.bin"));
            std::fs::write(tmp.path().join(&path), format!("content {index}")).unwrap();
            path
        })
        .collect();
    add(&repo, &paths, &NoopProgress).unwrap();
    commit_all(tmp.path(), "add files");
    for (index, path) in paths.iter().enumerate() {
        std::fs::write(tmp.path().join(path), format!("edited {index}")).unwrap();
    }

    let config_loads_before = gat_engine::test_support::config_loads();
    let policy_compilations_before = gat_engine::test_support::policy_compilations();
    let cache_location_resolutions_before = gat_engine::test_support::cache_location_resolutions();
    let materialization_strategy_resolutions_before =
        gat_engine::test_support::materialization_strategy_resolutions();
    run(
        &repo,
        SyncRequest {
            force: true,
            ..request()
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1
    );
    assert_eq!(
        gat_engine::test_support::policy_compilations() - policy_compilations_before,
        1
    );
    assert_eq!(
        gat_engine::test_support::cache_location_resolutions() - cache_location_resolutions_before,
        1
    );
    assert_eq!(
        gat_engine::test_support::materialization_strategy_resolutions()
            - materialization_strategy_resolutions_before,
        1
    );
}

#[test]
fn hook_fetch_sync_and_repair_share_one_operation() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    repo.write_config_fixture(&Config {
        cache: gat_core::config::CacheConfig {
            materialization_strategy: Some("copy".parse().unwrap()),
            ..Default::default()
        },
        sync: gat_core::config::SyncConfig {
            auto_fetch: Some(true),
            auto_repair: Some(true),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    for path in ["a.bin", "b.bin"] {
        std::fs::write(tmp.path().join(path), b"hello").unwrap();
    }
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();
    commit_all(tmp.path(), "add files");
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();
    push(&repo, &NoopProgress);

    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    let root = cache_root(&repo);
    let object = root.object_path_for_test(&oid);
    root.make_object_writable_for_test(&oid).unwrap();
    std::fs::write(&object, b"corrupted").unwrap();
    std::fs::write(tmp.path().join("a.bin"), b"edited a").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"edited b").unwrap();

    let config_loads_before = gat_engine::test_support::config_loads();
    let remote_opens_before = gat_engine::test_support::remote_opens();
    gat_command::hook(
        &repo,
        gat_command::HookRequest,
        &gat_core::progress::NoopProgress,
    )
    .unwrap();

    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1
    );
    assert_eq!(
        gat_engine::test_support::remote_opens() - remote_opens_before,
        1
    );
    assert_eq!(std::fs::read(object).unwrap(), b"hello");
}

#[test]
fn hook_never_rematerializes_after_strategy_change() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    repo.write_config_fixture(&Config {
        cache: gat_core::config::CacheConfig {
            materialization_strategy: Some("copy".parse().unwrap()),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    std::fs::write(tmp.path().join("a.bin"), b"hello").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add file");
    gat_command::hook(&repo, gat_command::HookRequest, &NoopProgress).unwrap();

    let mut config = repo.load_config_scoped(ConfigScope::Project).unwrap();
    config.cache.materialization_strategy = Some("symlink".parse().unwrap());
    repo.write_scoped_config_fixture(&config, ConfigScope::Project)
        .unwrap();
    let outcome = gat_command::hook(&repo, gat_command::HookRequest, &NoopProgress).unwrap();

    assert_eq!(outcome.outcome.rematerialized, 0);
    assert!(
        !std::fs::symlink_metadata(tmp.path().join("a.bin"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn reshape_and_reconciliation_share_one_progress_task() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    let mut config = repo.load_config_scoped(ConfigScope::Project).unwrap();
    config.lock.shard_levels = Some(LockShardLevels::new(2).unwrap());
    repo.write_scoped_config_fixture(&config, ConfigScope::Project)
        .unwrap();

    let progress = RecordingProgress::new();
    let outcome = run(&repo, request(), &progress).unwrap();

    assert_eq!(
        outcome.reshaped,
        Some(gat_core::lock::LockShardLevels::new(2).unwrap())
    );
    assert_eq!(
        progress.count_of(gat_core::progress::ProgressOperation::Synchronizing),
        1
    );
    assert_eq!(progress.max_active_tasks(), 1);
}

#[test]
fn sync_and_hook_return_reports_with_the_same_unresolved_paths() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    repo.write_config_fixture(&Config {
        cache: gat_core::config::CacheConfig {
            materialization_strategy: Some("copy".parse().unwrap()),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    std::fs::write(tmp.path().join("local.bin"), b"original").unwrap();
    add(&repo, &[PathBuf::from("local.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "track local.bin");
    std::fs::write(tmp.path().join("local.bin"), b"locally edited").unwrap();

    let synced = run(&repo, request(), &NoopProgress).unwrap();
    let hooked = gat_command::hook(&repo, gat_command::HookRequest, &NoopProgress).unwrap();
    assert!(!synced.is_clean());
    assert!(!hooked.is_clean());
    assert_eq!(synced.outcome.conflicts, vec!["local.bin".to_string()]);
    assert_eq!(hooked.outcome.conflicts, synced.outcome.conflicts);
    assert_eq!(hooked.outcome.missing, synced.outcome.missing);
    assert_eq!(hooked.outcome.corrupted, synced.outcome.corrupted);
    assert_eq!(
        std::fs::read(tmp.path().join("local.bin")).unwrap(),
        b"locally edited"
    );
}
