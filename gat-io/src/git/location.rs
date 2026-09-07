use std::path::{Path, PathBuf};

use gat_core::git_location::GitLocationSpec;

use super::BoxedSource;

/// Semantic classification of a parsed Git repository location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitLocationKind {
    /// A bare local filesystem path that can be opened directly.
    LocalPath,
    /// A URL or scp-like location that must be cloned.
    Clone,
}

/// An invalid or under-specified Git repository location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GitLocationError {
    /// Gix could not parse the input as a recognized Git location.
    #[error(
        "not a recognized Git repository location (expected a local path, an \
         `https://`/`http://`/`ssh://`/`git://`/`file://` URL, or an scp-like \
         `[user@]host:path`)"
    )]
    Invalid,
    /// The parsed repository path has no usable basename.
    #[error(
        "has no usable repository name to infer a mount target from; specify TARGET explicitly"
    )]
    NoRepositoryName,
}

/// A validated Git repository location.
///
/// The parsed Gix URL remains private; callers can inspect only Gat's
/// semantic classification and derived repository name.
#[derive(Clone)]
pub struct GitLocation {
    parsed: gix::Url,
    kind: GitLocationKind,
}

impl std::fmt::Debug for GitLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitLocation")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl GitLocation {
    /// Returns whether this location is opened directly or cloned.
    #[must_use]
    pub const fn kind(&self) -> GitLocationKind {
        self.kind
    }

    /// Infers Gat's default mount-target basename.
    pub fn repository_name(&self) -> Result<PathBuf, GitLocationError> {
        let decoded = gix::path::from_bstr(&self.parsed.path);
        repo_basename(&decoded)
            .map(PathBuf::from)
            .ok_or(GitLocationError::NoRepositoryName)
    }
}

/// Parses and classifies a semantic Git location.
///
/// Parse failures deliberately retain neither the raw input nor Gix's
/// error because either may contain credentials.
pub fn parse_location(spec: &GitLocationSpec) -> Result<GitLocation, GitLocationError> {
    let parsed = gix::url::parse(spec.as_location_str()).map_err(|_| GitLocationError::Invalid)?;
    let kind = if parsed.scheme == gix::url::Scheme::File && parsed.serialize_alternative_form {
        GitLocationKind::LocalPath
    } else {
        GitLocationKind::Clone
    };
    Ok(GitLocation { parsed, kind })
}

/// Semantic stage at which repository cloning failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCloneErrorKind {
    PrepareDestination,
    PrepareClone,
    Fetch,
    Checkout,
}

impl std::fmt::Display for GitCloneErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PrepareDestination => "preparing the clone destination",
            Self::PrepareClone => "preparing to clone",
            Self::Fetch => "fetching",
            Self::Checkout => "checking out",
        })
    }
}

/// Failure to clone a repository.
#[derive(Debug)]
pub struct GitCloneError {
    kind: GitCloneErrorKind,
    destination: Option<PathBuf>,
    source: BoxedSource,
}

impl GitCloneError {
    #[must_use]
    pub const fn kind(&self) -> GitCloneErrorKind {
        self.kind
    }

    #[must_use]
    pub fn destination(&self) -> Option<&Path> {
        self.destination.as_deref()
    }

    fn new(
        kind: GitCloneErrorKind,
        destination: &Path,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            destination: Some(destination.to_path_buf()),
            source: Box::new(source),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn test_error(
        kind: GitCloneErrorKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            destination: None,
            source: Box::new(source),
        }
    }
}

impl std::fmt::Display for GitCloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(destination) = &self.destination {
            write!(
                f,
                "could not clone repository into `{}` ({})",
                destination.display(),
                self.kind
            )
        } else {
            write!(f, "could not clone repository ({})", self.kind)
        }
    }
}

