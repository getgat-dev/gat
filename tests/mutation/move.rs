use gat_engine::Repository as Repo;

fn layout(root: &Path) -> gat_io::RepositoryLayout {
    gat_io::RepositoryLayout::at(root.to_path_buf())
}
use std::path::Path;

type MoveError = gat_command::MoveError;
type MoveOutcome = gat_command::MoveOutcome;
type Result<T> = std::result::Result<T, MoveError>;

fn mv(repo: &Repo, src: &Path, dst: &Path, force: bool) -> Result<MoveOutcome> {
    gat_command::move_path(
        repo,
        gat_command::MoveRequest {
            src: gat_core::lexical_path::GatPath::normalize(src).expect("valid source path"),
            dst: gat_core::lexical_path::GatPath::normalize(dst).expect("valid destination path"),
            force,
        },
    )
}

fn mv_with_progress(
    repo: &Repo,
    src: &Path,
    dst: &Path,
    force: bool,
    progress: &dyn gat_core::progress::ProgressReporter,
) -> Result<MoveOutcome> {
    gat_command::move_with_progress(
        repo,
        gat_command::MoveRequest {
            src: gat_core::lexical_path::GatPath::normalize(src).expect("valid source path"),
            dst: gat_core::lexical_path::GatPath::normalize(dst).expect("valid destination path"),
            force,
        },
        progress,
    )
}

mod tests {
    use super::*;
    use crate::matching_lock_shape;
    #[cfg(unix)]
    use gat_core::lock::{Lock, LockShardId, LockShardLevels};
    use gat_core::progress::NoopProgress;
    use gat_engine::Repository as Repo;
    use std::path::PathBuf;

