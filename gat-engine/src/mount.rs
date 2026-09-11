//! Repository-bound mount recovery and state-transition workflows.
//!
//! Commands retain CLI conversion, scope/ownership/route policy, outcome
//! construction, and presentation. This module owns the repository mutation
//! lock and the complete journal -> delete -> config -> replay protocol.
//! Journal codecs and staged files remain in `gat-io`; physical desired-state
//! shape and `SQLite` coordination remain behind its opaque mount session.

use crate::repository::{Repository, RepositoryError};
use gat_core::config::{Config, ConfigScope};
use gat_core::git::{GitCommitId, GitRevisionSpec};
use gat_core::git_location::GitLocationSpec;
use gat_core::lexical_path::GatPath;
use gat_core::name::MountName;
use gat_core::progress::{
    ProgressActivity, ProgressHandle, ProgressOperation, ProgressReporter, ProgressSpec,
    with_progress_typed,
};
use gat_core::selection::Selection;
use gat_io::{AtomicError, RepoLock};
use gat_io::{DesiredPublicationError, MountMutationSession, MountReplayResult, StateStoreError};
use gat_io::{MountJournal, MountJournalError, MountTxnChange, MountTxnPhase, MountTxnRecord};

type BoxedSource = Box<dyn std::error::Error + Send + Sync + 'static>;

const MOUNT_DELETE_WINDOW: usize = 4096;
const MOUNT_IMPORT_WINDOW: std::num::NonZeroUsize = std::num::NonZeroUsize::new(4096).unwrap();

/// A parsed mount source location whose physical Git representation remains
/// below the command boundary.
pub struct MountSourceLocation {
    spec: GitLocationSpec,
    location: gat_io::GitLocation,
}

#[derive(Debug, thiserror::Error)]
#[error("could not prepare the mount source repository")]
pub struct MountSourceError {
    kind: MountSourceErrorKind,
    #[source]
    source: BoxedSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountSourceErrorKind {
    Cancelled,
    InvalidLocation,
    MissingRepositoryName,
    LocalSourceMissing,
    ResolvePath,
    CreateTemporary,
    ClonePrepare,
    CloneFetch,
    CloneCheckout,
    OpenRepository,
    ResolveRevision,
    UnsupportedHashKind,
    LoadConfig,
    InferredTarget,
}

impl MountSourceError {
    #[must_use]
    pub const fn kind(&self) -> MountSourceErrorKind {
        self.kind
    }

    fn new(
        kind: MountSourceErrorKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            source: Box::new(source),
        }
    }

    fn location(source: gat_io::GitLocationError) -> Self {
        let kind = match source {
            gat_io::GitLocationError::Invalid => MountSourceErrorKind::InvalidLocation,
            gat_io::GitLocationError::NoRepositoryName => {
                MountSourceErrorKind::MissingRepositoryName
            }
        };
        Self::new(kind, source)
    }

    fn prepare(source: gat_io::PrepareGitWorktreeError) -> Self {
        let kind = match &source {
            gat_io::PrepareGitWorktreeError::LocalSourceMissing { .. } => {
                MountSourceErrorKind::LocalSourceMissing
            }
            gat_io::PrepareGitWorktreeError::ResolveLocal { .. }
            | gat_io::PrepareGitWorktreeError::ResolveClone { .. } => {
                MountSourceErrorKind::ResolvePath
            }
            gat_io::PrepareGitWorktreeError::CreateTemporary(_) => {
                MountSourceErrorKind::CreateTemporary
            }
            gat_io::PrepareGitWorktreeError::Clone(error) => match error.kind() {
                gat_io::GitCloneErrorKind::Cancelled => MountSourceErrorKind::Cancelled,
                gat_io::GitCloneErrorKind::PrepareDestination
                | gat_io::GitCloneErrorKind::PrepareClone => MountSourceErrorKind::ClonePrepare,
                gat_io::GitCloneErrorKind::Fetch => MountSourceErrorKind::CloneFetch,
                gat_io::GitCloneErrorKind::Checkout => MountSourceErrorKind::CloneCheckout,
            },
        };
        Self::new(kind, source)
    }

    fn resolve(source: gat_io::ResolveCommitError) -> Self {
        let kind = match source.kind() {
            gat_io::ResolveCommitErrorKind::OpenRepository => MountSourceErrorKind::OpenRepository,
            gat_io::ResolveCommitErrorKind::ResolveRevision => {
                MountSourceErrorKind::ResolveRevision
            }
            gat_io::ResolveCommitErrorKind::UnsupportedHashKind => {
                MountSourceErrorKind::UnsupportedHashKind
            }
        };
        Self::new(kind, source)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn test_error(
        kind: MountSourceErrorKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::new(kind, source)
    }
}

impl MountSourceLocation {
    pub fn parse(spec: GitLocationSpec) -> Result<Self, MountSourceError> {
        let location = gat_io::parse_location(&spec).map_err(MountSourceError::location)?;
        Ok(Self { spec, location })
    }

    pub fn inferred_target(&self) -> Result<GatPath, MountSourceError> {
        let name = self
            .location
            .repository_name()
            .map_err(MountSourceError::location)?;
        GatPath::normalize(name)
            .map_err(|source| MountSourceError::new(MountSourceErrorKind::InferredTarget, source))
    }

    #[must_use]
    pub const fn spec(&self) -> &GitLocationSpec {
        &self.spec
    }