impl std::error::Error for GitCloneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn clone_with_worktree_impl(
    location: &GitLocation,
    destination: &Path,
) -> Result<(), GitCloneError> {
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    let mut prepare = gix::clone::PrepareFetch::new(
        location.parsed.clone(),
        destination,
        gix::create::Kind::WithWorktree,
        gix::create::Options::default(),
        gix::open::Options::default(),
    )
    .map_err(|source| GitCloneError::new(GitCloneErrorKind::PrepareClone, destination, source))?;
    let (mut checkout, _) = prepare
        .fetch_then_checkout(gix::progress::Discard, &interrupt)
        .map_err(|source| GitCloneError::new(GitCloneErrorKind::Fetch, destination, source))?;
    checkout
        .main_worktree(gix::progress::Discard, &interrupt)
        .map_err(|source| GitCloneError::new(GitCloneErrorKind::Checkout, destination, source))?;
    Ok(())
}

/// A local worktree prepared from either a local path or a temporary clone.
pub struct PreparedGitWorktree {
    root: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}

impl PreparedGitWorktree {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Strictly resolves a revision in this prepared repository.
    pub fn resolve_commit(
        &self,
        revision: &gat_core::git::GitRevisionSpec,
    ) -> Result<gat_core::git::GitCommitId, super::ResolveCommitError> {
        super::resolve_commit_at_path(&self.root, revision)
    }

