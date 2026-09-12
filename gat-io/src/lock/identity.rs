//! Canonical desired-state identity with sparse-update-friendly aggregate
//! maintenance and a stat-first, content-hash identity path.
//!
//! Git-visible `gat.lock` state is identified consistently through one
//! per-shard content identity
//! ([`ShardContentIdentity`], always `BLAKE3(raw shard file bytes)`, with
//! no Git-index tier and no Git object-ID representation involved) and
//! one whole-lock identity algorithm -- a domain-separated **XOR set
//! accumulator** ([`CanonicalDesiredIdentity`]) rather than an ordered
//! fold, so a sparse k-shard mutation updates it in O(k) time (via
//! [`CanonicalDesiredIdentity::toggle_shard`]) while a full recomputation
//! is O(Q) metadata work over the live shard set, embarrassingly parallel
//! (see `current_desired_identity_with_prior`'s Rayon map/reduce) with
//! content hashing needed only for cache misses.
//!
//! Deliberately placed under `gat-io`'s lock owner so both state refresh
//! and current-revision observation share this physical protocol without
//! exposing it to engine or duplicating it above the I/O boundary.
//!
//! [`ShardContentIdentity`], [`CanonicalDesiredIdentity`],
//! their pure BLAKE3 hashing/XOR combination algebra, and the shared lock
//! format version constant now live in `gat_core::lock::identity` (and
//! are re-exported here) since they have zero I/O dependency at all --
//! only the stat-first/coherent-read observation protocol (this module's
//! remaining contents) requires a live filesystem.

use crate::file_state::{StatProof, coherent_observation, observe_regular_file_no_follow};
use crate::lock::LockError;
use crate::lock::LockShardId;
use gat_core::lock::{CanonicalDesiredIdentity, ShardContentIdentity};
use std::path::Path;

/// This module's own `Result` alias, matching [`super::Result`] -- every
/// fallible function here returns a typed [`LockError`], not
/// `anyhow::Result`.
type Result<T> = super::Result<T>;

/// Directly delegates to [`coherent_observation`], generic over this
/// module's own typed [`LockError`] (via `LockError`'s `#[from]
/// FileStateError`) so `op`'s failure and `coherent_observation`'s own
/// stat-race detection both stay fully typed end to end -- no `anyhow`
/// round-trip, no downcast recovery.
fn coherent_read_bytes(
    path: &Path,
    op: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<crate::file_state::CoherentObservation<Vec<u8>>> {
    coherent_observation(path, op)
}

/// Hash `bytes` with BLAKE3 -- the one, only shard content identity
/// computation in the codebase: every caller that
/// needs a shard's canonical identity, whether during publication
/// (`save_sparse_shards`/`publish_flat_shard`), refresh
/// (state reconciliation), or full live observation
/// (`current_desired_identity_with_prior`), calls this exact function
/// over the exact bytes it read/rendered, so identical shard bytes always
/// produce the identical [`ShardContentIdentity`] no matter which of
/// those paths computed it. Delegates to the pure core
/// [`ShardContentIdentity::hash`]; this wrapper exists only to keep the
/// test-only call-count instrumentation below.
#[must_use]
pub fn hash_shard_bytes(bytes: &[u8]) -> ShardContentIdentity {
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_hash_shard_bytes_call();
    ShardContentIdentity::hash(bytes)
}

/// Test-only instrumentation proving a proof-agnostic caller
/// (e.g. [`crate::LockStore::publish_repository`]) never computes
/// a [`ShardContentIdentity`] it would only discard. Thread-local because
/// `cargo test` runs tests concurrently on separate threads.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static HASH_SHARD_BYTES_CALLS: Cell<usize> = const { Cell::new(0) };
        static CURRENT_IDENTITY_CONTENT_READ_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn record_hash_shard_bytes_call() {
        HASH_SHARD_BYTES_CALLS.with(|c| c.set(c.get() + 1));
    }

    pub fn hash_shard_bytes_call_count() -> usize {
        HASH_SHARD_BYTES_CALLS.with(Cell::get)
    }

    pub fn record_current_identity_content_read_call() {
        CURRENT_IDENTITY_CONTENT_READ_CALLS.with(|c| c.set(c.get() + 1));
    }

    pub fn current_identity_content_read_call_count() -> usize {
        CURRENT_IDENTITY_CONTENT_READ_CALLS.with(Cell::get)
    }
}