    pub fn prepare(
        self,
        revision: Option<&GitRevisionSpec>,
        progress: &dyn ProgressReporter,
        cancellation: &crate::TransferCancellation,
    ) -> Result<PreparedMountSource, MountSourceError> {
        let worktree = match self.location.kind() {
            gat_io::GitLocationKind::LocalPath => {
                gat_io::prepare_worktree(&self.location, &self.spec, cancellation.git_interrupt())
                    .map_err(MountSourceError::prepare)?
            }
            gat_io::GitLocationKind::Clone => with_progress_typed(
                progress,
                ProgressSpec::indeterminate(ProgressOperation::CloningSource),
                |_| {
                    gat_io::prepare_worktree(
                        &self.location,
                        &self.spec,
                        cancellation.git_interrupt(),
                    )
                },
            )
            .map_err(MountSourceError::prepare)?,
        };
        let default_revision;
        let revision = if let Some(revision) = revision {
            revision
        } else {
            default_revision = GitRevisionSpec::from("HEAD");
            &default_revision
        };
        let (rev_lock, config) = with_progress_typed(
            progress,
            ProgressSpec::indeterminate(ProgressOperation::LoadingState),
            |_| {
                Ok::<_, MountSourceError>((
                    worktree
                        .resolve_commit(revision)
                        .map_err(MountSourceError::resolve)?,
                    worktree.load_project_config().map_err(|source| {
                        MountSourceError::new(MountSourceErrorKind::LoadConfig, source)
                    })?,
                ))
            },
        )?;
        Ok(PreparedMountSource {
            rev_lock,
            config,
            worktree,
        })
    }
}

/// A prepared upstream repository kept alive for bounded row staging.
pub struct PreparedMountSource {
    rev_lock: GitCommitId,
    config: Config,
    worktree: gat_io::PreparedGitWorktree,
}

impl PreparedMountSource {
    #[must_use]
    pub const fn rev_lock(&self) -> GitCommitId {
        self.rev_lock
    }

    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub const fn rows(&self, selection: Selection) -> MountRowSource<'_> {
        MountRowSource {
            source: MountRowSourceKind::Prepared(&self.worktree),
            selection,
        }
    }
}

/// Engine-owned semantic stages for mount recovery and mutation.
#[derive(Debug, thiserror::Error)]
pub enum MountWorkflowError {
    #[error("could not acquire mount mutation authority")]
    Acquire {
        #[source]
        source: BoxedSource,
    },
    #[error("could not read the pending mount transaction journal")]
    ReadJournal {
        #[source]
        source: BoxedSource,
    },
    #[error("could not open mount repository state")]
    OpenState {
        #[source]
        source: BoxedSource,
    },
    #[error("could not read mount repository state")]
    ReadState {
        #[source]
        source: BoxedSource,
    },
    #[error("could not stage the mount source snapshot: {source}")]
    StageRows {
        #[source]
        source: BoxedSource,
    },
    #[error("could not publish the mount transaction journal")]
    WriteJournal {
        #[source]
        source: BoxedSource,
    },
    #[error("could not delete mount-owned desired rows")]
    DeleteRows {
        #[source]
        source: BoxedSource,
    },
    #[error("could not publish mount configuration")]
    PublishConfig {
        #[source]
        source: BoxedSource,
    },
    #[error("could not replay the staged mount snapshot")]
    ReplayRows {
        #[source]
        source: BoxedSource,
    },
    #[error("could not regenerate excludes after the mount mutation")]
    SyncExcludes {
        #[source]
        source: BoxedSource,
    },
    #[error("could not clean up the completed mount transaction")]
    Cleanup {
        #[source]
        source: BoxedSource,
    },
    #[cfg(any(test, feature = "test-support"))]
    #[error(transparent)]
    Fault(#[from] gat_core::fault::InjectedFault),
}

impl MountWorkflowError {
    fn acquire(source: AtomicError) -> Self {
        Self::Acquire {
            source: Box::new(source),
        }
    }

    fn read_journal(source: MountJournalError) -> Self {
        Self::ReadJournal {
            source: Box::new(source),
        }
    }

    fn open_state(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::OpenState {
            source: Box::new(source),
        }
    }

    fn read_state(source: StateStoreError) -> Self {
        Self::ReadState {
            source: Box::new(source),
        }
    }

    fn stage_rows(source: MountJournalError) -> Self {
        Self::StageRows {
            source: Box::new(source),
        }
    }

    fn write_journal(source: MountJournalError) -> Self {
        Self::WriteJournal {
            source: Box::new(source),
        }
    }

    fn delete_rows(source: DesiredPublicationError) -> Self {
        Self::DeleteRows {
            source: Box::new(source),
        }
    }

    fn publish_config(source: RepositoryError) -> Self {
        Self::PublishConfig {
            source: Box::new(source),
        }
    }

    fn replay_rows(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::ReplayRows {
            source: Box::new(source),
        }
    }

    fn sync_excludes(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::SyncExcludes {
            source: Box::new(source),
        }
    }

    fn cleanup(source: MountJournalError) -> Self {
        Self::Cleanup {
            source: Box::new(source),
        }
    }
}

/// An opaque source-lock reader selected by command policy.
///
/// The source repository's physical root remains private to engine/I/O.
pub struct MountRowSource<'source> {
    source: MountRowSourceKind<'source>,
    selection: Selection,
}

enum MountRowSourceKind<'source> {
    Prepared(&'source gat_io::PreparedGitWorktree),
}

impl MountRowSource<'_> {
    fn stage_selected(
        &self,
        journal: &MountJournal,
        target: &GatPath,
    ) -> Result<usize, MountJournalError> {
        match self.source {
            MountRowSourceKind::Prepared(worktree) => {
                journal.stage_selected(worktree, &self.selection, target, MOUNT_IMPORT_WINDOW)
            }
        }
    }
}

/// A complete add transition. Its shape makes an old target unrepresentable.
pub(crate) struct MountAdd<'source> {
    scope: ConfigScope,
    name: MountName,
    target: GatPath,
    pre_config: Config,
    post_config: Config,
    post_effective_config: Config,
    source: MountRowSource<'source>,
}

impl<'source> MountAdd<'source> {
    #[must_use]
    pub const fn new(
        scope: ConfigScope,
        name: MountName,
        target: GatPath,
        pre_config: Config,
        post_config: Config,
        post_effective_config: Config,
        source: MountRowSource<'source>,
    ) -> Self {
        Self {
            scope,
            name,
            target,
            pre_config,
            post_config,
            post_effective_config,
            source,
        }
    }
}

