//! Shared stat proofs for cache objects, materialized files, desired-lock
//! shards, and managed Git exclusions. These accelerators use the same
//! metadata comparison and coherent-observation rules.
//!
//! This module observes filesystem metadata but owns neither `gat.lock`
//! nor any `SQLite` schema. The target rule it implements is:
//! **hash to establish identity, stat to preserve identity, re-hash
//! only when the preservation proof is untrustworthy.**
//!
//! [`StatProof`] is intentionally small and cross-platform: exact byte
//! size plus the modification time at the platform's highest available
//! resolution (whole seconds plus nanoseconds, as returned by
//! `Metadata::modified()`). It deliberately excludes inode/device
//! numbers, ctime, uid/gid, and observation timestamps.
//!
//! `coherent_observation` is the single shared primitive every
//! content-identity-establishing operation in the crate routes through:
//! it stats `path` (no-follow, requiring a regular file) immediately
//! before running the caller's operation and again immediately after. If
//! both stats agree exactly, the observation is [`CoherentObservation`]
//! and its `proof` is safe to persist and reuse for a later stat-only
//! match. Any disagreement -- a missing path, a symlink/directory/other
//! non-regular object on either side, or two regular-file stats that
//! differ -- fails with an error: there is no proof-less "still
//! trustworthy" outcome. A rewrite that preserves the exact persisted
//! `StatProof` is not detected.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Everything `coherent_observation` itself can fail with -- distinct
/// from whatever `op` (the caller-supplied read/hash) can fail with.
/// Generic over the caller's own error type (`E`) via `From`, mirroring
/// [`crate::state::desired::StateStore::
/// with_desired_paths`]'s pattern: each caller's typed error gains a
/// `#[from] FileStateError` variant instead of `coherent_observation`
/// itself needing to know every caller's error type, and instead of a
/// caller round-tripping through `anyhow` + `downcast` to recover it.
#[derive(Debug, thiserror::Error)]
pub enum FileStateError {
    /// `path` did not resolve to an existing regular file (no-follow)
    /// immediately before `op` ran.
    #[error("{} is not an existing regular file", path.display())]
    NotRegularFile { path: PathBuf },

    /// `path` did not resolve to the same regular file (missing,
    /// became a symlink/directory/other special file, or its stat
    /// changed) immediately after `op` ran: the observation was not
    /// coherent.
    #[error("{} changed while being observed -- not one coherent read", path.display())]
    Observed { path: PathBuf },
}

/// The one shared, cross-platform proof of a regular file's content
/// identity: exact byte size plus the modification time at the
/// platform's full native resolution (whole seconds and nanoseconds).
/// Device, inode, ownership, and observation timestamps are deliberately
/// excluded so the proof remains small and cross-platform.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct StatProof {
    pub(crate) size: u64,
    pub(crate) mtime_secs: i64,
    pub(crate) mtime_nanos: i64,
}

impl std::fmt::Debug for StatProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatProof").finish_non_exhaustive()
    }
}

impl StatProof {
    /// Whether `self` (a freshly observed proof) proves the file's
    /// content identity is still whatever `prior` was recorded for: an
    /// exact `(size, mtime_secs, mtime_nanos)` match. See
    /// `coherent_observation` and [`check_known_oid`], which combine
    /// this comparison with a pre/post regular-file stat pair to detect
    /// an observed mutation. Every stat-only identity-preservation
    /// shortcut in the application must delegate to this same comparison
    /// rather than reimplementing it, or a subset of it, independently.
    pub const fn matches(&self, prior: &Self) -> bool {
        self.size == prior.size
            && self.mtime_secs == prior.mtime_secs
            && self.mtime_nanos == prior.mtime_nanos
    }
}

/// Stat `path` without following symlinks, returning a [`StatProof`]
/// only when it resolves to a regular file. A symlink, directory,
/// special file, or a path that can't be stat'd at all (including one
/// that does not exist) yields `None`: Gat never treats a non-regular
/// file as carrying a trustworthy content-identity proof.
pub fn observe_regular_file_no_follow(path: &Path) -> Option<StatProof> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    stat_proof_from_metadata(&meta)
}

