use super::desired::{apply_shard_entries_tx, clear_shard_desired_tx};
use super::{
    CanonicalShardIdBuf, Connection, Entry, Oid, Result, StateResultExt, StateStore,
    StateStoreError, decode_shard_id, descendant_range, params_from_iter, sql_chunk_size,
    sql_placeholders,
};
use crate::atomic::{AtomicError, RepoLock};
use crate::file_state::StatProof;
use crate::lock::{
    LockError, LockShardId, LockShardLevels, LockStore, ShardObservationChange, shard_id_for_path,
    shard_levels_from_id,
};
use gat_core::lock::{CanonicalDesiredIdentity, LockDomainError};
#[cfg(any(test, feature = "test-support"))]
use rusqlite::OptionalExtension;

/// Failures while coherently observing the live desired lock and applying
/// its incremental mirror refresh.
#[derive(Debug, thiserror::Error)]
pub enum DesiredRefreshError {
    #[error(transparent)]
    Atomic(#[from] AtomicError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    State(#[from] StateStoreError),
}

/// Semantic result of refreshing the live desired lock into local state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesiredRefresh {
    identity: CanonicalDesiredIdentity,
    shard_levels: LockShardLevels,
}

impl DesiredRefresh {
    #[must_use]
    pub const fn identity(&self) -> CanonicalDesiredIdentity {
        self.identity
    }

    #[must_use]
    pub const fn shard_levels(&self) -> LockShardLevels {
        self.shard_levels
    }
}

/// `(shard_id, prior_identity, new_identity, proof, entries)`: a shard
/// whose content actually changed (or is being seen for the first time,
/// in which case `prior_identity` is `None`) since the last refresh. The
/// prior identity is carried alongside the new one so
/// [`apply_shard_catalog_tx`] can maintain the persisted
/// [`crate::lock::CanonicalDesiredIdentity`] incrementally --
/// `XORing` the old contribution out and the new one in -- instead of
/// having to reread and re-derive it from the whole `lock_shards`
/// catalog. `proof` is the shared, optional
/// [`crate::file_state::StatProof`] this identity may be reused from on
/// a later refresh.
pub(crate) struct ChangedShard {
    pub shard_id: LockShardId,
    pub prior_identity: Option<ShardIdentity>,
    pub identity: ShardIdentity,
    pub proof: Option<StatProof>,
    pub entries: Vec<Entry>,
}
/// The identity/proof-only half of `ChangedShard`, without the row
/// payload -- what [`apply_shard_catalog_tx`] actually consumes. Sparse
/// mutation publication (`record_published_shards`) builds this directly
/// instead of a full `ChangedShard`, since it never has (and doesn't
/// need) a `Vec<Entry>` to attach: the desired rows were already written
/// to `state` before publication ran.
pub(crate) struct ChangedShardMeta {
    pub shard_id: LockShardId,
    pub prior_identity: Option<ShardIdentity>,
    pub identity: ShardIdentity,
    pub proof: Option<StatProof>,
}

impl From<&ChangedShard> for ChangedShardMeta {
    fn from(shard: &ChangedShard) -> Self {
        Self {
            shard_id: shard.shard_id,
            prior_identity: shard.prior_identity,
            identity: shard.identity,
            proof: shard.proof,
        }
    }
}
/// `(shard_id, prior_identity)`: a shard absent from the new catalog. Its
/// identity is always its prior one (there is no "new" identity for a
/// removal) -- carried so [`apply_shard_catalog_tx`] can XOR its
/// contribution back out of the persisted whole-lock identity.
pub(crate) struct RemovedShard {
    pub shard_id: LockShardId,
    pub prior_identity: ShardIdentity,
}

/// Below this many changed paths, [`StateStore::find_directory_conflict`]
/// uses [`StateStore::find_directory_conflict_sparse`]'s indexed
/// lookup (cost proportional to the changed scope) instead of
/// [`StateStore::find_directory_conflict_bulk`]'s single
/// full-mirror merge-walk. This is an algorithmic crossover threshold,
/// not a `SQLite` bind-variable transport budget, so it is chosen and kept
/// independent of [`sql_chunk_size`]/`SQL_BIND_BUDGET`: below it, the
/// sparse path's per-path descendant lookups plus one batch of chunked
/// ancestor lookups stay cheap relative to the changed scope, and the
/// bulk path's single-statement-regardless-of-size advantage only pays
/// for itself once the changed set is at least this large.
const SPARSE_DIRECTORY_CONFLICT_PATHS: usize = 500;

