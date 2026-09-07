//! Incremental desired-state indexing: keeps the `state` table's desired
//! half (see `gat_io::StateStore`) current with whatever
//! `gat.lock` (flat or sharded) currently contains, reparsing only shards
//! whose content actually changed since the last refresh, and only ever
//! writing to SQLite when something actually needs to change.
//!
//! This is the reconciliation half of sync planning: it decides which paths'
//! desired state currently disagrees with materialized state. It is
//! deliberately independent of `Validation` (`super::plan`'s
//! file/cache-object checks) -- reconciliation never inspects a
//! tracked file's own working-tree content, only `gat.lock` and
//! materialized state.
//!
//! Engine delegates the physical lock observation and SQLite refresh to
//! [`gat_io::StateStore::refresh_desired_state`]. Every
//! lock shape is treated uniformly as 0..N shards there (a flat
//! `gat.lock` is the one-shard case), and each shard's content identity is
//! computed with two tiers of increasing cost -- a local stat cache, then
//! a direct content hash. This is per-shard stat acceleration, not a
//! tree/subtree Merkle proof: every shard file is still enumerated and
//! stat'd on every refresh, while an unchanged Git tree does not currently
//! let a whole subtree of shards be skipped as a unit.
//!
//! 1. **Local stat cache**: if the shard file's shared
//!    recorded stat proof hasn't changed since the identity
//!    was last recorded, reuse it without reading the file at all.
//! 2. **Content hash**: otherwise, read the file exactly once through
//!    the shared coherent-observation primitive
//!    and hash it with BLAKE3 (`gat_io::lock_hash_shard_bytes`);
//!    if the shard also needs reparsing (new or content-changed), its
//!    rows are parsed from that exact same buffer -- never a second,
//!    independent read.
//!
//! A shard's desired rows are only rewritten in `state` when its identity
//! actually changed from what was last recorded. When only the stat proof
//! changed, just that proof is refreshed; when neither changed, nothing is
//! written.

use crate::repository::Repository as Repo;
use gat_core::lock::CanonicalDesiredIdentity;
#[cfg(test)]
use gat_core::lock::Lock;
#[cfg(test)]
use gat_core::lock::LockShardId;
#[cfg(test)]
use gat_io::RepoLock;
#[cfg(test)]
use gat_io::lock_race_test_hooks as race_test_hooks;
use gat_io::{DesiredRefreshError, StateStore};

use super::Result;

/// Refresh the desired-state mirror/dirty index so it reflects whatever
/// `gat.lock` currently contains, reparsing only shards whose content
/// identity changed. Safe (and cheap) to call on every sync/plan; when
/// nothing changed since the previous refresh this touches no file
/// content -- and no `SQLite` write -- at all, beyond a `stat()` per shard.
///
/// The I/O capability holds the same repo-wide `RepoLock` a `gat.lock` shape reshape/sparse
/// publish takes across shard enumeration, read, and validation
/// through its incremental `SQLite` update, so every caller of
/// this shared reconciliation boundary -- not just `gat add`'s previous
/// local guard -- is protected against a reshape/sparse publish landing
/// mid-refresh (e.g. a flat->sharded swap between the shape probe and
/// shard enumeration, or a shard file replaced between its read and
/// that update). Lock acquisition is reentrant on this thread,
/// so a caller that already holds it (e.g. `gat sync` across
/// plan+execute) is unaffected. Deliberately scoped to just this
/// refresh -- never held across a caller's own, potentially expensive
/// walk/hash/ingest work (e.g. `add`'s directory walk) -- so lock
/// contention stays bounded to the cheap stat/read/validate work here.
///
/// A brand-new/just-rewritten shard (a fresh checkout, `git worktree add`,
/// or restore) establishes its desired identity immediately through one
/// coherent read the very first time this is called -- there is no
/// wall-clock wait involved.
pub(crate) fn refresh(repo: &Repo, store: &mut StateStore) -> Result<RefreshResult> {
    let refreshed = store
        .refresh_desired_state(repo.layout())
        .map_err(|err| match err {
            DesiredRefreshError::Atomic(err) => super::SyncError::from(err),
            DesiredRefreshError::Lock(err) => super::SyncError::from(err),
            DesiredRefreshError::State(err) => super::SyncError::from(err),
        })?;
    Ok(RefreshResult {
        desired_identity: refreshed.identity(),
        shard_levels: refreshed.shard_levels(),
    })
}

/// Refresh and pin one comparison's current desired-state view before the
/// refresh lock is released.
pub(crate) fn refresh_pinned(repo: &Repo, store: &mut StateStore) -> Result<RefreshResult> {
    let refreshed =
        store
            .refresh_and_pin_desired_state(repo.layout())
            .map_err(|err| match err {
                DesiredRefreshError::Atomic(err) => super::SyncError::from(err),
                DesiredRefreshError::Lock(err) => super::SyncError::from(err),
                DesiredRefreshError::State(err) => super::SyncError::from(err),
            })?;
    Ok(RefreshResult {
        desired_identity: refreshed.identity(),
        shard_levels: refreshed.shard_levels(),
    })
}