    use ::test_support::add;
    use test_support::git_repo_with_initial_commit as test_repo;

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path).unwrap()
    }

    #[test]
    fn mv_exclude_failure_reports_completed_move_and_retry_cannot_resume() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        let exclude = tmp.path().join(".git/info/exclude");
        std::fs::remove_file(&exclude).unwrap();
        std::fs::create_dir(&exclude).unwrap();

        let error = mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false).unwrap_err();
        assert!(matches!(error, MoveError::Published { source, .. }
            if matches!(*source, gat_engine::RepositoryMutationError::RegenerateExcludes { .. })));
        assert!(!tmp.path().join("a.bin").exists());
        assert_eq!(std::fs::read(tmp.path().join("b.bin")).unwrap(), b"payload");
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "b.bin");
        let materialized = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
        assert_eq!(materialized.entries.len(), 1);
        assert_eq!(materialized.entries[0].path, "b.bin");
        assert!(matches!(
            mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false),
            Err(MoveError::SourceNotTracked { .. })
        ));
        assert!(exclude.is_dir());
    }

    #[test]
    fn mv_renames_a_tracked_file_updating_lock_and_disk() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();

        mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false).unwrap();

        assert!(!tmp.path().join("a.bin").exists());
        assert_eq!(std::fs::read(tmp.path().join("b.bin")).unwrap(), b"payload");
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "b.bin");
        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("b.bin"));
        assert!(!exclude.contains("a.bin"));
    }

    /// A flat `gat.lock` mutation must take the same sparse,
    /// touched-shard-scoped pipeline as a sharded one (flat is
    /// the degenerate one-shard case, not a separate implementation).
    /// Asserts `mv` on a flat repo publishes exactly one shard identity,
    /// keyed by the `"gat.lock"` sentinel, and that `gat.lock` stays a
    /// plain file rather than becoming a `gat.lock/` directory.
    #[test]
    fn mv_in_a_flat_repo_uses_the_sparse_pipeline_and_stays_a_single_file() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        assert!(
            matching_lock_shape(&repo, tmp.path()).unwrap().is_none(),
            "nothing tracked yet"
        );
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        assert_eq!(
            matching_lock_shape(&repo, tmp.path()).unwrap(),
            Some(gat_core::lock::LockShardLevels::FLAT)
        );

        mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false).unwrap();

        assert!(
            tmp.path().join("gat.lock").is_file(),
            "flat mv must publish a single gat.lock file, never a gat.lock/ directory"
        );
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "b.bin");

        let mut store = gat_io::StateStore::open(&layout(tmp.path())).unwrap();
        gat_engine::test_support::refresh_desired_index(&repo, &mut store).unwrap();
        let shard_ids = gat_io::state_shard_ids_for_test(&store).unwrap();
        assert_eq!(
            shard_ids,
            vec![gat_core::lock::LockShardId::flat()],
            "flat desired rows must all share one logical shard id"
        );
        assert_eq!(
            store
                .desired_rows(gat_io::DesiredQuery::all())
                .unwrap()
                .len(),
            1
        );
    }

    /// Regression test for GAT-ARCH-04: `gat mv` must keep materialized
    /// state consistent with the new path, or a later `gat sync` would
    /// see `(None, Some(prior))` for the old path (spurious removal) and
    /// `(Some(desired), None)` for the new one (spurious re-materialize).
    #[test]
    fn mv_moves_materialized_state_to_the_new_path() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        ::test_support::sync(&repo, &NoopProgress).unwrap();

        mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false).unwrap();

        let materialized = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
        assert!(materialized.entries.iter().all(|e| e.path != "a.bin"));
        assert!(materialized.entries.iter().any(|e| e.path == "b.bin"));

        let outcome = ::test_support::sync(&repo, &NoopProgress).unwrap();
        assert!(
            outcome.outcome.is_clean(),
            "sync after `mv` must be a no-op, not schedule a removal/re-materialize: {outcome:?}"
        );
        assert_eq!(std::fs::read(tmp.path().join("b.bin")).unwrap(), b"payload");
    }

    #[test]
    fn mv_renames_a_tracked_directory_and_all_nested_entries() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/nested")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/nested/b.bin"), b"b").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();

        mv(&repo, Path::new("data"), Path::new("renamed"), false).unwrap();

        assert!(!tmp.path().join("data").exists());
        assert_eq!(
            std::fs::read(tmp.path().join("renamed/a.bin")).unwrap(),
            b"a"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("renamed/nested/b.bin")).unwrap(),
            b"b"
        );
        let mut paths: Vec<String> = gat_io::LockStore::load_repository(
            &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
        )
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
        paths.sort();
        assert_eq!(paths, vec!["renamed/a.bin", "renamed/nested/b.bin"]);
    }

    #[test]
    fn mv_on_an_untracked_path_errors() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        let err = mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false).unwrap_err();
        assert!(
            matches!(err, MoveError::SourceNotTracked { ref path } if path == "a.bin"),
            "expected MoveError::SourceNotTracked, got: {err:?}"
        );
    }

    /// Regression test for the `gat mv` overwrite issue: without
    /// `--force`, an existing (untracked) destination must cause the
    /// whole operation to fail closed, leaving both files' bytes and
    /// `gat.lock` untouched -- `std::fs::rename` alone would silently
    /// replace `b.bin` on platforms where that's the OS's semantics.
    #[test]
    fn mv_without_force_refuses_to_replace_an_existing_untracked_destination() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"source").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"destination").unwrap();

        let err = mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already exists") && msg.contains("--force"),
            "expected an actionable already-exists/--force error, got: {msg}"
        );
        assert!(
            matches!(err, MoveError::DestinationExists { .. }),
            "expected MoveError::DestinationExists, got: {err:?}"
        );

        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"source");
        assert_eq!(
            std::fs::read(tmp.path().join("b.bin")).unwrap(),
            b"destination"
        );
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a.bin");
    }

    /// `--force` explicitly permits replacing an existing destination
    /// file.
    #[test]
    fn mv_with_force_replaces_an_existing_untracked_destination() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"source").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"destination").unwrap();

        mv(&repo, Path::new("a.bin"), Path::new("b.bin"), true).unwrap();

        assert!(!tmp.path().join("a.bin").exists());
        assert_eq!(std::fs::read(tmp.path().join("b.bin")).unwrap(), b"source");
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "b.bin");
    }

    /// A collision with an existing Gat-tracked destination must be
    /// rejected without `--force`, and cleanly replaced (old row and
    /// materialized-state ownership removed, no duplicate/stale rows)
    /// under `--force`.
    #[test]
    fn mv_destination_collision_with_a_tracked_row_requires_force_and_cleans_up_ownership() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"source").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"destination").unwrap();
        add(
            &repo,
            &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            &NoopProgress,
        )
        .unwrap();
        ::test_support::sync(&repo, &NoopProgress).unwrap();

        let err = mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already tracked") && msg.contains("--force"),
            "expected an actionable already-tracked/--force error, got: {msg}"
        );
        assert!(
            matches!(err, MoveError::DestinationTracked { .. }),
            "expected MoveError::DestinationTracked, got: {err:?}"
        );
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"source");
        assert_eq!(
            std::fs::read(tmp.path().join("b.bin")).unwrap(),
            b"destination"
        );
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 2);

        mv(&repo, Path::new("a.bin"), Path::new("b.bin"), true).unwrap();

        assert!(!tmp.path().join("a.bin").exists());
        assert_eq!(std::fs::read(tmp.path().join("b.bin")).unwrap(), b"source");
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(
            entries.len(),
            1,
            "old b.bin row must not linger: {entries:?}"
        );
        assert_eq!(entries[0].path, "b.bin");
        let materialized = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
        assert_eq!(
            materialized.entries.len(),
            1,
            "old b.bin materialized ownership must be forgotten: {:?}",
            materialized.entries
        );
        assert_eq!(materialized.entries[0].path, "b.bin");
    }

    #[test]
    #[cfg(unix)]
    fn mv_in_a_sharded_repo_only_rewrites_the_old_and_new_shard_files() {
        use std::collections::HashMap;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut cfg = repo.load_config().unwrap();
        cfg.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(2).unwrap());
        repo.write_config_fixture(&cfg).unwrap();
        let mut paths = Vec::new();
        for i in 0..40 {
            let path = format!("file-{i}.bin");
            std::fs::write(tmp.path().join(&path), format!("payload-{i}")).unwrap();
            paths.push(PathBuf::from(path));
        }
        add(&repo, &paths, &NoopProgress).unwrap();

        let io_layout = layout(tmp.path());
        let before: HashMap<_, _> = gat_io::lock_test_support::shard_inodes(&io_layout).unwrap();
        let src = "file-0.bin";
        let src_shard = LockShardId::for_path(&gp(src), LockShardLevels::new(2).unwrap());
        let dst = (0..200)
            .map(|i| format!("renamed-{i}/file-0.bin"))
            .find(|candidate| {
                LockShardId::for_path(&gp(candidate), LockShardLevels::new(2).unwrap()) != src_shard
            })
            .unwrap();
        let dst_shard = LockShardId::for_path(&gp(&dst), LockShardLevels::new(2).unwrap());

        mv(&repo, Path::new(src), Path::new(&dst), false).unwrap();

        for (shard_id, ino_after) in gat_io::lock_test_support::shard_inodes(&io_layout).unwrap() {
            if shard_id == src_shard {
                assert_ne!(before[&shard_id], ino_after);
            } else if shard_id == dst_shard {
                assert!(
                    before.get(&shard_id).is_none_or(|ino| *ino != ino_after),
                    "destination shard should be new or rewritten"
                );
            } else {
                assert_eq!(before[&shard_id], ino_after);
            }
        }
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert!(lock.entries.iter().all(|e| e.path != src));
        assert!(lock.entries.iter().any(|e| e.path == dst));
    }

    /// Force never introduces implicit Unix-`mv` directory-target
    /// semantics: an existing directory at `dst` stays rejected even with
    /// `--force`.
    #[test]
    fn mv_refuses_an_existing_directory_destination_even_with_force() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        std::fs::create_dir_all(tmp.path().join("dir")).unwrap();

        let err_no_force = mv(&repo, Path::new("a.bin"), Path::new("dir"), false).unwrap_err();
        let err_force = mv(&repo, Path::new("a.bin"), Path::new("dir"), true).unwrap_err();
        assert!(
            matches!(err_no_force, MoveError::DestinationIsDirectory { .. }),
            "expected MoveError::DestinationIsDirectory, got: {err_no_force:?}"
        );
        assert!(
            matches!(err_force, MoveError::DestinationIsDirectory { .. }),
            "expected MoveError::DestinationIsDirectory even with --force, got: {err_force:?}"
        );
        assert!(tmp.path().join("a.bin").exists());
        assert!(tmp.path().join("dir").is_dir());
    }

    #[test]
    fn mv_refuses_to_touch_a_source_owned_row_or_move_into_a_source_prefix() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut cfg = repo.load_config().unwrap();
        cfg.mounts.by_name.insert(
            gat_core::name::MountName::from_string("models".to_string()),
            gat_core::config::MountConfig {
                url: "u".to_string().into(),
                target: gp("data/models"),
                path: gat_core::lexical_path::GatSubpath::Root,
                rev: None,
                rev_lock: None,
                include: Vec::new(),
                exclude: Vec::new(),
            },
        );
        repo.write_config_fixture(&cfg).unwrap();

        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        // Moving a root-owned file *into* a mount's target must be
        // refused.
        let err = mv(
            &repo,
            Path::new("a.bin"),
            Path::new("data/models/a.bin"),
            false,
        )
        .unwrap_err();
        assert!(
            matches!(err, MoveError::MountOwned(_)),
            "expected MoveError::MountOwned, got: {err:?}"
        );

        // A mount-owned row (as `gat mount pull` would write) must not
        // be movable by `gat mv`.
        let mut lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        lock.upsert(
            gp("data/models/b.bin"),
            gat_core::oid::Oid::from_hex(&"b".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();
        let err = mv(
            &repo,
            Path::new("data/models/b.bin"),
            Path::new("c.bin"),
            false,
        )
        .unwrap_err();
        assert!(
            matches!(err, MoveError::MountOwned(_)),
            "expected MoveError::MountOwned, got: {err:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn mv_refuses_to_move_a_source_through_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("a.bin");
        std::fs::write(&outside_file, b"outside").unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();

        // Simulate a lock row that points through a symlinked ancestor
        // (e.g. tampered with, or arriving via an untrusted source).
        let mut lock = Lock::default();
        lock.upsert(
            gp("link/a.bin"),
            gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();

        let err = mv(&repo, Path::new("link/a.bin"), Path::new("b.bin"), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("symlinked ancestor"),
            "expected symlink-ancestor rejection, got: {msg}"
        );
        assert!(
            matches!(err, MoveError::Path(_)),
            "expected MoveError::Path, got: {err:?}"
        );
        assert!(
            outside_file.exists(),
            "file outside the worktree must not be moved"
        );
        assert!(!tmp.path().join("b.bin").exists());
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(
            entries.len(),
            1,
            "lock must remain untouched since the move failed"
        );
        assert_eq!(entries[0].path, "link/a.bin");
    }

    #[test]
    #[cfg(unix)]
    fn mv_refuses_to_move_a_destination_through_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();

        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();

        let err = mv(&repo, Path::new("a.bin"), Path::new("link/b.bin"), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("symlinked ancestor"),
            "expected symlink-ancestor rejection, got: {msg}"
        );
        assert!(
            matches!(err, MoveError::Path(_)),
            "expected MoveError::Path, got: {err:?}"
        );
        assert!(
            !outside.path().join("b.bin").exists(),
            "file must not be written outside the worktree via a symlinked destination ancestor"
        );
        assert!(
            tmp.path().join("a.bin").exists(),
            "source file must remain untouched since the move failed"
        );
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "a.bin");
    }

    /// Once the filesystem rename
    /// has already happened, a failure to persist `gat.lock` must not
    /// leave the file at `dst` while `gat.lock` still claims it's at
    /// `src` -- `mv` rolls the rename back locally instead. The source
    /// and destination live in a subdirectory with its own (writable)
    /// permissions so the rollback rename -- which only needs write
    /// access to that subdirectory, not the repo root where `gat.lock`
    /// lives -- can succeed even while the root is read-only.
    #[test]
    #[cfg(unix)]
    fn mv_rolls_back_the_rename_when_the_lock_write_fails_after_a_successful_rename() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("sub/a.bin")], &NoopProgress).unwrap();

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = mv(&repo, Path::new("sub/a.bin"), Path::new("sub/b.bin"), false);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.unwrap_err();
        assert!(
            matches!(err, MoveError::RolledBack { .. }),
            "expected MoveError::RolledBack, got: {err:?}"
        );
        assert!(
            tmp.path().join("sub/a.bin").exists(),
            "the rename must be rolled back so the file is back at its original path"
        );
        assert!(
            !tmp.path().join("sub/b.bin").exists(),
            "the destination must not keep the file after rollback"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("sub/a.bin")).unwrap(),
            b"payload"
        );
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(
            entries.len(),
            1,
            "gat.lock must remain unchanged since its write failed"
        );
        assert_eq!(entries[0].path, "sub/a.bin");
    }

    /// Variant-level coverage for the authoritative worktree rename error: a plain
    /// filesystem rename failure (source disappears out from under `mv`
    /// between the plan phase and the rename itself), reached before any
    /// tracked-state mutation, so there's nothing to roll back.
    #[test]
    fn mv_reports_a_plain_filesystem_rename_failure() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        // Removed after `mv`'s plan phase confirms `a.bin` is tracked and
        // confined, but before the `ApplyingChanges` rename itself runs.
        std::fs::remove_file(tmp.path().join("a.bin")).unwrap();

        let err = mv(&repo, Path::new("a.bin"), Path::new("b.bin"), false).unwrap_err();
        assert!(
            matches!(
                err,
                MoveError::Worktree(ref source)
                    if matches!(source.as_ref(), gat_engine::WorktreeMoveError::Rename { .. })
            ),
            "expected MoveError::Worktree(Rename), got: {err:?}"
        );
    }

    /// The root input boundary rejects a destination escaping the repository
    /// before constructing the authoritative typed command request.
    #[test]
    fn mv_rejects_a_destination_that_escapes_the_repository_root() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();

        let err = gat_core::lexical_path::GatPath::normalize(Path::new("../escaped")).unwrap_err();
        assert!(
            err.to_string().contains("escapes the repository root"),
            "{err}"
        );
    }

    fn mv_with_threads(
        repo: &Repo,
        src: &Path,
        dst: &Path,
        force: bool,
        threads: usize,
    ) -> Result<MoveOutcome> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| mv(repo, src, dst, force))
    }

    /// Remapping a large moved subtree's destination paths
    /// (non-sharded `lock.entries` rewrite and the sharded `moved_paths`
    /// construction) runs across rayon's pool. Single-threaded and
    /// default-parallelism runs against identical fixtures must produce
    /// the same outcome and the same final `gat.lock` content/order.
    #[test]
    fn mv_large_directory_matches_single_threaded_and_default_pools() {
        fn run_with_threads(threads: usize) -> (MoveOutcome, Vec<String>) {
            let tmp = test_repo();
            let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            std::fs::create_dir_all(tmp.path().join("bulk")).unwrap();
            let files: Vec<PathBuf> = (0..200)
                .map(|i| PathBuf::from(format!("bulk/file-{i:04}.bin")))
                .collect();
            for file in &files {
                std::fs::write(tmp.path().join(file), file.to_string_lossy().as_bytes()).unwrap();
            }
            add(&repo, &files, &NoopProgress).unwrap();

            let outcome =
                mv_with_threads(&repo, Path::new("bulk"), Path::new("moved"), false, threads)
                    .unwrap();
            let mut paths: Vec<String> = gat_io::LockStore::load_repository(
                &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
            )
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.path.to_string())
            .collect();
            paths.sort();
            (outcome, paths)
        }

        let single = run_with_threads(1);
        let default = run_with_threads(rayon::current_num_threads());
        assert_eq!(single, default);
        assert_eq!(single.1.len(), 200);
        assert!(single.1.first().unwrap().starts_with("moved/"));
    }

    /// The sparse, touched-shard-scoped move pipeline must resolve its whole
    /// read/planning phase -- matching, collision-checking, confinement -- under
    /// one `ResolvingSelection` task, sequential with and separate from the
    /// final `ApplyingChanges` task, never nested and never concurrent with it.
    #[test]
    fn mv_sparse_never_has_more_than_one_active_progress_task() {
        use crate::common::RecordingProgress;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        assert_eq!(
            matching_lock_shape(&repo, tmp.path()).unwrap(),
            Some(gat_core::lock::LockShardLevels::FLAT)
        );

        let progress = RecordingProgress::new();
        mv_with_progress(
            &repo,
            Path::new("a.bin"),
            Path::new("b.bin"),
            false,
            &progress,
        )
        .unwrap();

        assert_eq!(
            progress.max_active_tasks(),
            1,
            "the sparse mv pipeline must never have more than one progress task active at once"
        );
    }

    /// The full-`Lock` fallback path (reached when the on-disk lock shape
    /// doesn't match the configured `shard_levels`, i.e. a pending reshape)
    /// must uphold the same one-active-task invariant across its
    /// matching/collision-checking phase and its separate, later
    /// `ApplyingChanges` publish phase.
    #[test]
    fn mv_full_lock_fallback_never_has_more_than_one_active_progress_task() {
        use crate::common::RecordingProgress;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        assert_eq!(
            matching_lock_shape(&repo, tmp.path()).unwrap(),
            Some(gat_core::lock::LockShardLevels::FLAT)
        );

        // Force a pending reshape -- bump the configured shard_levels
        // without reshaping on disk yet -- so `mv` must take the
        // full-`Lock` fallback rather than the sparse pipeline.
        let mut cfg = repo.load_config().unwrap();
        cfg.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(1).unwrap());
        repo.write_config_fixture(&cfg).unwrap();
        assert!(
            matching_lock_shape(&repo, tmp.path()).unwrap().is_none(),
            "a shard_levels change with no reshape yet must force the fallback"
        );

        let progress = RecordingProgress::new();
        mv_with_progress(
            &repo,
            Path::new("a.bin"),
            Path::new("b.bin"),
            false,
            &progress,
        )
        .unwrap();

        assert_eq!(
            progress.max_active_tasks(),
            1,
            "the full-lock mv fallback must never have more than one progress task active at once"
        );
    }

    /// An error surfaced from
    /// inside `ResolvingSelection` (here, an existing untracked
    /// destination without `--force`) must never leave more than one task
    /// active, and `ApplyingChanges` must never have begun at all --
    /// exercising the error path, not only the success path already
    /// covered above.
    #[test]
    fn mv_error_path_never_has_more_than_one_active_progress_task() {
        use crate::common::RecordingProgress;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"source").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"destination").unwrap();

        let progress = RecordingProgress::new();
        mv_with_progress(
            &repo,
            Path::new("a.bin"),
            Path::new("b.bin"),
            false,
            &progress,
        )
        .unwrap_err();

        assert_eq!(
            progress.max_active_tasks(),
            1,
            "an error inside ResolvingSelection must never leave more than one task active"
        );
        assert_eq!(
            progress.count_of(gat_core::progress::ProgressOperation::ApplyingChanges),
            0,
            "ApplyingChanges must never begin once ResolvingSelection has already failed"
        );
    }
}
