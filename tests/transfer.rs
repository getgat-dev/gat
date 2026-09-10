use gat_core::progress::NoopProgress;
use gat_core::progress::ProgressSpec;
mod common;

use common::RecordingProgress;
use gat_core::config::{MountConfig, RouteConfig};
use gat_core::history::{HistoryRoot, HistorySelection, HistoryTraversal};
use gat_core::name::RemoteName;
use gat_core::path_scope::normalize_path_scope;
use gat_core::progress::{ProgressActivity, ProgressOperation, ProgressReporter};
use gat_core::selection::Selection;
use gat_engine::{DesiredOperation, ExecutionLimits, Repository as Repo};
use gat_io as storage;
use gat_io::{hash_file_call_count, with_exclusive_hash_file_call_count};
use std::path::{Path, PathBuf};

use ::test_support::add;
use test_support::{git_repo_with_initial_commit as test_repo, remote_add_with_default, route_add};
use test_support_git::{commit_all, stage_all};

/// Reads the production planning bound instead of duplicating it in tests.
fn transfer_planning_window() -> usize {
    ExecutionLimits::production().transfer.window.get()
}

/// Repair shares the production transfer planning bound.
fn repair_window() -> usize {
    transfer_planning_window()
}

fn layout(root: &Path) -> gat_io::RepositoryLayout {
    gat_io::RepositoryLayout::at(root.to_path_buf())
}

fn scoped_selection(path: &Path) -> Selection {
    Selection::from_scope_patterns(normalize_path_scope(path).unwrap(), Vec::new(), Vec::new())
}

fn cache_path(repo: &Repo) -> PathBuf {
    cache_root(repo).display_path().to_path_buf()
}

fn cache_root(repo: &Repo) -> gat_io::CacheRoot {
    gat_engine::test_support::cache_root(repo)
}

fn push(
    repo: &Repo,
    selection: Option<&Selection>,
    remote: Option<&RemoteName>,
    progress: &dyn ProgressReporter,
) -> Result<gat_command::PushOutcome, gat_command::PushError> {
    gat_command::push(
        repo,
        gat_command::PushRequest {
            selection,
            remote,
            source: gat_command::PushSource::Current,
        },
        progress,
    )
}

fn push_with_history(
    repo: &Repo,
    selection: &Selection,
    remote: Option<&RemoteName>,
    history: &HistorySelection,
    progress: &dyn ProgressReporter,
) -> Result<gat_command::PushOutcome, gat_command::PushError> {
    gat_command::push(
        repo,
        gat_command::PushRequest {
            selection: Some(selection),
            remote,
            source: gat_command::PushSource::History(history),
        },
        progress,
    )
}

fn push_selected(
    mut desired: DesiredOperation<'_>,
    remote: Option<&RemoteName>,
    progress: &dyn ProgressReporter,
) -> Result<gat_command::PushOutcome, gat_command::PushError> {
    let selection = Selection::root();
    gat_command::push_with_desired_operation(
        &mut desired,
        gat_command::PushRequest {
            selection: Some(&selection),
            remote,
            source: gat_command::PushSource::Current,
        },
        progress,
    )
}

fn repair_corrupted(
    operation: &mut gat_engine::Operation<'_>,
    remote: Option<&RemoteName>,
    corrupted: &[(gat_core::lexical_path::GatPath, gat_core::oid::Oid)],
    progress: &gat_core::progress::ProgressHandle,
) -> gat_command::RepairOutcome {
    gat_command::repair_with_operation(
        operation,
        gat_command::RepairRequest { corrupted, remote },
        progress,
    )
}

fn fetch_current(
    repo: &Repo,
    selection: &Selection,
    remote: Option<&RemoteName>,
    progress: &dyn ProgressReporter,
) -> Result<gat_command::FetchOutcome, gat_command::FetchError> {
    gat_command::fetch(
        repo,
        gat_command::FetchRequest {
            selection: Some(selection),
            remote,
            source: gat_command::FetchSource::Current,
        },
        progress,
    )
}

fn fetch_history(
    repo: &Repo,
    selection: &Selection,
    remote: Option<&RemoteName>,
    history: &HistorySelection,
    progress: &dyn ProgressReporter,
) -> Result<gat_command::FetchOutcome, gat_command::FetchError> {
    gat_command::fetch(
        repo,
        gat_command::FetchRequest {
            selection: Some(selection),
            remote,
            source: gat_command::FetchSource::History(history),
        },
        progress,
    )
}

fn fetch_selected(
    desired: &mut DesiredOperation<'_>,
    selection: &Selection,
    remote: Option<&RemoteName>,
    progress: &dyn ProgressReporter,
) -> Result<gat_command::FetchOutcome, gat_command::FetchError> {
    gat_command::fetch_with_desired_operation(
        desired,
        gat_command::FetchRequest {
            selection: Some(selection),
            remote,
            source: gat_command::FetchSource::Current,
        },
        progress,
    )
}

fn pull_current(
    repo: &Repo,
    selection: Selection,
    remote: Option<RemoteName>,
    progress: &dyn ProgressReporter,
) -> Result<gat_command::SyncOutcome, gat_command::SyncError> {
    gat_command::pull(
        repo,
        gat_command::PullRequest {
            selection: Some(selection),
            remote,
            history: None,
        },
        progress,
    )
}

fn pull_selected(
    desired: DesiredOperation<'_>,
    selection: Selection,
    remote: Option<RemoteName>,
    history: Option<HistorySelection>,
    progress: &dyn ProgressReporter,
) -> Result<gat_command::SyncOutcome, gat_command::SyncError> {
    gat_command::pull_with_desired_operation(
        desired,
        gat_command::PullRequest {
            selection: Some(selection),
            remote,
            history,
        },
        progress,
    )
}

fn gp(path: &str) -> gat_core::lexical_path::GatPath {
    gat_core::lexical_path::GatPath::parse_canonical(path).unwrap()
}

fn mount_config(target: &str) -> MountConfig {
    MountConfig {
        // hygiene-ok: pure config-value string for a fixture MountConfig; never dialed as a real URL.
        url: "https://example.com/source.git".to_string().into(),
        target: gp(target),
        path: gat_core::lexical_path::GatSubpath::Root,
        rev: None,
        rev_lock: Some("deadbeef".repeat(5).parse().unwrap()),
        include: Vec::new(),
        exclude: Vec::new(),
    }
}

#[test]
fn history_aware_push_reports_resolution_before_pushing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add a.bin");
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();
    let progress = RecordingProgress::new();
    let selection = gat_core::history::HistorySelection {
        roots: vec![HistoryRoot::Revision("HEAD".to_string().into())],
        ..Default::default()
    };

    push_with_history(&repo, &Selection::root(), None, &selection, &progress).unwrap();

    // Remote checking and uploading are both represented as activity on
    // the *same* logical `Pushing` task, never as separate task
    // lifecycles: exactly one such task must exist for this push, and
    // selection resolution must be reported as the task's *first*
    // activity, before any per-object upload activity.
    assert_eq!(progress.count_of(ProgressOperation::Pushing), 1);
    let task = progress.only(ProgressOperation::Pushing);
    assert_eq!(
        task.activities.first(),
        Some(&ProgressActivity::ResolvingSelection)
    );
    assert!(
        task.activities.iter().any(|activity| matches!(
            activity,
            ProgressActivity::TransferringFile { path } if path == "a.bin"
        )),
        "expected a per-object upload activity after resolution, got {:?}",
        task.activities
    );
    assert_eq!(progress.max_active_tasks(), 1);
}

#[test]
fn fetch_progress_reports_activity_message_per_object() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add a.bin");
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    let progress = RecordingProgress::new();

    fetch_current(&repo, &Selection::root(), None, &progress).unwrap();

    // The one logical `Fetching` task must have advanced its item
    // position and reported the object it was downloading as typed activity.
    let fetch_task = progress.only(ProgressOperation::Fetching);
    assert!(fetch_task.position > 0);
    assert!(fetch_task.activities.iter().any(|activity| matches!(
        activity,
        ProgressActivity::TransferringFile { path } if path == "a.bin"
    )));
}

#[test]
fn push_then_fetch_roundtrips_through_file_remote() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    commit_all(tmp.path(), "add big.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap();
    assert!(cache_root(&repo).presence().contains(&oid));
}

/// `fetch` must still succeed and download objects correctly even
/// when the shared proof DB opens fine but then fails mid-operation
/// persisting a download's proof: `cache.sqlite3` is a disposable
/// accelerator, so a raw `SQLite` failure there must only cost a
/// later re-verification hash, never fail an otherwise-successful
/// fetch.
#[test]
fn fetch_succeeds_when_the_proof_db_fails_after_opening_successfully() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    commit_all(tmp.path(), "add big.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    let objects_dir = cache_path(&repo);
    std::fs::create_dir_all(&objects_dir).unwrap();
    {
        cache_root(&repo).break_database_for_test();
    }

    fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap();
    assert!(cache_root(&repo).presence().contains(&oid));
}

#[test]
fn fetch_skips_remote_and_hash_when_cached_object_is_already_verified() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    let handle = rt.handle().clone();

    let fetched = with_exclusive_hash_file_call_count(|| {
        let _enter = handle.enter();
        fetch_current(&repo, &Selection::root(), None, &NoopProgress)
            .unwrap()
            .fetched
    });
    assert_eq!(fetched, 0);
    assert_eq!(hash_file_call_count(), 0);
}

#[test]
fn fetch_downloads_a_file_added_but_not_yet_git_added_or_committed() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    // deliberately no `git add`/commit: push/fetch both always use the
    // on-disk (unstaged) lock.

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap();
    assert!(cache_root(&repo).presence().contains(&oid));
}

#[test]
fn pull_fetches_and_checks_out() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add big.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();
    let mut cfg = repo.load_config().unwrap();
    cfg.sync.trust_state = Some(false);
    repo.save_config(&cfg).unwrap();

    std::fs::remove_file(tmp.path().join("big.bin")).unwrap();
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    pull_current(&repo, Selection::root(), None, &NoopProgress).unwrap();
    assert_eq!(
        std::fs::read(tmp.path().join("big.bin")).unwrap(),
        b"payload"
    );
}

