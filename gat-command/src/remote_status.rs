//! History-aware remote-presence status orchestration.

use super::selection::{self, ResolvedSelection};
use gat_core::history::HistorySelection;
use gat_core::lexical_path::GatPath;
use gat_core::name::{RemoteName, RouteName};
use gat_core::oid::Oid;
use gat_core::progress::{
    ProgressActivity, ProgressHandle, ProgressOperation, ProgressReporter, ProgressSpec,
    ProgressUnit,
};
use gat_core::selection::Selection;
use gat_engine::{
    DesiredOperation, HistoryError, RemoteCatalogError, RemoteId, RemotePresenceError,
    RemotePresenceObligation, RemoteSessionError, Repository, ResolvedRemote, SelectedObject,
    StreamingWindow, UnknownRemoteOverrideError, visit_current_state_objects,
    visit_history_objects,
};

#[derive(Clone, Copy, Debug)]
pub struct RemoteStatusRequest<'a> {
    /// None uses configured defaults; Some replaces them completely.
    pub selection: Option<&'a Selection>,
    pub remote: Option<&'a RemoteName>,
    pub history: Option<&'a HistorySelection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingRemoteObject {
    pub object: SelectedObject,
    pub remote_name: RemoteName,
    pub route: Option<RouteName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteStatusOutcome {
    pub scope: super::SelectionScope,
    pub checked: usize,
    pub missing: Vec<MissingRemoteObject>,
    pub shallow: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("no remote configured for `{path}`")]
pub struct MissingRemoteConfigError {
    pub path: GatPath,
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteStatusError {
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
    MissingRemoteConfig(#[from] MissingRemoteConfigError),
    #[error(transparent)]
    Presence(#[from] RemotePresenceError),
}

impl From<gat_engine::RepoSnapshotError> for RemoteStatusError {
    fn from(error: gat_engine::RepoSnapshotError) -> Self {
        Self::Acquisition(Box::new(error))
    }
}

pub fn remote_status(
    repo: &Repository,
    request: RemoteStatusRequest<'_>,
    progress: &dyn ProgressReporter,
) -> Result<RemoteStatusOutcome, RemoteStatusError> {
    let mut desired = DesiredOperation::acquire(repo, progress)?;
    remote_status_with_desired_operation(&mut desired, request, progress)
}

#[doc(hidden)]
pub fn remote_status_with_desired_operation(
    desired: &mut DesiredOperation<'_>,
    request: RemoteStatusRequest<'_>,
    progress: &dyn ProgressReporter,
) -> Result<RemoteStatusOutcome, RemoteStatusError> {
    let (operation, desired_view) = desired.split_for_selection();
    let ResolvedSelection { selection, scope } =
        selection::resolve(request.selection, operation.config())?;
    let repo = operation.repo();
    let checking = progress.begin(ProgressSpec::items(
        ProgressOperation::RemoteStatus,
        ProgressUnit::Entries,
        None,
    ));
    let task = checking.handle();

    if let Some(name) = request.remote {
        let id = operation.remotes_catalog().resolve(Some(name))?;
        operation.validate_remote(id)?;
    }

    let mut window: StreamingWindow<(RemoteId, Oid), StatusObligation> =
        StreamingWindow::new(operation.limits().transfer.window);
    let mut checked = 0usize;
    let mut missing = Vec::new();
    task.set_activity(ProgressActivity::selection_or_state(
        request.history.is_some(),
    ));

    let mut visit = |object: SelectedObject| -> Result<(), RemoteStatusError> {
        let remote = operation
            .policy()
            .resolved_remote_for_path(
                operation.remotes_catalog(),
                request.remote,
                &object.representative_path,
            )?
            .ok_or_else(|| MissingRemoteConfigError {
                path: object.representative_path.clone(),
            })?;
        let key = (remote.id(), object.oid);
        window.record(
            key,
            || StatusObligation {
                object: Some(object),
                remote,
            },
            |mut batch| {
                run_status_window(
                    operation,
                    batch.as_mut_slice(),
                    &mut checked,
                    &mut missing,
                    &task,
                )
            },
        )
    };

    let shallow = if let Some(history) = request.history {
        visit_history_objects(repo, history, &selection, &mut visit)?
    } else {
        visit_current_state_objects(desired_view, &selection, &mut visit)?;
        false
    };

    window.finish(|mut batch| {
        run_status_window(
            operation,
            batch.as_mut_slice(),
            &mut checked,
            &mut missing,
            &task,
        )
    })?;
    checking.finish();

    Ok(RemoteStatusOutcome {
        scope,
        checked,
        missing,
        shallow,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusObligation {
    object: Option<SelectedObject>,
    remote: ResolvedRemote,
}

impl RemotePresenceObligation for StatusObligation {
    fn oid(&self) -> Oid {
        self.object.as_ref().expect("object is present").oid
    }

    fn resolved_remote(&self) -> &ResolvedRemote {
        &self.remote
    }

    fn representative_path(&self) -> &GatPath {
        &self
            .object
            .as_ref()
            .expect("object is present")
            .representative_path
    }
}

fn run_status_window(
    operation: &mut gat_engine::Operation<'_>,
    obligations: &mut [StatusObligation],
    checked: &mut usize,
    missing: &mut Vec<MissingRemoteObject>,
    task: &ProgressHandle,
) -> Result<(), RemoteStatusError> {
    task.set_activity(ProgressActivity::CheckingRemote);

    // Each result slot is filled in as soon as its own check completes
    // (`checked` and progress advance immediately, not after the whole window), but
    // `missing` below is always built by walking `obligations` in their
    // original order afterwards, so the reported output never depends on
    // completion order.
    let mut present = vec![false; obligations.len()];
    operation.check_remote_presence_streaming(
        obligations,
        |result| {
            *checked += 1;
            task.inc(1);
            task.set_activity(ProgressActivity::CheckedRemoteObject {
                path: obligations[result.request_index]
                    .representative_path()
                    .clone(),
            });
            present[result.request_index] = result.present;
        },
        task,
    )?;

    let catalog = operation.remotes_catalog();
    let policy = operation.policy();
    for (obligation, present) in obligations.iter_mut().zip(present) {
        if !present {
            let remote = obligation.remote;
            let object = obligation
                .object
                .take()
                .expect("each presence result addresses one obligation");
            missing.push(MissingRemoteObject {
                object,
                remote_name: catalog.remote_name(remote.id()),
                route: policy.route_name(&remote).cloned(),
            });
        }
    }
    Ok(())
}

impl From<gat_engine::RepositoryError> for RemoteStatusError {
    fn from(error: gat_engine::RepositoryError) -> Self {
        Self::Repository(Box::new(error))
    }
}
