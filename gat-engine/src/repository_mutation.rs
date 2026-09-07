//! Repository-bound add/remove/move state workflows.
//!
//! Commands retain selector, ownership, progress, and rollback policy. This
//! service owns refreshed desired/materialized state and delegates all
//! publication-shape, repository-lock, shard, receipt, proof, and `SQLite`
//! decisions to `gat-io`.

use crate::SyncError;
use crate::cache_session::CacheSession;
use crate::repository::Repository;
use gat_core::config::{Config, DEFAULT_INGEST_STRATEGY, IngestStrategy};
use gat_core::globs::GatGlobPattern;
use gat_core::lexical_path::GatPath;
use gat_core::lock::Entry;
use gat_core::oid::Oid;
use gat_io::GatIgnore;
use gat_io::{
    AddCandidateDiscoveryError, DesiredCandidateScope, DesiredMutationOpenError,
    DesiredMutationSession, DesiredPublicationError, DesiredStateOpenError, DesiredStateSession,
    MaterializationPreparationError, PreparedMaterialization,
};

type BoxedSource = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Below this size, coarse whole-percent activity updates add more noise than
/// useful feedback.
pub const LARGE_FILE_PROGRESS_THRESHOLD: u64 = 64 * 1024 * 1024;

/// Git's semantic status for one root-relative path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GitPathStatus {
    pub tracked: bool,
    pub gitignored: bool,
}

impl From<gat_io::GitPathStatus> for GitPathStatus {
    fn from(status: gat_io::GitPathStatus) -> Self {
        Self {
            tracked: status.tracked,
            gitignored: status.gitignored,
        }
    }
}

/// A path eligible for add preparation, optionally carrying the desired OID
/// already obtained by directory discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddCandidate {
    pub path: GatPath,
    pub desired_oid: Option<Oid>,
}

/// An engine-owned semantic entry paired with private I/O proof state.
///
/// Callers may inspect the entry for command policy. Only repository services
/// can construct or consume the proof-bearing representation.
#[derive(Debug)]
pub struct MaterializedEntry(PreparedMaterialization);

impl MaterializedEntry {
    #[must_use]
    pub const fn entry(&self) -> &Entry {
        self.0.entry()
    }
}

impl PartialEq for MaterializedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.entry() == other.entry()
    }
}

impl Eq for MaterializedEntry {}

/// Semantic bounds for a command-owned desired-state candidate scan.
#[derive(Clone, Debug)]
pub enum DesiredScope {
    Any,
    Subtree(GatPath),
    Prefix(String),
    Exact(GatPath),
}

impl DesiredScope {
    fn into_io(self) -> DesiredCandidateScope {
        match self {
            Self::Any => DesiredCandidateScope::Any,
            Self::Subtree(path) => DesiredCandidateScope::Subtree(path),
            Self::Prefix(prefix) => DesiredCandidateScope::Prefix(prefix),
            Self::Exact(path) => DesiredCandidateScope::Exact(path),
        }
    }
}

