//! Move both halves of repository state together, retaining source rows before
//! clearing either scope so overlapping prefixes are remapped exactly once.

use std::collections::BTreeSet;

use super::{
    CanonicalShardIdBuf, Connection, DesiredStateWrite, GatPath, Oid, Result, StateResultExt,
    decode_path, decode_shard_id, decode_stat_proof, descendant_range, encode_stat_proof,
    shard_id_for_path,
};
use crate::file_state::StatProof;
use crate::lock::{LockShardId, LockShardLevels};

struct MovedRow {
    path: GatPath,
    desired: Option<Oid>,
    materialized: Option<Oid>,
    proof: Option<StatProof>,
}

pub(super) struct RepositoryMoveRows {
    pub(super) src: GatPath,
    pub(super) dst: GatPath,
    moved: Vec<MovedRow>,
    touched: BTreeSet<LockShardId>,
}

impl RepositoryMoveRows {
    pub(super) fn read(conn: &Connection, src: &GatPath, dst: &GatPath) -> Result<Self> {
        let (lower, upper) = descendant_range(src.as_str());
        let mut statement = conn
            .prepare(
                "SELECT path, desired_oid, materialized_oid, materialized_proof, desired_shard_id
             FROM state WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
            )
            .state_context("preparing repository move")?;
        let source = statement
            .query_map((src.as_str(), &lower, &upper), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<[u8; 32]>>(1)?,
                    row.get::<_, Option<[u8; 32]>>(2)?,
                    row.get_ref(3)?
                        .as_blob_or_null()?
                        .and_then(decode_stat_proof),
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .state_context("reading repository move source")?;
        let mut moved = Vec::new();
        let mut touched = BTreeSet::new();
        for row in source {
            let (path, desired, materialized, proof, shard) =
                row.state_context("reading repository move row")?;
            if let Some(shard) = shard {
                touched.insert(decode_shard_id(&shard, "desired_shard_id")?);
            }
            let path = decode_path(path, "repository move row")?;
            moved.push(MovedRow {
                path,
                desired: desired.map(Oid::from_bytes),
                materialized: materialized.map(Oid::from_bytes),
                proof,
            });
        }
        Ok(Self {
            src: src.clone(),
            dst: dst.clone(),
            moved,
            touched,
        })
    }

    pub(super) fn source_paths(&self) -> impl Iterator<Item = &GatPath> {
        self.moved
            .iter()
            .filter(|row| row.desired.is_some())
            .map(|row| &row.path)
    }

    pub(super) fn has_tracked_source(&self) -> bool {
        self.source_paths().next().is_some()
    }

    pub(super) fn apply(
        self,
        write: &DesiredStateWrite<'_>,
        levels: LockShardLevels,
    ) -> Result<BTreeSet<LockShardId>> {
        if !self.has_tracked_source() {
            return Ok(BTreeSet::new());
        }
        let Self {
            src,
            dst,
            moved,
            mut touched,
        } = self;
        let (lower, upper) = descendant_range(src.as_str());
        touched.extend(super::desired::desired_shard_ids_for_scope_inner(
            &write.tx, &dst,
        )?);
        // Disjoint exact and descendant deletes let SQLite scan each range directly.
        write
            .tx
            .execute("DELETE FROM state WHERE path = ?1", [src.as_str()])
            .state_context("removing repository move source")?;
        write
            .tx
            .execute(
                "DELETE FROM state WHERE path >= ?1 AND path < ?2",
                (&lower, &upper),
            )
            .state_context("removing repository move source")?;
        let (lower, upper) = descendant_range(dst.as_str());
        // Replaced tracked destinations lose both halves. Materialized-only
        // destinations survive unless an incoming observation replaces them.
        write
            .tx
            .execute(
                "DELETE FROM state WHERE desired_oid IS NOT NULL AND path = ?1",
                [dst.as_str()],
            )
            .state_context("clearing repository move destination")?;
        write
            .tx
            .execute(
                "DELETE FROM state WHERE desired_oid IS NOT NULL AND path >= ?1 AND path < ?2",
                (&lower, &upper),
            )
            .state_context("clearing repository move destination")?;
        let mut insert = write
            .tx
            .prepare(
                "INSERT INTO state (path, desired_oid, desired_shard_id, materialized_oid, materialized_proof)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(path) DO UPDATE SET
                     desired_oid = excluded.desired_oid,
                     desired_shard_id = excluded.desired_shard_id,
                     materialized_oid = COALESCE(excluded.materialized_oid, state.materialized_oid),
                     materialized_proof = CASE WHEN excluded.materialized_oid IS NOT NULL
                         THEN excluded.materialized_proof ELSE state.materialized_proof END",
            )
            .state_context("preparing moved repository rows")?;
        for row in moved {
            let path = row.path.with_replaced_prefix(&src, &dst);
            let shard = row.desired.map(|_| {
                let shard = shard_id_for_path(&path, levels);
                touched.insert(shard);
                CanonicalShardIdBuf::encode(shard)
            });
            let proof = row.proof.as_ref().map(encode_stat_proof);
            insert
                .execute(rusqlite::params![
                    path.as_str(),
                    row.desired.as_ref().map(Oid::as_bytes),
                    shard.as_ref().map(CanonicalShardIdBuf::as_str),
                    row.materialized.as_ref().map(Oid::as_bytes),
                    proof.as_ref(),
                ])
                .state_context("writing moved repository rows")?;
        }
        Ok(touched)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl DesiredStateWrite<'_> {
    pub(super) fn move_repository_rows(
        &self,
        src: &GatPath,
        dst: &GatPath,
        levels: LockShardLevels,
    ) -> Result<BTreeSet<LockShardId>> {
        RepositoryMoveRows::read(&self.tx, src, dst)?.apply(self, levels)
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
    fn late_move_write_failure_rolls_back_both_halves() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&layout).unwrap();
        let mut entries = (0_u32..513)
            .map(|index| Entry {
                path: path(&format!("source/{index:04}")),
                oid: Oid::from_bytes(*blake3::hash(&index.to_le_bytes()).as_bytes()),
            })
            .collect::<Vec<_>>();
        let levels = LockShardLevels::new(1).unwrap();
        store.upsert_desired_for_test(&entries, levels).unwrap();
        let mut rows = entries
            .iter()
            .enumerate()
            .filter(|(index, _)| index % 3 != 0)
            .map(|(index, entry)| MaterializedRow {
                path: entry.path.clone(),
                oid: entry.oid,
                proof: (index % 2 == 0).then(|| StatProof::for_test(42, -7, 123)),
            })
            .collect::<Vec<_>>();
        store.upsert_rows(&rows).unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_late_move AFTER INSERT ON state
             WHEN NEW.path >= 'target/' AND NEW.path < 'target0'
               AND (SELECT COUNT(*) FROM state WHERE path >= 'target/' AND path < 'target0') >= 300
             BEGIN SELECT RAISE(ABORT, 'late move failure'); END;",
            )
            .unwrap();
        let result = store.desired_write::<_, super::super::StateStoreError>(|write| {
            write.move_repository_rows(&path("source"), &path("target"), levels)
        });
        assert!(result.is_err());
        assert_eq!(store.load_desired_as_lock().unwrap().entries, entries);
        assert_eq!(store.load_all_raw().unwrap(), rows);
        store
            .conn
            .execute_batch("DROP TRIGGER fail_late_move")
            .unwrap();
        store
            .desired_write::<_, super::super::StateStoreError>(|write| {
                write.move_repository_rows(&path("source"), &path("target"), levels)
            })
            .unwrap();
        for entry in &mut entries {
            entry.path = entry
                .path
                .with_replaced_prefix(&path("source"), &path("target"));
        }
        for row in &mut rows {
            row.path = row
                .path
                .with_replaced_prefix(&path("source"), &path("target"));
        }
        assert_eq!(store.load_desired_as_lock().unwrap().entries, entries);
        assert_eq!(store.load_all_raw().unwrap(), rows);
    }

