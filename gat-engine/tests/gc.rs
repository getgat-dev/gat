use std::io::Write;
use std::num::NonZeroUsize;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use gat_core::cache_location::CacheLocation;
use gat_core::endpoint::RemoteUrlTemplate;
use gat_core::git::{GitRevisionSpec, GitTimestamp};
use gat_core::git_location::GitLocationSpec;
use gat_core::history::{HistoryRoot, HistorySelection, HistoryTraversal, TimeWindow};
use gat_core::lexical_path::GatPath;
use gat_core::lock::LockShardLevels;
use gat_core::name::RemoteName;
use gat_core::oid::Oid;
use gat_core::progress::{
    ActivityBackend, NoopProgress, ProgressActivity, ProgressOperation, ProgressReporter,
    ProgressSpec, ProgressTask,
};
use gat_engine::{GcError, GcOptions, Repository};
use gat_io::{CacheRoot, LockStore, RemoteClient, RepositoryLayout, object_key_oid};

fn cache_root(repo: &Repository) -> CacheRoot {
    gat_engine::test_support::cache_root(repo)
}

#[test]
fn explicit_repository_union_and_history_selection_match_for_both_targets() {
    gat_engine::initialize_backends();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    for remote_target in [false, true] {
        let storage = tempfile::tempdir().unwrap();
        let current = test_repo();
        let repo = Repository::at(current.path().to_path_buf());
        let remote_name = if remote_target {
            Some(use_remote(&repo, storage.path()))
        } else {
            use_shared_cache(&repo, storage.path());
            None
        };
        let current_oid = track(&repo, current.path(), "current.bin", b"current");
        let peer = test_repo();
        let peer_repo = Repository::at(peer.path().to_path_buf());
        let old = track(&peer_repo, peer.path(), "old.bin", b"old");
        commit_all(peer.path(), "old");
        untrack(&peer_repo, peer.path(), "old.bin");
        let tip = track(&peer_repo, peer.path(), "tip.bin", b"tip");
        commit_all(peer.path(), "tip");
        let uncommitted = track(&peer_repo, peer.path(), "uncommitted.bin", b"uncommitted");
        let second = test_repo();
        let second_repo = Repository::at(second.path().to_path_buf());
        let second_oid = track(&second_repo, second.path(), "second.bin", b"second");
        commit_all(second.path(), "second");
        let remote = remote_name.as_ref().map(|_| remote_for(&repo));
        // Reuse the repository history, restoring the swept inventory before
        // each selection so every mode starts with the same five objects.
        for mode in ["no-history", "tips", "all-history"] {
            let full_history = mode == "all-history";
            for (oid, bytes) in [
                (current_oid, b"current".as_slice()),
                (old, b"old"),
                (tip, b"tip"),
                (uncommitted, b"uncommitted"),
                (second_oid, b"second"),
            ] {
                if let Some(remote) = &remote {
                    remote.write(&object_key_oid(&oid), bytes.to_vec()).unwrap();
                } else {
                    assert_eq!(
                        cache_root(&repo)
                            .writer()
                            .ingest(std::io::Cursor::new(bytes))
                            .unwrap()
                            .0
                            .oid,
                        oid
                    );
                }
            }
            let peer_location = GitLocationSpec::from_string(peer.path().display().to_string());
            let repositories = [
                peer_location.clone(),
                peer_location,
                GitLocationSpec::from_string(gat_io::remote_file_url_for_test(second.path())),
            ];
            let history = if full_history {
                HistorySelection::conservative_default()
            } else {
                HistorySelection {
                    roots: vec![HistoryRoot::Head],
                    traversal: HistoryTraversal::Tips,
                    ..Default::default()
                }
            };
            let report = repo
                .garbage_collect(
                    &GcOptions {
                        repositories: &repositories,
                        dry_run: false,
                        unsafe_override: remote_target,
                        remote: remote_name.as_ref(),
                        history: (mode != "no-history").then_some(&history),
                    },
                    &NoopProgress,
                )
                .unwrap();
            assert_eq!(report.deleted, if full_history { 1 } else { 2 });
            for (oid, expected) in [
                (current_oid, true),
                (old, full_history),
                (tip, true),
                (uncommitted, false),
                (second_oid, true),
            ] {
                let present = if let Some(remote) = &remote {
                    remote.exists(&object_key_oid(&oid)).unwrap()
                } else {
                    cache_root(&repo).presence().contains(&oid)
                };
                assert_eq!(
                    present, expected,
                    "remote={remote_target}, history={full_history}, oid={oid}"
                );
            }
        }
    }
}

