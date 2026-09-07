//! Physical Git repository access.
//!
//! This module exposes semantic repository facts while keeping Gix
//! repositories and concrete Gix errors inside `gat-io`.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gat_core::git::{GitCommitId, GitRevisionSpec};
use gat_core::history::HistorySelection;
use gat_core::lock::Entry;

use crate::RepositoryLayout;

mod discovery;
mod exclude;
mod history;
mod integration;
mod location;
mod lock_snapshot;

pub use discovery::{
    AddExclusion, AddExclusionReason, GatIgnore, GatIgnoreError, GitDiscovery, GitDiscoveryError,
    GitDiscoveryErrorKind, GitIgnoreMatcher, GitPathStatus,
};
#[cfg(any(test, feature = "test-support"))]
pub use exclude::test_support as info_exclude_test_support;
pub(crate) use exclude::verify_info_exclude;
pub use exclude::{
    InfoExcludeError, InfoExcludeMutation, InfoExcludeSnapshot, InfoExcludeUpdate,
    InfoExcludeVerification, mutate_info_exclude, read_info_exclude,
};
pub use history::{GitHistoryError, GitHistoryErrorKind, HistoryStats, parse_cli_date};
pub use integration::{
    GitIntegration, GitIntegrationError, GitIntegrationErrorKind, GitIntegrationStatus,
};
pub use location::{
    GitCloneError, GitCloneErrorKind, GitLocation, GitLocationError, GitLocationKind,
    PrepareBareGitRepositoryError, PrepareGitWorktreeError, PreparedBareGitRepository,
    PreparedGitWorktree, parse_location, prepare_bare_repository, prepare_worktree,
};
#[cfg(any(test, feature = "test-support"))]
pub use lock_snapshot::test_support as lock_snapshot_test_support;
pub use lock_snapshot::{LockSnapshot, LockSnapshotError, LockSnapshotErrorKind, SnapshotShard};

type BoxedSource = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Opaque, reusable access to one opened Git repository.
///
/// Gix repository, object, tree, blob, ref, and walk values never cross this
/// boundary. Cloned snapshots share the same opened repository.
pub struct GitReader {
    repo: Rc<gix::Repository>,
    root: PathBuf,
}

impl GitReader {
    /// Opens one repository for reuse across related Git reads.
    pub fn open(layout: &RepositoryLayout) -> Result<Self, GitOpenError> {
        Self::open_at_path(layout.root_path())
    }

    pub(super) fn open_at_path(root: &Path) -> Result<Self, GitOpenError> {
        let repo = gix::open(root).map_err(|source| GitOpenError {
            path: root.to_path_buf(),
            source: Box::new(source),
        })?;
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_repository_open();
        Ok(Self {
            repo: Rc::new(repo),
            root: root.to_path_buf(),
        })
    }

    /// Reads the staged `gat.lock` through this opened repository.
    pub fn staged_lock_snapshot(&self) -> Result<LockSnapshot, LockSnapshotError> {
        LockSnapshot::staged_with(Rc::clone(&self.repo), &self.root)
    }

    /// Reads `gat.lock` at one revision through this opened repository.
    pub fn lock_snapshot_at(
        &self,
        revision: &GitRevisionSpec,
    ) -> Result<LockSnapshot, LockSnapshotError> {
        LockSnapshot::at_rev_with(Rc::clone(&self.repo), &self.root, revision)
    }

    /// Visits each selected commit exactly once in deterministic ID order.
    pub fn visit_history_commits<E>(
        &self,
        selection: &HistorySelection,
        visit: impl FnMut(GitCommitId) -> Result<(), E>,
    ) -> Result<HistoryStats, E>
    where
        E: From<GitHistoryError>,
    {
        history::visit_history_commits_with(&self.repo, selection, visit)
    }

    /// Visits selected entries from every distinct historical `gat.lock`.
    pub fn visit_history_lock_entries<E>(
        &self,
        selection: &HistorySelection,
        keep: impl Fn(&str) -> bool,
        visit: impl FnMut(&Entry) -> Result<(), E>,
    ) -> Result<HistoryStats, E>
    where
        E: From<GitHistoryError> + From<gat_core::lock::LockError>,
    {
        history::visit_history_lock_entries_with(&self.repo, selection, keep, visit)
    }
}

