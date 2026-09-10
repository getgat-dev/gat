//! Pure semantic `gat.yaml` configuration model: every persisted config
//! section's typed value, merge/validation logic,
//! and value-level parsing, independent of any filesystem, YAML-decoding,
//! or document-version concern. A repository can have up to three config
//! layers -- global, project, local -- which merge into one effective
//! [`Config`] with local overriding project overriding global (see
//! [`Config::merge_layers`]).
//!
//! `gat_io::ConfigStore` adds the I/O-owned failure modes (unreadable file,
//! invalid UTF-8, invalid YAML syntax, unsupported version, ...) and owns
//! the raw wire DTOs, document version, filesystem reads/writes, and
//! route-name/path normalization performed at config-load time.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Typed failures from parsing, validating, and resolving `gat.yaml`'s
/// schema and values -- every variant that never depends on filesystem
/// or on-disk state. `gat_io::ConfigStore` adds the I/O-owned failure modes
/// around it (unreadable file, invalid UTF-8, invalid syntax, unsupported
/// version). Every variant's own [`thiserror`]-derived `Display` is
/// deliberately terse and never embeds CLI instructions -- actionable
/// next-steps belong in the presentation layer built from this error,
/// not model text.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid setting {key}: {reason}")]
    InvalidSettingValue {
        key: crate::settings::SettingKey,
        reason: crate::settings::SettingValueError,
    },
    #[error("no selection named `{name}`")]
    UnknownSelection { name: crate::name::SelectionName },
    /// A standalone route path (not yet attached to a named route in a
    /// config file, e.g. a `gat route add <path>` CLI argument) is not a
    /// valid Gat-lexical relative path.
    #[error("`{input}` is not a valid path")]
    InvalidPath {
        input: String,
        #[source]
        source: Box<crate::lexical_path::LexicalPathError>,
    },

    /// A named route's `path` (loaded from a `gat.yaml`) is not a valid
    /// Gat-lexical relative path.
    #[error("route `{name}` has an invalid path `{path}`")]
    InvalidRoutePath {
        name: String,
        path: String,
        #[source]
        source: Box<crate::lexical_path::LexicalPathError>,
    },

    /// A named mount's `path` (the source subpath it pulls from, loaded
    /// from a `gat.yaml`) is not a valid Gat-lexical subpath.
    #[error("mount `{name}` has an invalid path `{path}`")]
    InvalidMountSourcePath {
        name: String,
        path: String,
        #[source]
        source: Box<crate::lexical_path::LexicalPathError>,
    },

    /// A route is named `*`, which is reserved for `gat route list`'s
    /// synthetic default-remote row.
    #[error("route name `*` is reserved for gat route list's synthetic default-remote row")]
    ReservedRouteName,

    /// Two distinct routes configure the exact same effective path.
    #[error("routes `{first_name}` and `{second_name}` both configure the same path `{path}`")]
    RouteConflict {
        first_name: String,
        second_name: String,
        path: String,
    },

    /// A mount's target is `.` (the repository root), which a mount can
    /// never own.
    #[error("mount target cannot be `.` (the repository root)")]
    MountTargetIsRoot { name: Option<String> },

    /// A mount's target equals, contains, or is contained within another
    /// mount's target.
    #[error("mount target `{target}` overlaps mount `{other_name}`'s target `{other_target}`")]
    MountTargetOverlap {
        name: Option<String>,
        target: String,
        other_name: String,
        other_target: String,
    },

    /// `cache.materialization_strategy`/a link-mode CLI value names a mode
    /// gat does not know.
    #[error("unknown materialization mode `{value}`")]
    InvalidLinkMode { value: String },

    /// `cache.materialization_strategy` lists the same mode more than
    /// once.
    #[error("duplicate materialization mode `{mode}` in cache.materialization_strategy")]
    DuplicateLinkMode { mode: String },

    /// `cache.materialization_strategy` was given an empty list.
    #[error("cache.materialization_strategy must list at least one mode")]
    EmptyLinkModeList,

    /// `cache.ingest_strategy`'s value names a strategy gat does not
    /// know.
    #[error("unknown ingest strategy `{value}`")]
    InvalidIngestStrategy { value: String },

    /// `lock.shard_levels`'s value is not a valid number.
    #[error("invalid lock.shard_levels value `{value}` (expected a number)")]
    InvalidShardLevels {
        value: String,
        #[source]
        source: std::num::ParseIntError,
    },

    /// `lock.shard_levels`'s value exceeds the maximum supported depth.
    #[error("lock.shard_levels `{levels}` exceeds the maximum of {max}")]
    ShardLevelsExceedsMaximum { levels: u8, max: u8 },

    /// A `git.ignore_patterns` entry is negated (`!`-prefixed), which
    /// gat's derived excludes do not support.
    #[error(
        "git.ignore_patterns entry `{pattern}` is negated (`!`); negated patterns are not \
         supported"
    )]
    InvalidGitIgnorePattern { pattern: String },

    /// Each ignore-pattern value must represent exactly one line.
    #[error("git.ignore_patterns entry `{pattern}` contains a line break")]
    MultilineGitIgnorePattern { pattern: String },

    /// A named mount's `include`/`exclude` (loaded from a `gat.yaml`) has
    /// an invalid glob pattern.
    #[error("mount `{name}` has an invalid {field} pattern `{pattern}`")]
    InvalidMountGlobPattern {
        name: String,
        field: &'static str,
        pattern: String,
        #[source]
        source: Box<crate::globs::GlobError>,
    },

    /// The requested remote name is not defined in `remotes:`.
    #[error("no remote named `{name}`")]
    UnknownRemote { name: String },

    /// A boolean-valued config field (e.g. `sync.trust_state`) was given a
    /// value that is not `true`/`false`.
    #[error("invalid {field} value `{value}` (expected true/false)")]
    InvalidBooleanValue { field: &'static str, value: String },
}

/// Bridges [`crate::git_ignore::GitIgnorePatternError`] into this
/// module's own [`ConfigError`], so `?` at call sites that already
/// return `Result<_, ConfigError>` (e.g. [`validate_ignore_patterns`])
/// converts automatically without every caller needing its own explicit
/// match.
impl From<crate::git_ignore::GitIgnorePatternError> for ConfigError {
    fn from(err: crate::git_ignore::GitIgnorePatternError) -> Self {
        match err {
            crate::git_ignore::GitIgnorePatternError::Negated { pattern } => {
                Self::InvalidGitIgnorePattern { pattern }
            }
            crate::git_ignore::GitIgnorePatternError::Multiline { pattern } => {
                Self::MultilineGitIgnorePattern { pattern }
            }
        }
    }
}

/// Which of the (up to) three `gat.yaml` locations a `gat config`/`gat
/// remote`/`gat mount` write targets, chosen with that command's
/// `--global`/`--project`/`--local` flag (`--project` is the default when
/// none is given). Reading always uses the merged effective config (see
/// [`Config::merge_layers`]) regardless of scope -- these flags only ever
/// pick where a write lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigScope {
    /// `~/.gat/gat.yaml` -- applies to every repository for this user.
    Global,
    /// `<repo_root>/gat.yaml` -- committed to git, shared by everyone who
    /// clones the repo. The default scope.
    #[default]
    Project,
    /// `<repo_root>/.gat/gat.yaml` -- repo-local and (since it lives
    /// under `.gat/`) never committed; for machine-specific overrides.
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigScopeDoc {
    pub scope: ConfigScope,
    pub name: &'static str,
    pub path: &'static str,
    pub committed: bool,
    pub intended_use: &'static str,
}

impl ConfigScope {
    pub const ALL: [Self; 3] = [Self::Global, Self::Project, Self::Local];

    /// This scope's override precedence: higher wins on merge
    /// (`Global` < `Project` < `Local`), matching [`Config::merge_layers`]'s
    /// `[global, project, local]` layering order.
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            Self::Global => 0,
            Self::Project => 1,
            Self::Local => 2,
        }
    }

    #[must_use]
    pub const fn documentation(self) -> ConfigScopeDoc {
        match self {
            Self::Global => ConfigScopeDoc {
                scope: self,
                name: "Global",
                path: "~/.gat/gat.yaml",
                committed: false,
                intended_use: "User-wide defaults shared by every repository.",
            },
            Self::Project => ConfigScopeDoc {
                scope: self,
                name: "Project (default)",
                path: "<repo>/gat.yaml",
                committed: true,
                intended_use: "Repository configuration shared with clones.",
            },
            Self::Local => ConfigScopeDoc {
                scope: self,
                name: "Local",
                path: "<repo>/.gat/gat.yaml",
                committed: false,
                intended_use: "Machine-specific repository overrides.",
            },
        }
    }
}

impl std::fmt::Display for ConfigScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Local => "local",
        };
        f.write_str(name)
    }
}

/// One semantic configuration layer, or the merged effective configuration
/// assembled from several layers. The persisted document version is owned by
/// `gat-io` and does not participate in semantic merging.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct Config {
    #[serde(
        default,
        skip_serializing_if = "crate::settings::NetworkConfig::is_empty"
    )]
    pub network: crate::settings::NetworkConfig,
    /// Named remotes and an optional default, managed with `gat remote`.
    /// Adding a remote never chooses a default; use `gat remote default`.
    #[serde(default, skip_serializing_if = "RemotesConfig::is_empty")]
    pub remotes: RemotesConfig,
    /// Local object cache settings. Set with `gat config cache.<key> <value>`.
    #[serde(default, skip_serializing_if = "CacheConfig::is_empty")]
    pub cache: CacheConfig,
    /// `gat sync` defaults. Set with `gat config sync.<key> <value>`.
    #[serde(default, skip_serializing_if = "SyncConfig::is_empty")]
    pub sync: SyncConfig,
    /// Reusable path selections and an optional default choice.
    #[serde(default, skip_serializing_if = "SelectionsConfig::is_empty")]
    pub selections: SelectionsConfig,
    /// `gat.lock` on-disk layout settings. Set with `gat config lock.<key>
    /// <value>`.
    #[serde(default, skip_serializing_if = "LockConfig::is_empty")]
    pub lock: LockConfig,
    /// Named mounts: subtrees imported from another Git repository under
    /// an owned destination target, each owning every Gat-managed path
    /// beneath that target. Keyed by a stable mount `NAME`. Set with `gat
    /// mount add <NAME> <URL> <TARGET> [...]`; only `gat mount` commands
    /// may write here or touch a mount-owned `gat.lock` row.
    #[serde(default, skip_serializing_if = "MountsConfig::is_empty")]
    pub mounts: MountsConfig,
    /// Named routes: path-based storage routing policy, keyed by a
    /// stable route `NAME`. Each route selects which named remote serves
    /// a given tracked path's object bytes, independent of who owns that
    /// path. A route's `path` is root-relative and `/`-separated; the
    /// most-specific (longest) matching path across every configured
    /// route wins, falling back to `remotes.default` when nothing
    /// matches. Route names never affect this precedence. Set with `gat
    /// route add <NAME> <REMOTE> <PATH>`; introspect with `gat
    /// route list`/`gat route show <NAME>`. Routes never grant
    /// ownership/mutation permission over mount-owned paths.
    #[serde(default, skip_serializing_if = "RoutesConfig::is_empty")]
    pub routes: RoutesConfig,
    /// `.git/info/exclude` derivation policy. Set with `gat config
    /// git.<key> <value>`. These settings control derivation of Git's
    /// repository-local exclude file. See [`GitConfig`].
    #[serde(default, skip_serializing_if = "GitConfig::is_empty")]
    pub git: GitConfig,
}