/// As [`observe_regular_file_no_follow`], but from metadata the caller
/// already has in hand (e.g. returned alongside a just-completed write)
/// instead of re-statting the path -- avoiding a second, potentially
/// racing filesystem call for a proof about metadata already observed.
pub fn stat_proof_from_metadata(meta: &std::fs::Metadata) -> Option<StatProof> {
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_stat_proof_from_metadata_call();
    if !meta.is_file() {
        return None;
    }
    let mtime = meta.modified().ok()?;
    let since_epoch = mtime.duration_since(UNIX_EPOCH).unwrap_or_default();
    Some(StatProof {
        size: meta.len(),
        mtime_secs: i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
        mtime_nanos: i64::from(since_epoch.subsec_nanos()),
    })
}

/// The result of a successful `coherent_observation`: `op`'s own
/// result, paired with the post-operation [`StatProof`] that proves it
/// is safe to persist and reuse for a later stat-only match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoherentObservation<T> {
    pub(crate) value: T,
    pub(crate) proof: StatProof,
}

/// Stat `path` (no-follow, requiring a regular file) immediately before
/// and immediately after running `op`, succeeding with a
/// [`CoherentObservation`] only when both stats agree that `path` was,
/// and remained, the exact same regular file throughout. This is the one
/// shared primitive every operation in the crate that establishes a
/// content identity from a live filesystem read must route through.
///
/// Capturing the "before" stat immediately before `op` runs and the
/// "after" stat immediately after it returns -- rather than, say,
/// stat'ing well before or well after the actual read/hash -- is what
/// makes a mismatch here a reliable proof that persisting an
/// `(identity, proof)` pair would be unsound, not just a heuristic.
///
/// # What this proves, and what it deliberately does not
///
/// A successful result proves `op`'s value describes `path`'s content at
/// one single, coherent instant: no mutation crossed the observation
/// that this primitive's own before/after regular-file stat comparison
/// could detect. It does **not** defend against a rewrite that happens
/// to preserve `path`'s exact `(size, mtime_secs, mtime_nanos)` -- a
/// filesystem/tool capable of round-tripping both values exactly can
/// defeat this check.
///
/// Any pre/post disagreement -- a missing path, a symlink/directory/
/// other non-regular object on either side, or two regular-file stats
/// that differ -- fails with an error: unlike the `Stable`/`Racy`/
/// `Unstable` model this replaces, there is no successful-but-proof-less
/// outcome any more. `op` still always runs (its side effects, if any,
/// already happened by the time a caller could observe this error), but
/// its result is only ever returned wrapped in a proof the caller can
/// trust.
pub fn coherent_observation<T, E>(
    path: &Path,
    op: impl FnOnce() -> std::result::Result<T, E>,
) -> std::result::Result<CoherentObservation<T>, E>
where
    E: From<FileStateError>,
{
    let before = observe_regular_file_no_follow(path).ok_or_else(|| {
        E::from(FileStateError::NotRegularFile {
            path: path.to_path_buf(),
        })
    })?;
    let value = op()?;
    let after = observe_regular_file_no_follow(path).ok_or_else(|| {
        E::from(FileStateError::Observed {
            path: path.to_path_buf(),
        })
    })?;
    if !after.matches(&before) {
        return Err(E::from(FileStateError::Observed {
            path: path.to_path_buf(),
        }));
    }
    Ok(CoherentObservation {
        value,
        proof: after,
    })
}

