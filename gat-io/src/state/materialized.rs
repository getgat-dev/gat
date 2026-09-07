use rusqlite::types::{ToSqlOutput, ValueRef};

#[cfg(any(test, feature = "test-support"))]
use super::Entry;
use super::{
    GatPath, Lock, MaterializedRow, MaterializedRows, Result, StatProof, StateMutation,
    StateMutationKind, StateResultExt, StateStore, StateStoreError, descendant_range,
    encode_stat_proof, load_materialized_rows, params_from_iter, sql_chunk_size, sql_placeholders,
};

/// Insert or update the *materialized* half of `rows` inside `tx`
/// using chunked, multi-row `INSERT ... ON CONFLICT DO UPDATE` statements
/// instead of one prepared-statement `execute()` per row. Never touches
/// `desired_oid`/`desired_shard_id` -- a fresh row gets them as `NULL`, an
/// existing row keeps whatever it already had.
fn bulk_upsert_materialized(
    tx: &rusqlite::Transaction<'_>,
    rows: &[MaterializedRow],
) -> Result<()> {
    for chunk in rows.chunks(sql_chunk_size(3)) {
        let placeholders = sql_placeholders("(?, ?, ?)", chunk.len());
        let sql = format!(
            "INSERT INTO state (
                 path,
                 materialized_oid,
                 materialized_proof
             ) VALUES {placeholders}
             ON CONFLICT(path) DO UPDATE SET
                 materialized_oid = excluded.materialized_oid,
                 materialized_proof = excluded.materialized_proof"
        );
        // Borrow paths and OIDs; only encoded proofs need temporary storage.
        // Keep those fixed-size values together instead of allocating per row.
        let proofs: Vec<_> = chunk
            .iter()
            .map(|row| row.proof.as_ref().map(encode_stat_proof))
            .collect();
        let params = chunk.iter().zip(&proofs).flat_map(|(row, proof)| {
            [
                ValueRef::Text(row.path.as_str().as_bytes()),
                ValueRef::Blob(row.oid.as_bytes()),
                proof
                    .as_ref()
                    .map_or(ValueRef::Null, |proof| ValueRef::Blob(proof)),
            ]
        });
        tx.execute(&sql, params_from_iter(params.map(ToSqlOutput::Borrowed)))
            .with_state_context(|| {
                format!("upserting {} materialized-state row(s)", chunk.len())
            })?;
    }
    Ok(())
}

/// Refresh only `materialized_proof` for already-materialized rows, in
/// chunked set-based `UPDATE`s rather than one statement per row. Rows
/// whose `materialized_oid` is `NULL` remain untouched.
fn bulk_refresh_materialized_proofs(
    tx: &rusqlite::Transaction<'_>,
    proofs: &[(&GatPath, StatProof)],
) -> Result<()> {
    for chunk in proofs.chunks(sql_chunk_size(2)) {
        let values = sql_placeholders("(?, ?)", chunk.len());
        // `UPDATE ... FROM v` (rather than a `SET x = (SELECT ... WHERE
        // v.path = state.path)` correlated subquery) lets SQLite plan
        // this as a single scan-of-`v`-then-indexed-lookup-into-`state`
        // join: the correlated-subquery form re-scanned all of `v` once
        // per matched `state` row (no index on the ephemeral `v` table),
        // making one chunk's cost quadratic in chunk size.
        let sql = format!(
            "WITH v(path, proof) AS (VALUES {values})
             UPDATE state
             SET materialized_proof = v.proof
             FROM v
             WHERE state.path = v.path AND state.materialized_oid IS NOT NULL"
        );
        let encoded: Vec<_> = chunk
            .iter()
            .map(|(_, proof)| encode_stat_proof(proof))
            .collect();
        let params = chunk.iter().zip(&encoded).flat_map(|((path, _), proof)| {
            [
                ValueRef::Text(path.as_str().as_bytes()),
                ValueRef::Blob(proof),
            ]
        });
        tx.execute(&sql, params_from_iter(params.map(ToSqlOutput::Borrowed)))
            .with_state_context(|| {
                format!(
                    "refreshing materialized stat cache for {} row(s)",
                    chunk.len()
                )
            })?;
    }
    Ok(())
}

