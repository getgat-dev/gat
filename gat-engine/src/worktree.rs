//! Semantic repository worktree access for command orchestration.
//!
//! Physical path resolution and filesystem operations remain in `gat-io`;
//! callers see only repository-relative paths, semantic classifications, and
//! engine-owned errors.

use crate::Repository;
use gat_core::lexical_path::GatPath;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Missing,
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationKind {
    Missing,
    Directory,
    Other,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreePathError {
    #[error("path `{path}` must be relative")]
    NotRelative { path: String },
    #[error("path `{path}` escapes the worktree (contains `..`)")]
    ParentTraversal { path: String },
    #[error("path `{path}` cannot be materialized on this host")]
    NotMaterializable { path: String },
    #[error("path `{path}` contains non-UTF-8 characters")]
    NonUtf8Component { path: String },
    #[error("path `{path}` escapes the worktree after joining with the repository root")]
    EscapesWorktree { path: String },
    #[error(
        "path `{path}` traverses symlinked ancestor `{ancestor}`; refusing to {verb} outside the repository"
    )]
    SymlinkAncestor {
        path: String,
        ancestor: String,
        verb: &'static str,
    },
    #[error("`{path}` is gat/git infrastructure and can never be added")]
    ForbiddenInfrastructurePath { path: String },
    #[error("`{path}` is a symlink; gat does not track symlinks")]
    UnsupportedLeafSymlink { path: String },
    #[error("{operation} `{}` failed", path.display())]
    Io {
        operation: &'static str,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("internal error: {detail}")]
    Internal { detail: String },
}

