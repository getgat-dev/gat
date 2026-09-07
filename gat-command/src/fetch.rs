//! Bounded current- and history-aware fetch orchestration.

use super::selection::{self, ResolvedSelection};
use gat_core::history::HistorySelection;
use gat_core::name::RemoteName;
use gat_core::oid::Oid;
use gat_core::progress::{
    ProgressActivity, ProgressHandle, ProgressOperation, ProgressReporter, ProgressSpec,
    ProgressUnit,
};
use gat_core::selection::Selection;
use gat_engine::{
    DesiredOperation, DownloadError, DownloadObject, HistoryError, RemoteCatalogError,
    RemoteSessionError, Repository, SelectedObject, StreamingWindow, UnknownRemoteOverrideError,
    download_window, visit_current_state_objects, visit_history_objects,
};

#[derive(Clone, Copy, Debug)]
pub enum FetchSource<'a> {
    Current,
    History(&'a HistorySelection),
    CurrentAndHistory(Option<&'a HistorySelection>),
}

#[derive(Clone, Copy, Debug)]
pub struct FetchRequest<'a> {
    /// None uses configured defaults; Some replaces them completely.
    pub selection: Option<&'a Selection>,
    pub remote: Option<&'a RemoteName>,
    pub source: FetchSource<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchOutcome {
    pub scope: super::SelectionScope,
    pub fetched: usize,
    pub shallow: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
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
    Download(#[from] DownloadError),
}

impl From<gat_engine::RepoSnapshotError> for FetchError {
    fn from(error: gat_engine::RepoSnapshotError) -> Self {
        Self::Acquisition(Box::new(error))
    }
}

pub fn fetch(
    repo: &Repository,
    request: FetchRequest<'_>,
    progress: &dyn ProgressReporter,
) -> Result<FetchOutcome, FetchError> {
    let mut desired = DesiredOperation::acquire(repo, progress)?;
    fetch_with_desired_operation(&mut desired, request, progress)
}

pub fn fetch_with_desired_operation(
    desired: &mut DesiredOperation<'_>,
    request: FetchRequest<'_>,
    progress: &dyn ProgressReporter,
) -> Result<FetchOutcome, FetchError> {
    let (operation, desired_view) = desired.split_for_selection();
    let repo = operation.repo();
    let ResolvedSelection { selection, scope } =
        selection::resolve(request.selection, operation.config())?;
    let history_selected = matches!(
        request.source,
        FetchSource::History(_) | FetchSource::CurrentAndHistory(Some(_))
    );
    let fetching = progress.begin(ProgressSpec::items(
        ProgressOperation::Fetching,
        ProgressUnit::Objects,
        None,
    ));
    let task = fetching.handle();
    task.set_activity(ProgressActivity::selection_or_state(history_selected));

    let mut window: StreamingWindow<Oid, SelectedObject> =
        StreamingWindow::new(operation.limits().transfer.window);
    let mut fetched = 0usize;
    let mut visit = |object: SelectedObject| -> Result<(), FetchError> {
        let oid = object.oid;
        window.record(
            oid,
            || object,
            |batch| {
                run_fetch_window(
                    operation,
                    batch.drain(),
                    request.remote,
                    &mut fetched,
                    &task,
                )
            },
        )
    };

    let shallow = match request.source {
        FetchSource::Current => {
            visit_current_state_objects(desired_view, &selection, &mut visit)?;
            false
        }
        FetchSource::History(history) => {
            visit_history_objects(repo, history, &selection, &mut visit)?
        }
        FetchSource::CurrentAndHistory(history) => {
            visit_current_state_objects(desired_view, &selection, &mut visit)?;
            match history {
                Some(history) => visit_history_objects(repo, history, &selection, &mut visit)?,
                None => false,
            }
        }
    };

    let saw_any = window.unique_count() > 0;
    window.finish(|batch| {
        run_fetch_window(
            operation,
            batch.drain(),
            request.remote,
            &mut fetched,
            &task,
        )
    })?;
    fetching.finish();

    if !saw_any && let Some(name) = request.remote {
        let id = operation.remotes_catalog().resolve(Some(name))?;
        operation.validate_remote(id)?;
    }

    Ok(FetchOutcome {
        scope,
        fetched,
        shallow,
    })
}

fn run_fetch_window(
    operation: &mut gat_engine::Operation<'_>,
    objects: std::vec::Drain<'_, SelectedObject>,
    remote: Option<&RemoteName>,
    fetched: &mut usize,
    task: &ProgressHandle,
) -> Result<(), FetchError> {
    let downloads = {
        let policy = operation.policy();
        let catalog = operation.remotes_catalog();
        objects
            .map(|object| {
                let resolved = policy
                    .resolved_remote_for_path(catalog, remote, &object.representative_path)?
                    .ok_or_else(|| super::MissingRemoteConfigError {
                        path: object.representative_path.clone(),
                    })?;
                Ok(DownloadObject::new(
                    object.oid,
                    object.representative_path,
                    resolved,
                ))
            })
            .collect::<Result<Vec<_>, FetchError>>()?
    };
    *fetched += download_window(operation, downloads, task)?.downloaded;
    Ok(())
}

impl From<gat_engine::RepositoryError> for FetchError {
    fn from(error: gat_engine::RepositoryError) -> Self {
        Self::Repository(Box::new(error))
    }
}
