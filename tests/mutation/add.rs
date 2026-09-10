use gat_core::lexical_path::GatPath;
use gat_core::lock::Entry;
use gat_core::progress::ProgressReporter;
use gat_engine::{AddCandidate, MaterializedEntry, Repository as Repo};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

type AddError = gat_command::AddError;
type AddedEntries = Vec<MaterializedEntry>;
type Result<T> = std::result::Result<T, AddError>;

fn paths_and_oids(lock: &gat_core::lock::Lock) -> Vec<(String, String)> {
    lock.entries
        .iter()
        .map(|e| (e.path.to_string(), e.oid.to_string()))
        .collect()
}

fn cache_path(repo: &Repo) -> PathBuf {
    cache_root(repo).display_path().to_path_buf()
}

fn cache_root(repo: &Repo) -> gat_io::CacheRoot {
    gat_engine::test_support::cache_root(repo)
}

fn add(
    repo: &Repo,
    paths: &[PathBuf],
    progress: &dyn ProgressReporter,
) -> Result<gat_command::AddOutcome> {
    let paths = paths
        .iter()
        .map(gat_core::path_scope::normalize_path_scope)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    gat_command::add(
        repo,
        gat_command::AddRequest {
            paths,
            force: false,
        },
        progress,
    )
}

fn add_with_options(
    repo: &Repo,
    paths: &[PathBuf],
    force: bool,
    progress: &dyn ProgressReporter,
    _lifecycle: &gat::lifecycle::Lifecycle,
) -> Result<gat_command::AddOutcome> {
    let paths = paths
        .iter()
        .map(gat_core::path_scope::normalize_path_scope)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    gat_command::add(repo, gat_command::AddRequest { paths, force }, progress)
}

mod tests {
    use super::*;
    use crate::common::RecordingProgress;
    use crate::matching_lock_shape;
    use gat_core::lock::Lock;
    use gat_core::progress::{NoopProgress, ProgressActivity};
    use gat_engine::Repository as Repo;

