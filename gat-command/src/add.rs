use crate::ownership::{OwnershipError, assert_root_owned};
use gat_core::globs::{GatGlobPattern, GlobBound, GlobError};
use gat_core::lexical_path::{GatPath, LexicalPathError};
use gat_core::lifecycle::Surface;
use gat_core::path_scope::PathScope;
use gat_core::progress::{
    ProgressActivity, ProgressHandle, ProgressOperation, ProgressReporter, ProgressSpec,
    ProgressTask, ProgressUnit,
};
use gat_engine::{
    AddCandidate, AddExclusion, DesiredState, EffectivePathPolicy, EntryKind,
    MaterializationSession, MaterializedEntry, PathPolicyError, RemoteCatalog, RemoteCatalogError,
    Repository, RepositoryError, RepositoryMutationError, RepositoryStateError, WorktreePathError,
    inspect_read_path, reject_infrastructure_path,
};
use std::collections::HashSet;

struct AddLimits {
    candidates: usize,
    #[cfg(any(test, feature = "test-support"))]
    high_water: std::cell::Cell<usize>,
}

impl Default for AddLimits {
    fn default() -> Self {
        Self {
            candidates: 4096,
            #[cfg(any(test, feature = "test-support"))]
            high_water: Default::default(),
        }
    }
}

