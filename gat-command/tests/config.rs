use gat_command::ConfigSource;
use gat_command::{
    ConfigAction, ConfigError, ConfigOutcome, ConfigRequest, ConfigScalarValue, config,
    config_with_lifecycle_observer,
};
use gat_core::config::{ConfigScope, IngestStrategy, MaterializationMode};
use gat_core::config_keys::{ConfigKey, ConfigResource};
use gat_core::lifecycle::Surface;
use gat_core::lock::LockShardLevels;
use gat_engine::Repository;

fn repository() -> (tempfile::TempDir, Repository) {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::at(temp.path().to_path_buf());
    (temp, repo)
}

const fn request(key: ConfigKey, action: ConfigAction) -> ConfigRequest {
    ConfigRequest {
        key,
        action,
        scope: ConfigScope::Project,
    }
}

const fn scoped_request(key: ConfigKey, action: ConfigAction, scope: ConfigScope) -> ConfigRequest {
    ConfigRequest { key, action, scope }
}

#[test]
fn raw_keys_and_action_cardinality_follow_the_typed_registry() {
    let err = ConfigRequest::from_raw(
        "cache.link".to_string(),
        ConfigAction::Get,
        ConfigScope::Project,
    )
    .unwrap_err();
    let ConfigError::UnknownKey { key } = err else {
        panic!("expected an unknown key");
    };
    assert_eq!(key, "cache.link");

    let (_temp, repo) = repository();
    for key in ConfigKey::CANONICAL {
        config(&repo, request(key, ConfigAction::Get))
            .unwrap_or_else(|error| panic!("{key} has no command handler: {error}"));
    }

    assert!(matches!(
        config(
            &repo,
            request(ConfigKey::CacheLocation, ConfigAction::Set(Vec::new()))
        ),
        Err(ConfigError::WrongValueCount {
            key: ConfigKey::CacheLocation,
            got: 0
        })
    ));
    assert!(matches!(
        config(
            &repo,
            request(ConfigKey::CacheLocation, ConfigAction::Clear)
        ),
        Err(ConfigError::ClearNotSupportedForScalar {
            key: ConfigKey::CacheLocation
        })
    ));
    assert!(matches!(
        config(
            &repo,
            request(ConfigKey::GitIgnorePatterns, ConfigAction::Set(Vec::new()))
        ),
        Err(ConfigError::EmptyListNotAllowed {
            key: ConfigKey::GitIgnorePatterns
        })
    ));
}

#[test]
fn resource_keys_are_rejected_before_execution_for_every_config_action() {
    for (key, expected) in [
        ("remotes.origin.url", ConfigResource::Remote),
        ("remotes.default", ConfigResource::Remote),
        ("routes.data.remote", ConfigResource::Route),
        ("mounts.models.rev", ConfigResource::Mount),
        ("mounts.models.rev_lock", ConfigResource::Mount),
        ("selections.training.include", ConfigResource::Selection),
        ("selections.default", ConfigResource::Selection),
        ("mounts", ConfigResource::Mount),
        ("routes.data.unknown", ConfigResource::Route),
    ] {
        for action in [
            ConfigAction::Get,
            ConfigAction::Set(vec!["value".to_string()]),
            ConfigAction::Clear,
            ConfigAction::Unset,
        ] {
            let expected_read_only = action == ConfigAction::Get;
            let error =
                ConfigRequest::from_raw(key.to_string(), action, ConfigScope::Local).unwrap_err();
            assert!(matches!(
                error,
                ConfigError::ManagedResource { key: rejected, resource, read_only }
                    if rejected == key && resource == expected && read_only == expected_read_only
            ));
        }
    }
    for key in [
        "mounts_extra.models.rev",
        "remote.origin.url",
        "cache.unknown",
    ] {
        assert!(matches!(
            ConfigRequest::from_raw(key.to_string(), ConfigAction::Get, ConfigScope::Project),
            Err(ConfigError::UnknownKey { .. })
        ));
    }
}