/// Named object-storage endpoints and the default remote's name.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct RemotesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<crate::name::RemoteName>,
    #[serde(flatten)]
    pub by_name: BTreeMap<crate::name::RemoteName, RemoteConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    pub url: crate::endpoint::RemoteUrlTemplate,
}

impl RemoteConfig {
    #[must_use]
    pub const fn new(url: crate::endpoint::RemoteUrlTemplate) -> Self {
        Self { url }
    }
}

impl From<crate::endpoint::RemoteUrlTemplate> for RemoteConfig {
    fn from(url: crate::endpoint::RemoteUrlTemplate) -> Self {
        Self::new(url)
    }
}

impl From<String> for RemoteConfig {
    fn from(url: String) -> Self {
        Self::new(crate::endpoint::RemoteUrlTemplate::from_string(url))
    }
}

impl RemotesConfig {
    fn is_empty(&self) -> bool {
        self.default.is_none() && self.by_name.is_empty()
    }

    /// Layers `override_` over `self`: entries in `override_.by_name` win
    /// on a name collision. An explicitly selected default replaces the
    /// lower-scope default. Combines the global/project/local `remotes:` sections into one
    /// effective set (see [`Config::merge_layers`]).
    fn merged_with(mut self, override_: Self) -> Self {
        self.by_name.extend(override_.by_name);
        Self {
            default: override_.default.or(self.default),
            by_name: self.by_name,
        }
    }
}

/// Local object cache settings, nested under `cache:` in `gat.yaml`.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct CacheConfig {
    /// Where the local content-addressed cache lives: an absolute path, or
    /// relative to the repo root. Unset means `<repo>/.gat/objects`. The
    /// `GAT_CACHE_LOCATION` environment variable overrides this when set (e.g.
    /// to share one cache across repos/checkouts without editing
    /// `gat.yaml`). Set with `gat config cache.location <path>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<crate::cache_location::CacheLocation>,
    /// How `gat sync` materializes cache objects into the working tree: a
    /// preference list of modes, tried in order until one succeeds. See
    /// [`MaterializationMode`]. Unset means `copy`. Set with `gat config cache.materialization_strategy
    /// <mode> [<mode>...]`; persists as a YAML sequence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialization_strategy: Option<MaterializationStrategy>,
    /// Which large-file ingestion implementation `gat add`/`gat fetch` use.
    /// See [`IngestStrategy`]. Unset means [`DEFAULT_INGEST_STRATEGY`]. Set
    /// with `gat config cache.ingest_strategy <mode>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingest_strategy: Option<IngestStrategy>,
}

/// A single `cache.materialization_strategy` materialization mode. Parsed/validated once at
/// the `gat.yaml`/`gat config` boundary (see [`MaterializationStrategy`]) instead of
/// staying a bare `&str` all the way down to the I/O materializer, so a
/// misspelled or unsupported mode is rejected where the user typed it,
/// not deep inside the sync engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MaterializationMode {
    /// Copy-on-write clone — fast, space-efficient, and safe from cache
    /// corruption since the destination is its own copy-on-write inode.
    Reflink,
    /// Shares inode/bytes with the cache — free, but requires the cache
    /// and working tree on the same filesystem, and the file stays
    /// read-only since the cache entry is protected.
    Hardlink,
    /// Works across filesystems, but exposes the cache object path
    /// through the link.
    Symlink,
    /// Always works, but duplicates the data.
    Copy,
}

impl MaterializationMode {
    /// Every variant, in the order `gat config`/docs should list them.
    pub const ALL: [Self; 4] = [Self::Reflink, Self::Hardlink, Self::Symlink, Self::Copy];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reflink => "reflink",
            Self::Hardlink => "hardlink",
            Self::Symlink => "symlink",
            Self::Copy => "copy",
        }
    }
}

impl std::fmt::Display for MaterializationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MaterializationMode {
    type Err = ConfigError;

    fn from_str(s: &str) -> std::result::Result<Self, ConfigError> {
        match s {
            "reflink" => Ok(Self::Reflink),
            "hardlink" => Ok(Self::Hardlink),
            "symlink" => Ok(Self::Symlink),
            "copy" => Ok(Self::Copy),
            other => Err(ConfigError::InvalidLinkMode {
                value: other.to_string(),
            }),
        }
    }
}

/// `cache.materialization_strategy`'s parsed preference list: an ordered, non-empty list of
/// distinct [`MaterializationMode`]s, tried in turn by the I/O materializer until one
/// succeeds. Rejects empty lists and duplicate modes at parse time (i.e.
/// at `gat config cache.materialization_strategy <value>` or `gat.yaml` load), instead of
/// letting either invalid state persist into materialization.
/// Serializes/deserializes as a native YAML sequence of [`MaterializationMode`]
/// strings (e.g. `[reflink, hardlink, copy]`), not a comma-separated
/// scalar. `gat config cache.materialization_strategy` accepts one CLI
/// argument per mode (see [`Self::from_values`]) instead of a single
/// `,`-delimited
/// value.
///
/// ```compile_fail
/// use gat_core::config::MaterializationStrategy;
/// let invalid = MaterializationStrategy(Vec::new());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializationStrategy(Vec<MaterializationMode>);

impl MaterializationStrategy {
    #[must_use]
    pub fn modes(&self) -> &[MaterializationMode] {
        &self.0
    }

    /// Validates a caller-supplied ordered list of modes -- shared by
    /// [`Self::from_values`] (the `gat config cache.materialization_strategy` CLI boundary)
    /// and [`Deserialize`] (the `gat.yaml` boundary) so both enforce the
    /// same non-empty/distinct/valid-`MaterializationMode` rule. Order is preserved
    /// as given: it's the fallback try-order, not sorted.
    fn validate(modes: Vec<MaterializationMode>) -> std::result::Result<Self, ConfigError> {
        if modes.is_empty() {
            return Err(ConfigError::EmptyLinkModeList);
        }
        for (i, mode) in modes.iter().enumerate() {
            if modes[..i].contains(mode) {
                return Err(ConfigError::DuplicateLinkMode {
                    mode: mode.to_string(),
                });
            }
        }
        Ok(Self(modes))
    }

    /// Parses `cache.materialization_strategy`'s CLI representation -- one positional argument
    /// per mode, e.g. `gat config cache.materialization_strategy reflink hardlink copy` --
    /// preserving order. Never splits on `,`: a value like `reflink` is one
    /// whole mode, not a delimited list.
    pub fn from_values<S: AsRef<str>>(values: &[S]) -> std::result::Result<Self, ConfigError> {
        let modes = values
            .iter()
            .map(|v| v.as_ref().parse())
            .collect::<std::result::Result<Vec<MaterializationMode>, ConfigError>>()?;
        Self::validate(modes)
    }
}

impl TryFrom<Vec<MaterializationMode>> for MaterializationStrategy {
    type Error = ConfigError;

    fn try_from(modes: Vec<MaterializationMode>) -> Result<Self, Self::Error> {
        Self::validate(modes)
    }
}

impl From<MaterializationMode> for MaterializationStrategy {
    fn from(mode: MaterializationMode) -> Self {
        Self(vec![mode])
    }
}

/// `cache.materialization_strategy`'s fallback when unset. Plain `copy` is the safest
/// cross-platform default (reflink/hardlink support varies a lot by
/// filesystem and OS); opt into the faster, space-saving modes explicitly
/// with `gat config cache.materialization_strategy reflink hardlink symlink copy`.
impl Default for MaterializationStrategy {
    fn default() -> Self {
        Self::from(MaterializationMode::Copy)
    }
}

/// Parses a single mode string, e.g. `"copy".parse::<MaterializationStrategy>()`, into a
/// one-element `MaterializationStrategy` -- a convenience for tests and call sites that
/// already have one mode in hand, not a `,`-delimited list parser.
impl std::str::FromStr for MaterializationStrategy {
    type Err = ConfigError;

    fn from_str(s: &str) -> std::result::Result<Self, ConfigError> {
        let mode: MaterializationMode = s.parse()?;
        Ok(Self::from(mode))
    }
}

impl Serialize for MaterializationStrategy {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MaterializationStrategy {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let modes = Vec::<MaterializationMode>::deserialize(deserializer)?;
        Self::validate(modes).map_err(serde::de::Error::custom)
    }
}

/// Which large-file `ingest_file` implementation to use. The `Safe`
/// strategy is the default because it guarantees that published bytes match
/// their content identifier; the other strategies are explicit opt-ins.
/// Unset `cache.ingest_strategy` means [`DEFAULT_INGEST_STRATEGY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IngestStrategy {
    /// Default. Copy first, then BLAKE3-hash the copy. Never trusts an
    /// independent re-read of a possibly-concurrently-modified source, so
    /// it's the only strategy that guarantees the invariant a
    /// content-addressed cache depends on -- the published object's bytes
    /// always match its own oid, even if the source changes during
    /// ingestion -- but it pays for copy and hash sequentially instead of
    /// overlapping them.
    Safe,
    /// Explicit opt-in; not the default. Optimistically copy and hash the
    /// source concurrently, but fingerprint
    /// the source's size+mtime before and after and fall back to safely
    /// re-hashing the copy if either changed mid-operation. This keeps the
    /// common-case speed advantage of overlapping copy and hash, but the
    /// size+mtime fingerprint is only a heuristic: a
    /// same-size rewrite within the filesystem's timestamp granularity can
    /// evade it, so unlike `Safe` this cannot guarantee the published
    /// bytes match their own oid under concurrent source modification.
    /// Choose this only if you accept that risk in exchange for speed.
    Hybrid,
    /// Explicit opt-in; not the default. Map the source into memory once
    /// and have the copy and the hash both read from that single mapping,
    /// so they can't independently re-read the source out of sync with
    /// each other. The source file must remain stable for the lifetime of
    /// that mapping: another process modifying, truncating, or replacing
    /// the mapped file while both readers are running is outside this
    /// strategy's safety assumptions, so like `Hybrid` it does not carry
    /// `Safe`'s guarantee under concurrent source modification. Choose
    /// this only when that source-file stability can be guaranteed.
    Mmap,
}

/// Valid `cache.ingest_strategy` values, in the order `gat config` should
/// list them.
pub const INGEST_STRATEGIES: &[&str] = &{
    let mut names = [""; IngestStrategy::ALL.len()];
    let mut index = 0;
    while index < names.len() {
        names[index] = IngestStrategy::ALL[index].as_str();
        index += 1;
    }
    names
};

/// `cache.ingest_strategy`'s default when unset. For a
/// content-addressed cache, correctness by construction takes priority
/// over the benchmark advantage of overlapping copy and hash, so `Safe` --
/// the only strategy that guarantees the published oid always describes
/// the exact published bytes, including when the source changes during
/// ingestion -- is the default. `Hybrid` and `Mmap` remain available as
/// explicit opt-ins (`gat config cache.ingest_strategy hybrid|mmap`) for
/// users who accept their concurrent-source-modification risk in exchange
/// for speed; `mmap` in particular assumes the source file stays stable for
/// the duration of the mapping/read, so `Safe` remains the right choice
/// when that cannot be guaranteed.
pub const DEFAULT_INGEST_STRATEGY: IngestStrategy = IngestStrategy::Safe;

