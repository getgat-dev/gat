//! Typed documentation schema for every persisted `gat.yaml` field.
//!
//! Runtime configuration behavior remains authoritative in [`crate::config`].
//! This module records the same semantics in a pure, const-friendly form for
//! command validation, diagnostics, and presentation metadata.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigSection {
    Remotes,
    Cache,
    Sync,
    Selection,
    Lock,
    Mounts,
    Routes,
    GitIntegration,
    Network,
}

impl ConfigSection {
    pub const ALL: [Self; 9] = [
        Self::Remotes,
        Self::Cache,
        Self::Sync,
        Self::Selection,
        Self::Lock,
        Self::Mounts,
        Self::Routes,
        Self::GitIntegration,
        Self::Network,
    ];

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Remotes => "Remotes",
            Self::Cache => "Cache",
            Self::Sync => "Sync",
            Self::Selection => "Selections",
            Self::Lock => "Lock",
            Self::Mounts => "Mounts",
            Self::Routes => "Routes",
            Self::GitIntegration => "Git integration",
            Self::Network => "Network",
        }
    }
}

/// A supported configuration path. Named fields and defaults cannot contain
/// arbitrary namespaces, missing fields, or unsupported field combinations.
///
/// ```compile_fail
/// use gat_core::config_keys::{ConfigPath, ResourceField};
/// let invalid = ConfigPath::ResourceField(ResourceField::RemotePath);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPath {
    Setting(SettingKey),
    ResourceDefault(ResourceDefault),
    ResourceField(ResourceField),
}

impl ConfigPath {
    #[must_use]
    pub fn display(self) -> String {
        self.canonical().to_owned()
    }

    #[must_use]
    pub const fn canonical(self) -> &'static str {
        match self {
            Self::Setting(key) => key.as_str(),
            Self::ResourceDefault(default) => default.canonical(),
            Self::ResourceField(field) => field.canonical(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueCardinality {
    Scalar,
    List,
}

/// A namespace owned by its resource command family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigResource {
    Remote,
    Route,
    Mount,
    Selection,
}

macro_rules! resource_fields {
    ($( $resource:ident, $namespace:literal => { $( $field:ident: $name:literal ),+ $(,)? } ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ResourceField {
            $( $( $field, )+ )+
        }

        impl ResourceField {
            #[must_use]
            pub const fn canonical(self) -> &'static str {
                match self { $( $( Self::$field => concat!($namespace, ".<name>.", $name), )+ )+ }
            }

            #[must_use]
            pub const fn name(self) -> &'static str {
                match self { $( $( Self::$field => $name, )+ )+ }
            }

            #[must_use]
            pub const fn resource(self) -> ConfigResource {
                match self { $( $( Self::$field => ConfigResource::$resource, )+ )+ }
            }
        }

        impl ConfigResource {
            /// Recognize namespaces, including unknown fields, without depending
            /// on the documentation registry.
            #[must_use]
            pub fn from_key(key: &str) -> Option<Self> {
                match key.split('.').next()? {
                    $( $namespace => Some(Self::$resource), )+
                    _ => None,
                }
            }
        }
    };
}

resource_fields! {
    Remote, "remotes" => { RemoteUrl: "url" },
    Route, "routes" => { RoutePath: "path", RouteRemote: "remote" },
    Mount, "mounts" => {
        MountUrl: "url", MountTarget: "target", MountPath: "path",
        MountRevision: "rev", MountRevisionLock: "rev_lock",
        MountInclude: "include", MountExclude: "exclude",
    },
    Selection, "selections" => {
        SelectionPath: "path", SelectionInclude: "include", SelectionExclude: "exclude",
    },
}

/// Only remotes and selections have a named-resource default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceDefault {
    Remote,
    Selection,
}

impl ResourceDefault {
    #[must_use]
    pub const fn canonical(self) -> &'static str {
        match self {
            Self::Remote => "remotes.default",
            Self::Selection => "selections.default",
        }
    }
}

