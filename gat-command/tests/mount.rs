use gat_command::{MountError, MountOutcome, MountRequest, mount};
use gat_core::config::{Config, ConfigScope, MountConfig};
use gat_core::git_location::GitLocationSpec;
use gat_core::lexical_path::{GatPath, GatSubpath};
use gat_core::lock::{Entry, Lock};
use gat_core::name::{MountName, RemoteName};
use gat_core::oid::Oid;
use gat_core::progress::NoopProgress;
use gat_core::selection::Selection;
use gat_engine::{Repository, RepositoryError};
use test_support_git::{commit_all, run_git};

fn gp(path: &str) -> GatPath {
    GatPath::parse_canonical(path).unwrap()
}

fn mount_config(target: &str) -> MountConfig {
    MountConfig {
        // hygiene-ok: configuration-only test URL; never dialed.
        url: GitLocationSpec::from_string("https://example.invalid/source.git".to_string()),
        target: gp(target),
        path: GatSubpath::Root,
        rev: None,
        rev_lock: None,
        include: Vec::new(),
        exclude: Vec::new(),
    }
}

fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("README"), "fixture").unwrap();
    commit_all(dir.path(), "initial");
    dir
}

fn source_repo(entries: &[(&str, char)]) -> tempfile::TempDir {
    let dir = git_repo();
    let repo = Repository::at(dir.path().to_path_buf());
    repo.save_lock(&Lock {
        entries: entries
            .iter()
            .map(|(path, digit)| Entry {
                path: gp(path),
                oid: Oid::from_hex(&digit.to_string().repeat(64)).unwrap(),
            })
            .collect(),
    })
    .unwrap();
    commit_all(dir.path(), "source lock");
    dir
}

fn add_request(source: &std::path::Path, target: Option<&str>, scope: ConfigScope) -> MountRequest {
    MountRequest::Add {
        name: MountName::from_string("vendor".to_string()),
        location: GitLocationSpec::from_string(source.display().to_string()),
        target: target.map(gp),
        path: GatSubpath::Root,
        revision: None,
        remote: None,
        no_setup: false,
        include: Vec::new(),
        exclude: Vec::new(),
        scope,
    }
}

fn desired_paths(repo: &Repository) -> Vec<GatPath> {
    let mut paths = Vec::new();
    repo.visit_current_desired_entries(&Selection::root(), |entry| paths.push(entry.path))
        .unwrap();
    paths
}

#[test]
fn add_show_list_update_and_remove_share_the_authoritative_path() {
    let source = source_repo(&[("a.bin", 'a'), ("nested/b.bin", 'b')]);
    let destination = git_repo();
    let repo = Repository::at(destination.path().to_path_buf());

    let added = mount(
        &repo,
        add_request(source.path(), Some("vendor"), ConfigScope::Project),
        &NoopProgress,
    )
    .unwrap();
    assert!(matches!(
        added,
        MountOutcome::Added {
            entries: 2,
            ref target,
            ..
        } if target == &gp("vendor")
    ));
    assert_eq!(
        desired_paths(&repo),
        vec![gp("vendor/a.bin"), gp("vendor/nested/b.bin")]
    );

    let listed = mount(&repo, MountRequest::List, &NoopProgress).unwrap();
    assert!(matches!(listed, MountOutcome::List(records) if records.len() == 1));

    let shown = mount(
        &repo,
        MountRequest::Show {
            name: MountName::from_string("vendor".to_string()),
        },
        &NoopProgress,
    )
    .unwrap();
    assert!(matches!(
        shown,
        MountOutcome::Show(details) if details.tracked_rows == 2 && details.target == gp("vendor") && details.scope == ConfigScope::Project
    ));

    let updated = mount(
        &repo,
        MountRequest::Update {
            name: MountName::from_string("vendor".to_string()),
            location: None,
            target: Some(gp("third-party")),
            path: None,
            revision: None,
            remote: None,
            no_setup: false,
            include: None,
            exclude: None,
            scope: ConfigScope::Project,
        },
        &NoopProgress,
    )
    .unwrap();
    assert!(matches!(
        updated,
        MountOutcome::Updated {
            removed: 2,
            added: 2,
            ref target,
            ..
        } if target == &gp("third-party")
    ));
    assert_eq!(
        desired_paths(&repo),
        vec![gp("third-party/a.bin"), gp("third-party/nested/b.bin")]
    );

    let removed = mount(
        &repo,
        MountRequest::Remove {
            name: MountName::from_string("vendor".to_string()),
            detach_only: false,
            scope: ConfigScope::Project,
        },
        &NoopProgress,
    )
    .unwrap();
    assert!(matches!(
        removed,
        MountOutcome::Removed {
            owned_rows: 2,
            detach_only: false,
            ..
        }
    ));
    assert!(desired_paths(&repo).is_empty());
}

