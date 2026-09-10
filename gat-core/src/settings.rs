//! Closed, typed general settings. Resource definitions and default choices are
//! deliberately absent from assignments and environment identities.

use crate::cache_location::CacheLocation;
use crate::config::{Config, ConfigError, IngestStrategy, MaterializationStrategy};
use crate::git_ignore::GitIgnorePattern;
use crate::lock::LockShardLevels;
use serde::{Deserialize, Serialize};

/// Safe value-validation reason. Never contains the rejected input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingValueError {
    #[error("expected exactly one value")]
    ScalarRequired,
    #[error("invalid setting value")]
    InvalidValue,
    #[error("value is outside the supported range")]
    OutOfRange,
    #[error("expected true or false")]
    InvalidBoolean,
    #[error("path must not be empty")]
    EmptyPath,
}

macro_rules! bounded {
    ($name:ident, $max:expr) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "u32", into = "u32")]
        pub struct $name(std::num::NonZeroUsize);

        impl $name {
            pub const MAX: u32 = $max;
            pub const fn new(value: u32) -> Result<Self, SettingValueError> {
                if value == 0 || value > Self::MAX {
                    Err(SettingValueError::OutOfRange)
                } else {
                    match std::num::NonZeroUsize::new(value as usize) {
                        Some(value) => Ok(Self(value)),
                        None => Err(SettingValueError::OutOfRange),
                    }
                }
            }
            const fn checked_default(value: u32) -> Self {
                match Self::new(value) {
                    Ok(value) => value,
                    Err(_) => panic!("invalid built-in setting default"),
                }
            }
            #[must_use]
            #[allow(
                clippy::cast_possible_truncation,
                reason = "Construction accepts only u32 values bounded by MAX"
            )]
            pub const fn get(self) -> u32 {
                self.0.get() as u32
            }
        }
        impl TryFrom<u32> for $name {
            type Error = SettingValueError;
            fn try_from(value: u32) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
        impl From<$name> for u32 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }
        impl SettingDomain for $name {
            fn parse(key: SettingKey, values: &[String]) -> Result<Self, ConfigError> {
                let text = scalar(key, values)?;
                let value = if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                    text.parse::<u32>().ok()
                } else {
                    None
                };
                value
                    .and_then(|n| Self::new(n).ok())
                    .ok_or_else(|| invalid(key, SettingValueError::OutOfRange))
            }
        }
    };
}

bounded!(TimeoutSeconds, 86_400);
bounded!(ConcurrencyLimit, 65_535);

impl TimeoutSeconds {
    #[must_use]
    pub const fn duration(self) -> std::time::Duration {
        std::time::Duration::from_secs(self.get() as u64)
    }
}

impl ConcurrencyLimit {
    #[must_use]
    pub const fn capacity(self) -> std::num::NonZeroUsize {
        self.0
    }
}

/// General network settings, inherited independently of named remotes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_timeout_seconds: Option<TimeoutSeconds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_timeout_seconds: Option<TimeoutSeconds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_timeout_seconds: Option<TimeoutSeconds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_concurrency: Option<ConcurrencyLimit>,
}

/// Narrow service options; all defaults come from the setting declarations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkOptions {
    pub readiness_timeout: TimeoutSeconds,
    pub operation_timeout: TimeoutSeconds,
    pub io_timeout: TimeoutSeconds,
    pub request_concurrency: ConcurrencyLimit,
}

