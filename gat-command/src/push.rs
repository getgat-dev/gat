//! Bounded current- and history-aware push orchestration.

use super::selection::{self, ResolvedSelection};
use gat_core::history::HistorySelection;
use gat_core::lexical_path::GatPath;
use gat_core::name::{MountName, RemoteName};
use gat_core::oid::Oid;
use gat_core::progress::{
    ProgressActivity, ProgressHandle, ProgressOperation, ProgressReporter, ProgressSpec,
    ProgressUnit,
};
use gat_core::selection::Selection;
use gat_engine::{
    DesiredOperation, HistoryError, PublishError, PublishObject, PublishStatus, RemoteCatalogError,
    RemoteId, RemotePresenceError, RemoteSessionError, Repository, ResolvedRemote, StreamingWindow,
    UnknownRemoteOverrideError, UploadError, publish_window, visit_current_state_objects,
    visit_history_objects,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug)]
pub enum PushSource<'a> {
    Current,
    History(&'a HistorySelection),
}

#[derive(Clone, Copy, Debug)]
pub struct PushRequest<'a> {
    /// None uses configured defaults; Some replaces them completely.
    pub selection: Option<&'a Selection>,
    pub remote: Option<&'a RemoteName>,
    pub source: PushSource<'a>,
}

/// Why an object selected for push was not uploaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushSkipReason {
    MountOwned {
        owner_name: MountName,
        owner_target: GatPath,
    },
    CacheMissing,
    CacheCorrupt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushSkip {
    pub path: GatPath,
    pub reason: PushSkipReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushOutcome {
    pub scope: super::SelectionScope,
    /// Unique non-mount-owned objects selected for publication.
    pub total: usize,
    pub skipped: Vec<PushSkip>,
    pub shallow: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error(transparent)]
    Repository(Box<gat_engine::RepositoryError>),
    #[error(transparent)]
    Acquisition(#[from] Box<gat_engine::RepoSnapshotError>),
    #[error(transparent)]
    DesiredState(#[from] gat_engine::RepositoryStateError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Lock(#[from] gat_core::lock::LockError),
    #[error(transparent)]
    RemoteCatalog(#[from] RemoteCatalogError),
    #[error(transparent)]
    RemoteSession(#[from] RemoteSessionError),
    #[error(transparent)]
    UnknownOverride(#[from] UnknownRemoteOverrideError),
    #[error(transparent)]
    MissingRemoteConfig(#[from] super::MissingRemoteConfigError),
    #[error(transparent)]
    Presence(#[from] RemotePresenceError),
    #[error(transparent)]
    Upload(#[from] UploadError),
}

impl From<gat_engine::RepoSnapshotError> for PushError {
    fn from(error: gat_engine::RepoSnapshotError) -> Self {
        Self::Acquisition(Box::new(error))
    }
}

impl From<PublishError> for PushError {
    fn from(error: PublishError) -> Self {
        match error {
            PublishError::Presence(source) => Self::Presence(source),
            PublishError::Upload(source) => Self::Upload(source),
        }
    }
}

pub fn push(
    repo: &Repository,
    request: PushRequest<'_>,
    progress: &dyn ProgressReporter,
) -> Result<PushOutcome, PushError> {
    let mut desired = DesiredOperation::acquire(repo, progress)?;
    push_with_desired_operation(&mut desired, request, progress)
}

#[doc(hidden)]
pub fn push_with_desired_operation(
    desired: &mut DesiredOperation<'_>,
    request: PushRequest<'_>,
    progress: &dyn ProgressReporter,
) -> Result<PushOutcome, PushError> {
    let (operation, desired_view) = desired.split_for_selection();
    let repo = operation.repo();
    let ResolvedSelection { selection, scope } =
        selection::resolve(request.selection, operation.config())?;
    let history_selected = matches!(request.source, PushSource::History(_));
    let allow_mount_owned = selection
        .scope_path()
        .is_some_and(|scope| operation.policy().owner_for_path(scope).is_some());
    let mut window: StreamingWindow<(RemoteId, Oid), PushObligation> =
        StreamingWindow::new(operation.limits().transfer.window);
    let mut root_owned_oids = BTreeSet::new();
    let mut skipped = Vec::<(usize, PushSkip)>::new();
    let mut next_index = 0usize;

    let pushing = progress.begin(ProgressSpec::items(
        ProgressOperation::Pushing,
        ProgressUnit::Entries,
        None,
    ));
    let task = pushing.handle();
    task.set_activity(ProgressActivity::selection_or_state(history_selected));

    let mut visit = |path: &GatPath, oid: Oid| -> Result<(), PushError> {
        let selected_index = next_index;
        next_index += 1;
        let policy = operation.policy();
        if !allow_mount_owned && let Some(owner) = policy.owner_for_path(path) {
            skipped.push((
                selected_index,
                PushSkip {
                    path: path.clone(),
                    reason: PushSkipReason::MountOwned {
                        owner_name: owner.name.clone(),
                        owner_target: owner.target.clone(),
                    },
                },
            ));
            return Ok(());
        }

        let remote = policy
            .resolved_remote_for_path(operation.remotes_catalog(), request.remote, path)?
            .ok_or_else(|| super::MissingRemoteConfigError { path: path.clone() })?;
        root_owned_oids.insert(oid);
        let key = (remote.id(), oid);
        window.record(
            key,
            || PushObligation {
                oid,
                representative_path: path.clone(),
                remote,
                selected_index,
            },
            |batch| run_push_window(operation, batch.drain(), &mut skipped, &task),
        )
    };

    let shallow = match request.source {
        PushSource::Current => {
            visit_current_state_objects(desired_view, &selection, |object| {
                visit(&object.representative_path, object.oid)
            })?;
            false
        }
        PushSource::History(history) => {
            visit_history_objects(repo, history, &selection, |object| {
                visit(&object.representative_path, object.oid)
            })?
        }
    };

    window.finish(|batch| run_push_window(operation, batch.drain(), &mut skipped, &task))?;
    pushing.finish();

    let total = root_owned_oids.len();
    if total == 0
        && let Some(name) = request.remote
    {
        let id = operation.remotes_catalog().resolve(Some(name))?;
        operation.validate_remote(id)?;
    }

    skipped.sort_by_key(|(index, _)| *index);
    Ok(PushOutcome {
        scope,
        total,
        skipped: skipped.into_iter().map(|(_, skip)| skip).collect(),
        shallow,
    })
}

struct PushObligation {
    oid: Oid,
    representative_path: GatPath,
    remote: ResolvedRemote,
    selected_index: usize,
}

fn run_push_window(
    operation: &mut gat_engine::Operation<'_>,
    obligations: std::vec::Drain<'_, PushObligation>,
    skipped: &mut Vec<(usize, PushSkip)>,
    task: &ProgressHandle,
) -> Result<(), PushError> {
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_push_window(obligations.len());

    let (objects, metadata): (Vec<_>, Vec<_>) = obligations
        .map(|obligation| {
            let object = PublishObject::new(
                obligation.oid,
                obligation.representative_path.clone(),
                obligation.remote,
            );
            (
                object,
                (
                    obligation.oid,
                    obligation.selected_index,
                    obligation.representative_path,
                ),
            )
        })
        .unzip();
    let outcome = publish_window(operation, objects, task)?;
    debug_assert_eq!(metadata.len(), outcome.statuses.len());

    let mut cache_skips = BTreeMap::<Oid, (usize, GatPath, PushSkipReason)>::new();
    for ((oid, selected_index, path), status) in metadata.into_iter().zip(outcome.statuses) {
        let reason = match status {
            PublishStatus::AlreadyPresent | PublishStatus::Uploaded => None,
            PublishStatus::CacheMissing => Some(PushSkipReason::CacheMissing),
            PublishStatus::CacheCorrupt => Some(PushSkipReason::CacheCorrupt),
        };
        if let Some(reason) = reason {
            cache_skips
                .entry(oid)
                .and_modify(|existing| {
                    if selected_index < existing.0 {
                        *existing = (selected_index, path.clone(), reason.clone());
                    }
                })
                .or_insert((selected_index, path, reason));
        }
    }
    skipped.extend(
        cache_skips
            .into_values()
            .map(|(selected_index, path, reason)| (selected_index, PushSkip { path, reason })),
    );
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static PUSH_WINDOW_CALLS: Cell<usize> = const { Cell::new(0) };
        static PUSH_WINDOW_HIGH_WATER: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record_push_window(size: usize) {
        PUSH_WINDOW_CALLS.with(|count| count.set(count.get() + 1));
        PUSH_WINDOW_HIGH_WATER.with(|high_water| high_water.set(high_water.get().max(size)));
    }

    pub fn push_window_calls() -> usize {
        PUSH_WINDOW_CALLS.with(Cell::get)
    }

    pub fn push_window_high_water() -> usize {
        PUSH_WINDOW_HIGH_WATER.with(Cell::get)
    }
}

impl From<gat_engine::RepositoryError> for PushError {
    fn from(error: gat_engine::RepositoryError) -> Self {
        Self::Repository(Box::new(error))
    }
}