#[test]
fn clearing_mount_filters_expands_the_snapshot_and_preserves_the_other_list() {
    use gat_core::globs::GatGlobPattern;
    let source = source_repo(&[("a.bin", 'a'), ("other.txt", 'b'), ("nested/b.bin", 'c')]);
    let destination = git_repo();
    let repo = Repository::at(destination.path().to_path_buf());
    let mut request = add_request(source.path(), Some("vendor"), ConfigScope::Project);
    let MountRequest::Add {
        include, exclude, ..
    } = &mut request
    else {
        panic!("expected add request");
    };
    include.push(GatGlobPattern::parse("**/*.bin").unwrap());
    exclude.push(GatGlobPattern::parse("nested/**").unwrap());
    mount(&repo, request, &NoopProgress).unwrap();
    assert_eq!(desired_paths(&repo), vec![gp("vendor/a.bin")]);

    for (clear_include, clear_exclude, expected) in [
        (false, false, vec!["vendor/a.bin"]),
        (true, false, vec!["vendor/a.bin", "vendor/other.txt"]),
        (
            false,
            true,
            vec!["vendor/a.bin", "vendor/nested/b.bin", "vendor/other.txt"],
        ),
    ] {
        mount(
            &repo,
            MountRequest::Update {
                name: "vendor".into(),
                location: None,
                target: None,
                path: None,
                revision: None,
                remote: None,
                no_setup: true,
                include: clear_include.then(Vec::new),
                exclude: clear_exclude.then(Vec::new),
                scope: ConfigScope::Project,
            },
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(
            desired_paths(&repo),
            expected.into_iter().map(gp).collect::<Vec<_>>()
        );
        let MountOutcome::Show(details) = mount(
            &repo,
            MountRequest::Show {
                name: "vendor".into(),
            },
            &NoopProgress,
        )
        .unwrap() else {
            panic!("expected mount details");
        };
        assert_eq!(details.exclude.is_empty(), clear_exclude);
        assert_eq!(details.include.is_empty(), clear_include || clear_exclude);
    }
}

#[test]
fn add_infers_the_repository_name_and_explicit_target_wins() {
    let parent = tempfile::tempdir().unwrap();
    let source_path = parent.path().join("source-name.git");
    std::fs::create_dir(&source_path).unwrap();
    run_git(&source_path, &["init", "-q", "-b", "main"]);
    std::fs::write(source_path.join("README"), "fixture").unwrap();
    commit_all(&source_path, "initial");
    let source = Repository::at(source_path.clone());
    source
        .save_lock(&Lock {
            entries: vec![Entry {
                path: gp("asset.bin"),
                oid: Oid::from_hex(&"c".repeat(64)).unwrap(),
            }],
        })
        .unwrap();
    commit_all(&source_path, "source lock");

    let inferred_dir = git_repo();
    let inferred_repo = Repository::at(inferred_dir.path().to_path_buf());
    let outcome = mount(
        &inferred_repo,
        add_request(&source_path, None, ConfigScope::Project),
        &NoopProgress,
    )
    .unwrap();
    assert!(matches!(
        outcome,
        MountOutcome::Added { ref target, .. } if target == &gp("source-name")
    ));

    let explicit_dir = git_repo();
    let explicit_repo = Repository::at(explicit_dir.path().to_path_buf());
    let outcome = mount(
        &explicit_repo,
        add_request(&source_path, Some("chosen"), ConfigScope::Project),
        &NoopProgress,
    )
    .unwrap();
    assert!(matches!(
        outcome,
        MountOutcome::Added { ref target, .. } if target == &gp("chosen")
    ));
}

#[test]
fn add_rejects_root_owned_rows_without_mutating_config() {
    let source = source_repo(&[("asset.bin", 'd')]);
    let destination = git_repo();
    let repo = Repository::at(destination.path().to_path_buf());
    repo.save_lock(&Lock {
        entries: vec![Entry {
            path: gp("vendor/local.bin"),
            oid: Oid::from_hex(&"e".repeat(64)).unwrap(),
        }],
    })
    .unwrap();

    let error = mount(
        &repo,
        add_request(source.path(), Some("vendor"), ConfigScope::Project),
        &NoopProgress,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        MountError::RootOwnedAssets { ref path } if path == &gp("vendor")
    ));
    assert!(
        repo.load_config_scoped(ConfigScope::Project)
            .unwrap()
            .mounts
            .by_name
            .is_empty()
    );
    assert_eq!(desired_paths(&repo), vec![gp("vendor/local.bin")]);
}

