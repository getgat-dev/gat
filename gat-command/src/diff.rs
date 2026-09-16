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

/// A diff contains only paths whose content identity changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffChange {
    Added { oid: gat_core::oid::Oid },
    Removed,
    Modified { oid: gat_core::oid::Oid },
}

/// A changed path with ownership from the effective configuration.
///
/// Unchanged comparison rows cannot be passed to the diff renderer.
///
/// ```compile_fail
/// use gat_command::DiffRow;
/// use gat_core::{lexical_path::GatPath, oid::Oid};
/// let row = DiffRow {
///     path: GatPath::parse_canonical("file.bin").unwrap(),
///     change: gat_engine::RowChange::Unchanged { oid: Oid::from_bytes([0; 32]) },
///     mount: None,
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffRow {
    pub path: gat_core::lexical_path::GatPath,
    pub change: DiffChange,
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
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    #[error(transparent)]
    Policy(#[from] gat_engine::PathPolicyError),
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
            let rows = with_progress_typed(
                progress,
                ProgressSpec::indeterminate(ProgressOperation::ComparingState),
                |_| -> Result<_, DiffError> {
                    let rows = current.revision_with_current(&from, &selection, Unchanged::Drop)?;
                    annotate_rows(rows, &current.config().mounts)
                },
            )?;
            (scope, rows)
        }
        DiffTarget::Revision(revision) => {
            let config = repo.load_config()?;
            let ResolvedSelection { selection, scope } =
                selection::resolve(selection.as_ref(), &config)?;
            let rows = with_progress_typed(
                progress,
                ProgressSpec::indeterminate(ProgressOperation::ComparingState),
                |_| -> Result<_, DiffError> {
                    let rows = repo.comparisons().revisions(
                        &from,
                        revision,
                        &selection,
                        Unchanged::Drop,
                    )?;
                    annotate_rows(rows, &config.mounts)
                },
            )?;
            (scope, rows)
        }
    };

    if rows.is_empty() {
        Ok(DiffOutcome::NoChanges { from, to, scope })
    } else {
        Ok(DiffOutcome::Changes {
            scope,
            from,
            to,
            rows,
        })
    }
}

fn annotate_rows(
    rows: Vec<gat_engine::ChangedRow>,
    mounts: &gat_core::config::MountsConfig,
) -> Result<Vec<DiffRow>, DiffError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ownership = gat_engine::MountOwnership::new(mounts)?;
    let mut annotated = Vec::with_capacity(rows.len());
    for row in rows {
        let change = match row.change {
            gat_engine::RowChange::Added { oid } => DiffChange::Added { oid },
            gat_engine::RowChange::Removed => DiffChange::Removed,
            gat_engine::RowChange::Modified { oid } => DiffChange::Modified { oid },
            gat_engine::RowChange::Unchanged { .. } => continue,
        };
        annotated.push(DiffRow {
            mount: ownership
                .owner_for_path(&row.path)
                .map(|owner| owner.name.clone()),
            path: row.path,
            change,
        });
    }
    Ok(annotated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::{lexical_path::GatPath, oid::Oid};
    use gat_engine::{ChangedRow, RowChange};

    #[test]
    fn annotation_keeps_only_changes_and_preserves_their_identity() {
        let oid = Oid::from_bytes([7; 32]);
        let rows = [
            ("added", RowChange::Added { oid }),
            ("removed", RowChange::Removed),
            ("modified", RowChange::Modified { oid }),
            ("unchanged", RowChange::Unchanged { oid }),
        ]
        .into_iter()
        .map(|(path, change)| ChangedRow {
            path: GatPath::parse_canonical(path).unwrap(),
            change,
        })
        .collect();
        let annotated = annotate_rows(rows, &gat_core::config::MountsConfig::default()).unwrap();
        let actual: Vec<_> = annotated
            .iter()
            .map(|row| (row.path.as_str(), row.change))
            .collect();
        assert_eq!(
            actual,
            vec![
                ("added", DiffChange::Added { oid }),
                ("removed", DiffChange::Removed),
                ("modified", DiffChange::Modified { oid }),
            ]
        );
    }
}
