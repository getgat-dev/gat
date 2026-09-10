//! The only production acquisition of Gat-owned process environment inputs.

use gat_core::settings::{SettingAssignment, SettingKey, SettingsLayer};
use std::borrow::Cow;
use std::collections::{BTreeMap, btree_map::Entry};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A name validated against the template/setting ASCII grammar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentName(String);
impl EnvironmentName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputValueReason {
    NonUnicode,
    InvalidJsonList,
    InvalidValue,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum InvocationInputError {
    #[error("invalid environment setting {key}")]
    Setting {
        key: SettingKey,
        reason: InputValueReason,
    },
    #[error("duplicate environment name")]
    DuplicateName { name: EnvironmentName },
}

#[derive(Clone)]
pub struct TemplateResolver(Arc<BTreeMap<String, OsString>>);
impl std::fmt::Debug for TemplateResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TemplateResolver").finish_non_exhaustive()
    }
}
impl TemplateResolver {
    pub(crate) fn expand(&self, template: &str) -> Result<String, crate::InterpolateError> {
        crate::remote::interpolate_with(template, |name| {
            let value = self
                .0
                .get(normalize(name).as_ref())
                .ok_or_else(|| crate::InterpolateError::MissingVariable { name: name.into() })?;
            value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| crate::InterpolateError::NonUnicodeVariable { name: name.into() })
        })
    }
}

/// Immutable invocation input snapshot. No raw environment map is exposed.
#[derive(Clone)]
pub struct InvocationInputs {
    settings: SettingsLayer,
    home: Option<PathBuf>,
    templates: TemplateResolver,
}
impl std::fmt::Debug for InvocationInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvocationInputs").finish_non_exhaustive()
    }
}
impl InvocationInputs {
    pub fn capture_process() -> Result<Self, InvocationInputError> {
        Self::from_pairs(std::env::vars_os())
    }