    /// Loads this prepared repository's project configuration.
    pub fn load_project_config(&self) -> Result<gat_core::config::Config, crate::ConfigError> {
        crate::ConfigStore::load_file(&self.root.join("gat.yaml"))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PrepareGitWorktreeError {
    #[error("the local source `{}` does not exist", path.display())]
    LocalSourceMissing { path: PathBuf },
    #[error("could not resolve the local source path `{}`", path.display())]
    ResolveLocal {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not create a temporary clone directory")]
    CreateTemporary(#[source] std::io::Error),
    #[error(transparent)]
    Clone(#[from] GitCloneError),
    #[error("could not resolve the cloned source path `{}`", path.display())]
    ResolveClone {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Prepares a repository location for semantic engine access.
pub fn prepare_worktree(
    location: &GitLocation,
    spec: &GitLocationSpec,
) -> Result<PreparedGitWorktree, PrepareGitWorktreeError> {
    if location.kind() == GitLocationKind::LocalPath {
        let local = Path::new(spec.as_location_str());
        if !local.exists() {
            return Err(PrepareGitWorktreeError::LocalSourceMissing {
                path: local.to_path_buf(),
            });
        }
        return Ok(PreparedGitWorktree {
            root: std::fs::canonicalize(local).map_err(|source| {
                PrepareGitWorktreeError::ResolveLocal {
                    path: local.to_path_buf(),
                    source,
                }
            })?,
            _temporary: None,
        });
    }

    let temporary = tempfile::tempdir().map_err(PrepareGitWorktreeError::CreateTemporary)?;
    clone_with_worktree_impl(location, temporary.path())?;
    let root = std::fs::canonicalize(temporary.path()).map_err(|source| {
        PrepareGitWorktreeError::ResolveClone {
            path: temporary.path().to_path_buf(),
            source,
        }
    })?;
    Ok(PreparedGitWorktree {
        root,
        _temporary: Some(temporary),
    })
}

fn clone_bare_impl(location: &GitLocation, destination: &Path) -> Result<(), GitCloneError> {
    if destination.exists() {
        std::fs::remove_dir_all(destination).map_err(|source| {
            GitCloneError::new(GitCloneErrorKind::PrepareDestination, destination, source)
        })?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|source| {
            GitCloneError::new(GitCloneErrorKind::PrepareDestination, destination, source)
        })?;
    }
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    let mut prepare = gix::clone::PrepareFetch::new(
        location.parsed.clone(),
        destination,
        gix::create::Kind::Bare,
        gix::create::Options::default(),
        gix::open::Options::default(),
    )
    .map_err(|source| GitCloneError::new(GitCloneErrorKind::PrepareClone, destination, source))?;
    prepare
        .fetch_only(gix::progress::Discard, &interrupt)
        .map_err(|source| GitCloneError::new(GitCloneErrorKind::Fetch, destination, source))?;
    Ok(())
}

/// A temporary bare repository prepared for semantic history inspection.
///
/// The reader is declared before the temporary directory so all Gix handles
/// close before cleanup, including on Windows.
pub struct PreparedBareGitRepository {
    reader: super::GitReader,
    _temporary: tempfile::TempDir,
}

impl PreparedBareGitRepository {
    /// Visits entries reachable from the selected repository history.
    pub fn visit_history_lock_entries<E>(
        &self,
        selection: &gat_core::history::HistorySelection,
        keep: impl Fn(&str) -> bool,
        visit: impl FnMut(&gat_core::lock::Entry) -> Result<(), E>,
    ) -> Result<super::HistoryStats, E>
    where
        E: From<super::GitHistoryError> + From<gat_core::lock::LockError>,
    {
        self.reader
            .visit_history_lock_entries(selection, keep, visit)
    }
}

/// Failure to prepare a temporary bare repository.
#[derive(Debug, thiserror::Error)]
pub enum PrepareBareGitRepositoryError {
    #[error("could not create a temporary bare clone directory")]
    CreateTemporary(#[source] std::io::Error),
    #[error(transparent)]
    Clone(#[from] GitCloneError),
    #[error(transparent)]
    Open(#[from] super::GitOpenError),
}

/// Bare-clones a location into an independently owned temporary repository.
pub fn prepare_bare_repository(
    location: &GitLocation,
) -> Result<PreparedBareGitRepository, PrepareBareGitRepositoryError> {
    let temporary = tempfile::Builder::new()
        .prefix("gat-bare-")
        .tempdir()
        .map_err(PrepareBareGitRepositoryError::CreateTemporary)?;
    prepare_bare_repository_in_owner(location, temporary)
}

fn prepare_bare_repository_in_owner(
    location: &GitLocation,
    temporary: tempfile::TempDir,
) -> Result<PreparedBareGitRepository, PrepareBareGitRepositoryError> {
    clone_bare_impl(location, temporary.path())?;
    let reader = super::GitReader::open_at_path(temporary.path())?;
    Ok(PreparedBareGitRepository {
        reader,
        _temporary: temporary,
    })
}

#[cfg(test)]
fn prepare_bare_repository_in(
    location: &GitLocation,
    parent: &Path,
) -> Result<PreparedBareGitRepository, PrepareBareGitRepositoryError> {
    let temporary = tempfile::Builder::new()
        .prefix("gat-bare-")
        .tempdir_in(parent)
        .map_err(PrepareBareGitRepositoryError::CreateTemporary)?;
    prepare_bare_repository_in_owner(location, temporary)
}

fn repo_basename(path: &Path) -> Option<std::ffi::OsString> {
    let file_name = path.file_name()?;
    let file_name = if file_name == ".git" {
        path.parent()?.file_name()?
    } else {
        file_name
    };
    let name = file_name.to_str()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    (!name.is_empty()).then(|| std::ffi::OsString::from(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<GitLocation, GitLocationError> {
        parse_location(&GitLocationSpec::from(raw))
    }

    fn git(root: &Path, args: &[&str]) {
        test_support_git::run_git(root, args);
    }

    fn file_url(path: &Path) -> String {
        let path = path.display().to_string().replace('\\', "/");
        if let Some(stripped) = path.strip_prefix('/') {
            format!("file:///{stripped}")
        } else {
            format!("file:///{path}")
        }
    }

    #[test]
    fn classifies_local_paths_and_clone_locations() {
        // hygiene-ok: path strings are parsed only and never touched on disk.
        for path in ["../models", "/tmp/models", "models"] {
            assert_eq!(parse(path).unwrap().kind(), GitLocationKind::LocalPath);
        }
        for url in [
            // hygiene-ok: URL strings are parsed only and never fetched.
            "https://github.com/acme/models",
            "ssh://git@github.com/acme/models.git",
            "git@github.com:acme/models.git",
            "file:///tmp/models.git",
        ] {
            assert_eq!(parse(url).unwrap().kind(), GitLocationKind::Clone);
        }
    }

    #[test]
    fn infers_repository_names_from_supported_forms() {
        for (raw, expected) in [
            // hygiene-ok: URL strings are parsed only and never fetched.
            ("https://github.com/acme/models.git", "models"),
            ("git@github.com:acme/models.git", "models"),
            ("../my_models_repo", "my_models_repo"),
            // hygiene-ok: path strings are parsed only and never touched on disk.
            ("/tmp/my-repo/.git", "my-repo"),
            // hygiene-ok: URL strings are parsed only and never fetched.
            ("https://github.com/acme/my%20models.git", "my models"),
        ] {
            assert_eq!(
                parse(raw).unwrap().repository_name().unwrap(),
                PathBuf::from(expected)
            );
        }
    }

    #[test]
    fn invalid_location_error_never_contains_raw_input() {
        // hygiene-ok: malformed URL text is parsed only to test redaction.
        let raw = "https://exa mple.com/path?token=marker-xyz-should-not-leak";
        let message = parse(raw).unwrap_err().to_string();
        assert!(!message.contains("marker-xyz-should-not-leak"));
        assert!(!message.contains("exa mple.com"));
    }

    #[test]
    fn prepared_local_worktree_exposes_semantic_revision_and_config() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-q"]);
        std::fs::write(source.path().join("tracked"), b"content").unwrap();
        std::fs::write(
            source.path().join("gat.yaml"),
            b"version: 1\nsync:\n  auto_fetch: true\n",
        )
        .unwrap();
        git(source.path(), &["add", "-A"]);
        git(source.path(), &["commit", "-q", "-m", "initial"]);

        let spec = GitLocationSpec::from_string(source.path().display().to_string());
        let location = parse_location(&spec).unwrap();
        let prepared = prepare_worktree(&location, &spec).unwrap();

        assert!(
            prepared
                .resolve_commit(&gat_core::git::GitRevisionSpec::from("HEAD"))
                .is_ok()
        );
        assert!(prepared.load_project_config().unwrap().sync.auto_fetch());
    }

    #[test]
    fn temporary_clone_lives_exactly_as_long_as_prepared_worktree() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-q"]);
        std::fs::write(source.path().join("tracked"), b"content").unwrap();
        git(source.path(), &["add", "-A"]);
        git(source.path(), &["commit", "-q", "-m", "initial"]);

        let spec = GitLocationSpec::from_string(file_url(source.path()));
        let location = parse_location(&spec).unwrap();
        let prepared = prepare_worktree(&location, &spec).unwrap();
        let cloned_root = prepared.root().to_path_buf();

        assert!(cloned_root.join("tracked").is_file());
        drop(prepared);
        assert!(!cloned_root.exists());
    }

    #[test]
    fn temporary_bare_clone_is_opened_once_and_removed_on_drop() {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-q"]);
        std::fs::write(source.path().join("tracked"), b"content").unwrap();
        git(source.path(), &["add", "-A"]);
        git(source.path(), &["commit", "-q", "-m", "initial"]);
        let spec = GitLocationSpec::from_string(file_url(source.path()));
        let location = parse_location(&spec).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let opens_before = super::super::test_support::repository_opens();

        let prepared = prepare_bare_repository_in(&location, parent.path()).unwrap();

        assert_eq!(
            super::super::test_support::repository_opens(),
            opens_before + 1
        );
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);
        drop(prepared);
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn failed_bare_clone_removes_its_partial_temporary_directory() {
        let missing = tempfile::tempdir().unwrap().path().join("missing.git");
        let spec = GitLocationSpec::from_string(file_url(&missing));
        let location = parse_location(&spec).unwrap();
        let parent = tempfile::tempdir().unwrap();

        assert!(prepare_bare_repository_in(&location, parent.path()).is_err());
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }
}
