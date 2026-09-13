use gat_core::{
    config::{MountConfig, MountsConfig},
    lexical_path::{GatPath, GatSubpath},
    name::MountName,
};

#[test]
fn mount_validation_matches_pairwise_overlap_rules_in_any_order() {
    let paths: Vec<_> = [
        "a", "a-", "a--", "a.", "a/b", "a/b/c", "a0", "ab", "ab/c", "é", "é/x",
    ]
    .into_iter()
    .map(|p| GatPath::parse_canonical(p).unwrap())
    .collect();
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
                let mounts = MountsConfig {
                    by_name: selected
                        .into_iter()
                        .enumerate()
                        .map(|(i, target)| {
                            (
                                MountName::from_string(format!("mount-{i}")),
                                MountConfig {
                                    url: "upstream".to_owned().into(),
                                    target: target.clone(),
                                    path: GatSubpath::Root,
                                    rev: None,
                                    rev_lock: None,
                                    include: vec![],
                                    exclude: vec![],
                                },
                            )
                        })
                        .collect(),
                };
                assert_eq!(mounts.validate_effective().is_ok(), valid, "{selected:?}");
            }
        }
    }
}
