use crate::ownership::{OwnershipError, assert_no_first_owned_match, assert_root_owned};
use crate::parallel;
use gat_core::globs::{GatGlobPattern, GlobBound, GlobError};
use gat_core::lexical_path::{GatPath, LexicalPathError};
use gat_core::lock::path_matches_scope;
use gat_core::path_scope::{self, PathScope};
use gat_core::progress::{
    NoopProgress, ProgressActivity, ProgressOperation, ProgressReporter, ProgressSpec,
    with_progress_typed,
};
use gat_engine::{
    DesiredScope, EffectivePathPolicy, PathPolicyError, RemoteCatalog, RemoteCatalogError,
    Repository, RepositoryMutationError, WorktreePathError, WorktreeRemoveError, remove_and_prune,
    validate_mutation_path,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveRequest {
    pub paths: Vec<PathScope>,
    pub cached: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveOutcome {
    pub paths: Vec<GatPath>,
}

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error(transparent)]
    MountOwned(#[from] OwnershipError),
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error(transparent)]
    Lexical(#[from] LexicalPathError),
    #[error(transparent)]
    Glob(#[from] GlobError),
    #[error(transparent)]
    Acquisition(Box<gat_engine::RepoSnapshotError>),
    #[error(transparent)]
    PathPolicy(#[from] PathPolicyError),
    #[error(transparent)]
    RemoteCatalog(#[from] RemoteCatalogError),
    #[error(transparent)]
    RepositoryMutation(#[from] Box<RepositoryMutationError>),
    #[error("working-tree cleanup failed after publishing removals")]
    Cleanup(#[source] Box<WorktreeRemoveError>),
    #[error("materialized ownership cleanup failed after publishing removals")]
    ForgetMaterialized {
        cached: bool,
        #[source]
        source: Box<RepositoryMutationError>,
    },
    #[error("exclude regeneration failed after publishing removals")]
    SyncExcludes(#[source] Box<RepositoryMutationError>),
}

impl From<gat_engine::RepoSnapshotError> for RemoveError {
    fn from(error: gat_engine::RepoSnapshotError) -> Self {
        Self::Acquisition(Box::new(error))
    }
}

impl From<RepositoryMutationError> for RemoveError {
    fn from(error: RepositoryMutationError) -> Self {
        Self::RepositoryMutation(Box::new(error))
    }
}

type Result<T> = std::result::Result<T, RemoveError>;

pub fn remove(repo: &Repository, request: RemoveRequest) -> Result<RemoveOutcome> {
    remove_with_progress(repo, request, &NoopProgress)
}

pub fn remove_with_progress(
    repo: &Repository,
    request: RemoveRequest,
    progress: &dyn ProgressReporter,
) -> Result<RemoveOutcome> {
    repo.with_desired_mutation(progress, |cfg, mut desired| {
        let catalog = RemoteCatalog::from_config(&cfg.remotes)?;
        let policy = EffectivePathPolicy::from_config(cfg, &catalog)?;
        let plan = with_progress_typed(
            progress,
            ProgressSpec::indeterminate(ProgressOperation::ResolvingSelection),
            |task| -> Result<(Vec<RemoveSelector>, Vec<GatPath>)> {
                task.handle()
                    .set_activity(ProgressActivity::ClassifyingSelectors);
                let mut selectors = Vec::with_capacity(request.paths.len());
                for scope in request.paths {
                    let path = match scope {
                        PathScope::Root => {
                            selectors.push(RemoveSelector::Root);
                            continue;
                        }
                        PathScope::Path(path) => path,
                    };
                    let is_glob = path_scope::has_glob_metacharacters(path.as_str())
                        && !desired.desired_any_subtree(&path).map_err(Box::new)?;
                    let selector = if is_glob {
                        RemoveSelector::Glob(GatGlobPattern::from_path(&path)?)
                    } else {
                        assert_root_owned(&policy, &path)?;
                        RemoveSelector::Prefix(path)
                    };
                    selectors.push(selector);
                }

                task.handle()
                    .set_activity(ProgressActivity::ScanningDesiredState);
                let mut removed_by_arg = vec![Vec::new(); selectors.len()];
                let mut first_owned_match = vec![None; selectors.len()];
                let scopes = selectors
                    .iter()
                    .map(RemoveSelector::desired_scope)
                    .collect::<Vec<_>>();
                desired
                    .resolve_removals(scopes, |path| {
                        let Some(index) =
                            selectors.iter().position(|selector| selector.matches(path))
                        else {
                            return false;
                        };
                        if first_owned_match[index].is_none()
                            && policy.owner_for_path(path).is_some()
                        {
                            first_owned_match[index] = Some(path.clone());
                            return false;
                        }
                        removed_by_arg[index].push(path.clone());
                        true
                    })
                    .map_err(Box::new)?;
                assert_no_first_owned_match(&policy, first_owned_match)?;
                let removed = removed_by_arg.into_iter().flatten().collect::<Vec<_>>();
                if !request.cached {
                    validate_deletion_paths(repo, &removed)?;
                }
                Ok((selectors, removed))
            },
        )?;

        let (selectors, removed) = plan;
        if removed.is_empty() {
            return Ok(RemoveOutcome { paths: Vec::new() });
        }
        let prefixes = selectors
            .iter()
            .filter_map(|selector| match selector {
                RemoveSelector::Prefix(path) => Some(path.clone()),
                RemoveSelector::Root | RemoveSelector::Glob(_) => None,
            })
            .collect::<Vec<_>>();
        let include_exact = selectors
            .iter()
            .any(|selector| matches!(selector, RemoveSelector::Root | RemoveSelector::Glob(_)));

        with_progress_typed(
            progress,
            ProgressSpec::indeterminate(ProgressOperation::ApplyingChanges),
            |_| -> Result<()> {
                desired
                    .publish_removals(&removed, &prefixes, include_exact)
                    .map_err(Box::new)?;
                if !request.cached {
                    remove_and_prune(repo, &removed)
                        .map_err(|source| RemoveError::Cleanup(Box::new(source)))?;
                }
                desired.forget_materialized(&removed).map_err(|source| {
                    RemoveError::ForgetMaterialized {
                        cached: request.cached,
                        source: Box::new(source),
                    }
                })?;
                desired
                    .sync_excludes()
                    .map_err(|source| RemoveError::SyncExcludes(Box::new(source)))?;
                Ok(())
            },
        )?;
        Ok(RemoveOutcome { paths: removed })
    })
}

fn validate_deletion_paths(repo: &Repository, entries: &[GatPath]) -> Result<()> {
    parallel::map_ordered(entries, |entry| validate_mutation_path(repo, entry))?;
    Ok(())
}

enum RemoveSelector {
    Prefix(GatPath),
    Root,
    Glob(GatGlobPattern),
}

impl RemoveSelector {
    fn matches(&self, path: &GatPath) -> bool {
        match self {
            Self::Root => true,
            Self::Prefix(prefix) => path_matches_scope(path, prefix),
            Self::Glob(pattern) => pattern.matches(path.as_str()),
        }
    }

    fn desired_scope(&self) -> DesiredScope {
        match self {
            Self::Root => DesiredScope::Any,
            Self::Prefix(path) => DesiredScope::Subtree(path.clone()),
            Self::Glob(pattern) => match pattern.bound() {
                GlobBound::Any => DesiredScope::Any,
                GlobBound::Prefix(prefix) => DesiredScope::Prefix(prefix.to_string()),
                GlobBound::Exact(path) => DesiredScope::Exact(
                    GatPath::parse_canonical(path).expect("normalized exact glob bound"),
                ),
            },
        }
    }
}
