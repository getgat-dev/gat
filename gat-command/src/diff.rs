//! `gat diff` orchestration over semantic engine comparisons.

use super::selection::{self, ResolvedSelection};
use gat_core::git::GitRevisionSpec;
use gat_core::progress::{ProgressOperation, ProgressReporter, ProgressSpec, with_progress_typed};
use gat_core::selection::Selection;
use gat_engine::{CompareError, Repository, Unchanged};

#[derive(Clone, Debug)]
pub struct DiffRequest {
    pub from: GitRevisionSpec,
    pub to: DiffTarget,
    /// None uses configured defaults; Some replaces them completely.
    pub selection: Option<Selection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffTarget {
    Revision(GitRevisionSpec),
    WorkingTree,
}

/// A changed path with ownership from the effective configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffRow {
    pub path: gat_core::lexical_path::GatPath,
    pub change: gat_engine::RowChange,
    pub mount: Option<gat_core::name::MountName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffOutcome {
    NoChanges {
        scope: super::SelectionScope,
        from: GitRevisionSpec,
        to: DiffTarget,
    },
    Changes {
        scope: super::SelectionScope,
        from: GitRevisionSpec,
        to: DiffTarget,
        rows: Vec<DiffRow>,
        changes: usize,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    #[error(transparent)]
    Snapshot(#[from] gat_engine::RepoSnapshotError),
    #[error(transparent)]
    Repository(#[from] gat_engine::RepositoryError),
    #[error(transparent)]
    Compare(#[from] CompareError),
}

pub fn diff(
    repo: &Repository,
    request: DiffRequest,
    progress: &dyn ProgressReporter,
) -> Result<DiffOutcome, DiffError> {
    let DiffRequest {
        from,
        to,
        selection,
    } = request;

    let (scope, rows) = match &to {
        DiffTarget::WorkingTree => {
            let current = repo.comparisons().current(progress)?;
            let ResolvedSelection { selection, scope } =
                selection::resolve(selection.as_ref(), current.config())?;
            let rows = current.revision_with_current(&from, &selection, Unchanged::Drop)?;
            (scope, annotate_rows(rows, &current.config().mounts))
        }
        DiffTarget::Revision(revision) => {
            let config = repo.load_config()?;
            let ResolvedSelection { selection, scope } =
                selection::resolve(selection.as_ref(), &config)?;
            let rows = with_progress_typed(
                progress,
                ProgressSpec::indeterminate(ProgressOperation::LoadingState),
                |_| {
                    repo.comparisons()
                        .revisions(&from, revision, &selection, Unchanged::Drop)
                },
            )?;
            (scope, annotate_rows(rows, &config.mounts))
        }
    };

    let changes = rows.len();
    if changes == 0 {
        Ok(DiffOutcome::NoChanges { from, to, scope })
    } else {
        Ok(DiffOutcome::Changes {
            scope,
            from,
            to,
            rows,
            changes,
        })
    }
}

fn annotate_rows(
    rows: Vec<gat_engine::ChangedRow>,
    mounts: &gat_core::config::MountsConfig,
) -> Vec<DiffRow> {
    if rows.is_empty() {
        return Vec::new();
    }
    let ownership = gat_engine::MountOwnership::new(mounts);
    rows.into_iter()
        .map(|row| DiffRow {
            mount: ownership
                .owner_for_path(&row.path)
                .map(|owner| owner.name.clone()),
            path: row.path,
            change: row.change,
        })
        .collect()
}