macro_rules! settings {
    ($( $key:ident, $field:ident: $ty:ty => $section:ident.$member:ident, $path:literal, $resolution:ident($default:expr), $environment:literal; )*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum SettingKey { $( $key, )* }

        impl SettingKey {
            pub const CANONICAL: [Self; settings!(@count $( $key )*)] = [$(Self::$key,)*];
            #[must_use]
            pub fn parse(value: &str) -> Option<Self> {
                match value { $( $path => Some(Self::$key), )* _ => None }
            }
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $( Self::$key => $path, )* }
            }
            #[must_use]
            pub const fn supports_environment(self) -> bool {
                match self { $( Self::$key => $environment, )* }
            }
            #[must_use]
            pub fn environment_name(self) -> String {
                format!("GAT_{}", self.as_str().replace('.', "_").to_ascii_uppercase())
            }
            #[must_use]
            pub const fn cardinality(self) -> crate::config_keys::ValueCardinality {
                match self { $( Self::$key => <$ty as SettingDomain>::CARDINALITY, )* }
            }
            pub fn parse_values(self, values: &[String]) -> Result<SettingAssignment, ConfigError> {
                match self { $( Self::$key => <$ty as SettingDomain>::parse(self, values).map(SettingAssignment::$key), )* }
            }
            #[must_use]
            pub const fn is_set(self, config: &Config) -> bool {
                match self { $( Self::$key => config.$section.$member.is_some(), )* }
            }
            #[must_use]
            pub fn read(self, config: &Config) -> Option<SettingAssignment> {
                match self { $( Self::$key => config.$section.$member.clone().map(SettingAssignment::$key), )* }
            }
            #[must_use]
            pub fn default_value(self) -> Option<SettingAssignment> {
                match self { $( Self::$key => settings!(@optional $resolution, defaults::$field()).map(SettingAssignment::$key), )* }
            }
            pub fn unset(self, config: &mut Config) {
                match self { $( Self::$key => config.$section.$member = None, )* }
            }
        }

        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum SettingAssignment { $( $key($ty), )* }
        impl SettingAssignment {
            #[must_use]
            pub const fn key(&self) -> SettingKey {
                match self { $( Self::$key(_) => SettingKey::$key, )* }
            }
            pub fn apply(self, config: &mut Config) {
                match self { $( Self::$key(value) => config.$section.$member = Some(value), )* }
            }
        }

        #[allow(clippy::missing_const_for_fn, reason = "Uniform catalog accessors include allocating list defaults")]
        mod defaults {
            use super::*;
            $( pub(super) fn $field() -> settings!(@type $resolution, $ty) { $default } )*
        }

        /// Resolved general settings; only the repository-derived cache location
        /// may be absent. All scalar/list defaults are represented by values.
        #[derive(Clone)]
        pub struct EffectiveSettings { $( $field: settings!(@type $resolution, $ty), )* }
        impl EffectiveSettings {
            $( #[must_use] pub const fn $field(&self) -> &settings!(@type $resolution, $ty) { &self.$field } )*
            #[must_use]
            pub fn from_config(config: &Config) -> Self {
                Self { $( $field: settings!(@resolve $resolution, config.$section.$member.clone(), defaults::$field), )* }
            }
        }
        impl std::fmt::Debug for EffectiveSettings {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.debug_struct("EffectiveSettings").finish_non_exhaustive() }
        }

        /// Partial general settings only; cannot contain a named resource.
        #[derive(Clone, Default, PartialEq, Eq)]
        pub struct SettingsLayer { $( $field: Option<$ty>, )* }
        impl SettingsLayer {
            // Consuming configuration merges transfer owned lists instead of cloning
            // them through a temporary SettingsLayer.
            pub(crate) fn merge_config(target: &mut Config, source: &mut Config) {
                $( if let Some(value) = source.$section.$member.take() { target.$section.$member = Some(value); } )*
            }

            #[must_use]
            pub fn from_config(config: &Config) -> Self {
                Self { $( $field: config.$section.$member.clone(), )* }
            }
            #[must_use]
            pub fn get(&self, key: SettingKey) -> Option<SettingAssignment> {
                match key { $( SettingKey::$key => self.$field.clone().map(SettingAssignment::$key), )* }
            }
            pub fn set(&mut self, assignment: SettingAssignment) {
                match assignment { $( SettingAssignment::$key(value) => self.$field = Some(value), )* }
            }
            #[must_use]
            pub const fn contains(&self, key: SettingKey) -> bool {
                match key { $( SettingKey::$key => self.$field.is_some(), )* }
            }
            pub fn apply_to(&self, config: &mut Config) {
                $( if let Some(value) = &self.$field { config.$section.$member = Some(value.clone()); } )*
            }
        }
    };
    (@type required, $ty:ty) => { $ty };
    (@type derived, $ty:ty) => { Option<$ty> };
    (@optional required, $value:expr) => { Some($value) };
    (@optional derived, $value:expr) => { $value };
    (@resolve required, $value:expr, $default:path) => { $value.unwrap_or_else($default) };
    (@resolve derived, $value:expr, $default:path) => { $value.or_else($default) };
    (@count $( $key:ident )*) => { <[()]>::len(&[$(settings!(@one $key)),*]) };
    (@one $key:ident) => { () };
}