/// The result of [`resolve_shard_identity`] resolving one shard file's
/// current canonical content identity against an optional prior
/// `(identity, proof)` record.
#[derive(Debug)]
pub enum ShardIdentityResolution {
    /// The prior proof exactly matched the shard's current stat: the
    /// prior identity is reused with no content read at all.
    StatOnly {
        identity: ShardContentIdentity,
        proof: StatProof,
    },
    /// A coherent content read (see
    /// [`crate::file_state::coherent_observation`]): `identity` may equal
    /// the prior one (nothing to reparse) or be new/changed. Carries the
    /// exact bytes read so a caller that needs parsed rows for a new/
    /// changed identity can derive them from this same buffer, never a
    /// second read.
    Coherent {
        identity: ShardContentIdentity,
        proof: StatProof,
        bytes: Vec<u8>,
    },
}

/// Resolve one shard file's current canonical [`ShardContentIdentity`]
/// against an optional `(identity, proof)` prior record, using the one
/// shared tiered stat-first -> coherent-observation -> BLAKE3 policy
/// every domain that establishes desired-shard identity must share:
/// state reconciliation and `current_desired_identity_with_prior` route
/// through this exact function instead of maintaining independent
/// stat/read/hash models.
///
/// - **Tier 1** (stat-only): if `prior` carries a proof that exactly
///   matches the shard's current stat, the prior identity is reused with
///   no content read ([`ShardIdentityResolution::StatOnly`]).
/// - **Tier 2** (content read): otherwise, `read` runs exactly once,
///   wrapped in [`crate::file_state::coherent_observation`], and the
///   resulting bytes are hashed exactly once
///   ([`ShardIdentityResolution::Coherent`]) -- whether or not the
///   identity changed from `prior`'s. An observed pre/post mutation
///   during the read fails this call outright rather than returning any
///   resolution.
///
/// Deliberately takes `prior` as a plain `(ShardContentIdentity,
/// Option<StatProof>)` pair rather than a `worktree`/`runtime`-specific
/// catalog-row type, so both call sites can build it from whatever their
/// own prior-catalog representation happens to be without this
/// dependency-neutral module depending on either.
pub fn resolve_shard_identity(
    full_path: &Path,
    prior: Option<(ShardContentIdentity, Option<StatProof>)>,
    read: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<ShardIdentityResolution> {
    if let Some((prior_identity, Some(prior_proof))) = prior
        && let Some(current) = observe_regular_file_no_follow(full_path)
        && current.matches(&prior_proof)
    {
        return Ok(ShardIdentityResolution::StatOnly {
            identity: prior_identity,
            proof: prior_proof,
        });
    }

    let observation = coherent_read_bytes(full_path, read)?;
    let identity = hash_shard_bytes(&observation.value);
    Ok(ShardIdentityResolution::Coherent {
        identity,
        proof: observation.proof,
        bytes: observation.value,
    })
}

/// Test-only: the sequential, reference-implementation XOR fold used
/// both by this module's own algebra tests and by `state_store`'s
/// incremental-vs-from-scratch regression tests to cross-check the
/// production parallel reduction (`current_desired_identity_with_prior`)
/// and the incremental sparse-toggle path
/// (`state_store::reconciliation::apply_shard_catalog_tx`) -- never
/// called from production code. Kept here (not in `gat-core`) since
/// `#[cfg(test)]` items are not visible across a crate boundary.
#[cfg(any(test, feature = "test-support"))]
pub fn desired_identity_from_shards<'a>(
    shards: impl IntoIterator<Item = (LockShardId, &'a ShardContentIdentity)>,
) -> CanonicalDesiredIdentity {
    shards
        .into_iter()
        .fold(CanonicalDesiredIdentity::empty(), |acc, (id, identity)| {
            acc.toggle_shard(id, identity)
        })
}

pub fn current_desired_identity(root: &Path) -> Result<CanonicalDesiredIdentity> {
    current_desired_identity_with_prior(root, |_shard_id: LockShardId| None)
}

/// As [`current_desired_identity`], but before falling back to a direct
/// content read for a given shard, first asks `prior_for_shard` whether
/// *its* own catalog already has a `(identity, proof)` record for that
/// shard id. This is how [`crate::state::StateStore`] provides a
/// genuine, no-read current-revision fast path backed by its durable per-shard
/// stat cache (best-effort and read-only -- see
/// [`crate::state::StateStore::open_if_exists`]) while this lock
/// module sees only the shared
/// [`crate::file_state::StatProof`] and a closure, never the store or its
/// types. A closure that always returns `None` (as [`current_desired_identity`]
/// passes) simply disables the accelerator, falling straight through to a
/// direct read + BLAKE3 for every shard -- always correct, only slower.
///
/// Every shard is resolved by the same
/// [`resolve_shard_identity`] logic state reconciliation uses --
/// an exact stored proof hit reuses the prior identity with no read, and
/// every proof miss performs one coherent no-follow regular-file read/hash
/// and accepts whatever identity that produces (new, changed, or
/// unchanged). A pre/post metadata mismatch observed during that read
/// fails closed immediately instead of retrying. This closes the gap
/// where a lock-bypassing writer racing this read could otherwise be
/// invisible to `Operation::mutate()`'s revalidation gate.
///
/// Explicitly embarrassingly parallel: each
/// shard is resolved to its own [`CanonicalDesiredIdentity`] component
/// completely independently of every other shard, and Rayon's `reduce`
/// XOR-combines the per-worker partials into one final accumulator.
/// Deliberately does not sort shard ids, collect an ordered
/// `(id, identity)` vector, or route every shard through one shared
/// mutable accumulator (mutex or atomic) -- the only cross-shard
/// coordination is that final associative/commutative reduction, so
/// there is nothing here for shard count to serialize through. This
/// function never opens a Git repository or index: the live shard set on
/// disk is the sole source of truth, and every cache miss is exactly one
/// `std::fs::read` followed by one `BLAKE3(bytes)`.
pub fn current_desired_identity_with_prior(
    root: &Path,
    prior_for_shard: impl Fn(LockShardId) -> Option<(ShardContentIdentity, Option<StatProof>)> + Sync,
) -> Result<CanonicalDesiredIdentity> {
    use rayon::prelude::*;

    let shard_files = super::persistence::list_shard_files(root)?;

    shard_files
        .par_iter()
        .map(|shard| {
            let resolution =
                resolve_shard_identity(&shard.full_path, prior_for_shard(shard.shard_id), || {
                    #[cfg(any(test, feature = "test-support"))]
                    race_test_hooks::fire_before_read(&shard.full_path);
                    #[cfg(any(test, feature = "test-support"))]
                    test_support::record_current_identity_content_read_call();
                    std::fs::read(&shard.full_path)
                        .map_err(|source| LockError::io("reading", &shard.full_path, source))
                })?;
            let identity = match resolution {
                ShardIdentityResolution::StatOnly { identity, .. }
                | ShardIdentityResolution::Coherent { identity, .. } => identity,
            };
            Ok::<_, LockError>(
                CanonicalDesiredIdentity::empty().toggle_shard(shard.shard_id, &identity),
            )
        })
        .try_reduce(CanonicalDesiredIdentity::empty, |a, b| Ok(a.xor(b)))
}

/// Test-only deterministic race injection for
/// `current_desired_identity_with_prior`'s per-shard content read,
/// supporting `engine::workspace::sync::desired_index` tests of
/// the same shape: production code never
/// needs this -- it exists so `commands::operation_tests` can prove
/// `Operation::mutate`'s revalidation gate (which calls
/// `current_desired_revision` -> this function) rejects a shard rewrite
/// that lands between the resolver's pre-read and post-read stat
/// observations, without depending on a real, inherently flaky
/// thread-timing race.
#[cfg(any(test, feature = "test-support"))]
pub mod race_test_hooks {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};

    type Hook = Arc<Mutex<Box<dyn FnMut(&Path) + Send>>>;

    // Reads run on Rayon workers. Key hooks by fixture path so parallel tests
    // cannot replace or clear one another's injections.
    static BEFORE_READ: LazyLock<Mutex<HashMap<PathBuf, Hook>>> = LazyLock::new(Mutex::default);

    /// Owns one fixture's injection, including cleanup during unwinding.
    #[must_use]
    pub struct HookGuard {
        path: PathBuf,
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            BEFORE_READ.lock().unwrap().remove(&self.path);
        }
    }

    /// Install a fixture-scoped hook, visible to reads on any worker thread.
    ///
    /// # Panics
    /// Panics if the path already has a hook or the registry is poisoned.
    pub fn install(path: PathBuf, hook: impl FnMut(&Path) + Send + 'static) -> HookGuard {
        let installed = {
            let mut hooks = BEFORE_READ.lock().unwrap();
            match hooks.entry(path.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Arc::new(Mutex::new(Box::new(hook))));
                    true
                }
                std::collections::hash_map::Entry::Occupied(_) => false,
            }
        };
        assert!(installed, "a fixture may install only one hook per path");
        HookGuard { path }
    }

    /// # Panics
    /// Panics if this fixture's hook or a hook mutex is poisoned.
    pub fn fire_before_read(path: &Path) {
        let hook = BEFORE_READ.lock().unwrap().get(path).cloned();
        if let Some(hook) = hook {
            // Do not run user callbacks under the registry lock. A callback
            // panic must not poison another fixture's registration or cleanup.
            hook.lock().unwrap()(path);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[test]
        fn fixture_guards_are_independent_and_visible_on_workers() {
            let calls = Arc::new(AtomicUsize::new(0));
            let first_calls = calls.clone();
            let first = install(PathBuf::from("first-test-shard"), move |_| {
                first_calls.fetch_add(1, Ordering::SeqCst);
            });
            let second_calls = calls.clone();
            let second = install(PathBuf::from("second-test-shard"), move |_| {
                second_calls.fetch_add(10, Ordering::SeqCst);
            });
            std::thread::spawn(|| {
                fire_before_read(Path::new("first-test-shard"));
                fire_before_read(Path::new("second-test-shard"));
                fire_before_read(Path::new("unregistered-test-shard"));
            })
            .join()
            .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 11);
            drop(first);
            fire_before_read(Path::new("first-test-shard"));
            fire_before_read(Path::new("second-test-shard"));
            assert_eq!(calls.load(Ordering::SeqCst), 21);
            drop(second);
            fire_before_read(Path::new("second-test-shard"));
            assert_eq!(calls.load(Ordering::SeqCst), 21);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a canonical shard-id literal used throughout these
    /// fixtures.
    fn sid(raw: &str) -> LockShardId {
        LockShardId::parse_canonical(raw).unwrap()
    }

    /// Identical raw
    /// shard bytes must always produce the identical
    /// [`ShardContentIdentity`] no matter which production path computed
    /// it -- [`hash_shard_bytes`] called directly (as publication does),
    /// or reached indirectly through
    /// `current_desired_identity_with_prior`'s full live observation
    /// (which falls back to exactly the same function on a cache miss).
    /// There is now only one leaf identity computation in the codebase;
    /// this proves every caller of it agrees.
    #[test]
    fn identical_shard_bytes_produce_the_identical_leaf_identity_from_every_path() {
        let tmp = tempfile::tempdir().unwrap();
        let oid = "a".repeat(64);
        let bytes = format!("{0}\n{oid}\ta.bin\n", crate::lock::VERSION);
        let path = tmp.path().join("gat.lock");
        std::fs::write(&path, &bytes).unwrap();
        // With no prior identity to compare against, this shard's proof
        // miss simply performs one coherent read/hash and accepts the
        // resulting identity immediately -- no wait or retry is needed
        // for a freshly written shard under the coherent-observation
        // model.

        let direct = hash_shard_bytes(bytes.as_bytes());
        let observed = current_desired_identity_with_prior(tmp.path(), |_| None).unwrap();
        let expected = CanonicalDesiredIdentity::empty().toggle_shard(sid("gat.lock"), &direct);
        assert_eq!(observed, expected);
    }

    /// [`ShardContentIdentity::decode`] must reject any length other than
    /// exactly 32 bytes rather than silently truncating or padding --
    /// storage-boundary callers depend on this to fail closed on a
    /// malformed persisted row.
    #[test]
    fn decode_rejects_any_length_other_than_32_bytes() {
        assert!(ShardContentIdentity::decode(&[0u8; 31]).is_err());
        assert!(ShardContentIdentity::decode(&[0u8; 33]).is_err());
        assert!(ShardContentIdentity::decode(&[]).is_err());
        assert!(ShardContentIdentity::decode(&[7u8; 32]).is_ok());
    }

    fn identity(byte: u8) -> ShardContentIdentity {
        ShardContentIdentity::from_array([byte; 32])
    }

    /// XOR-accumulating the same shard set in a
    /// different order must produce the identical [`CanonicalDesiredIdentity`]
    /// -- the whole point of using an associative/commutative combine
    /// instead of an ordered fold.
    #[test]
    fn desired_identity_from_shards_is_independent_of_order() {
        let shards = [
            (sid("gat.lock/00/0a.tsv"), identity(1)),
            (sid("gat.lock/00/0b.tsv"), identity(2)),
            (sid("gat.lock/01/0c.tsv"), identity(3)),
        ];
        let forward =
            desired_identity_from_shards(shards.iter().map(|(id, identity)| (*id, identity)));
        let mut reversed = shards;
        reversed.reverse();
        let backward =
            desired_identity_from_shards(reversed.iter().map(|(id, identity)| (*id, identity)));
        assert_eq!(forward, backward);

        // A third, arbitrary permutation, for good measure.
        let shuffled = [shards[1], shards[2], shards[0]];
        let shuffled_result =
            desired_identity_from_shards(shuffled.iter().map(|(id, identity)| (*id, identity)));
        assert_eq!(forward, shuffled_result);
    }

    /// The identity element of the XOR set accumulator: an empty shard
    /// set reduces to [`CanonicalDesiredIdentity::empty`] with no special
    /// casing anywhere in [`desired_identity_from_shards`].
    #[test]
    fn desired_identity_from_shards_of_empty_set_is_the_empty_identity() {
        let empty: [(LockShardId, &ShardContentIdentity); 0] = [];
        assert_eq!(
            desired_identity_from_shards(empty.into_iter()),
            CanonicalDesiredIdentity::empty()
        );
    }

    /// Adding a shard's contribution and then removing the exact same
    /// `(shard_id, identity)` pair returns to the original accumulator --
    /// XOR is self-inverting, so "toggle in, toggle out" is always a
    /// no-op regardless of what else has accumulated in between.
    #[test]
    fn toggle_shard_is_self_inverting() {
        let base = CanonicalDesiredIdentity::empty().toggle_shard(sid("gat.lock"), &identity(9));
        let toggled_in = base.toggle_shard(sid("gat.lock/00/0a.tsv"), &identity(5));
        assert_ne!(toggled_in, base);
        let toggled_back_out = toggled_in.toggle_shard(sid("gat.lock/00/0a.tsv"), &identity(5));
        assert_eq!(toggled_back_out, base);
    }

    /// A shard whose content identity is replaced (not merely added or
    /// removed) must update the accumulator by exactly `old XOR new` --
    /// toggling out the old identity's component and toggling in the new
    /// one -- the operation [`state_store::reconciliation::apply_shard_catalog_tx`]
    /// (see its own module) performs incrementally for a changed shard.
    #[test]
    fn replacing_a_shard_identity_is_equivalent_to_toggling_old_out_and_new_in() {
        let before = CanonicalDesiredIdentity::empty()
            .toggle_shard(sid("gat.lock/00/0a.tsv"), &identity(1))
            .toggle_shard(sid("gat.lock/00/0b.tsv"), &identity(2));
        let after_replace = before
            .toggle_shard(sid("gat.lock/00/0b.tsv"), &identity(2))
            .toggle_shard(sid("gat.lock/00/0b.tsv"), &identity(7));

        let expected_from_scratch = desired_identity_from_shards([
            (sid("gat.lock/00/0a.tsv"), &identity(1)),
            (sid("gat.lock/00/0b.tsv"), &identity(7)),
        ]);
        assert_eq!(after_replace, expected_from_scratch);
    }

    /// A full from-scratch recomputation over some final shard set must
    /// agree exactly with a sequence of sparse [`CanonicalDesiredIdentity::toggle_shard`]
    /// calls that starts from empty, adds every shard once, then replaces
    /// one shard's identity and removes another -- exercising add, replace,
    /// and remove all in the same accumulator, the same three kinds of
    /// sparse mutation `apply_shard_catalog_tx` performs incrementally.
    #[test]
    fn full_recomputation_matches_a_sequence_of_sparse_toggles() {
        let mut acc = CanonicalDesiredIdentity::empty();
        acc = acc.toggle_shard(sid("gat.lock/00/0a.tsv"), &identity(1));
        acc = acc.toggle_shard(sid("gat.lock/00/0b.tsv"), &identity(2));
        acc = acc.toggle_shard(sid("gat.lock/01/0c.tsv"), &identity(3));
        // Replace b's identity: toggle the old one out, the new one in.
        acc = acc.toggle_shard(sid("gat.lock/00/0b.tsv"), &identity(2));
        acc = acc.toggle_shard(sid("gat.lock/00/0b.tsv"), &identity(9));
        // Remove c entirely.
        acc = acc.toggle_shard(sid("gat.lock/01/0c.tsv"), &identity(3));

        let final_shards = [
            (sid("gat.lock/00/0a.tsv"), identity(1)),
            (sid("gat.lock/00/0b.tsv"), identity(9)),
        ];
        let from_scratch =
            desired_identity_from_shards(final_shards.iter().map(|(id, identity)| (*id, identity)));
        assert_eq!(acc, from_scratch);
    }

    /// Sequential [`desired_identity_from_shards`] and a differently
    /// partitioned/ordered parallel Rayon reduction of the exact same
    /// shard components must agree -- proving
    /// `current_desired_identity_with_prior`'s embarrassingly parallel
    /// map/reduce is just another valid grouping of the same associative
    /// XOR combine, not a second, independently-defined algorithm.
    #[test]
    fn sequential_reduction_matches_differently_partitioned_parallel_reduction() {
        use rayon::prelude::*;

        let shards: Vec<(LockShardId, ShardContentIdentity)> = (0..37)
            .map(|i| {
                (
                    sid(&format!("gat.lock/{i:02x}.tsv")),
                    identity(u8::try_from(i % 251).unwrap()),
                )
            })
            .collect();

        let sequential =
            desired_identity_from_shards(shards.iter().map(|(id, identity)| (*id, identity)));

        // Chunk into differently-sized, arbitrarily ordered partitions and
        // reduce each partition sequentially before combining partitions
        // with Rayon's own (possibly out-of-order) parallel reduction.
        let parallel = shards
            .par_chunks(5)
            .map(|chunk| {
                chunk
                    .iter()
                    .rev()
                    .fold(CanonicalDesiredIdentity::empty(), |acc, (id, identity)| {
                        acc.toggle_shard(*id, identity)
                    })
            })
            .reduce(
                CanonicalDesiredIdentity::empty,
                CanonicalDesiredIdentity::xor,
            );

        assert_eq!(sequential, parallel);
    }
}
