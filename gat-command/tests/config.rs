use gat_command::ConfigSource;
use gat_command::{
    ConfigAction, ConfigError, ConfigOutcome, ConfigRequest, ConfigScalarValue, config,
    config_with_lifecycle_observer,
};
use gat_core::config::{ConfigScope, IngestStrategy, MaterializationMode};
use gat_core::config_keys::ConfigKey;
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
    let ConfigError::UnknownKey { key, supported } = err else {
        panic!("expected an unknown key");
    };
    assert_eq!(key, "cache.link");
    for supported_key in ConfigKey::CANONICAL {
        assert!(
            supported.contains(supported_key.as_str()),
            "diagnostic omitted {supported_key}"
        );
    }
    assert!(!supported.contains("git.exclude_patterns"));

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
