use gat_command::{AddRequest, MoveRequest, RemoveRequest};
use gat_core::lexical_path::GatPath;
use gat_core::path_scope::normalize_path_scope;
use gat_core::progress::NoopProgress;
use gat_engine::Repository;

fn repository() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().unwrap();
    test_support_git::run_git(tmp.path(), &["init", "-q"]);
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    (tmp, repo)
}

#[test]
fn typed_mutation_requests_share_one_authoritative_repository_path() {
    let (tmp, repo) = repository();
    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();

    let added = gat_command::add(
        &repo,
        AddRequest {
            paths: vec![normalize_path_scope("a.bin").unwrap()],
            force: false,
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(added.added_count, 1);

    let moved = gat_command::move_path(
        &repo,
        MoveRequest {
            src: GatPath::normalize("a.bin").unwrap(),
            dst: GatPath::normalize("b.bin").unwrap(),
            force: false,
        },
    )
    .unwrap();
    assert_eq!(moved.src.as_str(), "a.bin");
    assert_eq!(moved.dst.as_str(), "b.bin");

    let removed = gat_command::remove(
        &repo,
        RemoveRequest {
            paths: vec![normalize_path_scope("b.bin").unwrap()],
            cached: false,
        },
    )
    .unwrap();
    assert_eq!(
        removed
            .paths
            .iter()
            .map(gat_core::lexical_path::GatPath::as_str)
            .collect::<Vec<_>>(),
        vec!["b.bin"]
    );
    assert!(!tmp.path().join("b.bin").exists());
}

#[cfg(feature = "test-support")]
#[test]
fn add_reuses_one_cache_session_across_batches_and_skips_it_for_reuse() {
    let (tmp, repo) = repository();
    std::fs::create_dir(tmp.path().join("nested")).unwrap();
    std::fs::write(tmp.path().join("top.bin"), b"top").unwrap();
    std::fs::write(tmp.path().join("nested/child.bin"), b"child").unwrap();
    let request = AddRequest {
        paths: vec![
            normalize_path_scope("top.bin").unwrap(),
            normalize_path_scope("nested").unwrap(),
        ],
        force: false,
    };

    let cache_opens_before = gat_engine::test_support::cache_db_opens();
    let resolutions_before = gat_engine::test_support::cache_location_resolutions();
    let remote_opens_before = gat_engine::test_support::remote_opens();

    let outcome = gat_command::add(&repo, request.clone(), &NoopProgress).unwrap();

    assert_eq!(outcome.added_count, 2);
    assert_eq!(
        gat_engine::test_support::cache_db_opens() - cache_opens_before,
        1,
        "one add invocation must reuse one proof client across multiple flushes"
    );
    assert_eq!(
        gat_engine::test_support::cache_location_resolutions() - resolutions_before,
        1,
        "one add invocation must resolve its cache location once"
    );
    assert_eq!(
        gat_engine::test_support::remote_opens(),
        remote_opens_before,
        "local add must not open a remote"
    );

    let materialized = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
    assert_eq!(materialized.entries.len(), 2);
    assert!(
        materialized
            .entries
            .iter()
            .any(|entry| entry.path.as_str() == "top.bin")
    );
    assert!(
        materialized
            .entries
            .iter()
            .any(|entry| entry.path.as_str() == "nested/child.bin")
    );

    let cache_opens_before_reuse = gat_engine::test_support::cache_db_opens();
    let second = gat_command::add(&repo, request, &NoopProgress).unwrap();

    assert_eq!(second.added_count, 2);
    assert_eq!(
        gat_engine::test_support::cache_db_opens(),
        cache_opens_before_reuse,
        "reuse-only add must not open the proof database"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn add_error_after_multiple_batches_drops_one_cache_session_without_publishing_state() {
    let (tmp, repo) = repository();
    std::fs::create_dir(tmp.path().join("nested")).unwrap();
    std::fs::write(tmp.path().join("top.bin"), b"top").unwrap();
    std::fs::write(tmp.path().join("nested/child.bin"), b"child").unwrap();

    let cache_opens_before = gat_engine::test_support::cache_db_opens();
    let error = gat_command::add(
        &repo,
        AddRequest {
            paths: vec![
                normalize_path_scope("top.bin").unwrap(),
                normalize_path_scope("nested").unwrap(),
                normalize_path_scope("missing*").unwrap(),
            ],
            force: false,
        },
        &NoopProgress,
    )
    .unwrap_err();

    assert!(matches!(error, gat_command::AddError::NoMatch { .. }));
    assert_eq!(
        gat_engine::test_support::cache_db_opens() - cache_opens_before,
        1,
        "failed add must not reopen the proof client between batches or during cleanup"
    );
    assert!(
        gat_engine::test_support::load_materialized_for_test(&repo)
            .unwrap()
            .entries
            .is_empty(),
        "an add error before publication must not record partial materialized state"
    );
}

#[test]
fn move_reports_the_typed_destination_collision_without_mutating_files() {
    let (tmp, repo) = repository();
    std::fs::write(tmp.path().join("a.bin"), b"source").unwrap();
    std::fs::write(tmp.path().join("b.bin"), b"destination").unwrap();
    gat_command::add(
        &repo,
        AddRequest {
            paths: vec![normalize_path_scope("a.bin").unwrap()],
            force: false,
        },
        &NoopProgress,
    )
    .unwrap();

    let error = gat_command::move_path(
        &repo,
        MoveRequest {
            src: GatPath::normalize("a.bin").unwrap(),
            dst: GatPath::normalize("b.bin").unwrap(),
            force: false,
        },
    )
    .unwrap_err();

    assert!(matches!(
        error,
        gat_command::MoveError::DestinationExists { .. }
    ));
    assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"source");
    assert_eq!(
        std::fs::read(tmp.path().join("b.bin")).unwrap(),
        b"destination"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn add_repairs_deleted_cache_objects_and_whole_cache() {
    let (tmp, repo) = repository();
    for name in ["a.bin", "b.bin"] {
        std::fs::write(tmp.path().join(name), b"shared payload").unwrap();
    }
    let request = AddRequest {
        paths: vec![normalize_path_scope("*.bin").unwrap()],
        force: false,
    };
    gat_command::add(&repo, request.clone(), &NoopProgress).unwrap();
    let materialized = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
    let cache = gat_engine::test_support::cache_root(&repo);
    let oid = materialized.entries[0].oid;
    let object = cache.object_path_for_test(&oid);
    cache.make_object_writable_for_test(&oid).unwrap();
    std::fs::remove_file(&object).unwrap();
    let repaired = gat_command::add(&repo, request.clone(), &NoopProgress).unwrap();
    assert_eq!(repaired.added_count, 2);
    assert_eq!(std::fs::read(&object).unwrap(), b"shared payload");
    cache.make_object_writable_for_test(&oid).unwrap();
    std::fs::remove_dir_all(cache.display_path()).unwrap();
    gat_command::add(&repo, request, &NoopProgress).unwrap();
    assert_eq!(std::fs::read(&object).unwrap(), b"shared payload");
    std::fs::remove_file(tmp.path().join("a.bin")).unwrap();
    let synced = gat_command::sync(
        &repo,
        gat_command::SyncRequest {
            selection: Some(gat_core::selection::Selection::root()),
            force: false,
            dry_run: false,
            trust_state: false,
            fetch: false,
            repair: false,
            remote: None,
            rematerialize: false,
        },
        &NoopProgress,
    )
    .unwrap();
    assert!(synced.completion.is_clean());
    assert_eq!(
        std::fs::read(tmp.path().join("a.bin")).unwrap(),
        b"shared payload"
    );
}

#[test]
fn add_counts_overlapping_selectors_once() {
    let (tmp, repo) = repository();
    std::fs::create_dir(tmp.path().join("data")).unwrap();
    for name in ["a.bin", "b.bin"] {
        std::fs::write(tmp.path().join("data").join(name), name).unwrap();
    }
    let outcome = gat_command::add(
        &repo,
        AddRequest {
            paths: ["data/a.bin", "data", "data/*.bin", "data/b.bin", "data"]
                .map(|path| normalize_path_scope(path).unwrap())
                .to_vec(),
            force: false,
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(outcome.added_count, 2);
    assert_eq!(
        outcome
            .rows
            .iter()
            .filter_map(|row| row.file_count)
            .sum::<usize>(),
        1
    );
}

#[test]
fn add_reports_ignored_entries_with_bounded_sorted_samples() {
    let (tmp, repo) = repository();
    std::fs::write(tmp.path().join(".gitignore"), "*.bin\nblocked/\n").unwrap();
    std::fs::create_dir(tmp.path().join("blocked")).unwrap();
    std::fs::write(tmp.path().join("blocked/unseen.bin"), b"unseen").unwrap();
    for index in (0..20).rev() {
        std::fs::write(tmp.path().join(format!("{index:02}.bin")), b"ignored").unwrap();
    }
    let outcome = gat_command::add(
        &repo,
        AddRequest {
            paths: vec![normalize_path_scope("**/*.bin").unwrap()],
            force: false,
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(outcome.added_count, 0);
    let ignored = outcome
        .exclusions
        .iter()
        .find(|item| item.reason == gat_engine::AddExclusionReason::GitIgnore)
        .unwrap();
    assert_eq!(ignored.files, 20);
    assert_eq!(ignored.directories, 1);
    assert_eq!(
        ignored
            .samples
            .iter()
            .map(gat_core::lexical_path::GatPath::as_str)
            .collect::<Vec<_>>(),
        ["00.bin", "01.bin", "02.bin"]
    );
}

#[test]
fn add_infrastructure_is_unconditional_but_nested_basenames_are_allowed() {
    let (tmp, repo) = repository();
    std::fs::create_dir(tmp.path().join("assets")).unwrap();
    std::fs::write(tmp.path().join("assets/gat.yaml"), b"ordinary").unwrap();
    std::fs::write(tmp.path().join("assets/gat.lock"), b"ordinary").unwrap();
    for path in ["gat.yaml", "gat.lock", ".gat", ".git"] {
        let error = gat_command::add(
            &repo,
            AddRequest {
                paths: vec![normalize_path_scope(path).unwrap()],
                force: true,
            },
            &NoopProgress,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            gat_command::AddError::Path(
                gat_engine::WorktreePathError::ForbiddenInfrastructurePath { .. }
            )
        ));
    }
    let outcome = gat_command::add(
        &repo,
        AddRequest {
            paths: vec![normalize_path_scope("assets").unwrap()],
            force: true,
        },
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(outcome.added_count, 2);
}

#[cfg(feature = "test-support")]
#[test]
fn tiny_windows_bound_explicit_and_directory_preparation() {
    for explicit in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        test_support_git::run_git(tmp.path(), &["init", "-q"]);
        std::fs::create_dir(tmp.path().join("data")).unwrap();
        let paths: Vec<_> = (0..13)
            .map(|index| {
                let path = format!("data/{index:02}.bin");
                std::fs::write(tmp.path().join(&path), path.as_bytes()).unwrap();
                normalize_path_scope(path).unwrap()
            })
            .collect();
        let request = AddRequest {
            paths: if explicit {
                paths
            } else {
                vec![normalize_path_scope("data").unwrap()]
            },
            force: false,
        };
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        for _ in 0..2 {
            let (result, high_water) = gat_command::add_with_window_for_test(
                &repo,
                request.clone(),
                std::num::NonZeroUsize::new(2).unwrap(),
                &NoopProgress,
            )
            .unwrap();
            assert_eq!(result.added_count, 13);
            assert_eq!(high_water, 2);
        }
    }
}

#[cfg(feature = "test-support")]
#[test]
fn add_rejects_nonregular_cache_objects_without_publishing() {
    let (tmp, repo) = repository();
    std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
    let request = AddRequest {
        paths: vec![normalize_path_scope("a.bin").unwrap()],
        force: false,
    };
    gat_command::add(&repo, request.clone(), &NoopProgress).unwrap();
    let entries = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
    let cache = gat_engine::test_support::cache_root(&repo);
    let oid = entries.entries[0].oid;
    let path = cache.object_path_for_test(&oid);
    cache.make_object_writable_for_test(&oid).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(gat_command::add(&repo, request, &NoopProgress).is_err());
    assert_eq!(
        gat_engine::test_support::load_materialized_for_test(&repo).unwrap(),
        entries
    );
}

#[cfg(feature = "test-support")]
#[test]
fn add_repairs_external_cache_in_many_windows_after_state_loss() {
    let (tmp, repo) = repository();
    let external = tempfile::tempdir().unwrap();
    let mut config = repo.load_config().unwrap();
    config.cache.location = Some(
        gat_core::cache_location::CacheLocation::try_from_path(external.path().to_path_buf())
            .expect("nonempty cache location"),
    );
    repo.write_config_fixture(&config).unwrap();
    std::fs::create_dir(tmp.path().join("data")).unwrap();
    for index in 0..9 {
        std::fs::write(
            tmp.path().join(format!("data/{index}.bin")),
            if index % 2 == 0 {
                b"even".as_slice()
            } else {
                b"odd".as_slice()
            },
        )
        .unwrap();
    }
    let request = AddRequest {
        paths: vec![normalize_path_scope("data").unwrap()],
        force: false,
    };
    gat_command::add(&repo, request.clone(), &NoopProgress).unwrap();
    let entries = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
    let cache = gat_engine::test_support::cache_root(&repo);
    let missing = entries.entries[0].oid;
    cache.make_object_writable_for_test(&missing).unwrap();
    std::fs::remove_file(cache.object_path_for_test(&missing)).unwrap();
    std::fs::remove_file(tmp.path().join(".gat/state/state.sqlite3")).unwrap();
    std::fs::write(tmp.path().join("data/1.bin"), b"changed").unwrap();
    std::fs::write(tmp.path().join("data/new.bin"), b"new").unwrap();
    let before = gat_engine::test_support::cache_db_opens();
    let (outcome, high_water) = gat_command::add_with_window_for_test(
        &repo,
        request,
        std::num::NonZeroUsize::new(2).unwrap(),
        &NoopProgress,
    )
    .unwrap();
    assert_eq!(outcome.added_count, 10);
    assert_eq!(high_water, 2);
    assert_eq!(gat_engine::test_support::cache_db_opens() - before, 1);
    let entries = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
    for entry in entries.entries {
        assert_eq!(
            std::fs::read(cache.object_path_for_test(&entry.oid)).unwrap(),
            std::fs::read(tmp.path().join(entry.path.as_str())).unwrap()
        );
    }
}

#[test]
fn nonrecursive_unmatched_glob_is_not_blocked_by_an_ignored_subtree() {
    let (tmp, repo) = repository();
    std::fs::create_dir(tmp.path().join("blocked")).unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "blocked/\n").unwrap();
    std::fs::write(tmp.path().join("blocked/unseen.bin"), b"ignored").unwrap();
    let error = gat_command::add(
        &repo,
        AddRequest {
            paths: vec![normalize_path_scope("*.bin").unwrap()],
            force: false,
        },
        &NoopProgress,
    )
    .unwrap_err();
    assert!(matches!(error, gat_command::AddError::NoMatch { .. }));
}

#[test]
fn unmatched_glob_does_not_count_an_incompatible_ignored_directory() {
    let (tmp, repo) = repository();
    std::fs::create_dir(tmp.path().join("data")).unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "data/\n").unwrap();
    std::fs::write(tmp.path().join("data/x.bin"), b"ignored").unwrap();
    let error = gat_command::add(
        &repo,
        AddRequest {
            paths: vec![normalize_path_scope("d?/*.bin").unwrap()],
            force: false,
        },
        &NoopProgress,
    )
    .unwrap_err();
    assert!(matches!(error, gat_command::AddError::NoMatch { pattern } if pattern == "d?/*.bin"));
}

#[test]
fn compatible_ignored_subtrees_still_explain_zero_adds() {
    let (tmp, repo) = repository();
    std::fs::create_dir_all(tmp.path().join("da/nested")).unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "da/\n").unwrap();
    std::fs::write(tmp.path().join("da/nested/x.bin"), b"ignored").unwrap();
    for pattern in ["d?/*.bin", "d[ab]/*.bin", "d?/**/*.bin", "d?/**"] {
        let outcome = gat_command::add(
            &repo,
            AddRequest {
                paths: vec![normalize_path_scope(pattern).unwrap()],
                force: false,
            },
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(outcome.added_count, 0);
        let ignored = outcome
            .exclusions
            .iter()
            .find(|item| item.reason == gat_engine::AddExclusionReason::GitIgnore)
            .unwrap();
        assert_eq!(ignored.files, 0);
        assert_eq!(ignored.directories, 1);
        assert_eq!(ignored.samples, vec![GatPath::normalize("da").unwrap()]);
    }
}

#[test]
fn tracked_descendants_do_not_hide_an_unsearched_ignored_scope() {
    let (tmp, repo) = repository();
    std::fs::create_dir(tmp.path().join("data")).unwrap();
    std::fs::write(tmp.path().join("data/kept.txt"), b"tracked").unwrap();
    gat_command::add(
        &repo,
        AddRequest {
            paths: vec![normalize_path_scope("data/kept.txt").unwrap()],
            force: false,
        },
        &NoopProgress,
    )
    .unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "data/\n").unwrap();
    std::fs::write(tmp.path().join("data/new.bin"), b"new content").unwrap();

    for (selector, expected_added) in [("**/*.bin", 0), ("**/kept.txt", 1)] {
        let outcome = gat_command::add(
            &repo,
            AddRequest {
                paths: vec![normalize_path_scope(selector).unwrap()],
                force: false,
            },
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(outcome.added_count, expected_added, "{selector}");
        let ignored = outcome
            .exclusions
            .iter()
            .find(|item| item.reason == gat_engine::AddExclusionReason::GitIgnore)
            .unwrap();
        assert_eq!(ignored.files, 0);
        assert_eq!(ignored.directories, 1);
        assert_eq!(ignored.samples, vec![GatPath::normalize("data").unwrap()]);
    }
}