#[test]
fn materialization_strategy_preserves_order_and_rejected_changes_do_not_write() {
    let (temp, repo) = repository();
    let set = config(
        &repo,
        request(
            ConfigKey::CacheMaterializationStrategy,
            ConfigAction::Set(vec![
                "reflink".to_string(),
                "hardlink".to_string(),
                "copy".to_string(),
            ]),
        ),
    )
    .unwrap();
    assert_eq!(
        set,
        ConfigOutcome::SetList {
            key: ConfigKey::CacheMaterializationStrategy,
            values: vec![
                "reflink".to_string(),
                "hardlink".to_string(),
                "copy".to_string()
            ],
        }
    );
    assert_eq!(
        repo.load_config()
            .unwrap()
            .cache
            .materialization_strategy
            .unwrap()
            .modes(),
        &[
            MaterializationMode::Reflink,
            MaterializationMode::Hardlink,
            MaterializationMode::Copy
        ]
    );

    let path = temp.path().join("gat.yaml");
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        config(
            &repo,
            request(
                ConfigKey::CacheMaterializationStrategy,
                ConfigAction::Set(vec!["teleport".to_string()])
            )
        ),
        Err(ConfigError::Config(
            gat_core::config::ConfigError::InvalidLinkMode { .. }
        ))
    ));
    assert!(matches!(
        config(
            &repo,
            request(
                ConfigKey::CacheMaterializationStrategy,
                ConfigAction::Set(vec!["copy".to_string(), "copy".to_string()])
            )
        ),
        Err(ConfigError::Config(
            gat_core::config::ConfigError::DuplicateLinkMode { .. }
        ))
    ));
    assert!(matches!(
        config(
            &repo,
            request(ConfigKey::CacheMaterializationStrategy, ConfigAction::Clear)
        ),
        Err(ConfigError::ClearAlwaysInvalid {
            key: ConfigKey::CacheMaterializationStrategy,
            ..
        })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);

    config(
        &repo,
        request(ConfigKey::CacheMaterializationStrategy, ConfigAction::Unset),
    )
    .unwrap();
    assert_eq!(
        config(
            &repo,
            request(ConfigKey::CacheMaterializationStrategy, ConfigAction::Get)
        )
        .unwrap(),
        ConfigOutcome::Values {
            key: ConfigKey::CacheMaterializationStrategy,
            values: vec!["copy".to_string()],
            source: ConfigSource::Default
        }
    );
}

#[test]
fn ingest_strategy_is_typed_and_lifecycle_reads_reuse_the_config_snapshot() {
    let (_temp, repo) = repository();
    let observed = std::cell::RefCell::new(Vec::new());
    #[cfg(feature = "test-support")]
    let before = gat_engine::test_support::config_loads();
    let outcome = config_with_lifecycle_observer(
        &repo,
        request(ConfigKey::CacheIngestStrategy, ConfigAction::Get),
        &|surface| {
            if let Surface::ConfigValue { key, value } = surface {
                observed
                    .borrow_mut()
                    .push((key.to_string(), value.to_string()));
            }
        },
    )
    .unwrap();
    #[cfg(feature = "test-support")]
    assert_eq!(gat_engine::test_support::config_loads() - before, 1);
    assert_eq!(
        outcome,
        ConfigOutcome::Value {
            key: ConfigKey::CacheIngestStrategy,
            value: ConfigScalarValue::IngestStrategy(IngestStrategy::Safe),
            source: ConfigSource::Default
        }
    );
    assert_eq!(
        observed.into_inner(),
        vec![("cache.ingest_strategy".to_string(), "safe".to_string())]
    );

    for (raw, expected) in [
        ("hybrid", IngestStrategy::Hybrid),
        ("mmap", IngestStrategy::Mmap),
    ] {
        assert_eq!(
            config(
                &repo,
                request(
                    ConfigKey::CacheIngestStrategy,
                    ConfigAction::Set(vec![raw.to_string()])
                )
            )
            .unwrap(),
            ConfigOutcome::Set {
                key: ConfigKey::CacheIngestStrategy,
                value: ConfigScalarValue::IngestStrategy(expected),
            }
        );
    }
    assert!(matches!(
        config(
            &repo,
            request(
                ConfigKey::CacheIngestStrategy,
                ConfigAction::Set(vec!["teleport".to_string()])
            )
        ),
        Err(ConfigError::Config(
            gat_core::config::ConfigError::InvalidIngestStrategy { .. }
        ))
    ));
}