/// Clear the *materialized* half of exactly the rows named by `paths`
/// (an `IN (...)` list), then delete any of those rows that are now
/// entirely empty (`desired_oid`/`materialized_oid` both `NULL`) -- a row
/// only exists to record a non-`NULL` desired or materialized half, never
/// as a bare tombstone.
fn clear_materialized_exact_tx(tx: &rusqlite::Transaction<'_>, paths: &[&GatPath]) -> Result<()> {
    for chunk in paths.chunks(sql_chunk_size(1)) {
        let placeholders = sql_placeholders("?", chunk.len());
        tx.execute(
            &format!(
                "UPDATE state
                 SET materialized_oid = NULL,
                     materialized_proof = NULL
                 WHERE path IN ({placeholders})"
            ),
            params_from_iter(chunk.iter().map(|p| p.as_str())),
        )
        .with_state_context(|| format!("clearing {} materialized-state row(s)", chunk.len()))?;
        tx.execute(
            &format!(
                "DELETE FROM state WHERE path IN ({placeholders})
                 AND desired_oid IS NULL AND materialized_oid IS NULL"
            ),
            params_from_iter(chunk.iter().map(|p| p.as_str())),
        )
        .with_state_context(|| format!("pruning {} emptied state row(s)", chunk.len()))?;
    }
    Ok(())
}

/// Clear the materialized half of a lexical subtree without first
/// collecting its paths.
///
/// Mount recovery repeats this even when an earlier attempt already removed
/// the desired rows, making derived-state cleanup independently idempotent.
fn clear_materialized_prefix_tx(tx: &rusqlite::Transaction<'_>, path: &GatPath) -> Result<()> {
    let path = path.as_str();
    let (lower, upper) = descendant_range(path);
    tx.execute(
        "UPDATE state
         SET materialized_oid = NULL,
             materialized_proof = NULL
         WHERE materialized_oid IS NOT NULL
           AND (path = ?1 OR (path >= ?2 AND path < ?3))",
        (path, &lower, &upper),
    )
    .state_context("clearing materialized-state rows")?;
    tx.execute(
        "DELETE FROM state WHERE (path = ?1 OR (path >= ?2 AND path < ?3))
         AND desired_oid IS NULL AND materialized_oid IS NULL",
        (path, &lower, &upper),
    )
    .state_context("pruning emptied state rows")?;
    Ok(())
}

/// Clear the materialized half of the row at `path` and every row
/// nested under it (`path/...`) within an already-open transaction,
/// returning the paths actually cleared. Used by
/// [`StateStore::remove_prefix`], a test-only single-path
/// convenience.
#[cfg(any(test, feature = "test-support"))]
fn remove_prefix_tx(tx: &rusqlite::Transaction<'_>, path: &GatPath) -> Result<Vec<GatPath>> {
    let path_text = path.as_str();
    let (lower, upper) = descendant_range(path_text);
    let removed: Vec<String> = tx
        .prepare(
            "SELECT path FROM state
             WHERE materialized_oid IS NOT NULL
               AND (path = ?1 OR (path >= ?2 AND path < ?3))",
        )
        .state_context("preparing materialized-state prefix query")?
        .query_map((path_text, &lower, &upper), |row| row.get(0))
        .state_context("querying materialized-state rows to remove")?
        .collect::<rusqlite::Result<_>>()
        .state_context("reading materialized-state rows to remove")?;
    clear_materialized_prefix_tx(tx, path)?;
    removed
        .into_iter()
        .map(|s| super::decode_path(s, "removed materialized-state row"))
        .collect()
}