/// The one [`CanonicalDesiredIdentity`] this refresh materialized into the
/// `state` mirror -- callers that need to prove they mutated exactly the
/// revision they observed (see `crate::mutation::MutationGuard`)
/// compare this against the identity captured when their operation
/// started, instead of re-deriving it a second, possibly different way.
#[derive(Debug)]
pub struct RefreshResult {
    pub desired_identity: CanonicalDesiredIdentity,
    pub(crate) shard_levels: gat_core::lock::LockShardLevels,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyncError;
    use crate::test_harness::git_repo;
    use crate::workspace::sync::{LockFailureKind, SyncErrorKind};
    use gat_core::lexical_path::GatPath;
    use gat_io::StateStore;
    use std::path::Path;

    fn ingest(repo: &Repo, content: impl std::io::Read) -> gat_io::Ingested {
        repo.resolved_cache_root()
            .writer()
            .ingest(content)
            .unwrap()
            .0
    }

    /// RAII guard clearing [`race_test_hooks`] on drop (including on
    /// panic/early return), so a test that injects a mid-read race can
    /// never leak its hook into a later test sharing the same OS thread.
    struct RaceHookGuard;

    impl Drop for RaceHookGuard {
        fn drop(&mut self) {
            race_test_hooks::clear();
        }
    }

