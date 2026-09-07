//! Use-case boundary for `gat gc`.

use gat_core::history::{HistoryRequest, HistorySelection};
use gat_core::lifecycle;
use gat_core::name::RemoteName;
use gat_core::progress::ProgressReporter;
use gat_engine::{GcOptions, Repository};

pub use gat_engine::{
    GcError as GcEngineError, GcFailure, GcFailureKind, GcRepositoryFailureKind, GcRepositoryIssue,
};

#[derive(Clone, Debug)]
pub struct GcRequest {
    pub dry_run: bool,
    pub unsafe_override: bool,
    pub remote: Option<RemoteName>,
    pub repositories: Vec<gat_core::git_location::GitLocationSpec>,
    pub history: HistoryRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcOutcome {
    pub dry_run: bool,
    pub deleted: usize,
    pub uncertain: usize,
    pub incomplete_repositories: usize,
    pub forced_incomplete_keep_set: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error(transparent)]
    Engine(#[from] GcEngineError),
}

pub fn gc(
    repo: &Repository,
    request: GcRequest,
    progress: &dyn ProgressReporter,
) -> Result<GcOutcome, GcError> {
    gc_with_lifecycle_observer(repo, request, progress, &|_| {})
}

pub fn gc_with_lifecycle_observer(
    repo: &Repository,
    request: GcRequest,
    progress: &dyn ProgressReporter,
    observe: &dyn Fn(lifecycle::Surface<'_>),
) -> Result<GcOutcome, GcError> {
    observe(lifecycle::Surface::Command("gc"));
    let history = match request.history {
        HistoryRequest::CommandDefault => Some(HistorySelection::conservative_default()),
        HistoryRequest::Disabled => None,
        HistoryRequest::Selected(selection) => Some(selection),
    };
    let report = repo.garbage_collect(
        &GcOptions {
            dry_run: request.dry_run,
            unsafe_override: request.unsafe_override,
            remote: request.remote.as_ref(),
            repositories: &request.repositories,
            history: history.as_ref(),
        },
        progress,
    )?;
    Ok(GcOutcome {
        dry_run: request.dry_run,
        deleted: report.deleted,
        uncertain: report.uncertain,
        incomplete_repositories: report.incomplete_repositories,
        forced_incomplete_keep_set: report.forced_incomplete_keep_set,
    })
}
