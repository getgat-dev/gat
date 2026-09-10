//! Shared default selection across command workflows, independent of the CLI.
use gat_command::{
    DiffOutcome, DiffRequest, DiffTarget, FetchRequest, FetchSource, HookRequest, LsFilesRequest,
    PullRequest, PushRequest, PushSource, RemoteStatusRequest, SelectionScope, StatusOutcome,
    StatusRequest, SyncRequest, diff, fetch, hook, ls_files, pull, push, remote_status, status,
    sync,
};
use gat_core::config::{Config, SelectionConfig};
use gat_core::globs::GatGlobPattern;
use gat_core::progress::NoopProgress;
use gat_core::selection::Selection;
use std::path::PathBuf;
use test_support::{add, git_repo_with_initial_commit, remote_add_with_default};
use test_support_git::commit_all;

#[test]
fn seven_commands_and_hooks_share_defaults_and_explicit_root_replaces_them() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    gat_engine::initialize_backends();
    let temp = git_repo_with_initial_commit();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(temp.path().to_path_buf());
    let paths = ["models/a.bin", "models/experimental/b.bin", "archive/c.bin"];
    for path in paths {
        let dest = temp.path().join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, path.as_bytes()).unwrap();
    }
    add(&repo, &paths.map(PathBuf::from), &NoopProgress).unwrap();
    repo.write_config_fixture(&Config {
        selections: gat_core::config::SelectionsConfig {
            default: Some("runtime".into()),
            by_name: std::collections::BTreeMap::from([(
                "runtime".into(),
                SelectionConfig {
                    path: gat_core::lexical_path::GatSubpath::normalize("models").unwrap(),
                    include: Some(vec![GatGlobPattern::parse("**").unwrap()]),
                    exclude: Some(vec![GatGlobPattern::parse("experimental/**").unwrap()]),
                },
            )]),
        },
        ..Default::default()
    })
    .unwrap();
    let root = Selection::root();
    for (selection, count, scope) in [
        (None, 1, SelectionScope::Configured),
        (Some(root.clone()), 3, SelectionScope::Unrestricted),
    ] {
        let listed = ls_files(
            &repo,
            LsFilesRequest {
                selection: selection.clone(),
            },
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(listed.paths.len(), count);
        assert_eq!(listed.scope, scope);
        let StatusOutcome::WorkingTree {
            rows,
            scope: status_scope,
            ..
        } = status(&repo, StatusRequest { selection }, &NoopProgress).unwrap()
        else {
            panic!("expected rows")
        };
        assert_eq!(
            rows.into_iter().map(|row| row.path).collect::<Vec<_>>(),
            listed.paths
        );
        assert_eq!(status_scope, scope);
    }
    commit_all(temp.path(), "assets");
    for (selection, count) in [(None, 1), (Some(root.clone()), 3)] {
        let DiffOutcome::Changes { rows, .. } = diff(
            &repo,
            DiffRequest {
                from: "HEAD~1".into(),
                to: DiffTarget::Revision("HEAD".into()),
                selection,
            },
            &NoopProgress,
        )
        .unwrap() else {
            panic!("expected diff")
        };
        assert_eq!(rows.len(), count);
    }
    let remote = tempfile::tempdir().unwrap();
    remote_add_with_default(
        &repo,
        "origin",
        gat_io::remote_file_url_for_test(remote.path()),
    )
    .unwrap();
    let published = push(
        &repo,
        PushRequest {
            selection: None,
            remote: None,
            source: PushSource::Current,
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(published.total, 1);
    for (selection, checked, missing) in [(None, 1, 0), (Some(&root), 3, 2)] {
        let presence = remote_status(
            &repo,
            RemoteStatusRequest {
                selection,
                remote: None,
                history: None,
            },
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(presence.checked, checked);
        assert_eq!(presence.missing.len(), missing);
    }
    let published = push(
        &repo,
        PushRequest {
            selection: Some(&root),
            remote: None,
            source: PushSource::Current,
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(published.total, 3);

    let lock = gat_io::LockStore::load_repository(&gat_io::RepositoryLayout::at(
        temp.path().to_path_buf(),
    ))
    .unwrap();
    let cache = gat_engine::test_support::cache_root(&repo);
    let clear = || {
        for entry in &lock.entries {
            let file = temp.path().join(entry.path.as_str());
            if file.exists() {
                std::fs::remove_file(file).unwrap();
            }
            let object = cache.object_path_for_test(&entry.oid);
            if object.exists() {
                cache.make_object_writable_for_test(&entry.oid).unwrap();
                std::fs::remove_file(object).unwrap();
            }
        }
    };
    let assert_materialized = |expected| {
        assert_eq!(paths.map(|path| temp.path().join(path).exists()), expected);
    };
    clear();
    for (selection, count) in [(None, 1), (Some(&root), 2)] {
        let fetched = fetch(
            &repo,
            FetchRequest {
                selection,
                remote: None,
                source: FetchSource::Current,
            },
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(fetched.fetched, count);
    }
    let sync_request = |selection| SyncRequest {
        selection,
        force: false,
        dry_run: false,
        trust_state: false,
        fetch: false,
        repair: false,
        remote: None,
        rematerialize: false,
    };
    for (selection, expected) in [
        (None, [true, false, false]),
        (Some(root.clone()), [true; 3]),
    ] {
        sync(&repo, sync_request(selection), &NoopProgress).unwrap();
        assert_materialized(expected);
    }
    clear();
    for (selection, fetched, expected) in
        [(None, 1, [true, false, false]), (Some(root), 2, [true; 3])]
    {
        let pulled = pull(
            &repo,
            PullRequest {
                selection,
                remote: None,
                history: None,
            },
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(pulled.fetched, fetched);
        assert_materialized(expected);
    }
    for path in paths {
        std::fs::remove_file(temp.path().join(path)).unwrap();
    }
    hook(&repo, HookRequest, &NoopProgress).unwrap();
    assert_materialized([true, false, false]);
}
