//! Root integration characterization for typed remote-status commands.

use gat_core::history::HistorySelection;
use gat_core::name::RemoteName;
use gat_core::selection::Selection;
use gat_engine::{DesiredOperation, Repository as Repo};

mod common;

use common::RecordingProgress;
use gat_core::path_scope::normalize_path_scope;
use gat_core::progress::{NoopProgress, ProgressActivity, ProgressOperation, ProgressReporter};
use gat_engine::test_support as engine_test_support;
use gat_io::object_key_oid;
use std::path::{Path, PathBuf};

use ::test_support::add;
use test_support::{git_repo_with_initial_commit as test_repo, remote_add_with_default};
use test_support_git::commit_all;

fn gp(path: &str) -> gat_core::lexical_path::GatPath {
    gat_core::lexical_path::GatPath::parse_canonical(path).unwrap()
}

fn layout(root: &Path) -> gat_io::RepositoryLayout {
    gat_io::RepositoryLayout::at(root.to_path_buf())
}

fn scoped_selection(path: &Path) -> Selection {
    Selection::from_scope_patterns(normalize_path_scope(path).unwrap(), Vec::new(), Vec::new())
}

const fn request<'a>(
    selection: &'a Selection,
    remote: Option<&'a RemoteName>,
    history: Option<&'a HistorySelection>,
) -> gat_command::RemoteStatusRequest<'a> {
    gat_command::RemoteStatusRequest {
        selection: Some(selection),
        remote,
        history,
    }
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

#[test]
fn current_state_remote_status_reports_the_one_unpushed_object() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"a-content").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"b-content").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();
    // deliberately no commit: current-state remote status must see
    // `gat add`ed files immediately.

    let selection = scoped_selection(Path::new("a.bin"));
    push(&repo, Some(&selection), None, &NoopProgress).unwrap();

    let selection = Selection::root();
    let outcome = gat_command::remote_status(
        &repo,
        gat_command::RemoteStatusRequest {
            selection: Some(&selection),
            remote: None,
            history: None,
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(outcome.checked, 2);
    assert_eq!(outcome.missing.len(), 1);
    assert_eq!(outcome.missing[0].object.representative_path, "b.bin");
    assert!(!outcome.shallow);
}

#[test]
fn current_state_remote_status_path_scope_restricts_the_selection() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    std::fs::create_dir_all(tmp.path().join("data")).unwrap();
    std::fs::write(tmp.path().join("data/a.bin"), b"a-content").unwrap();
    std::fs::write(tmp.path().join("other.bin"), b"other-content").unwrap();
    add(
        &repo,
        &[PathBuf::from("data/a.bin"), PathBuf::from("other.bin")],
        &NoopProgress,
    )
    .unwrap();

    let selection = scoped_selection(Path::new("data"));
    let outcome =
        gat_command::remote_status(&repo, request(&selection, None, None), &NoopProgress).unwrap();
    assert_eq!(outcome.checked, 1);
    assert_eq!(outcome.missing.len(), 1);
    assert_eq!(outcome.missing[0].object.representative_path, "data/a.bin");
}

/// An explicit override is validated without initializing a remote when
/// the selection contains no work.
#[test]
fn remote_status_explicit_name_validates_without_opening_for_an_empty_selection() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    // Deliberately no `gat add`: the selection has nothing to check.

    let before = engine_test_support::remote_open_count_for("origin");
    let selection = Selection::root();
    let remote = RemoteName::from_string("origin".to_string());
    let outcome = gat_command::remote_status(
        &repo,
        request(&selection, Some(&remote), None),
        &NoopProgress,
    )
    .unwrap();
    let after = engine_test_support::remote_open_count_for("origin");

    assert_eq!(outcome.checked, 0);
    assert_eq!(
        after - before,
        0,
        "an explicit override with no work must stay on the network-free validation path"
    );
}

