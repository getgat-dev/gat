//! Pure, three-way semantic merge of `gat.lock` content, keyed by canonical
//! tracked path instead of by text line/hunk. This module
//! has no knowledge of Git, files, or the merge-driver protocol at all --
//! it only implements the merge *rule* over already-parsed [`Lock`]
//! values, so it can be unit-tested directly and reused by both the
//! flat-file and sharded on-disk shapes (each shard file is itself a
//! self-contained lock document, so the same rule applies unchanged).
//!
//! For every path in the union of `ancestor`/`ours`/`theirs`, a value is
//! the optional `oid` (`None` meaning "not tracked at this path in this
//! version"):
//!
//! ```text
//! ours == theirs   => keep that value
//! ours == ancestor => take theirs
//! theirs == ancestor => take ours
//! otherwise        => conflict
//! ```
//!
//! This is exactly Git's own three-way merge rule, just applied per key of
//! a path -> value map instead of per line of text -- which is what lets
//! two independent insertions into the same sorted textual gap merge
//! cleanly instead of colliding as a false conflict.
//!
//! Same-path conflicts are reported before checking the resolved map for
//! file/directory-prefix conflicts. Success certifies single-file invariants;
//! shard placement and cross-shard validation remain the caller's responsibility.

use super::{Entry, Lock};
use crate::lexical_path::GatPath;
use crate::oid::Oid;
use std::collections::BTreeMap;

/// A same-path change that can't be resolved automatically: both sides
/// changed `path` relative to the ancestor, to different values (including
/// one side deleting it while the other modified it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: GatPath,
    pub ancestor: Option<Oid>,
    pub ours: Option<Oid>,
    pub theirs: Option<Oid>,
}

/// A merge cannot produce a valid single-file lock.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MergeConflict {
    /// All incompatible same-path changes, in decoded-path order.
    #[error("{} path(s) changed incompatibly", .0.len())]
    Paths(Vec<Conflict>),
    /// The resolved map tracks both a file and one of its descendants.
    #[error("merged paths conflict: {ancestor:?} and {descendant:?}")]
    DirectoryPrefix {
        ancestor: String,
        descendant: String,
    },
}

/// Resolve per-path changes, then certify the combined single-file map.
///
/// Success returns unique, strictly path-ordered entries with no tracked
/// file/directory-prefix overlap. Same-path conflicts take precedence over
/// prefix conflicts; otherwise the first prefix conflict in path order is returned.
/// Inputs must have unique canonical paths; their entry order is immaterial.
/// This does not validate shard placement or conflicts with other files.
pub fn merge_three_way(ancestor: &Lock, ours: &Lock, theirs: &Lock) -> Result<Lock, MergeConflict> {
    // Resolve all three versions through one ordered union. Inputs need not
    // already be path-sorted.
    let mut paths: BTreeMap<&GatPath, [Option<Oid>; 3]> = BTreeMap::new();
    for (side, lock) in [ancestor, ours, theirs].into_iter().enumerate() {
        for entry in &lock.entries {
            paths.entry(&entry.path).or_default()[side] = Some(entry.oid);
        }
    }

    let mut entries = Vec::with_capacity(paths.len());
    let mut conflicts = Vec::new();

    for (path, [av, ov, tv]) in paths {
        let resolved = if ov == tv {
            ov
        } else if ov == av {
            tv
        } else if tv == av {
            ov
        } else {
            conflicts.push(Conflict {
                path: path.clone(),
                ancestor: av,
                ours: ov,
                theirs: tv,
            });
            continue;
        };

        if let Some(oid) = resolved {
            entries.push(Entry {
                path: path.clone(),
                oid,
            });
        }
    }

    if !conflicts.is_empty() {
        return Err(MergeConflict::Paths(conflicts));
    }
    if let Some((ancestor, descendant)) = directory_prefix_conflict(&entries) {
        return Err(MergeConflict::DirectoryPrefix {
            ancestor: ancestor.to_owned(),
            descendant: descendant.to_owned(),
        });
    }
    Ok(Lock { entries })
}

