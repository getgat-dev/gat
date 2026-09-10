//! `gat.yaml`'s I/O-owned configuration boundary: [`ConfigStore`] reads
//! and decodes a config file from disk into the pure semantic
//! [`gat_core::config::Config`], writes one back out atomically, reports
//! filesystem/YAML-decoding/encoding failures, and normalizes
//! hand-authored text (route/mount paths, reserved names) at the point
//! it enters the typed model. The pure semantic model itself -- every
//! persisted config section's typed value, merge/validation logic, and
//! value-level parsing -- lives in [`gat_core::config`]; see its module
//! docs for the layering rules (global/project/local,
//! [`Config::merge_layers`]).
//!
//! Higher layers delegate through [`ConfigStore`] and reuse
//! [`crate::atomic::write_atomic`]
//! without duplicating decode/encode logic.

use gat_core::config::{
    CacheConfig, Config, ConfigScope, GitConfig, LockConfig, MountConfig, MountsConfig,
    RESERVED_DEFAULT_ROUTE_NAME, RemotesConfig, RouteConfig, RoutesConfig, SelectionsConfig,
    SyncConfig,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{LayoutError, RepositoryLayout};

/// The only `gat.yaml` document version this build understands.
pub const CONFIG_VERSION: u32 = 1;

/// Every way a `gat.yaml` config file can fail to be read or decoded
/// into the pure semantic [`Config`]: a filesystem/I/O-owned failure
/// specific to this on-disk loader (`Unreadable`/`InvalidUtf8`/
/// `InvalidSyntax`/`UnsupportedVersion`), or a pure semantic/config-value
/// failure from [`gat_core::config`] (wrapped as [`Self::Domain`]).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file exists but could not be read (permissions, I/O
    /// error, ...) -- distinct from a file whose *contents* are invalid.
    #[error("could not read {}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The config file's bytes are not valid UTF-8.
    #[error("{} is not valid UTF-8", path.display())]
    InvalidUtf8 {
        path: PathBuf,
        #[source]
        source: std::str::Utf8Error,
    },

    /// The config file's contents could not be parsed as YAML, or don't
    /// match `gat.yaml`'s schema. `line`/`column` are the parser's own
    /// stable position info (safe to surface as structured detail); the
    /// underlying parser message/type names are kept only as the hidden
    /// technical source, never rendered directly.
    #[error("{} has invalid syntax or does not match gat.yaml's schema", path.display())]
    InvalidSyntax {
        path: PathBuf,
        line: Option<usize>,
        column: Option<usize>,
        #[source]
        source: yaml_serde::Error,
    },

    /// The config file declares a `version` this build does not
    /// understand.
    #[error(
        "{} has version {found} but this gat only understands version {expected}",
        path.display()
    )]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        expected: u32,
    },

    /// A pure semantic/config-value failure -- parsing, validating, or
    /// resolving `gat.yaml`'s schema and values once decoded, independent
    /// of any filesystem or on-disk state. See
    /// [`gat_core::config::ConfigError`].
    #[error("{} contains an invalid configuration value: {source}", path.display())]
    Domain {
        path: PathBuf,
        #[source]
        source: Box<gat_core::config::ConfigError>,
    },
}

/// Every way writing a `gat.yaml` config file back out can fail:
/// encoding the in-memory [`Config`] as YAML, or the atomic filesystem
/// publish itself (directory creation, temp-file write, rename --
/// distinguished by [`crate::atomic::AtomicError`]'s own variants).
#[derive(Debug, thiserror::Error)]
pub enum ConfigWriteError {
    /// The in-memory [`Config`] could not be serialized back to YAML.
    #[error("could not encode {} as YAML", path.display())]
    Serialize {
        path: PathBuf,
        #[source]
        source: yaml_serde::Error,
    },

