use std::path::{Path, PathBuf};

use gat_core::lexical_path::{GatPath, LexicalPathError};
use gix::bstr::ByteSlice;

use super::BoxedSource;
use crate::RepositoryLayout;

/// Semantic stage at which Git path discovery failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitDiscoveryErrorKind {
    OpenRepository,
    ReadIndex,
    ConfigureWalk,
    Walk,
    InvalidPath,
}

/// Failure to discover or classify worktree paths through Git.
#[derive(Debug)]
pub struct GitDiscoveryError {
    kind: GitDiscoveryErrorKind,
    root: PathBuf,
    operation: String,
    source: BoxedSource,
}

impl GitDiscoveryError {
    #[must_use]
    pub const fn kind(&self) -> GitDiscoveryErrorKind {
        self.kind
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    fn new(
        kind: GitDiscoveryErrorKind,
        root: &Path,
        operation: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            root: root.to_path_buf(),
            operation: operation.into(),
            source: Box::new(source),
        }
    }
}

impl std::fmt::Display for GitDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.kind == GitDiscoveryErrorKind::InvalidPath {
            write!(f, "{} failed: {}", self.operation, self.source)
        } else {
            write!(f, "{} failed", self.operation)
        }
    }
}

impl std::error::Error for GitDiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Git's semantic status for one root-relative path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GitPathStatus {
    pub tracked: bool,
    pub gitignored: bool,
}

/// Why an observed add input was excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddExclusionReason {
    GitIgnore,
    GatIgnore,
    GitTracked,
    Infrastructure,
}

/// Counts describe observed entries only; pruned directory contents are unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddExclusion {
    pub reason: AddExclusionReason,
    pub files: usize,
    pub directories: usize,
    pub samples: Vec<GatPath>,
}

impl AddExclusion {
    pub fn record(
        summary: &mut Vec<Self>,
        reason: AddExclusionReason,
        path: GatPath,
        directory: bool,
    ) {
        let index = summary
            .iter()
            .position(|item| item.reason == reason)
            .unwrap_or_else(|| {
                summary.push(Self {
                    reason,
                    files: 0,
                    directories: 0,
                    samples: Vec::new(),
                });
                summary.len() - 1
            });
        let item = &mut summary[index];
        if directory {
            item.directories += 1;
        } else {
            item.files += 1;
        }
        match item.samples.binary_search(&path) {
            Ok(_) => {}
            Err(index) if index < 3 => {
                item.samples.insert(index, path);
                item.samples.truncate(3);
            }
            Err(_) => {}
        }
    }
}

/// An opened Git path-discovery capability.
///
/// Repository, index, exclude-stack, and dirwalk types remain private.
pub struct GitDiscovery {
    root: PathBuf,
    repo: gix::Repository,
    index: gix::worktree::Index,
}