pub use crate::settings::SettingKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyListPolicy {
    Allowed,
    MeansEverything,
    Forbidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigElementSpec {
    String,
    Glob,
    GitIgnorePattern,
    MaterializationMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigValueSpec {
    Boolean,
    String,
    Path,
    GitLocation,
    RemoteUrl,
    Unsigned {
        min: Option<u64>,
        max: Option<u64>,
    },
    Enum {
        values: &'static [&'static str],
    },
    List {
        element: ConfigElementSpec,
        empty: EmptyListPolicy,
        ordered: bool,
        unique: bool,
    },
}

impl ConfigValueSpec {
    #[must_use]
    pub const fn cardinality(self) -> ValueCardinality {
        match self {
            Self::List { .. } => ValueCardinality::List,
            _ => ValueCardinality::Scalar,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDocValue {
    Boolean(bool),
    Unsigned(u64),
    String(&'static str),
    List(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistedDefault {
    Unset,
    Value(ConfigDocValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveDefault {
    Setting(SettingKey),
    SameAsPersisted,
    Value(ConfigDocValue),
    Derived(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigDefault {
    pub persisted: PersistedDefault,
    pub effective: EffectiveDefault,
}

pub const UNSET: ConfigDefault = ConfigDefault {
    persisted: PersistedDefault::Unset,
    effective: EffectiveDefault::SameAsPersisted,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigWriteSurface {
    Config,
    Remote,
    Mount,
    Selection,
    Route,
    Automatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSetter {
    Command { path: &'static str },
    Automatic { description: &'static str },
}

pub struct ConfigFieldSpec {
    pub path: ConfigPath,
    pub section: ConfigSection,
    pub value: ConfigValueSpec,
    pub default: ConfigDefault,
    pub description: &'static str,
    pub write_surface: ConfigWriteSurface,
    pub setter: ConfigSetter,

    pub examples: &'static [ConfigDocValue],
}

const EMPTY_LIST: ConfigDefault = ConfigDefault {
    persisted: PersistedDefault::Unset,
    effective: EffectiveDefault::Value(ConfigDocValue::List(&[])),
};
const ALL_PATHS: ConfigDefault = ConfigDefault {
    persisted: PersistedDefault::Unset,
    effective: EffectiveDefault::Derived("all tracked paths under the selected path"),
};
const CONFIG_SETTER: ConfigSetter = ConfigSetter::Command { path: "config" };
#[cfg(test)]
const MATERIALIZATION_MODES: &[&str] = &["reflink", "hardlink", "symlink", "copy"];

pub const CONFIG_KEYS: &[ConfigFieldSpec] = &[
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::NetworkReadinessTimeout),
        section: ConfigSection::Network,
        value: ConfigValueSpec::Unsigned {
            min: Some(1),
            max: Some(crate::settings::TimeoutSeconds::MAX as u64),
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::NetworkReadinessTimeout),
        },
        description: "Total budget in seconds for establishing remote readiness.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::Unsigned(5)],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::NetworkOperationTimeout),
        section: ConfigSection::Network,
        value: ConfigValueSpec::Unsigned {
            min: Some(1),
            max: Some(crate::settings::TimeoutSeconds::MAX as u64),
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::NetworkOperationTimeout),
        },
        description: "Timeout in seconds for one backend operation attempt, not an entire transfer.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::Unsigned(60)],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::NetworkIoTimeout),
        section: ConfigSection::Network,
        value: ConfigValueSpec::Unsigned {
            min: Some(1),
            max: Some(crate::settings::TimeoutSeconds::MAX as u64),
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::NetworkIoTimeout),
        },
        description: "Timeout in seconds for one read, write, or body operation.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::Unsigned(60)],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::NetworkRequestConcurrency),
        section: ConfigSection::Network,
        value: ConfigValueSpec::Unsigned {
            min: Some(1),
            max: Some(crate::settings::ConcurrencyLimit::MAX as u64),
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::NetworkRequestConcurrency),
        },
        description: "Maximum simultaneous physical network requests per operation; logical scheduling is derived internally.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::Unsigned(256)],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceDefault(ResourceDefault::Remote),
        section: ConfigSection::Remotes,
        value: ConfigValueSpec::String,
        default: UNSET,
        description: "Name of the remote used when no explicit remote or more-specific route applies.",
        write_surface: ConfigWriteSurface::Remote,
        setter: ConfigSetter::Command {
            path: "remote default",
        },
        examples: &[ConfigDocValue::String("origin")],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::RemoteUrl),
        section: ConfigSection::Remotes,
        value: ConfigValueSpec::RemoteUrl,
        default: UNSET,
        description: "Object-storage URL for one named remote.",
        write_surface: ConfigWriteSurface::Remote,
        setter: ConfigSetter::Command { path: "remote add" },
        examples: &[ConfigDocValue::String("s3://example-bucket/project")],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::CacheLocation),
        section: ConfigSection::Cache,
        value: ConfigValueSpec::Path,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::CacheLocation),
        },
        description: "Absolute cache path or a path relative to the repository root.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::String("/mnt/gat-cache")],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::CacheMaterializationStrategy),
        section: ConfigSection::Cache,
        value: ConfigValueSpec::List {
            element: ConfigElementSpec::MaterializationMode,
            empty: EmptyListPolicy::Forbidden,
            ordered: true,
            unique: true,
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::CacheMaterializationStrategy),
        },
        description: "Ordered fallback modes used to materialize cached objects into the working tree.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::List(&["reflink", "copy"])],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::CacheIngestStrategy),
        section: ConfigSection::Cache,
        value: ConfigValueSpec::Enum {
            values: crate::config::INGEST_STRATEGIES,
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::CacheIngestStrategy),
        },
        description: "Copy-and-hash strategy used when publishing local files into the content-addressed cache.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::String("safe")],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::SyncTrustState),
        section: ConfigSection::Sync,
        value: ConfigValueSpec::Boolean,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::SyncTrustState),
        },
        description: "Trust recorded materialized state without inspecting the working tree.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::Boolean(true)],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::SyncAutoFetch),
        section: ConfigSection::Sync,
        value: ConfigValueSpec::Boolean,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::SyncAutoFetch),
        },
        description: "Fetch missing objects before normal sync reconciliation.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::Boolean(true)],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceDefault(ResourceDefault::Selection),
        section: ConfigSection::Selection,
        value: ConfigValueSpec::String,
        default: UNSET,
        description: "Optional default selection name. Inherits independently of named definitions. Without a default, operations select the whole repository. Adding a selection never chooses a default.",
        write_surface: ConfigWriteSurface::Selection,
        setter: ConfigSetter::Command {
            path: "selection default",
        },
        examples: &[ConfigDocValue::String("runtime")],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::SelectionPath),
        section: ConfigSection::Selection,
        value: ConfigValueSpec::Path,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Value(ConfigDocValue::String(".")),
        },
        description: "Literal file or directory relative to the repository root. Same-name definitions replace as a whole across layers. An empty definition selects everything.",
        write_surface: ConfigWriteSurface::Selection,
        setter: ConfigSetter::Command {
            path: "selection add",
        },
        examples: &[ConfigDocValue::String("models")],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::SelectionInclude),
        section: ConfigSection::Selection,
        value: ConfigValueSpec::List {
            element: ConfigElementSpec::Glob,
            empty: EmptyListPolicy::MeansEverything,
            ordered: true,
            unique: false,
        },
        default: ALL_PATHS,
        description: "Include patterns relative to the selected path for sync, pull, fetch, ls-files, status, diff, and push. Any explicit --path, --include, or --exclude replaces the complete configured selection.",
        write_surface: ConfigWriteSurface::Selection,
        setter: ConfigSetter::Command {
            path: "selection add",
        },
        examples: &[ConfigDocValue::List(&["data/**", "models/**"])],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::SelectionExclude),
        section: ConfigSection::Selection,
        value: ConfigValueSpec::List {
            element: ConfigElementSpec::Glob,
            empty: EmptyListPolicy::Allowed,
            ordered: true,
            unique: false,
        },
        default: EMPTY_LIST,
        description: "Exclude patterns relative to the selected path. Exclusions win. Explicit CLI selection replaces the complete configured selection.",
        write_surface: ConfigWriteSurface::Selection,
        setter: ConfigSetter::Command {
            path: "selection add",
        },
        examples: &[ConfigDocValue::List(&["tmp/**"])],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::SyncAutoRepair),
        section: ConfigSection::Sync,
        value: ConfigValueSpec::Boolean,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::SyncAutoRepair),
        },
        description: "Re-fetch and rematerialize cache objects found corrupted during sync.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::Boolean(true)],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::LockShardLevels),
        section: ConfigSection::Lock,
        value: ConfigValueSpec::Unsigned {
            min: Some(0),
            max: Some(crate::config::MAX_SHARD_LEVELS as u64),
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::LockShardLevels),
        },
        description: "Number of hash fan-out directory levels used by the persisted lock.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::Unsigned(2)],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::MountUrl),
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::GitLocation,
        default: UNSET,
        description: "Git repository from which this mount imports tracked rows.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        examples: &[ConfigDocValue::String("../models")],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::MountTarget),
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::Path,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Derived("repository name from the mount URL"),
        },
        description: "Destination subtree owned by the mount.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        examples: &[ConfigDocValue::String("releases/resnet")],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::MountPath),
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::Path,
        default: ConfigDefault {
            persisted: PersistedDefault::Value(ConfigDocValue::String(".")),
            effective: EffectiveDefault::SameAsPersisted,
        },
        description: "Subtree inside the source repository from which rows are selected.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        examples: &[ConfigDocValue::String("exports")],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::MountRevision),
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::String,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Derived("source repository default branch"),
        },
        description: "Git revision the mount tracks.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        examples: &[ConfigDocValue::String("main")],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::MountRevisionLock),
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::String,
        default: UNSET,
        description: "Exact commit resolved from the requested mount revision.",
        write_surface: ConfigWriteSurface::Automatic,
        setter: ConfigSetter::Automatic {
            description: "resolved automatically by {{command:mount add}} and {{command:mount update}}",
        },
        examples: &[],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::MountInclude),
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::List {
            element: ConfigElementSpec::Glob,
            empty: EmptyListPolicy::MeansEverything,
            ordered: true,
            unique: false,
        },
        default: ALL_PATHS,
        description: "Glob patterns selecting source rows relative to the mount path.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        examples: &[ConfigDocValue::List(&["**/*.onnx"])],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::MountExclude),
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::List {
            element: ConfigElementSpec::Glob,
            empty: EmptyListPolicy::Allowed,
            ordered: true,
            unique: false,
        },
        default: EMPTY_LIST,
        description: "Glob patterns excluded from the mount selection.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        examples: &[ConfigDocValue::List(&["tests/**"])],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::RoutePath),
        section: ConfigSection::Routes,
        value: ConfigValueSpec::Path,
        default: UNSET,
        description: "Root-relative tracked-path prefix served by the route.",
        write_surface: ConfigWriteSurface::Route,
        setter: ConfigSetter::Command { path: "route add" },
        examples: &[ConfigDocValue::String("data/datasets")],
    },
    ConfigFieldSpec {
        path: ConfigPath::ResourceField(ResourceField::RouteRemote),
        section: ConfigSection::Routes,
        value: ConfigValueSpec::String,
        default: UNSET,
        description: "Named remote that serves the route's path.",
        write_surface: ConfigWriteSurface::Route,
        setter: ConfigSetter::Command { path: "route add" },
        examples: &[ConfigDocValue::String("backup")],
    },
    ConfigFieldSpec {
        path: ConfigPath::Setting(SettingKey::GitIgnorePatterns),
        section: ConfigSection::GitIntegration,
        value: ConfigValueSpec::List {
            element: ConfigElementSpec::GitIgnorePattern,
            empty: EmptyListPolicy::Allowed,
            ordered: true,
            unique: false,
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Setting(SettingKey::GitIgnorePatterns),
        },
        description: "Single-line, non-negated Git-ignore patterns used to derive Gat's managed `.git/info/exclude` block.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        examples: &[ConfigDocValue::List(&[
            "*.safetensors",
            "/artifacts/**/*.bin",
        ])],
    },
];

