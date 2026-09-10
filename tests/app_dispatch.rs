//! In-process integration tests for `gat::app::run`: the dispatch/policy
//! layer between argument parsing and command implementations. Unlike
//! `tests/cli_integration.rs` (which spawns the compiled `gat` binary and
//! checks streams/exit codes) these tests call `app::run` directly with a
//! parsed [`Cli`] and an explicit [`Context`], so they can assert on the
//! structured [`Outcome`] itself -- dispatch/policy behavior independent
//! of terminal rendering -- without paying for a process spawn.

use gat::app::{self, Context};
use gat::cli::Cli;
use gat::progress::NoopProgress;
use gat_command::{
    ConfigOutcome, ConfigScalarValue, DiffOutcome, DiffTarget, MountOutcome, RemoteOutcome,
    RouteOutcome, StatusOutcome, StatusRequest,
};
use gat_core::config::ConfigScope;
use gat_core::path_scope;
use gat_engine::Repository as Repo;

#[path = "common/mod.rs"]
mod common;
use common::{commit_all, test_repo};

/// Delegates to the shared `test-support` crate's `parse_cli`, so this
/// file doesn't keep its own duplicate of the fixed `"gat"` argv[0]
/// prepending logic.
fn parse(args: &[&str]) -> Cli {
    test_support::parse_cli(args)
}

fn add(repo: &Repo, paths: &[std::path::PathBuf]) {
    let paths = paths
        .iter()
        .map(path_scope::normalize_path_scope)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    gat_command::add(
        repo,
        gat_command::AddRequest {
            paths,
            force: false,
        },
        &NoopProgress,
    )
    .unwrap();
}

/// `app::run` dispatches `gat status` (on a repo with nothing tracked
/// yet) to `gat-command`, returning `Outcome::Status` wrapping
/// `StatusOutcome::NoTrackedFiles` -- exercised through the same
/// dispatch path `main.rs` uses, not by calling `gat_command::status`
/// directly.
#[test]
fn status_dispatches_to_status_outcome() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);
    let cli = parse(&["status"]);

    let result = app::run(cli, &context, &NoopProgress);
    let outcome = result.unwrap();

    match outcome {
        app::Outcome::Status(StatusOutcome::NoTrackedFiles) => {}
        _ => panic!("expected Outcome::Status(NoTrackedFiles)"),
    }
}

#[test]
fn remote_dispatch_converts_cli_values_to_a_typed_command_outcome() {
    let tmp = test_repo();
    let remote = tempfile::tempdir().unwrap();
    let url = test_support_git::file_remote_url(remote.path());
    let context = Context::new(
        gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf()),
    );

    let outcome = app::run(
        parse(&["remote", "add", "origin", &url]),
        &context,
        &NoopProgress,
    )
    .unwrap();

    let app::Outcome::Remote(RemoteOutcome::Added {
        name,
        url: configured,
    }) = outcome
    else {
        panic!("expected Outcome::Remote(Added)");
    };
    assert_eq!(name.as_str(), "origin");
    assert_eq!(configured.as_template_str(), url);
}

#[test]
fn route_dispatch_normalizes_cli_values_once_into_a_typed_command_request() {
    let tmp = test_repo();
    let remote = tempfile::tempdir().unwrap();
    let url = test_support_git::file_remote_url(remote.path());
    let root = tmp.path().to_path_buf();
    let context = Context::new(
        gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(root.clone()),
    );
    app::run(
        parse(&["remote", "add", "archive", &url]),
        &context,
        &NoopProgress,
    )
    .unwrap();

    let config_loads_before = gat_engine::test_support::config_loads();
    let scoped_loads_before = gat_engine::test_support::scoped_config_loads();
    let outcome = app::run(
        parse(&[
            "route",
            "add",
            "models",
            "archive",
            "./vendor//models/",
            "--local",
        ]),
        &context,
        &NoopProgress,
    )
    .unwrap();

    assert!(matches!(
        outcome,
        app::Outcome::Route(RouteOutcome::Added { name, path, remote })
            if name.as_str() == "models"
                && path.as_str() == "vendor/models"
                && remote.as_str() == "archive"
    ));
    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1,
        "route add should read the effective config once"
    );
    assert_eq!(
        gat_engine::test_support::scoped_config_loads() - scoped_loads_before,
        0,
        "route add should reuse the selected scope from its layer snapshot"
    );
    assert!(
        gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(root)
            .load_config_scoped(ConfigScope::Local)
            .unwrap()
            .routes
            .by_name
            .contains_key("models")
    );
}

