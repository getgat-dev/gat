use gat_core::lexical_path::GatPath;
use gat_io::{RepositoryLayout, WorktreeDestinationKind, WorktreeEntryKind, WorktreePathError};

fn path(value: &str) -> GatPath {
    GatPath::parse_canonical(value).expect("canonical fixture path")
}

const fn assert_copy<T: Copy>() {}

#[test]
fn layout_creates_a_copyable_repository_bound_worktree_capability() {
    assert_copy::<gat_io::WorktreeClient<'_>>();

    let temp = tempfile::tempdir().expect("temporary worktree");
    std::fs::write(temp.path().join("source.bin"), b"payload").expect("write fixture");
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let worktree = layout.worktree_client();

    assert_eq!(
        worktree.inspect(&path("source.bin")).unwrap(),
        WorktreeEntryKind::File
    );
    assert_eq!(
        worktree.inspect_destination(&path("nested")).unwrap(),
        WorktreeDestinationKind::Missing
    );

    worktree
        .move_path(&path("source.bin"), &path("nested/destination.bin"))
        .unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("nested/destination.bin")).unwrap(),
        b"payload"
    );

    worktree
        .remove_and_prune(&[path("nested/destination.bin")])
        .unwrap();
    assert!(!temp.path().join("nested").exists());
}

#[cfg(unix)]
#[test]
fn layout_capability_rejects_symlinked_mutation_ancestors() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("temporary worktree");
    let outside = tempfile::tempdir().expect("outside directory");
    symlink(outside.path(), temp.path().join("linked")).expect("create fixture symlink");
    let layout = RepositoryLayout::at(temp.path().to_path_buf());

    assert!(matches!(
        layout
            .worktree_client()
            .validate_mutation(&path("linked/file.bin")),
        Err(WorktreePathError::SymlinkAncestor { .. })
    ));
    assert!(!outside.path().join("file.bin").exists());
}
