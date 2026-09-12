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

/// Cache presence exists exactly for rows that still have tracked content.
///
/// ```compile_fail
/// use gat_command::StatusChange;
/// use gat_core::oid::Oid;
/// let change = StatusChange::Added { oid: Oid::from_bytes([0; 32]) };
/// ```
///
/// ```compile_fail
/// use gat_command::{StatusChange, CachePresence};
/// let change = StatusChange::Removed { cache: CachePresence::Present };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusChange {
    Added {
        oid: gat_core::oid::Oid,
        cache: CachePresence,
    },
    Modified {
        oid: gat_core::oid::Oid,
        cache: CachePresence,
    },
    Unchanged {
        oid: gat_core::oid::Oid,
        cache: CachePresence,
    },
    Removed,
}

impl StatusChange {
    #[must_use]
    pub const fn cache_presence(self) -> Option<CachePresence> {
        match self {
            Self::Added { cache, .. }
            | Self::Modified { cache, .. }
            | Self::Unchanged { cache, .. } => Some(cache),
            Self::Removed => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusRow {
    pub path: GatPath,
    pub change: StatusChange,
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
    // Snapshot validation, comparison, and result preparation all scale with
    // tracked entries even when the desired-state mirror was already current.
    let (rows_by_path, changes) = with_progress_typed(
        progress,
        ProgressSpec::indeterminate(ProgressOperation::ComparingState),
        |_| -> Result<_, StatusError> {
            let rows = current.staged_with_current(&selection, Unchanged::Keep)?;
            let changes = rows
                .iter()
                .filter(|row| !matches!(row.change, RowChange::Unchanged { .. }))
                .count();
            Ok((rows, changes))
        },
    )?;

    if rows_by_path.is_empty() {
        return Ok(if scope == super::SelectionScope::Unrestricted {
            StatusOutcome::NoTrackedFiles
        } else {
            StatusOutcome::NoMatchingFiles { scope }
        });
    }

    let ownership = gat_engine::MountOwnership::new(&config.mounts);
    let cache = current.cache_presence();
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
                    let cache = |oid| {
                        if presence[&oid] {
                            CachePresence::Present
                        } else {
                            CachePresence::Missing
                        }
                    };
                    let change = match change {
                        RowChange::Added { oid } => StatusChange::Added {
                            oid,
                            cache: cache(oid),
                        },
                        RowChange::Modified { oid } => StatusChange::Modified {
                            oid,
                            cache: cache(oid),
                        },
                        RowChange::Unchanged { oid } => StatusChange::Unchanged {
                            oid,
                            cache: cache(oid),
                        },
                        RowChange::Removed => StatusChange::Removed,
                    };
                    inspect.inc(1);
                    StatusRow {
                        mount: ownership
                            .owner_for_path(&path)
                            .map(|owner| owner.name.clone()),
                        path,
                        change,
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
