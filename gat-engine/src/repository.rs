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
    #[error("could not recover pending mount changes before editing configuration")]
    PendingMountRecovery(#[source] Box<crate::MountWorkflowError>),
    #[error("configuration changed since it was read")]
    ConfigurationChanged { scope: ConfigScope },
    #[error("could not lock settings")]
    SettingLock { source: gat_io::AtomicError },
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

    #[error("persisted configuration references an undefined remote")]
    UndefinedResourceRemote { name: gat_core::name::RemoteName },

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
    pub(crate) inputs: std::sync::Arc<gat_io::InvocationInputs>,
}

/// Publication capability tied to the repository whose configuration locks it owns.
/// It can only be created after pending mount recovery has completed.
pub(crate) struct ConfigurationEdit<'repo> {
    repo: &'repo Repository,
    _guard: RepoLock,
}

impl ConfigurationEdit<'_> {
    pub(crate) fn commit(
        &self,
        layers: &ConfigLayers,
        candidate: &Config,
        scope: ConfigScope,
    ) -> Result<(), RepositoryError> {
        self.repo
            .validate_config_candidate(layers, candidate, scope)?;
        self.repo.save_config_scoped(candidate, scope)
    }
}

/// One operation-scoped read of every `gat.yaml` layer.
///
/// Mount policy performs several provenance and candidate-effective checks
/// while holding repository mutation authority. Keeping the three decoded
/// layers together avoids re-reading the same files for each check.
#[derive(Clone, Debug)]
pub struct ConfigLayers {
    layers: [CapturedConfig; 3],
    overrides: gat_core::settings::SettingsLayer,
}