#[test]
fn config_dispatch_converts_cli_values_to_a_typed_command_outcome() {
    let tmp = test_repo();
    let context = Context::new(
        gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf()),
    );

    let outcome = app::run(
        parse(&["config", "sync.auto_fetch", "true"]),
        &context,
        &NoopProgress,
    )
    .unwrap();

    let app::Outcome::Configured(ConfigOutcome::Set { key, value }) = outcome else {
        panic!("expected Outcome::Configured(Set)");
    };
    assert_eq!(key.as_str(), "sync.auto_fetch");
    assert_eq!(value, ConfigScalarValue::Boolean(true));
}

#[test]
fn mount_dispatch_converts_cli_values_and_uses_typed_outcomes() {
    let source = test_repo();
    std::fs::write(source.path().join("model.bin"), b"payload").unwrap();
    let source_repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(source.path().to_path_buf());
    add(&source_repo, &[std::path::PathBuf::from("model.bin")]);
    commit_all(source.path(), "track source snapshot");

    let destination = test_repo();
    let context = Context::new(
        gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(destination.path().to_path_buf()),
    );
    let source_location = source.path().to_string_lossy().into_owned();
    let config_loads_before = gat_engine::test_support::config_loads();
    let outcome = app::run(
        parse(&["mount", "add", "models", &source_location, "vendor/models"]),
        &context,
        &NoopProgress,
    )
    .unwrap();

    let app::Outcome::Mount(MountOutcome::Added {
        name,
        target,
        entries,
        ..
    }) = outcome
    else {
        panic!("expected Outcome::Mount(Added)");
    };
    assert_eq!(name.as_str(), "models");
    assert_eq!(target.as_str(), "vendor/models");
    assert_eq!(entries, 1);
    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        2,
        "mount add should read once before source preparation and once after locking"
    );

    let config_loads_before = gat_engine::test_support::config_loads();
    let listed = app::run(parse(&["mount", "list"]), &context, &NoopProgress).unwrap();
    let app::Outcome::Mount(MountOutcome::List(records)) = listed else {
        panic!("expected Outcome::Mount(List)");
    };
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name.as_str(), "models");
    assert_eq!(records[0].target.as_str(), "vendor/models");
    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1,
        "mount list should read the config layers once"
    );

    let config_loads_before = gat_engine::test_support::config_loads();
    let updated = app::run(
        parse(&["mount", "update", "models"]),
        &context,
        &NoopProgress,
    )
    .unwrap();
    let app::Outcome::Mount(MountOutcome::Updated {
        name,
        target,
        removed,
        added,
        ..
    }) = updated
    else {
        panic!("expected Outcome::Mount(Updated)");
    };
    assert_eq!(name.as_str(), "models");
    assert_eq!(target.as_str(), "vendor/models");
    assert_eq!((removed, added), (1, 1));
    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        2,
        "mount update should read once before source preparation and once after locking"
    );

    let config_loads_before = gat_engine::test_support::config_loads();
    let shown = app::run(parse(&["mount", "show", "models"]), &context, &NoopProgress).unwrap();
    let app::Outcome::Mount(MountOutcome::Show(details)) = shown else {
        panic!("expected Outcome::Mount(Show)");
    };
    assert_eq!(details.name.as_str(), "models");
    assert_eq!(details.target.as_str(), "vendor/models");
    assert_eq!(details.tracked_rows, 1);
    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1,
        "mount show should read the config layers once"
    );

    let config_loads_before = gat_engine::test_support::config_loads();
    let removed = app::run(
        parse(&["mount", "remove", "models"]),
        &context,
        &NoopProgress,
    )
    .unwrap();
    let app::Outcome::Mount(MountOutcome::Removed {
        name,
        target,
        owned_rows,
        detach_only,
        ..
    }) = removed
    else {
        panic!("expected Outcome::Mount(Removed)");
    };
    assert_eq!(name.as_str(), "models");
    assert_eq!(target.as_str(), "vendor/models");
    assert_eq!(owned_rows, 1);
    assert!(!detach_only);
    assert_eq!(
        gat_engine::test_support::config_loads() - config_loads_before,
        1,
        "mount remove should read the config layers once"
    );
}