/// Shared row-decoding for `lock_shards` catalog reads, run either over
/// the whole table (`shard_ids: None`, [`StateStore::all_shard_identities`])
/// or scoped to an exact, chunked set of shard ids (`Some`, used by sparse
/// mutation publication so a touched-shard-only lookup never pays for the
/// full catalog). Takes `&Connection` rather than `&StateStore` so it
/// also works against an open [`DesiredStateWrite`]'s transaction, which
/// derefs to the same connection.
fn shard_identities_query(
    conn: &Connection,
    shard_ids: Option<&std::collections::BTreeSet<LockShardId>>,
) -> Result<std::collections::HashMap<LockShardId, StoredShard>> {
    type ShardIdentityRow = (String, Vec<u8>, Option<Vec<u8>>);
    let mut out = std::collections::HashMap::new();
    // Instrumentation distinguishes a touched-shard-scoped
    // catalog fetch from a whole-catalog one, so a test can prove a
    // sparse mutation's single prior-lookup call site never falls back
    // to (or duplicates into) the unscoped whole-catalog path.
    #[cfg(any(test, feature = "test-support"))]
    match shard_ids {
        Some(_) => super::query::test_support::record_scoped_shard_identities_call(),
        None => super::query::test_support::record_full_shard_identities_call(),
    }
    // A free `fn` (not a closure) so it can stay generic over `params`'s
    // exact `Params` implementor: the unscoped branch binds no
    // parameters at all (`()`), while the scoped branch binds a chunk of
    // borrowed canonical shard-id strings (`ParamsFromIter`) -- neither
    // needs a `Vec<&dyn ToSql>` allocation or a `CanonicalShardIdBuf: ToSql`
    // impl to reach `rusqlite`.
    fn run(
        conn: &Connection,
        out: &mut std::collections::HashMap<LockShardId, StoredShard>,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<()> {
        // Instrumentation lets tests prove
        // a scoped, touched-shard-only lookup (`shard_ids: Some(..)`,
        // used by sparse mutation publication) never falls back to
        // preparing as many statements as an unscoped full-catalog scan
        // over the same total shard count would need.
        #[cfg(any(test, feature = "test-support"))]
        super::query::test_support::record_sql_statement_prepared();
        let mut stmt = conn
            .prepare(sql)
            .state_context("preparing shard catalog query")?;
        let rows: Vec<ShardIdentityRow> = stmt
            .query_map(params, |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .state_context("querying shard catalog")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .state_context("reading shard catalog")?;
        for (shard_id, identity, proof) in rows {
            let shard_id = decode_shard_id(&shard_id, "stored shard id")?;
            let identity =
                ShardIdentity::decode(&identity).map_err(|source| StateStoreError::InvalidRow {
                    detail: format!(
                        "decoding stored shard identity for shard {shard_id:?}: {source}"
                    ),
                })?;
            // A malformed/unrecognized encoded proof is never trusted as
            // state -- it decodes to `None`, exactly like a `NULL` row,
            // which just forces the next refresh to re-establish the
            // identity from bytes instead of reusing a stat-only match.
            let proof = proof
                .as_deref()
                .and_then(crate::file_state::decode_stat_proof);
            out.insert(shard_id, StoredShard { identity, proof });
        }
        Ok(())
    }
    const BASE_SQL: &str = "SELECT shard_id, identity, proof FROM lock_shards";
    match shard_ids {
        None => run(conn, &mut out, BASE_SQL, ())?,
        Some(ids) => {
            // Never materializes a textual collection proportional to the
            // complete requested shard-ID set: `bufs` is a small, reused
            // buffer filled directly from the caller's `BTreeSet`
            // iterator, one bounded chunk at a time, without first
            // collecting the whole set into a `Vec<&LockShardId>`.
            let chunk_size = sql_chunk_size(1);
            let mut bufs: Vec<CanonicalShardIdBuf> = Vec::with_capacity(chunk_size);
            let mut ids = ids.iter();
            loop {
                bufs.clear();
                bufs.extend(
                    ids.by_ref()
                        .take(chunk_size)
                        .copied()
                        .map(CanonicalShardIdBuf::encode),
                );
                if bufs.is_empty() {
                    break;
                }
                let placeholders = sql_placeholders("?", bufs.len());
                let sql = format!("{BASE_SQL} WHERE shard_id IN ({placeholders})");
                run(
                    conn,
                    &mut out,
                    &sql,
                    params_from_iter(bufs.iter().map(CanonicalShardIdBuf::as_str)),
                )?;
            }
        }
    }
    Ok(out)
}

/// The exact-shard-id-scoped counterpart of [`StateStore::all_shard_identities`],
/// run against an open [`DesiredStateWrite`] transaction so sparse mutation
/// publication (`gat add`/`rm`/`mv`) can look up only the shards it already
/// knows it touched, instead of first materializing the entire `lock_shards`
/// catalog (proportional to the total shard count `Q`).
pub(crate) fn shard_identities_for_tx(
    tx: &rusqlite::Transaction<'_>,
    shard_ids: &std::collections::BTreeSet<LockShardId>,
) -> Result<std::collections::HashMap<LockShardId, StoredShard>> {
    shard_identities_query(tx, Some(shard_ids))
}

/// The proper ancestor directory paths of `path` in root-to-leaf order,
/// e.g. `"a"`, `"a/b"` for `"a/b/c"` -- excludes `path` itself. Used for
/// look up whether any of `path`'s ancestors is itself already a tracked
/// file elsewhere (the "nested under an already-tracked ancestor" half
/// of the directory-conflict invariant).
fn path_ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(move |(i, _)| &path[..i])
}

/// `.git/info/exclude`'s currently recorded generating-input fingerprint,
/// managed-block content identity, path count, and optional shared
/// `StatProof`, as read back from `reconciliation_meta` in one query --
/// see [`StateStore::exclude_record`]. Kept as one struct (rather
/// than four independent accessor methods) so
/// `gat-engine`'s excludes stable-observation coordinator can decide
/// proof-hit/proof-miss/rebuild from a single, internally-consistent
/// snapshot instead of four separate reads that could race against a
/// concurrent write.
#[derive(Debug, Clone, Default)]
pub struct ExcludeRecord {
    /// `H(CanonicalDesiredIdentity, git.ignore_patterns)` at the time the
    /// managed block was last regenerated -- answers "what Gat should
    /// render." `None` before the block has ever been generated.
    fingerprint: Option<[u8; 32]>,
    /// How many paths the managed block was last regenerated with.
    count: usize,
    /// `BLAKE3` of exactly the gat-managed block's own bytes (never the
    /// whole file) -- answers "what Gat last verified it rendered."
    /// `None` before the block has ever been generated.
    block_identity: Option<[u8; 32]>,
    /// The shared, optional `StatProof` answering "can metadata prove
    /// that verified output is still present without rereading it."
    /// `None` means the block identity above is known but must be
    /// re-established from bytes before any stat-only reuse.
    proof: Option<StatProof>,
}

impl ExcludeRecord {
    pub const fn fingerprint(&self) -> Option<[u8; 32]> {
        self.fingerprint
    }

    pub const fn count(&self) -> usize {
        self.count
    }

    pub const fn block_identity(&self) -> Option<[u8; 32]> {
        self.block_identity
    }

    pub fn verify_info_exclude(
        &self,
        layout: &crate::RepositoryLayout,
        begin: &str,
        end: &str,
    ) -> std::result::Result<crate::git::InfoExcludeVerification, crate::git::InfoExcludeError>
    {
        crate::git::verify_info_exclude(layout, self, begin, end)
    }

    pub(crate) const fn proof(&self) -> Option<StatProof> {
        self.proof
    }

    #[cfg(any(test, feature = "test-support"))]
    pub const fn has_reusable_proof(&self) -> bool {
        self.proof.is_some()
    }
}

impl StateStore {
    // -- Desired-state mirror / dirty index (reconciliation) --
    //
    // Everything below is the *reconciliation* half of incremental sync
    // planning (see `gat-engine` desired-state indexing): it tracks
    // which paths' desired state (mirrored here from `gat.lock`) disagrees
    // with materialized state, using a persisted stat-cache accelerator
    // for deciding whether a shard needs reparsing. It never inspects the
    // working-tree file content itself -- that remains `Validation`'s job
    // in `sync::plan`, kept deliberately separate from reconciliation.

    /// Refresh the desired-state mirror from the live canonical lock.
    ///
    /// The repository lock covers one prior-catalog read, one live shard
    /// observation, invariant validation, and the conditional `SQLite`
    /// update. The returned identity is read from the incrementally
    /// maintained catalog, avoiding a second shard enumeration/stat/hash
    /// pass after the refresh.
    pub fn refresh_desired_identity(
        &mut self,
        layout: &crate::RepositoryLayout,
    ) -> std::result::Result<CanonicalDesiredIdentity, DesiredRefreshError> {
        Ok(self.refresh_desired_state(layout)?.identity)
    }

    /// Refresh the desired-state mirror and return the identity and topology
    /// established by that one live-lock observation.
    pub fn refresh_desired_state(
        &mut self,
        layout: &crate::RepositoryLayout,
    ) -> std::result::Result<DesiredRefresh, DesiredRefreshError> {
        self.refresh_desired_state_inner(layout, false, None)
    }

    /// Refresh the desired-state mirror and pin the resulting `SQLite` read
    /// snapshot before releasing repository synchronization authority.
    pub fn refresh_and_pin_desired_state(
        &mut self,
        layout: &crate::RepositoryLayout,
    ) -> std::result::Result<DesiredRefresh, DesiredRefreshError> {
        self.refresh_desired_state_inner(layout, true, None)
    }

    pub(crate) fn refresh_and_pin_with_catalog(
        &mut self,
        layout: &crate::RepositoryLayout,
        catalog: &mut std::collections::HashMap<LockShardId, StoredShard>,
    ) -> std::result::Result<DesiredRefresh, DesiredRefreshError> {
        self.refresh_desired_state_inner(layout, true, Some(catalog))
    }

    fn refresh_desired_state_inner(
        &mut self,
        layout: &crate::RepositoryLayout,
        pin_snapshot: bool,
        catalog_out: Option<&mut std::collections::HashMap<LockShardId, StoredShard>>,
    ) -> std::result::Result<DesiredRefresh, DesiredRefreshError> {
        let _guard = RepoLock::acquire_repository(layout)?;
        let mut catalog = self.all_shard_identities()?;
        let priors = catalog
            .iter()
            .map(|(shard_id, stored)| (*shard_id, (stored.identity, stored.proof)))
            .collect();
        let shard_results = LockStore::observe_shards(layout.root_path(), &priors)?;
        let current_ids: std::collections::HashSet<LockShardId> =
            shard_results.iter().map(|shard| shard.shard_id).collect();
        let shard_levels = crate::lock::shard_levels_for_ids(current_ids.iter().copied())?;

        let mut changed_or_new = Vec::new();
        let mut stat_only_updates = Vec::new();
        for shard in shard_results {
            match shard.change {
                ShardObservationChange::Unchanged => {}
                ShardObservationChange::StatOnly(stat) => {
                    stat_only_updates.push((shard.shard_id, Some(stat)));
                }
                ShardObservationChange::Content {
                    prior_identity,
                    identity,
                    proof,
                    entries,
                } => {
                    changed_or_new.push(ChangedShard {
                        shard_id: shard.shard_id,
                        prior_identity,
                        identity,
                        proof: Some(proof),
                        entries,
                    });
                }
            }
        }

        let removed_ids: Vec<RemovedShard> = catalog
            .iter()
            .filter(|(id, _)| !current_ids.contains(id))
            .map(|(id, stored)| RemovedShard {
                shard_id: *id,
                prior_identity: stored.identity,
            })
            .collect();

        self.check_sharded_lock_invariants(&changed_or_new, &removed_ids)?;
        self.apply_shard_refresh(&changed_or_new, &stat_only_updates, &removed_ids)?;

        let identity = CanonicalDesiredIdentity::from_bytes(self.desired_fingerprint()?);
        if pin_snapshot {
            self.pin_snapshot()?;
        }
        if let Some(catalog_out) = catalog_out {
            for removed in removed_ids {
                catalog.remove(&removed.shard_id);
            }
            for (id, proof) in stat_only_updates {
                if let Some(stored) = catalog.get_mut(&id) {
                    stored.proof = proof;
                }
            }
            for changed in changed_or_new {
                catalog.insert(
                    changed.shard_id,
                    StoredShard {
                        identity: changed.identity,
                        proof: changed.proof,
                    },
                );
            }
            *catalog_out = catalog;
        }
        Ok(DesiredRefresh {
            identity,
            shard_levels,
        })
    }

    /// The entire shard catalog, keyed by `shard_id`, in one query --
    /// used by [`Self::refresh_desired_identity`] instead of a per-shard lookup,
    /// so leaf enumeration over a large shard set costs one `SQLite`
    /// round trip rather than one per shard.
    pub(crate) fn all_shard_identities(
        &self,
    ) -> Result<std::collections::HashMap<LockShardId, StoredShard>> {
        shard_identities_query(&self.conn, None)
    }

    /// Validate changed/new shards against each other and the unchanged rows
    /// already indexed in `store`, preserving full-lock placement, ownership,
    /// and directory-prefix invariants without loading the whole lock.
    fn check_sharded_lock_invariants(
        &self,
        changed_or_new: &[ChangedShard],
        removed_ids: &[RemovedShard],
    ) -> std::result::Result<(), DesiredRefreshError> {
        if changed_or_new.is_empty() {
            return Ok(());
        }

        for shard in changed_or_new {
            let levels = shard_levels_from_id(&shard.shard_id);
            for entry in &shard.entries {
                let expected_shard_id = shard_id_for_path(&entry.path, levels);
                if expected_shard_id != shard.shard_id {
                    return Err(LockError::from(LockDomainError::MisplacedShardRow {
                        path: entry.path.to_string(),
                        actual: shard.shard_id.to_canonical_string(),
                        expected: expected_shard_id.to_canonical_string(),
                    })
                    .into());
                }
            }
        }

        let mut owner: std::collections::HashMap<&str, LockShardId> =
            std::collections::HashMap::new();
        for shard in changed_or_new {
            for entry in &shard.entries {
                if let Some(&other) = owner.get(entry.path.as_str())
                    && other != shard.shard_id
                {
                    return Err(LockError::from(LockDomainError::PathInMultipleShards {
                        path: entry.path.to_string(),
                    })
                    .into());
                }
                owner.insert(entry.path.as_str(), shard.shard_id);
            }
        }

        let same_refresh_paths: std::collections::BTreeSet<&str> = changed_or_new
            .iter()
            .flat_map(|shard| shard.entries.iter().map(|entry| entry.path.as_str()))
            .collect();
        crate::lock::validate_no_path_directory_conflicts(&same_refresh_paths)
            .map_err(crate::lock::LockError::from)?;

        let mut exclude: std::collections::HashSet<LockShardId> =
            changed_or_new.iter().map(|shard| shard.shard_id).collect();
        exclude.extend(removed_ids.iter().map(|removed| removed.shard_id));
        let all_paths: Vec<gat_core::lexical_path::GatPath> = changed_or_new
            .iter()
            .flat_map(|shard| shard.entries.iter().map(|entry| entry.path.clone()))
            .collect();

        if let Some((path, _other_shard_id)) = self
            .find_conflicting_shard_owners(&all_paths, &exclude)?
            .into_iter()
            .next()
        {
            return Err(LockError::from(LockDomainError::PathInMultipleShards {
                path: path.to_string(),
            })
            .into());
        }

        if let Some((file_path, descendant_path)) =
            self.find_directory_conflict(&all_paths, &exclude)?
        {
            return Err(LockError::from(LockDomainError::DirectoryPrefixConflict {
                ancestor: file_path.to_string(),
                descendant: descendant_path.to_string(),
            })
            .into());
        }
        Ok(())
    }

    /// One shard's currently-stored identity/stat, or `None` if it isn't
    /// (yet) in the catalog -- the single-shard-scoped counterpart of
    /// `Self::all_shard_identities`. Only production callers now go
    /// through the whole-catalog `Self::all_shard_identities` fetch
    /// (used both to accelerate a full-lock publish and to compute the
    /// removed-shard diff in one pass); this single-shard lookup is kept
    /// as a test-only convenience for asserting one shard's identity
    /// without materializing the whole catalog.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    pub(crate) fn shard_identity(&self, shard_id: LockShardId) -> Result<Option<StoredShard>> {
        let ids = std::collections::BTreeSet::from([shard_id]);
        Ok(shard_identities_query(&self.conn, Some(&ids))?.remove(&shard_id))
    }

    /// Among `paths`, every one already recorded in `state` under a
    /// `desired_shard_id` that isn't in `exclude_shard_ids`, paired with
    /// that other shard's id -- used by [`Self::refresh_desired_identity`] to
    /// detect a path claimed by two different live shards (a global
    /// lock invariant a full `LockStore::load_all` also enforces, just within
    /// one in-memory merge instead of across independently reparsed
    /// shards). `exclude_shard_ids` is the set of shards this refresh
    /// cycle is itself about to overwrite or remove, since their
    /// currently-recorded ownership is stale and about to be replaced.
    pub fn find_conflicting_shard_owners(
        &self,
        paths: &[gat_core::lexical_path::GatPath],
        exclude_shard_ids: &std::collections::HashSet<LockShardId>,
    ) -> Result<Vec<(gat_core::lexical_path::GatPath, LockShardId)>> {
        let mut conflicts = Vec::new();
        for chunk in paths.chunks(sql_chunk_size(1)) {
            let placeholders = sql_placeholders("?", chunk.len());
            let sql = format!(
                "SELECT path, desired_shard_id FROM state
                 WHERE path IN ({placeholders}) AND desired_shard_id IS NOT NULL"
            );
            let mut stmt = self
                .conn
                .prepare(&sql)
                .state_context("preparing cross-shard duplicate check")?;
            let rows = stmt
                .query_map(
                    params_from_iter(chunk.iter().map(gat_core::lexical_path::GatPath::as_str)),
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .state_context("querying cross-shard duplicate check")?;
            for row in rows {
                let (path, shard_id) =
                    row.state_context("reading cross-shard duplicate check row")?;
                let path = super::decode_path(path, "cross-shard duplicate check row")?;
                let shard_id = decode_shard_id(&shard_id, "desired_shard_id")?;
                if !exclude_shard_ids.contains(&shard_id) {
                    conflicts.push((path, shard_id));
                }
            }
        }
        Ok(conflicts)
    }

    /// The first directory-prefix conflict between `paths` (this refresh
    /// cycle's changed/new rows) and whatever `state` already has
    /// recorded under a `desired_shard_id` outside `exclude_shard_ids` --
    /// the cross-shard half of the same "a real tree cannot have a
    /// tracked file *and* tracked descendants under it" invariant
    /// `LockStore::load_all`'s directory-conflict validation
    /// enforces over a full in-memory load, checked here against
    /// `SQLite`'s already-indexed rows instead of reparsing every other
    /// shard. Checks both directions: a `path` that already has a
    /// tracked descendant elsewhere, and a `path` that is itself nested
    /// under an already-tracked ancestor elsewhere. Returns
    /// `(tracked-as-file path, tracked-as-descendant path)` for the first
    /// conflict found, or `None`.
    ///
    /// Below `SPARSE_DIRECTORY_CONFLICT_PATHS` changed paths, delegates
    /// to `Self::find_directory_conflict_sparse` -- an indexed lookup
    /// proportional to the changed scope, so an ordinary small
    /// mutation/refresh against a huge desired mirror never has to
    /// stream every existing row just to prove there's no conflict.
    /// Above that threshold, delegates to
    /// `Self::find_directory_conflict_bulk`'s single ordered
    /// merge-walk, which stays cheaper than one query per changed path
    /// once the changed set is itself large (as in a big reshape).
    pub fn find_directory_conflict(
        &self,
        paths: &[gat_core::lexical_path::GatPath],
        exclude_shard_ids: &std::collections::HashSet<LockShardId>,
    ) -> Result<
        Option<(
            gat_core::lexical_path::GatPath,
            gat_core::lexical_path::GatPath,
        )>,
    > {
        if paths.is_empty() {
            return Ok(None);
        }
        let mut sorted_paths: Vec<&gat_core::lexical_path::GatPath> = paths.iter().collect();
        sorted_paths.sort_unstable_by_key(|path| path.as_str());
        sorted_paths.dedup_by(|a, b| a.as_str() == b.as_str());

        if sorted_paths.len() <= SPARSE_DIRECTORY_CONFLICT_PATHS {
            self.find_directory_conflict_sparse(&sorted_paths, exclude_shard_ids)
        } else {
            self.find_directory_conflict_bulk(&sorted_paths, exclude_shard_ids)
        }
    }

    /// Indexed-lookup counterpart to `Self::find_directory_conflict_bulk`
    /// used for a small changed-path set (see
    /// `SPARSE_DIRECTORY_CONFLICT_PATHS`): one descendant-range query
    /// per changed path (proportional to what's actually nested under
    /// it, not to the total desired-state size) to catch a changed path
    /// that already has tracked descendants elsewhere, plus one batch of
    /// deduplicated, chunked (`sql_chunk_size`) exact ancestor-path lookups to
    /// catch a changed path nested under an already-tracked ancestor.
    /// Exclusion is applied in Rust against each returned row, so this
    /// never binds `exclude_shard_ids` into SQL. Total `SQLite` work here
    /// scales with the changed scope, never with total rows in `state`.
    fn find_directory_conflict_sparse(
        &self,
        sorted_paths: &[&gat_core::lexical_path::GatPath],
        exclude_shard_ids: &std::collections::HashSet<LockShardId>,
    ) -> Result<
        Option<(
            gat_core::lexical_path::GatPath,
            gat_core::lexical_path::GatPath,
        )>,
    > {
        for &path in sorted_paths {
            let (lower, upper) = descendant_range(path.as_str());
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT path, desired_shard_id FROM state
                     WHERE desired_oid IS NOT NULL AND path >= ?1 AND path < ?2
                     ORDER BY path",
                )
                .state_context("preparing sparse directory-conflict descendant query")?;
            #[cfg(any(test, feature = "test-support"))]
            super::query::test_support::record_sql_statement_prepared();
            let mut rows = stmt
                .query((&lower, &upper))
                .state_context("querying sparse directory-conflict descendants")?;
            while let Some(row) = rows
                .next()
                .state_context("reading sparse directory-conflict descendant row")?
            {
                let existing_path: String = row.get(0)?;
                let shard_id: Option<String> = row.get(1)?;
                let shard_id = shard_id
                    .map(|id| decode_shard_id(&id, "desired_shard_id"))
                    .transpose()?;
                #[cfg(any(test, feature = "test-support"))]
                super::query::test_support::record_existing_row_visited();
                if shard_id.is_some_and(|id| exclude_shard_ids.contains(&id)) {
                    continue;
                }
                let existing_path =
                    super::decode_path(existing_path, "sparse directory-conflict descendant row")?;
                return Ok(Some((path.clone(), existing_path)));
            }
        }

        // Deduplicated proper ancestors of every changed path (not the
        // path itself), mapped back to one changed path nested under
        // each -- used both to build the `IN (...)` lookup and to
        // report which changed path a hit conflicts with.
        let mut ancestor_to_descendant: std::collections::BTreeMap<
            String,
            &gat_core::lexical_path::GatPath,
        > = std::collections::BTreeMap::new();
        for &path in sorted_paths {
            for ancestor in path_ancestors(path.as_str()) {
                ancestor_to_descendant
                    .entry(ancestor.to_string())
                    .or_insert(path);
            }
        }
        let ancestor_paths: Vec<String> = ancestor_to_descendant.keys().cloned().collect();
        for chunk in ancestor_paths.chunks(sql_chunk_size(1)) {
            let placeholders = sql_placeholders("?", chunk.len());
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT path, desired_shard_id FROM state
                     WHERE desired_oid IS NOT NULL AND path IN ({placeholders})"
                ))
                .state_context("preparing sparse directory-conflict ancestor query")?;
            #[cfg(any(test, feature = "test-support"))]
            super::query::test_support::record_sql_statement_prepared();
            let mut rows = stmt
                .query(params_from_iter(chunk.iter()))
                .state_context("querying sparse directory-conflict ancestors")?;
            while let Some(row) = rows
                .next()
                .state_context("reading sparse directory-conflict ancestor row")?
            {
                let existing_path: String = row.get(0)?;
                let shard_id: Option<String> = row.get(1)?;
                let shard_id = shard_id
                    .map(|id| decode_shard_id(&id, "desired_shard_id"))
                    .transpose()?;
                #[cfg(any(test, feature = "test-support"))]
                super::query::test_support::record_existing_row_visited();
                if shard_id.is_some_and(|id| exclude_shard_ids.contains(&id)) {
                    continue;
                }
                if let Some(&descendant) = ancestor_to_descendant.get(&existing_path) {
                    let existing_path = super::decode_path(
                        existing_path,
                        "sparse directory-conflict ancestor row",
                    )?;
                    return Ok(Some((existing_path, descendant.clone())));
                }
            }
        }

        Ok(None)
    }

    /// Implemented as a single ordered merge-walk: one `ORDER BY path`
    /// cursor over the existing, non-excluded desired state is streamed
    /// alongside sorted `paths`, instead of one descendant range query
    /// plus one ancestor point query *per changed path* (which is one
    /// `SQLite` execution per path-component in the worst case) and
    /// instead of binding every excluded shard id into a single `NOT IN
    /// (...)` clause (which can exceed `SQLite`'s bind-variable limit on a
    /// large reshape). Exclusion is applied row-by-row in Rust against
    /// `exclude_shard_ids` as the cursor is consumed, so this issues
    /// exactly one query regardless of how many shards are excluded or
    /// how many paths changed. Used above
    /// `SPARSE_DIRECTORY_CONFLICT_PATHS` changed paths, where reading
    /// every existing row once is cheaper than one query per path.
    fn find_directory_conflict_bulk(
        &self,
        sorted_paths: &[&gat_core::lexical_path::GatPath],
        exclude_shard_ids: &std::collections::HashSet<LockShardId>,
    ) -> Result<
        Option<(
            gat_core::lexical_path::GatPath,
            gat_core::lexical_path::GatPath,
        )>,
    > {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, desired_shard_id FROM state
                 WHERE desired_oid IS NOT NULL ORDER BY path",
            )
            .state_context("preparing directory-conflict merge-walk")?;
        #[cfg(any(test, feature = "test-support"))]
        super::query::test_support::record_sql_statement_prepared();
        let mut rows = stmt
            .query([])
            .state_context("querying directory-conflict merge-walk")?;

        // Still-open prefix candidates from the merged stream so far,
        // each paired with which side it came from; each candidate's
        // exclusive upper bound (`path + "0"`, same as
        // `FilteredRowCursor::open_ancestors`) is tested via
        // `exceeds_directory_upper_bound` against the path directly
        // rather than stored as an owned `String`. Bounded by live
        // nesting/overlap between the two sorted sequences, not by the
        // total number of existing rows.
        let mut open: Vec<(gat_core::lexical_path::GatPath, bool)> = Vec::new();
        let mut pending_existing = next_non_excluded_row(&mut rows, exclude_shard_ids)?;

        for &path in sorted_paths {
            while let Some((existing_path, _)) = &pending_existing {
                if existing_path.as_str() >= path.as_str() {
                    break;
                }
                if let Some(conflict) = merge_walk_check_and_open(&mut open, existing_path, false) {
                    return Ok(Some(conflict));
                }
                pending_existing = next_non_excluded_row(&mut rows, exclude_shard_ids)?;
            }
            if let Some(conflict) = merge_walk_check_and_open(&mut open, path, true) {
                return Ok(Some(conflict));
            }
        }

        // Drain existing rows for as long as some still-open changed
        // path's descendant range could still contain them; once the
        // cursor passes every open changed path's upper bound, no
        // further existing row can conflict with anything already seen.
        while let Some((existing_path, _)) = &pending_existing {
            let still_in_range = open.iter().any(|(ancestor, is_changed)| {
                *is_changed
                    && !gat_core::lock::validated::exceeds_directory_upper_bound(
                        existing_path.as_str(),
                        ancestor.as_str(),
                    )
            });
            if !still_in_range {
                break;
            }
            if let Some(conflict) = merge_walk_check_and_open(&mut open, existing_path, false) {
                return Ok(Some(conflict));
            }
            pending_existing = next_non_excluded_row(&mut rows, exclude_shard_ids)?;
        }

        Ok(None)
    }

    /// Every `shard_id` currently in the catalog, for detecting shards
    /// that disappeared since the last refresh (reshape, or every path in
    /// that shard removed). Production code derives this set from
    /// `Self::all_shard_identities`'s already-fetched catalog map
    /// instead (see `desired_index::refresh`) to avoid a second full
    /// `lock_shards` scan; this remains test-only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn all_shard_ids(&self) -> Result<Vec<LockShardId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT shard_id FROM lock_shards")
            .state_context("preparing shard catalog query")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .state_context("querying shard catalog")?
            .collect::<rusqlite::Result<Vec<String>>>()
            .state_context("reading shard catalog")?;
        ids.into_iter()
            .map(|id| {
                LockShardId::parse_canonical(&id).map_err(|source| StateStoreError::InvalidRow {
                    detail: format!("corrupt shard_id {id:?} in lock_shards: {source}"),
                })
            })
            .collect()
    }

    /// Apply one refresh cycle's worth of shard changes atomically:
    /// replace the desired rows for every changed/new shard in
    /// `changed_or_new` and record their new identity/stat, refresh only
    /// the cached stat fingerprint (never touching `state`) for shards in
    /// `stat_only_updates` whose content identity didn't change, and drop
    /// the desired rows (and catalog entry) for every shard in
    /// `removed_shard_ids` absent from the new catalog. See
    /// `desired_index::refresh` for the identity comparisons that decide
    /// what belongs in each list.
    ///
    /// If all three lists are empty -- every shard's local stat cache
    /// proved its identity unchanged and safe to trust (see
    /// `desired_index`'s tier-1 cache) -- this
    /// returns without ever opening a write transaction: a fully clean
    /// refresh is a handful of `stat()` calls, not a durable commit.
    ///
    /// This never has to fully re-derive `dirty` from a
    /// `desired`/`materialized` table diff: `dirty` lives on the
    /// same row each of these statements writes, as a `GENERATED ALWAYS`
    /// column, so touching only the shards that actually changed is
    /// always enough to keep it correct -- including for paths whose
    /// materialized half was changed out from under reconciliation
    /// entirely by `record_materialized`/`move_materialized`/
    /// `forget_materialized` (`gat add`/`mv`/`rm --cached`), since those
    /// write through the very same `state` row.
    pub(crate) fn apply_shard_refresh(
        &mut self,
        changed_or_new: &[ChangedShard],
        stat_only_updates: &[(LockShardId, Option<StatProof>)],
        removed_shards: &[RemovedShard],
    ) -> Result<()> {
        if changed_or_new.is_empty() && stat_only_updates.is_empty() && removed_shards.is_empty() {
            return Ok(());
        }

        let tx = self
            .conn
            .transaction()
            .state_context("beginning desired-state transaction")?;

        for removed in removed_shards {
            clear_shard_desired_tx(&tx, removed.shard_id)?;
        }
        for shard in changed_or_new {
            apply_shard_entries_tx(&tx, shard.shard_id, &shard.entries)?;
        }
        // `apply_shard_catalog_tx` only records identity/stat metadata --
        // it never consumes the row payload -- so this passes the
        // metadata-only view rather than requiring the catalog updater to
        // ignore an owned `Vec<Entry>` clone per shard.
        let meta: Vec<ChangedShardMeta> =
            changed_or_new.iter().map(ChangedShardMeta::from).collect();
        apply_shard_catalog_tx(&tx, &meta, stat_only_updates, removed_shards)?;

        tx.commit()
            .state_context("committing desired-state refresh")
    }

    /// Seed the shard catalog directly from `evidence` -- the
    /// `crate::lock::FullLockEvidence` a full-`gat.lock` write just
    /// established -- instead of requiring the caller to first translate
    /// gat-io's opaque publication receipt into `ChangedShard`/
    /// `RemovedShard` itself. `evidence` is read
    /// only through `crate::lock::FullLockEvidence::into_shards`
    /// and each I/O-private full-shard receipt's own
    /// `shard_id`/`identity`/`proof`/`into_entries` accessors, never its
    /// private physical `shape` field or any other representation detail
    /// -- this store, not `engine::repo`, is the one place that inspects
    /// stat proofs or shard identities merely to mirror a successful lock
    /// write. Every shard's prior identity comes from one whole-catalog
    /// fetch, reused both to build the changed/new set and to compute the
    /// removed-shard diff, then folded into `Self::apply_shard_refresh`.
    pub fn apply_full_lock_evidence(
        &mut self,
        evidence: crate::lock::FullLockEvidence,
    ) -> Result<()> {
        let prior_catalog = self.all_shard_identities()?;
        let changed_or_new: Vec<ChangedShard> = evidence
            .into_shards()
            .into_iter()
            .map(|shard| {
                let shard_id = shard.shard_id();
                let identity = shard.identity();
                let proof = shard.proof();
                let prior_identity = prior_catalog.get(&shard_id).map(|s| s.identity);
                ChangedShard {
                    shard_id,
                    prior_identity,
                    identity,
                    proof: Some(proof),
                    entries: shard.into_entries(),
                }
            })
            .collect();
        let touched: std::collections::HashSet<LockShardId> =
            changed_or_new.iter().map(|s| s.shard_id).collect();
        let removed_shards: Vec<RemovedShard> = prior_catalog
            .into_iter()
            .filter(|(id, _)| !touched.contains(id))
            .map(|(id, stored)| RemovedShard {
                shard_id: id,
                prior_identity: stored.identity,
            })
            .collect();

        self.apply_shard_refresh(&changed_or_new, &[], &removed_shards)
    }

    /// The current whole-desired-lock-set [`crate::lock::CanonicalDesiredIdentity`]
    /// (as raw bytes), maintained incrementally by `Self::apply_shard_refresh`.
    /// Combined with `git.ignore_patterns`
    /// (see `gat-engine`'s excludes fingerprint) to decide whether
    /// `.git/info/exclude` needs regenerating, without ever loading the
    /// full desired lock.
    pub fn desired_fingerprint(&self) -> Result<[u8; 32]> {
        let bytes: Vec<u8> = self
            .conn
            .query_row(
                "SELECT desired_fingerprint FROM reconciliation_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .state_context("reading desired fingerprint")?;
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| StateStoreError::InvalidRow {
                detail: "invalid desired fingerprint length".to_string(),
            })
    }

    /// `.git/info/exclude`'s currently recorded generating-input
    /// fingerprint, managed-block content identity, path count, and
    /// optional shared `StatProof`, read in one query -- see
    /// `ExcludeRecord`. `gat-engine`'s excludes stable-observation
    /// coordinator uses this to decide, without ever reading the output
    /// file itself, whether a proof-only fast path can apply at all.
    pub fn exclude_record(&self) -> Result<ExcludeRecord> {
        #[allow(clippy::type_complexity)]
        let row: (Option<Vec<u8>>, i64, Option<Vec<u8>>, Option<Vec<u8>>) = self
            .conn
            .query_row(
                "SELECT exclude_fingerprint, exclude_count, exclude_block_identity, exclude_proof
                 FROM reconciliation_meta WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .state_context("reading exclude output identity")?;
        let (fingerprint, count, block_identity, proof) = row;
        Ok(ExcludeRecord {
            fingerprint: fingerprint.and_then(|b| b.as_slice().try_into().ok()),
            count: usize::try_from(count).unwrap_or(0),
            block_identity: block_identity.and_then(|b| b.as_slice().try_into().ok()),
            // A malformed/unrecognized encoded proof is never trusted as
            // state -- see `shard_identities_query`'s identical policy.
            proof: proof
                .as_deref()
                .and_then(crate::file_state::decode_stat_proof),
        })
    }

    /// Refresh only `reconciliation_meta`'s exclude proof (not the
    /// fingerprint/count/block identity), for the case where a fresh
    /// `crate::file_state::coherent_observation` of `.git/info/exclude`
    /// proved the gat-managed block is still exactly what was last
    /// written: `Some(proof)` carries the resulting reusable stat-only
    /// fast path.
    pub fn refresh_exclude_verification(
        &mut self,
        verification: crate::git::InfoExcludeVerification,
    ) -> Result<()> {
        let Some(proof) = verification.into_refreshed_proof() else {
            return Ok(());
        };
        self.conn
            .execute(
                "UPDATE reconciliation_meta SET exclude_proof = ?1 WHERE id = 1",
                (crate::file_state::encode_stat_proof(&proof).to_vec(),),
            )
            .state_context("refreshing exclude output proof")?;
        Ok(())
    }

    /// Record that `.git/info/exclude`'s gat-managed block now matches
    /// `fingerprint` (the desired-lock-set fingerprint combined with
    /// `git.ignore_patterns`), contains `count` paths, and has content
    /// identity `block_identity` (`BLAKE3` of exactly the managed block's
    /// own bytes) -- along with the opaque update receipt from the physical
    /// publication -- so
    /// the next clean sync's fast path can prove both the inputs and the
    /// output file itself are unchanged, without regenerating it again.
    pub fn record_exclude_output(
        &mut self,
        fingerprint: [u8; 32],
        count: usize,
        block_identity: [u8; 32],
        update: crate::git::InfoExcludeUpdate,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE reconciliation_meta
                 SET exclude_fingerprint = ?1, exclude_count = ?2,
                     exclude_block_identity = ?3, exclude_proof = ?4
                 WHERE id = 1",
                (
                    fingerprint.to_vec(),
                    i64::try_from(count).unwrap_or(i64::MAX),
                    block_identity.to_vec(),
                    update
                        .proof()
                        .map(|p| crate::file_state::encode_stat_proof(&p).to_vec()),
                ),
            )
            .state_context("recording exclude output identity")?;
        Ok(())
    }

    /// Whether any path is currently flagged dirty. Only used by tests;
    /// production code queries [`Self::dirty_rows_in_scope`] directly
    /// (an empty result already means "nothing is dirty").
    #[cfg(any(test, feature = "test-support"))]
    pub fn has_dirty(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM state WHERE dirty = 1 LIMIT 1",
                [],
                |_| Ok(()),
            )
            .optional()
            .state_context("checking dirty state")?
            .is_some())
    }

    /// Every dirty path, in sorted order, with its current desired/
    /// materialized halves. Only used by tests; production planning
    /// calls [`Self::dirty_rows_in_scope_after`] directly so path scope
    /// and keyset pagination can be pushed into the same indexed query.
    #[cfg(any(test, feature = "test-support"))]
    pub fn dirty_rows(&self) -> Result<Vec<DirtyRow>> {
        self.dirty_rows_in_scope(None)
    }

    /// Like [`Self::dirty_rows`], but restricted to exactly `scope` plus
    /// everything nested under it (same lexical byte-range semantics as
    /// `descendant_range`/[`Self::with_rows_in_scope`]), or every dirty
    /// row when `scope` is `None`. Kept for tests that assert scoping in
    /// isolation from pagination; production planning calls
    /// [`Self::dirty_rows_in_scope_after`] directly.
    #[cfg(any(test, feature = "test-support"))]
    pub fn dirty_rows_in_scope(
        &self,
        scope: Option<&gat_core::lexical_path::GatPath>,
    ) -> Result<Vec<DirtyRow>> {
        let raw = query_dirty_rows(&self.conn, scope, None, -1)?;
        raw_into_dirty_rows(raw)
    }

    /// Visit dirty rows in the exact-or-descendant scope using keyset pagination: only
    /// rows with `path > after` are returned (`after` is the last path
    /// seen in the previous page, or `None` for the first page), and at
    /// most `limit` rows are returned. Lets a fresh/large sync stream
    /// dirty rows in bounded chunks -- fetch a chunk, materialize/persist
    /// it, then fetch the next -- instead of loading every dirty row (and
    /// building every corresponding action) into memory at once. `path`
    /// order is stable and strictly increasing across pages because the
    /// underlying query is `ORDER BY path`, and processed rows
    /// stop being `dirty = 1` once their state is persisted, so this
    /// keyset walk never re-visits or skips a row even though the dirty
    /// set shrinks as pagination proceeds.
    pub fn dirty_rows_in_scope_after(
        &self,
        scope: Option<&gat_core::lexical_path::GatPath>,
        after: Option<&gat_core::lexical_path::GatPath>,
        limit: usize,
    ) -> Result<Vec<DirtyRow>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let raw = query_dirty_rows(&self.conn, scope, after, limit)?;
        raw_into_dirty_rows(raw)
    }
}