#[test]
fn cache_location_stays_typed_and_get_loads_config_once() {
    let (temp, repo) = repository();
    let outcome = config(
        &repo,
        request(
            ConfigKey::CacheLocation,
            ConfigAction::Set(vec!["shared-cache".to_string()]),
        ),
    )
    .unwrap();
    assert!(matches!(
        outcome,
        ConfigOutcome::Set {
            key: ConfigKey::CacheLocation,
            value: ConfigScalarValue::CacheLocation(ref value),
        } if value.as_path() == std::path::Path::new("shared-cache")
    ));

    #[cfg(feature = "test-support")]
    let before_loads = gat_engine::test_support::config_loads();
    let outcome = config(&repo, request(ConfigKey::CacheLocation, ConfigAction::Get)).unwrap();
    #[cfg(feature = "test-support")]
    assert_eq!(gat_engine::test_support::config_loads() - before_loads, 1);
    assert_eq!(
        outcome,
        ConfigOutcome::Value {
            key: ConfigKey::CacheLocation,
            value: ConfigScalarValue::Path(temp.path().join("shared-cache")),
            source: ConfigSource::Scope(ConfigScope::Project)
        }
    );
}

#[test]
fn boolean_settings_are_typed_and_local_false_overrides_project_true() {
    let (_temp, repo) = repository();
    for key in [
        ConfigKey::SyncTrustState,
        ConfigKey::SyncAutoFetch,
        ConfigKey::SyncAutoRepair,
    ] {
        assert_eq!(
            config(&repo, request(key, ConfigAction::Get)).unwrap(),
            ConfigOutcome::Value {
                key,
                value: ConfigScalarValue::Boolean(false),
                source: ConfigSource::Default
            }
        );
        assert_eq!(
            config(
                &repo,
                request(key, ConfigAction::Set(vec!["true".to_string()]))
            )
            .unwrap(),
            ConfigOutcome::Set {
                key,
                value: ConfigScalarValue::Boolean(true),
            }
        );
        assert!(matches!(
            config(
                &repo,
                request(key, ConfigAction::Set(vec!["nope".to_string()]))
            ),
            Err(ConfigError::Config(
                gat_core::config::ConfigError::InvalidBooleanValue { .. }
            ))
        ));
        config(
            &repo,
            scoped_request(
                key,
                ConfigAction::Set(vec!["false".to_string()]),
                ConfigScope::Local,
            ),
        )
        .unwrap();
        assert_eq!(
            config(&repo, request(key, ConfigAction::Get)).unwrap(),
            ConfigOutcome::Value {
                key,
                value: ConfigScalarValue::Boolean(false),
                source: ConfigSource::Scope(ConfigScope::Local)
            }
        );
    }
}

#[test]
fn shard_levels_are_typed_and_invalid_values_do_not_replace_the_saved_value() {
    let (temp, repo) = repository();
    assert_eq!(
        config(
            &repo,
            request(ConfigKey::LockShardLevels, ConfigAction::Get)
        )
        .unwrap(),
        ConfigOutcome::Value {
            key: ConfigKey::LockShardLevels,
            value: ConfigScalarValue::ShardLevels(LockShardLevels::new(0).unwrap()),
            source: ConfigSource::Default
        }
    );
    config(
        &repo,
        request(
            ConfigKey::LockShardLevels,
            ConfigAction::Set(vec!["2".to_string()]),
        ),
    )
    .unwrap();
    let before = std::fs::read(temp.path().join("gat.yaml")).unwrap();
    for invalid in ["5", "not-a-number"] {
        assert!(matches!(
            config(
                &repo,
                request(
                    ConfigKey::LockShardLevels,
                    ConfigAction::Set(vec![invalid.to_string()])
                )
            ),
            Err(ConfigError::Config(_))
        ));
    }
    assert_eq!(std::fs::read(temp.path().join("gat.yaml")).unwrap(), before);
    assert_eq!(
        repo.load_config().unwrap().lock.shard_levels,
        Some(LockShardLevels::new(2).unwrap())
    );
}

