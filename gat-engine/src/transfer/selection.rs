//! Streaming object selection for current desired state and Git history.

use crate::desired_snapshot::DesiredView;
use crate::history::HistoryError;
use crate::repository::Repository as Repo;
use gat_core::history::HistorySelection;
use gat_core::lexical_path::GatPath;
use gat_core::lock::LockError;
use gat_core::oid::Oid;
use gat_core::selection::Selection;

/// One required object with a deterministic representative tracked path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedObject {
    pub oid: Oid,
    pub representative_path: GatPath,
}

/// Streams selected desired-state rows through an engine-owned error surface.
pub fn visit_current_state_objects<E>(
    view: DesiredView<'_>,
    selection: &Selection,
    mut sink: impl FnMut(SelectedObject) -> Result<(), E>,
) -> Result<(), E>
where
    E: From<crate::RepositoryStateError>,
{
    view.visit_entries(selection, |entry| {
        sink(SelectedObject {
            oid: entry.oid,
            representative_path: entry.path,
        })
    })
}

/// Streams selected historical lock rows and reports shallow traversal.
#[doc(hidden)]
pub fn visit_history_objects<E>(
    repo: &Repo,
    history: &HistorySelection,
    selection: &Selection,
    mut sink: impl FnMut(SelectedObject) -> Result<(), E>,
) -> Result<bool, E>
where
    E: From<HistoryError> + From<LockError>,
{
    let stats = repo.visit_history_lock_entries::<E>(
        history,
        |path| selection.matches_str(path),
        |entry| {
            sink(SelectedObject {
                oid: entry.oid,
                representative_path: entry.path.clone(),
            })
        },
    )?;
    Ok(stats.shallow)
}