/// Pull the next row from [`StateStore::find_directory_conflict`]'s
/// `ORDER BY path` cursor whose `desired_shard_id` isn't in
/// `exclude_shard_ids`, skipping any that is -- the row-at-a-time
/// counterpart of filtering a `NOT IN (...)` clause in SQL, without
/// binding the whole excluded set as query parameters.
fn next_non_excluded_row(
    rows: &mut rusqlite::Rows,
    exclude_shard_ids: &std::collections::HashSet<LockShardId>,
) -> Result<Option<(gat_core::lexical_path::GatPath, Option<LockShardId>)>> {
    while let Some(row) = rows
        .next()
        .state_context("reading directory-conflict merge-walk row")?
    {
        let path: String = row.get(0)?;
        let shard_id: Option<String> = row.get(1)?;
        let shard_id = shard_id
            .map(|id| decode_shard_id(&id, "desired_shard_id"))
            .transpose()?;
        #[cfg(any(test, feature = "test-support"))]
        super::query::test_support::record_existing_row_visited();
        if shard_id.is_some_and(|id| exclude_shard_ids.contains(&id)) {
            continue;
        }
        let path = super::decode_path(path, "directory-conflict merge-walk row")?;
        return Ok(Some((path, shard_id)));
    }
    Ok(None)
}