/// Route-consistent remote resolution applied to `status`
/// the same way it applies to `push`/`fetch`): a bare `--remote`
/// routes each selected path independently and may need several
/// remotes or none at all, so it must never eagerly open a remote before the selection is
/// known to actually need one. An empty selection performs no
/// path-based remote work and opens nothing, even though a
/// `remotes.default` remote is configured.
#[test]
fn remote_status_bare_flag_never_opens_a_remote_for_an_empty_selection() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();
    // Deliberately no `gat add`: the selection has nothing to check.

    let before = engine_test_support::remote_open_count_for("origin");
    let selection = Selection::root();
    let outcome =
        gat_command::remote_status(&repo, request(&selection, None, None), &NoopProgress).unwrap();
    let after = engine_test_support::remote_open_count_for("origin");

    assert_eq!(outcome.checked, 0);
    assert_eq!(
        after, before,
        "a bare `--remote` with nothing to route through any remote must not \
         open any remote at all"
    );
}

#[test]
fn history_selection_reports_an_older_revision_missing_while_current_is_clean() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"version-a").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version a");

    std::fs::write(tmp.path().join("a.bin"), b"version-b-longer").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    commit_all(tmp.path(), "version b");

    // Only the current (B) content is on the remote.
    push(&repo, None, None, &NoopProgress).unwrap();

    let path_selection = Selection::root();
    let current =
        gat_command::remote_status(&repo, request(&path_selection, None, None), &NoopProgress)
            .unwrap();
    assert_eq!(current.missing.len(), 0);

    let selection = gat_core::history::HistorySelection {
        roots: vec![gat_core::history::HistoryRoot::Revision(
            "HEAD~1".to_string().into(),
        )],
        ..Default::default()
    };
    let path_selection = Selection::root();
    let historical = gat_command::remote_status(
        &repo,
        gat_command::RemoteStatusRequest {
            selection: Some(&path_selection),
            remote: None,
            history: Some(&selection),
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(historical.checked, 1);
    assert_eq!(historical.missing.len(), 1);
    assert_eq!(historical.missing[0].object.representative_path, "a.bin");
}

#[test]
fn duplicate_oid_across_selected_rows_produces_one_selected_object() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    let url = gat_io::remote_file_url_for_test(remote_dir.path());
    remote_add_with_default(&repo, "origin", url).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"same-content").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"same-content").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();

    let selection = Selection::root();
    let outcome =
        gat_command::remote_status(&repo, request(&selection, None, None), &NoopProgress).unwrap();
    assert_eq!(outcome.checked, 1);
    assert_eq!(outcome.missing.len(), 1);
}

#[test]
fn remote_status_progress_reports_selection_resolution_and_remote_checks() {
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
    commit_all(tmp.path(), "add a.bin");
    let progress = RecordingProgress::new();
    let selection = gat_core::history::HistorySelection {
        roots: vec![gat_core::history::HistoryRoot::Revision(
            "HEAD".to_string().into(),
        )],
        ..Default::default()
    };

    let path_selection = Selection::root();
    let outcome = gat_command::remote_status(
        &repo,
        request(&path_selection, None, Some(&selection)),
        &progress,
    )
    .unwrap();

    // File remotes have no readiness phase. Selection and object checks
    // share the current logical task without a Connecting activity.
    assert_eq!(progress.count_of(ProgressOperation::RemoteStatus), 1);
    let task = progress.only(ProgressOperation::RemoteStatus);
    assert_eq!(
        task.activities,
        vec![
            ProgressActivity::ResolvingSelection,
            ProgressActivity::CheckingRemote,
            ProgressActivity::CheckedRemoteObject {
                path: gat_core::lexical_path::GatPath::normalize("a.bin").unwrap(),
            },
        ]
    );
    assert_eq!(task.position, outcome.checked as u64);
    assert_eq!(progress.max_active_tasks(), 1);
}

