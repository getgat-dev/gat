use std::path::Path;

use gat_command::{
    InitConfigOutcome, InitError, InitGitIntegrationOutcome, InitHooksOutcome, InitOutcome,
    InitRequest, init,
};
use gat_engine::InitializationErrorKind;
use gat_engine::Repository;

fn git(dir: &Path, args: &[&str]) {
    test_support_git::run_git(dir, args);
}

fn git_repo() -> tempfile::TempDir {
    test_support_git::empty_git_repo()
}

fn converge(repo: &Repository, request: InitRequest) -> InitOutcome {
    converge_result(repo, request).expect("init")
}

fn converge_result(repo: &Repository, request: InitRequest) -> Result<InitOutcome, InitError> {
    init(repo, request)
}

fn hook_installed(root: &Path) -> bool {
    std::fs::read_to_string(root.join(".git/hooks/post-checkout"))
        .is_ok_and(|contents| contents.contains("gat hook post-checkout"))
}

fn merge_driver_installed(root: &Path) -> bool {
    std::fs::read_to_string(root.join(".git/config"))
        .is_ok_and(|contents| contents.contains("[merge \"gat-lock\"]"))
}

fn attributes_installed(root: &Path) -> bool {
    std::fs::read_to_string(root.join(".git/info/attributes"))
        .is_ok_and(|contents| contents.contains("gat-lock"))
}

#[test]
fn converges_all_hook_and_merge_driver_flag_combinations() {
    for (no_hooks, no_merge_driver) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let tmp = git_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        converge(
            &repo,
            InitRequest {
                no_hooks,
                no_merge_driver,
                example_config: false,
            },
        );

        assert_eq!(hook_installed(tmp.path()), !no_hooks);
        assert_eq!(merge_driver_installed(tmp.path()), !no_merge_driver);
        assert_eq!(attributes_installed(tmp.path()), !no_merge_driver);
    }
}

#[test]
fn repeated_convergence_reports_stable_state() {
    let tmp = git_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    converge(&repo, InitRequest::default());

    let installed = converge(&repo, InitRequest::default());
    assert_eq!(installed.hooks, InitHooksOutcome::AlreadyInstalled);
    assert_eq!(
        installed.merge_driver,
        InitGitIntegrationOutcome::AlreadyInstalled
    );
    assert_eq!(
        installed.attributes,
        InitGitIntegrationOutcome::AlreadyInstalled
    );

    let removal = InitRequest {
        no_hooks: true,
        no_merge_driver: true,
        example_config: false,
    };
    converge(&repo, removal);
    let absent = converge(&repo, removal);
    assert_eq!(absent.hooks, InitHooksOutcome::AlreadyAbsent);
    assert_eq!(
        absent.merge_driver,
        InitGitIntegrationOutcome::AlreadyAbsent
    );
    assert_eq!(absent.attributes, InitGitIntegrationOutcome::AlreadyAbsent);
}

#[test]
fn example_config_is_opt_in_and_never_overwrites() {
    let tmp = git_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let config_path = tmp.path().join("gat.yaml");

    let plain = converge(&repo, InitRequest::default());
    assert_eq!(plain.config, None);
    assert!(!config_path.exists());

    std::fs::write(&config_path, "version: 1\n").expect("seed config");
    let existing = converge(
        &repo,
        InitRequest {
            example_config: true,
            ..InitRequest::default()
        },
    );
    assert_eq!(existing.config, Some(InitConfigOutcome::AlreadyPresent));
    assert_eq!(
        std::fs::read_to_string(config_path).expect("read config"),
        "version: 1\n"
    );
}