/// One step of [`StateStore::find_directory_conflict`]'s merge-walk:
/// retire every still-open candidate whose upper bound `path` has reached
/// or passed (same rule as [`gat_core::lock::validated::FilteredRowCursor`]'s
/// `open_ancestors`), report a conflict if `path` is a directory prefix
/// of (or the descendant of) a still-open candidate from the *other*
/// side, and otherwise open `path` itself as a new candidate. Same-side
/// conflicts are never reported here: changed-vs-changed conflicts are
/// already checked in-memory before this runs, and existing-vs-existing
/// rows were already valid when persisted.
fn merge_walk_check_and_open(
    open: &mut Vec<(gat_core::lexical_path::GatPath, bool)>,
    path: &gat_core::lexical_path::GatPath,
    is_changed: bool,
) -> Option<(
    gat_core::lexical_path::GatPath,
    gat_core::lexical_path::GatPath,
)> {
    open.retain(|(ancestor, _)| {
        !gat_core::lock::validated::exceeds_directory_upper_bound(path.as_str(), ancestor.as_str())
    });
    let conflict = open
        .iter()
        .find(|(other_path, other_is_changed)| {
            *other_is_changed != is_changed
                && gat_core::lock::validated::is_directory_prefix(
                    other_path.as_str(),
                    path.as_str(),
                )
        })
        .map(|(other_path, _)| (other_path.clone(), path.clone()));
    if conflict.is_none() {
        open.push((path.clone(), is_changed));
    }
    conflict
}

