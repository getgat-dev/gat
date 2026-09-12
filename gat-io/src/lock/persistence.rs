//! Flat/sharded on-disk persistence for `gat.lock`: shape detection,
//! full and incremental writes, atomic file publication, and the
//! `gat.lock/` shard-directory layout. The semantic model (`Entry`,
//! `Lock`, parsing/display, path scope matching) lives in
//! `gat_core::lock`; this module only knows how those values are
//! read from and written to disk.

use super::{
    Entry, Lock, LockDomainError, LockError, LockShardId, LockShardLevels, PersistenceError,
};
use gat_core::lock::{LockShardIdError, LockShardLevelsError};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::path::{Path, PathBuf};

pub use super::Result;

/// Wrap a shard parse failure ([`Lock::parse`]) with the shard file path
/// that failed, as [`LockError::CorruptShard`], retaining the complete
/// typed source for technical inspection.
fn wrap_shard_parse_error(path: &Path, err: impl Into<LockError>) -> LockError {
    LockError::CorruptShard {
        path: path.to_path_buf(),
        source: Box::new(err.into()),
    }
}

/// Translate a malformed shard-ID spelling ([`LockShardIdError`]) into
/// [`LockError::CorruptShard`] with the physical path this module
/// discovered it at -- the semantic type itself carries no `PathBuf`
/// (see [`LockShardId::parse_canonical`]'s own doc comment), so this is
/// the one place that context gets attached for filesystem callers.
fn wrap_shard_id_error(path: &Path, err: LockShardIdError) -> LockError {
    LockError::CorruptShard {
        path: path.to_path_buf(),
        source: Box::new(err),
    }
}

/// Translate a physical shard-tree depth beyond
/// [`LockShardLevels::MAX`] (discovered by walking `gat.lock/`'s
/// directory structure, never trusted directly) into
/// [`LockError::CorruptShard`] -- a depth that deep cannot have been
/// written by this build, so it is treated exactly like any other
/// structurally malformed shard tree rather than reaching a later
/// `expect`.
fn wrap_shard_depth_error(path: &Path, err: LockShardLevelsError) -> LockError {
    LockError::CorruptShard {
        path: path.to_path_buf(),
        source: Box::new(err),
    }
}

/// Translate a [`LockError::MixedShardTopology`] discovered while
/// enumerating a live `gat.lock/` tree into [`LockError::CorruptShard`]
/// with the physical directory this module discovered it under -- the
/// semantic classifier ([`shard_topology`]) carries no `PathBuf`, so this
/// is the one place filesystem callers attach that context.
fn wrap_shard_topology_error(path: &Path, err: LockError) -> LockError {
    LockError::CorruptShard {
        path: path.to_path_buf(),
        source: Box::new(err),
    }
}

pub type SparseShardPublish = (Vec<ShardEvidence>, Vec<LockShardId>);

/// This shard's path relative to `<root>/gat.lock`, for filesystem
/// access -- `None` for the flat sentinel (the flat shard *is*
/// `<root>/gat.lock`, not a path nested under it). Only ever called
/// at the point a shard file actually needs to be opened, created, or
/// removed. Kept in this module rather than on [`LockShardId`] itself:
/// converting a shard identity to an actual filesystem path is always an
/// explicit, on-demand step at this module's IO boundary (see
/// [`LockShardId`]'s own doc comment), and this adapter depends only on
/// [`LockShardId::prefix_bytes`], never on its private fields.
fn relative_shard_path(shard_id: &LockShardId) -> Option<PathBuf> {
    let prefix = shard_id.prefix_bytes();
    let (last, leading) = prefix.split_last()?;
    let mut rel = PathBuf::new();
    for byte in leading {
        rel.push(format!("{byte:02x}"));
    }
    rel.push(format!("{last:02x}.tsv"));
    Some(rel)
}

/// [`LockError::UnsupportedOnDiskKind`] for a symlink found where
/// `gat.lock`/a shard leaf must be a regular file or directory --
/// shared by every no-follow leaf-kind probe in this module:
/// a symlink masquerading as the managed object must fail outright
/// rather than be silently followed).
pub fn symlink_error(path: &std::path::Path) -> LockError {
    LockError::UnsupportedOnDiskKind {
        path: path.to_path_buf(),
        detail: "a symlink; gat.lock must be a regular file or directory, not a symlink"
            .to_string(),
    }
}

/// [`LockError::UnsupportedOnDiskKind`] for a leaf that is neither a
/// regular file nor a directory (FIFO, socket, device, ...).
pub fn unsupported_kind_error(path: &std::path::Path) -> LockError {
    LockError::UnsupportedOnDiskKind {
        path: path.to_path_buf(),
        detail: "not a regular file or directory; gat.lock must be a regular file or \
                 directory, not a FIFO/socket/device/other special object"
            .to_string(),
    }
}

/// Directly delegates to [`crate::file_state::coherent_observation`],
/// generic over this module's own typed [`LockError`] (via `LockError`'s
/// `#[from] FileStateError`) so `op`'s failure and `coherent_observation`'s
/// own stat-race detection both stay fully typed end to end -- no
/// `anyhow` round-trip, no downcast recovery.
fn coherent_read_to_string(
    path: &Path,
    op: impl FnOnce() -> Result<String>,
) -> Result<crate::file_state::CoherentObservation<String>> {
    crate::file_state::coherent_observation(path, op)
}

/// Bucketed shard entries plus the full set of shard-relative paths that
/// should exist afterward, as [`prepare_sharded_buckets`] returns
/// it -- factored into a named alias purely to keep that signature (and
/// its `into_par_iter` callers) legible.
type ShardBuckets = (
    BTreeMap<LockShardId, Vec<Entry>>,
    std::collections::HashSet<std::path::PathBuf>,
);

/// A reshape proven necessary by an unlocked shape observation and confirmed
/// while holding the repository-wide lock. Applying it retains that lock in
/// the returned capability so a caller can coherently observe the newly
/// published lock before allowing another writer to proceed.
#[must_use = "a prepared lock reshape must be applied or dropped explicitly"]
pub struct PendingLockReshape {
    root: std::path::PathBuf,
    target: LockShardLevels,
    lock: crate::atomic::RepoLock,
}

/// Proof that a reshape completed while the repository-wide lock remains
/// held. Dropping this value releases the lock.
#[must_use = "keep the completed reshape alive while observing its result"]
pub struct CompletedLockReshape {
    _lock: crate::atomic::RepoLock,
}

impl PendingLockReshape {
    /// Load the complete logical lock exactly once and transactionally
    /// publish it at the target depth.
    pub fn apply(self) -> Result<CompletedLockReshape> {
        #[cfg(any(test, feature = "test-support"))]
        super::test_support::record_reshape_full_load();

        let lock = load(&self.root)?;
        reshape_transactional(&self.root, &lock, self.target)?;
        Ok(CompletedLockReshape { _lock: self.lock })
    }
}

/// Prepare a reshape only when the live representation differs from
/// `target`. The ordinary missing/matching path performs one shape
/// observation and acquires no lock. A mismatch acquires the repository-wide
/// lock and rechecks the shape before returning a capability,
/// so a concurrent reshape cannot cause an unnecessary full load.
pub(crate) fn begin_reshape(
    layout: &crate::RepositoryLayout,
    target: LockShardLevels,
) -> Result<Option<PendingLockReshape>> {
    let root = layout.root_path();
    match on_disk_shape(root)? {
        Some(shape) if shape.shard_levels() != target => {}
        Some(_) | None => return Ok(None),
    }

    let lock = crate::atomic::RepoLock::acquire_repository(layout)?;
    match on_disk_shape(root)? {
        Some(shape) if shape.shard_levels() != target => Ok(Some(PendingLockReshape {
            root: root.to_path_buf(),
            target,
            lock,
        })),
        Some(_) | None => Ok(None),
    }
}

/// Publish the complete logical lock at `target`, transactionally reshaping
/// an existing mismatched representation before writing the final bytes.
pub(crate) fn publish_complete(
    layout: &crate::RepositoryLayout,
    lock: &Lock,
    target: LockShardLevels,
) -> Result<()> {
    let root = layout.root_path();
    let _guard = crate::atomic::RepoLock::acquire_repository(layout)?;
    match on_disk_shape(root)? {
        Some(shape) if shape.shard_levels() != target => {
            reshape_transactional(root, lock, target)?;
            save_for_shape(lock, root, OnDiskShape::for_levels(target))
        }
        Some(shape) => save_for_shape(lock, root, shape),
        None => save(lock, root, target),
    }
}

/// Evidence-returning sibling of [`publish_complete`].
pub(crate) fn publish_complete_with_evidence(
    layout: &crate::RepositoryLayout,
    lock: &Lock,
    target: LockShardLevels,
) -> Result<FullLockEvidence> {
    let root = layout.root_path();
    let _guard = crate::atomic::RepoLock::acquire_repository(layout)?;
    match on_disk_shape(root)? {
        Some(shape) if shape.shard_levels() != target => {
            reshape_transactional(root, lock, target)?;
            save_for_shape_with_publication(lock, root, OnDiskShape::for_levels(target))
        }
        Some(shape) => save_for_shape_with_publication(lock, root, shape),
        None => save_with_publication(lock, root, target),
    }
}

/// Neutral evidence describing one shard file [`publish_rendered_shard`]
/// (or a caller built on top of it) has just published, or has just
/// coherently confirmed already holds the target content: the shard's
/// logical id, its canonical content identity, and a
/// [`crate::file_state::StatProof`] describing exactly the bytes now on
/// disk. `proof` is never optional: a successful
/// [`publish_rendered_shard`] call always establishes a concrete,
/// reusable proof (whether by minting one from a just-completed atomic
/// write or by carrying one forward from a coherent, provably-paired
/// read/stat) -- a missing, non-regular, or otherwise unprovable outcome
/// is a hard error instead of a partially-trustworthy success.
///
/// Opaque outside `gat-io`: fields are private,
/// read only through [`Self::shard_id`], [`Self::identity`], and
/// [`Self::proof`] -- a caller consumes exactly the semantic/accelerator
/// values it needs (which shard, its content identity, its stat-cache
/// proof) without being able to reconstruct or pattern-match this type's
/// own representation.
pub(crate) struct ShardEvidence {
    shard_id: LockShardId,
    identity: super::ShardContentIdentity,
    proof: crate::file_state::StatProof,
}

impl ShardEvidence {
    /// The logical shard this evidence describes: the flat `"gat.lock"`
    /// sentinel, or a sharded `"gat.lock/../..tsv"` id.
    pub(crate) const fn shard_id(&self) -> LockShardId {
        self.shard_id
    }

    /// The published shard's canonical BLAKE3 content identity.
    pub(crate) const fn identity(&self) -> super::ShardContentIdentity {
        self.identity
    }

    /// The [`crate::file_state::StatProof`] describing exactly the bytes
    /// now on disk for this shard -- never a stale/unprovable proof; see
    /// this type's own doc comment.
    pub(crate) const fn proof(&self) -> crate::file_state::StatProof {
        self.proof
    }
}

/// One shard's [`ShardEvidence`] together with the exact `Entry` rows a
/// full-`gat.lock` save just rendered and published for it, as produced
/// by [`save_with_publication`] and its siblings. Carrying the
/// entries alongside the evidence (rather than just the evidence alone)
/// is what lets [`FullLockEvidence`] seed/rebuild a desired-state mirror
/// straight from the write that just happened, with no second read/parse
/// of the shard file it just wrote.
///
/// Opaque outside `gat-io`: read only through
/// [`Self::shard_id`], [`Self::identity`], [`Self::proof`], and
/// [`Self::into_entries`].
pub(crate) struct FullShardEvidence {
    evidence: ShardEvidence,
    entries: Vec<Entry>,
}

impl FullShardEvidence {
    /// The logical shard this evidence describes.
    pub(crate) const fn shard_id(&self) -> LockShardId {
        self.evidence.shard_id()
    }

    /// The published shard's canonical content identity.
    pub(crate) const fn identity(&self) -> super::ShardContentIdentity {
        self.evidence.identity()
    }

    /// The stat-cache proof [`Self::shard_id`]'s shard file was just
    /// published/confirmed with.
    pub(crate) const fn proof(&self) -> crate::file_state::StatProof {
        self.evidence.proof()
    }

    /// Take ownership of the exact rows rendered into this shard.
    pub(crate) fn into_entries(self) -> Vec<Entry> {
        self.entries
    }
}

/// The complete evidence produced by one full-`gat.lock` save (flat:
/// exactly one shard; sharded: one per shard file actually
/// published/confirmed) -- what [`save_with_publication`] and its siblings
/// return so the engine can update its desired-state mirror straight from
/// the write it just
/// performed, instead of re-reading/re-hashing/re-stat'ing `gat.lock`
/// (or asking [`on_disk_shape`] again) afterward.
///
pub struct FullLockEvidence {
    shards: Vec<FullShardEvidence>,
}

impl FullLockEvidence {
    /// Take ownership of every shard this save published or confirmed.
    pub(crate) fn into_shards(self) -> Vec<FullShardEvidence> {
        self.shards
    }
}

/// Which write behavior [`publish_rendered_shard`] uses for one shard
/// leaf.
pub enum ShardPublishPolicy {
    /// Unconditionally publish: a full from-scratch save/reshape, where
    /// every target shard file is rewritten regardless of what (if
    /// anything) is already on disk. Never pays for tier 1/2's stat,
    /// read, or compare below.
    AlwaysWrite,
    /// Skip the write when the shard leaf can be proven to already hold
    /// `rendered`'s bytes, accelerated by an optional prior
    /// `(identity, proof)` record from the caller's own catalog -- an
    /// incremental/sparse publish, where an unchanged shard should cost
    /// as little as possible.
    SkipIfUnchanged {
        prior: Option<(super::ShardContentIdentity, crate::file_state::StatProof)>,
    },
}

/// Publish one already-rendered shard file at `path`: the write-side
/// mirror of [`super::resolve_shard_identity`], sharing its tiered
/// stat-first -> coherent-observation -> hash-once policy so every domain
/// that publishes a desired-lock shard (flat, sharded-full, and sparse
/// writers) goes through one function instead of duplicating the
/// unchanged-content decision three times.
///
/// - **Tier 1** (`SkipIfUnchanged` with a prior proof that still matches
///   the shard's current no-follow stat, zero content reads):
///   - `prior`'s identity equals `identity` (the target content is
///     already what's published): return `prior`'s proof immediately.
///   - `prior`'s identity differs from `identity` (the target content is
///     known to have changed): the matching proof already establishes
///     the current bytes are exactly `prior`'s content, so there is
///     nothing to gain by reading them again before overwriting --
///     publish `rendered` immediately via one atomic write.
/// - **Tier 2** (`SkipIfUnchanged` only, on a prior-proof miss or absent
///   prior): inspect the existing leaf, no-follow.
///   - Missing: falls straight through to tier 3, no read attempted.
///   - Regular file, size differs from `rendered`: a definitive
///     zero-content-read inequality shortcut (the same one
///     [`super::IdentityCheck::Hashed`]'s size-mismatch case uses on the
///     read side) -- falls through to tier 3 without opening the file.
///   - Regular file, size matches: exactly one
///     [`crate::file_state::coherent_observation`] read, compared
///     byte-for-byte against `rendered` (not re-hashed -- direct
///     equality is sufficient and cheaper): equal returns the
///     observation's own proof with no write (the proof is provably
///     paired with the exact bytes just compared); unequal falls through
///     to tier 3.
///   - Symlink, directory, or other non-regular object: a structural
///     error. Unlike a regular-file content mismatch, this is never
///     silently overwritten by falling through to tier 3 -- a symlink or
///     directory masquerading as a shard leaf means something has
///     corrupted the canonical on-disk representation, and publishing
///     over it would mask that instead of surfacing it. Explicit
///     flat<->sharded shape transitions remove the expected conflicting
///     representation themselves before calling this.
/// - **Tier 3** (publish): atomically publish `rendered` via
///   [`crate::atomic::write_atomic_with_proof`] and return its proof.
///
/// A pre/post metadata mismatch observed by tier 2's coherent read (an
/// actual concurrent mutation, not just a leaf-type mismatch) fails this
/// call outright rather than retrying -- there is no retry loop inside
/// this primitive.
fn publish_rendered_shard(
    path: &Path,
    shard_id: LockShardId,
    rendered: &str,
    identity: super::ShardContentIdentity,
    policy: ShardPublishPolicy,
) -> Result<ShardEvidence> {
    let evidence = |proof: crate::file_state::StatProof| ShardEvidence {
        shard_id,
        identity,
        proof,
    };

    if let ShardPublishPolicy::SkipIfUnchanged {
        prior: Some((prior_identity, prior_proof)),
    } = &policy
    {
        let current = crate::file_state::observe_regular_file_no_follow(path);
        if current.is_some_and(|c| c.matches(prior_proof)) {
            if *prior_identity == identity {
                return Ok(evidence(*prior_proof));
            }
            let published = crate::atomic::write_atomic_with_proof(path, rendered)?;
            return Ok(evidence(published.proof));
        }
    }

    let always_write = matches!(policy, ShardPublishPolicy::AlwaysWrite);
    let proof = publish_if_changed(path, rendered, always_write, true)?
        .expect("publish_if_changed(.., want_proof: true) always returns Some");
    Ok(evidence(proof))
}

/// Proof-agnostic counterpart of [`publish_rendered_shard`]: publish
/// `rendered` at `path` under the same tier-2/tier-3 rule (unconditional
/// write for a full reshape, otherwise skip the write when one coherent
/// read proves the existing regular file already holds those exact
/// bytes), but without computing or returning a
/// [`super::ShardContentIdentity`]/[`crate::file_state::StatProof`] pair
/// at all -- a caller with no prior-proof catalog to accelerate tier 1
/// with, and no interest in the evidence a successful publish would
/// establish, never pays for the `ShardContentIdentity` BLAKE3 hash that
/// evidence would require, nor for the extra fstat
/// [`crate::atomic::write_atomic_with_proof`] would otherwise take on the
/// tier-3 write branch purely to mint a [`crate::file_state::StatProof`]
/// this caller would immediately discard. [`save`],
/// [`save_matching_disk_shape`], and [`save_file_atomic`] all
/// route through this instead of through [`publish_rendered_shard`] and
/// discarding its result.
fn publish_rendered_shard_content(path: &Path, rendered: &str, always_write: bool) -> Result<()> {
    publish_if_changed(path, rendered, always_write, false).map(|_| ())
}

/// Shared tier-2/tier-3 core of both [`publish_rendered_shard`] and
/// [`publish_rendered_shard_content`]: with `always_write`, publish
/// `rendered` unconditionally; otherwise inspect the existing leaf
/// no-follow (missing falls through to a write with no read attempted; a
/// regular file whose size already differs from `rendered` falls through
/// with no read either; a regular file whose size matches gets exactly
/// one [`crate::file_state::coherent_observation`] read compared
/// byte-for-byte against `rendered`; a symlink/directory/other
/// non-regular leaf is a structural error rather than a silent
/// overwrite). Returns the [`crate::file_state::StatProof`] describing
/// the bytes now on disk -- from the coherent read when it proved them
/// unchanged, or from the atomic write when it didn't -- as `Some` when
/// `want_proof`, or always `None` when the caller doesn't need it.
///
/// `want_proof` only changes the cost of the tier-3 write branch: the
/// tier-2 coherent-read branch's proof comes from
/// [`crate::file_state::coherent_observation`]'s own mandatory
/// before/after stat (needed for its correctness check regardless), so
/// returning it costs nothing extra either way. The tier-3 write branch,
/// though, chooses between [`crate::atomic::write_atomic_with_proof`]
/// (one extra fstat on the freshly written temp file, to mint a proof)
/// and plain [`crate::atomic::write_atomic`] (no extra fstat) based on
/// `want_proof`, so a proof-agnostic caller ([`publish_rendered_shard_content`])
/// never pays for a stat it would only discard.
fn publish_if_changed(
    path: &Path,
    rendered: &str,
    always_write: bool,
    want_proof: bool,
) -> Result<Option<crate::file_state::StatProof>> {
    if !always_write {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => {
                // Zero-content-read inequality shortcut: a size mismatch
                // alone already proves the existing bytes differ from
                // `rendered`, so there is nothing to gain by opening the
                // file before falling through to an atomic write.
                if meta.len() == rendered.len() as u64 {
                    let observation = coherent_read_to_string(path, || {
                        #[cfg(test)]
                        race_test_hooks::fire_before_read(path);
                        std::fs::read_to_string(path)
                            .map_err(|source| LockError::io("reading", path, source))
                    })?;
                    if observation.value == rendered {
                        return Ok(Some(observation.proof));
                    }
                }
            }
            Ok(_) => {
                return Err(LockError::UnsupportedOnDiskKind {
                    path: path.to_path_buf(),
                    detail: "not a regular file -- refusing to publish a shard over a symlink/directory/special leaf".to_string(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Missing: fall straight through to a write, no read attempted.
            }
            Err(e) => {
                return Err(e).map_err(|source| LockError::io("stat'ing", path, source));
            }
        }
    }

    if want_proof {
        let published = crate::atomic::write_atomic_with_proof(path, rendered)?;
        Ok(Some(published.proof))
    } else {
        crate::atomic::write_atomic(path, rendered)?;
        Ok(None)
    }
}

/// Which validated interrupted-reshape candidate an explicit maintenance
/// command chose to recover from. Deliberately narrow and separately named:
/// callers must already have decided whether they are restoring the old
/// authoritative backup or promoting the staged new representation, rather
/// than reaching for any generic "auto recover" primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReshapeRecoveryChoice {
    RestoreBackup,
    PromoteStaged,
}

/// Which shape `<root>/gat.lock` is actually written in *right now*, as
/// opposed to whatever `gat.yaml`'s `lock.shard_levels` currently asks
/// for -- the two only ever agree once something reshapes it (see
/// `Repo::reshape_lock`, called by every command that touches `gat.lock`:
/// `add`/`rm`/`mv` via `Repo::save_lock`, and `gat sync` directly).
/// `Sharded` carries the depth actually found on disk, since a config
/// edit that changes the depth doesn't retroactively change what's
/// already written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum OnDiskShape {
    Flat,
    Sharded(LockShardLevels),
}

impl OnDiskShape {
    /// The `shard_levels` value that would leave this shape unchanged if
    /// passed to [`save`].
    pub const fn shard_levels(self) -> LockShardLevels {
        match self {
            Self::Flat => LockShardLevels::FLAT,
            Self::Sharded(levels) => levels,
        }
    }

    /// The inverse of `Self::shard_levels`: the shape a `gat.yaml`
    /// `lock.shard_levels` of `levels` calls for (flat -> [`Self::Flat`],
    /// otherwise [`Self::Sharded`]). Flat is the one-shard case, so this is
    /// the one place that maps a validated fan-out
    /// depth to its shape, instead of every caller re-deriving the same
    /// `if levels.is_flat() { Flat } else { Sharded(levels) }` by hand.
    pub const fn for_levels(levels: LockShardLevels) -> Self {
        if levels.is_flat() {
            Self::Flat
        } else {
            Self::Sharded(levels)
        }
    }
}

/// The single semantic classifier for "what shape is this *complete*
/// discovered set of [`LockShardId`]s written in" -- shared by every
/// persisted-boundary reader (Git's `LockSnapshot`, live filesystem
/// discovery's [`observe_full_lock_with_evidence`]) instead of each
/// deriving its own notion of shape from a maximum depth or an arbitrary
/// single id.
///
/// A completed `gat.lock` tree is always uniform: every
/// tracked path lives at the flat sentinel, or every tracked path lives
/// at the exact same sharded fan-out depth. This rejects both ways a
/// discovered set can fail that invariant -- the flat sentinel alongside
/// sharded leaves, and sharded leaves at more than one depth -- as
/// [`LockError::MixedShardTopology`] rather than silently picking a
/// shape from whichever id happened to be seen first or the maximum
/// depth present. An empty set (nothing tracked yet) reports
/// [`OnDiskShape::Flat`], the degenerate zero-shard case.
pub(crate) fn shard_topology(ids: impl IntoIterator<Item = LockShardId>) -> Result<OnDiskShape> {
    let mut seen: Option<LockShardLevels> = None;
    for id in ids {
        let levels = id.levels();
        match seen {
            None => seen = Some(levels),
            Some(first) if first == levels => {}
            Some(first) => {
                return Err(LockError::MixedShardTopology {
                    first_levels: first.get(),
                    second_levels: levels.get(),
                });
            }
        }
    }
    Ok(OnDiskShape::for_levels(
        seen.unwrap_or(LockShardLevels::FLAT),
    ))
}

/// Validate that a complete set of logical shard IDs has one uniform
/// topology and return its semantic shard depth. An empty set is flat.
pub fn shard_levels_for_ids(ids: impl IntoIterator<Item = LockShardId>) -> Result<LockShardLevels> {
    shard_topology(ids).map(OnDiskShape::shard_levels)
}

/// One shard file backing the current `gat.lock` (flat or sharded), as
/// seen by the incremental desired-state indexer: every lock shape,
/// including the flat one-file case, is exposed uniformly as 0..N shard
/// files so refresh/planning never has to branch on lock shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShardFile {
    /// Stable logical identifier for this shard, and the key for
    /// look it up in the `SQLite` stat-cache accelerator (see
    /// `desired_index::shard_change`): `"gat.lock"` for the flat
    /// case, or the shard's path relative to the repo root (e.g.
    /// `"gat.lock/12/34.tsv"`) when sharded.
    pub(crate) shard_id: LockShardId,
    /// Absolute path to the shard file on disk.
    pub(crate) full_path: std::path::PathBuf,
}