impl IngestStrategy {
    /// Every variant, in the order configuration and documentation list them.
    pub const ALL: [Self; 3] = [Self::Safe, Self::Hybrid, Self::Mmap];

    /// Static lowercase spelling, matching [`Display`](std::fmt::Display)
    /// -- used wherever a `&'static str` is needed (e.g.
    /// `lifecycle::Surface::ConfigValue` observation) instead of
    /// allocating via `.to_string()`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Hybrid => "hybrid",
            Self::Mmap => "mmap",
        }
    }
}

impl std::fmt::Display for IngestStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for IngestStrategy {
    type Err = ConfigError;

    fn from_str(s: &str) -> std::result::Result<Self, ConfigError> {
        Self::ALL
            .into_iter()
            .find(|strategy| strategy.as_str() == s)
            .ok_or_else(|| ConfigError::InvalidIngestStrategy {
                value: s.to_owned(),
            })
    }
}

impl CacheConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Named selections merge by name, replacing complete definitions on collision.
/// The optional default pointer inherits independently of the definitions.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct SelectionsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<crate::name::SelectionName>,
    #[serde(flatten)]
    pub by_name: BTreeMap<crate::name::SelectionName, SelectionConfig>,
}

impl SelectionsConfig {
    fn is_empty(&self) -> bool {
        self.default.is_none() && self.by_name.is_empty()
    }

    fn merged_with(mut self, override_: Self) -> Self {
        self.by_name.extend(override_.by_name);
        self.default = override_.default.or(self.default);
        self
    }

    pub fn validate_effective(&self) -> Result<(), ConfigError> {
        if let Some(name) = &self.default
            && !self.by_name.contains_key(name)
        {
            return Err(ConfigError::UnknownSelection { name: name.clone() });
        }
        Ok(())
    }
}

/// One complete named selection. Same-name definitions replace as a whole.
/// Explicit CLI selection replaces the path and both pattern lists.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct SelectionConfig {
    /// Literal repository-root-relative file or directory; defaults to root.
    #[serde(
        default,
        skip_serializing_if = "crate::lexical_path::GatSubpath::is_root"
    )]
    pub path: crate::lexical_path::GatSubpath,
    /// Include alternatives relative to path; omitted or empty selects everything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<crate::globs::GatGlobPattern>>,
    /// Excludes relative to path win over includes; omitted or empty excludes nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<crate::globs::GatGlobPattern>>,
}

impl SelectionConfig {
    /// Whether this definition has root scope and no pattern restrictions,
    /// independent of lock contents. Omitted and empty lists are equivalent.
    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.path.is_root()
            && self.include.as_ref().is_none_or(Vec::is_empty)
            && self.exclude.as_ref().is_none_or(Vec::is_empty)
    }
}

/// Move a saved definition into its matcher without cloning or recompiling patterns.
impl From<SelectionConfig> for crate::selection::Selection {
    fn from(definition: SelectionConfig) -> Self {
        Self::from_scope_patterns(
            definition.path.into_path_scope(),
            definition.include.unwrap_or_default(),
            definition.exclude.unwrap_or_default(),
        )
    }
}

/// Synchronization behavior under `sync:` in `gat.yaml`. Shared path defaults
/// are configured separately through [`SelectionsConfig`].
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    /// Whether `gat sync` trusts cached desired/materialized state
    /// without touching the working tree when no `--trust-state` flag is
    /// given. Unset means inherit from lower-priority config layers; the
    /// built-in default is `false`, which keeps validation enabled.
    /// `true` opts into the trust-state fast path. Set with `gat config
    /// sync.trust_state <true|false>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_state: Option<bool>,
    /// Fetch missing cache objects from the remote before reconciling,
    /// like `gat pull` does, instead of `gat sync`'s normal local-cache-only
    /// behavior. Unset means inherit from a lower-priority layer, falling
    /// back to the built-in default `false`. Set with `gat config
    /// sync.auto_fetch <true|false>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_fetch: Option<bool>,
    /// Automatically re-fetch and re-materialize any cache object found
    /// corrupted during a sync conflict, as if `--repair` were always
    /// passed. Unset means inherit from a lower-priority layer, falling
    /// back to the built-in default `false`. Set with `gat config
    /// sync.auto_repair <true\|false>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_repair: Option<bool>,
}

impl SyncConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Effective `auto_fetch`, after layering: an explicit value (`true` or
    /// `false`) set in any layer wins over lower-priority layers, and the
    /// built-in default `false` applies only when every layer left it
    /// unset. See [`crate::settings::SettingsLayer`].
    #[must_use]
    pub fn auto_fetch(&self) -> bool {
        self.auto_fetch.unwrap_or(false)
    }

    /// Effective `auto_repair`, after layering. See [`Self::auto_fetch`].
    #[must_use]
    pub fn auto_repair(&self) -> bool {
        self.auto_repair.unwrap_or(false)
    }
}

/// `gat.lock` on-disk layout, nested under `lock:` in `gat.yaml`.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct LockConfig {
    /// How many fan-out directory levels to shard `gat.lock` into, keyed
    /// by the first `shard_levels` bytes of `blake3(path)` (hex-encoded,
    /// two characters per level) -- the same fan-out trick
    /// `.gat/objects/` already uses for cache objects. Unset or
    /// `0` means a single flat `gat.lock` file (today's format, and the
    /// default). A positive `N` replaces it with a `gat.lock/` directory
    /// of shard files, e.g. `N=1` gives `gat.lock/xx.tsv`, `N=2` gives
    /// `gat.lock/xx/yy.tsv`. Set with `gat config lock.shard_levels <n>`.
    ///
    /// Configuration loading and [`parse_shard_levels`] validate this against
    /// [`MAX_SHARD_LEVELS`], which is stricter than the type's structural limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard_levels: Option<crate::lock::LockShardLevels>,
}

/// Highest `lock.shard_levels` accepted by configuration validation.
/// This product limit is stricter than [`crate::lock::LockShardLevels::MAX`],
/// the four-byte representation's structural limit.
pub const MAX_SHARD_LEVELS: u8 = 2;

impl LockConfig {
    const fn is_empty(&self) -> bool {
        self.shard_levels.is_none()
    }

    /// The effective shard level: flat (the default) when unset.
    #[must_use]
    pub fn shard_levels(&self) -> crate::lock::LockShardLevels {
        self.shard_levels
            .unwrap_or(crate::lock::LockShardLevels::FLAT)
    }
}

/// Validate a shard depth against both the product and structural limits.
/// Shared by [`parse_shard_levels`] and configuration decoding in `gat-io`.
pub fn validate_shard_levels(
    levels: u8,
) -> std::result::Result<crate::lock::LockShardLevels, ConfigError> {
    if levels > MAX_SHARD_LEVELS {
        return Err(ConfigError::ShardLevelsExceedsMaximum {
            levels,
            max: MAX_SHARD_LEVELS,
        });
    }
    crate::lock::LockShardLevels::new(levels).map_err(|source| {
        ConfigError::ShardLevelsExceedsMaximum {
            levels: source.depth,
            max: source.max,
        }
    })
}

/// Parses and validates a `lock.shard_levels` value, as set via
/// `gat config lock.shard_levels <n>`. Rejects anything above
/// [`MAX_SHARD_LEVELS`] at the config boundary, the same place
/// `MaterializationStrategy`/`IngestStrategy` validate their own settings, rather than
/// deep inside `Lock`'s file I/O.
pub fn parse_shard_levels(
    s: &str,
) -> std::result::Result<crate::lock::LockShardLevels, ConfigError> {
    let levels: u8 = s
        .parse()
        .map_err(|source| ConfigError::InvalidShardLevels {
            value: s.to_string(),
            source,
        })?;
    validate_shard_levels(levels)
}

/// Parses a boolean-valued `sync.*` setting (e.g. `sync.trust_state`,
/// `sync.auto_fetch`, `sync.auto_repair`), as set via `gat config
/// sync.<field> <value>`. Only the literal strings `true`/`false` are
/// accepted; `field` is the dotted config key, used verbatim in the
/// resulting diagnostic so the user knows which setting was rejected.
pub fn parse_sync_bool_setting(
    field: &'static str,
    value: &str,
) -> std::result::Result<bool, ConfigError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ConfigError::InvalidBooleanValue {
            field,
            value: value.to_string(),
        }),
    }
}

/// `.git/info/exclude` derivation policy, nested under `git:` in
/// `gat.yaml`. The managed block is always rebuilt from `gat.lock`, `.gat/`,
/// and the
/// settings here -- never from directory ownership metadata persisted in
/// `gat.lock` itself, and never from a filesystem or git-index walk.
#[derive(Debug, Serialize, Clone, PartialEq, Eq, Default)]
pub struct GitConfig {
    /// Ordered list of Git-ignore-style patterns written verbatim into
    /// gat's managed `.git/info/exclude` block, e.g. `*.safetensors` or
    /// `/artifacts/**/*.bin`. Interpreted with `.gitignore`/
    /// `.git/info/exclude` semantics, not filesystem-glob semantics.
    /// Line breaks and negated (`!`-prefixed) patterns are rejected. Unset
    /// (the default) means every Gat-tracked file without LF gets its own
    /// exact exclude rule instead; add patterns here (e.g. a directory
    /// pattern like `/data/`) to opt into broader, hand-authored coverage
    /// and skip the per-file rules it makes redundant. The deprecated
    /// `git.exclude_patterns` alias is accepted on input (see
    /// `GitConfigRaw`/[`Self::deserialize`] below), but values always
    /// persist under this canonical `ignore_patterns` name. `None` means unset in
    /// this layer (inherit from a lower-priority layer); `Some(vec![])`
    /// is an explicit empty override -- see [`crate::settings::SettingsLayer`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_patterns: Option<Vec<crate::git_ignore::GitIgnorePattern>>,
}

/// Shadow of [`GitConfig`]'s on-disk shape, accepting both the canonical
/// `ignore_patterns` key and the deprecated `exclude_patterns` alias so
/// [`GitConfig`]'s own `Deserialize` impl can detect and reject a hand-edited
/// `gat.yaml` that sets both rather than silently choosing one.
///
/// Both fields use the "double `Option`" pattern
/// (`#[serde(default, deserialize_with = "deserialize_present")]`) rather
/// than a plain `Option<Vec<String>>`: with a plain `Option<T>`, most
/// format deserializers (including YAML) special-case an explicit `null`
/// as `None`, making it indistinguishable from the key being missing
/// entirely -- which would let e.g. `ignore_patterns: null` alongside a
/// present `exclude_patterns` evade the "both keys present" conflict
/// check below, and would silently accept `null` as if it meant "no
/// patterns" rather than rejecting it as the wrong type. With
/// `deserialize_present`, a missing key still defaults to `None` (via
/// `#[serde(default)]`, which never invokes the deserializer at all),
/// but a key that is *present* -- including `null` -- always deserializes
/// its value as `Vec<String>`, so a present `null` fails with a type
/// error instead of silently meaning "absent".
#[derive(Deserialize, Default)]
struct GitConfigRaw {
    #[serde(default, deserialize_with = "deserialize_present")]
    ignore_patterns: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    exclude_patterns: Option<Vec<String>>,
}