    #[test]
    fn untracked_source_observations_are_not_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&layout).unwrap();
        let rows = [MaterializedRow {
            path: path("source/a"),
            oid: Oid::from_bytes([7; 32]),
            proof: Some(StatProof::for_test(42, -7, 123)),
        }];
        store.upsert_rows(&rows).unwrap();
        let touched = store
            .desired_write::<_, super::super::StateStoreError>(|write| {
                write.move_repository_rows(&path("source"), &path("target"), LockShardLevels::FLAT)
            })
            .unwrap();
        assert!(touched.is_empty());
        assert_eq!(store.load_all_raw().unwrap(), rows);
    }

    #[test]
    fn invalid_source_path_rolls_back_the_move() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&layout).unwrap();
        let entries = [Entry {
            path: path("source/a"),
            oid: Oid::from_bytes([1; 32]),
        }];
        store
            .upsert_desired_for_test(&entries, LockShardLevels::FLAT)
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO state(path, materialized_oid) VALUES ('source//invalid', ?1)",
                [Oid::from_bytes([2; 32]).as_bytes()],
            )
            .unwrap();
        let result = store.desired_write::<_, super::super::StateStoreError>(|write| {
            write.move_repository_rows(&path("source"), &path("target"), LockShardLevels::FLAT)
        });
        assert!(result.is_err());
        assert_eq!(store.load_desired_as_lock().unwrap().entries, entries);
        let paths = store
            .conn
            .prepare("SELECT path FROM state ORDER BY path")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(paths, ["source//invalid", "source/a"]);
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

    #[test]
    fn moving_mixed_rows_preserves_absent_halves_and_proofs() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&layout).unwrap();
        let desired = ["source/a", "source/b"].map(|value| Entry {
            path: path(value),
            oid: Oid::from_bytes([1; 32]),
        });
        store
            .upsert_desired_for_test(&desired, LockShardLevels::FLAT)
            .unwrap();
        let materialized = [
            // A stale file observation can coexist with desired descendants.
            MaterializedRow {
                path: path("source"),
                oid: Oid::from_bytes([4; 32]),
                proof: Some(StatProof::for_test(8, 9, 10)),
            },
            MaterializedRow {
                path: path("source/b"),
                oid: Oid::from_bytes([2; 32]),
                proof: Some(StatProof::for_test(42, -7, 123)),
            },
            MaterializedRow {
                path: path("source/c"),
                oid: Oid::from_bytes([3; 32]),
                proof: None,
            },
        ];
        store.upsert_rows(&materialized).unwrap();
        store
            .desired_write::<_, super::super::StateStoreError>(|write| {
                write.move_repository_rows(&path("source"), &path("target"), LockShardLevels::FLAT)
            })
            .unwrap();
        assert_eq!(
            store.load_desired_as_lock().unwrap().entries,
            desired.map(|entry| Entry {
                path: entry
                    .path
                    .with_replaced_prefix(&path("source"), &path("target")),
                oid: entry.oid,
            })
        );
        let actual = store.load_all_raw().unwrap();
        assert_eq!(actual.len(), materialized.len());
        for (actual, expected) in actual.iter().zip(&materialized) {
            assert_eq!(
                actual.path,
                expected
                    .path
                    .with_replaced_prefix(&path("source"), &path("target"))
            );
            assert_eq!(actual.oid, expected.oid);
            assert_eq!(actual.proof, expected.proof);
        }
    }
}