/// Enumerate every shard file currently backing `<root>/gat.lock`, in the
/// uniform `ShardFile` shape the desired-state indexer consumes. A flat
/// `gat.lock` is the degenerate one-shard case; nothing tracked yet
/// (missing file/directory) is zero shards. Order is unspecified.
///
/// This is a universal live-lock read entry point (used by
/// `StateStore::refresh_desired_state`, the main path
/// `status`/`ls-files`/every other reconciliation consumer reads current
/// desired state through), so it must detect a reshape transaction left
/// behind by a killed process the same way [`on_disk_shape`]/[`load`]
/// do: without that, the crash window between a reshape's first rename
/// (live -> backup) and its second (staged -> live) would otherwise read
/// as "nothing tracked" here. Detection only runs on the missing-live-path
/// slow path -- an existing live `gat.lock` is guaranteed to already be
/// either the complete old or new representation (see
/// [`reshape::fail_if_pending_reshape`]'s doc comment), so the common case
/// pays no extra `read_dir` at all. A genuinely interrupted transaction is
/// never repaired here -- this fails closed instead (see
/// [`reshape::fail_if_pending_reshape`]).
pub(crate) fn list_shard_files(root: &Path) -> Result<Vec<ShardFile>> {
    let path = root.join("gat.lock");
    if let Some(files) = list_shard_files_at(root, &path)? {
        return Ok(files);
    }
    reshape::fail_if_pending_reshape(root)?;
    Ok(list_shard_files_at(root, &path)?.unwrap_or_default())
}

/// `None` means the live path is currently missing -- either nothing's
/// ever been tracked, or an interrupted reshape transaction that
/// [`reshape::fail_if_pending_reshape`] has already ruled out; the caller
/// only retries after that check has passed.
///
/// No-follow throughout: a symlink masquerading as either the
/// flat `gat.lock` file or the `gat.lock/` shard directory itself must
/// fail desired-state observation outright rather than be silently
/// followed and read as though it were the real managed object.
fn list_shard_files_at(root: &Path, path: &Path) -> Result<Option<Vec<ShardFile>>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).map_err(|source| LockError::io("reading", path, source)),
    };
    if meta.file_type().is_symlink() {
        return Err(symlink_error(path));
    }
    if meta.is_file() {
        return Ok(Some(vec![ShardFile {
            shard_id: LockShardId::flat(),
            full_path: path.to_path_buf(),
        }]));
    }
    if !meta.is_dir() {
        return Err(unsupported_kind_error(path));
    }
    let mut files = Vec::new();
    shard::collect_files(path, &mut files)?;
    let files = files
        .into_iter()
        .map(|full_path| {
            let rel = full_path
                .strip_prefix(root)
                .expect("shard file is under repo root")
                .to_str()
                .ok_or_else(|| LockError::NonUtf8Path {
                    path: full_path.clone(),
                })?
                .replace(std::path::MAIN_SEPARATOR, "/");
            Ok(ShardFile {
                shard_id: LockShardId::parse_canonical(&rel)
                    .map_err(|err| wrap_shard_id_error(&full_path, err))?,
                full_path,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // A completed live `gat.lock/` directory is always uniformly
    // sharded: reject a mixed-depth tree here, the same
    // way Git-persisted discovery does, instead of letting downstream
    // readers (state-store refresh, evidence collection) silently treat
    // whichever depth they happen to notice first as "the" shape.
    shard_topology(files.iter().map(|file| file.shard_id))
        .map_err(|err| wrap_shard_topology_error(path, err))?;
    Ok(Some(files))
}

/// Recovery-side counterpart of `load` + a `_with_publication`
/// save: observes the exact bytes/identity/proof of every currently-live
/// shard file under `<root>/gat.lock` via one coherent read per shard
/// (list -> `par_iter` -> coherent read/hash/parse), instead of obtaining
/// evidence by loading, then republishing (even a no-op tier-2
/// "already matches" publish) a canonical file that's already durably on
/// disk and doesn't need to be written again. A crash-recovery/repair
/// caller that's about to trust whatever's currently on disk as
/// canonical (`mount::txn::recover_pending_mount_txn`,
/// `system::state::repair`) uses this to seed its desired-state mirror
/// directly, with no filesystem mutation at all.
///
/// Performs the same cross-shard duplicate-path and
/// shard-placement-matches-hash validation [`load_sharded`]
/// performs on an ordinary load, since this reads the very same on-disk
/// representation that validation guards -- a corrupt/hand-edited shard
/// tree is rejected here exactly as it would be by a normal `load`.
///
pub(super) fn observe_full_lock_with_evidence(root: &Path) -> Result<FullLockEvidence> {
    let shard_files = list_shard_files(root)?;
    // `list_shard_files` already rejects a mixed-depth tree
    // (`list_shard_files_at`'s `shard_topology` check), so this shape is
    // only ever derived from a discovered set that has already passed
    // uniform-topology validation -- never from an arbitrary "first"
    // shard's own depth.
    shard_topology(shard_files.iter().map(|file| file.shard_id))?;

    let shards: Vec<FullShardEvidence> = shard_files
        .par_iter()
        .map(|shard_file| -> Result<FullShardEvidence> {
            let path = &shard_file.full_path;
            let observation = coherent_read_to_string(path, || {
                #[cfg(any(test, feature = "test-support"))]
                super::test_support::record_full_lock_evidence_shard_read();
                std::fs::read_to_string(path)
                    .map_err(|source| LockError::io("reading", path, source))
            })?;
            let identity = super::identity::hash_shard_bytes(observation.value.as_bytes());
            let parsed =
                Lock::parse(&observation.value).map_err(|err| wrap_shard_parse_error(path, err))?;
            Ok(FullShardEvidence {
                evidence: ShardEvidence {
                    shard_id: shard_file.shard_id,
                    identity,
                    proof: observation.proof,
                },
                entries: parsed.entries,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Cross-shard duplicates first, matching the same "a path belongs to
    // exactly one shard" error `load_sharded` gives a corrupt/
    // hand-edited tree.
    let mut seen_paths: BTreeSet<gat_core::lexical_path::GatPath> = BTreeSet::new();
    for shard in &shards {
        for entry in &shard.entries {
            if !seen_paths.insert(entry.path.clone()) {
                return Err(LockDomainError::PathInMultipleShards {
                    path: entry.path.as_str().to_string(),
                }
                .into());
            }
        }
    }
    // Once every path is known to belong to exactly one shard, check it's
    // the shard the hash placement `shard_id_for_path` predicts.
    for shard in &shards {
        let levels = shard_levels_from_id(&shard.evidence.shard_id);
        for entry in &shard.entries {
            let expected_shard_id = shard_id_for_path(&entry.path, levels);
            if expected_shard_id != shard.evidence.shard_id {
                return Err(LockDomainError::MisplacedShardRow {
                    path: entry.path.as_str().to_string(),
                    actual: shard.evidence.shard_id.to_canonical_string(),
                    expected: expected_shard_id.to_canonical_string(),
                }
                .into());
            }
        }
    }
    super::validate_no_path_directory_conflicts(&seen_paths)?;

    Ok(FullLockEvidence { shards })
}

/// counterpart of [`list_shard_files`]: flat is the degenerate one-shard
/// case, so callers never branch on lock shape.
///
/// `keep` is applied while each shard is parsed
/// ([`Lock::visit_filtered`]), not afterward: hash sharding gives a
/// path-scoped read no locality to narrow *which* shards to visit (every
/// shard must still be opened), but a row a narrow `--path`/
/// `--include`/`--exclude` selection discards is never turned into an
/// owned [`Entry`] in the first place. A caller with nothing
/// to narrow by (e.g. `gc`, which needs every row) passes `|_| true`.
///
/// Unlike [`load`] this deliberately does **not** build the
/// whole-repo path set rejects a path tracked by two different
/// shards: that check is inherently O(total rows) in memory, and the
/// callers that stream (e.g. `gc` reachability, which only unions object
/// ids) are unaffected by a duplicate path -- a duplicate can only make
/// them keep more, never less. Per-shard parse/format validation is
/// unchanged. Callers that need the cross-shard invariant enforced must
/// keep using [`load`].
#[cfg(test)]
pub(crate) fn visit_lock_rows(
    root: &Path,
    keep: impl Fn(&str) -> bool,
    mut visit: impl FnMut(&Entry) -> Result<()>,
) -> Result<()> {
    for shard in list_shard_files(root)? {
        let text = std::fs::read_to_string(&shard.full_path)
            .map_err(|source| LockError::io("reading", &shard.full_path, source))?;
        let mut visit_err: Option<LockError> = None;
        let outcome = super::visit_filtered_matching(
            &text,
            |path| keep(path),
            |entry| {
                visit(&entry).map_err(|err| {
                    visit_err = Some(err);
                    gat_core::lock::LockError::CallbackFailed
                })
            },
        );
        if let Some(err) = visit_err {
            return Err(err);
        }
        outcome.map_err(|err| wrap_shard_parse_error(&shard.full_path, err))?;
    }
    Ok(())
}

/// Source-side counterpart of [`visit_lock_rows`] that preserves the full
/// validation contract of [`load_sharded`] while still filtering rows
/// as each shard is parsed.
///
/// A bounded row decoder for the multi-shard merge. Complete validation happens
/// in the merge's first pass; consumer callbacks run only in its second pass.
/// Flat files use a retained certified view instead, so they are read and decoded once.
struct ShardLineCursor {
    reader: std::io::BufReader<std::fs::File>,
    path: std::path::PathBuf,
    line: String,
    line_num: usize,
}

impl ShardLineCursor {
    fn open(path: &Path) -> Result<Self> {
        #[cfg(any(test, feature = "test-support"))]
        super::test_support::record_shard_text_read();
        let file =
            std::fs::File::open(path).map_err(|source| LockError::io("reading", path, source))?;
        let mut reader = std::io::BufReader::new(file);
        let mut header = String::new();
        std::io::BufRead::read_line(&mut reader, &mut header)
            .map_err(|source| LockError::io("reading", path, source))?;
        if !header.ends_with('\n') || gat_core::newline::strip_terminator(&header) != super::VERSION
        {
            return Err(LockDomainError::UnsupportedVersion {
                expected: super::VERSION.to_string(),
                got: header,
            }
            .into());
        }
        Ok(Self {
            reader,
            path: path.to_path_buf(),
            line: String::new(),
            line_num: 1,
        })
    }

    /// The next row as owned `(path, oid)`, or `None` at end of file.
    /// Each row is validated exactly as [`gat_core::lock::validated::ValidatedLockFile`]
    /// validates it, but ordering/duplicate/prefix bookkeeping across
    /// shards is the caller's job (it needs to merge-walk several of
    /// these cursors together), not this type's. `oid` is already the
    /// decoded [`gat_core::oid::Oid`] `parse_row` produces -- a fixed
    /// 32-byte `Copy` value, cheaper to carry through the merge heap
    /// than its 64-character hex text and never re-decoded downstream.
    fn next_row(&mut self) -> Result<Option<(String, gat_core::oid::Oid)>> {
        self.line.clear();
        let n = std::io::BufRead::read_line(&mut self.reader, &mut self.line)
            .map_err(|source| LockError::io("reading", &self.path, source))?;
        if n == 0 {
            return Ok(None);
        }
        self.line_num += 1;
        let line = self
            .line
            .strip_suffix('\n')
            .ok_or(LockDomainError::MalformedRow {
                line: self.line_num,
                reason: gat_core::lock::MalformedRowReason::MissingLineFeed,
            })?;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (path, oid) = super::parse_row(line, self.line_num)?;
        Ok(Some((path.into_owned(), oid)))
    }
}

/// One shard's pending head row in the k-way merge below, ordered by
/// `path` first (then `shard_idx` as an arbitrary but deterministic
/// tiebreak) so a [`std::collections::BinaryHeap`] of these can select
/// the next row across every shard in `O(log shards)` instead of a
/// linear scan over every shard's head each time.
struct MergeHead {
    path: String,
    oid: gat_core::oid::Oid,
    shard_idx: usize,
}

impl PartialEq for MergeHead {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.shard_idx == other.shard_idx
    }
}
impl Eq for MergeHead {}
impl PartialOrd for MergeHead {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MergeHead {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        #[cfg(any(test, feature = "test-support"))]
        super::test_support::record_merge_head_comparison();
        self.path
            .cmp(&other.path)
            .then_with(|| self.shard_idx.cmp(&other.shard_idx))
    }
}

/// One open k-way merge over every shard's [`ShardLineCursor`]: a
/// min-heap of pending per-shard head rows, refilled from the shard that
/// contributed the row just popped. Bounded by `shards.len()` heap
/// entries plus one pending row per shard -- never a whole shard's rows,
/// let alone the whole tree's.
///
/// [`visit_lock_rows_validated`] opens two of these in turn over the
/// same `shards` slice: [`validate_ordered_merge`] uses one to prove
/// order/placement/cross-shard-conflict validity with nothing retained
/// past the current row, and only once that succeeds does
/// [`emit_ordered_merge`] open a second, fresh one to actually call
/// `keep`/`visit` incrementally.
struct ShardMerge {
    cursors: Vec<ShardLineCursor>,
    heap: BinaryHeap<Reverse<MergeHead>>,
}

impl ShardMerge {
    fn open(shards: &[ShardFile]) -> Result<Self> {
        let mut cursors = shards
            .iter()
            .map(|shard| ShardLineCursor::open(&shard.full_path))
            .collect::<Result<Vec<_>>>()?;
        let mut heap: BinaryHeap<Reverse<MergeHead>> = BinaryHeap::new();
        for (shard_idx, cursor) in cursors.iter_mut().enumerate() {
            if let Some((path, oid)) = cursor
                .next_row()
                .map_err(|err| wrap_shard_parse_error(&shards[shard_idx].full_path, err))?
            {
                heap.push(Reverse(MergeHead {
                    path,
                    oid,
                    shard_idx,
                }));
            }
        }
        Ok(Self { cursors, heap })
    }

    /// The globally next row across every shard (refilling that shard's
    /// head from its cursor), or `None` once every shard is exhausted.
    fn next(
        &mut self,
        shards: &[ShardFile],
    ) -> Result<Option<(String, gat_core::oid::Oid, usize)>> {
        let Some(Reverse(MergeHead {
            path,
            oid,
            shard_idx,
        })) = self.heap.pop()
        else {
            return Ok(None);
        };
        if let Some((next_path, next_oid)) = self.cursors[shard_idx]
            .next_row()
            .map_err(|err| wrap_shard_parse_error(&shards[shard_idx].full_path, err))?
        {
            self.heap.push(Reverse(MergeHead {
                path: next_path,
                oid: next_oid,
                shard_idx,
            }));
        }
        Ok(Some((path, oid, shard_idx)))
    }

    /// The 1-based line number of the row [`Self::next`] just returned
    /// for `shard_idx`, for duplicate-row error messages.
    fn line_num(&self, shard_idx: usize) -> usize {
        self.cursors[shard_idx].line_num
    }
}

fn check_shard_placement(
    shard: &ShardFile,
    levels: LockShardLevels,
    path: &str,
) -> gat_core::lock::Result<()> {
    // `path` comes straight from a validated row cursor/visitor, so hash the
    // borrowed canonical text without allocating a temporary `GatPath`.
    let expected_shard_id = gat_core::lock::validated::shard_id_for_path(path, levels);
    if expected_shard_id != shard.shard_id {
        return Err(LockDomainError::MisplacedShardRow {
            path: path.to_string(),
            actual: shard.shard_id.to_canonical_string(),
            expected: expected_shard_id.to_canonical_string(),
        }
        .into());
    }
    Ok(())
}

/// Certify ordering, placement, uniqueness and directory conflicts across shards
/// before the emission pass invokes consumer callbacks. Unordered input is invalid.
fn validate_ordered_merge(
    shards: &[ShardFile],
    exact_path: Option<&str>,
    narrow_enabled: bool,
) -> Result<bool> {
    let mut merge = ShardMerge::open(shards)?;
    let mut open: Vec<(String, usize)> = Vec::new();
    let mut last_path_per_shard: Vec<Option<String>> = vec![None; shards.len()];
    let mut exact_found = false;

    while let Some((path, _oid, shard_idx)) = merge.next(shards)? {
        if let Some(last) = &last_path_per_shard[shard_idx]
            && path.as_str() < last.as_str()
        {
            return Err(LockDomainError::MalformedRow {
                line: merge.line_num(shard_idx),
                reason: gat_core::lock::MalformedRowReason::UnorderedPath { path },
            }
            .into());
        }
        last_path_per_shard[shard_idx] = Some(path.clone());

        let shard = &shards[shard_idx];
        let levels = shard_levels_from_id(&shard.shard_id);
        check_shard_placement(shard, levels, &path)?;

        let line_num = merge.line_num(shard_idx);
        super::check_ordered_row_conflict(
            &mut open,
            &path,
            shard_idx,
            |path, same_owner| {
                if same_owner {
                    LockDomainError::DuplicatePath {
                        path: path.to_string(),
                        line: Some(line_num),
                    }
                    .into()
                } else {
                    LockDomainError::PathInMultipleShards {
                        path: path.to_string(),
                    }
                    .into()
                }
            },
            |path, ancestor| {
                LockDomainError::DirectoryPrefixConflict {
                    ancestor: ancestor.to_string(),
                    descendant: path.to_string(),
                }
                .into()
            },
        )?;

        if narrow_enabled && exact_path == Some(path.as_str()) {
            exact_found = true;
        }

        open.push((path, shard_idx));
        #[cfg(any(test, feature = "test-support"))]
        super::test_support::observe_retained_ordered_rows(open.len());
    }

    Ok(exact_found)
}

/// The emission pass behind [`visit_lock_rows_validated`]'s ordered fast
/// path, run only once [`validate_ordered_merge`] has already proven the
/// whole tree valid and ordered. Re-merges the same shards from fresh
/// cursors and calls `keep`/`visit` immediately per selected row --
/// nothing beyond the current row and the shard-head heap is ever
/// retained here, so peak state does not grow with how many rows the
/// selection matches.
fn emit_ordered_merge(
    shards: &[ShardFile],
    exact_path: Option<&str>,
    narrow_to_exact: bool,
    keep: &impl Fn(&str) -> bool,
    visit: &mut impl FnMut(&Entry) -> Result<()>,
) -> Result<()> {
    let mut merge = ShardMerge::open(shards)?;
    while let Some((path, oid, _shard_idx)) = merge.next(shards)? {
        if narrow_to_exact && exact_path != Some(path.as_str()) {
            continue;
        }
        if keep(&path) {
            visit(&super::entry_from_validated_parts(path, oid))?;
        }
    }
    Ok(())
}

pub(crate) fn visit_lock_rows_validated(
    root: &Path,
    exact_path: Option<&str>,
    keep: impl Fn(&str) -> bool,
    mut visit: impl FnMut(&Entry) -> Result<()>,
) -> Result<()> {
    let mut shards = list_shard_files(root)?;
    shards.sort_by_key(|a| a.shard_id);

    if let [shard] = shards.as_slice() {
        let text = coherent_read_to_string(&shard.full_path, || {
            std::fs::read_to_string(&shard.full_path)
                .map_err(|source| LockError::io("reading", &shard.full_path, source))
        })?
        .value;
        #[cfg(any(test, feature = "test-support"))]
        super::test_support::record_shard_text_read();
        let view = gat_core::lock::validated::ValidatedLockFile::parse(&text)
            .map_err(|err| wrap_shard_parse_error(&shard.full_path, err))?;
        let levels = shard_levels_from_id(&shard.shard_id);
        for (path, _) in view.rows() {
            check_shard_placement(shard, levels, path)?;
        }
        let exact_found =
            exact_path.is_some_and(|exact| view.rows().any(|(path, _)| path == exact));
        #[cfg(any(test, feature = "test-support"))]
        if exact_found {
            super::test_support::record_selected_shard_parse();
        }
        for (path, oid) in view.rows() {
            if (!exact_found || exact_path == Some(path)) && keep(path) {
                visit(&super::entry_from_validated_parts(path, oid))?;
            }
        }
        return Ok(());
    }

    // Narrowing to a single confirmed `exact_path` row is only
    // unambiguous when every shard shares one fan-out depth (so
    // `shard_id_for_path` predicts one consistent shard for it); a mixed
    // tree (e.g. mid-reshape) disables narrowing entirely, same as
    // before.
    let narrow_enabled = exact_path.is_some()
        && shards
            .iter()
            .map(|shard| shard_levels_from_id(&shard.shard_id))
            .collect::<BTreeSet<_>>()
            .len()
            == 1;

    let exact_found = validate_ordered_merge(&shards, exact_path, narrow_enabled)?;
    let narrow_to_exact = narrow_enabled && exact_found;
    #[cfg(any(test, feature = "test-support"))]
    if narrow_to_exact {
        super::test_support::record_selected_shard_parse();
    }
    emit_ordered_merge(&shards, exact_path, narrow_to_exact, &keep, &mut visit)?;

    Ok(())
}

/// Detects the shape (and, if sharded, the fan-out depth) `<root>/gat.lock`
/// is actually written in, purely from what's on disk -- never from
/// `gat.yaml`. `None` means nothing's been written yet (a fresh repo, or
/// every tracked file has been removed and the directory pruned away).
///
/// Used by [`save_matching_disk_shape`] (so a single-entry
/// `add`/`rm`/`mv` write, via `Repo::save_lock`, only rewrites the shard
/// file(s) that entry's change actually touches instead of paying to
/// rewrite the whole tree) and by `Repo::reshape_lock` (so a reshape --
/// called both by `Repo::save_lock`, ahead of every entry write, and
/// directly by `gat sync` -- is a no-op whenever `lock.shard_levels`
/// already matches what's on disk).
pub(crate) fn on_disk_shape(root: &Path) -> Result<Option<OnDiskShape>> {
    let live_path = root.join("gat.lock");
    if let Some(shape) = on_disk_shape_at(&live_path)? {
        return Ok(Some(shape));
    }
    // Slow path only: the live path is missing, which is either the
    // ordinary "nothing tracked yet" case or a reshape transaction left
    // behind by a killed process (the crash window between its two
    // commit renames). This only detects that ambiguity purely from disk
    // state and fails closed if it's the latter -- an existing live path
    // never needs this check (see `list_shard_files`'s doc comment), and
    // an interrupted transaction is never repaired automatically (see
    // `reshape::fail_if_pending_reshape`).
    reshape::fail_if_pending_reshape(root)?;
    on_disk_shape_at(&live_path)
}

/// No-follow leaf-kind probe for a canonical publication boundary that
/// must never silently follow a symlink masquerading as the expected
/// representation: `Some(true)` for a regular file,
/// `Some(false)` for a directory, `None` if nothing is there yet at all.
/// A symlink or other non-regular, non-directory object is a hard error
/// rather than silently treated as a file, a directory, or absent --
/// unlike `Path::is_file`/`Path::is_dir`, which both follow symlinks.
fn no_follow_leaf_kind(path: &Path) -> Result<Option<bool>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(symlink_error(path)),
        Ok(meta) if meta.is_file() => Ok(Some(true)),
        Ok(meta) if meta.is_dir() => Ok(Some(false)),
        Ok(_) => Err(unsupported_kind_error(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).map_err(|source| LockError::io("reading", path, source)),
    }
}

/// The shape-detection half of [`on_disk_shape`], generalized to an
/// arbitrary file/directory path rather than always `<root>/gat.lock` --
/// shared with [`reshape::reshape_transactional`], which needs to detect
/// the shape it just staged at a transaction-scoped path before ever
/// touching the live `gat.lock` location.
pub(crate) fn on_disk_shape_at(path: &Path) -> Result<Option<OnDiskShape>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).map_err(|source| LockError::io("reading", path, source)),
    };
    if meta.file_type().is_symlink() {
        return Err(symlink_error(path));
    }
    if meta.is_file() {
        return Ok(Some(OnDiskShape::Flat));
    }
    if !meta.is_dir() {
        return Err(unsupported_kind_error(path));
    }
    shard::first_file(path)?
        .map(|file| {
            let depth = file
                .strip_prefix(path)
                .expect("found file is under gat.lock/")
                .components()
                .count();
            let depth = u8::try_from(depth).unwrap_or(u8::MAX);
            LockShardLevels::new(depth)
                .map(OnDiskShape::Sharded)
                .map_err(|err| wrap_shard_depth_error(path, err))
        })
        .transpose()
}

/// The current on-disk `gat.lock` shape (flat or sharded), but only when it
/// already matches `target` (a caller-supplied `lock.shard_levels`) --
/// flat is the degenerate one-shard case, so a matching flat
/// shape is just as eligible for the sparse, touched-shard-scoped
/// mutation pipeline as a matching sharded one. A pending
/// reshape-on-config-change returns `None` so the caller falls back to
/// the full-`Lock` rewrite path (which reshapes as a side effect), rather
/// than taking the sparse fast path against a shape that's about to
/// change anyway.
///
/// Private: production callers acquire the repo-wide lock and resolve
/// this shape atomically (see [`LockWriteGuard`]/[`LockStore::acquire_matching_shape`])
/// rather than observing the shape first and locking afterward, which
/// would leave a window for a concurrent reshape to invalidate the
/// observation before the lock is even held.
fn current_matching_lock_shape(
    root: &Path,
    target: LockShardLevels,
) -> Result<Option<OnDiskShape>> {
    Ok(match on_disk_shape(root)? {
        Some(shape) if shape.shard_levels() == target => Some(shape),
        _ => None,
    })
}

/// Same as [`current_matching_lock_shape`], but also returns the repo-wide
/// [`crate::atomic::RepoLock`] guard already acquired to answer it, so a
/// caller that goes on to publish through the sparse, touched-shard
/// pipeline can keep holding that very same guard across the whole "read
/// shape, choose which shard(s) changed, publish" window instead of only
/// serializing each primitive's own already-locked write. `RepoLock::acquire_repository`
/// is reentrant on the same thread, so the nested acquisitions
/// `save_sparse_shards`/`publish_flat_shard`/reshape make while this guard
/// is held are a cheap no-op, not a second OS-level lock cycle.
fn current_matching_lock_shape_locked(
    layout: &crate::RepositoryLayout,
    target: LockShardLevels,
) -> Result<(crate::atomic::RepoLock, Option<OnDiskShape>)> {
    let root = layout.root_path();
    let guard = crate::atomic::RepoLock::acquire_repository(layout)?;
    let shape = current_matching_lock_shape(root, target)?;
    Ok((guard, shape))
}

/// Same locking contract as [`current_matching_lock_shape_locked`], but
/// treats "nothing tracked on disk yet" (no `gat.lock` file/tree at all) as
/// eligible for the sparse, touched-shard-scoped publication pipeline too,
/// using the shape `target` calls for ([`OnDiskShape::for_levels`]). There
/// is no existing shape to conflict with in that case -- publishing sparse
/// shards simply creates the tree fresh -- so the only reason left to fall
/// back to the established full-`Lock` rewrite is a genuine shape
/// *mismatch* (an on-disk shape that doesn't match `target`, which the
/// fallback also reshapes as a side effect) or a pending reshape
/// transaction (still surfaced as an error by [`on_disk_shape`], exactly
/// as in the non-fresh case).
fn current_or_target_lock_shape_locked(
    layout: &crate::RepositoryLayout,
    target: LockShardLevels,
) -> Result<(crate::atomic::RepoLock, Option<OnDiskShape>)> {
    let root = layout.root_path();
    let guard = crate::atomic::RepoLock::acquire_repository(layout)?;
    let shape = match on_disk_shape(root)? {
        Some(shape) if shape.shard_levels() == target => Some(shape),
        Some(_) => None,
        None => Some(OnDiskShape::for_levels(target)),
    };
    Ok((guard, shape))
}

/// Holds the repository lock so the observed `gat.lock` shape stays stable
/// until this guard is dropped. Mutation callers use
/// [`Self::can_publish_incrementally`] to select sparse publication or a
/// complete rewrite without accessing the physical representation.
pub struct LockWriteGuard {
    // Held for the full lifetime of the shape observation.
    _guard: crate::atomic::RepoLock,
    shape: Option<OnDiskShape>,
}

impl LockWriteGuard {
    /// `true` when the sparse, touched-shard-scoped publication pipeline
    /// applies (a matching on-disk shape, or -- for
    /// `LockStore::acquire_current_or_target_shape` -- nothing tracked
    /// yet, including the flat shape). `false` means the caller must fall
    /// back to the full-`Lock` rewrite (which also performs any pending
    /// reshape as a side effect).
    #[must_use]
    pub const fn can_publish_incrementally(&self) -> bool {
        self.shape.is_some()
    }

    /// `true` when the sparse pipeline applies *and* the resolved shape is
    /// specifically the flat, single-`gat.lock`-file shape (as opposed to
    /// a genuinely sharded `gat.lock/` tree) -- used only where a caller's
    /// publication strategy itself differs between flat and sharded.
    pub(crate) const fn is_flat(&self) -> bool {
        matches!(self.shape, Some(OnDiskShape::Flat))
    }

    /// The resolved shape's `shard_levels`, or `None` when
    /// [`Self::can_publish_incrementally`] is `false`. Lets a caller
    /// destructure the incremental-vs-full-rewrite decision in one step
    /// (`let Some(levels) = shape_lock.shard_levels() else { /* full
    /// rewrite */ }`) instead of checking a separate boolean before
    /// unwrapping.
    pub(crate) fn shard_levels(&self) -> Option<LockShardLevels> {
        self.shape.map(OnDiskShape::shard_levels)
    }
}

/// [`current_matching_lock_shape_locked`], wrapped as [`LockWriteGuard`] so
/// callers never see the `OnDiskShape` it resolved -- only its opaque
/// capabilities. The public entry point is
/// [`super::LockStore::acquire_matching_shape`].
pub(crate) fn acquire_matching_shape(
    layout: &crate::RepositoryLayout,
    target: LockShardLevels,
) -> Result<LockWriteGuard> {
    let (guard, shape) = current_matching_lock_shape_locked(layout, target)?;
    Ok(LockWriteGuard {
        _guard: guard,
        shape,
    })
}

/// [`current_or_target_lock_shape_locked`], wrapped as [`LockWriteGuard`] so
/// callers never see the `OnDiskShape` it resolved -- only its opaque
/// capabilities. The public entry point is
/// [`super::LockStore::acquire_current_or_target_shape`].
pub(crate) fn acquire_current_or_target_shape(
    layout: &crate::RepositoryLayout,
    target: LockShardLevels,
) -> Result<LockWriteGuard> {
    let (guard, shape) = current_or_target_lock_shape_locked(layout, target)?;
    Ok(LockWriteGuard {
        _guard: guard,
        shape,
    })
}

/// Load a lock-format file or shard tree from `path`, validating it against
/// the concrete on-disk `shape` the caller already expects there. Shared by
/// transactional reshape recovery/inspection code that needs the same parse
/// and cross-shard invariant checks [`load`] applies, but against a
/// transaction-scoped path rather than the live `<root>/gat.lock` location.
pub(crate) fn load_shape_at(path: &Path, shape: OnDiskShape) -> Result<Lock> {
    match shape {
        OnDiskShape::Flat => load_file(path),
        OnDiskShape::Sharded(_) => load_sharded(path),
    }
}

fn error_chain_has_not_found(err: &LockError) -> bool {
    err.is_not_found_race()
}

#[cfg(test)]
thread_local! {
    /// Test-only seam letting a test pause [`load`]'s missing-path
    /// branch exactly between it confirming the live path is absent and it
    /// calling [`reshape::fail_if_pending_reshape`] -- the crash/race
    /// window a concurrent reshape's second commit rename plus cleanup can
    /// land in. Lets a regression drive that race to completion
    /// deterministically instead of relying on thread-scheduling luck (see
    /// `simulate_crash_after_first_rename` for the analogous seam
    /// reproduce reshape crash windows elsewhere).
    static LOAD_MISSING_PATH_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn set_load_missing_path_hook_for_test(hook: impl FnOnce() + 'static) {
    LOAD_MISSING_PATH_HOOK.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
}

/// Render `entries` into one lock-format shard file exactly the same way
/// every other writer in this module does: serialize with `Lock`'s
/// normal display format, sorted by path. Keeping that "sorted, then
/// rendered" step in one helper means the full-save path
/// ([`save_sharded`]), the sparse touched-shard path
/// ([`save_sparse_shards`]), and any caller computing a shard's content
/// identity from the bytes it just wrote can all rely on one canonical
/// byte representation.
///
/// Every sparse-publish caller already hands this an already-ordered
/// per-shard row set (`desired_rows_by_shard_ids`'s `(shard_id, path)`
/// order), so this checks that cheaply (`O(n)`, no allocation) and
/// renders directly from the borrowed slice instead of always cloning
/// into a fresh, resorted `Vec` -- the clone/sort only actually runs for
/// a caller (e.g. [`save_sharded`]'s per-bucket render) that can't
/// promise its input is already ordered.
fn render_entries(entries: &[Entry]) -> String {
    #[cfg(any(test, feature = "test-support"))]
    super::test_support::record_render_entries_call();
    if entries.is_sorted_by(|a, b| a.path <= b.path) {
        return render_sorted_entries(entries);
    }
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    render_sorted_entries(&sorted)
}

/// Serialize an already-path-sorted entry slice in the canonical
/// lock-format byte representation. Only [`render_entries`]
/// should call this directly -- it does not itself sort or validate.
fn render_sorted_entries(entries: &[Entry]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{}", super::VERSION);
    for e in entries {
        let _ = writeln!(out, "{}\t{}", e.oid, gat_core::lock::EscapedPath(&e.path));
    }
    out
}

/// Read `<root>/gat.lock` from disk. Empty lock if nothing exists yet
/// (nothing tracked so far). Shape (single file vs. sharded
/// `gat.lock/` directory) is detected from what's actually on disk,
/// never from `gat.yaml`'s `lock.shard_levels` -- so a config edit
/// alone never makes a real, already-written `gat.lock/` directory
/// briefly unreadable (see [`save`] for the write side of this
/// same split).
///
/// Dispatches straight off the live path's own file/directory type
/// instead of first calling [`on_disk_shape`]: for a sharded lock,
/// `on_disk_shape`/`on_disk_shape_at` would otherwise descend one
/// branch via `shard::first_file` purely to infer the fan-out depth,
/// which [`load_sharded`] doesn't need (it discovers every
/// shard file itself) and which would pay for an extra `read_dir`
/// traversal on every ordinary sharded read -- a real cost on
/// high-latency/network filesystems. Only the "neither a file nor a
/// directory" case (nothing tracked yet, or an interrupted reshape
/// transaction) needs the slower [`reshape::fail_if_pending_reshape`]
/// check; an existing live path (file or directory) never does.
pub(super) fn load(root: &Path) -> Result<Lock> {
    let path = root.join("gat.lock");
    for attempt in 0..2 {
        let loaded = match no_follow_leaf_kind(&path)? {
            Some(true) => load_file(&path),
            Some(false) => load_sharded(&path),
            None => {
                // Nothing live at `path` yet -- but a concurrent reshape may
                // complete (and clean up its transaction) between the checks
                // above and this point, republishing a live `gat.lock` we'd
                // otherwise miss. `fail_if_pending_reshape` only errors out
                // on a still-*interrupted* transaction; it does not itself
                // guarantee `path` is still absent once it returns, so
                // re-check the live path before concluding there's genuinely
                // nothing tracked.
                #[cfg(test)]
                if let Some(hook) = LOAD_MISSING_PATH_HOOK.with(|cell| cell.borrow_mut().take()) {
                    hook();
                }
                reshape::fail_if_pending_reshape(root)?;
                match no_follow_leaf_kind(&path)? {
                    Some(true) => load_file(&path),
                    Some(false) => load_sharded(&path),
                    None => return Ok(Lock::default()),
                }
            }
        };
        match loaded {
            Ok(lock) => return Ok(lock),
            Err(err) if attempt == 0 && error_chain_has_not_found(&err) => {}
            Err(err) => return Err(err),
        }
    }
    unreachable!("two-attempt retry loop must return or continue at most once")
}

/// Write `<root>/gat.lock` in the shape `shard_levels` calls for: `0`
/// is a single flat file, sorted by path for a stable, diffable file
/// and published atomically. A positive value
/// shards entries across a `gat.lock/` directory of fan-out files
/// (see [`shard::rel_path`]), keyed by the first `shard_levels` bytes
/// of `blake3(path)` -- the same fan-out trick `.gat/objects/` uses
/// for cache objects. Each shard file is written atomically the same
/// way the flat file always was, so concurrent writers touching
/// different shards never interfere; this performs a full
/// rewrite of the whole entry set (never a merge), reshaping between
/// flat/sharded or across depths as needed -- an unconditional reshape
/// like this is expensive enough that it's only ever triggered when
/// the on-disk shape doesn't already match (via `Repo::reshape_lock`,
/// called by both `Repo::save_lock` -- and so every `add`/`rm`/`mv`
/// -- and `gat sync` directly), never unconditionally by routine
/// mutations (see [`save_matching_disk_shape`], which those go
/// through instead once the shape is already up to date).
pub(super) fn save(lock: &Lock, root: &Path, shard_levels: LockShardLevels) -> Result<()> {
    let path = root.join("gat.lock");
    if shard_levels.is_flat() {
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|source| LockError::io("removing sharded", &path, source))?;
        }
        save_file_atomic(lock, &path)
    } else {
        save_sharded(lock, &path, shard_levels, false)
    }
}

/// The evidence-returning sibling of [`save`]: same unconditional
/// reshape/full-rewrite behavior, but returns the [`FullLockEvidence`]
/// the write itself just established (the on-disk shape written, plus
/// identity + [`crate::file_state::StatProof`] + entries for every
/// shard file now on disk), so a caller can update a desired-state
/// mirror directly from this write instead of re-reading/re-hashing
/// `gat.lock` (or asking [`on_disk_shape`] again) afterward.
pub fn save_with_publication(
    lock: &Lock,
    root: &Path,
    shard_levels: LockShardLevels,
) -> Result<FullLockEvidence> {
    let path = root.join("gat.lock");
    if shard_levels.is_flat() {
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|source| LockError::io("removing sharded", &path, source))?;
        }
        Ok(FullLockEvidence {
            shards: vec![save_file_atomic_with_publication(lock, &path)?],
        })
    } else {
        Ok(FullLockEvidence {
            shards: save_sharded_with_publication(lock, &path, shard_levels, false)?,
        })
    }
}