#[test]
fn example_config_creation_is_inert() {
    let tmp = git_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let outcome = converge(
        &repo,
        InitRequest {
            example_config: true,
            ..InitRequest::default()
        },
    );

    assert_eq!(outcome.config, Some(InitConfigOutcome::Created));
    let contents = std::fs::read_to_string(tmp.path().join("gat.yaml")).expect("read config");
    assert!(
        contents
            .lines()
            .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
    );

    let example = contents
        .split_once("\n\n")
        .expect("header separated from example")
        .1
        .lines()
        .map(|line| line.strip_prefix("# ").expect("commented example line"))
        .collect::<Vec<_>>()
        .join("\n");
    let example_path = tmp.path().join("example.yaml");
    std::fs::write(&example_path, example).expect("write uncommented example");
    let config = gat_engine::load_config_file(&example_path).expect("valid example config");
    assert!(config.remotes.default.is_none());
    assert_eq!(
        config.remotes.by_name["origin"].url.as_template_str(),
        "s3://bucket/prefix"
    );
    assert_eq!(
        config.cache.materialization_strategy.unwrap().modes(),
        &[
            gat_core::config::MaterializationMode::Hardlink,
            gat_core::config::MaterializationMode::Copy,
        ]
    );
    assert_eq!(config.sync.auto_fetch, Some(true));
    assert_eq!(config.sync.auto_repair, Some(true));
    let selection = config.selections.by_name["example"].clone();
    assert_eq!(selection.include[0].as_str(), "data/**");
    assert_eq!(selection.exclude[0].as_str(), "data/tmp/**");
    assert_eq!(
        config.lock.shard_levels,
        Some(gat_core::lock::LockShardLevels::FLAT)
    );
    assert_eq!(
        config.git.ignore_patterns.unwrap()[0].as_str(),
        "*.safetensors"
    );
}

#[test]
fn hook_and_git_content_survive_install_and_removal() {
    let tmp = git_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let hooks = tmp.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    std::fs::write(hooks.join("post-checkout"), "#!/bin/sh\r\necho custom\r\n")
        .expect("custom hook");
    git(
        tmp.path(),
        &["config", "--local", "user.name", "Someone Else"],
    );
    let attributes = tmp.path().join(".git/info/attributes");
    std::fs::create_dir_all(attributes.parent().expect("attributes parent")).expect("info dir");
    std::fs::write(&attributes, "*.bin binary\r\n").expect("attributes");

    converge(&repo, InitRequest::default());
    converge(
        &repo,
        InitRequest {
            no_hooks: true,
            no_merge_driver: true,
            example_config: false,
        },
    );

    assert_eq!(
        std::fs::read_to_string(hooks.join("post-checkout")).expect("hook"),
        "#!/bin/sh\r\necho custom\r\n"
    );
    assert!(
        std::fs::read_to_string(tmp.path().join(".git/config"))
            .expect("git config")
            .contains("Someone Else")
    );
    assert_eq!(
        std::fs::read_to_string(attributes).expect("attributes"),
        "*.bin binary\r\n"
    );
}

#[test]
fn linked_worktree_uses_the_common_git_directory() {
    let tmp = git_repo();
    git(
        tmp.path(),
        &["commit", "--allow-empty", "-q", "-m", "initial"],
    );
    let worktree_parent = tempfile::tempdir().expect("worktree parent");
    let worktree = worktree_parent.path().join("linked");
    git(
        tmp.path(),
        &[
            "worktree",
            "add",
            worktree.to_str().expect("utf8 path"),
            "-b",
            "linked",
        ],
    );
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(worktree);

    converge(&repo, InitRequest::default());

    assert!(hook_installed(tmp.path()));
    assert!(merge_driver_installed(tmp.path()));
    assert!(attributes_installed(tmp.path()));
}

#[test]
fn explicit_cache_override_wins_without_ambient_environment() {
    let tmp = git_repo();
    let _repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let global = tempfile::tempdir().expect("global config");
    std::fs::write(
        global.path().join("gat.yaml"),
        "cache:\n  location: ignored\n",
    )
    .expect("global config");

    let repo = gat_engine::Invocation::from_pairs([("GAT_CACHE_LOCATION", "explicit-cache")])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let outcome = init(
        &repo,
        InitRequest {
            no_hooks: true,
            no_merge_driver: true,
            example_config: false,
        },
    )
    .expect("init");

    assert_eq!(
        outcome.cache_location.display_path(),
        tmp.path().join("explicit-cache")
    );
}

#[test]
fn unreadable_hook_fails_after_merge_integration_without_touching_hook() {
    let tmp = git_repo();
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
    let path = tmp.path().join(".git/hooks/post-checkout");
    std::fs::create_dir_all(path.parent().expect("hook parent")).expect("hooks dir");
    let bytes = [0x23, 0x21, 0x0a, 0x80, 0x81];
    std::fs::write(&path, bytes).expect("invalid hook");

    let error = converge_result(&repo, InitRequest::default()).expect_err("must fail");

    assert!(matches!(
        error,
        InitError::Engine(ref source)
            if source.kind() == InitializationErrorKind::NonUtf8Hook
    ));
    assert_eq!(std::fs::read(path).expect("hook bytes"), bytes);
    assert!(merge_driver_installed(tmp.path()));
    assert!(attributes_installed(tmp.path()));
}
