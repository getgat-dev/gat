//! Pure canonical desired-state identity types. Only the fixed-width,
//! deterministic BLAKE3-based value types and their pure combination
//! algebra live here -- the stat-first/coherent-read
//! observation protocol, live filesystem shard enumeration, and any
//! `StatProof`-carrying types live in `gat-io`, since they require
//! filesystem access.

use crate::lock::shard::LockShardId;

/// The lock format version string every [`ShardContentIdentity`]/
/// [`CanonicalDesiredIdentity`] domain separation is keyed on. Kept as a
/// single core-owned constant so [`crate::lock::VERSION`]
/// (used for the on-disk header line as well) and this module's hashing
/// always agree -- see `shard_identity_component`.
pub const LOCK_VERSION: &str = "version https://getgat.dev/spec/lock-v1";

/// One shard file's canonical content identity: `BLAKE3(raw shard file
/// bytes)`, always computed the same way regardless of caller (refresh,
/// publication, or full live observation) -- never overrides
/// worktree content, only to decide whether a shard's desired rows need
/// reparsing, and to fold into a [`CanonicalDesiredIdentity`].
///
/// Fixed-width and `Copy`: a 32-byte BLAKE3 digest, never a variable-length
/// or heap-allocated representation, so every shard identity has exactly
/// one valid encoding -- there is no other shape for a caller to
/// (mis)construct or for storage to validate against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShardContentIdentity([u8; 32]);

