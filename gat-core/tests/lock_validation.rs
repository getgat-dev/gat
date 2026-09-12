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