impl StateStore {
    /// Load every materialized row, sorted by path -- the same order a
    /// flat `gat.lock`-format file's rows were always written in. This is
    /// the full-scan path for whole-repo operations; scoped reads should
    /// prefer [`Self::with_rows_in_scope`].
    pub fn load_all(&self) -> Result<Lock> {
        Ok(Lock {
            entries: self
                .load_all_raw()?
                .into_iter()
                .map(MaterializedRow::into_entry)
                .collect(),
        })
    }

    pub fn load_all_raw(&self) -> Result<Vec<MaterializedRow>> {
        load_materialized_rows(
            &self.conn,
            "SELECT path, materialized_oid, materialized_proof
             FROM state WHERE materialized_oid IS NOT NULL ORDER BY path",
            [],
        )
    }

    /// Load exactly the row at `scope` plus every row nested under it,
    /// matching the same exact-file/subtree semantics as
    /// [`gat_core::lock::path_matches_scope`] but expressed as the
    /// lexical byte range from `descendant_range` so `SQLite` work scales
    /// with rows in scope rather than total rows in the state table.
    ///
    /// Only used by tests (production planning uses
    /// [`Self::with_rows_in_scope`] directly to avoid collecting a `Vec`);
    /// kept as the `Entry`-typed counterpart to [`Self::load_all`] for
    /// equivalence assertions.
    #[cfg(any(test, feature = "test-support"))]
    pub fn load_scope(&self, scope: &str) -> Result<Vec<Entry>> {
        Ok(self
            .load_scope_raw(scope)?
            .into_iter()
            .map(MaterializedRow::into_entry)
            .collect())
    }