/// Search the sorted result without an auxiliary index or allocated bound strings.
/// The first key at or above `path + '/'` is its first possible descendant;
/// punctuation siblings such as `foo.bar` can separate it from the tracked file.
fn directory_prefix_conflict(entries: &[Entry]) -> Option<(&str, &str)> {
    for (index, entry) in entries.iter().enumerate() {
        let path = entry.path.as_str();
        let remaining = &entries[index + 1..];
        // All keys with this byte prefix are contiguous. If the immediate
        // successor is outside that range, no later key can be a descendant.
        if !remaining
            .first()
            .is_some_and(|next| next.path.as_str().starts_with(path))
        {
            continue;
        }
        let next = remaining.partition_point(|candidate| {
            candidate
                .path
                .as_str()
                .bytes()
                .cmp(path.bytes().chain(std::iter::once(b'/')))
                .is_lt()
        });
        if let Some(candidate) = remaining.get(next) {
            let descendant = candidate.path.as_str();
            if super::codec::is_directory_prefix(path, descendant) {
                return Some((path, descendant));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(entries: &[(&str, &str)]) -> Lock {
        Lock {
            entries: entries
                .iter()
                .map(|(path, oid)| Entry {
                    path: GatPath::parse_canonical(path).unwrap(),
                    oid: Oid::from_hex(oid).unwrap(),
                })
                .collect(),
        }
    }

    fn oid(hex: &str) -> Oid {
        Oid::from_hex(hex).unwrap()
    }

    const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const OID_C: &str = "ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccdd";

    #[test]
    fn prefix_search_matches_pairwise_reference_for_every_subset() {
        let mut pool = [
            "a",
            "a!",
            "a!b",
            "a.b",
            "a/b",
            "a/b/c",
            "a0",
            "b",
            "b/c",
            "é\0",
            "é\0/child",
            "é\t",
            "é\t/child",
        ];
        pool.sort_unstable();
        for mask in 0..(1usize << pool.len()) {
            let entries: Vec<_> = pool
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, path)| (*path, OID_A))
                .collect();
            let lock = lock(&entries);
            let expected = entries.iter().enumerate().find_map(|(i, (path, _))| {
                entries[i + 1..].iter().find_map(|(descendant, _)| {
                    descendant
                        .strip_prefix(path)
                        .is_some_and(|suffix| suffix.starts_with('/'))
                        .then_some((*path, *descendant))
                })
            });
            assert_eq!(
                directory_prefix_conflict(&lock.entries),
                expected,
                "subset {mask}"
            );
        }
    }

    #[test]
    fn merged_directory_prefix_conflicts_are_rejected_in_both_directions() {
        for (file, descendant) in [("foo", "foo/bar"), ("é\t", "é\t/child")] {
            let ancestor = lock(&[]);
            let file_side = lock(&[(file, OID_A)]);
            let descendant_side = lock(&[("foo.bar", OID_B), (descendant, OID_C)]);
            for (ours, theirs) in [
                (&file_side, &descendant_side),
                (&descendant_side, &file_side),
            ] {
                assert_eq!(
                    merge_three_way(&ancestor, ours, theirs).unwrap_err(),
                    MergeConflict::DirectoryPrefix {
                        ancestor: file.to_owned(),
                        descendant: descendant.to_owned(),
                    }
                );
            }
        }
    }

    #[test]
    fn replacing_a_file_with_descendants_and_punctuation_siblings_is_valid() {
        let ancestor = lock(&[("foo", OID_A)]);
        let ours = lock(&[("foo/bar", OID_B)]);
        let theirs = lock(&[("foo", OID_A), ("foo.bar", OID_C)]);
        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        assert_eq!(
            Lock::parse(&merged.to_string()).unwrap().entries,
            merged.entries
        );
        assert_eq!(
            merged
                .entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["foo.bar", "foo/bar"]
        );
    }

    #[test]
    fn same_path_conflicts_take_precedence_over_prefix_conflicts() {
        let ancestor = lock(&[]);
        let ours = lock(&[("a", OID_A), ("foo", OID_A)]);
        let theirs = lock(&[("a", OID_B), ("foo/bar", OID_B)]);
        let MergeConflict::Paths(conflicts) =
            merge_three_way(&ancestor, &ours, &theirs).unwrap_err()
        else {
            panic!("expected same-path conflict first");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "a");
    }

    #[test]
    fn independent_additions_on_both_sides_merge_cleanly() {
        let ancestor = lock(&[("a.bin", OID_A), ("d.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_A), ("b.bin", OID_B), ("d.bin", OID_A)]);
        let theirs = lock(&[("a.bin", OID_A), ("c.bin", OID_C), ("d.bin", OID_A)]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        let paths: Vec<_> = merged.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a.bin", "b.bin", "c.bin", "d.bin"]);
    }

    #[test]
    fn independent_modifications_to_distinct_paths_merge_cleanly() {
        let ancestor = lock(&[("a.bin", OID_A), ("b.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_B), ("b.bin", OID_A)]);
        let theirs = lock(&[("a.bin", OID_A), ("b.bin", OID_C)]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        let mut entries = merged.entries;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(entries[0].oid, oid(OID_B));
        assert_eq!(entries[1].oid, oid(OID_C));
    }

    #[test]
    fn identical_additions_on_both_sides_merge_to_one_entry() {
        let ancestor = lock(&[]);
        let ours = lock(&[("a.bin", OID_A)]);
        let theirs = lock(&[("a.bin", OID_A)]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        assert_eq!(merged.entries.len(), 1);
        assert_eq!(merged.entries[0].oid, oid(OID_A));
    }

    #[test]
    fn identical_modifications_on_both_sides_merge_cleanly() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_B)]);
        let theirs = lock(&[("a.bin", OID_B)]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        assert_eq!(
            merged.entries,
            vec![Entry {
                path: GatPath::parse_canonical("a.bin").unwrap(),
                oid: oid(OID_B),
            }]
        );
    }

    #[test]
    fn one_side_change_takes_the_changed_value() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_B)]);
        let theirs = lock(&[("a.bin", OID_A)]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        assert_eq!(merged.entries[0].oid, oid(OID_B));
    }

    #[test]
    fn one_side_delete_is_honored() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[]);
        let theirs = lock(&[("a.bin", OID_A)]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        assert!(merged.entries.is_empty());
    }

    #[test]
    fn both_side_delete_stays_deleted() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[]);
        let theirs = lock(&[]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        assert!(merged.entries.is_empty());
    }

    #[test]
    fn same_path_add_add_with_different_values_conflicts() {
        let ancestor = lock(&[]);
        let ours = lock(&[("a.bin", OID_B)]);
        let theirs = lock(&[("a.bin", OID_C)]);

        let MergeConflict::Paths(conflicts) =
            merge_three_way(&ancestor, &ours, &theirs).unwrap_err()
        else {
            panic!("expected same-path conflicts");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "a.bin");
        assert_eq!(conflicts[0].ancestor, None);
        assert_eq!(conflicts[0].ours, Some(oid(OID_B)));
        assert_eq!(conflicts[0].theirs, Some(oid(OID_C)));
    }

    #[test]
    fn same_path_modify_modify_with_different_values_conflicts() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_B)]);
        let theirs = lock(&[("a.bin", OID_C)]);

        let MergeConflict::Paths(conflicts) =
            merge_three_way(&ancestor, &ours, &theirs).unwrap_err()
        else {
            panic!("expected same-path conflicts");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "a.bin");
    }

    #[test]
    fn modify_delete_conflicts() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[]);
        let theirs = lock(&[("a.bin", OID_B)]);

        let MergeConflict::Paths(conflicts) =
            merge_three_way(&ancestor, &ours, &theirs).unwrap_err()
        else {
            panic!("expected same-path conflicts");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].ours, None);
        assert_eq!(conflicts[0].theirs, Some(oid(OID_B)));
    }

    #[test]
    fn delete_modify_conflicts_symmetrically() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_B)]);
        let theirs = lock(&[]);

        let MergeConflict::Paths(conflicts) =
            merge_three_way(&ancestor, &ours, &theirs).unwrap_err()
        else {
            panic!("expected same-path conflicts");
        };
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].ours, Some(oid(OID_B)));
        assert_eq!(conflicts[0].theirs, None);
    }

    #[test]
    fn result_is_deterministically_sorted_by_path() {
        let ancestor = lock(&[]);
        let ours = lock(&[("z.bin", OID_A), ("a.bin", OID_A)]);
        let theirs = lock(&[("m.bin", OID_A)]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        let paths: Vec<_> = merged.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a.bin", "m.bin", "z.bin"]);
    }

    #[test]
    fn multiple_independent_conflicts_are_all_reported() {
        let ancestor = lock(&[("a.bin", OID_A), ("b.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_B), ("b.bin", OID_B)]);
        let theirs = lock(&[("a.bin", OID_C), ("b.bin", OID_C)]);

        let MergeConflict::Paths(conflicts) =
            merge_three_way(&ancestor, &ours, &theirs).unwrap_err()
        else {
            panic!("expected same-path conflicts");
        };
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].path, "a.bin");
        assert_eq!(conflicts[1].path, "b.bin");
    }
}