/// One row of a shard's content identity as stored in `lock_shards`:
/// `identity` is always known; `proof` is the shared, optional
/// `StatProof` this identity may be reused from without reading the
/// file again -- `None` means the identity is known but must be
/// re-established from bytes before any stat-only reuse (stat-proof
/// `lock_shards` does not persist a
/// separate, richer stat/racy shape of its own -- the shared
/// `StatProof` is the sole persisted proof type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredShard {
    pub identity: ShardIdentity,
    pub proof: Option<StatProof>,
}

/// A shard file's canonical content identity -- see
/// `crate::lock::ShardContentIdentity`, the shared type this is an
/// alias for: `BLAKE3(raw shard file bytes)`, always
/// computed the same way whether it comes from publication, refresh, or
/// full live observation (see `desired_index`). It never overrides
/// worktree content -- only to decide whether a shard needs reparsing, and
/// to XOR-accumulate (via `crate::lock::CanonicalDesiredIdentity::toggle_shard`)
/// into the whole-lock [`crate::lock::CanonicalDesiredIdentity`].
pub(crate) type ShardIdentity = crate::lock::ShardContentIdentity;

fn read_desired_identity_tx(
    tx: &rusqlite::Transaction<'_>,
) -> Result<crate::lock::CanonicalDesiredIdentity> {
    let bytes: Vec<u8> = tx
        .query_row(
            "SELECT desired_fingerprint FROM reconciliation_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .state_context("reading desired fingerprint")?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| StateStoreError::InvalidRow {
            detail: "invalid desired fingerprint length".to_string(),
        })?;
    Ok(crate::lock::CanonicalDesiredIdentity::from_bytes(bytes))
}