    /// The atomic filesystem publish of the encoded YAML failed (parent
    /// directory creation, temp-file write/sync, or rename).
    #[error(transparent)]
    Write(#[from] crate::atomic::AtomicError),
}

/// Failure to resolve or read one repository-owned configuration scope.
#[derive(Debug, thiserror::Error)]
pub enum ScopedConfigError {
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Read(#[from] ConfigError),
}

/// Failure to resolve or write one repository-owned configuration scope.
#[derive(Debug, thiserror::Error)]
pub enum ScopedConfigWriteError {
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Write(#[from] ConfigWriteError),
}

/// Human-authored deserialization input for `routes:`/`mounts:` in
/// `gat.yaml`, mirroring [`Config`] field-for-field except that
/// path-bearing values (`routes.*.path`, `mounts.*.target`) are held as
/// raw, not-yet-validated `String`s rather than
/// [`gat_core::lexical_path::GatPath`]. [`ConfigStore::load_file`]
/// deserializes the YAML into this representation first, then explicitly
/// converts each path-bearing field with the lenient
/// [`gat_core::lexical_path::GatPath::normalize`] (see
/// [`ConfigInput::into_config`]) -- this is the one place hand-authored
/// `gat.yaml` text is allowed to use lenient spellings (`./foo`,
/// redundant separators, backslashes, a trailing separator); the
/// resulting [`Config`]/[`RouteConfig`]/[`MountConfig`] remain strictly
/// typed domain structures whose own generic `Deserialize` (used e.g. by
/// the mount-transaction journal, whose text is machine-produced and
/// already canonical) expects canonical, already-validated values.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigInput {
    #[serde(default)]
    network: gat_core::settings::NetworkConfig,
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    remotes: RemotesConfig,
    #[serde(default)]
    cache: CacheConfig,
    #[serde(default)]
    sync: SyncConfig,
    #[serde(default)]
    selections: SelectionsConfig,
    #[serde(default)]
    lock: LockConfigInput,
    #[serde(default)]
    mounts: MountsConfigInput,
    #[serde(default)]
    routes: RoutesConfigInput,
    #[serde(default)]
    git: GitConfig,
}

/// Borrowing output document that adds persistence metadata without cloning
/// the semantic configuration or weakening its typed values.
#[derive(Serialize)]
struct ConfigDocument<'a> {
    version: u32,
    #[serde(flatten)]
    config: &'a Config,
}

/// Human-authored deserialization input for `lock:` in `gat.yaml`,
/// mirroring [`LockConfig`] except that `shard_levels` is held as a raw,
/// not-yet-validated `u8` -- so a hand-typed value like `3` (over
/// [`gat_core::config::MAX_SHARD_LEVELS`]) is rejected with
/// [`gat_core::config::ConfigError::ShardLevelsExceedsMaximum`] right
/// here, the same diagnostic `gat config lock.shard_levels 3` already
/// gives, instead of deserializing successfully and only failing later.
#[derive(Debug, Deserialize, Default)]
struct LockConfigInput {
    #[serde(default)]
    shard_levels: Option<u8>,
}

impl LockConfigInput {
    fn into_lock_config(self) -> std::result::Result<LockConfig, gat_core::config::ConfigError> {
        Ok(LockConfig {
            shard_levels: self
                .shard_levels
                .map(gat_core::config::validate_shard_levels)
                .transpose()?,
        })
    }
}

#[derive(Debug, Deserialize, Default)]
struct RoutesConfigInput {
    #[serde(flatten)]
    by_name: BTreeMap<String, RouteConfigInput>,
}

#[derive(Debug, Deserialize)]
struct RouteConfigInput {
    path: String,
    remote: String,
}

impl RouteConfigInput {
    /// Converts a raw, hand-authored route path into a canonical
    /// [`RouteConfig`], preserving the route's `name` in the resulting
    /// diagnostic so an invalid path fails closed with
    /// [`gat_core::config::ConfigError::InvalidRoutePath`] rather than a
    /// generic YAML syntax error.
    fn into_route_config(
        self,
        name: &str,
    ) -> std::result::Result<RouteConfig, gat_core::config::ConfigError> {
        let path = gat_core::lexical_path::GatPath::normalize(&self.path).map_err(|source| {
            gat_core::config::ConfigError::InvalidRoutePath {
                name: name.to_string(),
                path: self.path,
                source: Box::new(source),
            }
        })?;
        Ok(RouteConfig {
            path,
            remote: self.remote.into(),
        })
    }
}

#[derive(Debug, Deserialize, Default)]
struct MountsConfigInput {
    #[serde(flatten)]
    by_name: BTreeMap<String, MountConfigInput>,
}

#[derive(Debug, Deserialize)]
struct MountConfigInput {
    url: String,
    target: String,
    #[serde(default = "default_source_subpath_str")]
    path: String,
    #[serde(default)]
    rev: Option<gat_core::git::GitRevisionSpec>,
    #[serde(default)]
    rev_lock: Option<gat_core::git::GitCommitId>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

fn default_source_subpath_str() -> String {
    ".".to_string()
}

impl MountConfigInput {
    /// Converts a raw, hand-authored mount target into a canonical
    /// [`MountConfig`], preserving the mount's `name` in the resulting
    /// diagnostic. The unrepresentable root case (`.`, `./`, or empty --
    /// [`gat_core::lexical_path::LexicalPathError::EmptyPath`]) is
    /// handled here, before a [`gat_core::lexical_path::GatPath`] is ever
    /// constructed, and mapped to the existing mount-root diagnostic
    /// rather than a generic path error.
    fn into_mount_config(
        self,
        name: &str,
    ) -> std::result::Result<MountConfig, gat_core::config::ConfigError> {
        let target = gat_core::lexical_path::GatPath::normalize(&self.target).map_err(
            |source| match source {
                gat_core::lexical_path::LexicalPathError::EmptyPath { .. } => {
                    gat_core::config::ConfigError::MountTargetIsRoot {
                        name: Some(name.to_string()),
                    }
                }
                source => gat_core::config::ConfigError::InvalidPath {
                    input: self.target,
                    source: Box::new(source),
                },
            },
        )?;
        let path = gat_core::lexical_path::GatSubpath::normalize(&self.path).map_err(|source| {
            gat_core::config::ConfigError::InvalidMountSourcePath {
                name: name.to_string(),
                path: self.path.clone(),
                source: Box::new(source),
            }
        })?;
        let include = parse_mount_glob_patterns(name, "include", self.include)?;
        let exclude = parse_mount_glob_patterns(name, "exclude", self.exclude)?;
        Ok(MountConfig {
            url: gat_core::git_location::GitLocationSpec::from_string(self.url),
            target,
            path,
            rev: self.rev,
            rev_lock: self.rev_lock,
            include,
            exclude,
        })
    }
}

/// Converts a mount's raw, hand-authored `include`/`exclude` glob strings
/// into validated [`gat_core::globs::GatGlobPattern`]s, preserving the
/// mount's `name` and which of `include`/`exclude` failed in the
/// resulting diagnostic.
fn parse_mount_glob_patterns(
    name: &str,
    field: &'static str,
    patterns: Vec<String>,
) -> std::result::Result<Vec<gat_core::globs::GatGlobPattern>, gat_core::config::ConfigError> {
    patterns
        .into_iter()
        .map(|pattern| {
            gat_core::globs::GatGlobPattern::parse(&pattern).map_err(|source| {
                gat_core::config::ConfigError::InvalidMountGlobPattern {
                    name: name.to_string(),
                    field,
                    pattern,
                    source: Box::new(source),
                }
            })
        })
        .collect()
}

impl ConfigInput {
    /// Converts every hand-authored path-bearing field (route paths,
    /// mount targets) into its canonical typed form, name-by-name so
    /// invalid-path diagnostics keep the offending route/mount's name.
    fn into_config(self) -> std::result::Result<Config, gat_core::config::ConfigError> {
        let mut routes_by_name = BTreeMap::new();
        for (name, route) in self.routes.by_name {
            let route = route.into_route_config(&name)?;
            routes_by_name.insert(gat_core::name::RouteName::from_string(name), route);
        }
        let mut mounts_by_name = BTreeMap::new();
        for (name, mount) in self.mounts.by_name {
            let mount = mount.into_mount_config(&name)?;
            mounts_by_name.insert(gat_core::name::MountName::from_string(name), mount);
        }
        Ok(Config {
            network: self.network,
            remotes: self.remotes,
            cache: self.cache,
            sync: self.sync,
            selections: self.selections,
            lock: self.lock.into_lock_config()?,
            mounts: MountsConfig {
                by_name: mounts_by_name,
            },
            routes: RoutesConfig {
                by_name: routes_by_name,
            },
            git: self.git,
        })
    }
}

const fn default_version() -> u32 {
    CONFIG_VERSION
}

/// Rejects the reserved `*` route name at config-load time (`*` is `gat
/// route list`'s synthetic default-remote row and must never be a real,
/// hand-authored route) -- also checked again against the merged
/// effective route set by
/// [`gat_core::config::RoutesConfig::validate_effective`]. Each route's
/// `path` has already been converted to a canonical `GatPath` by
/// [`ConfigInput::into_config`] (config-file text is one of
/// `GatPath::normalize`'s accepted, lenient construction boundaries), so
/// there is no separate path-normalization pass here.
fn normalize_route_paths(
    cfg: Config,
) -> std::result::Result<Config, gat_core::config::ConfigError> {
    if cfg.routes.by_name.contains_key(RESERVED_DEFAULT_ROUTE_NAME) {
        return Err(gat_core::config::ConfigError::ReservedRouteName);
    }
    Ok(cfg)
}

/// The I/O-owned `gat.yaml` persistence façade: owns scoped config-file
/// decoding/encoding, directory creation (via
/// [`crate::atomic::write_atomic`]), and atomic publication. Holds no
/// state of its own. Repository-owned operations accept a
/// [`RepositoryLayout`] and semantic scope, while explicit paths remain
/// available for standalone config-document protocols. Config-layer merging
/// remains the engine's responsibility.
#[derive(Debug, Clone, Copy)]
pub struct ConfigStore;

/// Opaque revision bound to the exact scope document that was read.
#[derive(Clone)]
pub struct ConfigRevision {
    path: PathBuf,
    bytes: Option<std::sync::Arc<[u8]>>,
}
impl std::fmt::Debug for ConfigRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigRevision { .. }")
    }
}
impl ConfigRevision {
    pub fn is_current(
        &self,
        layout: &RepositoryLayout,
        scope: ConfigScope,
        global: Option<&Path>,
    ) -> Result<bool, ScopedConfigError> {
        let path = layout.config_path_for_home(scope, global)?;
        if path != self.path {
            return Ok(false);
        }
        Ok(read_optional(&path)?.as_deref() == self.bytes.as_deref())
    }
}
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