#[test]
fn explicit_repository_failures_are_deduplicated_and_dry_runs_are_uncertain() {
    gat_engine::initialize_backends();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    for remote_target in [false, true] {
        let storage = tempfile::tempdir().unwrap();
        let current = test_repo();
        let repo = Repository::at(current.path().to_path_buf());
        let remote_name = remote_target.then(|| use_remote(&repo, storage.path()));
        let orphan = cache_root(&repo)
            .writer()
            .ingest(std::io::Cursor::new(b"orphan"))
            .unwrap()
            .0
            .oid;
        let remote = remote_name.as_ref().map(|_| remote_for(&repo));
        if let Some(remote) = &remote {
            remote
                .write(&object_key_oid(&orphan), b"orphan".to_vec())
                .unwrap();
        }
        let missing =
            GitLocationSpec::from_string(current.path().join("missing").display().to_string());
        let repositories = [missing.clone(), missing];
        let mut options = GcOptions {
            repositories: &repositories,
            dry_run: true,
            unsafe_override: false,
            remote: remote_name.as_ref(),
            history: None,
        };
        let report = repo.garbage_collect(&options, &NoopProgress).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.uncertain, 1);
        assert_eq!(report.incomplete_repositories, 1);
        options.dry_run = false;
        let error = repo.garbage_collect(&options, &NoopProgress).unwrap_err();
        if remote_target {
            assert!(matches!(error, GcError::RemoteDeletionRequiresUnsafe));
        } else {
            assert!(matches!(error, GcError::IncompleteKeepSet { .. }));
        }
        assert!(cache_root(&repo).presence().contains(&orphan));
        if let Some(remote) = &remote {
            assert!(remote.exists(&object_key_oid(&orphan)).unwrap());
        }
        options.unsafe_override = true;
        let report = repo.garbage_collect(&options, &NoopProgress).unwrap();
        assert_eq!(report.deleted, 1);
        assert!(report.forced_incomplete_keep_set);
        assert_eq!(report.incomplete_repositories, 1);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = test_support_git::command(dir, args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn test_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    git(temp.path(), &["init", "-q", "-b", "main"]);
    git(temp.path(), &["config", "core.autocrlf", "false"]);
    std::fs::write(temp.path().join(".git/info/exclude"), ".gat/\n").unwrap();
    commit_all(temp.path(), "initial");
    temp
}

fn commit_all(dir: &Path, message: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
}

fn commit_all_at(dir: &Path, message: &str, seconds: i64) {
    git(dir, &["add", "-A"]);
    let date = format!("@{seconds} +0000");
    let output = test_support_git::command(dir, &["commit", "-q", "--allow-empty", "-m", message])
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .expect("commit with timestamp");
    assert!(output.status.success());
}

fn track(repo: &Repository, root: &Path, path: &str, bytes: &[u8]) -> Oid {
    std::fs::write(root.join(path), bytes).expect("write tracked fixture");
    let config = repo.load_config().expect("load cache config");
    let cache_root = RepositoryLayout::at(root.to_path_buf())
        .resolve_cache_root(None, config.cache.location.as_ref());
    let oid = cache_root
        .writer()
        .ingest(std::io::Cursor::new(bytes))
        .expect("ingest fixture")
        .0
        .oid;
    let layout = gat_io::RepositoryLayout::at(root.to_path_buf());
    let mut lock = LockStore::load_repository(&layout).expect("load lock");
    lock.upsert(GatPath::normalize(path).expect("canonical path"), oid);
    repo.save_lock(&lock).expect("save lock");
    oid
}

fn untrack(repo: &Repository, root: &Path, path: &str) {
    let layout = gat_io::RepositoryLayout::at(root.to_path_buf());
    let mut lock = LockStore::load_repository(&layout).expect("load lock");
    let path = GatPath::parse_canonical(path).expect("canonical path");
    lock.entries.retain(|entry| entry.path != path);
    repo.save_lock(&lock).expect("save lock");
    let _ = std::fs::remove_file(root.join(path.as_str()));
}

fn head(dir: &Path) -> gat_core::git::GitCommitId {
    let layout = gat_io::RepositoryLayout::at(dir.to_path_buf());
    gat_io::resolve_commit(&layout, &GitRevisionSpec::from("HEAD")).expect("resolve HEAD")
}

fn collect(
    repo: &Repository,
    history: &HistorySelection,
    dry_run: bool,
    unsafe_override: bool,
    remote: Option<&RemoteName>,
    progress: &dyn ProgressReporter,
) -> Result<gat_engine::GcReport, GcError> {
    repo.garbage_collect(
        &GcOptions {
            repositories: &[],
            dry_run,
            unsafe_override,
            remote,
            history: Some(history),
        },
        progress,
    )
}

fn use_shared_cache(repo: &Repository, path: &Path) {
    let mut config = repo
        .load_config_scoped(gat_core::config::ConfigScope::Project)
        .expect("load project config");
    config.cache.location = Some(CacheLocation::from_path(path.to_path_buf()));
    repo.save_config_scoped(&config, gat_core::config::ConfigScope::Project)
        .expect("save project config");
}

fn use_remote(repo: &Repository, remote_root: &Path) -> RemoteName {
    let name = RemoteName::from_string("origin".to_string());
    let mut config = repo
        .load_config_scoped(gat_core::config::ConfigScope::Project)
        .expect("load project config");
    config.remotes.default = Some(name.clone());
    config.remotes.by_name.insert(
        name.clone(),
        RemoteUrlTemplate::from_string(gat_io::remote_file_url_for_test(remote_root)).into(),
    );
    repo.save_config_scoped(&config, gat_core::config::ConfigScope::Project)
        .expect("save project config");
    name
}

fn remote_for(repo: &Repository) -> RemoteClient {
    let config = repo.load_config().expect("load effective config");
    let name = config.remotes.default.as_ref().expect("default remote");
    let remote = config.remotes.by_name.get(name).expect("configured remote");
    RemoteClient::open(remote.url.as_template_str()).expect("open remote")
}

#[test]
fn tips_and_depth_bound_the_history_kept_by_gc() {
    let temp = test_repo();
    let repo = Repository::at(temp.path().to_path_buf());
    let oldest = track(&repo, temp.path(), "oldest.bin", b"oldest");
    commit_all(temp.path(), "add oldest");
    untrack(&repo, temp.path(), "oldest.bin");
    commit_all(temp.path(), "remove oldest");
    let middle = track(&repo, temp.path(), "middle.bin", b"middle");
    commit_all(temp.path(), "add middle");
    let newest = track(&repo, temp.path(), "newest.bin", b"newest");
    commit_all(temp.path(), "add newest");
    let revision = GitRevisionSpec::from(head(temp.path()).to_string());

    collect(
        &repo,
        &HistorySelection {
            roots: vec![HistoryRoot::Revision(revision.clone())],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(2),
            },
            ..Default::default()
        },
        false,
        false,
        None,
        &NoopProgress,
    )
    .unwrap();

    assert!(!cache_root(&repo).presence().contains(&oldest));
    assert!(cache_root(&repo).presence().contains(&middle));
    assert!(cache_root(&repo).presence().contains(&newest));

    let old_again = cache_root(&repo)
        .writer()
        .ingest(std::io::Cursor::new(b"oldest"))
        .unwrap()
        .0
        .oid;
    collect(
        &repo,
        &HistorySelection {
            roots: vec![HistoryRoot::Revision(revision)],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        },
        false,
        false,
        None,
        &NoopProgress,
    )
    .unwrap();
    assert!(!cache_root(&repo).presence().contains(&old_again));
}

