use crate::ownership::{OwnershipError, assert_no_owned_entry, assert_root_owned};
use gat_core::lexical_path::GatPath;
use gat_core::progress::{
    NoopProgress, ProgressActivity, ProgressOperation, ProgressReporter, ProgressSpec,
    with_progress_typed,
};
use gat_engine::{
    DestinationKind, EffectivePathPolicy, PathPolicyError, RemoteCatalog, RemoteCatalogError,
    Repository, RepositoryMutationError, WorktreeMoveError, WorktreePathError,
    WorktreeRollbackError, inspect_move_destination, move_worktree_path, rollback_worktree_move,
    validate_mutation_path,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRequest {
    pub src: GatPath,
    pub dst: GatPath,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveOutcome {
    pub src: GatPath,
    pub dst: GatPath,
}

#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    #[error(transparent)]
    MountOwned(#[from] OwnershipError),
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error(transparent)]
    Acquisition(Box<gat_engine::RepoSnapshotError>),
    #[error(transparent)]
    PathPolicy(#[from] PathPolicyError),
    #[error(transparent)]
    RemoteCatalog(#[from] RemoteCatalogError),
    #[error(transparent)]
    RepositoryMutation(#[from] Box<RepositoryMutationError>),
    #[error("`{path}` is not tracked by gat")]
    SourceNotTracked { path: GatPath },
    #[error("`{dst}` is already tracked by gat; use `gat mv --force {src} {dst}` to replace it")]
    DestinationTracked { src: GatPath, dst: GatPath },
    #[error(
        "`{path}` is an existing directory; `gat mv` does not support moving into a directory, name the destination explicitly"
    )]
    DestinationIsDirectory { path: GatPath },
    #[error("`{dst}` already exists; use `gat mv --force {src} {dst}` to replace it")]
    DestinationExists { src: GatPath, dst: GatPath },
    #[error("metadata update failed after publishing the move")]
    Published {
        src: GatPath,
        dst: GatPath,
        #[source]
        source: Box<RepositoryMutationError>,
    },
    #[error("checking `{path}`")]
    CheckDestination {
        path: GatPath,
        #[source]
        source: WorktreePathError,
    },
    #[error(transparent)]
    Worktree(#[from] Box<WorktreeMoveError>),
    #[error("rolled back the rename of {src} to {dst}")]
    RolledBack {
        src: GatPath,
        dst: GatPath,
        #[source]
        source: Box<Self>,
    },
    #[error(
        "moved {src} to {dst} on disk but failed to update gat.lock, and the automatic rollback failed; the file is currently at {dst}"
    )]
    RollbackFailed {
        src: GatPath,
        dst: GatPath,
        #[source]
        save_source: Box<Self>,
        rollback_source: Box<WorktreeRollbackError>,
    },
}

impl From<gat_engine::RepoSnapshotError> for MoveError {
    fn from(error: gat_engine::RepoSnapshotError) -> Self {
        Self::Acquisition(Box::new(error))
    }
}

impl From<RepositoryMutationError> for MoveError {
    fn from(error: RepositoryMutationError) -> Self {
        Self::RepositoryMutation(Box::new(error))
    }
}

type Result<T> = std::result::Result<T, MoveError>;

pub fn move_path(repo: &Repository, request: MoveRequest) -> Result<MoveOutcome> {
    move_with_progress(repo, request, &NoopProgress)
}

pub fn move_with_progress(
    repo: &Repository,
    request: MoveRequest,
    progress: &dyn ProgressReporter,
) -> Result<MoveOutcome> {
    let MoveRequest { src, dst, force } = request;
    repo.with_desired_mutation(progress, |cfg, mut desired| {
        let catalog = RemoteCatalog::from_config(&cfg.remotes)?;
        let policy = EffectivePathPolicy::from_config(cfg, &catalog)?;
        assert_root_owned(&policy, &dst)?;
        let dst_collisions = with_progress_typed(
            progress,
            ProgressSpec::indeterminate(ProgressOperation::ResolvingSelection),
            |task| -> Result<_> {
                task.handle()
                    .set_activity(ProgressActivity::MatchingSourcePath);
                let (matches, dst_collisions) =
                    desired.resolve_move(&src, &dst).map_err(Box::new)?;
                if matches.is_empty() {
                    return Err(MoveError::SourceNotTracked { path: src.clone() });
                }
                assert_no_owned_entry(&policy, &matches)?;
                validate_mutation_path(repo, &src)?;

                task.handle()
                    .set_activity(ProgressActivity::CheckingDestination);
                assert_no_owned_entry(&policy, &dst_collisions)?;
                if !dst_collisions.is_empty() && !force {
                    return Err(MoveError::DestinationTracked {
                        src: src.clone(),
                        dst: dst.clone(),
                    });
                }
                preflight_destination(repo, &src, &dst, force)?;
                Ok(dst_collisions)
            },
        )?;
        let dst_collision_paths = dst_collisions
            .into_iter()
            .map(|entry| entry.path)
            .collect::<Vec<_>>();

        with_progress_typed(
            progress,
            ProgressSpec::indeterminate(ProgressOperation::ApplyingChanges),
            |_| -> Result<()> {
                move_on_disk(repo, &src, &dst)?;
                if let Err(error) = desired.publish_move(&src, &dst, &dst_collision_paths) {
                    return Err(rollback_or_report(
                        MoveError::RepositoryMutation(Box::new(error)),
                        repo,
                        &src,
                        &dst,
                    ));
                }
                let published = |source| MoveError::Published {
                    src: src.clone(),
                    dst: dst.clone(),
                    source: Box::new(source),
                };
                if !dst_collision_paths.is_empty() {
                    desired
                        .forget_materialized(&dst_collision_paths)
                        .map_err(published)?;
                }
                desired.move_materialized(&src, &dst).map_err(published)?;
                desired.sync_excludes().map_err(published)?;
                Ok(())
            },
        )?;
        Ok(MoveOutcome { src, dst })
    })
}

fn preflight_destination(
    repo: &Repository,
    src: &GatPath,
    dst: &GatPath,
    force: bool,
) -> Result<()> {
    let kind = match inspect_move_destination(repo, dst) {
        Ok(kind) => kind,
        Err(source @ WorktreePathError::Io { .. }) => {
            return Err(MoveError::CheckDestination {
                path: dst.clone(),
                source,
            });
        }
        Err(error) => return Err(MoveError::Path(error)),
    };
    match kind {
        DestinationKind::Directory => Err(MoveError::DestinationIsDirectory { path: dst.clone() }),
        DestinationKind::Other if !force => Err(MoveError::DestinationExists {
            src: src.clone(),
            dst: dst.clone(),
        }),
        DestinationKind::Missing | DestinationKind::Other => Ok(()),
    }
}

fn move_on_disk(repo: &Repository, src: &GatPath, dst: &GatPath) -> Result<()> {
    move_worktree_path(repo, src, dst).map_err(|error| match error {
        WorktreeMoveError::Path(source) => MoveError::Path(source),
        error => MoveError::Worktree(Box::new(error)),
    })
}

fn rollback_or_report(
    save_error: MoveError,
    repo: &Repository,
    src: &GatPath,
    dst: &GatPath,
) -> MoveError {
    match rollback_worktree_move(repo, src, dst) {
        Ok(()) => MoveError::RolledBack {
            src: src.clone(),
            dst: dst.clone(),
            source: Box::new(save_error),
        },
        Err(rollback_source) => MoveError::RollbackFailed {
            src: src.clone(),
            dst: dst.clone(),
            save_source: Box::new(save_error),
            rollback_source: Box::new(rollback_source),
        },
    }
}