/// A complete update transition. Both old and new targets are mandatory.
pub(crate) struct MountUpdate<'source> {
    scope: ConfigScope,
    name: MountName,
    old_target: GatPath,
    new_target: GatPath,
    pre_config: Config,
    post_config: Config,
    post_effective_config: Config,
    source: MountRowSource<'source>,
}

impl<'source> MountUpdate<'source> {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        scope: ConfigScope,
        name: MountName,
        old_target: GatPath,
        new_target: GatPath,
        pre_config: Config,
        post_config: Config,
        post_effective_config: Config,
        source: MountRowSource<'source>,
    ) -> Self {
        Self {
            scope,
            name,
            old_target,
            new_target,
            pre_config,
            post_config,
            post_effective_config,
            source,
        }
    }
}

/// A destructive remove transition. A new target/source cannot be supplied.
pub(crate) struct MountRemove {
    scope: ConfigScope,
    name: MountName,
    target: GatPath,
    pre_config: Config,
    post_config: Config,
    post_effective_config: Config,
}

impl MountRemove {
    #[must_use]
    pub const fn new(
        scope: ConfigScope,
        name: MountName,
        target: GatPath,
        pre_config: Config,
        post_config: Config,
        post_effective_config: Config,
    ) -> Self {
        Self {
            scope,
            name,
            target,
            pre_config,
            post_config,
            post_effective_config,
        }
    }
}

/// Semantic counts from one completed mount transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MountMutationOutcome {
    pub removed: usize,
    pub imported: usize,
}

/// Repository-bound entry point for mount recovery and mutation.
pub struct MountService<'repo> {
    repo: &'repo Repository,
}

impl<'repo> MountService<'repo> {
    pub(crate) const fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Recover an abandoned mount transaction under repository mutation
    /// authority.
    pub fn recover_pending(
        &self,
        progress: &dyn ProgressReporter,
    ) -> Result<(), MountWorkflowError> {
        let guard = self
            .repo
            .acquire_configuration_lock()
            .map_err(MountWorkflowError::acquire)?;
        recover_pending_mount_transaction_locked(self.repo, &guard, progress).map(|_| ())
    }

    pub(crate) fn recover_pending_locked(
        &self,
        guard: &RepoLock,
        progress: &dyn ProgressReporter,
    ) -> Result<(), MountWorkflowError> {
        recover_pending_mount_transaction_locked(self.repo, guard, progress).map(|_| ())
    }

