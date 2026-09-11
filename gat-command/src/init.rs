//! Pure `gat init` orchestration.

use gat_engine::{
    InitializationError, IntegrationStatus, ManagedHook, Repository, ResolvedCacheLocation,
};

static EXAMPLE_CONFIG: std::sync::LazyLock<gat_core::config::Config> =
    std::sync::LazyLock::new(|| {
        use gat_core::config::{
            CacheConfig, Config, GitConfig, LockConfig, MaterializationStrategy, RemotesConfig,
            SyncConfig,
        };

        let origin = gat_core::name::RemoteName::from_string("origin".into());
        Config {
            remotes: RemotesConfig {
                default: None,
                by_name: [(
                    origin,
                    gat_core::endpoint::RemoteUrlTemplate::from_string("s3://bucket/prefix".into())
                        .into(),
                )]
                .into(),
            },
            cache: CacheConfig {
                location: Some(
                    gat_core::cache_location::CacheLocation::try_from_path("/var/cache/gat".into())
                        .expect("nonempty cache location"),
                ),
                materialization_strategy: Some(
                    MaterializationStrategy::from_values(&["hardlink", "copy"])
                        .expect("valid example materialization strategy"),
                ),
                ..CacheConfig::default()
            },
            sync: SyncConfig {
                auto_fetch: Some(true),
                auto_repair: Some(true),
                ..SyncConfig::default()
            },
            selections: gat_core::config::SelectionsConfig {
                by_name: std::collections::BTreeMap::from([(
                    "example".into(),
                    gat_core::config::SelectionConfig {
                        include: vec![
                            gat_core::globs::GatGlobPattern::parse("data/**")
                                .expect("valid example include glob"),
                        ],
                        exclude: vec![
                            gat_core::globs::GatGlobPattern::parse("data/tmp/**")
                                .expect("valid example exclude glob"),
                        ],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
            lock: LockConfig {
                shard_levels: Some(gat_core::lock::LockShardLevels::FLAT),
            },
            git: GitConfig {
                ignore_patterns: Some(vec![
                    gat_core::git_ignore::GitIgnorePattern::parse("*.safetensors")
                        .expect("valid example ignore pattern"),
                ]),
            },
            ..Config::default()
        }
    });

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InitRequest {
    pub no_hooks: bool,
    pub no_merge_driver: bool,
    pub example_config: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitHooksOutcome {
    Installed(Vec<ManagedHook>),
    AlreadyInstalled,
    Removed(Vec<ManagedHook>),
    AlreadyAbsent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitGitIntegrationOutcome {
    Installed,
    AlreadyInstalled,
    Removed,
    AlreadyAbsent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitConfigOutcome {
    Created,
    AlreadyPresent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitOutcome {
    pub cache_location: ResolvedCacheLocation,
    pub hooks: InitHooksOutcome,
    pub merge_driver: InitGitIntegrationOutcome,
    pub attributes: InitGitIntegrationOutcome,
    pub config: Option<InitConfigOutcome>,
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error(transparent)]
    Repository(Box<gat_engine::RepositoryError>),
    #[error(transparent)]
    Engine(#[from] InitializationError),
}

pub fn init(repo: &Repository, request: InitRequest) -> Result<InitOutcome, InitError> {
    let service = repo.initialization()?;
    let (merge_driver, attributes) = if request.no_merge_driver {
        let attributes = removed_status(service.uninstall_merge_attributes()?);
        let merge_driver = removed_status(service.uninstall_merge_driver()?);
        (merge_driver, attributes)
    } else {
        let merge_driver = installed_status(service.install_merge_driver()?);
        let attributes = installed_status(service.install_merge_attributes()?);
        (merge_driver, attributes)
    };
    let hooks = if request.no_hooks {
        let changes = service.uninstall_hooks()?.changed;
        if changes.is_empty() {
            InitHooksOutcome::AlreadyAbsent
        } else {
            InitHooksOutcome::Removed(changes)
        }
    } else {
        let changes = service.install_hooks()?.changed;
        if changes.is_empty() {
            InitHooksOutcome::AlreadyInstalled
        } else {
            InitHooksOutcome::Installed(changes)
        }
    };
    let config = if request.example_config {
        Some(
            if service.create_project_config_if_absent(&EXAMPLE_CONFIG)? {
                InitConfigOutcome::Created
            } else {
                InitConfigOutcome::AlreadyPresent
            },
        )
    } else {
        None
    };
    let cache_location = service
        .initialize_cache()
        .map_err(|e| InitError::Repository(Box::new(e)))?;
    Ok(InitOutcome {
        cache_location,
        hooks,
        merge_driver,
        attributes,
        config,
    })
}

const fn installed_status(status: IntegrationStatus) -> InitGitIntegrationOutcome {
    match status {
        IntegrationStatus::Changed => InitGitIntegrationOutcome::Installed,
        IntegrationStatus::Unchanged => InitGitIntegrationOutcome::AlreadyInstalled,
    }
}

const fn removed_status(status: IntegrationStatus) -> InitGitIntegrationOutcome {
    match status {
        IntegrationStatus::Changed => InitGitIntegrationOutcome::Removed,
        IntegrationStatus::Unchanged => InitGitIntegrationOutcome::AlreadyAbsent,
    }
}
