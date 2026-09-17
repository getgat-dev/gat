use super::query::DesiredQuery;
use super::reconciliation::{apply_shard_catalog_tx, shard_identities_for_tx};
use super::{
    CanonicalShardIdBuf, Connection, DesiredPaths, DesiredStateWrite, Entry, GatPath, Lock,
    RemovedShard, Result, StateResultExt, StateStore, StateStoreError, StoredShard, decode_row_raw,
    decode_shard_id, descendant_range, params_from_iter, shard_id_for_path, sql_chunk_size,
    sql_chunk_size_with_shared_binds, sql_placeholders,
};
use crate::lock::{LockError, LockShardId, LockStore, LockWriteGuard};

/// One semantic desired-state removal to apply before publishing the
/// resulting touched lock shards.
pub enum DesiredRemoval<'a> {
    /// Clear exactly the selected paths.
    Exact(&'a [GatPath]),
    /// Clear the selected path and every desired row nested beneath it.
    Prefix(&'a GatPath),
}

impl DesiredStateWrite<'_> {
    fn incremental_shard_levels(shape_lock: &LockWriteGuard) -> crate::lock::LockShardLevels {
        shape_lock
            .shard_levels()
            .expect("desired sparse publication requires an incremental LockWriteGuard")
    }

    fn upsert_and_publish<E>(
        &self,
        layout: &crate::RepositoryLayout,
        shape_lock: &LockWriteGuard,
        entries: &[Entry],
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        if entries.is_empty() {
            return Ok(());
        }
        let mut touched = self
            .desired_shard_ids_for_paths(entries.iter().map(|entry| &entry.path))
            .map_err(E::from)?;
        let shard_levels = Self::incremental_shard_levels(shape_lock);
        self.upsert_entries(entries, shard_levels)
            .map_err(E::from)?;
        touched.extend(
            entries
                .iter()
                .map(|entry| shard_id_for_path(&entry.path, shard_levels)),
        );
        self.publish_touched(layout, shape_lock, &touched)
    }

    fn remove_and_publish<E>(
        &self,
        layout: &crate::RepositoryLayout,
        shape_lock: &LockWriteGuard,
        removals: &[DesiredRemoval<'_>],
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        let mut touched = std::collections::BTreeSet::new();
        for removal in removals {
            match removal {
                DesiredRemoval::Exact(paths) => {
                    touched.extend(
                        self.desired_shard_ids_for_paths(paths.iter())
                            .map_err(E::from)?,
                    );
                    self.remove_paths(paths).map_err(E::from)?;
                }
                DesiredRemoval::Prefix(path) => {
                    touched.extend(
                        desired_shard_ids_for_scope_inner(&self.tx, path).map_err(E::from)?,
                    );
                    clear_desired_prefix_tx(&self.tx, path).map_err(E::from)?;
                }
            }
        }
        if touched.is_empty() {
            return Ok(());
        }
        self.publish_touched(layout, shape_lock, &touched)
    }

    /// Publish the post-mutation contents of exactly `touched_shard_ids` and
    /// update the desired-state shard catalog from the resulting opaque
    /// publication receipt. Physical shape dispatch, proof adaptation, and
    /// receipt interpretation remain entirely inside `gat-io`.
    pub(crate) fn publish_touched<E>(
        &self,
        layout: &crate::RepositoryLayout,
        shape_lock: &LockWriteGuard,
        touched_shard_ids: &std::collections::BTreeSet<LockShardId>,
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        if touched_shard_ids.is_empty() {
            return Ok(());
        }
        if shape_lock.is_flat() {
            return self.publish_flat_streaming(layout);
        }
        let rows_by_shard = self
            .desired_rows_by_shard_ids(touched_shard_ids)
            .map_err(E::from)?;
        let priors = self.shard_identities(touched_shard_ids).map_err(E::from)?;
        let publish_priors = Self::sparse_publish_priors(&priors);
        let (published, removed) = LockStore::publish_touched(
            layout,
            shape_lock,
            touched_shard_ids,
            &rows_by_shard,
            &publish_priors,
        )
        .map_err(E::from)?;
        self.record_published_shards(&published, &removed, &priors)
            .map_err(E::from)
    }

    /// Stream the complete post-mutation desired state into one flat
    /// publication and update the shard catalog from its opaque receipt.
    /// Used by bounded batched replay so destination memory stays constant.
    pub(crate) fn publish_flat_streaming<E>(
        &self,
        layout: &crate::RepositoryLayout,
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        let touched = std::collections::BTreeSet::from([LockShardId::flat()]);
        let priors = self.shard_identities(&touched).map_err(E::from)?;
        let (published, removed) = self.with_desired_rows(DesiredQuery::all(), |mut rows| {
            LockStore::publish_flat_streaming(layout, || match rows.next() {
                Ok(Some(row)) => Ok(Some(row.into_entry())),
                Ok(None) => Ok(None),
                Err(source) => Err(LockError::RowSource(Box::new(source))),
            })
            .map_err(E::from)
        })?;
        self.record_published_shards(&published, &removed, &priors)
            .map_err(E::from)
    }

    fn sparse_publish_priors(
        priors: &std::collections::HashMap<LockShardId, StoredShard>,
    ) -> std::collections::BTreeMap<
        LockShardId,
        (
            crate::lock::ShardContentIdentity,
            crate::file_state::StatProof,
        ),
    > {
        priors
            .iter()
            .filter_map(|(shard_id, stored)| {
                stored
                    .proof
                    .map(|proof| (*shard_id, (stored.identity, proof)))
            })
            .collect()
    }

    /// Clear the desired half of an explicit set of exact paths, the
    /// counterpart of prefix removal for a selector that has no
    /// lexical locality (a glob): the caller has already streamed the
    /// desired cursor and knows exactly which rows matched, so the delete
    /// is keyed by those paths instead of a range.
    pub fn remove_paths(&self, paths: &[GatPath]) -> Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        super::query::test_support::record_remove_paths_call();
        clear_desired_exact_tx(&self.tx, paths)
    }

    /// Every distinct `desired_shard_id` currently attached to `paths` in
    /// this transaction's view of `state`. Used before and after a scoped
    /// mutation to identify exactly which on-disk shard files may now differ.
    ///
    /// Accepts any borrowed-`GatPath` iterator so callers whose paths live
    /// inside `Entry` rows (or any other owner) never need to clone them
    /// into a standalone `Vec<GatPath>` just to satisfy this lookup.
    pub fn desired_shard_ids_for_paths<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a GatPath>,
    ) -> Result<std::collections::BTreeSet<LockShardId>> {
        desired_shard_ids_for_paths_inner(&self.tx, paths)
    }

    /// The complete current row set for each touched shard ID, from this
    /// transaction's post-mutation `SQLite` view. Sparse sharded publication
    /// uses this instead of ever materializing the full desired lock: only
    /// the shard files named in `shard_ids` are re-derived, rendered, and
    /// potentially rewritten.
    pub(crate) fn desired_rows_by_shard_ids(
        &self,
        shard_ids: &std::collections::BTreeSet<LockShardId>,
    ) -> Result<std::collections::BTreeMap<LockShardId, Vec<Entry>>> {
        desired_rows_by_shard_ids_inner(&self.tx, shard_ids)
    }

    /// Update the touched shard catalog (`lock_shards`) and whole-lock-set
    /// identity after the corresponding shard files were successfully
    /// published from this transaction's desired rows. Unlike
    /// [`StateStore::apply_shard_refresh`], this assumes the caller has
    /// already applied the desired-row mutation in the same transaction, so it
    /// only records shard identity/stat metadata and the incremental
    /// identity update.
    ///
    /// `prior_catalog` must already be scoped to (at least) every shard
    /// named in `changed_or_new` and `removed_shard_ids` -- callers obtain
    /// it via [`Self::shard_identities`] *before* publishing to disk, and
    /// reuse that exact same map both to accelerate publication's own
    /// unchanged-content fast path and here, rather than this function
    /// re-querying the identical touched-shard-scoped rows a second time
    /// This shared fetch avoids repeating the same touched-shard-scoped
    /// lookup during publication and catalog recording. A changed/new shard whose id isn't found in
    /// `prior_catalog` (a never-seen shard) simply has no prior identity
    /// to XOR out.
    pub(crate) fn record_published_shards(
        &self,
        changed_or_new: &[crate::lock::ShardEvidence],
        removed_shard_ids: &[LockShardId],
        prior_catalog: &std::collections::HashMap<LockShardId, StoredShard>,
    ) -> Result<()> {
        let changed_or_new: Vec<super::reconciliation::ChangedShardMeta> = changed_or_new
            .iter()
            .map(|shard| {
                let shard_id = shard.shard_id();
                let prior = prior_catalog.get(&shard_id).map(|stored| stored.identity);
                super::reconciliation::ChangedShardMeta {
                    shard_id,
                    prior_identity: prior,
                    identity: shard.identity(),
                    proof: Some(shard.proof()),
                }
            })
            .collect();
        let removed: Vec<RemovedShard> = removed_shard_ids
            .iter()
            .filter_map(|shard_id| {
                prior_catalog.get(shard_id).map(|stored| RemovedShard {
                    shard_id: *shard_id,
                    prior_identity: stored.identity,
                })
            })
            .collect();
        apply_shard_catalog_tx(&self.tx, &changed_or_new, &[], &removed)
    }

    /// Fetch the `lock_shards` catalog rows for exactly `shard_ids`, in one
    /// chunked query scoped to that set -- never the entire catalog
    /// (proportional to the total shard count `Q`), regardless of how
    /// large the rest of it is. Callers that both need a publish-time
    /// prior-proof accelerator and must later record the same shards'
    /// prior identity (e.g. [`Self::record_published_shards`]) should
    /// call this once, before publishing to disk, and reuse the same map
    /// for both, rather than querying twice.
    pub(crate) fn shard_identities(
        &self,
        shard_ids: &std::collections::BTreeSet<LockShardId>,
    ) -> Result<std::collections::HashMap<LockShardId, StoredShard>> {
        shard_identities_for_tx(&self.tx, shard_ids)
    }

    /// Upsert the desired half (`oid/shard_id`) for the given entries,
    /// computing `desired_shard_id` from the configured shard depth. Used
    /// by `gat add`'s sparse sharded path to write only the changed/new
    /// desired rows instead of rebuilding the full lock.
    pub fn upsert_entries(
        &self,
        entries: &[Entry],
        shard_levels: crate::lock::LockShardLevels,
    ) -> Result<()> {
        upsert_desired_entries_tx(&self.tx, entries, shard_levels, "upserting")
    }
}