/// Write `<root>/gat.lock`, preserving whatever shape is already on
/// disk (flat vs. sharded, and at whatever depth) instead of reshaping
/// it to match `shard_levels` -- `shard_levels` (`gat.yaml`'s current
/// `lock.shard_levels`) is only consulted the very first time
/// `gat.lock` is written for a repo, when there's nothing on disk yet
/// to preserve. When the on-disk shape is already sharded, only the
/// shard file(s) whose entries actually changed are rewritten (see
/// [`save_sharded`]'s `incremental` flag), so a routine
/// single-file `add`/`rm`/`mv` never pays for a full-tree rewrite just
/// because sharding happens to be configured.
///
/// This is what `Repo::save_lock` (used by `add`/`rm`/`mv`) calls
/// through for the entry write itself, *after* first calling
/// `Repo::reshape_lock` to upgrade the on-disk shape to match
/// `lock.shard_levels` -- so by the time this runs, the on-disk shape
/// this preserves is already current, and a `lock.shard_levels` config
/// change takes effect at the very next command that touches
/// `gat.lock`, not only at the next `gat sync`.
///
/// Genuinely proof-agnostic: unlike routing through
/// [`save_matching_disk_shape_with_publication`] and discarding
/// the result, this never computes a per-shard
/// [`super::ShardContentIdentity`] or [`crate::file_state::StatProof`]
/// that would only be thrown away.
#[cfg(test)]
pub(super) fn save_matching_disk_shape(
    lock: &Lock,
    root: &Path,
    shard_levels: LockShardLevels,
) -> Result<()> {
    match on_disk_shape(root)? {
        None => save(lock, root, shard_levels),
        Some(shape) => save_for_shape(lock, root, shape),
    }
}

/// The proof-agnostic per-shape dispatch [`save_matching_disk_shape`]
/// performs directly once it already knows the on-disk shape --
/// exposed separately so a caller that already has `shape` in hand
/// (for example [`crate::repository::Repo::save_lock`], via
/// `Repo::reshape_lock_to_current_shape`) doesn't need to pay for
/// [`on_disk_shape`]'s directory read a second time just to reach the
/// same dispatch. Never computes a per-shard [`super::ShardContentIdentity`]
/// or [`crate::file_state::StatProof`] that would only be discarded --
/// see [`save_for_shape_with_publication`] for the evidence-
/// returning sibling.
pub(crate) fn save_for_shape(lock: &Lock, root: &Path, shape: OnDiskShape) -> Result<()> {
    match shape {
        OnDiskShape::Flat => save_file_atomic(lock, &root.join("gat.lock")),
        OnDiskShape::Sharded(levels) => save_sharded(lock, &root.join("gat.lock"), levels, true),
    }
}

/// The evidence-returning sibling of the (now-inlined)
/// proof-agnostic per-shape dispatch [`save_matching_disk_shape`]
/// performs directly. Every caller needs the evidence this returns,
/// so there is no separate discard-the-evidence wrapper here.
pub(crate) fn save_for_shape_with_publication(
    lock: &Lock,
    root: &Path,
    shape: OnDiskShape,
) -> Result<FullLockEvidence> {
    match shape {
        OnDiskShape::Flat => Ok(FullLockEvidence {
            shards: vec![save_file_atomic_with_publication(
                lock,
                &root.join("gat.lock"),
            )?],
        }),
        OnDiskShape::Sharded(levels) => Ok(FullLockEvidence {
            shards: save_sharded_with_publication(lock, &root.join("gat.lock"), levels, true)?,
        }),
    }
}

/// Read every shard file under a `gat.lock/` directory (at whatever
/// depth it was written with) and concatenate their entries, in
/// sorted shard-file order so the result is deterministic regardless
/// of directory-read order, matching how a flat `gat.lock`'s rows are
/// already sorted by `save`. Shard files are read and parsed in
/// parallel (`rayon`) since `gat.lock` is read on essentially every
/// command -- with hundreds of shard files, this is the difference
/// between one thread's worth of I/O and every available core's.
/// [`Lock::parse`] already rejects a path appearing twice *within*
/// one shard; a path tracked by two *different* shards (corrupt or
/// hand-edited content -- `save_sharded` never produces this) is
/// rejected here, the same "a tracked path belongs to exactly one
/// shard" invariant a sharded lock must uphold as a flat one -- as is
/// one path being a directory prefix of another across shards, and a
/// row physically living in a shard file other than the one
/// `shard_id_for_path` would place it in (corruption a lazy
/// single-shard lookup such as `LockSnapshot::entry_for_path` trusts
/// without re-checking, so a full load is where it's caught).
fn load_sharded(dir: &Path) -> Result<Lock> {
    let mut shard_files = Vec::new();
    shard::collect_files(dir, &mut shard_files)?;
    shard_files.sort();
    let shards: Vec<(LockShardId, Vec<Entry>)> = shard_files
        .par_iter()
        .map(|file| -> Result<(LockShardId, Vec<Entry>)> {
            let rel = file
                .strip_prefix(dir)
                .expect("shard file is under gat.lock/");
            let shard_text = format!(
                "gat.lock/{}",
                rel.to_str()
                    .ok_or_else(|| LockError::NonUtf8Path { path: file.clone() })?
                    .replace(std::path::MAIN_SEPARATOR, "/")
            );
            let shard_id = LockShardId::parse_canonical(&shard_text)
                .map_err(|err| wrap_shard_id_error(file, err))?;
            let text = std::fs::read_to_string(file)
                .map_err(|source| LockError::io("reading", file, source))?;
            let shard = Lock::parse(&text).map_err(|err| wrap_shard_parse_error(file, err))?;
            Ok((shard_id, shard.entries))
        })
        .collect::<Result<Vec<_>>>()?;

    // A completed live `gat.lock/` directory is always uniformly
    // sharded: reject a mixed-depth tree here, the
    // same way `list_shard_files`/Git-persisted discovery do, before
    // ever reaching the cross-shard-duplicate or placement checks
    // below (both of which assume every shard shares one depth).
    shard_topology(shards.iter().map(|(id, _)| *id))
        .map_err(|err| wrap_shard_topology_error(dir, err))?;

    // Cross-shard duplicates first, in shard-file order, matching the
    // "a path belongs to exactly one shard" error a corrupt/hand-edited
    // tree gets regardless of which of its two conflicting copies this
    // happened to concatenate first (or last).
    let mut seen_paths: BTreeSet<gat_core::lexical_path::GatPath> = BTreeSet::new();
    for (_, entries) in &shards {
        for entry in entries {
            if !seen_paths.insert(entry.path.clone()) {
                return Err(LockDomainError::PathInMultipleShards {
                    path: entry.path.as_str().to_string(),
                }
                .into());
            }
        }
    }

    // Once every path is known to belong to exactly one shard, check
    // it's the shard the hash placement `shard_id_for_path` predicts --
    // corruption a lazy single-shard lookup (e.g.
    // `LockSnapshot::entry_for_path`) trusts without re-checking, so a
    // full load is where it's caught.
    for (shard_id, entries) in &shards {
        let levels = shard_id.levels();
        for entry in entries {
            let expected_shard_id = shard_id_for_path(&entry.path, levels);
            if expected_shard_id != *shard_id {
                return Err(LockDomainError::MisplacedShardRow {
                    path: entry.path.as_str().to_string(),
                    actual: shard_id.to_canonical_string(),
                    expected: expected_shard_id.to_canonical_string(),
                }
                .into());
            }
        }
    }

    super::validate_no_path_directory_conflicts(&seen_paths)?;
    Ok(Lock {
        entries: shards
            .into_iter()
            .flat_map(|(_, entries)| entries)
            .collect(),
    })
}

/// Write entries into a `gat.lock/` directory sharded across
/// `shard_levels` fan-out levels, then remove any leftover shard
/// file/directory that isn't part of this save's target set --
/// covering both "`shard_levels` changed" (stale shards at the old
/// depth) and "some shard emptied out" (a path was removed and its
/// shard now has no rows left). Buckets are written in parallel
/// (`rayon`), the same way `.gat/objects/` ingestion already
/// parallelizes its own I/O.
///
/// When `incremental` is `true` (the `save_matching_disk_shape` path),
/// a shard file is only actually rewritten if its target contents
/// differ from what's already there -- an `add`/`rm`/`mv` touching one
/// path only ever rewrites the one or few shards that path's row(s)
/// landed in, not every shard file. When `false` (the `save`/reshape
/// path), every target shard file is written unconditionally, since a
/// reshape's whole point is a full, from-scratch rewrite.
///
/// Genuinely proof-agnostic: each bucket goes through
/// [`publish_rendered_shard_content`] instead of
/// [`publish_rendered_shard`], so no per-shard
/// [`super::ShardContentIdentity`] is ever computed here only to be
/// discarded.
fn save_sharded(
    lock: &Lock,
    path: &Path,
    shard_levels: LockShardLevels,
    incremental: bool,
) -> Result<()> {
    let (buckets, kept) = prepare_sharded_buckets(lock, path, shard_levels)?;
    buckets
        .into_par_iter()
        .map(|(shard_id, entries)| -> Result<()> {
            let rel = relative_shard_path(&shard_id)
                .expect("sharded bucket keys are never the flat sentinel");
            let file_path = path.join(&rel);
            let rendered = render_entries(&entries);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|source| LockError::io("creating", parent, source))?;
            }
            publish_rendered_shard_content(&file_path, &rendered, !incremental)
        })
        .collect::<Result<Vec<_>>>()?;
    shard::remove_stale(path, &kept)?;
    Ok(())
}

/// Shared bucket-construction step of [`save_sharded`] and
/// [`save_sharded_with_publication`]: validate paths, clear a
/// conflicting flat file (the flat->sharded shape transition),
/// (re)create the shard directory, and fan the current entries out
/// into their target shard-file buckets. Returns the buckets to
/// publish plus the full set of shard-relative paths that should
/// still exist afterward, for [`shard::remove_stale`] to reconcile
/// against.
fn prepare_sharded_buckets(
    lock: &Lock,
    path: &Path,
    shard_levels: LockShardLevels,
) -> Result<ShardBuckets> {
    if path.is_file() {
        std::fs::remove_file(path)
            .map_err(|source| LockError::io("removing flat", path, source))?;
    }
    std::fs::create_dir_all(path).map_err(|source| LockError::io("creating", path, source))?;

    let mut buckets: std::collections::BTreeMap<LockShardId, Vec<Entry>> =
        std::collections::BTreeMap::new();
    for entry in &lock.entries {
        buckets
            .entry(LockShardId::for_path(&entry.path, shard_levels))
            .or_default()
            .push(entry.clone());
    }
    let kept: std::collections::HashSet<_> = buckets
        .keys()
        .map(|shard_id| {
            relative_shard_path(shard_id).expect("sharded bucket keys are never the flat sentinel")
        })
        .collect();
    Ok((buckets, kept))
}

/// The evidence-returning sibling of [`save_sharded`]: same
/// parallel bucket-write/stale-removal shape, but each bucket's body
/// goes through the shared [`publish_rendered_shard`] primitive
/// instead of its own inline read-compare-then-write, and the
/// resulting per-shard [`ShardEvidence`] (identity + proof)
/// is collected and returned instead of discarded.
fn save_sharded_with_publication(
    lock: &Lock,
    path: &Path,
    shard_levels: LockShardLevels,
    incremental: bool,
) -> Result<Vec<FullShardEvidence>> {
    let (buckets, kept) = prepare_sharded_buckets(lock, path, shard_levels)?;

    let published = buckets
        .into_par_iter()
        .map(|(shard_id, mut entries)| -> Result<FullShardEvidence> {
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            let rel = relative_shard_path(&shard_id)
                .expect("sharded bucket keys are never the flat sentinel");
            let file_path = path.join(&rel);
            let rendered = render_entries(&entries);
            let identity = super::identity::hash_shard_bytes(rendered.as_bytes());
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|source| LockError::io("creating", parent, source))?;
            }
            let policy = if incremental {
                ShardPublishPolicy::SkipIfUnchanged { prior: None }
            } else {
                ShardPublishPolicy::AlwaysWrite
            };
            let evidence =
                publish_rendered_shard(&file_path, shard_id, &rendered, identity, policy)?;
            Ok(FullShardEvidence { evidence, entries })
        })
        .collect::<Result<Vec<_>>>()?;
    shard::remove_stale(path, &kept)?;
    Ok(published)
}