impl AddLimits {
    fn observe(&self, count: usize) {
        debug_assert!(count <= self.candidates);
        #[cfg(any(test, feature = "test-support"))]
        self.high_water.set(self.high_water.get().max(count));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddRequest {
    pub paths: Vec<PathScope>,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddOutcome {
    pub rows: Vec<AddedRow>,
    pub added_count: usize,
    pub exclusions: Vec<AddExclusion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddedRow {
    pub path: PathScope,
    pub file_count: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error(transparent)]
    Snapshot(Box<gat_engine::RepoSnapshotError>),
    #[error("`{path}` is already tracked by git")]
    AlreadyGitTracked { path: GatPath },
    #[error("`{path}` is ignored by git")]
    IgnoredByGit { path: GatPath },
    #[error("`{path}` is ignored by .gatignore")]
    IgnoredByGatignore { path: GatPath },
    #[error(transparent)]
    MountOwned(#[from] OwnershipError),
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error(transparent)]
    Lexical(#[from] LexicalPathError),
    #[error("`{path}` is not a regular file or directory")]
    UnsupportedFileType { path: GatPath },
    #[error("`{pattern}` does not exist and matches no files")]
    NoMatch { pattern: GatPath },
    #[error(transparent)]
    Glob(#[from] GlobError),
    #[error(transparent)]
    Config(#[from] Box<RepositoryError>),
    #[error(transparent)]
    PathPolicy(#[from] PathPolicyError),
    #[error(transparent)]
    RemoteCatalog(#[from] RemoteCatalogError),
    #[error(transparent)]
    RepositoryState(#[from] Box<RepositoryStateError>),
    #[error(transparent)]
    RepositoryMutation(#[from] Box<RepositoryMutationError>),
}

impl From<gat_engine::RepoSnapshotError> for AddError {
    fn from(error: gat_engine::RepoSnapshotError) -> Self {
        Self::Snapshot(Box::new(error))
    }
}

impl From<RepositoryError> for AddError {
    fn from(error: RepositoryError) -> Self {
        Self::Config(Box::new(error))
    }
}

impl From<RepositoryStateError> for AddError {
    fn from(error: RepositoryStateError) -> Self {
        Self::RepositoryState(Box::new(error))
    }
}

impl From<RepositoryMutationError> for AddError {
    fn from(error: RepositoryMutationError) -> Self {
        Self::RepositoryMutation(Box::new(error))
    }
}

type Result<T> = std::result::Result<T, AddError>;
type AddedEntries = Vec<MaterializedEntry>;

pub fn add(
    repo: &Repository,
    request: AddRequest,
    progress: &dyn ProgressReporter,
) -> Result<AddOutcome> {
    add_with_lifecycle_observer(repo, request, progress, &|_| {})
}

pub fn add_with_lifecycle_observer(
    repo: &Repository,
    request: AddRequest,
    progress: &dyn ProgressReporter,
    observe: &dyn Fn(Surface<'_>),
) -> Result<AddOutcome> {
    add_with_limits(repo, request, progress, observe, &AddLimits::default())
}

fn add_with_limits(
    repo: &Repository,
    request: AddRequest,
    progress: &dyn ProgressReporter,
    observe: &dyn Fn(Surface<'_>),
    limits: &AddLimits,
) -> Result<AddOutcome> {
    repo.with_add_state(progress, |cfg, desired| {
        let catalog = RemoteCatalog::from_config(&cfg.remotes)?;
        let policy = EffectivePathPolicy::from_config(cfg, &catalog)?;

        let mut exclusions = Vec::new();
        let (added, rows, added_count) = {
            let mut context = AddContext {
                progress,
                observe,
                limits,
                desired: &desired,
                materialization: desired.materialization_session(),
                seen: HashSet::new(),
                directories: Vec::new(),
                exclusions: &mut exclusions,
                discovering: None,
                hashing: None,
            };
            let mut added = Vec::new();
            let mut rows = Vec::new();
            let mut added_count = 0;
            let mut pending_unique_files = Vec::new();
            let flush = |context: &mut AddContext<'_, '_, '_, '_>,
                         pending: &mut Vec<GatPath>,
                         added: &mut AddedEntries|
             -> Result<()> { flush_pending_files(context, pending, added) };

            desired.with_git_path_status_lookup(|lookup| -> Result<()> {
                let mut selectors = HashSet::new();
                for scope in &request.paths {
                    if !selectors.insert(match scope {
                        PathScope::Root => None,
                        PathScope::Path(path) => Some(path),
                    }) {
                        continue;
                    }
                    if matches!(scope, PathScope::Root) {
                        flush(&mut context, &mut pending_unique_files, &mut added)?;
                        let count =
                            add_dir(&mut context, &policy, None, &mut added, request.force)?;
                        added_count += count;
                        rows.push(AddedRow {
                            path: PathScope::Root,
                            file_count: Some(count),
                        });
                        continue;
                    }
                    let PathScope::Path(path) = scope else {
                        unreachable!()
                    };
                    flush_before_error(
                        reject_infrastructure_path(path).map_err(AddError::from),
                        || flush(&mut context, &mut pending_unique_files, &mut added),
                    )?;
                    let kind = flush_before_error(
                        inspect_read_path(repo, path).map_err(AddError::from),
                        || flush(&mut context, &mut pending_unique_files, &mut added),
                    )?;
                    match kind {
                        EntryKind::Symlink => {
                            flush(&mut context, &mut pending_unique_files, &mut added)?;
                            return Err(AddError::Path(
                                WorktreePathError::UnsupportedLeafSymlink {
                                    path: path.to_string(),
                                },
                            ));
                        }
                        EntryKind::Directory => {
                            flush(&mut context, &mut pending_unique_files, &mut added)?;
                            let count = add_dir(
                                &mut context,
                                &policy,
                                Some(path),
                                &mut added,
                                request.force,
                            )?;
                            added_count += count;
                            rows.push(AddedRow {
                                path: PathScope::Path(path.clone()),
                                file_count: Some(count),
                            });
                        }
                        EntryKind::File => {
                            flush_before_error(
                                assert_addable(&context, &policy, path, request.force),
                                || flush(&mut context, &mut pending_unique_files, &mut added),
                            )?;
                            let status = flush_before_error(lookup(path.as_str()), || {
                                flush(&mut context, &mut pending_unique_files, &mut added)
                            })?;
                            if status.tracked {
                                flush(&mut context, &mut pending_unique_files, &mut added)?;
                                return Err(AddError::AlreadyGitTracked { path: path.clone() });
                            }
                            if !request.force
                                && status.gitignored
                                && !desired.desired_any_exact(path).map_err(Box::new)?
                            {
                                flush(&mut context, &mut pending_unique_files, &mut added)?;
                                return Err(AddError::IgnoredByGit { path: path.clone() });
                            }
                            if context.seen.insert(path.clone()) {
                                pending_unique_files.push(path.clone());
                                rows.push(AddedRow {
                                    path: PathScope::Path(path.clone()),
                                    file_count: None,
                                });
                                added_count += 1;
                                if pending_unique_files.len() == context.limits.candidates {
                                    flush(&mut context, &mut pending_unique_files, &mut added)?;
                                }
                            }
                        }
                        EntryKind::Other => {
                            flush(&mut context, &mut pending_unique_files, &mut added)?;
                            return Err(AddError::UnsupportedFileType { path: path.clone() });
                        }
                        EntryKind::Missing => {
                            flush(&mut context, &mut pending_unique_files, &mut added)?;
                            let matched = flush_before_error(
                                add_glob(&mut context, &policy, path, &mut added, request.force),
                                || flush(&mut context, &mut pending_unique_files, &mut added),
                            )?;
                            added_count += matched.len();
                            rows.extend(matched.into_iter().map(|path| AddedRow {
                                path: PathScope::Path(path),
                                file_count: None,
                            }));
                        }
                    }
                }
                flush(&mut context, &mut pending_unique_files, &mut added)
            })?;
            (added, rows, added_count)
        };

        exclusions.sort_unstable_by_key(|item| item.reason);
        if added.is_empty() {
            return Ok(AddOutcome {
                rows,
                added_count,
                exclusions,
            });
        }

        let mut desired = desired.into_mutation().map_err(Box::new)?;
        let applying = progress.begin(ProgressSpec::indeterminate(
            ProgressOperation::ApplyingChanges,
        ));
        applying
            .handle()
            .set_activity(ProgressActivity::PublishingDesiredState);
        desired.publish_upserts(added).map_err(Box::new)?;
        applying
            .handle()
            .set_activity(ProgressActivity::RecordingMaterializedState);
        desired.record_published_materialized().map_err(Box::new)?;
        applying
            .handle()
            .set_activity(ProgressActivity::RegeneratingExcludes);
        desired.sync_excludes().map_err(Box::new)?;
        applying.finish();

        Ok(AddOutcome {
            rows,
            added_count,
            exclusions,
        })
    })
}

struct AddContext<'a, 'repo, 'config, 'materialization> {
    exclusions: &'a mut Vec<AddExclusion>,
    limits: &'a AddLimits,
    seen: HashSet<GatPath>,
    directories: Vec<Option<GatPath>>,
    progress: &'a dyn ProgressReporter,
    discovering: Option<ProgressTask>,
    hashing: Option<ProgressTask>,
    observe: &'a dyn Fn(Surface<'_>),
    desired: &'materialization DesiredState<'repo, 'config>,
    materialization: MaterializationSession<'materialization, 'repo, 'config>,
}

impl AddContext<'_, '_, '_, '_> {
    fn discovery_activity(&mut self, activity: ProgressActivity) {
        let task = if let Some(hashing) = &self.hashing {
            hashing
        } else {
            self.discovering.get_or_insert_with(|| {
                self.progress.begin(ProgressSpec::indeterminate(
                    ProgressOperation::DiscoveringFiles,
                ))
            })
        };
        task.set_activity(activity);
    }

    fn hashing_handle(&mut self) -> ProgressHandle {
        // Selection and reuse classification continue between hashing windows,
        // so the whole-operation hash count has no known total up front.
        self.discovering.take();
        self.hashing
            .get_or_insert_with(|| {
                self.progress.begin(ProgressSpec::items(
                    ProgressOperation::Hashing,
                    ProgressUnit::Files,
                    None,
                ))
            })
            .handle()
    }
}

fn flush_before_error<T>(result: Result<T>, flush: impl FnOnce() -> Result<()>) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            flush()?;
            Err(error)
        }
    }
}

fn flush_pending_files(
    context: &mut AddContext<'_, '_, '_, '_>,
    pending: &mut Vec<GatPath>,
    added: &mut AddedEntries,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    context.limits.observe(pending.len());
    context.discovery_activity(ProgressActivity::CheckingReuseStatus);
    let candidates = pending
        .drain(..)
        .map(|path| AddCandidate {
            path,
            desired_oid: None,
        })
        .collect();
    let (reused, to_hash) = context.materialization.partition_reusable(candidates)?;
    added.extend(reused);
    added.extend(ingest_files(context, &to_hash)?);
    pending.clear();
    Ok(())
}

fn assert_addable(
    context: &AddContext<'_, '_, '_, '_>,
    policy: &EffectivePathPolicy,
    path: &GatPath,
    force: bool,
) -> Result<()> {
    reject_infrastructure_path(path)?;
    assert_root_owned(policy, path)?;
    if !force && context.desired.is_gatignored(path) {
        return Err(AddError::IgnoredByGatignore { path: path.clone() });
    }
    Ok(())
}

fn ingest_files(
    context: &mut AddContext<'_, '_, '_, '_>,
    files: &[GatPath],
) -> Result<AddedEntries> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let strategy = context.desired.ingest_strategy();
    (context.observe)(Surface::ConfigValue {
        key: "cache.ingest_strategy",
        value: strategy.as_str(),
    });
    let handle = context.hashing_handle();
    Ok(context.materialization.ingest_materialized_entries(
        files,
        strategy,
        |path, percent| {
            handle.set_activity(ProgressActivity::HashingFile {
                path: path.clone(),
                percent,
            });
        },
        || handle.inc(1),
    )?)
}

fn add_dir(
    context: &mut AddContext<'_, '_, '_, '_>,
    policy: &EffectivePathPolicy,
    path: Option<&GatPath>,
    added: &mut AddedEntries,
    force: bool,
) -> Result<usize> {
    if let Some(path) = path {
        reject_infrastructure_path(path)?;
    }
    if context.directories.iter().any(|prior| match (prior, path) {
        (None, _) => true,
        (Some(prior), Some(path)) => {
            path == prior
                || path
                    .as_str()
                    .strip_prefix(prior.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }
        _ => false,
    }) {
        return Ok(0);
    }
    context.discovery_activity(ProgressActivity::WalkingDirectory);
    let candidates = context
        .desired
        .discover_add_candidates(path, None, force, context.exclusions)
        .map_err(Box::new)?;
    let count = process_candidates(context, policy, candidates, added, false)?.0;
    context.directories.push(path.cloned());
    Ok(count)
}

fn add_glob(
    context: &mut AddContext<'_, '_, '_, '_>,
    policy: &EffectivePathPolicy,
    pattern: &GatPath,
    added: &mut AddedEntries,
    force: bool,
) -> Result<Vec<GatPath>> {
    let glob = GatGlobPattern::parse(pattern.as_str())?;
    let directory = match glob_traversal_dir(&glob) {
        "" => None,
        directory => Some(GatPath::parse_canonical(directory)?),
    };
    context.discovery_activity(ProgressActivity::ExpandingPattern {
        pattern: pattern.to_string(),
    });
    let excluded_before: usize = context
        .exclusions
        .iter()
        .map(|item| item.files + item.directories)
        .sum();
    let candidates = context
        .desired
        .discover_add_candidates(directory.as_ref(), Some(&glob), force, context.exclusions)
        .map_err(Box::new)?;
    if candidates.is_empty()
        && context
            .exclusions
            .iter()
            .map(|item| item.files + item.directories)
            .sum::<usize>()
            == excluded_before
    {
        return Err(AddError::NoMatch {
            pattern: pattern.clone(),
        });
    }
    Ok(process_candidates(context, policy, candidates, added, true)?.1)
}

fn process_candidates(
    context: &mut AddContext<'_, '_, '_, '_>,
    policy: &EffectivePathPolicy,
    candidates: Vec<AddCandidate>,
    added: &mut AddedEntries,
    retain_paths: bool,
) -> Result<(usize, Vec<GatPath>)> {
    for candidate in &candidates {
        assert_root_owned(policy, &candidate.path)?;
    }
    let mut paths = Vec::new();
    let mut count = 0;
    let mut candidates = candidates.into_iter();
    loop {
        let window: Vec<_> = candidates
            .by_ref()
            .filter(|candidate| context.seen.insert(candidate.path.clone()))
            .take(context.limits.candidates)
            .collect();
        if window.is_empty() {
            break;
        }
        context.limits.observe(window.len());
        count += window.len();
        if retain_paths {
            paths.extend(window.iter().map(|candidate| candidate.path.clone()));
        }
        context.discovery_activity(ProgressActivity::CheckingReuseStatus);
        let (reused, to_hash) = context.materialization.partition_reusable(window)?;
        added.extend(reused);
        added.extend(ingest_files(context, &to_hash)?);
    }
    Ok((count, paths))
}

fn glob_traversal_dir(glob: &GatGlobPattern) -> &str {
    match glob.bound() {
        GlobBound::Any => "",
        GlobBound::Prefix(prefix) | GlobBound::Exact(prefix) => match prefix.rfind('/') {
            Some(index) => &prefix[..index],
            None => "",
        },
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn add_with_window_for_test(
    repo: &Repository,
    request: AddRequest,
    window: std::num::NonZeroUsize,
    progress: &dyn ProgressReporter,
) -> Result<(AddOutcome, usize)> {
    let limits = AddLimits {
        candidates: window.get(),
        high_water: Default::default(),
    };
    let outcome = add_with_limits(repo, request, progress, &|_| {}, &limits)?;
    Ok((outcome, limits.high_water.get()))
}