/// Upsert desired rows inside `tx`, deriving shard IDs from `shard_levels`.
/// Paths and OIDs are borrowed;
/// encoded shard IDs use a reusable chunk buffer. Parameters stream into
/// `SQLite` without a second collection. `verb` identifies the operation
/// in technical error context.
fn upsert_desired_entries_tx(
    tx: &rusqlite::Transaction<'_>,
    entries: &[Entry],
    shard_levels: crate::lock::LockShardLevels,
    verb: &str,
) -> Result<()> {
    let chunk_size = sql_chunk_size(3);
    let mut shard_bufs: Vec<CanonicalShardIdBuf> =
        Vec::with_capacity(entries.len().min(chunk_size));
    for chunk in entries.chunks(chunk_size) {
        let placeholders = sql_placeholders("(?, ?, ?)", chunk.len());
        let sql = format!(
            "INSERT INTO state (path, desired_oid, desired_shard_id)
             VALUES {placeholders}
             ON CONFLICT(path) DO UPDATE SET
                 desired_oid = excluded.desired_oid,
                 desired_shard_id = excluded.desired_shard_id"
        );
        // A row's shard ID can differ from its neighbors' (unlike
        // `apply_shard_entries_tx`'s single shared shard), so each row
        // still needs its own bind -- but as a reused, fixed-capacity
        // buffer rather than an owned `String` allocated per row.
        shard_bufs.clear();
        shard_bufs.extend(
            chunk
                .iter()
                .map(|e| CanonicalShardIdBuf::encode(shard_id_for_path(&e.path, shard_levels))),
        );
        let params = chunk.iter().zip(&shard_bufs).flat_map(|(entry, shard)| {
            [
                rusqlite::types::ToSqlOutput::from(entry.path.as_str()),
                rusqlite::types::ToSqlOutput::from(entry.oid.as_bytes().as_slice()),
                rusqlite::types::ToSqlOutput::from(shard.as_str()),
            ]
        });
        tx.execute(&sql, params_from_iter(params))
            .with_state_context(|| format!("{verb} {} desired-state row(s)", chunk.len()))?;
    }
    Ok(())
}

