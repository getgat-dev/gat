//! `gat config` orchestration over semantic configuration values.

use gat_core::cache_location::CacheLocation;
use gat_core::config::{
    ConfigScope, DEFAULT_INGEST_STRATEGY, IngestStrategy, MaterializationStrategy,
    parse_shard_levels, parse_sync_bool_setting, validate_ignore_patterns,
};
use gat_core::config_keys::{ConfigKey, ConfigResource, ValueCardinality};
use gat_core::lifecycle::Surface;
use gat_core::lock::LockShardLevels;
use gat_engine::{Repository, RepositoryError};
use std::path::PathBuf;

type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigAction {
    Get,
    Set(Vec<String>),
    Clear,
    Unset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigRequest {
    pub key: ConfigKey,
    pub action: ConfigAction,
    pub scope: ConfigScope,
}

impl ConfigRequest {
    pub fn from_raw(key: String, action: ConfigAction, scope: ConfigScope) -> Result<Self> {
        let Some(key_value) = ConfigKey::parse(&key) else {
            if let Some(resource) = ConfigResource::from_key(&key) {
                return Err(ConfigError::ManagedResource {
                    key,
                    resource,
                    read_only: matches!(action, ConfigAction::Get),
                });
            }
            return Err(ConfigError::UnknownKey { key });
        };
        Ok(Self {
            key: key_value,
            action,
            scope,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigScalarValue {
    CacheLocation(CacheLocation),
    Path(PathBuf),
    IngestStrategy(IngestStrategy),
    Boolean(bool),
    ShardLevels(LockShardLevels),
}

/// Origin of an effective setting, independent of the requested write scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    Default,
    Scope(ConfigScope),
    CacheEnvironment,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigOutcome {
    Value {
        key: ConfigKey,
        value: ConfigScalarValue,
        source: ConfigSource,
    },
    Values {
        key: ConfigKey,
        values: Vec<String>,
        source: ConfigSource,
    },
    Set {
        key: ConfigKey,
        value: ConfigScalarValue,
    },
    SetList {
        key: ConfigKey,
        values: Vec<String>,
    },
    Cleared {
        key: ConfigKey,
    },
    Unset {
        key: ConfigKey,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config key `{key}` belongs to a managed resource")]
    ManagedResource {
        key: String,
        resource: ConfigResource,
        read_only: bool,
    },

    #[error("unknown config key `{key}`")]
    UnknownKey { key: String },

    #[error("`{key}` takes exactly one value (got {got})")]
    WrongValueCount { key: ConfigKey, got: usize },

    #[error("`--clear` isn't supported for scalar key `{key}`")]
    ClearNotSupportedForScalar { key: ConfigKey },

    #[error("`{key}` takes one or more values")]
    EmptyListNotAllowed { key: ConfigKey },

    #[error("`{key}` cannot be cleared to an empty list")]
    ClearAlwaysInvalid {
        key: ConfigKey,
        #[source]
        source: gat_core::config::ConfigError,
    },

    #[error(transparent)]
    Config(#[from] gat_core::config::ConfigError),

    #[error(transparent)]
    Repository(Box<RepositoryError>),
}

impl From<RepositoryError> for ConfigError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(Box::new(error))
    }
}

pub fn config(repo: &Repository, request: ConfigRequest) -> Result<ConfigOutcome> {
    config_with_lifecycle_observer(repo, request, &|_| {})
}

#[allow(
    clippy::missing_panics_doc,
    reason = "The ingest strategy was just assigned Some"
)]
pub fn config_with_lifecycle_observer(
    repo: &Repository,
    request: ConfigRequest,
    observe: &dyn Fn(Surface<'_>),
) -> Result<ConfigOutcome> {
    validate_action(request.key, &request.action)?;

    if request.key == ConfigKey::GitExcludePatterns {
        observe(Surface::ConfigAlias {
            canonical: ConfigKey::GitIgnorePatterns.as_str(),
            alias: ConfigKey::GitExcludePatterns.as_str(),
        });
    }

    let read_snapshot = if request.action == ConfigAction::Get {
        let layers = repo.load_config_layers()?;
        let source =
            crate::resource::defining_scope(&layers, |cfg| key_is_defined(cfg, request.key))
                .map_or(ConfigSource::Default, ConfigSource::Scope);
        Some((layers.effective()?, source))
    } else {
        None
    };
    let source = read_snapshot
        .as_ref()
        .map_or(ConfigSource::Default, |(_, source)| *source);
    let mut effective_for_lifecycle = read_snapshot.map(|(config, _)| config);
    if request.key == ConfigKey::CacheIngestStrategy {
        let observed = if let ConfigAction::Set(values) = &request.action {
            values.first().cloned().unwrap_or_default()
        } else {
            let effective = match effective_for_lifecycle.take() {
                Some(config) => config,
                None => repo.load_config()?,
            };
            let observed = effective
                .cache
                .ingest_strategy
                .unwrap_or(DEFAULT_INGEST_STRATEGY)
                .to_string();
            effective_for_lifecycle = Some(effective);
            observed
        };
        observe(Surface::ConfigValue {
            key: ConfigKey::CacheIngestStrategy.as_str(),
            value: &observed,
        });
    }

    let mut cfg = match &request.action {
        ConfigAction::Get => match effective_for_lifecycle {
            Some(effective) => effective,
            None => repo.load_config()?,
        },
        _ => repo.load_config_scoped(request.scope)?,
    };

    let key = request.key;
    let canonical = key.canonical();
    match key {
        ConfigKey::CacheLocation => match request.action {
            ConfigAction::Set(values) => {
                let value = one(values);
                cfg.cache.location = Some(CacheLocation::from_path(PathBuf::from(&value)));
                repo.save_config_scoped(&cfg, request.scope)?;
                Ok(ConfigOutcome::Set {
                    key: canonical,
                    value: ConfigScalarValue::CacheLocation(CacheLocation::from_path(
                        PathBuf::from(value),
                    )),
                })
            }
            ConfigAction::Unset => {
                cfg.cache.location = None;
                save_unset(repo, &cfg, request.scope, canonical)
            }
            ConfigAction::Get => {
                let (location, origin) = repo.resolve_cache_location(&cfg);
                Ok(ConfigOutcome::Value {
                    key: canonical,
                    value: ConfigScalarValue::Path(location.display_path().to_path_buf()),
                    source: if origin == gat_engine::CacheLocationOrigin::Environment {
                        ConfigSource::CacheEnvironment
                    } else {
                        source
                    },
                })
            }
            ConfigAction::Clear => unreachable!("scalar clear rejected"),
        },
        ConfigKey::CacheMaterializationStrategy => match request.action {
            ConfigAction::Set(values) => {
                cfg.cache.materialization_strategy =
                    Some(MaterializationStrategy::from_values(&values)?);
                repo.save_config_scoped(&cfg, request.scope)?;
                Ok(ConfigOutcome::SetList {
                    key: canonical,
                    values,
                })
            }
            ConfigAction::Clear => {
                let source = MaterializationStrategy::from_values::<String>(&[])
                    .expect_err("empty materialization strategies are invalid");
                Err(ConfigError::ClearAlwaysInvalid {
                    key: canonical,
                    source,
                })
            }
            ConfigAction::Unset => {
                cfg.cache.materialization_strategy = None;
                save_unset(repo, &cfg, request.scope, canonical)
            }
            ConfigAction::Get => Ok(ConfigOutcome::Values {
                key: canonical,
                values: cfg
                    .cache
                    .materialization_strategy
                    .unwrap_or_default()
                    .modes()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                source,
            }),
        },
        ConfigKey::CacheIngestStrategy => match request.action {
            ConfigAction::Set(values) => {
                let value = one(values);
                cfg.cache.ingest_strategy = Some(value.parse()?);
                repo.save_config_scoped(&cfg, request.scope)?;
                Ok(ConfigOutcome::Set {
                    key: canonical,
                    value: ConfigScalarValue::IngestStrategy(
                        cfg.cache.ingest_strategy.expect("just assigned"),
                    ),
                })
            }
            ConfigAction::Unset => {
                cfg.cache.ingest_strategy = None;
                save_unset(repo, &cfg, request.scope, canonical)
            }
            ConfigAction::Get => Ok(ConfigOutcome::Value {
                key: canonical,
                value: ConfigScalarValue::IngestStrategy(
                    cfg.cache.ingest_strategy.unwrap_or(DEFAULT_INGEST_STRATEGY),
                ),
                source,
            }),
            ConfigAction::Clear => unreachable!("scalar clear rejected"),
        },
        ConfigKey::SyncTrustState | ConfigKey::SyncAutoFetch | ConfigKey::SyncAutoRepair => {
            match request.action {
                ConfigAction::Set(values) => {
                    let value = parse_sync_bool_setting(key.as_str(), &one(values))?;
                    match key {
                        ConfigKey::SyncTrustState => cfg.sync.trust_state = Some(value),
                        ConfigKey::SyncAutoFetch => cfg.sync.auto_fetch = Some(value),
                        ConfigKey::SyncAutoRepair => cfg.sync.auto_repair = Some(value),
                        _ => unreachable!(),
                    }
                    repo.save_config_scoped(&cfg, request.scope)?;
                    Ok(ConfigOutcome::Set {
                        key: canonical,
                        value: ConfigScalarValue::Boolean(value),
                    })
                }
                ConfigAction::Unset => {
                    match key {
                        ConfigKey::SyncTrustState => cfg.sync.trust_state = None,
                        ConfigKey::SyncAutoFetch => cfg.sync.auto_fetch = None,
                        ConfigKey::SyncAutoRepair => cfg.sync.auto_repair = None,
                        _ => unreachable!(),
                    }
                    save_unset(repo, &cfg, request.scope, canonical)
                }
                ConfigAction::Get => {
                    let value = match key {
                        ConfigKey::SyncTrustState => cfg.sync.trust_state.unwrap_or(false),
                        ConfigKey::SyncAutoFetch => cfg.sync.auto_fetch(),
                        ConfigKey::SyncAutoRepair => cfg.sync.auto_repair(),
                        _ => unreachable!(),
                    };
                    Ok(ConfigOutcome::Value {
                        key: canonical,
                        value: ConfigScalarValue::Boolean(value),
                        source,
                    })
                }
                ConfigAction::Clear => unreachable!("scalar clear rejected"),
            }
        }
        ConfigKey::LockShardLevels => match request.action {
            ConfigAction::Set(values) => {
                let levels = parse_shard_levels(&one(values))?;
                cfg.lock.shard_levels = Some(levels);
                repo.save_config_scoped(&cfg, request.scope)?;
                Ok(ConfigOutcome::Set {
                    key: canonical,
                    value: ConfigScalarValue::ShardLevels(levels),
                })
            }
            ConfigAction::Unset => {
                cfg.lock.shard_levels = None;
                save_unset(repo, &cfg, request.scope, canonical)
            }
            ConfigAction::Get => Ok(ConfigOutcome::Value {
                key: canonical,
                value: ConfigScalarValue::ShardLevels(cfg.lock.shard_levels()),
                source,
            }),
            ConfigAction::Clear => unreachable!("scalar clear rejected"),
        },
        ConfigKey::GitIgnorePatterns | ConfigKey::GitExcludePatterns => match request.action {
            ConfigAction::Set(values) => {
                cfg.git.ignore_patterns = Some(validate_ignore_patterns(values.clone())?);
                repo.save_config_scoped(&cfg, request.scope)?;
                Ok(ConfigOutcome::SetList {
                    key: canonical,
                    values,
                })
            }
            ConfigAction::Clear => {
                cfg.git.ignore_patterns = Some(Vec::new());
                repo.save_config_scoped(&cfg, request.scope)?;
                Ok(ConfigOutcome::Cleared { key: canonical })
            }
            ConfigAction::Unset => {
                cfg.git.ignore_patterns = None;
                save_unset(repo, &cfg, request.scope, canonical)
            }
            ConfigAction::Get => Ok(ConfigOutcome::Values {
                key: canonical,
                values: cfg
                    .git
                    .effective_ignore_patterns()
                    .iter()
                    .map(|pattern| pattern.as_str().to_string())
                    .collect(),
                source,
            }),
        },
    }
}

const fn key_is_defined(config: &gat_core::config::Config, key: ConfigKey) -> bool {
    match key {
        ConfigKey::CacheLocation => config.cache.location.is_some(),
        ConfigKey::CacheMaterializationStrategy => config.cache.materialization_strategy.is_some(),
        ConfigKey::CacheIngestStrategy => config.cache.ingest_strategy.is_some(),
        ConfigKey::SyncTrustState => config.sync.trust_state.is_some(),
        ConfigKey::SyncAutoFetch => config.sync.auto_fetch.is_some(),
        ConfigKey::SyncAutoRepair => config.sync.auto_repair.is_some(),
        ConfigKey::LockShardLevels => config.lock.shard_levels.is_some(),
        ConfigKey::GitIgnorePatterns | ConfigKey::GitExcludePatterns => {
            config.git.ignore_patterns.is_some()
        }
    }
}

fn validate_action(key: ConfigKey, action: &ConfigAction) -> Result<()> {
    match (action, key.cardinality()) {
        (ConfigAction::Set(values), ValueCardinality::Scalar) if values.len() != 1 => {
            Err(ConfigError::WrongValueCount {
                key,
                got: values.len(),
            })
        }
        (ConfigAction::Clear, ValueCardinality::Scalar) => {
            Err(ConfigError::ClearNotSupportedForScalar { key })
        }
        (ConfigAction::Set(values), ValueCardinality::List) if values.is_empty() => {
            Err(ConfigError::EmptyListNotAllowed { key })
        }
        _ => Ok(()),
    }
}

fn one(values: Vec<String>) -> String {
    values.into_iter().next().expect("scalar arity validated")
}

fn save_unset(
    repo: &Repository,
    cfg: &gat_core::config::Config,
    scope: ConfigScope,
    key: ConfigKey,
) -> Result<ConfigOutcome> {
    repo.save_config_scoped(cfg, scope)?;
    Ok(ConfigOutcome::Unset { key })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::tempdir().unwrap();
        let repo = Repository::at(temp.path().to_path_buf());
        (temp, repo)
    }

    fn request(key: &str, action: ConfigAction) -> ConfigRequest {
        ConfigRequest::from_raw(key.to_string(), action, ConfigScope::Project).unwrap()
    }

    #[test]
    fn reads_effective_config_once() {
        let (_temp, repo) = repository();
        let before_loads = gat_engine::test_support::config_loads();

        let outcome = config(&repo, request("cache.location", ConfigAction::Get)).unwrap();

        assert!(matches!(
            outcome,
            ConfigOutcome::Value {
                value: ConfigScalarValue::Path(_),
                ..
            }
        ));
        assert_eq!(gat_engine::test_support::config_loads() - before_loads, 1);
    }

    #[test]
    fn ingest_strategy_lifecycle_read_reuses_the_effective_config() {
        let (_temp, repo) = repository();
        let before = gat_engine::test_support::config_loads();

        config_with_lifecycle_observer(
            &repo,
            request("cache.ingest_strategy", ConfigAction::Get),
            &|_| {},
        )
        .unwrap();

        assert_eq!(gat_engine::test_support::config_loads() - before, 1);
    }

    #[test]
    fn scoped_mutation_loads_the_selected_layer_once() {
        let (_temp, repo) = repository();
        let before = gat_engine::test_support::scoped_config_loads();

        config(
            &repo,
            request(
                "sync.auto_fetch",
                ConfigAction::Set(vec!["true".to_string()]),
            ),
        )
        .unwrap();

        assert_eq!(gat_engine::test_support::scoped_config_loads() - before, 1);
    }

    #[test]
    fn deprecated_ignore_alias_persists_and_reports_the_canonical_key() {
        let (_temp, repo) = repository();
        let aliases = std::cell::Cell::new(0);
        let outcome = config_with_lifecycle_observer(
            &repo,
            request(
                "git.exclude_patterns",
                ConfigAction::Set(vec!["*.bin".to_string()]),
            ),
            &|surface| {
                if matches!(surface, Surface::ConfigAlias { .. }) {
                    aliases.set(aliases.get() + 1);
                }
            },
        )
        .unwrap();

        assert_eq!(aliases.get(), 1);
        assert!(matches!(
            outcome,
            ConfigOutcome::SetList {
                key: ConfigKey::GitIgnorePatterns,
                ..
            }
        ));
        assert_eq!(
            repo.load_config().unwrap().git.effective_ignore_patterns()[0].as_str(),
            "*.bin"
        );
    }
}