impl std::fmt::Debug for GitDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitDiscovery")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl GitDiscovery {
    /// Opens a repository and its index for repeated discovery queries.
    pub fn open(layout: &RepositoryLayout) -> Result<Self, GitDiscoveryError> {
        let root = layout.root_path();
        let repo = gix::open(root).map_err(|source| {
            GitDiscoveryError::new(
                GitDiscoveryErrorKind::OpenRepository,
                root,
                "opening git repository",
                source,
            )
        })?;
        let index = repo.index_or_empty().map_err(|source| {
            GitDiscoveryError::new(
                GitDiscoveryErrorKind::ReadIndex,
                root,
                "opening git index",
                source,
            )
        })?;
        Ok(Self {
            root: root.to_path_buf(),
            repo,
            index,
        })
    }

    /// Whether `path` is present in the plain-Git index.
    pub fn is_tracked(&self, path: &GatPath) -> bool {
        self.index
            .entry_by_path(gix::bstr::BStr::new(path.as_str().as_bytes()))
            .is_some()
    }

    /// Discovers regular files under a literal root-relative directory.
    pub fn discover_files(
        &self,
        dir: Option<&GatPath>,
        force: bool,
    ) -> Result<Vec<GatPath>, GitDiscoveryError> {
        self.discover_files_observing(dir, force, &mut |_, _, _| true)
    }

    pub fn discover_files_observing(
        &self,
        dir: Option<&GatPath>,
        force: bool,
        observe: &mut dyn FnMut(&str, AddExclusionReason, bool) -> bool,
    ) -> Result<Vec<GatPath>, GitDiscoveryError> {
        let mut collect = PruneInfraDelegate {
            paths: Vec::new(),
            force,
            observe,
        };
        let mut options = self.repo.dirwalk_options().map_err(|source| {
            GitDiscoveryError::new(
                GitDiscoveryErrorKind::ConfigureWalk,
                &self.root,
                "configuring git directory walk",
                source,
            )
        })?;
        options = options
            .emit_ignored(Some(gix::dir::walk::EmissionMode::Matching))
            .emit_tracked(true);
        let patterns: Vec<gix::bstr::BString> = dir
            .map(|dir| gix::bstr::BString::from(format!(":(literal,top){dir}")))
            .into_iter()
            .collect();
        self.repo
            .dirwalk(
                &self.index,
                patterns,
                &Default::default(),
                options,
                &mut collect,
            )
            .map_err(|source| {
                GitDiscoveryError::new(
                    GitDiscoveryErrorKind::Walk,
                    &self.root,
                    format!("walking directory {}", dir.map_or("", GatPath::as_str)),
                    source,
                )
            })?;

        collect.paths.sort_unstable();
        collect
            .paths
            .into_iter()
            .map(|path| {
                let rel = String::from_utf8(path.into()).map_err(|error| {
                    GitDiscoveryError::new(
                        GitDiscoveryErrorKind::InvalidPath,
                        &self.root,
                        "decoding discovered git path",
                        LexicalPathError::NonUtf8 {
                            display: String::from_utf8_lossy(error.as_bytes()).into_owned(),
                        },
                    )
                })?;
                GatPath::from_canonical_string(rel).map_err(|source| {
                    GitDiscoveryError::new(
                        GitDiscoveryErrorKind::InvalidPath,
                        &self.root,
                        "validating discovered git path",
                        source,
                    )
                })
            })
            .collect()
    }

    /// Runs repeated index/exclude-stack lookups without reopening Git.
    pub fn with_path_status_lookup<T, E>(
        &self,
        f: impl FnOnce(&mut dyn FnMut(&str) -> Result<GitPathStatus, GitDiscoveryError>) -> Result<T, E>,
    ) -> Result<Result<T, E>, GitDiscoveryError> {
        let mut excludes = self
            .repo
            .excludes(
                &self.index,
                None,
                gix::worktree::stack::state::ignore::Source::default(),
            )
            .map_err(|source| {
                GitDiscoveryError::new(
                    GitDiscoveryErrorKind::ConfigureWalk,
                    &self.root,
                    "configuring git exclude stack",
                    source,
                )
            })?;
        let mut lookup = |rel: &str| -> Result<GitPathStatus, GitDiscoveryError> {
            let tracked = self
                .index
                .entry_by_path(gix::bstr::BStr::new(rel.as_bytes()))
                .is_some();
            let platform = excludes.at_path(rel, None).map_err(|source| {
                GitDiscoveryError::new(
                    GitDiscoveryErrorKind::ConfigureWalk,
                    &self.root,
                    format!("checking git excludes for {rel}"),
                    source,
                )
            })?;
            Ok(GitPathStatus {
                tracked,
                gitignored: platform.is_excluded(),
            })
        };
        Ok(f(&mut lookup))
    }
}

struct PruneInfraDelegate<'a> {
    paths: Vec<gix::bstr::BString>,
    force: bool,
    observe: &'a mut dyn FnMut(&str, AddExclusionReason, bool) -> bool,
}