    pub fn from_pairs<K: Into<OsString>, V: Into<OsString>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Self, InvocationInputError> {
        let mut variables = BTreeMap::new();
        for (key, value) in pairs {
            let key = key.into();
            let Some(key) = key.to_str().filter(|s| valid_name(s)) else {
                continue;
            };
            match variables.entry(normalize(key).into_owned()) {
                Entry::Vacant(entry) => {
                    entry.insert(value.into());
                }
                Entry::Occupied(entry) => {
                    return Err(InvocationInputError::DuplicateName {
                        name: EnvironmentName(entry.key().clone()),
                    });
                }
            }
        }
        let mut settings = SettingsLayer::default();
        for key in SettingKey::CANONICAL {
            if !key.supports_environment() {
                continue;
            }
            if let Some(value) = variables.get(&key.environment_name()) {
                settings.set(decode(key, value)?);
            }
        }
        let home_name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = variables
            .get(home_name)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        Ok(Self {
            settings,
            home,
            templates: TemplateResolver(Arc::new(variables)),
        })
    }
    #[must_use]
    pub const fn settings(&self) -> &SettingsLayer {
        &self.settings
    }
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }
    #[must_use]
    pub fn templates(&self) -> TemplateResolver {
        self.templates.clone()
    }
}
fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn normalize(name: &str) -> Cow<'_, str> {
    if cfg!(windows) && name.bytes().any(|byte| byte.is_ascii_lowercase()) {
        Cow::Owned(name.to_ascii_uppercase())
    } else {
        Cow::Borrowed(name)
    }
}
fn decode(key: SettingKey, value: &OsStr) -> Result<SettingAssignment, InvocationInputError> {
    let invalid = |reason| InvocationInputError::Setting { key, reason };
    if key == SettingKey::CacheLocation {
        return gat_core::cache_location::CacheLocation::try_from_path(PathBuf::from(value))
            .map(SettingAssignment::CacheLocation)
            .map_err(|_| invalid(InputValueReason::InvalidValue));
    }
    let value = value
        .to_str()
        .ok_or_else(|| invalid(InputValueReason::NonUnicode))?;
    let values = match key.cardinality() {
        gat_core::config_keys::ValueCardinality::Scalar => vec![value.to_owned()],
        gat_core::config_keys::ValueCardinality::List => serde_json::from_str::<Vec<String>>(value)
            .map_err(|_| invalid(InputValueReason::InvalidJsonList))?,
    };
    key.parse_values(&values)
        .map_err(|_| invalid(InputValueReason::InvalidValue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::config::Config;

    fn configured(pairs: &[(&str, &str)]) -> Config {
        let inputs = InvocationInputs::from_pairs(pairs.iter().copied()).unwrap();
        let mut config = Config::default();
        inputs.settings().apply_to(&mut config);
        config
    }

    #[test]
    fn catalog_names_are_unique_and_every_setting_decodes_through_its_domain() {
        let mut names = std::collections::BTreeSet::new();
        for key in SettingKey::CANONICAL {
            let name = key.environment_name();
            assert!(names.insert(normalize(&name).into_owned()));
            let values = match key {
                SettingKey::CacheLocation => vec!["relative/cache".to_string()],
                SettingKey::CacheMaterializationStrategy => vec!["hardlink".into(), "copy".into()],
                SettingKey::CacheIngestStrategy => vec!["safe".into()],
                SettingKey::SyncTrustState
                | SettingKey::SyncAutoFetch
                | SettingKey::SyncAutoRepair => vec!["true".into()],
                SettingKey::LockShardLevels => vec!["2".into()],
                SettingKey::GitIgnorePatterns => vec!["a,b/**".into(), "space name/**".into()],
                _ => vec!["7".into()],
            };
            let text = match key.cardinality() {
                gat_core::config_keys::ValueCardinality::Scalar => values[0].clone(),
                gat_core::config_keys::ValueCardinality::List => {
                    serde_json::to_string(&values).unwrap()
                }
            };
            let from_environment = configured(&[(&name, &text)]);
            assert_eq!(
                key.read(&from_environment),
                Some(key.parse_values(&values).unwrap())
            );
            let serialized = yaml_serde::to_string(&from_environment).unwrap();
            let file: Config = yaml_serde::from_str(&serialized).unwrap();
            assert_eq!(key.read(&file), key.read(&from_environment));
        }
    }

    #[test]
    fn unknown_names_resources_defaults_and_removed_names_never_set_configuration() {
        let config = configured(&[
            ("GAT_REMOTES_DEFAULT", "origin"),
            ("GAT_SELECTIONS_DEFAULT", "all"),
            ("GAT_REMOTES_ORIGIN_URL", "file:///tmp/objects"),
            ("GAT_MOUNTS_MODEL_TARGET", "model"),
            ("GAT_ROUTES_DATA_REMOTE", "origin"),
            ("GAT_CACHE_DIR", "ignored"),
            ("GAT_GC_REPOSITORY_CONCURRENCY", "invalid"),
            ("GAT_CONNECT_TIMEOUT", "invalid"),
            ("GAT_GIT_EXCLUDE_PATTERNS", "invalid"),
            ("GAT_UNKNOWN", "anything"),
        ]);
        assert_eq!(config, Config::default());
    }

    #[test]
    fn lists_are_json_arrays_with_domain_validation_and_no_extra_splitting() {
        let config = configured(&[(
            "GAT_GIT_IGNORE_PATTERNS",
            r#"["a,b/**", "space name/**", ""]"#,
        )]);
        assert_eq!(
            config
                .git
                .ignore_patterns
                .unwrap()
                .iter()
                .map(gat_core::git_ignore::GitIgnorePattern::as_str)
                .collect::<Vec<_>>(),
            ["a,b/**", "space name/**", ""]
        );
        assert!(
            configured(&[("GAT_GIT_IGNORE_PATTERNS", "[]")])
                .git
                .ignore_patterns
                .unwrap()
                .is_empty()
        );
        for value in [
            "",
            "null",
            "{}",
            "[1]",
            "[\"x\",]",
            "a,b",
            "[\"!keep\"]",
            "[\"line\\nbreak\"]",
        ] {
            assert!(
                InvocationInputs::from_pairs([("GAT_GIT_IGNORE_PATTERNS", value)]).is_err(),
                "{value}"
            );
        }
        for value in ["[]", "[\"copy\",\"copy\"]", "[\"unknown\"]"] {
            assert!(
                InvocationInputs::from_pairs([("GAT_CACHE_MATERIALIZATION_STRATEGY", value)])
                    .is_err()
            );
        }
    }

    #[test]
    fn invalid_inputs_are_eager_and_debug_never_contains_values() {
        for (name, value) in [
            ("GAT_SYNC_AUTO_FETCH", "TRUE"),
            ("GAT_LOCK_SHARD_LEVELS", "3"),
            ("GAT_CACHE_LOCATION", ""),
            ("GAT_NETWORK_REQUEST_CONCURRENCY", "0"),
            ("GAT_NETWORK_IO_TIMEOUT_SECONDS", "86401"),
            ("GAT_NETWORK_REQUEST_CONCURRENCY", "65536"),
        ] {
            assert!(InvocationInputs::from_pairs([(name, value)]).is_err());
        }
        let secret = "SYNTHETIC-SECRET";
        let inputs = InvocationInputs::from_pairs([("TOKEN", secret)]).unwrap();
        assert!(!format!("{inputs:?}").contains(secret));
        assert_eq!(
            inputs.templates.expand("file:///${TOKEN}").unwrap(),
            format!("file:///{secret}")
        );
        let error = InvocationInputs::from_pairs([("GAT_SYNC_AUTO_FETCH", secret)]).unwrap_err();
        assert!(!format!("{error:?} {error}").contains(secret));
        let duplicate =
            InvocationInputs::from_pairs([("TOKEN", secret), ("TOKEN", secret)]).unwrap_err();
        assert!(!format!("{duplicate:?}").contains(secret));
    }

    #[test]
    fn home_and_template_inputs_are_optional_and_case_follows_the_platform() {
        let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        assert!(
            InvocationInputs::from_pairs([(home, "")])
                .unwrap()
                .home()
                .is_none()
        );
        let inputs =
            InvocationInputs::from_pairs([("gat_sync_auto_fetch", "true"), ("token", "value")])
                .unwrap();
        assert_eq!(
            inputs.settings.contains(SettingKey::SyncAutoFetch),
            cfg!(windows)
        );
        assert_eq!(inputs.templates.expand("${TOKEN}").is_ok(), cfg!(windows));
        assert_eq!(
            InvocationInputs::from_pairs([("TOKEN", "a"), ("token", "b")]).is_err(),
            cfg!(windows)
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_paths_survive_and_non_unicode_scalars_and_templates_fail_safely() {
        use std::os::unix::ffi::OsStringExt;
        let native = OsString::from_vec(vec![b'x', 0xff]);
        let inputs = InvocationInputs::from_pairs([
            (OsString::from("GAT_CACHE_LOCATION"), native.clone()),
            (OsString::from("HOME"), native.clone()),
            (OsString::from("TOKEN"), native.clone()),
        ])
        .unwrap();
        assert_eq!(inputs.home().unwrap().as_os_str(), native);
        assert!(matches!(
            inputs.templates.expand("${TOKEN}"),
            Err(crate::InterpolateError::NonUnicodeVariable { .. })
        ));
        assert!(
            InvocationInputs::from_pairs([(OsString::from("GAT_SYNC_AUTO_FETCH"), native)])
                .is_err()
        );
    }
}