/// Every path currently desired under `shard_id`, for finding rows whose
/// desired half needs clearing when a shard is reparsed or disappears.
fn desired_shard_paths_tx(
    tx: &rusqlite::Transaction<'_>,
    shard_id: LockShardId,
) -> Result<Vec<GatPath>> {
    let shard_id = CanonicalShardIdBuf::encode(shard_id);
    tx.prepare("SELECT path FROM state WHERE desired_shard_id = ?1")
        .state_context("preparing desired-shard path query")?
        .query_map([shard_id.as_str()], |row| row.get::<_, String>(0))
        .state_context("querying desired-shard paths")?
        .map(|row| {
            super::decode_path(
                row.state_context("reading desired-shard paths")?,
                "desired-shard row",
            )
        })
        .collect()
}

/// Clear the desired half of each exact path, preserving materialized state.
/// Rows without a materialized half are deleted directly.
fn clear_desired_exact_tx(tx: &rusqlite::Transaction<'_>, paths: &[GatPath]) -> Result<()> {
    super::clear_exact_paths_tx(
        tx,
        paths,
        "DELETE FROM state WHERE path = ?1 AND materialized_oid IS NULL",
        "UPDATE state SET desired_oid = NULL, desired_shard_id = NULL
         WHERE path = ?1 AND desired_oid IS NOT NULL",
    )
}