/// Semantic preparation/read stages for repository mutation workflows.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryStateError {
    #[error("could not open repository state")]
    Open {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not refresh desired repository state")]
    Refresh {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not read desired repository state")]
    Read {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not verify reusable materialized entries")]
    Verify {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not discover repository paths")]
    Discover {
        #[source]
        source: BoxedSource,
    },
    #[error("could not read the repository .gatignore")]
    GatIgnore {
        #[source]
        source: std::io::Error,
    },
    #[error("could not ingest worktree content")]
    Ingest {
        #[source]
        source: Box<SyncError>,
    },
}

impl RepositoryStateError {
    pub(crate) fn open(source: gat_io::StateStoreError) -> Self {
        Self::Open {
            source: Box::new(source.into()),
        }
    }

    pub(crate) fn refresh(source: SyncError) -> Self {
        Self::Refresh {
            source: Box::new(source),
        }
    }

    fn from_open(error: DesiredStateOpenError) -> Self {
        match error {
            DesiredStateOpenError::Open(source) => Self::Open {
                source: Box::new(source.into()),
            },
            DesiredStateOpenError::Refresh(source) => Self::Refresh {
                source: Box::new(refresh_failure(source)),
            },
        }
    }

    pub(crate) fn read(source: gat_io::StateStoreError) -> Self {
        Self::Read {
            source: Box::new(source.into()),
        }
    }

    fn verify(source: MaterializationPreparationError) -> Self {
        Self::Verify {
            source: Box::new(preparation_failure(source)),
        }
    }

    fn discover(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Discover {
            source: Box::new(source),
        }
    }

    fn ingest(source: MaterializationPreparationError) -> Self {
        Self::Ingest {
            source: Box::new(preparation_failure(source)),
        }
    }
}

/// Semantic mutation stages for desired/materialized repository state.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryMutationError {
    #[error("desired state changed during preparation")]
    Stale,
    #[error("could not acquire repository mutation state")]
    Acquire {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not read repository mutation state")]
    Read {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not publish desired repository state")]
    Publish {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not record materialized repository state")]
    RecordMaterialized {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not forget materialized repository state")]
    ForgetMaterialized {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not move materialized repository state")]
    MoveMaterialized {
        #[source]
        source: Box<SyncError>,
    },
    #[error("could not regenerate repository excludes")]
    RegenerateExcludes {
        #[source]
        source: Box<SyncError>,
    },
}

impl RepositoryMutationError {
    fn acquire(source: DesiredMutationOpenError) -> Self {
        if matches!(source, DesiredMutationOpenError::Stale) {
            return Self::Stale;
        }
        let source = match source {
            DesiredMutationOpenError::Stale => unreachable!(),
            DesiredMutationOpenError::Acquire(source) => source.into(),
            DesiredMutationOpenError::Open(source) => source.into(),
            DesiredMutationOpenError::Refresh(source) => refresh_failure(source),
        };
        Self::Acquire {
            source: Box::new(source),
        }
    }

    fn read(source: gat_io::StateStoreError) -> Self {
        Self::Read {
            source: Box::new(source.into()),
        }
    }

    fn publish(source: DesiredPublicationError) -> Self {
        let source = publication_failure(source);
        Self::Publish {
            source: Box::new(source),
        }
    }
}

fn publication_failure(source: DesiredPublicationError) -> SyncError {
    match source {
        DesiredPublicationError::State(source) => source.into(),
        DesiredPublicationError::Lock(source) => source.into(),
        DesiredPublicationError::Atomic(source) => source.into(),
    }
}

fn refresh_failure(source: gat_io::DesiredRefreshError) -> SyncError {
    match source {
        gat_io::DesiredRefreshError::Atomic(source) => source.into(),
        gat_io::DesiredRefreshError::Lock(source) => source.into(),
        gat_io::DesiredRefreshError::State(source) => source.into(),
    }
}

fn preparation_failure(source: MaterializationPreparationError) -> SyncError {
    match source {
        MaterializationPreparationError::State(source) => source.into(),
        MaterializationPreparationError::Worktree(source) => source.into(),
        MaterializationPreparationError::Cache(source) => source.into(),
    }
}

/// Unlocked desired/materialized preparation bound to one repository and one
/// immutable effective-config snapshot.
pub struct DesiredState<'repo, 'config> {
    repo: &'repo Repository,
    config: &'config Config,
    gatignore: GatIgnore,
    session: DesiredStateSession<'repo>,
}

impl<'repo, 'config> DesiredState<'repo, 'config> {
    pub(crate) fn open(
        repo: &'repo Repository,
        config: &'config Config,
    ) -> Result<Self, RepositoryStateError> {
        let gatignore =
            GatIgnore::load(repo.layout()).map_err(|error| RepositoryStateError::GatIgnore {
                source: error.into_source(),
            })?;
        let session =
            DesiredStateSession::open(repo.layout()).map_err(RepositoryStateError::from_open)?;
        Ok(Self {
            repo,
            config,
            gatignore,
            session,
        })
    }

    pub fn desired_any_exact(&self, path: &GatPath) -> Result<bool, RepositoryStateError> {
        self.session
            .desired_any_exact(path)
            .map_err(RepositoryStateError::read)
    }

    /// Create the side-effect-free cache capability shared by every
    /// materialization batch in one add invocation.
    pub fn materialization_session(&self) -> MaterializationSession<'_, 'repo, 'config> {
        MaterializationSession {
            desired: self,
            cache_root: self.repo.resolved_cache_root_from(self.config),
            cache: CacheSession::default(),
        }
    }

    pub fn ingest_strategy(&self) -> IngestStrategy {
        self.config
            .cache
            .ingest_strategy
            .unwrap_or(DEFAULT_INGEST_STRATEGY)
    }

    pub fn is_gatignored(&self, path: &GatPath) -> bool {
        self.gatignore.is_ignored(path.as_str())
    }

    pub fn partition_reusable(
        &self,
        candidates: Vec<AddCandidate>,
    ) -> Result<(Vec<MaterializedEntry>, Vec<GatPath>), RepositoryStateError> {
        let candidates = candidates
            .into_iter()
            .map(|candidate| (candidate.path, candidate.desired_oid))
            .collect();
        let (reused, to_hash) = self
            .session
            .partition_reusable(candidates, None)
            .map_err(RepositoryStateError::verify)?;
        Ok((reused.into_iter().map(MaterializedEntry).collect(), to_hash))
    }

    pub fn discover_add_candidates(
        &self,
        dir: Option<&GatPath>,
        residual: Option<&GatGlobPattern>,
        force: bool,
        exclusions: &mut Vec<crate::AddExclusion>,
    ) -> Result<Vec<AddCandidate>, RepositoryStateError> {
        self.session
            .discover_add_candidates(dir, residual, &self.gatignore, force, exclusions)
            .map(|candidates| {
                candidates
                    .into_iter()
                    .map(|(path, desired_oid)| AddCandidate { path, desired_oid })
                    .collect()
            })
            .map_err(|source: AddCandidateDiscoveryError| match source {
                AddCandidateDiscoveryError::State(source) => RepositoryStateError::read(source),
                AddCandidateDiscoveryError::Git(source) => RepositoryStateError::discover(source),
            })
    }

    pub fn with_git_path_status_lookup<T, E>(
        &self,
        f: impl FnOnce(&mut dyn FnMut(&str) -> Result<GitPathStatus, E>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<RepositoryStateError>,
    {
        self.session
            .with_git_path_status_lookup(|lookup| {
                let mut adapted = |rel: &str| {
                    lookup(rel)
                        .map(GitPathStatus::from)
                        .map_err(RepositoryStateError::discover)
                        .map_err(E::from)
                };
                f(&mut adapted)
            })
            .map_err(RepositoryStateError::discover)
            .map_err(E::from)?
    }

    pub fn into_mutation(self) -> Result<DesiredMutation<'repo, 'config>, RepositoryMutationError> {
        let session = self
            .session
            .into_mutation(self.config.lock.shard_levels())
            .map_err(RepositoryMutationError::acquire)?;
        Ok(DesiredMutation {
            repo: self.repo,
            config: self.config,
            session,
        })
    }
}

/// Add-scoped proof-client ownership.
///
/// Construction only resolves the effective cache location. Proof-database
/// opening remains lazy until the first batch that needs hashing.
pub struct MaterializationSession<'state, 'repo, 'config> {
    desired: &'state DesiredState<'repo, 'config>,
    cache_root: gat_io::CacheRoot,
    cache: CacheSession,
}

impl MaterializationSession<'_, '_, '_> {
    /// Reuse requires both unchanged worktree identity and a present cache object.
    /// Presence checks neither open the proof database nor register byte access.
    pub fn partition_reusable(
        &self,
        candidates: Vec<AddCandidate>,
    ) -> Result<(Vec<MaterializedEntry>, Vec<GatPath>), RepositoryStateError> {
        let candidates = candidates
            .into_iter()
            .map(|candidate| (candidate.path, candidate.desired_oid))
            .collect();
        let (reused, to_hash) = self
            .desired
            .session
            .partition_reusable(candidates, Some(&self.cache_root.presence()))
            .map_err(RepositoryStateError::verify)?;
        Ok((reused.into_iter().map(MaterializedEntry).collect(), to_hash))
    }

    pub fn ingest_materialized_entries(
        &mut self,
        files: &[GatPath],
        strategy: IngestStrategy,
        on_progress: impl Fn(&GatPath, Option<u8>) + Sync,
        on_complete: impl Fn() + Sync,
    ) -> Result<Vec<MaterializedEntry>, RepositoryStateError> {
        self.cache
            .ingest_materializations(
                &self.desired.session,
                &self.cache_root,
                files,
                strategy,
                on_progress,
                on_complete,
            )
            .map(|entries| entries.into_iter().map(MaterializedEntry).collect())
            .map_err(RepositoryStateError::ingest)
    }
}

/// One lock-stable desired/materialized mutation bound to exactly one
/// repository.
pub struct DesiredMutation<'repo, 'config> {
    repo: &'repo Repository,
    config: &'config Config,
    session: DesiredMutationSession<'repo>,
}

impl<'repo, 'config> DesiredMutation<'repo, 'config> {
    pub(crate) fn acquire(
        repo: &'repo Repository,
        config: &'config Config,
    ) -> Result<Self, RepositoryMutationError> {
        let session = DesiredMutationSession::acquire(repo.layout(), config.lock.shard_levels())
            .map_err(RepositoryMutationError::acquire)?;
        Ok(Self {
            repo,
            config,
            session,
        })
    }

    pub fn desired_any_subtree(&self, path: &GatPath) -> Result<bool, RepositoryMutationError> {
        self.session
            .desired_any_subtree(path)
            .map_err(RepositoryMutationError::read)
    }

    pub fn publish_upserts(
        &mut self,
        entries: Vec<MaterializedEntry>,
    ) -> Result<(), RepositoryMutationError> {
        self.session
            .publish_upserts(entries.into_iter().map(|entry| entry.0).collect())
            .map_err(RepositoryMutationError::publish)
    }

    pub fn record_published_materialized(&mut self) -> Result<(), RepositoryMutationError> {
        self.session
            .record_published_materialized()
            .map_err(|source| RepositoryMutationError::RecordMaterialized {
                source: Box::new(source.into()),
            })
    }

    pub fn resolve_move(
        &mut self,
        src: &GatPath,
        dst: &GatPath,
    ) -> Result<(Vec<Entry>, Vec<Entry>), RepositoryMutationError> {
        self.session
            .resolve_move(src, dst)
            .map_err(RepositoryMutationError::read)
    }

    pub fn publish_move(
        &mut self,
        src: &GatPath,
        dst: &GatPath,
        collision_paths: &[GatPath],
    ) -> Result<(), RepositoryMutationError> {
        self.session
            .publish_move(src, dst, collision_paths)
            .map_err(RepositoryMutationError::publish)
    }

    pub fn resolve_removals(
        &mut self,
        scopes: Vec<DesiredScope>,
        remove: impl FnMut(&GatPath) -> bool,
    ) -> Result<(), RepositoryMutationError> {
        let scopes = scopes
            .into_iter()
            .map(DesiredScope::into_io)
            .collect::<Vec<_>>();
        self.session
            .resolve_removals(&scopes, remove)
            .map_err(RepositoryMutationError::read)
    }

    pub fn publish_removals(
        &mut self,
        affected_paths: &[GatPath],
        prefixes: &[GatPath],
        include_exact: bool,
    ) -> Result<(), RepositoryMutationError> {
        self.session
            .publish_removals(affected_paths, prefixes, include_exact)
            .map_err(RepositoryMutationError::publish)
    }

    pub fn forget_materialized(
        &mut self,
        paths: &[GatPath],
    ) -> Result<(), RepositoryMutationError> {
        self.session.forget_materialized(paths).map_err(|source| {
            RepositoryMutationError::ForgetMaterialized {
                source: Box::new(source.into()),
            }
        })
    }

    pub fn move_materialized(
        &mut self,
        src: &GatPath,
        dst: &GatPath,
    ) -> Result<(), RepositoryMutationError> {
        self.session.move_materialized(src, dst).map_err(|source| {
            RepositoryMutationError::MoveMaterialized {
                source: Box::new(source.into()),
            }
        })
    }

    pub fn sync_excludes(&self) -> Result<crate::excludes::SyncStatus, RepositoryMutationError> {
        crate::excludes::sync_from_mutation(self.repo, &self.session, false, self.config).map_err(
            |source| RepositoryMutationError::RegenerateExcludes {
                source: Box::new(Box::new(source).into()),
            },
        )
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn load_materialized_for_test(
    repo: &Repository,
) -> Result<gat_core::lock::Lock, RepositoryStateError> {
    gat_io::state_load_materialized_for_test(repo.layout()).map_err(RepositoryStateError::read)
}

/// Seeds semantic materialized entries for integration tests and benchmark
/// fixtures while keeping persistence rows and filesystem proofs below I/O.
#[cfg(any(test, feature = "test-support"))]
pub fn record_materialized_for_test(
    repo: &Repository,
    entries: &[Entry],
) -> Result<(), RepositoryMutationError> {
    gat_io::state_record_materialized_for_test(repo.layout(), entries).map_err(|source| {
        RepositoryMutationError::RecordMaterialized {
            source: Box::new(publication_failure(source)),
        }
    })
}