settings! {
    CacheLocation, cache_location: CacheLocation => cache.location, "cache.location", derived(None), true;
    CacheMaterializationStrategy, materialization: MaterializationStrategy => cache.materialization_strategy, "cache.materialization_strategy", required(MaterializationStrategy::default()), true;
    CacheIngestStrategy, ingest: IngestStrategy => cache.ingest_strategy, "cache.ingest_strategy", required(crate::config::DEFAULT_INGEST_STRATEGY), true;
    SyncTrustState, trust_state: bool => sync.trust_state, "sync.trust_state", required(false), true;
    SyncAutoFetch, auto_fetch: bool => sync.auto_fetch, "sync.auto_fetch", required(false), true;
    SyncAutoRepair, auto_repair: bool => sync.auto_repair, "sync.auto_repair", required(false), true;
    LockShardLevels, shard_levels: LockShardLevels => lock.shard_levels, "lock.shard_levels", required(LockShardLevels::FLAT), true;
    GitIgnorePatterns, ignore_patterns: Vec<GitIgnorePattern> => git.ignore_patterns, "git.ignore_patterns", required(Vec::new()), true;
    NetworkReadinessTimeout, readiness: TimeoutSeconds => network.readiness_timeout_seconds, "network.readiness_timeout_seconds", required(const { TimeoutSeconds::checked_default(5) }), true;
    NetworkOperationTimeout, operation: TimeoutSeconds => network.operation_timeout_seconds, "network.operation_timeout_seconds", required(const { TimeoutSeconds::checked_default(30) }), true;
    NetworkIoTimeout, io: TimeoutSeconds => network.io_timeout_seconds, "network.io_timeout_seconds", required(const { TimeoutSeconds::checked_default(60) }), true;
    NetworkRequestConcurrency, requests: ConcurrencyLimit => network.request_concurrency, "network.request_concurrency", required(const { ConcurrencyLimit::checked_default(256) }), true;
}

impl std::fmt::Display for SettingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for SettingsLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsLayer").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingChange {
    Set(SettingAssignment),
    Unset(SettingKey),
}

impl EffectiveSettings {
    #[must_use]
    pub const fn network(&self) -> NetworkOptions {
        NetworkOptions {
            readiness_timeout: self.readiness,
            operation_timeout: self.operation,
            io_timeout: self.io,
            request_concurrency: self.requests,
        }
    }
}

impl NetworkConfig {
    /// Resolve only the network fields, without cloning unrelated configuration.
    #[must_use]
    pub fn resolve(&self) -> NetworkOptions {
        NetworkOptions {
            readiness_timeout: self
                .readiness_timeout_seconds
                .unwrap_or_else(defaults::readiness),
            operation_timeout: self
                .operation_timeout_seconds
                .unwrap_or_else(defaults::operation),
            io_timeout: self.io_timeout_seconds.unwrap_or_else(defaults::io),
            request_concurrency: self.request_concurrency.unwrap_or_else(defaults::requests),
        }
    }
}

impl Default for NetworkOptions {
    fn default() -> Self {
        Self {
            readiness_timeout: defaults::readiness(),
            operation_timeout: defaults::operation(),
            io_timeout: defaults::io(),
            request_concurrency: defaults::requests(),
        }
    }
}

trait SettingDomain: Sized {
    const CARDINALITY: crate::config_keys::ValueCardinality =
        crate::config_keys::ValueCardinality::Scalar;
    fn parse(key: SettingKey, values: &[String]) -> Result<Self, ConfigError>;
}
const fn invalid(key: SettingKey, reason: SettingValueError) -> ConfigError {
    ConfigError::InvalidSettingValue { key, reason }
}
fn scalar(key: SettingKey, values: &[String]) -> Result<&str, ConfigError> {
    match values {
        [value] => Ok(value),
        _ => Err(invalid(key, SettingValueError::ScalarRequired)),
    }
}
impl SettingDomain for bool {
    fn parse(key: SettingKey, values: &[String]) -> Result<Self, ConfigError> {
        match scalar(key, values)? {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(invalid(key, SettingValueError::InvalidBoolean)),
        }
    }
}
impl SettingDomain for CacheLocation {
    fn parse(key: SettingKey, values: &[String]) -> Result<Self, ConfigError> {
        Self::try_from_path(std::path::PathBuf::from(scalar(key, values)?))
            .map_err(|reason| invalid(key, reason))
    }
}
impl SettingDomain for IngestStrategy {
    fn parse(key: SettingKey, values: &[String]) -> Result<Self, ConfigError> {
        scalar(key, values)?.parse()
    }
}
impl SettingDomain for MaterializationStrategy {
    const CARDINALITY: crate::config_keys::ValueCardinality =
        crate::config_keys::ValueCardinality::List;
    fn parse(_key: SettingKey, values: &[String]) -> Result<Self, ConfigError> {
        Self::from_values(values)
    }
}
impl SettingDomain for LockShardLevels {
    fn parse(key: SettingKey, values: &[String]) -> Result<Self, ConfigError> {
        crate::config::parse_shard_levels(scalar(key, values)?)
    }
}
impl SettingDomain for Vec<GitIgnorePattern> {
    const CARDINALITY: crate::config_keys::ValueCardinality =
        crate::config_keys::ValueCardinality::List;
    fn parse(_key: SettingKey, values: &[String]) -> Result<Self, ConfigError> {
        crate::config::validate_ignore_patterns(values.to_vec())
    }
}

impl NetworkConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Effective-setting provenance is independent of writable file scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingSource {
    Default,
    Scope(crate::config::ConfigScope),
    Environment(SettingKey),
}