/// The outcome of [`check_known_oid`]: whether `expected_oid` was
/// trusted purely from a stat match, or had to be (re-)established by
/// actually hashing `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityCheck {
    /// The current stat exactly matched the prior proof: `expected_oid`
    /// is trusted without reading the file's content at all.
    Proven,
    /// The stat alone couldn't prove identity (missing/mismatched proof,
    /// or an unreadable/non-regular-file path), so `path` was actually
    /// hashed. `oid` is the freshly computed hash; `matches` records
    /// whether it confirmed `expected_oid`; `proof` is `Some` whenever a
    /// coherent observation was actually performed to produce `oid` (see
    /// `coherent_observation`), or `None` for the definitive
    /// size-mismatch shortcut below, which never reads or hashes `path`
    /// at all. A pre/post mutation observed while (re-)hashing `path`
    /// fails this call outright (see `coherent_observation`) rather
    /// than returning any `Hashed` variant; there is no
    /// proof-less-but-still-trustworthy `Hashed` outcome.
    Hashed {
        oid: gat_core::oid::Oid,
        matches: bool,
        proof: Option<StatProof>,
    },
}

/// Something that can tell whether a freshly hashed native oid matches
/// its own expected identity, without requiring that identity to already
/// be typed as an [`gat_core::oid::Oid`]. [`check_known_oid`] only ever
/// needs this comparison on its `Hashed` fallback path (see below) --
/// its far more common stat-proven early return never inspects the
/// expected oid at all -- so keeping the expected side generic over this
/// trait lets a native caller (e.g. a SQLite-backed `Oid`) compare with
/// zero allocation, while a caller that only holds a hex `&str` (a
/// genuine text boundary) still pays
/// nothing beyond the one hex decode this comparison already requires.
pub trait ExpectedOid {
    fn oid_eq(&self, computed: &gat_core::oid::Oid) -> bool;
}

impl ExpectedOid for &str {
    fn oid_eq(&self, computed: &gat_core::oid::Oid) -> bool {
        computed.eq_hex(self)
    }
}

impl ExpectedOid for gat_core::oid::Oid {
    fn oid_eq(&self, computed: &gat_core::oid::Oid) -> bool {
        self == computed
    }
}

impl ExpectedOid for &gat_core::oid::Oid {
    /// Native compare -- lets [`check_known_oid`]'s callers pass a native
    /// `&Oid` straight through without any hex-encoding at all, since the
    /// far more common stat-proven early return never reaches this
    /// comparison, and the fallback hash path now yields a native `Oid`
    /// too.
    fn oid_eq(&self, computed: &gat_core::oid::Oid) -> bool {
        *self == computed
    }
}

/// Decide whether `expected_oid` is still trustworthy for `path`,
/// implementing the target rule "hash to establish identity, stat to
/// preserve identity, re-hash only when the preservation proof is no
/// longer trustworthy".
///
/// `prior_proof` must only ever be the proof recorded alongside
/// `expected_oid` itself -- passing a proof recorded for a different OID
/// would defeat the whole point of this check. When no proof-only match
/// can be established, `hash` is invoked to actually (re-)read `path`
/// (e.g. [`crate::cache::object::hash_file`]) through
/// `coherent_observation`: a mutation observed while hashing fails
/// this call outright rather than returning any `Hashed` result that
/// could be mistaken for a confirmed identity.
///
/// One exception needs no read at all: a `size` that differs from
/// `prior_proof`'s recorded size is definitive proof the content differs
/// (two different-length byte sequences can never hash to the same
/// digest). This lets a genuine size change be reported as a conflict
/// without ever opening `path`'s content, even when the file has since
/// become unreadable.
pub fn check_known_oid<E>(
    path: &Path,
    expected_oid: impl ExpectedOid,
    prior_proof: Option<&StatProof>,
    hash: impl FnOnce(&Path) -> std::result::Result<gat_core::oid::Oid, E>,
) -> std::result::Result<IdentityCheck, E>
where
    E: From<FileStateError>,
{
    if let Some(prior) = prior_proof
        && let Some(current) = observe_regular_file_no_follow(path)
    {
        if current.matches(prior) {
            return Ok(IdentityCheck::Proven);
        }
        if current.size != prior.size {
            return Ok(IdentityCheck::Hashed {
                oid: gat_core::oid::Oid::from_bytes([0; 32]),
                matches: false,
                proof: None,
            });
        }
    }
    let observation = coherent_observation(path, || hash(path))?;
    let matches = expected_oid.oid_eq(&observation.value);
    Ok(IdentityCheck::Hashed {
        oid: observation.value,
        matches,
        proof: Some(observation.proof),
    })
}