impl ConfigStore {
    pub fn capture_scope(
        layout: &RepositoryLayout,
        scope: ConfigScope,
        global: Option<&Path>,
    ) -> Result<(Config, ConfigRevision), ScopedConfigError> {
        let path = layout.config_path_for_home(scope, global)?;
        let bytes = read_optional(&path)?;
        let config = bytes
            .as_deref()
            .map(|bytes| Self::decode(&path, bytes))
            .transpose()?
            .unwrap_or_default();
        Ok((
            config,
            ConfigRevision {
                path,
                bytes: bytes.map(Into::into),
            },
        ))
    }

    /// Reads one repository-owned `gat.yaml` scope without exposing its path.
    pub fn load_scope(
        layout: &RepositoryLayout,
        scope: ConfigScope,
        global_config_dir: Option<&Path>,
    ) -> std::result::Result<Config, ScopedConfigError> {
        let path = layout.config_path_for_home(scope, global_config_dir)?;
        Ok(Self::load_file(&path)?)
    }

    /// Writes one repository-owned `gat.yaml` scope without exposing its path.
    pub fn save_scope(
        layout: &RepositoryLayout,
        scope: ConfigScope,
        global_config_dir: Option<&Path>,
        cfg: &Config,
    ) -> std::result::Result<(), ScopedConfigWriteError> {
        let path = layout.config_path_for_home(scope, global_config_dir)?;
        if scope == ConfigScope::Local {
            layout.local_directory().ensure().map_err(|error| {
                ConfigWriteError::Write(crate::AtomicError::DirectoryUnavailable {
                    path: error.path,
                    source: error.source,
                })
            })?;
        }
        Ok(Self::save_file(&path, cfg)?)
    }

