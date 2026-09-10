//! `gat config` orchestration over semantic configuration values.

use gat_core::cache_location::CacheLocation;
use gat_core::config::{ConfigScope, IngestStrategy};
use gat_core::config_keys::{ConfigResource, SettingKey, ValueCardinality};
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
    key: SettingKey,
    action: ConfigAction,
    scope: ConfigScope,
    alias: bool,
}

impl ConfigRequest {
    #[must_use]
    pub const fn new(key: SettingKey, action: ConfigAction, scope: ConfigScope) -> Self {
        Self {
            key,
            action,
            scope,
            alias: false,
        }
    }
    pub fn from_raw(key: String, action: ConfigAction, scope: ConfigScope) -> Result<Self> {
        let alias = key == "git.exclude_patterns";
        let Some(key_value) = SettingKey::parse(if alias { "git.ignore_patterns" } else { &key })
        else {
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
            alias,
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
    Unsigned(u32),
}

pub use gat_core::settings::SettingSource as ConfigSource;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigOutcome {
    Masked {
        change: Box<Self>,
        source: ConfigSource,
    },
    Value {
        key: SettingKey,
        value: ConfigScalarValue,
        source: ConfigSource,
    },
    Values {
        key: SettingKey,
        values: Vec<String>,
        source: ConfigSource,
    },
    Set {
        key: SettingKey,
        value: ConfigScalarValue,
    },
    SetList {
        key: SettingKey,
        values: Vec<String>,
    },
    Cleared {
        key: SettingKey,
    },
    Unset {
        key: SettingKey,
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
    WrongValueCount { key: SettingKey, got: usize },

    #[error("`--clear` isn't supported for scalar key `{key}`")]
    ClearNotSupportedForScalar { key: SettingKey },

    #[error("`{key}` takes one or more values")]
    EmptyListNotAllowed { key: SettingKey },

    #[error("`{key}` cannot be cleared to an empty list")]
    ClearAlwaysInvalid {
        key: SettingKey,
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

pub fn config_with_lifecycle_observer(
    repo: &Repository,
    request: ConfigRequest,
    observe: &dyn Fn(Surface<'_>),
) -> Result<ConfigOutcome> {
    use gat_core::settings::SettingChange;
    validate_action(request.key, &request.action)?;
    let key = request.key;
    if request.alias {
        observe(Surface::ConfigAlias {
            canonical: key.as_str(),
            alias: "git.exclude_patterns",
        });
    }
    let (value, source) = match &request.action {
        ConfigAction::Get => repo.read_setting(key)?,
        ConfigAction::Set(values) => {
            let value = key.parse_values(values)?;
            let source = repo.change_setting(request.scope, SettingChange::Set(value.clone()))?;
            (value, source)
        }
        ConfigAction::Clear => {
            let value = key
                .parse_values(&[])
                .map_err(|source| ConfigError::ClearAlwaysInvalid { key, source })?;
            let source = repo.change_setting(request.scope, SettingChange::Set(value))?;
            return Ok(masked(
                ConfigOutcome::Cleared { key },
                source,
                request.scope,
            ));
        }
        ConfigAction::Unset => {
            let source = repo.change_setting(request.scope, SettingChange::Unset(key))?;
            return Ok(masked(ConfigOutcome::Unset { key }, source, request.scope));
        }
    };
    if let gat_core::settings::SettingAssignment::CacheIngestStrategy(strategy) = &value {
        observe(Surface::ConfigValue {
            key: key.as_str(),
            value: strategy.as_str(),
        });
    }
    let get = request.action == ConfigAction::Get;
    let outcome = match present_assignment(value, get) {
        SettingPresentation::Scalar(value) if get => ConfigOutcome::Value { key, value, source },
        SettingPresentation::Scalar(value) => ConfigOutcome::Set { key, value },
        SettingPresentation::List(values) if get => ConfigOutcome::Values {
            key,
            values,
            source,
        },
        SettingPresentation::List(values) => ConfigOutcome::SetList { key, values },
    };
    Ok(if get {
        outcome
    } else {
        masked(outcome, source, request.scope)
    })
}

enum SettingPresentation {
    Scalar(ConfigScalarValue),
    List(Vec<String>),
}
fn present_assignment(
    value: gat_core::settings::SettingAssignment,
    resolved: bool,
) -> SettingPresentation {
    use SettingPresentation::{List, Scalar};
    use gat_core::settings::SettingAssignment as A;
    match value {
        A::CacheLocation(value) if resolved => {
            Scalar(ConfigScalarValue::Path(value.as_path().to_path_buf()))
        }
        A::CacheLocation(value) => Scalar(ConfigScalarValue::CacheLocation(value)),
        A::CacheMaterializationStrategy(value) => {
            List(value.modes().iter().map(ToString::to_string).collect())
        }
        A::CacheIngestStrategy(value) => Scalar(ConfigScalarValue::IngestStrategy(value)),
        A::SyncTrustState(value) | A::SyncAutoFetch(value) | A::SyncAutoRepair(value) => {
            Scalar(ConfigScalarValue::Boolean(value))
        }
        A::LockShardLevels(value) => Scalar(ConfigScalarValue::ShardLevels(value)),
        A::GitIgnorePatterns(value) => List(value.iter().map(|v| v.as_str().to_owned()).collect()),
        A::NetworkReadinessTimeout(value)
        | A::NetworkOperationTimeout(value)
        | A::NetworkIoTimeout(value) => Scalar(ConfigScalarValue::Unsigned(value.get())),
        A::NetworkRequestConcurrency(value) => Scalar(ConfigScalarValue::Unsigned(value.get())),
    }
}

const fn validate_action(key: SettingKey, action: &ConfigAction) -> Result<()> {
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

fn masked(change: ConfigOutcome, source: ConfigSource, scope: ConfigScope) -> ConfigOutcome {
    let higher = matches!(
        (scope, source),
        (_, ConfigSource::Environment(_))
            | (
                ConfigScope::Global,
                ConfigSource::Scope(ConfigScope::Project | ConfigScope::Local)
            )
            | (
                ConfigScope::Project,
                ConfigSource::Scope(ConfigScope::Local)
            )
    );
    if higher {
        ConfigOutcome::Masked {
            change: Box::new(change),
            source,
        }
    } else {
        change
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, Repository) {
        let temp = tempfile::tempdir().unwrap();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(temp.path().to_path_buf());
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
    fn scoped_mutation_captures_each_layer_once() {
        let (_temp, repo) = repository();
        let before = gat_engine::test_support::config_loads();

        config(
            &repo,
            request(
                "sync.auto_fetch",
                ConfigAction::Set(vec!["true".to_string()]),
            ),
        )
        .unwrap();

        assert_eq!(gat_engine::test_support::config_loads() - before, 1);
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
                key: SettingKey::GitIgnorePatterns,
                ..
            }
        ));
        assert_eq!(
            repo.load_config().unwrap().git.effective_ignore_patterns()[0].as_str(),
            "*.bin"
        );
    }
}