/// `missing_from_remote` increments
/// the shared `RemoteStatus` handle exactly once per object it
/// actually checks, so the task's final position must equal the
/// exact number of unique objects checked -- never doubled by an
/// additional `checking_remote.inc(window_len)` per flushed window.
/// Repeated across several tiny window sizes so no window size can
/// hide an accidental double-count that only a production-sized
/// window would happen to mask.
#[test]
fn remote_status_position_equals_checked_count_and_is_never_doubled() {
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

    // Grow one repository between cases; each operation still checks the
    // original number of objects across three full windows and a tail.
    let mut seeded = 0;
    for window in [1usize, 2, 3, 5] {
        let count = window * 3 + 1;
        let mut paths = Vec::new();
        for i in seeded..count {
            let path = format!("obj-{i}.bin");
            std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
            paths.push(PathBuf::from(path));
        }
        add(&repo, &paths, &NoopProgress).unwrap();
        seeded = count;

        let limits = ExecutionLimits::for_test(window, 10_000, 4096, 8, 4);
        let progress = RecordingProgress::new();
        let config_loads_before = gat_engine::test_support::config_loads();
        let mut desired_op =
            DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
        let selection = Selection::root();
        let outcome = gat_command::remote_status_with_desired_operation(
            &mut desired_op,
            request(&selection, None, None),
            &progress,
        )
        .unwrap();

        let task = progress.only(ProgressOperation::RemoteStatus);
        assert_eq!(
            task.position, outcome.checked as u64,
            "window size {window}: final position must equal the exact checked count, \
             never a multiple of it"
        );
        assert_eq!(outcome.checked, count);
        assert_eq!(
            gat_engine::test_support::config_loads() - config_loads_before,
            1,
            "window size {window}: one remote-status operation must load effective config once"
        );
    }
}