/// Read an arbitrary lock-format file (used for `gat.lock` itself via
/// `load`). Empty lock if the file doesn't exist yet.
pub(super) fn load_file(path: &Path) -> Result<Lock> {
    if !path.exists() {
        return Ok(Lock::default());
    }
    let text =
        std::fs::read_to_string(path).map_err(|source| LockError::io("reading", path, source))?;
    Lock::parse(&text).map_err(|err| wrap_shard_parse_error(path, err))
}

/// Read one arbitrary lock document without parsing it. Missing files are
/// represented explicitly so protocol layers can apply their own semantics
/// without a separate existence probe.
pub(super) fn read_file_if_present(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(LockError::io("reading", path, source)),
    }
}

/// Write an arbitrary lock-format file atomically (write to a
/// collision-safe sibling temp file, then rename into place), so a
/// process interrupted mid-write never leaves a truncated/corrupt file
/// at `path`, and concurrent writers to the same path never clobber
/// each other's temp file. Used for both `gat.lock` itself (via
/// [`save`]) and the materialized sync state, which must survive
/// interruption.
///
/// Paths are validated by `GatPath` and escaped by the shared control-escape writer.
///
/// Genuinely proof-agnostic: goes through
/// [`publish_rendered_shard_content`] rather than
/// [`publish_rendered_shard`], so this never computes a
/// [`super::ShardContentIdentity`] BLAKE3 hash purely to discard it.
pub(super) fn save_file_atomic(lock: &Lock, path: &Path) -> Result<()> {
    let rendered = render_entries(&lock.entries);
    publish_rendered_shard_content(path, &rendered, true)
}

/// The evidence-returning sibling of [`save_file_atomic`]: same
/// unconditional atomic write, but goes through the shared
/// [`publish_rendered_shard`] primitive (`AlwaysWrite`, mirroring the
/// always-write behavior [`save_file_atomic`] already had) so
/// the flat-file case returns the same [`ShardEvidence`]
/// shape the sharded path does.
fn save_file_atomic_with_publication(lock: &Lock, path: &Path) -> Result<FullShardEvidence> {
    let mut entries = lock.entries.clone();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let rendered = render_entries(&entries);
    let identity = super::identity::hash_shard_bytes(rendered.as_bytes());
    let evidence = publish_rendered_shard(
        path,
        LockShardId::flat(),
        &rendered,
        identity,
        ShardPublishPolicy::AlwaysWrite,
    )?;
    Ok(FullShardEvidence { evidence, entries })
}

/// Compute the shard file identifier the desired-state mirror stores for one
/// tracked path at `shard_levels` fan-out levels: the same root-relative
/// `"gat.lock/..."` path [`list_shard_files`] later reports back when it
/// re-enumerates the shard tree from disk. Keeping the `SQLite` mirror and the
/// on-disk writer on this one helper avoids duplicating the string-shaping
/// convention in two places and accidentally teaching them different shard IDs
/// for the same tracked path.
/// `shard_levels == 0` is the flat, one-shard case: every path
/// maps to the same literal `"gat.lock"` sentinel [`list_shard_files`] and
/// the desired-state indexer already use for the flat
/// shape, rather than a `gat.lock/`-nested shard file. This lets the
/// scoped `SQLite` desired-state mutation helpers (`upsert_entries`,
/// `remove_prefix`, `move_prefix`) treat flat and sharded identically:
/// they always attach a `desired_shard_id`, and the caller decides how to
/// publish it ([`publish_flat_shard`] vs. [`save_sparse_shards`]).
pub fn shard_id_for_path(
    path: &gat_core::lexical_path::GatPath,
    shard_levels: LockShardLevels,
) -> LockShardId {
    LockShardId::for_path(path, shard_levels)
}

/// The fan-out depth a shard's own logical id implies: `0` for the flat
/// sentinel `"gat.lock"`, or the number of `/`-separated components under
/// `gat.lock/` otherwise (e.g. `"gat.lock/aa/bb.tsv"` is `2`). A shard
/// file's id already encodes the depth it was actually written at, so
/// this never needs `gat.yaml`'s *current* `lock.shard_levels` (which may
/// differ from what's on disk mid-reshape) -- the inverse of
/// [`shard_id_for_path`], for re-checking that a row a caller is about
/// to trust (`load_sharded`'s full-load placement check,
/// `desired_index::refresh`'s incremental counterpart) actually lives in
/// the shard file its own path hashes to.
pub const fn shard_levels_from_id(shard_id: &LockShardId) -> LockShardLevels {
    shard_id.levels()
}

/// Rewrite only the touched shard files of an already-sharded `gat.lock/`
/// tree, leaving every untouched shard file completely alone.
///
/// Callers pass the exact set of shard IDs whose current contents may have
/// changed plus, for the subset that still exist after the mutation, the full
/// post-mutation row set currently belonging in each shard. This is the
/// on-disk counterpart of `SQLite`'s scoped desired-state mutations: a sharded
/// `gat rm`/`gat mv` can update the few shard files those rows actually touch
/// without re-bucketing or diff-checking every other desired row in the repo.
///
/// `priors` carries whatever `(identity, proof)` the caller's own desired
/// mirror already has on record for a subset of `touched_shard_ids` -- when present, it
/// accelerates [`publish_rendered_shard`]'s tier 1. A touched shard with no
/// entry in `priors` (or no `rows_by_shard` entry, unrelated to disk
/// content) simply falls through to tier 2/3. As with
/// [`publish_rendered_shard`]'s tiers generally, a touched shard whose
/// rendered bytes are already identical is left un-rewritten; a touched
/// shard that is now empty has its file deleted and any newly-empty
/// intermediate directories pruned. Untouched shard files are neither read
/// nor written.
pub fn save_sparse_shards(
    layout: &crate::RepositoryLayout,
    touched_shard_ids: &BTreeSet<LockShardId>,
    rows_by_shard: &BTreeMap<LockShardId, Vec<Entry>>,
    priors: &BTreeMap<LockShardId, (super::ShardContentIdentity, crate::file_state::StatProof)>,
) -> Result<SparseShardPublish> {
    let root = layout.root_path();
    let _guard = crate::atomic::RepoLock::acquire_repository(layout)?;
    let base = root.join("gat.lock");
    std::fs::create_dir_all(&base).map_err(|source| LockError::io("creating", &base, source))?;
    let mut removed = Vec::new();
    let mut non_empty = Vec::new();
    for shard_id in touched_shard_ids {
        let rel = relative_shard_path(shard_id).ok_or_else(|| LockError::CorruptShard {
            path: PathBuf::from(shard_id.to_canonical_string()),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid sharded lock shard id",
            )),
        })?;
        match rows_by_shard.get(shard_id) {
            Some(entries) if !entries.is_empty() => non_empty.push((*shard_id, rel)),
            _ => {
                shard::remove_file_and_empty_parents(&base, &rel)?;
                removed.push(*shard_id);
            }
        }
    }
    let published: Vec<Result<ShardEvidence>> = non_empty
        .par_iter()
        .map(|(shard_id, rel)| -> Result<ShardEvidence> {
            let file_path = base.join(rel);
            let entries = rows_by_shard
                .get(shard_id)
                .expect("non-empty shard ids always have rows");
            let rendered = render_entries(entries);
            let identity = super::identity::hash_shard_bytes(rendered.as_bytes());
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|source| LockError::io("creating", parent, source))?;
            }
            publish_rendered_shard(
                &file_path,
                *shard_id,
                &rendered,
                identity,
                ShardPublishPolicy::SkipIfUnchanged {
                    prior: priors.get(shard_id).copied(),
                },
            )
        })
        .collect();
    Ok((published.into_iter().collect::<Result<_>>()?, removed))
}

/// The flat counterpart of [`save_sparse_shards`]: publish `<root>/gat.lock`
/// (a single file, never a `gat.lock/` directory) directly from the ordered
/// row set belonging to the one logical flat shard, instead of
/// materializing a complete [`Lock`] just to call [`save_file_atomic`].
/// Flat is the degenerate one-shard case: rewriting it is
/// inherently `O(total rows)` output, but this still avoids the extra
/// `O(total rows)` *input-side* allocation of rebuilding a whole `Lock`
/// first, and -- like `save_sparse_shards`'s incremental mode -- skips the
/// write entirely when the rendered bytes already match what's on disk.
///
/// `entries` is the complete post-mutation desired row set (already sorted
/// by path); an empty slice means every tracked path was removed, so the
/// file itself is deleted rather than left behind empty. Returns `None` in
/// that case, `Some` with the freshly published shard's identity otherwise.
/// `prior`, when the caller's own desired mirror already has it on record
/// for the flat `"gat.lock"` shard, accelerates [`publish_rendered_shard`]'s
/// tier 1 the same way [`save_sparse_shards`]'s `priors` map does.
pub fn publish_flat_shard(
    layout: &crate::RepositoryLayout,
    entries: &[Entry],
    prior: Option<(super::ShardContentIdentity, crate::file_state::StatProof)>,
) -> Result<Option<ShardEvidence>> {
    let root = layout.root_path();
    let _guard = crate::atomic::RepoLock::acquire_repository(layout)?;
    let path = root.join("gat.lock");
    if entries.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|source| LockError::io("removing empty", &path, source))?;
        }
        return Ok(None);
    }
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_flat_shard_publish(entries.len());
    let rendered = render_entries(entries);
    let identity = super::identity::hash_shard_bytes(rendered.as_bytes());
    // A stale sharded `gat.lock/` directory left over from a prior shape
    // must be cleared before publishing the flat file in its place --
    // `publish_rendered_shard`'s atomic rename can't replace a directory.
    if path.is_dir() {
        std::fs::remove_dir_all(&path)
            .map_err(|source| LockError::io("removing sharded", &path, source))?;
    }
    Ok(Some(publish_rendered_shard(
        &path,
        LockShardId::flat(),
        &rendered,
        identity,
        ShardPublishPolicy::SkipIfUnchanged { prior },
    )?))
}

/// The streaming, destination-bounded counterpart of [`publish_flat_shard`]
/// instead of the caller first materializing the
/// complete post-mutation flat row set into one `Vec<Entry>`
/// (`desired_rows_by_shard_ids`), `next_row` is called repeatedly (`Ok(None)`
/// signals exhaustion; rows must already be in ascending `path` order, the
/// same order the `SQLite` desired-state cursor already yields) and each row
/// is validated, rendered, and written straight to a same-directory temp
/// file as it's produced, feeding the exact same bytes into an
/// incremental BLAKE3 hasher in the same pass. At most one row is ever
/// resident here at a time, so peak destination-side memory for a huge
/// first mount / recovery replay stays a small constant rather than
/// `O(selected rows)`, and (unlike a Git-blob-framed hash, which needs the
/// total length up front) BLAKE3 never requires a second read of the
/// completed temp file to compute its identity: the hasher is finalized
/// once writing is done, and the file is published atomically exactly
/// like [`publish_flat_shard`] (including replacing a stale sharded
/// `gat.lock/` directory left over from a prior shape).
///
/// Deliberately skips [`publish_flat_shard`]'s "unchanged content" short
/// circuit -- checking that would require the previous complete content in
/// hand to compare against, which is exactly the `O(N)` retention this
/// function exists to avoid. Only used by the batched mount replay path,
/// which only runs when at least one row window is actually being
/// imported, so an already-up-to-date flat file is not the common case
/// here the way it is for `publish_flat_shard`'s other callers
/// (`add`/`rm`/`mv`).
pub fn publish_flat_shard_streaming(
    layout: &crate::RepositoryLayout,
    mut next_row: impl FnMut() -> Result<Option<Entry>>,
) -> Result<Option<ShardEvidence>> {
    use std::io::{BufWriter, Write};

    let root = layout.root_path();
    let _guard = crate::atomic::RepoLock::acquire_repository(layout)?;
    let path = root.join("gat.lock");
    let Some(first) = next_row()? else {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|source| LockError::io("removing empty", &path, source))?;
        }
        return Ok(None);
    };
    #[cfg(any(test, feature = "test-support"))]
    let mut row_count = 0usize;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(root)
        .map_err(|source| LockError::io("creating temp file in", root, source))?;
    // Unlike Git's blob framing (which needs the total content length
    // known up front), BLAKE3 can be fed incrementally with no known
    // total length required -- so the hasher is updated with exactly the
    // same bytes as they're written, in the same pass, and finalized once
    // writing is done. There is no second read of the temp file at all.
    let mut hasher = blake3::Hasher::new();
    {
        let mut writer = BufWriter::new(tmp.as_file_mut());
        let mut write_row = |bytes: &[u8]| -> Result<()> {
            hasher.update(bytes);
            writer
                .write_all(bytes)
                .map_err(|source| LockError::io("writing temp file for", &path, source))
        };
        write_row(format!("{}\n", super::VERSION).as_bytes())?;
        let mut row = Some(first);
        while let Some(entry) = row {
            write_row(
                format!(
                    "{}\t{}\n",
                    entry.oid,
                    gat_core::lock::EscapedPath(&entry.path)
                )
                .as_bytes(),
            )?;
            #[cfg(any(test, feature = "test-support"))]
            {
                row_count += 1;
                test_support::observe_max_retained_flat_publish_rows(1);
            }
            row = next_row()?;
        }
        writer
            .flush()
            .map_err(|source| LockError::io("flushing temp file for", &path, source))?;
    }
    let content_hash = super::ShardContentIdentity::from_array(*hasher.finalize().as_bytes());
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| LockError::io("syncing temp file publishing", &path, source))?;
    if path.is_dir() {
        std::fs::remove_dir_all(&path)
            .map_err(|source| LockError::io("removing sharded", &path, source))?;
    }
    let published = crate::atomic::persist_finalized_with_proof(tmp, &path)?;
    #[cfg(any(test, feature = "test-support"))]
    test_support::record_flat_shard_publish(row_count);
    Ok(Some(ShardEvidence {
        shard_id: LockShardId::flat(),
        identity: content_hash,
        proof: published.proof,
    }))
}

/// On-disk fan-out layout for a sharded `gat.lock/` directory: which shard
/// file a given tracked path's row belongs in, and how to enumerate /
/// clean up shard files, all keyed by `blake3(path)` just as typed cache OIDs
/// are placed into their content-addressed fan-out layout.
mod shard {
    use super::LockError;
    use super::Result;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    /// Path, relative to the `gat.lock/` directory, of the shard file that
    /// `path`'s row belongs in at `levels` fan-out levels:
    /// the first `levels` bytes of `blake3(path)`, hex-encoded two
    /// characters per level, nested one directory per level except the
    /// last, which is the `.tsv` file itself. E.g. `levels=1` ->
    /// `xx.tsv`, `levels=2` -> `xx/yy.tsv`.
    #[cfg(test)]
    pub fn rel_path(
        path: &gat_core::lexical_path::GatPath,
        levels: gat_core::lock::LockShardLevels,
    ) -> PathBuf {
        let shard_id = gat_core::lock::LockShardId::for_path(path, levels);
        super::relative_shard_path(&shard_id).expect("non-flat shard has a relative path")
    }

    /// Every shard file under `dir` (any depth), collected as absolute
    /// paths. Order is unspecified (directory-read order); callers that
    /// need determinism (e.g. [`super::load`]) sort the result.
    ///
    /// No-follow: a symlink leaf masquerading as a shard file
    /// (or as an intermediate fan-out directory) fails desired-state
    /// observation outright rather than being silently followed and read
    /// as though it were the real managed shard.
    pub fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in
            std::fs::read_dir(dir).map_err(|source| LockError::io("reading", dir, source))?
        {
            let entry = entry.map_err(|source| LockError::io("reading", dir, source))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|source| LockError::io("reading", &path, source))?;
            if file_type.is_symlink() {
                return Err(super::symlink_error(&path));
            }
            if file_type.is_dir() {
                collect_files(&path, out)?;
            } else if file_type.is_file() {
                out.push(path);
            } else {
                return Err(super::unsupported_kind_error(&path));
            }
        }
        Ok(())
    }

    /// Find one arbitrary shard file under `dir` (any depth), descending
    /// only as far as needed to find it -- unlike [`collect_files`], this
    /// stops as soon as a leaf file is found instead of enumerating the
    /// whole tree, since callers that only need the shape (e.g.
    /// [`super::on_disk_shape`]) only ever look at one file anyway. `Ok(None)`
    /// if `dir` (and everything under it) is empty.
    ///
    /// No-follow, same as [`collect_files`]: a symlink leaf fails instead
    /// of being silently followed, and a FIFO/socket/device/other special
    /// leaf is likewise rejected rather than treated as absent or read as
    /// a canonical shard file.
    pub fn first_file(dir: &Path) -> Result<Option<PathBuf>> {
        for entry in
            std::fs::read_dir(dir).map_err(|source| LockError::io("reading", dir, source))?
        {
            let entry = entry.map_err(|source| LockError::io("reading", dir, source))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|source| LockError::io("reading", &path, source))?;
            if file_type.is_symlink() {
                return Err(super::symlink_error(&path));
            }
            if file_type.is_dir() {
                if let Some(found) = first_file(&path)? {
                    return Ok(Some(found));
                }
            } else if file_type.is_file() {
                return Ok(Some(path));
            } else {
                return Err(super::unsupported_kind_error(&path));
            }
        }
        Ok(None)
    }

    /// Remove every file/directory under `dir` that isn't in `keep`
    /// (relative paths, as produced by [`rel_path`]), and prune any
    /// subdirectory left empty afterward. Called after writing a save's
    /// target shard files, so a `shard_levels` change or an entry moving to
    /// a different shard never leaves a stale file behind.
    pub fn remove_stale(dir: &Path, keep: &HashSet<PathBuf>) -> Result<()> {
        fn walk(base: &Path, dir: &Path, keep: &HashSet<PathBuf>) -> Result<()> {
            for entry in
                std::fs::read_dir(dir).map_err(|source| LockError::io("reading", dir, source))?
            {
                let entry = entry.map_err(|source| LockError::io("reading", dir, source))?;
                let path = entry.path();
                if path.is_dir() {
                    walk(base, &path, keep)?;
                    if std::fs::read_dir(&path)
                        .map_err(|source| LockError::io("reading", &path, source))?
                        .next()
                        .is_none()
                    {
                        std::fs::remove_dir(&path)
                            .map_err(|source| LockError::io("removing", &path, source))?;
                    }
                } else {
                    let rel = path
                        .strip_prefix(base)
                        .expect("walked path is under base")
                        .to_path_buf();
                    if !keep.contains(&rel) {
                        std::fs::remove_file(&path)
                            .map_err(|source| LockError::io("removing", &path, source))?;
                    }
                }
            }
            Ok(())
        }
        walk(dir, dir, keep)
    }

    /// Delete one known shard file (relative to the `gat.lock/` directory) if
    /// it exists, then prune any now-empty parent directories back up toward
    /// `base`. This is the sparse-write counterpart of [`remove_stale`]:
    /// sparse publication already knows exactly which touched shard emptied
    /// out, so it should remove only that one file instead of rescanning the
    /// whole shard tree to rediscover it.
    pub fn remove_file_and_empty_parents(base: &Path, rel: &Path) -> Result<()> {
        let file = base.join(rel);
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).map_err(|source| LockError::io("removing", file, source)),
        }
        let mut cur = file.parent();
        while let Some(dir) = cur {
            if dir == base {
                break;
            }
            if std::fs::read_dir(dir)
                .map_err(|source| LockError::io("reading", dir, source))?
                .next()
                .is_some()
            {
                break;
            }
            std::fs::remove_dir(dir).map_err(|source| LockError::io("removing", dir, source))?;
            cur = dir.parent();
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn gp(s: &str) -> gat_core::lexical_path::GatPath {
            gat_core::lexical_path::GatPath::parse_canonical(s).unwrap()
        }

        #[test]
        fn rel_path_matches_documented_shape() {
            let hash = blake3::hash(b"data/a.bin");
            let b = hash.as_bytes();
            assert_eq!(
                rel_path(
                    &gp("data/a.bin"),
                    gat_core::lock::LockShardLevels::new(1).unwrap(),
                ),
                PathBuf::from(format!("{:02x}.tsv", b[0]))
            );
            assert_eq!(
                rel_path(
                    &gp("data/a.bin"),
                    gat_core::lock::LockShardLevels::new(2).unwrap(),
                ),
                PathBuf::from(format!("{:02x}/{:02x}.tsv", b[0], b[1]))
            );
        }

        #[test]
        fn rel_path_is_deterministic_for_the_same_path_and_levels() {
            let levels = gat_core::lock::LockShardLevels::new(2).unwrap();
            assert_eq!(
                rel_path(&gp("a.bin"), levels),
                rel_path(&gp("a.bin"), levels)
            );
        }
    }
}

/// Crash/cancellation-safe `gat.lock` shape reshaping: stages the complete
/// destination representation outside the live
/// `gat.lock` path, publishes a small transaction record before touching
/// anything live, then commits by renaming the old live representation
/// aside (a backup) and the staged one into place. A transaction left
/// behind by a killed/cancelled process is *detected* by
/// [`fail_if_pending_reshape`], called before every ordinary read of the
/// live path that would otherwise find it missing ([`super::on_disk_shape`],
/// [`super::load`], [`super::list_shard_files`]) -- so an interrupted
/// reshape is never mistaken for an empty lock, and a mixed-depth tree is
/// never accepted as a completed one. Per design decision, an ordinary read
/// never repairs what it finds: it fails closed with a diagnostic instead,
/// since silently restoring/promoting on a normal command's behalf would
/// mutate `gat.lock` state the user never asked to change. Actually
/// resolving the interrupted transaction (e.g. restoring the backup) is
/// deliberately left to a dedicated recovery/maintenance command (tracked
/// separately, e.g. `gat system`) that inspects and classifies recovery
/// state before acting -- this module intentionally does not expose a
/// general-purpose recovery primitive capable of that. The only cleanup
/// this module performs on its own is [`cleanup_completed_reshape`], used
/// internally by [`reshape_transactional`] to discard harmless leftover
/// scratch from a completed reshape before starting a new
/// one -- and it is deliberately incapable of restoring a missing live
/// path from a backup (never reached in the genuinely-interrupted case
/// anyway, since [`super::on_disk_shape`] -- always consulted first by
/// every caller that goes on to reshape -- already fails closed on that
/// state).
mod reshape {
    use super::Result;
    use super::{
        Lock, LockError, LockShardLevels, OnDiskShape, PersistenceError, ReshapeRecoveryChoice,
        load_shape_at, on_disk_shape_at, save_file_atomic, save_sharded,
    };
    use std::path::{Path, PathBuf};

    /// Where every in-flight (or crashed-and-not-yet-recovered) reshape
    /// transaction's scratch state lives, relative to the repo root --
    /// never inside the live `gat.lock`/`gat.lock/` path itself, so a
    /// reader that only ever looks at `gat.lock` can't stumble onto a
    /// half-built transaction by accident.
    fn reshape_root(root: &Path) -> PathBuf {
        root.join(".gat").join("lock-reshape")
    }

    /// Small, self-contained transaction record persisted once, before any
    /// live-path rename, at `<txn dir>/txn.json` -- durable proof that a
    /// reshape got far enough to need recovering rather than just being
    /// abandoned scratch work. `staging_path`/`backup_path` are absolute
    /// (transaction-scoped, ephemeral paths under this same repo, so
    /// portability across repos/machines is never a concern).
    #[derive(serde::Serialize, serde::Deserialize)]
    struct TxnRecord {
        id: String,
        source_shape: OnDiskShape,
        target_shape: OnDiskShape,
        staging_path: PathBuf,
        backup_path: PathBuf,
        /// Always `"prepared"` right now: the one and only state this
        /// record is ever written in -- recorded
        /// anyway, both for forward compatibility and so a malformed or
        /// unrecognized value fails recovery closed instead of silently.
        phase: String,
    }

    const PHASE_PREPARED: &str = "prepared";

    /// Read every entry's `(path, oid)` pair to prove a staged
    /// reshape is semantically equivalent to the lock it was built from
    /// before it's ever allowed to become the live representation.
    fn oid_map(lock: &Lock) -> std::collections::BTreeMap<&str, gat_core::oid::Oid> {
        lock.entries
            .iter()
            .map(|e| (e.path.as_str(), e.oid))
            .collect()
    }