impl gix::dir::walk::Delegate for PruneInfraDelegate<'_> {
    fn emit(
        &mut self,
        entry: gix::dir::EntryRef<'_>,
        _collapsed_directory_status: Option<gix::dir::entry::Status>,
    ) -> gix::dir::walk::Action {
        let rel = entry.rela_path.to_str_lossy();
        let reason = if crate::worktree::is_infrastructure_path(&rel) {
            Some(AddExclusionReason::Infrastructure)
        } else if matches!(entry.status, gix::dir::entry::Status::Tracked) {
            Some(AddExclusionReason::GitTracked)
        } else if !self.force && matches!(entry.status, gix::dir::entry::Status::Ignored(_)) {
            Some(AddExclusionReason::GitIgnore)
        } else {
            None
        };
        if let Some(reason) = reason {
            if !(self.observe)(
                &rel,
                reason,
                matches!(
                    entry.disk_kind,
                    Some(gix::dir::entry::Kind::Directory | gix::dir::entry::Kind::Repository)
                ),
            ) {
                return gix::dir::walk::Action::Break(());
            }
            return gix::dir::walk::Action::Continue(());
        }
        if entry.disk_kind == Some(gix::dir::entry::Kind::File) {
            self.paths.push(entry.rela_path.as_ref().to_owned());
        }
        gix::dir::walk::Action::Continue(())
    }

    fn can_recurse(
        &mut self,
        entry: gix::dir::EntryRef<'_>,
        for_deletion: Option<gix::dir::walk::ForDeletionMode>,
        worktree_root_is_repository: bool,
    ) -> bool {
        let rel = entry.rela_path.to_str_lossy();
        if crate::worktree::is_infrastructure_path(&rel) {
            return false;
        }
        if self.force
            && matches!(entry.status, gix::dir::entry::Status::Ignored(_))
            && is_recursable_dir(entry.disk_kind, worktree_root_is_repository)
        {
            return true;
        }
        entry.status.can_recurse(
            entry.disk_kind,
            entry.pathspec_match,
            for_deletion,
            worktree_root_is_repository,
        )
    }
}

const fn is_recursable_dir(
    kind: Option<gix::dir::entry::Kind>,
    worktree_root_is_repository: bool,
) -> bool {
    match kind {
        Some(gix::dir::entry::Kind::Directory) => true,
        Some(gix::dir::entry::Kind::Repository) => worktree_root_is_repository,
        _ => false,
    }
}

/// Failure to load a repository's optional `.gatignore`.
#[derive(Debug, thiserror::Error)]
#[error("could not read {}", path.display())]
pub struct GatIgnoreError {
    path: PathBuf,
    #[source]
    source: std::io::Error,
}

impl GatIgnoreError {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn into_source(self) -> std::io::Error {
        self.source
    }
}

/// A parsed root `.gatignore`, or an empty matcher when the file is absent.
pub struct GatIgnore(Option<GitIgnoreMatcher>);

impl GatIgnore {
    pub fn load(layout: &RepositoryLayout) -> Result<Self, GatIgnoreError> {
        let path = layout.root_path().join(".gatignore");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self(None));
            }
            Err(source) => return Err(GatIgnoreError { path, source }),
        };
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        Ok(Self(
            (!lines.is_empty()).then(|| GitIgnoreMatcher::new(lines)),
        ))
    }

    #[must_use]
    pub fn is_ignored(&self, rel: &str) -> bool {
        self.0
            .as_ref()
            .is_some_and(|matcher| matcher.is_ignored(rel))
    }
}

/// An opaque Git-ignore pattern matcher backed by Gix semantics.
pub struct GitIgnoreMatcher(gix::ignore::Search);

impl GitIgnoreMatcher {
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Self {
        Self(gix::ignore::Search::from_overrides(
            patterns,
            gix::ignore::search::Ignore::default(),
        ))
    }

    #[must_use]
    pub fn is_ignored(&self, rel: &str) -> bool {
        use gix::glob::pattern::Case;

        let bytes = rel.as_bytes();
        let mut ignored = false;
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'/'
                && let Some(matched) = self.0.pattern_matching_relative_path(
                    gix::bstr::BStr::new(&bytes[..index]),
                    Some(true),
                    Case::Sensitive,
                )
            {
                ignored = !matched.pattern.is_negative();
            }
        }
        if let Some(matched) = self.0.pattern_matching_relative_path(
            gix::bstr::BStr::new(bytes),
            Some(false),
            Case::Sensitive,
        ) {
            ignored = !matched.pattern.is_negative();
        }
        ignored
    }
}
