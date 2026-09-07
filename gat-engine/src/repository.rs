//! Git repository discovery and semantic repository configuration access.

use gat_core::config::{Config, ConfigScope};
use gat_io::{AtomicError, RepoLock};
use gat_io::{
    ConfigError, ConfigStore, ConfigWriteError, LockError, LockStore, ScopedConfigError,
    ScopedConfigWriteError,
};
use std::path::{Path, PathBuf};

/// Typed failures from repository discovery and repo-local/global
/// `gat.yaml` path resolution and loading. `Config`'s own parsing/version
/// errors and mount/route validation surface here through
/// [`gat_io::ConfigError`]-carrying `#[source]`
/// fields (e.g. [`Self::ConfigLoad`], [`Self::InvalidEffectiveMounts`]).
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    /// [`std::env::current_dir`] failed (e.g. the directory was deleted
    /// out from under the process, or is otherwise unreadable).
    #[error("could not resolve the current working directory")]
    CurrentDirectory(#[source] std::io::Error),

    /// No `.git` marker was found in the current directory or any
    /// ancestor.
    #[error("not a git repository (or any parent up to /)")]
    NotRepository,

    /// A global-scoped config operation was requested but
    /// `$HOME`/`%USERPROFILE%` is not set.
    #[error(
        "cannot resolve the global gat config path: $HOME (or %USERPROFILE% on Windows) is not set"
    )]
    ConfigPathUnavailable,

    /// Reading/parsing one scope's `gat.yaml` failed while resolving the
    /// effective config (`Repository::load_config`).
    #[error("failed to load the {scope} gat.yaml")]
    ConfigLoad {
        scope: ConfigScope,
        #[source]
        source: ConfigError,
    },

    /// The effective (merged) mount configuration is invalid.
    #[error("invalid effective mount configuration")]
    InvalidEffectiveMounts(#[source] gat_core::config::ConfigError),

    /// The effective (merged) route configuration is invalid.
    #[error("invalid effective route configuration")]
    InvalidEffectiveRoutes(#[source] gat_core::config::ConfigError),
    #[error("invalid effective selections")]
    InvalidEffectiveSelections(#[source] gat_core::config::ConfigError),

    /// Reading/parsing a single scope's `gat.yaml` failed (`gat config`/
    /// `gat remote`/`gat mount`'s scoped read-before-write, via
    /// [`Repository::load_config_scoped`]) -- distinct from [`Self::ConfigLoad`],
    /// which is the *merged, effective* read every other command uses.
    #[error("failed to load the {scope} gat.yaml")]
    ConfigLoadScoped {
        scope: ConfigScope,
        #[source]
        source: ConfigError,
    },

    /// The parent directory of a scope's `gat.yaml` (`~/.gat/` or
    /// `<repo_root>/.gat/`) could not be created before writing.
    #[error("could not create the directory for the {scope} gat.yaml")]
    ConfigDirectoryCreate {
        scope: ConfigScope,
        #[source]
        source: ConfigWriteError,
    },

    /// The in-memory `Config` could not be serialized back to YAML before
    /// writing a scope's `gat.yaml`.
    #[error("failed to serialize the {scope} gat.yaml")]
    ConfigSerialize {
        scope: ConfigScope,
        #[source]
        source: ConfigWriteError,
    },

    /// Writing a scope's `gat.yaml` failed at the filesystem layer (e.g.
    /// disk full, permission denied).
    #[error("failed to write the {scope} gat.yaml")]
    ConfigWrite {
        scope: ConfigScope,
        kind: crate::repository_access::FilesystemFailureKind,
        #[source]
        source: ConfigWriteError,
    },
}

/// Safe classification of configuration access without exposing parser or OS text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigAccessFailureKind {
    Invalid,
    UnsupportedVersion { found: u32, expected: u32 },
    Filesystem(crate::FilesystemFailureKind),
}

impl RepositoryError {
    #[must_use]
    pub fn config_failure_kind(&self) -> ConfigAccessFailureKind {
        use crate::repository_access::{RepositoryAccessFailureKind, classify_atomic, classify_io};
        match self {
            Self::ConfigLoad { source, .. } | Self::ConfigLoadScoped { source, .. } => match source
            {
                ConfigError::Unreadable { source, .. } => {
                    ConfigAccessFailureKind::Filesystem(classify_io(source))
                }
                ConfigError::UnsupportedVersion {
                    found, expected, ..
                } => ConfigAccessFailureKind::UnsupportedVersion {
                    found: *found,
                    expected: *expected,
                },
                ConfigError::InvalidUtf8 { .. }
                | ConfigError::InvalidSyntax { .. }
                | ConfigError::Domain { .. } => ConfigAccessFailureKind::Invalid,
            },
            Self::ConfigDirectoryCreate {
                source: ConfigWriteError::Write(source),
                ..
            } => match classify_atomic(source) {
                RepositoryAccessFailureKind::Filesystem(kind) => {
                    ConfigAccessFailureKind::Filesystem(kind)
                }
                _ => ConfigAccessFailureKind::Filesystem(crate::FilesystemFailureKind::Unavailable),
            },
            Self::ConfigWrite { kind, .. } => ConfigAccessFailureKind::Filesystem(*kind),
            _ => ConfigAccessFailureKind::Invalid,
        }
    }
}

/// Every way an operation reading/writing `gat.lock` through [`Repository`] can
/// fail: loading the effective config it needs first (e.g. to resolve
/// `lock.shard_levels`), acquiring the repo-wide lock guarding the write,
/// or the write/reshape/parse itself.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error(transparent)]
    Config(#[from] Box<RepositoryError>),
    #[error(transparent)]
    Lock(crate::repository_access::RepositoryAccessError),
    #[error(transparent)]
    Atomic(crate::repository_access::RepositoryAccessError),
    #[error(transparent)]
    RemoteConfig(#[from] gat_core::config::ConfigError),
}

impl From<RepositoryError> for RepoError {
    fn from(err: RepositoryError) -> Self {
        Self::Config(Box::new(err))
    }
}

impl From<LockError> for RepoError {
    fn from(source: LockError) -> Self {
        Self::Lock(crate::repository_access::RepositoryAccessError::from_lock(
            source,
        ))
    }
}

impl From<AtomicError> for RepoError {
    fn from(source: AtomicError) -> Self {
        Self::Atomic(crate::repository_access::RepositoryAccessError::from_atomic(source))
    }
}

/// Repository discovery and layout-path resolution now live in
/// `gat-io`'s [`gat_io::RepositoryLayout`] (physical
/// facts only, no config/lock domain knowledge); this maps its
/// [`gat_io::LayoutError`] 1:1 onto the equivalent
/// `RepositoryError` variants so every existing call site keeps seeing
/// the same typed failures.
impl From<gat_io::LayoutError> for RepositoryError {
    fn from(err: gat_io::LayoutError) -> Self {
        use gat_io::LayoutError;
        match err {
            LayoutError::CurrentDirectory(source) => Self::CurrentDirectory(source),
            LayoutError::NotRepository => Self::NotRepository,
            LayoutError::ConfigPathUnavailable => Self::ConfigPathUnavailable,
        }
    }
}

#[derive(Debug)]
pub struct Repository {
    layout: gat_io::RepositoryLayout,
}

/// One operation-scoped read of every `gat.yaml` layer.
///
/// Mount policy performs several provenance and candidate-effective checks
/// while holding repository mutation authority. Keeping the three decoded
/// layers together avoids re-reading the same files for each check.
#[derive(Clone, Debug)]
pub struct ConfigLayers {
    layers: [Config; 3],
}

impl ConfigLayers {
    const fn index(scope: ConfigScope) -> usize {
        match scope {
            ConfigScope::Global => 0,
            ConfigScope::Project => 1,
            ConfigScope::Local => 2,
        }
    }

    #[must_use]
    pub const fn scoped(&self, scope: ConfigScope) -> &Config {
        &self.layers[Self::index(scope)]
    }

    pub fn effective(&self) -> std::result::Result<Config, RepositoryError> {
        validate_effective(Config::merge_layers(self.layers.clone()))
    }

    pub fn candidate_effective(
        &self,
        scope: ConfigScope,
        scoped: &Config,
    ) -> std::result::Result<Config, RepositoryError> {
        let mut layers = self.layers.clone();
        layers[Self::index(scope)] = scoped.clone();
        validate_effective(Config::merge_layers(layers))
    }

    #[must_use]
    pub fn mount_defining_scope(&self, name: &gat_core::name::MountName) -> Option<ConfigScope> {
        [
            ConfigScope::Local,
            ConfigScope::Project,
            ConfigScope::Global,
        ]
        .into_iter()
        .find(|scope| self.scoped(*scope).mounts.by_name.contains_key(name))
    }
}

fn validate_effective(effective: Config) -> std::result::Result<Config, RepositoryError> {
    effective
        .mounts
        .validate_effective()
        .map_err(RepositoryError::InvalidEffectiveMounts)?;
    effective
        .routes
        .validate_effective()
        .map_err(RepositoryError::InvalidEffectiveRoutes)?;
    effective
        .selections
        .validate_effective()
        .map_err(RepositoryError::InvalidEffectiveSelections)?;
    Ok(effective)
}

/// Origin selected when resolving the effective cache directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheLocationOrigin {
    Configuration,
    Environment,
}

/// Engine-private proof that repository mutation admission acquired the
/// physical lock and revalidated the expected desired revision.
pub(crate) struct RepositoryMutationAccess {
    _lock: RepoLock,
}

impl Repository {
    pub fn discover() -> std::result::Result<Self, RepositoryError> {
        Ok(Self::from_layout(gat_io::RepositoryLayout::discover()?))
    }

    #[cfg(test)]
    fn discover_from(dir: PathBuf) -> std::result::Result<Self, RepositoryError> {
        Ok(Self::from_layout(gat_io::RepositoryLayout::discover_from(
            dir,
        )?))
    }

    #[must_use]
    pub fn at(root: PathBuf) -> Self {
        Self::from_layout(gat_io::RepositoryLayout::at(root))
    }

    /// Observe the current canonical desired revision through the
    /// repository service without exposing lock layout or stat-cache
    /// acceleration details.
    pub fn current_desired_revision(
        &self,
    ) -> std::result::Result<
        crate::repository_state::DesiredRevision,
        crate::repository_state::DesiredRevisionError,
    > {
        crate::repository_state::observe_desired_revision(self)
    }

    /// Revalidate `expected` while holding repository mutation authority,
    /// without exposing the physical lock token.
    pub fn revalidate_desired_revision(
        &self,
        expected: &crate::repository_state::DesiredRevision,
    ) -> std::result::Result<(), crate::repository_state::DesiredRevisionError> {
        let _access = self.acquire_mutation_access(expected)?;
        Ok(())
    }

    /// Acquire mutation authority and transfer its lock lifetime as one
    /// opaque engine capability. Validation and token construction happen
    /// in the same critical section, so there is no release/reacquire race.
    pub(crate) fn acquire_mutation_access(
        &self,
        expected: &crate::repository_state::DesiredRevision,
    ) -> std::result::Result<RepositoryMutationAccess, crate::repository_state::DesiredRevisionError>
    {
        let lock = RepoLock::acquire_repository(self.layout())?;
        if self.current_desired_revision()? != *expected {
            return Err(crate::repository_state::StaleDesiredRevisionError.into());
        }
        Ok(RepositoryMutationAccess { _lock: lock })
    }

    /// This repo's physical layout, for delegating path/discovery logic to
    /// `gat-io`'s [`gat_io::RepositoryLayout`].
    pub(crate) const fn layout(&self) -> &gat_io::RepositoryLayout {
        &self.layout
    }

    pub(crate) fn worktree_client(&self) -> gat_io::WorktreeClient<'_> {
        self.layout.worktree_client()
    }

    const fn from_layout(layout: gat_io::RepositoryLayout) -> Self {
        Self { layout }
    }

    /// Open one unlocked, refreshed repository-state service for add
    /// discovery and hashing. The returned session is bound to this
    /// repository and consumes the already-resolved config snapshot.
    pub fn desired_state<'repo, 'config>(
        &'repo self,
        config: &'config Config,
    ) -> std::result::Result<
        crate::repository_mutation::DesiredState<'repo, 'config>,
        crate::repository_mutation::RepositoryStateError,
    > {
        crate::repository_mutation::DesiredState::open(self, config)
    }

    /// Acquire one lock-stable repository mutation service for remove/move
    /// workflows. Physical lock shape and state persistence remain below the
    /// returned semantic session.
    pub fn desired_mutation<'repo, 'config>(
        &'repo self,
        config: &'config Config,
    ) -> std::result::Result<
        crate::repository_mutation::DesiredMutation<'repo, 'config>,
        crate::repository_mutation::RepositoryMutationError,
    > {
        crate::repository_mutation::DesiredMutation::acquire(self, config)
    }

    /// Repository-bound mount recovery and state-transition service.
    ///
    /// The service owns repository mutation authority and exposes no journal,
    /// shape-lock, `SQLite`, or raw synchronization-path types.
    #[must_use]
    pub const fn mounts(&self) -> crate::mount::MountService<'_> {
        crate::mount::MountService::new(self)
    }

    /// Repository-bound semantic desired-state comparisons.
    #[must_use]
    pub const fn comparisons(&self) -> crate::compare::ComparisonService<'_> {
        crate::compare::ComparisonService::new(self)
    }

    /// Presence-only cache inspection without exposing physical cache paths
    /// or opening the cache proof database.
    #[must_use]
    pub fn cache_presence(&self) -> crate::CachePresenceSession {
        crate::CachePresenceSession::new(self)
    }

    /// Stream the current selected desired entries after one state open and
    /// refresh, without exposing state-store errors or cursor types.
    pub fn visit_current_desired_entries(
        &self,
        selection: &gat_core::selection::Selection,
        visit: impl FnMut(gat_core::lock::Entry),
    ) -> Result<(), crate::RepositoryStateError> {
        crate::desired_snapshot::visit_current_desired_entries(self, selection, visit)
    }

    /// `~/.gat/gat.yaml` (global) location's directory, if `$HOME` (or
    /// `%USERPROFILE%` on Windows) is set. `None` (rather than an error)
    /// when it isn't, so a missing home directory only breaks an explicit
    /// `--global` read/write, not every other command's effective-config
    /// read (which simply treats the global layer as empty).
    ///
    /// Reads the environment once here and delegates to
    /// the pure global-config resolver, so tests can
    /// exercise the resolution logic against an explicit home directory
    /// instead of mutating the process-wide `HOME`/`USERPROFILE`
    /// environment variables (which is unsound to do from parallel unit
    /// tests).
    ///
    /// Public so the command-owned init environment boundary can
    /// explicit-input entry point (see
    /// the root init orchestration can resolve the
    /// real ambient global-config directory once at gat's own production
    /// call site, while test fixtures supply their own deterministic fake
    /// directory instead of calling this at all -- see `test-support`'s
    /// `GatInitContext`.
    pub(crate) fn global_config_dir() -> Option<PathBuf> {
        Self::global_config_dir_from(gat_io::home_dir().as_deref())
    }

    /// Pure variant of [`Self::global_config_dir`]: resolves the global
    /// config directory from an explicit, already-resolved home directory
    /// (or `None` if there isn't one) instead of reading the environment
    /// itself.
    fn global_config_dir_from(home: Option<&Path>) -> Option<PathBuf> {
        gat_io::RepositoryLayout::global_config_dir_from(home)
    }

    /// Resolve the operational object cache without exposing its host path.
    pub(crate) fn resolved_cache_root(&self) -> gat_io::CacheRoot {
        let cache_dir_override = gat_io::cache_dir_override();
        if let Some(dir) = &cache_dir_override {
            return self
                .layout()
                .resolve_cache_root(Some(dir.as_os_str()), None);
        }

        match self.load_config() {
            Ok(cfg) => self.resolved_cache_root_from(&cfg),
            Err(_) => self.layout().resolve_cache_root(None, None),
        }
    }

    pub(crate) fn resolved_cache_root_from(&self, config: &Config) -> gat_io::CacheRoot {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::record_cache_location_resolution();
        let cache_dir_override = gat_io::cache_dir_override();
        self.resolved_cache_root_from_override(cache_dir_override.as_deref(), config)
    }

    /// Resolve the effective cache location from an already-loaded config
    /// without exposing the repository's physical layout.
    #[must_use]
    pub fn resolve_cache_location(
        &self,
        config: &Config,
    ) -> (
        crate::initialization::ResolvedCacheLocation,
        CacheLocationOrigin,
    ) {
        let override_dir = gat_io::cache_dir_override();
        let root = self.resolved_cache_root_from_override(override_dir.as_deref(), config);
        let origin = if override_dir.is_some() {
            CacheLocationOrigin::Environment
        } else {
            CacheLocationOrigin::Configuration
        };
        (
            crate::initialization::ResolvedCacheLocation::new(root.display_path().to_path_buf()),
            origin,
        )
    }

    pub(crate) fn resolved_cache_root_from_override(
        &self,
        cache_dir_override: Option<&std::ffi::OsStr>,
        config: &Config,
    ) -> gat_io::CacheRoot {
        self.layout()
            .resolve_cache_root(cache_dir_override, config.cache.location.as_ref())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn resolved_cache_root_with(
        &self,
        cache_dir_override: Option<&std::ffi::OsStr>,
        global_config_dir: Option<PathBuf>,
    ) -> gat_io::CacheRoot {
        if let Some(dir) = cache_dir_override {
            return self.layout().resolve_cache_root(Some(dir), None);
        }
        match self.load_config_with_global_dir(global_config_dir) {
            Ok(cfg) => self.resolved_cache_root_from_override(None, &cfg),
            Err(_) => self.layout().resolve_cache_root(None, None),
        }
    }

    /// The effective `gat.yaml`: the global (`~/.gat/gat.yaml`), project
    /// (`<repo_root>/gat.yaml`), and local (`<repo_root>/.gat/gat.yaml`)
    /// files merged together, local overriding project overriding global,
    /// using each section's merge rules (see [`Config::merge_layers`]). Every command that
    /// only reads config (as opposed to `gat config`/`gat remote`/`gat
    /// source`'s scoped writes) should go through this rather than reading
    /// one location directly, so a setting made at any location takes
    /// effect. A missing `$HOME` simply drops the global layer instead of
    /// failing, since most commands don't care where -- or whether -- a
    /// global config exists.
    ///
    /// Resolves the global config directory from the real environment
    /// once and delegates to the explicit-input config loader, the
    /// pure loader, so tests can exercise the merge logic against an
    /// explicit global config directory instead of mutating `HOME`/
    /// `USERPROFILE`.
    pub fn load_config(&self) -> std::result::Result<Config, RepositoryError> {
        self.load_config_layers()?.effective()
    }

    /// Reads every configuration layer once for operation-scoped policy.
    pub fn load_config_layers(&self) -> std::result::Result<ConfigLayers, RepositoryError> {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::record_config_load();
        self.load_config_layers_with_global_dir(Self::global_config_dir())
    }

    /// Pure variant of [`Self::load_config`] that merges the global layer
    /// from an explicit, already-resolved global config directory (or
    /// `None`, treating the global layer as empty) instead of reading the
    /// environment itself. `pub(crate)` so explicit cache resolution and
    /// command-owned init's explicit-input entry point can reuse it.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn load_config_with_global_dir(
        &self,
        global_config_dir: Option<PathBuf>,
    ) -> std::result::Result<Config, RepositoryError> {
        self.load_config_layers_with_global_dir(global_config_dir)?
            .effective()
    }

    fn load_config_layers_with_global_dir(
        &self,
        global_config_dir: Option<PathBuf>,
    ) -> std::result::Result<ConfigLayers, RepositoryError> {
        let global = match global_config_dir {
            Some(dir) => self.load_effective_scope(ConfigScope::Global, Some(&dir))?,
            None => Config::default(),
        };
        let project = self.load_effective_scope(ConfigScope::Project, None)?;
        let local = self.load_effective_scope(ConfigScope::Local, None)?;
        Ok(ConfigLayers {
            layers: [global, project, local],
        })
    }

    fn load_effective_scope(
        &self,
        scope: ConfigScope,
        global_config_dir: Option<&Path>,
    ) -> std::result::Result<Config, RepositoryError> {
        ConfigStore::load_scope(self.layout(), scope, global_config_dir).map_err(
            |error| match error {
                ScopedConfigError::Layout(source) => source.into(),
                ScopedConfigError::Read(source) => RepositoryError::ConfigLoad { scope, source },
            },
        )
    }

    /// Reads only the `gat.yaml` at `scope`, without merging in the other
    /// two locations -- what `gat config`/`gat remote`/`gat mount` read
    /// before mutating and writing back to that same scope, so a
    /// `--global` write only ever contains what was already in the global
    /// file (plus the one field just changed), never values merged in
    /// from project/local.
    pub fn load_config_scoped(
        &self,
        scope: ConfigScope,
    ) -> std::result::Result<Config, RepositoryError> {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::record_scoped_config_load();
        self.load_config_scoped_with_global_dir(scope, Self::global_config_dir().as_deref())
    }

    fn load_config_scoped_with_global_dir(
        &self,
        scope: ConfigScope,
        global_config_dir: Option<&Path>,
    ) -> std::result::Result<Config, RepositoryError> {
        ConfigStore::load_scope(self.layout(), scope, global_config_dir).map_err(
            |error| match error {
                ScopedConfigError::Layout(source) => source.into(),
                ScopedConfigError::Read(source) => {
                    RepositoryError::ConfigLoadScoped { scope, source }
                }
            },
        )
    }

    /// Loads only this repository's project `gat.yaml`.
    ///
    /// This is the source-repository config acquisition used by mount
    /// planning; decoding remains owned by [`gat_io::ConfigStore`].
    pub fn load_project_config(&self) -> std::result::Result<Config, RepositoryError> {
        self.load_config_scoped(ConfigScope::Project)
    }

    pub(crate) fn snapshot_input(
        &self,
        config: Config,
        desired_revision: crate::repository_state::DesiredRevision,
    ) -> crate::snapshot::SnapshotInput {
        let cache_root = self.resolved_cache_root_from(&config);
        let materialization_strategy = config
            .cache
            .materialization_strategy
            .clone()
            .unwrap_or_default();
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::record_materialization_strategy_resolution();
        crate::snapshot::SnapshotInput::new(
            config,
            cache_root,
            materialization_strategy,
            desired_revision,
        )
    }

    pub fn save_config(&self, cfg: &Config) -> std::result::Result<(), RepositoryError> {
        self.save_config_scoped(cfg, ConfigScope::Project)
    }

    /// Writes `cfg` to `scope`'s `gat.yaml`, creating its parent directory
    /// (`~/.gat/` or `<repo_root>/.gat/`) first if needed -- unlike the
    /// project location, the global and local directories aren't
    /// guaranteed to exist yet.
    pub fn save_config_scoped(
        &self,
        cfg: &Config,
        scope: ConfigScope,
    ) -> std::result::Result<(), RepositoryError> {
        ConfigStore::save_scope(
            self.layout(),
            scope,
            Self::global_config_dir().as_deref(),
            cfg,
        )
        .map_err(|error| match error {
            ScopedConfigWriteError::Layout(source) => source.into(),
            ScopedConfigWriteError::Write(source) => Self::map_config_write_error(scope, source),
        })
    }

    /// Maps I/O-owned scoped write failures onto semantic repository errors
    /// without copying the physical config path into the engine API.
    fn map_config_write_error(scope: ConfigScope, err: ConfigWriteError) -> RepositoryError {
        match err {
            source @ ConfigWriteError::Serialize { .. } => {
                RepositoryError::ConfigSerialize { scope, source }
            }
            source @ ConfigWriteError::Write(AtomicError::DirectoryUnavailable { .. }) => {
                RepositoryError::ConfigDirectoryCreate { scope, source }
            }
            source @ ConfigWriteError::Write(_) => {
                let kind = match &source {
                    ConfigWriteError::Write(atomic) => {
                        match crate::repository_access::classify_atomic(atomic) {
                            crate::repository_access::RepositoryAccessFailureKind::Filesystem(
                                kind,
                            ) => kind,
                            crate::repository_access::RepositoryAccessFailureKind::RepositoryLocked
                            | crate::repository_access::RepositoryAccessFailureKind::Lock(_) => {
                                crate::repository_access::FilesystemFailureKind::Unavailable
                            }
                        }
                    }
                    ConfigWriteError::Serialize { .. } => unreachable!(),
                };
                RepositoryError::ConfigWrite {
                    scope,
                    kind,
                    source,
                }
            }
        }
    }

    /// The effective `lock.shard_levels` (flat, the default, when unset) --
    /// how many `gat.lock` this repo's `gat.yaml` currently asks
    /// [`gat_core::lock::Lock`] publisher to write with the
    /// *next time it reshapes* (see [`Self::reshape_lock`]). Not
    /// necessarily what's on disk right now.
    pub fn lock_shard_levels(
        &self,
    ) -> std::result::Result<gat_core::lock::LockShardLevels, RepoError> {
        Ok(self.load_config()?.lock.shard_levels())
    }

    /// Write `gat.lock`, upgrading its on-disk layout to match the
    /// current `lock.shard_levels` first (see [`Self::reshape_lock`]) and
    /// then writing `lock`'s entries in that (now up to date) shape. Every
    /// command that mutates `gat.lock`'s *entries* (`add`, `rm`, `mv`)
    /// should save through this rather than naming the I/O persistence
    /// capability directly.
    ///
    /// Reshaping first means a `lock.shard_levels` edit takes effect on
    /// the very next `add`/`rm`/`mv`, not only the next `gat sync` --
    /// commands that manipulate the lock are expected to keep its layout
    /// current, not just its entries. The reshape is a no-op (just a
    /// stat/directory read) whenever the on-disk shape already matches,
    /// so this stays cheap in the common case where sharding hasn't just
    /// changed.
    pub fn save_lock(&self, lock: &gat_core::lock::Lock) -> std::result::Result<(), RepoError> {
        Ok(LockStore::publish_repository(
            self.layout(),
            lock,
            self.lock_shard_levels()?,
        )?)
    }

    /// Reshape `gat.lock` on disk into whatever shape `lock.shard_levels`
    /// currently calls for, if it isn't already in that shape -- a no-op,
    /// without touching disk at all, when the on-disk shape already
    /// matches (including when nothing's been written yet, since the
    /// first `add`/`rm`/`mv` will pick up `lock.shard_levels` itself; see
    /// [`Self::save_lock`]). Returns the new `shard_levels` if a reshape
    /// actually happened, `None` otherwise, so callers can report it.
    ///
    /// Complete lock publication applies the configured shape itself.
    /// This explicit operation is used by `gat sync` (including
    /// hook-triggered syncs and `gat pull`) when no lock-entry publication
    /// would otherwise occur.
    pub fn reshape_lock(
        &self,
    ) -> std::result::Result<Option<gat_core::lock::LockShardLevels>, RepoError> {
        let target = self.lock_shard_levels()?;
        let Some(reshape) = LockStore::begin_repository_reshape(self.layout(), target)? else {
            return Ok(None);
        };
        let _completed = reshape.apply()?;
        Ok(Some(target))
    }
}

