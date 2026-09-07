//! Repository-bound Git history services.
//!
//! Selection and result values remain semantic, while repository opening,
//! revision resolution, traversal, and object reads stay in `gat-io`.

use std::path::Path;

use gat_core::git::{GitCommitId, GitRevisionSpec, GitTimestamp};
use gat_core::history::HistorySelection;
use gat_core::lock::Entry;

use crate::repository::Repository;

/// Semantic stage at which history traversal failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryErrorKind {
    /// The repository could not be opened.
    OpenRepository,
    /// A date expression was invalid.
    InvalidDate,
    /// A selected revision could not be resolved.
    RevisionResolution,
    /// Commit/ref traversal failed.
    Traversal,
    /// A historical lock snapshot was invalid.
    InvalidLockSnapshot,
    /// A selected commit used an unsupported hash kind.
    UnsupportedHashKind,
}

/// Engine-owned history failure retaining the I/O error as its source.
/// Storage-layer conversion stays inside the engine's history operations.
///
/// ```compile_fail
/// fn forward(value: gat_io::GitHistoryError) -> gat_engine::HistoryError {
///     value.into()
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[error("history access failed")]
pub struct HistoryError(#[source] gat_io::GitHistoryError);

impl HistoryError {
    /// Returns the semantic failure stage.
    #[must_use]
    pub const fn kind(&self) -> HistoryErrorKind {
        match self.0.kind() {
            gat_io::GitHistoryErrorKind::OpenRepository => HistoryErrorKind::OpenRepository,
            gat_io::GitHistoryErrorKind::InvalidDate => HistoryErrorKind::InvalidDate,
            gat_io::GitHistoryErrorKind::RevisionResolution => HistoryErrorKind::RevisionResolution,
            gat_io::GitHistoryErrorKind::Traversal => HistoryErrorKind::Traversal,
            gat_io::GitHistoryErrorKind::InvalidLockSnapshot => {
                HistoryErrorKind::InvalidLockSnapshot
            }
            gat_io::GitHistoryErrorKind::UnsupportedHashKind => {
                HistoryErrorKind::UnsupportedHashKind
            }
        }
    }

    /// Returns the repository root for open failures.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.0.root()
    }

    /// Returns the revision, date, operation, or snapshot label.
    #[must_use]
    pub fn subject(&self) -> &str {
        self.0.subject()
    }

    /// Returns safe structural detail for invalid lock snapshots.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.0.detail()
    }
}

/// Parses a CLI date through Git's date grammar into a semantic timestamp.
pub fn parse_cli_date(input: &str) -> Result<GitTimestamp, HistoryError> {
    gat_io::parse_cli_date(input).map_err(HistoryError)
}

/// Semantic stage at which strict commit resolution failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveCommitErrorKind {
    /// The repository could not be opened.
    OpenRepository,
    /// The revision did not resolve to a commit.
    ResolveRevision,
    /// The resolved commit used an unsupported hash kind.
    UnsupportedHashKind,
}

/// Engine-owned strict revision-resolution failure.
/// Storage-layer conversion stays inside the engine's history operations.
///
/// ```compile_fail
/// fn forward(value: gat_io::ResolveCommitError) -> gat_engine::ResolveCommitError {
///     value.into()
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ResolveCommitError(gat_io::ResolveCommitError);

impl ResolveCommitError {
    /// Returns the semantic failure stage.
    #[must_use]
    pub const fn kind(&self) -> ResolveCommitErrorKind {
        match self.0.kind() {
            gat_io::ResolveCommitErrorKind::OpenRepository => {
                ResolveCommitErrorKind::OpenRepository
            }
            gat_io::ResolveCommitErrorKind::ResolveRevision => {
                ResolveCommitErrorKind::ResolveRevision
            }
            gat_io::ResolveCommitErrorKind::UnsupportedHashKind => {
                ResolveCommitErrorKind::UnsupportedHashKind
            }
        }
    }

