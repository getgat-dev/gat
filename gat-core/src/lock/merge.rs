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
//! This module constructs no errors at all: merge conflicts are represented
//! as ordinary data ([`merge_three_way`]'s `Err(Vec<Conflict>)` case), not
//! failures, and the merge rule is total over already-parsed [`Lock`] values.

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

/// Merge `ours` and `theirs` against their common `ancestor`, keyed by
/// path. Returns the merged, path-sorted [`Lock`] on a clean merge, or the
/// full list of same-path conflicts (one entry per conflicting path, in
/// canonical path order) if any remain.
///
/// Every path present in any of the three inputs is considered
/// independently: a path added, removed, or modified on only one side (or
/// identically on both) always merges cleanly, no matter what unrelated
/// paths changed elsewhere in the same lock document.
pub fn merge_three_way(ancestor: &Lock, ours: &Lock, theirs: &Lock) -> Result<Lock, Vec<Conflict>> {
    // One ordered union retains all three values without separate indexes or
    // a second collection of keys. Inputs need not already be path-sorted.
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

    if conflicts.is_empty() {
        Ok(Lock { entries })
    } else {
        Err(conflicts)
    }
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
    fn independent_additions_on_both_sides_merge_cleanly() {
        let ancestor = lock(&[("a.bin", OID_A), ("d.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_A), ("b.bin", OID_B), ("d.bin", OID_A)]);
        let theirs = lock(&[("a.bin", OID_A), ("c.bin", OID_C), ("d.bin", OID_A)]);

        let merged = merge_three_way(&ancestor, &ours, &theirs).unwrap();
        let mut paths: Vec<_> = merged.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
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

        let conflicts = merge_three_way(&ancestor, &ours, &theirs).unwrap_err();
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

        let conflicts = merge_three_way(&ancestor, &ours, &theirs).unwrap_err();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "a.bin");
    }

    #[test]
    fn modify_delete_conflicts() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[]);
        let theirs = lock(&[("a.bin", OID_B)]);

        let conflicts = merge_three_way(&ancestor, &ours, &theirs).unwrap_err();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].ours, None);
        assert_eq!(conflicts[0].theirs, Some(oid(OID_B)));
    }

    #[test]
    fn delete_modify_conflicts_symmetrically() {
        let ancestor = lock(&[("a.bin", OID_A)]);
        let ours = lock(&[("a.bin", OID_B)]);
        let theirs = lock(&[]);

        let conflicts = merge_three_way(&ancestor, &ours, &theirs).unwrap_err();
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

        let conflicts = merge_three_way(&ancestor, &ours, &theirs).unwrap_err();
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].path, "a.bin");
        assert_eq!(conflicts[1].path, "b.bin");
    }
}
