use gat_core::config::{Config, ConfigScope};
use gat_core::settings::{SettingAssignment, SettingChange, SettingKey, SettingSource};
use gat_engine::Invocation;

#[test]
fn immutable_environment_is_shared_across_repositories_while_file_reads_stay_fresh() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let invocation = Invocation::from_pairs([("GAT_NETWORK_REQUEST_CONCURRENCY", "3")]).unwrap();
    for directory in [&first, &second] {
        let repo = invocation.repository_at(directory.path().to_path_buf());
        repo.change_setting(
            ConfigScope::Project,
            SettingChange::Set(
                SettingKey::SyncAutoFetch
                    .parse_values(&["true".into()])
                    .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            repo.read_setting(SettingKey::SyncAutoFetch).unwrap().1,
            SettingSource::Scope(ConfigScope::Project)
        );
        assert_eq!(
            repo.read_setting(SettingKey::NetworkRequestConcurrency)
                .unwrap()
                .1,
            SettingSource::Environment(SettingKey::NetworkRequestConcurrency)
        );
        repo.change_setting(
            ConfigScope::Project,
            SettingChange::Set(SettingAssignment::SyncAutoFetch(false)),
        )
        .unwrap();
        assert_eq!(repo.load_config().unwrap().sync.auto_fetch, Some(false));
    }
}

#[test]
fn environment_masking_never_persists_other_overrides_or_resources() {
    let directory = tempfile::tempdir().unwrap();
    let invocation = Invocation::from_pairs([
        ("GAT_SYNC_AUTO_FETCH", "true"),
        ("GAT_GIT_IGNORE_PATTERNS", "[\"secret-pattern\"]"),
        ("GAT_REMOTES_DEFAULT", "ignored"),
    ])
    .unwrap();
    let repo = invocation.repository_at(directory.path().to_path_buf());
    let source = repo
        .change_setting(
            ConfigScope::Project,
            SettingChange::Set(SettingAssignment::SyncAutoFetch(false)),
        )
        .unwrap();
    assert_eq!(
        source,
        SettingSource::Environment(SettingKey::SyncAutoFetch)
    );
    let persisted = repo.load_config_scoped(ConfigScope::Project).unwrap();
    assert_eq!(persisted.sync.auto_fetch, Some(false));
    assert!(persisted.git.ignore_patterns.is_none());
    assert!(persisted.remotes.default.is_none());
    repo.change_setting(
        ConfigScope::Project,
        SettingChange::Unset(SettingKey::SyncAutoFetch),
    )
    .unwrap();
    assert_eq!(
        repo.load_config_scoped(ConfigScope::Project).unwrap(),
        Config::default()
    );
    assert_eq!(repo.load_config().unwrap().sync.auto_fetch, Some(true));
}

#[test]
fn unrelated_dangling_default_does_not_block_resource_inspection_or_repair() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(directory.path().to_path_buf());
    let config = Config {
        selections: gat_core::config::SelectionsConfig {
            default: Some("missing".into()),
            ..Default::default()
        },
        ..Config::default()
    };
    repo.write_config_fixture(&config).unwrap();
    assert!(gat_engine::remote(&repo, gat_engine::RemoteRequest::List).is_ok());
    assert!(
        gat_engine::saved_selection(
            &repo,
            gat_engine::SelectionRequest::Default {
                action: gat_engine::DefaultAction::Unset,
                scope: ConfigScope::Project
            }
        )
        .is_ok()
    );
    assert!(repo.load_config().is_ok());
}