#[test]
fn depth_is_consumed_before_time_filtering() {
    let temp = test_repo();
    let repo = Repository::at(temp.path().to_path_buf());
    let ancient = track(&repo, temp.path(), "ancient.bin", b"ancient");
    commit_all_at(temp.path(), "add ancient", 1_000);
    untrack(&repo, temp.path(), "ancient.bin");
    commit_all_at(temp.path(), "remove ancient", 500);
    let newest = track(&repo, temp.path(), "newest.bin", b"newest");
    commit_all_at(temp.path(), "add newest", 1_500);

    collect(
        &repo,
        &HistorySelection {
            roots: vec![HistoryRoot::Revision(GitRevisionSpec::from(
                head(temp.path()).to_string(),
            ))],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(2),
            },
            time: TimeWindow {
                since: Some(GitTimestamp::from(900)),
                until: None,
            },
            ..Default::default()
        },
        false,
        false,
        None,
        &NoopProgress,
    )
    .unwrap();

    assert!(!cache_root(&repo).presence().contains(&ancient));
    assert!(cache_root(&repo).presence().contains(&newest));
}

#[test]
fn all_refs_keeps_custom_commit_refs_and_ignores_blob_refs() {
    let temp = test_repo();
    let repo = Repository::at(temp.path().to_path_buf());
    let branch_oid = track(&repo, temp.path(), "branch.bin", b"branch");
    commit_all(temp.path(), "branch");
    let branch_tip = head(temp.path());
    let custom_oid = track(&repo, temp.path(), "custom.bin", b"custom");
    commit_all(temp.path(), "custom");
    let custom_tip = head(temp.path());
    git(
        temp.path(),
        &["update-ref", "refs/custom/kept", &custom_tip.to_string()],
    );
    git(temp.path(), &["reset", "--hard", &branch_tip.to_string()]);
    let mut child = test_support_git::command(temp.path(), &["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"not a commit")
        .unwrap();
    let blob = String::from_utf8(child.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    git(temp.path(), &["update-ref", "refs/custom/blob", &blob]);
    let lock_text = test_support_git::command(temp.path(), &["show", "refs/custom/kept:gat.lock"])
        .output()
        .unwrap();
    assert!(lock_text.status.success());
    assert!(
        String::from_utf8(lock_text.stdout)
            .unwrap()
            .contains(&custom_oid.to_string())
    );

    let selection = HistorySelection {
        roots: vec![HistoryRoot::AllRefs],
        traversal: HistoryTraversal::Ancestors { per_root: None },
        ..Default::default()
    };
    let mut historical = Vec::new();
    repo.visit_history_lock_entries::<Box<dyn std::error::Error>>(
        &selection,
        |_| true,
        |entry| {
            historical.push(entry.oid);
            Ok(())
        },
    )
    .unwrap();
    assert!(historical.contains(&custom_oid));

    collect(&repo, &selection, false, false, None, &NoopProgress).unwrap();

    assert!(cache_root(&repo).presence().contains(&branch_oid));
    assert!(cache_root(&repo).presence().contains(&custom_oid));
}

#[test]
fn dangling_ref_aborts_before_any_object_is_deleted() {
    let temp = test_repo();
    let repo = Repository::at(temp.path().to_path_buf());
    let kept = track(&repo, temp.path(), "kept.bin", b"kept");
    commit_all(temp.path(), "kept");
    let orphan = cache_root(&repo)
        .writer()
        .ingest(std::io::Cursor::new(b"orphan"))
        .unwrap()
        .0
        .oid;
    let ref_path = temp.path().join(".git/refs/gat-tests/dangling");
    std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
    std::fs::write(ref_path, "0123456789abcdef0123456789abcdef01234567\n").unwrap();

    assert!(
        collect(
            &repo,
            &HistorySelection::conservative_default(),
            false,
            false,
            None,
            &NoopProgress,
        )
        .is_err()
    );
    assert!(cache_root(&repo).presence().contains(&kept));
    assert!(cache_root(&repo).presence().contains(&orphan));
}

#[test]
fn current_lock_is_always_kept_across_flat_and_sharded_history() {
    let temp = test_repo();
    let repo = Repository::at(temp.path().to_path_buf());
    let flat = track(&repo, temp.path(), "flat.bin", b"flat");
    commit_all(temp.path(), "flat");
    let mut config = repo
        .load_config_scoped(gat_core::config::ConfigScope::Project)
        .unwrap();
    config.lock.shard_levels = Some(LockShardLevels::new(2).unwrap());
    repo.save_config_scoped(&config, gat_core::config::ConfigScope::Project)
        .unwrap();
    let sharded = track(&repo, temp.path(), "sharded.bin", b"sharded");
    commit_all(temp.path(), "sharded");
    let workspace = track(&repo, temp.path(), "workspace.bin", b"workspace");

    collect(
        &repo,
        &HistorySelection {
            roots: vec![HistoryRoot::Revision(GitRevisionSpec::from(
                head(temp.path()).to_string(),
            ))],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        },
        false,
        false,
        None,
        &NoopProgress,
    )
    .unwrap();

    for oid in [flat, sharded, workspace] {
        assert!(cache_root(&repo).presence().contains(&oid));
    }
}

#[test]
fn shallow_clone_with_crlf_lock_fails_closed() {
    let shared = tempfile::tempdir().unwrap();
    let source = test_repo();
    let source_repo = Repository::at(source.path().to_path_buf());
    use_shared_cache(&source_repo, shared.path());
    track(&source_repo, source.path(), "kept.bin", b"kept");
    commit_all(source.path(), "kept");
    commit_all(source.path(), "new shallow tip");

    let clone_parent = tempfile::tempdir().unwrap();
    let clone = clone_parent.path().join("clone");
    let source_url = gat_io::remote_file_url_for_test(source.path());
    let output = test_support_git::command(
        source.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            "--no-checkout",
            &source_url,
            clone.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert!(output.status.success());
    git(&clone, &["config", "core.autocrlf", "true"]);
    git(&clone, &["checkout", "-q"]);
    let lock_text = std::fs::read_to_string(clone.join("gat.lock")).unwrap();
    assert!(lock_text.contains("\r\n"));
    let repo = Repository::at(clone);

    assert!(matches!(
        collect(
            &repo,
            &HistorySelection::conservative_default(),
            false,
            false,
            None,
            &NoopProgress,
        ),
        Err(GcError::ShallowHistory { .. })
    ));
}

#[derive(Default)]
struct ProgressState {
    operations: Vec<ProgressOperation>,
    positions: Vec<u64>,
    totals: Vec<Option<u64>>,
    activities: Vec<ProgressActivity>,
    active: usize,
    max_active: usize,
}

struct RecordingBackend {
    state: Arc<Mutex<ProgressState>>,
    index: usize,
}

impl ActivityBackend for RecordingBackend {
    fn inc(&self, delta: u64) {
        self.state.lock().unwrap().positions[self.index] += delta;
    }

    fn set_activity(&self, activity: &ProgressActivity) {
        self.state.lock().unwrap().activities.push(activity.clone());
    }

    fn finish(&self) {
        self.state.lock().unwrap().active -= 1;
    }
}

#[derive(Default)]
struct RecordingProgress {
    state: Arc<Mutex<ProgressState>>,
}

impl ProgressReporter for RecordingProgress {
    fn begin(&self, spec: ProgressSpec) -> ProgressTask {
        let mut state = self.state.lock().unwrap();
        let index = state.operations.len();
        state.operations.push(spec.operation());
        state.positions.push(0);
        state.totals.push(spec.total());
        state.active += 1;
        state.max_active = state.max_active.max(state.active);
        drop(state);
        ProgressTask::from_backend(Arc::new(RecordingBackend {
            state: Arc::clone(&self.state),
            index,
        }))
    }
}

#[test]
fn remote_gc_keeps_explicit_peer_and_reuses_one_listing_pass() {
    gat_engine::initialize_backends();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    let remote_root = tempfile::tempdir().unwrap();

    let first = test_repo();
    let first_repo = Repository::at(first.path().to_path_buf());
    let remote_name = use_remote(&first_repo, remote_root.path());
    let first_oid = track(&first_repo, first.path(), "first.bin", b"first");
    commit_all(first.path(), "first");
    let remote = remote_for(&first_repo);
    remote
        .write(&object_key_oid(&first_oid), b"first".to_vec())
        .unwrap();

    let second = test_repo();
    let second_repo = Repository::at(second.path().to_path_buf());
    use_remote(&second_repo, remote_root.path());
    let second_oid = track(&second_repo, second.path(), "second.bin", b"second");
    commit_all(second.path(), "second");
    let second_remote = remote_for(&second_repo);
    second_remote
        .write(&object_key_oid(&second_oid), b"second".to_vec())
        .unwrap();

    let orphan = Oid::from_hex(&"f".repeat(64)).unwrap();
    remote
        .write(&object_key_oid(&orphan), b"orphan".to_vec())
        .unwrap();
    let progress = RecordingProgress::default();

    let repositories = [GitLocationSpec::from_string(
        second.path().display().to_string(),
    )];
    let selection = HistorySelection::conservative_default();
    let report = first_repo
        .garbage_collect(
            &GcOptions {
                repositories: &repositories,
                dry_run: false,
                unsafe_override: true,
                remote: Some(&remote_name),
                history: Some(&selection),
            },
            &progress,
        )
        .unwrap();

    assert_eq!(report.deleted, 1);
    assert!(remote.exists(&object_key_oid(&first_oid)).unwrap());
    assert!(remote.exists(&object_key_oid(&second_oid)).unwrap());
    assert!(!remote.exists(&object_key_oid(&orphan)).unwrap());
    let state = progress.state.lock().unwrap();
    assert_eq!(
        state.operations,
        vec![
            ProgressOperation::ComputingReachability,
            ProgressOperation::ListingRemoteObjects,
            ProgressOperation::GarbageCollecting,
        ]
    );
    assert_eq!(state.max_active, 1);
    assert_eq!(state.positions[1], 3);
    assert_eq!(state.totals[2], Some(state.positions[1]));
    assert_eq!(state.positions[2], 3);
    assert!(
        state
            .activities
            .iter()
            .any(|activity| matches!(activity, ProgressActivity::DeletingRemoteObjects))
    );
    assert!(
        state
            .activities
            .iter()
            .any(|activity| matches!(activity, ProgressActivity::CloningSource { .. }))
    );
}

#[test]
fn no_history_gc_rejects_malformed_current_lock() {
    let temp = test_repo();
    let repo = Repository::at(temp.path().to_path_buf());
    let orphan = track(&repo, temp.path(), "file.bin", b"payload");
    std::fs::write(temp.path().join("gat.lock"), "invalid lock contents\n").unwrap();
    for dry_run in [true, false] {
        assert!(
            repo.garbage_collect(
                &GcOptions {
                    repositories: &[],
                    dry_run,
                    unsafe_override: false,
                    remote: None,
                    history: None,
                },
                &NoopProgress
            )
            .is_err()
        );
        assert!(cache_root(&repo).presence().contains(&orphan));
    }
}