#[test]
fn diff_dispatches_with_semantic_revision_targets() {
    let tmp = test_repo();
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    add(&repo, &[std::path::PathBuf::from("big.bin")]);
    commit_all(tmp.path(), "track big.bin");
    let context = Context::new(repo);

    let outcome = app::run(parse(&["diff"]), &context, &NoopProgress).unwrap();

    assert_eq!(
        match outcome {
            app::Outcome::Diff(DiffOutcome::NoChanges { from, to, .. }) => {
                (from.as_str().to_owned(), to)
            }
            _ => panic!("expected Outcome::Diff(NoChanges)"),
        },
        ("HEAD".to_owned(), DiffTarget::WorkingTree)
    );
}

/// `gat add` then `gat status` round-trips through `app::run` twice
/// against the same `Context`, showing a tracked file with no pending
/// changes -- dispatch composes across invocations the same way separate
/// `gat` process invocations would.
#[test]
fn add_then_status_round_trips_through_dispatch() {
    let tmp = test_repo();
    std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let add_result = app::run(parse(&["add", "big.bin"]), &context, &NoopProgress);
    let add_outcome = add_result.unwrap();
    assert!(matches!(add_outcome, app::Outcome::Added(_)));

    let status_result = app::run(parse(&["status"]), &context, &NoopProgress);
    let status_outcome = status_result.unwrap();
    match status_outcome {
        app::Outcome::Status(StatusOutcome::WorkingTree { rows, changes, .. }) => {
            assert_eq!(rows.len(), 1);
            // Not yet `git add`ed/committed, so the staged `gat.lock`
            // doesn't have this entry yet -- it shows as one pending
            // change, same as `git status` would show an uncommitted
            // file.
            assert_eq!(changes, 1);
        }
        _ => panic!("expected Outcome::Status(WorkingTree)"),
    }
}

/// `gat sync --trust-state` resolves to trust-state validation even when
/// `gat.yaml` doesn't set `sync.trust_state` -- explicit CLI flags win
/// over config, per `app::run`'s sync validation policy.
#[test]
fn sync_trust_state_flag_overrides_unset_config() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(parse(&["sync", "--trust-state"]), &context, &NoopProgress);
    let outcome = result.unwrap();

    assert!(matches!(outcome, app::Outcome::Synced(_)));
}

/// Successful dispatch and successful completion are separate policies.
#[test]
fn clean_sync_has_a_successful_exit_code() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(parse(&["sync"]), &context, &NoopProgress);
    let outcome = result.unwrap();
    assert_eq!(app::exit_code(&outcome), 0);
}

/// `gat push --remote <name>` with an unknown, explicitly-named remote
/// fails inside `gat_command::push` (or the config lookup it does);
/// `app::run` propagates that error unchanged rather than converting it
/// into some other `Outcome` variant. (An empty-selection bare `push`
/// with no remote configured at all succeeds; see
/// route-consistent remote resolution, so this test exercises
/// the still-erroring explicit-name case instead.)
#[test]
fn dispatch_propagates_command_errors() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(
        parse(&["push", "--remote", "does-not-exist"]),
        &context,
        &NoopProgress,
    );
    let Err(err) = result else {
        panic!("expected an error");
    };
    assert!(!err.diagnostic().summary().is_empty());
}

/// `gat sync --dry-run --fetch` is rejected before any fetch is attempted
/// (there is no remote configured, so a real fetch attempt would fail
/// with a different error): `--dry-run` never fetches,
/// and an explicit `--fetch` alongside it is a clear usage error rather
/// than a silently-ignored flag.
#[test]
fn sync_dry_run_rejects_explicit_fetch() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(
        parse(&["sync", "--dry-run", "--fetch"]),
        &context,
        &NoopProgress,
    );

    let Err(err) = result else {
        panic!("expected --dry-run --fetch to be rejected");
    };
    let diagnostic = err.diagnostic();
    assert!(diagnostic.summary().contains("--dry-run"), "{diagnostic:?}");
    assert!(diagnostic.summary().contains("--fetch"), "{diagnostic:?}");
}

/// `gat sync --dry-run --repair` is likewise rejected: dry-run never
/// repairs, so combining it with an explicit `--repair` is a clear usage
/// error instead of a silently-ignored flag.
#[test]
fn sync_dry_run_rejects_explicit_repair() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(
        parse(&["sync", "--dry-run", "--repair"]),
        &context,
        &NoopProgress,
    );

    let Err(err) = result else {
        panic!("expected --dry-run --repair to be rejected");
    };
    let diagnostic = err.diagnostic();
    assert!(diagnostic.summary().contains("--dry-run"), "{diagnostic:?}");
    assert!(diagnostic.summary().contains("--repair"), "{diagnostic:?}");
}