    /// Only used by tests/equivalence assertions -- production scoped
    /// reads go through [`Self::with_rows_in_scope`], which never
    /// collects the scan into a `Vec`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn load_scope_raw(&self, scope: &str) -> Result<Vec<MaterializedRow>> {
        let scope = scope.strip_suffix('/').unwrap_or(scope);
        let (lower, upper) = descendant_range(scope);
        load_materialized_rows(
            &self.conn,
            "SELECT path, materialized_oid, materialized_proof
             FROM state
             WHERE materialized_oid IS NOT NULL
               AND (path = ?1 OR (path >= ?2 AND path < ?3))
             ORDER BY path",
            (scope, &lower, &upper),
        )
    }

    /// Batched, exact-path counterpart to [`Self::with_rows_in_scope`]: rows
    /// for exactly the paths in `paths` (no prefix/descendant expansion),
    /// in `IN (...)` chunks of `sql_chunk_size` rather than one query
    /// per path -- callers like `gat add`'s stat-first identity check
    /// look up a batch of explicitly selected files at once,
    /// so this scales with the number of `IN (...)` round trips, never
    /// with total rows in `state`.
    /// Order is whatever `SQLite` returns for each chunk, not `paths`'
    /// order -- callers that need a specific path's row should index the
    /// result by `path` rather than relying on position.
    pub fn materialized_rows_for(
        &self,
        paths: &[gat_core::lexical_path::GatPath],
    ) -> Result<Vec<MaterializedRow>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut rows = Vec::with_capacity(paths.len());
        for chunk in paths.chunks(sql_chunk_size(1)) {
            let placeholders = sql_placeholders("?", chunk.len());
            rows.extend(load_materialized_rows(
                &self.conn,
                &format!(
                    "SELECT path, materialized_oid, materialized_proof
                     FROM state
                     WHERE materialized_oid IS NOT NULL AND path IN ({placeholders})"
                ),
                params_from_iter(chunk.iter().map(gat_core::lexical_path::GatPath::as_str)),
            )?);
        }
        Ok(rows)
    }

    /// Run `f` against a `path`-ordered cursor over either every
    /// materialized row (`scope = None`) or exactly `scope` plus
    /// everything nested under it (`scope = Some(...)`, same lexical
    /// byte-range semantics as `descendant_range`) -- without first
    /// collecting the scan into a `Vec<MaterializedRow>`.
    ///
    /// This is the production read path for sync planning
    /// (engine sync planning): the `rusqlite::Statement`/`Rows` backing
    /// the cursor stay borrowed for the duration of `f` and never escape
    /// this call, so a full-repo plan avoids holding a second
    /// complete materialized snapshot in memory alongside the desired
    /// state. `load_all_raw`/`load_scope_raw` remain for tests/equivalence
    /// assertions and any caller that genuinely wants a `Vec`.
    pub fn with_rows_in_scope<T, E>(
        &self,
        scope: Option<&gat_core::lexical_path::GatPath>,
        f: impl FnOnce(MaterializedRows<'_>) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<StateStoreError>,
    {
        if let Some(scope) = scope {
            let (lower, upper) = descendant_range(scope.as_str());
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT path, materialized_oid, materialized_proof
                     FROM state
                     WHERE materialized_oid IS NOT NULL
                       AND (path = ?1 OR (path >= ?2 AND path < ?3))
                     ORDER BY path",
                )
                .state_context("preparing materialized-state scope cursor")
                .map_err(E::from)?;
            let rows = stmt
                .query((scope.as_str(), &lower, &upper))
                .state_context("opening materialized-state scope cursor")
                .map_err(E::from)?;
            f(MaterializedRows { rows })
        } else {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT path, materialized_oid, materialized_proof
                     FROM state
                     WHERE materialized_oid IS NOT NULL ORDER BY path",
                )
                .state_context("preparing materialized-state cursor")
                .map_err(E::from)?;
            let rows = stmt
                .query([])
                .state_context("opening materialized-state cursor")
                .map_err(E::from)?;
            f(MaterializedRows { rows })
        }
    }

    /// Insert or update the materialized half of rows for `entries` in
    /// one explicit transaction. `dirty` for each affected path is
    /// recomputed by `SQLite` itself (the `GENERATED ALWAYS` column) as
    /// part of this very same statement/transaction -- no separate
    /// dirty-recompute pass or second commit follows this.
    #[cfg(any(test, feature = "test-support"))]
    pub fn upsert_many(&mut self, entries: &[Entry]) -> Result<()> {
        let rows = entries
            .iter()
            .cloned()
            .map(|entry| MaterializedRow::from_entry(entry, None))
            .collect::<Vec<_>>();
        self.upsert_rows(&rows)
    }

    pub fn upsert_rows(&mut self, rows: &[MaterializedRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction()
            .state_context("beginning materialized-state transaction")?;
        bulk_upsert_materialized(&tx, rows)?;
        tx.commit()
            .state_context("committing materialized-state upsert")
    }

    /// Clear the materialized half of exactly the rows for `paths` (no
    /// prefix expansion -- callers resolve directory scopes against
    /// `gat.lock` first and pass the concrete rows affected), via one or
    /// more chunked statements inside one transaction. `dirty` updates
    /// (and, if a row's desired half was already `NULL` too, deletion of
    /// the now-empty row) happen in the same transaction.
    pub fn remove_exact(&mut self, paths: &[GatPath]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction()
            .state_context("beginning materialized-state transaction")?;
        let refs: Vec<&GatPath> = paths.iter().collect();
        clear_materialized_exact_tx(&tx, &refs)?;
        tx.commit()
            .state_context("committing materialized-state removal")
    }

    /// Clear materialized state for one lexical subtree without collecting
    /// the matching paths.
    pub(crate) fn clear_materialized_prefix(&mut self, path: &GatPath) -> Result<()> {
        let tx = self
            .conn
            .transaction()
            .state_context("beginning materialized-state transaction")?;
        clear_materialized_prefix_tx(&tx, path)?;
        tx.commit()
            .state_context("committing materialized-state removal")
    }

    /// Clear the materialized half of the row at `path` and every row
    /// nested under it (`path/...`), returning the paths actually
    /// cleared. See `descendant_range` for the lexical, case-sensitive
    /// matching this uses instead of `LIKE`/`GLOB`. Only a single-path
    /// convenience over `remove_prefix_tx` now (the executor batches
    /// removals via [`Self::apply_batch`] instead), kept for tests that
    /// exercise prefix-removal semantics directly.
    #[cfg(any(test, feature = "test-support"))]
    pub fn remove_prefix(&mut self, path: &GatPath) -> Result<Vec<GatPath>> {
        let tx = self
            .conn
            .transaction()
            .state_context("beginning materialized-state transaction")?;
        let removed = remove_prefix_tx(&tx, path)?;
        tx.commit()
            .state_context("committing materialized-state removal")?;
        Ok(removed)
    }

    /// Persist a bounded batch of materialized-state mutations, in one
    /// transaction, applied in the exact order given in `ops`. This is
    /// a single commit for the whole batch rather than one per mutation:
    /// callers (the sync executor) only push a filesystem mutation's
    /// corresponding [`StateMutation`] here *after* that mutation has
    /// already succeeded, then flush a bounded group of them together --
    /// so a single fresh/large sync durably commits state on the order of
    /// `ops.len()` many mutations divided by the caller's batch size,
    /// rather than once per path.
    ///
    /// Preserving order matters: a file-to-directory (or
    /// directory-to-file) transition can put a `Remove` of an ancestor
    /// path *before* an `Upsert` of a descendant path in the same batch
    /// (e.g. `Remove("a")` then `Upsert("a/b")`). Applying every upsert
    /// before every removal (or vice versa) can make a later
    /// prefix-removal clear the state just written by an earlier upsert.
    /// Contiguous runs of the same variant are still coalesced into one
    /// bulk statement each, so large materialization workloads remain
    /// batched efficiently -- only the relative order between an upsert
    /// run and a removal run is preserved, not per-row batching.
    pub fn apply_batch(&mut self, ops: &[StateMutation]) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction()
            .state_context("beginning batched materialized-state transaction")?;
        let mut i = 0;
        while i < ops.len() {
            match &ops[i].0 {
                StateMutationKind::Upsert(_) => {
                    let start = i;
                    while i < ops.len() && matches!(ops[i].0, StateMutationKind::Upsert(_)) {
                        i += 1;
                    }
                    let rows: Vec<MaterializedRow> = ops[start..i]
                        .iter()
                        .map(|op| match &op.0 {
                            StateMutationKind::Upsert(row) => row.clone(),
                            StateMutationKind::RefreshStat { .. } => unreachable!(),
                            StateMutationKind::RemoveExact(_) => {
                                unreachable!()
                            }
                        })
                        .collect();
                    bulk_upsert_materialized(&tx, &rows)?;
                }
                StateMutationKind::RefreshStat { .. } => {
                    let start = i;
                    while i < ops.len() && matches!(ops[i].0, StateMutationKind::RefreshStat { .. })
                    {
                        i += 1;
                    }
                    let proofs: Vec<(&GatPath, StatProof)> = ops[start..i]
                        .iter()
                        .map(|op| match &op.0 {
                            StateMutationKind::RefreshStat { path, proof } => (path, *proof),
                            StateMutationKind::Upsert(_) | StateMutationKind::RemoveExact(_) => {
                                unreachable!()
                            }
                        })
                        .collect();
                    bulk_refresh_materialized_proofs(&tx, &proofs)?;
                }
                StateMutationKind::RemoveExact(_) => {
                    let start = i;
                    while i < ops.len() && matches!(ops[i].0, StateMutationKind::RemoveExact(_)) {
                        i += 1;
                    }
                    let paths: Vec<&GatPath> = ops[start..i]
                        .iter()
                        .map(|op| match &op.0 {
                            StateMutationKind::RemoveExact(path) => path,
                            StateMutationKind::Upsert(_)
                            | StateMutationKind::RefreshStat { .. } => {
                                unreachable!()
                            }
                        })
                        .collect();
                    clear_materialized_exact_tx(&tx, &paths)?;
                }
            }
        }
        tx.commit()
            .state_context("committing batched materialized-state mutation")
    }

    /// Move the materialized half of every row at `src` (exact path) or
    /// nested under it (`src/...`) to the equivalent path under `dst`, in
    /// one transaction. A destination row's existing desired half (if
    /// any) is left untouched -- only `materialized_oid`/
    /// `materialized_proof` move. Rows already present at a destination
    /// path have their materialized half overwritten (matching `gat mv`'s
    /// existing collision behavior of unconditionally upserting the
    /// moved rows). Rows are read via `descendant_range` once (a stable
    /// snapshot for this transaction), rewritten under `dst`, then the
    /// original `src` rows' materialized half is cleared -- the same
    /// upsert-then-remove order required when `dst` itself falls inside
    /// `src`'s own scope.
    pub fn move_prefix(&mut self, src: &GatPath, dst: &GatPath) -> Result<()> {
        let src = src.as_str();
        let dst = dst.as_str();
        let (lower, upper) = descendant_range(src);
        let tx = self
            .conn
            .transaction()
            .state_context("beginning materialized-state transaction")?;
        let rest_start = i64::try_from(src.len() + 1).map_err(|_| StateStoreError::InvalidRow {
            detail: format!("src path too long ({} bytes)", src.len()),
        })?;
        tx.execute(
            "INSERT INTO state (
                path,
                materialized_oid, materialized_proof
             )
             SELECT CASE WHEN path = ?1 THEN ?2 ELSE ?2 || substr(path, ?3) END,
                   materialized_oid, materialized_proof
             FROM state
             WHERE materialized_oid IS NOT NULL
               AND (path = ?1 OR (path >= ?4 AND path < ?5))
             ON CONFLICT(path) DO UPDATE SET
                 materialized_oid = excluded.materialized_oid,
                 materialized_proof = excluded.materialized_proof",
            (src, dst, rest_start, &lower, &upper),
        )
        .state_context("moving materialized-state rows")?;
        tx.execute(
            "UPDATE state
             SET materialized_oid = NULL,
                materialized_proof = NULL
             WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
            (src, &lower, &upper),
        )
        .state_context("clearing old materialized-state rows after move")?;
        tx.execute(
            "DELETE FROM state WHERE (path = ?1 OR (path >= ?2 AND path < ?3))
             AND desired_oid IS NULL AND materialized_oid IS NULL",
            (src, &lower, &upper),
        )
        .state_context("pruning emptied state rows after move")?;
        tx.commit()
            .state_context("committing materialized-state move")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialized_bindings_preserve_paths_oids_and_optional_proofs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let proof = StatProof {
            size: 42,
            mtime_secs: -7,
            mtime_nanos: 123,
        };
        let mut rows: Vec<_> = ["a'quoted.bin", "b\nline.bin", "é.bin"]
            .into_iter()
            .enumerate()
            .map(|(index, path)| MaterializedRow {
                path: GatPath::parse_canonical(path).unwrap(),
                oid: gat_core::oid::Oid::from_bytes([u8::try_from(index).unwrap(); 32]),
                proof: (index != 1).then_some(proof),
            })
            .collect();
        for _ in 0..2 {
            store.upsert_rows(&rows).unwrap();
            let loaded = store.load_all_raw().unwrap();
            for (actual, expected) in loaded.iter().zip(&rows) {
                assert_eq!(actual.path, expected.path);
                assert_eq!(actual.oid, expected.oid);
                assert_eq!(actual.proof, expected.proof);
            }
            assert_eq!(loaded.len(), rows.len());
            for row in &mut rows {
                row.proof = row.proof.is_none().then_some(proof);
            }
        }
    }
}