    /// Returns the unresolved revision expression.
    #[must_use]
    pub fn revision(&self) -> &str {
        self.0.revision()
    }
}

/// Outcome of visiting a resolved history selection.
/// Storage-layer conversion stays inside the engine's history operations.
///
/// ```compile_fail
/// fn forward(value: gat_io::HistoryStats) -> gat_engine::HistoryVisitStats {
///     value.into()
/// }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryVisitStats {
    /// Number of distinct commits visited after root deduplication.
    pub visited: usize,
    /// Whether the repository's available history is shallow.
    pub shallow: bool,
}

impl HistoryVisitStats {
    const fn from_io(stats: gat_io::HistoryStats) -> Self {
        Self {
            visited: stats.visited,
            shallow: stats.shallow,
        }
    }
}

impl Repository {
    /// Resolves a revision strictly to a commit in this repository.
    pub fn resolve_commit(
        &self,
        revision: &GitRevisionSpec,
    ) -> Result<GitCommitId, ResolveCommitError> {
        gat_io::resolve_commit(self.layout(), revision).map_err(ResolveCommitError)
    }

    /// Streams each selected commit exactly once in deterministic ID order.
    pub fn visit_history_commits<E>(
        &self,
        selection: &HistorySelection,
        visit: impl FnMut(GitCommitId) -> Result<(), E>,
    ) -> Result<HistoryVisitStats, E>
    where
        E: From<HistoryError>,
    {
        enum Bridge<E> {
            History(HistoryError),
            Callback(E),
        }

        impl<E> From<gat_io::GitHistoryError> for Bridge<E> {
            fn from(error: gat_io::GitHistoryError) -> Self {
                Self::History(HistoryError(error))
            }
        }

        let reader = match gat_io::GitReader::open(self.layout()) {
            Ok(reader) => reader,
            Err(error) => {
                return Err(E::from(HistoryError(gat_io::GitHistoryError::from(error))));
            }
        };
        let mut visit = visit;
        reader
            .visit_history_commits::<Bridge<E>>(selection, |commit| {
                visit(commit).map_err(Bridge::Callback)
            })
            .map(HistoryVisitStats::from_io)
            .map_err(|error| match error {
                Bridge::History(error) => E::from(error),
                Bridge::Callback(error) => error,
            })
    }

    /// Streams selected historical lock rows after collecting and deduplicating
    /// the selected commit IDs in deterministic order.
    pub fn visit_history_lock_entries<E>(
        &self,
        selection: &HistorySelection,
        keep: impl Fn(&str) -> bool,
        visit: impl FnMut(&Entry) -> Result<(), E>,
    ) -> Result<HistoryVisitStats, E>
    where
        E: From<HistoryError> + From<gat_core::lock::LockError>,
    {
        enum Bridge<E> {
            History(HistoryError),
            Lock(gat_core::lock::LockError),
            Callback(E),
        }

        impl<E> From<gat_io::GitHistoryError> for Bridge<E> {
            fn from(error: gat_io::GitHistoryError) -> Self {
                Self::History(HistoryError(error))
            }
        }

        impl<E> From<gat_core::lock::LockError> for Bridge<E> {
            fn from(error: gat_core::lock::LockError) -> Self {
                Self::Lock(error)
            }
        }

        let reader = match gat_io::GitReader::open(self.layout()) {
            Ok(reader) => reader,
            Err(error) => {
                return Err(E::from(HistoryError(gat_io::GitHistoryError::from(error))));
            }
        };
        let mut visit = visit;
        reader
            .visit_history_lock_entries::<Bridge<E>>(selection, keep, |entry| {
                visit(entry).map_err(Bridge::Callback)
            })
            .map(HistoryVisitStats::from_io)
            .map_err(|error| match error {
                Bridge::History(error) => E::from(error),
                Bridge::Lock(error) => E::from(error),
                Bridge::Callback(error) => error,
            })
    }
}