/// Clear the desired half of the row at `path` and every desired row nested
/// under it (`path/...`) within an already-open transaction, returning the
/// paths whose desired half was actually cleared. This is the desired-side
/// counterpart of [`remove_prefix_tx`]: same lexical, case-sensitive
/// exact-or-descendant matching, same "prune the row entirely if both halves
/// are now NULL" rule, just against `desired_*` instead of `materialized_*`.
#[cfg(any(test, feature = "test-support"))]
fn remove_desired_prefix_tx(
    tx: &rusqlite::Transaction<'_>,
    path: &GatPath,
) -> Result<Vec<GatPath>> {
    let path_str = path.as_str();
    let (lower, upper) = descendant_range(path_str);
    let removed: Vec<String> = tx
        .prepare(
            "SELECT path FROM state
             WHERE desired_oid IS NOT NULL
               AND (path = ?1 OR (path >= ?2 AND path < ?3))",
        )
        .state_context("preparing desired-state prefix query")?
        .query_map((path_str, &lower, &upper), |row| row.get(0))
        .state_context("querying desired-state rows to remove")?
        .collect::<rusqlite::Result<_>>()
        .state_context("reading desired-state rows to remove")?;
    if !removed.is_empty() {
        clear_desired_prefix_tx(tx, path)?;
    }
    removed
        .into_iter()
        .map(|s| super::decode_path(s, "removed desired-state row"))
        .collect()
}

fn clear_desired_prefix_tx(tx: &rusqlite::Transaction<'_>, path: &GatPath) -> Result<()> {
    let path = path.as_str();
    let (lower, upper) = descendant_range(path);
    tx.execute(
        "DELETE FROM state WHERE (path = ?1 OR (path >= ?2 AND path < ?3))
         AND materialized_oid IS NULL",
        (path, &lower, &upper),
    )
    .state_context("pruning emptied state rows")?;
    tx.execute(
        "UPDATE state
         SET desired_oid = NULL,
             desired_shard_id = NULL
         WHERE desired_oid IS NOT NULL
           AND (path = ?1 OR (path >= ?2 AND path < ?3))",
        (path, &lower, &upper),
    )
    .state_context("clearing desired-state rows")?;
    Ok(())
}

fn desired_shard_ids_for_paths_inner<'a>(
    conn: &Connection,
    paths: impl IntoIterator<Item = &'a GatPath>,
) -> Result<std::collections::BTreeSet<LockShardId>> {
    let mut paths = paths.into_iter();
    let chunk_size = sql_chunk_size(1);
    let mut chunk = Vec::with_capacity(chunk_size);
    let mut shard_ids = std::collections::BTreeSet::new();
    loop {
        chunk.clear();
        chunk.extend(paths.by_ref().take(chunk_size));
        if chunk.is_empty() {
            break;
        }
        let placeholders = sql_placeholders("?", chunk.len());
        let mut stmt = conn
            .prepare(&format!(
                "SELECT DISTINCT desired_shard_id FROM state
                 WHERE path IN ({placeholders}) AND desired_shard_id IS NOT NULL"
            ))
            .state_context("preparing desired-shard-id lookup")?;
        query_shard_ids(
            &mut stmt,
            params_from_iter(chunk.iter().map(|p| p.as_str())),
            &mut shard_ids,
        )?;
    }
    Ok(shard_ids)
}

/// Discover destination publication work without materializing displaced rows.
pub(super) fn desired_shard_ids_for_scope_inner(
    conn: &Connection,
    scope: &GatPath,
) -> Result<std::collections::BTreeSet<LockShardId>> {
    let (lower, upper) = descendant_range(scope.as_str());
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT desired_shard_id FROM state
             WHERE desired_shard_id IS NOT NULL
             AND (path = ?1 OR (path >= ?2 AND path < ?3))",
        )
        .state_context("preparing desired-shard-id lookup")?;
    let mut shard_ids = std::collections::BTreeSet::new();
    query_shard_ids(&mut stmt, (scope.as_str(), lower, upper), &mut shard_ids)?;
    Ok(shard_ids)
}

fn query_shard_ids(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
    shard_ids: &mut std::collections::BTreeSet<LockShardId>,
) -> Result<()> {
    let rows = stmt
        .query_map(params, |row| row.get::<_, String>(0))
        .state_context("querying desired-shard-id lookup")?;
    for row in rows {
        let text = row.state_context("reading desired-shard-id row")?;
        shard_ids.insert(decode_shard_id(&text, "desired_shard_id")?);
    }
    Ok(())
}

