//! `gat route` orchestration over repository-scoped configuration services.

use gat_core::config::{Config, ConfigScope, RESERVED_DEFAULT_ROUTE_NAME, RouteConfig};
use gat_core::lexical_path::GatPath;
use gat_core::name::{RemoteName, RouteName};
use gat_engine::Repository;
use gat_engine::RepositoryError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteRequest {
    List,
    Add {
        name: RouteName,
        remote: RemoteName,
        path: GatPath,
        scope: ConfigScope,
    },
    Update {
        name: RouteName,
        remote: Option<RemoteName>,
        path: Option<GatPath>,
        scope: ConfigScope,
    },
    Remove {
        name: RouteName,
        scope: ConfigScope,
    },
    Show {
        name: RouteName,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteRecord {
    pub name: RouteName,
    pub path: GatPath,
    pub remote: RemoteName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultRemoteRoute {
    Configured { remote: RemoteName },
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteDetails {
    pub name: RouteName,
    pub path: GatPath,
    pub remote: RemoteName,
    pub scope: ConfigScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteOutcome {
    List {
        routes: Vec<RouteRecord>,
        default_remote: DefaultRemoteRoute,
    },
    Added {
        name: RouteName,
        path: GatPath,
        remote: RemoteName,
    },
    Updated {
        name: RouteName,
        path: GatPath,
        remote: RemoteName,
    },
    Removed {
        name: RouteName,
        revealed: Option<ConfigScope>,
    },
    Show(RouteDetails),
}

#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error(transparent)]
    Scope(#[from] crate::resource::ResourceScopeError),
    #[error("no remote named `{remote}`")]
    UnknownRemote { remote: RemoteName },

    #[error("route name `*` is reserved for `gat route list`'s synthetic default-remote row")]
    ReservedName,

    #[error("route `{name}` already exists")]
    AlreadyExists { name: RouteName },

    #[error("no route named `{name}`")]
    NotFound { name: RouteName },

    #[error(transparent)]
    Repository(Box<RepositoryError>),
}

impl From<RepositoryError> for RouteError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(Box::new(error))
    }
}

type Result<T> = std::result::Result<T, RouteError>;

#[allow(
    clippy::missing_panics_doc,
    reason = "Every effective definition comes from a loaded configuration layer"
)]
pub fn route(repo: &Repository, request: RouteRequest) -> Result<RouteOutcome> {
    match request {
        RouteRequest::List => {
            let cfg = repo.load_config()?;
            let routes = cfg
                .routes
                .by_name
                .into_iter()
                .map(|(name, route)| RouteRecord {
                    name,
                    path: route.path,
                    remote: route.remote,
                })
                .collect();
            let default_remote = match cfg.remotes.default {
                Some(remote) => DefaultRemoteRoute::Configured { remote },
                None => DefaultRemoteRoute::Missing,
            };
            Ok(RouteOutcome::List {
                routes,
                default_remote,
            })
        }
        RouteRequest::Add {
            name,
            remote,
            path,
            scope,
        } => {
            let layers = repo.load_config_layers()?;
            crate::resource::check_add_scope(
                &layers,
                crate::resource::ResourceKind::Route,
                name.as_str(),
                scope,
                |c| c.routes.by_name.contains_key(&name),
            )?;
            if name.as_str() == RESERVED_DEFAULT_ROUTE_NAME {
                return Err(RouteError::ReservedName);
            }
            validate_remote_exists(&layers.effective()?, &remote)?;
            let mut cfg = layers.scoped(scope).clone();
            if cfg.routes.by_name.contains_key(&name) {
                return Err(RouteError::AlreadyExists { name });
            }
            cfg.routes.by_name.insert(
                name.clone(),
                RouteConfig {
                    path: path.clone(),
                    remote: remote.clone(),
                },
            );
            layers.candidate_effective(scope, &cfg)?;
            repo.save_config_scoped(&cfg, scope)?;
            Ok(RouteOutcome::Added { name, path, remote })
        }
        RouteRequest::Update {
            name,
            remote,
            path,
            scope,
        } => {
            let layers = repo.load_config_layers()?;
            crate::resource::check_scope(
                &layers,
                crate::resource::ResourceKind::Route,
                name.as_str(),
                scope,
                |c| c.routes.by_name.contains_key(&name),
            )?;
            let effective = layers.effective()?;
            if let Some(remote) = &remote {
                validate_remote_exists(&effective, remote)?;
            }
            let mut cfg = layers.scoped(scope).clone();
            let existing = cfg
                .routes
                .by_name
                .get(&name)
                .cloned()
                .ok_or_else(|| RouteError::NotFound { name: name.clone() })?;
            let path = path.unwrap_or(existing.path);
            let remote = remote.unwrap_or(existing.remote);
            cfg.routes.by_name.insert(
                name.clone(),
                RouteConfig {
                    path: path.clone(),
                    remote: remote.clone(),
                },
            );
            layers.candidate_effective(scope, &cfg)?;
            repo.save_config_scoped(&cfg, scope)?;
            Ok(RouteOutcome::Updated { name, path, remote })
        }
        RouteRequest::Remove { name, scope } => {
            let layers = repo.load_config_layers()?;
            crate::resource::check_scope(
                &layers,
                crate::resource::ResourceKind::Route,
                name.as_str(),
                scope,
                |c| c.routes.by_name.contains_key(&name),
            )?;
            let mut cfg = layers.scoped(scope).clone();
            if cfg.routes.by_name.remove(&name).is_none() {
                return Err(RouteError::NotFound { name });
            }
            layers.candidate_effective(scope, &cfg)?;
            repo.save_config_scoped(&cfg, scope)?;
            Ok(RouteOutcome::Removed {
                revealed: crate::resource::revealed_scope(&layers, scope, |c| {
                    c.routes.by_name.contains_key(&name)
                }),
                name,
            })
        }
        RouteRequest::Show { name } => {
            let layers = repo.load_config_layers()?;
            let cfg = layers.effective()?;
            let route = cfg
                .routes
                .by_name
                .get(&name)
                .cloned()
                .ok_or_else(|| RouteError::NotFound { name: name.clone() })?;
            let scope =
                crate::resource::defining_scope(&layers, |c| c.routes.by_name.contains_key(&name))
                    .expect("effective definition comes from a layer");
            Ok(RouteOutcome::Show(RouteDetails {
                name,
                path: route.path,
                remote: route.remote,
                scope,
            }))
        }
    }
}

fn validate_remote_exists(cfg: &Config, remote: &RemoteName) -> Result<()> {
    if cfg.remotes.by_name.contains_key(remote) {
        Ok(())
    } else {
        Err(RouteError::UnknownRemote {
            remote: remote.clone(),
        })
    }
}