    /// Run command-owned mount policy under engine-owned mutation authority.
    ///
    /// `E: From<MountWorkflowError>` keeps command errors typed without
    /// introducing a reverse dependency from engine to command.
    #[cfg(test)]
    pub(crate) fn with_locked<T, E>(
        &self,
        progress: &dyn ProgressReporter,
        operation: impl FnOnce(&mut LockedMount<'repo, '_>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<MountWorkflowError>,
    {
        let guard = self
            .repo
            .acquire_configuration_lock()
            .map_err(MountWorkflowError::acquire)
            .map_err(E::from)?;
        drop(
            recover_pending_mount_transaction_locked(self.repo, &guard, progress)
                .map_err(E::from)?,
        );
        let layers = self
            .repo
            .load_config_layers()
            .map_err(MountWorkflowError::publish_config)
            .map_err(E::from)?;
        let levels = layers.unvalidated_effective().lock.shard_levels();
        let mut locked = LockedMount {
            repo: self.repo,
            progress,
            state: None,
            levels,
            layers,
        };
        operation(&mut locked)
    }

    /// Run mount policy with one post-recovery configuration snapshot.
    ///
    /// The effective lock shape is retained by the locked service, avoiding
    /// a second config read when desired state is opened lazily.
    pub(crate) fn with_locked_config<T, E>(
        &self,
        progress: &dyn ProgressReporter,
        operation: impl FnOnce(
            &mut LockedMount<'repo, '_>,
            crate::repository::ConfigLayers,
            Config,
        ) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<MountWorkflowError> + From<RepositoryError>,
    {
        let guard = self
            .repo
            .acquire_configuration_lock()
            .map_err(MountWorkflowError::acquire)
            .map_err(E::from)?;
        // Recovery finishes using its recorded layout. The new operation must
        // adopt current settings rather than reuse that publication session.
        drop(
            recover_pending_mount_transaction_locked(self.repo, &guard, progress)
                .map_err(E::from)?,
        );
        let layers = self.repo.load_config_layers().map_err(E::from)?;
        let effective = layers.unvalidated_effective();
        let mut locked = LockedMount {
            repo: self.repo,
            progress,
            state: None,
            levels: effective.lock.shard_levels(),
            layers: layers.clone(),
        };
        // Cancellation is safe before a new journaled mutation. Once it starts,
        // finish journal, config, row replay, and excludes as one recovery unit.
        self.repo.check_cancelled().map_err(E::from)?;
        operation(&mut locked, layers, effective)
    }
}

/// Semantic view and authoritative transition executor available only while
/// [`MountService`] retains repository mutation authority.
pub(crate) struct LockedMount<'repo, 'progress> {
    repo: &'repo Repository,
    progress: &'progress dyn ProgressReporter,
    state: Option<MountMutationSession<'repo>>,
    levels: gat_core::lock::LockShardLevels,
    layers: crate::ConfigLayers,
}

impl<'repo> LockedMount<'repo, '_> {
    fn state(&mut self) -> Result<&mut MountMutationSession<'repo>, MountWorkflowError> {
        if self.state.is_none() {
            let levels = self.levels;
            let state = with_progress_typed(
                self.progress,
                ProgressSpec::indeterminate(ProgressOperation::LoadingState),
                |_| {
                    MountMutationSession::acquire(self.repo.layout(), levels)
                        .map_err(MountWorkflowError::open_state)
                },
            )?;
            self.state = Some(state);
        }
        Ok(self.state.as_mut().expect("mount state initialized"))
    }

    pub fn desired_count(&mut self, target: &GatPath) -> Result<u64, MountWorkflowError> {
        self.state()?
            .desired_count_subtree(target)
            .map_err(MountWorkflowError::read_state)
    }

    pub fn desired_any(&mut self, target: &GatPath) -> Result<bool, MountWorkflowError> {
        self.state()?
            .desired_any_subtree(target)
            .map_err(MountWorkflowError::read_state)
    }

    pub fn has_root_owned_assets(
        &mut self,
        target: &GatPath,
        exclude_prefix: Option<&GatPath>,
    ) -> Result<bool, MountWorkflowError> {
        self.state()?
            .has_desired_outside(target, exclude_prefix)
            .map_err(MountWorkflowError::read_state)
    }

    pub fn add(
        &mut self,
        transition: MountAdd<'_>,
    ) -> Result<MountMutationOutcome, MountWorkflowError> {
        self.state()?;
        let MountAdd {
            scope,
            name,
            target,
            pre_config,
            post_config,
            post_effective_config,
            source,
        } = transition;
        let journal = mount_journal(self.repo);
        let imported = with_progress_typed(
            self.progress,
            ProgressSpec::indeterminate(ProgressOperation::ApplyingChanges),
            |task| -> Result<usize, MountWorkflowError> {
                let handle = task.handle();
                handle.set_activity(ProgressActivity::StagingRows);
                let row_windows = source
                    .stage_selected(&journal, &target)
                    .map_err(MountWorkflowError::stage_rows)?;
                mount_fault("add.staged")?;
                let mut record = MountTxnRecord {
                    change: MountTxnChange::Add {
                        target,
                        row_windows,
                    },
                    scope,
                    name,
                    post_config,
                    pre_config,
                    shard_levels: self.levels,
                    phase: MountTxnPhase::Publish,
                };
                self.repo
                    .validate_config_candidate(&self.layers, &record.post_config, record.scope)
                    .map_err(MountWorkflowError::publish_config)?;
                journal
                    .write(&record)
                    .map_err(MountWorkflowError::write_journal)?;
                mount_fault("add.journaled")?;
                handle.set_activity(ProgressActivity::PublishingConfig);
                self.repo
                    .save_config_scoped(&record.post_config, record.scope)
                    .map_err(MountWorkflowError::publish_config)?;
                mount_fault("add.config_published")?;
                let mut replay = MountReplayResult::default();
                let replay_result =
                    replay_record(self.state()?, &journal, &record, &handle, &mut replay);
                if let Err(error) = replay_result {
                    if !replay.publication_may_have_landed
                        && self
                            .repo
                            .save_config_scoped(&record.pre_config, record.scope)
                            .is_ok()
                    {
                        let _ = clear_mount_transaction(self.repo);
                    }
                    return Err(error);
                }
                mark_published(&journal, &mut record)?;
                handle.set_activity(ProgressActivity::RegeneratingExcludes);
                sync_mount_excludes(self.repo, self.state()?, &post_effective_config)?;
                Ok(replay.imported)
            },
        )?;
        clear_mount_transaction(self.repo)?;
        Ok(MountMutationOutcome {
            removed: 0,
            imported,
        })
    }

    #[allow(
        clippy::missing_panics_doc,
        reason = "The transition is validated before its old target is accessed"
    )]
    pub fn update(
        &mut self,
        transition: MountUpdate<'_>,
    ) -> Result<MountMutationOutcome, MountWorkflowError> {
        self.state()?;
        let MountUpdate {
            scope,
            name,
            old_target,
            new_target,
            pre_config,
            post_config,
            post_effective_config,
            source,
        } = transition;
        let journal = mount_journal(self.repo);
        let outcome = with_progress_typed(
            self.progress,
            ProgressSpec::indeterminate(ProgressOperation::ApplyingChanges),
            |task| -> Result<MountMutationOutcome, MountWorkflowError> {
                let handle = task.handle();
                handle.set_activity(ProgressActivity::StagingRows);
                let row_windows = source
                    .stage_selected(&journal, &new_target)
                    .map_err(MountWorkflowError::stage_rows)?;
                mount_fault("update.staged")?;
                let mut record = MountTxnRecord {
                    change: MountTxnChange::Update {
                        old_target,
                        new_target,
                        row_windows,
                    },
                    scope,
                    name,
                    post_config,
                    pre_config,
                    shard_levels: self.levels,
                    phase: MountTxnPhase::Publish,
                };
                self.repo
                    .validate_config_candidate(&self.layers, &record.post_config, record.scope)
                    .map_err(MountWorkflowError::publish_config)?;
                journal
                    .write(&record)
                    .map_err(MountWorkflowError::write_journal)?;
                mount_fault("update.journaled")?;
                handle.set_activity(ProgressActivity::DeletingOwnedRows);
                let removed = delete_record_rows(self.state()?, &record.change)?;
                mount_fault("update.deleted")?;
                handle.set_activity(ProgressActivity::PublishingConfig);
                self.repo
                    .save_config_scoped(&record.post_config, record.scope)
                    .map_err(MountWorkflowError::publish_config)?;
                mount_fault("update.config_published")?;
                let mut replay = MountReplayResult::default();
                replay_record(self.state()?, &journal, &record, &handle, &mut replay)?;
                mark_published(&journal, &mut record)?;
                handle.set_activity(ProgressActivity::RegeneratingExcludes);
                sync_mount_excludes(self.repo, self.state()?, &post_effective_config)?;
                Ok(MountMutationOutcome {
                    removed,
                    imported: replay.imported,
                })
            },
        )?;
        clear_mount_transaction(self.repo)?;
        Ok(outcome)
    }

    #[allow(
        clippy::missing_panics_doc,
        reason = "The transition is validated before its old target is accessed"
    )]
    pub fn remove(
        &mut self,
        transition: MountRemove,
    ) -> Result<MountMutationOutcome, MountWorkflowError> {
        self.state()?;
        let MountRemove {
            scope,
            name,
            target,
            pre_config,
            post_config,
            post_effective_config,
        } = transition;
        let journal = mount_journal(self.repo);
        let removed = with_progress_typed(
            self.progress,
            ProgressSpec::indeterminate(ProgressOperation::ApplyingChanges),
            |task| -> Result<usize, MountWorkflowError> {
                journal
                    .reset_staged_rows()
                    .map_err(MountWorkflowError::stage_rows)?;
                let mut record = MountTxnRecord {
                    change: MountTxnChange::Remove { target },
                    scope,
                    name,
                    post_config,
                    pre_config,
                    shard_levels: self.levels,
                    phase: MountTxnPhase::Publish,
                };
                self.repo
                    .validate_config_candidate(&self.layers, &record.post_config, record.scope)
                    .map_err(MountWorkflowError::publish_config)?;
                journal
                    .write(&record)
                    .map_err(MountWorkflowError::write_journal)?;
                mount_fault("remove.journaled")?;
                task.handle()
                    .set_activity(ProgressActivity::DeletingOwnedRows);
                let removed = delete_record_rows(self.state()?, &record.change)?;
                mount_fault("remove.deleted")?;
                task.handle()
                    .set_activity(ProgressActivity::PublishingConfig);
                self.repo
                    .save_config_scoped(&record.post_config, record.scope)
                    .map_err(MountWorkflowError::publish_config)?;
                mark_published(&journal, &mut record)?;
                task.handle()
                    .set_activity(ProgressActivity::RegeneratingExcludes);
                sync_mount_excludes(self.repo, self.state()?, &post_effective_config)?;
                Ok(removed)
            },
        )?;
        clear_mount_transaction(self.repo)?;
        Ok(MountMutationOutcome {
            removed,
            imported: 0,
        })
    }

    /// A detach is exactly one atomic config transition and intentionally
    /// writes no journal or desired rows.
    pub fn detach(
        &self,
        scope: ConfigScope,
        post_config: &Config,
    ) -> Result<(), MountWorkflowError> {
        self.repo
            .validate_config_candidate(&self.layers, post_config, scope)
            .map_err(MountWorkflowError::publish_config)?;
        self.repo
            .save_config_scoped(post_config, scope)
            .map_err(MountWorkflowError::publish_config)
    }
}