/// Failure to open a Git repository for structural path discovery.
#[derive(Debug, thiserror::Error)]
#[error("could not open the git repository at `{}`", path.display())]
pub struct GitOpenError {
    path: PathBuf,
    #[source]
    source: Box<gix::open::Error>,
}

impl GitOpenError {
    /// OS classification of repository discovery/configuration access.
    #[must_use]
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        match &*self.source {
            gix::open::Error::Io(source)
            | gix::open::Error::Config(gix::config::Error::Io { source, .. }) => {
                Some(source.kind())
            }
            gix::open::Error::NotARepository {
                source:
                    gix::discover::is_git::Error::Metadata { source, .. }
                    | gix::discover::is_git::Error::MissingCommonDir { source, .. }
                    | gix::discover::is_git::Error::GitFile(
                        gix::discover::path::from_gitdir_file::Error::Io(source),
                    )
                    | gix::discover::is_git::Error::CurrentDir(source)
                    | gix::discover::is_git::Error::FindHeadRef(
                        gix::refs::file::find::existing::Error::Find(
                            gix::refs::file::find::Error::ReadFileContents { source, .. },
                        ),
                    ),
                ..
            } => Some(source.kind()),
            gix::open::Error::Config(
                gix::config::Error::Init(gix_config::file::init::Error::Includes(source))
                | gix::config::Error::ResolveIncludes(source)
                | gix::config::Error::FromEnv(gix_config::file::init::from_env::Error::Includes(
                    source,
                )),
            ) => match source {
                gix_config::file::includes::Error::CopyBuffer(source)
                | gix_config::file::includes::Error::Io { source, .. } => Some(source.kind()),
                gix_config::file::includes::Error::Realpath(
                    gix::path::realpath::Error::ReadLink(source)
                    | gix::path::realpath::Error::CurrentWorkingDir(source),
                ) => Some(source.kind()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The repository path that could not be opened.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Returns the repository's common Git directory.
///
/// Opening is isolated from system, user, application, and environment
/// configuration because common-directory discovery is purely structural.
fn common_dir_at(root: &Path) -> Result<PathBuf, GitOpenError> {
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_common_dir_resolution();
    let repo =
        gix::open_opts(root, gix::open::Options::isolated()).map_err(|source| GitOpenError {
            path: root.to_path_buf(),
            source: Box::new(source),
        })?;
    Ok(repo.common_dir().to_path_buf())
}

/// Semantic stage at which strict commit resolution failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveCommitErrorKind {
    OpenRepository,
    ResolveRevision,
    UnsupportedHashKind,
}

/// Failure to resolve a revision to an exact commit identity.
#[derive(Debug)]
pub struct ResolveCommitError {
    kind: ResolveCommitErrorKind,
    root: PathBuf,
    revision: String,
    source: Option<BoxedSource>,
}

impl ResolveCommitError {
    /// The semantic stage that failed.
    #[must_use]
    pub const fn kind(&self) -> ResolveCommitErrorKind {
        self.kind
    }

    /// The unresolved revision expression.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

impl std::fmt::Display for ResolveCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ResolveCommitErrorKind::OpenRepository => {
                write!(
                    f,
                    "could not open the git repository at `{}`",
                    self.root.display()
                )
            }
            ResolveCommitErrorKind::ResolveRevision => {
                write!(f, "could not resolve revision `{}`", self.revision)
            }
            ResolveCommitErrorKind::UnsupportedHashKind => {
                write!(f, "the resolved commit uses an unsupported hash kind")
            }
        }
    }
}

impl std::error::Error for ResolveCommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

fn resolve_commit_object<'repo>(
    repo: &'repo gix::Repository,
    revision: &str,
) -> Result<gix::Commit<'repo>, BoxedSource> {
    repo.rev_parse_single(revision)
        .map_err(|source| Box::new(source) as BoxedSource)?
        .object()
        .map_err(|source| Box::new(source) as BoxedSource)?
        .peel_to_commit()
        .map_err(|source| Box::new(source) as BoxedSource)
}