#[test]
fn pull_never_rematerializes_an_already_correct_file_after_a_strategy_change() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    repo.save_config(&gat_core::config::Config {
        cache: gat_core::config::CacheConfig {
            materialization_strategy: Some("copy".parse().unwrap()),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();
    std::fs::write(tmp.path().join("a.bin"), b"hello").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add a.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();
    pull_current(&repo, Selection::root(), None, &NoopProgress).unwrap();

    // Only the materialization strategy changes; `gat pull` has no
    // `--rematerialize` option of its own, and `gat_command::pull` never
    // requests rematerialization during reconciliation, so the
    // already-correct file must be left exactly as it was.
    let mut cfg = repo
        .load_config_scoped(gat_core::config::ConfigScope::Project)
        .unwrap();
    cfg.cache.materialization_strategy = Some("symlink".parse().unwrap());
    repo.save_config_scoped(&cfg, gat_core::config::ConfigScope::Project)
        .unwrap();

    let outcome = pull_current(&repo, Selection::root(), None, &NoopProgress).unwrap();

    assert_eq!(outcome.outcome.rematerialized, 0);
    assert!(
        !std::fs::symlink_metadata(tmp.path().join("a.bin"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "gat pull must never rematerialize an already-correct file merely \
         because the strategy config changed"
    );
}

/// `pull`'s worktree-mutating sync phase
/// must reject -- via `StaleDesiredRevisionError`, through
/// `Operation::mutate` -- before touching the working tree/materialized
/// state, if the desired state (`gat.lock`) changed after `pull`'s
/// `DesiredOperation` (and so its desired-state selection/remote work) was
/// already captured. Simulates the race directly rather than with real
/// threads (already covered for raw lock contention by
/// `concurrent_sync_attempts_do_not_corrupt_state`): builds one
/// `DesiredOperation` (as the real `pull` entry point does), then lands a
/// second, independent `gat add` -- exactly the kind of concurrent
/// desired-state mutation another process could commit while this
/// operation's fetch/selection work was in flight -- before feeding that
/// now-stale operation into `gat_command::pull_with_desired_operation`.
#[test]
fn pull_rejects_a_stale_desired_revision_before_any_worktree_mutation() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add big.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::remove_file(tmp.path().join("big.bin")).unwrap();

    // Captures this operation's desired-state revision now, before the
    // concurrent mutation below.
    let desired_op = DesiredOperation::acquire(&repo, &NoopProgress).unwrap();

    // A second, independent `gat add` -- simulating another process
    // committing a desired-state change while `desired_op`'s owner was
    // still doing fetch/selection work.
    std::fs::write(tmp.path().join("second.bin"), b"more payload").unwrap();
    add(&repo, &[PathBuf::from("second.bin")], &NoopProgress).unwrap();

    let err = pull_selected(desired_op, Selection::root(), None, None, &NoopProgress).unwrap_err();
    assert!(matches!(
        err,
        gat_command::SyncError::Reconciliation(ref sync)
            if sync.kind()
                == &gat_engine::SyncErrorKind::MutationAuthority(
                    gat_engine::SyncMutationAuthorityFailureKind::Conflict
                )
    ));

    // The rejected mutation must never have touched the working tree:
    // `big.bin` (which the stale, would-be reconciliation would have
    // recreated) stays absent.
    assert!(!tmp.path().join("big.bin").exists());
}

/// `hook()` (the Git-hook-triggered
/// fetch+sync entry point) and repair+resync both reconcile through the
/// exact same `sync_with_operation` -> `sync_with_operation_impl` ->
/// `sync_from_snapshot` -> `Operation::mutate` path `pull` does (there is no
/// separate bespoke acquire/revalidate/sync sequence per composite command,
/// for hook-triggered sync --
/// so a stale desired revision must be rejected there too, before any
/// worktree mutation, exactly as
/// `pull_rejects_a_stale_desired_revision_before_any_worktree_mutation`
/// proves for `pull`. Drives `sync_with_operation` directly (the shared
/// function both `hook()` and repair+resync call into) with a
/// `DesiredOperation` whose desired-state revision has gone stale, exactly
/// as `hook()`/`pull()` build one before their own sync phase.
#[test]
fn hook_and_repair_resync_share_pulls_stale_desired_revision_rejection_before_any_worktree_mutation()
 {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add big.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::remove_file(tmp.path().join("big.bin")).unwrap();

    // Captures this operation's desired-state revision now, exactly as
    // `hook()`/repair+resync do before their own sync phase.
    let desired_op = DesiredOperation::acquire(&repo, &NoopProgress).unwrap();

    // A second, independent `gat add` -- simulating another process
    // committing a desired-state change while the hook/repair+resync
    // invocation's earlier work (fetch, or the repair pass itself) was
    // still in flight.
    std::fs::write(tmp.path().join("second.bin"), b"more payload").unwrap();
    add(&repo, &[PathBuf::from("second.bin")], &NoopProgress).unwrap();

    let mut operation = desired_op.finish_selection();
    let err = gat_command::sync_with_operation(
        &mut operation,
        gat_command::SyncRequest {
            selection: Some(Selection::root()),
            force: false,
            dry_run: false,
            trust_state: false,
            fetch: false,
            repair: false,
            remote: None,
            rematerialize: false,
        },
        0,
        false,
        false,
        &NoopProgress,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        gat_command::SyncError::Reconciliation(ref sync)
            if sync.kind()
                == &gat_engine::SyncErrorKind::MutationAuthority(
                    gat_engine::SyncMutationAuthorityFailureKind::Conflict
                )
    ));

    // The rejected mutation must never have touched the working tree:
    // `big.bin` (which the stale, would-be reconciliation would have
    // recreated) stays absent.
    assert!(!tmp.path().join("big.bin").exists());
}

/// One `gat pull` invocation -- its selection resolution,
/// `fetch_selected`, `sync_with_operation`, and any implicit repair that
/// sync triggers -- must share one coherent `Operation`/`Session`: exactly
/// one effective-config load for the
/// whole invocation, and at most one remote operator initialized per
/// effective remote actually used (never the configured-but-unused
/// second remote).
#[test]
fn pull_shares_one_coherent_context_across_fetch_sync_and_repair() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    repo.save_config(&gat_core::config::Config {
        cache: gat_core::config::CacheConfig {
            materialization_strategy: Some("copy".parse().unwrap()),
            ..Default::default()
        },
        sync: gat_core::config::SyncConfig {
            trust_state: Some(false),
            auto_repair: Some(true),
            ..Default::default()
        },
        ..Default::default()
    })
    .unwrap();

    std::fs::write(tmp.path().join("fetched.bin"), b"payload").unwrap();
    std::fs::write(tmp.path().join("a.bin"), b"hello").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"hello").unwrap();
    add(
        &repo,
        &[
            PathBuf::from("fetched.bin"),
            PathBuf::from("a.bin"),
            PathBuf::from("b.bin"),
        ],
        &NoopProgress,
    )
    .unwrap();
    commit_all(tmp.path(), "add fetched.bin, a.bin, and b.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    // A second, configured-but-never-used remote: nothing routes to
    // it and it isn't the default, so a well-behaved pull must never
    // initialize its operator.
    let unused_remote_dir = tempfile::tempdir().unwrap();
    let unused_url = gat_io::remote_file_url_for_test(unused_remote_dir.path());
    remote_add_with_default(&repo, "unused", unused_url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    // Force a genuine fetch: remove the local checkout and cache
    // object for `fetched.bin` only, leaving `a.bin`/`b.bin`'s cache
    // entries intact so they can still be corrupted below.
    let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
    let fetched_oid = lock
        .entries
        .iter()
        .find(|e| e.path == "fetched.bin")
        .unwrap()
        .oid;
    std::fs::remove_file(tmp.path().join("fetched.bin")).unwrap();
    let fetched_obj = cache_root(&repo).object_path_for_test(&fetched_oid);
    let _ = cache_root(&repo).make_object_writable_for_test(&fetched_oid);
    std::fs::remove_file(&fetched_obj).unwrap();

    // Force a genuine repair: corrupt the shared oid's cache object
    // and locally edit both paths referencing it.
    let oid = lock.entries.iter().find(|e| e.path == "a.bin").unwrap().oid;
    let cache_root = cache_root(&repo);
    let obj = cache_root.object_path_for_test(&oid);
    cache_root.make_object_writable_for_test(&oid).unwrap();
    std::fs::write(&obj, b"corrupted").unwrap();
    // Remove (rather than locally edit) the working copies so plan
    // has to materialize them from the cache and discovers the
    // corruption, instead of reporting a conflict against a local
    // edit (which `pull`, unlike `gat sync --force`, can never
    // override).
    std::fs::remove_file(tmp.path().join("a.bin")).unwrap();
    std::fs::remove_file(tmp.path().join("b.bin")).unwrap();

    let config_loads_before = gat_engine::test_support::config_loads();
    let remote_opens_before = gat_engine::test_support::remote_opens();
    let materialization_strategy_resolutions_before =
        gat_engine::test_support::materialization_strategy_resolutions();
    let cache_db_opens_before = gat_io::cache_proof_test_support::snapshot().cache_db_opens;
    pull_current(&repo, Selection::root(), None, &NoopProgress).unwrap();
    let cache_db_opens_after = gat_io::cache_proof_test_support::snapshot().cache_db_opens;

    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1,
        "one gat pull invocation (fetch, sync, and its repair pass) \
             must load effective config exactly once"
    );
    assert_eq!(
        gat_engine::test_support::remote_opens() - remote_opens_before,
        1,
        "fetching from and repairing against the same effective remote \
             must initialize its operator at most once, and the unused \
             second remote must never be initialized"
    );
    assert_eq!(
        gat_engine::test_support::materialization_strategy_resolutions()
            - materialization_strategy_resolutions_before,
        1,
        "one gat pull invocation (fetch, sync, and its repair pass, \
             including the post-repair reconciliation rerun) must resolve \
             the cache.materialization_strategy materialization mode exactly once"
    );
    // This one operation's fetch, repair, and
    // (post-repair) resync phases must all share exactly one proof-index
    // (`cache.sqlite3`) connection -- no phase directly constructs its
    // own `CacheClient`/`CacheState`.
    assert_eq!(
        cache_db_opens_after - cache_db_opens_before,
        1,
        "one gat pull invocation (fetch, sync, and its repair pass) must \
             open the proof database exactly once, sharing one CacheSession \
             across every phase instead of each phase opening its own"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("fetched.bin")).unwrap(),
        b"payload"
    );
    assert_eq!(std::fs::read(&obj).unwrap(), b"hello");
}

/// `gat pull`'s fetch phase
/// failing (a broken remote whose file URL root is a plain file, not a
/// directory) must never leave more than one task active, and the later
/// `Synchronizing`/`Repairing` phases must never begin once fetch has
/// already failed -- the `Fetching` task's owner must be dropped before
/// any subsequent phase could begin.
#[test]
fn pull_fetch_failure_never_advances_past_fetching() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add a.bin");

    let not_a_dir = tmp.path().join("not-a-directory");
    std::fs::write(&not_a_dir, b"not a directory").unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(&not_a_dir),
    )
    .unwrap();

    // Force a genuine fetch attempt: remove the local checkout and cache
    // object so `pull` can't just synchronize from what's already local.
    let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
    let oid = lock.entries[0].oid;
    std::fs::remove_file(tmp.path().join("a.bin")).unwrap();
    let obj = cache_root(&repo).object_path_for_test(&oid);
    let _ = cache_root(&repo).make_object_writable_for_test(&oid);
    std::fs::remove_file(&obj).unwrap();

    let progress = RecordingProgress::new();
    pull_current(&repo, Selection::root(), None, &progress).unwrap_err();

    assert_eq!(
        progress.max_active_tasks(),
        1,
        "a fetch failure must never leave more than one task active"
    );
    assert_eq!(
        progress.count_of(ProgressOperation::Synchronizing),
        0,
        "Synchronizing must never begin once the Fetching phase has already failed"
    );
    assert_eq!(
        progress.count_of(ProgressOperation::Repairing),
        0,
        "Repairing must never begin once the Fetching phase has already failed"
    );
}

#[test]
fn remote_named_add_list_and_push_fetch_by_name() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add big.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "backup", url).unwrap();
    let backup = RemoteName::from_string("backup".to_string());
    push(&repo, None, Some(&backup), &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    let backup = RemoteName::from_string("backup".to_string());
    fetch_current(&repo, &Selection::root(), Some(&backup), &NoopProgress).unwrap();
    assert!(cache_path(&repo).exists());
}

/// Route-consistent remote resolution: an empty selection
/// performs no path-based remote work, so `push` with a bare (no
/// explicit `--remote`) selection must succeed even with no
/// `remotes.default` configured -- unlike the old "always eagerly
/// resolve/open a remote, even for an empty selection" preflight.
#[test]
fn bare_push_with_an_empty_selection_and_no_configured_remote_succeeds() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    // Deliberately no `gat add` and no configured remote at all.

    let outcome = push(&repo, None, None, &NoopProgress).unwrap();
    assert_eq!(outcome.total, 0);
}

/// Route-consistent remote resolution: even though an empty
/// selection does not require `remotes.default`; an *explicitly*
/// supplied `--remote NAME` must still be validated -- a typo'd
/// override name is a caller mistake that should surface clearly, not
/// be silently accepted just because there was nothing to push.
#[test]
fn explicit_remote_push_with_an_empty_selection_still_validates_the_name() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    // Deliberately no `gat add` and no configured remote at all.

    let missing = RemoteName::from_string("does-not-exist".to_string());
    let err = push(&repo, None, Some(&missing), &NoopProgress).unwrap_err();
    assert!(err.to_string().contains("does-not-exist"));
}

/// Route-consistent remote resolution mirrors the push case:
/// an empty selection performs no path-based remote work, so `fetch`
/// with a bare (no explicit `--remote`) selection must succeed even
/// with no `remotes.default` configured.
#[test]
fn bare_fetch_with_an_empty_selection_and_no_configured_remote_succeeds() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    // Deliberately no `gat add` and no configured remote at all.

    let outcome = fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap();
    assert_eq!(outcome.fetched, 0);
}

/// Route-consistent remote resolution: even though an empty
/// selection does not require `remotes.default`; an *explicitly*
/// supplied `--remote NAME` must still be validated for `fetch`, same
/// as `push`.
#[test]
fn explicit_remote_fetch_with_an_empty_selection_still_validates_the_name() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    // Deliberately no `gat add` and no configured remote at all.

    let remote = RemoteName::from_string("does-not-exist".to_string());
    let err = fetch_current(&repo, &Selection::root(), Some(&remote), &NoopProgress).unwrap_err();
    assert!(err.to_string().contains("does-not-exist"));
}

#[test]
fn push_with_nothing_tracked_reports_zero_items() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    // Nothing has ever been `gat add`ed, so gat.lock has no entries.

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    push(&repo, None, None, &NoopProgress).unwrap();
    assert!(
        std::fs::read_dir(remote_dir.path())
            .unwrap()
            .next()
            .is_none(),
        "nothing is tracked, so nothing should be pushed"
    );
}

#[test]
fn push_skips_entries_missing_from_local_cache_without_erroring() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add big.bin");

    // Simulate a fresh clone: gat.lock is committed but the local
    // object cache is empty (nothing was ever `gat add`ed/fetched here).
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    std::fs::create_dir_all(cache_path(&repo)).unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    // Push must not error just because an object isn't cached locally.
    let progress = RecordingProgress::new();
    let outcome = push(&repo, None, None, &progress).unwrap();
    assert_eq!(outcome.total, 1);
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(outcome.skipped[0].path, "big.bin");
    assert_eq!(
        outcome.skipped[0].reason,
        gat_command::PushSkipReason::CacheMissing
    );
    // A missing-from-cache obligation still reaches a terminal outcome
    // (skipped) and must advance the `Pushing` position exactly once,
    // same as an uploaded or already-present obligation would.
    let task = progress.only(ProgressOperation::Pushing);
    assert_eq!(
        task.position, 1,
        "a skipped-missing obligation must still be counted as processed"
    );
    assert!(
        std::fs::read_dir(remote_dir.path())
            .unwrap()
            .next()
            .is_none(),
        "nothing should have been uploaded"
    );
}

/// An obligation that aborts on
/// a fatal error *before* reaching any terminal outcome (already-present,
/// uploaded, skipped-missing, skipped-corrupt) must never advance the
/// `Pushing` position. Routing the only obligation to a remote whose file
/// URL root is a plain file, not a directory, makes the remote-presence
/// check itself fail with a genuine `opendal` error -- not a graceful
/// per-object skip -- so the obligation never reaches a terminal outcome.
#[test]
fn push_fatal_presence_check_error_does_not_advance_the_pushing_position() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let not_a_dir = tmp.path().join("not-a-directory");
    std::fs::write(&not_a_dir, b"not a directory").unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(&not_a_dir),
    )
    .unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();

    let progress = RecordingProgress::new();
    push(&repo, None, None, &progress).unwrap_err();

    let task = progress.only(ProgressOperation::Pushing);
    assert_eq!(
        task.position, 0,
        "the sole obligation aborted on a fatal error before reaching a \
         terminal outcome and must not be counted as processed"
    );
}

#[test]
fn push_skips_corrupt_cached_objects_with_a_distinct_message() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    let cache_root = cache_root(&repo);
    let path = cache_root.object_path_for_test(&oid);
    cache_root.make_object_writable_for_test(&oid).unwrap();
    std::fs::write(&path, b"corrupted bytes").unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    let progress = RecordingProgress::new();
    let outcome = push(&repo, None, None, &progress).unwrap();
    assert_eq!(outcome.total, 1);
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(
        outcome.skipped[0].reason,
        gat_command::PushSkipReason::CacheCorrupt
    );
    // A corrupt-cache obligation still reaches a terminal outcome
    // (skipped) and must advance the `Pushing` position exactly once.
    let task = progress.only(ProgressOperation::Pushing);
    assert_eq!(
        task.position, 1,
        "a skipped-corrupt obligation must still be counted as processed"
    );
}

