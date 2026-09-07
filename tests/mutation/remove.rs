use gat_core::progress::ProgressReporter;
use gat_engine::Repository as Repo;
use std::path::Path;

fn layout(root: &Path) -> gat_io::RepositoryLayout {
    gat_io::RepositoryLayout::at(root.to_path_buf())
}
use std::path::PathBuf;

type RemoveError = gat_command::RemoveError;
type RemoveOutcome = gat_command::RemoveOutcome;
type Result<T> = std::result::Result<T, RemoveError>;

fn rm(repo: &Repo, paths: &[PathBuf], cached: bool) -> Result<RemoveOutcome> {
    rm_with_progress(repo, paths, cached, &gat_core::progress::NoopProgress)
}

fn rm_with_progress(
    repo: &Repo,
    paths: &[PathBuf],
    cached: bool,
    progress: &dyn ProgressReporter,
) -> Result<RemoveOutcome> {
    let paths = paths
        .iter()
        .map(gat_core::path_scope::normalize_path_scope)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    gat_command::remove_with_progress(repo, gat_command::RemoveRequest { paths, cached }, progress)
}

mod tests {
    use super::*;
    use crate::matching_lock_shape;
    use gat_core::lock::Lock;
    use gat_core::progress::NoopProgress;
    use gat_engine::Repository as Repo;