impl From<gat_io::WorktreePathError> for WorktreePathError {
    fn from(error: gat_io::WorktreePathError) -> Self {
        use gat_io::WorktreePathError as IoError;

        match error {
            IoError::NotRelative { path } => Self::NotRelative { path },
            IoError::ParentTraversal { path } => Self::ParentTraversal { path },
            IoError::NotMaterializable { path } => Self::NotMaterializable { path },
            IoError::NonUtf8Component { path } => Self::NonUtf8Component { path },
            IoError::EscapesWorktree { path } => Self::EscapesWorktree { path },
            IoError::SymlinkAncestor {
                path,
                ancestor,
                verb,
            } => Self::SymlinkAncestor {
                path,
                ancestor,
                verb,
            },
            IoError::ForbiddenInfrastructurePath { path } => {
                Self::ForbiddenInfrastructurePath { path }
            }
            IoError::UnsupportedLeafSymlink { path } => Self::UnsupportedLeafSymlink { path },
            IoError::Io {
                operation,
                path,
                source,
            } => Self::Io {
                operation,
                path,
                source,
            },
            IoError::Internal { detail } => Self::Internal { detail },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error("creating parent directory for {path}")]
    CreateParent {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("moving {src} to {dst}")]
    Rename {
        src: String,
        dst: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error("moving {dst} back to {src}")]
    Rename {
        src: String,
        dst: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error("could not remove `{path}` from the working tree")]
    Delete {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not remove directory `{path}`")]
    Prune {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub fn inspect_read_path(
    repo: &Repository,
    path: &GatPath,
) -> Result<EntryKind, WorktreePathError> {
    let kind = repo
        .worktree_client()
        .inspect(path)
        .map_err(WorktreePathError::from)?;
    Ok(match kind {
        gat_io::WorktreeEntryKind::Missing => EntryKind::Missing,
        gat_io::WorktreeEntryKind::File => EntryKind::File,
        gat_io::WorktreeEntryKind::Directory => EntryKind::Directory,
        gat_io::WorktreeEntryKind::Symlink => EntryKind::Symlink,
        gat_io::WorktreeEntryKind::Other => EntryKind::Other,
    })
}

pub fn validate_mutation_path(repo: &Repository, path: &GatPath) -> Result<(), WorktreePathError> {
    repo.worktree_client()
        .validate_mutation(path)
        .map_err(WorktreePathError::from)
}

pub fn reject_infrastructure_path(path: &GatPath) -> Result<(), WorktreePathError> {
    gat_io::WorktreeClient::reject_infrastructure(path).map_err(WorktreePathError::from)
}

pub fn inspect_move_destination(
    repo: &Repository,
    path: &GatPath,
) -> Result<DestinationKind, WorktreePathError> {
    repo.worktree_client()
        .inspect_destination(path)
        .map(|kind| match kind {
            gat_io::WorktreeDestinationKind::Missing => DestinationKind::Missing,
            gat_io::WorktreeDestinationKind::Directory => DestinationKind::Directory,
            gat_io::WorktreeDestinationKind::Other => DestinationKind::Other,
        })
        .map_err(WorktreePathError::from)
}

pub fn move_path(repo: &Repository, src: &GatPath, dst: &GatPath) -> Result<(), MoveError> {
    repo.worktree_client()
        .move_path(src, dst)
        .map_err(|error| match error {
            gat_io::MovePathError::Path(source) => MoveError::Path(WorktreePathError::from(source)),
            gat_io::MovePathError::CreateParent { path, source } => {
                MoveError::CreateParent { path, source }
            }
            gat_io::MovePathError::Rename { src, dst, source } => {
                MoveError::Rename { src, dst, source }
            }
        })
}

pub fn rollback_move(repo: &Repository, src: &GatPath, dst: &GatPath) -> Result<(), RollbackError> {
    repo.worktree_client()
        .rollback_move(src, dst)
        .map_err(|error| match error {
            gat_io::RollbackMoveError::Path(source) => {
                RollbackError::Path(WorktreePathError::from(source))
            }
            gat_io::RollbackMoveError::Rename { src, dst, source } => {
                RollbackError::Rename { src, dst, source }
            }
        })
}

pub fn remove_and_prune(repo: &Repository, paths: &[GatPath]) -> Result<(), RemoveError> {
    repo.worktree_client()
        .remove_and_prune(paths)
        .map_err(|error| match error {
            gat_io::RemovePathError::Path(source) => {
                RemoveError::Path(WorktreePathError::from(source))
            }
            gat_io::RemovePathError::Delete { path, source } => {
                RemoveError::Delete { path, source }
            }
            gat_io::RemovePathError::Prune(source) => RemoveError::Prune {
                path: source.path.display().to_string(),
                source: source.source,
            },
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_harness::test_repo;

    fn gp(path: &str) -> GatPath {
        GatPath::parse_canonical(path).unwrap()
    }

    #[test]
    fn inspection_returns_semantic_kind_without_exposing_a_physical_path() {
        let tmp = test_repo();
        let repo = Repository::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("file.bin"), b"payload").unwrap();

        assert_eq!(
            inspect_read_path(&repo, &gp("file.bin")).unwrap(),
            EntryKind::File
        );
        assert_eq!(
            inspect_read_path(&repo, &gp("missing.bin")).unwrap(),
            EntryKind::Missing
        );
    }

    #[test]
    fn move_and_remove_delegate_through_repository_relative_paths() {
        let tmp = test_repo();
        let repo = Repository::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();

        move_path(&repo, &gp("a.bin"), &gp("nested/b.bin")).unwrap();
        assert!(!tmp.path().join("a.bin").exists());
        assert_eq!(
            std::fs::read(tmp.path().join("nested/b.bin")).unwrap(),
            b"payload"
        );

        remove_and_prune(&repo, &[gp("nested/b.bin")]).unwrap();
        assert!(!tmp.path().join("nested").exists());
    }

    #[test]
    fn rollback_failure_preserves_engine_error_classification() {
        let tmp = test_repo();
        let repo = Repository::at(tmp.path().to_path_buf());

        let error = rollback_move(&repo, &gp("a.bin"), &gp("missing.bin")).unwrap_err();

        assert!(matches!(
            error,
            RollbackError::Rename { source, .. }
                if source.kind() == std::io::ErrorKind::NotFound
        ));
    }
}