    use gat::lifecycle::Lifecycle;
    use gat_engine::test_support::LARGE_FILE_PROGRESS_THRESHOLD;
    use test_support::git_repo_with_initial_commit as test_repo;
    use test_support_git::{commit_all, run_git as git};

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path).unwrap()
    }

    fn oid(hex: &str) -> gat_core::oid::Oid {
        gat_core::oid::Oid::from_hex(hex).unwrap()
    }

    fn layout(root: &Path) -> gat_io::RepositoryLayout {
        gat_io::RepositoryLayout::at(root.to_path_buf())
    }

    fn refreshed_store(repo: &Repo, root: &Path) -> gat_io::StateStore {
        let mut store = gat_io::StateStore::open(&layout(root)).unwrap();
        gat_engine::test_support::refresh_desired_index(repo, &mut store).unwrap();
        store
    }

    fn staged_lock(root: &Path) -> Lock {
        gat_io::LockSnapshot::staged(&layout(root))
            .expect("staged lock snapshot")
            .to_lock()
            .expect("valid staged lock")
    }

    fn error_chain_text(error: &(dyn std::error::Error + 'static)) -> String {
        let mut messages = vec![error.to_string()];
        let mut current = error.source();
        while let Some(source) = current {
            messages.push(source.to_string());
            current = source.source();
        }
        messages.join(": ")
    }

    fn partition_with_threads(
        repo: &Repo,
        files: &[String],
        threads: usize,
    ) -> Result<(AddedEntries, Vec<GatPath>)> {
        let candidates: Vec<AddCandidate> = files
            .iter()
            .map(|p| AddCandidate {
                path: gp(p),
                desired_oid: None,
            })
            .collect();
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| {
                // The refreshed store now backs *both* halves of the reuse
                // check (desired via `desired_rows_for_paths`, materialized via
                // `materialized_rows_for`), so seed its desired side from the
                // on-disk lock exactly as the production path does.
                let cfg = repo.load_config()?;
                let desired = repo.desired_state(&cfg).map_err(Box::new)?;
                desired
                    .partition_reusable(candidates)
                    .map_err(AddError::from)
            })
    }

    fn operation_index(
        ops: &[gat_core::progress::ProgressOperation],
        needle: gat_core::progress::ProgressOperation,
    ) -> usize {
        ops.iter().position(|op| *op == needle).unwrap()
    }

    #[test]
    fn add_reports_loading_and_preparation_before_hashing() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        let progress = RecordingProgress::new();

        add(&repo, &[PathBuf::from("a.bin")], &progress).unwrap();

        let ops = progress.operations();
        // Materialized-state open and desired-state refresh are both
        // sub-phases of one loading phase, distinguished only by
        // typed activity changes on a single LoadingState task, never
        // by separate task lifecycles.
        let loading_state_count = ops
            .iter()
            .filter(|op| **op == gat_core::progress::ProgressOperation::LoadingState)
            .count();
        assert_eq!(
            loading_state_count, 1,
            "materialized-state and desired-state loading must share one LoadingState task"
        );
        let loading_task = progress.only(gat_core::progress::ProgressOperation::LoadingState);
        assert_eq!(
            loading_task.activities,
            vec![
                ProgressActivity::OpeningMaterializedState,
                ProgressActivity::RefreshingDesiredState,
            ]
        );
        assert!(ops.contains(&gat_core::progress::ProgressOperation::DiscoveringFiles));
        assert!(ops.contains(&gat_core::progress::ProgressOperation::Hashing));
        assert!(
            operation_index(
                &ops,
                gat_core::progress::ProgressOperation::DiscoveringFiles
            ) < operation_index(&ops, gat_core::progress::ProgressOperation::Hashing)
        );
        // Publication (writing the touched shard/lock) is its own final
        // sequential phase, reported after Hashing.
        assert!(ops.contains(&gat_core::progress::ProgressOperation::ApplyingChanges));
        assert!(
            operation_index(&ops, gat_core::progress::ProgressOperation::Hashing)
                < operation_index(&ops, gat_core::progress::ProgressOperation::ApplyingChanges)
        );
        assert_eq!(progress.max_active_tasks(), 1);
    }

    /// A file above
    /// `LARGE_FILE_PROGRESS_THRESHOLD` gets coarse whole-percent activity
    /// updates during ingest instead of looking frozen -- but those updates
    /// must never advance the logical `Hashing` task's position;
    /// the position advances exactly once, after the file is fully
    /// ingested, exactly like any other file.
    #[test]
    fn large_file_percent_activity_updates_never_move_the_hashing_position() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let big = vec![
            0u8;
            usize::try_from(LARGE_FILE_PROGRESS_THRESHOLD + 1)
                .expect("progress threshold must fit in usize")
        ];
        std::fs::write(tmp.path().join("big.bin"), &big).unwrap();
        let progress = RecordingProgress::new();

        add(&repo, &[PathBuf::from("big.bin")], &progress).unwrap();

        let task = progress.only(gat_core::progress::ProgressOperation::Hashing);
        assert_eq!(
            task.position, 1,
            "the position must advance exactly once for the one large file ingested, \
             regardless of how many percent-activity messages were emitted along the way"
        );
        assert_eq!(task.total, None);
        assert!(
            task.activities.iter().any(|activity| matches!(
                activity,
                ProgressActivity::HashingFile {
                    path,
                    percent: Some(_)
                } if path == "big.bin"
            )),
            "a file above the threshold must report coarse percent activity: {:?}",
            task.activities
        );
    }

    /// Desired-state publication,
    /// materialized-state recording, and excludes regeneration are all
    /// part of the same final "applying changes" phase, so the
    /// `ApplyingChanges` task's activities must cover all three
    /// sub-steps in order -- none of them may run silently after the task
    /// has already finished.
    #[test]
    fn add_applying_changes_task_covers_publish_materialize_and_excludes() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        let progress = RecordingProgress::new();

        add(&repo, &[PathBuf::from("a.bin")], &progress).unwrap();

        let task = progress.only(gat_core::progress::ProgressOperation::ApplyingChanges);
        assert!(task.finished);
        assert_eq!(
            task.activities,
            vec![
                ProgressActivity::PublishingDesiredState,
                ProgressActivity::RecordingMaterializedState,
                ProgressActivity::RegeneratingExcludes,
            ],
            "all three publication sub-steps must be reported as activity on the one \
             ApplyingChanges task, in order, before it finishes"
        );
    }

    /// All selectors contribute to one cumulative hashing task, with at most
    /// one progress task active even when discovery resumes between selectors.
    #[test]
    fn add_mixing_a_directory_and_a_file_never_has_more_than_one_active_progress_task() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(tmp.path().join("dir/nested.bin"), b"payload-1").unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"payload-2").unwrap();
        let progress = RecordingProgress::new();

        add(
            &repo,
            &[PathBuf::from("dir"), PathBuf::from("a.bin")],
            &progress,
        )
        .unwrap();

        assert_eq!(progress.max_active_tasks(), 1);
        let task = progress.only(gat_core::progress::ProgressOperation::Hashing);
        assert_eq!(task.position, 2);
        assert_eq!(task.total, None);
    }

    #[test]
    fn add_hashing_progress_is_cumulative_across_windows_and_selectors() {
        for explicit in [false, true] {
            let tmp = test_repo();
            let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            std::fs::create_dir(tmp.path().join("data")).unwrap();
            let paths: Vec<_> = (0..7)
                .map(|index| {
                    let path = format!("data/{index}.bin");
                    std::fs::write(tmp.path().join(&path), path.as_bytes()).unwrap();
                    gat_core::path_scope::normalize_path_scope(path).unwrap()
                })
                .collect();
            let request = gat_command::AddRequest {
                paths: if explicit {
                    paths
                } else {
                    vec![gat_core::path_scope::normalize_path_scope("data").unwrap()]
                },
                force: false,
            };
            let progress = RecordingProgress::new();
            let (outcome, high_water) = gat_command::add_with_window_for_test(
                &repo,
                request,
                std::num::NonZeroUsize::new(2).unwrap(),
                &progress,
            )
            .unwrap();
            assert_eq!(outcome.added_count, 7);
            assert_eq!(high_water, 2);
            let hashing = progress.only(gat_core::progress::ProgressOperation::Hashing);
            assert_eq!(hashing.position, 7);
            assert_eq!(hashing.total, None);
            assert!(hashing.finished);
            assert_eq!(progress.max_active_tasks(), 1);
            assert!(progress.tasks().iter().all(|task| task.finished));
        }
    }

    /// A bare glob argument
    /// (`gat add 'dir/*.bin'`) discovers via `add_glob`'s gix-native
    /// pipeline, no different in kind from `add_dir`'s directory
    /// walk -- it must be covered by its own `DiscoveringFiles` task
    /// rather than running silently, and must never overlap with the
    /// subsequent `Hashing` task.
    #[test]
    fn add_bare_glob_is_covered_by_a_discovering_files_task() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(tmp.path().join("dir/one.bin"), b"payload-1").unwrap();
        std::fs::write(tmp.path().join("dir/two.bin"), b"payload-2").unwrap();
        let progress = RecordingProgress::new();

        add(&repo, &[PathBuf::from("dir/*.bin")], &progress).unwrap();

        let ops = progress.operations();
        assert!(
            ops.contains(&gat_core::progress::ProgressOperation::DiscoveringFiles),
            "glob expansion must be reported as a DiscoveringFiles task"
        );
        assert!(ops.contains(&gat_core::progress::ProgressOperation::Hashing));
        assert!(
            operation_index(
                &ops,
                gat_core::progress::ProgressOperation::DiscoveringFiles
            ) < operation_index(&ops, gat_core::progress::ProgressOperation::Hashing),
            "glob discovery must finish before hashing begins"
        );
        assert_eq!(
            progress.max_active_tasks(),
            1,
            "glob discovery and hashing must never be simultaneously active"
        );
    }

    /// A pending ordinary file argument
    /// followed by a bare glob argument that fails to expand (malformed
    /// pattern syntax, an `Err` from `GatGlobPattern::parse` itself rather
    /// than merely zero matches) must not leave the glob's
    /// `DiscoveringFiles` task overlapping the pending-file flush's own
    /// `DiscoveringFiles`/`Hashing` task -- `discover.finish()` must run
    /// before `flush_before_error`'s error-path flush ever begins a
    /// second task.
    #[test]
    fn add_pending_file_then_failing_glob_never_overlaps_progress_tasks() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"payload").unwrap();
        let progress = RecordingProgress::new();

        let err = add(
            &repo,
            &[
                PathBuf::from("a.bin"),
                // An unmatched `[` is invalid glob pattern syntax (a
                // `glob::PatternError`), not merely a pattern with zero
                // matches -- this exercises `GatGlobPattern::parse`'s `Err`
                // path itself.
                PathBuf::from("dir/[unclosed"),
            ],
            &progress,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").to_lowercase().contains("pattern")
                || format!("{err:#}").to_lowercase().contains("glob"),
            "expected a glob-pattern error, got: {err:#}"
        );
        assert_eq!(
            progress.max_active_tasks(),
            1,
            "the failing glob's DiscoveringFiles task and the pending file's \
             own flush-triggered task must never be simultaneously active"
        );
    }

    #[test]
    fn add_file_then_status_shows_cached() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "big.bin");
        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("big.bin"));
        commit_all(tmp.path(), "add big.bin");

        let files: Vec<String> = staged_lock(tmp.path())
            .entries
            .into_iter()
            .map(|e| e.path.to_string())
            .collect();
        assert_eq!(files, vec!["big.bin".to_string()]);
    }

    /// `add` must still succeed and correctly record the entry even when
    /// the shared proof DB opens fine but then fails mid-operation (as
    /// opposed to never opening at all): `cache.sqlite3` is a disposable
    /// accelerator, so a raw `SQLite` failure persisting a freshly-ingested
    /// object's proof must only cost a later re-verification hash, never
    /// fail an otherwise-successful `add`.
    #[test]
    fn add_succeeds_when_the_proof_db_fails_after_opening_successfully() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();

        cache_root(&repo).break_database_for_test();

        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "big.bin");
    }

    /// `gat add` must record a materialized-state stat proof (via
    /// `sync::record_materialized_with_stats`) that a subsequent
    /// independent stat of the just-added file still matches -- otherwise
    /// the next `gat sync` would have to re-hash every file `gat add` just
    /// hashed instead of trusting the stamp `gat add` itself established.
    /// The proof describes exactly the bytes just ingested, so it is
    /// always recorded and must be reusable by a later stat-only match.
    #[test]
    fn add_records_a_materialized_stat_a_fresh_stat_still_proves_unchanged() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let path = tmp.path().join("big.bin");
        std::fs::write(&path, b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        let rows = gat_io::StateStore::open(&layout(tmp.path()))
            .unwrap()
            .load_all_raw()
            .unwrap();
        assert_eq!(rows.len(), 1);

        assert!(
            gat_io::state_materialized_row_proof_matches_path_for_test(&rows[0], &path),
            "the stat gat add recorded must be reusable by a later stat-only match"
        );
    }

    /// Publication boundary ("seed local desired identity
    /// immediately after successful publication"): once `add` writes a
    /// (flat, unsharded) `gat.lock`, the `SQLite` desired mirror must
    /// already reflect it -- a fresh `StateStore` opened right
    /// after, with no intervening `desired_index::refresh()`, must read
    /// back exactly the entries `add` just published. Before this, the
    /// mirror stayed stale until whatever command ran next paid to
    /// reparse/rehash the file `add` had just written.
    #[test]
    fn add_seeds_the_desired_mirror_without_a_separate_refresh() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        let mirrored = gat_io::StateStore::open(&layout(tmp.path()))
            .unwrap()
            .load_desired_as_lock()
            .unwrap();
        let on_disk = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        // The desired mirror is OID-only: compare path/oid, not
        // `size` (a development-only `gat.lock` field the
        // mirror never persists -- see `MaterializedRow::into_entry`).
        assert_eq!(paths_and_oids(&mirrored), paths_and_oids(&on_disk));
        assert_eq!(mirrored.entries.len(), 1);
        assert_eq!(mirrored.entries[0].path, "big.bin");
    }

    /// Stat-first identity: re-`add`ing an already-tracked,
    /// byte-for-byte unchanged file must reuse its recorded OID without
    /// reading the file's bytes at all. The initial `add`'s own coherent
    /// observation already records a reusable proof describing exactly
    /// the ingested bytes, so a zero-cost stat-only reuse is available
    /// immediately -- no separate settling call is needed. This is
    /// characterized by tightening what the working tree/cache allow:
    /// making the object cache directory read-only means any actual
    /// ingest (which creates a `NamedTempFile` there) fails outright,
    /// and additionally making the source file itself unreadable means
    /// even a direct verifying hash (`storage::hash_file`) fails --
    /// so a second `add` succeeding despite that proves it touched
    /// neither the cache nor the file's content at all.
    #[test]
    #[cfg(unix)]
    fn add_repeated_on_an_unchanged_file_never_rehashes_it() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let file = tmp.path().join("big.bin");
        std::fs::write(&file, b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let original_oid = lock.entries[0].oid;

        let objects_dir = cache_path(&repo);
        let original_objects_mode = std::fs::metadata(&objects_dir).unwrap().permissions();
        std::fs::set_permissions(
            &objects_dir,
            std::fs::Permissions::from_mode(0o555), // read + execute, no write
        )
        .unwrap();

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = add(&repo, &[PathBuf::from("big.bin")], &NoopProgress);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&objects_dir, original_objects_mode).unwrap();

        result.unwrap_or_else(|e| panic!("second add must not need to read the file's bytes: {e}"));
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].oid, original_oid);
    }

    /// Quantitative counterpart to
    /// `add_repeated_on_an_unchanged_file_never_rehashes_it`: once a
    /// path's stamp is recorded and confirmed unchanged, a further `add`
    /// of that same unchanged path must call [`storage::hash_file`]
    /// exactly zero times: a clean repeated `gat add` must hash zero
    /// bytes.
    #[test]
    fn add_repeated_on_an_unchanged_file_calls_hash_file_zero_times() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let file = tmp.path().join("big.bin");
        std::fs::write(&file, b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        // First re-`add` establishes the reusable proof from the initial
        // ingest's own coherent observation, so it already needs no
        // further hash; this second call simply confirms the repo is now
        // in the steady state the assertion below measures.
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        gat_io::with_exclusive_hash_file_call_count(|| {
            add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
            assert_eq!(gat_io::hash_file_call_count(), 0);
        });
    }

    #[test]
    fn partition_reusable_large_unchanged_set_matches_single_threaded_and_default_pools() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let files: Vec<String> = (0..64).map(|i| format!("bulk/file-{i}.bin")).collect();
        std::fs::create_dir_all(tmp.path().join("bulk")).unwrap();
        for file in &files {
            std::fs::write(tmp.path().join(file), file.as_bytes()).unwrap();
        }
        let args: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        add(&repo, &args, &NoopProgress).unwrap();

        let default = partition_with_threads(&repo, &files, rayon::current_num_threads()).unwrap();
        let single = partition_with_threads(&repo, &files, 1).unwrap();

        assert_eq!(default, single);
        assert_eq!(default.0.len(), files.len());
        assert!(default.1.is_empty());
    }

    #[test]
    fn partition_reusable_mixed_classification_matches_single_threaded_and_default_pools() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let tracked = ["stat.bin", "verify.bin", "changed.bin"];
        for file in tracked {
            std::fs::write(tmp.path().join(file), format!("payload:{file}")).unwrap();
        }
        let tracked_args: Vec<PathBuf> = tracked.iter().map(PathBuf::from).collect();
        add(&repo, &tracked_args, &NoopProgress).unwrap();

        std::fs::write(tmp.path().join("verify.bin"), b"payload:verify.bin").unwrap();
        std::fs::write(tmp.path().join("changed.bin"), b"new payload").unwrap();
        std::fs::write(tmp.path().join("new.bin"), b"brand new").unwrap();
        let files = vec![
            "changed.bin".to_string(),
            "stat.bin".to_string(),
            "verify.bin".to_string(),
            "new.bin".to_string(),
        ];

        let default = partition_with_threads(&repo, &files, rayon::current_num_threads()).unwrap();
        let single = partition_with_threads(&repo, &files, 1).unwrap();

        assert_eq!(default, single);
        assert_eq!(
            default
                .0
                .iter()
                .map(|entry| entry.entry().path.as_str())
                .collect::<Vec<_>>(),
            vec!["stat.bin", "verify.bin"]
        );
        assert_eq!(
            default.1,
            vec!["changed.bin".to_string(), "new.bin".to_string()]
        );
    }

    #[test]
    fn partition_reusable_reports_the_first_error_in_input_order() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let tmp = test_repo();
            let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), tmp.path().join("bad-a")).unwrap();
            symlink(outside.path(), tmp.path().join("bad-b")).unwrap();

            let entries = vec![
                Entry {
                    path: gp("bad-a/file.bin"),
                    oid: oid(&"a".repeat(64)),
                },
                Entry {
                    path: gp("bad-b/file.bin"),
                    oid: oid(&"b".repeat(64)),
                },
            ];
            let mut store = gat_io::StateStore::open(&layout(tmp.path())).unwrap();
            store.upsert_many(&entries).unwrap();
            gat_io::LockStore::publish_repository(
                &layout(tmp.path()),
                &Lock {
                    entries: entries.clone(),
                },
                gat_core::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            let files = ["bad-b/file.bin".to_string(), "bad-a/file.bin".to_string()];
            let candidates: Vec<AddCandidate> = files
                .iter()
                .map(|p| AddCandidate {
                    path: gp(p),
                    desired_oid: None,
                })
                .collect();
            let cfg = repo.load_config().unwrap();
            let desired = repo.desired_state(&cfg).unwrap();
            let err = desired.partition_reusable(candidates).unwrap_err();
            let message = error_chain_text(&err);
            assert!(
                message.contains("bad-b"),
                "expected the first failing path in input order, got: {message}"
            );
        }
    }

    /// The same stat-first reuse must also cover a directory `add` that
    /// walks over files already tracked, unchanged, alongside genuinely
    /// new ones -- only the new file should require a writable object
    /// cache.
    #[test]
    fn add_dir_reuses_unchanged_files_and_only_hashes_new_ones() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("dir")).unwrap();
        std::fs::write(tmp.path().join("dir/old.bin"), b"old payload").unwrap();
        add(&repo, &[PathBuf::from("dir")], &NoopProgress).unwrap();

        std::fs::write(tmp.path().join("dir/new.bin"), b"new payload").unwrap();
        let new_oid = blake3::hash(b"new payload").to_hex().to_string();
        add(&repo, &[PathBuf::from("dir")], &NoopProgress).unwrap();

        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 2);
        let new_entry = lock
            .entries
            .iter()
            .find(|e| e.path == "dir/new.bin")
            .unwrap();
        assert_eq!(new_entry.oid, oid(&new_oid));
    }

    /// A command's returned `AddOutcome` must be identical regardless of
    /// which `ProgressReporter` runs alongside it -- progress is a
    /// side-channel to stderr, never something that can change the
    /// durable result (requirement: outcomes are the same regardless of
    /// progress implementation).
    #[test]
    fn add_outcome_is_identical_regardless_of_progress_reporter() {
        use crate::common::RecordingProgress;

        let tmp_a = test_repo();
        let repo_a = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp_a.path().to_path_buf());
        std::fs::write(tmp_a.path().join("big.bin"), b"payload").unwrap();
        let outcome_a = add(&repo_a, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        let tmp_b = test_repo();
        let repo_b = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp_b.path().to_path_buf());
        std::fs::write(tmp_b.path().join("big.bin"), b"payload").unwrap();
        let outcome_b = add(
            &repo_b,
            &[PathBuf::from("big.bin")],
            &RecordingProgress::new(),
        )
        .unwrap();

        assert_eq!(outcome_a, outcome_b);
    }

    #[test]
    fn add_directory_writes_one_entry_per_file() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/b.bin"), b"b").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        commit_all(tmp.path(), "add data");

        let lock = staged_lock(tmp.path());
        assert_eq!(lock.entries.len(), 2);
        let mut paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["data/a.bin", "data/b.bin"]);
    }

    #[test]
    fn add_normalizes_a_dot_slash_and_trailing_slash_directory_argument() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        add(&repo, &[PathBuf::from("./data/")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "data/a.bin");
    }

    #[test]
    fn add_glob_pattern_expands_and_tracks_matches() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.psd"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.psd"), b"b").unwrap();
        add(&repo, &[PathBuf::from("*.psd")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let mut paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["a.psd", "b.psd"]);
    }

    #[test]
    fn add_glob_tracks_files_without_creating_entries_for_matching_directories() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir(tmp.path().join("directory.psd")).unwrap();
        std::fs::write(tmp.path().join("file.psd"), b"file").unwrap();

        add(&repo, &[PathBuf::from("*.psd")], &NoopProgress).unwrap();

        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "file.psd");
    }

    /// Glob-matched new files must respect Git ignores exactly like a
    /// directory add -- semantic parity with directory discovery
    /// (`add_dir_gitignore_now_governs_new_file_discovery`): a Git-ignored
    /// new file is never even discovered, so it's silently skipped rather
    /// than raising a hard "ignored by git" error. When it's the glob's
    /// only possible match, that leaves zero candidates, which surfaces
    /// as the ordinary "matches no files" error -- not a git-ignore-
    /// specific one (superseded below by typed exclusion reporting).
    #[test]
    fn add_glob_explains_a_new_gitignored_match_without_force() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "*.psd\n").unwrap();
        std::fs::write(tmp.path().join("a.psd"), b"a").unwrap();
        let outcome = add(&repo, &[PathBuf::from("*.psd")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 0);
        assert_eq!(
            outcome.exclusions[0].reason,
            gat_engine::AddExclusionReason::GitIgnore
        );
        assert_eq!(outcome.exclusions[0].files, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert!(lock.entries.is_empty());
    }

    /// `--force` disables ignore filtering *within* the glob's matched
    /// scope, so a Git-ignored new file the glob matches is now
    /// included -- parity with `add_force_bypasses_gitignore_refusal_for_an_explicit_path`
    /// and directory-add force behavior.
    #[test]
    fn add_glob_force_includes_a_new_gitignored_match() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "*.psd\n").unwrap();
        std::fs::write(tmp.path().join("a.psd"), b"a").unwrap();
        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("*.psd")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "a.psd");
    }

    /// A glob that matches an already Gat-tracked path must keep matching
    /// it even once gat's own managed `.git/info/exclude` block makes it
    /// Git-ignored -- glob parity with
    /// `add_reuses_an_existing_gat_tracked_explicit_path_despite_gats_own_managed_exclude_block`.
    #[test]
    fn add_glob_reuses_an_existing_gat_tracked_match_despite_gats_own_managed_exclude_block() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.psd"), b"a").unwrap();
        add(&repo, &[PathBuf::from("*.psd")], &NoopProgress).unwrap();

        let outcome = add(&repo, &[PathBuf::from("*.psd")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 1);
    }

    #[test]
    fn add_glob_is_non_recursive_unless_double_star_is_used() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/deep")).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/deep/a.bin"), b"a").unwrap();

        add(&repo, &[PathBuf::from("*.bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(
            lock.entries
                .into_iter()
                .map(|entry| entry.path)
                .collect::<Vec<_>>(),
            vec!["a.bin".to_string()]
        );

        // Resetting `gat.lock` directly (bypassing `rm`) leaves gat's own
        // managed `.git/info/exclude` block stale, which the full-stack
        // `is_excluded()` check would otherwise treat as a
        // hard git-ignore with no desired-state exemption to bypass it --
        // regenerate excludes from the same empty `Lock` to keep this an
        // ordinary, non-ignored path like a real `gat rm` would.
        let empty = Lock::default();
        gat_io::LockStore::publish_repository(
            &layout(tmp.path()),
            &empty,
            gat_core::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        gat_engine::test_support::sync_excludes_from_lock_for_test(&repo, &empty, false).unwrap();
        add(&repo, &[PathBuf::from("**/*.bin")], &NoopProgress).unwrap();
        let mut paths: Vec<String> = gat_io::LockStore::load_repository(
            &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
        )
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| entry.path.to_string())
        .collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["a.bin", "data/a.bin", "data/deep/a.bin"]);
    }

    #[test]
    fn add_glob_normalizes_backslashes_and_mixed_separators() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/deep")).unwrap();
        std::fs::write(tmp.path().join("data/deep/a.bin"), b"a").unwrap();

        add(&repo, &[PathBuf::from(r".\data\**\*.bin")], &NoopProgress).unwrap();
        let paths: Vec<String> = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.path.to_string())
            .collect();
        assert_eq!(paths, vec!["data/deep/a.bin".to_string()]);
    }

    #[test]
    fn add_file_with_spaces_in_name() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("my file.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("my file.bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries[0].path, "my file.bin");
    }

    #[test]
    fn add_file_with_unicode_name() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("café☕.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("café☕.bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries[0].path, "café☕.bin");
    }

    #[test]
    fn add_literal_filename_containing_glob_metacharacters() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("file[1].bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("file[1].bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries[0].path, "file[1].bin");
    }

    /// A *directory* argument containing glob metacharacters (`*`, `?`,
    /// `[`, `]`) must be treated as a literal, top-rooted path by
    /// discovery -- not an implicitly-patterned gix pathspec that could
    /// (mis)interpret those characters as wildcards. A directory named
    /// `a[b]` must select only that literal directory, not also `ab`
    /// (which an unescaped `[]` pathspec would also match). This is a
    /// defense-in-depth regression test for `:(literal,top)` in
    /// `discover_add_candidates`: the current gix dirwalk already
    /// happens to scope by literal path segment rather than glob-
    /// expanding `dir`, so this guards against that changing silently
    /// (e.g. on a gix upgrade), not a currently-reproducible bug.
    #[test]
    fn add_directory_with_glob_metacharacters_in_its_name_is_treated_literally() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("a[b]")).unwrap();
        std::fs::write(tmp.path().join("a[b]/wanted.bin"), b"a").unwrap();
        // A sibling directory an unescaped-glob interpretation of `a[b]`
        // would also match, to prove only the literal directory was
        // selected.
        std::fs::create_dir_all(tmp.path().join("ab")).unwrap();
        std::fs::write(tmp.path().join("ab/unwanted.bin"), b"b").unwrap();

        add(&repo, &[PathBuf::from("a[b]")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a[b]/wanted.bin"]);
    }

    #[cfg(unix)]
    #[test]
    fn add_rejects_non_utf8_path_with_clear_error() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let path = PathBuf::from(OsString::from_vec(b"invalid-\xFF-utf8".to_vec()));
        let err = add(&repo, &[path], &NoopProgress).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("UTF-8") || msg.contains("non-UTF-8"), "{msg}");
    }

    #[test]
    fn add_rejects_absolute_path_outside_repo() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        assert!(add(&repo, &[tmp.path().join("big.bin")], &NoopProgress).is_err());
    }

    #[test]
    fn add_rejects_rooted_scope_without_adding_the_repo() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();

        assert!(add(&repo, &[PathBuf::from("/")], &NoopProgress).is_err());
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn add_rejects_rooted_and_unc_inputs_consistently() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        for input in [r"\\server\share", "//server/share"] {
            assert!(add(&repo, &[PathBuf::from(input)], &NoopProgress).is_err());
        }
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    /// Gat path identity is host-independent; a leading segment that
    /// merely looks like a Windows drive letter is an ordinary path
    /// segment, not a rejected drive-relative spelling. Whether
    /// it can be *materialized* as a real file name is a separate,
    /// host-dependent filesystem concern -- on this (Unix) test host it
    /// can, so `add` must accept and track it like any other name.
    #[cfg(unix)]
    #[test]
    fn add_accepts_a_windows_drive_like_leading_segment() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("C:foo"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("C:foo")], &NoopProgress).unwrap();
        assert_eq!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries[0]
                .path,
            "C:foo"
        );
    }

    #[test]
    fn add_rejects_path_with_parent_dir_traversal() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        assert!(add(&repo, &[PathBuf::from("../secret")], &NoopProgress).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn add_refuses_an_explicit_symlink_argument() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("target.bin"), b"payload").unwrap();
        symlink(tmp.path().join("target.bin"), tmp.path().join("link.bin")).unwrap();

        let err = add(&repo, &[PathBuf::from("link.bin")], &NoopProgress).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected a symlink rejection: {err}"
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
    fn add_glob_silently_skips_a_symlinked_match() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("target.bin"), b"payload").unwrap();
        symlink(tmp.path().join("target.bin"), tmp.path().join("link.bin")).unwrap();

        // Discovery never emits symlinks as candidates (mirroring
        // directory-add symlink handling), so `*.bin` selects only the
        // real file `target.bin`; `link.bin` is silently excluded rather
        // than raising a hard error.
        let outcome = add(&repo, &[PathBuf::from("*.bin")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "target.bin");
    }

    /// When a glob's only possible match is a symlink, discovery finds
    /// zero candidates -- surfaced as the ordinary "matches no files"
    /// error rather than a symlink-specific one, matching the
    /// gitignored-only-match case above (an *explicit* symlink argument
    /// is a different code path and still gets a symlink-specific error;
    /// see `add_refuses_an_explicit_symlink_argument`).
    #[test]
    #[cfg(unix)]
    fn add_glob_matching_only_a_symlink_reports_no_files_matched() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("target.psd"), b"payload").unwrap();
        symlink(tmp.path().join("target.psd"), tmp.path().join("link.psd")).unwrap();
        std::fs::remove_file(tmp.path().join("target.psd")).unwrap();

        let err = add(&repo, &[PathBuf::from("*.psd")], &NoopProgress).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not exist and matches no files"),
            "{err}"
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
    fn add_refuses_a_file_beneath_a_symlinked_ancestor_directory() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.bin"), b"top secret").unwrap();
        symlink(outside.path(), tmp.path().join("outside-link")).unwrap();

        let err = add(
            &repo,
            &[PathBuf::from("outside-link/secret.bin")],
            &NoopProgress,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("symlinked ancestor"),
            "expected an ancestor-symlink rejection: {msg}"
        );
        // Nothing was ingested, so the lock and object cache stay
        // untouched by this rejected add.
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!cache_path(&repo).exists());
    }

    #[test]
    fn add_missing_path_with_no_glob_match_errors() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        assert!(add(&repo, &[PathBuf::from("nope.bin")], &NoopProgress).is_err());
    }

    /// An explicitly-named path that exists on disk but is neither a
    /// regular file nor a directory (here, a FIFO) is refused with a
    /// dedicated "unsupported file type" error, not silently treated as
    /// a no-match glob (which would report the misleading "does not
    /// exist and matches no files").
    #[cfg(unix)]
    #[test]
    fn add_refuses_an_explicit_fifo_argument() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let path = tmp.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo failed for {}", path.display());

        let err = add(&repo, &[PathBuf::from("pipe")], &NoopProgress).unwrap_err();
        assert!(
            err.to_string().contains("pipe") && err.to_string().contains("not a regular file"),
            "unexpected error message: {err}"
        );
    }

    /// The central error-path invariant, checked here on `add`'s
    /// simplest failure (an unmatched literal path, no glob involved):
    /// whichever task was active when the error propagated must have
    /// been finished by the `?`-triggered `ProgressTask` drop, not left
    /// dangling -- so once `add` has returned, `RecordingProgress` must
    /// report zero currently-active tasks, regardless of which phase
    /// (`LoadingState`, `DiscoveringFiles`, hashing, `ApplyingChanges`)
    /// was open at the moment of failure.
    #[test]
    fn add_error_path_leaves_zero_active_progress_tasks() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let progress = RecordingProgress::new();
        let err = add(&repo, &[PathBuf::from("nope.bin")], &progress).unwrap_err();
        assert!(err.to_string().contains("nope.bin") || !err.to_string().is_empty());
        assert_eq!(
            progress.active_tasks(),
            0,
            "add's error path must finish every task it began, not leave one dangling"
        );
    }

    /// `add` only persists
    /// `gat.lock` once, after every named path has been hashed/ingested --
    /// so a failure partway through a multi-path `add` (here, a later
    /// path that doesn't exist) means `gat.lock` is never written at all,
    /// not even for a path that was already hashed/published first. The
    /// materialized-state mirror (only ever seeded from a successfully
    /// persisted `gat.lock`, see `add_seeds_the_desired_mirror_without_a_
    /// separate_refresh` above) records no rows either. The already-
    /// hashed content stays an orphaned, but harmless and content-
    /// addressed, cache object -- later reclaimable by `gat gc`.
    #[test]
    fn add_persists_neither_lock_nor_state_when_a_later_named_path_is_missing() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("real.bin"), b"data").unwrap();

        let err = add(
            &repo,
            &[PathBuf::from("real.bin"), PathBuf::from("missing.bin")],
            &NoopProgress,
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing.bin"), "{err}");

        assert!(!tmp.path().join("gat.lock").exists());
        assert_eq!(std::fs::read(tmp.path().join("real.bin")).unwrap(), b"data");

        let cache_objects = std::fs::read_dir(cache_path(&repo)).map_or(0, std::fs::ReadDir::count);
        assert!(
            cache_objects > 0,
            "expected an orphaned cache object for real.bin's already-hashed content"
        );

        let rows = gat_io::StateStore::open(&layout(tmp.path()))
            .unwrap()
            .load_all_raw()
            .unwrap();
        assert!(
            rows.is_empty(),
            "no state row should be recorded when add fails before persisting gat.lock"
        );
    }

    #[test]
    fn add_refuses_a_path_owned_by_a_source() {
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

        std::fs::create_dir_all(tmp.path().join("data/models")).unwrap();
        std::fs::write(tmp.path().join("data/models/a.bin"), b"a").unwrap();
        assert!(add(&repo, &[PathBuf::from("data/models/a.bin")], &NoopProgress).is_err());
        // A directory add that reaches into the mount's target must also
        // be refused, even though the argument itself ("data") doesn't
        // name the source's path directly.
        assert!(add(&repo, &[PathBuf::from("data")], &NoopProgress).is_err());
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn add_refuses_an_explicit_path_already_tracked_by_git() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("plain.txt"), b"already in git").unwrap();
        commit_all(tmp.path(), "add plain.txt");

        let err = add(&repo, &[PathBuf::from("plain.txt")], &NoopProgress).unwrap_err();
        assert!(err.to_string().contains("already tracked by git"), "{err}");
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn add_reports_a_glob_matched_path_already_tracked_by_git() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("tracked.psd"), b"a").unwrap();
        commit_all(tmp.path(), "add tracked.psd");

        let outcome = add(&repo, &[PathBuf::from("*.psd")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 0);
        assert!(outcome.exclusions.iter().any(|item| item.reason
            == gat_engine::AddExclusionReason::GitTracked
            && item.files == 1));
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn add_directory_reports_already_git_tracked_members() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/tracked.txt"), b"a").unwrap();
        commit_all(tmp.path(), "add tracked");
        std::fs::write(tmp.path().join("data/untracked.bin"), b"b").unwrap();

        let outcome = add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        assert!(outcome.exclusions.iter().any(|item| item.reason
            == gat_engine::AddExclusionReason::GitTracked
            && item.files == 1));
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "data/untracked.bin");
    }

    /// A Gat-tracked path in the desired overlay must never be
    /// reselected by a directory add once it becomes plain-Git-tracked
    /// (e.g. `git add -f`'d and committed after gat already recorded
    /// it): the desired overlay exists only to keep already-Gat-tracked
    /// paths addable despite Git ignores, never to override the plain-
    /// Git-tracked refusal itself.
    #[test]
    fn add_directory_overlay_never_reselects_a_path_that_became_git_tracked() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("shared.bin"), b"a").unwrap();
        add(&repo, &[PathBuf::from("shared.bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        // Commit `gat.lock` itself (real git-tracked infrastructure, per
        // so the second `add(".")` below sees no other new
        // candidates besides `shared.bin`.
        commit_all(tmp.path(), "commit gat.lock");

        // `git add -f` bypasses gat's own managed `.git/info/exclude`
        // block and makes the path plain-Git-tracked despite still
        // having a desired-state row.
        git(tmp.path(), &["add", "-f", "shared.bin"]);
        git(
            tmp.path(),
            &["commit", "-q", "-m", "force-track shared.bin"],
        );

        let outcome = add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();
        assert_eq!(
            outcome.added_count, 0,
            "plain-Git-tracked shared.bin must not be reselected via the desired overlay"
        );
    }

    #[test]
    fn add_refuses_an_explicit_path_matching_gatignore() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gatignore"), "*.tmp\n").unwrap();
        std::fs::write(tmp.path().join("scratch.tmp"), b"a").unwrap();

        let err = add(&repo, &[PathBuf::from("scratch.tmp")], &NoopProgress).unwrap_err();
        assert!(err.to_string().contains(".gatignore"), "{err}");
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    /// `.git`/`.gat` are unconditional infrastructure -- explicitly naming
    /// a path under either must always fail, independent of `.gatignore`,
    /// `.gitignore`, or whatever `.git/info/exclude` currently contains.
    #[test]
    fn add_refuses_an_explicit_path_under_dot_git_or_dot_gat() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let err = add(&repo, &[PathBuf::from(".git/info/exclude")], &NoopProgress).unwrap_err();
        assert!(err.to_string().contains("infrastructure"), "{err}");

        std::fs::create_dir_all(tmp.path().join(".gat")).unwrap();
        std::fs::write(tmp.path().join(".gat/marker"), b"x").unwrap();
        let err = add(&repo, &[PathBuf::from(".gat/marker")], &NoopProgress).unwrap_err();
        assert!(err.to_string().contains("infrastructure"), "{err}");
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty()
        );
    }

    /// A directory add (`gat add .`) must never surface `.git/` or `.gat/`
    /// contents as candidates, even though both hold real, non-symlink
    /// regular files (`.git/config`, `.gat/gat.yaml`).
    #[test]
    fn add_directory_never_discovers_dot_git_or_dot_gat_contents() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("keep.bin"), b"a").unwrap();

        add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();

        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.bin"]);
    }

    /// A large `.gat/objects` tree with a missing/empty
    /// `.git/info/exclude` must never be discovered by `gat add .`, with
    /// or without `--force` -- root `.git`/`.gat` are pruned structurally
    /// at the gix dirwalk boundary itself (see `PruneInfraDelegate`),
    /// not merely filtered afterward by relying on gat's generated
    /// managed exclude block, which this test deliberately leaves
    /// missing.
    #[test]
    fn add_dot_never_walks_a_large_dot_gat_objects_tree_without_managed_excludes() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("keep.bin"), b"a").unwrap();
        // No prior `add()` call has run yet, so `.git/info/exclude` has
        // never been (re)generated with gat's managed block -- git
        // itself creates it empty by default, which is exactly the
        // "missing/empty" scenario this test targets.
        std::fs::write(tmp.path().join(".git/info/exclude"), "").unwrap();
        for shard in 0..20 {
            let dir = tmp.path().join(format!(".gat/objects/{shard:02x}"));
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..100 {
                std::fs::write(dir.join(format!("obj-{i:04}")), b"payload").unwrap();
            }
        }

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from(".")],
            true, // --force must not reopen .git/.gat for traversal either
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.bin"]);
    }

    #[test]
    fn add_directory_reports_gatignored_members() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gatignore"), "scratch/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("scratch")).unwrap();
        std::fs::write(tmp.path().join("scratch/drop.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("keep.bin"), b"b").unwrap();

        add(
            &repo,
            &[PathBuf::from("scratch"), PathBuf::from("keep.bin")],
            &NoopProgress,
        )
        .unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.bin"]);
    }

    #[test]
    fn add_rejects_a_new_explicit_path_that_is_gitignored() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "big.bin\n").unwrap();
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();

        // A brand-new, Git-ignored path is an actionable error rather than
        // a silent add with an advisory hint:
        // Git ignores govern discovery of *new* Gat candidates.
        let err = add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap_err();
        assert!(err.to_string().contains("ignored by git"), "{err}");
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert!(lock.entries.is_empty());
    }

    #[test]
    fn add_force_bypasses_gitignore_refusal_for_an_explicit_path() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "big.bin\n").unwrap();
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("big.bin")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["big.bin"]);
    }

    #[test]
    fn add_force_bypasses_gatignore_refusal_for_an_explicit_path() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gatignore"), "big.bin\n").unwrap();
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("big.bin")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 1);
    }

    #[test]
    fn add_force_does_not_bypass_plain_git_tracked_refusal() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("tracked.bin"), b"payload").unwrap();
        commit_all(tmp.path(), "track file");

        let err = add_with_options(
            &repo,
            &[PathBuf::from("tracked.bin")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("already tracked by git"), "{err}");
    }

    #[test]
    fn add_force_does_not_bypass_infrastructure_path_refusal() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        let err = add_with_options(
            &repo,
            &[PathBuf::from(".git/info/exclude")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("gat/git infrastructure"), "{err}");
    }

    /// `--force` follows git's own style: normal argument selection
    /// determines scope, and force disables ignore filtering *within*
    /// it -- so `gat add --force <dir>` includes Git-ignored (and
    /// `.gatignore`d) new files under that directory instead of
    /// silently skipping them, parity with the explicit-path force
    /// behavior above (`add_force_bypasses_gitignore_refusal_for_an_explicit_path`).
    #[test]
    fn add_dir_force_includes_gitignored_and_gatignored_new_members() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "*.bin\n").unwrap();
        std::fs::write(tmp.path().join(".gatignore"), "scratch/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("data/scratch")).unwrap();
        std::fs::write(tmp.path().join("data/model.bin"), b"model").unwrap();
        std::fs::write(tmp.path().join("data/scratch/drop.bin"), b"drop").unwrap();
        std::fs::write(tmp.path().join("data/notes.txt"), b"notes").unwrap();

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("data")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 3);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let mut paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec!["data/model.bin", "data/notes.txt", "data/scratch/drop.bin"]
        );
    }

    /// The whole-*directory* ignore case (as opposed to the
    /// `*.bin`-pattern case above, which only ignores files by name, not
    /// by pruning a directory outright): `.gitignore` says `ignored/`,
    /// and `ignored/nested/a.bin` sits two levels below that ignored
    /// root. `--force` on the ignored directory itself must still
    /// discover and add the nested file -- proving forced discovery
    /// actually descends into an ignored directory rather than only
    /// force-including files gix would otherwise emit without
    /// recursing further.
    #[test]
    fn add_force_on_an_ignored_directory_descends_into_its_nested_members() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("ignored/nested")).unwrap();
        std::fs::write(tmp.path().join("ignored/nested/a.bin"), b"a").unwrap();

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("ignored/")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["ignored/nested/a.bin"]);
    }

    /// Same whole-directory-ignore case as
    /// `add_force_on_an_ignored_directory_descends_into_its_nested_members`,
    /// but rooted at `.` (`gat add --force .`) instead of naming
    /// `ignored/` directly -- root-scope force must still descend into
    /// an ignored subtree while root `.git/**`/`.gat/**` stay pruned.
    #[test]
    fn add_force_dot_descends_into_an_ignored_directory_while_excluding_infra() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("ignored/nested")).unwrap();
        std::fs::write(tmp.path().join("ignored/nested/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("keep.bin"), b"k").unwrap();

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from(".")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 3);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let mut paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec![".gitignore", "ignored/nested/a.bin", "keep.bin"]
        );
    }

    /// Same whole-directory-ignore case again, this time through a glob
    /// argument (`gat add --force 'ignored/**/*.bin'`) -- glob discovery
    /// must get the same forced descent into an ignored directory that
    /// directory discovery does (semantic parity between the two forms).
    #[test]
    fn add_force_glob_descends_into_an_ignored_directory_for_matching_members() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("ignored/nested")).unwrap();
        std::fs::write(tmp.path().join("ignored/nested/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("ignored/nested/b.txt"), b"b").unwrap();

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("ignored/**/*.bin")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["ignored/nested/a.bin"]);
    }

    /// `--force` disables ignore filtering *within* the requested scope
    /// but never widens the scope itself: an ignored file that sits
    /// outside the named directory stays unselected even under
    /// `--force`.
    #[test]
    fn add_force_does_not_widen_directory_scope_to_a_sibling_ignored_directory() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("ignored")).unwrap();
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        std::fs::write(tmp.path().join("ignored/outside-scope.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("other/inside-scope.bin"), b"b").unwrap();
        std::fs::write(tmp.path().join("other/inside-scope.txt"), b"c").unwrap();

        // Forcing `other/` must never reach into the sibling `ignored/`
        // directory: scope is unchanged by `--force`, only ignore
        // filtering within it is.
        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("other")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 2);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let mut paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec!["other/inside-scope.bin", "other/inside-scope.txt"]
        );
    }

    /// Same scope-not-widened invariant as
    /// `add_force_does_not_widen_directory_scope_to_a_sibling_ignored_directory`,
    /// but for the glob argument form: a residual glob's non-matching
    /// members inside scope, and an ignored directory outside scope,
    /// must both stay unselected under `--force`.
    #[test]
    fn add_force_glob_does_not_widen_scope_or_bypass_the_residual_matcher() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("ignored")).unwrap();
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        std::fs::write(tmp.path().join("ignored/outside-scope.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("other/inside-scope.bin"), b"b").unwrap();
        std::fs::write(tmp.path().join("other/inside-scope.txt"), b"c").unwrap();

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("other/**/*.bin")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["other/inside-scope.bin"]);
    }

    /// Directory-add force parity with directory-add non-force: without
    /// `--force`, the same tree only picks up the non-ignored file --
    /// confirms `--force` widens filtering, not scope, for directory
    /// arguments (parity with `add_dir_gitignore_now_governs_new_file_discovery`).
    #[test]
    fn add_dir_without_force_reports_the_same_ignored_members() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "*.bin\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/model.bin"), b"model").unwrap();
        std::fs::write(tmp.path().join("data/notes.txt"), b"notes").unwrap();

        let outcome = add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries[0].path, "data/notes.txt");
    }

    /// `--force` never bypasses the symlink restriction, for any of the
    /// three argument shapes (explicit path, directory, glob) -- an
    /// explicitly-named symlink is still a hard error, and a directory/
    /// glob's symlink members are still silently excluded from
    /// discovery rather than force-included.
    #[test]
    #[cfg(unix)]
    fn add_force_does_not_bypass_the_symlink_restriction() {
        use std::os::unix::fs::symlink;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/target.bin"), b"payload").unwrap();
        symlink(
            tmp.path().join("data/target.bin"),
            tmp.path().join("data/link.bin"),
        )
        .unwrap();

        let err = add_with_options(
            &repo,
            &[PathBuf::from("data/link.bin")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");

        let outcome = add_with_options(
            &repo,
            &[PathBuf::from("data")],
            true,
            &NoopProgress,
            &Lifecycle::new(),
        )
        .unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries[0].path, "data/target.bin");
    }

    #[test]
    fn add_succeeds_for_an_explicit_path_not_in_gitignore() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();

        let outcome = add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 1);
    }

    #[test]
    fn add_reuses_an_existing_gat_tracked_explicit_path_despite_gats_own_managed_exclude_block() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();

        // `big.bin` is excluded via `.git/info/exclude`:
        // it's already Gat-tracked, so the desired-state overlay must
        // keep it addable even though gat's own managed block Git-
        // ignores it -- this must never be mistaken for a *new* file
        // that's ignored and thus refused.
        let outcome = add(&repo, &[PathBuf::from("big.bin")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 1);
    }

    /// A failure to persist
    /// `gat.lock` -- after the file has already been hashed and its
    /// object published into the cache -- must never report the path as
    /// tracked, must never touch the working-tree file `add` hashed from,
    /// and may leave the just-published cache object orphaned (harmless,
    /// content-addressed, reclaimed later by `gat gc`) rather than trying
    /// any risky rollback of an immutable, potentially shared object.
    #[test]
    #[cfg(unix)]
    fn add_orphans_a_cache_object_but_leaves_the_path_untracked_when_gat_lock_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        // Route the object cache outside the repo root so making the root
        // read-only (to force `gat.lock`'s write to fail) doesn't also
        // block cache ingestion -- isolating the failure to exactly the
        // boundary this test characterizes.
        let cache_dir = tempfile::tempdir().unwrap();
        let mut cfg = repo.load_config().unwrap();
        cfg.cache.location = Some(
            gat_core::cache_location::CacheLocation::try_from_path(cache_dir.path().to_path_buf())
                .expect("nonempty cache location"),
        );
        repo.write_config_fixture(&cfg).unwrap();

        std::fs::write(tmp.path().join("big.bin"), b"payload").unwrap();
        let expected_oid = gat_core::oid::Oid::from_bytes(*blake3::hash(b"payload").as_bytes());

        // Pre-create (and pre-warm) the materialized-state SQLite mirror,
        // then keep its directory independently writable: `add` now
        // refreshes the desired mirror before ingesting, and SQLite's WAL
        // mode needs to create/write `-wal`/`-shm` files alongside the
        // database, which a read-only repo root would otherwise block --
        // isolating the injected failure to exactly `gat.lock`'s write,
        // like the cache relocation above already does for cache writes.
        gat_io::StateStore::open(&layout(tmp.path())).unwrap();
        std::fs::set_permissions(
            tmp.path().join(".gat/state"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = add(&repo, &[PathBuf::from("big.bin")], &NoopProgress);
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            result.is_err(),
            "add must fail when gat.lock can't be written"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("big.bin")).unwrap(),
            b"payload",
            "the source file add hashed from must be untouched"
        );
        assert!(
            gat_io::LockStore::load_repository(&layout(tmp.path()))
                .unwrap()
                .entries
                .is_empty(),
            "gat.lock must not claim the path is tracked when its write failed"
        );
        assert!(
            gat_engine::test_support::load_materialized_for_test(&repo)
                .unwrap()
                .entries
                .is_empty(),
            "materialized state must not claim the path is tracked either"
        );
        let cache_root = layout(tmp.path()).resolve_cache_root(Some(
            &gat_core::cache_location::CacheLocation::try_from_path(std::path::PathBuf::from(
                cache_dir.path().as_os_str(),
            ))
            .expect("nonempty fixture cache path"),
        ));
        let cache_path = cache_root.object_path_for_test(&expected_oid);
        assert!(
            cache_path.exists(),
            "the ingested object must remain cached (orphaned but harmless) even though \
             tracking failed"
        );
    }

    #[test]
    #[cfg(unix)]
    fn add_discovers_and_roundtrips_escaped_paths() {
        for selection in ["data", "data/*.bin"] {
            let tmp = test_repo();
            let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            std::fs::create_dir(tmp.path().join("data")).unwrap();
            for name in ["fi\tle.bin", "fi\nle.bin", "fi\rle.bin", "fi\"le.bin"] {
                std::fs::write(tmp.path().join("data").join(name), b"payload").unwrap();
            }
            add(&repo, &[PathBuf::from(selection)], &NoopProgress).unwrap();
            let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
            assert_eq!(lock.entries.len(), 4);
            for entry in lock.entries {
                assert_eq!(
                    std::fs::read(tmp.path().join(entry.path.as_str())).unwrap(),
                    b"payload"
                );
            }
        }
    }

    // ---- Directory candidate discovery regression tests ----

    /// A modified managed descendant is revisited by a subsequent
    /// `gat add <dir>`: writing new content must update the OID in
    /// `gat.lock`, not silently skip the already-excluded path.
    #[test]
    fn add_dir_revisits_modified_managed_descendant() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/nested")).unwrap();
        std::fs::write(tmp.path().join("data/nested/a.bin"), b"old").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        let old_oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries
            .iter()
            .find(|e| e.path == "data/nested/a.bin")
            .unwrap()
            .oid;

        // Modify the file
        std::fs::write(tmp.path().join("data/nested/a.bin"), b"new").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        let new_oid = gat_io::LockStore::load_repository(&layout(tmp.path()))
            .unwrap()
            .entries
            .iter()
            .find(|e| e.path == "data/nested/a.bin")
            .unwrap()
            .oid;
        assert_ne!(old_oid, new_oid, "modified file must get a new OID");
        assert_eq!(new_oid, oid(blake3::hash(b"new").to_hex().as_ref()));
    }

    /// After the first `gat add`, managed paths appear in
    /// `.git/info/exclude`. A subsequent `gat add <dir>` must
    /// still discover those paths — gat's own exclude block must not hide
    /// them from the directory walk.
    #[test]
    fn add_dir_not_hidden_by_managed_exclude() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"content").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();

        // Confirm it's in .git/info/exclude
        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(
            exclude.contains("data/a.bin"),
            "managed path should be in .git/info/exclude"
        );

        // Re-add: should still discover data/a.bin despite it being excluded
        let outcome = add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 1);
    }

    /// Warm stat-first re-add of a directory with unchanged managed
    /// descendants performs zero content hashes while still revisiting
    /// files (uses same counter mechanism as
    /// `add_dir_reuses_unchanged_files_and_only_hashes_new_ones`).
    #[test]
    fn add_dir_warm_unchanged_zero_hashes() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"content").unwrap();
        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();

        // A re-add of the same unchanged directory should be zero-hash
        // immediately: the initial add's coherent observation already
        // recorded a reusable proof for every file.
        gat_io::with_exclusive_hash_file_call_count(|| {
            add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
            assert_eq!(
                gat_io::hash_file_call_count(),
                0,
                "warm unchanged re-add must perform zero content hashes"
            );
        });
    }

    /// `.gitignore` governs discovery of *new* Gat candidates: a
    /// Git-ignored new file is never discovered by a
    /// directory add at all, so it's silently skipped -- no advisory
    /// hint is recorded any more.
    #[test]
    fn add_dir_gitignore_now_governs_new_file_discovery() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "*.bin\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/model.bin"), b"model").unwrap();
        std::fs::write(tmp.path().join("data/notes.txt"), b"notes").unwrap();

        // `.gitignore` governs new Gat-candidate discovery: a Git-ignored
        // new file is never even discovered
        // by a directory add, so it isn't silently added with an
        // advisory hint any more -- only the non-ignored sibling is.
        let outcome = add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        assert_eq!(outcome.added_count, 1);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "data/notes.txt");
    }

    /// Nested recursion is explicitly covered — deeply nested paths
    /// like `data/nested/deeper/a.bin` are discovered by `gat add data`.
    #[test]
    fn add_dir_deeply_nested() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data/nested/deeper")).unwrap();
        std::fs::write(tmp.path().join("data/nested/deeper/a.bin"), b"deep").unwrap();

        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "data/nested/deeper/a.bin");
    }

    /// A `.gitignore`d directory (the canonical `node_modules/` case) is
    /// pruned by gix's own dirwalk rather than walked and filtered
    /// per-file: none of its (potentially huge number of) descendants are
    /// ever discovered as Gat candidates.
    #[test]
    fn add_dot_prunes_a_gitignored_node_modules_style_directory() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/pkg/nested")).unwrap();
        std::fs::write(tmp.path().join("node_modules/pkg/index.js"), b"x").unwrap();
        std::fs::write(tmp.path().join("node_modules/pkg/nested/a.js"), b"y").unwrap();
        std::fs::write(tmp.path().join("keep.bin"), b"a").unwrap();

        add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(
            !paths.iter().any(|p| p.starts_with("node_modules/")),
            "no descendant of the gitignored node_modules/ tree should ever be discovered, got {paths:?}"
        );
        assert!(paths.contains(&"keep.bin"), "got {paths:?}");
    }

    /// A nested `.gitignore` (rules scoped to a subdirectory, not just the
    /// repo root) is honored by directory discovery.
    #[test]
    fn add_dot_honors_a_nested_gitignore() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/.gitignore"), "*.tmp\n").unwrap();
        std::fs::write(tmp.path().join("data/keep.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/drop.tmp"), b"b").unwrap();

        add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(!paths.contains(&"data/drop.tmp"), "got {paths:?}");
        assert!(paths.contains(&"data/keep.bin"), "got {paths:?}");
    }

    /// A user entry in `.git/info/exclude` (independent of any
    /// `.gitignore`) is honored by directory discovery exactly like a
    /// `.gitignore` rule -- both are real Git ignore sources gix's
    /// dirwalk already understands.
    #[test]
    fn add_dot_honors_a_git_info_exclude_entry() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join(".git/info/exclude"), "drop.bin\n").unwrap();
        std::fs::write(tmp.path().join("drop.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("keep.bin"), b"b").unwrap();

        add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.bin"]);
    }

    /// Deleting `.gat/state/state.sqlite3` (the materialized/desired
    /// state mirror) must not turn a subsequent `gat add .` into a
    /// full re-ingest of every already-tracked file: the desired-state
    /// overlay is rebuilt straight from `gat.lock`, unchanged files keep
    /// their recorded OID, and only a verification hash (never a fresh
    /// object copy) is needed per existing file.
    #[test]
    fn add_dot_after_state_db_deletion_recovers_without_reingesting_unchanged_files() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"unchanged").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"also-unchanged").unwrap();
        add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();
        let lock_before = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let oids_before_by_path: HashMap<String, String> = lock_before
            .entries
            .iter()
            .map(|e| (e.path.to_string(), e.oid.to_string()))
            .collect();

        std::fs::remove_file(tmp.path().join(".gat/state/state.sqlite3")).unwrap();
        gat_io::with_exclusive_hash_file_call_count(|| {
            add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();
            let hashes_after_recovery = gat_io::hash_file_call_count();
            // One verification hash per pre-existing, unchanged file --
            // never zero (that would mean OIDs weren't actually
            // re-established from `gat.lock`), and never more than that
            // (that would mean something was hashed more than once).
            // `gat.lock` itself is a brand-new file at this point (not
            // present on the first `add .`, which is what wrote it) and
            // goes through normal ingest, not this verification path.
            assert_eq!(
                hashes_after_recovery, 2,
                "expected exactly one verification hash per pre-existing file, got {hashes_after_recovery}"
            );
        });

        let lock_after = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let oids_after_by_path: HashMap<String, String> = lock_after
            .entries
            .iter()
            .map(|e| (e.path.to_string(), e.oid.to_string()))
            .collect();
        for (path, oid) in &oids_before_by_path {
            assert_eq!(
                oids_after_by_path.get(path),
                Some(oid),
                "`{path}` must retain its OID across recovery"
            );
        }
    }

    /// A directory/glob-overlay candidate already carries its
    /// `desired_oid` straight from discovery's single scoped
    /// `DesiredQuery` read (see `DesiredState::discover_add_candidates`),
    /// so `partition_reusable` must never issue a second, per-candidate
    /// `DesiredQuery::exact` lookup for it -- that would turn one bounded
    /// scoped read into `O(candidates)` additional desired-state queries.
    /// Pins this by asserting the underlying `with_desired_rows`/
    /// `desired_rows` call count for a repeated `gat add .` over many
    /// already-Gat-tracked files is a small constant (the overlay's one
    /// scoped read), never proportional to how many files are re-added.
    #[test]
    fn add_dot_reusing_many_existing_gat_tracked_files_issues_one_desired_state_query() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        for i in 0..50 {
            std::fs::write(tmp.path().join(format!("f{i}.bin")), format!("body-{i}")).unwrap();
        }
        add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();

        let before = gat_io::state_test_support::snapshot();
        add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();
        let after = gat_io::state_test_support::snapshot();
        let desired_rows_calls = after.1 - before.1;
        assert!(
            desired_rows_calls <= 2,
            "expected a bounded, scope-sized number of desired-state queries \
             (discovery's one scoped overlay read, plus at most one \
             fallback for any unresolved explicit paths -- there are none \
             here), not one per re-added file; got {desired_rows_calls} \
             for 50 already-Gat-tracked candidates"
        );
    }

    // ---- Path scope / root / literal-vs-glob regression tests ----

    /// `gat add .` selects repo-root candidates recursively.
    #[test]
    fn add_dot_selects_repo_root_recursively() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("root.bin"), b"root").unwrap();
        std::fs::write(tmp.path().join("data/nested.bin"), b"nested").unwrap();

        let outcome = add(&repo, &[PathBuf::from(".")], &NoopProgress).unwrap();
        // At least our two files must be included
        assert!(outcome.added_count >= 2);
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<&str> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"root.bin"));
        assert!(paths.contains(&"data/nested.bin"));
    }

    /// `gat add` with `./data/` normalises the same as `data`.
    #[test]
    fn add_dot_slash_data_slash_normalizes_like_data() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::create_dir(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        add(&repo, &[PathBuf::from("./data/")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "data/a.bin");
    }

    // ---- Sparse desired-state publication regression tests ----

    /// In a sharded repo, adding one new file only rewrites the
    /// touched shard file, not all shard files. Verified by comparing
    /// inode numbers before and after the add.
    #[test]
    #[cfg(unix)]
    fn add_in_sharded_repo_only_rewrites_touched_shard() {
        use gat_core::lock::LockShardId;
        use gat_core::lock::LockShardLevels;
        use std::collections::HashMap;

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut cfg = repo.load_config().unwrap();
        cfg.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(1).unwrap());
        repo.write_config_fixture(&cfg).unwrap();

        // Create enough files to spread across multiple shards
        let mut paths = Vec::new();
        for i in 0..40 {
            let path = format!("file-{i}.bin");
            std::fs::write(tmp.path().join(&path), format!("payload-{i}")).unwrap();
            paths.push(PathBuf::from(path));
        }
        add(&repo, &paths, &NoopProgress).unwrap();

        // Record inode numbers for all shard files
        let io_layout = layout(tmp.path());
        let shard_files_before: HashMap<LockShardId, u64> =
            gat_io::lock_test_support::shard_inodes(&io_layout).unwrap();
        assert!(
            shard_files_before.len() > 1,
            "need multiple shards for this test"
        );

        // Add one new file
        let new_path = "new-file.bin";
        std::fs::write(tmp.path().join(new_path), b"new payload").unwrap();
        let new_shard = LockShardId::for_path(&gp(new_path), LockShardLevels::new(1).unwrap());
        add(&repo, &[PathBuf::from(new_path)], &NoopProgress).unwrap();

        // Only the touched shard should have a different inode
        let shard_files_after = gat_io::lock_test_support::shard_inodes(&io_layout).unwrap();
        for (shard_id, ino_after) in shard_files_after {
            if let Some(&ino_before) = shard_files_before.get(&shard_id) {
                if shard_id == new_shard {
                    assert_ne!(
                        ino_before, ino_after,
                        "touched shard {shard_id} must be rewritten"
                    );
                } else {
                    assert_eq!(
                        ino_before, ino_after,
                        "untouched shard {shard_id} must NOT be rewritten"
                    );
                }
            }
        }

        // Verify the new file is in the lock
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert!(lock.entries.iter().any(|e| e.path == new_path));
        assert_eq!(lock.entries.len(), 41);
    }

    /// A sharded `gat add` must keep `.git/info/exclude` in
    /// sync exactly like the flat path does, streaming desired paths from
    /// the retained mutation session instead of silently falling back to a
    /// complete-lock read that a sharded add never actually needed.
    #[test]
    fn add_in_sharded_repo_keeps_info_exclude_in_sync() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut cfg = repo.load_config().unwrap();
        cfg.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(1).unwrap());
        repo.write_config_fixture(&cfg).unwrap();

        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        add(&repo, &[PathBuf::from("a.bin")], &NoopProgress).unwrap();
        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("a.bin"), "a.bin must be excluded");

        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();
        add(&repo, &[PathBuf::from("b.bin")], &NoopProgress).unwrap();
        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("a.bin"), "a.bin must still be excluded");
        assert!(exclude.contains("b.bin"), "b.bin must now be excluded too");
    }

    /// An already-sharded `gat add <directory>` with nested
    /// descendants must regenerate `.git/info/exclude` correctly via the
    /// retained mutation-session path, not just for individually added
    /// files. Every descendant must
    /// end up in the desired state and be genuinely ignored by git's own
    /// ignore semantics, while unrelated files stay visible.
    #[test]
    fn add_directory_in_sharded_repo_covers_nested_descendants_via_store_backed_path() {
        use gix::bstr::BStr;
        use gix::glob::pattern::Case;
        use gix::ignore::Search;
        use gix::ignore::search::Ignore;

        fn is_ignored(exclude_text: &str, path: &str) -> bool {
            let lines: Vec<String> = exclude_text
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
                .map(str::to_string)
                .collect();
            let search = Search::from_overrides(lines, Ignore::default());
            let bytes = path.as_bytes();
            let mut matched = false;
            for (i, b) in bytes.iter().enumerate() {
                if *b == b'/'
                    && let Some(m) = search.pattern_matching_relative_path(
                        BStr::new(&bytes[..i]),
                        Some(true),
                        Case::Sensitive,
                    )
                {
                    matched = !m.pattern.is_negative();
                }
            }
            if let Some(m) = search.pattern_matching_relative_path(
                BStr::new(bytes),
                Some(false),
                Case::Sensitive,
            ) {
                matched = !m.pattern.is_negative();
            }
            matched
        }

        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut cfg = repo.load_config().unwrap();
        cfg.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(1).unwrap());
        repo.write_config_fixture(&cfg).unwrap();

        // Establish the sharded on-disk shape with an initial, unrelated
        // file before exercising the directory add.
        std::fs::write(tmp.path().join("seed.bin"), b"seed").unwrap();
        add(&repo, &[PathBuf::from("seed.bin")], &NoopProgress).unwrap();
        assert!(
            matching_lock_shape(&repo, tmp.path())
                .unwrap()
                .is_some_and(|levels| !levels.is_flat()),
            "repo must already be sharded before the directory add"
        );

        std::fs::create_dir_all(tmp.path().join("data/nested/deeper")).unwrap();
        std::fs::write(tmp.path().join("data/a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("data/nested/b.bin"), b"b").unwrap();
        std::fs::write(tmp.path().join("data/nested/deeper/c.bin"), b"c").unwrap();
        // Unrelated, untracked file elsewhere in the repo that must never
        // be hidden by the directory add.
        std::fs::write(tmp.path().join("keep.txt"), b"keep").unwrap();

        add(&repo, &[PathBuf::from("data")], &NoopProgress).unwrap();

        // Still sharded afterwards -- excludes used the retained sparse
        // mutation view, not a fallback to a full flat `Lock`.
        assert!(
            matching_lock_shape(&repo, tmp.path())
                .unwrap()
                .is_some_and(|levels| !levels.is_flat()),
            "directory add must not have flattened the sharded lock"
        );

        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        for descendant in [
            "data/a.bin",
            "data/nested/b.bin",
            "data/nested/deeper/c.bin",
        ] {
            assert!(
                lock.entries.iter().any(|e| e.path == descendant),
                "{descendant} must be in desired state"
            );
        }

        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        for descendant in [
            "data/a.bin",
            "data/nested/b.bin",
            "data/nested/deeper/c.bin",
        ] {
            assert!(
                is_ignored(&exclude, descendant),
                "{descendant} must be ignored by git semantics"
            );
        }
        assert!(
            !is_ignored(&exclude, "keep.txt"),
            "unrelated untracked file must not be hidden"
        );
    }

    /// Flat-lock (non-sharded) behavior is unchanged — adding a file
    /// to a flat repo still works correctly.
    #[test]
    fn add_flat_lock_behavior_unchanged() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.bin"), b"b").unwrap();
        add(
            &repo,
            &[PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            &NoopProgress,
        )
        .unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 2);

        // Add one more
        std::fs::write(tmp.path().join("c.bin"), b"c").unwrap();
        add(&repo, &[PathBuf::from("c.bin")], &NoopProgress).unwrap();
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 3);
    }

    /// A flat `gat.lock` `add` must take the same sparse,
    /// touched-shard-scoped pipeline as a sharded one (flat is
    /// the degenerate one-shard case, not a separate full-lock rewrite).
    /// Asserts that after adding to a flat repo the on-disk shape is
    /// `Some(OnDiskShape::Flat)`, `gat.lock` stays a plain file rather than
    /// becoming a `gat.lock/` directory, and the desired mirror holds exactly
    /// one logical shard identity keyed by the `"gat.lock"` sentinel.
    #[test]
    fn add_in_a_flat_repo_uses_the_sparse_pipeline_and_stays_a_single_file() {
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

        // Adding a second file must keep the single-file flat shape, going
        // through the sparse flat publish (a `"gat.lock"` sentinel shard),
        // never fanning out into a `gat.lock/` directory.
        std::fs::write(tmp.path().join("b.bin"), b"payload2").unwrap();
        add(&repo, &[PathBuf::from("b.bin")], &NoopProgress).unwrap();

        assert!(
            tmp.path().join("gat.lock").is_file(),
            "flat add must publish a single gat.lock file, never a gat.lock/ directory"
        );
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        assert_eq!(lock.entries.len(), 2);

        let store = refreshed_store(&repo, tmp.path());
        let shard_ids = gat_io::state_shard_ids_for_test(&store).unwrap();
        assert_eq!(
            shard_ids,
            vec![gat_core::lock::LockShardId::flat()],
            "flat desired rows must all share one logical shard id"
        );
    }

    /// Coordinated writer-vs-reshape race: `add`'s shape-selection-through-
    /// publication window and `Repo::reshape_lock`'s transactional reshape
    /// both serialize through the same `RepoLock`, so a reshape
    /// racing an `add` must never land in between shape selection and
    /// publication and make the touched-shard ids/rows `add` computed
    /// stale by the time they're published. A `std::sync::Barrier` forces
    /// both threads to actually start at the same instant every
    /// iteration (rather than relying on incidental scheduling to ever
    /// hit the race window), and the real OS-level `flock`/`LockFileEx`
    /// underneath `RepoLock` is what then serializes them -- whichever
    /// one the OS lets through first, the other must see a fully
    /// consistent, single-shape lock to read/reshape/publish against.
    #[test]
    fn add_and_reshape_racing_concurrently_never_corrupts_the_lock() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        std::fs::write(tmp.path().join("seed.bin"), b"seed").unwrap();
        add(&repo, &[PathBuf::from("seed.bin")], &NoopProgress).unwrap();

        for i in 0..20 {
            let filename = format!("race-{i}.bin");
            std::fs::write(
                tmp.path().join(&filename),
                format!("payload-{i}").as_bytes(),
            )
            .unwrap();

            // Alternate directions so every race reshapes the live lock,
            // without a second, serial reshape just to reset the fixture.
            let target_depth = if i % 2 == 0 { "2" } else { "0" };
            gat_command::config(
                &repo,
                gat_command::ConfigRequest::new(
                    gat_core::config_keys::SettingKey::LockShardLevels,
                    gat_command::ConfigAction::Set(vec![target_depth.to_string()]),
                    gat_core::config::ConfigScope::Project,
                ),
            )
            .unwrap();

            let barrier = std::sync::Barrier::new(2);
            let repo = &repo;
            let filename_ref = &filename;
            let added = std::thread::scope(|s| {
                let add = s.spawn(|| {
                    barrier.wait();
                    add(repo, &[PathBuf::from(filename_ref.clone())], &NoopProgress)
                });
                s.spawn(|| {
                    barrier.wait();
                    repo.reshape_lock().unwrap();
                });
                add.join().unwrap()
            });
            let published = match added {
                Ok(_) => true,
                Err(AddError::RepositoryMutation(error))
                    if matches!(*error, gat_engine::RepositoryMutationError::Stale) =>
                {
                    false
                }
                Err(error) => panic!("unexpected add failure: {error:?}"),
            };

            // Whichever order actually ran, the live lock must be exactly
            // one consistent shape (never a mixed-depth tree left by an
            // interleaved reshape/publish), and must still contain every
            // successfully published path. A stale add can refuse this iteration -- never
            // silently dropped by a writer racing a reshape's snapshot-
            // then-publish window.
            let levels = gat_io::LockStore::current_repository_shard_levels(
                &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
            )
            .unwrap()
            .expect("something must be tracked by now");
            assert!(
                levels.is_flat() || levels == gat_core::lock::LockShardLevels::new(2).unwrap(),
                "iteration {i}: unexpected on-disk shard depth {levels:?}"
            );
            let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
            assert_eq!(lock.entries.iter().any(|e| e.path == filename), published);
            assert!(lock.entries.iter().any(|e| e.path == "seed.bin"));
        }
    }

    /// Deterministic version of the race above: rather than relying on a
    /// `Barrier` to merely start both threads at once (which a
    /// regression in the outer guard could still pass by luck if scheduling
    /// keeps the two calls from ever actually overlapping), this pins the
    /// race window --
    /// writer holds the shape-selection guard, a reshape attempt lands
    /// while it's still held, then the writer publishes -- using real
    /// channel handoffs instead of timing. It calls the same production
    /// functions `add` itself calls
    /// (shape-lock acquisition/state-owned sparse publication)
    /// directly, only with an explicit pause inserted between shape
    /// selection and publication: the writer thread parks on a channel
    /// `recv()` *while still holding the guard*, and only resumes once
    /// the same thread-scoped `gat_io::atomic_test_support::with_acquire_attempt_hook`
    /// used elsewhere in this crate confirms the
    /// reshape thread's `RepoLock::acquire` inside `reshape_lock` has
    /// actually reached and blocked on the real OS-level
    /// `flock`/`LockFileEx` -- guaranteed by that handshake, not a fixed
    /// sleep guessing how long the reshape thread takes to get there.
    #[test]
    fn writer_holds_the_shape_selection_guard_across_a_concurrent_reshape_attempt() {
        let tmp = test_repo();
        let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        // Default `lock.shard_levels` is `0` (flat), matching what's on
        // disk after this seed `add` -- the writer below must observe
        // `Some(Flat)` from `current_matching_lock_shape_locked`.
        std::fs::write(tmp.path().join("seed.bin"), b"seed").unwrap();
        add(&repo, &[PathBuf::from("seed.bin")], &NoopProgress).unwrap();

        std::fs::write(tmp.path().join("race.bin"), b"race-payload").unwrap();

        let (writer_ready_tx, writer_ready_rx) = std::sync::mpsc::channel::<()>();
        let (resume_writer_tx, resume_writer_rx) = std::sync::mpsc::channel::<()>();
        let (acquire_attempted_tx, acquire_attempted_rx) = std::sync::mpsc::channel::<()>();

        let repo_ref = &repo;
        let root = tmp.path();
        std::thread::scope(|s| {
            let reshaper = s.spawn(move || {
                writer_ready_rx.recv().unwrap();
                // Flip the config to a shape that actually differs from
                // what's on disk, so `reshape_lock` below has real work
                // to do rather than a no-op that never touches the live
                // path at all -- safe to do without the guard, since
                // config isn't part of the on-disk lock state it protects.
                gat_command::config(
                    repo_ref,
                    gat_command::ConfigRequest::new(
                        gat_core::config_keys::SettingKey::LockShardLevels,
                        gat_command::ConfigAction::Set(vec!["2".to_string()]),
                        gat_core::config::ConfigScope::Project,
                    ),
                )
                .unwrap();
                repo_ref.reshape_lock().unwrap()
            });

            // Same thread-scoped acquire-attempt hook as
            // other `RepoLock::acquire` race tests in this crate: it lets the
            // writer below wait for deterministic
            // proof that the reshaper's `RepoLock::acquire` call has
            // actually reached and blocked on the real OS lock, rather
            // than a fixed sleep guessing how long that takes.
            let writer = gat_io::atomic_test_support::with_acquire_attempt_hook(
                reshaper.thread().id(),
                acquire_attempted_tx,
                || {
                    let writer = s.spawn(move || {
                        let io_layout = layout(root);
                        let mut store = gat_io::StateStore::open(&io_layout).unwrap();
                        gat_engine::test_support::refresh_desired_index(repo_ref, &mut store)
                            .unwrap();
                        // Same guard `add` holds from shape selection through
                        // publication (`LockWriteGuard`).
                        let shape_lock = gat_io::lock_test_support::acquire_matching_shape(
                            &io_layout,
                            repo_ref.lock_shard_levels().unwrap(),
                        )
                        .unwrap();
                        assert!(
                            shape_lock.can_publish_incrementally(),
                            "flat shape must already match config"
                        );

                        // Tell the reshape thread it's safe to attempt now --
                        // the guard above is guaranteed still held at this
                        // point, since this thread hasn't released it yet.
                        writer_ready_tx.send(()).unwrap();
                        // Block *while still holding the guard* until the
                        // main thread below confirms the reshape thread's
                        // `RepoLock::acquire` call has actually reached the
                        // real OS lock, guaranteeing it blocks on this
                        // guard's underlying lock for the whole wait.
                        resume_writer_rx.recv().unwrap();

                        let new_entries = vec![Entry {
                            path: gp("race.bin"),
                            oid: oid(&"d".repeat(64)),
                        }];
                        store
                            .publish_desired_upsert::<gat_io::DesiredPublicationError>(
                                &io_layout,
                                &shape_lock,
                                &new_entries,
                            )
                            .unwrap();
                        drop(shape_lock);
                    });

                    acquire_attempted_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect(
                            "reshape_lock must reach RepoLock::acquire's OS-lock boundary \
                             while the writer still holds the shape-selection guard",
                        );
                    resume_writer_tx.send(()).unwrap();

                    writer
                },
            );

            writer.join().unwrap();
            let reshaped_to = reshaper.join().unwrap();
            assert_eq!(
                reshaped_to,
                Some(gat_core::lock::LockShardLevels::new(2).unwrap()),
                "reshape must still have run once the writer released the guard"
            );
        });

        let levels = gat_io::LockStore::current_repository_shard_levels(
            &gat_io::RepositoryLayout::at(tmp.path().to_path_buf()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            levels,
            gat_core::lock::LockShardLevels::new(2).unwrap(),
            "the reshape that was forced to wait for the writer's guard must still land cleanly"
        );
        let lock = gat_io::LockStore::load_repository(&layout(tmp.path())).unwrap();
        let paths: Vec<_> = lock.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(
            paths.contains(&"seed.bin") && paths.contains(&"race.bin"),
            "both the pre-existing and race-window entries must survive: {paths:?}"
        );
    }
}