    use ::test_support::add;
    use test_support::git_repo_with_initial_commit as test_repo;

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path).unwrap()
    }

    #[test]
    fn rm_delete_failure_publishes_removal_and_retry_does_not_resume_cleanup() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let path = tmp.path().join("a.bin");
        std::fs::write(&path, b"tracked").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        let materialized = gat_engine::test_support::load_materialized_for_test(&repo).unwrap();
        let exclude_path = tmp.path().join(".git/info/exclude");
        let excludes = std::fs::read(&exclude_path).unwrap();
        // A directory at the tracked file path deterministically rejects remove_file,
        // including when the test runs with elevated filesystem permissions.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("untracked"), b"keep").unwrap();

        let error = rm(&repo, &[PathBuf::from("a.bin")], false).unwrap_err();
        assert!(matches!(error, RemoveError::Cleanup(source)
            if matches!(*source, gat_engine::WorktreeRemoveError::Delete { .. })));
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert_eq!(
            gat_engine::test_support::load_materialized_for_test(&repo)
                .unwrap()
                .entries,
            materialized.entries
        );
        assert_eq!(std::fs::read(&exclude_path).unwrap(), excludes);

        assert!(
            rm(&repo, &[PathBuf::from("a.bin")], false)
                .unwrap()
                .paths
                .is_empty()
        );
        assert_eq!(std::fs::read(path.join("untracked")).unwrap(), b"keep");
        assert_eq!(std::fs::read(&exclude_path).unwrap(), excludes);
        assert_eq!(
            gat_engine::test_support::load_materialized_for_test(&repo)
                .unwrap()
                .entries,
            materialized.entries
        );
    }

    #[test]
    fn rm_ownership_failure_leaves_stale_state_and_retry_does_not_resume() {
        for cached in [false, true] {
            let tmp = test_repo();
            let repo = Repo::at(tmp.path().to_path_buf());
            std::fs::write(tmp.path().join("a.bin"), b"tracked").unwrap();
            add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
            let excludes = std::fs::read(tmp.path().join(".git/info/exclude")).unwrap();
            let connection =
                rusqlite::Connection::open(tmp.path().join(".gat/state/state.sqlite3")).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_forget BEFORE UPDATE OF materialized_oid ON state
                 WHEN OLD.materialized_oid IS NOT NULL AND NEW.materialized_oid IS NULL
                 BEGIN SELECT RAISE(FAIL, 'injected ownership failure'); END;",
                )
                .unwrap();

            let error = rm(&repo, &[PathBuf::from("a.bin")], cached).unwrap_err();
            assert!(
                matches!(error, RemoveError::ForgetMaterialized { cached: actual, .. } if actual == cached)
            );
            assert!(
                gat_io::LockStore::load_repository(&layout(tmp.path()))
                    .unwrap()
                    .entries
                    .is_empty()
            );
            assert_eq!(tmp.path().join("a.bin").exists(), cached);
            assert_eq!(
                gat_engine::test_support::load_materialized_for_test(&repo)
                    .unwrap()
                    .entries
                    .len(),
                1
            );
            connection
                .execute_batch("DROP TRIGGER reject_forget;")
                .unwrap();
            assert!(
                rm(&repo, &[PathBuf::from("a.bin")], cached)
                    .unwrap()
                    .paths
                    .is_empty()
            );
            assert_eq!(
                gat_engine::test_support::load_materialized_for_test(&repo)
                    .unwrap()
                    .entries
                    .len(),
                1
            );
            assert_eq!(
                std::fs::read(tmp.path().join(".git/info/exclude")).unwrap(),
                excludes
            );
        }
    }

    #[test]
    fn rm_exclude_failure_is_post_publication_and_retry_does_not_repair_it() {
        for cached in [false, true] {
            let tmp = test_repo();
            let repo = Repo::at(tmp.path().to_path_buf());
            std::fs::write(tmp.path().join("a.bin"), b"tracked").unwrap();
            add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
            let exclude = tmp.path().join(".git/info/exclude");
            std::fs::remove_file(&exclude).unwrap();
            std::fs::create_dir(&exclude).unwrap();

            let error = rm(&repo, &[PathBuf::from("a.bin")], cached).unwrap_err();
            assert!(matches!(error, RemoveError::SyncExcludes(_)));
            assert!(
                gat_io::LockStore::load_repository(&layout(tmp.path()))
                    .unwrap()
                    .entries
                    .is_empty()
            );
            assert!(
                gat_engine::test_support::load_materialized_for_test(&repo)
                    .unwrap()
                    .entries
                    .is_empty()
            );
            assert_eq!(tmp.path().join("a.bin").exists(), cached);
            assert!(
                rm(&repo, &[PathBuf::from("a.bin")], cached)
                    .unwrap()
                    .paths
                    .is_empty()
            );
            assert!(exclude.is_dir());
        }
    }

    /// An `rm` whose selectors match nothing tracked must not
    /// republish `gat.lock` (or touch materialized state/excludes) with
    /// otherwise-identical content -- verified here by asserting the file
    /// isn't rewritten at all (permissions stripped so a write would fail).
    #[test]
    fn rm_on_an_untracked_path_does_not_rewrite_gat_lock() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        let lock_path = tmp.path().join("gat.lock");
        let before = std::fs::read(&lock_path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&lock_path).unwrap().permissions();
            perms.set_mode(0o400);
            std::fs::set_permissions(&lock_path, perms).unwrap();
        }

        let outcome = rm(&repo, &[PathBuf::from("not-tracked.bin")], false).unwrap();
        assert!(outcome.paths.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&lock_path).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&lock_path, perms).unwrap();
        }
        assert_eq!(
            std::fs::read(&lock_path).unwrap(),
            before,
            "gat.lock must be byte-for-byte unchanged when nothing matched"
        );
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    /// Same publication boundary as `add`'s equivalent test: once `rm`
    /// removes an entry and republishes `gat.lock`, the `SQLite` desired
    /// mirror must already agree -- without a separate
    /// `desired_index::refresh()` -- the repository mutation service applies
    /// canonical publication evidence to the mirror in the same operation.
    #[test]
    fn rm_seeds_the_desired_mirror_without_a_separate_refresh() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();
        add(
            &repo,
            &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            &NoopProgress,
        )
        .unwrap();

        rm(&repo, &[PathBuf::from("a.bin")], false).unwrap();

        let mirrored = gat_io::StateStore::open(&layout(tmp.path()))
            .unwrap()
            .load_desired_as_lock()
            .unwrap();
        let on_disk = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(mirrored.entries, on_disk.entries);
        assert_eq!(mirrored.entries.len(), 1);
        assert_eq!(mirrored.entries[0].path, "b.bin");
    }

    #[test]
    fn rm_cached_removes_lock_entry_unexcludes_and_keeps_file() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        rm(&repo, &[PathBuf::from("big.bin")], true).unwrap();
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        let exclude =
            std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap_or_default();
        assert!(!exclude.contains("big.bin"));
        assert!(tmp.path().join("big.bin").exists());
    }

    /// Regression test for GAT-ARCH-04: `rm --cached` must relinquish
    /// materialized-state ownership too, or a later `gat sync` would see
    /// `(desired=None, prior=Some)` for the retained file and schedule it
    /// for removal, turning "keep this file" into a later deletion.
    #[test]
    fn rm_cached_forgets_materialized_state_so_a_later_sync_keeps_the_file() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        ::test_support::sync(&repo, &NoopProgress).unwrap();
        assert!(
            gat_engine::test_support::load_materialized_for_test(&repo)
                .unwrap()
                .entries
                .iter()
                .any(|e| e.path == "big.bin")
        );

        rm(&repo, &[PathBuf::from("big.bin")], true).unwrap();
        assert!(
            gat_engine::test_support::load_materialized_for_test(&repo)
                .unwrap()
                .entries
                .iter()
                .all(|e| e.path != "big.bin"),
            "materialized state must forget `big.bin` once `gat.lock` no longer tracks it"
        );

        ::test_support::sync(&repo, &NoopProgress).unwrap();
        assert!(
            tmp.path().join("big.bin").exists(),
            "a later sync must not delete a file `rm --cached` deliberately kept on disk"
        );
    }

    #[test]
    fn rm_without_cached_also_deletes_the_file() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        rm(&repo, &[PathBuf::from("big.bin")], false).unwrap();
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!tmp.path().join("big.bin").exists());
    }

    /// Normal (non-`--cached`) `rm` deletes the file *and* must forget
    /// materialized state for it, so a stray leftover row can't confuse a
    /// later sync either.
    #[test]
    fn rm_without_cached_also_forgets_materialized_state() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        ::test_support::sync(&repo, &NoopProgress).unwrap();

        rm(&repo, &[PathBuf::from("big.bin")], false).unwrap();
        assert!(
            gat_engine::test_support::load_materialized_for_test(&repo)
                .unwrap()
                .entries
                .iter()
                .all(|e| e.path != "big.bin")
        );
    }

    #[test]
    fn rm_on_a_directory_removes_all_nested_lock_entries_and_the_directory() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/nested")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/nested/b.bin"), b"b").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            2
        );

        let outcome = rm(&repo, &[PathBuf::from("data")], false).unwrap();
        assert_eq!(outcome.paths.len(), 2);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!tmp.path().join("data").exists());
    }

    #[test]
    fn rm_on_a_directory_reports_the_real_tracked_file_count_and_keeps_untracked_files() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/nested")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/nested/b.bin"), b"b").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            2
        );

        // An untracked file living alongside the tracked ones must survive
        // `gat rm data`: only the tracked rows are reported and deleted, so
        // the count reflects the real number of tracked files (not "1 file
        // removed" for the folder), and the folder isn't blindly wiped.
        std::fs::write(tmp.path().join("data/untracked.txt"), b"keep me").unwrap();

        let outcome = rm(&repo, &[PathBuf::from("data")], false).unwrap();
        assert_eq!(outcome.paths.len(), 2);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!tmp.path().join("data/a.bin").exists());
        assert!(!tmp.path().join("data/nested").exists());
        assert!(tmp.path().join("data").exists());
        assert!(tmp.path().join("data/untracked.txt").exists());
    }

    /// The touched-ancestor pruning walk must climb through every level a
    /// deleted file's directory chain vacates, not just its immediate
    /// parent -- a deeply nested tracked file (`a/b/c/d/e.bin`) whose whole
    /// chain becomes empty must leave nothing behind up to (but never
    /// including) the repository root.
    #[test]
    fn rm_prunes_every_level_of_a_deeply_nested_directory_chain() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("a/b/c/d")).unwrap();
        std::fs::write(tmp.path().join("a/b/c/d/e.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("a/b/c/d/e.bin")], &NoopProgress).unwrap();

        let outcome = rm(&repo, &[PathBuf::from("a/b/c/d/e.bin")], false).unwrap();
        assert_eq!(outcome.paths.len(), 1);
        assert!(
            !tmp.path().join("a").exists(),
            "the whole empty chain up to the repo root must be pruned"
        );
    }

    /// A single file `depth` levels deep must prune every one of its
    /// unique ancestor directories in one pass -- the cleanup path
    /// performs at most one `remove_dir` attempt per unique touched
    /// ancestor and no separate symlink-metadata probing pass, so this
    /// must succeed and leave nothing behind even for a fairly deep
    /// chain.
    #[test]
    fn rm_prunes_a_deep_chain_of_unique_touched_ancestors() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let depth = 24usize;
        let mut rel = PathBuf::new();
        for i in 0..depth {
            rel = rel.join(format!("d{i}"));
        }
        std::fs::create_dir_all(tmp.path().join(&rel)).unwrap();
        let file_rel = rel.join("leaf.bin");
        std::fs::write(tmp.path().join(&file_rel), b"payload").unwrap();
        add(&repo, std::slice::from_ref(&file_rel), &NoopProgress).unwrap();

        let outcome = rm(&repo, &[file_rel], false).unwrap();
        assert_eq!(outcome.paths.len(), 1);
        assert!(!tmp.path().join("d0").exists());
    }

    /// An untracked file at any level of the chain must stop pruning right
    /// there: everything below it (now empty) is still removed, but the
    /// directory holding the untracked file, and everything above it, must
    /// survive.
    #[test]
    fn rm_stops_pruning_at_a_directory_still_holding_an_untracked_file() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("a/b/c")).unwrap();
        std::fs::write(tmp.path().join("a/b/c/e.bin"), b"payload").unwrap();
        std::fs::write(tmp.path().join("a/b/keep.txt"), b"keep me").unwrap();
        add(&repo, &[PathBuf::from("a/b/c/e.bin")], &NoopProgress).unwrap();

        rm(&repo, &[PathBuf::from("a/b/c/e.bin")], false).unwrap();
        assert!(
            !tmp.path().join("a/b/c").exists(),
            "the now-empty leaf directory must still be pruned"
        );
        assert!(
            tmp.path().join("a/b").exists(),
            "a directory holding an untracked file must survive"
        );
        assert!(tmp.path().join("a/b/keep.txt").exists());
    }

    /// An empty sibling directory that has nothing to do with any deleted
    /// path must never be touched: pruning is derived strictly from the
    /// ancestors of concretely deleted tracked paths, never from a
    /// directory scan of the selector's own subtree.
    #[test]
    fn rm_leaves_an_unrelated_empty_directory_untouched() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/tracked")).unwrap();
        std::fs::create_dir_all(tmp.path().join("data/untouched")).unwrap();
        std::fs::write(tmp.path().join("data/tracked/f.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("data/tracked/f.bin")], &NoopProgress).unwrap();

        rm(&repo, &[PathBuf::from("data/tracked/f.bin")], false).unwrap();
        assert!(!tmp.path().join("data/tracked").exists());
        assert!(
            tmp.path().join("data/untouched").exists(),
            "an unrelated empty sibling directory must survive"
        );
        assert!(
            tmp.path().join("data").exists(),
            "the parent still holds the untouched sibling"
        );
    }

    /// A glob selector's matches are scattered across arbitrarily many
    /// directories: pruning must be derived from each individually matched
    /// row's own resolved path, never from the glob pattern string or any
    /// notion of "the selected subtree".
    #[test]
    fn rm_glob_prunes_every_directory_its_matches_actually_vacated() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("g/one")).unwrap();
        std::fs::create_dir_all(tmp.path().join("g/two")).unwrap();
        std::fs::write(tmp.path().join("g/one/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("g/two/b.bin"), b"b").unwrap();
        std::fs::write(tmp.path().join("g/two/c.txt"), b"c").unwrap();
        add(
            &repo,
            &[
                PathBuf::from("g/one/a.bin"),
                PathBuf::from("g/two/b.bin"),
                PathBuf::from("g/two/c.txt"),
            ],
            &NoopProgress,
        )
        .unwrap();

        let outcome = rm(&repo, &[PathBuf::from("g/**/*.bin")], false).unwrap();
        assert_eq!(outcome.paths.len(), 2);
        assert!(
            !tmp.path().join("g/one").exists(),
            "`g/one` is left empty by the glob's only match under it"
        );
        assert!(
            tmp.path().join("g/two").exists(),
            "`g/two` still holds the untouched `c.txt`"
        );
        assert!(tmp.path().join("g/two/c.txt").exists());
    }

    /// A selector matching zero tracked paths must do no directory cleanup
    /// whatsoever -- not even a lookup of whether its own directory exists
    /// -- so an untracked, otherwise-empty directory named by the selector
    /// is left exactly as it was.
    #[test]
    fn rm_glob_matching_nothing_tracked_does_no_directory_cleanup() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("untracked")).unwrap();

        let outcome = rm(&repo, &[PathBuf::from("untracked/*.bin")], false).unwrap();
        assert!(outcome.paths.is_empty());
        assert!(tmp.path().join("untracked").exists());
    }

    /// `--cached` must never touch the working tree at all: the tracked
    /// file (and the now-untracked-but-still-materialized directory) stay
    /// on disk exactly as they were, even though the directory would have
    /// become empty had the file actually been deleted.
    #[test]
    fn rm_cached_does_no_directory_cleanup() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("data/a.bin")], &NoopProgress).unwrap();

        let outcome = rm(&repo, &[PathBuf::from("data/a.bin")], true).unwrap();
        assert_eq!(outcome.paths.len(), 1);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(tmp.path().join("data").exists());
        assert!(tmp.path().join("data/a.bin").exists());
    }

    /// Pruning must stop precisely at (and never remove) the repository
    /// root itself, even when every tracked file directly at the root is
    /// removed.
    #[test]
    fn rm_pruning_never_removes_the_repository_root() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("only.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("only.bin")], &NoopProgress).unwrap();

        rm(&repo, &[PathBuf::from("only.bin")], false).unwrap();
        assert!(
            tmp.path().exists(),
            "the repository root must never be pruned"
        );
        assert!(!tmp.path().join("only.bin").exists());
    }

    /// Confinement/symlink safety must hold for the ancestor-pruning pass
    /// itself, not only for the earlier per-file `confine_mutation` check:
    /// a symlinked directory living *outside* any deleted path's ancestor
    /// chain must never be considered a pruning candidate, and its target
    /// must survive untouched.
    #[test]
    #[cfg(unix)]
    fn rm_pruning_never_follows_an_unrelated_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("victim.bin"), b"outside").unwrap();

        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"payload").unwrap();
        symlink(outside.path(), tmp.path().join("data/link")).unwrap();
        add(&repo, &[PathBuf::from("data/a.bin")], &NoopProgress).unwrap();

        rm(&repo, &[PathBuf::from("data/a.bin")], false).unwrap();
        // `data` still holds the untracked `link` symlink, so it survives,
        // and its target must never be touched or traversed.
        assert!(tmp.path().join("data").exists());
        assert!(tmp.path().join("data/link").exists());
        assert!(outside.path().join("victim.bin").exists());
    }

    #[test]
    fn rm_non_glob_uses_exact_and_nested_scope_matching() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/nested")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/nested/b.bin"), b"b").unwrap();
        std::fs::write(tmp.path().join("data.bin"), b"c").unwrap();
        add(
            &repo,
            &[PathBuf::from("data"), PathBuf::from("data.bin")],
            &NoopProgress,
        )
        .unwrap();

        let outcome = rm(&repo, &[PathBuf::from("data")], true).unwrap();
        let removed: Vec<String> = outcome.paths.into_iter().map(|p| p.to_string()).collect();
        assert_eq!(
            removed,
            vec!["data/a.bin".to_string(), "data/nested/b.bin".to_string()]
        );
        let remaining: Vec<String> = gat_io::LockStore::load_repository(
            &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
        )
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
        assert_eq!(remaining, vec!["data.bin".to_string()]);

        let outcome = rm(&repo, &[PathBuf::from("data.bin")], true).unwrap();
        let removed: Vec<String> = outcome.paths.into_iter().map(|p| p.to_string()).collect();
        assert_eq!(removed, vec!["data.bin".to_string()]);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn rm_normalizes_messy_path_spellings_before_matching_the_lock() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/nested")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/nested/b.bin"), b"b").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            2
        );

        // "./data/" should behave identically to the clean "data" spelling.
        rm(&repo, &[PathBuf::from("./data/")], false).unwrap();
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!tmp.path().join("data").exists());
    }

    #[test]
    fn rm_removes_lock_entry_even_when_file_already_missing_on_disk() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        std::fs::remove_file(tmp.path().join("big.bin")).unwrap();
        let outcome = rm(&repo, &[PathBuf::from("big.bin")], false).unwrap();
        assert_eq!(outcome.paths.len(), 1);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn rm_removes_lock_entries_even_when_directory_already_missing_on_disk() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/nested")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/nested/b.bin"), b"b").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            2
        );
        std::fs::remove_dir_all(tmp.path().join("data")).unwrap();

        let outcome = rm(&repo, &[PathBuf::from("data")], false).unwrap();
        assert_eq!(outcome.paths.len(), 2);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn rm_matches_a_glob_pattern_against_lock_entries_even_if_files_are_gone() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("g")).unwrap();
        std::fs::write(tmp.path().join("g/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("g/b.bin"), b"b").unwrap();
        std::fs::write(tmp.path().join("g/c.txt"), b"c").unwrap();
        add(
            &repo,
            &[
                PathBuf::from("g/a.bin"),
                PathBuf::from("g/b.bin"),
                PathBuf::from("g/c.txt"),
            ],
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            3
        );

        // Delete the actual files behind gat's back: the glob still has
        // to match against `gat.lock`'s rows, not the (now empty) working
        // tree, so the two `.bin` rows are dropped and `c.txt`'s row is
        // untouched regardless of file presence.
        std::fs::remove_file(tmp.path().join("g/a.bin")).unwrap();
        std::fs::remove_file(tmp.path().join("g/b.bin")).unwrap();

        let outcome = rm(&repo, &[PathBuf::from("g/*.bin")], true).unwrap();
        assert_eq!(outcome.paths.len(), 2);
        let remaining: Vec<String> = gat_io::LockStore::load_repository(
            &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
        )
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
        assert_eq!(remaining, vec!["g/c.txt".to_string()]);
    }

    #[test]
    fn rm_glob_is_non_recursive_unless_double_star_is_used() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/deep")).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/deep/a.bin"), b"a").unwrap();
        add(
            &repo,
            &[
                PathBuf::from("a.bin"),
                PathBuf::from("data/a.bin"),
                PathBuf::from("data/deep/a.bin"),
            ],
            &NoopProgress,
        )
        .unwrap();

        let removed = rm(&repo, &[PathBuf::from("*.bin")], true).unwrap();
        let removed_paths: Vec<String> = removed.paths.into_iter().map(|p| p.to_string()).collect();
        assert_eq!(removed_paths, vec!["a.bin".to_string()]);

        let mut remaining: Vec<String> = gat_io::LockStore::load_repository(
            &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
        )
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
        remaining.sort();
        assert_eq!(remaining, vec!["data/a.bin", "data/deep/a.bin"]);

        let removed = rm(&repo, &[PathBuf::from("**/*.bin")], true).unwrap();
        let mut removed_paths: Vec<String> =
            removed.paths.into_iter().map(|p| p.to_string()).collect();
        removed_paths.sort();
        assert_eq!(removed_paths, vec!["data/a.bin", "data/deep/a.bin"]);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn rm_rejects_rooted_scope_without_removing_everything() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        assert!(rm(&repo, &[PathBuf::from("/")], true).is_err());
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn rm_rejects_rooted_and_unc_inputs_consistently() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        for input in [r"\\server\share", "//server/share"] {
            assert!(rm(&repo, &[PathBuf::from(input)], true).is_err());
        }
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn rm_glob_refuses_to_touch_a_source_owned_lock_row() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
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
        repo.save_config(&cfg).unwrap();
        let mut lock = Lock::default();
        lock.upsert(
            gp("data/models/a.bin"),
            gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();

        assert!(rm(&repo, &[PathBuf::from("data/models/*.bin")], true).is_err());
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn rm_refuses_a_source_owned_lock_row() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
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
        repo.save_config(&cfg).unwrap();
        // Simulate a mount-owned row (`gat mount pull` writes
        // these; `rm` must not be able to touch them either directly or
        // via an ancestor directory argument).
        let mut lock = Lock::default();
        lock.upsert(
            gp("data/models/a.bin"),
            gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();

        assert!(rm(&repo, &[PathBuf::from("data/models/a.bin")], true).is_err());
        assert!(rm(&repo, &[PathBuf::from("data")], true).is_err());
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    /// A glob selector has no lexical locality, so it streams the whole
    /// ordered desired cursor -- but it is still a *sparse* mutation
    /// only the shards holding the rows it actually claimed
    /// get rewritten, exactly like a prefix selector.
    #[test]
    #[cfg(unix)]
    fn rm_with_a_glob_selector_only_rewrites_the_shards_it_matched() {
        use gat_core::lock::{LockShardId, LockShardLevels};
        use std::collections::HashMap;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut cfg = repo.load_config().unwrap();
        cfg.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(1).unwrap());
        repo.save_config(&cfg).unwrap();
        std::fs::create_dir_all(tmp.path().join("keep")).unwrap();
        let mut paths = Vec::new();
        for i in 0..20 {
            let path = format!("glob-{i}.dat");
            std::fs::write(tmp.path().join(&path), format!("payload-{i}")).unwrap();
            paths.push(PathBuf::from(path));
        }
        for i in 0..20 {
            let path = format!("keep/other-{i}.bin");
            std::fs::write(tmp.path().join(&path), format!("keep-{i}")).unwrap();
            paths.push(PathBuf::from(path));
        }
        add(&repo, &paths, &NoopProgress).unwrap();

        let matched_shards: std::collections::HashSet<LockShardId> = (0..20)
            .map(|i| {
                LockShardId::for_path(
                    &gp(&format!("glob-{i}.dat")),
                    LockShardLevels::new(1).unwrap(),
                )
            })
            .collect();
        let io_layout = layout(tmp.path());
        let before: HashMap<LockShardId, u64> =
            gat_io::lock_test_support::shard_inodes(&io_layout).unwrap();

        let outcome = rm(&repo, &[PathBuf::from("*.dat")], true).unwrap();
        assert_eq!(outcome.paths.len(), 20);

        let remaining: Vec<String> = gat_io::LockStore::load_repository(
            &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
        )
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.path.to_string())
        .collect();
        assert_eq!(remaining.len(), 20);
        assert!(remaining.iter().all(|p| p.starts_with("keep/")));

        for (shard_id, ino_after) in gat_io::lock_test_support::shard_inodes(&io_layout).unwrap() {
            let Some(ino_before) = before.get(&shard_id) else {
                continue;
            };
            if !matched_shards.contains(&shard_id) {
                assert_eq!(
                    *ino_before, ino_after,
                    "shard {shard_id} was rewritten by a glob rm that never matched it"
                );
            }
        }
    }

    #[test]
    fn rm_bounded_glob_uses_bounded_candidates_not_full_scan() {
        use gat_io::lock_flat_publish_test_support as publication;
        use gat_io::state_test_support as test_support;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut paths = Vec::new();
        for i in 0..80 {
            let path = format!("other/file-{i:03}.bin");
            std::fs::create_dir_all(tmp.path().join("other")).unwrap();
            std::fs::write(tmp.path().join(&path), "x").unwrap();
            paths.push(PathBuf::from(path));
        }
        std::fs::create_dir_all(tmp.path().join("data/models")).unwrap();
        for name in ["model-a.onnx", "model-b.bin", "other.onnx"] {
            let path = format!("data/models/{name}");
            std::fs::write(tmp.path().join(&path), "y").unwrap();
            paths.push(PathBuf::from(path));
        }
        add(&repo, &paths, &NoopProgress).unwrap();

        publication::reset_flat_shard_publish_counters();
        let before = test_support::snapshot();
        let outcome = rm(&repo, &[PathBuf::from("data/models/model-*.onnx")], true).unwrap();
        let after = test_support::snapshot();
        assert_eq!(publication::flat_shard_publish_calls(), 1);
        let candidate_rows_visited =
            after.3 - before.3 - publication::flat_shard_publish_row_total();

        assert_eq!(outcome.paths.len(), 1);
        assert!(
            candidate_rows_visited <= 3,
            "bounded glob should visit only its lexical-prefix candidates, not the whole desired set"
        );
    }

    /// Multiple glob selectors have no lexical locality among them either,
    /// so a naive per-selector scan would stream the whole ordered desired
    /// cursor once per glob argument (`O(M*Q)` for `M` globs, plus each
    /// selector's own one-time literal-precedence classification check).
    /// They must instead share one merged full-cursor pass: with `M` glob
    /// arguments the selection `with_desired_rows` call count is `M + 1` (one
    /// narrow classification existence check per glob argument, plus
    /// exactly one merged removal scan) -- never `2 * M` (a separate full
    /// removal scan per glob argument too) -- while still producing
    /// identical claim/removal results to running each selector separately.
    /// The separate flat-publication cursor is counted independently.
    #[test]
    fn rm_collapses_multiple_glob_selectors_into_one_full_cursor_pass() {
        use gat_io::lock_flat_publish_test_support as publication;
        use gat_io::state_test_support as test_support;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut paths = Vec::new();
        for name in ["a.bin", "b.bin", "c.txt", "keep.dat"] {
            std::fs::write(tmp.path().join(name), name).unwrap();
            paths.push(PathBuf::from(name));
        }

        add(&repo, &paths, &NoopProgress).unwrap();

        let glob_selectors = [
            PathBuf::from("*.bin"),
            PathBuf::from("*.txt"),
            PathBuf::from("keep.*"),
        ];
        publication::reset_flat_shard_publish_counters();
        let (_, calls_before, ..) = test_support::snapshot();
        let outcome = rm(&repo, &glob_selectors, true).unwrap();
        let (_, calls_after, ..) = test_support::snapshot();
        // The publication cursor is empty, so it removes the lock without rendering rows.
        assert_eq!(publication::flat_shard_publish_calls(), 0);

        assert_eq!(outcome.paths.len(), 4);
        assert_eq!(
            calls_after - calls_before - 1,
            glob_selectors.len() + 1,
            "{} glob selectors must share exactly one merged with_desired_rows \
             removal scan (plus one classification check per selector), not \
             one removal scan each",
            glob_selectors.len()
        );

        assert!(
            gat_io::LockStore::load_repository(&gat_io::RepositoryLayout::at(
                tmp.path().to_path_buf(),
            ))
            .unwrap()
            .entries
            .is_empty()
        );
    }

    #[test]
    fn rm_mixed_literal_and_bounded_globs_share_one_logical_candidate_stream() {
        use gat_io::lock_flat_publish_test_support as publication;
        use gat_io::state_test_support as test_support;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/models")).unwrap();
        std::fs::create_dir_all(tmp.path().join("assets")).unwrap();
        let paths = [
            "data/a.bin",
            "data/models/model-a.onnx",
            "data/models/model-b.bin",
            "assets/x.bin",
            "other/y.bin",
        ];
        let add_paths: Vec<PathBuf> = paths
            .iter()
            .map(|path| {
                if let Some(parent) = tmp.path().join(path).parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(tmp.path().join(path), path).unwrap();
                PathBuf::from(path)
            })
            .collect();
        add(&repo, &add_paths, &NoopProgress).unwrap();

        let selectors = [
            PathBuf::from("data"),
            PathBuf::from("data/models/model-*.onnx"),
            PathBuf::from("assets/*.bin"),
        ];
        publication::reset_flat_shard_publish_counters();
        let before = test_support::snapshot();
        let outcome = rm(&repo, &selectors, true).unwrap();
        let after = test_support::snapshot();
        assert_eq!(publication::flat_shard_publish_calls(), 1);

        assert_eq!(outcome.paths.len(), 4);
        assert_eq!(
            after.1 - before.1 - publication::flat_shard_publish_calls(),
            selectors.len(),
            "two glob classification checks plus one removal stream"
        );
        assert_eq!(
            after.3 - before.3 - publication::flat_shard_publish_row_total(),
            4,
            "bounded literal+glob selection should only visit the coalesced candidate union"
        );
    }

    #[test]
    fn rm_unbounded_glob_keeps_single_full_scan_fallback() {
        use gat_io::lock_flat_publish_test_support as publication;
        use gat_io::state_test_support as test_support;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut paths = Vec::new();
        for name in ["a.bin", "b.bin", "c.txt", "d.txt"] {
            std::fs::write(tmp.path().join(name), name).unwrap();
            paths.push(PathBuf::from(name));
        }
        add(&repo, &paths, &NoopProgress).unwrap();

        publication::reset_flat_shard_publish_counters();
        let before = test_support::snapshot();
        let outcome = rm(&repo, &[PathBuf::from("*.bin")], true).unwrap();
        let after = test_support::snapshot();
        assert_eq!(publication::flat_shard_publish_calls(), 1);
        assert_eq!(outcome.paths.len(), 2);
        assert_eq!(
            after.1 - before.1 - publication::flat_shard_publish_calls(),
            2,
            "one classification existence check plus one full removal stream"
        );
        assert_eq!(
            after.3 - before.3 - publication::flat_shard_publish_row_total(),
            4
        );
    }

    /// The mutation side of the same `M`-glob merge must not regress into
    /// once-per-glob-selector deletion: once the merged cursor pass above
    /// has resolved the authoritative claimed set, `remove_paths` (the
    /// exact-path deletion used by root/glob selectors, since they have no
    /// indexed range) must run exactly once total, never once per glob
    /// argument.
    #[test]
    fn rm_applies_the_merged_glob_removal_set_exactly_once() {
        use gat_io::state_test_support as test_support;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut paths = Vec::new();
        for name in ["a.bin", "b.bin", "c.txt", "keep.dat"] {
            std::fs::write(tmp.path().join(name), name).unwrap();
            paths.push(PathBuf::from(name));
        }
        add(&repo, &paths, &NoopProgress).unwrap();

        let glob_selectors = [
            PathBuf::from("*.bin"),
            PathBuf::from("*.txt"),
            PathBuf::from("keep.*"),
        ];
        let calls_before = test_support::remove_paths_calls();
        let outcome = rm(&repo, &glob_selectors, true).unwrap();
        let calls_after = test_support::remove_paths_calls();

        assert_eq!(outcome.paths.len(), 4);
        assert_eq!(
            calls_after - calls_before,
            1,
            "{} glob selectors must apply exactly one merged remove_paths \
             deletion, not one per selector",
            glob_selectors.len()
        );
    }

    /// A path claimed by an earlier selector must not also be reported
    /// against (or double-removed by) a later, overlapping selector, even
    /// when the merged full-cursor path (point 5 above) checks every
    /// selector against every row instead of running each selector's scan
    /// separately: claim precedence is "first selector by argument order
    /// that matches", exactly mirroring the full-`Lock` fallback's
    /// `selectors.iter().position(...)` rule.
    #[test]
    fn rm_overlapping_prefix_and_glob_selectors_claim_each_path_exactly_once() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("other.bin"), b"b").unwrap();
        add(
            &repo,
            &[PathBuf::from("data/a.bin"), PathBuf::from("other.bin")],
            &NoopProgress,
        )
        .unwrap();

        // "data" (a prefix) is listed first and claims "data/a.bin"; the
        // glob "*.bin" that follows also matches it, but must not claim it
        // a second time -- and must still separately claim "other.bin".
        let outcome = rm(
            &repo,
            &[PathBuf::from("data"), PathBuf::from("*.bin")],
            true,
        )
        .unwrap();
        let mut removed: Vec<String> = outcome.paths.into_iter().map(|p| p.to_string()).collect();
        removed.sort();
        assert_eq!(
            removed,
            vec!["data/a.bin".to_string(), "other.bin".to_string()]
        );
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    #[cfg(unix)]
    fn rm_in_a_sharded_repo_only_rewrites_the_touched_shard_file() {
        use gat_core::lock::{LockShardId, LockShardLevels};
        use std::collections::HashMap;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut cfg = repo.load_config().unwrap();
        cfg.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(1).unwrap());
        repo.save_config(&cfg).unwrap();
        let mut paths = Vec::new();
        for i in 0..40 {
            let path = format!("file-{i}.bin");
            std::fs::write(tmp.path().join(&path), format!("payload-{i}")).unwrap();
            paths.push(PathBuf::from(path));
        }
        add(&repo, &paths, &NoopProgress).unwrap();

        let counts: HashMap<_, _> = gat_io::LockStore::load_repository(
            &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
        )
        .unwrap()
        .entries
        .into_iter()
        .fold(HashMap::<LockShardId, usize>::new(), |mut counts, entry| {
            *counts
                .entry(LockShardId::for_path(
                    &entry.path,
                    LockShardLevels::new(1).unwrap(),
                ))
                .or_default() += 1;
            counts
        });
        let removed_path = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .find(|path| {
                counts[&LockShardId::for_path(&gp(path), LockShardLevels::new(1).unwrap())] > 1
            })
            .unwrap();
        let removed_shard =
            LockShardId::for_path(&gp(&removed_path), LockShardLevels::new(1).unwrap());

        let io_layout = layout(tmp.path());
        let shard_files = gat_io::lock_test_support::shard_inodes(&io_layout).unwrap();

        rm(&repo, &[PathBuf::from(&removed_path)], true).unwrap();

        let after = gat_io::lock_test_support::shard_inodes(&io_layout).unwrap();
        let before: HashMap<_, _> = shard_files;
        for (shard_id, ino_after) in after {
            if shard_id == removed_shard {
                assert_ne!(before[&shard_id], ino_after);
            } else {
                assert_eq!(before[&shard_id], ino_after);
            }
        }
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .iter()
                .all(|e| e.path != removed_path)
        );
    }

    /// A flat `gat.lock` `rm` must take the same sparse,
    /// touched-shard-scoped pipeline as a sharded one (flat is
    /// the degenerate one-shard case, not a separate full-lock rewrite).
    /// Asserts that removing one of several tracked files from a flat repo
    /// keeps the shape `Some(OnDiskShape::Flat)`, keeps `gat.lock` a plain
    /// file rather than a `gat.lock/` directory, and leaves the desired
    /// mirror with exactly one logical shard identity keyed by the
    /// `"gat.lock"` sentinel.
    #[test]
    fn rm_in_a_flat_repo_uses_the_sparse_pipeline_and_stays_a_single_file() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();
        add(
            &repo,
            &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(
            matching_lock_shape(&repo, tmp.path()).unwrap(),
            Some(gat_core::lock::LockShardLevels::FLAT)
        );

        rm(&repo, &[PathBuf::from("a.bin")], false).unwrap();

        assert!(
            tmp.path().join("gat.lock").is_file(),
            "flat rm must publish a single gat.lock file, never a gat.lock/ directory"
        );
        assert_eq!(
            matching_lock_shape(&repo, tmp.path()).unwrap(),
            Some(gat_core::lock::LockShardLevels::FLAT)
        );
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "b.bin");
        assert!(!tmp.path().join("a.bin").exists());

        let mut store = gat_io::StateStore::open(&layout(tmp.path())).unwrap();
        gat_engine::test_support::refresh_desired_index(&repo, &mut store).unwrap();
        let shard_ids = gat_io::state_shard_ids_for_test(&store).unwrap();
        assert_eq!(
            shard_ids,
            vec![gat_core::lock::LockShardId::flat()],
            "flat desired rows must all share one logical shard id"
        );
    }

    #[test]
    #[cfg(unix)]
    fn rm_refuses_to_remove_a_file_through_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("victim.bin");
        std::fs::write(&outside_file, b"outside").unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();

        // Simulate a lock row that was tampered with (or that arrived via
        // an untrusted source) to point through a symlinked ancestor.
        let mut lock = Lock::default();
        lock.upsert(
            gp("link/victim.bin"),
            gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();

        let err = rm(&repo, &[PathBuf::from("link/victim.bin")], false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("symlinked ancestor"),
            "expected symlink-ancestor rejection, got: {msg}"
        );
        assert!(
            outside_file.exists(),
            "file outside the worktree must not be removed"
        );
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            1,
            "lock row must remain untouched since the removal failed"
        );
    }

    #[test]
    #[cfg(unix)]
    fn rm_cached_skips_worktree_resolution_for_selected_paths() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("victim.bin");
        std::fs::write(&outside_file, b"outside").unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();

        let mut lock = Lock::default();
        lock.upsert(
            gp("link/victim.bin"),
            gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();

        rm(&repo, &[PathBuf::from("link/victim.bin")], true).unwrap();
        assert!(outside_file.exists());
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    #[cfg(unix)]
    fn rm_on_a_symlinked_directory_argument_matching_nothing_tracked_does_no_cleanup() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("nested")).unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();

        // No lock rows live under `link`: directory cleanup is derived only
        // from concretely removed tracked paths, so a selector matching
        // nothing must never even look at (let alone recurse into) the
        // selector's own directory argument -- succeeding with nothing
        // removed, and leaving the symlinked target's contents untouched.
        let outcome = rm(&repo, &[PathBuf::from("link")], false).unwrap();
        assert!(outcome.paths.is_empty());
        assert!(outside.path().join("nested").exists());
    }

    /// `gat.lock` is persisted
    /// *before* any working-tree delete, specifically so a failure here
    /// can never delete user data while `gat.lock` still claims the path
    /// is tracked. If persisting `gat.lock` fails, the file must still be
    /// on disk and `gat.lock` must still list it as tracked.
    #[test]
    #[cfg(unix)]
    fn rm_leaves_the_file_on_disk_and_gat_lock_unchanged_when_the_lock_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = rm(&repo, &[PathBuf::from("big.bin")], false);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            result.is_err(),
            "rm must fail when gat.lock can't be written"
        );
        assert!(
            tmp.path().join("big.bin").exists(),
            "the working-tree file must not be deleted when gat.lock failed to persist first"
        );
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].path, "big.bin",
            "gat.lock must still list the path as tracked since its write failed"
        );
    }

    /// Same contract as above for `--cached`: a failed `gat.lock` write
    /// must leave the file on disk and `gat.lock` unchanged, which for
    /// `--cached` is trivially also its steady-state behavior (the file
    /// is never touched either way), but the tracked-state answer still
    /// has to be right.
    #[test]
    #[cfg(unix)]
    fn rm_cached_leaves_gat_lock_unchanged_when_the_lock_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = rm(&repo, &[PathBuf::from("big.bin")], true);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            result.is_err(),
            "rm --cached must fail when gat.lock can't be written"
        );
        assert!(tmp.path().join("big.bin").exists());
        let entries = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "big.bin");
    }

    // Path scope, root, and literal-versus-glob cases.

    /// `gat rm --cached .` removes all Gat-managed desired rows.
    #[test]
    fn rm_cached_dot_removes_all() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();
        add(
            &repo,
            &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            &NoopProgress,
        )
        .unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            2
        );

        let outcome = rm(&repo, &[PathBuf::from(".")], true).unwrap();
        assert_eq!(outcome.paths.len(), 2);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        // Files must still exist since we used --cached
        assert!(tmp.path().join("a.bin").exists());
        assert!(tmp.path().join("b.bin").exists());
    }

    /// Literal `file[1].bin` round-trips through `gat add` and
    /// `gat rm --cached` without being misinterpreted as a glob pattern.
    #[test]
    fn rm_literal_with_glob_metachar_roundtrips() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("file[1].bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("file[1].bin")], &NoopProgress).unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            1
        );

        rm(&repo, &[PathBuf::from("file[1].bin")], true).unwrap();
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    /// A normal glob such as `*.bin` works with the
    /// literal-precedence fix.
    #[test]
    fn rm_glob_still_works() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();
        add(
            &repo,
            &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            &NoopProgress,
        )
        .unwrap();

        let outcome = rm(&repo, &[PathBuf::from("*.bin")], true).unwrap();
        assert_eq!(outcome.paths.len(), 2);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    /// Missing-on-disk tracked paths remain selectable by `rm`
    /// (desired state authoritative, not filesystem).
    #[test]
    fn rm_missing_on_disk_tracked_path_selectable() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        // Remove the file but it's still tracked
        std::fs::remove_file(tmp.path().join("a.bin")).unwrap();

        let outcome = rm(&repo, &[PathBuf::from("a.bin")], true).unwrap();
        assert_eq!(outcome.paths.len(), 1);
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    fn rm_with_threads(
        repo: &Repo,
        paths: &[PathBuf],
        cached: bool,
        threads: usize,
    ) -> Result<RemoveOutcome> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| rm(repo, paths, cached))
    }

    /// `resolve_deletion_paths`'s per-row `confine_mutation` runs
    /// across rayon's pool for a large directory removal. Single-threaded
    /// and default-parallelism runs must produce identical output/order
    /// and identical final `gat.lock`/worktree state.
    #[test]
    fn rm_large_directory_matches_single_threaded_and_default_pools() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("bulk")).unwrap();
        let files: Vec<PathBuf> = (0..200)
            .map(|i| PathBuf::from(format!("bulk/file-{i:04}.bin")))
            .collect();
        for file in &files {
            std::fs::write(tmp.path().join(file), file.to_string_lossy().as_bytes()).unwrap();
        }
        add(&repo, &files, &NoopProgress).unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .len(),
            files.len()
        );

        // Snapshot then restore the tracked/worktree state so both runs
        // start from the exact same fixture.
        let lock_before = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();

        let single = rm_with_threads(&repo, &[PathBuf::from("bulk")], false, 1).unwrap();
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!tmp.path().join("bulk").exists());

        // Restore the fixture for the default-parallelism run.
        std::fs::create_dir_all(tmp.path().join("bulk")).unwrap();
        for file in &files {
            std::fs::write(tmp.path().join(file), file.to_string_lossy().as_bytes()).unwrap();
        }
        repo.save_lock(&lock_before).unwrap();

        let default = rm_with_threads(
            &repo,
            &[PathBuf::from("bulk")],
            false,
            rayon::current_num_threads(),
        )
        .unwrap();
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!tmp.path().join("bulk").exists());

        assert_eq!(single.paths.len(), files.len());
        assert_eq!(single, default);
    }

    /// When several selected rows fail the symlink-ancestor
    /// confinement check, the reported failure must be the same one a
    /// serial, in-order scan would report -- not whichever path a
    /// parallel worker happened to check first -- under both a
    /// single-threaded and the default rayon pool.
    #[test]
    #[cfg(unix)]
    fn rm_reports_the_first_confinement_failure_in_path_order_regardless_of_thread_count() {
        use std::os::unix::fs::symlink;

        fn run_with_threads(threads: usize) -> String {
            let tmp = test_repo();
            let repo = Repo::at(tmp.path().to_path_buf());
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), tmp.path().join("bad-a")).unwrap();
            symlink(outside.path(), tmp.path().join("bad-b")).unwrap();
            symlink(outside.path(), tmp.path().join("bad-c")).unwrap();

            let mut lock = Lock::default();
            lock.upsert_many([
                gat_core::lock::Entry {
                    path: gp("bad-a/file.bin"),
                    oid: gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
                },
                gat_core::lock::Entry {
                    path: gp("bad-b/file.bin"),
                    oid: gat_core::oid::Oid::from_hex(&"b".repeat(64)).unwrap(),
                },
                gat_core::lock::Entry {
                    path: gp("bad-c/file.bin"),
                    oid: gat_core::oid::Oid::from_hex(&"c".repeat(64)).unwrap(),
                },
            ]);
            repo.save_lock(&lock).unwrap();

            let paths = [
                PathBuf::from("bad-c/file.bin"),
                PathBuf::from("bad-a/file.bin"),
                PathBuf::from("bad-b/file.bin"),
            ];
            let err = rm_with_threads(&repo, &paths, false, threads).unwrap_err();
            assert_eq!(
                gat_io::LockStore::load_repository(&layout(tmp.path()))
                    .unwrap()
                    .entries
                    .len(),
                3,
                "lock rows must remain untouched since the removal failed"
            );
            // The message embeds this run's own tempdir path (`outside`)
            // inside the symlinked-ancestor clause, so only the leading,
            // thread-count-independent part identifying which selector
            // failed first is meaningful to compare across runs.
            let message = err.to_string();
            message
                .split("traverses symlinked ancestor")
                .next()
                .unwrap()
                .to_string()
        }

        let single = run_with_threads(1);
        let default = run_with_threads(rayon::current_num_threads());
        assert!(
            single.contains("bad-c"),
            "expected the first-in-input-order path (`bad-c`), got: {single}"
        );
        assert_eq!(
            single, default,
            "the reported failure must not depend on thread count"
        );
    }

    /// Regression test for the literal-vs-glob classification/mutation
    /// generation race: `rm` must acquire the repo-wide `RepoLock` *before*
    /// refreshing the desired-state mirror it classifies selectors
    /// against, and hold it through shape decision, mutation, and publish,
    /// so a concurrent writer can never publish a newer generation between
    /// classification and mutation. Proven the same way
    /// `desired_index::refresh`'s own analogous test does: a "holder"
    /// thread takes the identical `RepoLock` first and parks on it, while
    /// `rm`'s call (on another thread) is proven -- via the acquire-attempt
    /// hook, not a timing guess -- to have reached `RepoLock::acquire`'s
    /// OS-lock boundary and blocked there before the holder releases it.
    #[test]
    fn rm_acquires_the_repo_lock_before_refreshing_desired_state_for_classification() {
        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"hello").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();

        let (holder_acquired_tx, holder_acquired_rx) = std::sync::mpsc::channel::<()>();
        let (release_holder_tx, release_holder_rx) = std::sync::mpsc::channel::<()>();
        let (rm_done_tx, rm_done_rx) = std::sync::mpsc::channel::<()>();
        let (acquire_attempted_tx, acquire_attempted_rx) = std::sync::mpsc::channel::<()>();

        let repo_ref = &repo;
        let root = tmp.path();
        std::thread::scope(|s| {
            // Spawned (and thus assigned a `ThreadId`) before the holder,
            // so the acquire-attempt hook below is guaranteed to be
            // installed before the holder can even signal it has the
            // lock -- the `rm` thread itself stays parked on
            // `holder_acquired_rx` until then.
            let remover = s.spawn(move || {
                holder_acquired_rx.recv().unwrap();
                rm(repo_ref, &[PathBuf::from("a.bin")], false).unwrap();
                rm_done_tx.send(()).unwrap();
            });

            gat_io::atomic_test_support::with_acquire_attempt_hook(
                remover.thread().id(),
                acquire_attempted_tx,
                || {
                    let holder = s.spawn(move || {
                        let layout = gat_io::RepositoryLayout::at(root.to_path_buf());
                        let guard = gat_io::RepoLock::acquire_repository(&layout).unwrap();
                        holder_acquired_tx.send(()).unwrap();
                        release_holder_rx.recv().unwrap();
                        drop(guard);
                    });

                    acquire_attempted_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect(
                            "rm() must reach RepoLock::acquire's OS-lock boundary \
                             while the holder still holds the lock",
                        );

                    assert!(
                        rm_done_rx.try_recv().is_err(),
                        "rm() must still be blocked on the held RepoLock, proving \
                         classification/refresh happens no earlier than lock acquisition"
                    );

                    release_holder_tx.send(()).unwrap();
                    rm_done_rx.recv().unwrap();
                    holder.join().unwrap();
                },
            );

            remover.join().unwrap();
        });

        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    /// The sparse, touched-shard-scoped removal path (a non-root, non-glob
    /// selector against a flat `gat.lock`) must resolve its whole
    /// read/planning phase -- classification, desired-state scan, deletion-path
    /// confinement -- under one `ResolvingSelection` task, sequential with and
    /// separate from the final `ApplyingChanges` task, never nested and never
    /// concurrent with it.
    #[test]
    fn rm_scoped_never_has_more_than_one_active_progress_task() {
        use crate::common::RecordingProgress;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        let progress = RecordingProgress::new();
        rm_with_progress(&repo, &[PathBuf::from("big.bin")], false, &progress).unwrap();

        assert_eq!(
            progress.max_active_tasks(),
            1,
            "the sparse rm pipeline must never have more than one progress task active at once"
        );
    }

    /// Root removal must keep selection and publication progress sequential,
    /// even though its selection now streams through the incremental path.
    #[test]
    fn rm_root_selection_never_has_more_than_one_active_progress_task() {
        use crate::common::RecordingProgress;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();
        add(
            &repo,
            &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            &NoopProgress,
        )
        .unwrap();

        let progress = RecordingProgress::new();
        rm_with_progress(&repo, &[PathBuf::from(".")], true, &progress).unwrap();

        assert_eq!(
            progress.max_active_tasks(),
            1,
            "root rm must never have more than one progress task active at once"
        );
    }

    /// An error surfaced from
    /// inside `ResolvingSelection` (here, a lock row pointing through a
    /// symlinked ancestor) must never leave more than one task active, and
    /// `ApplyingChanges` must never have begun at all -- exercising the
    /// error path, not only the success path already covered above.
    #[test]
    #[cfg(unix)]
    fn rm_error_path_never_has_more_than_one_active_progress_task() {
        use crate::common::RecordingProgress;
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("victim.bin");
        std::fs::write(&outside_file, b"outside").unwrap();
        symlink(outside.path(), tmp.path().join("link")).unwrap();

        let mut lock = Lock::default();
        lock.upsert(
            gp("link/victim.bin"),
            gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();

        let progress = RecordingProgress::new();
        rm_with_progress(&repo, &[PathBuf::from("link/victim.bin")], false, &progress).unwrap_err();

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