fn desired_rows_by_shard_ids_inner(
    conn: &Connection,
    shard_ids: &std::collections::BTreeSet<LockShardId>,
) -> Result<std::collections::BTreeMap<LockShardId, Vec<Entry>>> {
    let mut rows_by_shard: std::collections::BTreeMap<LockShardId, Vec<Entry>> =
        std::collections::BTreeMap::new();
    // Never materializes a textual collection proportional to the
    // complete requested shard-ID set: `bufs` is a small, reused buffer
    // filled directly from the caller's `BTreeSet` iterator, one bounded
    // chunk at a time, without first collecting the whole set into a
    // `Vec<&LockShardId>`.
    let chunk_size = sql_chunk_size(1);
    let mut bufs: Vec<CanonicalShardIdBuf> = Vec::with_capacity(chunk_size);
    let mut ids = shard_ids.iter();
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
        let sql = format!(
            "SELECT desired_shard_id, path, desired_oid FROM state
             WHERE desired_shard_id IN ({placeholders})
             ORDER BY desired_shard_id, path"
        );
        let mut stmt = conn
            .prepare(&sql)
            .state_context("preparing desired-shard row query")?;
        let mut rows = stmt
            .query(params_from_iter(
                bufs.iter().map(CanonicalShardIdBuf::as_str),
            ))
            .state_context("querying desired-shard rows")?;
        // `ORDER BY desired_shard_id, path` means every row sharing one
        // spelling arrives contiguously: `last_shard` remembers that
        // spelling next to its already-parsed `LockShardId` so a whole
        // group is priced as one `parse_canonical` call (plus allocation-
        // free `LockShardId: PartialEq<str>` comparisons for the rest of
        // the group) rather than once per row. Reset per chunk since a
        // new statement's first row always needs its own comparison.
        let mut last_shard: Option<(String, LockShardId)> = None;
        while let Some(row) = rows.next().state_context("reading desired-shard row")? {
            let raw = row
                .get_ref(0)
                .state_context("reading desired-shard row")?
                .as_str()
                .map_err(rusqlite::Error::from)
                .state_context("reading desired-shard row")?;
            let shard_id = match &last_shard {
                Some((last_raw, id)) if last_raw == raw => *id,
                _ => {
                    let id = decode_shard_id(raw, "desired_shard_id")?;
                    last_shard = Some((raw.to_string(), id));
                    id
                }
            };
            let path: String = row.get(1).state_context("reading desired-shard row")?;
            let oid = super::blob_column(row, 2).state_context("reading desired-shard row")?;
            rows_by_shard
                .entry(shard_id)
                .or_default()
                .push(decode_row_raw(path, oid, None)?.into_entry());
        }
    }
    Ok(rows_by_shard)
}

/// Clear the desired half of every row currently attributed to
/// `shard_id` -- used when a shard file disappears entirely.
pub(super) fn clear_shard_desired_tx(
    tx: &rusqlite::Transaction<'_>,
    shard_id: LockShardId,
) -> Result<()> {
    let paths = desired_shard_paths_tx(tx, shard_id)?;
    clear_desired_exact_tx(tx, &paths)
}

/// Reconcile one shard's desired rows in `state` against `entries` (that
/// shard's freshly reparsed content): clear the desired half of any path
/// attributed to `shard_id` but absent from the new set, then
/// upsert the desired half (`oid/shard_id`) for every entry. A path's
/// materialized half (if any) is never touched by this -- only
/// `desired_*` columns are written -- and `desired_shard_id` is rewritten
/// unconditionally even when `desired_oid` doesn't change, so pure
/// resharding (same `(path, oid)`, different shard provenance) never flips
/// `dirty` (a `GENERATED` column derived only from oid, never `shard_id`).
pub(super) fn apply_shard_entries_tx(
    tx: &rusqlite::Transaction<'_>,
    shard_id: LockShardId,
    entries: &[Entry],
) -> Result<()> {
    let new_paths: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.path.as_str()).collect();
    let stale: Vec<GatPath> = desired_shard_paths_tx(tx, shard_id)?
        .into_iter()
        .filter(|p| !new_paths.contains(p.as_str()))
        .collect();
    if !stale.is_empty() {
        clear_desired_exact_tx(tx, &stale)?;
    }

    // `shard_id` is identical for every row this call writes -- encode it
    // once into a fixed-capacity buffer and bind it through a single
    // shared numbered SQL parameter per statement, instead of re-encoding
    // and binding the same canonical string once per row.
    let shard_buf = CanonicalShardIdBuf::encode(shard_id);
    let chunk_size = sql_chunk_size_with_shared_binds(2, 1);
    for chunk in entries.chunks(chunk_size) {
        // Bind the shared value first. A high-numbered parameter in the first
        // tuple makes SQLite search its growing variable list for subsequent
        // lower-numbered parameters, causing quadratic statement preparation.
        // Anonymous row parameters advance monotonically; repeated ?1 lookups
        // always find the first variable.
        let placeholders = sql_placeholders("(?1, ?, ?)", chunk.len());
        let sql = format!(
            "INSERT INTO state (desired_shard_id, path, desired_oid)
             VALUES {placeholders}
             ON CONFLICT(path) DO UPDATE SET
                 desired_oid = excluded.desired_oid,
                 desired_shard_id = excluded.desired_shard_id"
        );
        let params = std::iter::once(rusqlite::types::ToSqlOutput::from(shard_buf.as_str())).chain(
            chunk.iter().flat_map(|entry| {
                [
                    rusqlite::types::ToSqlOutput::from(entry.path.as_str()),
                    rusqlite::types::ToSqlOutput::from(entry.oid.as_bytes().as_slice()),
                ]
            }),
        );
        tx.execute(&sql, params_from_iter(params))
            .with_state_context(|| format!("upserting {} desired-state row(s)", chunk.len()))?;
    }
    Ok(())
}