    /// Stage `lock` under a fresh transaction directory in the shape
    /// `target` calls for, validate the staged copy actually matches
    /// (same shape *and* same path -> OID map as `lock`), publish a
    /// transaction record, then commit by renaming the current live
    /// `<root>/gat.lock` aside as a backup and the staged copy into its
    /// place -- so the only two moments the live path itself is
    /// mutated are single, already-atomic renames, and every byte of the
    /// new shape is written and independently verified beforehand.
    ///
    /// Called by `Repo::reshape_lock` only once the on-disk shape is
    /// already known to differ from `target` (see `on_disk_shape`), so
    /// `root.join("gat.lock")` is assumed to already exist here.
    pub fn reshape_transactional(root: &Path, lock: &Lock, target: LockShardLevels) -> Result<()> {
        cleanup_completed_reshape(root)?;

        let live_path = root.join("gat.lock");
        let source_shape = on_disk_shape_at(&live_path)?.ok_or_else(|| {
            LockError::Persistence(PersistenceError::NothingToReshape {
                live_path: live_path.clone(),
            })
        })?;
        let target_shape = OnDiskShape::for_levels(target);
        if source_shape == target_shape {
            return Ok(());
        }

        let (txn_dir, staging_path) =
            prepare_and_commit_first_rename(root, lock, source_shape, target_shape)?;

        // 4b. Second commit rename: promote the staged target into place.
        std::fs::rename(&staging_path, &live_path)
            .map_err(|source| LockError::io("promoting", &staging_path, source))?;
        crate::atomic::sync_dir(root);

        // 5. Cleanup: the transaction is fully committed once the live
        // path holds the new representation, regardless of whether this
        // last step completes -- recovery finishes it if it doesn't.
        cleanup_txn_dir(&txn_dir);
        Ok(())
    }

    /// Steps 1-4a of [`reshape_transactional`]: stage the destination,
    /// validate it, publish the durable transaction record, then run only
    /// the *first* commit rename (live -> backup). Factored out so
    /// [`reshape_transactional`] and the test-only
    /// [`simulate_crash_after_first_rename`] share the exact same
    /// staging/validation/publish logic -- the latter exists purely to let
    /// tests outside this module reproduce the crash window between the
    /// two commit renames without duplicating it. Returns the transaction
    /// directory and staging path so the caller can run (or, for the test
    /// helper, deliberately not run) the second rename.
    fn prepare_and_commit_first_rename(
        root: &Path,
        lock: &Lock,
        source_shape: OnDiskShape,
        target_shape: OnDiskShape,
    ) -> Result<(PathBuf, PathBuf)> {
        let live_path = root.join("gat.lock");
        let root_dir = reshape_root(root);
        std::fs::create_dir_all(&root_dir)
            .map_err(|source| LockError::io("creating", &root_dir, source))?;
        let txn_id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let txn_dir = root_dir.join(&txn_id);
        std::fs::create_dir_all(&txn_dir)
            .map_err(|source| LockError::io("creating", &txn_dir, source))?;
        let staging_path = txn_dir.join("new");
        let backup_path = txn_dir.join("backup");
        let record_path = txn_dir.join("txn.json");

        // 1. Stage the complete destination outside the live path.
        match target_shape {
            OnDiskShape::Flat => save_file_atomic(lock, &staging_path)?,
            OnDiskShape::Sharded(levels) => save_sharded(lock, &staging_path, levels, false)?,
        }

        // 2. Validate before ever touching the live path: staged shape
        // and staged entries must exactly match the source.
        let staged_shape = on_disk_shape_at(&staging_path)?;
        if staged_shape != Some(target_shape) {
            return Err(LockError::Persistence(
                PersistenceError::StagedShapeMismatch {
                    staging_path,
                    actual: staged_shape.map(OnDiskShape::shard_levels),
                    expected: target_shape.shard_levels(),
                },
            ));
        }
        let staged_lock = load_shape_at(&staging_path, target_shape).map_err(|err| {
            LockError::Persistence(PersistenceError::StagedLockInvalid {
                staging_path: staging_path.clone(),
                source: Box::new(err),
            })
        })?;
        if oid_map(&staged_lock) != oid_map(lock) {
            return Err(LockError::Persistence(
                PersistenceError::StagedOidMapMismatch { staging_path },
            ));
        }

        // 3. Persist the transaction record before touching the live path.
        let record = TxnRecord {
            id: txn_id,
            source_shape,
            target_shape,
            staging_path: staging_path.clone(),
            backup_path: backup_path.clone(),
            phase: PHASE_PREPARED.to_string(),
        };
        let record_json = serde_json::to_string_pretty(&record).map_err(|source| {
            LockError::Persistence(PersistenceError::TxnRecordEncode { source })
        })?;
        crate::atomic::write_atomic(&record_path, &record_json)?;
        crate::atomic::sync_dir(&txn_dir);
        crate::atomic::sync_dir(&root_dir);

        // 4a. First commit rename: move the old live representation aside
        // as a backup.
        std::fs::rename(&live_path, &backup_path)
            .map_err(|source| LockError::io("moving aside", &live_path, source))?;
        crate::atomic::sync_dir(root);

        Ok((txn_dir, staging_path))
    }

    fn cleanup_txn_dir(txn_dir: &Path) {
        let _ = std::fs::remove_dir_all(txn_dir);
    }

    /// Test-only: reproduce the exact crash window between a reshape's two
    /// commit renames -- after `rename(live, backup)`, before
    /// `rename(staged, live)` -- for tests in other modules (e.g.
    /// `engine::workspace::sync::desired_index`) that need to prove a *different*
    /// live-lock read entry point recovers a pending reshape before
    /// reading, without reaching into this module's private transaction
    /// internals themselves.
    #[cfg(any(test, feature = "test-support"))]
    pub fn simulate_crash_after_first_rename(
        root: &Path,
        lock: &Lock,
        target_levels: LockShardLevels,
    ) -> Result<()> {
        let live_path = root.join("gat.lock");
        let source_shape = on_disk_shape_at(&live_path)?.ok_or_else(|| {
            LockError::Persistence(PersistenceError::NothingToReshape {
                live_path: live_path.clone(),
            })
        })?;
        let target_shape = OnDiskShape::for_levels(target_levels);
        prepare_and_commit_first_rename(root, lock, source_shape, target_shape)?;
        Ok(())
    }

    /// Discard leftover scratch from reshape transactions that are either
    /// harmless (crashed before ever touching the live path) or already
    /// fully committed (the second commit rename already ran) -- never
    /// invoked automatically from an ordinary read path, only internally
    /// by [`reshape_transactional`] to tidy up before starting a new
    /// transaction. Idempotent, and a cheap no-op (one `read_dir` that
    /// finds nothing) once nothing's pending.
    ///
    /// Deliberately narrow, by design: unlike a general-purpose recovery
    /// primitive, this is *incapable* of restoring a missing live path
    /// from a backup. A transaction interrupted between the two commit
    /// renames (`backup_path` exists, but the live path doesn't) is left
    /// entirely untouched and reported as an error instead of repaired --
    /// [`super::on_disk_shape`] (always consulted first by every caller
    /// that goes on to reshape) already fails closed on that state via
    /// [`fail_if_pending_reshape`], so this function is never actually
    /// reached in the genuinely-interrupted case; the assertion here is
    /// only a second line of defense. Actually resolving that state (by
    /// restoring the backup, or by explicitly accepting an already-staged
    /// target) is deliberately left to a dedicated recovery
    /// command/inspection tool (tracked separately) that classifies and
    /// acts on recovery state explicitly, rather than a helper any
    /// mutation path could reach for.
    pub fn cleanup_completed_reshape(root: &Path) -> Result<()> {
        let root_dir = reshape_root(root);
        let entries = match std::fs::read_dir(&root_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).map_err(|source| LockError::io("reading", &root_dir, source)),
        };
        for entry in entries {
            let entry = entry.map_err(|source| LockError::io("reading", &root_dir, source))?;
            if entry
                .file_type()
                .map_err(|source| LockError::io("reading", entry.path(), source))?
                .is_dir()
            {
                cleanup_completed_txn_dir(root, &entry.path())?;
            }
        }
        Ok(())
    }

    /// Remove only transaction scratch proven disposable: pre-record staging
    /// directories that never touched the live path, prepared records whose
    /// first commit rename never ran and therefore left the live path intact,
    /// and fully committed transactions whose promoted live path already
    /// exists. Interrupted or malformed transactions are preserved exactly as
    /// found for explicit inspection/recovery.
    pub fn clean_disposable_reshape_scratch(root: &Path) -> Result<usize> {
        let root_dir = reshape_root(root);
        let entries = match std::fs::read_dir(&root_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).map_err(|source| LockError::io("reading", &root_dir, source)),
        };
        let mut removed = 0usize;
        for entry in entries {
            let entry = entry.map_err(|source| LockError::io("reading", &root_dir, source))?;
            if entry
                .file_type()
                .map_err(|source| LockError::io("reading", entry.path(), source))?
                .is_dir()
                && clean_txn_dir_if_disposable(root, &entry.path())?
            {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Explicit maintenance-only interrupted-reshape recovery: validate the
    /// chosen `choice` candidate independently, then promote it back to the
    /// live `gat.lock` path. Never guesses a choice for the caller, and
    /// refuses to overwrite an already-present live path.
    pub fn recover_prepared_reshape(
        root: &Path,
        txn_dir: &Path,
        choice: ReshapeRecoveryChoice,
    ) -> Result<()> {
        let record = read_prepared_record(root, txn_dir)?;
        let expected_txn_dir = reshape_root(root).join(&record.id);
        if txn_dir != expected_txn_dir {
            return Err(LockError::Persistence(PersistenceError::TxnIdMismatch {
                txn_dir: txn_dir.to_path_buf(),
                record_path: txn_dir.join("txn.json"),
                record_id: record.id,
            }));
        }
        let expected_backup = txn_dir.join("backup");
        let expected_staging = txn_dir.join("new");
        if record.backup_path != expected_backup || record.staging_path != expected_staging {
            return Err(LockError::Persistence(
                PersistenceError::CandidatePathsOutsideTxnDir {
                    record_path: txn_dir.join("txn.json"),
                },
            ));
        }

        let live_path = root.join("gat.lock");
        if live_path.exists()
            && let Some(shape) = on_disk_shape_at(&live_path)?
            && load_shape_at(&live_path, shape).is_ok()
        {
            return Err(LockError::Persistence(PersistenceError::LiveAlreadyValid {
                live_path: live_path.clone(),
            }));
        }

        let (candidate_path, expected_shape) = match choice {
            ReshapeRecoveryChoice::RestoreBackup => (record.backup_path, record.source_shape),
            ReshapeRecoveryChoice::PromoteStaged => (record.staging_path, record.target_shape),
        };
        let detected = on_disk_shape_at(&candidate_path)?;
        if detected != Some(expected_shape) {
            return Err(LockError::Persistence(
                PersistenceError::CandidateShapeMismatch {
                    candidate_path,
                    actual: detected.map(OnDiskShape::shard_levels),
                    expected: expected_shape.shard_levels(),
                },
            ));
        }
        load_shape_at(&candidate_path, expected_shape).map_err(|err| {
            LockError::Persistence(PersistenceError::CandidateInvalid {
                candidate_path: candidate_path.clone(),
                source: Box::new(err),
            })
        })?;
        if live_path.exists() {
            let displaced_live = txn_dir.join("replaced-live");
            if displaced_live.exists() {
                return Err(LockError::Persistence(
                    PersistenceError::DisplacedLiveAlreadyExists { displaced_live },
                ));
            }
            std::fs::rename(&live_path, &displaced_live)
                .map_err(|source| LockError::io("moving aside", &live_path, source))?;
            crate::atomic::sync_dir(root);
        }
        std::fs::rename(&candidate_path, &live_path)
            .map_err(|source| LockError::io("promoting", &candidate_path, source))?;
        crate::atomic::sync_dir(root);
        // Deliberately do *not* clean up `txn_dir` here: `repair` restores
        // correctness, `clean` discards leftovers -- keeping that split
        // observable even right after an explicit recovery means the now-
        // completed transaction scratch is left for `gat system clean
        // lock` (`clean_disposable_reshape_scratch`) to remove, which
        // already recognizes this exact post-recovery state (a `PREPARED`
        // record with the live path now present) as disposable.
        Ok(())
    }

    /// Read-only counterpart of [`cleanup_completed_reshape`], used by every
    /// ordinary live-lock read entry point ([`super::on_disk_shape`],
    /// [`super::load`], [`super::list_shard_files`]) on their
    /// missing-live-path slow path. Detects whether the live path is
    /// missing because a reshape transaction was interrupted between its
    /// two commit renames, and if so, fails closed with a diagnostic --
    /// it never renames, restores, or removes anything on disk. Recovering
    /// the interrupted transaction is deliberately left to a dedicated
    /// recovery/maintenance command instead (tracked separately, e.g.
    /// `gat system`), which inspects and classifies recovery state before
    /// acting on it explicitly.
    ///
    /// Since the caller has already confirmed the live path is missing, any
    /// valid durable `PREPARED` transaction record is itself proof of a
    /// problem: it means a live representation existed when the
    /// transaction was prepared, so a missing live path can never be
    /// "nothing tracked" while that record is still around -- this fails
    /// closed regardless of whether `backup_path` has been created yet.
    /// Only scratch left by a crash *before* a durable record was ever
    /// published (no `txn.json` at all) is harmless and ignored, since in
    /// that case the live path was never touched and being missing really
    /// does just mean nothing has ever been tracked.
    pub fn fail_if_pending_reshape(root: &Path) -> Result<()> {
        let root_dir = reshape_root(root);
        let entries = match std::fs::read_dir(&root_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).map_err(|source| LockError::io("reading", &root_dir, source)),
        };
        for entry in entries {
            let entry = entry.map_err(|source| LockError::io("reading", &root_dir, source))?;
            if entry
                .file_type()
                .map_err(|source| LockError::io("reading", entry.path(), source))?
                .is_dir()
            {
                fail_if_txn_dir_is_pending(root, &entry.path())?;
            }
        }
        Ok(())
    }

    fn fail_if_txn_dir_is_pending(root: &Path, txn_dir: &Path) -> Result<()> {
        let record_path = txn_dir.join("txn.json");
        let record_text = match std::fs::read_to_string(&record_path) {
            Ok(text) => text,
            // No durable transaction record was ever published, so the
            // live path was never touched -- this is leftover staging
            // scratch work from a crash before step 3, harmless to an
            // ordinary read.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).map_err(|source| LockError::io("reading", record_path, source));
            }
        };
        let live_path = root.join("gat.lock");
        let record: TxnRecord = serde_json::from_str(&record_text).map_err(|source| {
            LockError::Persistence(PersistenceError::TxnRecordMalformed {
                record_path: record_path.clone(),
                source,
            })
        })?;
        if record.phase != PHASE_PREPARED {
            return Err(LockError::Persistence(
                PersistenceError::TxnRecordUnrecognizedPhase {
                    record_path,
                    phase: record.phase,
                },
            ));
        }
        // The caller has already confirmed the live path is missing, and a
        // valid `PREPARED` durable record proves a live representation
        // existed when this transaction was prepared (`reshape_transactional`
        // only ever publishes one once it has snapshotted a real on-disk
        // shape to reshape). A missing live path is therefore never
        // "nothing tracked" once such a record exists, whether or not
        // `backup_path` itself is present yet -- fail closed either way
        // instead of guessing.
        Err(LockError::Persistence(
            PersistenceError::ReshapeInterruptedLiveMissing {
                record_path,
                live_path,
            },
        ))
    }

    fn read_prepared_record(_root: &Path, txn_dir: &Path) -> Result<TxnRecord> {
        let record_path = txn_dir.join("txn.json");
        let record_text = std::fs::read_to_string(&record_path)
            .map_err(|source| LockError::io("reading", &record_path, source))?;
        let record: TxnRecord = serde_json::from_str(&record_text).map_err(|source| {
            LockError::Persistence(PersistenceError::TxnRecordMalformed {
                record_path: record_path.clone(),
                source,
            })
        })?;
        if record.phase != PHASE_PREPARED {
            return Err(LockError::Persistence(
                PersistenceError::TxnRecordUnrecognizedPhase {
                    record_path,
                    phase: record.phase,
                },
            ));
        }
        Ok(record)
    }

    fn clean_txn_dir_if_disposable(root: &Path, txn_dir: &Path) -> Result<bool> {
        let record_path = txn_dir.join("txn.json");
        let record_text = match std::fs::read_to_string(&record_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cleanup_txn_dir(txn_dir);
                return Ok(true);
            }
            Err(e) => {
                return Err(e).map_err(|source| LockError::io("reading", record_path, source));
            }
        };
        let Ok(record) = serde_json::from_str::<TxnRecord>(&record_text) else {
            return Ok(false);
        };
        if record.phase != PHASE_PREPARED {
            return Ok(false);
        }

        let live_path = root.join("gat.lock");
        if !record.backup_path.exists() {
            if live_path.exists() {
                cleanup_txn_dir(txn_dir);
                return Ok(true);
            }
            return Ok(false);
        }
        if live_path.exists() {
            cleanup_txn_dir(txn_dir);
            return Ok(true);
        }
        Ok(false)
    }

    fn cleanup_completed_txn_dir(root: &Path, txn_dir: &Path) -> Result<()> {
        let record_path = txn_dir.join("txn.json");
        let record_text = match std::fs::read_to_string(&record_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No durable transaction record was ever published, so the
                // live path was never touched -- this is leftover staging
                // scratch work from a crash before step 3, safe to discard.
                cleanup_txn_dir(txn_dir);
                return Ok(());
            }
            Err(e) => {
                return Err(e).map_err(|source| LockError::io("reading", record_path, source));
            }
        };
        let record: TxnRecord = serde_json::from_str(&record_text).map_err(|source| {
            LockError::Persistence(PersistenceError::TxnRecordMalformed {
                record_path: record_path.clone(),
                source,
            })
        })?;
        if record.phase != PHASE_PREPARED {
            return Err(LockError::Persistence(
                PersistenceError::TxnRecordUnrecognizedPhase {
                    record_path,
                    phase: record.phase,
                },
            ));
        }

        let live_path = root.join("gat.lock");
        if !record.backup_path.exists() {
            // Crashed before (or exactly at) moving the old live
            // representation aside: the live path is untouched and still
            // the complete old representation. Nothing to clean up but
            // the transaction's own scratch space.
            cleanup_txn_dir(txn_dir);
            return Ok(());
        }
        if live_path.exists() {
            // The staged target was already promoted to the live path:
            // the commit already finished, so the now-redundant backup
            // and transaction scratch space are safe to discard.
            cleanup_txn_dir(txn_dir);
            return Ok(());
        }
        // The old live representation was moved aside, but the staged
        // target was never promoted: the live path is genuinely missing.
        // Restoring it is a policy decision this narrowly-scoped cleanup
        // is deliberately incapable of making -- fail closed instead of
        // guessing (see doc comment on `cleanup_completed_reshape`).
        // `super::on_disk_shape`/`fail_if_pending_reshape` already catch
        // this state before any caller reaches this function; this is
        // only a second line of defense.
        Err(LockError::Persistence(
            PersistenceError::ReshapeInterruptedAfterBackup {
                record_path,
                live_path,
                backup_path: record.backup_path,
            },
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::lock::Entry;
        use crate::lock::persistence::{load, save};

        fn sample_lock() -> Lock {
            let mut lock = Lock::default();
            lock.upsert_many([
                Entry {
                    path: gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                    oid: gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
                },
                Entry {
                    path: gat_core::lexical_path::GatPath::parse_canonical("dir/b.bin").unwrap(),
                    oid: gat_core::oid::Oid::from_hex(&"b".repeat(64)).unwrap(),
                },
                Entry {
                    path: gat_core::lexical_path::GatPath::parse_canonical("dir/c.bin").unwrap(),
                    oid: gat_core::oid::Oid::from_hex(&"c".repeat(64)).unwrap(),
                },
            ]);
            lock
        }

        fn oids(lock: &Lock) -> std::collections::BTreeMap<String, String> {
            lock.entries
                .iter()
                .map(|e: &Entry| (e.path.to_string(), e.oid.to_hex()))
                .collect()
        }

        #[test]
        fn reshape_transactional_flat_to_sharded_preserves_entries_and_cleans_up_scratch_space() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            reshape_transactional(
                tmp.path(),
                &lock,
                crate::lock::LockShardLevels::new(2).unwrap(),
            )
            .unwrap();

            assert_eq!(
                on_disk_shape_at(&tmp.path().join("gat.lock")).unwrap(),
                Some(OnDiskShape::Sharded(
                    crate::lock::LockShardLevels::new(2).unwrap()
                ))
            );
            assert_eq!(oids(&load(tmp.path()).unwrap()), oids(&lock));
            assert!(
                std::fs::read_dir(reshape_root(tmp.path()))
                    .map_or(true, |mut it| it.next().is_none()),
                "committed reshape must leave no scratch space behind"
            );
        }

        #[test]
        fn reshape_transactional_sharded_to_flat_preserves_entries() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(1).unwrap(),
            )
            .unwrap();

            reshape_transactional(
                tmp.path(),
                &lock,
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            assert_eq!(
                on_disk_shape_at(&tmp.path().join("gat.lock")).unwrap(),
                Some(OnDiskShape::Flat)
            );
            assert_eq!(oids(&load(tmp.path()).unwrap()), oids(&lock));
        }

        #[test]
        fn reshape_transactional_between_two_sharded_depths_preserves_entries() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(1).unwrap(),
            )
            .unwrap();

            reshape_transactional(
                tmp.path(),
                &lock,
                crate::lock::LockShardLevels::new(2).unwrap(),
            )
            .unwrap();

            assert_eq!(
                on_disk_shape_at(&tmp.path().join("gat.lock")).unwrap(),
                Some(OnDiskShape::Sharded(
                    crate::lock::LockShardLevels::new(2).unwrap()
                ))
            );
            assert_eq!(oids(&load(tmp.path()).unwrap()), oids(&lock));
        }

        /// Build a `<txn>/txn.json` transaction record plus whatever backup
        /// path is asked for -- but never actually call
        /// [`reshape_transactional`] end to end -- so each recovery test
        /// can hand-place disk state matching a crash at one exact point,
        /// the same way `visit_lock_rows_validated_detects_a_cross_shard_duplicate_path`
        /// hand-builds a mixed-depth tree elsewhere in this file.
        fn write_prepared_txn(
            root: &Path,
            source_shape: OnDiskShape,
            target_shape: OnDiskShape,
        ) -> (PathBuf, PathBuf, PathBuf) {
            let txn_dir = reshape_root(root).join("txn-under-test");
            std::fs::create_dir_all(&txn_dir).unwrap();
            let staging_path = txn_dir.join("new");
            let backup_path = txn_dir.join("backup");
            let record = TxnRecord {
                id: "txn-under-test".to_string(),
                source_shape,
                target_shape,
                staging_path: staging_path.clone(),
                backup_path: backup_path.clone(),
                phase: PHASE_PREPARED.to_string(),
            };
            crate::atomic::write_atomic(
                &txn_dir.join("txn.json"),
                &serde_json::to_string_pretty(&record).unwrap(),
            )
            .unwrap();
            (txn_dir, staging_path, backup_path)
        }

        #[test]
        fn cleanup_discards_staging_scratch_left_by_a_crash_before_any_record_was_written() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();
            let before = std::fs::read_to_string(tmp.path().join("gat.lock")).unwrap();

            // A crash while staging, before step 3 ever writes a
            // transaction record: just a bare txn dir with scratch
            // content and no `txn.json`.
            let txn_dir = reshape_root(tmp.path()).join("orphan");
            std::fs::create_dir_all(&txn_dir).unwrap();
            std::fs::write(txn_dir.join("new"), "partial garbage").unwrap();

            cleanup_completed_reshape(tmp.path()).unwrap();

            assert_eq!(
                std::fs::read_to_string(tmp.path().join("gat.lock")).unwrap(),
                before,
                "live gat.lock must be untouched by a crash that never got past staging"
            );
            assert!(!txn_dir.exists());
        }

        #[test]
        fn cleanup_discards_the_transaction_when_the_live_path_was_never_moved_aside() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();
            let before = std::fs::read_to_string(tmp.path().join("gat.lock")).unwrap();

            // A crash right after the transaction record was made durable
            // but before either commit rename: `backup_path` doesn't exist
            // yet, so the live path is still the complete old
            // representation.
            let (txn_dir, staging_path, _backup_path) = write_prepared_txn(
                tmp.path(),
                OnDiskShape::Flat,
                OnDiskShape::Sharded(crate::lock::LockShardLevels::new(2).unwrap()),
            );
            save_sharded(
                &lock,
                &staging_path,
                LockShardLevels::new(2).unwrap(),
                false,
            )
            .unwrap();

            cleanup_completed_reshape(tmp.path()).unwrap();

            assert_eq!(
                std::fs::read_to_string(tmp.path().join("gat.lock")).unwrap(),
                before,
                "live gat.lock must be untouched when neither commit rename ran yet"
            );
            assert!(!txn_dir.exists());
        }

        #[test]
        fn cleanup_fails_closed_when_only_the_first_commit_rename_ran() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            // A crash right after `rename(live, backup)` but before
            // `rename(staged, live)`: the live path is gone, and the
            // complete old representation sits at `backup_path` instead.
            // `cleanup_completed_reshape` is deliberately incapable of
            // restoring it -- that's a policy decision left to a
            // dedicated recovery command instead (see doc comment on
            // `cleanup_completed_reshape`).
            let (txn_dir, staging_path, backup_path) = write_prepared_txn(
                tmp.path(),
                OnDiskShape::Flat,
                OnDiskShape::Sharded(crate::lock::LockShardLevels::new(2).unwrap()),
            );
            save_sharded(
                &lock,
                &staging_path,
                LockShardLevels::new(2).unwrap(),
                false,
            )
            .unwrap();
            std::fs::rename(tmp.path().join("gat.lock"), &backup_path).unwrap();
            assert!(!tmp.path().join("gat.lock").exists());

            let err = cleanup_completed_reshape(tmp.path()).unwrap_err();

            assert!(
                matches!(
                    err,
                    LockError::Persistence(PersistenceError::ReshapeInterruptedAfterBackup { .. })
                ),
                "expected a fail-closed diagnostic, got {err:#}"
            );
            assert!(
                !tmp.path().join("gat.lock").exists(),
                "cleanup must never restore the live path itself"
            );
            assert!(backup_path.exists(), "cleanup must never touch the backup");
            assert!(
                txn_dir.exists(),
                "cleanup must never remove unresolved transaction state"
            );
        }

        #[test]
        fn cleanup_finishes_the_commit_when_the_staged_target_was_already_promoted() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            // A crash right after `rename(staged, live)` but before
            // cleanup: both the (now-redundant) backup and the promoted
            // live path exist.
            let (txn_dir, staging_path, backup_path) = write_prepared_txn(
                tmp.path(),
                OnDiskShape::Flat,
                OnDiskShape::Sharded(crate::lock::LockShardLevels::new(2).unwrap()),
            );
            save_sharded(
                &lock,
                &staging_path,
                LockShardLevels::new(2).unwrap(),
                false,
            )
            .unwrap();
            std::fs::rename(tmp.path().join("gat.lock"), &backup_path).unwrap();
            std::fs::rename(&staging_path, tmp.path().join("gat.lock")).unwrap();

            cleanup_completed_reshape(tmp.path()).unwrap();

            assert_eq!(
                on_disk_shape_at(&tmp.path().join("gat.lock")).unwrap(),
                Some(OnDiskShape::Sharded(
                    crate::lock::LockShardLevels::new(2).unwrap()
                )),
                "recovery must keep the already-promoted new representation"
            );
            assert_eq!(oids(&load(tmp.path()).unwrap()), oids(&lock));
            assert!(!txn_dir.exists());
            assert!(!backup_path.exists());
        }

        #[test]
        fn cleanup_fails_closed_on_a_malformed_transaction_record() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            let txn_dir = reshape_root(tmp.path()).join("corrupt");
            std::fs::create_dir_all(&txn_dir).unwrap();
            crate::atomic::write_atomic(&txn_dir.join("txn.json"), "not valid json").unwrap();

            let err = cleanup_completed_reshape(tmp.path()).unwrap_err();
            assert!(
                matches!(
                    err,
                    LockError::Persistence(PersistenceError::TxnRecordMalformed { .. })
                ),
                "expected a fail-closed recovery error, got {err:#}"
            );
        }

        #[test]
        fn on_disk_shape_and_load_both_fail_closed_on_a_pending_reshape_instead_of_reading_it() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            let (_txn_dir, staging_path, backup_path) = write_prepared_txn(
                tmp.path(),
                OnDiskShape::Flat,
                OnDiskShape::Sharded(crate::lock::LockShardLevels::new(2).unwrap()),
            );
            save_sharded(
                &lock,
                &staging_path,
                LockShardLevels::new(2).unwrap(),
                false,
            )
            .unwrap();
            std::fs::rename(tmp.path().join("gat.lock"), &backup_path).unwrap();

            // Neither `on_disk_shape` nor `load` may silently repair
            // the missing-live-path window an interrupted reshape can
            // leave behind -- both must detect it and fail closed instead,
            // leaving every on-disk byte untouched for an explicit
            // recovery command to inspect later.
            let shape_err = super::super::on_disk_shape(tmp.path()).unwrap_err();
            assert!(
                format!("{shape_err:#}").contains("interrupted"),
                "expected a fail-closed diagnostic, got {shape_err:#}"
            );
            let load_err = load(tmp.path()).unwrap_err();
            assert!(
                format!("{load_err:#}").contains("interrupted"),
                "expected a fail-closed diagnostic, got {load_err:#}"
            );
            assert!(
                !tmp.path().join("gat.lock").exists(),
                "a fail-closed read must never restore the live path itself"
            );
            assert!(
                backup_path.exists(),
                "a fail-closed read must never touch the backup either"
            );
        }

        /// `desired_index::refresh` (the main live desired-state read
        /// path every normal command uses, e.g. `status`/`ls-files`) reads
        /// through [`super::super::list_shard_files`], not `on_disk_shape`
        /// or `load` directly. That path must detect a pending
        /// reshape too, and fail closed exactly like the other two --
        /// otherwise the crash window after `rename(live, backup)` and
        /// before `rename(staged, live)` would have `list_shard_files` see
        /// a missing live path and report zero shards, so a refresh
        /// landing in that window would interpret desired state as
        /// completely (and silently) empty instead of raising a
        /// diagnostic.
        #[test]
        fn list_shard_files_fails_closed_on_a_pending_reshape_instead_of_reporting_empty() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            let (_txn_dir, staging_path, backup_path) = write_prepared_txn(
                tmp.path(),
                OnDiskShape::Flat,
                OnDiskShape::Sharded(crate::lock::LockShardLevels::new(2).unwrap()),
            );
            save_sharded(
                &lock,
                &staging_path,
                LockShardLevels::new(2).unwrap(),
                false,
            )
            .unwrap();
            // Simulate a crash right after the first commit rename: the
            // live path is missing, exactly the window the refresh path
            // must never mistake for "nothing tracked".
            std::fs::rename(tmp.path().join("gat.lock"), &backup_path).unwrap();
            assert!(!tmp.path().join("gat.lock").exists());

            let err = super::super::list_shard_files(tmp.path()).unwrap_err();
            assert!(
                matches!(
                    err,
                    LockError::Persistence(PersistenceError::ReshapeInterruptedLiveMissing { .. })
                ),
                "expected a fail-closed diagnostic instead of an empty shard list, got {err:#}"
            );
            assert!(
                !tmp.path().join("gat.lock").exists(),
                "a fail-closed read must never restore the live path itself"
            );
        }

        /// Ordinary scratch left behind by a crash *before* a durable
        /// transaction record was ever published (no `txn.json` at all) is
        /// harmless: the live path being missing in that case just means
        /// nothing has ever been tracked, so every read entry point must
        /// keep reporting the ordinary empty state rather than failing
        /// closed.
        #[test]
        fn read_entry_points_ignore_harmless_pre_record_scratch_and_report_the_ordinary_empty_state()
         {
            let tmp = tempfile::tempdir().unwrap();

            let orphan = reshape_root(tmp.path()).join("orphan");
            std::fs::create_dir_all(&orphan).unwrap();
            std::fs::write(orphan.join("new"), "partial garbage").unwrap();

            assert_eq!(super::super::on_disk_shape(tmp.path()).unwrap(), None);
            assert!(load(tmp.path()).unwrap().entries.is_empty());
            assert!(
                super::super::list_shard_files(tmp.path())
                    .unwrap()
                    .is_empty()
            );
        }

        /// A valid, durable `PREPARED` transaction record is itself proof
        /// that a live representation existed when the reshape started --
        /// `reshape_transactional` never publishes one before snapshotting
        /// a real on-disk shape. So once the live path is confirmed
        /// missing, every read entry point must fail closed even if the
        /// crash happened before the first commit rename ever ran (i.e.
        /// `backup_path` was never created): a missing live path alongside
        /// a durable record is never "nothing tracked".
        #[test]
        fn read_entry_points_fail_closed_on_a_prepared_record_with_no_backup_and_no_live_path() {
            let tmp = tempfile::tempdir().unwrap();

            write_prepared_txn(
                tmp.path(),
                OnDiskShape::Flat,
                OnDiskShape::Sharded(crate::lock::LockShardLevels::new(2).unwrap()),
            );

            let err = super::super::on_disk_shape(tmp.path()).unwrap_err();
            assert!(matches!(
                err,
                LockError::Persistence(PersistenceError::ReshapeInterruptedLiveMissing { .. })
            ));
            let err = load(tmp.path()).unwrap_err();
            assert!(matches!(
                err,
                LockError::Persistence(PersistenceError::ReshapeInterruptedLiveMissing { .. })
            ));
            let err = super::super::list_shard_files(tmp.path()).unwrap_err();
            assert!(matches!(
                err,
                LockError::Persistence(PersistenceError::ReshapeInterruptedLiveMissing { .. })
            ));
        }

        /// Pins the exact race `load`'s optimized missing-path branch
        /// must not lose: a reader observes the live path as genuinely
        /// missing (the crash window after the first commit rename), then
        /// a concurrent reshape runs its *entire* remaining commit --
        /// second rename plus transaction cleanup -- before the reader
        /// gets to `fail_if_pending_reshape`. Since that cleanup removes
        /// the only evidence a reshape was ever pending, the reader's
        /// `fail_if_pending_reshape` call sees nothing and would
        /// could otherwise fall straight through to `Ok(Lock::default())`
        /// without ever re-checking whether the live path had come back.
        /// Uses the test-only `set_load_missing_path_hook_for_test` seam to
        /// land the race deterministically instead of depending on thread
        /// scheduling.
        #[test]
        fn load_reobserves_the_live_path_if_a_reshape_completes_in_the_missing_path_window() {
            let tmp = tempfile::tempdir().unwrap();
            let lock = sample_lock();
            save(
                &lock,
                tmp.path(),
                crate::lock::LockShardLevels::new(0).unwrap(),
            )
            .unwrap();

            let (txn_dir, staging_path, backup_path) = write_prepared_txn(
                tmp.path(),
                OnDiskShape::Flat,
                OnDiskShape::Sharded(crate::lock::LockShardLevels::new(2).unwrap()),
            );
            save_sharded(
                &lock,
                &staging_path,
                LockShardLevels::new(2).unwrap(),
                false,
            )
            .unwrap();
            let live_path = tmp.path().join("gat.lock");
            // Reproduce the crash window: first commit rename already ran
            // (old flat file moved aside to `backup_path`), second commit
            // rename (`staging_path` -> live) hasn't happened yet -- the
            // live path is genuinely missing right now, exactly what a
            // reader's `is_file`/`is_dir` checks would observe.
            std::fs::rename(&live_path, &backup_path).unwrap();
            assert!(!live_path.exists());

            let txn_dir_for_hook = txn_dir;
            let staging_path_for_hook = staging_path;
            let live_path_for_hook = live_path.clone();
            super::super::set_load_missing_path_hook_for_test(move || {
                // The concurrent reshape finishes: promote the staged
                // target into place, then fully clean up its transaction
                // directory -- all before the paused reader resumes and
                // calls `fail_if_pending_reshape`, which will therefore
                // find nothing pending at all.
                std::fs::rename(&staging_path_for_hook, &live_path_for_hook).unwrap();
                std::fs::remove_dir_all(&txn_dir_for_hook).unwrap();
            });

            let reloaded = load(tmp.path()).unwrap();
            assert_eq!(
                oids(&reloaded),
                oids(&lock),
                "a reshape that completes inside the reader's missing-path window must be \
                 observed, not silently reported as an empty lock"
            );
            assert!(
                live_path.is_dir(),
                "the reshape's completed sharded representation must be left in place"
            );
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use reshape::simulate_crash_after_first_rename;
pub use reshape::{
    clean_disposable_reshape_scratch, recover_prepared_reshape, reshape_transactional,
};

/// Test-only deterministic race injection for [`publish_if_changed`]'s
/// tier-2 coherent read. Tests can prove a shard mutation landing between
/// the tier-2 comparison read's pre/post stats fails that publish
/// closed, and so tests can count exactly how many times this read runs
/// per publish call without depending on a real, inherently flaky
/// thread-timing race.
#[cfg(test)]
pub mod race_test_hooks {
    use std::path::Path;
    use std::sync::Mutex;

    type Hook = Box<dyn FnMut(&Path) + Send>;

    // Process-wide (not thread-local): `publish_if_changed` may run on
    // any Rayon worker thread during a sharded save, not necessarily the
    // test's own thread.
    static BEFORE_READ: Mutex<Option<Hook>> = Mutex::new(None);

    /// Serializes every test that installs a hook here against every
    /// other one, since [`BEFORE_READ`] is a single process-wide slot:
    /// without this, two tests running concurrently (cargo test's
    /// default multi-threaded runner) could overwrite each other's hook
    /// mid-measurement. Acquire this (a guard is recommended, e.g.
    /// `RaceHookGuard`) for the entire install-measure-clear window.
    pub static GATE: Mutex<()> = Mutex::new(());

    /// Install a hook that runs immediately before the tier-2 comparison
    /// read. Tests must pair this with [`clear`] (a guard is recommended)
    /// so the hook never leaks into an unrelated test running later in
    /// the same process. Because this hook is process-wide, callers must
    /// filter on the exact path they expect inside their closure.
    pub fn set(hook: impl FnMut(&Path) + Send + 'static) {
        *BEFORE_READ.lock().unwrap() = Some(Box::new(hook));
    }

    /// Remove any installed hook.
    pub fn clear() {
        *BEFORE_READ.lock().unwrap() = None;
    }

    pub fn fire_before_read(path: &Path) {
        if let Some(hook) = BEFORE_READ.lock().unwrap().as_mut() {
            hook(path);
        }
    }
}

/// Test-only structural instrumentation for the destination-side flat
/// `gat.lock` publication path -- distinct from
/// [`super::test_support::max_retained_ordered_rows`], which measures the
/// *source*-side ordered lock reader's retained-row high-water mark, not
/// how much work the flat publish target repeats.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static FLAT_SHARD_PUBLISH_CALLS: Cell<usize> = const { Cell::new(0) };
        static FLAT_SHARD_PUBLISH_ROW_TOTAL: Cell<usize> = const { Cell::new(0) };
        static MAX_RETAINED_FLAT_PUBLISH_ROWS: Cell<usize> = const { Cell::new(0) };
    }

    /// Reset both counters below, so a test can measure just the call(s)
    /// under test.
    pub fn reset_flat_shard_publish_counters() {
        FLAT_SHARD_PUBLISH_CALLS.with(|c| c.set(0));
        FLAT_SHARD_PUBLISH_ROW_TOTAL.with(|c| c.set(0));
        MAX_RETAINED_FLAT_PUBLISH_ROWS.with(|c| c.set(0));
    }

    /// Record that `super::publish_flat_shard_streaming` is currently
    /// retaining `count` rows at once, updating the running high-water
    /// mark if `count` exceeds it. The streaming publish path never holds
    /// more than one row in memory (it writes each straight through a
    /// bounded `BufWriter`), so a test can assert this stays at `1`
    /// regardless of the total row count replayed.
    pub fn observe_max_retained_flat_publish_rows(count: usize) {
        MAX_RETAINED_FLAT_PUBLISH_ROWS.with(|c| {
            if count > c.get() {
                c.set(count);
            }
        });
    }

    /// The peak `count` seen by [`observe_max_retained_flat_publish_rows`]
    /// since the last [`reset_flat_shard_publish_counters`].
    pub fn max_retained_flat_publish_rows() -> usize {
        MAX_RETAINED_FLAT_PUBLISH_ROWS.with(Cell::get)
    }

    pub fn record_flat_shard_publish(row_count: usize) {
        FLAT_SHARD_PUBLISH_CALLS.with(|c| c.set(c.get() + 1));
        FLAT_SHARD_PUBLISH_ROW_TOTAL.with(|c| c.set(c.get() + row_count));
    }

    /// How many times `super::publish_flat_shard` actually rendered and
    /// (potentially) rewrote `<root>/gat.lock` since the last reset. A
    /// bounded first/recovery mount replay must publish the flat file at
    /// most a small constant number of times regardless of how many
    /// staged row windows were replayed -- not once per window.
    pub fn flat_shard_publish_calls() -> usize {
        FLAT_SHARD_PUBLISH_CALLS.with(Cell::get)
    }

    /// The sum of `entries.len()` across every `super::publish_flat_shard`
    /// call since the last reset -- the cumulative row volume actually
    /// serialized into the flat file. A single full publish of `N` rows
    /// contributes `N`; republishing the same growing file once per
    /// window (the O(N²/window) regression this instruments against)
    /// contributes roughly `N + 2N + 3N + ...`, so a test can assert this
    /// stays close to `N` (one full publish) rather than growing
    /// quadratically with the number of windows.
    pub fn flat_shard_publish_row_total() -> usize {
        FLAT_SHARD_PUBLISH_ROW_TOTAL.with(Cell::get)
    }

    /// A synthetic [`super::ShardEvidence`] for a test that needs to
    /// exercise a receipt-consuming API (e.g.
    /// `crate::state::DesiredStateWrite::record_published_shards`
    /// in the root `gat` crate) without going through an actual publish.
    /// The type has no public constructor outside `gat-io` itself: its
    /// fields are private and read only through
    /// `shard_id`/`identity`/`proof`), so a caller that needs one just to
    /// assert against a receipt-shaped input builds it through this
    /// instead.
    #[cfg(test)]
    pub(crate) const fn shard_evidence(
        shard_id: super::LockShardId,
        identity: super::super::ShardContentIdentity,
        proof: crate::file_state::StatProof,
    ) -> super::ShardEvidence {
        super::ShardEvidence {
            shard_id,
            identity,
            proof,
        }
    }
}