/// Deserializes a present field's value as `T`, wrapping it in `Some` --
/// used together with `#[serde(default)]` so a *missing* key defaults to
/// `None` without ever calling this function, while a *present* key
/// (including an explicit `null`) always deserializes as `T` and fails
/// with a normal type-mismatch error if it isn't one. See
/// `GitConfigRaw`'s doc comment for why this differs from a plain
/// `Option<T>` field.
fn deserialize_present<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for GitConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = GitConfigRaw::deserialize(deserializer)?;
        // Reject by *key presence*, not by non-empty value: a hand-edited
        // `gat.yaml` that sets both spellings (even `ignore_patterns: []`
        // alongside a non-empty `exclude_patterns`, or vice versa) is still
        // an ambiguous conflict and must fail loudly rather than silently
        // pick one.
        let ignore_patterns = match (raw.ignore_patterns, raw.exclude_patterns) {
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "gat.yaml sets both `git.ignore_patterns` and the deprecated \
                     `git.exclude_patterns`; keep only `git.ignore_patterns` (its canonical \
                     replacement) and remove the other",
                ));
            }
            (Some(ignore), None) => Some(ignore),
            (None, Some(exclude)) => Some(exclude),
            (None, None) => None,
        };
        let ignore_patterns = match ignore_patterns {
            Some(raw) => Some(
                raw.into_iter()
                    .map(crate::git_ignore::GitIgnorePattern::parse)
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(serde::de::Error::custom)?,
            ),
            None => None,
        };
        Ok(Self { ignore_patterns })
    }
}

impl GitConfig {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Effective `ignore_patterns`: `&[]` when unset or explicitly empty.
    #[must_use]
    pub fn effective_ignore_patterns(&self) -> &[crate::git_ignore::GitIgnorePattern] {
        self.ignore_patterns.as_deref().unwrap_or(&[])
    }
}

/// Validates `git.ignore_patterns` entries, as set via `gat config
/// git.ignore_patterns <pattern> [<pattern>...]` (or its deprecated
/// `git.exclude_patterns` alias) or hand-edited into `gat.yaml`: rejects
/// line breaks and negated (`!`-prefixed) patterns, since gat's derived
/// excludes only subtract paths from `git status`, never add them back.
/// Converts each raw
/// string into a validated `GitIgnorePattern` in the same pass, so a
/// caller that validates also ends up holding the typed values it needs
/// to store -- no separate re-parse.
pub fn validate_ignore_patterns(
    patterns: Vec<String>,
) -> std::result::Result<Vec<crate::git_ignore::GitIgnorePattern>, ConfigError> {
    Ok(patterns
        .into_iter()
        .map(crate::git_ignore::GitIgnorePattern::parse)
        .collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Named mounts (`mounts:` in `gat.yaml`), keyed by a stable mount
/// `NAME`. Each mount imports `gat.lock` rows from another Git repository
/// (`url`/`rev`/`rev_lock`) under an owned destination `target` in this
/// repository, optionally through a destination-local git `remote` and
/// filtered by `include`/`exclude` globs (matched relative to `path`). The
/// `target` a mount declares is what it owns in the working tree; mount
/// targets must not overlap.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct MountsConfig {
    #[serde(flatten)]
    pub by_name: BTreeMap<crate::name::MountName, MountConfig>,
}

/// The mount that owns a given path: its stable `name` (the `mounts:`
/// config key) and the `target` it owns. Returned by
/// [`MountsConfig::owner_of`] so ownership diagnostics can name both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountOwner<'a> {
    pub name: &'a crate::name::MountName,
    pub target: &'a crate::lexical_path::GatPath,
}

impl MountsConfig {
    fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// The mount that owns `path` (a `gat.lock`-style root-relative,
    /// `/`-separated path), if any -- i.e. `path` is equal to, or nested
    /// beneath, some mount's `target`. `None` means `path` is root-owned.
    ///
    /// This is a low-frequency configuration-inspection API. Runtime
    /// mutation and reconciliation paths compile mount ownership into
    /// `gat-engine`'s indexed path policy instead of scanning the configured
    /// map once per selected row.
    #[must_use]
    pub fn owner_of(&self, path: &crate::lexical_path::GatPath) -> Option<MountOwner<'_>> {
        self.by_name
            .iter()
            .find(|(_, src)| path.is_or_under(&src.target))
            .map(|(name, src)| MountOwner {
                name,
                target: &src.target,
            })
    }

    /// Layers `override_` over `self`: mounts in `override_` win on a
    /// name collision, same as [`RemotesConfig::merged_with`].
    fn merged_with(mut self, override_: Self) -> Self {
        self.by_name.extend(override_.by_name);
        self
    }

    /// Validates that `target` is a legal mount target given the mounts
    /// already configured (this should be called against the *effective*,
    /// fully-merged mount set -- global + project + local -- not a single
    /// config layer, so a conflict defined in any layer is still caught):
    /// not `.`, and not overlapping (equal to, containing, or contained
    /// within) any other existing mount's target. `exclude_name` excludes a
    /// mount from the check (its own current target, when validating a
    /// `gat mount update` target move).
    /// The typed [`crate::lexical_path::GatPath`] already excludes the root;
    /// root rejection happens when the path is constructed.
    pub fn check_target(
        &self,
        target: &crate::lexical_path::GatPath,
        exclude_name: Option<&crate::name::MountName>,
    ) -> std::result::Result<(), ConfigError> {
        for (other_name, other) in &self.by_name {
            if Some(other_name) == exclude_name {
                continue;
            }
            if target.is_or_under(&other.target) || other.target.is_or_under(target) {
                return Err(ConfigError::MountTargetOverlap {
                    name: exclude_name.map(ToString::to_string),
                    target: target.as_str().to_string(),
                    other_name: other_name.to_string(),
                    other_target: other.target.as_str().to_string(),
                });
            }
        }
        Ok(())
    }

    /// Validates the *whole* effective mount set (every configured mount,
    /// however it got here -- layered global/project/local, or hand-edited
    /// directly into one `gat.yaml`) fails closed on any equal, ancestor, or
    /// descendant target overlap, not only a single proposed mutation.
    /// [`Self::check_target`] validates one candidate target against every
    /// *other* mount; this reuses it for every mount against every other,
    /// so a conflict introduced entirely outside `gat mount add`/`update`
    /// (e.g. hand-authored config, or two layers each individually valid
    /// but jointly overlapping after merge) is still caught before any
    /// ownership-sensitive operation reads this config. Same-name
    /// collisions across layers are resolved by `Self::merged_with`
    /// before this ever runs, so this only ever sees one target per name.
    pub fn validate_effective(&self) -> std::result::Result<(), ConfigError> {
        for (name, mount) in &self.by_name {
            self.check_target(&mount.target, Some(name))?;
        }
        Ok(())
    }
}

/// A single named route: `routes.<NAME>` in `gat.yaml`. `path` is the
/// root-relative, `/`-separated tracked-path prefix this route applies to
/// (and everything beneath it, unless a more specific route overrides
/// it); `remote` is the named remote that serves it. The route's `NAME`
/// is a stable identity used for config layering, CLI management, and
/// diagnostics -- it never participates in runtime routing precedence,
/// which is decided purely by `path` specificity (see
/// [`RoutesConfig::route_for`]).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct RouteConfig {
    /// Root-relative, `/`-separated tracked-path prefix this route
    /// applies to.
    pub path: crate::lexical_path::GatPath,
    /// Named remote (`remotes:` in `gat.yaml`) that serves `path`.
    pub remote: crate::name::RemoteName,
}

/// Canonicalizes a hand-authored or CLI-supplied route path using the
/// same lexical, root-relative path rules already enforced for tracked
/// paths ([`crate::lexical_path::GatPath::normalize`]): rejects
/// absolute paths and `..` traversal, and collapses
/// `./`, redundant separators, and a trailing slash so two
/// differently-spelled paths that mean the same location compare equal.
/// There is no route-specific path semantics -- a route path is just a
/// tracked-path prefix.
pub fn normalize_route_path<P: AsRef<std::path::Path>>(
    path: P,
) -> std::result::Result<crate::lexical_path::GatPath, ConfigError> {
    crate::lexical_path::GatPath::normalize(&path).map_err(|source| ConfigError::InvalidPath {
        input: path.as_ref().to_string_lossy().into_owned(),
        source: Box::new(source),
    })
}

/// Path-based storage routing policy (`routes:` in `gat.yaml`), keyed by
/// stable route `NAME`: which named remote serves a given root-relative,
/// `/`-separated tracked path. Segment-aware and hierarchical -- both
/// mount targets and routes are path-prefix policies, but they answer
/// different questions (ownership vs. storage). Routes may nest inside,
/// cross, or ignore mount boundaries entirely; a route never grants
/// ownership over a mount-owned path, it only selects where that path's
/// object bytes live. Route names are used for config layering
/// (`merged_with`), CLI management (`gat route`), and diagnostics; they
/// do not affect which route wins for a given path -- effective route
/// *paths* must be unique (see [`Self::validate_effective`]), and among
/// matching routes the most-specific `path` always wins regardless of
/// name.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct RoutesConfig {
    #[serde(flatten)]
    pub by_name: BTreeMap<crate::name::RouteName, RouteConfig>,
}

/// The synthetic name `gat route list` uses for its default-remote
/// fallback row. Reserved at the config boundary itself -- rejected on
/// `gat route add`, on mount-generated route names, and here on config
/// load/validation -- so a real, user-defined or hand-authored route can
/// never collide with it and make `route list` ambiguous.
pub const RESERVED_DEFAULT_ROUTE_NAME: &str = "*";

impl RoutesConfig {
    fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Layers `override_` over `self`: routes in `override_` win on a
    /// same-`NAME` collision, same as [`MountsConfig::merged_with`].
    fn merged_with(mut self, override_: Self) -> Self {
        self.by_name.extend(override_.by_name);
        self
    }

    /// The most-specific (longest matching-prefix) route for `path` (a
    /// `gat.lock`-style root-relative, `/`-separated path), if any --
    /// i.e. among every configured route whose path is `path` itself, or
    /// an ancestor of it, the one with the most path segments wins. Route
    /// names never participate in this precedence; only `path`
    /// specificity does. Returns the matched route's name, its own path,
    /// and the remote name it selects. `None` means no configured route
    /// matches; callers fall back to `remotes.default`.
    ///
    /// This is a low-frequency configuration-inspection API used by
    /// configuration and presentation workflows. Runtime routing compiles
    /// routes into `gat-engine`'s indexed path policy and retains compact
    /// route/remote IDs through transfer and reconciliation hot paths.
    #[must_use]
    pub fn route_for<'a>(&'a self, path: &crate::lexical_path::GatPath) -> Option<RouteMatch<'a>> {
        self.by_name
            .iter()
            .filter(|(_, route)| path.is_or_under(&route.path))
            .max_by_key(|(_, route)| route.path.as_str().matches('/').count())
            .map(|(name, route)| RouteMatch {
                name,
                route: &route.path,
                remote: &route.remote,
            })
    }

    /// Validates the *whole* effective route set (however it got here --
    /// layered global/project/local, or hand-edited directly into one
    /// `gat.yaml`) fails closed when two *distinct* route names share the
    /// exact same normalized `path` -- an ambiguous, unresolvable
    /// configuration, since neither route is more specific than the
    /// other. Nested (ancestor/descendant) paths across different names
    /// remain valid: the deepest-matching path simply wins at resolution
    /// time (see [`Self::route_for`]). Same-name collisions across layers
    /// are resolved by `Self::merged_with` before this ever runs, so
    /// this only ever sees one path per name.
    pub fn validate_effective(&self) -> std::result::Result<(), ConfigError> {
        if self.by_name.contains_key(RESERVED_DEFAULT_ROUTE_NAME) {
            return Err(ConfigError::ReservedRouteName);
        }
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, route) in &self.by_name {
            if let Some(other_name) = seen.insert(route.path.as_str(), name.as_str()) {
                return Err(ConfigError::RouteConflict {
                    first_name: other_name.to_string(),
                    second_name: name.to_string(),
                    path: route.path.as_str().to_string(),
                });
            }
        }
        Ok(())
    }
}