/// Proof-format version 1: byte 0 is the version tag, bytes 1..9 are
/// `size` as a fixed-endianness `u64`, bytes 9..17 are `mtime_secs` as a
/// fixed-endianness `i64`, and bytes 17..21 are `mtime_nanos` as a
/// fixed-endianness `i32` -- an explicit application wire format, not a
/// serialization of Rust's in-memory struct layout, so it stays stable
/// and architecture-independent across `gat` versions and platforms.
/// Carries full sub-second resolution in the version-1 wire format. The
/// format is decoded only when its version and length are recognized.
const PROOF_VERSION_1: u8 = 1;

/// The exact, fixed encoded length of a version-1 [`StatProof`].
pub const ENCODED_LEN_V1: usize = 21;

/// Encode `proof` into its explicit, versioned wire format (see
/// [`PROOF_VERSION_1`]). Proof-format versioning here is independent of
/// any `SQLite` schema versioning a persisting caller may also have.
pub fn encode_stat_proof(proof: &StatProof) -> [u8; ENCODED_LEN_V1] {
    let mut out = [0u8; ENCODED_LEN_V1];
    out[0] = PROOF_VERSION_1;
    out[1..9].copy_from_slice(&proof.size.to_le_bytes());
    out[9..17].copy_from_slice(&proof.mtime_secs.to_le_bytes());
    out[17..21].copy_from_slice(&proof.mtime_nanos.to_le_bytes()[..4]);
    out
}

/// Decode a [`StatProof`] written by [`encode_stat_proof`].
/// An unknown version, a wrong length, an out-of-range nanosecond value
/// (must be `0..1_000_000_000`), or otherwise malformed bytes decode as
/// `None` -- "no reusable proof" -- never as trusted state: a caller
/// that can't recognize its own encoding must always fall back to
/// re-establishing identity by hash, not silently trust unverified
/// bytes.
pub fn decode_stat_proof(bytes: &[u8]) -> Option<StatProof> {
    if bytes.len() != ENCODED_LEN_V1 {
        return None;
    }
    if bytes[0] != PROOF_VERSION_1 {
        return None;
    }
    let size = u64::from_le_bytes(bytes[1..9].try_into().ok()?);
    let mtime_secs = i64::from_le_bytes(bytes[9..17].try_into().ok()?);
    let mtime_nanos = i32::from_le_bytes(bytes[17..21].try_into().ok()?);
    if !(0..1_000_000_000).contains(&mtime_nanos) {
        return None;
    }
    Some(StatProof {
        size,
        mtime_secs,
        mtime_nanos: i64::from(mtime_nanos),
    })
}