#[test]
fn ignore_patterns_preserve_commas_and_alias_writes_the_canonical_key() {
    let (temp, repo) = repository();
    let aliases = std::cell::Cell::new(0);
    let values = vec![
        "*.safetensors".to_string(),
        "/artifacts,legacy/**/*.bin".to_string(),
    ];
    let outcome = config_with_lifecycle_observer(
        &repo,
        request(
            ConfigKey::GitExcludePatterns,
            ConfigAction::Set(values.clone()),
        ),
        &|surface| {
            if matches!(
                surface,
                Surface::ConfigAlias {
                    canonical: "git.ignore_patterns",
                    alias: "git.exclude_patterns"
                }
            ) {
                aliases.set(aliases.get() + 1);
            }
        },
    )
    .unwrap();
    assert_eq!(aliases.get(), 1);
    assert_eq!(
        outcome,
        ConfigOutcome::SetList {
            key: ConfigKey::GitIgnorePatterns,
            values: values.clone(),
        }
    );
    assert_eq!(
        config(
            &repo,
            request(ConfigKey::GitExcludePatterns, ConfigAction::Get)
        )
        .unwrap(),
        ConfigOutcome::Values {
            key: ConfigKey::GitIgnorePatterns,
            values: values.clone(),
            source: ConfigSource::Scope(ConfigScope::Project)
        }
    );
    let yaml = std::fs::read_to_string(temp.path().join("gat.yaml")).unwrap();
    assert!(yaml.contains("ignore_patterns"));
    assert!(!yaml.contains("exclude_patterns"));

    config(
        &repo,
        scoped_request(
            ConfigKey::GitIgnorePatterns,
            ConfigAction::Clear,
            ConfigScope::Local,
        ),
    )
    .unwrap();
    assert_eq!(
        config(
            &repo,
            request(ConfigKey::GitIgnorePatterns, ConfigAction::Get)
        )
        .unwrap(),
        ConfigOutcome::Values {
            key: ConfigKey::GitIgnorePatterns,
            values: Vec::new(),
            source: ConfigSource::Scope(ConfigScope::Local)
        }
    );
    config(
        &repo,
        scoped_request(
            ConfigKey::GitIgnorePatterns,
            ConfigAction::Unset,
            ConfigScope::Local,
        ),
    )
    .unwrap();
    assert_eq!(
        config(
            &repo,
            request(ConfigKey::GitIgnorePatterns, ConfigAction::Get)
        )
        .unwrap(),
        ConfigOutcome::Values {
            key: ConfigKey::GitIgnorePatterns,
            values,
            source: ConfigSource::Scope(ConfigScope::Project)
        }
    );
}

#[test]
fn saved_selections_resolve_without_worktree_or_lock_and_keep_sparse_definitions() {
    use gat_command::{SelectionOutcome, SelectionRequest, named_selection, saved_selection};
    use gat_core::config::SelectionConfig;
    use gat_core::globs::GatGlobPattern;
    use gat_core::lexical_path::GatSubpath;

    let (temp, repo) = repository();
    for (name, definition, matches_future) in [
        (
            "future",
            SelectionConfig {
                path: GatSubpath::normalize("future-assets").unwrap(),
                ..Default::default()
            },
            true,
        ),
        (
            "none",
            SelectionConfig {
                exclude: Some(vec![GatGlobPattern::parse("**").unwrap()]),
                ..Default::default()
            },
            false,
        ),
    ] {
        let outcome = saved_selection(
            &repo,
            SelectionRequest::Add {
                name: name.into(),
                definition,
                scope: ConfigScope::Project,
            },
        )
        .unwrap();
        assert!(matches!(
            outcome,
            SelectionOutcome::Saved {
                unrestricted: false,
                ..
            }
        ));
        let selection = named_selection(&repo, &name.into()).unwrap();
        assert!(!selection.matches_str("root.bin"));
        assert_eq!(
            selection.matches_str("future-assets/model.bin"),
            matches_future
        );
        // Resolving consumes only the in-memory configuration, not the saved definition.
        assert!(named_selection(&repo, &name.into()).is_ok());
    }
    assert!(!temp.path().join("gat.lock").exists());
    assert!(!temp.path().join("future-assets").exists());
    std::fs::write(
        temp.path().join("gat.yaml"),
        "selections:\n  all: {}\n  default: all\n",
    )
    .unwrap();
    let selection = named_selection(&repo, &"all".into()).unwrap();
    assert!(selection.is_unrestricted());
    assert!(selection.matches_str("root.bin"));
    assert!(selection.matches_str("future-assets/model.bin"));
    let outcome = saved_selection(
        &repo,
        SelectionRequest::Default {
            name: None,
            unset: false,
            scope: ConfigScope::Project,
        },
    )
    .unwrap();
    let SelectionOutcome::Default {
        record: Some(record),
        ..
    } = outcome
    else {
        panic!("expected the persisted default");
    };
    assert!(record.definition.is_unrestricted());
}