#[test]
fn broad_push_skips_mount_owned_paths_but_explicit_mount_root_push_is_allowed() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::create_dir_all(tmp.path().join("vendor/models")).unwrap();
    std::fs::write(tmp.path().join("root.bin"), b"shared-bytes").unwrap();
    std::fs::write(tmp.path().join("vendor/models/model.bin"), b"shared-bytes").unwrap();
    add(
        &repo,
        &[
            PathBuf::from("root.bin"),
            PathBuf::from("vendor/models/model.bin"),
        ],
        &NoopProgress,
    )
    .unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();
    let mut cfg = repo.load_config().unwrap();
    cfg.mounts.by_name.insert(
        gat_core::name::MountName::from_string("models".to_string()),
        mount_config("vendor/models"),
    );
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("vendor/models".to_string()),
        RouteConfig {
            path: gp("vendor/models"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    let broad = push(&repo, None, None, &NoopProgress).unwrap();
    assert_eq!(broad.total, 1);
    assert_eq!(broad.skipped.len(), 1);
    assert_eq!(broad.skipped[0].path, "vendor/models/model.bin");
    assert_eq!(
        broad.skipped[0].reason,
        gat_command::PushSkipReason::MountOwned {
            owner_name: "models".into(),
            owner_target: gp("vendor/models"),
        }
    );
    assert!(
        origin_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists(),
        "the root-owned duplicate oid should still be pushed"
    );
    assert!(
        !backup_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists(),
        "the mount-owned path should be skipped by the broad push"
    );

    // With no configured path, this include is relative to the repository root,
    // not explicit scope intent that permits publishing mount-owned objects.
    cfg.selections.default = Some("runtime".into());
    cfg.selections
        .by_name
        .entry("runtime".into())
        .or_default()
        .include = Some(vec![
        gat_core::globs::GatGlobPattern::parse("vendor/models/**").unwrap(),
    ]);
    repo.save_config(&cfg).unwrap();
    let configured = gat_command::push(
        &repo,
        gat_command::PushRequest {
            selection: None,
            remote: None,
            source: gat_command::PushSource::Current,
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(configured.total, 0);
    assert_eq!(configured.skipped.len(), 1);
    assert!(matches!(
        configured.skipped[0].reason,
        gat_command::PushSkipReason::MountOwned { .. }
    ));
    assert!(
        !backup_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists()
    );

    let selection = scoped_selection(Path::new("vendor/models"));
    let explicit = push(&repo, Some(&selection), None, &NoopProgress).unwrap();
    assert_eq!(explicit.total, 1);
    assert!(explicit.skipped.is_empty());
    assert!(
        backup_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists(),
        "an explicit mount-root selection should be allowed to seed its routed remote"
    );
}

#[test]
fn push_uploads_files_added_but_not_yet_git_added_or_committed() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
    // deliberately no `git add`/commit of gat.lock: push always uses
    // the on-disk (unstaged) lock, so `gat add` alone is enough.

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    push(&repo, None, None, &NoopProgress).unwrap();
    assert!(
        std::fs::read_dir(remote_dir.path())
            .unwrap()
            .next()
            .is_some(),
        "the object added but not yet git added/committed should still be pushed"
    );
}

#[test]
fn push_uses_verified_cache_state_without_rehashing_when_remote_is_missing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    std::fs::remove_file(remote_dir.path().join(storage::object_key_oid(&oid))).unwrap();
    let handle = rt.handle().clone();

    // The first push established a persisted cache proof. Removing only the
    // remote object makes this second push need the local bytes again while
    // proving it can reuse that verification across operation boundaries.
    let (outcome, hashes) = with_exclusive_hash_file_call_count(|| {
        let _enter = handle.enter();
        let outcome = push(&repo, None, None, &NoopProgress).unwrap();
        (outcome, hash_file_call_count())
    });
    assert_eq!(outcome.total, 1);
    assert!(outcome.skipped.is_empty());
    assert_eq!(hashes, 0);
}

#[test]
fn push_batches_cache_proof_work_for_remote_missing_objects() {
    use gat_engine::ExecutionLimits;
    use gat_io::cache_proof_test_support as test_support;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let count = 96;
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let path = format!("object-{index:03}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{index}")).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();
    let _ = std::fs::remove_file(cache_path(&repo).join("cache.sqlite3"));

    let before = test_support::snapshot();
    let limits = ExecutionLimits::for_test(count, 10_000, 4096, count, count);
    let operation = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = push_selected(operation, None, &NoopProgress).unwrap();
    let after = test_support::snapshot();

    assert_eq!(outcome.total, count);
    assert!(outcome.skipped.is_empty());
    assert!(
        after.proof_lookup_requests - before.proof_lookup_requests < count,
        "publish must prepare set-based verification batches, not one proof lookup per OID"
    );
    assert!(
        after.proof_mutation_transactions - before.proof_mutation_transactions < count,
        "publish must commit proof deltas in batches, not one transaction per OID"
    );
}

#[test]
fn push_uploads_one_oid_to_two_routed_remotes_and_hashes_it_once() {
    use gat_io::cache_proof_test_support as test_support;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"shared-bytes").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"shared-bytes").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();
    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("b.bin".to_string()),
        RouteConfig {
            path: gp("b.bin"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    let _ = std::fs::remove_file(cache_path(&repo).join("cache.sqlite3"));
    let handle = rt.handle().clone();
    let (outcome, fs_verifications) = with_exclusive_hash_file_call_count(|| {
        let _enter = handle.enter();
        let before = test_support::snapshot();
        let outcome = push(&repo, None, None, &NoopProgress).unwrap();
        let after = test_support::snapshot();
        (outcome, after.fs_verifications - before.fs_verifications)
    });
    assert_eq!(outcome.total, 1);
    assert!(outcome.skipped.is_empty());
    assert_eq!(fs_verifications, 1);
    assert!(
        origin_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists()
    );
    assert!(
        backup_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists()
    );
}

/// The `Pushing` task's position counts
/// *processed* publication obligations -- one per (remote, oid) pair,
/// reaching any terminal outcome (already-present, uploaded, or
/// skipped) -- not unique objects, so a single oid fanned out to two
/// routed remotes must advance the position twice even though
/// `PushOutcome.total` (deduplicated by oid) reports only one. The unit
/// must therefore be `Entries`, not `Objects`, since "objects" would
/// misleadingly imply a 1:1 correspondence with `total` that this
/// position does not have.
#[test]
fn push_fanned_out_oid_advances_the_pushing_position_once_per_remote_obligation() {
    use gat_io::cache_proof_test_support as test_support;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"shared-bytes").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"shared-bytes").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();
    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("b.bin".to_string()),
        RouteConfig {
            path: gp("b.bin"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();
    let _ = std::fs::remove_file(cache_path(&repo).join("cache.sqlite3"));

    let progress = RecordingProgress::new();
    let _ = test_support::snapshot();
    let outcome = push(&repo, None, None, &progress).unwrap();

    assert_eq!(outcome.total, 1, "one unique oid, deduplicated");
    let task = progress.only(ProgressOperation::Pushing);
    assert_eq!(task.unit, Some(gat_core::progress::ProgressUnit::Entries));
    assert_eq!(
        task.position, 2,
        "two remote publication obligations for the one fanned-out oid"
    );
}

/// Push must only open a remote operator for
/// a configured remote that at least one selected object actually routes
/// to -- a third configured remote with no route/default obligation
/// pointing at it must never be initialized, exactly like repair's
/// analogous laziness guarantee
/// ([`repair_corrupted_opens_the_shared_remote_once_for_several_distinct_oids_and_never_opens_an_unused_remote`]).
#[test]
fn push_never_opens_a_configured_remote_that_no_object_routes_to() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();

    let origin_dir = tempfile::tempdir().unwrap();
    let unused_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "unused",
        gat_io::remote_file_url_for_test(unused_dir.path()),
    )
    .unwrap();

    let origin_opens_before = gat_engine::test_support::remote_open_count_for("origin");
    let unused_opens_before = gat_engine::test_support::remote_open_count_for("unused");

    push(&repo, None, None, &NoopProgress).unwrap();

    assert_eq!(
        gat_engine::test_support::remote_open_count_for("origin") - origin_opens_before,
        1,
        "the default `origin` remote must be opened exactly once for the one pushed object"
    );
    assert_eq!(
        gat_engine::test_support::remote_open_count_for("unused") - unused_opens_before,
        0,
        "a configured remote with no routed/default obligation must never be opened"
    );
}

#[test]
fn push_does_not_verify_local_bytes_when_remote_already_has_the_oid() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    let cache_root = cache_root(&repo);
    let path = cache_root.object_path_for_test(&oid);
    cache_root.make_object_writable_for_test(&oid).unwrap();
    std::fs::write(&path, b"corrupted bytes").unwrap();

    let outcome = push(&repo, None, None, &NoopProgress).unwrap();
    assert_eq!(outcome.total, 1);
    assert!(outcome.skipped.is_empty());
}

/// The all-remote-present fast path: when every selected oid is already
/// on the remote, `push` uploads nothing and must therefore open no
/// ordinary proof DB and hash no local bytes merely to "verify" objects
/// it won't send. Asserted structurally via the cache proof
/// instrumentation, not wall-clock timing.
#[test]
fn push_with_everything_already_remote_opens_no_proof_db_and_hashes_nothing() {
    use gat_io::cache_proof_test_support as test_support;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    let handle = rt.handle().clone();
    // Prime the remote so the second push sees everything present.
    push(&repo, None, None, &NoopProgress).unwrap();

    let (opens, hashes) = with_exclusive_hash_file_call_count(|| {
        let _enter = handle.enter();
        let before = test_support::snapshot();
        push(&repo, None, None, &NoopProgress).unwrap();
        let after = test_support::snapshot();
        (
            after.cache_db_opens - before.cache_db_opens,
            hash_file_call_count(),
        )
    });
    assert_eq!(opens, 0, "all-remote-present push must open no proof DB");
    assert_eq!(hashes, 0, "all-remote-present push must not hash");
}

#[test]
fn push_uploads_the_latest_unstaged_content_even_after_staging_older_content() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("a.bin"), b"a-content").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    stage_all(tmp.path()); // a.bin staged

    // Re-`gat add` with new content after staging: gat.lock on disk now
    // has a newer oid for a.bin than what's staged.
    std::fs::write(tmp.path().join("a.bin"), b"a-content-v2").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    let newest_oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    push(&repo, None, None, &NoopProgress).unwrap();
    let uploaded = remote_dir.path().join(storage::object_key_oid(&newest_oid));
    assert!(
        uploaded.exists(),
        "the newest unstaged content should have been pushed, not the older staged one"
    );
}

#[test]
fn historical_push_selects_the_old_object_not_current_state() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-a").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version a");
    let oid_a = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    std::fs::write(tmp.path().join("a.bin"), b"version-b-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version b");
    let oid_b = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    assert_ne!(oid_a, oid_b);

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    let selection = HistorySelection {
        roots: vec![HistoryRoot::Revision("HEAD~1".to_string().into())],
        ..Default::default()
    };
    let outcome =
        push_with_history(&repo, &Selection::root(), None, &selection, &NoopProgress).unwrap();
    assert_eq!(outcome.total, 1);
    assert!(outcome.skipped.is_empty());

    assert!(
        remote_dir
            .path()
            .join(storage::object_key_oid(&oid_a))
            .exists(),
        "the historically selected (older) object should be on the remote"
    );
    assert!(
        !remote_dir
            .path()
            .join(storage::object_key_oid(&oid_b))
            .exists(),
        "current state should not be pushed by an explicit historical selection"
    );
}

#[test]
fn duplicate_oids_are_pushed_only_once() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"same-bytes").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"same-bytes").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    let outcome = push(&repo, None, None, &NoopProgress).unwrap();
    assert_eq!(outcome.total, 1);
    assert!(outcome.skipped.is_empty());
}

#[test]
fn historical_fetch_selects_the_old_object_not_current_state() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-a").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version a");
    let oid_a = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    // Push once per commit, since push always uploads whatever's
    // currently the desired content, so the remote ends up with both.
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-b-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version b");
    let oid_b = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();

    let selection = HistorySelection {
        roots: vec![HistoryRoot::Revision("HEAD~1".to_string().into())],
        ..Default::default()
    };
    let outcome =
        fetch_history(&repo, &Selection::root(), None, &selection, &NoopProgress).unwrap();
    assert_eq!(outcome.fetched, 1);
    assert!(!outcome.shallow);
    assert!(cache_root(&repo).presence().contains(&oid_a));
    assert!(!cache_root(&repo).presence().contains(&oid_b));
}

#[test]
fn fetch_uses_the_first_selected_paths_route_for_duplicate_oids() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"shared-bytes").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"shared-bytes").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();
    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("b.bin".to_string()),
        RouteConfig {
            path: gp("b.bin"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    let origin = RemoteName::from_string("origin".to_string());
    push(&repo, None, Some(&origin), &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    let outcome = fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap();
    assert_eq!(outcome.fetched, 1);
    assert!(cache_root(&repo).presence().contains(&oid));
    assert!(
        !backup_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists(),
        "the later routed path must not trigger a second remote fetch or fallback"
    );
}

#[test]
fn fetch_does_not_fall_back_to_a_later_paths_remote_when_the_first_route_is_missing() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"shared-bytes").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"shared-bytes").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();
    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("a.bin".to_string()),
        RouteConfig {
            path: gp("a.bin"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    let origin = RemoteName::from_string("origin".to_string());
    push(&repo, None, Some(&origin), &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    let err = fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("remote `backup` via route `a.bin`"));
    assert!(message.contains("selected by `a.bin`"));
}

#[test]
fn fetch_leaves_a_corrupt_local_object_in_place_until_the_download_is_verified() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    let cache_root = cache_root(&repo);
    let path = cache_root.object_path_for_test(&oid);
    cache_root.make_object_writable_for_test(&oid).unwrap();
    std::fs::write(&path, b"corrupt-old!").unwrap();

    let remote_dir = tempfile::tempdir().unwrap();
    let key = storage::object_key_oid(&oid);
    let remote_path = remote_dir.path().join(&key);
    std::fs::create_dir_all(remote_path.parent().unwrap()).unwrap();
    std::fs::write(&remote_path, b"wrong-remote-bytes").unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    let err = fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap_err();
    assert!(err.to_string().contains("fetched object hash mismatch"));
    assert_eq!(std::fs::read(&path).unwrap(), b"corrupt-old!");
}