/// Strictly resolves `revision` to a commit and returns its semantic ID.
///
/// The repository, revision object, commit, and concrete Gix errors remain
/// private to this operation.
pub fn resolve_commit(
    layout: &RepositoryLayout,
    revision: &GitRevisionSpec,
) -> Result<GitCommitId, ResolveCommitError> {
    resolve_commit_at_path(layout.root_path(), revision)
}

pub fn resolve_commit_at_path(
    root: &Path,
    revision: &GitRevisionSpec,
) -> Result<GitCommitId, ResolveCommitError> {
    let revision_text = revision.as_str();
    let repo = gix::open(root).map_err(|source| ResolveCommitError {
        kind: ResolveCommitErrorKind::OpenRepository,
        root: root.to_path_buf(),
        revision: revision_text.to_string(),
        source: Some(Box::new(source)),
    })?;
    let commit =
        resolve_commit_object(&repo, revision_text).map_err(|source| ResolveCommitError {
            kind: ResolveCommitErrorKind::ResolveRevision,
            root: root.to_path_buf(),
            revision: revision_text.to_string(),
            source: Some(source),
        })?;
    match commit.id {
        gix::ObjectId::Sha1(bytes) => Ok(GitCommitId::Sha1(bytes)),
        gix::ObjectId::Sha256(bytes) => Ok(GitCommitId::Sha256(bytes)),
        #[allow(unreachable_patterns)]
        _ => Err(ResolveCommitError {
            kind: ResolveCommitErrorKind::UnsupportedHashKind,
            root: root.to_path_buf(),
            revision: revision_text.to_string(),
            source: None,
        }),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static REPOSITORY_OPENS: Cell<usize> = const { Cell::new(0) };
        static COMMON_DIR_RESOLUTIONS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn record_repository_open() {
        REPOSITORY_OPENS.with(|counter| counter.set(counter.get() + 1));
    }

    pub fn repository_opens() -> usize {
        REPOSITORY_OPENS.with(Cell::get)
    }

    pub(crate) fn record_common_dir_resolution() {
        COMMON_DIR_RESOLUTIONS.with(|counter| counter.set(counter.get() + 1));
    }

    pub fn common_dir_resolutions() -> usize {
        COMMON_DIR_RESOLUTIONS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_dir_reports_the_git_directory_without_exposing_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        gix::init(tmp.path()).unwrap();

        assert_eq!(common_dir_at(tmp.path()).unwrap(), tmp.path().join(".git"));
    }

    #[test]
    fn common_dir_errors_name_the_requested_path() {
        let tmp = tempfile::tempdir().unwrap();

        let err = common_dir_at(tmp.path()).unwrap_err();

        assert_eq!(err.path(), tmp.path());
    }

    #[test]
    fn resolve_commit_classifies_an_unborn_head_without_exposing_gix_types() {
        let tmp = tempfile::tempdir().unwrap();
        gix::init(tmp.path()).unwrap();
        let layout = RepositoryLayout::at(tmp.path().to_path_buf());

        let err = resolve_commit(&layout, &GitRevisionSpec::from("HEAD")).unwrap_err();

        assert_eq!(err.kind(), ResolveCommitErrorKind::ResolveRevision);
        assert_eq!(err.revision(), "HEAD");
    }

    #[test]
    fn git_integration_resolves_the_common_directory_once() {
        let tmp = tempfile::tempdir().unwrap();
        gix::init(tmp.path()).unwrap();
        let layout = RepositoryLayout::at(tmp.path().to_path_buf());
        let before = test_support::common_dir_resolutions();

        let integration = GitIntegration::open(&layout).unwrap();
        assert_eq!(integration.read_hook("pre-commit").unwrap(), None);
        assert_eq!(integration.read_hook("pre-push").unwrap(), None);

        assert_eq!(test_support::common_dir_resolutions() - before, 1);
    }
}