fn write_desired_identity_tx(
    tx: &rusqlite::Transaction<'_>,
    identity: crate::lock::CanonicalDesiredIdentity,
) -> Result<()> {
    tx.execute(
        "UPDATE reconciliation_meta SET desired_fingerprint = ?1 WHERE id = 1",
        (identity.as_bytes().to_vec(),),
    )
    .state_context("updating desired fingerprint")?;
    Ok(())
}

/// Apply one refresh/publish cycle's catalog changes: upsert each changed
/// or new shard's identity/stat, refresh stat-only updates without
/// touching content identity, drop removed shards, and maintain the
/// persisted whole-lock [`crate::lock::CanonicalDesiredIdentity`]
/// incrementally in the same transaction --
/// reading it once, then `toggle_shard`-ing out every removed/replaced
/// prior contribution and `toggle_shard`-ing in every new one. This never
/// rereads or re-derives the identity from the whole `lock_shards`
/// catalog: cost here is strictly proportional to `changed_or_new.len() +
/// removed_shards.len()`, never to the total shard count, regardless of
/// how large the rest of the catalog is.
pub(super) fn apply_shard_catalog_tx(
    tx: &rusqlite::Transaction<'_>,
    changed_or_new: &[ChangedShardMeta],
    stat_only_updates: &[(LockShardId, Option<StatProof>)],
    removed_shards: &[RemovedShard],
) -> Result<()> {
    // A `stat_only`-only refresh never touches shard identities at all,
    // so the persisted whole-lock identity is provably unchanged and
    // doesn't need reading back, toggling, or rewriting.
    let touches_identity = !changed_or_new.is_empty() || !removed_shards.is_empty();
    let mut identity = if touches_identity {
        Some(read_desired_identity_tx(tx)?)
    } else {
        None
    };

    for removed in removed_shards {
        let encoded = CanonicalShardIdBuf::encode(removed.shard_id);
        tx.execute(
            "DELETE FROM lock_shards WHERE shard_id = ?1",
            [encoded.as_str()],
        )
        .state_context("removing shard-catalog row for a disappeared shard")?;
        if let Some(acc) = identity.as_mut() {
            *acc = acc.toggle_shard(removed.shard_id, &removed.prior_identity);
        }
    }
    for shard in changed_or_new {
        upsert_shard_identity_tx(tx, shard.shard_id, &shard.identity, shard.proof)?;
        if let Some(acc) = identity.as_mut() {
            if let Some(prior_identity) = &shard.prior_identity {
                *acc = acc.toggle_shard(shard.shard_id, prior_identity);
            }
            *acc = acc.toggle_shard(shard.shard_id, &shard.identity);
        }
    }
    for (shard_id, proof) in stat_only_updates {
        let encoded = CanonicalShardIdBuf::encode(*shard_id);
        tx.execute(
            "UPDATE lock_shards SET proof = ?2 WHERE shard_id = ?1",
            (
                encoded.as_str(),
                proof.map(|p| crate::file_state::encode_stat_proof(&p).to_vec()),
            ),
        )
        .state_context("refreshing shard stat cache")?;
    }

    if let Some(identity) = identity {
        write_desired_identity_tx(tx, identity)?;
    }
    Ok(())
}

