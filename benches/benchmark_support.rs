//! Reusable benchmark fixtures.
//!
//! This module intentionally contains no benchmark scenarios. Import it from a
//! temporary Criterion target with `mod benchmark_support;` and compose only
//! the state required by the operation being measured.
#![allow(dead_code)]

use gat_core::lexical_path::GatPath;
use gat_core::lock::{Entry, Lock, LockShardLevels};
use gat_core::oid::Oid;
use std::path::{Path, PathBuf};

/// A hermetic repository fixture suitable for benchmark setup.
///
/// The owned [`test_support::TestRepo`] keeps all Git/Gat state below a
/// temporary directory. The engine repository is cached alongside it so
/// benchmark bodies do not repeatedly reconstruct that facade unless that is
/// explicitly what they intend to measure.
pub struct RepoFixture {
    repo: test_support::TestRepo,
    engine: gat_engine::Repository,
}

impl RepoFixture {
    /// A Git repository with no Gat initialization yet.
    #[must_use]
    pub fn git() -> Self {
        Self::from_test_repo(test_support::TestRepo::empty_git_repo())
    }

    /// A Git repository with one initial commit.
    #[must_use]
    pub fn git_with_initial_commit() -> Self {
        Self::from_test_repo(test_support::TestRepo::git_repo_with_initial_commit())
    }

    /// A Git repository initialized through Gat's authoritative init path.
    #[must_use]
    pub fn gat() -> Self {
        Self::from_test_repo(test_support::TestRepo::empty_gat_repo())
    }

    /// A Gat-initialized repository with a resolvable `HEAD`.
    #[must_use]
    pub fn gat_with_initial_commit() -> Self {
        Self::from_test_repo(test_support::TestRepo::gat_repo())
    }

    fn from_test_repo(repo: test_support::TestRepo) -> Self {
        let engine = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(repo.path().to_path_buf());
        Self { repo, engine }
    }

    /// Repository root owned by this fixture.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.repo.path()
    }

    /// The production engine facade for this fixture.
    #[must_use]
    pub const fn repository(&self) -> &gat_engine::Repository {
        &self.engine
    }

    /// The cache directory resolved by Gat initialization.
    ///
    /// Panics for fixtures created with [`Self::git`] or
    /// [`Self::git_with_initial_commit`], which have not initialized Gat.
    #[must_use]
    pub fn objects_dir(&self) -> PathBuf {
        PathBuf::from(self.repo.cache_dir())
    }

    /// Writes a worktree file, creating parent directories as needed.
    pub fn write(&self, relative_path: &str, contents: impl AsRef<[u8]>) {
        self.repo.write(relative_path, contents);
    }

    /// Reads the desired-state lock through the physical storage capability.
    ///
    /// # Panics
    ///
    /// Panics if the benchmark fixture's desired-state lock cannot be loaded.
    #[must_use]
    pub fn load_lock(&self) -> Lock {
        gat_io::LockStore::load_repository(&gat_io::RepositoryLayout::at(self.root().to_path_buf()))
            .expect("load benchmark fixture gat.lock")
    }

    /// Persists the desired-state lock with the requested shard layout.
    ///
    /// # Panics
    ///
    /// Panics if the benchmark fixture's desired-state lock cannot be persisted.
    pub fn save_lock(&self, lock: &Lock, levels: LockShardLevels) {
        gat_io::LockStore::publish_repository(
            &gat_io::RepositoryLayout::at(self.root().to_path_buf()),
            lock,
            levels,
        )
        .expect("save benchmark fixture gat.lock");
    }

    /// Persists exactly `entries` as desired state.
    pub fn seed_desired(&self, entries: &[Entry], levels: LockShardLevels) {
        self.save_lock(
            &Lock {
                entries: entries.to_vec(),
            },
            levels,
        );
    }

    /// Records materialized state directly, outside the timed operation.
    ///
    /// # Panics
    ///
    /// Panics if materialized state cannot be recorded for the benchmark fixture.
    pub fn seed_materialized(&self, entries: &[Entry]) {
        gat_engine::test_support::record_materialized_for_test(self.repository(), entries)
            .expect("record benchmark fixture materialized state");
    }

    /// Seeds matching desired and materialized state for clean/no-op workloads.
    pub fn seed_clean_state(&self, entries: &[Entry], levels: LockShardLevels) {
        self.seed_desired(entries, levels);
        self.seed_materialized(entries);
    }

    /// Ingests bytes into this fixture's cache and returns the production OID.
    ///
    /// # Panics
    ///
    /// Panics if the bytes cannot be ingested into the benchmark fixture's cache.
    #[must_use]
    pub fn ingest(&self, contents: &[u8]) -> Oid {
        gat_io::cache_benchmark_support::ingest(&self.objects_dir(), contents)
            .expect("ingest benchmark fixture object")
            .oid
    }

    /// Writes a real worktree file and matching cache object.
    ///
    /// # Panics
    ///
    /// Panics if the worktree file or cache object cannot be written.
    #[must_use]
    pub fn write_ingested(&self, path: GatPath, contents: &[u8]) -> Entry {
        self.write(path.as_str(), contents);
        let oid = self.ingest(contents);
        Entry { path, oid }
    }
}

/// A filesystem-backed remote fixture owned by a temporary directory.
pub struct FileRemote {
    dir: tempfile::TempDir,
}

impl FileRemote {
    /// Creates a temporary filesystem-backed remote.
    ///
    /// # Panics
    ///
    /// Panics if a temporary directory cannot be created.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("create benchmark file remote"),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    #[must_use]
    pub fn url(&self) -> String {
        test_support::file_remote_url(self.path())
    }
}

impl Default for FileRemote {
    fn default() -> Self {
        Self::new()
    }
}

/// Deterministic nested path useful for full and scoped-prefix scans.
///
/// # Panics
///
/// Panics only if the deterministic path unexpectedly fails canonical parsing.
#[must_use]
pub fn synthetic_path(i: u64) -> GatPath {
    GatPath::parse_canonical(&format!(
        "d0-{:04}/d1-{:03}/d2-{:02}/file-{:06}.bin",
        i / 10_000,
        (i / 100) % 100,
        (i / 10) % 10,
        i
    ))
    .expect("synthetic benchmark path is canonical")
}

/// Deterministic, distinct placeholder OID.
///
/// Use this only when the measured code does not require a real cache object
/// or worktree file behind the row.
#[must_use]
pub fn synthetic_oid(i: u64) -> Oid {
    let mut rng = Splitmix64::new(i ^ 0xD1CE_5EED);
    let mut bytes = [0; 32];
    for chunk in bytes.as_chunks_mut::<8>().0 {
        chunk.copy_from_slice(&rng.next_u64().to_le_bytes());
    }
    Oid::from_bytes(bytes)
}

/// One deterministic content-less desired-state entry.
#[must_use]
pub fn synthetic_entry(i: u64) -> Entry {
    Entry {
        path: synthetic_path(i),
        oid: synthetic_oid(i),
    }
}

/// `count` deterministic content-less desired-state entries.
pub fn synthetic_entries(count: u64) -> Vec<Entry> {
    (0..count).map(synthetic_entry).collect()
}

/// Deterministic bytes suitable for real cache/worktree fixtures.
#[must_use]
pub fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Splitmix64::new(seed ^ 0xC0FF_EE00);
    let mut out = vec![0; len];
    for chunk in out.chunks_mut(8) {
        let bytes = rng.next_u64().to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    out
}

struct Splitmix64(u64);

impl Splitmix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

// This harness-free target keeps the helper API covered by all-target checks.
fn main() {}