#[test]
fn scope_precedence_errors_remain_typed_through_the_public_command() {
    let source = source_repo(&[("asset.bin", 'f')]);
    let destination = git_repo();
    let repo = Repository::at(destination.path().to_path_buf());
    let name = MountName::from_string("vendor".to_string());
    let mut project = Config::default();
    project
        .mounts
        .by_name
        .insert(name.clone(), mount_config("project-vendor"));
    repo.save_config_scoped(&project, ConfigScope::Project)
        .unwrap();
    let mut local = Config::default();
    local
        .mounts
        .by_name
        .insert(name.clone(), mount_config("local-vendor"));
    repo.save_config_scoped(&local, ConfigScope::Local).unwrap();

    let error = mount(
        &repo,
        MountRequest::Update {
            name: name.clone(),
            location: Some(GitLocationSpec::from_string(
                source.path().display().to_string(),
            )),
            target: None,
            path: None,
            revision: None,
            remote: None,
            no_setup: false,
            include: None,
            exclude: None,
            scope: ConfigScope::Project,
        },
        &NoopProgress,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        MountError::ShadowedOnMutate {
            shadowing_scope: ConfigScope::Local,
            ..
        }
    ));

    let error = mount(
        &repo,
        add_request(source.path(), Some("new-target"), ConfigScope::Local),
        &NoopProgress,
    )
    .unwrap_err();
    assert!(matches!(error, MountError::AlreadyExists { .. }));

    let error = mount(
        &repo,
        MountRequest::Remove {
            name,
            detach_only: false,
            scope: ConfigScope::Global,
        },
        &NoopProgress,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        MountError::WrongScope {
            actual_scope: ConfigScope::Local,
            ..
        }
    ));
}

#[test]
fn invalid_additions_fail_before_preparing_the_source() {
    let destination = git_repo();
    let repo = Repository::at(destination.path().to_path_buf());
    let mut local = Config::default();
    local.mounts.by_name.insert(
        MountName::from_string("vendor".to_string()),
        mount_config("local-vendor"),
    );
    repo.save_config_scoped(&local, ConfigScope::Local).unwrap();
    let missing_source = destination.path().join("missing-source");
    for scope in [ConfigScope::Local, ConfigScope::Project] {
        let error = mount(
            &repo,
            add_request(&missing_source, Some("vendor"), scope),
            &NoopProgress,
        )
        .unwrap_err();
        match scope {
            ConfigScope::Local => assert!(matches!(error, MountError::AlreadyExists { .. })),
            ConfigScope::Project => assert!(matches!(error, MountError::ShadowedOnAdd { .. })),
            ConfigScope::Global => unreachable!(),
        }
    }
}

#[test]
fn invalid_updates_fail_before_preparing_the_source() {
    let destination = git_repo();
    let repo = Repository::at(destination.path().to_path_buf());
    let name = MountName::from_string("vendor".to_string());
    let mut project = Config::default();
    let mut local = Config::default();

    for case in 0..3 {
        if case == 1 {
            local
                .mounts
                .by_name
                .insert(name.clone(), mount_config("local-vendor"));
            repo.save_config_scoped(&local, ConfigScope::Local).unwrap();
        } else if case == 2 {
            project
                .mounts
                .by_name
                .insert(name.clone(), mount_config("project-vendor"));
            repo.save_config_scoped(&project, ConfigScope::Project)
                .unwrap();
        }
        let error = mount(
            &repo,
            MountRequest::Update {
                name: name.clone(),
                location: Some(GitLocationSpec::from_string(
                    destination
                        .path()
                        .join("missing-source")
                        .display()
                        .to_string(),
                )),
                target: None,
                path: None,
                revision: None,
                remote: None,
                no_setup: false,
                include: None,
                exclude: None,
                scope: ConfigScope::Project,
            },
            &NoopProgress,
        )
        .unwrap_err();
        match case {
            0 => assert!(matches!(error, MountError::NotFound { .. })),
            1 => assert!(matches!(
                error,
                MountError::WrongScope {
                    actual_scope: ConfigScope::Local,
                    ..
                }
            )),
            2 => assert!(matches!(
                error,
                MountError::ShadowedOnMutate {
                    shadowing_scope: ConfigScope::Local,
                    ..
                }
            )),
            _ => unreachable!(),
        }
    }
}

#[test]
fn malformed_effective_mounts_fail_without_touching_desired_rows() {
    let destination = git_repo();
    let repo = Repository::at(destination.path().to_path_buf());
    let mut project = Config::default();
    project.mounts.by_name.insert(
        MountName::from_string("parent".to_string()),
        mount_config("vendor"),
    );
    project.mounts.by_name.insert(
        MountName::from_string("child".to_string()),
        mount_config("vendor/nested"),
    );
    repo.save_config_scoped(&project, ConfigScope::Project)
        .unwrap();

    let error = mount(&repo, MountRequest::List, &NoopProgress).unwrap_err();
    assert!(matches!(
        error,
        MountError::Repository(source)
            if matches!(*source, RepositoryError::InvalidEffectiveMounts(_))
    ));
    assert!(desired_paths(&repo).is_empty());
}