fn upsert_shard_identity_tx(
    tx: &rusqlite::Transaction<'_>,
    shard_id: LockShardId,
    identity: &ShardIdentity,
    proof: Option<StatProof>,
) -> Result<()> {
    let encoded = CanonicalShardIdBuf::encode(shard_id);
    tx.execute(
        "INSERT INTO lock_shards (shard_id, identity, proof)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(shard_id) DO UPDATE SET
             identity = excluded.identity,
             proof = excluded.proof",
        (
            encoded.as_str(),
            identity.as_bytes().to_vec(),
            proof.map(|p| crate::file_state::encode_stat_proof(&p).to_vec()),
        ),
    )
    .state_context("upserting shard identity")?;
    Ok(())
}

/// A dirty path with its current desired/materialized halves, as
/// produced by [`StateStore::dirty_rows_in_scope_after`]. Reconciliation identity
/// is OID-only, so a dirty row carries just the desired/materialized OIDs
/// (no size); the optional stat proof is a worktree acceleration recorded
/// per-path in `materialized_proof` and consulted during full planning,
/// not part of dirty-row identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyRow {
    pub path: gat_core::lexical_path::GatPath,
    pub desired: Option<Oid>,
    pub materialized: Option<Oid>,
}

type RawDirtyRow = (String, Option<Vec<u8>>, Option<Vec<u8>>);

fn dirty_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawDirtyRow> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, Option<Vec<u8>>>(1)?,
        row.get::<_, Option<Vec<u8>>>(2)?,
    ))
}

/// Shared query behind [`StateStore::dirty_rows_in_scope`] and
/// [`StateStore::dirty_rows_in_scope_after`]: dirty rows in
/// `scope` (or every dirty row when `scope` is `None`), optionally
/// keyset-paginated past `after`, capped at `limit` (`-1` for no cap --
/// `SQLite` treats a negative `LIMIT` as unbounded).
fn query_dirty_rows(
    conn: &Connection,
    scope: Option<&gat_core::lexical_path::GatPath>,
    after: Option<&gat_core::lexical_path::GatPath>,
    limit: i64,
) -> Result<Vec<RawDirtyRow>> {
    match scope {
        Some(scope) => {
            let (lower, upper) = descendant_range(scope.as_str());
            if let Some(after) = after {
                let mut stmt = conn
                    .prepare(
                        "SELECT path, desired_oid, materialized_oid
                         FROM state
                         WHERE dirty = 1 AND (path = ?1 OR (path >= ?2 AND path < ?3))
                           AND path > ?4
                         ORDER BY path
                         LIMIT ?5",
                    )
                    .state_context("preparing scoped dirty-rows page query")?;
                stmt.query_map(
                    (scope.as_str(), &lower, &upper, after.as_str(), limit),
                    dirty_row_from_row,
                )
                .state_context("querying scoped dirty rows page")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .state_context("reading scoped dirty rows page")
            } else {
                let mut stmt = conn
                    .prepare(
                        "SELECT path, desired_oid, materialized_oid
                         FROM state
                         WHERE dirty = 1 AND (path = ?1 OR (path >= ?2 AND path < ?3))
                         ORDER BY path
                         LIMIT ?4",
                    )
                    .state_context("preparing scoped dirty-rows query")?;
                stmt.query_map((scope.as_str(), &lower, &upper, limit), dirty_row_from_row)
                    .state_context("querying scoped dirty rows")?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .state_context("reading scoped dirty rows")
            }
        }
        None => {
            if let Some(after) = after {
                let mut stmt = conn
                    .prepare(
                        "SELECT path, desired_oid, materialized_oid
                     FROM state WHERE dirty = 1 AND path > ?1
                     ORDER BY path
                     LIMIT ?2",
                    )
                    .state_context("preparing dirty-rows page query")?;
                stmt.query_map((after.as_str(), limit), dirty_row_from_row)
                    .state_context("querying dirty rows page")?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .state_context("reading dirty rows page")
            } else {
                let mut stmt = conn
                    .prepare(
                        "SELECT path, desired_oid, materialized_oid
                     FROM state WHERE dirty = 1
                     ORDER BY path
                     LIMIT ?1",
                    )
                    .state_context("preparing dirty-rows query")?;
                stmt.query_map((limit,), dirty_row_from_row)
                    .state_context("querying dirty rows")?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .state_context("reading dirty rows")
            }
        }
    }
}

fn raw_into_dirty_rows(raw: Vec<RawDirtyRow>) -> Result<Vec<DirtyRow>> {
    raw.into_iter()
        .map(|(path, d_oid, m_oid)| {
            Ok(DirtyRow {
                path: gat_core::lexical_path::GatPath::from_canonical_string(path).map_err(
                    |source| StateStoreError::InvalidRow {
                        detail: format!("dirty-row join has invalid path: {source}"),
                    },
                )?,
                desired: decode_optional_oid(d_oid)?,
                materialized: decode_optional_oid(m_oid)?,
            })
        })
        .collect()
}

