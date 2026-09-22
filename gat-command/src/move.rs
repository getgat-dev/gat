use crate::ownership::{OwnershipError, assert_no_owned_entry, assert_root_owned};
use gat_core::lexical_path::GatPath;
use gat_core::progress::{
    NoopProgress, ProgressActivity, ProgressOperation, ProgressReporter, ProgressSpec,
    with_progress_typed,
};
use gat_engine::{
    DestinationKind, MountOwnership, PathPolicyError, Repository, RepositoryMutationError,
    WorktreeMoveError, WorktreePathError, WorktreeRollbackError, inspect_move_destination,
    move_worktree_path, validate_mutation_path,
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
        source: Box<RepositoryMutationError>,
    },
    #[error(
        "tracking-state publication failed for the move from {src} to {dst}, and automatic rollback was incomplete"
    )]
    RollbackFailed {
        src: GatPath,
        dst: GatPath,
        #[source]
        save_source: Box<RepositoryMutationError>,
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
        let policy = MountOwnership::new(&cfg.mounts)?;
        assert_root_owned(&policy, &dst)?;
        let prepared = with_progress_typed(
            progress,
            ProgressSpec::indeterminate(ProgressOperation::ResolvingSelection),
            |task| -> Result<_> {
                task.set_activity(ProgressActivity::MatchingSourcePath);
                let prepared = desired
                    .prepare_move(&src, &dst)
                    .map_err(Box::new)?
                    .ok_or_else(|| MoveError::SourceNotTracked { path: src.clone() })?;
                prepared
                    .source_paths()
                    .try_for_each(|path| assert_root_owned(&policy, path))?;
                validate_mutation_path(repo, &src)?;

                task.set_activity(ProgressActivity::CheckingDestination);
                assert_no_owned_entry(&policy, prepared.collisions())?;
                if !prepared.collisions().is_empty() && !force {
                    return Err(MoveError::DestinationTracked {
                        src: src.clone(),
                        dst: dst.clone(),
                    });
                }
                preflight_destination(repo, &src, &dst, force)?;
                Ok(prepared)
            },
        )?;

        let applying = progress.begin(ProgressSpec::indeterminate(
            ProgressOperation::ApplyingChanges,
        ));
        let pending = move_on_disk(repo, &src, &dst)?;
        if let Err(error) = prepared.publish() {
            return Err(rollback_or_report(error, pending, &src, &dst));
        }
        pending.commit();
        desired
            .sync_excludes()
            .map_err(|source| MoveError::Published {
                src: src.clone(),
                dst: dst.clone(),
                source: Box::new(source),
            })?;
        applying.finish();
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

fn move_on_disk(
    repo: &Repository,
    src: &GatPath,
    dst: &GatPath,
) -> Result<gat_engine::PendingMove> {
    move_worktree_path(repo, src, dst).map_err(|error| match error {
        WorktreeMoveError::Path(source) => MoveError::Path(source),
        error => MoveError::Worktree(Box::new(error)),
    })
}

fn rollback_or_report(
    save_error: RepositoryMutationError,
    pending: gat_engine::PendingMove,
    src: &GatPath,
    dst: &GatPath,
) -> MoveError {
    match pending.rollback() {
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