#[cfg(test)]
mod tests {
    fn layout(root: &std::path::Path) -> crate::RepositoryLayout {
        crate::RepositoryLayout::at(root.to_path_buf())
    }
    use super::*;

    fn gp(s: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(s).unwrap()
    }

    fn entry(path: &str, oid_hex: &str) -> Entry {
        Entry {
            path: gp(path),
            oid: gat_core::oid::Oid::from_hex(oid_hex).unwrap(),
        }
    }

    fn insert(lock: &mut Lock, path: &str, oid_hex: impl AsRef<str>) {
        let entry = entry(path, oid_hex.as_ref());
        lock.upsert(entry.path, entry.oid);
    }

    /// RAII guard clearing [`race_test_hooks`] on drop (including on
    /// panic/early return), so a test that injects/counts a
    /// [`publish_if_changed`] read can never leak its hook into a later
    /// test sharing the same OS thread.
    /// RAII guard clearing [`race_test_hooks`] on drop (including on
    /// panic/early return), so a test that injects/counts a
    /// [`publish_if_changed`] read can never leak its hook into a later
    /// test sharing the same OS thread. Also holds
    /// [`race_test_hooks::GATE`] for its whole lifetime, serializing every
    /// test that uses this module's single process-wide hook slot
    /// against every other one.
    struct RaceHookGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl RaceHookGuard {
        fn acquire() -> Self {
            Self(race_test_hooks::GATE.lock().unwrap())
        }
    }

    impl Drop for RaceHookGuard {
        fn drop(&mut self) {
            race_test_hooks::clear();
        }
    }