/// `gat status --rev <rev>` (any history-selection flag) without
/// `--remote` is rejected as an app-level policy error: local `gat
/// status` has no history concept. The resulting `Failure`'s diagnostic
/// is specific and stable (the final
/// user diagnostics are stable and low-level-free"), and mentions
/// `--remote` as the fix, without ever needing a raw error string.
#[test]
fn status_history_flag_without_remote_is_rejected() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(parse(&["status", "--rev", "HEAD"]), &context, &NoopProgress);

    let Err(err) = result else {
        panic!("expected a history-flag-without-remote error");
    };
    let diagnostic = err.diagnostic();
    assert_eq!(diagnostic.summary(), "History flags require `--remote`");
    assert!(
        diagnostic
            .detail()
            .is_some_and(|detail| detail.contains("history")),
        "{diagnostic:?}"
    );
}

/// The same history-flag-without-remote policy error, reached with a
/// different history flag (`--branches`) and `Diagnostic::hints()`
/// asserted directly, exercising `AppError::HistoryRequiresRemote`'s
/// `Failure` conversion independently of the specific flag combination
/// reaches it.
#[test]
fn status_history_flag_diagnostic_has_an_actionable_hint() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(parse(&["status", "--branches"]), &context, &NoopProgress);

    let Err(err) = result else {
        panic!("expected a history-flag-without-remote error");
    };
    let diagnostic = err.diagnostic();
    assert_eq!(diagnostic.hints().len(), 1);
    assert!(diagnostic.hints()[0].contains("--remote"), "{diagnostic:?}");
}

/// `gat sync --dry-run` with `sync.auto_fetch=true` configured must not
/// fetch: unlike an explicit `--fetch`, the implicit config-driven fetch
/// is silently suppressed for the run rather than rejected, and the sync
/// still succeeds locally (there is no remote configured, so a real fetch
/// attempt would have failed).
#[test]
fn sync_dry_run_suppresses_auto_fetch() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    gat_command::config(
        &repo,
        gat_command::ConfigRequest::from_raw(
            "sync.auto_fetch".to_string(),
            gat_command::ConfigAction::Set(vec!["true".to_string()]),
            gat_core::config::ConfigScope::Project,
        )
        .unwrap(),
    )
    .unwrap();
    let context = Context::new(repo);

    let result = app::run(parse(&["sync", "--dry-run"]), &context, &NoopProgress);
    let outcome = result.unwrap();

    let app::Outcome::Synced(synced) = outcome else {
        panic!("expected Outcome::Synced");
    };
    assert_eq!(synced.fetched, 0);
    assert!(synced.outcome.dry_run);
}

/// Sanity check that the extracted command API remains usable without
/// going through `app::run`'s dispatch.
#[test]
fn status_command_is_reachable_without_app_run() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let outcome = gat_command::status(
        &repo,
        StatusRequest {
            selection: Some(gat_core::selection::Selection::root()),
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(outcome, StatusOutcome::NoTrackedFiles);
}

/// `gat gc`, an experimental command, dispatches
/// successfully to `Outcome::GarbageCollected` *and* returns exactly one
/// experimental [`gat::lifecycle::Notice`] for `gat gc` -- the
/// notice is additive, never changing the outcome/success shape itself.
#[test]
fn gc_dispatch_returns_an_experimental_notice() {
    use gat::lifecycle::NoticeKind;

    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(parse(&["gc", "--dry-run"]), &context, &NoopProgress);
    let outcome = result.unwrap();
    let notices = context.lifecycle.take_notices();

    assert!(matches!(outcome, app::Outcome::GarbageCollected(_)));
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, NoticeKind::Experimental);
    assert_eq!(notices[0].subject, "`gat gc`");
}

/// A stable command like `gat status` carries no lifecycle notices.
#[test]
fn status_dispatch_returns_no_notices() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(parse(&["status"]), &context, &NoopProgress);
    result.unwrap();

    assert!(context.lifecycle.take_notices().is_empty());
}