#[test]
fn detach_only_removes_configuration_but_preserves_owned_rows() {
    let source = source_repo(&[("asset.bin", '1')]);
    let destination = git_repo();
    let repo = Repository::at(destination.path().to_path_buf());
    mount(
        &repo,
        add_request(source.path(), Some("vendor"), ConfigScope::Project),
        &NoopProgress,
    )
    .unwrap();

    let outcome = mount(
        &repo,
        MountRequest::Remove {
            name: MountName::from_string("vendor".to_string()),
            detach_only: true,
            scope: ConfigScope::Project,
        },
        &NoopProgress,
    )
    .unwrap();
    assert!(matches!(
        outcome,
        MountOutcome::Removed {
            owned_rows: 1,
            detach_only: true,
            ..
        }
    ));
    assert_eq!(desired_paths(&repo), vec![gp("vendor/asset.bin")]);
    assert!(
        repo.load_config_scoped(ConfigScope::Project)
            .unwrap()
            .mounts
            .by_name
            .is_empty()
    );
}

#[test]
fn automatic_setup_imports_reuses_and_can_be_skipped() {
    let source = source_repo(&[("asset.bin", '2')]);
    let source_repo = Repository::at(source.path().to_path_buf());
    let remote = RemoteName::from_string("archive".to_string());
    let mut source_config = Config::default();
    source_config.remotes.default = Some(remote.clone());
    source_config.remotes.by_name.insert(
        remote.clone(),
        gat_core::endpoint::RemoteUrlTemplate::from_string(format!(
            "file://{}",
            source.path().display()
        ))
        .into(),
    );
    source_repo
        .save_config_scoped(&source_config, ConfigScope::Project)
        .unwrap();
    commit_all(source.path(), "source config");

    for (existing_name, no_setup, expected_name) in [
        (None, false, "archive"),
        (Some("mirror"), false, "mirror"),
        (Some("archive"), false, "archive-2"),
        (None, true, "archive"),
    ] {
        let destination = git_repo();
        let repo = Repository::at(destination.path().to_path_buf());
        if let Some(name) = existing_name {
            let mut config = Config::default();
            config.remotes.by_name.insert(
                name.into(),
                if name == "archive" {
                    gat_core::endpoint::RemoteUrlTemplate::from_string(format!(
                        "file://{}",
                        destination.path().display()
                    ))
                    .into()
                } else {
                    source_config.remotes.by_name[&remote].clone()
                },
            );
            config.remotes.default = Some(name.into());
            repo.save_config(&config).unwrap();
        }
        let expected_default = existing_name.map(RemoteName::from);
        let expected_count = if existing_name == Some("archive") {
            2
        } else {
            1
        };
        let mut request = add_request(source.path(), Some("vendor"), ConfigScope::Project);
        if let MountRequest::Add { no_setup: skip, .. } = &mut request {
            *skip = no_setup;
        }
        mount(&repo, request, &NoopProgress).unwrap();
        let config = repo.load_config().unwrap();
        assert_eq!(config.remotes.default, expected_default);
        if no_setup {
            assert!(config.remotes.by_name.is_empty());
            assert!(config.routes.by_name.is_empty());
        } else {
            assert_eq!(config.remotes.by_name.len(), expected_count);
            assert_eq!(
                config.routes.by_name["vendor"].remote.as_str(),
                expected_name
            );
        }

        mount(
            &repo,
            MountRequest::Update {
                name: "vendor".into(),
                location: None,
                target: None,
                path: None,
                revision: None,
                remote: None,
                no_setup: true,
                include: None,
                exclude: None,
                scope: ConfigScope::Project,
            },
            &NoopProgress,
        )
        .unwrap();
        let after_skip = repo.load_config().unwrap();
        assert_eq!(after_skip.remotes, config.remotes);
        assert_eq!(after_skip.routes, config.routes);

        // An opt-out is invocation-only: a later update can set storage up.
        mount(
            &repo,
            MountRequest::Update {
                name: "vendor".into(),
                location: None,
                target: None,
                path: None,
                revision: None,
                remote: None,
                no_setup: false,
                include: None,
                exclude: None,
                scope: ConfigScope::Project,
            },
            &NoopProgress,
        )
        .unwrap();
        let config = repo.load_config().unwrap();
        assert_eq!(config.remotes.by_name.len(), expected_count);
        assert_eq!(
            config.routes.by_name["vendor"].remote.as_str(),
            expected_name
        );
        assert_eq!(config.remotes.default, expected_default);
    }
}
