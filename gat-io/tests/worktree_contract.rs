use gat_core::lexical_path::GatPath;
use gat_io::{RepositoryLayout, WorktreeDestinationKind, WorktreeEntryKind, WorktreePathError};

fn path(value: &str) -> GatPath {
    GatPath::parse_canonical(value).expect("canonical fixture path")
}

const fn assert_copy<T: Copy>() {}

#[test]
fn removal_finishes_file_work_and_reports_the_first_input_order_error() {
    let temp = tempfile::tempdir().unwrap();
    for directory in ["a", "z"] {
        std::fs::create_dir(temp.path().join(directory)).unwrap();
        std::fs::write(temp.path().join(directory).join("untracked"), b"keep").unwrap();
    }
    std::fs::write(temp.path().join("file"), b"remove").unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let error = layout
        .worktree_client()
        .remove_and_prune(&[path("z"), path("file"), path("a")])
        .unwrap_err();
    assert!(matches!(error, gat_io::RemovePathError::Delete { path, .. } if path == "z"));
    assert!(!temp.path().join("file").exists());
    for directory in ["a", "z"] {
        assert_eq!(
            std::fs::read(temp.path().join(directory).join("untracked")).unwrap(),
            b"keep"
        );
    }
}

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
        .unwrap()
        .commit();
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

#[cfg(unix)]
#[test]
fn batch_preflight_does_not_authorize_later_symlinked_mutations() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let layout = RepositoryLayout::at(temp.path().to_path_buf());
    let worktree = layout.worktree_client();
    let paths = [path("data/a"), path("data/b"), path("other/c")];
    std::fs::create_dir(temp.path().join("data")).unwrap();
    worktree.validate_mutations(&paths).unwrap();
    std::fs::remove_dir(temp.path().join("data")).unwrap();
    std::fs::write(outside.path().join("a"), b"untouched").unwrap();
    symlink(outside.path(), temp.path().join("data")).unwrap();

    assert!(matches!(
        worktree.validate_mutations(&paths),
        Err(WorktreePathError::SymlinkAncestor { .. })
    ));
    assert!(worktree.remove_and_prune(&paths).is_err());
    assert_eq!(
        std::fs::read(outside.path().join("a")).unwrap(),
        b"untouched"
    );
}