impl StateStore {
    /// Test-support seam for seeding desired rows without exposing the
    /// transaction that owns their `SQLite` mutation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn upsert_desired_for_test(
        &mut self,
        entries: &[Entry],
        shard_levels: crate::lock::LockShardLevels,
    ) -> Result<()> {
        self.desired_write(|write| write.upsert_entries(entries, shard_levels))
    }

    /// Publish a complete logical desired state at the configured shard
    /// depth and update this store's desired mirror from the publication
    /// receipt under one repository lock.
    pub fn publish_desired_complete<E>(
        &mut self,
        layout: &crate::RepositoryLayout,
        lock: &Lock,
        shard_levels: crate::lock::LockShardLevels,
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError> + From<crate::atomic::AtomicError>,
    {
        let _guard = crate::atomic::RepoLock::acquire_repository(layout).map_err(E::from)?;
        let evidence = LockStore::publish_complete_with_evidence(layout, lock, shard_levels)
            .map_err(E::from)?;
        self.apply_full_lock_evidence(evidence).map_err(E::from)
    }

    /// Rebuild the desired mirror from the complete logical lock currently
    /// on disk without exposing physical shard evidence to the caller.
    pub fn observe_desired_complete<E>(
        &mut self,
        layout: &crate::RepositoryLayout,
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        let evidence =
            LockStore::observe_full_with_evidence(layout.root_path()).map_err(E::from)?;
        self.apply_full_lock_evidence(evidence).map_err(E::from)
    }

    /// Upsert semantic desired entries and publish exactly the old and new
    /// shards affected by their paths in one `SQLite` transaction.
    pub fn publish_desired_upsert<E>(
        &mut self,
        layout: &crate::RepositoryLayout,
        shape_lock: &LockWriteGuard,
        entries: &[Entry],
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        self.desired_write(|desired| desired.upsert_and_publish(layout, shape_lock, entries))
    }

    /// Apply prepared add windows in one transaction, then publish each touched
    /// shard once. The caller retains proofs until publication has succeeded.
    pub(crate) fn publish_prepared_add<E>(
        &mut self,
        layout: &crate::RepositoryLayout,
        shape_lock: &LockWriteGuard,
        entries: &[super::repository::PreparedMaterialization],
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        self.desired_write(|desired| {
            let mut touched = std::collections::BTreeSet::new();
            for window in entries.chunks(4096) {
                let paths: Vec<_> = window
                    .iter()
                    .filter(|entry| !entry.unchanged_desired())
                    .map(|entry| entry.entry().path.clone())
                    .collect();
                if paths.is_empty() {
                    continue;
                }
                let existing = desired
                    .desired_rows(DesiredQuery::exact(&paths))
                    .map_err(E::from)?;
                let existing: std::collections::HashMap<_, _> =
                    existing.iter().map(|entry| (&entry.path, entry)).collect();
                let batch: Vec<_> = window
                    .iter()
                    .filter(|entry| !entry.unchanged_desired())
                    .map(super::repository::PreparedMaterialization::entry)
                    .filter(|entry| {
                        existing
                            .get(&entry.path)
                            .is_none_or(|prior| *prior != *entry)
                    })
                    .cloned()
                    .collect();
                if batch.is_empty() {
                    continue;
                }
                touched.extend(
                    desired
                        .desired_shard_ids_for_paths(batch.iter().map(|entry| &entry.path))
                        .map_err(E::from)?,
                );
                desired
                    .upsert_entries(
                        &batch,
                        DesiredStateWrite::incremental_shard_levels(shape_lock),
                    )
                    .map_err(E::from)?;
                touched.extend(
                    desired
                        .desired_shard_ids_for_paths(batch.iter().map(|entry| &entry.path))
                        .map_err(E::from)?,
                );
            }
            desired.publish_touched(layout, shape_lock, &touched)
        })
    }

    /// Apply exact and indexed-prefix desired removals, then publish the
    /// affected shards once from the transaction's post-mutation view.
    pub fn publish_desired_removals<E>(
        &mut self,
        layout: &crate::RepositoryLayout,
        shape_lock: &LockWriteGuard,
        removals: &[DesiredRemoval<'_>],
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        self.desired_write(|desired| desired.remove_and_publish(layout, shape_lock, removals))
    }

    /// Move a desired prefix and publish the union of its prior source,
    /// overwritten destination, and resulting destination shards.
    #[cfg(any(test, feature = "test-support"))]
    pub fn publish_desired_move<E>(
        &mut self,
        layout: &crate::RepositoryLayout,
        shape_lock: &LockWriteGuard,
        src: &GatPath,
        dst: &GatPath,
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        self.desired_write(|desired| {
            let touched = desired
                .move_repository_rows(
                    src,
                    dst,
                    DesiredStateWrite::incremental_shard_levels(shape_lock),
                )
                .map_err(E::from)?;
            desired.publish_touched(layout, shape_lock, &touched)
        })
    }

    /// Replay bounded desired-entry windows using the publication strategy
    /// appropriate to the locked shape. Flat state is upserted in one
    /// transaction and streamed to disk once; sharded state preserves
    /// per-window publication and progress/fault boundaries.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_desired_upsert_windows<E>(
        &mut self,
        layout: &crate::RepositoryLayout,
        shape_lock: &LockWriteGuard,
        mut next_window: impl FnMut() -> std::result::Result<Option<Vec<Entry>>, E>,
        imported: &mut usize,
        maybe_published: &mut bool,
        mut after_window: impl FnMut() -> std::result::Result<(), E>,
        after_flat_publish: impl FnOnce() -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E>
    where
        E: From<StateStoreError> + From<LockError>,
    {
        if shape_lock.is_flat() {
            let imported_here = self.desired_write(|desired| {
                let mut count = 0usize;
                while let Some(batch) = next_window()? {
                    if batch.is_empty() {
                        continue;
                    }
                    desired
                        .upsert_entries(
                            &batch,
                            DesiredStateWrite::incremental_shard_levels(shape_lock),
                        )
                        .map_err(E::from)?;
                    count += batch.len();
                }
                if count == 0 {
                    return Ok::<usize, E>(0);
                }
                *maybe_published = true;
                desired.publish_flat_streaming::<E>(layout)?;
                after_flat_publish()?;
                Ok::<usize, E>(count)
            })?;
            *imported += imported_here;
            if imported_here != 0 {
                after_window()?;
            }
            return Ok(());
        }

        while let Some(batch) = next_window()? {
            if batch.is_empty() {
                continue;
            }
            let count = batch.len();
            *maybe_published = true;
            self.publish_desired_upsert::<E>(layout, shape_lock, &batch)?;
            *imported += count;
            after_window()?;
        }
        Ok(())
    }

    /// Clear the desired half of the row at `path` and every desired row
    /// nested under it (`path/...`), returning the paths whose desired half
    /// was actually cleared. Like the materialized-side
    /// [`Self::remove_prefix`], this is a single scoped mutation backed by one
    /// lexical range query and one transaction, not a full desired-lock scan
    /// followed by `Vec::retain`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn remove_desired_prefix(&mut self, path: &GatPath) -> Result<Vec<GatPath>> {
        let tx = self
            .conn
            .transaction()
            .state_context("beginning desired-state transaction")?;
        let removed = remove_desired_prefix_tx(&tx, path)?;
        tx.commit()
            .state_context("committing desired-state removal")?;
        Ok(removed)
    }

    /// Every distinct `desired_shard_id` currently attached to `paths`,
    /// deduplicated in `SQLite` rather than by loading and bucketing the whole
    /// desired mirror in Rust. Sharded `gat rm`/`gat mv` use the before/after
    /// sets from this query to know exactly which shard files might need
    /// rewriting.
    #[cfg(any(test, feature = "test-support"))]
    pub fn desired_shard_ids_for_paths<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a GatPath>,
    ) -> Result<std::collections::BTreeSet<LockShardId>> {
        desired_shard_ids_for_paths_inner(&self.conn, paths)
    }

    /// The full current row set for each shard in `shard_ids`, grouped by
    /// `desired_shard_id`. Sparse sharded publication asks `SQLite` for exactly
    /// these touched shards' post-mutation contents and no others, then
    /// renders only those shard files.
    #[cfg(any(test, feature = "test-support"))]
    pub fn desired_rows_by_shard_ids(
        &self,
        shard_ids: &std::collections::BTreeSet<LockShardId>,
    ) -> Result<std::collections::BTreeMap<LockShardId, Vec<Entry>>> {
        desired_rows_by_shard_ids_inner(&self.conn, shard_ids)
    }

    /// Run `f` inside one explicit desired-state transaction, committing only
    /// if `f` returns `Ok`. Sharded `gat rm`/`gat mv` use this to keep the
    /// `SQLite` desired mirror and the sparse touched-shard filesystem publish
    /// on one failure boundary: if writing any shard file fails, the SQL
    /// mutation rolls back instead of leaving `SQLite` claiming a desired state
    /// the on-disk `gat.lock/` tree never actually reached.
    pub(crate) fn desired_write<T, E>(
        &mut self,
        f: impl FnOnce(&DesiredStateWrite<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StateStoreError>,
    {
        let tx = self
            .conn
            .transaction()
            .state_context("beginning desired-state transaction")
            .map_err(E::from)?;
        let desired = DesiredStateWrite { tx };
        let out = f(&desired)?;
        desired
            .tx
            .commit()
            .state_context("committing desired-state mutation")
            .map_err(E::from)?;
        Ok(out)
    }

    /// The complete desired mirror as a [`Lock`], sorted by path -- reads
    /// straight from `SQLite` instead of reparsing/sorting `gat.lock` from
    /// disk. The caller must first refresh the mirror from every current shard.
    ///
    /// **Full materialization is deliberate here and only here.** This is
    /// the persistence/interchange representation: use it for
    /// an operation whose actual result *is* the complete desired state
    /// (a flat-shape full rewrite, a reshape, an explicit cross-shape
    /// fallback). Every other reader -- selection-aware or not -- should
    /// express its intent as a [`DesiredQuery`] and stream/collect through
    /// [`Self::with_desired_rows`]/[`Self::desired_rows`] instead.
    ///
    pub fn load_desired_as_lock(&self) -> Result<Lock> {
        Ok(Lock {
            entries: self.desired_rows(DesiredQuery::all())?,
        })
    }

    /// Every desired path, in lexical order, as a cursor rather than a
    /// `Vec<Entry>` -- the `path` column alone, with no `desired_oid`
    /// decode. `path` is this table's primary key, so the
    /// cursor is already both sorted and duplicate-free with no extra
    /// `sort`/`dedup` pass needed downstream (see
    /// `gat_engine`'s excludes synchronization, the reason this exists:
    /// regenerating `.git/info/exclude` from desired state needs
    /// nothing but ordered paths, so it shouldn't have to pay for a full
    /// [`Self::load_desired_as_lock`] materialization first).
    pub fn with_desired_paths<T, E>(
        &self,
        f: impl FnOnce(DesiredPaths<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StateStoreError>,
    {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM state WHERE desired_oid IS NOT NULL ORDER BY path")
            .state_context("preparing desired-path cursor")
            .map_err(E::from)?;
        let rows = stmt
            .query([])
            .state_context("opening desired-path cursor")
            .map_err(E::from)?;
        f(DesiredPaths { rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_scope_lookup_includes_exact_and_nested_paths_but_not_prefix_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        let mut store = StateStore::open(&layout).unwrap();
        let cases = [
            ("target", "gat.lock/01.tsv"),
            ("target/child", "gat.lock/02.tsv"),
            ("target/deep/child", "gat.lock/02.tsv"),
            ("target.other", "gat.lock/03.tsv"),
            ("target0/child", "gat.lock/04.tsv"),
        ];
        for (path, shard) in cases {
            let tx = store.conn.transaction().unwrap();
            apply_shard_entries_tx(
                &tx,
                LockShardId::parse_canonical(shard).unwrap(),
                &[Entry {
                    path: GatPath::parse_canonical(path).unwrap(),
                    oid: gat_core::oid::Oid::from_bytes([0; 32]),
                }],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let actual = desired_shard_ids_for_scope_inner(
            &store.conn,
            &GatPath::parse_canonical("target").unwrap(),
        )
        .unwrap();
        let expected = ["gat.lock/01.tsv", "gat.lock/02.tsv"]
            .into_iter()
            .map(|raw| LockShardId::parse_canonical(raw).unwrap())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn shard_refresh_binds_shared_id_across_chunks_and_clears_stale_rows() {
        let temp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(temp.path().to_path_buf());
        let mut store = StateStore::open(&layout).unwrap();
        let shard = LockShardId::flat();
        let entries = (0..sql_chunk_size_with_shared_binds(2, 1) + 3)
            .map(|index| Entry {
                path: GatPath::parse_canonical(&format!("file-{index:06}")).unwrap(),
                oid: gat_core::oid::Oid::from_bytes(*blake3::hash(&index.to_le_bytes()).as_bytes()),
            })
            .collect::<Vec<_>>();
        for expected in [entries.as_slice(), &entries[..3], &entries[..0]] {
            let tx = store.conn.transaction().unwrap();
            apply_shard_entries_tx(&tx, shard, expected).unwrap();
            tx.commit().unwrap();
            assert_eq!(store.load_desired_as_lock().unwrap().entries, expected);
        }
    }
}