#[test]
fn pull_fetches_mount_owned_paths_from_their_routed_remote() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::create_dir_all(tmp.path().join("vendor/models")).unwrap();
    std::fs::write(tmp.path().join("vendor/models/model.bin"), b"payload").unwrap();
    add(
        &repo,
        &[PathBuf::from("vendor/models/model.bin")],
        &NoopProgress,
    )
    .unwrap();
    commit_all(tmp.path(), "add mount-owned file");

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();
    let mut cfg = repo.load_config().unwrap();
    cfg.mounts.by_name.insert(
        gat_core::name::MountName::from_string("models".to_string()),
        mount_config("vendor/models"),
    );
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("vendor/models".to_string()),
        RouteConfig {
            path: gp("vendor/models"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    cfg.sync.trust_state = Some(false);
    repo.save_config(&cfg).unwrap();

    let selection = scoped_selection(Path::new("vendor/models"));
    push(&repo, Some(&selection), None, &NoopProgress).unwrap();

    std::fs::remove_file(tmp.path().join("vendor/models/model.bin")).unwrap();
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    pull_current(&repo, Selection::root(), None, &NoopProgress).unwrap();
    assert_eq!(
        std::fs::read(tmp.path().join("vendor/models/model.bin")).unwrap(),
        b"payload"
    );
}

#[test]
fn history_aware_pull_still_materializes_current_desired_state() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-a").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version a");
    let oid_a = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-b-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version b");
    let oid_b = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    push(&repo, None, None, &NoopProgress).unwrap();

    let mut cfg = repo.load_config().unwrap();
    cfg.sync.trust_state = Some(false);
    repo.save_config(&cfg).unwrap();

    std::fs::remove_file(tmp.path().join("a.bin")).unwrap();
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();

    let selection = HistorySelection {
        roots: vec![HistoryRoot::Revision("HEAD~1".to_string().into())],
        ..Default::default()
    };
    let desired_op = DesiredOperation::acquire(&repo, &NoopProgress).unwrap();
    let outcome = pull_selected(
        desired_op,
        Selection::root(),
        None,
        Some(selection),
        &NoopProgress,
    )
    .unwrap();
    assert!(outcome.outcome.is_clean());

    // The historically selected object was prefetched...
    assert!(cache_root(&repo).presence().contains(&oid_a));
    // ...as was current state's object, via the mandatory union...
    assert!(cache_root(&repo).presence().contains(&oid_b));
    // ...but only current state (B) was materialized, never A.
    assert_eq!(
        std::fs::read(tmp.path().join("a.bin")).unwrap(),
        b"version-b-longer"
    );
}

#[test]
fn path_scope_restricts_historical_push_before_oid_dedup() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::create_dir_all(tmp.path().join("dir")).unwrap();
    std::fs::write(tmp.path().join("a.bin"), b"a-content").unwrap();
    std::fs::write(tmp.path().join("dir/b.bin"), b"b-content").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("dir/b.bin")],
        &NoopProgress,
    )
    .unwrap();
    commit_all(tmp.path(), "two tracked paths");
    let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries;
    let oid_a = entries.iter().find(|e| e.path == "a.bin").unwrap().oid;
    let oid_b = entries.iter().find(|e| e.path == "dir/b.bin").unwrap().oid;

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    let selection = HistorySelection {
        roots: vec![HistoryRoot::Revision("HEAD".to_string().into())],
        ..Default::default()
    };
    let outcome = push_with_history(
        &repo,
        &scoped_selection(Path::new("dir")),
        None,
        &selection,
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(outcome.total, 1);

    assert!(
        remote_dir
            .path()
            .join(storage::object_key_oid(&oid_b))
            .exists(),
        "the in-scope historical object should be pushed"
    );
    assert!(
        !remote_dir
            .path()
            .join(storage::object_key_oid(&oid_a))
            .exists(),
        "the out-of-scope historical object should not be pushed"
    );
}

/// `repair_corrupted` must deduplicate a corrupted oid across the
/// *entire* operation, not just within one `repair_window()` -- so an oid
/// referenced at both the very first and very last position of a
/// larger-than-one-window corrupted list is still repaired exactly
/// once.
#[test]
fn repair_corrupted_dedups_the_same_oid_across_a_repair_window_boundary() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    std::fs::write(tmp.path().join("shared.bin"), b"shared").unwrap();
    add(&repo, &[PathBuf::from("shared.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add shared.bin");
    push(&repo, None, None, &NoopProgress).unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let mut operation =
        gat_engine::acquire_operation_without_desired_state(&repo, &NoopProgress).unwrap();

    // The shared oid at index 0 and again past index `repair_window()`,
    // with `repair_window()` distinct filler oids (which don't exist on
    // the remote, so they fail -- irrelevant to what this test proves)
    // in between, so the two shared-oid entries land in different
    // windows.
    let mut corrupted: Vec<(gat_core::lexical_path::GatPath, gat_core::oid::Oid)> =
        vec![(gp("shared.bin"), oid)];
    for i in 0..repair_window() {
        corrupted.push((
            gp(&format!("filler-{i}.bin")),
            gat_core::oid::Oid::from_hex(&format!("{i:064x}")).unwrap(),
        ));
    }
    corrupted.push((gp("shared-again.bin"), oid));

    let repair_calls_before = gat_command::repair_test_support::repair_oid_calls();
    let repair_task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Repairing));
    let outcome = repair_corrupted(&mut operation, None, &corrupted, &repair_task.handle());

    assert_eq!(
        gat_command::repair_test_support::repair_oid_calls() - repair_calls_before,
        repair_window() + 1,
        "the shared oid must be repaired exactly once even though its two              referencing paths land in different repair_window()-sized windows"
    );
    assert_eq!(
        outcome.repaired, 2,
        "both paths sharing the deduplicated oid must be reported as repaired"
    );
    // Repair is deliberately best-effort, not fail-fast:
    // every filler oid fails (it references no real object), yet the last
    // filler lands past the first repair_window() boundary and the
    // `repair_oid_calls` count above already proves it was still attempted.
    // Confirm the earlier window's many failures are recorded rather than
    // aborting the whole repair.
    assert_eq!(
        outcome.failures.len(),
        repair_window(),
        "every filler oid must be reported as a failure, proving an earlier \
             window's failures don't stop a later window's oids from being attempted"
    );
    assert!(
        gat_command::repair_test_support::repair_window_high_water() <= repair_window(),
        "repair's transient in-flight work per window must never exceed the \
             declared repair_window() bound"
    );
}

#[test]
fn repair_continues_after_missing_and_mismatched_objects_with_one_transfer_slot() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    for window in [1, 4] {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let remote_dir = tempfile::tempdir().unwrap();
        remote_add_with_default(
            &repo,
            "origin",
            gat_io::remote_file_url_for_test(remote_dir.path()),
        )
        .unwrap();

        let hash = |bytes: &[u8]| gat_core::oid::Oid::from_bytes(*blake3::hash(bytes).as_bytes());
        let wrong_bytes = vec![41; 2 * gat_io::TRANSFER_CHUNK_SIZE + 17];
        let tiny_bytes = b"valid tiny object".to_vec();
        let large_bytes = vec![42; 3 * gat_io::TRANSFER_CHUNK_SIZE + 17];
        let oids = [
            hash(b"missing"),
            hash(b"expected replacement"),
            hash(&tiny_bytes),
            hash(&large_bytes),
        ];
        for (oid, bytes) in [
            (oids[1], wrong_bytes.as_slice()),
            (oids[2], tiny_bytes.as_slice()),
            (oids[3], large_bytes.as_slice()),
        ] {
            let path = remote_dir.path().join(gat_io::object_key_oid(&oid));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let corrupted = ["missing.bin", "mismatch.bin", "tiny.bin", "large.bin"]
            .into_iter()
            .zip(oids)
            .map(|(path, oid)| (gp(path), oid))
            .collect::<Vec<_>>();
        // One slot forces failures to finish before the valid objects start.
        // Exercise both one shared window and four successive windows.
        let limits = ExecutionLimits::for_test(window, 10_000, 4096, 1, 1);
        let mut desired =
            DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
        let progress = RecordingProgress::new();
        let task = progress.begin(ProgressSpec::items(
            ProgressOperation::Repairing,
            gat_core::progress::ProgressUnit::Entries,
            Some(4),
        ));
        let (operation, _) = desired.split_for_selection();
        let outcome = repair_corrupted(operation, None, &corrupted, &task.handle());
        drop(task);
        assert_eq!(outcome.repaired, 2);
        assert_eq!(outcome.failures.len(), 2);
        for (failure, (path, oid)) in outcome.failures.iter().zip(&corrupted[..2]) {
            assert_eq!(&failure.path, path);
            assert_eq!(&failure.oid, oid);
        }
        assert!(matches!(
            outcome.failures[0].error.as_ref(),
            gat_command::RepairError::DataPlane(gat_engine::RepairError::RemoteRead {
                kind: gat_engine::RepairRemoteFailureKind::NotFound,
                ..
            })
        ));
        assert!(matches!(
            outcome.failures[1].error.as_ref(),
            gat_command::RepairError::DataPlane(gat_engine::RepairError::HashMismatch {
                expected, actual, ..
            }) if *expected == oids[1] && *actual == hash(&wrong_bytes)
        ));
        assert_eq!(progress.only(ProgressOperation::Repairing).position, 4);
        let root = cache_root(&repo);
        let cache = root.open_client();
        for oid in [oids[0], oids[1], hash(&wrong_bytes)] {
            assert_eq!(
                cache.verify(&oid).unwrap(),
                gat_io::ObjectVerification::Missing
            );
        }
        for oid in &oids[2..] {
            assert_eq!(
                cache.verify(oid).unwrap(),
                gat_io::ObjectVerification::Valid
            );
        }
    }
}

/// Repair's progress position counts
/// attempted `(path, oid)` entries, not unique objects -- the same shared
/// oid attempted from two different paths must advance the position
/// twice, matching `corrupted.len()` exactly, since the `Entries` unit
/// (not `Objects`) is what the position actually represents.
#[test]
fn repair_corrupted_position_equals_attempted_entries_not_unique_oids() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    std::fs::write(tmp.path().join("shared.bin"), b"shared").unwrap();
    add(&repo, &[PathBuf::from("shared.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add shared.bin");
    push(&repo, None, None, &NoopProgress).unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let mut operation =
        gat_engine::acquire_operation_without_desired_state(&repo, &NoopProgress).unwrap();

    let corrupted: Vec<(gat_core::lexical_path::GatPath, gat_core::oid::Oid)> =
        vec![(gp("shared.bin"), oid), (gp("shared-again.bin"), oid)];

    let progress = RecordingProgress::new();
    let repair_task = progress.begin(ProgressSpec::items(
        ProgressOperation::Repairing,
        gat_core::progress::ProgressUnit::Entries,
        Some(corrupted.len() as u64),
    ));
    let outcome = repair_corrupted(&mut operation, None, &corrupted, &repair_task.handle());
    drop(repair_task);

    assert_eq!(
        outcome.repaired, 2,
        "both paths sharing the deduplicated oid are repaired"
    );
    let task = progress.only(ProgressOperation::Repairing);
    assert_eq!(
        task.position,
        corrupted.len() as u64,
        "position must count attempted (path, oid) entries, not the one unique oid"
    );
}

#[test]
fn repair_route_failures_do_not_open_the_cache() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let mut operation =
        gat_engine::acquire_operation_without_desired_state(&repo, &NoopProgress).unwrap();
    let corrupted = vec![(gp("unrouted.bin"), gat_core::oid::Oid::from_bytes([7; 32]))];
    let cache_db_opens_before = gat_io::cache_proof_test_support::snapshot().cache_db_opens;
    let repair_task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Repairing));

    let outcome = repair_corrupted(&mut operation, None, &corrupted, &repair_task.handle());

    assert_eq!(outcome.repaired, 0);
    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(
        gat_io::cache_proof_test_support::snapshot().cache_db_opens,
        cache_db_opens_before
    );
}

/// Repairing several *distinct* corrupted oids that all resolve
/// to the same effective remote must initialize that remote's
/// operator exactly once (`RemoteSession` reuse, not once per oid), and a
/// second, configured-but-never-routed-to remote must never be
/// initialized at all.
#[test]
fn repair_corrupted_opens_the_shared_remote_once_for_several_distinct_oids_and_never_opens_an_unused_remote()
 {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    let unused_remote_dir = tempfile::tempdir().unwrap();
    let unused_url = gat_io::remote_file_url_for_test(unused_remote_dir.path());
    remote_add_with_default(&repo, "unused", unused_url).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"a-content").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"b-content").unwrap();
    std::fs::write(tmp.path().join("c.bin"), b"c-content").unwrap();
    add(
        &repo,
        &[
            PathBuf::from("a.bin"),
            PathBuf::from("b.bin"),
            PathBuf::from("c.bin"),
        ],
        &NoopProgress,
    )
    .unwrap();
    commit_all(tmp.path(), "add a.bin, b.bin, and c.bin");
    push(&repo, None, None, &NoopProgress).unwrap();

    let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
    let mut corrupted: Vec<(gat_core::lexical_path::GatPath, gat_core::oid::Oid)> = Vec::new();
    for path in ["a.bin", "b.bin", "c.bin"] {
        let oid = lock.entries.iter().find(|e| e.path == path).unwrap().oid;
        let cache_root = cache_root(&repo);
        let obj = cache_root.object_path_for_test(&oid);
        cache_root.make_object_writable_for_test(&oid).unwrap();
        std::fs::write(&obj, b"corrupted").unwrap();
        corrupted.push((gp(path), oid));
    }

    let mut operation =
        gat_engine::acquire_operation_without_desired_state(&repo, &NoopProgress).unwrap();
    let remote_opens_before = gat_engine::test_support::remote_opens();
    let repair_task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Repairing));
    let outcome = repair_corrupted(&mut operation, None, &corrupted, &repair_task.handle());

    assert_eq!(
        outcome.repaired, 3,
        "all three distinct corrupted objects should repair"
    );
    assert!(outcome.failures.is_empty());
    assert_eq!(
        gat_engine::test_support::remote_opens() - remote_opens_before,
        1,
        "three distinct corrupted oids routed to the same remote must \
             initialize that remote's operator exactly once, and the unused \
             second remote must never be opened"
    );
}