/// An end-to-end `gat status --remote`,
/// driven under deliberately tiny positive `ExecutionLimits` over a
/// selection large enough to span several transfer-planning windows,
/// with some objects present on the remote and some missing spread
/// across window boundaries, must still report the exact correct
/// missing set -- the same observable behavior a production-limits
/// run produces -- proving the bounded-window consolidation preserves
/// pre-consolidation command output across every window boundary.
#[test]
fn remote_status_with_tiny_limits_reports_the_correct_missing_set_across_many_windows() {
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
    let count = window * 2 + 1;
    let mut paths = Vec::new();
    for i in 0..count {
        let path = format!("obj-{i}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();
    push(&repo, None, None, &NoopProgress).unwrap();

    // After the push, remove every other object's file straight from
    // the remote's storage directory, so "missing from remote"
    // entries land on both sides of at least one window boundary
    // rather than all being contiguous.
    let expected_missing: std::collections::BTreeSet<usize> =
        (0..count).filter(|i| i % 2 != 0).collect();
    for i in &expected_missing {
        let oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries
            .iter()
            .find(|e| e.path == format!("obj-{i}.bin"))
            .unwrap()
            .oid;
        let key = object_key_oid(&oid);
        let _ = std::fs::remove_file(remote_dir.path().join(key));
    }

    let mut desired_op =
        DesiredOperation::acquire_with_limits(&repo, &NoopProgress, limits).unwrap();
    let selection = Selection::root();
    let outcome = gat_command::remote_status_with_desired_operation(
        &mut desired_op,
        request(&selection, None, None),
        &NoopProgress,
    )
    .unwrap();

    assert_eq!(outcome.checked, count);
    let reported_missing: std::collections::BTreeSet<usize> = outcome
        .missing
        .iter()
        .map(|obj| {
            obj.object
                .representative_path
                .as_str()
                .trim_start_matches("obj-")
                .trim_end_matches(".bin")
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(
        reported_missing, expected_missing,
        "the exact missing set must be correctly reported across every tiny-limits window"
    );
}

/// The engine scheduler can report results out of input order (covered by
/// its deterministic reversed-completion unit test). This command-level
/// test covers the other half of the contract: a concurrent status window
/// still emits `missing` in original selection order, never callback order.
#[test]
fn remote_status_missing_output_preserves_selection_order_regardless_of_presence_completion_order()
{
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

    let count = 16;
    let mut paths = Vec::new();
    for i in 0..count {
        let path = format!("obj-{i:02}.bin");
        std::fs::write(tmp.path().join(&path), format!("payload-{i}").as_bytes()).unwrap();
        paths.push(PathBuf::from(path));
    }
    add(&repo, &paths, &NoopProgress).unwrap();

    // Every object is missing from the remote (nothing pushed), so
    // `missing` covers the entire selection: any accidental
    // completion-order leak would be immediately visible as a
    // non-ascending sequence.
    let selection = Selection::root();
    let outcome =
        gat_command::remote_status(&repo, request(&selection, None, None), &NoopProgress).unwrap();

    assert_eq!(outcome.checked, count);
    assert_eq!(outcome.missing.len(), count);
    let reported_order: Vec<usize> = outcome
        .missing
        .iter()
        .map(|obj| {
            obj.object
                .representative_path
                .as_str()
                .trim_start_matches("obj-")
                .trim_end_matches(".bin")
                .parse()
                .unwrap()
        })
        .collect();
    let expected_order: Vec<usize> = (0..count).collect();
    assert_eq!(
        reported_order, expected_order,
        "missing objects must be reported in original selection order, \
         never in remote-presence completion order"
    );
}

/// Route-consistent remote resolution: a repository with only
/// per-path routes configured (no `remotes.default` at all) must
/// still support a bare `gat status --remote` -- the same
/// `EffectivePathPolicy::resolved_remote_for_path` precedence
/// `push`/`fetch` already rely on applies here too, so the absence
/// of a default remote is not an error as long as every selected
/// path resolves through some route.
#[test]
fn remote_status_resolves_a_route_only_repository_with_no_default_remote() {
    use gat_core::config::RouteConfig;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());
    let remote_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "routed",
        gat_io::remote_file_url_for_test(remote_dir.path()),
    )
    .unwrap();

    let mut cfg = repo.load_config().unwrap();
    cfg.remotes.default = None;
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("a.bin".to_string()),
        RouteConfig {
            path: gp("a.bin"),
            remote: gat_core::name::RemoteName::from_string("routed".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();

    let selection = Selection::root();
    let outcome =
        gat_command::remote_status(&repo, request(&selection, None, None), &NoopProgress).unwrap();
    assert_eq!(outcome.checked, 1);
    assert_eq!(outcome.missing.len(), 1);
    assert_eq!(outcome.missing[0].object.representative_path, "a.bin");
    assert_eq!(outcome.missing[0].remote_name.as_str(), "routed");
}

/// Route-consistent remote resolution: a path with no configured
/// route and no `remotes.default` produces the same actionable,
/// path-specific "no remote configured" error `push`/`fetch` already
/// surface, not a generic/blank failure.
#[test]
fn remote_status_errors_on_a_path_with_no_route_and_no_default_remote() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
    // Deliberately no remote configured at all.

    let selection = Selection::root();
    let err = gat_command::remote_status(&repo, request(&selection, None, None), &NoopProgress)
        .unwrap_err();
    assert!(
        err.to_string().contains("a.bin"),
        "error must name the specific unrouted path, got: {err}"
    );
}

/// Route-consistent remote resolution: the same oid selected via two
/// paths that route to two *different* remotes must produce two
/// distinct `(remote, oid)` checks (never deduplicated purely by
/// oid), while missing/present status is tracked independently per
/// remote.
#[test]
fn remote_status_same_oid_on_two_routes_produces_two_distinct_checks() {
    use gat_core::config::RouteConfig;

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

    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("b.bin".to_string()),
        RouteConfig {
            path: gp("b.bin"),
            remote: gat_core::name::RemoteName::from_string("backup".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"shared-bytes").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"shared-bytes").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();

    // Only present on `origin`, so `a.bin` (routed to `origin`) is
    // clean while `b.bin` (routed to `backup`) is still missing --
    // proving the two routes were checked independently rather than
    // deduplicated by oid alone.
    let selection = scoped_selection(Path::new("a.bin"));
    push(&repo, Some(&selection), None, &NoopProgress).unwrap();

    let selection = Selection::root();
    let outcome =
        gat_command::remote_status(&repo, request(&selection, None, None), &NoopProgress).unwrap();
    assert_eq!(
        outcome.checked, 2,
        "the same oid routed to two different remotes must count as two checks"
    );
    assert_eq!(outcome.missing.len(), 1);
    assert_eq!(outcome.missing[0].object.representative_path, "b.bin");
    assert_eq!(outcome.missing[0].remote_name.as_str(), "backup");
}

/// Route-consistent remote resolution: a configured remote that no
/// selected path routes to must never be opened by a bare `status
/// --remote`, mirroring `push_never_opens_a_configured_remote_that_no_object_routes_to`.
#[test]
fn remote_status_never_opens_a_configured_remote_that_no_object_routes_to() {
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

    let origin_before = engine_test_support::remote_open_count_for("origin");
    let unused_before = engine_test_support::remote_open_count_for("unused");

    let selection = Selection::root();
    gat_command::remote_status(&repo, request(&selection, None, None), &NoopProgress).unwrap();

    assert_eq!(
        engine_test_support::remote_open_count_for("origin") - origin_before,
        1,
        "the default `origin` remote must be opened exactly once for the one checked object"
    );
    assert_eq!(
        engine_test_support::remote_open_count_for("unused") - unused_before,
        0,
        "a configured remote with no routed/default obligation must never be opened"
    );
}

/// An explicit `gat status --remote NAME` naming an unknown remote
/// must fail with an actionable error naming that remote, the same
/// as `push`/`fetch`'s explicit-name validation.
#[test]
fn remote_status_explicit_unknown_remote_name_errors() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let selection = Selection::root();
    let remote = RemoteName::from_string("does-not-exist".to_string());
    let err = gat_command::remote_status(
        &repo,
        request(&selection, Some(&remote), None),
        &NoopProgress,
    )
    .unwrap_err();
    assert!(err.to_string().contains("does-not-exist"));
}

/// Route-consistent remote resolution: an explicit `--remote NAME`
/// has strictly higher precedence than path routing, exactly as it
/// does for `push`/`fetch`. With two paths routed to two different
/// configured remotes, `gat status --remote override` must check
/// *both* against the explicitly named `override` remote, never
/// against either path's configured route -- opening `override` and
/// leaving both routed remotes untouched.
#[test]
fn remote_status_explicit_name_overrides_both_paths_configured_routes() {
    use gat_core::config::RouteConfig;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    opendal::init_default_registry();
    let tmp = test_repo();
    let repo = Repo::at(tmp.path().to_path_buf());

    let first_route_dir = tempfile::tempdir().unwrap();
    let second_route_dir = tempfile::tempdir().unwrap();
    let override_dir = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "route-a",
        gat_io::remote_file_url_for_test(first_route_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "route-b",
        gat_io::remote_file_url_for_test(second_route_dir.path()),
    )
    .unwrap();
    remote_add_with_default(
        &repo,
        "override",
        gat_io::remote_file_url_for_test(override_dir.path()),
    )
    .unwrap();

    let mut cfg = repo.load_config().unwrap();
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("a.bin".to_string()),
        RouteConfig {
            path: gp("a.bin"),
            remote: gat_core::name::RemoteName::from_string("route-a".to_string()),
        },
    );
    cfg.routes.by_name.insert(
        gat_core::name::RouteName::from_string("b.bin".to_string()),
        RouteConfig {
            path: gp("b.bin"),
            remote: gat_core::name::RemoteName::from_string("route-b".to_string()),
        },
    );
    repo.save_config(&cfg).unwrap();

    std::fs::write(tmp.path().join("a.bin"), b"a-content").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"b-content").unwrap();
    add(
        &repo,
        &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
        &NoopProgress,
    )
    .unwrap();
    // Deliberately never pushed anywhere: both objects are missing
    // from every remote, including `override`.

    let first_route_opens_before = engine_test_support::remote_open_count_for("route-a");
    let second_route_opens_before = engine_test_support::remote_open_count_for("route-b");
    let override_before = engine_test_support::remote_open_count_for("override");

    let selection = Selection::root();
    let remote = RemoteName::from_string("override".to_string());
    let outcome = gat_command::remote_status(
        &repo,
        request(&selection, Some(&remote), None),
        &NoopProgress,
    )
    .unwrap();

    assert_eq!(
        outcome.checked, 2,
        "both selected objects must be checked, both against the override remote"
    );
    assert_eq!(outcome.missing.len(), 2);
    for missing in &outcome.missing {
        assert_eq!(
            missing.remote_name.as_str(),
            "override",
            "an explicit --remote NAME must override both paths' configured routes"
        );
        assert_eq!(
            missing.route, None,
            "an explicit override is not a route -- route must be None"
        );
    }

    assert_eq!(
        engine_test_support::remote_open_count_for("override") - override_before,
        1,
        "the explicitly named override remote must be opened exactly once"
    );
    assert_eq!(
        engine_test_support::remote_open_count_for("route-a") - first_route_opens_before,
        0,
        "a.bin's configured route must never be opened when --remote override wins"
    );
    assert_eq!(
        engine_test_support::remote_open_count_for("route-b") - second_route_opens_before,
        0,
        "b.bin's configured route must never be opened when --remote override wins"
    );
}