    fn refresh_with_threads(
        repo: &Repo,
        store: &mut StateStore,
        threads: usize,
    ) -> Result<RefreshResult> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| refresh(repo, store))
    }

    /// Compare the desired mirror with lock rows using their path and OID.
    fn paths_and_oids(lock: &Lock) -> Vec<(String, String)> {
        lock.entries
            .iter()
            .map(|e| (e.path.to_string(), e.oid.to_hex()))
            .collect()
    }

    /// Writes `path` into `gat.lock` (or the appropriate shard, for a
    /// sharded lock). A brand-new shard's desired identity is established
    /// immediately from a coherent read of its own bytes, so this test
    /// helper needs no separate settling step before the next
    /// `refresh()` call observes it.
    fn track(repo: &Repo, path: &str, content: &[u8]) {
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(repo, content);
        lock.upsert(GatPath::parse_canonical(path).unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();
    }

    /// A changed shard's identity and rows must derive from the same byte
    /// buffer. Compare both persisted results with the shard bytes on disk.
    #[test]
    fn refresh_persists_identity_and_rows_derived_from_the_same_byte_buffer_on_a_content_change() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();

        // Change the shard's content directly (a new entry), independent
        // of `track`'s own object-store ingestion plumbing.
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let replacement = ingest(&repo, &b"updated"[..]);
        lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), replacement.oid);
        gat_io::LockStore::publish_repository(
            repo.layout(),
            &lock,
            gat_core::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();

        let bytes_on_disk = std::fs::read(tmp.path().join("gat.lock")).unwrap();
        let expected_identity = gat_io::lock_hash_shard_bytes(&bytes_on_disk);
        let expected_entries = Lock::parse(&String::from_utf8_lossy(&bytes_on_disk))
            .unwrap()
            .entries;

        refresh(&repo, &mut store).unwrap();

        let (stored_identity, _) =
            gat_io::state_shard_observation_for_test(&store, LockShardId::flat())
                .unwrap()
                .unwrap();
        assert_eq!(stored_identity, expected_identity);
        let stored_lock = store.load_desired_as_lock().unwrap();
        let expected_lock = Lock {
            entries: expected_entries,
        };
        assert_eq!(paths_and_oids(&stored_lock), paths_and_oids(&expected_lock));
    }

    #[test]
    fn refresh_indexes_a_flat_lock_as_one_shard() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        let mut store = StateStore::open(repo.layout()).unwrap();

        let refreshed = refresh(&repo, &mut store).unwrap();

        let lock = store.load_desired_as_lock().unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "a.bin");
        assert_eq!(
            refreshed.shard_levels,
            gat_core::lock::LockShardLevels::FLAT
        );
    }

    #[test]
    fn pinned_refresh_keeps_the_comparison_on_one_desired_generation() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        let mut reader = StateStore::open(repo.layout()).unwrap();

        let refreshed = refresh_pinned(&repo, &mut reader).unwrap();
        assert_eq!(
            refreshed.shard_levels,
            gat_core::lock::LockShardLevels::FLAT
        );

        let mut writer = StateStore::open(repo.layout()).unwrap();
        writer
            .upsert_desired_for_test(
                &[gat_core::lock::Entry {
                    path: gat_core::lexical_path::GatPath::parse_canonical("later.bin").unwrap(),
                    oid: gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
                }],
                gat_core::lock::LockShardLevels::FLAT,
            )
            .unwrap();

        let lock = reader.load_desired_as_lock().unwrap();
        assert_eq!(lock.entries.len(), 1);
        assert_eq!(lock.entries[0].path, "a.bin");
    }

    #[test]
    fn refresh_is_a_no_op_on_a_second_call_with_no_changes() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        let mut store = StateStore::open(repo.layout()).unwrap();

        let refreshed = refresh(&repo, &mut store).unwrap();
        assert_eq!(
            refreshed.shard_levels,
            gat_core::lock::LockShardLevels::FLAT
        );
        let ids_before = store.all_shard_ids().unwrap();
        let refreshed = refresh(&repo, &mut store).unwrap();
        assert_eq!(
            refreshed.shard_levels,
            gat_core::lock::LockShardLevels::FLAT
        );
        let ids_after = store.all_shard_ids().unwrap();

        assert_eq!(ids_before, ids_after);
        assert_eq!(store.load_desired_as_lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn refresh_reparses_a_changed_shard_into_the_parallel_result() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        for i in 0..48 {
            let content = format!("payload-{i}");
            let ingested = ingest(&repo, content.as_bytes());
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i}.bin")).unwrap(),
                ingested.oid,
            );
        }
        gat_io::LockStore::publish_repository(
            repo.layout(),
            &lock,
            gat_core::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let mut store = StateStore::open(repo.layout()).unwrap();
        let refreshed = refresh(&repo, &mut store).unwrap();
        assert_eq!(
            refreshed.shard_levels,
            gat_core::lock::LockShardLevels::new(2).unwrap()
        );

        let replacement = ingest(&repo, &b"updated payload"[..]);
        lock.upsert(
            GatPath::parse_canonical("file-0.bin").unwrap(),
            replacement.oid,
        );
        gat_io::LockStore::publish_repository(
            repo.layout(),
            &lock,
            gat_core::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        refresh(&repo, &mut store).unwrap();

        let mut expected = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        expected.entries.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            paths_and_oids(&store.load_desired_as_lock().unwrap()),
            paths_and_oids(&expected)
        );
    }

    #[test]
    fn refresh_matches_single_threaded_and_default_rayon_pools() {
        let build_repo = || {
            let tmp = git_repo();
            let repo = Repo::at(tmp.path().to_path_buf());
            let mut lock = Lock::default();
            // 24 distinct shards is comfortably above typical core counts,
            // so the default rayon pool genuinely dispatches this across
            // multiple threads rather than degenerating into an accidental
            // single-threaded pass (this only needs to exceed likely
            // parallelism, not stress-test shard volume the way
            // `refresh_correctly_reconciles_many_shards_at_once` does).
            for i in 0..24 {
                let content = format!("payload-{i}");
                let ingested = ingest(&repo, content.as_bytes());
                lock.upsert(
                    GatPath::parse_canonical(&format!("dir/file-{i}.bin")).unwrap(),
                    ingested.oid,
                );
            }
            gat_io::LockStore::publish_repository(
                repo.layout(),
                &lock,
                gat_core::lock::LockShardLevels::new(2).unwrap(),
            )
            .unwrap();
            (tmp, repo, lock)
        };

        let (_tmp_a, repo_a, mut lock_a) = build_repo();
        let (_tmp_b, repo_b, mut lock_b) = build_repo();
        for (lock, repo) in [(&mut lock_a, &repo_a), (&mut lock_b, &repo_b)] {
            lock.entries
                .retain(|entry| !entry.path.as_str().ends_with("5.bin"));
            let ingested = ingest(repo, &b"replaced"[..]);
            lock.upsert(
                GatPath::parse_canonical("dir/file-0.bin").unwrap(),
                ingested.oid,
            );
            let ingested = ingest(repo, &b"another replacement"[..]);
            lock.upsert(
                GatPath::parse_canonical("dir/extra.bin").unwrap(),
                ingested.oid,
            );
            gat_io::LockStore::publish_repository(
                repo.layout(),
                lock,
                gat_core::lock::LockShardLevels::new(2).unwrap(),
            )
            .unwrap();
        }

        let mut default_store = StateStore::open(repo_a.layout()).unwrap();
        refresh(&repo_a, &mut default_store).unwrap();
        let mut single_store = StateStore::open(repo_b.layout()).unwrap();
        refresh_with_threads(&repo_b, &mut single_store, 1).unwrap();

        assert_eq!(
            default_store.load_desired_as_lock().unwrap().entries,
            single_store.load_desired_as_lock().unwrap().entries
        );
    }

    /// A truly clean second refresh must never even open a
    /// write transaction: exercised here by holding an unrelated
    /// exclusive write lock on the database from a second connection and
    /// verifying `refresh` still succeeds -- it could only do so by never
    /// trying to write itself, since a genuine write would time out
    /// against the held lock.
    #[test]
    fn a_truly_unchanged_refresh_never_opens_a_write_transaction() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();

        let _blocker =
            gat_io::state_test_support::block_writes(&tmp.path().join(".gat/state/state.sqlite3"));

        refresh(&repo, &mut store).unwrap();
    }

    /// The write transaction
    /// test above proves a warm refresh performs no *write*, but a
    /// stat-only proof hit must also perform no *read* of the shard's
    /// bytes at all -- reads a stale gat.lock permission structurally
    /// the same way `warm_validated_sync_does_not_read_gat_lock_content`
    /// (in `engine::workspace::sync::tests`) does for a full sync, but scoped to
    /// `desired_index::refresh()` in isolation.
    #[test]
    #[cfg(unix)]
    fn a_truly_unchanged_refresh_never_reads_the_shard_bytes() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();

        let lock_path = tmp.path().join("gat.lock");
        let original_mode = std::fs::metadata(&lock_path).unwrap().permissions().mode();
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        struct RestorePerms(std::path::PathBuf, u32);
        impl Drop for RestorePerms {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
            }
        }
        let _restore = RestorePerms(lock_path, original_mode);

        // A stat-only proof hit must succeed even though the shard's
        // bytes are now unreadable, because it never opens the file.
        refresh(&repo, &mut store).unwrap();
    }

    #[test]
    fn refresh_detects_a_removed_shard() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();
        assert_eq!(store.load_desired_as_lock().unwrap().entries.len(), 1);

        std::fs::remove_file(tmp.path().join("gat.lock")).unwrap();
        refresh(&repo, &mut store).unwrap();

        assert!(store.load_desired_as_lock().unwrap().entries.is_empty());
        assert!(store.all_shard_ids().unwrap().is_empty());
    }

    /// `refresh` is the main live desired-state read path every
    /// normal command uses (`status`'s store refresh, and `ls-files`,
    /// which can stay on the refreshed-store path entirely) -- so it must
    /// detect a pending `lock.shard_levels` reshape exactly like
    /// `LockStore::load_all` does, not treat the crash window between
    /// a reshape's two commit renames as "nothing tracked". Per design,
    /// it must not repair that state itself either: simulates a crash
    /// right after the first rename (live -> backup, live path now
    /// missing) and asserts a refresh through this path fails closed with
    /// a diagnostic, leaving every on-disk byte untouched for an explicit
    /// recovery command, rather than either reporting an empty desired
    /// state or silently restoring the pre-reshape entries itself.
    #[test]
    fn refresh_fails_closed_on_a_pending_reshape_simulated_after_the_first_commit_rename() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        track(&repo, "b.bin", b"world");
        let lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();

        gat_io::simulate_lock_crash_after_first_rename(
            tmp.path(),
            &lock,
            gat_core::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        assert!(
            !tmp.path().join("gat.lock").exists(),
            "simulated crash must leave the live path missing, exactly the window under test"
        );

        let mut store = StateStore::open(repo.layout()).unwrap();
        let err = refresh(&repo, &mut store).unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::Lock(LockFailureKind::Corrupt),
            "a refresh landing in the crash window must fail closed with a diagnostic \
             instead of reporting an empty desired state"
        );
        assert!(
            !tmp.path().join("gat.lock").exists(),
            "a fail-closed refresh must never restore the live path itself"
        );
    }

    #[test]
    fn refresh_marks_new_desired_paths_dirty_against_empty_materialized_state() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        let mut store = StateStore::open(repo.layout()).unwrap();

        refresh(&repo, &mut store).unwrap();

        assert!(store.has_dirty().unwrap());
        let rows = store.dirty_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "a.bin");
        assert!(rows[0].desired.is_some());
        assert!(rows[0].materialized.is_none());
    }

    /// A successful flat `gat.lock` publication must be reflected in the
    /// mirror right away, from the rendered bytes already in memory, not
    /// left for the next [`refresh`] to rediscover.
    #[test]
    fn publish_desired_lock_populates_the_mirror_from_a_freshly_written_lock() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(&repo, &b"hello"[..]);
        lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
        let mut store = StateStore::open(repo.layout()).unwrap();
        store
            .publish_desired_complete::<SyncError>(
                repo.layout(),
                &lock,
                repo.lock_shard_levels().unwrap(),
            )
            .unwrap();

        let mirrored = store.load_desired_as_lock().unwrap();
        assert_eq!(paths_and_oids(&mirrored), paths_and_oids(&lock));
    }

    /// A subsequent [`refresh`] must trust the identity
    /// [`StateStore::publish_desired_complete`] just recorded
    /// rather than reparsing the shard's entries again: this is
    /// exercised by holding an exclusive write lock on the database (the
    /// same technique
    /// `a_truly_unchanged_refresh_never_opens_a_write_transaction` uses)
    /// and asserting `refresh` still succeeds.
    #[test]
    fn refresh_after_seeding_does_not_need_to_rewrite_state() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(&repo, &b"hello"[..]);
        lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
        let mut store = StateStore::open(repo.layout()).unwrap();
        store
            .publish_desired_complete::<SyncError>(
                repo.layout(),
                &lock,
                repo.lock_shard_levels().unwrap(),
            )
            .unwrap();

        let _blocker =
            gat_io::state_test_support::block_writes(&tmp.path().join(".gat/state/state.sqlite3"));

        refresh(&repo, &mut store).unwrap();
    }

    /// A shard
    /// rewrite that lands *between* the pre-read stat taken in
    /// `shard_change` and the actual content read must never let
    /// `refresh()` commit a proof paired with bytes observed during that
    /// interval -- the mismatch must fail `refresh()` closed immediately,
    /// with no automatic retry. Forces the race deterministically via
    /// [`race_test_hooks`] (real thread-timing races would make this
    /// test flaky) instead of relying on a second thread.
    #[test]
    fn refresh_fails_closed_when_a_shard_is_rewritten_mid_read() {
        let _guard = RaceHookGuard;
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(&repo, &b"before"[..]);
        lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();

        // Precompute a still-valid, but different-length, rewrite of the
        // shard (an extra tracked path) so the hook below only has to
        // write already-ingested bytes, not race real ingestion against
        // the read itself.
        let mut rewritten = lock.clone();
        let ingested2 = ingest(&repo, &b"after-rewrite-payload"[..]);
        rewritten.upsert(GatPath::parse_canonical("b.bin").unwrap(), ingested2.oid);
        rewritten.entries.sort_by(|a, b| a.path.cmp(&b.path));
        let rewritten_bytes = rewritten.to_string();

        // Rewrite the shard the instant before the coherent-observation
        // read runs, so the pre-read and post-read stats are provably
        // different -- this is the exact race the coherent-observation
        // primitive exists to detect.
        //
        // `race_test_hooks` is process-wide, so
        // under a parallel test run it also fires for every *other*
        // test's concurrently-running `shard_change()` calls, not just
        // this test's own. Without this path guard, the hook would
        // clobber an unrelated test's shard file with these rewritten
        // bytes any time the two tests' reads happened to overlap --
        // exactly the kind of nondeterministic cross-test corruption
        // this primitive exists to make deterministic, not introduce.
        // Gating on the exact shard path this test cares about keeps the
        // injected race scoped to this test's own fixture.
        let target_path = tmp.path().join("gat.lock");
        race_test_hooks::set(move |path: &Path| {
            if path == target_path {
                std::fs::write(path, rewritten_bytes.as_bytes()).unwrap();
            }
        });

        let mut store = StateStore::open(repo.layout()).unwrap();
        let result = refresh(&repo, &mut store);
        race_test_hooks::clear();
        assert!(
            result.is_err(),
            "a shard observed through a changed pre-read/post-read stat pair must fail \
             refresh() closed rather than commit a proof-less row for it"
        );

        // Prove the failure really did leave nothing committed: a clean
        // refresh (no race) afterward still succeeds and produces the
        // shard's original identity.
        refresh(&repo, &mut store).unwrap();
        let bytes_on_disk = std::fs::read(tmp.path().join("gat.lock")).unwrap();
        let expected_identity = gat_io::lock_hash_shard_bytes(&bytes_on_disk);
        let (stored_identity, _) =
            gat_io::state_shard_observation_for_test(&store, LockShardId::flat())
                .unwrap()
                .unwrap();
        assert_eq!(stored_identity, expected_identity);
    }

    /// A brand-new
    /// `gat.lock`, exactly mirroring a shard that just landed via a real
    /// `git clone`/`checkout`/`git worktree add`, must be read, hashed,
    /// parsed, assigned a reusable proof, and reflected by `refresh()`
    /// immediately in one call -- no wall-clock wait, retry, or second
    /// call is needed under the coherent-observation model.
    #[test]
    fn refresh_establishes_a_brand_new_shards_identity_immediately() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(&repo, &b"fresh-checkout"[..]);
        lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();

        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();

        let (_, has_proof) = gat_io::state_shard_observation_for_test(&store, LockShardId::flat())
            .unwrap()
            .unwrap();
        assert!(
            has_proof,
            "a single refresh() call on a brand-new shard must mint a reusable proof \
             immediately, with no wait or retry"
        );
        assert_eq!(
            store.load_desired_as_lock().unwrap().entries.len(),
            1,
            "the shard's row must be materialized in the same call"
        );
    }

    #[test]
    fn publish_desired_lock_populates_the_mirror_for_a_sharded_lock() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(&repo, &b"hello"[..]);
        lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
        gat_io::LockStore::publish_repository(
            repo.layout(),
            &lock,
            gat_core::lock::LockShardLevels::new(1).unwrap(),
        )
        .unwrap();

        let mut store = StateStore::open(repo.layout()).unwrap();
        store
            .publish_desired_complete::<SyncError>(
                repo.layout(),
                &lock,
                gat_core::lock::LockShardLevels::new(1).unwrap(),
            )
            .unwrap();

        let mirrored = store.load_desired_as_lock().unwrap();
        assert_eq!(paths_and_oids(&mirrored), paths_and_oids(&lock));
    }

    #[test]
    fn resharding_with_identical_content_produces_no_dirty_rows() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        for i in 0..20 {
            track(&repo, &format!("f{i}.bin"), format!("c{i}").as_bytes());
        }
        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();

        // Materialize desired == materialized by copying the rows over,
        // simulating a completed sync, then reshard the lock file.
        let entries = store.load_desired_as_lock().unwrap().entries;
        store.upsert_many(&entries).unwrap();
        assert!(!store.has_dirty().unwrap());

        let lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        gat_io::LockStore::publish_repository(
            repo.layout(),
            &lock,
            gat_core::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        refresh(&repo, &mut store).unwrap();

        assert!(!store.has_dirty().unwrap());
        assert_eq!(store.load_desired_as_lock().unwrap().entries.len(), 20);
    }

    /// Every lock shape is 0..N *independently* parsed shards (module
    /// docs); the incremental mirror must still fail closed on a path
    /// tracked by two different shards, the same global invariant a
    /// complete `gat_io::LockStore::load_repository()` enforces -- not silently let whichever
    /// shard is applied last win (see
    /// `check_sharded_lock_invariants`).
    #[test]
    fn refresh_fails_closed_when_two_shards_in_the_same_refresh_track_the_same_path() {
        // Models a reshape overlap: an old level-1 shard and a new
        // level-2 shard coexist on disk -- a mixed-depth completed tree,
        // which is now rejected as a topology error before refresh ever
        // gets to indexing individual rows (see
        // `gat_core::lock::shard_topology`).
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("48.tsv"),
            format!(
                "{}\n\"shared.bin\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "a".repeat(64)
            ),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("48")).unwrap();
        std::fs::write(
            dir.join("48").join("dd.tsv"),
            format!(
                "{}\n\"shared.bin\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "b".repeat(64)
            ),
        )
        .unwrap();
        let mut store = StateStore::open(repo.layout()).unwrap();

        let err = refresh(&repo, &mut store).unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::Lock(LockFailureKind::Corrupt),
            "expected a mixed-shard-topology classification"
        );
        // Nothing should have been committed -- the conflicting shards'
        // desired rows are never partially applied.
        assert!(store.load_desired_as_lock().unwrap().entries.is_empty());
    }

    /// Same invariant, but the conflict spans refresh cycles: one shard
    /// is already indexed from a prior refresh, and a second shard --
    /// the only one that changed this cycle -- now brings a different
    /// fan-out depth onto disk alongside it. The incremental mirror must
    /// still catch this even though the first shard's content identity
    /// didn't change and so wouldn't otherwise be touched by this
    /// refresh.
    #[test]
    fn refresh_fails_closed_when_a_changed_shard_claims_a_path_owned_by_an_unrelated_shard() {
        // Same reshape-overlap shape as above, but spanning refresh
        // cycles: the level-1 shard is already indexed and unchanged
        // this cycle, while a level-2 shard only appears on disk on the
        // *second* refresh -- still a mixed-depth completed tree.
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("d5.tsv"),
            format!(
                "{}\n\"existing.bin\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "a".repeat(64)
            ),
        )
        .unwrap();
        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();
        assert_eq!(store.load_desired_as_lock().unwrap().entries.len(), 1);

        std::fs::create_dir_all(dir.join("d5")).unwrap();
        std::fs::write(
            dir.join("d5").join("69.tsv"),
            format!(
                "{}\n\"existing.bin\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "b".repeat(64)
            ),
        )
        .unwrap();

        let err = refresh(&repo, &mut store).unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::Lock(LockFailureKind::Corrupt),
            "expected a mixed-shard-topology classification"
        );
    }

    #[test]
    fn refresh_fails_closed_when_a_row_is_stored_in_the_wrong_shard() {
        // "existing.bin" hashes to shard "d5.tsv" at level 1, not "00.tsv"
        // -- refresh must reject a row that isn't stored in the shard
        // file `shard_id_for_path` predicts for it, the same placement
        // invariant `LockStore::load_all` enforces on a full load.
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("00.tsv"),
            format!(
                "{}\n\"existing.bin\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "a".repeat(64)
            ),
        )
        .unwrap();
        let mut store = StateStore::open(repo.layout()).unwrap();

        let err = refresh(&repo, &mut store).unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::Lock(LockFailureKind::Corrupt),
            "expected a shard-placement classification"
        );
        assert!(store.load_desired_as_lock().unwrap().entries.is_empty());
    }

    /// Same directory-prefix invariant `LockStore::load_all` enforces
    /// over its complete in-memory merge (a path can't be tracked
    /// directly *and* as a directory prefix of another tracked path),
    /// but spread across two independently, correctly-placed shards --
    /// refresh must reject it without ever reparsing every other shard
    /// to notice.
    #[test]
    fn refresh_fails_closed_on_a_cross_shard_directory_prefix_conflict() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("04.tsv"),
            format!(
                "{}\n\"foo\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "a".repeat(64)
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("2d.tsv"),
            format!(
                "{}\n\"foo/bar\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "b".repeat(64)
            ),
        )
        .unwrap();
        let mut store = StateStore::open(repo.layout()).unwrap();

        let err = refresh(&repo, &mut store).unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::Lock(LockFailureKind::Corrupt),
            "expected a directory-prefix conflict classification"
        );
        assert!(store.load_desired_as_lock().unwrap().entries.is_empty());
    }

    /// The directory-prefix half of the invariant above, but spanning
    /// refresh cycles like `refresh_fails_closed_when_a_changed_shard_claims_a_path_owned_by_an_unrelated_shard`:
    /// the ancestor path is already indexed and unchanged this cycle,
    /// and only the descendant's shard changes.
    #[test]
    fn refresh_fails_closed_when_a_changed_shard_nests_under_an_unrelated_shards_path() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("04.tsv"),
            format!(
                "{}\n\"foo\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "a".repeat(64)
            ),
        )
        .unwrap();
        std::fs::write(dir.join("2d.tsv"), format!("{}\n", gat_core::lock::VERSION)).unwrap();
        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();
        assert_eq!(store.load_desired_as_lock().unwrap().entries.len(), 1);

        std::fs::write(
            dir.join("2d.tsv"),
            format!(
                "{}\n\"foo/bar\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "b".repeat(64)
            ),
        )
        .unwrap();

        let err = refresh(&repo, &mut store).unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::Lock(LockFailureKind::Corrupt),
            "expected a directory-prefix conflict classification"
        );
    }

    /// `find_directory_conflict`'s descendant query must not stop at the
    /// lexically-*first* descendant row and give up once that one
    /// happens to belong to an excluded (changed/removed) shard -- a
    /// later descendant owned by a different, unchanged shard can still
    /// be a real conflict. Here `foo/0100` (lexically first, owned by
    /// the shard that is about to start owning `foo` itself, so it's
    /// excluded this cycle) sorts before `foo/0101` (owned by an
    /// unrelated, unchanged shard) -- exclusion must be applied before
    /// picking the first candidate, not after.
    #[test]
    fn refresh_finds_a_directory_conflict_past_an_excluded_shards_lexically_first_descendant() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        // `shard_id_for_path("foo", LockShardLevels::new(1).unwrap())` and `shard_id_for_path("foo/0100", LockShardLevels::new(1).unwrap())`
        // are both `gat.lock/04.tsv`; `shard_id_for_path("foo/0101", LockShardLevels::new(1).unwrap())` is
        // `gat.lock/88.tsv`.
        std::fs::write(
            dir.join("04.tsv"),
            format!(
                "{}\n\"foo/0100\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "a".repeat(64)
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("88.tsv"),
            format!(
                "{}\n\"foo/0101\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "b".repeat(64)
            ),
        )
        .unwrap();
        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();
        assert_eq!(store.load_desired_as_lock().unwrap().entries.len(), 2);

        // Shard `04` (owning `foo/0100`, lexically first among
        // `foo`'s descendants) changes to own `foo` itself, conflicting
        // with `foo/0101`, still owned by unchanged shard `88`.
        std::fs::write(
            dir.join("04.tsv"),
            format!(
                "{}\n\"foo\"\tblake3:{}\n",
                gat_core::lock::VERSION,
                "c".repeat(64)
            ),
        )
        .unwrap();

        let err = refresh(&repo, &mut store).unwrap_err();
        assert_eq!(
            err.kind(),
            &SyncErrorKind::Lock(LockFailureKind::Corrupt),
            "expected a directory-prefix conflict classification"
        );
    }

    #[test]
    fn refresh_correctly_reconciles_many_shards_at_once() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        // 24 initial shards, 12 replaced, and 12 new -- still "many"
        // shards of each kind (added/changed/removed) reconciled in one
        // `refresh()` call
        for i in 0..24 {
            let content = format!("payload-{i}");
            let ingested = ingest(&repo, content.as_bytes());
            lock.upsert(
                GatPath::parse_canonical(&format!("nested/file-{i}.bin")).unwrap(),
                ingested.oid,
            );
        }
        gat_io::LockStore::publish_repository(
            repo.layout(),
            &lock,
            gat_core::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let mut store = StateStore::open(repo.layout()).unwrap();
        refresh(&repo, &mut store).unwrap();

        lock.entries
            .retain(|entry| !entry.path.as_str().ends_with("0.bin"));
        for i in 0..12 {
            let content = format!("replacement-{i}");
            let ingested = ingest(&repo, content.as_bytes());
            lock.upsert(
                GatPath::parse_canonical(&format!("nested/file-{}.bin", i * 3 + 1)).unwrap(),
                ingested.oid,
            );
        }
        for i in 24..36 {
            let content = format!("new-{i}");
            let ingested = ingest(&repo, content.as_bytes());
            lock.upsert(
                GatPath::parse_canonical(&format!("nested/file-{i}.bin")).unwrap(),
                ingested.oid,
            );
        }
        gat_io::LockStore::publish_repository(
            repo.layout(),
            &lock,
            gat_core::lock::LockShardLevels::new(1).unwrap(),
        )
        .unwrap();
        refresh(&repo, &mut store).unwrap();

        let mut expected = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        expected.entries.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            paths_and_oids(&store.load_desired_as_lock().unwrap()),
            paths_and_oids(&expected)
        );
    }

    /// Deterministic regression for the shared reconciliation-boundary
    /// invariant (see [`refresh`]'s doc comment): `refresh` must actually
    /// block on the real OS-level `flock`/`LockFileEx` underneath
    /// [`RepoLock`] for as long as a concurrent reshape/publish holds it,
    /// rather than racing straight through to read shard files that a
    /// reshape might be replacing mid-enumeration. Uses real channel
    /// handoffs throughout, not timing: a "holder" thread acquires the
    /// identical `RepoLock` `refresh` itself now takes and parks on it
    /// while a "refresher" thread's call is in flight. Rather than
    /// guessing with a sleep, the test installs
    /// [`gat_io::atomic_test_support`]'s acquire-attempt hook, scoped to
    /// the refresher's own thread id, before the holder is even spawned;
    /// `RepoLock::acquire` fires it right before its first (possibly
    /// blocking) OS-lock attempt, so receiving on that channel is proof
    /// `refresh` has actually reached the lock boundary and -- because the
    /// holder still holds the lock at that point -- could not have
    /// proceeded past it, before the test asserts it hasn't finished and
    /// only then releases the holder.
    #[test]
    fn refresh_serializes_behind_a_concurrent_repo_lock_holder() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");

        let (holder_acquired_tx, holder_acquired_rx) = std::sync::mpsc::channel::<()>();
        let (release_holder_tx, release_holder_rx) = std::sync::mpsc::channel::<()>();
        let (refresh_done_tx, refresh_done_rx) = std::sync::mpsc::channel::<()>();
        let (acquire_attempted_tx, acquire_attempted_rx) = std::sync::mpsc::channel::<()>();

        let repo_ref = &repo;
        std::thread::scope(|s| {
            // Spawned (and thus assigned a `ThreadId`) before the holder,
            // so the acquire-attempt hook below is guaranteed to be
            // installed before the holder can even signal it has the
            // lock -- the refresher itself stays parked on
            // `holder_acquired_rx` until then.
            let refresher = s.spawn(move || {
                holder_acquired_rx.recv().unwrap();
                let mut store = StateStore::open(repo_ref.layout()).unwrap();
                refresh(repo_ref, &mut store).unwrap();
                refresh_done_tx.send(()).unwrap();
            });

            gat_io::atomic_test_support::with_acquire_attempt_hook(
                refresher.thread().id(),
                acquire_attempted_tx,
                || {
                    let holder = s.spawn(move || {
                        let guard = RepoLock::acquire_repository(repo_ref.layout()).unwrap();
                        holder_acquired_tx.send(()).unwrap();
                        release_holder_rx.recv().unwrap();
                        drop(guard);
                    });

                    acquire_attempted_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect(
                            "refresh() must reach RepoLock::acquire's OS-lock boundary \
                             while the holder still holds the lock",
                        );

                    assert!(
                        refresh_done_rx.try_recv().is_err(),
                        "refresh() must still be blocked on the held RepoLock"
                    );

                    release_holder_tx.send(()).unwrap();
                    refresh_done_rx.recv().unwrap();
                    holder.join().unwrap();
                },
            );

            refresher.join().unwrap();
        });
    }
}