/// A route lookup match: the configured route's name, the path that
/// matched, and the remote name it selects. Returned by
/// [`RoutesConfig::route_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteMatch<'a> {
    pub name: &'a crate::name::RouteName,
    pub route: &'a crate::lexical_path::GatPath,
    pub remote: &'a crate::name::RemoteName,
}

/// A single named mount: `mounts.<NAME>` in `gat.yaml`. Owns every
/// Gat-managed path beneath its `target`; mount targets must not overlap.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MountConfig {
    /// Upstream Git repository this mount pulls from (a local path or a
    /// remote URL).
    pub url: crate::git_location::GitLocationSpec,
    /// Destination target this mount owns in this repository. Every
    /// Gat-managed path beneath it belongs to this mount. Cannot be `.`,
    /// and cannot overlap another mount's target.
    pub target: crate::lexical_path::GatPath,
    /// Path within the upstream repository this mount pulls from --
    /// `include`/`exclude` are matched against paths relative to this,
    /// and `gat.lock` rows are copied from beneath it (then reparented
    /// under `target`). `"."` (the default) means the upstream root.
    #[serde(
        default,
        skip_serializing_if = "crate::lexical_path::GatSubpath::is_root"
    )]
    pub path: crate::lexical_path::GatSubpath,
    /// Git ref (branch, tag, or commit) to track. Unset means the other
    /// repository's default branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<crate::git::GitRevisionSpec>,
    /// Exact commit `rev` resolved to at `gat mount add` time (via the
    /// other repository's own git history, using `gix`), pinning the
    /// mount to a specific commit until it's refreshed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev_lock: Option<crate::git::GitCommitId>,
    /// Glob patterns selecting which paths this mount tracks (matched
    /// relative to `path`). Component-aware: `*.bin` matches only
    /// immediate children while `**/*.bin` matches recursively. Empty
    /// means everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<crate::globs::GatGlobPattern>,
    /// Glob patterns excluded from `include` (or from everything, if
    /// `include` is empty), matched relative to `path` with the same
    /// component-aware semantics as `include`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<crate::globs::GatGlobPattern>,
}

impl Config {
    /// Layers `override_` over `self`: for each nested section, uses that
    /// section's own rules (whole named definitions or
    /// individual scalar settings -- see [`crate::settings::SettingsLayer`],
    /// [`RemotesConfig::merged_with`], etc.). Document metadata is absent
    /// from this semantic operation.
    fn merged_with(mut self, mut override_: Self) -> Self {
        crate::settings::SettingsLayer::merge_config(&mut self, &mut override_);
        self.remotes = self.remotes.merged_with(override_.remotes);
        self.selections = self.selections.merged_with(override_.selections);
        self.mounts = self.mounts.merged_with(override_.mounts);
        self.routes = self.routes.merged_with(override_.routes);
        self
    }