#[test]
fn default_mutations_report_candidate_provenance_from_one_config_snapshot() {
    use gat_command::{RemoteOutcome, RemoteRequest, SelectionOutcome, SelectionRequest};
    use gat_core::config::{Config, RemoteConfig};
    use gat_core::endpoint::RemoteUrlTemplate;

    for selection in [true, false] {
        let (_temp, repo) = repository();
        let mut config = Config::default();
        for name in ["one", "two"] {
            config
                .selections
                .by_name
                .insert(name.into(), Default::default());
            config.remotes.by_name.insert(
                name.into(),
                RemoteConfig {
                    // hygiene-ok: configuration-only fixture; no remote is opened.
                    url: RemoteUrlTemplate::from_string("file:///unused".into()),
                },
            );
        }
        repo.save_config_scoped(&config, ConfigScope::Project)
            .unwrap();
        for (name, scope, expected, chosen) in [
            (
                Some("one"),
                ConfigScope::Project,
                Some("one"),
                Some(ConfigScope::Project),
            ),
            (
                Some("two"),
                ConfigScope::Local,
                Some("two"),
                Some(ConfigScope::Local),
            ),
            // A project edit must still report the higher-priority local choice.
            (
                Some("one"),
                ConfigScope::Project,
                Some("two"),
                Some(ConfigScope::Local),
            ),
            (
                None,
                ConfigScope::Local,
                Some("one"),
                Some(ConfigScope::Project),
            ),
            (None, ConfigScope::Project, None, None),
        ] {
            #[cfg(feature = "test-support")]
            let before = gat_engine::test_support::config_loads();
            let (actual, chosen_in, defined_in) = if selection {
                let SelectionOutcome::Default { record, chosen_in } = gat_command::saved_selection(
                    &repo,
                    SelectionRequest::Default {
                        name: name.map(Into::into),
                        unset: name.is_none(),
                        scope,
                    },
                )
                .unwrap() else {
                    panic!("expected selection default");
                };
                assert!(record.as_ref().is_none_or(|record| record.is_default));
                let defined_in = record.as_ref().map(|record| record.scope);
                (
                    record.map(|record| record.name.as_str().to_owned()),
                    chosen_in,
                    defined_in,
                )
            } else {
                let RemoteOutcome::Default {
                    name,
                    chosen_in,
                    defined_in,
                } = gat_command::remote(
                    &repo,
                    RemoteRequest::Default {
                        name: name.map(Into::into),
                        unset: name.is_none(),
                        scope,
                    },
                )
                .unwrap()
                else {
                    panic!("expected remote default");
                };
                (
                    name.map(|name| name.as_str().to_owned()),
                    chosen_in,
                    defined_in,
                )
            };
            #[cfg(feature = "test-support")]
            assert_eq!(gat_engine::test_support::config_loads() - before, 1);
            assert_eq!(actual.as_deref(), expected);
            assert_eq!(chosen_in, chosen);
            assert_eq!(defined_in, expected.map(|_| ConfigScope::Project));
            let saved = repo.load_config_scoped(scope).unwrap();
            let saved_name = if selection {
                saved
                    .selections
                    .default
                    .map(|name| name.as_str().to_owned())
            } else {
                saved.remotes.default.map(|name| name.as_str().to_owned())
            };
            assert_eq!(saved_name.as_deref(), name);
        }
    }
}