/// When two paths sharing the same corrupted oid resolve to
/// *different* effective remotes (one routed, one not), the
/// first-occurrence-in-`corrupted`-order rule must pick the
/// representative's remote deterministically -- and that choice must
/// not depend on which side of a `repair_window()` boundary either path
/// happens to land on.
#[test]
fn repair_corrupted_resolves_the_same_oid_different_route_representative_deterministically_across_a_window_boundary()
 {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let default_remote_dir = tempfile::tempdir().unwrap();
    let default_url = gat_io::remote_file_url_for_test(default_remote_dir.path());
    remote_add_with_default(&repo, "origin", default_url).unwrap();
    let routed_remote_dir = tempfile::tempdir().unwrap();
    let routed_url = gat_io::remote_file_url_for_test(routed_remote_dir.path());
    remote_add_with_default(&repo, "alt", routed_url).unwrap();

    // "routed.bin" is explicitly routed to "alt"; "plain.bin" shares
    // its oid with "routed.bin" but is left unrouted -- it still gets
    // its own route resolved from the operation snapshot below (see
    // the `remote_route_resolutions` assertions), even though only
    // the first-occurrence representative's resolution is the one
    // actually selects the download source for the shared oid.
    // Both need `origin` (the current default) reachable to push, so
    // the default is only cleared below, once pushing is done.
    route_add(&repo, "routed.bin", "alt", "routed.bin").unwrap();

    std::fs::write(tmp.path().join("routed.bin"), b"shared").unwrap();
    std::fs::write(tmp.path().join("plain.bin"), b"shared").unwrap();
    add(
        &repo,
        &[PathBuf::from("routed.bin"), PathBuf::from("plain.bin")],
        &NoopProgress,
    )
    .unwrap();
    commit_all(tmp.path(), "add routed.bin and plain.bin");
    push(&repo, None, None, &NoopProgress).unwrap();

    // Clear the repository default remote now that pushing is done:
    // every filler path below is unrouted, so with no default to fall
    // back to it fails to resolve *without* opening any remote
    // operator, keeping the `origin`/`alt` open counts asserted below
    // attributable solely to the one shared oid this test is about.
    {
        let mut cfg = repo
            .load_config_scoped(gat_core::config::ConfigScope::Project)
            .unwrap();
        cfg.remotes.default = None;
        repo.save_config_scoped(&cfg, gat_core::config::ConfigScope::Project)
            .unwrap();
    }

    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    // First variant: the `alt`-routed path is the first occurrence in
    // `corrupted`'s order, with more than `repair_window()` filler oids
    // (which don't exist on either remote, so they simply fail --
    // irrelevant to what this test proves) separating it from the
    // default-routed path, so the two land in different windows.
    let mut operation =
        gat_engine::acquire_operation_without_desired_state(&repo, &NoopProgress).unwrap();
    let mut corrupted = vec![(gp("routed.bin"), oid)];
    for i in 0..repair_window() {
        corrupted.push((
            gp(&format!("filler-{i}.bin")),
            gat_core::oid::Oid::from_hex(&format!("{i:064x}")).unwrap(),
        ));
    }
    corrupted.push((gp("plain.bin"), oid));

    let route_resolutions_before = gat_engine::test_support::remote_route_resolutions();
    let remote_opens_before = gat_engine::test_support::remote_opens();
    let alt_opens_before = gat_engine::test_support::remote_open_count_for("alt");
    let origin_opens_before = gat_engine::test_support::remote_open_count_for("origin");
    let corrupted_len = corrupted.len();
    let repair_task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Repairing));
    let outcome = repair_corrupted(&mut operation, None, &corrupted, &repair_task.handle());

    assert_eq!(
        outcome.repaired, 2,
        "both paths sharing the deduplicated oid must be reported as repaired"
    );
    assert_eq!(
        gat_engine::test_support::remote_route_resolutions() - route_resolutions_before,
        corrupted_len,
        "the effective remote must be resolved from the operation \
             snapshot for every `(path, oid)` entry in `corrupted` -- \
             including `plain.bin`, which shares an already-seen oid with \
             the first-occurrence `routed.bin` representative -- not just \
             the retained representative per unique oid"
    );
    assert_eq!(
        gat_engine::test_support::remote_open_count_for("alt") - alt_opens_before,
        1,
        "the first-occurrence path routes to `alt`, so it -- not the \
             default `origin` remote -- must be opened to repair the \
             shared oid"
    );
    assert_eq!(
        gat_engine::test_support::remote_open_count_for("origin") - origin_opens_before,
        0,
        "the default `origin` remote must never be opened when the \
             first-occurrence representative routes to `alt`"
    );
    assert_eq!(
        gat_engine::test_support::remote_opens() - remote_opens_before,
        1,
        "only one remote operator (the first occurrence's resolved \
             remote) may be opened for the shared oid, regardless of how \
             many differently routed paths reference it"
    );

    // Second variant: same paths, same oid, but with the default-
    // routed path now first and the alt-routed path pushed past the
    // window boundary instead -- the resolved remote must flip to
    // `origin` accordingly, proving the rule tracks `corrupted`'s own
    // order rather than being pinned to one particular remote.
    route_add(&repo, "plain.bin", "origin", "plain.bin").unwrap();
    let cache_root = cache_root(&repo);
    let obj = cache_root.object_path_for_test(&oid);
    cache_root.make_object_writable_for_test(&oid).unwrap();
    std::fs::write(&obj, b"corrupted").unwrap();
    let mut operation2 =
        gat_engine::acquire_operation_without_desired_state(&repo, &NoopProgress).unwrap();
    let mut corrupted2 = vec![(gp("plain.bin"), oid)];
    for i in 0..repair_window() {
        corrupted2.push((
            gp(&format!("filler2-{i}.bin")),
            gat_core::oid::Oid::from_hex(&format!("{i:064x}")).unwrap(),
        ));
    }
    corrupted2.push((gp("routed.bin"), oid));

    let alt_opens_before_2 = gat_engine::test_support::remote_open_count_for("alt");
    let origin_opens_before_2 = gat_engine::test_support::remote_open_count_for("origin");
    let route_resolutions_before_2 = gat_engine::test_support::remote_route_resolutions();
    let second_corrupted_len = corrupted2.len();
    let repair_task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Repairing));
    let outcome2 = repair_corrupted(&mut operation2, None, &corrupted2, &repair_task.handle());
    assert_eq!(outcome2.repaired, 2);
    assert_eq!(
        gat_engine::test_support::remote_route_resolutions() - route_resolutions_before_2,
        second_corrupted_len,
        "every `(path, oid)` entry -- including `routed.bin`, which \
             now shares an already-seen oid with the first-occurrence \
             `plain.bin` representative -- must still have its effective \
             remote resolved from the operation snapshot, regardless of \
             which path happens to be the retained representative"
    );
    assert_eq!(
        gat_engine::test_support::remote_open_count_for("origin") - origin_opens_before_2,
        1,
        "with plain.bin (routed to `origin`) as the first occurrence \
             this time, `origin` -- not `alt` -- must be the remote \
             opened to repair the shared oid"
    );
    assert_eq!(
        gat_engine::test_support::remote_open_count_for("alt") - alt_opens_before_2,
        0,
        "the `alt` remote must never be opened when the \
             first-occurrence representative routes to the default remote"
    );
}

/// One top-level `gat push` invocation must construct
/// exactly one repository snapshot (one effective-config load) regardless
/// of how many objects/paths it selects, rather than reloading config
/// once for selection resolution and again for remote/cache access.
#[test]
fn push_invocation_loads_effective_config_exactly_once() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"other").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let config_loads_before = gat_engine::test_support::config_loads();
    push(&repo, None, None, &NoopProgress).unwrap();

    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1,
        "one `gat push` invocation must load effective config exactly once"
    );
}

/// As [`push_invocation_loads_effective_config_exactly_once`], but for
/// `gat fetch`.
#[test]
fn fetch_invocation_loads_effective_config_exactly_once() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();

    let config_loads_before = gat_engine::test_support::config_loads();
    fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap();

    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1,
        "one `gat fetch` invocation must load effective config exactly once"
    );
}

