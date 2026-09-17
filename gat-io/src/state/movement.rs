//! Move both halves of repository state together, retaining source rows before
//! clearing either scope so overlapping prefixes are remapped exactly once.

use std::collections::BTreeSet;

use super::{
    CanonicalShardIdBuf, DesiredStateWrite, GatPath, Oid, Result, StateResultExt, decode_path,
    decode_stat_proof, descendant_range, encode_stat_proof, params_from_iter, shard_id_for_path,
    sql_chunk_size, sql_placeholders,
};
use crate::file_state::StatProof;
use crate::lock::{LockShardId, LockShardLevels};
use rusqlite::types::{ToSqlOutput, ValueRef};

struct MovedRow {
    path: GatPath,
    desired: Option<Oid>,
    materialized: Option<Oid>,
    proof: Option<StatProof>,
}

impl DesiredStateWrite<'_> {
    pub(super) fn move_repository_rows(
        &self,
        src: &GatPath,
        dst: &GatPath,
        levels: LockShardLevels,
    ) -> Result<BTreeSet<LockShardId>> {
        let (lower, upper) = descendant_range(src.as_str());
        let mut statement = self
            .tx
            .prepare(
                "SELECT path, desired_oid, materialized_oid, materialized_proof
             FROM state WHERE path = ?1 OR (path >= ?2 AND path < ?3) ORDER BY path",
            )
            .state_context("preparing repository move")?;
        let source = statement
            .query_map((src.as_str(), &lower, &upper), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<[u8; 32]>>(1)?,
                    row.get::<_, Option<[u8; 32]>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })
            .state_context("reading repository move source")?;
        let mut moved = Vec::new();
        for row in source {
            let (path, desired, materialized, proof) =
                row.state_context("reading repository move row")?;
            moved.push(MovedRow {
                path: decode_path(path, "repository move row")?.with_replaced_prefix(src, dst),
                desired: desired.map(Oid::from_bytes),
                materialized: materialized.map(Oid::from_bytes),
                proof: proof.as_deref().and_then(decode_stat_proof),
            });
        }
        drop(statement);
        if !moved.iter().any(|row| row.desired.is_some()) {
            return Ok(BTreeSet::new());
        }
        let mut touched = super::desired::desired_shard_ids_for_scope_inner(&self.tx, src)?;
        touched.extend(super::desired::desired_shard_ids_for_scope_inner(
            &self.tx, dst,
        )?);
        self.tx
            .execute(
                "DELETE FROM state WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
                (src.as_str(), &lower, &upper),
            )
            .state_context("removing repository move source")?;
        let (lower, upper) = descendant_range(dst.as_str());
        // Replaced tracked destinations lose both halves. Materialized-only
        // destinations survive unless an incoming observation replaces them.
        self.tx
            .execute(
                "DELETE FROM state WHERE desired_oid IS NOT NULL
             AND (path = ?1 OR (path >= ?2 AND path < ?3))",
                (dst.as_str(), &lower, &upper),
            )
            .state_context("clearing repository move destination")?;
        for chunk in moved.chunks(sql_chunk_size(5)) {
            let shards = chunk
                .iter()
                .map(|row| {
                    row.desired.map(|_| {
                        let shard = shard_id_for_path(&row.path, levels);
                        touched.insert(shard);
                        CanonicalShardIdBuf::encode(shard)
                    })
                })
                .collect::<Vec<_>>();
            let proofs = chunk
                .iter()
                .map(|row| row.proof.as_ref().map(encode_stat_proof))
                .collect::<Vec<_>>();
            let sql = format!(
                "INSERT INTO state (path, desired_oid, desired_shard_id, materialized_oid, materialized_proof)
                 VALUES {}
                 ON CONFLICT(path) DO UPDATE SET
                     desired_oid = excluded.desired_oid,
                     desired_shard_id = excluded.desired_shard_id,
                     materialized_oid = COALESCE(excluded.materialized_oid, state.materialized_oid),
                     materialized_proof = CASE WHEN excluded.materialized_oid IS NOT NULL
                         THEN excluded.materialized_proof ELSE state.materialized_proof END",
                sql_placeholders("(?, ?, ?, ?, ?)", chunk.len()),
            );
            let params =
                chunk
                    .iter()
                    .zip(&shards)
                    .zip(&proofs)
                    .flat_map(|((row, shard), proof)| {
                        [
                            ValueRef::Text(row.path.as_str().as_bytes()),
                            row.desired
                                .as_ref()
                                .map_or(ValueRef::Null, |oid| ValueRef::Blob(oid.as_bytes())),
                            shard.as_ref().map_or(ValueRef::Null, |shard| {
                                ValueRef::Text(shard.as_str().as_bytes())
                            }),
                            row.materialized
                                .as_ref()
                                .map_or(ValueRef::Null, |oid| ValueRef::Blob(oid.as_bytes())),
                            proof
                                .as_ref()
                                .map_or(ValueRef::Null, |proof| ValueRef::Blob(proof)),
                        ]
                    });
            self.tx
                .execute(&sql, params_from_iter(params.map(ToSqlOutput::Borrowed)))
                .state_context("writing moved repository rows")?;
        }
        Ok(touched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MaterializedRow, StateStore};
    use gat_core::lock::Entry;

    fn path(value: &str) -> GatPath {
        GatPath::parse_canonical(value).unwrap()
    }

    #[test]
    fn complete_row_moves_preserve_overlaps_and_recompute_shards() {
        for (src, dst, expected) in [
            (
                "tree",
                "tree/sub",
                vec![
                    "outside",
                    "tree/sub/a",
                    "tree/sub/sub/b",
                    "tree/sub/sub/deep/c",
                ],
            ),
            ("tree/sub", "tree", vec!["outside", "tree/b", "tree/deep/c"]),
            (
                "tree/a",
                "outside",
                vec!["outside", "tree/sub/b", "tree/sub/deep/c"],
            ),
            (
                "tree/a",
                "new",
                vec!["new", "outside", "tree/sub/b", "tree/sub/deep/c"],
            ),
            (
                "tree",
                "移動\0先",
                vec![
                    "outside",
                    "移動\0先/a",
                    "移動\0先/sub/b",
                    "移動\0先/sub/deep/c",
                ],
            ),
            (
                "missing",
                "tree",
                vec!["outside", "tree/a", "tree/sub/b", "tree/sub/deep/c"],
            ),
            (
                "tree",
                "tree",
                vec!["outside", "tree/a", "tree/sub/b", "tree/sub/deep/c"],
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
            let mut store = StateStore::open(&layout).unwrap();
            let entries = ["outside", "tree/a", "tree/sub/b", "tree/sub/deep/c"].map(|p| Entry {
                path: path(p),
                oid: Oid::from_bytes([1; 32]),
            });
            let levels = LockShardLevels::new(2).unwrap();
            store.upsert_desired_for_test(&entries, levels).unwrap();
            let proof = StatProof::for_test(42, -7, 123);
            let rows = entries
                .iter()
                .map(|entry| MaterializedRow {
                    path: entry.path.clone(),
                    oid: Oid::from_bytes([2; 32]),
                    proof: Some(proof),
                })
                .collect::<Vec<_>>();
            store.upsert_rows(&rows).unwrap();
            store
                .desired_write::<_, super::super::StateStoreError>(|write| {
                    write.move_repository_rows(&path(src), &path(dst), levels)
                })
                .unwrap();
            let desired = store.load_desired_as_lock().unwrap();
            assert_eq!(
                desired
                    .entries
                    .iter()
                    .map(|entry| entry.path.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            let materialized = store.load_all_raw().unwrap();
            assert_eq!(
                materialized
                    .iter()
                    .map(|row| row.path.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(
                materialized
                    .iter()
                    .all(|row| row.oid == Oid::from_bytes([2; 32]) && row.proof == Some(proof))
            );
            let ids = store
                .desired_shard_ids_for_paths(
                    &desired
                        .entries
                        .iter()
                        .map(|entry| entry.path.clone())
                        .collect::<Vec<_>>(),
                )
                .unwrap();
            assert_eq!(
                ids,
                desired
                    .entries
                    .iter()
                    .map(|entry| shard_id_for_path(&entry.path, levels))
                    .collect()
            );
        }
    }

    #[test]
    fn a_desired_only_source_preserves_an_untracked_destination_observation() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&layout).unwrap();
        store
            .upsert_desired_for_test(
                &[Entry {
                    path: path("source"),
                    oid: Oid::from_bytes([1; 32]),
                }],
                LockShardLevels::FLAT,
            )
            .unwrap();
        let proof = StatProof::for_test(42, -7, 123);
        store
            .upsert_rows(&[MaterializedRow {
                path: path("target"),
                oid: Oid::from_bytes([2; 32]),
                proof: Some(proof),
            }])
            .unwrap();
        store
            .desired_write::<_, super::super::StateStoreError>(|write| {
                write.move_repository_rows(&path("source"), &path("target"), LockShardLevels::FLAT)
            })
            .unwrap();
        let desired = store.load_desired_as_lock().unwrap();
        assert_eq!(
            desired.entries,
            vec![Entry {
                path: path("target"),
                oid: Oid::from_bytes([1; 32])
            }]
        );
        let materialized = store.load_all_raw().unwrap();
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].oid, Oid::from_bytes([2; 32]));
        assert_eq!(materialized[0].proof, Some(proof));
    }
}