#[cfg(test)]
mod tests {
    use super::Repository as Repo;
    use super::*;
    use gat_core::lexical_path::GatPath;
    use gat_core::lock::Lock;
    use gat_core::oid::Oid;

    fn gp(path: &str) -> GatPath {
        GatPath::parse_canonical(path).unwrap()
    }

    fn oid(digit: char) -> Oid {
        Oid::from_hex(&digit.to_string().repeat(64)).unwrap()
    }

    fn test_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        tmp
    }

    fn set_shard_levels(repo: &Repo, levels: u8) {
        let mut cfg = repo.load_config_scoped(ConfigScope::Project).unwrap();
        cfg.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(levels).unwrap());
        repo.save_config_scoped(&cfg, ConfigScope::Project).unwrap();
    }

    #[test]
    fn discovery_not_repository_error_preserves_its_semantic_kind() {
        let err = RepositoryError::from(gat_io::LayoutError::NotRepository);
        assert!(matches!(err, RepositoryError::NotRepository));
    }

    /// A directory with a real `.git` marker several levels below the
    /// walk's start is found -- `discover_from` walks up, not just checks
    /// the exact starting directory.
    #[test]
    fn discover_from_finds_a_git_marker_in_an_ancestor_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();

        let repo = Repo::discover_from(nested).unwrap();
        assert_eq!(
            repo.resolved_cache_root_from_override(None, &Config::default())
                .display_path(),
            root.join(".gat/objects")
        );
    }

    /// `RepositoryError::CurrentDirectory` retains the underlying OS error
    /// as a technical source for the root presentation layer.
    #[test]
    fn current_directory_failure_retains_the_os_error_as_its_source() {
        const SENTINEL: &str = "gat-repository-test-sentinel: stale NFS file handle";
        let source = std::io::Error::other(SENTINEL);
        let err = RepositoryError::CurrentDirectory(source);
        let technical: &dyn std::error::Error = &err;
        let chained = std::iter::successors(Some(technical), |e| e.source())
            .any(|e| e.to_string().contains(SENTINEL));
        assert!(
            chained,
            "expected the OS message somewhere in the source chain"
        );
    }

    /// An explicit global scoped load without `$HOME`/`%USERPROFILE%`
    /// must fail with `ConfigPathUnavailable`, not panic or silently default.
    #[test]
    fn global_scoped_load_reports_config_path_unavailable_without_a_home_directory() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let err = repo
            .load_config_scoped_with_global_dir(ConfigScope::Global, None)
            .unwrap_err();
        assert!(matches!(err, RepositoryError::ConfigPathUnavailable));
    }

    /// A malformed `gat.yaml` must surface as a typed `ConfigLoad` failure
    /// naming the failing scope without exposing its path, and the underlying YAML
    /// parser's raw message must stay out of the rendered diagnostic even
    /// though it's retained as a technical source.
    #[test]
    fn config_load_hides_the_raw_parser_message_but_keeps_it_as_a_technical_source() {
        const SENTINEL: &str = "gat-repository-test-sentinel-malformed-yaml";
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        // `git.ignore_patterns` is a sequence field; giving it a scalar
        // string makes `serde`'s own "invalid type" message echo that
        // string back verbatim -- exactly the kind of raw third-party
        // parser text that must never reach a user.
        std::fs::write(
            tmp.path().join("gat.yaml"),
            format!("git:\n  ignore_patterns: {SENTINEL}\n"),
        )
        .unwrap();

        let err = repo.load_config().unwrap_err();
        let RepositoryError::ConfigLoad { scope, source } = err else {
            panic!("expected RepositoryError::ConfigLoad, got {err:?}");
        };
        assert_eq!(scope, ConfigScope::Project);
        let ConfigError::InvalidSyntax {
            source: yaml_source,
            ..
        } = &source
        else {
            panic!("expected ConfigError::InvalidSyntax, got {source:?}");
        };
        assert!(format!("{yaml_source}").contains(SENTINEL));
    }

    #[test]
    fn objects_dir_override_wins_without_reading_process_environment() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut config = Config::default();
        config.cache.location = Some(gat_core::cache_location::CacheLocation::from_path(
            "configured-cache".into(),
        ));
        let override_path = tmp.path().join("override-cache");

        assert_eq!(
            repo.resolved_cache_root_from_override(Some(override_path.as_os_str()), &config)
                .display_path(),
            override_path.as_path()
        );
        assert_eq!(
            repo.resolved_cache_root_from_override(None, &config)
                .display_path(),
            tmp.path().join("configured-cache").as_path()
        );
    }

    /// `load_config` must fail closed on an invalid effective
    /// mount set even when it comes entirely from one hand-authored
    /// `gat.yaml`, not just a conflict introduced by `gat mount add`.
    #[test]
    fn load_config_fails_closed_on_hand_authored_overlapping_mount_targets() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(
            tmp.path().join("gat.yaml"),
            "version: 1\n\
             mounts:\n\
             \x20\x20models:\n\
             \x20\x20\x20\x20url: ../models\n\
             \x20\x20\x20\x20target: vendor/models\n\
             \x20\x20assets:\n\
             \x20\x20\x20\x20url: ../assets\n\
             \x20\x20\x20\x20target: vendor\n",
        )
        .unwrap();

        let err = repo.load_config().unwrap_err();
        let RepositoryError::InvalidEffectiveMounts(source) = err else {
            panic!("expected RepositoryError::InvalidEffectiveMounts, got {err:?}");
        };
        assert!(format!("{source}").contains("overlaps"));
    }

    #[test]
    fn reshape_lock_is_a_no_op_when_nothing_is_on_disk_yet() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let loads_before = gat_io::lock_test_support::reshape_full_loads();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
        let result = gat_io::atomic_test_support::with_acquire_attempt_hook(
            std::thread::current().id(),
            attempted_tx,
            || repo.reshape_lock(),
        );
        assert_eq!(result.unwrap(), None);
        assert!(attempted_rx.try_recv().is_err());
        assert_eq!(
            gat_io::lock_test_support::reshape_full_loads(),
            loads_before
        );
        assert!(!tmp.path().join("gat.lock").exists());
    }

    #[test]
    fn save_lock_upgrades_the_layout_when_shard_levels_config_changed() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());

        let mut lock = Lock::default();
        lock.upsert(gp("a.bin"), oid('a'));
        repo.save_lock(&lock).unwrap();
        assert!(tmp.path().join("gat.lock").is_file());

        set_shard_levels(&repo, 2);
        lock.upsert(gp("b.bin"), oid('b'));
        repo.save_lock(&lock).unwrap();
        assert!(
            tmp.path().join("gat.lock").is_dir(),
            "save_lock must upgrade the on-disk shape to match the new config"
        );
        assert_eq!(
            gat_io::LockStore::load_repository(repo.layout())
                .unwrap()
                .entries
                .len(),
            2
        );
    }

    /// `Repo::save_lock` is the explicitly proof-agnostic repository API:
    /// it must dispatch to `LockStore::publish_complete`, not the
    /// evidence-returning publication path, so it never computes a per-shard
    /// `ShardContentIdentity` BLAKE3 hash or mints a `StatProof` merely to
    /// discard it -- both on a fresh write (nothing on disk yet) and on a
    /// write that follows a reshape (shape already resolved and reused).
    #[test]
    fn save_lock_never_computes_identity_or_proof_values_it_would_discard() {
        use gat_io::lock_identity_test_support as identity_test_support;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        lock.upsert(gp("a.bin"), oid('a'));

        // Fresh write: `save_lock` must use proof-agnostic publication.
        let hash_before = identity_test_support::hash_shard_bytes_call_count();
        let proof_before = gat_io::file_state_test_support::stat_proof_from_metadata_call_count();
        repo.save_lock(&lock).unwrap();
        assert_eq!(
            identity_test_support::hash_shard_bytes_call_count(),
            hash_before,
            "a fresh save_lock write must not compute a ShardContentIdentity it discards"
        );
        assert_eq!(
            gat_io::file_state_test_support::stat_proof_from_metadata_call_count(),
            proof_before,
            "a fresh save_lock write must not mint a StatProof it discards"
        );

        // Reshape to sharded, then save again through the same proof-agnostic
        // repository boundary.
        set_shard_levels(&repo, 2);
        lock.upsert(gp("b.bin"), oid('b'));
        let hash_before = identity_test_support::hash_shard_bytes_call_count();
        let proof_before = gat_io::file_state_test_support::stat_proof_from_metadata_call_count();
        repo.save_lock(&lock).unwrap();
        assert_eq!(
            identity_test_support::hash_shard_bytes_call_count(),
            hash_before,
            "a reshape-then-save_lock write must not compute a ShardContentIdentity it discards"
        );
        assert_eq!(
            gat_io::file_state_test_support::stat_proof_from_metadata_call_count(),
            proof_before,
            "a reshape-then-save_lock write must not mint a StatProof it discards"
        );
    }

    #[test]
    fn reshape_lock_is_a_no_op_when_on_disk_shape_already_matches_config() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());

        let mut lock = Lock::default();
        lock.upsert(gp("a.bin"), oid('a'));
        repo.save_lock(&lock).unwrap();
        assert!(tmp.path().join("gat.lock").is_file());

        // Default `lock.shard_levels` is `0` (flat), matching what's on
        // disk, so there's nothing to reshape. The no-op path must neither
        // acquire the repository lock nor load the complete logical lock.
        let loads_before = gat_io::lock_test_support::reshape_full_loads();
        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
        let result = gat_io::atomic_test_support::with_acquire_attempt_hook(
            std::thread::current().id(),
            attempted_tx,
            || repo.reshape_lock(),
        );
        assert_eq!(result.unwrap(), None);
        assert!(attempted_rx.try_recv().is_err());
        assert_eq!(
            gat_io::lock_test_support::reshape_full_loads(),
            loads_before
        );
        assert!(tmp.path().join("gat.lock").is_file());
    }

    #[test]
    fn reshape_lock_converts_flat_to_sharded_and_reports_the_new_depth() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());

        let mut lock = Lock::default();
        lock.upsert(gp("a.bin"), oid('a'));
        repo.save_lock(&lock).unwrap();
        assert!(tmp.path().join("gat.lock").is_file());

        set_shard_levels(&repo, 2);
        let loads_before = gat_io::lock_test_support::reshape_full_loads();
        assert_eq!(
            repo.reshape_lock().unwrap(),
            Some(gat_core::lock::LockShardLevels::new(2).unwrap())
        );
        assert_eq!(
            gat_io::lock_test_support::reshape_full_loads(),
            loads_before + 1,
            "a genuine reshape must load the logical lock exactly once"
        );
        assert!(tmp.path().join("gat.lock").is_dir());
        assert_eq!(
            gat_io::LockStore::load_repository(repo.layout())
                .unwrap()
                .entries
                .len(),
            1
        );

        // Reshaping again with the same target config is now a no-op.
        assert_eq!(repo.reshape_lock().unwrap(), None);
    }

    /// SAFETY: mutating `$HOME` would race with any other test that reads
    /// or writes it concurrently, which is unsound under the default
    /// parallel test runner. These tests instead call the pure
    /// [`Repo::load_config_with_global_dir`] entry point with a fake home
    /// directory, so no process-global environment mutation -- and no
    /// serializing mutex -- is needed at all. Real `HOME`/`USERPROFILE`
    /// wiring is covered once at the spawned-binary level instead (see
    /// `tests/cli_integration.rs`'s environment tests).
    #[test]
    fn load_config_merges_global_project_and_local_with_local_winning() {
        let fake_home = tempfile::tempdir().unwrap();
        let global_dir = Repo::global_config_dir_from(Some(fake_home.path())).unwrap();

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());

        let mut global_cfg = gat_core::config::Config::default();
        global_cfg.cache.location = Some(gat_core::cache_location::CacheLocation::from_path(
            "global-cache".into(),
        ));
        global_cfg
            .selections
            .by_name
            .entry("runtime".into())
            .or_default()
            .include = Some(vec![
            gat_core::globs::GatGlobPattern::parse("global-target").unwrap(),
        ]);
        ConfigStore::save_scope(
            repo.layout(),
            ConfigScope::Global,
            Some(&global_dir),
            &global_cfg,
        )
        .unwrap();

        let mut project_cfg = gat_core::config::Config::default();
        project_cfg.cache.location = Some(gat_core::cache_location::CacheLocation::from_path(
            "project-cache".into(),
        ));
        repo.save_config_scoped(&project_cfg, ConfigScope::Project)
            .unwrap();

        let mut local_cfg = gat_core::config::Config::default();
        local_cfg
            .selections
            .by_name
            .entry("runtime".into())
            .or_default()
            .include = Some(vec![
            gat_core::globs::GatGlobPattern::parse("local-target").unwrap(),
        ]);
        repo.save_config_scoped(&local_cfg, ConfigScope::Local)
            .unwrap();

        let merged = repo.load_config_with_global_dir(Some(global_dir)).unwrap();
        // project overrides global's `cache.location`...
        assert_eq!(
            merged.cache.location,
            Some(gat_core::cache_location::CacheLocation::from_path(
                "project-cache".into()
            ))
        );
        // ...and local replaces the global selection; the absent project
        // selection does not affect inheritance.
        assert_eq!(
            merged.selections.by_name["runtime"].include,
            Some(vec![
                gat_core::globs::GatGlobPattern::parse("local-target").unwrap()
            ])
        );
    }

    #[test]
    fn save_config_scoped_local_and_project_do_not_clobber_each_other() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());

        let mut project_cfg = gat_core::config::Config::default();
        project_cfg.cache.location = Some(gat_core::cache_location::CacheLocation::from_path(
            "project-cache".into(),
        ));
        repo.save_config_scoped(&project_cfg, ConfigScope::Project)
            .unwrap();

        let mut local_cfg = gat_core::config::Config::default();
        local_cfg.cache.location = Some(gat_core::cache_location::CacheLocation::from_path(
            "local-cache".into(),
        ));
        repo.save_config_scoped(&local_cfg, ConfigScope::Local)
            .unwrap();

        assert_eq!(
            repo.load_config_scoped(ConfigScope::Project)
                .unwrap()
                .cache
                .location,
            Some(gat_core::cache_location::CacheLocation::from_path(
                "project-cache".into()
            ))
        );
        assert_eq!(
            repo.load_config_scoped(ConfigScope::Local)
                .unwrap()
                .cache
                .location,
            Some(gat_core::cache_location::CacheLocation::from_path(
                "local-cache".into()
            ))
        );
        // The local write must land under `.gat/`, not the project root.
        assert!(tmp.path().join(".gat").join("gat.yaml").is_file());
        assert!(tmp.path().join("gat.yaml").is_file());
    }

    /// If the directory a scope's `gat.yaml` would live under cannot be
    /// created (here, a regular file already occupies that name), the
    /// failure surfaces as a typed `ConfigDirectoryCreate`, not a generic
    /// `anyhow` chain -- and the underlying `io::Error` stays out of the
    /// rendered diagnostic's summary/detail text (only in the technical
    /// source), per the "no raw OS messages" rule already established
    /// for `RepositoryError::CurrentDirectory`.
    #[test]
    fn save_config_scoped_reports_a_typed_error_when_the_parent_directory_cannot_be_created() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        // Occupy `.gat/` with a plain file so `create_dir_all` fails.
        std::fs::remove_dir_all(tmp.path().join(".gat")).ok();
        std::fs::write(tmp.path().join(".gat"), b"not a directory").unwrap();

        let cfg = gat_core::config::Config::default();
        let err = repo
            .save_config_scoped(&cfg, ConfigScope::Local)
            .unwrap_err();
        assert!(
            matches!(
                err,
                RepositoryError::ConfigDirectoryCreate {
                    scope: ConfigScope::Local,
                    ..
                }
            ),
            "expected ConfigDirectoryCreate, got {err:?}"
        );
    }

    /// If the `gat.yaml` path itself is occupied by a directory, the
    /// atomic write fails and surfaces as a typed `ConfigWrite` wrapping
    /// the typed `atomic::AtomicError` directly.
    #[test]
    fn save_config_scoped_reports_a_typed_error_when_the_write_itself_fails() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::remove_file(tmp.path().join("gat.yaml")).ok();
        std::fs::create_dir_all(tmp.path().join("gat.yaml")).unwrap();

        let cfg = gat_core::config::Config::default();
        let err = repo
            .save_config_scoped(&cfg, ConfigScope::Project)
            .unwrap_err();
        assert!(
            matches!(
                err,
                RepositoryError::ConfigWrite {
                    scope: ConfigScope::Project,
                    ..
                }
            ),
            "expected ConfigWrite, got {err:?}"
        );
    }
}
