use gat_core::{
    lexical_path::GatPath,
    lock::{Entry, Lock},
    oid::Oid,
};

#[test]
fn resident_validation_matches_pairwise_tree_invariants_in_any_order() {
    let paths: Vec<_> = [
        "a", "a-", "a--", "a.", "a/b", "a/b/c", "a0", "ab", "ab/c", "é", "é/x", "a\n", "a\n/b",
    ]
    .into_iter()
    .map(|p| GatPath::parse_canonical(p).unwrap())
    .collect();
    assert!(Lock::default().validate().is_ok());
    for a in &paths {
        for b in &paths {
            for c in &paths {
                let selected = [a, b, c];
                let valid = (0..3).all(|i| {
                    ((i + 1)..3).all(|j| {
                        !selected[i].is_or_under(selected[j])
                            && !selected[j].is_or_under(selected[i])
                    })
                });
                let lock = Lock {
                    entries: selected
                        .into_iter()
                        .map(|path| Entry {
                            path: path.clone(),
                            oid: Oid::from_bytes([1; 32]),
                        })
                        .collect(),
                };
                assert_eq!(lock.validate().is_ok(), valid, "{selected:?}");
            }
        }
    }
}

#[test]
fn ordered_directory_validation_matches_pairwise_conflicts() {
    use gat_core::lock::{
        LockDomainError, LockError, validated::validate_no_path_directory_conflicts,
    };
    let pool = [
        "a", "a!", "a!/b", "a.b", "a/b", "a/b/c", "a0", "b", "é", "é\n", "é\n/x", "é/x",
    ];
    for mask in 0..(1usize << pool.len()) {
        let paths: std::collections::BTreeSet<_> = pool
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .map(|(_, path)| *path)
            .collect();
        let expected = paths.iter().find_map(|ancestor| {
            paths
                .iter()
                .find(|descendant| {
                    descendant
                        .strip_prefix(*ancestor)
                        .is_some_and(|suffix| suffix.starts_with('/'))
                })
                .map(|descendant| (*ancestor, *descendant))
        });
        match (validate_no_path_directory_conflicts(&paths), expected) {
            (Ok(()), None) => {}
            (
                Err(LockError::Domain(LockDomainError::DirectoryPrefixConflict {
                    ancestor,
                    descendant,
                })),
                Some(_),
            ) => {
                assert!(paths.contains(ancestor.as_str()));
                assert!(paths.contains(descendant.as_str()));
                assert!(
                    descendant
                        .strip_prefix(&ancestor)
                        .is_some_and(|suffix| suffix.starts_with('/'))
                );
            }
            (actual, expected) => {
                panic!("mismatched validation for {paths:?}: {actual:?}, {expected:?}")
            }
        }
    }
}