    fn save_sparse_shards_with_threads(
        root: &Path,
        touched_shard_ids: &BTreeSet<LockShardId>,
        rows_by_shard: &BTreeMap<LockShardId, Vec<Entry>>,
        threads: usize,
    ) -> Result<SparseShardPublish> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| {
                save_sparse_shards(
                    &layout(root),
                    touched_shard_ids,
                    rows_by_shard,
                    &BTreeMap::new(),
                )
            })
    }

    /// The identity
    /// [`publish_flat_shard_streaming`] returns must equal
    /// `hash_shard_bytes` of the exact bytes it published, even though it
    /// never rereads the completed temp file to compute that identity --
    /// proving the single-pass incremental hasher agrees with the
    /// in-memory canonical primitive over the same final bytes.
    #[test]
    fn publish_flat_shard_streaming_identity_matches_hash_shard_bytes_of_the_published_file() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![
            entry(
                "a.bin",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            entry(
                "b.bin",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
            entry(
                "c.bin",
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            ),
        ];
        let mut rows = entries.into_iter();
        let published = publish_flat_shard_streaming(&layout(tmp.path()), || Ok(rows.next()))
            .unwrap()
            .unwrap();

        let on_disk = std::fs::read(tmp.path().join("gat.lock")).unwrap();
        assert_eq!(published.identity, crate::lock::hash_shard_bytes(&on_disk));
    }

    #[test]
    fn load_returns_empty_lock_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(tmp.path()).unwrap(), Lock::default());
    }

    #[test]
    fn flat_visit_certifies_the_file_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        insert(&mut lock, "data/b.bin", "b".repeat(64));
        insert(&mut lock, "data/c.bin", "c".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();

        let before = crate::lock::test_support::file_validation_parses();
        let mut kept = Vec::new();
        visit_lock_rows(
            tmp.path(),
            |path| path.starts_with("data/"),
            |entry| {
                kept.push(entry.path.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            crate::lock::test_support::file_validation_parses() - before,
            1,
            "a flat lock must be certified exactly once"
        );
        assert_eq!(kept, vec!["data/b.bin", "data/c.bin"]);
    }

    #[test]
    fn multi_shard_validation_retains_the_bounded_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..40 {
            insert(&mut lock, &format!("data/f{i:03}.bin"), format!("{i:064x}"));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let before = crate::lock::test_support::file_validation_parses();
        let mut kept = Vec::new();
        visit_lock_rows_validated(
            tmp.path(),
            None,
            |path| path == "data/f007.bin",
            |entry| {
                kept.push(entry.path.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            crate::lock::test_support::file_validation_parses() - before,
            0,
            "multi-shard validation retains the existing streaming merge"
        );
        assert_eq!(kept, vec!["data/f007.bin".to_string()]);
    }

    #[test]
    fn visit_lock_rows_validated_rejects_a_mixed_depth_completed_tree() {
        // A completed live `gat.lock/` tree is always uniformly sharded
        // A mixed-depth tree (e.g. an interrupted
        // reshape that wrote the new-depth shard before removing the
        // old-depth one) must be rejected as a topology error, not
        // silently processed (or misreported as a cross-shard
        // duplicate) just because each row still passes its own shard's
        // placement check.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gat.lock");
        let oid_a = "a".repeat(64);
        let oid_b = "b".repeat(64);
        let shallow = shard_id_for_path(&gp("shared.bin"), LockShardLevels::new(1).unwrap());
        let deep = shard_id_for_path(&gp("shared.bin"), LockShardLevels::new(2).unwrap());
        assert_ne!(shallow, deep);
        for (shard_id, oid) in [(&shallow, &oid_a), (&deep, &oid_b)] {
            let canonical = shard_id.to_canonical_string();
            let rel = canonical.strip_prefix("gat.lock/").unwrap();
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                format!("{0}\n{oid}\tshared.bin\n", super::super::VERSION),
            )
            .unwrap();
        }

        let err = visit_lock_rows_validated(tmp.path(), None, |_| true, |_| Ok(())).unwrap_err();
        assert!(
            format!("{err:#}").contains("more than one shard depth"),
            "expected a mixed-shard-topology error, got {err:#}"
        );
    }

    #[test]
    fn visit_lock_rows_validated_detects_a_same_shard_duplicate_path() {
        // A duplicate within one shard's own ordered rows must be reported
        // the same way `ValidatedLockFile`/`Lock::parse` always have --
        // "appears more than once" -- not misreported as a cross-shard
        // duplicate just because the merge walk's single global `open`
        // stack also happens to be where cross-shard duplicates are
        // caught; validation and failure behavior must remain stable.
        let tmp = tempfile::tempdir().unwrap();
        let oid = "a".repeat(64);
        std::fs::write(
            tmp.path().join("gat.lock"),
            format!(
                "{0}\n{oid}\tshared.bin\n{oid}\tshared.bin\n",
                super::super::VERSION
            ),
        )
        .unwrap();

        let err = visit_lock_rows_validated(tmp.path(), None, |_| true, |_| Ok(())).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("tracked more than once"),
            "expected a same-shard duplicate-path error, got {message}"
        );
        assert!(
            !message.contains("tracked by more than one shard"),
            "a same-shard duplicate must not be reported as a cross-shard one, got {message}"
        );
    }

    #[test]
    fn visit_lock_rows_validated_accepts_a_crlf_flat_lock() {
        // A hand-edited (or Windows-checked-out) flat `gat.lock` using
        // `CRLF` line endings must parse identically through the shard's
        // streaming `ShardLineCursor` path as its `LF` counterpart.
        let tmp = tempfile::tempdir().unwrap();
        let oid_a = "a".repeat(64);
        let oid_b = "b".repeat(64);
        std::fs::write(
            tmp.path().join("gat.lock"),
            format!(
                "{}\r\n{oid_a}\ta.bin\r\n{oid_b}\tdata/b.bin\r\n",
                super::super::VERSION
            ),
        )
        .unwrap();

        let mut kept = Vec::new();
        visit_lock_rows_validated(
            tmp.path(),
            None,
            |_| true,
            |entry| {
                kept.push(entry.path.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(kept, vec!["a.bin".to_string(), "data/b.bin".to_string()]);
    }

    #[test]
    fn visit_lock_rows_validated_accepts_crlf_across_multiple_shards() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..40 {
            insert(&mut lock, &format!("data/f{i:03}.bin"), format!("{i:064x}"));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        // Rewrite every shard file on disk from LF to CRLF in place,
        // simulating content that was checked out (or hand-edited) with
        // CRLF line endings while keeping the same rows/order.
        for shard in list_shard_files(tmp.path()).unwrap() {
            let text = std::fs::read_to_string(&shard.full_path).unwrap();
            let crlf = text.replace('\n', "\r\n");
            std::fs::write(&shard.full_path, crlf).unwrap();
        }

        let mut kept = Vec::new();
        visit_lock_rows_validated(
            tmp.path(),
            None,
            |_| true,
            |entry| {
                kept.push(entry.path.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(kept.len(), 40);
        assert_eq!(kept, {
            let mut sorted = kept.clone();
            sorted.sort();
            sorted
        });
    }

    #[test]
    fn visit_lock_rows_validated_rejects_unordered_shards_before_callbacks() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..20 {
            insert(&mut lock, &format!("data/f{i:03}.bin"), format!("{i:064x}"));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(1).unwrap(),
        )
        .unwrap();

        // Keep placement valid but violate mandatory within-file ordering.
        let shard = list_shard_files(tmp.path())
            .unwrap()
            .into_iter()
            .find(|shard| {
                std::fs::read_to_string(&shard.full_path)
                    .unwrap()
                    .lines()
                    .filter(|line| !line.is_empty() && *line != super::super::VERSION)
                    .count()
                    >= 2
            })
            .expect("fixture must produce a shard with at least two rows");
        let text = std::fs::read_to_string(&shard.full_path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        let (header, rows) = lines.split_at_mut(1);
        assert_eq!(header[0], super::super::VERSION);
        rows.reverse();
        let reordered = format!("{}\n{}\n", header[0], rows.join("\n"));
        assert_ne!(reordered, text, "reversing rows must actually change order");
        std::fs::write(&shard.full_path, reordered).unwrap();

        let error = visit_lock_rows_validated(
            tmp.path(),
            None,
            |_| panic!("selection before certification"),
            |_| panic!("emission before certification"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("strictly increasing order"));
    }

    #[test]
    fn visit_lock_rows_validation_errors_are_unchanged() {
        let cases = [
            (
                format!(
                    "{0}\n{1}\tshared.bin\n{2}\tshared.bin\n",
                    super::super::VERSION,
                    "a".repeat(64),
                    "b".repeat(64)
                ),
                "tracked more than once",
            ),
            (
                format!(
                    "{0}\n{1}\tfoo\n{2}\tfoo/bar\n",
                    super::super::VERSION,
                    "a".repeat(64),
                    "b".repeat(64)
                ),
                "directory prefix",
            ),
            (
                format!(
                    "{}\n\"bad.bin\"\tsha256:{}\n",
                    super::super::VERSION,
                    "a".repeat(64)
                ),
                "expected a TAB",
            ),
        ];

        for (i, (text, needle)) in cases.into_iter().enumerate() {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("gat.lock"), text).unwrap();
            let err = visit_lock_rows(tmp.path(), |_| true, |_| Ok(())).unwrap_err();
            assert!(
                format!("{err:#}").contains(needle),
                "case {i}: expected {needle:?} in {err:#}"
            );
        }
    }

    #[test]
    fn visit_lock_rows_validated_never_reads_a_whole_shard_text_on_the_ordered_fast_path() {
        // The ordered k-way merge must stream each shard one row at a
        // time, with bounded live buffering per shard
        // path, which is the only one that reads a shard's complete text
        // into memory via `std::fs::read_to_string`.
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..500 {
            insert(&mut lock, &format!("data/f{i:04}.bin"), format!("{i:064x}"));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let before = crate::lock::test_support::full_shard_text_reads();
        let mut kept = 0;
        visit_lock_rows_validated(
            tmp.path(),
            None,
            |_| true,
            |_| {
                kept += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            crate::lock::test_support::full_shard_text_reads() - before,
            0,
            "an ordered multi-shard lock must never read a whole shard's text into memory"
        );
        assert_eq!(kept, 500);
    }

    #[test]
    fn visit_lock_rows_validated_selects_the_next_merge_head_in_roughly_n_log_q_comparisons() {
        // The k-way merge must pick the next row across shards with a
        // heap (`O(log shards)` per row), not a linear scan over every
        // shard's pending head for every emitted row (`O(shards)` per
        // row) -- the difference is only visible with enough shards that
        // `shards` and `log2(shards)` are clearly distinguishable.
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        // Hundreds of shards already separate heap and linear-scan costs.
        let rows = 256;
        for i in 0..rows {
            insert(&mut lock, &format!("data/f{i:05}.bin"), format!("{i:064x}"));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(1).unwrap(),
        )
        .unwrap();
        let shard_count = list_shard_files(tmp.path()).unwrap().len();
        assert!(
            shard_count >= 128,
            "fixture must distinguish heap and scan costs"
        );

        let before = crate::lock::test_support::merge_head_comparisons();
        let mut kept = 0;
        visit_lock_rows_validated(
            tmp.path(),
            None,
            |_| true,
            |_| {
                kept += 1;
                Ok(())
            },
        )
        .unwrap();
        let comparisons = crate::lock::test_support::merge_head_comparisons() - before;
        assert_eq!(kept, rows);

        let linear_scan_cost = rows * shard_count;
        assert!(
            comparisons < linear_scan_cost / 4,
            "expected roughly n*log(shards) ({}) merge-head comparisons for {rows} rows \
             across {shard_count} shards, got {comparisons}, which is too close to the \
             n*shards linear-scan cost of {linear_scan_cost}",
            rows * usize::try_from(shard_count.next_power_of_two().ilog2()).unwrap()
        );
    }

    #[test]
    fn save_leaves_no_leftover_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name() != "gat.lock")
            .collect();
        assert!(leftovers.is_empty(), "leftover entries: {leftovers:?}");
    }

    #[test]
    fn save_from_concurrent_threads_never_leaves_a_corrupted_or_partial_lock_file() {
        // Many threads race to save distinct lock contents to the same
        // `gat.lock` path concurrently. Whichever writer wins, the file on
        // disk must always parse as one writer's complete, valid lock --
        // never a torn mix of two, and never left unparseable.
        let tmp = tempfile::tempdir().unwrap();
        let candidates: Vec<Lock> = (0..12)
            .map(|i| {
                let mut lock = Lock::default();
                insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
                lock
            })
            .collect();
        std::thread::scope(|s| {
            for lock in &candidates {
                let root = tmp.path();
                s.spawn(move || {
                    save(lock, root, crate::lock::LockShardLevels::new(0).unwrap()).unwrap();
                });
            }
        });
        let loaded = load(tmp.path()).unwrap();
        assert!(
            candidates.contains(&loaded),
            "final gat.lock wasn't any single writer's complete content"
        );
    }

    #[test]
    fn save_then_load_roundtrips_sorted_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "z.bin", "a".repeat(64));
        insert(&mut lock, "a.bin", "b".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let loaded = load(tmp.path()).unwrap();
        assert_eq!(
            loaded
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a.bin", "z.bin"]
        );
    }

    #[test]
    fn shard_levels_zero_writes_the_same_flat_file_as_before() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "d".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        assert!(tmp.path().join("gat.lock").is_file());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("gat.lock")).unwrap(),
            format!(
                // hygiene-ok: fixed, human-readable spec-URL header compared byte-for-byte; never dereferenced as a network address.
                "version https://getgat.dev/spec/lock-v1\n{0}\ta.bin\n",
                "d".repeat(64)
            )
        );
    }

    #[test]
    fn sharded_save_then_load_roundtrips_regardless_of_depth() {
        for levels in (1..=4u8).map(|n| LockShardLevels::new(n).unwrap()) {
            let tmp = tempfile::tempdir().unwrap();
            let mut lock = Lock::default();
            for i in 0..40 {
                insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
            }
            save(&lock, tmp.path(), levels).unwrap();
            assert!(
                tmp.path().join("gat.lock").is_dir(),
                "levels={levels:?} should produce a directory"
            );
            let mut loaded = load(tmp.path()).unwrap();
            loaded.entries.sort_by(|a, b| a.path.cmp(&b.path));
            let mut expected = lock.entries.clone();
            expected.sort_by(|a, b| a.path.cmp(&b.path));
            assert_eq!(loaded.entries, expected, "levels={levels:?}");
        }
    }

    #[test]
    fn sharded_save_spreads_entries_across_more_than_one_shard_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..64 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(1).unwrap(),
        )
        .unwrap();
        let mut shard_files = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files).unwrap();
        assert!(
            shard_files.len() > 1,
            "expected entries to land in more than one shard file, got {shard_files:?}"
        );
    }

    #[test]
    fn reshaping_from_flat_to_sharded_removes_the_old_flat_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        insert(&mut lock, "data/b.bin", "b".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        assert!(tmp.path().join("gat.lock").is_file());

        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        assert!(tmp.path().join("gat.lock").is_dir());
        let loaded = load(tmp.path()).unwrap();
        assert_eq!(loaded.entries.len(), 2);
    }

    #[test]
    fn reshaping_from_sharded_to_flat_removes_the_old_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        insert(&mut lock, "data/b.bin", "b".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        assert!(tmp.path().join("gat.lock").is_dir());

        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        assert!(tmp.path().join("gat.lock").is_file());
        let loaded = load(tmp.path()).unwrap();
        assert_eq!(loaded.entries.len(), 2);
    }

    #[test]
    fn changing_shard_levels_removes_stale_shard_files_at_the_old_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..20 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(1).unwrap(),
        )
        .unwrap();

        let mut shard_files = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files).unwrap();
        assert!(
            shard_files
                .iter()
                .all(|f| f.parent().unwrap().ends_with("gat.lock")),
            "no shard file should remain nested under a stale level-2 subdirectory: {shard_files:?}"
        );
        let loaded = load(tmp.path()).unwrap();
        assert_eq!(loaded.entries.len(), 20);
    }

    #[test]
    fn removing_an_entry_prunes_its_now_empty_shard_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "only-entry-in-its-shard.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(4).unwrap(),
        )
        .unwrap();
        let mut before = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut before).unwrap();
        assert_eq!(before.len(), 1);

        lock.entries.clear();
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(4).unwrap(),
        )
        .unwrap();
        let mut after = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut after).unwrap();
        assert!(after.is_empty(), "stale shard file left behind: {after:?}");
        assert_eq!(load(tmp.path()).unwrap(), Lock::default());
    }

    #[test]
    fn on_disk_shape_is_none_when_nothing_written_yet() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(on_disk_shape(tmp.path()).unwrap(), None);
    }

    #[test]
    fn on_disk_shape_detects_flat_and_sharded_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));

        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        assert_eq!(on_disk_shape(tmp.path()).unwrap(), Some(OnDiskShape::Flat));

        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            on_disk_shape(tmp.path()).unwrap(),
            Some(OnDiskShape::Sharded(
                crate::lock::LockShardLevels::new(2).unwrap()
            ))
        );
    }

    /// A symlink masquerading as `gat.lock` (whether pointing
    /// at a regular file or a directory) must fail desired-state
    /// observation outright, both through the shape detector and through
    /// [`list_shard_files`] itself -- never be silently followed and read
    /// as though it were the real managed object.
    #[cfg(unix)]
    #[test]
    fn on_disk_shape_and_list_shard_files_fail_on_a_symlinked_gat_lock() {
        let real_dir = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            real_dir.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            real_dir.path().join("gat.lock"),
            tmp.path().join("gat.lock"),
        )
        .unwrap();

        assert!(on_disk_shape(tmp.path()).is_err());
        assert!(list_shard_files(tmp.path()).is_err());
    }

    /// A symlink leaf inside a sharded `gat.lock/` tree
    /// masquerading as a shard file must likewise fail rather than be
    /// silently followed.
    #[cfg(unix)]
    #[test]
    fn list_shard_files_fails_on_a_symlinked_shard_leaf() {
        let real_dir = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            real_dir.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let gat_lock_dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&gat_lock_dir).unwrap();
        let mut shard_files = Vec::new();
        shard::collect_files(&real_dir.path().join("gat.lock"), &mut shard_files).unwrap();
        let real_shard = shard_files
            .first()
            .expect("sharded save produced at least one shard file");
        let rel = real_shard
            .strip_prefix(real_dir.path().join("gat.lock"))
            .unwrap();
        if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(gat_lock_dir.join(parent)).unwrap();
        }
        std::os::unix::fs::symlink(real_shard, gat_lock_dir.join(rel)).unwrap();

        assert!(list_shard_files(tmp.path()).is_err());
    }

    /// A FIFO or socket masquerading as the top-level `gat.lock`
    /// file must fail desired-state observation via a plain no-follow
    /// stat (`file_type()`), never via an attempted content read that
    /// could block indefinitely waiting for a FIFO peer. If either
    /// detector below tried to open/read a FIFO with no writer on the
    /// other end, this test would hang rather than return an error.
    #[cfg(unix)]
    #[test]
    fn on_disk_shape_and_list_shard_files_fail_on_a_special_file_gat_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gat.lock");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo failed for {}", path.display());

        assert!(on_disk_shape(tmp.path()).is_err());
        assert!(list_shard_files(tmp.path()).is_err());
    }

    /// A FIFO or socket masquerading as a shard leaf inside a
    /// sharded `gat.lock/` tree must fail the same way -- rejected by a
    /// no-follow stat, not by a blocking read attempt.
    #[cfg(unix)]
    #[test]
    fn list_shard_files_fails_on_a_special_file_shard_leaf() {
        let real_dir = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            real_dir.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let mut shard_files = Vec::new();
        shard::collect_files(&real_dir.path().join("gat.lock"), &mut shard_files).unwrap();
        let real_shard = shard_files
            .first()
            .expect("sharded save produced at least one shard file");
        let rel = real_shard
            .strip_prefix(real_dir.path().join("gat.lock"))
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let gat_lock_dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&gat_lock_dir).unwrap();
        if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(gat_lock_dir.join(parent)).unwrap();
        }
        let leaf = gat_lock_dir.join(rel);
        let status = std::process::Command::new("mkfifo")
            .arg(&leaf)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo failed for {}", leaf.display());

        assert!(list_shard_files(tmp.path()).is_err());
    }

    /// A FIFO or socket masquerading as an intermediate shard
    /// directory component (a level that must be a directory to descend
    /// further) must likewise fail immediately via stat, without ever
    /// attempting to `read_dir` or read through it.
    #[cfg(unix)]
    #[test]
    fn list_shard_files_fails_on_a_special_file_intermediate_shard_directory() {
        let real_dir = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            real_dir.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let mut shard_files = Vec::new();
        shard::collect_files(&real_dir.path().join("gat.lock"), &mut shard_files).unwrap();
        let real_shard = shard_files
            .first()
            .expect("sharded save produced at least one shard file");
        let rel = real_shard
            .strip_prefix(real_dir.path().join("gat.lock"))
            .unwrap();
        let first_component = rel
            .components()
            .next()
            .expect("sharded shard path has at least one directory component");

        let tmp = tempfile::tempdir().unwrap();
        let gat_lock_dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&gat_lock_dir).unwrap();
        let intermediate = gat_lock_dir.join(first_component);
        let status = std::process::Command::new("mkfifo")
            .arg(&intermediate)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "mkfifo failed for {}",
            intermediate.display()
        );

        assert!(list_shard_files(tmp.path()).is_err());
    }

    #[test]
    fn on_disk_shape_detects_depth_via_early_exit_in_a_multi_shard_tree() {
        // Enough entries at shard_levels=3 that they spread across several
        // shard files (not just the one `on_disk_shape` inspects), so this
        // exercises the early-exit path (`shard::first_file`) against a
        // genuinely multi-file, multi-directory-level tree rather than a
        // single leaf.
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..40 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(3).unwrap(),
        )
        .unwrap();

        let mut shard_files = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files).unwrap();
        assert!(
            shard_files.len() > 1,
            "expected several shard files across the tree, got {shard_files:?}"
        );

        assert_eq!(
            on_disk_shape(tmp.path()).unwrap(),
            Some(OnDiskShape::Sharded(
                crate::lock::LockShardLevels::new(3).unwrap()
            ))
        );
    }

    #[test]
    fn shard_first_file_finds_a_leaf_without_collecting_the_whole_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..40 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(3).unwrap(),
        )
        .unwrap();

        let dir = tmp.path().join("gat.lock");
        let found = shard::first_file(&dir)
            .unwrap()
            .expect("a shard file exists");
        assert_eq!(
            found.strip_prefix(&dir).unwrap().components().count(),
            3,
            "found file should be nested 3 levels deep: {found:?}"
        );

        let mut all = Vec::new();
        shard::collect_files(&dir, &mut all).unwrap();
        assert!(all.contains(&found));
    }

    #[test]
    fn shard_first_file_is_none_for_an_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(shard::first_file(&dir).unwrap(), None);
    }

    #[test]
    fn save_matching_disk_shape_uses_shard_levels_only_for_the_first_write() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        // Nothing on disk yet: the passed shard_levels is honored.
        save_matching_disk_shape(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        assert!(tmp.path().join("gat.lock").is_dir());

        // Something's already sharded on disk: a *different* shard_levels
        // argument is ignored, and the existing shape is preserved.
        insert(&mut lock, "b.bin", "b".repeat(64));
        save_matching_disk_shape(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        assert!(
            tmp.path().join("gat.lock").is_dir(),
            "save_matching_disk_shape must never reshape"
        );
        assert_eq!(load(tmp.path()).unwrap().entries.len(), 2);
    }

    #[test]
    #[cfg(unix)]
    fn save_matching_disk_shape_only_rewrites_the_shard_file_whose_entries_changed() {
        use std::collections::HashSet;
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..30 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let mut shard_files = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files).unwrap();
        assert!(
            shard_files.len() > 1,
            "expected more than one shard file, got {shard_files:?}"
        );
        let inodes_before: std::collections::HashMap<_, _> = shard_files
            .iter()
            .map(|f| (f.clone(), std::fs::metadata(f).unwrap().ino()))
            .collect();

        // Update just one entry and save through the incremental path.
        insert(&mut lock, "file-0.bin", "b".repeat(64));
        save_matching_disk_shape(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let mut shard_files_after = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files_after).unwrap();
        assert_eq!(
            shard_files.iter().collect::<HashSet<_>>(),
            shard_files_after.iter().collect::<HashSet<_>>(),
            "shard file set shouldn't change for a same-shape, in-place edit"
        );

        let changed_shard = shard::rel_path(
            &gp("file-0.bin"),
            LockShardLevels::new(2).expect("valid shard depth"),
        );
        let mut unchanged_count = 0;
        let mut changed_count = 0;
        for file in &shard_files_after {
            let rel = file.strip_prefix(tmp.path().join("gat.lock")).unwrap();
            let ino_after = std::fs::metadata(file).unwrap().ino();
            if rel == changed_shard {
                changed_count += 1;
                assert_ne!(
                    inodes_before[file], ino_after,
                    "the shard file containing the changed entry should have been rewritten"
                );
            } else {
                unchanged_count += 1;
                assert_eq!(
                    inodes_before[file],
                    ino_after,
                    "shard file {} should not have been rewritten",
                    file.display()
                );
            }
        }
        assert_eq!(changed_count, 1);
        assert!(unchanged_count > 0);
        assert_eq!(load(tmp.path()).unwrap().entries.len(), 30);
    }

    #[test]
    #[cfg(unix)]
    fn save_sparse_shards_only_rewrites_the_removed_shard_during_an_rm_like_update() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..40 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let removed_path = "file-0.bin";
        let removed_shard = shard_id_for_path(&gp(removed_path), LockShardLevels::new(2).unwrap());
        let before_contents = load(tmp.path()).unwrap();
        let mut touched_rows: BTreeMap<LockShardId, Vec<Entry>> = BTreeMap::new();
        touched_rows.insert(
            removed_shard,
            before_contents
                .entries
                .into_iter()
                .filter(|e| e.path != removed_path)
                .filter(|e| {
                    shard_id_for_path(&e.path, LockShardLevels::new(2).unwrap()) == removed_shard
                })
                .collect(),
        );
        let touched = BTreeSet::from([removed_shard]);

        let mut shard_files = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files).unwrap();
        let inodes_before: std::collections::HashMap<_, _> = shard_files
            .iter()
            .map(|f| {
                (
                    format!(
                        "gat.lock/{}",
                        f.strip_prefix(tmp.path().join("gat.lock"))
                            .unwrap()
                            .to_string_lossy()
                            .replace(std::path::MAIN_SEPARATOR, "/")
                    ),
                    std::fs::metadata(f).unwrap().ino(),
                )
            })
            .collect();

        let (_published, removed) = save_sparse_shards(
            &layout(tmp.path()),
            &touched,
            &touched_rows,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(removed, vec![removed_shard]);

        let loaded = load(tmp.path()).unwrap();
        assert!(loaded.entries.iter().all(|e| e.path != removed_path));
        let mut shard_files_after = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files_after).unwrap();
        for file in shard_files_after {
            let rel = format!(
                "gat.lock/{}",
                file.strip_prefix(tmp.path().join("gat.lock"))
                    .unwrap()
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            );
            let ino_after = std::fs::metadata(&file).unwrap().ino();
            if rel == removed_shard.to_canonical_string() {
                assert_ne!(inodes_before[&rel], ino_after);
            } else {
                assert_eq!(inodes_before[&rel], ino_after);
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn save_sparse_shards_only_rewrites_the_old_and_new_shards_during_an_mv_like_update() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..40 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let src = "file-0.bin";
        let dst = "renamed/file-0.bin";
        let src_shard = shard_id_for_path(&gp(src), LockShardLevels::new(2).unwrap());
        let dst_shard = shard_id_for_path(&gp(dst), LockShardLevels::new(2).unwrap());
        assert_ne!(src_shard, dst_shard, "test needs a cross-shard move");

        let before = load(tmp.path()).unwrap();
        let mut rows_by_shard: BTreeMap<LockShardId, Vec<Entry>> = BTreeMap::new();
        for shard_id in [src_shard, dst_shard] {
            rows_by_shard.insert(
                shard_id,
                before
                    .entries
                    .iter()
                    .filter(|e| {
                        shard_id_for_path(&e.path, LockShardLevels::new(2).unwrap()) == shard_id
                    })
                    .cloned()
                    .collect(),
            );
        }
        rows_by_shard
            .get_mut(&src_shard)
            .unwrap()
            .retain(|e| e.path.as_str() != src);
        rows_by_shard
            .get_mut(&dst_shard)
            .unwrap()
            .push(entry(dst, &"a".repeat(64)));
        let touched = BTreeSet::from([src_shard, dst_shard]);

        let mut shard_files = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files).unwrap();
        let inodes_before: std::collections::HashMap<_, _> = shard_files
            .iter()
            .map(|f| {
                (
                    format!(
                        "gat.lock/{}",
                        f.strip_prefix(tmp.path().join("gat.lock"))
                            .unwrap()
                            .to_string_lossy()
                            .replace(std::path::MAIN_SEPARATOR, "/")
                    ),
                    std::fs::metadata(f).unwrap().ino(),
                )
            })
            .collect();

        let (published, removed) = save_sparse_shards(
            &layout(tmp.path()),
            &touched,
            &rows_by_shard,
            &BTreeMap::new(),
        )
        .unwrap();
        let src_still_has_rows = rows_by_shard
            .get(&src_shard)
            .is_some_and(|rows| !rows.is_empty());
        assert_eq!(published.len(), if src_still_has_rows { 2 } else { 1 });
        assert_eq!(
            removed,
            if src_still_has_rows {
                Vec::new()
            } else {
                vec![src_shard]
            }
        );

        let loaded = load(tmp.path()).unwrap();
        assert!(loaded.entries.iter().all(|e| e.path != src));
        assert!(loaded.entries.iter().any(|e| e.path == dst));
        let mut shard_files_after = Vec::new();
        shard::collect_files(&tmp.path().join("gat.lock"), &mut shard_files_after).unwrap();
        for file in shard_files_after {
            let rel = format!(
                "gat.lock/{}",
                file.strip_prefix(tmp.path().join("gat.lock"))
                    .unwrap()
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            );
            let ino_after = std::fs::metadata(&file).unwrap().ino();
            if rel == src_shard.to_canonical_string() && src_still_has_rows {
                assert_ne!(inodes_before[&rel], ino_after);
            } else if rel == dst_shard.to_canonical_string() {
                assert!(
                    inodes_before.get(&rel).is_none_or(|ino| *ino != ino_after),
                    "destination shard should be new or rewritten"
                );
            } else {
                assert_eq!(inodes_before[&rel], ino_after);
            }
        }
        if !src_still_has_rows {
            assert!(!tmp.path().join(src_shard.to_canonical_string()).exists());
        }
    }

    #[test]
    fn save_sparse_shards_returns_the_exact_rendered_bytes_it_published() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..24 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let before = load(tmp.path()).unwrap();
        let shard_id = shard_id_for_path(&gp("file-0.bin"), LockShardLevels::new(2).unwrap());
        let mut rows_by_shard = BTreeMap::new();
        let mut shard_entries: Vec<Entry> = before
            .entries
            .iter()
            .filter(|e| shard_id_for_path(&e.path, LockShardLevels::new(2).unwrap()) == shard_id)
            .cloned()
            .collect();
        shard_entries.push(entry(
            "added.bin",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ));
        rows_by_shard.insert(shard_id, shard_entries.clone());

        let (published, removed) = save_sparse_shards(
            &layout(tmp.path()),
            &BTreeSet::from([shard_id]),
            &rows_by_shard,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(removed.is_empty());
        assert_eq!(published.len(), 1);
        let published = &published[0];
        let on_disk =
            std::fs::read_to_string(tmp.path().join(shard_id.to_canonical_string())).unwrap();
        assert_eq!(on_disk, render_entries(&shard_entries));
        assert_eq!(
            published.identity,
            crate::lock::hash_shard_bytes(on_disk.as_bytes())
        );
    }

    #[test]
    #[cfg(unix)]
    fn save_sparse_shards_skips_an_unchanged_rewrite_but_still_reports_identity_and_stat() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..20 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let shard_id = shard_id_for_path(&gp("file-0.bin"), LockShardLevels::new(2).unwrap());
        let rows_by_shard = BTreeMap::from([(
            shard_id,
            load(tmp.path())
                .unwrap()
                .entries
                .into_iter()
                .filter(|e| {
                    shard_id_for_path(&e.path, LockShardLevels::new(2).unwrap()) == shard_id
                })
                .collect(),
        )]);
        let touched = BTreeSet::from([shard_id]);

        let file_path = tmp.path().join(shard_id.to_canonical_string());
        let inode_before = std::fs::metadata(&file_path).unwrap().ino();
        let (published, removed) = save_sparse_shards(
            &layout(tmp.path()),
            &touched,
            &rows_by_shard,
            &BTreeMap::new(),
        )
        .unwrap();

        assert!(removed.is_empty());
        assert_eq!(published.len(), 1);
        assert_eq!(std::fs::metadata(&file_path).unwrap().ino(), inode_before);
        assert_eq!(
            published[0].identity,
            crate::lock::hash_shard_bytes(std::fs::read(&file_path).unwrap().as_slice())
        );
    }

    #[test]
    fn save_sparse_shards_prunes_empty_parent_directories_when_a_shard_becomes_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "only-entry-in-its-shard.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(4).unwrap(),
        )
        .unwrap();

        let shard_id = shard_id_for_path(
            &gp("only-entry-in-its-shard.bin"),
            LockShardLevels::new(4).unwrap(),
        );
        let canonical = shard_id.to_canonical_string();
        let rel = Path::new(&canonical).strip_prefix("gat.lock").unwrap();
        let parent = tmp
            .path()
            .join("gat.lock")
            .join(rel)
            .parent()
            .unwrap()
            .to_path_buf();

        let (published, removed) = save_sparse_shards(
            &layout(tmp.path()),
            &BTreeSet::from([shard_id]),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();

        assert!(published.is_empty());
        assert_eq!(removed, vec![shard_id]);
        assert!(
            !parent.exists(),
            "empty shard parent directories should be pruned"
        );
    }

    #[test]
    fn save_sparse_shards_matches_single_threaded_follow_up_publication() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..48 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();

        let src = "file-0.bin";
        let dst = "renamed/file-0.bin";
        let src_shard = shard_id_for_path(&gp(src), LockShardLevels::new(2).unwrap());
        let dst_shard = shard_id_for_path(&gp(dst), LockShardLevels::new(2).unwrap());
        let before = load(tmp.path()).unwrap();
        let mut rows_by_shard: BTreeMap<LockShardId, Vec<Entry>> = BTreeMap::new();
        for shard_id in [src_shard, dst_shard] {
            rows_by_shard.insert(
                shard_id,
                before
                    .entries
                    .iter()
                    .filter(|e| {
                        shard_id_for_path(&e.path, LockShardLevels::new(2).unwrap()) == shard_id
                    })
                    .cloned()
                    .collect(),
            );
        }
        rows_by_shard
            .get_mut(&src_shard)
            .unwrap()
            .retain(|e| e.path.as_str() != src);
        rows_by_shard
            .get_mut(&dst_shard)
            .unwrap()
            .push(entry(dst, &"b".repeat(64)));
        let removed_only = shard_id_for_path(&gp("file-1.bin"), LockShardLevels::new(2).unwrap());
        rows_by_shard.insert(
            removed_only,
            before
                .entries
                .iter()
                .filter(|e| {
                    shard_id_for_path(&e.path, LockShardLevels::new(2).unwrap()) == removed_only
                        && e.path != "file-1.bin"
                })
                .cloned()
                .collect(),
        );
        let touched = BTreeSet::from([src_shard, dst_shard, removed_only]);

        let (default_published, default_removed) = save_sparse_shards(
            &layout(tmp.path()),
            &touched,
            &rows_by_shard,
            &BTreeMap::new(),
        )
        .unwrap();
        let default_summary: Vec<_> = default_published
            .iter()
            .map(|shard| {
                (
                    shard.shard_id,
                    shard.identity,
                    shard.proof.size,
                    render_entries(&rows_by_shard[&shard.shard_id]),
                )
            })
            .collect();
        let (single_published, single_removed) =
            save_sparse_shards_with_threads(tmp.path(), &touched, &rows_by_shard, 1).unwrap();
        let single_summary: Vec<_> = single_published
            .iter()
            .map(|shard| {
                (
                    shard.shard_id,
                    shard.identity,
                    shard.proof.size,
                    render_entries(&rows_by_shard[&shard.shard_id]),
                )
            })
            .collect();

        assert_eq!(default_removed, single_removed);
        assert_eq!(default_summary, single_summary);
        for shard in default_published {
            assert_eq!(
                std::fs::read_to_string(tmp.path().join(shard.shard_id.to_canonical_string()))
                    .unwrap(),
                render_entries(&rows_by_shard[&shard.shard_id])
            );
        }
    }

    /// `save_sharded` always assigns each path to exactly one shard (by
    /// hash), so this can only happen with hand-edited/corrupted shard
    /// files -- but `load_sharded` must still fail closed instead of
    /// silently keeping whichever shard's row for the duplicated path it
    /// happened to concatenate first (or last).
    #[test]
    fn load_sharded_rejects_a_path_tracked_by_two_different_shards() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("00.tsv"),
            format!(
                "{0}\n{1}\tshared.bin\n",
                super::super::VERSION,
                "a".repeat(64)
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("01.tsv"),
            format!(
                "{0}\n{1}\tshared.bin\n",
                super::super::VERSION,
                "b".repeat(64)
            ),
        )
        .unwrap();

        let err = load(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("shared.bin") && msg.contains("more than one shard"),
            "expected a duplicate-path error, got: {msg}"
        );
    }

    /// `save_sharded` always writes a path's row into the shard
    /// `shard_id_for_path` predicts for it -- but `load_sharded` must
    /// still fail closed on a hand-edited/corrupted tree where a row
    /// physically lives in the wrong shard file, rather than silently
    /// trusting shard placement it never re-derives.
    #[test]
    fn load_sharded_rejects_a_row_stored_in_the_wrong_shard() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        // "misplaced.bin" does not hash to shard "00.tsv" -- whichever
        // shard it actually hashes to, "00.tsv" is not it (checked below).
        let path = "misplaced.bin";
        let actual_shard = shard_id_for_path(&gp(path), LockShardLevels::new(1).unwrap());
        assert_ne!(actual_shard, "gat.lock/00.tsv");
        std::fs::write(
            dir.join("00.tsv"),
            format!("{0}\n{1}\t{path}\n", super::super::VERSION, "a".repeat(64)),
        )
        .unwrap();

        let err = load(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(path) && msg.contains("belongs in"),
            "expected a shard-placement error, got: {msg}"
        );
    }

    /// A hand-edited/corrupted sharded tree can spread a directory-prefix
    /// conflict across two different shard files (unlike a single-shard
    /// conflict, which `Lock::parse` already rejects per shard);
    /// `load_sharded` must still catch it once all shards are aggregated.
    #[test]
    fn load_sharded_rejects_a_directory_prefix_conflict_spread_across_shards() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        let oid = "a".repeat(64);
        let foo_shard = shard_id_for_path(&gp("foo"), LockShardLevels::new(1).unwrap())
            .to_canonical_string()
            .strip_prefix("gat.lock/")
            .unwrap()
            .to_string();
        let foo_bar_shard = shard_id_for_path(&gp("foo/bar"), LockShardLevels::new(1).unwrap())
            .to_canonical_string()
            .strip_prefix("gat.lock/")
            .unwrap()
            .to_string();
        assert_ne!(
            foo_shard, foo_bar_shard,
            "test needs paths that land in different shards"
        );
        std::fs::write(
            dir.join(&foo_shard),
            format!("{0}\n{oid}\tfoo\n", super::super::VERSION),
        )
        .unwrap();
        std::fs::write(
            dir.join(&foo_bar_shard),
            format!("{0}\n{oid}\tfoo/bar\n", super::super::VERSION),
        )
        .unwrap();

        let err = load(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("foo"),
            "expected the directory-prefix conflict named, got: {msg}"
        );
    }

    #[test]
    fn shard_id_for_path_maps_every_path_to_the_flat_sentinel_when_levels_is_zero() {
        assert_eq!(
            shard_id_for_path(&gp("a.bin"), LockShardLevels::new(0).unwrap()),
            "gat.lock"
        );
        assert_eq!(
            shard_id_for_path(&gp("nested/b.bin"), LockShardLevels::new(0).unwrap()),
            "gat.lock"
        );
    }

    #[test]
    fn publish_flat_shard_writes_a_single_file_never_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = vec![entry(
            "a.bin",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )];

        let published = publish_flat_shard(&layout(tmp.path()), &entries, None)
            .unwrap()
            .unwrap();

        assert_eq!(published.shard_id, "gat.lock");
        assert!(tmp.path().join("gat.lock").is_file());
        assert_eq!(
            load(tmp.path()).unwrap().entries,
            vec![entry(
                "a.bin",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )]
        );
    }

    #[test]
    fn publish_flat_shard_with_no_entries_removes_the_file_and_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        assert!(tmp.path().join("gat.lock").is_file());

        let published = publish_flat_shard(&layout(tmp.path()), &[], None).unwrap();

        assert!(published.is_none());
        assert!(!tmp.path().join("gat.lock").exists());
    }

    #[test]
    #[cfg(unix)]
    fn publish_flat_shard_skips_an_unchanged_rewrite() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let path = tmp.path().join("gat.lock");
        let inode_before = std::fs::metadata(&path).unwrap().ino();

        let _published = publish_flat_shard(&layout(tmp.path()), &lock.entries, None)
            .unwrap()
            .unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode_before);
    }

    /// `publish_rendered_shard`'s tier 1 (matching prior identity
    /// *and* proof) is a pure stat -- zero content reads, zero writes.
    #[test]
    #[cfg(unix)]
    fn publish_rendered_shard_tier1_matching_prior_identity_reuses_the_proof_with_no_io() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let path = tmp.path().join("gat.lock");
        let inode_before = std::fs::metadata(&path).unwrap().ino();
        let rendered = render_entries(&lock.entries);
        let identity = crate::lock::hash_shard_bytes(rendered.as_bytes());
        let proof = crate::file_state::observe_regular_file_no_follow(&path).unwrap();

        let read_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = read_count.clone();
        let watched = path.clone();
        let _guard = RaceHookGuard::acquire();
        race_test_hooks::set(move |p: &Path| {
            if p == watched {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let published =
            publish_flat_shard(&layout(tmp.path()), &lock.entries, Some((identity, proof)))
                .unwrap()
                .unwrap();

        assert_eq!(read_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode_before);
        assert_eq!(published.proof, proof);
    }

    /// A matching prior proof whose identity is already known
    /// to differ from the target content publishes directly (the proof
    /// already establishes exactly what the current bytes are, so
    /// re-reading them first would gain nothing) -- one stat, zero
    /// content reads, one write.
    #[test]
    fn publish_rendered_shard_tier1_stale_identity_with_matching_proof_writes_without_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let path = tmp.path().join("gat.lock");
        let stale_proof = crate::file_state::observe_regular_file_no_follow(&path).unwrap();
        let stale_identity =
            crate::lock::hash_shard_bytes(render_entries(&lock.entries).as_bytes());

        insert(&mut lock, "b.bin", "b".repeat(64));

        let read_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = read_count.clone();
        let watched = path;
        let _guard = RaceHookGuard::acquire();
        race_test_hooks::set(move |p: &Path| {
            if p == watched {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let published = publish_flat_shard(
            &layout(tmp.path()),
            &lock.entries,
            Some((stale_identity, stale_proof)),
        )
        .unwrap()
        .unwrap();

        assert_eq!(read_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            load(tmp.path()).unwrap().entries,
            lock.entries,
            "the new content must actually have been published"
        );
        assert_ne!(published.proof, stale_proof);
    }

    /// A proof miss where the existing file's
    /// bytes already coherently match what would be published performs
    /// exactly one content read and zero writes, returning a proof paired
    /// with the exact bytes just compared.
    #[test]
    #[cfg(unix)]
    fn publish_rendered_shard_proof_miss_with_unchanged_content_reads_once_and_skips_the_write() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let path = tmp.path().join("gat.lock");
        let inode_before = std::fs::metadata(&path).unwrap().ino();

        let read_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = read_count.clone();
        let watched = path.clone();
        let _guard = RaceHookGuard::acquire();
        race_test_hooks::set(move |p: &Path| {
            if p == watched {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        // No prior at all: a proof miss.
        let published = publish_flat_shard(&layout(tmp.path()), &lock.entries, None)
            .unwrap()
            .unwrap();

        assert_eq!(read_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode_before);
        assert_eq!(
            published.proof,
            crate::file_state::observe_regular_file_no_follow(&path).unwrap()
        );
    }

    /// A size mismatch against the existing file is a definitive
    /// zero-content-read inequality shortcut -- falls straight through to
    /// an atomic write without ever opening the file.
    #[test]
    fn publish_rendered_shard_size_mismatch_skips_the_read_and_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let path = tmp.path().join("gat.lock");

        insert(&mut lock, "b.bin", "b".repeat(64));

        let read_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = read_count.clone();
        let watched = path;
        let _guard = RaceHookGuard::acquire();
        race_test_hooks::set(move |p: &Path| {
            if p == watched {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        publish_flat_shard(&layout(tmp.path()), &lock.entries, None)
            .unwrap()
            .unwrap();

        assert_eq!(read_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(load(tmp.path()).unwrap().entries, lock.entries);
    }

    /// A missing destination path performs zero old-content
    /// reads (there is nothing to read) and one write.
    #[test]
    fn publish_rendered_shard_missing_path_reads_nothing_and_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        let path = tmp.path().join("gat.lock");

        let read_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = read_count.clone();
        let watched = path;
        let _guard = RaceHookGuard::acquire();
        race_test_hooks::set(move |p: &Path| {
            if p == watched {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        publish_flat_shard(&layout(tmp.path()), &lock.entries, None)
            .unwrap()
            .unwrap();

        assert_eq!(read_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(tmp.path().join("gat.lock").is_file());
    }

    /// Ordinary [`save`]/[`save_matching_disk_shape`]
    /// (the proof-agnostic path every mutation command uses when it
    /// doesn't consume publish evidence) must never compute a
    /// [`super::super::ShardContentIdentity`] BLAKE3 hash or mint a
    /// [`crate::file_state::StatProof`] merely to discard it -- both the
    /// flat and sharded shapes.
    #[test]
    fn ordinary_save_never_computes_identity_or_proof_values_it_would_discard() {
        use crate::lock::identity_test_support;

        // Flat shape: `save_file_atomic` runs on the calling thread, so a
        // plain before/after snapshot on this thread is enough.
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..8 {
            insert(&mut lock, &format!("file-{i}.bin"), "a".repeat(64));
        }
        let hash_before = identity_test_support::hash_shard_bytes_call_count();
        let proof_before = crate::file_state::test_support::stat_proof_from_metadata_call_count();
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        assert_eq!(
            identity_test_support::hash_shard_bytes_call_count(),
            hash_before,
            "flat save must not compute a ShardContentIdentity it discards"
        );
        assert_eq!(
            crate::file_state::test_support::stat_proof_from_metadata_call_count(),
            proof_before,
            "flat save must not mint a StatProof it discards"
        );

        // Re-save unchanged (tier-2 coherent-read-compare branch, still
        // proof-agnostic): still no identity hash, and the proof that
        // `coherent_observation` returns for free from its own mandatory
        // before/after stat is still discarded rather than driving any
        // extra work.
        save_matching_disk_shape(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        assert_eq!(
            identity_test_support::hash_shard_bytes_call_count(),
            hash_before,
            "an unchanged flat re-save must not compute a ShardContentIdentity either"
        );

        // Sharded shape: `save_sharded`'s bucket writes run in parallel via
        // Rayon, so run the whole save on a single-threaded pool and take
        // the snapshot from inside `install` (the same thread the writes
        // actually run on).
        let sharded_tmp = tempfile::tempdir().unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let (hash_delta, proof_delta) = pool.install(|| {
            let hash_before = identity_test_support::hash_shard_bytes_call_count();
            let proof_before =
                crate::file_state::test_support::stat_proof_from_metadata_call_count();
            save(
                &lock,
                sharded_tmp.path(),
                crate::lock::LockShardLevels::new(4).unwrap(),
            )
            .unwrap();
            (
                identity_test_support::hash_shard_bytes_call_count() - hash_before,
                crate::file_state::test_support::stat_proof_from_metadata_call_count()
                    - proof_before,
            )
        });
        assert_eq!(
            hash_delta, 0,
            "sharded save must not compute a ShardContentIdentity it discards"
        );
        assert_eq!(
            proof_delta, 0,
            "sharded save must not mint a StatProof it discards"
        );
    }

    /// Crash-recovery evidence construction
    /// ([`super::observe_full_lock_with_evidence`], used by
    /// `commands::mount::txn::recover_pending_mount_txn` and
    /// `commands::system::state`'s repair path) reads every live shard
    /// file exactly once (never re-reads any of them while validating
    /// cross-shard invariants) and never writes anything back to disk
    /// merely to mint the proofs it returns -- it is purely a coherent
    /// read pass over whatever's already on disk.
    #[test]
    fn observe_full_lock_with_evidence_reads_every_shard_exactly_once_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        for i in 0..24 {
            insert(&mut lock, &format!("file-{i:04}.bin"), "a".repeat(64));
        }
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(4).unwrap(),
        )
        .unwrap();

        let shard_paths: Vec<std::path::PathBuf> = list_shard_files(tmp.path())
            .unwrap()
            .into_iter()
            .map(|f| f.full_path)
            .collect();
        let mtimes_before: Vec<_> = shard_paths
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
            .collect();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let (read_delta, evidence) = pool.install(|| {
            let before = crate::lock::test_support::full_lock_evidence_shard_reads();
            let evidence = observe_full_lock_with_evidence(tmp.path()).unwrap();
            let after = crate::lock::test_support::full_lock_evidence_shard_reads();
            (after - before, evidence)
        });

        assert_eq!(
            read_delta,
            evidence.shards.len(),
            "must read each live shard file exactly once, no re-reads"
        );

        for (path, mtime_before) in shard_paths.iter().zip(mtimes_before) {
            let mtime_after = std::fs::metadata(path).unwrap().modified().unwrap();
            assert_eq!(
                mtime_before, mtime_after,
                "recovery evidence construction must never rewrite a shard file"
            );
        }
    }

    /// Publication on the write side and
    /// [`super::resolve_shard_identity`] (read side) must interpret the
    /// exact same `(identity, proof)` prior identically under the shared
    /// [`crate::file_state::StatProof`] trust contract -- both a stat-only
    /// hit with zero content reads for an unchanged shard.
    #[test]
    fn publication_and_resolve_shard_identity_agree_on_the_same_stat_only_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let path = tmp.path().join("gat.lock");
        let rendered = render_entries(&lock.entries);
        let identity = crate::lock::hash_shard_bytes(rendered.as_bytes());
        let proof = crate::file_state::observe_regular_file_no_follow(&path).unwrap();

        // Read side: resolve_shard_identity must report a stat-only hit
        // with the exact same identity/proof, no content read.
        let resolution =
            crate::lock::resolve_shard_identity(&path, Some((identity, Some(proof))), || {
                panic!("resolve_shard_identity must not read the shard on a stat-only hit")
            })
            .unwrap();
        match resolution {
            crate::lock::ShardIdentityResolution::StatOnly {
                identity: resolved_identity,
                proof: resolved_proof,
            } => {
                assert_eq!(resolved_identity, identity);
                assert_eq!(resolved_proof, proof);
            }
            crate::lock::ShardIdentityResolution::Coherent { .. } => {
                panic!("expected a stat-only hit, not a coherent read")
            }
        }

        // Write side: publish_rendered_shard (via publish_flat_shard) must
        // agree -- the same prior is a tier-1 hit reusing the exact same
        // proof, with no content read and no rewrite.
        let published =
            publish_flat_shard(&layout(tmp.path()), &lock.entries, Some((identity, proof)))
                .unwrap()
                .unwrap();
        assert_eq!(published.identity, identity);
        assert_eq!(published.proof, proof);
    }

    /// The accepted metadata-preserving-rewrite limitation applies
    /// identically on both sides of the shared
    /// [`crate::file_state::StatProof`] trust contract. When a shard's
    /// bytes are rewritten in a way that happens to preserve the exact
    /// persisted `(size, mtime_secs, mtime_nanos)`, both
    /// [`super::resolve_shard_identity`] (read side) and
    /// [`publish_rendered_shard`] (write side, via
    /// [`ShardPublishPolicy::SkipIfUnchanged`]'s tier 1) trust the stale
    /// prior identity without reading the actual (changed) bytes -- this
    /// test documents that consistency, not a stronger guarantee either
    /// side secretly has over the other.
    #[test]
    fn metadata_preserving_rewrite_is_trusted_identically_by_both_publication_and_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shard");
        std::fs::write(&path, b"aaaa").unwrap();
        let identity = crate::lock::hash_shard_bytes(b"aaaa");
        let proof = crate::file_state::observe_regular_file_no_follow(&path).unwrap();

        // Rewrite with different, same-length content, then force the
        // exact same persisted mtime back onto it -- a metadata-preserving
        // rewrite indistinguishable from "unchanged" by `StatProof`
        // equality alone.
        std::fs::write(&path, b"bbbb").unwrap();
        let mtime = std::time::UNIX_EPOCH
            + std::time::Duration::new(
                u64::try_from(proof.mtime_secs).unwrap(),
                u32::try_from(proof.mtime_nanos).unwrap(),
            );
        let times = std::fs::FileTimes::new().set_modified(mtime);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(times)
            .unwrap();
        assert_eq!(
            crate::file_state::observe_regular_file_no_follow(&path),
            Some(proof),
            "the rewrite must be metadata-preserving for this test to be meaningful"
        );

        // Read side: trusts the stale `identity` (for "aaaa"), never
        // reading the actual current "bbbb" bytes.
        let resolution =
            crate::lock::resolve_shard_identity(&path, Some((identity, Some(proof))), || {
                panic!("a stat-only hit must never read the shard's actual bytes")
            })
            .unwrap();
        assert!(matches!(
            resolution,
            crate::lock::ShardIdentityResolution::StatOnly {
                identity: resolved_identity,
                proof: resolved_proof,
            } if resolved_identity == identity && resolved_proof == proof
        ));

        // Write side: publishing the exact same content ("aaaa") under
        // the same stale prior is tier 1's first branch (`prior.identity
        // == target identity`) -- it also trusts the stale proof and
        // returns immediately, never verifying that the file's actual
        // bytes ("bbbb") still match.
        let evidence = publish_rendered_shard(
            &path,
            LockShardId::flat(),
            "aaaa",
            identity,
            ShardPublishPolicy::SkipIfUnchanged {
                prior: Some((identity, proof)),
            },
        )
        .unwrap();
        assert_eq!(evidence.proof, proof);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"bbbb",
            "tier 1 must not have rewritten the file -- it trusted the stale proof \
             exactly as the read side did, leaving the actual (different) bytes in place"
        );
    }

    /// An ordinary stale prior (the file legitimately changed on
    /// disk since the prior proof was recorded -- a real mtime/size
    /// change, not a metadata-preserving rewrite) must never be trusted.
    /// Publication falls back to the filesystem (tier 2/3) and produces
    /// the correct target content, exactly as if no prior had been
    /// supplied at all.
    #[test]
    fn publish_rendered_shard_falls_back_to_the_filesystem_when_the_prior_proof_is_genuinely_stale()
    {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("shard");
        std::fs::write(&path, b"aaaa").unwrap();
        let stale_identity = crate::lock::hash_shard_bytes(b"aaaa");
        let stale_proof = crate::file_state::observe_regular_file_no_follow(&path).unwrap();

        // A genuine, later, independent write -- size differs, so the
        // stale proof can never match regardless of mtime resolution.
        std::fs::write(&path, b"a different length of content entirely").unwrap();

        let evidence = publish_rendered_shard(
            &path,
            LockShardId::flat(),
            "brand new target content",
            crate::lock::hash_shard_bytes(b"brand new target content"),
            ShardPublishPolicy::SkipIfUnchanged {
                prior: Some((stale_identity, stale_proof)),
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "brand new target content"
        );
        assert_eq!(
            evidence.identity,
            crate::lock::hash_shard_bytes(b"brand new target content")
        );
        assert_ne!(
            evidence.proof, stale_proof,
            "a genuinely stale prior must never be reused as the published proof"
        );
        assert_eq!(
            evidence.proof,
            crate::file_state::observe_regular_file_no_follow(&path).unwrap()
        );
    }

    #[test]
    fn publish_flat_shard_replaces_a_stale_sharded_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lock = Lock::default();
        insert(&mut lock, "a.bin", "a".repeat(64));
        save(
            &lock,
            tmp.path(),
            crate::lock::LockShardLevels::new(2).unwrap(),
        )
        .unwrap();
        assert!(tmp.path().join("gat.lock").is_dir());

        publish_flat_shard(&layout(tmp.path()), &lock.entries, None).unwrap();

        assert!(tmp.path().join("gat.lock").is_file());
        assert_eq!(load(tmp.path()).unwrap().entries, lock.entries);
    }
}