fn decode_optional_oid(oid: Option<Vec<u8>>) -> Result<Option<Oid>> {
    match oid {
        Some(oid) => {
            let oid: [u8; 32] =
                oid.as_slice()
                    .try_into()
                    .map_err(|_| StateStoreError::InvalidRow {
                        detail: format!("invalid oid length {} in dirty-row join", oid.len()),
                    })?;
            Ok(Some(Oid::from_bytes(oid)))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_layout::RepositoryLayout as Repo;

    fn state_directory() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn sid(raw: &str) -> LockShardId {
        LockShardId::parse_canonical(raw).unwrap()
    }

    /// A depth-3 [`LockShardId`] derived deterministically from `i`,
    /// distinct for every distinct `i` in `0..16_777_216` -- for
    /// fixtures that just need many unique shard ids and don't care
    /// about the id's relationship to any real path.
    fn nsid(i: u32) -> LockShardId {
        debug_assert!(i < 1 << 24, "nsid only covers 0..16_777_216 at depth 3");
        let b = i.to_be_bytes();
        sid(&format!(
            "gat.lock/{:02x}/{:02x}/{:02x}.tsv",
            b[1], b[2], b[3]
        ))
    }

    fn seed_shard(store: &mut StateStore, shard_id: LockShardId, byte: u8, entries: Vec<Entry>) {
        let proof = Some(StatProof {
            size: entries.len() as u64,
            mtime_secs: i64::from(byte),
            mtime_nanos: 0,
        });
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id,
                    prior_identity: None,
                    identity: ShardIdentity::from_array([byte; 32]),
                    proof,
                    entries,
                }],
                &[],
                &[],
            )
            .unwrap();
    }

    fn entry(path: &str, byte: u8) -> Entry {
        Entry {
            path: gat_core::lexical_path::GatPath::parse_canonical(path).unwrap(),
            oid: Oid::from_bytes([byte; 32]),
        }
    }

    #[test]
    fn scoped_shard_identity_lookup_spans_bounded_sql_chunks() {
        let tmp = state_directory();
        let repo = Repo::at(tmp.path().to_path_buf());
        let store = StateStore::open(&repo).unwrap();
        let ids = (0..(u32::try_from(sql_chunk_size(1)).unwrap() + 3))
            .map(nsid)
            .collect::<std::collections::BTreeSet<_>>();

        assert!(
            shard_identities_query(&store.conn, Some(&ids))
                .unwrap()
                .is_empty()
        );
    }

    /// `find_directory_conflict` must find a real conflict against an
    /// existing, non-excluded shard even when the excluded-shard set is
    /// far larger than one SQL bind-variable chunk -- exclusion is applied row-by-row in
    /// Rust as the single ordered cursor is consumed, not bound into one
    /// SQL statement, so there is no `SQLite` bind-variable limit to
    /// exceed regardless of how many shards this refresh is excluding.
    /// Also asserts exactly one SQL statement is prepared, proving this
    /// doesn't regress into one query per excluded shard or per changed
    /// path.
    #[test]
    fn find_directory_conflict_handles_an_exclude_set_larger_than_one_sql_chunk() {
        let tmp = state_directory();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();

        // Exclusion is applied row-by-row in Rust against `exclude` as
        // each returned row is consumed -- it is never bound into SQL --
        // so the excluded shard ids never need to correspond to real
        // rows in the database. Only two real SQLite rows are needed to
        // exercise the invariant: one real excluded shard that would
        // otherwise itself be the conflict (proving it's actually
        // skipped) and one real non-excluded shard holding the actual
        // conflict this test expects to find.
        let stat = Some(StatProof {
            size: 1,
            mtime_secs: 0,
            mtime_nanos: 0,
        });
        let excluded_real_shard = sid("gat.lock/aa.tsv");
        let shards: Vec<ChangedShard> = vec![
            ChangedShard {
                shard_id: excluded_real_shard,
                prior_identity: None,
                identity: ShardIdentity::from_array([1; 32]),
                proof: stat,
                entries: vec![entry("foo/excluded.bin", 1)],
            },
            ChangedShard {
                shard_id: sid("gat.lock/bb.tsv"),
                prior_identity: None,
                identity: ShardIdentity::from_array([250; 32]),
                proof: stat,
                entries: vec![entry("foo/bar", 250)],
            },
        ];
        store.apply_shard_refresh(&shards, &[], &[]).unwrap();

        // Pad the exclude set, entirely in memory, with synthetic shard
        // ids that never exist in the database, past one SQL
        // bind-variable chunk -- proving the size of this set alone
        // (regardless of how many of its entries are backed by real
        // rows) can never blow a SQL bind-variable limit.
        let mut exclude = std::collections::HashSet::new();
        exclude.insert(excluded_real_shard);
        for i in 0..u32::try_from(sql_chunk_size(1) * 2 + 3).unwrap() {
            exclude.insert(nsid(i));
        }

        let paths = vec![gat_core::lexical_path::GatPath::parse_canonical("foo").unwrap()];
        let before = crate::state::test_support::snapshot();
        let conflict = store.find_directory_conflict(&paths, &exclude).unwrap();
        let (statements_after, ..) = crate::state::test_support::snapshot();
        assert_eq!(
            conflict,
            Some((
                gat_core::lexical_path::GatPath::parse_canonical("foo").unwrap(),
                gat_core::lexical_path::GatPath::parse_canonical("foo/bar").unwrap()
            )),
            "the real conflict against the non-excluded shard must still be found \
             past an exclude set larger than one SQL chunk"
        );
        assert_eq!(
            statements_after - before.0,
            1,
            "exactly one SQL statement must be prepared regardless of how many \
             shards are excluded or how many paths are checked"
        );
    }

    /// A small changed-path set (well under `SPARSE_DIRECTORY_CONFLICT_PATHS`)
    /// must use the sparse, indexed-lookup plan and therefore only ever
    /// visit rows actually near the changed paths -- never scan the bulk
    /// of an otherwise-unrelated, much larger desired mirror. Proves the
    /// "sparse where possible" contract with a *row-visited* count, not
    /// just a statement count: a single full-mirror `ORDER BY path`
    /// statement would still satisfy a statement-count assertion while
    /// silently streaming every row.
    #[test]
    fn find_directory_conflict_does_not_scan_unrelated_rows_for_a_small_changed_set() {
        let tmp = state_directory();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();

        // A large, otherwise-unrelated desired mirror -- none of these
        // paths are anywhere near the one path this refresh is changing.
        const UNRELATED_ROWS: usize = 5_000;
        let unrelated_entries: Vec<Entry> = (0..UNRELATED_ROWS)
            .map(|i| entry(&format!("unrelated/{i:05}.bin"), (i % 256).to_le_bytes()[0]))
            .collect();
        seed_shard(&mut store, sid("gat.lock/aa.tsv"), 1, unrelated_entries);

        let paths =
            vec![gat_core::lexical_path::GatPath::parse_canonical("some/new/path.bin").unwrap()];
        let exclude = std::collections::HashSet::new();

        let before = crate::state::test_support::snapshot();
        let conflict = store.find_directory_conflict(&paths, &exclude).unwrap();
        let (statements_after, _, _, _, rows_visited_after) =
            crate::state::test_support::snapshot();
        assert_eq!(conflict, None);
        assert!(
            statements_after - before.0 <= 2,
            "a small changed set must stay within a handful of statements \
             (one descendant range query, plus one chunked ancestor lookup), \
             got {}",
            statements_after - before.0
        );
        assert!(
            rows_visited_after - before.4 < 10,
            "a one-path change must not visit anywhere near the {UNRELATED_ROWS} \
             unrelated rows in the desired mirror just to prove there's no \
             conflict, visited {}",
            rows_visited_after - before.4
        );
    }

    /// The same single-statement guarantee for a large *changed*-path
    /// set: `find_directory_conflict` must not regress into one
    /// descendant/ancestor query per path (or per `/`-separated
    /// component) as the number of changed paths grows.
    #[test]
    fn find_directory_conflict_checks_a_large_changed_path_set_with_one_statement() {
        let tmp = state_directory();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();

        seed_shard(
            &mut store,
            sid("gat.lock/aa.tsv"),
            10,
            vec![entry("nested/deep/path/leaf", 10)],
        );

        let paths: Vec<gat_core::lexical_path::GatPath> = (0..2000)
            .map(|i| {
                gat_core::lexical_path::GatPath::parse_canonical(&format!("other-{i:05}.bin"))
                    .unwrap()
            })
            .chain(std::iter::once(
                gat_core::lexical_path::GatPath::parse_canonical("nested/deep/path").unwrap(),
            ))
            .collect();
        let exclude = std::collections::HashSet::new();

        let before = crate::state::test_support::snapshot();
        let conflict = store.find_directory_conflict(&paths, &exclude).unwrap();
        let (statements_after, ..) = crate::state::test_support::snapshot();
        assert_eq!(
            conflict,
            Some((
                gat_core::lexical_path::GatPath::parse_canonical("nested/deep/path").unwrap(),
                gat_core::lexical_path::GatPath::parse_canonical("nested/deep/path/leaf").unwrap()
            ))
        );
        assert_eq!(
            statements_after - before.0,
            1,
            "a large changed-path set must still be checked with exactly one SQL statement"
        );
    }
}