    /// Folds `layers` (lowest precedence first) into one effective
    /// `Config`, each subsequent layer overriding the ones before it. This
    /// is how `gat.yaml`'s three possible locations -- global, project,
    /// local -- combine: the root `gat` crate's config loader calls this
    /// with `[global, project, local]` so local wins over project wins
    /// over global. Named definitions replace as a
    /// whole; other settings inherit field by field.
    pub fn merge_layers(layers: impl IntoIterator<Item = Self>) -> Self {
        layers.into_iter().fold(Self::default(), Self::merged_with)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::name::{MountName, RemoteName, RouteName};

    fn gp(path: &str) -> crate::lexical_path::GatPath {
        crate::lexical_path::GatPath::parse_canonical(path).unwrap()
    }

    fn cfg(mutate: impl FnOnce(&mut Config)) -> Config {
        let mut cfg = Config::default();
        mutate(&mut cfg);
        cfg
    }

    #[test]
    fn saved_selection_extent_agrees_with_matching_after_config_roundtrip() {
        for (yaml, unrestricted) in [
            ("{}", true),
            ("path: .", true),
            ("include: []\nexclude: []", true),
            ("path: models", false),
            ("include: ['*.bin']", false),
            ("exclude: ['**']", false),
        ] {
            let definition: SelectionConfig = yaml_serde::from_str(yaml).unwrap();
            let encoded = yaml_serde::to_string(&definition).unwrap();
            let decoded: SelectionConfig = yaml_serde::from_str(&encoded).unwrap();
            assert_eq!(decoded.is_unrestricted(), unrestricted, "{yaml}");
            let selection = crate::selection::Selection::from(decoded);
            assert_eq!(selection.is_unrestricted(), unrestricted, "{yaml}");
            if unrestricted {
                assert!(selection.matches_str("root.bin"));
                assert!(selection.matches_str("models/nested/data.bin"));
            }
        }
    }

    #[test]
    fn selections_merge_whole_definitions_and_independent_default_pointers() {
        let parse = |yaml: &str| yaml_serde::from_str::<Config>(yaml).unwrap();
        let project = parse(
            "selections:\n  default: runtime\n  runtime:\n    path: models\n    include: ['**/*.onnx']\n    exclude: ['experimental/**']\n  training:\n    path: datasets\n    include: ['**/*.parquet']\n",
        );
        let local = parse("selections:\n  default: training\n");
        let global =
            parse("selections:\n  default: runtime\n  runtime:\n    exclude: ['**/scratch/**']\n");
        let merged = Config::merge_layers([global, project.clone(), local]);
        assert_eq!(merged.selections.default.as_ref().unwrap(), "training");
        assert_eq!(merged.selections.by_name, project.selections.by_name);
        let replacement = parse("selections:\n  runtime: {}\n");
        let merged = Config::merge_layers([project.clone(), replacement]);
        assert_eq!(
            merged.selections.by_name["runtime"],
            SelectionConfig::default()
        );
        assert_eq!(merged.selections.default, project.selections.default);
        assert_eq!(merged.selections.by_name.len(), 2);
        assert!(merged.selections.validate_effective().is_ok());
        let dangling = parse("selections:\n  default: missing\n");
        assert!(dangling.selections.validate_effective().is_err());
    }

    #[test]
    fn named_remote_and_route_definitions_replace_across_three_layers() {
        let layer = |name: &str| {
            let mut config = Config::default();
            config.remotes.by_name.insert(
                RemoteName::from_string("origin".into()),
                RemoteConfig::from(format!("file:///{name}")),
            );
            config.routes.by_name.insert(
                RouteName::from_string("assets".into()),
                make_route(name, name),
            );
            config
        };
        let global = layer("global");
        let project = layer("project");
        let mut local = layer("local");
        local.routes.by_name.insert(
            RouteName::from_string("other".into()),
            make_route("other", "local"),
        );
        let merged = Config::merge_layers([global.clone(), project.clone(), local.clone()]);
        assert_eq!(merged.remotes, local.remotes);
        assert_eq!(merged.routes, local.routes);
        let revealed = Config::merge_layers([global, project.clone(), Config::default()]);
        assert_eq!(revealed.remotes, project.remotes);
        assert_eq!(revealed.routes, project.routes);
    }

    #[test]
    fn removed_sync_filters_and_invalid_selection_patterns_are_rejected() {
        for yaml in [
            "sync:\n  include: ['models/**']\n",
            "sync:\n  exclude: ['models/**']\n",
            "selections:\n  runtime:\n    extends: training\n",
            "selections:\n  runtime:\n    path: ../escape\n",
            "selections:\n  runtime:\n    path: /absolute\n",
            "selections:\n  runtime:\n    include: ['[']\n",
            "selections:\n  runtime:\n    exclude: ['../escape']\n",
        ] {
            assert!(yaml_serde::from_str::<Config>(yaml).is_err(), "{yaml}");
        }
    }

    #[test]
    fn materialization_mode_from_str_reports_invalid_link_mode_with_the_invalid_value() {
        let err = "bogus".parse::<MaterializationMode>().unwrap_err();
        let ConfigError::InvalidLinkMode { value } = err else {
            panic!("expected ConfigError::InvalidLinkMode");
        };
        assert_eq!(value, "bogus");
    }

    #[test]
    fn typed_materialization_strategies_validate_and_preserve_fallback_order() {
        use MaterializationMode::{Copy, Hardlink, Reflink};
        assert!(matches!(
            MaterializationStrategy::try_from(Vec::new()),
            Err(ConfigError::EmptyLinkModeList),
        ));
        assert!(matches!(
            MaterializationStrategy::try_from(vec![Copy, Reflink, Copy]),
            Err(ConfigError::DuplicateLinkMode { .. }),
        ));
        let strategy = MaterializationStrategy::try_from(vec![Reflink, Copy, Hardlink]).unwrap();
        assert_eq!(strategy.modes(), &[Reflink, Copy, Hardlink]);
        assert_eq!(
            MaterializationStrategy::from(Copy),
            MaterializationStrategy::default()
        );
    }

    #[test]
    fn materialization_strategy_from_values_rejects_an_empty_list() {
        let err = MaterializationStrategy::from_values::<&str>(&[]).unwrap_err();
        assert!(matches!(err, ConfigError::EmptyLinkModeList));
    }

    #[test]
    fn materialization_strategy_from_values_rejects_a_duplicate_mode() {
        let err = MaterializationStrategy::from_values(&["copy", "copy"]).unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateLinkMode { mode } if mode == "copy"));
    }

    #[test]
    fn ingest_strategy_names_agree_with_parsing_and_persisted_values() {
        for strategy in IngestStrategy::ALL {
            let name = strategy.as_str();
            assert_eq!(name.parse::<IngestStrategy>().unwrap(), strategy);
            assert_eq!(
                yaml_serde::from_str::<IngestStrategy>(name).unwrap(),
                strategy
            );
            assert_eq!(yaml_serde::to_string(&strategy).unwrap().trim(), name);
        }
    }

    #[test]
    fn ingest_strategy_from_str_reports_invalid_ingest_strategy_with_the_invalid_value() {
        let err = "bogus".parse::<IngestStrategy>().unwrap_err();
        let ConfigError::InvalidIngestStrategy { value } = err else {
            panic!("expected ConfigError::InvalidIngestStrategy");
        };
        assert_eq!(value, "bogus");
    }

    #[test]
    fn parse_shard_levels_rejects_a_non_numeric_value() {
        let err = parse_shard_levels("not-a-number").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidShardLevels { .. }));
    }

    #[test]
    fn parse_shard_levels_rejects_a_value_above_the_maximum() {
        let too_many = (u16::from(MAX_SHARD_LEVELS) + 1).to_string();
        let err = parse_shard_levels(&too_many).unwrap_err();
        assert!(matches!(err, ConfigError::ShardLevelsExceedsMaximum { .. }));
    }

    #[test]
    fn parse_sync_bool_setting_accepts_true_and_false() {
        assert!(parse_sync_bool_setting("sync.trust_state", "true").unwrap());
        assert!(!parse_sync_bool_setting("sync.trust_state", "false").unwrap());
    }

    #[test]
    fn parse_sync_bool_setting_rejects_anything_else() {
        let err = parse_sync_bool_setting("sync.auto_fetch", "yes").unwrap_err();
        match err {
            ConfigError::InvalidBooleanValue { field, value } => {
                assert_eq!(field, "sync.auto_fetch");
                assert_eq!(value, "yes");
            }
            other => panic!("expected InvalidBooleanValue, got {other:?}"),
        }
    }

    #[test]
    fn validate_ignore_patterns_reports_the_negated_pattern() {
        let err = validate_ignore_patterns(vec!["!keep.txt".to_string()]).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidGitIgnorePattern { pattern } if pattern == "!keep.txt"
        ));
    }

    #[test]
    fn check_target_reports_which_two_mounts_overlap() {
        let mut mounts = MountsConfig::default();
        mounts.by_name.insert(
            MountName::from_string("models".to_string()),
            MountConfig {
                url: "../models".to_string().into(),
                target: gp("vendor/models"),
                path: crate::lexical_path::GatSubpath::Root,
                rev: None,
                rev_lock: None,
                include: Vec::new(),
                exclude: Vec::new(),
            },
        );
        let err = mounts.check_target(&gp("vendor"), None).unwrap_err();
        let ConfigError::MountTargetOverlap {
            other_name,
            other_target,
            ..
        } = err
        else {
            panic!("expected ConfigError::MountTargetOverlap");
        };
        assert_eq!(other_name, "models");
        assert_eq!(other_target, "vendor/models");
    }

    #[test]
    fn routes_validate_effective_rejects_the_reserved_default_route_name() {
        let mut routes = RoutesConfig::default();
        routes.by_name.insert(
            RouteName::from_string(RESERVED_DEFAULT_ROUTE_NAME.to_string()),
            make_route("data", "origin"),
        );
        let err = routes.validate_effective().unwrap_err();
        assert!(matches!(err, ConfigError::ReservedRouteName));
    }

    #[test]
    fn routes_validate_effective_rejects_two_routes_with_the_same_path() {
        let mut routes = RoutesConfig::default();
        routes.by_name.insert(
            RouteName::from_string("a".to_string()),
            make_route("data", "origin"),
        );
        routes.by_name.insert(
            RouteName::from_string("b".to_string()),
            make_route("data", "backup"),
        );
        let err = routes.validate_effective().unwrap_err();
        assert!(matches!(err, ConfigError::RouteConflict { .. }));
    }

    #[test]
    fn owned_merge_moves_list_storage_and_preserves_explicit_empty_overrides() {
        let patterns = validate_ignore_patterns(vec!["*.bin".into(), "/models/".into()]).unwrap();
        let allocation = patterns.as_ptr();
        let layer = Config {
            git: GitConfig {
                ignore_patterns: Some(patterns),
            },
            ..Default::default()
        };
        let merged = Config::merge_layers([layer, Config::default()]);
        assert_eq!(
            merged.git.ignore_patterns.as_ref().unwrap().as_ptr(),
            allocation
        );
        assert_eq!(merged.git.effective_ignore_patterns().len(), 2);
        let empty = Config {
            git: GitConfig {
                ignore_patterns: Some(Vec::new()),
            },
            ..Default::default()
        };
        assert_eq!(
            Config::merge_layers([merged, empty]).git.ignore_patterns,
            Some(Vec::new())
        );
    }

    #[test]
    fn merge_layers_of_no_layers_is_the_default_config() {
        assert_eq!(Config::merge_layers([]), Config::default());
    }

    #[test]
    fn merge_layers_local_wins_over_project_wins_over_global_per_field() {
        let global = cfg(|c| {
            c.cache.location = Some(
                crate::cache_location::CacheLocation::try_from_path("global-cache".into())
                    .expect("nonempty cache location"),
            );
            c.selections
                .by_name
                .entry("runtime".into())
                .or_default()
                .include = Some(vec![
                crate::globs::GatGlobPattern::parse("global-target").unwrap(),
            ]);
        });
        let project = cfg(|c| {
            c.cache.location = Some(
                crate::cache_location::CacheLocation::try_from_path("project-cache".into())
                    .expect("nonempty cache location"),
            );
        });
        let local = cfg(|c| {
            c.selections
                .by_name
                .entry("runtime".into())
                .or_default()
                .include = Some(vec![
                crate::globs::GatGlobPattern::parse("local-target").unwrap(),
            ]);
        });

        let merged = Config::merge_layers([global, project, local]);

        // project overrides global's `cache.location`...
        assert_eq!(
            merged.cache.location,
            Some(
                crate::cache_location::CacheLocation::try_from_path("project-cache".into())
                    .expect("nonempty cache location")
            )
        );
        // ...and local replaces the global selection; the absent project
        // selection does not affect inheritance.
        assert_eq!(
            merged.selections.by_name["runtime"].include,
            Some(vec![
                crate::globs::GatGlobPattern::parse("local-target").unwrap()
            ])
        );
    }

    #[test]
    fn config_scope_documentation_is_complete_unique_and_matches_merge_order() {
        let mut scopes = ConfigScope::ALL;
        scopes.sort_by_key(|scope| scope.precedence());
        assert_eq!(
            scopes,
            [
                ConfigScope::Global,
                ConfigScope::Project,
                ConfigScope::Local
            ]
        );
        let precedences = scopes
            .iter()
            .map(|scope| scope.precedence())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(precedences.len(), scopes.len());
        let names = scopes
            .iter()
            .map(|scope| scope.documentation().name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), scopes.len());
        assert!(ConfigScope::Project.documentation().committed);
        assert!(!ConfigScope::Global.documentation().committed);
        assert!(!ConfigScope::Local.documentation().committed);
    }

    #[test]
    fn merge_layers_unset_fields_in_higher_layers_do_not_erase_lower_ones() {
        let global = cfg(|c| {
            c.cache.location = Some(
                crate::cache_location::CacheLocation::try_from_path("global-cache".into())
                    .expect("nonempty cache location"),
            );
        });
        let project = Config::default();
        let local = Config::default();

        let merged = Config::merge_layers([global, project, local]);
        assert_eq!(
            merged.cache.location,
            Some(
                crate::cache_location::CacheLocation::try_from_path("global-cache".into())
                    .expect("nonempty cache location")
            )
        );
    }

    #[test]
    fn cache_materialization_strategy_is_unset_by_default_and_resolves_to_copy() {
        assert_eq!(Config::default().cache.materialization_strategy, None);
        assert_eq!(
            MaterializationStrategy::default().modes(),
            &[MaterializationMode::Copy]
        );
    }

    #[test]
    fn cache_materialization_strategy_local_wins_over_project_wins_over_global() {
        let global = cfg(|c| {
            c.cache.materialization_strategy = Some("copy".parse().unwrap());
        });
        let project = cfg(|c| {
            c.cache.materialization_strategy = Some("hardlink".parse().unwrap());
        });
        let local = cfg(|c| {
            c.cache.materialization_strategy = Some(
                MaterializationStrategy::validate(vec![
                    MaterializationMode::Symlink,
                    MaterializationMode::Copy,
                ])
                .unwrap(),
            );
        });

        let merged = Config::merge_layers([global, project, local]);

        assert_eq!(
            merged.cache.materialization_strategy.unwrap().modes(),
            &[MaterializationMode::Symlink, MaterializationMode::Copy]
        );
    }

    #[test]
    fn cache_materialization_strategy_unset_in_higher_layers_falls_back_to_a_lower_one() {
        let global = cfg(|c| {
            c.cache.materialization_strategy = Some("reflink".parse().unwrap());
        });
        let project = Config::default();
        let local = Config::default();

        let merged = Config::merge_layers([global, project, local]);

        assert_eq!(
            merged.cache.materialization_strategy.unwrap().modes(),
            &[MaterializationMode::Reflink]
        );
    }

    #[test]
    fn cache_materialization_strategy_parses_a_yaml_sequence_in_order() {
        let cache: CacheConfig =
            yaml_serde::from_str("materialization_strategy: [reflink, hardlink, copy]\n").unwrap();
        assert_eq!(
            cache.materialization_strategy.unwrap().modes(),
            &[
                MaterializationMode::Reflink,
                MaterializationMode::Hardlink,
                MaterializationMode::Copy,
            ]
        );
    }

    #[test]
    fn cache_materialization_strategy_yaml_round_trips_through_load_and_save() {
        let cache = CacheConfig {
            materialization_strategy: Some(
                MaterializationStrategy::validate(vec![
                    MaterializationMode::Symlink,
                    MaterializationMode::Copy,
                ])
                .unwrap(),
            ),
            ..Default::default()
        };
        let yaml = yaml_serde::to_string(&cache).unwrap();
        assert!(
            yaml.contains("materialization_strategy:"),
            "expected the materialization_strategy key in: {yaml}"
        );
        assert!(
            !yaml.contains("link:") && !yaml.contains("\nlink"),
            "cache.link must never be emitted: {yaml}"
        );
        let round_tripped: CacheConfig = yaml_serde::from_str(&yaml).unwrap();
        assert_eq!(
            round_tripped.materialization_strategy,
            cache.materialization_strategy
        );
    }

    #[test]
    fn cache_materialization_strategy_rejects_a_duplicate_mode() {
        assert!(
            MaterializationStrategy::validate(vec![
                MaterializationMode::Copy,
                MaterializationMode::Copy,
            ])
            .is_err()
        );
    }

    #[test]
    fn cache_materialization_strategy_rejects_an_empty_list() {
        assert!(MaterializationStrategy::validate(vec![]).is_err());
    }

    #[test]
    fn remotes_config_merge_extends_by_name_and_overrides_on_collision() {
        let global = RemotesConfig {
            default: Some(RemoteName::from_string("origin".to_string())),
            by_name: BTreeMap::from([(
                RemoteName::from_string("origin".to_string()),
                "s3://global/bucket".to_string().into(),
            )]),
        };
        let local = RemotesConfig {
            default: None,
            by_name: BTreeMap::from([
                (
                    RemoteName::from_string("origin".to_string()),
                    "s3://local/bucket".to_string().into(),
                ),
                (
                    RemoteName::from_string("backup".to_string()),
                    "s3://local/backup".to_string().into(),
                ),
            ]),
        };

        let merged = global.merged_with(local);

        // local's `origin` wins over global's...
        assert_eq!(
            merged.by_name["origin"].url.as_template_str(),
            "s3://local/bucket"
        );
        // ...but global's entries not present in local survive, and...
        assert_eq!(
            merged.by_name["backup"].url.as_template_str(),
            "s3://local/backup"
        );
        // ...an unset `default` in the override falls back to the base.
        assert_eq!(
            merged.default,
            Some(RemoteName::from_string("origin".to_string()))
        );
    }

    #[test]
    fn mounts_config_merge_extends_by_name_and_overrides_on_collision() {
        let make_mount = |url: &str, target: &str| MountConfig {
            url: url.to_string().into(),
            target: gp(target),
            path: crate::lexical_path::GatSubpath::Root,
            rev: None,
            rev_lock: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let global = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("models".to_string()),
                make_mount("../global-models", "vendor/models"),
            )]),
        };
        let local = MountsConfig {
            by_name: BTreeMap::from([
                (
                    MountName::from_string("models".to_string()),
                    make_mount("../local-models", "vendor/models"),
                ),
                (
                    MountName::from_string("assets".to_string()),
                    make_mount("../local-assets", "vendor/assets"),
                ),
            ]),
        };

        let merged = global.merged_with(local);
        assert_eq!(
            merged.by_name["models"].url.as_location_str(),
            "../local-models"
        );
        assert_eq!(
            merged.by_name["assets"].url.as_location_str(),
            "../local-assets"
        );
    }

    /// `validate_effective` must fail closed on any equal,
    /// ancestor, or descendant target overlap in the *merged* mount set,
    /// even when neither individual layer is internally overlapping.
    #[test]
    fn validate_effective_rejects_overlap_introduced_only_after_merging_layers() {
        let make_mount = |url: &str, target: &str| MountConfig {
            url: url.to_string().into(),
            target: gp(target),
            path: crate::lexical_path::GatSubpath::Root,
            rev: None,
            rev_lock: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let global = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("models".to_string()),
                make_mount("../global-models", "vendor/models"),
            )]),
        };
        let local = MountsConfig {
            by_name: BTreeMap::from([(
                // A different mount *name*, but a target that overlaps
                // (is an ancestor of) `models`'s target -- invalid only
                // once merged, since each layer alone only has one mount.
                MountName::from_string("assets".to_string()),
                make_mount("../local-assets", "vendor"),
            )]),
        };
        let merged = global.merged_with(local);
        assert!(merged.validate_effective().is_err());
    }

    #[test]
    fn validate_effective_accepts_a_non_overlapping_merged_set() {
        let make_mount = |url: &str, target: &str| MountConfig {
            url: url.to_string().into(),
            target: gp(target),
            path: crate::lexical_path::GatSubpath::Root,
            rev: None,
            rev_lock: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let global = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("models".to_string()),
                make_mount("../global-models", "vendor/models"),
            )]),
        };
        let local = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("assets".to_string()),
                make_mount("../local-assets", "vendor/assets"),
            )]),
        };
        let merged = global.merged_with(local);
        assert!(merged.validate_effective().is_ok());
    }

    #[test]
    fn validate_effective_rejects_an_equal_target_collision() {
        let make_mount = |url: &str, target: &str| MountConfig {
            url: url.to_string().into(),
            target: gp(target),
            path: crate::lexical_path::GatSubpath::Root,
            rev: None,
            rev_lock: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let mounts = MountsConfig {
            by_name: BTreeMap::from([
                (
                    MountName::from_string("a".to_string()),
                    make_mount("../a", "vendor/models"),
                ),
                (
                    MountName::from_string("b".to_string()),
                    make_mount("../b", "vendor/models"),
                ),
            ]),
        };
        let err = mounts.validate_effective().unwrap_err();
        assert!(format!("{err:#}").contains("vendor/models"));
    }

    /// Equal target collision across layers, not just
    /// within one already-merged map -- global and local each define one
    /// mount alone, but their *merged* targets are identical.
    #[test]
    fn validate_effective_rejects_an_equal_target_collision_across_layers() {
        let make_mount = |url: &str, target: &str| MountConfig {
            url: url.to_string().into(),
            target: gp(target),
            path: crate::lexical_path::GatSubpath::Root,
            rev: None,
            rev_lock: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let global = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("models".to_string()),
                make_mount("../global-models", "vendor/models"),
            )]),
        };
        let local = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("assets".to_string()),
                make_mount("../local-assets", "vendor/models"),
            )]),
        };
        let merged = global.merged_with(local);
        let err = merged.validate_effective().unwrap_err();
        assert!(format!("{err:#}").contains("vendor/models"));
    }

    /// The reverse direction of
    /// `validate_effective_rejects_overlap_introduced_only_after_merging_layers`
    /// -- a *descendant* conflict across layers, i.e. the more specific
    /// layer defines the ancestor target and the less specific layer
    /// defines the nested descendant target.
    #[test]
    fn validate_effective_rejects_a_descendant_conflict_across_layers() {
        let make_mount = |url: &str, target: &str| MountConfig {
            url: url.to_string().into(),
            target: gp(target),
            path: crate::lexical_path::GatSubpath::Root,
            rev: None,
            rev_lock: None,
            include: Vec::new(),
            exclude: Vec::new(),
        };
        let global = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("assets".to_string()),
                make_mount("../global-assets", "vendor"),
            )]),
        };
        let local = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("models".to_string()),
                make_mount("../local-models", "vendor/models"),
            )]),
        };
        let merged = global.merged_with(local);
        assert!(merged.validate_effective().is_err());
    }

    #[test]
    fn owner_of_reports_mount_name_and_target() {
        let mounts = MountsConfig {
            by_name: BTreeMap::from([(
                MountName::from_string("pytorch".to_string()),
                MountConfig {
                    url: "../models".to_string().into(),
                    target: gp("vendor/pytorch"),
                    path: crate::lexical_path::GatSubpath::Root,
                    rev: None,
                    rev_lock: None,
                    include: Vec::new(),
                    exclude: Vec::new(),
                },
            )]),
        };
        let owner = mounts.owner_of(&gp("vendor/pytorch/model.bin")).unwrap();
        assert_eq!(owner.name, "pytorch");
        assert_eq!(owner.target, "vendor/pytorch");
        assert_eq!(mounts.owner_of(&gp("vendor/pytorch")), Some(owner));
        assert_eq!(mounts.owner_of(&gp("other/file.bin")), None);
        // Segment-aware: `vendor/pytorch2` is not owned by `vendor/pytorch`.
        assert_eq!(mounts.owner_of(&gp("vendor/pytorch2/x.bin")), None);
    }

    #[test]
    fn sync_config_merge_overrides_options_and_explicit_bool_wins() {
        let global = SyncConfig {
            trust_state: Some(true),
            auto_fetch: Some(true),
            auto_repair: Some(false),
        };
        let local = SyncConfig {
            trust_state: None,
            auto_fetch: Some(false),
            auto_repair: Some(true),
        };
        let merged = Config::merge_layers([
            Config {
                sync: global,
                ..Config::default()
            },
            Config {
                sync: local,
                ..Config::default()
            },
        ])
        .sync;
        assert_eq!(merged.trust_state, Some(true));
        assert!(!merged.auto_fetch());
        assert!(merged.auto_repair());
    }

    /// Merge table covering every combination of `unset`/`true`/`false`
    /// across global/project/local layers for `sync.auto_fetch`, asserting
    /// the documented precedence: local explicit value > project explicit
    /// value > global explicit value > built-in default (`false`).
    #[test]
    fn sync_config_auto_fetch_merge_table_covers_unset_true_false_layers() {
        fn sync_with_auto_fetch(auto_fetch: Option<bool>) -> SyncConfig {
            SyncConfig {
                auto_fetch,
                ..SyncConfig::default()
            }
        }

        type Layers = (Option<bool>, Option<bool>, Option<bool>, bool);
        let cases: &[Layers] = &[
            // (global, project, local, expected effective value)
            (None, None, None, false),
            (Some(true), None, None, true),
            (Some(false), None, None, false),
            (None, Some(true), None, true),
            (None, Some(false), None, false),
            (None, None, Some(true), true),
            (None, None, Some(false), false),
            // higher-priority explicit `false` overrides lower `true`.
            (Some(true), Some(false), None, false),
            (Some(true), None, Some(false), false),
            (None, Some(true), Some(false), false),
            (Some(true), Some(true), Some(false), false),
            // higher-priority explicit `true` overrides lower `false`.
            (Some(false), Some(true), None, true),
            (Some(false), None, Some(true), true),
            (None, Some(false), Some(true), true),
            (Some(false), Some(false), Some(true), true),
            // an omitted setting inherits from the next lower layer.
            (Some(true), None, None, true),
            (Some(false), None, None, false),
        ];

        for &(global, project, local, expected) in cases {
            let merged = Config::merge_layers([
                Config {
                    sync: sync_with_auto_fetch(global),
                    ..Config::default()
                },
                Config {
                    sync: sync_with_auto_fetch(project),
                    ..Config::default()
                },
                Config {
                    sync: sync_with_auto_fetch(local),
                    ..Config::default()
                },
            ]);
            assert_eq!(
                merged.sync.auto_fetch(),
                expected,
                "global={global:?} project={project:?} local={local:?}"
            );
        }
    }

    #[test]
    fn lock_config_merge_override_wins_when_set() {
        let global = LockConfig {
            shard_levels: Some(crate::lock::LockShardLevels::new(1).unwrap()),
        };
        let local = LockConfig {
            shard_levels: Some(crate::lock::LockShardLevels::new(2).unwrap()),
        };
        assert_eq!(
            Config::merge_layers([
                Config {
                    lock: global,
                    ..Config::default()
                },
                Config {
                    lock: local,
                    ..Config::default()
                }
            ])
            .lock
            .shard_levels,
            Some(crate::lock::LockShardLevels::new(2).unwrap())
        );

        let global = LockConfig {
            shard_levels: Some(crate::lock::LockShardLevels::new(1).unwrap()),
        };
        let local = LockConfig { shard_levels: None };
        assert_eq!(
            Config::merge_layers([
                Config {
                    lock: global,
                    ..Config::default()
                },
                Config {
                    lock: local,
                    ..Config::default()
                }
            ])
            .lock
            .shard_levels,
            Some(crate::lock::LockShardLevels::new(1).unwrap())
        );
    }

    #[test]
    fn git_config_deserializes_the_canonical_ignore_patterns_key() {
        let cfg: Config =
            yaml_serde::from_str("version: 1\ngit:\n  ignore_patterns:\n    - \"/data/\"\n")
                .unwrap();
        assert_eq!(
            cfg.git.ignore_patterns,
            Some(vec![
                crate::git_ignore::GitIgnorePattern::parse("/data/").unwrap()
            ])
        );
    }

    /// The deprecated `exclude_patterns` spelling must load and behave
    /// identically to the canonical spelling.
    #[test]
    fn git_config_deserializes_the_deprecated_exclude_patterns_alias() {
        let cfg: Config =
            yaml_serde::from_str("version: 1\ngit:\n  exclude_patterns:\n    - \"/data/\"\n")
                .unwrap();
        assert_eq!(
            cfg.git.ignore_patterns,
            Some(vec![
                crate::git_ignore::GitIgnorePattern::parse("/data/").unwrap()
            ])
        );
    }

    /// Hand-editing `gat.yaml` to set both the canonical key and the
    /// deprecated alias is an actionable conflict, not a silent pick of
    /// one over the other.
    #[test]
    fn git_config_rejects_both_canonical_and_deprecated_key_present() {
        let err = yaml_serde::from_str::<Config>(
            "version: 1\ngit:\n  ignore_patterns:\n    - \"/data/\"\n  exclude_patterns:\n    - \"/other/\"\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("git.ignore_patterns"), "{message}");
        assert!(message.contains("git.exclude_patterns"), "{message}");
    }

    /// Both keys being *present* is the conflict, independent of whether
    /// either value is empty: `ignore_patterns: []` alongside a non-empty
    /// `exclude_patterns` must still be rejected rather than silently
    /// falling back to the alias.
    #[test]
    fn git_config_rejects_both_keys_present_with_empty_canonical_and_non_empty_alias() {
        let err = yaml_serde::from_str::<Config>(
            "version: 1\ngit:\n  ignore_patterns: []\n  exclude_patterns:\n    - \"/other/\"\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("git.ignore_patterns"), "{message}");
        assert!(message.contains("git.exclude_patterns"), "{message}");
    }

    /// Same conflict, but with the empty list on the deprecated alias
    /// instead: presence still wins over emptiness.
    #[test]
    fn git_config_rejects_both_keys_present_with_non_empty_canonical_and_empty_alias() {
        let err = yaml_serde::from_str::<Config>(
            "version: 1\ngit:\n  ignore_patterns:\n    - \"/data/\"\n  exclude_patterns: []\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("git.ignore_patterns"), "{message}");
        assert!(message.contains("git.exclude_patterns"), "{message}");
    }

    /// Both keys present but both empty is still a conflict: presence, not
    /// content, is what's rejected.
    #[test]
    fn git_config_rejects_both_keys_present_when_both_are_empty() {
        let err = yaml_serde::from_str::<Config>(
            "version: 1\ngit:\n  ignore_patterns: []\n  exclude_patterns: []\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("git.ignore_patterns"), "{message}");
        assert!(message.contains("git.exclude_patterns"), "{message}");
    }

    /// Only the canonical key present, set to an empty list, must not be
    /// mistaken for "unset" and must not conflict with anything.
    #[test]
    fn git_config_accepts_only_canonical_key_present_and_empty() {
        let cfg: Config =
            yaml_serde::from_str("version: 1\ngit:\n  ignore_patterns: []\n").unwrap();
        assert_eq!(cfg.git.ignore_patterns, Some(Vec::new()));
    }

    /// An explicitly-present `null` value is a type error, not "absent":
    /// a plain `Option<Vec<String>>` field would otherwise treat
    /// `ignore_patterns: null` the same as the key being missing
    /// entirely, which both hides a malformed `gat.yaml` and would let it
    /// evade the both-keys-present conflict check below.
    #[test]
    fn git_config_rejects_explicit_null_canonical_key_as_a_type_error() {
        let err = yaml_serde::from_str::<Config>("version: 1\ngit:\n  ignore_patterns: null\n")
            .unwrap_err();
        // Not the "both keys present" conflict message -- a plain type
        // mismatch from serde, proving `null` was actually deserialized
        // as `Vec<String>` rather than special-cased into `None`.
        assert!(!err.to_string().contains("both"), "{err}");
    }

    #[test]
    fn git_config_rejects_explicit_null_alias_key_as_a_type_error() {
        let err = yaml_serde::from_str::<Config>("version: 1\ngit:\n  exclude_patterns: null\n")
            .unwrap_err();
        assert!(!err.to_string().contains("both"), "{err}");
    }

    /// `ignore_patterns: null` together with a present `exclude_patterns`
    /// must not be silently accepted by preferring the alias: an explicit
    /// `null` is presence, not absence, and (since it's rejected as a
    /// type error before the conflict check can even run) the whole
    /// document must fail to parse either way.
    #[test]
    fn git_config_rejects_explicit_null_canonical_alongside_present_alias() {
        assert!(yaml_serde::from_str::<Config>(
            "version: 1\ngit:\n  ignore_patterns: null\n  exclude_patterns:\n    - \"/other/\"\n",
        )
        .is_err());
    }

    #[test]
    fn git_config_rejects_explicit_null_alias_alongside_present_canonical() {
        assert!(yaml_serde::from_str::<Config>(
            "version: 1\ngit:\n  ignore_patterns:\n    - \"/data/\"\n  exclude_patterns: null\n",
        )
        .is_err());
    }

    #[test]
    fn git_config_serializes_only_the_canonical_key() {
        let cfg = Config {
            git: GitConfig {
                ignore_patterns: Some(vec![
                    crate::git_ignore::GitIgnorePattern::parse("/data/").unwrap(),
                ]),
            },
            ..Config::default()
        };
        let yaml = yaml_serde::to_string(&cfg).unwrap();
        assert!(yaml.contains("ignore_patterns"));
        assert!(!yaml.contains("exclude_patterns"));
    }

    #[test]
    fn validate_ignore_patterns_rejects_negated_entries() {
        assert!(validate_ignore_patterns(vec!["!keep.txt".to_string()]).is_err());
        assert!(validate_ignore_patterns(vec!["/data/".to_string()]).is_ok());
    }

    fn make_route(path: &str, remote: &str) -> RouteConfig {
        RouteConfig {
            path: gp(path),
            remote: RemoteName::from_string(remote.to_string()),
        }
    }

    #[test]
    fn route_for_picks_the_most_specific_matching_prefix() {
        let routes = RoutesConfig {
            by_name: BTreeMap::from([
                (
                    RouteName::from_string("bulk-vendor".to_string()),
                    make_route("vendor", "bulk"),
                ),
                (
                    RouteName::from_string("models".to_string()),
                    make_route("vendor/models", "models-store"),
                ),
                (
                    RouteName::from_string("private-models".to_string()),
                    make_route("vendor/models/private", "secure"),
                ),
            ]),
        };
        assert_eq!(
            routes
                .route_for(&gp("vendor/local/a.bin"))
                .map(|m| m.remote.as_str()),
            Some("bulk")
        );
        assert_eq!(
            routes
                .route_for(&gp("vendor/models/public/a.bin"))
                .map(|m| m.remote.as_str()),
            Some("models-store")
        );
        assert_eq!(
            routes
                .route_for(&gp("vendor/models/private/a.bin"))
                .map(|m| m.remote.as_str()),
            Some("secure")
        );
        assert_eq!(routes.route_for(&gp("datasets/a.bin")), None);
    }

    #[test]
    fn route_for_is_segment_aware_not_a_bare_prefix_match() {
        let routes = RoutesConfig {
            by_name: BTreeMap::from([(
                RouteName::from_string("data".to_string()),
                make_route("data", "bulk"),
            )]),
        };
        assert_eq!(routes.route_for(&gp("data2/a.bin")), None);
        assert_eq!(
            routes
                .route_for(&gp("data/a.bin"))
                .map(|m| m.remote.as_str()),
            Some("bulk")
        );
        assert_eq!(
            routes.route_for(&gp("data")).map(|m| m.remote.as_str()),
            Some("bulk")
        );
    }

    /// Route names never participate in precedence; only
    /// `path` specificity does, regardless of which name is
    /// lexicographically first/last.
    #[test]
    fn route_for_never_uses_name_for_tie_breaking() {
        let routes = RoutesConfig {
            by_name: BTreeMap::from([
                (
                    RouteName::from_string("zzz".to_string()),
                    make_route("vendor/models", "specific"),
                ),
                (
                    RouteName::from_string("aaa".to_string()),
                    make_route("vendor", "broad"),
                ),
            ]),
        };
        assert_eq!(
            routes
                .route_for(&gp("vendor/models/a.bin"))
                .map(|m| m.remote.as_str()),
            Some("specific")
        );
        assert_eq!(
            routes
                .route_for(&gp("vendor/models/a.bin"))
                .map(|m| m.name.as_str()),
            Some("zzz")
        );
    }

    #[test]
    fn routes_merged_with_overrides_win_on_same_name_collision() {
        let base = RoutesConfig {
            by_name: BTreeMap::from([(
                RouteName::from_string("models".to_string()),
                make_route("vendor", "bulk"),
            )]),
        };
        let override_ = RoutesConfig {
            by_name: BTreeMap::from([(
                RouteName::from_string("models".to_string()),
                make_route("vendor", "secure"),
            )]),
        };
        let merged = base.merged_with(override_);
        assert_eq!(
            merged
                .route_for(&gp("vendor/a.bin"))
                .map(|m| m.remote.as_str()),
            Some("secure")
        );
        assert_eq!(merged.by_name.len(), 1);
    }

    /// Layering by route `NAME`, not path, means an
    /// override with the *same name but a different path* still replaces
    /// the base definition outright, rather than the two paths coexisting.
    #[test]
    fn routes_merged_with_same_name_override_replaces_path_too() {
        let base = RoutesConfig {
            by_name: BTreeMap::from([(
                RouteName::from_string("models".to_string()),
                make_route("vendor/models", "bulk"),
            )]),
        };
        let override_ = RoutesConfig {
            by_name: BTreeMap::from([(
                RouteName::from_string("models".to_string()),
                make_route("vendor/models/v2", "bulk"),
            )]),
        };
        let merged = base.merged_with(override_);
        assert_eq!(merged.by_name.len(), 1);
        assert_eq!(merged.by_name["models"].path, "vendor/models/v2");
    }

    #[test]
    fn validate_effective_accepts_nested_routes_with_different_names() {
        let routes = RoutesConfig {
            by_name: BTreeMap::from([
                (
                    RouteName::from_string("models".to_string()),
                    make_route("vendor/models", "bulk"),
                ),
                (
                    RouteName::from_string("private-models".to_string()),
                    make_route("vendor/models/private", "secure"),
                ),
            ]),
        };
        assert!(routes.validate_effective().is_ok());
    }

    #[test]
    fn validate_effective_rejects_two_distinct_names_with_the_same_exact_path() {
        let routes = RoutesConfig {
            by_name: BTreeMap::from([
                (
                    RouteName::from_string("models".to_string()),
                    make_route("vendor/models", "bulk"),
                ),
                (
                    RouteName::from_string("models-alt".to_string()),
                    make_route("vendor/models", "secure"),
                ),
            ]),
        };
        assert!(routes.validate_effective().is_err());
    }

    /// A different-name/same-path conflict introduced only
    /// after layering (neither layer alone is invalid) must still be
    /// rejected once merged.
    #[test]
    fn validate_effective_rejects_a_same_path_conflict_introduced_only_after_merging_layers() {
        let global = RoutesConfig {
            by_name: BTreeMap::from([(
                RouteName::from_string("models".to_string()),
                make_route("vendor/models", "bulk"),
            )]),
        };
        let local = RoutesConfig {
            by_name: BTreeMap::from([(
                RouteName::from_string("models-alt".to_string()),
                make_route("vendor/models", "secure"),
            )]),
        };
        let merged = global.merged_with(local);
        assert!(merged.validate_effective().is_err());
    }

    /// Even if a `RoutesConfig` carrying the reserved name were
    /// constructed directly (bypassing `ConfigStore`'s per-layer check,
    /// e.g. via in-memory merging), `validate_effective` -- the
    /// single choke point every effective-config computation
    /// (including candidate configuration validation) runs
    /// through -- still rejects it.
    #[test]
    fn validate_effective_rejects_the_reserved_star_route_name() {
        let routes = RoutesConfig {
            by_name: BTreeMap::from([(
                RouteName::from_string(RESERVED_DEFAULT_ROUTE_NAME.to_string()),
                make_route("vendor/models", "bulk"),
            )]),
        };
        let err = routes.validate_effective().unwrap_err();
        assert!(
            format!("{err:#}").contains("reserved"),
            "expected an actionable reserved-name diagnostic, got: {err:#}"
        );
    }

    /// A hand-authored, already-normalized route path
    /// resolves identically to the equivalent route created via
    /// `normalize_route_path` (what `gat route add` uses), proving both
    /// surfaces share one canonical representation.
    #[test]
    fn hand_authored_normalized_route_matches_cli_created_route_equivalent() {
        let hand_authored = normalize_route_path("./vendor//models/").unwrap();
        let cli_created = normalize_route_path("vendor/models").unwrap();
        assert_eq!(hand_authored, cli_created);
    }
}
