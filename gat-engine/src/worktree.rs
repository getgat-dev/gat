//! Repository-bound worktree operations. The I/O layer owns physical access;
//! its semantic results and opaque move receipt need no second representation.

use crate::Repository;
use gat_core::lexical_path::GatPath;

pub use gat_io::{
    MovePathError as MoveError, PendingMove, RestoreMoveDestinationError,
    RollbackMoveError as RollbackError, WorktreeDestinationKind as DestinationKind,
    WorktreeEntryKind as EntryKind, WorktreePathError,
};

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
    repo.worktree_client().inspect(path)
}

pub fn validate_mutation_path(repo: &Repository, path: &GatPath) -> Result<(), WorktreePathError> {
    repo.worktree_client().validate_mutation(path)
}

pub fn validate_mutation_paths(
    repo: &Repository,
    paths: &[GatPath],
) -> Result<(), WorktreePathError> {
    repo.worktree_client().validate_mutations(paths)
}

pub fn reject_infrastructure_path(path: &GatPath) -> Result<(), WorktreePathError> {
    gat_io::WorktreeClient::reject_infrastructure(path)
}

pub fn inspect_move_destination(
    repo: &Repository,
    path: &GatPath,
) -> Result<DestinationKind, WorktreePathError> {
    repo.worktree_client().inspect_destination(path)
}

pub fn move_path(
    repo: &Repository,
    src: &GatPath,
    dst: &GatPath,
) -> Result<PendingMove, MoveError> {
    repo.worktree_client().move_path(src, dst)
}

pub fn remove_and_prune(repo: &Repository, paths: &[GatPath]) -> Result<(), RemoveError> {
    repo.worktree_client()
        .remove_and_prune(paths)
        .map_err(|error| match error {
            gat_io::RemovePathError::Path(source) => RemoveError::Path(source),
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();

        move_path(&repo, &gp("a.bin"), &gp("nested/b.bin"))
            .unwrap()
            .commit();
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
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        let pending = move_path(&repo, &gp("a.bin"), &gp("moved.bin")).unwrap();
        std::fs::remove_file(tmp.path().join("moved.bin")).unwrap();
        let error = pending.rollback().unwrap_err();

        assert!(matches!(
            error,
            RollbackError::Rename { source, .. }
                if source.kind() == std::io::ErrorKind::NotFound
        ));
    }
}