impl ShardContentIdentity {
    /// Wraps an already-computed 32-byte BLAKE3 digest directly -- no
    /// rehashing. Used by [`Self::hash`] and by callers that already hold
    /// validated 32 bytes (e.g. after [`Self::decode`]).
    #[must_use]
    pub const fn from_array(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Hash `bytes` with BLAKE3 -- the one, only shard content identity
    /// computation in the codebase: every caller that needs a shard's
    /// canonical identity, whether during publication, refresh, or full
    /// live observation, calls this exact function over the exact bytes
    /// it read/rendered, so identical shard bytes always produce the
    /// identical [`ShardContentIdentity`] no matter which of those paths
    /// computed it.
    #[must_use]
    pub fn hash(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Fallibly decode a persisted byte slice (e.g. `lock_shards.identity`
    /// read back from `SQLite`) into a [`ShardContentIdentity`], rejecting
    /// every length other than exactly 32 bytes rather than silently
    /// truncating/padding. Storage-boundary callers must fail closed on a
    /// malformed row instead of feeding unchecked bytes into the
    /// canonical XOR accumulator; this pure decoder only reports the
    /// length mismatch, leaving how that failure is classified (e.g. as
    /// a corrupted `SQLite` row) to the storage-boundary caller.
    pub fn decode(bytes: &[u8]) -> Result<Self, ShardContentIdentityDecodeError> {
        let array: [u8; 32] =
            bytes
                .try_into()
                .map_err(|_| ShardContentIdentityDecodeError::InvalidLength {
                    actual: bytes.len(),
                })?;
        Ok(Self(array))
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// A persisted [`ShardContentIdentity`] byte slice was not exactly 32
/// bytes long.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ShardContentIdentityDecodeError {
    #[error("invalid shard identity length: {actual} bytes, expected 32")]
    InvalidLength { actual: usize },
}

/// The canonical whole-desired-state identity depends only on the lock
/// format version and every shard's `(shard_id,
/// ShardContentIdentity)` pair, XOR-accumulated in any order by
/// [`CanonicalDesiredIdentity::toggle_shard`] -- the only whole-lock
/// identity algorithm in the codebase.
///
/// A collision-resistant 256-bit fingerprint/accumulator, not a
/// mathematically injective encoding of the shard set: equality is used
/// throughout the codebase as the generation proof that two observations
/// describe the same desired state, on the strength of BLAKE3's collision
/// resistance, not a guarantee that any two distinct desired states are
/// provably assigned distinct 256-bit accumulators.
///
/// Represented as a domain-separated **XOR set accumulator** over
/// per-shard components (see `shard_identity_component`), not an
/// ordered fold over a sorted whole-shard collection: XOR is
/// associative and commutative, so this is independent of shard
/// enumeration/iteration/partitioning order without ever having to sort
/// or retain an ordered `(shard_id, identity)` vector. This is what makes
/// *incremental* maintenance "sparse update"-friendly: updating a
/// persisted accumulator for a k-shard change costs O(k) --
/// `k` calls to [`Self::toggle_shard`] -- never O(total shard count). A
/// *full, from-scratch* recomputation is a different operation with a
/// different cost, left to the root crate's live filesystem observation
/// (embarrassingly parallel per-shard components combined by associative
/// XOR reduction).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CanonicalDesiredIdentity([u8; 32]);

impl CanonicalDesiredIdentity {
    /// The identity element of the XOR set accumulator: the canonical
    /// identity of a desired state with zero shards. Never a special
    /// case anywhere else in the codebase -- a freshly created database
    /// seeds its desired fingerprint with exactly these bytes
    /// (`zeroblob(32)`), and a from-scratch parallel reduction over zero
    /// shards produces this same value by construction, so an
    /// empty/never-tracked repository's persisted and independently
    /// observed identities agree immediately, with no special-cased
    /// empty-catalog fold required to make that true.
    #[must_use]
    pub const fn empty() -> Self {
        Self([0u8; 32])
    }

    /// Wraps an already-computed identity's raw 32 bytes directly (e.g.
    /// read back from a persisted desired fingerprint column, maintained
    /// incrementally via [`Self::toggle_shard`] -- no rehashing).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Combine two accumulator values with XOR -- the one binary
    /// operation this whole-lock identity is built from. Associative and
    /// commutative over the fixed 32-byte identity, so partial
    /// accumulators computed independently (by different shards, by
    /// different parallel workers, sequentially or not) can always be
    /// combined in any order or grouping and reach the same result.
    /// Allocation-free: this is exactly the fixed-size byte-wise XOR its
    /// signature promises, never a hash recomputation.
    #[must_use]
    pub fn xor(self, other: Self) -> Self {
        let mut out = [0u8; 32];
        for (o, (a, b)) in out.iter_mut().zip(self.0.iter().zip(other.0.iter())) {
            *o = a ^ b;
        }
        Self(out)
    }

    /// Toggle one shard's contribution into (or out of) this
    /// accumulator: XOR-combine `self` with `shard_identity_component(shard_id,
    /// identity)`. Because XOR is self-inverting, calling this twice with
    /// the same `(shard_id, identity)` returns to the original value --
    /// so a sparse mutation's incremental update is always exactly
    /// "toggle out the prior contribution (if any),
    /// toggle in whatever contribution is there now (if any)", never a
    /// full recomputation. Used identically by sequential sparse updates
    /// and by combining independently computed per-shard/per-worker
    /// partials into one running accumulator.
    #[must_use]
    pub fn toggle_shard(self, shard_id: LockShardId, identity: &ShardContentIdentity) -> Self {
        self.xor(Self(shard_identity_component(shard_id, identity)))
    }
}

impl std::fmt::Debug for CanonicalDesiredIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CanonicalDesiredIdentity({})",
            &blake3::Hash::from(self.0).to_hex()[..12]
        )
    }
}

/// One shard's domain-separated contribution to a [`CanonicalDesiredIdentity`]:
/// `BLAKE3(domain || framed shard_id || framed ShardContentIdentity)`. The
/// one place this component hash is computed -- every caller that needs
/// to add or remove a shard's contribution to the whole-lock XOR
/// accumulator goes through [`CanonicalDesiredIdentity::toggle_shard`],
/// which calls this, rather than any of them hashing shard identities
/// independently.
///
/// Domain-separating on the lock format version ([`LOCK_VERSION`]) keeps
/// shard layout/version implicit in the shard set itself rather than
/// requiring separate whole-lock aggregate metadata: a lock-format
/// version bump changes every shard's component, so it is expected (with
/// overwhelming probability, from BLAKE3's collision resistance, not as a
/// mathematical guarantee) to change the whole accumulator too, without a
/// dedicated version field anywhere in [`CanonicalDesiredIdentity`].
///
/// The domain string plus an explicit `shard_id` length prefix frame the
/// only variable-length input here; `identity` is always exactly 32
/// BLAKE3 bytes ([`ShardContentIdentity`] is fixed-width), so it needs no
/// length prefix of its own to stay unambiguous.
fn shard_identity_component(shard_id: LockShardId, identity: &ShardContentIdentity) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gat.lock canonical desired identity shard component blake3 v1");
    hasher.update(LOCK_VERSION.as_bytes());
    hasher.update(&[0u8]);
    // Feeds the exact same length-prefixed canonical shard-ID text bytes
    // the pre-`LockShardId` `String`-based hashing fed here -- moving
    // this algorithm from the root crate must not change a single byte
    // of this hash input.
    shard_id.hash_into(&mut hasher);
    hasher.update(identity.as_bytes());
    *hasher.finalize().as_bytes()
}