    /// Atomically creates a commented-out project configuration example if absent.
    pub fn create_project_if_absent(
        layout: &RepositoryLayout,
        config: &Config,
    ) -> std::result::Result<bool, ConfigWriteError> {
        let path = layout.config_path();
        let serialized = Self::serialize(&path, config)?;
        let mut contents = String::from(
            "# gat.yaml -- project configuration, committed to git and shared by\n\
# everyone who clones this repository. Every key below is optional and\n\
# shown commented-out with its default/example value; uncomment and\n\
# edit only what you need. See `gat config --help` or the Configuration\n\
# reference (https://getgat.dev/references/configuration) for the\n\
# full list of keys.\n\n",
        );
        for line in serialized.lines() {
            contents.push_str("# ");
            contents.push_str(line);
            contents.push('\n');
        }
        Ok(crate::atomic::write_atomic_if_absent(&path, &contents)?)
    }

    /// Reads a `gat.yaml` from an arbitrary path (defaults if missing).
    /// Used for explicit standalone documents and source-repository
    /// inspection where no current-repository layout exists. Repository-owned
    /// scoped access goes through [`Self::load_scope`].
    pub fn load_file(path: &Path) -> std::result::Result<Config, ConfigError> {
        read_optional(path)?
            .as_deref()
            .map(|bytes| Self::decode(path, bytes))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn decode(path: &Path, bytes: &[u8]) -> std::result::Result<Config, ConfigError> {
        let text = std::str::from_utf8(bytes).map_err(|source| ConfigError::InvalidUtf8 {
            path: path.to_path_buf(),
            source,
        })?;
        let input: ConfigInput = yaml_serde::from_str(text).map_err(|source| {
            let location = source.location();
            ConfigError::InvalidSyntax {
                path: path.to_path_buf(),
                line: location.as_ref().map(yaml_serde::Location::line),
                column: location.as_ref().map(yaml_serde::Location::column),
                source,
            }
        })?;
        if input.version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                path: path.to_path_buf(),
                found: input.version,
                expected: CONFIG_VERSION,
            });
        }
        let cfg = input.into_config().map_err(|source| ConfigError::Domain {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        normalize_route_paths(cfg).map_err(|source| ConfigError::Domain {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }

    /// Encodes `cfg` as YAML and publishes it atomically at `path`,
    /// always writing the current [`CONFIG_VERSION`]. Parent-directory
    /// creation is handled by [`crate::atomic::write_atomic`] itself.
    pub fn save_file(path: &Path, cfg: &Config) -> std::result::Result<(), ConfigWriteError> {
        let serialized = Self::serialize(path, cfg)?;
        crate::atomic::write_atomic(path, &serialized)?;
        Ok(())
    }

    fn serialize(path: &Path, cfg: &Config) -> Result<String, ConfigWriteError> {
        let document = ConfigDocument {
            version: CONFIG_VERSION,
            config: cfg,
        };
        yaml_serde::to_string(&document).map_err(|source| ConfigWriteError::Serialize {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path).unwrap()
    }

    #[test]
    fn load_file_of_a_nonexistent_path_returns_the_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConfigStore::load_file(&dir.path().join("gat.yaml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn remote_schema_rejects_missing_urls_and_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        for remote in ["{}", "{url: file:///storage, unexpected: true}"] {
            std::fs::write(&path, format!("remotes:\n  origin: {remote}\n")).unwrap();
            assert!(matches!(
                ConfigStore::load_file(&path),
                Err(ConfigError::InvalidSyntax { .. })
            ));
        }
    }

    #[test]
    fn gc_execution_policy_is_not_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(&path, "gc:\n  repository_concurrency: 4\n").unwrap();
        assert!(matches!(
            ConfigStore::load_file(&path),
            Err(ConfigError::InvalidSyntax { .. })
        ));
    }

    #[test]
    fn named_remote_and_default_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(
            &path,
            "remotes:\n  default: origin\n  origin:\n    url: file:///storage\n",
        )
        .unwrap();
        let config = ConfigStore::load_file(&path).unwrap();
        assert_eq!(config.remotes.default.as_ref().unwrap().as_str(), "origin");
        assert_eq!(
            config.remotes.by_name["origin"].url.as_template_str(),
            "file:///storage"
        );
        ConfigStore::save_file(&path, &config).unwrap();
        assert_eq!(ConfigStore::load_file(&path).unwrap(), config);
    }

    #[test]
    fn load_file_reports_unreadable_when_the_path_is_a_directory_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::create_dir(&path).unwrap();
        let err = ConfigStore::load_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Unreadable { .. }));
    }

    #[test]
    fn load_file_reports_invalid_syntax_on_malformed_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(&path, b"not: [valid").unwrap();
        let err = ConfigStore::load_file(&path).unwrap_err();
        let ConfigError::InvalidSyntax {
            line,
            column,
            source,
            ..
        } = err
        else {
            panic!("expected invalid syntax");
        };
        assert_eq!(line, source.location().map(|location| location.line()));
        assert_eq!(column, source.location().map(|location| location.column()));
        assert!(line.is_some());
        assert!(column.is_some());
    }

    #[test]
    fn load_file_reports_invalid_utf8_without_attempting_yaml_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(&path, b"version: 1\n\xff").unwrap();

        let err = ConfigStore::load_file(&path).unwrap_err();

        assert!(matches!(err, ConfigError::InvalidUtf8 { path: p, .. } if p == path));
    }

    #[test]
    fn load_file_reports_unsupported_version_with_the_found_and_expected_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(&path, b"version: 999\n").unwrap();
        let err = ConfigStore::load_file(&path).unwrap_err();
        let ConfigError::UnsupportedVersion {
            found, expected, ..
        } = &err
        else {
            panic!("expected ConfigError::UnsupportedVersion, got {err:?}");
        };
        assert_eq!(*found, 999);
        assert_eq!(*expected, CONFIG_VERSION);
    }

    #[test]
    fn load_file_accepts_a_missing_version_and_returns_only_semantic_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(&path, b"sync:\n  auto_fetch: true\n").unwrap();
        let cfg = ConfigStore::load_file(&path).unwrap();
        assert!(cfg.sync.auto_fetch());
    }

    #[test]
    fn load_file_rejects_a_mount_target_of_the_repository_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(
            &path,
            format!(
                "version: {CONFIG_VERSION}\n{}",
                concat!(
                    "mounts:\n  ",
                    // hygiene-ok: URL text is decoded from an in-memory config and never fetched.
                    "models:\n    url: https://example.test/models.git\n    target: .\n"
                )
            ),
        )
        .unwrap();
        let err = ConfigStore::load_file(&path).unwrap_err();
        let ConfigError::Domain {
            path: error_path,
            source,
        } = err
        else {
            panic!("expected a path-qualified mount-target error");
        };
        assert_eq!(error_path, path);
        assert!(matches!(
            *source,
            gat_core::config::ConfigError::MountTargetIsRoot { .. }
        ));
    }

    #[test]
    fn load_file_canonicalizes_redundant_route_path_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(
            &path,
            format!(
                "version: {CONFIG_VERSION}\n\
                 routes:\n  \
                 vendor:\n    path: ./vendor/models//\n    remote: bulk\n"
            ),
        )
        .unwrap();
        let cfg = ConfigStore::load_file(&path).unwrap();
        assert_eq!(
            cfg.routes.by_name.get("vendor").unwrap().path,
            gp("vendor/models")
        );
    }

    #[test]
    fn normalized_route_paths_still_collide_during_effective_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(
            &path,
            format!(
                "version: {CONFIG_VERSION}\n\
                 routes:\n  \
                 models:\n    path: vendor/models\n    remote: bulk\n  \
                 models-alt:\n    path: ./vendor/models/\n    remote: secure\n"
            ),
        )
        .unwrap();
        let cfg = ConfigStore::load_file(&path).unwrap();
        assert!(matches!(
            cfg.routes.validate_effective(),
            Err(gat_core::config::ConfigError::RouteConflict { .. })
        ));
    }

    #[test]
    fn load_file_rejects_invalid_hand_authored_route_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(
            &path,
            format!(
                "version: {CONFIG_VERSION}\n\
                 routes:\n  models:\n    path: ../escape\n    remote: bulk\n"
            ),
        )
        .unwrap();
        assert!(matches!(
            ConfigStore::load_file(&path),
            Err(ConfigError::Domain { .. })
        ));
    }

    #[test]
    fn load_file_rejects_a_hand_authored_reserved_star_route_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(
            &path,
            format!(
                "version: {CONFIG_VERSION}\n\
                 routes:\n  \
                 '*':\n    path: vendor/models\n    remote: bulk\n"
            ),
        )
        .unwrap();
        let err = ConfigStore::load_file(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("reserved"),
            "expected an actionable reserved-name diagnostic, got: {err:#}"
        );
    }

    #[test]
    fn load_file_accepts_ordinary_route_names_and_preserves_segment_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        std::fs::write(
            &path,
            format!(
                "version: {CONFIG_VERSION}\n\
                 routes:\n  \
                 vendor:\n    path: ./vendor/\n    remote: bulk\n  \
                 vendor-models:\n    path: vendor//models\n    remote: secure\n"
            ),
        )
        .unwrap();
        let cfg = ConfigStore::load_file(&path).unwrap();
        assert!(cfg.routes.validate_effective().is_ok());
        let matched = cfg
            .routes
            .route_for(&gp("vendor/models/weights.bin"))
            .unwrap();
        assert_eq!(matched.name, "vendor-models");
        assert_eq!(matched.remote, "secure");
    }

    #[test]
    fn save_file_then_load_file_round_trips_and_writes_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gat.yaml");
        let cfg = Config::default();
        ConfigStore::save_file(&path, &cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("version: {CONFIG_VERSION}\n")
        );
        let loaded = ConfigStore::load_file(&path).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn save_file_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("gat.yaml");
        ConfigStore::save_file(&path, &Config::default()).unwrap();
        assert!(path.is_file());
    }
}
