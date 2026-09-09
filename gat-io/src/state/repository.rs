//! Repository-bound desired/materialized-state sessions.
//!
//! This is the persistence boundary used by `gat-engine` repository
//! workflows. Callers provide semantic paths, entries, and the already
//! resolved target shard depth; this module owns the `SQLite` connection,
//! repository lock, live publication shape, touched-shard accounting,
//! publication evidence, and materialized proof rows.

use super::{
    CandidateBound, DesiredQuery, DesiredRefreshError, DesiredRows, MaterializedRow, StateStore,
    StateStoreError,
};
use crate::cache::{CacheClient, CacheError, IngestStrategy};
use crate::git::{AddExclusion, AddExclusionReason, GatIgnore, GitDiscovery, GitDiscoveryError};
use crate::lock::{LockError, LockShardLevels, LockStore, LockWriteGuard};
use crate::repository_layout::RepositoryLayout;
use crate::worktree::{self, WorktreeMutationError};
use gat_core::globs::GatGlobPattern;
use gat_core::lexical_path::GatPath;
use gat_core::lock::{Entry, Lock};
use gat_core::oid::Oid;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

/// Failure while opening and refreshing an unlocked desired-state session.
#[derive(Debug, thiserror::Error)]
pub enum DesiredStateOpenError {
    #[error("could not open repository state")]
    Open(#[source] StateStoreError),
    #[error("could not refresh desired repository state")]
    Refresh(#[source] DesiredRefreshError),
}

/// Failure while acquiring a lock-stable repository mutation session.
#[derive(Debug, thiserror::Error)]
pub enum DesiredMutationOpenError {
    #[error("desired state changed during preparation")]
    Stale,
    #[error("could not acquire desired-state publication authority")]
    Acquire(#[source] LockError),
    #[error("could not open repository state")]
    Open(#[source] StateStoreError),
    #[error("could not refresh desired repository state")]
    Refresh(#[source] DesiredRefreshError),
}

/// Failure while publishing a semantic desired-state mutation.
#[derive(Debug, thiserror::Error)]
pub enum DesiredPublicationError {
    #[error("desired-state persistence failed")]
    State(#[from] StateStoreError),
    #[error("canonical desired-state publication failed")]
    Lock(#[from] LockError),
    #[error("repository mutation lock acquisition failed")]
    Atomic(#[from] crate::atomic::AtomicError),
}

/// Failure while preparing proof-bearing materialized entries.
#[derive(Debug, thiserror::Error)]
pub enum MaterializationPreparationError {
    #[error("repository state lookup failed")]
    State(#[from] StateStoreError),
    #[error("worktree identity preparation failed")]
    Worktree(#[from] WorktreeMutationError),
    #[error("object cache preparation failed")]
    Cache(#[from] CacheError),
}

/// Failure while discovering add candidates from Git and desired state.
#[derive(Debug, thiserror::Error)]
pub enum AddCandidateDiscoveryError {
    #[error("Git path discovery failed")]
    Git(#[from] GitDiscoveryError),
    #[error("desired-state candidate lookup failed")]
    State(#[from] StateStoreError),
}

/// One semantic bound for a desired-state candidate scan.
///
/// This deliberately describes only the caller's logical search space. The
/// `SQLite` query plan selected for it remains private to this module.
#[derive(Clone, Debug)]
pub enum DesiredCandidateScope {
    Any,
    Subtree(GatPath),
    Prefix(String),
    Exact(GatPath),
}

impl DesiredCandidateScope {
    fn as_bound(&self) -> CandidateBound<'_> {
        match self {
            Self::Any => CandidateBound::Any,
            Self::Subtree(path) => CandidateBound::scope(path.as_str()),
            Self::Prefix(prefix) => CandidateBound::prefix(prefix),
            Self::Exact(path) => CandidateBound::exact(path.as_str()),
        }
    }
}

/// An entry paired with the private filesystem proof established for exactly
/// its content.
///
/// The semantic entry may be inspected by `gat-engine`; the proof cannot be
/// observed, separated, or manufactured outside `gat-io`.
pub struct PreparedMaterialization {
    entry: Entry,
    proof: Option<crate::file_state::StatProof>,
    unchanged_desired: bool,
}

impl std::fmt::Debug for PreparedMaterialization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedMaterialization")
            .field("entry", &self.entry)
            .finish_non_exhaustive()
    }
}

impl PreparedMaterialization {
    #[must_use]
    pub const fn entry(&self) -> &Entry {
        &self.entry
    }

    const fn new(entry: Entry, proof: Option<crate::file_state::StatProof>) -> Self {
        Self {
            entry,
            proof,
            unchanged_desired: false,
        }
    }

    pub(crate) const fn unchanged_desired(&self) -> bool {
        self.unchanged_desired
    }

    fn into_row(self) -> MaterializedRow {
        MaterializedRow::from_entry(self.entry, self.proof)
    }
}

/// An unlocked, refreshed desired/materialized-state view bound to one
/// repository for its full lifetime.
///
/// Add keeps this session alive across discovery and hashing, then consumes it
/// into [`DesiredMutationSession`] immediately before publication. That
/// transition retains the same `SQLite` connection and never refreshes twice.
pub struct DesiredStateSession<'repo> {
    layout: &'repo RepositoryLayout,
    store: StateStore,
    git: std::cell::OnceCell<GitDiscovery>,
    identity: crate::lock::CanonicalDesiredIdentity,
    catalog: HashMap<crate::lock::LockShardId, super::StoredShard>,
}

impl<'repo> DesiredStateSession<'repo> {
    pub fn open(layout: &'repo RepositoryLayout) -> Result<Self, DesiredStateOpenError> {
        let mut store = StateStore::open(layout).map_err(DesiredStateOpenError::Open)?;
        let mut catalog = HashMap::new();
        let identity = store
            .refresh_and_pin_with_catalog(layout, &mut catalog)
            .map_err(DesiredStateOpenError::Refresh)?
            .identity();
        Ok(Self {
            layout,
            store,
            git: std::cell::OnceCell::new(),
            identity,
            catalog,
        })
    }

    fn git(&self) -> Result<&GitDiscovery, GitDiscoveryError> {
        if self.git.get().is_none() {
            let _ = self.git.set(GitDiscovery::open(self.layout)?);
        }
        Ok(self.git.get().expect("initialized above"))
    }

    pub fn with_git_path_status_lookup<T, E>(
        &self,
        f: impl FnOnce(
            &mut dyn FnMut(&str) -> Result<crate::git::GitPathStatus, GitDiscoveryError>,
        ) -> Result<T, E>,
    ) -> Result<Result<T, E>, GitDiscoveryError> {
        self.git()?.with_path_status_lookup(f)
    }

    pub fn desired_any_exact(&self, path: &GatPath) -> Result<bool, StateStoreError> {
        self.store
            .desired_any(DesiredQuery::exact(std::slice::from_ref(path)))
    }

    pub fn partition_reusable(
        &self,
        candidates: Vec<(GatPath, Option<Oid>)>,
        presence: Option<&crate::CachePresence>,
    ) -> Result<(Vec<PreparedMaterialization>, Vec<GatPath>), MaterializationPreparationError> {
        let unresolved = candidates
            .iter()
            .filter(|(_, desired_oid)| desired_oid.is_none())
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let looked_up = if unresolved.is_empty() {
            Vec::new()
        } else {
            self.store.desired_rows(DesiredQuery::exact(&unresolved))?
        };
        let mut desired_oid_by_path = HashMap::with_capacity(candidates.len());
        for (path, desired_oid) in &candidates {
            if let Some(oid) = desired_oid {
                desired_oid_by_path.insert(path, oid);
            }
        }
        for entry in &looked_up {
            desired_oid_by_path.insert(&entry.path, &entry.oid);
        }

        let paths = candidates
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let materialized = self.store.materialized_rows_for(&paths)?;
        let materialized_by_path = materialized
            .iter()
            .map(|row| (&row.path, row))
            .collect::<HashMap<_, _>>();
        let results = candidates
            .par_iter()
            .map(|(path, _)| {
                let Some(&desired_oid) = desired_oid_by_path.get(path) else {
                    return Ok(None);
                };
                let row = materialized_by_path.get(path).copied();
                if row.is_some_and(|row| row.oid != *desired_oid) {
                    return Ok(None);
                }
                let prior_proof = row.and_then(|row| row.proof.as_ref());
                let Some(identity) = worktree::check_regular_file(
                    self.layout.root_path(),
                    path,
                    desired_oid,
                    prior_proof,
                )?
                else {
                    return Ok(None);
                };
                let proof = match identity {
                    crate::file_state::IdentityCheck::Proven => row.and_then(|row| row.proof),
                    crate::file_state::IdentityCheck::Hashed {
                        matches: true,
                        proof,
                        ..
                    } => proof,
                    crate::file_state::IdentityCheck::Hashed { matches: false, .. } => {
                        return Ok(None);
                    }
                };
                Ok(Some(PreparedMaterialization::new(
                    Entry {
                        path: path.clone(),
                        oid: *desired_oid,
                    },
                    proof,
                )))
            })
            .collect::<Vec<Result<_, MaterializationPreparationError>>>();

        let mut cached = if let Some(presence) = presence {
            let oids: HashSet<_> = results
                .iter()
                .filter_map(|result| {
                    result
                        .as_ref()
                        .ok()
                        .and_then(|entry| entry.as_ref())
                        .map(|entry| entry.entry.oid)
                })
                .collect();
            oids.into_par_iter()
                .map(|oid| (oid, presence.inspect_regular(&oid)))
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        };
        let mut reused = Vec::new();
        let mut to_hash = Vec::new();
        for ((path, _), result) in candidates.into_iter().zip(results) {
            if let Some(mut entry) = result? {
                if let Some(inspected) = cached.remove(&entry.entry.oid) {
                    let exists = inspected?;
                    cached.insert(entry.entry.oid, Ok(exists));
                    if !exists {
                        to_hash.push(path);
                        continue;
                    }
                }
                entry.unchanged_desired = true;
                reused.push(entry);
            } else {
                to_hash.push(path);
            }
        }
        Ok((reused, to_hash))
    }

    /// Ingest worktree files in deterministic bounded windows.
    ///
    /// Cache-publication receipts are consumed here and never cross the I/O
    /// boundary. The caller supplies its operation-scoped cache client so
    /// repeated batches share one proof connection. Proof-index persistence
    /// remains best-effort, matching cache correctness's existing
    /// graceful-degradation contract.
    pub fn ingest_materializations(
        &self,
        cache: &CacheClient,
        files: &[GatPath],
        strategy: IngestStrategy,
        large_file_threshold: u64,
        on_progress: impl Fn(&GatPath, Option<u8>) + Sync,
        on_complete: impl Fn() + Sync,
    ) -> Result<Vec<PreparedMaterialization>, MaterializationPreparationError> {
        let objects_dir = cache.objects_dir();
        let mut entries = Vec::with_capacity(files.len());
        for window in files.chunks(crate::cache::VERIFY_WINDOW) {
            let results = window
                .par_iter()
                .map(|path| {
                    on_progress(path, None);
                    let last_percent = std::sync::atomic::AtomicU64::new(u64::MAX);
                    let result = worktree::ingest_file(
                        self.layout.root_path(),
                        objects_dir,
                        path,
                        strategy,
                        large_file_threshold,
                        |read, len| {
                            let percent = read.saturating_mul(100).checked_div(len).unwrap_or(0);
                            if percent
                                != last_percent.swap(percent, std::sync::atomic::Ordering::Relaxed)
                            {
                                on_progress(path, u8::try_from(percent).ok());
                            }
                        },
                    )?;
                    on_complete();
                    Ok((
                        PreparedMaterialization::new(
                            Entry {
                                path: path.clone(),
                                oid: result.ingested.oid,
                            },
                            Some(result.proof),
                        ),
                        result.receipt,
                    ))
                })
                .collect::<Vec<Result<_, MaterializationPreparationError>>>();

            let mut receipts = Vec::new();
            for result in results {
                let (entry, receipt) = result?;
                entries.push(entry);
                receipts.extend(receipt);
            }
            if !receipts.is_empty() {
                let _ = cache.apply_publications(&receipts);
            }
        }
        Ok(entries)
    }

    /// Discover add candidates with one Git open and one streamed desired
    /// overlay from this session's already-refreshed store.
    #[allow(
        clippy::missing_panics_doc,
        reason = "The merge arm has already matched the desired entry as Some"
    )]
    pub fn discover_add_candidates(
        &self,
        dir: Option<&GatPath>,
        residual: Option<&GatGlobPattern>,
        gatignore: &GatIgnore,
        force: bool,
        exclusions: &mut Vec<AddExclusion>,
    ) -> Result<Vec<(GatPath, Option<Oid>)>, AddCandidateDiscoveryError> {
        let git = self.git()?;
        let mut observation_error = None;
        let mut ignored_files = Vec::new();
        let discovered_paths =
            git.discover_files_observing(dir, force, &mut |rel, reason, directory| {
                let Ok(path) = GatPath::parse_canonical(rel) else {
                    return true;
                };
                if let Some(dir) = dir {
                    let scope = dir.as_str();
                    if rel != scope
                        && !rel
                            .strip_prefix(scope)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                        && !(directory
                            && scope
                                .strip_prefix(rel)
                                .is_some_and(|suffix| suffix.starts_with('/')))
                    {
                        return true;
                    }
                }
                if directory && let Some(glob) = residual {
                    if !glob.can_match_descendant(rel) {
                        return true;
                    }
                    let prefix = match glob.bound() {
                        gat_core::globs::GlobBound::Any => "",
                        gat_core::globs::GlobBound::Prefix(prefix)
                        | gat_core::globs::GlobBound::Exact(prefix) => prefix,
                    };
                    if reason == AddExclusionReason::Infrastructure
                        && !crate::worktree::is_infrastructure_path(prefix)
                    {
                        return true;
                    }
                }

                if residual.is_some_and(|glob| !glob.matches(rel)) && !directory {
                    return true;
                }
                if reason == AddExclusionReason::GitIgnore && !directory {
                    ignored_files.push(path);
                    if ignored_files.len() == 4096
                        && let Err(error) =
                            self.record_ignored_files(&mut ignored_files, gatignore, exclusions)
                    {
                        observation_error = Some(error);
                        return false;
                    }
                    return true;
                }
                AddExclusion::record(exclusions, reason, path, directory);
                true
            });
        if let Some(error) = observation_error {
            return Err(error.into());
        }
        let discovered_paths = discovered_paths?;

        self.record_ignored_files(&mut ignored_files, gatignore, exclusions)?;
        let discovered_len = discovered_paths.len();
        let discovered = discovered_paths.into_iter().filter_map(|path| {
            let rel = path.as_str();
            if worktree::is_infrastructure_path(rel)
                || residual.is_some_and(|glob| !glob.matches(rel))
            {
                return None;
            }
            if !force && gatignore.is_ignored(rel) {
                AddExclusion::record(exclusions, AddExclusionReason::GatIgnore, path, false);
                return None;
            }
            Some((path, None))
        });

        let query = dir.map_or_else(DesiredQuery::all, DesiredQuery::scope);
        let next_desired = |rows: &mut DesiredRows<'_>| {
            while let Some(row) = rows.next()? {
                if worktree::is_infrastructure_path(row.path.as_str())
                    || (!force && gatignore.is_ignored(row.path.as_str()))
                {
                    continue;
                }
                if residual.is_some_and(|glob| !glob.matches(row.path.as_str())) {
                    continue;
                }
                if git.is_tracked(&row.path) {
                    continue;
                }
                let Ok((_, kind)) = worktree::inspect_read_path(self.layout.root_path(), &row.path)
                else {
                    continue;
                };
                if kind != worktree::EntryKind::File {
                    continue;
                }
                return Ok(Some((row.path, Some(row.oid))));
            }
            Ok::<_, StateStoreError>(None)
        };

        self.store.with_desired_rows(query, move |mut rows| {
            let mut candidates = Vec::with_capacity(discovered_len);
            let mut discovered = discovered.peekable();
            let mut desired = next_desired(&mut rows)?;
            loop {
                match (discovered.peek(), &desired) {
                    (None, None) => break,
                    (None, Some(_)) => {
                        candidates.push(desired.take().expect("matched Some"));
                        desired = next_desired(&mut rows)?;
                    }
                    (Some(_), None) => {
                        candidates.push(discovered.next().expect("matched Some"));
                    }
                    (Some((discovered_path, _)), Some((desired_path, _))) => {
                        match discovered_path.cmp(desired_path) {
                            std::cmp::Ordering::Less => {
                                candidates.push(discovered.next().expect("matched Some"));
                            }
                            std::cmp::Ordering::Greater => {
                                candidates.push(desired.take().expect("matched Some"));
                                desired = next_desired(&mut rows)?;
                            }
                            std::cmp::Ordering::Equal => {
                                candidates.push(desired.take().expect("matched Some"));
                                discovered.next();
                                desired = next_desired(&mut rows)?;
                            }
                        }
                    }
                }
            }
            Ok::<_, AddCandidateDiscoveryError>(candidates)
        })
    }

    fn record_ignored_files(
        &self,
        paths: &mut Vec<GatPath>,
        gatignore: &GatIgnore,
        exclusions: &mut Vec<AddExclusion>,
    ) -> Result<(), StateStoreError> {
        if paths.is_empty() {
            return Ok(());
        }
        let desired = self.store.desired_rows(DesiredQuery::exact(paths))?;
        let desired: HashSet<_> = desired.iter().map(|entry| &entry.path).collect();
        for path in paths.drain(..) {
            if desired.contains(&path) {
                if gatignore.is_ignored(path.as_str()) {
                    AddExclusion::record(exclusions, AddExclusionReason::GatIgnore, path, false);
                }
            } else {
                AddExclusion::record(exclusions, AddExclusionReason::GitIgnore, path, false);
            }
        }
        Ok(())
    }

    pub fn into_mutation(
        self,
        target: LockShardLevels,
    ) -> Result<DesiredMutationSession<'repo>, DesiredMutationOpenError> {
        let shape_lock = if self.identity == crate::lock::CanonicalDesiredIdentity::empty() {
            LockStore::acquire_current_or_target_shape(self.layout, target)
        } else {
            LockStore::acquire_matching_shape(self.layout, target)
        }
        .map_err(DesiredMutationOpenError::Acquire)?;
        let current =
            crate::lock::current_desired_identity_with_prior(self.layout.root_path(), |id| {
                self.catalog
                    .get(&id)
                    .map(|stored| (stored.identity, stored.proof))
            })
            .map_err(DesiredMutationOpenError::Acquire)?;
        if current != self.identity {
            return Err(DesiredMutationOpenError::Stale);
        }
        self.store
            .release_snapshot()
            .map_err(DesiredMutationOpenError::Open)?;
        Ok(DesiredMutationSession {
            layout: self.layout,
            store: self.store,
            shape_lock,
            target,
            full_lock: None,
            pending_materialized: None,
        })
    }
}

/// One lock-stable desired/materialized mutation session bound to one
/// repository.
///
/// Physical publication shape and receipts are private. The same refreshed
/// state connection is retained through desired publication, materialized
/// bookkeeping, and desired-path streaming for excludes.
pub struct DesiredMutationSession<'repo> {
    layout: &'repo RepositoryLayout,
    store: StateStore,
    shape_lock: LockWriteGuard,
    target: LockShardLevels,
    full_lock: Option<Lock>,
    pending_materialized: Option<Vec<PreparedMaterialization>>,
}

impl<'repo> DesiredMutationSession<'repo> {
    pub fn acquire(
        layout: &'repo RepositoryLayout,
        target: LockShardLevels,
    ) -> Result<Self, DesiredMutationOpenError> {
        let shape_lock = LockStore::acquire_matching_shape(layout, target)
            .map_err(DesiredMutationOpenError::Acquire)?;
        let mut store = StateStore::open(layout).map_err(DesiredMutationOpenError::Open)?;
        store
            .refresh_desired_identity(layout)
            .map_err(DesiredMutationOpenError::Refresh)?;
        Ok(Self {
            layout,
            store,
            shape_lock,
            target,
            full_lock: None,
            pending_materialized: None,
        })
    }

    pub fn desired_any_subtree(&self, path: &GatPath) -> Result<bool, StateStoreError> {
        self.store.desired_any(DesiredQuery::scope(path))
    }

    pub fn publish_upserts(
        &mut self,
        entries: Vec<PreparedMaterialization>,
    ) -> Result<(), DesiredPublicationError> {
        if self.shape_lock.can_publish_incrementally() {
            self.store.publish_prepared_add::<DesiredPublicationError>(
                self.layout.root_path(),
                &self.shape_lock,
                &entries,
            )?;
        } else {
            let semantic_entries = entries.iter().map(|entry| entry.entry.clone());
            let mut lock = self.store.load_desired_as_lock()?;
            lock.upsert_many(semantic_entries);
            self.store
                .publish_desired_complete::<DesiredPublicationError>(
                    self.layout,
                    &lock,
                    self.target,
                )?;
            self.full_lock = Some(lock);
        }
        self.pending_materialized = Some(entries);
        Ok(())
    }

    pub fn record_published_materialized(&mut self) -> Result<(), StateStoreError> {
        let Some(entries) = self.pending_materialized.take() else {
            return Ok(());
        };
        let mut entries = entries.into_iter();
        loop {
            let rows: Vec<_> = entries
                .by_ref()
                .take(4096)
                .map(PreparedMaterialization::into_row)
                .collect();
            if rows.is_empty() {
                return Ok(());
            }
            self.store.upsert_rows(&rows)?;
        }
    }

    pub fn resolve_move(
        &mut self,
        src: &GatPath,
        dst: &GatPath,
    ) -> Result<(Vec<Entry>, Vec<Entry>), StateStoreError> {
        if self.shape_lock.can_publish_incrementally() {
            let matches = self.store.desired_rows(DesiredQuery::scope(src))?;
            let source_paths: HashSet<&str> =
                matches.iter().map(|entry| entry.path.as_str()).collect();
            let collisions = self
                .store
                .desired_rows(DesiredQuery::scope(dst))?
                .into_iter()
                .filter(|entry| !source_paths.contains(entry.path.as_str()))
                .collect();
            return Ok((matches, collisions));
        }

        let lock = self.store.load_desired_as_lock()?;
        let matches = lock
            .entries
            .iter()
            .filter(|entry| gat_core::lock::path_matches_scope(&entry.path, src))
            .cloned()
            .collect::<Vec<_>>();
        let source_paths: HashSet<&str> = matches.iter().map(|entry| entry.path.as_str()).collect();
        let collisions = lock
            .entries
            .iter()
            .filter(|entry| gat_core::lock::path_matches_scope(&entry.path, dst))
            .filter(|entry| !source_paths.contains(entry.path.as_str()))
            .cloned()
            .collect();
        self.full_lock = Some(lock);
        Ok((matches, collisions))
    }

    pub fn publish_move(
        &mut self,
        src: &GatPath,
        dst: &GatPath,
        collision_paths: &[GatPath],
    ) -> Result<(), DesiredPublicationError> {
        if let Some(lock) = self.full_lock.as_mut() {
            let collisions: HashSet<&str> = collision_paths.iter().map(GatPath::as_str).collect();
            lock.entries = std::mem::take(&mut lock.entries)
                .into_par_iter()
                .filter_map(|mut entry| {
                    if collisions.contains(entry.path.as_str()) {
                        return None;
                    }
                    if gat_core::lock::path_matches_scope(&entry.path, src) {
                        entry.path = entry.path.with_replaced_prefix(src, dst);
                    }
                    Some(entry)
                })
                .collect();
            return self
                .store
                .publish_desired_complete::<DesiredPublicationError>(
                    self.layout,
                    lock,
                    self.target,
                );
        }

        self.store.publish_desired_move::<DesiredPublicationError>(
            self.layout.root_path(),
            &self.shape_lock,
            src,
            dst,
        )
    }

    pub fn resolve_removals(
        &mut self,
        scopes: &[DesiredCandidateScope],
        mut remove: impl FnMut(&GatPath) -> bool,
    ) -> Result<(), StateStoreError> {
        if !self.shape_lock.can_publish_incrementally() {
            let mut lock = self.store.load_desired_as_lock()?;
            lock.entries.retain(|entry| !remove(&entry.path));
            self.full_lock = Some(lock);
            return Ok(());
        }

        let bounds = scopes
            .iter()
            .map(DesiredCandidateScope::as_bound)
            .collect::<Vec<_>>();
        self.store
            .with_desired_rows(DesiredQuery::from_candidate_bounds(bounds), |mut rows| {
                while let Some(row) = rows.next()? {
                    remove(&row.path);
                }
                Ok(())
            })
    }

    pub fn publish_removals(
        &mut self,
        affected_paths: &[GatPath],
        prefixes: &[GatPath],
        include_exact: bool,
    ) -> Result<(), DesiredPublicationError> {
        if let Some(lock) = self.full_lock.as_ref() {
            return self
                .store
                .publish_desired_complete::<DesiredPublicationError>(
                    self.layout,
                    lock,
                    self.target,
                );
        }

        let mut removals = prefixes
            .iter()
            .map(super::desired::DesiredRemoval::Prefix)
            .collect::<Vec<_>>();
        if include_exact {
            removals.push(super::desired::DesiredRemoval::Exact(affected_paths));
        }
        self.store
            .publish_desired_removals::<DesiredPublicationError>(
                self.layout.root_path(),
                &self.shape_lock,
                affected_paths,
                &removals,
            )
    }

    pub fn forget_materialized(&mut self, paths: &[GatPath]) -> Result<(), StateStoreError> {
        self.store.remove_exact(paths)
    }

    pub fn move_materialized(
        &mut self,
        src: &GatPath,
        dst: &GatPath,
    ) -> Result<(), StateStoreError> {
        self.store.move_prefix(src, dst)
    }

    /// Visit desired paths from the cheapest already-available semantic
    /// source. Full-fallback mutations reuse their retained lock; sparse
    /// mutations stream the same retained store. Callers never observe which
    /// persistence shape supplied the rows. Covered descendants are skipped
    /// by indexed seeks in the store or filtered from an already-retained lock.
    pub fn visit_desired_paths<E>(
        &self,
        excluded: &super::DesiredPathExclusions,
        mut visit: impl FnMut(&GatPath) -> Result<(), E>,
    ) -> Result<(), E>
    where
        E: From<StateStoreError>,
    {
        if let Some(lock) = &self.full_lock {
            for entry in &lock.entries {
                if !excluded.contains(&entry.path) {
                    visit(&entry.path)?;
                }
            }
            return Ok(());
        }
        self.store.visit_desired_paths_excluding(excluded, visit)
    }
}

/// Result of replaying one mount transaction's staged desired rows.
///
/// `publication_may_have_landed` is deliberately independent of
/// `imported`: a durable lock-file publication can succeed before the
/// surrounding `SQLite` transaction fails to commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MountReplayResult {
    pub imported: usize,
    pub publication_may_have_landed: bool,
}

/// One opaque, lock-stable mount state session.
///
/// The session owns the refreshed `SQLite` connection and physical shape lock
/// for its whole lifetime. Callers can ask semantic count/occupancy questions
/// and apply bounded subtree deletion or staged-row replay, but cannot observe
/// flat/sharded shape, `SQLite` cursors, publication evidence, or lock paths.
pub struct MountMutationSession<'repo> {
    layout: &'repo RepositoryLayout,
    store: StateStore,
    shape_lock: LockWriteGuard,
    target: LockShardLevels,
}

impl<'repo> MountMutationSession<'repo> {
    pub fn acquire(
        layout: &'repo RepositoryLayout,
        target: LockShardLevels,
    ) -> Result<Self, DesiredMutationOpenError> {
        let shape_lock = LockStore::acquire_current_or_target_shape(layout, target)
            .map_err(DesiredMutationOpenError::Acquire)?;
        let mut store = StateStore::open(layout).map_err(DesiredMutationOpenError::Open)?;
        store
            .refresh_desired_identity(layout)
            .map_err(DesiredMutationOpenError::Refresh)?;
        Ok(Self {
            layout,
            store,
            shape_lock,
            target,
        })
    }

    pub fn desired_count_subtree(&self, path: &GatPath) -> Result<u64, StateStoreError> {
        self.store.desired_count(DesiredQuery::scope(path))
    }

    pub fn desired_any_subtree(&self, path: &GatPath) -> Result<bool, StateStoreError> {
        self.store.desired_any(DesiredQuery::scope(path))
    }

    /// Whether `path` contains any desired row outside `exclude_prefix`.
    ///
    /// The three lexical relationships preserve the existing no-query
    /// shrink, indexed disjoint, and early-stopping expansion paths.
    pub fn has_desired_outside(
        &self,
        path: &GatPath,
        exclude_prefix: Option<&GatPath>,
    ) -> Result<bool, StateStoreError> {
        match exclude_prefix {
            None => self.desired_any_subtree(path),
            Some(exclude) if path.is_or_under(exclude) => Ok(false),
            Some(exclude) if !exclude.is_or_under(path) => self.desired_any_subtree(path),
            Some(exclude) => self.store.with_desired_rows(
                DesiredQuery::scope(path),
                |mut rows| -> Result<bool, StateStoreError> {
                    while let Some(row) = rows.next()? {
                        if !row.path.is_or_under(exclude) {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                },
            ),
        }
    }

    /// Delete every desired row under `target` in bounded windows.
    ///
    /// Derived materialized state is cleared by prefix after desired
    /// publication, including when an earlier recovery attempt already
    /// removed every desired row.
    ///
    /// # Panics
    /// Panics if `window_size` is zero.
    pub fn delete_subtree_windowed(
        &mut self,
        target: &GatPath,
        window_size: usize,
    ) -> Result<usize, DesiredPublicationError> {
        assert!(window_size > 0, "mount deletion window must be non-zero");
        let removed = if self.shape_lock.can_publish_incrementally() {
            let mut total = 0usize;
            loop {
                let window = self.store.with_desired_rows(
                    DesiredQuery::scope(target),
                    |mut rows| -> Result<Vec<GatPath>, StateStoreError> {
                        let mut batch = Vec::new();
                        while batch.len() < window_size {
                            match rows.next()? {
                                Some(row) => batch.push(row.path),
                                None => break,
                            }
                        }
                        Ok(batch)
                    },
                )?;
                if window.is_empty() {
                    break;
                }
                total += window.len();
                self.store
                    .publish_desired_removals::<DesiredPublicationError>(
                        self.layout.root_path(),
                        &self.shape_lock,
                        &window,
                        &[super::desired::DesiredRemoval::Exact(&window)],
                    )?;
                self.store.remove_exact(&window)?;
            }
            total
        } else {
            let mut lock = self.store.load_desired_as_lock()?;
            let before = lock.entries.len();
            lock.entries.retain(|entry| !entry.path.is_or_under(target));
            let removed = before - lock.entries.len();
            if removed != 0 {
                self.store
                    .publish_desired_complete::<DesiredPublicationError>(
                        self.layout,
                        &lock,
                        self.target,
                    )?;
            }
            removed
        };
        self.store.clear_materialized_prefix(target)?;
        Ok(removed)
    }

    /// Replay bounded entry windows using the appropriate private
    /// publication strategy. Flat state is committed through one deferred,
    /// streamed publication; sparse state touches only affected shards.
    pub fn replay_windows<E>(
        &mut self,
        mut next_window: impl FnMut() -> std::result::Result<Option<Vec<Entry>>, E>,
        result: &mut MountReplayResult,
        mut after_window: impl FnMut() -> std::result::Result<(), E>,
        after_flat_publish: impl FnOnce() -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError> + From<crate::atomic::AtomicError>,
    {
        if !self.shape_lock.can_publish_incrementally() {
            let mut all = Vec::new();
            while let Some(window) = next_window()? {
                all.extend(window);
            }
            if all.is_empty() {
                return Ok(());
            }
            result.imported = all.len();
            let mut lock = self.store.load_desired_as_lock().map_err(E::from)?;
            lock.upsert_many(all);
            result.publication_may_have_landed = true;
            self.store
                .publish_desired_complete::<E>(self.layout, &lock, self.target)?;
            after_window()?;
            return Ok(());
        }

        self.store.publish_desired_upsert_windows(
            self.layout.root_path(),
            &self.shape_lock,
            &mut next_window,
            &mut result.imported,
            &mut result.publication_may_have_landed,
            &mut after_window,
            after_flat_publish,
        )?;
        Ok(())
    }

    /// Visit desired paths, seeking past excluded descendants without
    /// exposing their physical representation.
    pub fn visit_desired_paths<E>(
        &self,
        excluded: &super::DesiredPathExclusions,
        visit: impl FnMut(&GatPath) -> Result<(), E>,
    ) -> Result<(), E>
    where
        E: From<StateStoreError>,
    {
        self.store.visit_desired_paths_excluding(excluded, visit)
    }
}

/// Loads the materialized ledger for integration tests without exposing its
/// `SQLite` rows or connection.
#[cfg(any(test, feature = "test-support"))]
pub fn load_materialized_for_test(layout: &RepositoryLayout) -> Result<Lock, StateStoreError> {
    StateStore::open(layout)?.load_all()
}

/// Seeds the materialized ledger for integration tests and benchmark fixtures
/// without exposing its `SQLite` rows, filesystem proofs, or advisory lock.
#[cfg(any(test, feature = "test-support"))]
pub fn record_materialized_for_test(
    layout: &RepositoryLayout,
    entries: &[Entry],
) -> Result<(), DesiredPublicationError> {
    if entries.is_empty() {
        return Ok(());
    }
    let _guard = crate::atomic::RepoLock::acquire(&layout.sync_lock_path())?;
    let rows = entries
        .iter()
        .cloned()
        .map(|entry| {
            let proof = crate::worktree::observe_regular_file(layout.root_path(), &entry.path);
            MaterializedRow::from_entry(entry, proof)
        })
        .collect::<Vec<_>>();
    StateStore::open(layout)?.upsert_rows(&rows)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{self, LockShardId, shard_id_for_path};
    use crate::state::test_support;
    use std::collections::BTreeSet;

    #[derive(Debug, thiserror::Error)]
    enum PublishError {
        #[error(transparent)]
        State(#[from] StateStoreError),
        #[error(transparent)]
        Lock(#[from] LockError),
        #[error(transparent)]
        Atomic(#[from] crate::atomic::AtomicError),
        #[error("injected after filesystem publication")]
        InjectedAfterPublish,
    }

    fn layout() -> (tempfile::TempDir, RepositoryLayout) {
        let temp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(temp.path().to_path_buf());
        (temp, layout)
    }

    fn gp(path: &str) -> GatPath {
        GatPath::parse_canonical(path).unwrap()
    }

    fn oid(byte: char) -> Oid {
        Oid::from_hex(&byte.to_string().repeat(64)).unwrap()
    }

    fn publish_complete(
        layout: &RepositoryLayout,
        lock: &Lock,
        target: LockShardLevels,
    ) -> Result<(), PublishError> {
        let mut store = StateStore::open(layout)?;
        store.publish_desired_complete::<PublishError>(layout, lock, target)
    }

    #[test]
    fn mutation_sessions_share_descendant_pruning_across_sources() {
        for depth in [0, 2] {
            let (_temp, layout) = layout();
            let target = LockShardLevels::new(depth).unwrap();
            let lock = Lock {
                entries: ["data/a", "data/sub/b", "data0/c", "keep"]
                    .into_iter()
                    .map(|path| Entry {
                        path: gp(path),
                        oid: oid('a'),
                    })
                    .collect(),
            };
            publish_complete(&layout, &lock, target).unwrap();
            let excluded = super::super::DesiredPathExclusions::new([gp("data")]);
            {
                let mut session = DesiredMutationSession::acquire(&layout, target).unwrap();
                for retained_lock in [false, true] {
                    if retained_lock {
                        session.full_lock = Some(lock.clone());
                    }
                    let mut visited = Vec::new();
                    session
                        .visit_desired_paths(&excluded, |path| -> Result<(), StateStoreError> {
                            visited.push(path.as_str().to_string());
                            Ok(())
                        })
                        .unwrap();
                    assert_eq!(visited, ["data0/c", "keep"]);
                }
            }
            let session = MountMutationSession::acquire(&layout, target).unwrap();
            let mut visited = Vec::new();
            session
                .visit_desired_paths(&excluded, |path| -> Result<(), StateStoreError> {
                    visited.push(path.as_str().to_string());
                    Ok(())
                })
                .unwrap();
            assert_eq!(visited, ["data0/c", "keep"]);
        }
    }

    #[test]
    fn ignored_directory_summaries_do_not_query_each_desired_subtree() {
        let (temp, layout) = layout();
        test_support_git::run_git(temp.path(), &["init", "-q"]);
        std::fs::write(temp.path().join(".gitignore"), "data/d*/\n").unwrap();
        let entries = (0..10)
            .map(|index| {
                let directory = format!("data/d{index:02}");
                std::fs::create_dir_all(temp.path().join(&directory)).unwrap();
                std::fs::write(
                    temp.path().join(format!("{directory}/kept.txt")),
                    b"tracked",
                )
                .unwrap();
                std::fs::write(temp.path().join(format!("{directory}/new.bin")), b"unseen")
                    .unwrap();
                Entry {
                    path: gp(&format!("{directory}/kept.txt")),
                    oid: oid('a'),
                }
            })
            .collect();
        publish_complete(&layout, &Lock { entries }, LockShardLevels::FLAT).unwrap();
        let session = DesiredStateSession::open(&layout).unwrap();
        let ignore = GatIgnore::load(&layout).unwrap();
        let glob = GatGlobPattern::parse("**/*.bin").unwrap();
        let mut exclusions = Vec::new();
        let before = test_support::snapshot();
        let candidates = session
            .discover_add_candidates(None, Some(&glob), &ignore, false, &mut exclusions)
            .unwrap();
        let after = test_support::snapshot();
        assert!(candidates.is_empty());
        assert_eq!(
            after.0 - before.0,
            1,
            "only the desired overlay should query state"
        );
        assert_eq!(after.1 - before.1, 1);
        let ignored = exclusions
            .iter()
            .find(|item| item.reason == AddExclusionReason::GitIgnore)
            .unwrap();
        assert_eq!(ignored.files, 0, "pruned files must stay unvisited");
        assert_eq!(ignored.directories, 10);
        assert_eq!(
            ignored.samples,
            vec![gp("data/d00"), gp("data/d01"), gp("data/d02")]
        );
    }

    #[test]
    fn root_removal_streams_without_retaining_a_full_lock() {
        for depth in [0, 2] {
            let (_temp, layout) = layout();
            let target = LockShardLevels::new(depth).unwrap();
            let lock = Lock {
                entries: (0..129)
                    .map(|i| Entry {
                        path: gp(&format!("data/{i:03}.bin")),
                        oid: oid('a'),
                    })
                    .collect(),
            };
            publish_complete(&layout, &lock, target).unwrap();
            let mut mutation = DesiredMutationSession::acquire(&layout, target).unwrap();
            let mut paths = Vec::new();
            mutation
                .resolve_removals(&[DesiredCandidateScope::Any], |path| {
                    paths.push(path.clone());
                    true
                })
                .unwrap();
            assert_eq!(paths.len(), 129);
            assert!(mutation.full_lock.is_none());
            mutation.publish_removals(&paths, &[], true).unwrap();
            assert!(
                LockStore::load_all(layout.root_path())
                    .unwrap()
                    .entries
                    .is_empty()
            );
        }
    }

    #[test]
    fn mount_session_deletes_in_windows_and_repeats_idempotently() {
        let (_temp, layout) = layout();
        let target_shape = LockShardLevels::new(0).unwrap();
        let entries = (0..23)
            .map(|index| Entry {
                path: gp(&format!("mounted/file-{index:02}.bin")),
                oid: oid('a'),
            })
            .collect::<Vec<_>>();
        publish_complete(
            &layout,
            &Lock {
                entries: entries.clone(),
            },
            target_shape,
        )
        .unwrap();
        record_materialized_for_test(&layout, &entries).unwrap();

        let mut session = MountMutationSession::acquire(&layout, target_shape).unwrap();
        assert_eq!(session.desired_count_subtree(&gp("mounted")).unwrap(), 23);
        assert_eq!(
            session.delete_subtree_windowed(&gp("mounted"), 5).unwrap(),
            23
        );
        assert_eq!(session.desired_count_subtree(&gp("mounted")).unwrap(), 0);
        assert_eq!(
            session.delete_subtree_windowed(&gp("mounted"), 5).unwrap(),
            0
        );
        assert!(
            load_materialized_for_test(&layout)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn mount_session_clears_stale_materialized_prefix_without_desired_rows() {
        let (_temp, layout) = layout();
        let target_shape = LockShardLevels::new(0).unwrap();
        let entries = vec![Entry {
            path: gp("mounted/stale.bin"),
            oid: oid('a'),
        }];
        record_materialized_for_test(&layout, &entries).unwrap();

        let mut session = MountMutationSession::acquire(&layout, target_shape).unwrap();
        assert_eq!(
            session.delete_subtree_windowed(&gp("mounted"), 4).unwrap(),
            0
        );
        assert!(
            load_materialized_for_test(&layout)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn mount_session_replays_bounded_windows_through_one_flat_publication() {
        let (_temp, layout) = layout();
        let target_shape = LockShardLevels::new(0).unwrap();
        let mut session = MountMutationSession::acquire(&layout, target_shape).unwrap();
        let mut windows = (0..4)
            .map(|window| {
                (0..3)
                    .map(|row| Entry {
                        path: gp(&format!("mounted/{window}-{row}.bin")),
                        oid: oid('b'),
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<std::collections::VecDeque<_>>();
        let mut result = MountReplayResult::default();
        let mut publications = 0usize;
        session
            .replay_windows::<PublishError>(
                || Ok(windows.pop_front()),
                &mut result,
                || {
                    publications += 1;
                    Ok(())
                },
                || Ok(()),
            )
            .unwrap();

        assert!(windows.is_empty());
        assert_eq!(result.imported, 12);
        assert!(result.publication_may_have_landed);
        assert_eq!(publications, 1);
        assert_eq!(session.desired_count_subtree(&gp("mounted")).unwrap(), 12);
    }

    #[test]
    fn mount_session_retains_publication_evidence_when_flat_commit_fails() {
        let (_temp, layout) = layout();
        let target_shape = LockShardLevels::new(0).unwrap();
        let mut session = MountMutationSession::acquire(&layout, target_shape).unwrap();
        let mut windows = Some(vec![Entry {
            path: gp("mounted/published.bin"),
            oid: oid('c'),
        }]);
        let mut result = MountReplayResult::default();
        let outcome = session.replay_windows::<PublishError>(
            || Ok(windows.take()),
            &mut result,
            || Ok(()),
            || Err(PublishError::InjectedAfterPublish),
        );

        assert!(matches!(outcome, Err(PublishError::InjectedAfterPublish)));
        assert_eq!(result.imported, 0);
        assert!(result.publication_may_have_landed);
        assert!(
            LockStore::load_all(layout.root_path())
                .unwrap()
                .entries
                .iter()
                .any(|entry| entry.path == gp("mounted/published.bin"))
        );
    }

    #[test]
    fn complete_publication_holds_the_repository_lock_through_mirror_update() {
        let (_temp, layout) = layout();
        let mut lock = Lock::default();
        lock.upsert_many([Entry {
            path: gp("a.bin"),
            oid: oid('a'),
        }]);

        let (holder_acquired_tx, holder_acquired_rx) = std::sync::mpsc::channel::<()>();
        let (release_holder_tx, release_holder_rx) = std::sync::mpsc::channel::<()>();
        let (publish_done_tx, publish_done_rx) = std::sync::mpsc::channel::<()>();
        let (acquire_attempted_tx, acquire_attempted_rx) = std::sync::mpsc::channel::<()>();

        let layout_ref = &layout;
        let lock_ref = &lock;
        std::thread::scope(|scope| {
            let publisher = scope.spawn(move || {
                holder_acquired_rx.recv().unwrap();
                publish_complete(layout_ref, lock_ref, LockShardLevels::new(0).unwrap()).unwrap();
                publish_done_tx.send(()).unwrap();
            });

            crate::atomic::test_support::with_acquire_attempt_hook(
                publisher.thread().id(),
                acquire_attempted_tx,
                || {
                    let holder = scope.spawn(move || {
                        let guard =
                            crate::atomic::RepoLock::acquire(&layout_ref.sync_lock_path()).unwrap();
                        holder_acquired_tx.send(()).unwrap();
                        release_holder_rx.recv().unwrap();
                        drop(guard);
                    });

                    acquire_attempted_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect("publication must reach the repository lock boundary");
                    assert!(publish_done_rx.try_recv().is_err());
                    release_holder_tx.send(()).unwrap();
                    publish_done_rx.recv().unwrap();
                    holder.join().unwrap();
                },
            );
            publisher.join().unwrap();
        });

        assert_eq!(LockStore::load_all(layout.root_path()).unwrap(), lock);
    }

    #[test]
    fn sparse_upsert_queries_and_touches_only_the_affected_shards() {
        let (_temp, layout) = layout();
        let mut lock = Lock::default();
        for i in 0..200 {
            lock.upsert_many([Entry {
                path: gp(&format!("file-{i:04}.bin")),
                oid: oid('a'),
            }]);
        }
        let target = LockShardLevels::new(4).unwrap();
        LockStore::publish_repository(&layout, &lock, target).unwrap();

        let state = DesiredStateSession::open(&layout).unwrap();
        let touched_paths = ["file-0000.bin", "file-0001.bin"];
        let touched_shards = touched_paths
            .iter()
            .map(|path| shard_id_for_path(&gp(path), target).to_canonical_string())
            .collect::<BTreeSet<_>>();
        let mut untouched_before = Vec::new();
        for entry in std::fs::read_dir(layout.root_path().join("gat.lock")).unwrap() {
            let entry = entry.unwrap();
            let shard_id = format!("gat.lock/{}", entry.file_name().to_string_lossy());
            if !touched_shards.contains(&shard_id) {
                untouched_before
                    .push((entry.path(), entry.metadata().unwrap().modified().unwrap()));
            }
        }
        assert!(untouched_before.len() > 50);

        let entries = touched_paths
            .iter()
            .map(|path| {
                PreparedMaterialization::new(
                    Entry {
                        path: gp(path),
                        oid: oid('b'),
                    },
                    None,
                )
            })
            .collect();
        let before = test_support::shard_identities_call_counts();
        let mut mutation = state.into_mutation(target).unwrap();
        mutation.publish_upserts(entries).unwrap();
        let after = test_support::shard_identities_call_counts();

        assert_eq!(after.0 - before.0, 1);
        // Revision validation reuses the captured catalog, never unrelated shard rows.
        assert_eq!(after.1 - before.1, 0);
        for (path, modified) in untouched_before {
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                modified
            );
        }
    }

    #[test]
    fn flat_complete_publication_renders_hashes_and_publishes_once() {
        let (_temp, layout) = layout();
        let mut lock = Lock::default();
        for i in 0..16 {
            lock.upsert_many([Entry {
                path: gp(&format!("file-{i}.bin")),
                oid: oid('a'),
            }]);
        }
        let target = LockShardLevels::new(0).unwrap();
        LockStore::publish_repository(&layout, &lock, target).unwrap();

        let render_before = lock::test_support::render_entries_calls();
        let hash_before = lock::identity_test_support::hash_shard_bytes_call_count();
        let (scoped_before, full_before) = test_support::shard_identities_call_counts();
        publish_complete(&layout, &lock, target).unwrap();

        assert_eq!(
            lock::test_support::render_entries_calls() - render_before,
            1
        );
        assert_eq!(
            lock::identity_test_support::hash_shard_bytes_call_count() - hash_before,
            1
        );
        let (scoped_after, full_after) = test_support::shard_identities_call_counts();
        assert_eq!(scoped_after - scoped_before, 0);
        assert_eq!(full_after - full_before, 1);
    }

    #[test]
    fn sharded_complete_publication_buckets_renders_and_hashes_once() {
        let (_temp, layout) = layout();
        let mut lock = Lock::default();
        for i in 0..40 {
            lock.upsert_many([Entry {
                path: gp(&format!("file-{i:04}.bin")),
                oid: oid('a'),
            }]);
        }
        let target = LockShardLevels::new(2).unwrap();
        LockStore::publish_repository(&layout, &lock, target).unwrap();
        let bucket_count = (0..40)
            .map(|i| shard_id_for_path(&gp(&format!("file-{i:04}.bin")), target))
            .collect::<std::collections::HashSet<LockShardId>>()
            .len();
        assert!(bucket_count > 1);

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let (render_delta, hash_delta, scoped_delta, full_delta) = pool.install(|| {
            let render_before = lock::test_support::render_entries_calls();
            let hash_before = lock::identity_test_support::hash_shard_bytes_call_count();
            let (scoped_before, full_before) = test_support::shard_identities_call_counts();
            publish_complete(&layout, &lock, target).unwrap();
            let (scoped_after, full_after) = test_support::shard_identities_call_counts();
            (
                lock::test_support::render_entries_calls() - render_before,
                lock::identity_test_support::hash_shard_bytes_call_count() - hash_before,
                scoped_after - scoped_before,
                full_after - full_before,
            )
        });

        assert_eq!(render_delta, bucket_count);
        assert_eq!(hash_delta, bucket_count);
        assert_eq!(scoped_delta, 0);
        assert_eq!(full_delta, 1);
    }

    #[test]
    fn recording_published_materialization_preserves_an_absent_proof() {
        let (_temp, layout) = layout();
        let target = LockShardLevels::new(0).unwrap();
        let state = DesiredStateSession::open(&layout).unwrap();
        let mut mutation = state.into_mutation(target).unwrap();
        mutation
            .publish_upserts(vec![PreparedMaterialization::new(
                Entry {
                    path: gp("a.bin"),
                    oid: oid('a'),
                },
                None,
            )])
            .unwrap();
        mutation.record_published_materialized().unwrap();
        drop(mutation);

        let rows = StateStore::open(&layout)
            .unwrap()
            .materialized_rows_for(&[gp("a.bin")])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].proof, None);
    }

    #[test]
    fn prepared_materialization_debug_does_not_expose_proof_state() {
        let prepared = PreparedMaterialization::new(
            Entry {
                path: gp("a.bin"),
                oid: oid('a'),
            },
            None,
        );

        let debug = format!("{prepared:?}");
        assert!(debug.contains("a.bin"));
        assert!(!debug.contains("proof"));
        assert!(!debug.contains("None"));
    }
}

#[cfg(test)]
mod add_snapshot_tests {
    use super::*;

    #[test]
    fn multiple_add_windows_publish_one_large_shard_once() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        pool.install(|| {
            let tmp = tempfile::tempdir().unwrap();
            let layout = RepositoryLayout::at(tmp.path().to_path_buf());
            let target = LockShardLevels::new(1).unwrap();
            let seed = GatPath::parse_canonical("seed.bin").unwrap();
            let shard = crate::lock::shard_id_for_path(&seed, target);
            let lock = Lock {
                entries: vec![Entry {
                    path: seed,
                    oid: Oid::from_bytes([1; 32]),
                }],
            };
            LockStore::publish_repository(&layout, &lock, target).unwrap();
            // One full 4096-row publication window plus a partial second
            // window must still render the shared shard only once.
            let entries: Vec<_> = (0..)
                .map(|index| format!("data/{index}.bin"))
                // Generated ASCII paths are canonical by construction. Avoid
                // parsing the roughly 255 rejected candidates per retained row.
                .filter(|path| gat_core::lock::validated::shard_id_for_path(path, target) == shard)
                .take(4097)
                .map(|path| {
                    let path = GatPath::parse_canonical(&path).unwrap();
                    PreparedMaterialization::new(
                        Entry {
                            path,
                            oid: Oid::from_bytes([2; 32]),
                        },
                        None,
                    )
                })
                .collect();
            let session = DesiredStateSession::open(&layout).unwrap();
            let mut mutation = session.into_mutation(target).unwrap();
            let before = crate::lock::test_support::render_entries_calls();
            mutation.publish_upserts(entries).unwrap();
            assert_eq!(
                crate::lock::test_support::render_entries_calls() - before,
                1
            );
            mutation.record_published_materialized().unwrap();
            assert_eq!(LockStore::load_all(tmp.path()).unwrap().entries.len(), 4098);
        });
    }

    #[test]
    fn add_rejects_a_stale_preparation_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = RepositoryLayout::at(tmp.path().to_path_buf());
        let session = DesiredStateSession::open(&layout).unwrap();
        let mut changed = StateStore::open(&layout).unwrap();
        let lock = Lock {
            entries: vec![Entry {
                path: GatPath::parse_canonical("other.bin").unwrap(),
                oid: Oid::from_bytes([7; 32]),
            }],
        };
        changed
            .publish_desired_complete::<DesiredPublicationError>(
                &layout,
                &lock,
                LockShardLevels::FLAT,
            )
            .unwrap();
        assert!(matches!(
            session.into_mutation(LockShardLevels::FLAT),
            Err(DesiredMutationOpenError::Stale)
        ));
        assert_eq!(LockStore::load_all(tmp.path()).unwrap(), lock);
    }
}
