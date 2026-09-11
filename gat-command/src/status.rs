//! Local status and tracked-file listing orchestration.

use super::selection::{self, ResolvedSelection};
use gat_core::lexical_path::GatPath;
use gat_core::progress::{
    ProgressOperation, ProgressReporter, ProgressSpec, ProgressUnit, with_progress_typed,
};
use gat_core::selection::Selection;
use gat_engine::{ChangedRow, CompareError, Repository, RowChange, Unchanged};
use rayon::prelude::*;

fn probe_cache(
    rows_by_path: &[ChangedRow],
    contains: impl Fn(&gat_core::oid::Oid) -> bool + Sync,
) -> std::collections::HashMap<gat_core::oid::Oid, bool> {
    let mut presence: std::collections::HashMap<_, _> = rows_by_path
        .iter()
        .filter_map(|row| match row.change {
            RowChange::Added { oid }
            | RowChange::Modified { oid }
            | RowChange::Unchanged { oid } => Some((oid, false)),
            RowChange::Removed => None,
        })
        .collect();
    presence
        .par_iter_mut()
        .for_each(|(oid, present)| *present = contains(oid));
    presence
}

#[derive(Clone, Debug)]
pub struct StatusRequest {
    /// None uses configured defaults; Some replaces them completely.
    pub selection: Option<Selection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePresence {
    Present,
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusRow {
    pub path: GatPath,
    pub change: RowChange,
    pub cache_presence: Option<CachePresence>,
    pub mount: Option<gat_core::name::MountName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusOutcome {
    NoTrackedFiles,
    NoMatchingFiles {
        scope: super::SelectionScope,
    },
    WorkingTree {
        scope: super::SelectionScope,
        rows: Vec<StatusRow>,
        changes: usize,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error(transparent)]
    Snapshot(#[from] gat_engine::RepoSnapshotError),
    #[error(transparent)]
    Repository(#[from] gat_engine::RepositoryError),
    #[error(transparent)]
    Compare(#[from] CompareError),
}

pub fn status(
    repo: &Repository,
    request: StatusRequest,
    progress: &dyn ProgressReporter,
) -> Result<StatusOutcome, StatusError> {
    let current = repo.comparisons().current(progress)?;
    let config = current.config();
    let ResolvedSelection { selection, scope } =
        selection::resolve(request.selection.as_ref(), config)?;
    let rows_by_path = current.staged_with_current(&selection, Unchanged::Keep)?;

    if rows_by_path.is_empty() {
        return Ok(if scope == super::SelectionScope::Unrestricted {
            StatusOutcome::NoTrackedFiles
        } else {
            StatusOutcome::NoMatchingFiles { scope }
        });
    }

    let ownership = gat_engine::MountOwnership::new(&config.mounts);
    let cache = current.cache_presence();
    // Counting is pure and independent of parallel cache/ownership annotation.
    // Avoid making every changed row contend on the same atomic counter.
    let changes = rows_by_path
        .iter()
        .filter(|row| !matches!(row.change, RowChange::Unchanged { .. }))
        .count();
    let rows = with_progress_typed(
        progress,
        ProgressSpec::items(
            ProgressOperation::InspectingCache,
            ProgressUnit::Entries,
            Some(rows_by_path.len() as u64),
        ),
        |inspect| -> Result<Vec<StatusRow>, StatusError> {
            let presence = probe_cache(&rows_by_path, |oid| cache.contains(oid));
            let inspect = inspect.handle();
            Ok(rows_by_path
                .into_par_iter()
                .map(|ChangedRow { path, change }| {
                    let cache_presence = match &change {
                        RowChange::Added { oid }
                        | RowChange::Modified { oid }
                        | RowChange::Unchanged { oid } => Some(if presence[oid] {
                            CachePresence::Present
                        } else {
                            CachePresence::Missing
                        }),
                        RowChange::Removed => None,
                    };
                    inspect.inc(1);
                    StatusRow {
                        mount: ownership
                            .owner_for_path(&path)
                            .map(|owner| owner.name.clone()),
                        path,
                        change,
                        cache_presence,
                    }
                })
                .collect())
        },
    )?;

    Ok(StatusOutcome::WorkingTree {
        scope,
        rows,
        changes,
    })
}

#[derive(Clone, Debug)]
pub struct LsFilesRequest {
    /// None uses configured defaults; Some replaces them completely.
    pub selection: Option<Selection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LsFilesOutcome {
    pub scope: super::SelectionScope,
    pub paths: Vec<GatPath>,
}

#[derive(Debug, thiserror::Error)]
pub enum LsFilesError {
    #[error(transparent)]
    Repository(#[from] gat_engine::RepositoryError),
    #[error(transparent)]
    DesiredState(#[from] gat_engine::RepositoryStateError),
}

pub fn ls_files(
    repo: &Repository,
    request: LsFilesRequest,
    progress: &dyn ProgressReporter,
) -> Result<LsFilesOutcome, LsFilesError> {
    let ResolvedSelection { selection, scope } =
        selection::resolve_for_read(request.selection.as_ref(), repo)?;
    let mut paths = Vec::new();
    with_progress_typed(
        progress,
        ProgressSpec::items(ProgressOperation::LoadingState, ProgressUnit::Entries, None),
        |task| -> Result<(), LsFilesError> {
            let handle = task.handle();
            repo.visit_current_desired_entries(&selection, |entry| {
                paths.push(entry.path);
                handle.inc(1);
            })?;
            Ok(())
        },
    )?;
    Ok(LsFilesOutcome { scope, paths })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_presence_probes_shared_content_once_and_skips_removals() {
        let oid = gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap();
        let rows = vec![
            ChangedRow {
                path: GatPath::parse_canonical("a").unwrap(),
                change: RowChange::Added { oid },
            },
            ChangedRow {
                path: GatPath::parse_canonical("b").unwrap(),
                change: RowChange::Unchanged { oid },
            },
            ChangedRow {
                path: GatPath::parse_canonical("c").unwrap(),
                change: RowChange::Removed,
            },
        ];
        let probes = std::sync::atomic::AtomicUsize::new(0);
        let result = probe_cache(&rows, |_| {
            probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            false
        });
        assert_eq!(probes.into_inner(), 1);
        assert_eq!(result.len(), 1);
        assert!(!result[&oid]);
    }
}
