//! `gat mount` orchestration over engine-owned source and mutation services.

use gat_core::config::{
    Config, ConfigScope, MountConfig, RESERVED_DEFAULT_ROUTE_NAME, RouteConfig, RoutesConfig,
};
use gat_core::git::{GitCommitId, GitRevisionSpec};
use gat_core::git_location::GitLocationSpec;
use gat_core::globs::GatGlobPattern;
use gat_core::lexical_path::{GatPath, GatSubpath};
use gat_core::name::{MountName, RemoteName, RouteName};
use gat_core::progress::ProgressReporter;
use gat_core::selection::Selection;
use gat_engine::{
    ConfigLayers, LockedMount, MountAdd, MountRemove, MountSourceError, MountSourceErrorKind,
    MountSourceLocation, MountUpdate, MountWorkflowError, PreparedMountSource, Repository,
    RepositoryError,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountRequest {
    List,
    Show {
        name: MountName,
    },
    Add {
        name: MountName,
        location: GitLocationSpec,
        target: Option<GatPath>,
        path: GatSubpath,
        revision: Option<GitRevisionSpec>,
        remote: Option<RemoteName>,
        no_setup: bool,
        include: Vec<GatGlobPattern>,
        exclude: Vec<GatGlobPattern>,
        scope: ConfigScope,
    },
    Update {
        name: MountName,
        location: Option<GitLocationSpec>,
        target: Option<GatPath>,
        path: Option<GatSubpath>,
        revision: Option<GitRevisionSpec>,
        remote: Option<RemoteName>,
        no_setup: bool,
        include: Option<Vec<GatGlobPattern>>,
        exclude: Option<Vec<GatGlobPattern>>,
        scope: ConfigScope,
    },
    Remove {
        name: MountName,
        detach_only: bool,
        scope: ConfigScope,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRecord {
    pub name: MountName,
    pub target: GatPath,
    pub location: GitLocationSpec,
    pub revision: Option<GitRevisionSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRouteBootstrap {
    pub name: RouteName,
    pub path: GatPath,
    pub remote: RemoteName,
    pub previous: Option<RemoteName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchedRoute {
    pub name: RouteName,
    pub path: GatPath,
    pub remote: RemoteName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountDetails {
    pub name: MountName,
    pub location: GitLocationSpec,
    pub path: GatSubpath,
    pub target: GatPath,
    pub revision: Option<GitRevisionSpec>,
    pub revision_lock: Option<GitCommitId>,
    pub include: Vec<GatGlobPattern>,
    pub exclude: Vec<GatGlobPattern>,
    pub tracked_rows: u64,
    pub route: Option<MatchedRoute>,
    pub default_remote: Option<RemoteName>,
    pub scope: ConfigScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountOutcome {
    List(Vec<MountRecord>),
    Show(Box<MountDetails>),
    Added {
        name: MountName,
        target: GatPath,
        entries: usize,
        route: Option<MountRouteBootstrap>,
    },
    Updated {
        name: MountName,
        target: GatPath,
        removed: usize,
        added: usize,
        route: Option<MountRouteBootstrap>,
    },
    Removed {
        name: MountName,
        target: GatPath,
        owned_rows: u64,
        detach_only: bool,
        route_remaining: Option<RouteName>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("the target contains root-owned desired rows")]
    RootOwnedAssets { path: GatPath },
    #[error("no mount named `{name}`")]
    NotFound { name: MountName },
    #[error("mount `{name}` already exists")]
    AlreadyExists { name: MountName },
    #[error("mount `{name}` is shadowed by {shadowing_scope}")]
    ShadowedOnAdd {
        name: MountName,
        shadowing_scope: ConfigScope,
    },
    #[error("mount `{name}` would shadow the definition at {existing_scope}")]
    WouldShadowExisting {
        name: MountName,
        existing_scope: ConfigScope,
    },
    #[error("mount `{name}` is shadowed by {shadowing_scope}")]
    ShadowedOnMutate {
        name: MountName,
        shadowing_scope: ConfigScope,
    },
    #[error("removing mount `{name}` would change ownership under `{revealed_target}`")]
    RemovalWouldRevealOwnershipChange {
        name: MountName,
        revealed_scope: ConfigScope,
        revealed_target: GatPath,
    },
    #[error("mount `{name}` is defined at {actual_scope}, not {requested_scope}")]
    WrongScope {
        name: MountName,
        requested_scope: ConfigScope,
        actual_scope: ConfigScope,
    },
    #[error("no gat remote named `{name}`")]
    ExplicitRemoteNotFound { name: RemoteName },
    #[error("route `{route}` is defined above the requested write scope")]
    RouteScopeConflict {
        route: RouteName,
        target: GatPath,
        remote: RemoteName,
        defining_scope: ConfigScope,
        write_scope: ConfigScope,
    },
    #[error("could not prepare mount source")]
    Source {
        kind: MountSourceErrorKind,
        location: GitLocationSpec,
        revision: Option<GitRevisionSpec>,
        #[source]
        source: Box<MountSourceError>,
    },
    #[error(transparent)]
    Repository(Box<RepositoryError>),
    #[error(transparent)]
    Workflow(Box<MountWorkflowError>),
}

impl From<RepositoryError> for MountError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(Box::new(error))
    }
}

impl From<MountWorkflowError> for MountError {
    fn from(error: MountWorkflowError) -> Self {
        Self::Workflow(Box::new(error))
    }
}

type Result<T> = std::result::Result<T, MountError>;

pub fn mount(
    repo: &Repository,
    request: MountRequest,
    progress: &dyn ProgressReporter,
) -> Result<MountOutcome> {
    match request {
        MountRequest::List => repo.mounts().with_locked_config(progress, |_, _, config| {
            Ok(MountOutcome::List(
                config
                    .mounts
                    .by_name
                    .into_iter()
                    .map(|(name, mount)| MountRecord {
                        name,
                        target: mount.target,
                        location: mount.url,
                        revision: mount.rev,
                    })
                    .collect(),
            ))
        }),
        MountRequest::Show { name } => {
            repo.mounts()
                .with_locked_config(progress, |mount, layers, mut effective| {
                    let source = effective
                        .mounts
                        .by_name
                        .remove(&name)
                        .ok_or_else(|| MountError::NotFound { name: name.clone() })?;
                    let tracked_rows = mount.desired_count(&source.target)?;
                    let route =
                        effective
                            .routes
                            .route_for(&source.target)
                            .map(|route| MatchedRoute {
                                name: route.name.clone(),
                                path: route.route.clone(),
                                remote: route.remote.clone(),
                            });
                    let scope = layers
                        .mount_defining_scope(&name)
                        .unwrap_or(ConfigScope::Project);
                    Ok(MountOutcome::Show(Box::new(MountDetails {
                        name,
                        location: source.url,
                        path: source.path,
                        target: source.target,
                        revision: source.rev,
                        revision_lock: source.rev_lock,
                        include: source.include,
                        exclude: source.exclude,
                        tracked_rows,
                        route,
                        default_remote: effective.remotes.default,
                        scope,
                    })))
                })
        }
        MountRequest::Add {
            name,
            location,
            target,
            path,
            revision,
            remote,
            no_setup,
            include,
            exclude,
            scope,
        } => {
            // Avoid source preparation for rejected names; recheck under the
            // mutation lock below because configuration can change meanwhile.
            {
                let layers = repo.load_config_layers()?;
                if layers.scoped(scope).mounts.by_name.contains_key(&name) {
                    return Err(MountError::AlreadyExists { name });
                }
                ensure_add_not_shadowed(&layers, &name, scope)?;
            }
            let parsed = parse_source(location.clone(), revision.clone())?;
            let target = match target {
                Some(target) => target,
                None => parsed
                    .inferred_target()
                    .map_err(|error| source_error(error, location.clone(), revision.clone()))?,
            };
            let prepared = parsed
                .prepare(revision.as_ref(), progress)
                .map_err(|error| source_error(error, location.clone(), revision.clone()))?;

            repo.mounts()
                .with_locked_config(progress, |mount, layers, _effective_config| {
                    let mut scoped = layers.scoped(scope).clone();
                    if scoped.mounts.by_name.contains_key(&name) {
                        return Err(MountError::AlreadyExists { name });
                    }
                    ensure_add_not_shadowed(&layers, &name, scope)?;

                    let pre_config = scoped.clone();
                    scoped.mounts.by_name.insert(
                        name.clone(),
                        MountConfig {
                            url: location,
                            target: target.clone(),
                            path: path.clone(),
                            rev: revision,
                            rev_lock: Some(prepared.rev_lock()),
                            include: include.clone(),
                            exclude: exclude.clone(),
                        },
                    );
                    let effective = layers.candidate_effective(scope, &scoped)?;
                    check_no_root_owned_assets(mount, &target, None)?;
                    let route = if no_setup {
                        None
                    } else {
                        bootstrap_target_route(
                            &layers,
                            scope,
                            &mut scoped,
                            &effective,
                            prepared.config(),
                            remote.as_ref(),
                            &target,
                            &name,
                        )?
                    };
                    let post_effective = if no_setup {
                        effective
                    } else {
                        layers.candidate_effective(scope, &scoped)?
                    };
                    let selection =
                        Selection::from_scope_patterns(path.into_path_scope(), include, exclude);
                    let applied = mount.add(MountAdd::new(
                        scope,
                        name.clone(),
                        target.clone(),
                        pre_config,
                        scoped,
                        post_effective,
                        prepared.rows(selection),
                    ))?;
                    Ok(MountOutcome::Added {
                        name,
                        target,
                        entries: applied.imported,
                        route,
                    })
                })
        }
        MountRequest::Update {
            name,
            location,
            target,
            path,
            revision,
            remote,
            no_setup,
            include,
            exclude,
            scope,
        } => update_mount(
            repo,
            UpdateRequest {
                name,
                location,
                target,
                path,
                revision,
                remote,
                no_setup,
                include,
                exclude,
                scope,
            },
            progress,
        ),
        MountRequest::Remove {
            name,
            detach_only,
            scope,
        } => repo
            .mounts()
            .with_locked_config(progress, |mount, layers, _effective| {
                let scoped = layers.scoped(scope);
                let mut post_config = scoped.clone();
                let existing = post_config
                    .mounts
                    .by_name
                    .remove(&name)
                    .ok_or_else(|| wrong_scope_or_not_found(&layers, &name, scope))?;
                ensure_not_shadowed(&layers, &name, scope)?;

                let post_effective = layers.candidate_effective(scope, &post_config)?;
                ensure_removal_does_not_reveal_ownership_change(
                    &layers,
                    mount,
                    &name,
                    scope,
                    &existing.target,
                )?;
                let route_remaining = find_route_by_path(&post_effective.routes, &existing.target)
                    .map(|(name, _)| name.clone());
                let owned_rows = if detach_only {
                    let count = mount.desired_count(&existing.target)?;
                    mount.detach(scope, &post_config)?;
                    count
                } else {
                    mount
                        .remove(MountRemove::new(
                            scope,
                            name.clone(),
                            existing.target.clone(),
                            scoped.clone(),
                            post_config,
                            post_effective,
                        ))?
                        .removed as u64
                };
                Ok(MountOutcome::Removed {
                    name,
                    target: existing.target,
                    owned_rows,
                    detach_only,
                    route_remaining,
                })
            }),
    }
}

struct UpdateRequest {
    name: MountName,
    location: Option<GitLocationSpec>,
    target: Option<GatPath>,
    path: Option<GatSubpath>,
    revision: Option<GitRevisionSpec>,
    remote: Option<RemoteName>,
    no_setup: bool,
    include: Option<Vec<GatGlobPattern>>,
    exclude: Option<Vec<GatGlobPattern>>,
    scope: ConfigScope,
}

fn update_mount(
    repo: &Repository,
    request: UpdateRequest,
    progress: &dyn ProgressReporter,
) -> Result<MountOutcome> {
    // Reject invalid requests before potentially cloning a source. The locked
    // checks below remain authoritative if configuration changes meanwhile.
    let (provisional_location, provisional_revision) = {
        let layers = repo.load_config_layers()?;
        let provisional = layers
            .scoped(request.scope)
            .mounts
            .by_name
            .get(&request.name)
            .ok_or_else(|| wrong_scope_or_not_found(&layers, &request.name, request.scope))?;
        ensure_not_shadowed(&layers, &request.name, request.scope)?;
        (
            request
                .location
                .clone()
                .unwrap_or_else(|| provisional.url.clone()),
            request.revision.clone().or_else(|| provisional.rev.clone()),
        )
    };
    let mut prepared = Some((
        provisional_location.clone(),
        provisional_revision.clone(),
        prepare_source(
            provisional_location,
            provisional_revision.as_ref(),
            progress,
        )?,
    ));

    enum Attempt {
        Applied(Box<MountOutcome>),
        Reprepare {
            location: GitLocationSpec,
            revision: Option<GitRevisionSpec>,
        },
    }

    loop {
        let attempt = repo.mounts().with_locked_config(
            progress,
            |mount, layers, _effective_config| -> Result<Attempt> {
                let pre_config = layers.scoped(request.scope);
                let existing = pre_config
                    .mounts
                    .by_name
                    .get(&request.name)
                    .ok_or_else(|| {
                        wrong_scope_or_not_found(&layers, &request.name, request.scope)
                    })?;
                ensure_not_shadowed(&layers, &request.name, request.scope)?;
                let location = request
                    .location
                    .clone()
                    .unwrap_or_else(|| existing.url.clone());
                let revision = request.revision.clone().or_else(|| existing.rev.clone());
                let prepared_source = match prepared.take() {
                    Some((prepared_location, prepared_revision, source))
                        if prepared_location == location && prepared_revision == revision =>
                    {
                        source
                    }
                    _ => return Ok(Attempt::Reprepare { location, revision }),
                };
                let target = request
                    .target
                    .clone()
                    .unwrap_or_else(|| existing.target.clone());
                let path = request
                    .path
                    .clone()
                    .unwrap_or_else(|| existing.path.clone());
                let include = request
                    .include
                    .clone()
                    .unwrap_or_else(|| existing.include.clone());
                let exclude = request
                    .exclude
                    .clone()
                    .unwrap_or_else(|| existing.exclude.clone());

                let mut scoped = pre_config.clone();
                scoped.mounts.by_name.insert(
                    request.name.clone(),
                    MountConfig {
                        url: location,
                        target: target.clone(),
                        path: path.clone(),
                        rev: revision,
                        rev_lock: Some(prepared_source.rev_lock()),
                        include: include.clone(),
                        exclude: exclude.clone(),
                    },
                );
                let effective = layers.candidate_effective(request.scope, &scoped)?;
                if target != existing.target {
                    check_no_root_owned_assets(mount, &target, Some(&existing.target))?;
                }
                let route = if request.no_setup {
                    None
                } else {
                    bootstrap_target_route(
                        &layers,
                        request.scope,
                        &mut scoped,
                        &effective,
                        prepared_source.config(),
                        request.remote.as_ref(),
                        &target,
                        &request.name,
                    )?
                };
                let post_effective = if request.no_setup {
                    effective
                } else {
                    layers.candidate_effective(request.scope, &scoped)?
                };
                let selection =
                    Selection::from_scope_patterns(path.into_path_scope(), include, exclude);
                let applied = mount.update(MountUpdate::new(
                    request.scope,
                    request.name.clone(),
                    existing.target.clone(),
                    target.clone(),
                    pre_config.clone(),
                    scoped,
                    post_effective,
                    prepared_source.rows(selection),
                ))?;
                Ok(Attempt::Applied(Box::new(MountOutcome::Updated {
                    name: request.name.clone(),
                    target,
                    removed: applied.removed,
                    added: applied.imported,
                    route,
                })))
            },
        )?;

        match attempt {
            Attempt::Applied(outcome) => return Ok(*outcome),
            Attempt::Reprepare { location, revision } => {
                let source = prepare_source(location.clone(), revision.as_ref(), progress)?;
                prepared = Some((location, revision, source));
            }
        }
    }
}

fn parse_source(
    location: GitLocationSpec,
    revision: Option<GitRevisionSpec>,
) -> Result<MountSourceLocation> {
    MountSourceLocation::parse(location.clone())
        .map_err(|error| source_error(error, location, revision))
}

fn prepare_source(
    location: GitLocationSpec,
    revision: Option<&GitRevisionSpec>,
    progress: &dyn ProgressReporter,
) -> Result<PreparedMountSource> {
    let parsed = parse_source(location.clone(), revision.cloned())?;
    parsed
        .prepare(revision, progress)
        .map_err(|error| source_error(error, location, revision.cloned()))
}

fn source_error(
    source: MountSourceError,
    location: GitLocationSpec,
    revision: Option<GitRevisionSpec>,
) -> MountError {
    MountError::Source {
        kind: source.kind(),
        location,
        revision,
        source: Box::new(source),
    }
}

fn check_no_root_owned_assets(
    mount: &mut LockedMount<'_, '_>,
    path: &GatPath,
    exclude_prefix: Option<&GatPath>,
) -> Result<()> {
    if mount.has_root_owned_assets(path, exclude_prefix)? {
        Err(MountError::RootOwnedAssets { path: path.clone() })
    } else {
        Ok(())
    }
}

fn more_specific_scope(
    layers: &ConfigLayers,
    name: &MountName,
    scope: ConfigScope,
) -> Option<ConfigScope> {
    let scopes = match scope {
        ConfigScope::Global => [Some(ConfigScope::Local), Some(ConfigScope::Project)],
        ConfigScope::Project => [Some(ConfigScope::Local), None],
        ConfigScope::Local => [None, None],
    };
    scopes
        .into_iter()
        .flatten()
        .find(|other| layers.scoped(*other).mounts.by_name.contains_key(name))
}

fn less_specific_scope(
    layers: &ConfigLayers,
    name: &MountName,
    scope: ConfigScope,
) -> Option<ConfigScope> {
    let scopes = match scope {
        ConfigScope::Local => [Some(ConfigScope::Project), Some(ConfigScope::Global)],
        ConfigScope::Project => [Some(ConfigScope::Global), None],
        ConfigScope::Global => [None, None],
    };
    scopes
        .into_iter()
        .flatten()
        .find(|other| layers.scoped(*other).mounts.by_name.contains_key(name))
}

fn ensure_add_not_shadowed(
    layers: &ConfigLayers,
    name: &MountName,
    scope: ConfigScope,
) -> Result<()> {
    if let Some(shadowing_scope) = more_specific_scope(layers, name, scope) {
        return Err(MountError::ShadowedOnAdd {
            name: name.clone(),
            shadowing_scope,
        });
    }
    if let Some(existing_scope) = less_specific_scope(layers, name, scope) {
        return Err(MountError::WouldShadowExisting {
            name: name.clone(),
            existing_scope,
        });
    }
    Ok(())
}

fn ensure_not_shadowed(layers: &ConfigLayers, name: &MountName, scope: ConfigScope) -> Result<()> {
    if let Some(shadowing_scope) = more_specific_scope(layers, name, scope) {
        return Err(MountError::ShadowedOnMutate {
            name: name.clone(),
            shadowing_scope,
        });
    }
    Ok(())
}

fn wrong_scope_or_not_found(
    layers: &ConfigLayers,
    name: &MountName,
    requested_scope: ConfigScope,
) -> MountError {
    match layers.mount_defining_scope(name) {
        Some(actual_scope) if actual_scope != requested_scope => MountError::WrongScope {
            name: name.clone(),
            requested_scope,
            actual_scope,
        },
        _ => MountError::NotFound { name: name.clone() },
    }
}

fn ensure_removal_does_not_reveal_ownership_change(
    layers: &ConfigLayers,
    mount: &mut LockedMount<'_, '_>,
    name: &MountName,
    scope: ConfigScope,
    removed_target: &GatPath,
) -> Result<()> {
    let Some(revealed_scope) = less_specific_scope(layers, name, scope) else {
        return Ok(());
    };
    let Some(revealed) = layers.scoped(revealed_scope).mounts.by_name.get(name) else {
        return Ok(());
    };
    if revealed.target != *removed_target && mount.desired_any(&revealed.target)? {
        return Err(MountError::RemovalWouldRevealOwnershipChange {
            name: name.clone(),
            revealed_scope,
            revealed_target: revealed.target.clone(),
        });
    }
    Ok(())
}

fn find_route_by_path<'a>(
    routes: &'a RoutesConfig,
    path: &GatPath,
) -> Option<(&'a RouteName, &'a RouteConfig)> {
    routes.by_name.iter().find(|(_, route)| route.path == *path)
}

#[allow(clippy::too_many_arguments)]
fn bootstrap_target_route(
    layers: &ConfigLayers,
    write_scope: ConfigScope,
    scoped: &mut Config,
    effective: &Config,
    source: &Config,
    explicit: Option<&RemoteName>,
    target: &GatPath,
    mount_name: &MountName,
) -> Result<Option<MountRouteBootstrap>> {
    let remote = if let Some(name) = explicit {
        resolve_explicit_remote(scoped, effective, source, name)?
    } else {
        if find_route_by_path(&effective.routes, target).is_some() {
            return Ok(None);
        }
        let Some(remote) = resolve_default_remote(scoped, effective, source) else {
            return Ok(None);
        };
        remote
    };
    write_route_if_changed(
        layers,
        write_scope,
        scoped,
        effective,
        target,
        &remote,
        mount_name,
    )
}

fn resolve_explicit_remote(
    scoped: &mut Config,
    effective: &Config,
    source: &Config,
    name: &RemoteName,
) -> Result<RemoteName> {
    if effective.remotes.by_name.contains_key(name) {
        return Ok(name.clone());
    }
    if let Some(url) = source.remotes.by_name.get(name) {
        return Ok(reuse_by_url_or_import(scoped, effective, name, &url.url));
    }
    Err(MountError::ExplicitRemoteNotFound { name: name.clone() })
}

fn resolve_default_remote(
    scoped: &mut Config,
    effective: &Config,
    source: &Config,
) -> Option<RemoteName> {
    let name = source.remotes.default.as_ref()?;
    let url = source.remotes.by_name.get(name)?;
    Some(reuse_by_url_or_import(scoped, effective, name, &url.url))
}

fn reuse_by_url_or_import(
    scoped: &mut Config,
    effective: &Config,
    preferred: &RemoteName,
    url: &gat_core::endpoint::RemoteUrlTemplate,
) -> RemoteName {
    if let Some(name) = effective
        .remotes
        .by_name
        .iter()
        .find(|(_, existing)| existing.url.as_template_str() == url.as_template_str())
        .map(|(name, _)| name.clone())
    {
        return name;
    }
    let mut candidate = preferred.to_string();
    let mut suffix = 1;
    while effective.remotes.by_name.contains_key(candidate.as_str())
        || scoped.remotes.by_name.contains_key(candidate.as_str())
    {
        suffix += 1;
        candidate = format!("{preferred}-{suffix}");
    }
    let candidate = RemoteName::from_string(candidate);
    scoped
        .remotes
        .by_name
        .insert(candidate.clone(), url.clone().into());
    candidate
}

fn find_effective_route_by_path(
    layers: &ConfigLayers,
    effective: &Config,
    path: &GatPath,
) -> Option<(RouteName, RemoteName, ConfigScope)> {
    let (name, route) = find_route_by_path(&effective.routes, path)?;
    let scope = crate::resource::defining_scope(layers, |c| c.routes.by_name.contains_key(name))?;
    Some((name.clone(), route.remote.clone(), scope))
}

fn write_route_if_changed(
    layers: &ConfigLayers,
    write_scope: ConfigScope,
    scoped: &mut Config,
    effective: &Config,
    target: &GatPath,
    remote: &RemoteName,
    mount_name: &MountName,
) -> Result<Option<MountRouteBootstrap>> {
    let existing = find_effective_route_by_path(layers, effective, target);
    let previous = existing.as_ref().map(|(_, remote, _)| remote.clone());
    if previous.as_ref() == Some(remote) {
        return Ok(None);
    }
    if let Some((name, _, defining_scope)) = &existing
        && write_scope.precedence() < defining_scope.precedence()
    {
        return Err(MountError::RouteScopeConflict {
            route: name.clone(),
            target: target.clone(),
            remote: remote.clone(),
            defining_scope: *defining_scope,
            write_scope,
        });
    }
    let route_name = if let Some((name, _, _)) = existing {
        name
    } else {
        let mut candidate = mount_name.to_string();
        let mut suffix = 1;
        while candidate == RESERVED_DEFAULT_ROUTE_NAME
            || effective.routes.by_name.contains_key(candidate.as_str())
            || scoped.routes.by_name.contains_key(candidate.as_str())
        {
            suffix += 1;
            candidate = format!("{mount_name}-{suffix}");
        }
        RouteName::from_string(candidate)
    };
    scoped.routes.by_name.insert(
        route_name.clone(),
        RouteConfig {
            path: target.clone(),
            remote: remote.clone(),
        },
    );
    Ok(Some(MountRouteBootstrap {
        name: route_name,
        path: target.clone(),
        remote: remote.clone(),
        previous,
    }))
}