/// Test-only instrumentation proving a proof-agnostic caller
/// never mints a [`StatProof`] it would only discard. Thread-local
/// because `cargo test` runs tests concurrently on separate threads.
/// Gated on the `test-support` feature (in addition to `cfg(test)`) so
/// the root `gat` crate's own tests -- which link `gat-io` as an ordinary
/// (non-test) dependency and so cannot see another crate's `#[cfg(test)]`
/// items directly -- can still observe this instrumentation via
/// `gat-io = { path = "gat-io", features = ["test-support"] }` in
/// `[dev-dependencies]`; Cargo's feature unification then activates it
/// for every build of `gat-io` in that same `cargo test` invocation.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static STAT_PROOF_FROM_METADATA_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn record_stat_proof_from_metadata_call() {
        STAT_PROOF_FROM_METADATA_CALLS.with(|c| c.set(c.get() + 1));
    }

    pub fn stat_proof_from_metadata_call_count() -> usize {
        STAT_PROOF_FROM_METADATA_CALLS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_oid() -> gat_core::oid::Oid {
        gat_core::oid::Oid::from_hex(&"ab".repeat(32)).unwrap()
    }

    #[test]
    fn exact_size_and_mtime_hit_is_proven_with_zero_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();
        let prior = observe_regular_file_no_follow(&path).unwrap();

        let mut hash_calls = 0u32;
        let result = check_known_oid(&path, test_oid(), Some(&prior), |_| {
            hash_calls += 1;
            Ok::<_, FileStateError>(test_oid())
        })
        .unwrap();
        assert_eq!(result, IdentityCheck::Proven);
        assert_eq!(hash_calls, 0);
    }

    #[test]
    fn size_mismatch_is_a_definite_difference_that_skips_hashing_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();
        let mut prior = observe_regular_file_no_follow(&path).unwrap();
        prior.size += 1;

        let mut hash_calls = 0u32;
        let result = check_known_oid(&path, test_oid(), Some(&prior), |_| {
            hash_calls += 1;
            Ok::<_, FileStateError>(test_oid())
        })
        .unwrap();
        assert!(matches!(
            result,
            IdentityCheck::Hashed { matches: false, .. }
        ));
        assert_eq!(
            hash_calls, 0,
            "a size mismatch alone already proves the content differs -- no read is needed"
        );
    }

    #[test]
    fn mtime_mismatch_skips_stat_reuse_and_falls_back_to_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();
        let mut prior = observe_regular_file_no_follow(&path).unwrap();
        prior.mtime_secs -= 1;

        let mut hash_calls = 0u32;
        let result = check_known_oid(&path, test_oid(), Some(&prior), |_| {
            hash_calls += 1;
            Ok::<_, FileStateError>(test_oid())
        })
        .unwrap();
        assert!(matches!(
            result,
            IdentityCheck::Hashed { matches: true, .. }
        ));
        assert_eq!(hash_calls, 1);
    }

    #[test]
    fn missing_proof_hashes_when_identity_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();

        let mut hash_calls = 0u32;
        let result = check_known_oid(&path, test_oid(), None, |_| {
            hash_calls += 1;
            Ok::<_, FileStateError>(test_oid())
        })
        .unwrap();
        assert!(matches!(
            result,
            IdentityCheck::Hashed { matches: true, .. }
        ));
        assert_eq!(hash_calls, 1);
    }

    #[test]
    fn successful_hash_returns_a_reusable_proof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();

        let result = check_known_oid(&path, test_oid(), None, |_| {
            Ok::<_, FileStateError>(test_oid())
        })
        .unwrap();
        match result {
            IdentityCheck::Hashed { matches, proof, .. } => {
                assert!(matches);
                assert!(proof.is_some());
            }
            IdentityCheck::Proven => panic!("expected a hash to have been required"),
        }
    }

    /// Coherent-observation regression: an observed pre/post mutation
    /// during the coherent read/hash must fail the whole call closed,
    /// never return a confirmed match paired with an untrustworthy
    /// digest -- even when the hash closure happens to return a value
    /// equal to `expected_oid`.
    #[test]
    fn check_known_oid_fails_closed_when_the_file_changes_mid_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();
        let path_clone = path.clone();
        let result = check_known_oid(&path, test_oid(), None, move |_| {
            fs::write(&path_clone, b"hello world, changed mid hash").unwrap();
            Ok::<_, FileStateError>(test_oid())
        });
        assert!(
            result.is_err(),
            "an observed pre/post mutation must fail the call, not return a confirmed match"
        );
    }

    /// `coherent_observation` itself: an untouched file across `op`
    /// yields a proof that exactly matches a fresh independent stat of
    /// the file.
    #[test]
    fn coherent_observation_succeeds_when_the_file_is_untouched_during_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();

        let observed = coherent_observation(&path, || Ok::<_, FileStateError>(42)).unwrap();
        assert_eq!(observed.value, 42);
        assert_eq!(observed.proof.size, 5);
    }

    /// The race this primitive exists to prevent: if the file's metadata
    /// changes *during* `op` (a concurrent write), the whole call must
    /// fail -- never succeed with a proof that could be misread as
    /// proving `op`'s result still matches the file's current bytes.
    #[test]
    fn coherent_observation_fails_when_the_file_changes_during_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();
        let path_clone = path.clone();
        let result: std::result::Result<CoherentObservation<()>, FileStateError> =
            coherent_observation(&path, move || {
                fs::write(&path_clone, b"hello world, changed").unwrap();
                Ok(())
            });
        assert!(result.is_err());
    }

    /// A path that stops existing partway through `op` (e.g.
    /// concurrently removed) must likewise fail rather than succeed with
    /// a proof derived from a stale or partial observation.
    #[test]
    fn coherent_observation_fails_when_the_file_is_removed_during_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();
        let path_clone = path.clone();
        let result: std::result::Result<CoherentObservation<()>, FileStateError> =
            coherent_observation(&path, move || {
                fs::remove_file(&path_clone).unwrap();
                Ok(())
            });
        assert!(result.is_err());
    }

    /// A path that is missing before `op` even starts must fail
    /// immediately, without ever calling `op`.
    #[test]
    fn coherent_observation_fails_on_a_missing_path_without_calling_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.txt");
        let mut called = false;
        let result: std::result::Result<CoherentObservation<()>, FileStateError> =
            coherent_observation(&path, || {
                called = true;
                Ok(())
            });
        assert!(result.is_err());
        assert!(
            !called,
            "op must never run against a path that doesn't exist yet"
        );
    }

    /// A symlink leaf must never be observed as though its target were
    /// the managed file: `coherent_observation` fails outright rather
    /// than transparently following it.
    #[cfg(unix)]
    #[test]
    fn coherent_observation_fails_on_a_symlink_even_when_the_target_is_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, b"hello").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result: std::result::Result<CoherentObservation<()>, FileStateError> =
            coherent_observation(&link, || Ok(()));
        assert!(result.is_err());
    }

    #[test]
    fn missing_path_classifies_as_no_proof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.txt");
        assert_eq!(observe_regular_file_no_follow(&path), None);
    }

    #[test]
    fn directory_path_classifies_as_no_proof() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(observe_regular_file_no_follow(dir.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_path_classifies_as_no_proof_even_when_the_target_is_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, b"hello").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(observe_regular_file_no_follow(&link), None);
    }

    #[test]
    fn v1_codec_exact_length_version_and_fixed_endian_round_trip() {
        let proof = StatProof {
            size: 0x0102_0304_0506_0708,
            mtime_secs: -12345,
            mtime_nanos: 123_456_789,
        };
        let encoded = encode_stat_proof(&proof);
        assert_eq!(encoded.len(), ENCODED_LEN_V1);
        assert_eq!(encoded[0], 1, "version byte");
        // Fixed little-endian regardless of host architecture.
        assert_eq!(&encoded[1..9], &proof.size.to_le_bytes());
        assert_eq!(&encoded[9..17], &proof.mtime_secs.to_le_bytes());
        assert_eq!(&encoded[17..21], &proof.mtime_nanos.to_le_bytes()[..4]);
        let decoded = decode_stat_proof(&encoded).unwrap();
        assert_eq!(decoded, proof);
    }

    #[test]
    fn unknown_version_decodes_as_no_proof() {
        let proof = StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 1,
        };
        let mut encoded = encode_stat_proof(&proof);
        encoded[0] = 2;
        assert_eq!(decode_stat_proof(&encoded), None);
    }

    #[test]
    fn out_of_range_nanoseconds_decode_as_no_proof() {
        let proof = StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 1,
        };
        let mut encoded = encode_stat_proof(&proof);
        encoded[17..21].copy_from_slice(&1_000_000_000i32.to_le_bytes());
        assert_eq!(
            decode_stat_proof(&encoded),
            None,
            "a nanosecond value of exactly one billion or more is invalid"
        );
        encoded[17..21].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(
            decode_stat_proof(&encoded),
            None,
            "a negative nanosecond value is invalid"
        );
    }

    #[test]
    fn malformed_lengths_decode_as_no_proof() {
        let proof = StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 1,
        };
        let encoded = encode_stat_proof(&proof);
        assert_eq!(decode_stat_proof(&encoded[..16]), None);
        assert_eq!(decode_stat_proof(&[]), None);
        let mut too_long = encoded.to_vec();
        too_long.push(0);
        assert_eq!(decode_stat_proof(&too_long), None);
    }

    #[test]
    fn encoding_is_architecture_independent_and_not_rust_layout_dependent() {
        // A hand-built byte sequence (as another platform/process would
        // produce) must decode identically, proving the format is an
        // explicit wire contract rather than `#[repr(Rust)]` memory.
        let mut bytes = [0u8; ENCODED_LEN_V1];
        bytes[0] = 1;
        bytes[1..9].copy_from_slice(&42u64.to_le_bytes());
        bytes[9..17].copy_from_slice(&(-7i64).to_le_bytes());
        bytes[17..21].copy_from_slice(&123_456_789i32.to_le_bytes());
        let decoded = decode_stat_proof(&bytes).unwrap();
        assert_eq!(
            decoded,
            StatProof {
                size: 42,
                mtime_secs: -7,
                mtime_nanos: 123_456_789,
            }
        );
    }

    #[test]
    fn stat_proof_matches_requires_identical_size_and_full_mtime() {
        let prior = StatProof {
            size: 10,
            mtime_secs: 1_000,
            mtime_nanos: 123,
        };
        assert!(prior.matches(&prior));
        for current in [
            StatProof { size: 11, ..prior },
            StatProof {
                mtime_secs: 1_001,
                ..prior
            },
            StatProof {
                mtime_nanos: 124,
                ..prior
            },
        ] {
            assert!(!current.matches(&prior));
        }
    }

    #[test]
    fn the_stat_proof_codec_never_truncates_sub_second_mtime_back_to_whole_seconds() {
        for nanos in [0i64, 1, 999_999_999] {
            let proof = StatProof {
                size: 10,
                mtime_secs: 1_000,
                mtime_nanos: nanos,
            };
            let encoded = encode_stat_proof(&proof);
            let decoded = decode_stat_proof(&encoded).unwrap();
            assert_eq!(
                decoded, proof,
                "round-tripping through the wire codec must preserve full \
                 sub-second resolution, not truncate it to whole seconds"
            );
        }
    }

    #[test]
    fn observing_a_real_file_rewritten_within_the_same_second_detects_the_sub_second_change() {
        // A filesystem-level regression, not just the in-memory struct
        // comparison above: exercises the actual platform
        // `Metadata::modified()` resolution through
        // `observe_regular_file_no_follow`, on whatever resolution the
        // test host's filesystem actually exposes. If the host filesystem
        // only offers whole-second resolution, `mtime_nanos` is `0` on
        // both sides and this simply confirms `matches()` still agrees
        // with the (coarser) ground truth rather than asserting a
        // resolution the platform doesn't have.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();
        let before = observe_regular_file_no_follow(&path).unwrap();

        // Force the same on-disk mtime seconds but a different
        // sub-second component directly through `File::set_modified`, so
        // this doesn't depend on two real writes happening to land in
        // different nanoseconds.
        let bumped_nanos = if before.mtime_nanos < 500_000_000 {
            before.mtime_nanos + 1
        } else {
            before.mtime_nanos - 1
        };
        let bumped = UNIX_EPOCH
            + std::time::Duration::from_secs(u64::try_from(before.mtime_secs).unwrap())
            + std::time::Duration::from_nanos(u64::try_from(bumped_nanos).unwrap());
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(bumped)
            .unwrap();
        let after = observe_regular_file_no_follow(&path).unwrap();

        if after.mtime_nanos == before.mtime_nanos {
            // This filesystem/platform truncated the set mtime to whole
            // seconds (e.g. some non-Linux CI filesystems) -- there is no
            // sub-second resolution to detect here, and asserting one
            // would be testing the filesystem, not gat.
            return;
        }
        assert_eq!(after.mtime_secs, before.mtime_secs);
        assert_ne!(after.mtime_nanos, before.mtime_nanos);
        assert!(!after.matches(&before));
    }
}