pub fn settable_via_config_keys() -> impl Iterator<Item = &'static str> {
    CONFIG_KEYS
        .iter()
        .filter(|spec| spec.write_surface == ConfigWriteSurface::Config)
        .map(|spec| spec.path.canonical())
}

#[must_use]
pub fn cardinality_of(key: &str) -> Option<ValueCardinality> {
    CONFIG_KEYS
        .iter()
        .find(|spec| spec.path.canonical() == key)
        .map(|spec| spec.value.cardinality())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn resource_namespaces_recognize_unknown_fields_but_not_similar_prefixes() {
        for (namespace, resource) in [
            ("remotes", ConfigResource::Remote),
            ("routes", ConfigResource::Route),
            ("mounts", ConfigResource::Mount),
            ("selections", ConfigResource::Selection),
        ] {
            assert_eq!(ConfigResource::from_key(namespace), Some(resource));
            assert_eq!(
                ConfigResource::from_key(&format!("{namespace}.unknown.field")),
                Some(resource)
            );
            assert_eq!(
                ConfigResource::from_key(&format!("{namespace}_other.field")),
                None
            );
        }
        assert_eq!(
            ConfigResource::from_key("network.request_concurrency"),
            None
        );
        assert_eq!(ConfigResource::from_key(""), None);
    }

    #[test]
    fn removed_sync_filters_are_not_config_keys() {
        assert_eq!(SettingKey::parse("sync.include"), None);
        assert_eq!(SettingKey::parse("sync.exclude"), None);
    }

    #[test]
    fn every_key_is_unique_and_named_paths_are_canonical() {
        let keys = CONFIG_KEYS
            .iter()
            .map(|spec| spec.path.canonical())
            .collect::<HashSet<_>>();
        assert_eq!(keys.len(), CONFIG_KEYS.len());
        for spec in CONFIG_KEYS {
            assert_eq!(spec.path.display(), spec.path.canonical());
        }
    }

    #[test]
    fn semantic_constants_drive_documented_ranges_and_enums() {
        let shards = CONFIG_KEYS
            .iter()
            .find(|spec| spec.path.canonical() == "lock.shard_levels")
            .unwrap();
        assert!(matches!(
            shards.value,
            ConfigValueSpec::Unsigned {
                min: Some(0),
                max: Some(max)
            } if max == u64::from(crate::config::MAX_SHARD_LEVELS)
        ));
        let ingest = CONFIG_KEYS
            .iter()
            .find(|spec| spec.path.canonical() == "cache.ingest_strategy")
            .unwrap();
        assert!(matches!(
            ingest.value,
            ConfigValueSpec::Enum { values } if values == crate::config::INGEST_STRATEGIES
        ));
        assert_eq!(
            MATERIALIZATION_MODES,
            crate::config::MaterializationMode::ALL
                .map(super::super::config::MaterializationMode::as_str)
        );
    }

    #[test]
    fn every_persisted_section_and_named_field_is_documented() {
        let sections = CONFIG_KEYS
            .iter()
            .map(|spec| spec.section)
            .collect::<HashSet<_>>();
        assert_eq!(
            sections,
            ConfigSection::ALL.into_iter().collect::<HashSet<_>>()
        );
        for key in [
            "remotes.<name>.url",
            "mounts.<name>.url",
            "mounts.<name>.target",
            "mounts.<name>.path",
            "mounts.<name>.rev",
            "mounts.<name>.rev_lock",
            "mounts.<name>.include",
            "mounts.<name>.exclude",
            "routes.<name>.path",
            "routes.<name>.remote",
        ] {
            assert!(
                CONFIG_KEYS.iter().any(|spec| spec.path.canonical() == key),
                "missing {key}"
            );
        }
    }

    #[test]
    fn config_command_keys_match_the_schema_and_alias_canonicalizes() {
        assert_eq!(
            SettingKey::CANONICAL
                .into_iter()
                .map(SettingKey::as_str)
                .collect::<HashSet<_>>(),
            settable_via_config_keys().collect::<HashSet<_>>()
        );
        assert_eq!(SettingKey::parse("git.exclude_patterns"), None);
    }
}

impl ConfigFieldSpec {
    #[must_use]
    pub fn environment_override(&self) -> Option<String> {
        match self.path {
            ConfigPath::Setting(key) if key.supports_environment() => Some(key.environment_name()),
            _ => None,
        }
    }
}

/// Wire-schema ownership is independent of command presentation metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigEntryKind {
    Setting(SettingKey),
    ResourceField,
    ResourceDefault,
    Generated,
}
impl ConfigFieldSpec {
    #[must_use]
    pub const fn kind(&self) -> ConfigEntryKind {
        match self.path {
            ConfigPath::Setting(key) => ConfigEntryKind::Setting(key),
            ConfigPath::ResourceDefault(_) => ConfigEntryKind::ResourceDefault,
            ConfigPath::ResourceField(ResourceField::MountRevisionLock) => {
                ConfigEntryKind::Generated
            }
            ConfigPath::ResourceField(_) => ConfigEntryKind::ResourceField,
        }
    }
}
