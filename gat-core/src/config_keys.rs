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
}

impl ConfigSection {
    pub const ALL: [Self; 8] = [
        Self::Remotes,
        Self::Cache,
        Self::Sync,
        Self::Selection,
        Self::Lock,
        Self::Mounts,
        Self::Routes,
        Self::GitIntegration,
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
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPath {
    Static(&'static str),
    Named {
        section: &'static str,
        field: Option<&'static str>,
    },
}

impl ConfigPath {
    #[must_use]
    pub fn display(self) -> String {
        match self {
            Self::Static(path) => path.to_string(),
            Self::Named {
                section,
                field: Some(field),
            } => format!("{section}.<name>.{field}"),
            Self::Named {
                section,
                field: None,
            } => format!("{section}.<name>"),
        }
    }

    /// # Panics
    /// Panics for a named path that is not part of the supported configuration schema.
    #[must_use]
    pub fn canonical(self) -> &'static str {
        match self {
            Self::Static(path) => path,
            Self::Named {
                section: "remotes",
                field: Some("url"),
            } => "remotes.<name>.url",
            Self::Named {
                section: "mounts",
                field: Some("url"),
            } => "mounts.<name>.url",
            Self::Named {
                section: "mounts",
                field: Some("target"),
            } => "mounts.<name>.target",
            Self::Named {
                section: "mounts",
                field: Some("path"),
            } => "mounts.<name>.path",
            Self::Named {
                section: "mounts",
                field: Some("rev"),
            } => "mounts.<name>.rev",
            Self::Named {
                section: "mounts",
                field: Some("rev_lock"),
            } => "mounts.<name>.rev_lock",
            Self::Named {
                section: "mounts",
                field: Some("include"),
            } => "mounts.<name>.include",
            Self::Named {
                section: "mounts",
                field: Some("exclude"),
            } => "mounts.<name>.exclude",
            Self::Named {
                section: "routes",
                field: Some("path"),
            } => "routes.<name>.path",
            Self::Named {
                section: "routes",
                field: Some("remote"),
            } => "routes.<name>.remote",
            Self::Named {
                section: "selections",
                field: Some("path"),
            } => "selections.<name>.path",
            Self::Named {
                section: "selections",
                field: Some("include"),
            } => "selections.<name>.include",
            Self::Named {
                section: "selections",
                field: Some("exclude"),
            } => "selections.<name>.exclude",
            Self::Named { .. } => panic!("unsupported named configuration path"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueCardinality {
    Scalar,
    List,
}

/// A configuration key supported by `gat config`.
///
/// The deprecated [`Self::GitExcludePatterns`] spelling remains accepted,
/// but [`Self::canonical`] always returns the persisted canonical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigKey {
    CacheLocation,
    CacheMaterializationStrategy,
    CacheIngestStrategy,
    SyncTrustState,
    SyncAutoFetch,
    SyncAutoRepair,
    LockShardLevels,
    GitIgnorePatterns,
    GitExcludePatterns,
}

impl ConfigKey {
    pub const CANONICAL: [Self; 8] = [
        Self::CacheLocation,
        Self::CacheMaterializationStrategy,
        Self::CacheIngestStrategy,
        Self::SyncTrustState,
        Self::SyncAutoFetch,
        Self::SyncAutoRepair,
        Self::LockShardLevels,
        Self::GitIgnorePatterns,
    ];

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "cache.location" => Self::CacheLocation,
            "cache.materialization_strategy" => Self::CacheMaterializationStrategy,
            "cache.ingest_strategy" => Self::CacheIngestStrategy,
            "sync.trust_state" => Self::SyncTrustState,
            "sync.auto_fetch" => Self::SyncAutoFetch,
            "sync.auto_repair" => Self::SyncAutoRepair,
            "lock.shard_levels" => Self::LockShardLevels,
            "git.ignore_patterns" => Self::GitIgnorePatterns,
            "git.exclude_patterns" => Self::GitExcludePatterns,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CacheLocation => "cache.location",
            Self::CacheMaterializationStrategy => "cache.materialization_strategy",
            Self::CacheIngestStrategy => "cache.ingest_strategy",
            Self::SyncTrustState => "sync.trust_state",
            Self::SyncAutoFetch => "sync.auto_fetch",
            Self::SyncAutoRepair => "sync.auto_repair",
            Self::LockShardLevels => "lock.shard_levels",
            Self::GitIgnorePatterns => "git.ignore_patterns",
            Self::GitExcludePatterns => "git.exclude_patterns",
        }
    }

    #[must_use]
    pub const fn canonical(self) -> Self {
        match self {
            Self::GitExcludePatterns => Self::GitIgnorePatterns,
            key => key,
        }
    }

    #[must_use]
    #[allow(
        clippy::missing_panics_doc,
        reason = "Every canonical key has schema metadata; callers cannot violate this invariant"
    )]
    pub fn cardinality(self) -> ValueCardinality {
        cardinality_of(self.canonical().as_str())
            .expect("every canonical config command key has schema metadata")
    }
}

impl std::fmt::Display for ConfigKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

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

pub struct ConfigKeySpec {
    pub path: ConfigPath,
    pub section: ConfigSection,
    pub value: ConfigValueSpec,
    pub default: ConfigDefault,
    pub description: &'static str,
    pub write_surface: ConfigWriteSurface,
    pub setter: ConfigSetter,
    pub environment_override: Option<&'static str>,
    pub examples: &'static [ConfigDocValue],
}

const BOOL_FALSE: ConfigDefault = ConfigDefault {
    persisted: PersistedDefault::Unset,
    effective: EffectiveDefault::Value(ConfigDocValue::Boolean(false)),
};
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
const INGEST_MODES: &[&str] = &["safe", "hybrid", "mmap"];

pub const CONFIG_KEYS: &[ConfigKeySpec] = &[
    ConfigKeySpec {
        path: ConfigPath::Static("remotes.default"),
        section: ConfigSection::Remotes,
        value: ConfigValueSpec::String,
        default: UNSET,
        description: "Name of the remote used when no explicit remote or more-specific route applies.",
        write_surface: ConfigWriteSurface::Remote,
        setter: ConfigSetter::Command {
            path: "remote default",
        },
        environment_override: None,
        examples: &[ConfigDocValue::String("origin")],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "remotes",
            field: Some("url"),
        },
        section: ConfigSection::Remotes,
        value: ConfigValueSpec::RemoteUrl,
        default: UNSET,
        description: "Object-storage URL for one named remote.",
        write_surface: ConfigWriteSurface::Remote,
        setter: ConfigSetter::Command { path: "remote add" },
        environment_override: None,
        examples: &[ConfigDocValue::String("s3://example-bucket/project")],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("cache.location"),
        section: ConfigSection::Cache,
        value: ConfigValueSpec::Path,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Derived("<repo>/.gat/objects"),
        },
        description: "Absolute cache path or a path relative to the repository root.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        environment_override: Some("GAT_CACHE_DIR"),
        examples: &[ConfigDocValue::String("/mnt/gat-cache")],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("cache.materialization_strategy"),
        section: ConfigSection::Cache,
        value: ConfigValueSpec::List {
            element: ConfigElementSpec::MaterializationMode,
            empty: EmptyListPolicy::Forbidden,
            ordered: true,
            unique: true,
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Value(ConfigDocValue::List(&["copy"])),
        },
        description: "Ordered fallback modes used to materialize cached objects into the working tree.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        environment_override: None,
        examples: &[ConfigDocValue::List(&["reflink", "copy"])],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("cache.ingest_strategy"),
        section: ConfigSection::Cache,
        value: ConfigValueSpec::Enum {
            values: INGEST_MODES,
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Value(ConfigDocValue::String("safe")),
        },
        description: "Copy-and-hash strategy used when publishing local files into the content-addressed cache.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        environment_override: None,
        examples: &[ConfigDocValue::String("safe")],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("sync.trust_state"),
        section: ConfigSection::Sync,
        value: ConfigValueSpec::Boolean,
        default: BOOL_FALSE,
        description: "Trust recorded materialized state without inspecting the working tree.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        environment_override: None,
        examples: &[ConfigDocValue::Boolean(true)],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("sync.auto_fetch"),
        section: ConfigSection::Sync,
        value: ConfigValueSpec::Boolean,
        default: BOOL_FALSE,
        description: "Fetch missing objects before normal sync reconciliation.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        environment_override: None,
        examples: &[ConfigDocValue::Boolean(true)],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("selections.default"),
        section: ConfigSection::Selection,
        value: ConfigValueSpec::String,
        default: UNSET,
        description: "Optional default selection name. Inherits independently of named definitions. Without a default, operations select the whole repository. Adding a selection never chooses a default.",
        write_surface: ConfigWriteSurface::Selection,
        setter: ConfigSetter::Command {
            path: "selection default",
        },
        environment_override: None,
        examples: &[ConfigDocValue::String("runtime")],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "selections",
            field: Some("path"),
        },
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
        environment_override: None,
        examples: &[ConfigDocValue::String("models")],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "selections",
            field: Some("include"),
        },
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
        environment_override: None,
        examples: &[ConfigDocValue::List(&["data/**", "models/**"])],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "selections",
            field: Some("exclude"),
        },
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
        environment_override: None,
        examples: &[ConfigDocValue::List(&["tmp/**"])],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("sync.auto_repair"),
        section: ConfigSection::Sync,
        value: ConfigValueSpec::Boolean,
        default: BOOL_FALSE,
        description: "Re-fetch and rematerialize cache objects found corrupted during sync.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        environment_override: None,
        examples: &[ConfigDocValue::Boolean(true)],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("lock.shard_levels"),
        section: ConfigSection::Lock,
        value: ConfigValueSpec::Unsigned {
            min: Some(0),
            max: Some(crate::config::MAX_SHARD_LEVELS as u64),
        },
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Value(ConfigDocValue::Unsigned(0)),
        },
        description: "Number of hash fan-out directory levels used by the persisted lock.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        environment_override: None,
        examples: &[ConfigDocValue::Unsigned(2)],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "mounts",
            field: Some("url"),
        },
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::GitLocation,
        default: UNSET,
        description: "Git repository from which this mount imports tracked rows.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        environment_override: None,
        examples: &[ConfigDocValue::String("../models")],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "mounts",
            field: Some("target"),
        },
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::Path,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Derived("repository name from the mount URL"),
        },
        description: "Destination subtree owned by the mount.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        environment_override: None,
        examples: &[ConfigDocValue::String("releases/resnet")],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "mounts",
            field: Some("path"),
        },
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::Path,
        default: ConfigDefault {
            persisted: PersistedDefault::Value(ConfigDocValue::String(".")),
            effective: EffectiveDefault::SameAsPersisted,
        },
        description: "Subtree inside the source repository from which rows are selected.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        environment_override: None,
        examples: &[ConfigDocValue::String("exports")],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "mounts",
            field: Some("rev"),
        },
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::String,
        default: ConfigDefault {
            persisted: PersistedDefault::Unset,
            effective: EffectiveDefault::Derived("source repository default branch"),
        },
        description: "Git revision the mount tracks.",
        write_surface: ConfigWriteSurface::Mount,
        setter: ConfigSetter::Command { path: "mount add" },
        environment_override: None,
        examples: &[ConfigDocValue::String("main")],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "mounts",
            field: Some("rev_lock"),
        },
        section: ConfigSection::Mounts,
        value: ConfigValueSpec::String,
        default: UNSET,
        description: "Exact commit resolved from the requested mount revision.",
        write_surface: ConfigWriteSurface::Automatic,
        setter: ConfigSetter::Automatic {
            description: "resolved automatically by {{command:mount add}} and {{command:mount update}}",
        },
        environment_override: None,
        examples: &[],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "mounts",
            field: Some("include"),
        },
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
        environment_override: None,
        examples: &[ConfigDocValue::List(&["**/*.onnx"])],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "mounts",
            field: Some("exclude"),
        },
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
        environment_override: None,
        examples: &[ConfigDocValue::List(&["tests/**"])],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "routes",
            field: Some("path"),
        },
        section: ConfigSection::Routes,
        value: ConfigValueSpec::Path,
        default: UNSET,
        description: "Root-relative tracked-path prefix served by the route.",
        write_surface: ConfigWriteSurface::Route,
        setter: ConfigSetter::Command { path: "route add" },
        environment_override: None,
        examples: &[ConfigDocValue::String("data/datasets")],
    },
    ConfigKeySpec {
        path: ConfigPath::Named {
            section: "routes",
            field: Some("remote"),
        },
        section: ConfigSection::Routes,
        value: ConfigValueSpec::String,
        default: UNSET,
        description: "Named remote that serves the route's path.",
        write_surface: ConfigWriteSurface::Route,
        setter: ConfigSetter::Command { path: "route add" },
        environment_override: None,
        examples: &[ConfigDocValue::String("backup")],
    },
    ConfigKeySpec {
        path: ConfigPath::Static("git.ignore_patterns"),
        section: ConfigSection::GitIntegration,
        value: ConfigValueSpec::List {
            element: ConfigElementSpec::GitIgnorePattern,
            empty: EmptyListPolicy::Allowed,
            ordered: true,
            unique: false,
        },
        default: EMPTY_LIST,
        description: "Single-line, non-negated Git-ignore patterns used to derive Gat's managed `.git/info/exclude` block.",
        write_surface: ConfigWriteSurface::Config,
        setter: CONFIG_SETTER,
        environment_override: None,
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
    fn removed_sync_filters_are_not_config_keys() {
        assert_eq!(ConfigKey::parse("sync.include"), None);
        assert_eq!(ConfigKey::parse("sync.exclude"), None);
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
            ConfigKey::CANONICAL
                .into_iter()
                .map(ConfigKey::as_str)
                .collect::<HashSet<_>>(),
            settable_via_config_keys().collect::<HashSet<_>>()
        );
        assert_eq!(
            ConfigKey::GitExcludePatterns.canonical(),
            ConfigKey::GitIgnorePatterns
        );
    }
}