/// Retained full push-obligation metadata must never
/// exceed the configured `transfer.window` bound, even for a selection
/// many times larger than one window.
#[test]
fn push_never_retains_more_than_one_configured_window_of_obligation_metadata() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    let count = window * 5 + 1;
    let mut paths = Vec::new();
    for i in 0..count {
        let path = format!("obj-{i}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();

    let before = gat_command::push_test_support::push_window_high_water();
    let ctx = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = push_selected(ctx, None, &NoopProgress).unwrap();
    let after = gat_command::push_test_support::push_window_high_water();
    assert_eq!(outcome.total, count);
    assert!(
        after.max(before) <= window,
        "no push window should ever retain more than the configured transfer.window \
         obligations, saw {}",
        after.max(before)
    );
}

/// The first push window must run while the selection
/// visitor is still producing later rows -- i.e. more than one window is
/// actually dispatched for a selection several windows large, proving
/// true streaming rather than whole-selection planning followed by
/// chunking.
#[test]
fn push_dispatches_more_than_one_window_for_a_multi_window_selection() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    let count = window * 3 + 1;
    let mut paths = Vec::new();
    for i in 0..count {
        let path = format!("obj-{i}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();

    let before = gat_command::push_test_support::push_window_calls();
    let ctx = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    push_selected(ctx, None, &NoopProgress).unwrap();
    let after = gat_command::push_test_support::push_window_calls();
    assert!(
        after - before >= 3,
        "a selection {window}x+1 larger than one window must dispatch several \
         windows, not one whole-operation batch: observed {} window(s)",
        after - before
    );
}

/// The same push `(remote, oid)` key on both sides of
/// several window boundaries must still create exactly one publication
/// obligation/remote-presence check for that key.
#[test]
fn push_cross_window_global_dedup_pushes_a_repeated_oid_exactly_once() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    // Shared oid at the very first and very last path (alphabetically
    // first/last so a and z straddle several windows worth of filler
    // paths with distinct content in between), same content everywhere.
    std::fs::write(tmp.path().join("a-shared.bin"), b"shared-bytes").unwrap();
    let mut paths = vec![PathBuf::from("a-shared.bin")];
    for i in 0..(window * 2) {
        let path = format!("m-filler-{i:04}.bin");
        std::fs::write(tmp.path().join(&path), format!("filler-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    std::fs::write(tmp.path().join("z-shared.bin"), b"shared-bytes").unwrap();
    paths.push(PathBuf::from("z-shared.bin"));
    add(&repo, &paths, &NoopProgress).unwrap();

    let ctx = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = push_selected(ctx, None, &NoopProgress).unwrap();
    // window*2 fillers + 1 shared oid = the unique oid count; the shared
    // oid across a-shared.bin/z-shared.bin collapses to one.
    assert_eq!(outcome.total, window * 2 + 1);

    let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
    let shared_oid = lock
        .entries
        .iter()
        .find(|e| e.path == "a-shared.bin")
        .unwrap()
        .oid;
    let uploaded = remote_dir.path().join(storage::object_key_oid(&shared_oid));
    assert!(
        uploaded.exists(),
        "the shared oid must have been uploaded exactly once regardless of window boundary"
    );
}

/// One oid selected through paths routed to two
/// distinct remotes must retain two publication obligations (one per
/// remote), while local cache verification for that oid is shared.
#[test]
fn push_multi_remote_creates_one_obligation_per_remote_for_the_same_oid() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"shared-bytes").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"shared-bytes").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();
    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("b.bin".to_string()),
        RouteConfig {
            path: gp("b.bin"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    let outcome = push(&repo, None, None, &NoopProgress).unwrap();
    // Local verification is shared by oid: only one unique object is
    // reported, even though it was published to two remotes.
    assert_eq!(outcome.total, 1);
    assert!(
        origin_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists(),
        "the oid must be published to the first routed remote"
    );
    assert!(
        backup_dir
            .path()
            .join(storage::object_key_oid(&oid))
            .exists(),
        "the oid must be published to the second routed remote"
    );
}

/// The same oid, selected through enough
/// distinct routed paths that its `(remote, oid)` publication obligations
/// land in different bounded transfer windows, must still verify local
/// cache state for that oid exactly once for the whole operation --
/// `verify_windows` (memoized), not `verify_windows_unmemoized`, must
/// back push's window verification loop so a later window's obligation
/// reuses the earlier window's already-verified status rather than
/// re-hashing the same bytes again. Every configured remote must still
/// receive the publication, and `PushOutcome.total` must count the one
/// unique oid, not the two remote-fan-out obligations.
#[test]
fn push_verifies_a_cross_window_fanned_out_oid_exactly_once() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    // "a-shared.bin" (routed to backup) and "z-shared.bin" (default
    // remote) share one oid; enough filler paths in between push them
    // into different transfer windows the same way the existing
    // cross-window dedup test does.
    std::fs::write(tmp.path().join("a-shared.bin"), b"shared-bytes").unwrap();
    let mut paths = vec![PathBuf::from("a-shared.bin")];
    for i in 0..(window * 2) {
        let path = format!("m-filler-{i:04}.bin");
        std::fs::write(tmp.path().join(&path), format!("filler-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    std::fs::write(tmp.path().join("z-shared.bin"), b"shared-bytes").unwrap();
    paths.push(PathBuf::from("z-shared.bin"));
    add(&repo, &paths, &NoopProgress).unwrap();

    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("a-shared.bin".to_string()),
        RouteConfig {
            path: gp("a-shared.bin"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    let before = gat_io::cache_proof_test_support::snapshot().fs_verifications;
    let ctx = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = push_selected(ctx, None, &NoopProgress).unwrap();
    let after = gat_io::cache_proof_test_support::snapshot().fs_verifications;

    assert_eq!(outcome.total, window * 2 + 1);
    assert_eq!(
        after - before,
        window * 2 + 1,
        "each unique oid (including the cross-window fanned-out shared oid) \
         must be filesystem-verified exactly once for the whole push"
    );

    let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
    let shared_oid = lock
        .entries
        .iter()
        .find(|e| e.path == "a-shared.bin")
        .unwrap()
        .oid;
    assert!(
        backup_dir
            .path()
            .join(storage::object_key_oid(&shared_oid))
            .exists(),
        "the shared oid must be published to its routed remote"
    );
    assert!(
        origin_dir
            .path()
            .join(storage::object_key_oid(&shared_oid))
            .exists(),
        "the shared oid must be published to the default remote via the other path"
    );
}

/// The whole streaming push must always
/// produce exactly one logical `Pushing` task with the same final
/// normalized position/finished state, no matter how many transfer
/// windows the same selection happens to be split into. Window size is
/// an internal implementation detail and must never leak into the
/// reported logical progress.
#[test]
fn push_progress_is_invariant_across_transfer_window_sizes() {
    use gat_engine::ExecutionLimits;

    let object_count = 7usize;
    let mut final_positions = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let mut paths = Vec::new();
    for i in 0..object_count {
        let path = format!("obj-{i}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();

    for window in [1usize, 2, 4096] {
        // A fresh remote inventory forces every case to upload all objects,
        // while the repository and local immutable cache remain reusable.
        std::fs::remove_dir_all(remote_dir.path()).unwrap();
        std::fs::create_dir(remote_dir.path()).unwrap();

        let limits = ExecutionLimits::for_test(window, 10_000, 4096, 8, 4);
        let progress = RecordingProgress::new();
        let ctx = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
        let outcome = push_selected(ctx, None, &progress).unwrap();
        assert_eq!(outcome.total, object_count);
        assert!(outcome.skipped.is_empty());
        for i in 0..object_count {
            let content = format!("payload-{i}");
            let oid = gat_core::oid::Oid::from_bytes(*blake3::hash(content.as_bytes()).as_bytes());
            assert_eq!(
                std::fs::read(remote_dir.path().join(gat_io::object_key_oid(&oid))).unwrap(),
                content.as_bytes(),
                "window size {window} must upload every object into the cleared remote"
            );
        }

        assert_eq!(
            progress.count_of(ProgressOperation::Pushing),
            1,
            "window size {window} must still produce exactly one logical Pushing task"
        );
        let task = progress.only(ProgressOperation::Pushing);
        assert!(task.finished);
        assert_eq!(
            task.total, None,
            "the whole obligation count is not known before streaming begins, \
             so no window size may expose a determinate total"
        );
        final_positions.push(task.position);
    }

    assert!(
        final_positions.iter().all(|&p| p == final_positions[0]),
        "final logical position must be identical regardless of window size: {final_positions:?}"
    );
    assert_eq!(final_positions[0], object_count as u64);
}

// These progress tests need desired rows and real object bytes, but no
// worktree or materialized-state history. Seed a cold object namespace
// directly so their setup does not run the add/push workflows under test elsewhere.
fn seed_progress_objects(repo: &Repo, object_root: &Path, count: usize) {
    let entries = (0..count)
        .map(|i| {
            let content = format!("payload-{i}");
            let oid = gat_core::oid::Oid::from_bytes(*blake3::hash(content.as_bytes()).as_bytes());
            let object = object_root.join(gat_io::object_key_oid(&oid));
            std::fs::create_dir_all(object.parent().unwrap()).unwrap();
            std::fs::write(object, content).unwrap();
            gat_core::lock::Entry {
                path: gat_core::lexical_path::GatPath::parse_canonical(&format!("obj-{i:04}.bin"))
                    .unwrap(),
                oid,
            }
        })
        .collect();
    repo.save_lock(&gat_core::lock::Lock { entries }).unwrap();
}

/// Pushing many objects concurrently
/// (well beyond the transfer window size) must still produce exactly one
/// `Pushing` task/row -- worker/object count must never create O(N)
/// logical progress tasks.
#[test]
fn push_high_concurrency_never_creates_more_than_one_pushing_task() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::for_test(4, 10_000, 4096, 8, 4);
    let count = limits.transfer.window.get() * 20;
    seed_progress_objects(&repo, &cache_path(&repo), count);
    let oids: Vec<_> = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.oid)
        .collect();
    // Match an add-populated cache: establish all proofs in one batch so
    // tiny transfer windows do not each pay a cold-proof database commit.
    let statuses = cache_root(&repo).open_client().verify_many(&oids).unwrap();
    assert!(
        statuses
            .iter()
            .all(|status| *status == gat_io::ObjectVerification::Valid)
    );

    let progress = RecordingProgress::new();
    let ctx = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = push_selected(ctx, None, &progress).unwrap();
    assert_eq!(outcome.total, count);
    assert!(outcome.skipped.is_empty());
    for oid in &oids {
        assert!(
            remote_dir
                .path()
                .join(gat_io::object_key_oid(oid))
                .is_file()
        );
    }

    assert_eq!(progress.count_of(ProgressOperation::Pushing), 1);
    let task = progress.only(ProgressOperation::Pushing);
    assert_eq!(task.position, count as u64);
    // At most one logical progress task may be active at any instant,
    // regardless of transfer-window size or worker concurrency.
    assert_eq!(progress.max_active_tasks(), 1);
}

/// The whole streaming fetch must always
/// produce exactly one logical `Fetching` task, regardless of transfer
/// window size, with an equivalent final normalized position.
#[test]
fn fetch_progress_is_invariant_across_transfer_window_sizes() {
    use gat_engine::ExecutionLimits;

    let object_count = 7usize;
    let mut final_positions = Vec::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let mut paths = Vec::new();
    for i in 0..object_count {
        let path = format!("obj-{i}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();
    for window in [1usize, 2, 4096] {
        // Drop both objects and verification proofs before each case. The
        // operation from the previous iteration has already been dropped.
        std::fs::remove_dir_all(cache_path(&repo)).unwrap();

        let limits = ExecutionLimits::for_test(window, 10_000, 4096, 8, 4);
        let progress = RecordingProgress::new();
        let mut desired_op =
            DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
        let outcome = fetch_selected(&mut desired_op, &Selection::root(), None, &progress).unwrap();
        assert_eq!(outcome.fetched, object_count, "window size {window}");

        assert_eq!(
            progress.count_of(ProgressOperation::Fetching),
            1,
            "window size {window} must still produce exactly one logical Fetching task"
        );
        let task = progress.only(ProgressOperation::Fetching);
        assert!(task.finished);
        final_positions.push(task.position);
    }

    assert!(
        final_positions.iter().all(|&p| p == final_positions[0]),
        "final logical position must be identical regardless of window size: {final_positions:?}"
    );
    assert_eq!(final_positions[0], object_count as u64);
}

/// Fetching many objects (well beyond the
/// transfer window size) must still produce exactly one `Fetching`
/// task/row -- worker/object count must never create O(N) logical
/// progress tasks.
#[test]
fn fetch_high_concurrency_never_creates_more_than_one_fetching_task() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::for_test(4, 10_000, 4096, 8, 4);
    let count = limits.transfer.window.get() * 20;
    seed_progress_objects(&repo, remote_dir.path(), count);

    let progress = RecordingProgress::new();
    let mut desired_op =
        DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = fetch_selected(&mut desired_op, &Selection::root(), None, &progress).unwrap();
    assert_eq!(outcome.fetched, count);

    assert_eq!(progress.count_of(ProgressOperation::Fetching), 1);
    let task = progress.only(ProgressOperation::Fetching);
    assert_eq!(task.position, count as u64);
    // At most one logical progress task may be active at any instant,
    // regardless of transfer-window size or worker concurrency.
    assert_eq!(progress.max_active_tasks(), 1);
}

/// One oid selected through paths routed to two
/// distinct remotes, straddling a fetch window boundary, must still
/// resolve to exactly one download using the *first* selected path's
/// route -- window boundaries and a later, differently-routed
/// reference to the same oid must never trigger a second download or
/// change the chosen source.
#[test]
fn fetch_cross_window_dedup_uses_the_first_selected_paths_route_once() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let origin_dir = tempfile::tempdir().unwrap();
    let backup_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "backup",
        gat_io::remote_file_url_for_test(backup_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    // Shared oid at the very first (default-routed) and very last
    // (explicitly re-routed to "backup") path, with enough filler paths
    // of distinct content between them to straddle several windows.
    std::fs::write(tmp.path().join("a-shared.bin"), b"shared-bytes").unwrap();
    let mut paths = vec![PathBuf::from("a-shared.bin")];
    for i in 0..(window * 2) {
        let path = format!("m-filler-{i:04}.bin");
        std::fs::write(tmp.path().join(&path), format!("filler-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    std::fs::write(tmp.path().join("z-shared.bin"), b"shared-bytes").unwrap();
    paths.push(PathBuf::from("z-shared.bin"));
    add(&repo, &paths, &NoopProgress).unwrap();

    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("z-shared.bin".to_string()),
        RouteConfig {
            path: gp("z-shared.bin"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    push(&repo, None, None, &NoopProgress).unwrap();

    let shared_oid_for_removal = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries
        .iter()
        .find(|e| e.path == "a-shared.bin")
        .unwrap()
        .oid;
    // Remove the object from backup (the *later* selected path's route):
    // if fetch ever fell back to z-shared.bin's route instead of using
    // a-shared.bin's first-selected route, this fetch would fail.
    std::fs::remove_file(
        backup_dir
            .path()
            .join(storage::object_key_oid(&shared_oid_for_removal)),
    )
    .unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    let mut desired_op =
        DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let fetched = fetch_selected(&mut desired_op, &Selection::root(), None, &NoopProgress)
        .unwrap()
        .fetched;
    // window*2 fillers + 1 shared oid.
    assert_eq!(fetched, window * 2 + 1);

    let shared_oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries
        .iter()
        .find(|e| e.path == "a-shared.bin")
        .unwrap()
        .oid;
    let expected_key = storage::object_key_oid(&shared_oid);
    assert!(
        origin_dir.path().join(&expected_key).exists(),
        "the shared oid must be reachable from origin (first-path route)"
    );
    // The download must have come from the object's first-selected path
    // route (origin) -- pushed there above and thus already present.
}

/// A fetch whose selected objects are
/// already valid, proof-backed cache entries must resolve routes (a
/// cheap local policy lookup) but never open any remote operator at all
/// -- verification runs before route/remote resolution decides which
/// objects actually need a download, and every object here verifies
/// `Valid` before any operator would be opened.
#[test]
fn warm_fetch_opens_zero_remote_operators() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    // Objects are already cached locally and proof-verified from `add`;
    // fetching now must find everything `Valid` without downloading.
    let remote_opens_before = gat_engine::test_support::remote_opens();
    let outcome = fetch_current(&repo, &Selection::root(), None, &NoopProgress).unwrap();

    assert_eq!(
        outcome.fetched, 0,
        "an already-cached object must not be re-downloaded"
    );
    assert_eq!(
        gat_engine::test_support::remote_opens(),
        remote_opens_before,
        "a warm fetch whose objects are all already valid must never \
         initialize a remote operator"
    );
}

/// When the cache verifier's own
/// verification chunk (`gat_io::VERIFY_WINDOW`, overridden here
/// via the test-only injection point) is smaller than
/// `ExecutionLimits::transfer.window`, one transfer window spans several
/// verification subwindows -- each subwindow's `on_window` callback
/// receives a status slice that does not start at offset zero of the
/// original transfer-window object list past the first subwindow. Fetch
/// must associate each verification status with the correct engine
/// download object via a running offset rather than always re-slicing the
/// object-window prefix, so a mixed valid/missing/corrupt fetch spanning
/// several verification subwindows still downloads exactly the intended
/// objects.
#[test]
fn fetch_correctly_aligns_verification_across_several_subwindows_within_one_transfer_window() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    // A small verification chunk (3), well under a much larger transfer
    // window (20), so one transfer window's fetch spans several
    // verification subwindows.
    let _verify_guard = gat_io::cache_object_test_support::with_verify_window(3);
    let mut limits = ExecutionLimits::tiny();
    limits.transfer.window = std::num::NonZeroUsize::new(20).unwrap();

    // 10 objects: every 3rd object (by selection order) will be "missing"
    // locally after removal below, straddling several verification
    // subwindows of size 3.
    let count = 10;
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        let path = format!("obj-{i:02}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    // Remove every 3rd object's cache entry (by lexical path order, which
    // matches current-state selection order here) so it must be
    // re-downloaded; leave the rest valid.
    let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
    let mut entries: Vec<_> = lock.entries.iter().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let mut removed_oids = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if i % 3 == 0 {
            let cache_root = cache_root(&repo);
            let cache_path = cache_root.object_path_for_test(&entry.oid);
            let _ = cache_root.make_object_writable_for_test(&entry.oid);
            std::fs::remove_file(&cache_path).unwrap();
            removed_oids.push(entry.oid);
        }
    }

    let mut desired_op =
        DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let fetched = fetch_selected(&mut desired_op, &Selection::root(), None, &NoopProgress)
        .unwrap()
        .fetched;
    assert_eq!(
        fetched,
        removed_oids.len(),
        "exactly the objects removed from the local cache must be \
         redownloaded, regardless of how the transfer window and \
         verification subwindows relate"
    );
    for oid in &removed_oids {
        assert!(
            cache_root(&repo).presence().contains(oid),
            "removed object {oid} must be present again after fetch"
        );
    }
}

/// An end-to-end `pull` (fetch, then sync) forced
/// through tiny `ExecutionLimits` must still correctly fetch and
/// materialize every file even when the selection spans many more
/// windows than one bounded transfer window -- proving fetch's
/// window-boundary streaming and sync's dirty-row/merge-batch windowing
/// compose correctly within one coherent `DesiredOperation`.
#[test]
fn pull_end_to_end_with_tiny_limits_materializes_every_file_across_many_windows() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    let count = window * 3 + 1;
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        let path = format!("file-{i:04}.bin");
        std::fs::write(tmp.path().join(&path), format!("content-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();
    for path in &paths {
        std::fs::remove_file(tmp.path().join(path)).unwrap();
    }

    let desired_op = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = pull_selected(desired_op, Selection::root(), None, None, &NoopProgress).unwrap();
    assert_eq!(outcome.outcome.materialized, count);
    for (i, path) in paths.iter().enumerate() {
        assert_eq!(
            std::fs::read(tmp.path().join(path)).unwrap(),
            format!("content-{i}").into_bytes()
        );
    }
}

/// When the first *filled* transfer window's own
/// dispatch fails (not merely a resolution error before recording), the
/// selection producer must stop immediately -- later rows spanning
/// several further windows must never be visited/dispatched, and the
/// original failure must surface directly rather than being replaced by
/// a later traversal error. Before this fix, a `pending_error` sentinel
/// let the producer keep traversing every remaining row (as a no-op)
/// after the first failure.
#[test]
fn push_stops_dispatching_further_windows_after_the_first_filled_window_fails() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let not_a_dir = tmp.path().join("not-a-directory");
    std::fs::write(&not_a_dir, b"not a directory").unwrap();
    // "good" is added first so it becomes the repository default remote;
    // "bad" is only ever reached via the explicit routes below.
    let good_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "good",
        gat_io::remote_file_url_for_test(good_dir.path()),
    )
    .unwrap();
    remote_add_with_default(&repo, "bad", gat_io::remote_file_url_for_test(&not_a_dir)).unwrap();

    // First window (transfer.window == 2): both paths routed to the
    // broken remote, so `run_push_window`'s own remote-presence check
    // fails while processing this window.
    route_add(&repo, "bad-route", "bad", "aaa-bad-0.bin").unwrap();
    route_add(&repo, "bad-route-1", "bad", "aaa-bad-1.bin").unwrap();
    std::fs::write(tmp.path().join("aaa-bad-0.bin"), b"bad-0").unwrap();
    std::fs::write(tmp.path().join("aaa-bad-1.bin"), b"bad-1").unwrap();

    // Several further windows' worth of otherwise-valid paths, routed to
    // the working remote by default -- if traversal continued after the
    // first window's failure (the pre-fix `pending_error` behavior),
    // these would each fill and dispatch their own window.
    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    let mut later_paths = Vec::new();
    for i in 0..(window * 4) {
        let path = format!("zzz-good-{i}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        later_paths.push(PathBuf::from(path));
    }
    add(
        &repo,
        &[
            PathBuf::from("aaa-bad-0.bin"),
            PathBuf::from("aaa-bad-1.bin"),
        ],
        &NoopProgress,
    )
    .unwrap();
    add(&repo, &later_paths, &NoopProgress).unwrap();

    let before = gat_command::push_test_support::push_window_calls();
    let ctx = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let err = push_selected(ctx, None, &NoopProgress).unwrap_err();
    let after = gat_command::push_test_support::push_window_calls();

    assert_eq!(
        after - before,
        1,
        "only the first (failing) window should ever be dispatched; later rows spanning \
         several further windows must never be visited after the first failure, saw {} \
         window(s)",
        after - before
    );
    // Assert the typed remote identity; backend error wording varies by OS.
    assert!(matches!(
        err,
        gat_command::PushError::Upload(
            gat_engine::UploadError::FileWrite { remote_name, .. }
            | gat_engine::UploadError::RemoteOpen { remote_name, .. }
            | gat_engine::UploadError::WriterOpen { remote_name, .. }
        ) | gat_command::PushError::Presence(
            gat_engine::RemotePresenceError::RemoteOpen { remote_name, .. }
            | gat_engine::RemotePresenceError::PresenceCheck { remote_name, .. }
        ) if remote_name.as_ref() == "bad"
    ));
}

/// `status --remote` must execute its first
/// bounded remote-presence window as soon as it fills -- not materialize
/// the whole selection first and only then start remote I/O. With a tiny
/// `transfer.window`, several times more rows than one window, and only
/// the first window's worth of objects actually missing from the remote,
/// asserts several presence-check windows are dispatched (proving the
/// selection was streamed rather than resolved into one whole-operation
/// `Vec` before any remote check ran), each window never exceeds
/// `transfer.window`, and the sum of dispatched window sizes matches the
/// unique selection size.
#[test]
fn status_remote_streams_bounded_presence_windows_instead_of_materializing_the_whole_selection() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    let count = window * 3 + 1;
    let mut paths = Vec::new();
    for i in 0..count {
        let path = format!("obj-{i:04}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();
    // deliberately no push: every object is missing from the remote.

    gat_engine::test_support::reset_remote_check_window_sizes();
    let mut desired_op =
        DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let selection = Selection::root();
    let outcome = gat_command::remote_status_with_desired_operation(
        &mut desired_op,
        gat_command::RemoteStatusRequest {
            selection: Some(&selection),
            remote: None,
            history: None,
        },
        &NoopProgress,
    )
    .unwrap();

    assert_eq!(outcome.checked, count);
    assert_eq!(outcome.missing.len(), count);

    let sizes = gat_engine::test_support::remote_check_window_sizes();
    assert!(
        sizes.len() > 1,
        "a selection several windows wide must dispatch more than one bounded \
         remote-presence check, saw {} check(s)",
        sizes.len()
    );
    assert!(
        sizes.iter().all(|&s| s <= window),
        "no remote-presence window may exceed the configured transfer.window ({window}): {sizes:?}"
    );
    assert_eq!(
        sizes.iter().sum::<usize>(),
        count,
        "dispatched window sizes must sum to the whole unique selection"
    );
}

/// A composite fetch -> repair -> sync sequence,
/// deliberately driven through the *same* `Operation` end to end
/// (`DesiredOperation::acquire` -> `gat_command::fetch_with_desired_operation` ->
/// `finish_selection` -> `repair_corrupted` -> engine sync),
/// must share exactly one proof
/// database session, and must observe repair's just-applied proof delta
/// on its very next verification without reopening `cache.sqlite3` --
/// i.e. no phase along the way ever reopens its own independent cache.
#[test]
fn fetch_then_repair_then_sync_share_one_proof_session() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    std::fs::write(tmp.path().join("shared.bin"), b"shared").unwrap();
    add(&repo, &[PathBuf::from("shared.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add shared.bin");
    // This setup push runs under its own, separate operation -- only the
    // fetch/repair/mutate sequence below is the one operation under test.
    push(&repo, None, None, &NoopProgress).unwrap();
    let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let cache_db_opens_before = gat_io::cache_proof_test_support::snapshot().cache_db_opens;

    let mut desired_op = DesiredOperation::acquire(&repo, &NoopProgress).unwrap();
    // Fetch phase: repo is already fully up to date after the setup push
    // above, so this fetches nothing new, but it still exercises this
    // operation's own `CacheSession` for verification.
    let _fetched = fetch_selected(&mut desired_op, &Selection::root(), None, &NoopProgress)
        .unwrap()
        .fetched;

    // Explicit desired-capability lifetime boundary: the
    // desired reader is dropped here, before the repair/mutate phases
    // below, which need no desired rows at all.
    let mut operation = desired_op.finish_selection();

    // Repair phase, sharing the same `Operation`/`CacheSession`.
    let corrupted = vec![(gp("shared.bin"), oid)];
    let repair_task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Repairing));
    let repair = repair_corrupted(&mut operation, None, &corrupted, &repair_task.handle());
    assert_eq!(repair.repaired, 1, "the one corrupted oid must be repaired");
    assert!(
        repair.failures.is_empty(),
        "repair must not report any failure: {:?}",
        repair.failures
    );

    // Authoritative sync phase, sharing the same opaque Operation/session.
    // The engine owns mutation admission and the guard's lock lifetime.
    gat_engine::sync_from_snapshot(&mut operation, &gat_engine::SyncOptions::default(), None)
        .unwrap();

    let cache_db_opens_after = gat_io::cache_proof_test_support::snapshot().cache_db_opens;
    assert_eq!(
        cache_db_opens_after - cache_db_opens_before,
        1,
        "one proof database session must be shared by fetch and repair, \
         not reopened per phase"
    );

    // Correct proof invalidation after repair: a semantic download through
    // the same operation must recognize the just-repaired object as valid
    // without downloading or opening a second cache session.
    let path = gp("shared.bin");
    let remote = operation
        .policy()
        .resolved_remote_for_path(operation.remotes_catalog(), None, &path)
        .unwrap()
        .unwrap();
    let outcome = gat_engine::download_window(
        &mut operation,
        vec![gat_engine::DownloadObject::new(oid, path, remote)],
        &repair_task.handle(),
    )
    .unwrap();
    assert_eq!(
        outcome.downloaded, 0,
        "the repaired oid must verify through the authoritative download service"
    );
    assert_eq!(
        gat_io::cache_proof_test_support::snapshot().cache_db_opens - cache_db_opens_before,
        1,
        "post-repair verification must reuse the already-open proof session, not reopen it"
    );
}

/// A push whose selection spans several
/// bounded transfer windows, resolved entirely through the *default*
/// remote (no explicit `--remote`), must still initialize that remote's
/// operator exactly once for the whole operation -- not once per window
/// -- and must never initialize a second, configured-but-unrouted remote
/// at all. Complements `gat-engine`'s unit-level
/// `operator_is_reused_across_many_sequential_windows` test (which drives
/// `RemoteSession` directly) by proving the same guarantee
/// end to end through a real multi-window `push`, and complements
/// [`push_never_opens_a_configured_remote_that_no_object_routes_to`] by
/// spanning many windows rather than one.
#[test]
fn push_across_many_windows_to_the_default_remote_opens_its_operator_exactly_once() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    // `origin` is added first, so it (not `unused`) becomes the implicit
    // default remote every unrouted path resolves to.
    let origin_dir = tempfile::tempdir().unwrap();
    let unused_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(origin_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "unused",
        gat_io::remote_file_url_for_test(unused_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    let count = window * 5 + 1;
    let mut paths = Vec::new();
    for i in 0..count {
        let path = format!("obj-{i}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();

    let origin_opens_before = gat_engine::test_support::remote_open_count_for("origin");
    let unused_opens_before = gat_engine::test_support::remote_open_count_for("unused");

    // No explicit `--remote`: every object resolves through the default
    // route.
    let ctx = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = push_selected(ctx, None, &NoopProgress).unwrap();

    assert_eq!(
        outcome.total, count,
        "the whole multi-window selection must have been pushed"
    );
    assert_eq!(
        gat_engine::test_support::remote_open_count_for("origin") - origin_opens_before,
        1,
        "the default remote must be initialized exactly once for the whole \
         multi-window push, not once per window"
    );
    assert_eq!(
        gat_engine::test_support::remote_open_count_for("unused") - unused_opens_before,
        0,
        "a configured-but-unrouted remote must never be initialized, even \
         across many windows"
    );
    let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
    for entry in &lock.entries {
        assert!(
            origin_dir
                .path()
                .join(storage::object_key_oid(&entry.oid))
                .exists(),
            "every object must actually have been published to the resolved default remote"
        );
    }
}

/// An end-to-end `gat pull`, driven under
/// deliberately tiny positive `ExecutionLimits` over a selection large
/// enough to span several transfer windows, must still fetch and
/// materialize every single path correctly -- the same observable
/// behavior a production-limits pull produces -- proving fetch's window-
/// bounded consolidation preserves pre-consolidation command output
/// across every window boundary.
#[test]
fn pull_with_tiny_limits_fetches_and_materializes_every_path_across_many_windows() {
    use gat_engine::ExecutionLimits;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let limits = ExecutionLimits::tiny();
    let window = limits.transfer.window.get();
    let count = window * 3 + 1;
    let mut paths = Vec::new();
    for i in 0..count {
        let path = format!("obj-{i}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();
    commit_all(tmp.path(), "add many files");
    push(&repo, None, None, &NoopProgress).unwrap();

    // Simulate a fresh clone: no local objects, no working files.
    for i in 0..count {
        std::fs::remove_file(tmp.path().join(format!("obj-{i}.bin"))).unwrap();
    }
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();

    let desired_op = DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let outcome = pull_selected(desired_op, Selection::root(), None, None, &NoopProgress).unwrap();

    assert_eq!(
        outcome.fetched, count,
        "every object across every tiny-limits window must have been fetched"
    );
    assert_eq!(
        outcome.outcome.materialized, count,
        "every path across every tiny-limits window must be materialized"
    );
    for i in 0..count {
        assert_eq!(
            std::fs::read(tmp.path().join(format!("obj-{i}.bin"))).unwrap(),
            format!("payload-{i}").as_bytes(),
            "obj-{i}.bin must have been correctly fetched and restored"
        );
    }
}

/// Builds the typed selection produced by a plain `--rev X`. The private
/// CLI-to-domain conversion itself is covered beside `app`; these workflows
/// verify that transfer commands honor the resulting public contract.
fn rev_history_selection(rev: &str) -> HistorySelection {
    HistorySelection {
        roots: vec![HistoryRoot::Revision(rev.to_string().into())],
        traversal: HistoryTraversal::Tips,
        ..Default::default()
    }
}

#[test]
fn push_rev_flag_selects_only_that_snapshot_not_its_ancestry() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-1").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 1");
    let oid_1 = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    std::fs::write(tmp.path().join("a.bin"), b"version-2-mid").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 2");
    let oid_mid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    std::fs::write(tmp.path().join("a.bin"), b"version-3-head-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 3");
    let oid_head = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    assert_ne!(oid_1, oid_mid);
    assert_ne!(oid_mid, oid_head);
    assert_ne!(oid_1, oid_head);

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    // `--rev HEAD~1` selects the middle snapshot: with the pre-fix
    // always-`Ancestors` traversal this would also walk back to `oid_1`.
    let selection = rev_history_selection("HEAD~1");
    let outcome =
        push_with_history(&repo, &Selection::root(), None, &selection, &NoopProgress).unwrap();
    assert_eq!(outcome.total, 1);

    assert!(
        remote_dir
            .path()
            .join(storage::object_key_oid(&oid_mid))
            .exists(),
        "the explicitly selected revision's object must be pushed"
    );
    assert!(
        !remote_dir
            .path()
            .join(storage::object_key_oid(&oid_1))
            .exists(),
        "`--rev` alone must not walk ancestry: the selected revision's \
         ancestor object must not be pushed"
    );
    assert!(
        !remote_dir
            .path()
            .join(storage::object_key_oid(&oid_head))
            .exists(),
        "current/head state must not be implicitly unioned into an \
         explicit `--rev` selection"
    );
}

#[test]
fn fetch_rev_flag_selects_only_that_snapshot_not_its_ancestry() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-1").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 1");
    let oid_1 = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-2-mid").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 2");
    let oid_mid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-3-head-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 3");
    let oid_head = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();

    let selection = rev_history_selection("HEAD~1");
    let outcome =
        fetch_history(&repo, &Selection::root(), None, &selection, &NoopProgress).unwrap();
    assert_eq!(outcome.fetched, 1);
    assert!(!outcome.shallow);
    assert!(
        cache_root(&repo).presence().contains(&oid_mid),
        "the explicitly selected revision's object must be fetched"
    );
    assert!(
        !cache_root(&repo).presence().contains(&oid_1),
        "`--rev` alone must not walk ancestry: the selected revision's \
         ancestor object must not be fetched"
    );
    assert!(
        !cache_root(&repo).presence().contains(&oid_head),
        "current/head state must not be implicitly unioned into an \
         explicit `--rev` selection"
    );
}

#[test]
fn pull_rev_flag_prefetches_only_that_snapshot_alongside_current_state() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-1").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 1");
    let oid_1 = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-2-mid").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 2");
    let oid_mid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-3-head-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 3");
    let oid_head = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    push(&repo, None, None, &NoopProgress).unwrap();

    let mut cfg = repo.load_config().unwrap();
    cfg.sync.trust_state = Some(false);
    repo.save_config(&cfg).unwrap();

    std::fs::remove_file(tmp.path().join("a.bin")).unwrap();
    std::fs::remove_dir_all(cache_path(&repo)).unwrap();

    let selection = rev_history_selection("HEAD~1");
    let desired_op = DesiredOperation::acquire(&repo, &NoopProgress).unwrap();
    let outcome = pull_selected(
        desired_op,
        Selection::root(),
        None,
        Some(selection),
        &NoopProgress,
    )
    .unwrap();
    assert!(outcome.outcome.is_clean());

    // The explicitly selected revision's object was prefetched...
    assert!(cache_root(&repo).presence().contains(&oid_mid));
    // ...as was current (head) state's object, via the mandatory union...
    assert!(cache_root(&repo).presence().contains(&oid_head));
    // ...but `--rev` alone must not have walked ancestry to prefetch the
    // selected revision's own ancestor.
    assert!(!cache_root(&repo).presence().contains(&oid_1));
    // Only current (head) state was materialized to the worktree.
    assert_eq!(
        std::fs::read(tmp.path().join("a.bin")).unwrap(),
        b"version-3-head-longer"
    );
}

#[test]
fn remote_status_rev_flag_checks_only_that_snapshot_not_its_ancestry() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-1").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 1");
    let oid_1 = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    std::fs::write(tmp.path().join("a.bin"), b"version-2-mid").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 2");
    let oid_mid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    std::fs::write(tmp.path().join("a.bin"), b"version-3-head-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 3");
    let oid_head = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    assert_ne!(oid_1, oid_mid);
    assert_ne!(oid_mid, oid_head);

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    // Nothing has ever been pushed, so every reachable object is
    // "missing" from the remote; `--rev` must still narrow which objects
    // get checked/reported down to just the selected revision's, not its
    // full ancestry.
    let selection = rev_history_selection("HEAD~1");
    let path_selection = Selection::root();
    let outcome = gat_command::remote_status(
        &repo,
        gat_command::RemoteStatusRequest {
            selection: Some(&path_selection),
            remote: None,
            history: Some(&selection),
        },
        &NoopProgress,
    )
    .unwrap();

    assert_eq!(outcome.checked, 1);
    let missing_oids: Vec<gat_core::oid::Oid> =
        outcome.missing.iter().map(|obj| obj.object.oid).collect();
    assert!(
        missing_oids.contains(&oid_mid),
        "the explicitly selected revision's object must be checked/reported"
    );
    assert!(
        !missing_oids.contains(&oid_1),
        "`--rev` alone must not walk ancestry: the selected revision's \
         ancestor object must not be checked"
    );
    assert!(
        !missing_oids.contains(&oid_head),
        "current/head state must not be implicitly unioned into an \
         explicit `--rev` selection"
    );
}

/// Builds the typed selection produced by `--rev X --ancestors`.
fn rev_with_ancestors_history_selection(rev: &str) -> HistorySelection {
    HistorySelection {
        roots: vec![HistoryRoot::Revision(rev.to_string().into())],
        traversal: HistoryTraversal::Ancestors { per_root: None },
        ..Default::default()
    }
}

/// Builds the typed selection produced by `--rev X --depth N`.
fn rev_with_depth_history_selection(rev: &str, depth: usize) -> HistorySelection {
    HistorySelection {
        roots: vec![HistoryRoot::Revision(rev.to_string().into())],
        traversal: HistoryTraversal::Ancestors {
            per_root: std::num::NonZeroUsize::new(depth),
        },
        ..Default::default()
    }
}

/// Builds the typed selection produced by `--all-history`.
fn all_history_selection() -> HistorySelection {
    HistorySelection {
        roots: vec![HistoryRoot::AllRefs],
        traversal: HistoryTraversal::Ancestors { per_root: None },
        ..Default::default()
    }
}

#[test]
fn push_rev_with_ancestors_flag_walks_full_ancestry_but_not_head() {
    // `--rev X --ancestors` must select `X` and everything reachable
    // from it, but never anything only reachable from `HEAD` when `X`
    // isn't `HEAD` itself -- ancestry is walked backwards from the named
    // revision, not unioned with current state.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-1").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 1");
    let oid_1 = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    std::fs::write(tmp.path().join("a.bin"), b"version-2-mid").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 2");
    let oid_mid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    std::fs::write(tmp.path().join("a.bin"), b"version-3-head-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 3");
    let oid_head = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    assert_ne!(oid_1, oid_mid);
    assert_ne!(oid_mid, oid_head);

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    // `--rev HEAD~1 --ancestors` selects the middle snapshot and its
    // ancestor (the first commit's snapshot), but not the (newer) head
    // snapshot.
    let selection = rev_with_ancestors_history_selection("HEAD~1");
    let outcome =
        push_with_history(&repo, &Selection::root(), None, &selection, &NoopProgress).unwrap();
    assert_eq!(outcome.total, 2);

    assert!(
        remote_dir
            .path()
            .join(storage::object_key_oid(&oid_1))
            .exists(),
        "an ancestor of the selected revision must be pushed under --ancestors"
    );
    assert!(
        remote_dir
            .path()
            .join(storage::object_key_oid(&oid_mid))
            .exists(),
        "the selected revision itself must be pushed"
    );
    assert!(
        !remote_dir
            .path()
            .join(storage::object_key_oid(&oid_head))
            .exists(),
        "--ancestors walks backwards from the named revision only, never \
         forwards to HEAD"
    );
}

#[test]
fn fetch_rev_with_depth_flag_bounds_the_walked_ancestry() {
    // `--rev HEAD --depth 2` must fetch HEAD and HEAD~1's objects but
    // never reach HEAD~2 or earlier.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"version-1").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 1");
    let oid_1 = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-2-mid").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 2");
    let oid_mid = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-3-head-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version 3");
    let oid_head = gat_io::LockStore::load_repository(&layout(tmp.path()))
        .unwrap()
        .entries[0]
        .oid;
    push(&repo, None, None, &NoopProgress).unwrap();

    std::fs::remove_dir_all(cache_path(&repo)).unwrap();

    let selection = rev_with_depth_history_selection("HEAD", 2);
    let outcome =
        fetch_history(&repo, &Selection::root(), None, &selection, &NoopProgress).unwrap();
    assert_eq!(outcome.fetched, 2);
    assert!(!outcome.shallow);
    assert!(cache_root(&repo).presence().contains(&oid_head));
    assert!(cache_root(&repo).presence().contains(&oid_mid));
    assert!(
        !cache_root(&repo).presence().contains(&oid_1),
        "--depth 2 must never reach HEAD~2"
    );
}

#[test]
fn push_all_history_includes_a_commit_only_reachable_via_a_custom_ref() {
    // `--all-history` must protect any commit-bearing ref, not just the
    // currently checked-out branch -- a commit reachable only through a
    // custom ref must still be pushed.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let layout = layout(tmp.path());

    std::fs::write(tmp.path().join("branch.bin"), b"branch").unwrap();
    add(&repo, &[PathBuf::from("branch.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add branch.bin");
    let branch_oid = gat_io::LockStore::load_repository(&layout).unwrap().entries[0].oid;
    let branch_tip =
        gat_io::resolve_commit(&layout, &gat_core::git::GitRevisionSpec::from("HEAD")).unwrap();

    std::fs::write(tmp.path().join("custom.bin"), b"custom").unwrap();
    add(&repo, &[PathBuf::from("custom.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add custom.bin");
    let custom_oid = gat_io::LockStore::load_repository(&layout)
        .unwrap()
        .entries
        .iter()
        .find(|e| e.path == "custom.bin")
        .unwrap()
        .oid;
    let custom_tip =
        gat_io::resolve_commit(&layout, &gat_core::git::GitRevisionSpec::from("HEAD")).unwrap();
    test_support_git::run_git(
        tmp.path(),
        &["update-ref", "refs/gat-tests/kept", &custom_tip.to_string()],
    );
    // Detach the branch from `custom_tip` so `custom.bin`'s commit is
    // reachable *only* via the custom ref, never a branch tip.
    test_support_git::run_git(tmp.path(), &["reset", "--hard", &branch_tip.to_string()]);

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    let selection = all_history_selection();
    push_with_history(&repo, &Selection::root(), None, &selection, &NoopProgress).unwrap();

    assert!(
        remote_dir
            .path()
            .join(storage::object_key_oid(&branch_oid))
            .exists()
    );
    assert!(
        remote_dir
            .path()
            .join(storage::object_key_oid(&custom_oid))
            .exists(),
        "a commit only reachable via a custom (non-branch, non-tag) ref \
         must still be pushed under --all-history"
    );
}

/// Writes a dangling ref (syntactically valid target OID that was never
/// written to the object database) directly into `.git/refs`, bypassing
/// `git update-ref`'s own validation that the target exists --
/// mirroring what an interrupted transfer or a corrupted repository can
/// leave behind in practice. Shared by the `--all-history` fail-closed
/// regression tests below for `push`/`fetch`/`pull`/`gat status
/// --remote`.
fn write_dangling_ref(dir: &std::path::Path, name: &str) {
    let fake_oid = "0123456789abcdef0123456789abcdef01234567";
    let ref_path = dir.join(".git").join("refs").join(name);
    std::fs::create_dir_all(ref_path.parent().unwrap())
        .unwrap_or_else(|e| panic!("creating parent dir for dangling ref {name}: {e}"));
    std::fs::write(&ref_path, format!("{fake_oid}\n"))
        .unwrap_or_else(|e| panic!("writing dangling ref {name}: {e}"));
}

/// `--all-history` resolves `HistoryRoot::AllRefs`, so a dangling ref
/// (its target object was never written -- e.g. a corrupted repo or an
/// interrupted transfer) must make `push --all-history` fail outright
/// rather than silently pushing only what the *other* refs' history
/// happens to reach, which could under-report what actually needs
/// pushing.
#[test]
fn push_with_all_history_fails_closed_on_a_dangling_ref() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add a.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    write_dangling_ref(tmp.path(), "custom/dangling");

    let selection = all_history_selection();
    let result = push_with_history(&repo, &Selection::root(), None, &selection, &NoopProgress);
    assert!(
        result.is_err(),
        "push --all-history must fail closed on a dangling ref instead \
         of silently pushing an incomplete selection"
    );
}

/// As [`push_with_all_history_fails_closed_on_a_dangling_ref`], but for
/// `fetch --all-history`.
#[test]
fn fetch_with_all_history_fails_closed_on_a_dangling_ref() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add a.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    write_dangling_ref(tmp.path(), "custom/dangling");

    let selection = all_history_selection();
    let result = fetch_history(&repo, &Selection::root(), None, &selection, &NoopProgress);
    assert!(
        result.is_err(),
        "fetch --all-history must fail closed on a dangling ref instead \
         of silently fetching an incomplete selection"
    );
}

/// As [`push_with_all_history_fails_closed_on_a_dangling_ref`], but for
/// `pull --all-history`'s historical-prefetch phase.
#[test]
fn pull_with_all_history_fails_closed_on_a_dangling_ref() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add a.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    write_dangling_ref(tmp.path(), "custom/dangling");

    let selection = all_history_selection();
    let desired_op = DesiredOperation::acquire(&repo, &NoopProgress).unwrap();
    let result = pull_selected(
        desired_op,
        Selection::root(),
        None,
        Some(selection),
        &NoopProgress,
    );
    assert!(
        result.is_err(),
        "pull --all-history must fail closed on a dangling ref instead \
         of silently prefetching an incomplete selection"
    );
}

/// As [`push_with_all_history_fails_closed_on_a_dangling_ref`], but for
/// `gat status --remote --all-history`.
#[test]
fn remote_status_with_all_history_fails_closed_on_a_dangling_ref() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "add a.bin");

    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    write_dangling_ref(tmp.path(), "custom/dangling");

    let selection = all_history_selection();
    let path_selection = Selection::root();
    let result = gat_command::remote_status(
        &repo,
        gat_command::RemoteStatusRequest {
            selection: Some(&path_selection),
            remote: None,
            history: Some(&selection),
        },
        &NoopProgress,
    );
    assert!(
        result.is_err(),
        "status --remote --all-history must fail closed on a dangling \
         ref instead of silently reporting an incomplete selection"
    );
}