// Keep decoded content attached to the evidence captured from the same file.
#[derive(Clone, Debug)]
struct CapturedConfig {
    config: Config,
    revision: Option<gat_io::ConfigRevision>,
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
        &self.layers[Self::index(scope)].config
    }

    pub(crate) fn unvalidated_effective(&self) -> Config {
        let mut config = self.resource_view();
        self.overrides.apply_to(&mut config);
        config
    }

    pub(crate) fn resource_view(&self) -> Config {
        Config::merge_layers(self.layers.iter().map(|layer| layer.config.clone()))
    }

    pub fn effective(&self) -> std::result::Result<Config, RepositoryError> {
        validate_effective(self.unvalidated_effective())
    }

    // Ordinary reads no longer need provenance after merging; move their decoded
    // layers instead of cloning every definition and compiled pattern.
    fn into_effective(self) -> std::result::Result<Config, RepositoryError> {
        {
            let mut config = Config::merge_layers(self.layers.map(|layer| layer.config));
            self.overrides.apply_to(&mut config);
            validate_effective(config)
        }
    }

    pub fn candidate_effective(
        &self,
        scope: ConfigScope,
        scoped: &Config,
    ) -> std::result::Result<Config, RepositoryError> {
        let replaced = Self::index(scope);
        let layers = self.layers.iter().enumerate().map(|(index, layer)| {
            if index == replaced {
                scoped
            } else {
                &layer.config
            }
            .clone()
        });
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
        let lock = self.acquire_configuration_lock()?;
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

    pub(crate) const fn from_layout(
        layout: gat_io::RepositoryLayout,
        inputs: std::sync::Arc<gat_io::InvocationInputs>,
    ) -> Self {
        Self { layout, inputs }
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
    pub fn cache_presence(&self) -> Result<crate::CachePresenceSession, RepositoryError> {
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

    pub(crate) fn acquire_configuration_lock(&self) -> Result<RepoLock, gat_io::AtomicError> {
        RepoLock::acquire_configuration(self.layout(), self.global_config_dir().as_deref())
    }

    pub(crate) fn begin_configuration_edit(
        &self,
    ) -> Result<ConfigurationEdit<'_>, RepositoryError> {
        let guard = self
            .acquire_configuration_lock()
            .map_err(|source| RepositoryError::SettingLock { source })?;
        self.mounts()
            .recover_pending_locked(&guard, &gat_core::progress::NoopProgress)
            .map_err(|source| RepositoryError::PendingMountRecovery(Box::new(source)))?;
        Ok(ConfigurationEdit {
            repo: self,
            _guard: guard,
        })
    }

    pub(crate) fn global_config_dir(&self) -> Option<PathBuf> {
        gat_io::RepositoryLayout::global_config_dir_from(self.inputs.home())
    }

    pub(crate) fn resolved_cache_root(&self) -> Result<gat_io::CacheRoot, RepositoryError> {
        Ok(self.resolved_cache_root_from(&self.load_config()?))
    }

    pub(crate) fn resolved_cache_root_from(&self, config: &Config) -> gat_io::CacheRoot {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::record_cache_location_resolution();
        self.layout()
            .resolve_cache_root(config.cache.location.as_ref())
    }

    #[must_use]
    pub fn resolve_cache_location(
        &self,
        config: &Config,
    ) -> (
        crate::initialization::ResolvedCacheLocation,
        CacheLocationOrigin,
    ) {
        let root = self.resolved_cache_root_from(config);
        let origin = if self
            .inputs
            .settings()
            .contains(gat_core::settings::SettingKey::CacheLocation)
        {
            CacheLocationOrigin::Environment
        } else {
            CacheLocationOrigin::Configuration
        };
        (
            crate::initialization::ResolvedCacheLocation::new(root.display_path().to_path_buf()),
            origin,
        )
    }

    pub fn validate_remote_url(
        &self,
        template: &gat_core::endpoint::RemoteUrlTemplate,
    ) -> Result<(), crate::RemoteUrlValidationError> {
        crate::remote_catalog::validate_remote_url(template, &self.inputs.templates())
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
        self.load_config_layers()?.into_effective()
    }

    /// Reads every configuration layer once for operation-scoped policy.
    pub fn load_config_layers(&self) -> std::result::Result<ConfigLayers, RepositoryError> {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::record_config_load();
        self.load_config_layers_with_global_dir(self.global_config_dir())
    }

    fn load_config_layers_with_global_dir(
        &self,
        global_config_dir: Option<PathBuf>,
    ) -> std::result::Result<ConfigLayers, RepositoryError> {
        let capture = |scope, global| {
            ConfigStore::capture_scope(self.layout(), scope, global).map_err(|error| match error {
                ScopedConfigError::Layout(source) => source.into(),
                ScopedConfigError::Read(source) => RepositoryError::ConfigLoad { scope, source },
            })
        };
        let (global, global_revision) = match global_config_dir.as_deref() {
            Some(dir) => {
                let (config, revision) = capture(ConfigScope::Global, Some(dir))?;
                (config, Some(revision))
            }
            None => (Config::default(), None),
        };
        let (project, project_revision) = capture(ConfigScope::Project, None)?;
        let (local, local_revision) = capture(ConfigScope::Local, None)?;
        Ok(ConfigLayers {
            layers: [
                CapturedConfig {
                    config: global,
                    revision: global_revision,
                },
                CapturedConfig {
                    config: project,
                    revision: Some(project_revision),
                },
                CapturedConfig {
                    config: local,
                    revision: Some(local_revision),
                },
            ],
            overrides: self.inputs.settings().clone(),
        })
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
        self.load_config_scoped_with_global_dir(scope, self.global_config_dir().as_deref())
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

    pub(crate) fn validate_config_candidate(
        &self,
        layers: &ConfigLayers,
        candidate: &Config,
        scope: ConfigScope,
    ) -> Result<(), RepositoryError> {
        let effective = layers.candidate_effective(scope, candidate)?;
        for name in effective
            .remotes
            .default
            .iter()
            .chain(effective.routes.by_name.values().map(|route| &route.remote))
        {
            if !effective.remotes.by_name.contains_key(name) {
                return Err(RepositoryError::UndefinedResourceRemote { name: name.clone() });
            }
        }
        for check_scope in [
            ConfigScope::Global,
            ConfigScope::Project,
            ConfigScope::Local,
        ] {
            if let Some(revision) = &layers.layers[ConfigLayers::index(check_scope)].revision {
                let current = revision
                    .is_current(
                        self.layout(),
                        check_scope,
                        self.global_config_dir().as_deref(),
                    )
                    .map_err(|error| match error {
                        ScopedConfigError::Layout(source) => source.into(),
                        ScopedConfigError::Read(source) => RepositoryError::ConfigLoad {
                            scope: check_scope,
                            source,
                        },
                    })?;
                if !current {
                    return Err(RepositoryError::ConfigurationChanged { scope: check_scope });
                }
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn save_config(&self, cfg: &Config) -> std::result::Result<(), RepositoryError> {
        self.save_config_scoped(cfg, ConfigScope::Project)
    }

    /// Writes `cfg` to `scope`'s `gat.yaml`, creating its parent directory
    /// (`~/.gat/` or `<repo_root>/.gat/`) first if needed -- unlike the
    /// project location, the global and local directories aren't
    /// guaranteed to exist yet.
    pub(crate) fn save_config_scoped(
        &self,
        cfg: &Config,
        scope: ConfigScope,
    ) -> std::result::Result<(), RepositoryError> {
        ConfigStore::save_scope(
            self.layout(),
            scope,
            self.global_config_dir().as_deref(),
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

impl ConfigLayers {
    #[must_use]
    pub fn setting(
        &self,
        key: gat_core::settings::SettingKey,
    ) -> (
        Option<gat_core::settings::SettingAssignment>,
        gat_core::settings::SettingSource,
    ) {
        use gat_core::settings::SettingSource;
        let source = self.setting_source(key, None);
        let value = match source {
            SettingSource::Environment(_) => self.overrides.get(key),
            SettingSource::Scope(scope) => key.read(self.scoped(scope)),
            SettingSource::Default => key.default_value(),
        };
        (value, source)
    }

    fn setting_source(
        &self,
        key: gat_core::settings::SettingKey,
        candidate: Option<(ConfigScope, &Config)>,
    ) -> gat_core::settings::SettingSource {
        use gat_core::settings::SettingSource;
        if self.overrides.contains(key) {
            return SettingSource::Environment(key);
        }
        for scope in [
            ConfigScope::Local,
            ConfigScope::Project,
            ConfigScope::Global,
        ] {
            let config = match candidate {
                Some((replaced, config)) if scope == replaced => config,
                _ => self.scoped(scope),
            };
            if key.is_set(config) {
                return SettingSource::Scope(scope);
            }
        }
        SettingSource::Default
    }
}

impl Repository {
    #[allow(
        clippy::missing_panics_doc,
        reason = "Catalog defaults are exhaustive and resolved cache paths are nonempty"
    )]
    pub fn read_setting(
        &self,
        key: gat_core::settings::SettingKey,
    ) -> Result<
        (
            gat_core::settings::SettingAssignment,
            gat_core::settings::SettingSource,
        ),
        RepositoryError,
    > {
        let layers = self.load_config_layers()?;
        let (value, source) = layers.setting(key);
        let value = if key == gat_core::settings::SettingKey::CacheLocation {
            let mut config = Config::default();
            if let Some(value) = value {
                value.apply(&mut config);
            }
            gat_core::settings::SettingAssignment::CacheLocation(
                gat_core::cache_location::CacheLocation::try_from_path(
                    self.resolved_cache_root_from(&config)
                        .display_path()
                        .to_path_buf(),
                )
                .expect("nonempty cache location"),
            )
        } else {
            value.expect("every scalar/list setting has a default")
        };
        Ok((value, source))
    }

    pub fn change_setting(
        &self,
        scope: ConfigScope,
        change: gat_core::settings::SettingChange,
    ) -> Result<gat_core::settings::SettingSource, RepositoryError> {
        let edit = self.begin_configuration_edit()?;
        let layers = self.load_config_layers()?;
        let key = match &change {
            gat_core::settings::SettingChange::Set(value) => value.key(),
            gat_core::settings::SettingChange::Unset(key) => *key,
        };
        let mut candidate = layers.scoped(scope).clone();
        match change {
            gat_core::settings::SettingChange::Set(assignment) => assignment.apply(&mut candidate),
            gat_core::settings::SettingChange::Unset(key) => key.unset(&mut candidate),
        }
        edit.commit(&layers, &candidate, scope)?;
        Ok(layers.setting_source(key, Some((scope, &candidate))))
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Repository {
    /// Fixture-only raw document publication; production uses semantic workflows.
    pub fn write_config_fixture(&self, config: &Config) -> Result<(), RepositoryError> {
        self.save_config(config)
    }
    pub fn write_scoped_config_fixture(
        &self,
        config: &Config,
        scope: ConfigScope,
    ) -> Result<(), RepositoryError> {
        self.save_config_scoped(config, scope)
    }
}

#[cfg(test)]
mod tests {
    use super::Repository as Repo;
    use super::*;
    use gat_core::lexical_path::GatPath;
    use gat_core::lock::Lock;
    use gat_core::oid::Oid;

    #[test]
    fn candidate_config_replaces_one_layer_without_changing_the_snapshot() {
        let definition = |name: &str| {
            let mut config = Config::default();
            config
                .selections
                .by_name
                .insert(name.into(), Default::default());
            config.selections.default = Some(name.into());
            config
        };
        let layers = ConfigLayers {
            overrides: Default::default(),
            layers: [
                definition("global"),
                definition("project"),
                definition("local"),
            ]
            .map(|config| CapturedConfig {
                config,
                revision: None,
            }),
        };
        let baseline = layers.effective().unwrap();
        let candidate = definition("replacement");
        for (scope, removed) in [
            (ConfigScope::Global, "global"),
            (ConfigScope::Project, "project"),
            (ConfigScope::Local, "local"),
        ] {
            let effective = layers.candidate_effective(scope, &candidate).unwrap();
            let mut expected = baseline.selections.by_name.clone();
            expected.remove(removed);
            expected.insert("replacement".into(), Default::default());
            assert_eq!(effective.selections.by_name, expected);
            assert_eq!(
                effective.selections.default,
                Some(
                    if scope == ConfigScope::Local {
                        "replacement"
                    } else {
                        "local"
                    }
                    .into()
                )
            );
            assert_eq!(layers.effective().unwrap(), baseline);
        }
        let mut invalid = candidate;
        invalid.selections.default = Some("missing".into());
        assert!(matches!(
            layers.candidate_effective(ConfigScope::Local, &invalid),
            Err(RepositoryError::InvalidEffectiveSelections(_))
        ));
        assert_eq!(layers.effective().unwrap(), baseline);
    }

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

        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .discover_from(nested)
            .unwrap();
        assert_eq!(
            repo.resolved_cache_root_from(&Config::default())
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
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
        let override_path = tmp.path().join("override-cache");
        let repo =
            crate::Invocation::from_pairs([("GAT_CACHE_LOCATION", override_path.as_os_str())])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
        let mut config = Config::default();
        config.cache.location = Some(
            gat_core::cache_location::CacheLocation::try_from_path("configured-cache".into())
                .expect("nonempty cache location"),
        );
        repo.save_config(&config).unwrap();
        assert_eq!(
            repo.resolved_cache_root().unwrap().display_path(),
            override_path
        );
    }

    /// `load_config` must fail closed on an invalid effective
    /// mount set even when it comes entirely from one hand-authored
    /// `gat.yaml`, not just a conflict introduced by `gat mount add`.
    #[test]
    fn load_config_fails_closed_on_hand_authored_overlapping_mount_targets() {
        let tmp = test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

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
    /// [`crate::Invocation::from_pairs`] entry point with a fake home
    /// directory, so no process-global environment mutation -- and no
    /// serializing mutex -- is needed at all. Real `HOME`/`USERPROFILE`
    /// wiring is covered once at the spawned-binary level instead (see
    /// `tests/cli_integration.rs`'s environment tests).
    #[test]
    fn load_config_merges_global_project_and_local_with_local_winning() {
        let fake_home = tempfile::tempdir().unwrap();
        let global_dir =
            gat_io::RepositoryLayout::global_config_dir_from(Some(fake_home.path())).unwrap();

        let tmp = test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let mut global_cfg = gat_core::config::Config::default();
        global_cfg.cache.location = Some(
            gat_core::cache_location::CacheLocation::try_from_path("global-cache".into())
                .expect("nonempty cache location"),
        );
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
        project_cfg.cache.location = Some(
            gat_core::cache_location::CacheLocation::try_from_path("project-cache".into())
                .expect("nonempty cache location"),
        );
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

        let repo = crate::Invocation::from_pairs([(
            if cfg!(windows) { "USERPROFILE" } else { "HOME" },
            fake_home.path().as_os_str(),
        )])
        .unwrap()
        .repository_at(tmp.path().to_path_buf());
        let merged = repo.load_config().unwrap();
        // project overrides global's `cache.location`...
        assert_eq!(
            merged.cache.location,
            Some(
                gat_core::cache_location::CacheLocation::try_from_path("project-cache".into())
                    .expect("nonempty cache location")
            )
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let mut project_cfg = gat_core::config::Config::default();
        project_cfg.cache.location = Some(
            gat_core::cache_location::CacheLocation::try_from_path("project-cache".into())
                .expect("nonempty cache location"),
        );
        repo.save_config_scoped(&project_cfg, ConfigScope::Project)
            .unwrap();

        let mut local_cfg = gat_core::config::Config::default();
        local_cfg.cache.location = Some(
            gat_core::cache_location::CacheLocation::try_from_path("local-cache".into())
                .expect("nonempty cache location"),
        );
        repo.save_config_scoped(&local_cfg, ConfigScope::Local)
            .unwrap();

        assert_eq!(
            repo.load_config_scoped(ConfigScope::Project)
                .unwrap()
                .cache
                .location,
            Some(
                gat_core::cache_location::CacheLocation::try_from_path("project-cache".into())
                    .expect("nonempty cache location")
            )
        );
        assert_eq!(
            repo.load_config_scoped(ConfigScope::Local)
                .unwrap()
                .cache
                .location,
            Some(
                gat_core::cache_location::CacheLocation::try_from_path("local-cache".into())
                    .expect("nonempty cache location")
            )
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
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

#[cfg(test)]
mod revision_tests {
    use super::*;

    #[test]
    fn stale_or_foreign_scoped_candidates_cannot_overwrite_a_document() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let invocation = crate::Invocation::from_pairs([] as [(&str, &str); 0]).unwrap();
        let repo = invocation.repository_at(first.path().to_path_buf());
        let foreign = invocation.repository_at(second.path().to_path_buf());
        let layers = repo.load_config_layers().unwrap();
        let mut candidate = Config::default();
        candidate.sync.auto_fetch = Some(true);
        let foreign_edit = foreign.begin_configuration_edit().unwrap();
        assert!(matches!(
            foreign_edit.commit(&layers, &candidate, ConfigScope::Project),
            Err(RepositoryError::ConfigurationChanged { .. })
        ));
        repo.save_config(&candidate).unwrap();
        let edit = repo.begin_configuration_edit().unwrap();
        assert!(matches!(
            edit.commit(&layers, &Config::default(), ConfigScope::Project),
            Err(RepositoryError::ConfigurationChanged {
                scope: ConfigScope::Project
            })
        ));
        assert_eq!(
            repo.load_config_scoped(ConfigScope::Project)
                .unwrap()
                .sync
                .auto_fetch,
            Some(true)
        );
    }

    #[test]
    fn changed_global_dependency_invalidates_a_project_candidate() {
        let home = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let repo = crate::Invocation::from_pairs([(
            if cfg!(windows) { "USERPROFILE" } else { "HOME" },
            home.path().as_os_str(),
        )])
        .unwrap()
        .repository_at(directory.path().to_path_buf());
        let edit = repo.begin_configuration_edit().unwrap();
        let layers = repo.load_config_layers().unwrap();
        let mut global = Config::default();
        global.sync.auto_fetch = Some(true);
        repo.save_config_scoped(&global, ConfigScope::Global)
            .unwrap();
        assert!(matches!(
            edit.commit(&layers, &Config::default(), ConfigScope::Project),
            Err(RepositoryError::ConfigurationChanged {
                scope: ConfigScope::Global
            })
        ));
    }
}