#[derive(Debug, thiserror::Error)]
enum ReplayIoError {
    #[error(transparent)]
    State(#[from] StateStoreError),
    #[error(transparent)]
    Lock(#[from] gat_io::LockError),
    #[error(transparent)]
    Atomic(#[from] AtomicError),
    #[error(transparent)]
    Journal(#[from] MountJournalError),
    #[cfg(any(test, feature = "test-support"))]
    #[error(transparent)]
    Fault(#[from] gat_core::fault::InjectedFault),
}

fn replay_record(
    state: &mut MountMutationSession<'_>,
    journal: &MountJournal,
    record: &MountTxnRecord,
    handle: &ProgressHandle,
    result: &mut MountReplayResult,
) -> Result<(), MountWorkflowError> {
    if record.change.row_windows() == 0 {
        return Ok(());
    }
    handle.set_activity(ProgressActivity::ReplayingRows);
    let mut windows = journal.validated_staged_windows(record);
    state
        .replay_windows::<ReplayIoError>(
            || {
                let Some(rows) = windows.next() else {
                    return Ok(None);
                };
                replay_fault("replay.before_publish")?;
                Ok(Some(
                    rows?
                        .into_iter()
                        .map(|row| gat_core::lock::Entry {
                            path: row.path,
                            oid: row.oid,
                        })
                        .collect(),
                ))
            },
            result,
            || {
                replay_fault("replay.after_window")?;
                Ok(())
            },
            || {
                replay_fault("desired_publish.after_fs_before_commit")?;
                Ok(())
            },
        )
        .map_err(MountWorkflowError::replay_rows)
}

fn delete_record_rows(
    state: &mut MountMutationSession<'_>,
    change: &MountTxnChange,
) -> Result<usize, MountWorkflowError> {
    match change {
        MountTxnChange::Add { .. } => Ok(0),
        MountTxnChange::Update {
            old_target: target, ..
        }
        | MountTxnChange::Remove { target } => state
            .delete_subtree_windowed(target, MOUNT_DELETE_WINDOW)
            .map_err(MountWorkflowError::delete_rows),
    }
}

fn mark_published(
    journal: &MountJournal,
    record: &mut MountTxnRecord,
) -> Result<(), MountWorkflowError> {
    record.phase = MountTxnPhase::Regenerate;
    journal
        .write(record)
        .map_err(MountWorkflowError::write_journal)?;
    mount_fault("mount.published")
}

fn mount_journal(repo: &Repository) -> MountJournal {
    MountJournal::open(repo.layout())
}

fn sync_mount_excludes(
    repo: &Repository,
    state: &MountMutationSession<'_>,
    config: &Config,
) -> Result<(), MountWorkflowError> {
    crate::excludes::sync_from_mount_mutation(repo, state, false, config)
        .map_err(MountWorkflowError::sync_excludes)?;
    Ok(())
}

/// Removes the active journal before its now-disposable staged rows.
fn clear_mount_transaction(repo: &Repository) -> Result<(), MountWorkflowError> {
    let journal = mount_journal(repo);
    journal
        .remove_journal()
        .map_err(MountWorkflowError::cleanup)?;
    mount_fault("clear.journal_removed")?;
    journal
        .remove_staged_rows()
        .map_err(MountWorkflowError::cleanup)
}

#[cfg(any(test, feature = "test-support"))]
fn mount_fault(label: &str) -> Result<(), MountWorkflowError> {
    Ok(gat_core::fault::hit(label)?)
}

#[cfg(not(any(test, feature = "test-support")))]
#[inline]
#[allow(
    clippy::unnecessary_wraps,
    reason = "The no-op production hook matches the fallible fault-injection signature used in tests"
)]
const fn mount_fault(_label: &str) -> Result<(), MountWorkflowError> {
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn replay_fault(label: &str) -> Result<(), ReplayIoError> {
    Ok(gat_core::fault::hit(label)?)
}

#[cfg(not(any(test, feature = "test-support")))]
#[inline]
#[allow(
    clippy::unnecessary_wraps,
    reason = "The no-op production hook matches the fallible fault-injection signature used in tests"
)]
const fn replay_fault(_label: &str) -> Result<(), ReplayIoError> {
    Ok(())
}

pub(crate) fn recover_pending_mount_transaction_locked<'repo>(
    repo: &'repo Repository,
    _guard: &RepoLock,
    progress: &dyn ProgressReporter,
) -> Result<Option<MountMutationSession<'repo>>, MountWorkflowError> {
    let journal = mount_journal(repo);
    let Some(mut record) = journal.read().map_err(MountWorkflowError::read_journal)? else {
        return Ok(None);
    };
    let levels = record.shard_levels;
    let mut state = with_progress_typed(
        progress,
        ProgressSpec::indeterminate(ProgressOperation::LoadingState),
        |task| -> Result<MountMutationSession<'repo>, MountWorkflowError> {
            task.handle()
                .set_activity(ProgressActivity::ObservingLockState);
            let state = MountMutationSession::acquire(repo.layout(), levels)
                .map_err(MountWorkflowError::open_state)?;
            task.handle()
                .set_activity(ProgressActivity::RefreshingDesiredState);
            Ok(state)
        },
    )?;

    with_progress_typed(
        progress,
        ProgressSpec::indeterminate(ProgressOperation::ApplyingChanges),
        |task| {
            let handle = task.handle();
            handle.set_activity(ProgressActivity::RecoveringInterruptedMount);
            if record.phase == MountTxnPhase::Publish {
                handle.set_activity(ProgressActivity::DeletingOwnedRows);
                delete_record_rows(&mut state, &record.change)?;
                mount_fault("recover.after_delete")?;
                handle.set_activity(ProgressActivity::PublishingConfig);
                repo.save_config_scoped(&record.post_config, record.scope)
                    .map_err(MountWorkflowError::publish_config)?;
                mount_fault("recover.after_config")?;
                let mut replay = MountReplayResult::default();
                replay_record(&mut state, &journal, &record, &handle, &mut replay)?;
                mark_published(&journal, &mut record)?;
            }
            handle.set_activity(ProgressActivity::RegeneratingExcludes);
            let effective_config = repo
                .load_config()
                .map_err(MountWorkflowError::sync_excludes)?;
            sync_mount_excludes(repo, &state, &effective_config)?;
            Ok::<_, MountWorkflowError>(())
        },
    )?;

    clear_mount_transaction(repo)?;
    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::config::MountConfig;
    use gat_core::lexical_path::GatSubpath;
    use gat_core::lock::{Entry, Lock};
    use gat_core::oid::Oid;
    use gat_core::progress::NoopProgress;

    fn gp(path: &str) -> GatPath {
        GatPath::parse_canonical(path).unwrap()
    }

    fn mount_config(target: GatPath) -> MountConfig {
        MountConfig {
            // hygiene-ok: recovery test stores configuration only and never fetches this URL.
            url: "https://example.invalid/source.git".to_string().into(),
            target,
            path: GatSubpath::Root,
            rev: None,
            rev_lock: None,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    #[test]
    fn recovery_finishes_journaled_add_and_regenerates_excludes() {
        let source_dir = crate::test_harness::git_repo();
        let source = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(source_dir.path().to_path_buf());
        source
            .save_lock(&Lock {
                entries: vec![Entry {
                    path: gp("weights.bin"),
                    oid: Oid::from_hex(&"a".repeat(64)).unwrap(),
                }],
            })
            .unwrap();
        crate::test_harness::commit_all(source_dir.path(), "source lock");
        let prepared = MountSourceLocation::parse(GitLocationSpec::from_string(
            source_dir.path().display().to_string(),
        ))
        .unwrap()
        .prepare(None, &NoopProgress, &crate::TransferCancellation::default())
        .unwrap();

        let destination_dir = crate::test_harness::git_repo();
        let destination = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(destination_dir.path().to_path_buf());
        let name = MountName::from_string("models".to_string());
        let target = gp("models");
        let pre_config = Config::default();
        let mut post_config = pre_config.clone();
        post_config
            .mounts
            .by_name
            .insert(name.clone(), mount_config(target.clone()));

        let result = {
            let _fault = gat_core::fault::armed("add.journaled");
            destination.mounts().with_locked(
                &NoopProgress,
                |locked| -> Result<_, MountWorkflowError> {
                    locked.add(MountAdd::new(
                        ConfigScope::Project,
                        name.clone(),
                        target.clone(),
                        pre_config.clone(),
                        post_config.clone(),
                        post_config.clone(),
                        prepared.rows(Selection::root()),
                    ))
                },
            )
        };
        assert!(result.is_err());
        assert!(
            !destination
                .load_config_scoped(ConfigScope::Project)
                .unwrap()
                .mounts
                .by_name
                .contains_key(&name)
        );

        destination
            .with_desired_mutation(
                &NoopProgress,
                |config, desired| -> std::result::Result<(), Box<dyn std::error::Error>> {
                    assert!(config.mounts.by_name.contains_key(&name));
                    assert!(desired.desired_any_subtree(&gp("models/weights.bin"))?);
                    Ok(())
                },
            )
            .unwrap();
        destination.mounts().recover_pending(&NoopProgress).unwrap();

        assert!(
            destination
                .load_config_scoped(ConfigScope::Project)
                .unwrap()
                .mounts
                .by_name
                .contains_key(&name)
        );
        let excludes =
            std::fs::read_to_string(destination_dir.path().join(".git/info/exclude")).unwrap();
        assert!(excludes.contains("/models/weights.bin"));
        assert!(
            !destination_dir
                .path()
                .join(".gat/mount-transaction.json")
                .exists()
        );
        assert!(
            !destination_dir
                .path()
                .join(".gat/mount-transaction-rows")
                .exists()
        );

        let mounted_config = destination
            .load_config_scoped(ConfigScope::Project)
            .unwrap();
        let outcome = destination
            .mounts()
            .with_locked(&NoopProgress, |locked| -> Result<_, MountWorkflowError> {
                locked.remove(MountRemove::new(
                    ConfigScope::Project,
                    name,
                    target,
                    mounted_config,
                    Config::default(),
                    Config::default(),
                ))
            })
            .unwrap();
        assert_eq!(outcome.removed, 1);
        let excludes =
            std::fs::read_to_string(destination_dir.path().join(".git/info/exclude")).unwrap();
        assert!(!excludes.contains("/models/weights.bin"));
    }

    #[test]
    fn interruption_before_journal_publication_never_exposes_mount_state() {
        let source_dir = crate::test_harness::git_repo();
        let source = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(source_dir.path().to_path_buf());
        source
            .save_lock(&Lock {
                entries: vec![Entry {
                    path: gp("weights.bin"),
                    oid: Oid::from_hex(&"b".repeat(64)).unwrap(),
                }],
            })
            .unwrap();
        crate::test_harness::commit_all(source_dir.path(), "source lock");
        let prepared = MountSourceLocation::parse(GitLocationSpec::from_string(
            source_dir.path().display().to_string(),
        ))
        .unwrap()
        .prepare(None, &NoopProgress, &crate::TransferCancellation::default())
        .unwrap();

        let destination_dir = crate::test_harness::git_repo();
        let destination = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(destination_dir.path().to_path_buf());
        let name = MountName::from_string("models".to_string());
        let target = gp("models");
        let pre_config = Config::default();
        let mut post_config = pre_config.clone();
        post_config
            .mounts
            .by_name
            .insert(name.clone(), mount_config(target.clone()));

        let result = {
            let _fault = gat_core::fault::armed("add.staged");
            destination.mounts().with_locked(
                &NoopProgress,
                |locked| -> Result<_, MountWorkflowError> {
                    locked.add(MountAdd::new(
                        ConfigScope::Project,
                        name.clone(),
                        target.clone(),
                        pre_config.clone(),
                        post_config.clone(),
                        post_config.clone(),
                        prepared.rows(Selection::root()),
                    ))
                },
            )
        };
        assert!(result.is_err());
        assert!(
            !destination_dir
                .path()
                .join(".gat/mount-transaction.json")
                .exists()
        );

        destination.mounts().recover_pending(&NoopProgress).unwrap();
        assert!(
            destination
                .load_config_scoped(ConfigScope::Project)
                .unwrap()
                .mounts
                .by_name
                .is_empty()
        );
        let count = destination
            .mounts()
            .with_locked(&NoopProgress, |locked| locked.desired_count(&target))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn recovery_resumes_after_an_interruption_following_config_publication() {
        let source_dir = crate::test_harness::git_repo();
        let source = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(source_dir.path().to_path_buf());
        source
            .save_lock(&Lock {
                entries: vec![Entry {
                    path: gp("weights.bin"),
                    oid: Oid::from_hex(&"c".repeat(64)).unwrap(),
                }],
            })
            .unwrap();
        crate::test_harness::commit_all(source_dir.path(), "source lock");
        let prepared = MountSourceLocation::parse(GitLocationSpec::from_string(
            source_dir.path().display().to_string(),
        ))
        .unwrap()
        .prepare(None, &NoopProgress, &crate::TransferCancellation::default())
        .unwrap();

        let destination_dir = crate::test_harness::git_repo();
        let destination = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(destination_dir.path().to_path_buf());
        let name = MountName::from_string("models".to_string());
        let target = gp("models");
        let pre_config = Config::default();
        let mut post_config = pre_config.clone();
        post_config
            .mounts
            .by_name
            .insert(name.clone(), mount_config(target.clone()));

        {
            let _fault = gat_core::fault::armed("add.journaled");
            let result = destination.mounts().with_locked(
                &NoopProgress,
                |locked| -> Result<_, MountWorkflowError> {
                    locked.add(MountAdd::new(
                        ConfigScope::Project,
                        name.clone(),
                        target.clone(),
                        pre_config.clone(),
                        post_config.clone(),
                        post_config.clone(),
                        prepared.rows(Selection::root()),
                    ))
                },
            );
            assert!(result.is_err());
        }

        {
            let _fault = gat_core::fault::armed("recover.after_config");
            assert!(destination.mounts().recover_pending(&NoopProgress).is_err());
        }
        assert!(
            destination_dir
                .path()
                .join(".gat/mount-transaction.json")
                .exists()
        );

        destination.mounts().recover_pending(&NoopProgress).unwrap();
        assert!(
            destination
                .load_config_scoped(ConfigScope::Project)
                .unwrap()
                .mounts
                .by_name
                .contains_key(&name)
        );
        let count = destination
            .mounts()
            .with_locked(&NoopProgress, |locked| locked.desired_count(&target))
            .unwrap();
        assert_eq!(count, 1);
        assert!(
            !destination_dir
                .path()
                .join(".gat/mount-transaction.json")
                .exists()
        );
    }

    #[test]
    fn update_and_remove_recovery_roll_forward_from_deleted_rows() {
        let source_dir = crate::test_harness::git_repo();
        let source = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(source_dir.path().to_path_buf());
        source
            .save_lock(&Lock {
                entries: vec![Entry {
                    path: gp("weights.bin"),
                    oid: Oid::from_hex(&"d".repeat(64)).unwrap(),
                }],
            })
            .unwrap();
        crate::test_harness::commit_all(source_dir.path(), "source lock");
        let prepared = MountSourceLocation::parse(GitLocationSpec::from_string(
            source_dir.path().display().to_string(),
        ))
        .unwrap()
        .prepare(None, &NoopProgress, &crate::TransferCancellation::default())
        .unwrap();

        let destination_dir = crate::test_harness::git_repo();
        let destination = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(destination_dir.path().to_path_buf());
        let name = MountName::from_string("models".to_string());
        let old_target = gp("models");
        let new_target = gp("vendor/models");
        let empty = Config::default();
        let mut mounted = empty.clone();
        mounted
            .mounts
            .by_name
            .insert(name.clone(), mount_config(old_target.clone()));
        destination
            .mounts()
            .with_locked(&NoopProgress, |locked| {
                locked.add(MountAdd::new(
                    ConfigScope::Project,
                    name.clone(),
                    old_target.clone(),
                    empty.clone(),
                    mounted.clone(),
                    mounted.clone(),
                    prepared.rows(Selection::root()),
                ))
            })
            .unwrap();

        let mut updated = mounted.clone();
        updated.mounts.by_name.get_mut(&name).unwrap().target = new_target.clone();
        {
            let _fault = gat_core::fault::armed("update.deleted");
            let result = destination.mounts().with_locked(
                &NoopProgress,
                |locked| -> Result<_, MountWorkflowError> {
                    locked.update(MountUpdate::new(
                        ConfigScope::Project,
                        name.clone(),
                        old_target.clone(),
                        new_target.clone(),
                        mounted.clone(),
                        updated.clone(),
                        updated.clone(),
                        prepared.rows(Selection::root()),
                    ))
                },
            );
            assert!(result.is_err());
        }
        destination.mounts().recover_pending(&NoopProgress).unwrap();
        assert_eq!(
            destination
                .load_config_scoped(ConfigScope::Project)
                .unwrap()
                .mounts
                .by_name
                .get(&name)
                .unwrap()
                .target,
            new_target
        );
        destination
            .mounts()
            .with_locked(&NoopProgress, |locked| {
                assert_eq!(locked.desired_count(&old_target)?, 0);
                assert_eq!(locked.desired_count(&new_target)?, 1);
                Ok::<_, MountWorkflowError>(())
            })
            .unwrap();

        {
            let _fault = gat_core::fault::armed("remove.deleted");
            let result = destination.mounts().with_locked(
                &NoopProgress,
                |locked| -> Result<_, MountWorkflowError> {
                    locked.remove(MountRemove::new(
                        ConfigScope::Project,
                        name.clone(),
                        new_target.clone(),
                        updated.clone(),
                        empty.clone(),
                        empty.clone(),
                    ))
                },
            );
            assert!(result.is_err());
        }
        destination.mounts().recover_pending(&NoopProgress).unwrap();
        assert!(
            destination
                .load_config_scoped(ConfigScope::Project)
                .unwrap()
                .mounts
                .by_name
                .is_empty()
        );
        let count = destination
            .mounts()
            .with_locked(&NoopProgress, |locked| locked.desired_count(&new_target))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use gat_core::progress::NoopProgress;

    #[test]
    fn setting_edit_recovers_pending_publication_before_capturing_configuration() {
        let directory = crate::test_harness::git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(directory.path().to_path_buf());
        let post_config = Config {
            sync: gat_core::config::SyncConfig {
                auto_fetch: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        let journal = mount_journal(&repo);
        journal
            .write(&MountTxnRecord {
                change: MountTxnChange::Add {
                    target: GatPath::parse_canonical("models").unwrap(),
                    row_windows: 0,
                },
                scope: ConfigScope::Project,
                name: "models".into(),
                post_config,
                pre_config: Config::default(),
                shard_levels: gat_core::lock::LockShardLevels::FLAT,
                phase: MountTxnPhase::Publish,
            })
            .unwrap();
        repo.change_setting(
            ConfigScope::Project,
            gat_core::settings::SettingChange::Set(
                gat_core::settings::SettingAssignment::NetworkRequestConcurrency(
                    gat_core::settings::ConcurrencyLimit::new(7).unwrap(),
                ),
            ),
        )
        .unwrap();
        repo.mounts().recover_pending(&NoopProgress).unwrap();
        let config = repo.load_config_scoped(ConfigScope::Project).unwrap();
        assert_eq!(config.sync.auto_fetch, Some(true));
        assert_eq!(config.network.resolve().request_concurrency.get(), 7);
        assert!(journal.read().unwrap().is_none());
    }

    #[test]
    fn completed_publication_is_not_replayed_when_environment_or_file_settings_change() {
        let directory = crate::test_harness::git_repo();
        let repo = crate::Invocation::from_pairs([("GAT_LOCK_SHARD_LEVELS", "2")])
            .unwrap()
            .repository_at(directory.path().to_path_buf());
        let durable = Config {
            sync: gat_core::config::SyncConfig {
                auto_fetch: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        repo.save_config(&durable).unwrap();
        let journal = mount_journal(&repo);
        journal
            .write(&MountTxnRecord {
                change: MountTxnChange::Add {
                    target: GatPath::parse_canonical("models").unwrap(),
                    row_windows: 0,
                },
                scope: ConfigScope::Project,
                name: "models".into(),
                post_config: Config::default(),
                pre_config: Config::default(),
                shard_levels: gat_core::lock::LockShardLevels::FLAT,
                phase: MountTxnPhase::Regenerate,
            })
            .unwrap();
        {
            let _fault = gat_core::fault::armed("recover.after_config");
            repo.mounts().recover_pending(&NoopProgress).unwrap();
        }
        assert_eq!(
            repo.load_config_scoped(ConfigScope::Project).unwrap(),
            durable
        );
        assert!(journal.read().unwrap().is_none());
    }
}