/// Setting `cache.ingest_strategy` to the experimental `hybrid` value
/// produces exactly one experimental notice naming the value; setting it
/// to the stable default `safe` produces none.
#[test]
fn config_ingest_strategy_hybrid_and_safe_notices() {
    use gat::lifecycle::NoticeKind;

    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(
        parse(&["config", "cache.ingest_strategy", "hybrid"]),
        &context,
        &NoopProgress,
    );
    result.unwrap();
    let notices = context.lifecycle.take_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, NoticeKind::Experimental);

    let result = app::run(
        parse(&["config", "cache.ingest_strategy", "safe"]),
        &context,
        &NoopProgress,
    );
    result.unwrap();
    assert!(context.lifecycle.take_notices().is_empty());
}

/// `gat config git.exclude_patterns <value>` produces a deprecation
/// notice naming the canonical `git.ignore_patterns` replacement.
#[test]
fn config_git_exclude_patterns_alias_produces_deprecation_notice() {
    use gat::lifecycle::NoticeKind;

    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let context = Context::new(repo);

    let result = app::run(
        parse(&["config", "git.exclude_patterns", "/data/"]),
        &context,
        &NoopProgress,
    );
    result.unwrap();
    let notices = context.lifecycle.take_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, NoticeKind::Deprecated);
    assert_eq!(notices[0].replacement, Some("git.ignore_patterns"));
}

#[test]
fn gc_no_history_overrides_conservative_default() {
    let tmp = test_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let path = tmp.path().join("asset.bin");
    std::fs::write(&path, b"old").unwrap();
    add(&repo, &["asset.bin".into()]);
    commit_all(tmp.path(), "old asset");
    std::fs::write(&path, b"current").unwrap();
    add(&repo, &["asset.bin".into()]);
    let context = Context::new(repo);
    for (args, expected) in [
        (vec!["gc", "--dry-run"], 0),
        (vec!["gc", "--no-history", "--dry-run"], 1),
    ] {
        let outcome = app::run(parse(&args), &context, &NoopProgress).unwrap();
        let app::Outcome::GarbageCollected(report) = outcome else {
            panic!("expected GC outcome");
        };
        assert_eq!(report.deleted, expected);
    }
}

#[test]
fn status_and_transfers_default_to_current_state() {
    gat_engine::initialize_backends();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    let tmp = test_repo();
    let remote = tempfile::tempdir().unwrap();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    std::fs::write(tmp.path().join("asset.bin"), b"current").unwrap();
    add(&repo, &["asset.bin".into()]);
    let context = Context::new(repo);
    let url = url::Url::from_directory_path(remote.path())
        .unwrap()
        .to_string();
    app::run(
        parse(&["remote", "add", "origin", &url]),
        &context,
        &NoopProgress,
    )
    .unwrap();
    app::run(
        parse(&["remote", "default", "origin"]),
        &context,
        &NoopProgress,
    )
    .unwrap();
    // An invalid explicit history query proves history cannot be silently used.
    for command in ["push", "fetch", "pull", "status"] {
        let args = if command == "status" {
            vec![command, "--remote", "origin", "--rev", "missing"]
        } else {
            vec![command, "--rev", "missing"]
        };
        assert!(app::run(parse(&args), &context, &NoopProgress).is_err());
        let outcome = app::run(parse(&[command]), &context, &NoopProgress).unwrap();
        if let app::Outcome::Pushed(report) = outcome {
            assert_eq!(report.total, 1);
            assert!(report.skipped.is_empty());
        }
    }
}

#[test]
fn local_mutation_dispatch_recovers_and_loads_config_once() {
    let tmp = test_repo();
    let context = Context::new(
        gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf()),
    );
    std::fs::write(tmp.path().join("a.bin"), b"content").unwrap();
    add(&context.repo, &[std::path::PathBuf::from("a.bin")]);
    for args in [vec!["mv", "a.bin", "b.bin"], vec!["rm", "--cached", "."]] {
        let (send, receive) = std::sync::mpsc::channel();
        let before = gat_engine::test_support::config_loads();
        gat_io::atomic_test_support::with_acquire_attempt_hook(
            std::thread::current().id(),
            send,
            || app::run(parse(&args), &context, &NoopProgress),
        )
        .unwrap();
        assert_eq!(
            receive.try_iter().count(),
            1,
            "{args:?} reacquired the OS lock"
        );
        assert_eq!(gat_engine::test_support::config_loads() - before, 1);
    }
}
