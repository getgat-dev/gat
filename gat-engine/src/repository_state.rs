//! Canonical desired-state revision identity.
//!
//! [`DesiredRevision`] deterministically identifies the canonical
//! desired-state generation `gat.lock` currently represents -- a thin
//! wrapper around a shared [`CanonicalDesiredIdentity`]
//! ([`gat_core::lock`]), never a second, independently-derived identity
//! and never a second retained copy of the lock contents themselves.
//! It identifies only desired state and says nothing about effective config:
//! config is snapshot-isolated for the lifetime of one operation and is never
//! itself a reason to reject a mutation.
//!
//! A composite operation (`pull`, hook-triggered fetch+sync, repair+resync)
//! may do real remote/cache I/O *outside* [`gat_io::RepoLock`], but
//! must prove -- via [`crate::repository::Repository::revalidate_desired_revision`] -- that the
//! desired revision it is about to mutate the working tree/materialized
//! state under still matches the revision its policy/selection were
//! derived from, before performing that mutation.
//!
//! Desired-revision acquisition and revalidation are repository behavior
//! with their own I/O (reading `gat.lock`,
//! best-effort accelerating from the materialized-state mirror), not a
//! neutral runtime session primitive. It delegates observation to
//! [`gat_io::StateStore`], which keeps the optional `SQLite`
//! stat-cache accelerator inside `gat-io`; engine never sees the catalog,
//! stat proofs, or state implementation.

use crate::repository::{RepoError, Repository as Repo};
use gat_core::lock::CanonicalDesiredIdentity;
use gat_io::AtomicError;
use gat_io::{LockError, LockStore, StateStore};

/// Everything [`current_desired_revision`]/
/// `reshape_and_capture_revision` can fail with: a real
/// semantic repository-state access failure, a reshape's
/// [`crate::repository::RepoError`], or the dedicated
/// [`StaleDesiredRevisionError`] this module raises itself when a
/// composite operation's captured revision differs from the current
/// on-disk state.
#[derive(Debug, thiserror::Error)]
pub enum DesiredRevisionError {
    #[error(transparent)]
    Lock(crate::repository_access::RepositoryAccessError),
    #[error(transparent)]
    Atomic(crate::repository_access::RepositoryAccessError),
    #[error(transparent)]
    Repository(#[from] RepoError),
    #[error(transparent)]
    Stale(#[from] StaleDesiredRevisionError),
}

impl From<LockError> for DesiredRevisionError {
    fn from(source: LockError) -> Self {
        Self::Lock(crate::repository_access::RepositoryAccessError::from_lock(
            source,
        ))
    }
}

impl From<AtomicError> for DesiredRevisionError {
    fn from(source: AtomicError) -> Self {
        Self::Atomic(crate::repository_access::RepositoryAccessError::from_atomic(source))
    }
}

type Result<T> = std::result::Result<T, DesiredRevisionError>;

/// Test-only call counter for [`current_desired_revision`] (stable-
/// observation): production code never reads this: it
/// exists purely so a test can prove a warm `recover_and_open_coherent_snapshot`
/// performs zero full canonical observations, deriving its `DesiredRevision`
/// entirely from the one `desired_index::refresh()` it already ran.
/// The canonical desired-state (`gat.lock`) generation an operation depends
/// on for reconciliation: a thin wrapper around the one shared
/// [`CanonicalDesiredIdentity`] every consumer of `gat.lock`'s
/// current state agrees on -- this type says only *when* that identity was
/// captured, never a second, independently-derived identity of its own.
/// Compact by design -- carrying a full copy of the lock content here would
/// duplicate exactly the state `DesiredSnapshot` already owns. Deliberately
/// does not include any config identity:
/// config is snapshot-isolated per operation, not part of what makes a
/// mutation "stale".
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DesiredRevision {
    identity: CanonicalDesiredIdentity,
}

impl DesiredRevision {
    /// Wraps an already-established [`CanonicalDesiredIdentity`] as a
    /// [`DesiredRevision`] directly, for a caller that obtained the
    /// identity from somewhere other than a fresh
    /// [`current_desired_revision`] observation -- namely
    /// the desired-index refresh result,
    /// which already proves the exact generation it materialized (issue
    /// stable observation). Deliberately narrow: it
    /// exists so engine snapshot acquisition can build a
    /// `DesiredRevision` from a refresh result without re-deriving or
    /// duplicating any canonical identity logic itself.
    pub(crate) const fn from_identity(identity: CanonicalDesiredIdentity) -> Self {
        Self { identity }
    }

    /// The canonical identity this revision was captured at -- compared
    /// directly against `desired_index::refresh()`'s returned identity by
    /// [`super::mutation::MutationGuard::require_desired_identity`] (issue
    /// so the mirror a mutation reconciles against is
    /// proven to be exactly the revision this operation was admitted to
    /// mutate.
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) const fn identity(&self) -> CanonicalDesiredIdentity {
        self.identity
    }

    /// Test-support view of the wrapped identity for cross-crate
    /// integration assertions.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn identity(&self) -> CanonicalDesiredIdentity {
        self.identity
    }
}

impl std::fmt::Debug for DesiredRevision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesiredRevision")
            .field("identity", &self.identity)
            .finish()
    }
}

/// A dedicated stale-revision error: a
/// composite operation must never silently combine an old
/// [`DesiredRevision`] snapshot with newly-loaded canonical desired state.
#[derive(Debug)]
pub struct StaleDesiredRevisionError;

impl std::fmt::Display for StaleDesiredRevisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "desired state (gat.lock) changed during this operation; \
             re-run the command to observe the current state before mutating anything"
        )
    }
}

impl std::error::Error for StaleDesiredRevisionError {}

/// Recomputes the repository's *current* [`DesiredRevision`] directly from
/// the Git-visible `gat.lock` representation and compares it against
/// `expected`, returning [`StaleDesiredRevisionError`] if they disagree.
/// Only meaningful while the caller holds `_lock` -- taking it by
/// reference is this function's proof-of-lock-ownership contract: a caller
/// cannot call this without having already
/// acquired [`gat_io::RepoLock`] itself.
///
/// Never *creates* [`gat_io::StateStore`] to
/// answer this question. If one already exists, it is consulted only as a
/// best-effort speed-up whose every hit is itself re-verified by a genuine
/// stat comparison: the canonical revision is authoritatively the shared
/// [`CanonicalDesiredIdentity`] folded directly from the on-disk
/// `gat.lock`/`gat.lock/` shard bytes (see [`current_desired_revision`]).
/// A missing, unreadable, or ordinarily stale mirror row only ever costs
/// a cache miss (falling back to one coherent read/hash of the affected
/// shard), never producing a wrong answer for that specific gap -- but
/// this is not an unconditional guarantee against every possible drift:
/// a mirror row whose recorded `StatProof` still exactly matches live
/// metadata for content that changed via an exact metadata-preserving
/// rewrite is outside what any stat-based proof (mirrored or not) can
/// detect. A concurrent
/// *config-only* edit is never a reason to reject here either:
/// config is snapshot-isolated for this operation's whole lifetime, not
/// part of this revision.
/// Computes the repository's current [`DesiredRevision`] directly from the
/// live `gat.lock` representation on disk through
/// [`gat_io::StateStore`] -- authoritatively independent of
/// [`gat_io::StateStore`]'s derived `SQLite`
/// mirror: a missing, unreadable, or
/// ordinarily stale mirror row only ever costs a fallback read/hash for
/// the affected shard(s), never a wrong answer for that specific gap --
/// the shared stat-cache rule for every reader and publisher: an established
/// identity is
/// reused exactly when the live `StatProof` still matches, and any miss
/// simply re-derives it from bytes. The one thing this (and any
/// stat-based cache) does *not* detect is a rewrite whose metadata
/// exactly reproduces the recorded proof -- semantic cache corruption of
/// that specific kind is outside the accepted proof guarantee, not
/// something this fallback path closes. When an existing mirror *is*
/// readable, its durable per-shard stat cache is consulted purely as
/// a best-effort accelerator, using the same
/// stat-proof comparison `desired_index::refresh`'s own
/// local stat cache uses, just re-run here against the mirror's
/// last-recorded per-shard identity instead of being trusted implicitly.
/// This never *creates* that mirror (`StateStore::open_if_exists`),
/// so calling this from a strictly read-only context (`--dry-run`,
/// mutation-gate revalidation) is always safe, and a first-ever call
/// against a repo with no mirror yet still returns the right answer, just
/// via a direct read + BLAKE3 for every shard.
///
/// A shard-layout-only reshape (splitting/merging/re-leveling `gat.lock`
/// shards without changing any path/oid) changes this identity, since it
/// changes the exact shard-id set XOR-accumulated into it; each per-shard
/// component is keyed by its own `shard_id`
/// (see [`gat_core::lock::CanonicalDesiredIdentity`]), so two
/// different shard-id sets are expected (with overwhelming
/// probability, by BLAKE3's collision resistance, not as a mathematical
/// guarantee) to XOR-reduce to two different aggregates -- even though
/// canonical desired state (as reconciliation observes it) did not. This
/// is deliberate: a reshape
/// landing between a composite operation's snapshot capture and its
/// later mutation is treated exactly like any other concurrent
/// desired-state change -- the mutation is rejected and the caller
/// re-runs, never silently applying stale/mismatched state. See
/// `shard_layout_only_reshape_changes_the_desired_revision` in
/// the desired-revision command integration tests.
pub fn current_desired_revision(repo: &Repo) -> Result<DesiredRevision> {
    repo.current_desired_revision()
}

pub(crate) fn observe_desired_revision(repo: &Repo) -> Result<DesiredRevision> {
    // Test-only instrumentation:
    // counts calls to this full-canonical-observation entry point so a
    // test can prove a warm `recover_and_open_coherent_snapshot()` never
    // calls it at all -- it derives its `DesiredRevision` directly from
    // `desired_index::refresh()`'s own returned identity instead of a
    // separate live canonical observation.
    #[cfg(any(test, feature = "test-support"))]
    crate::test_support::record_canonical_observation();

    let identity = StateStore::observe_canonical_identity(repo.layout())?;
    Ok(DesiredRevision::from_identity(identity))
}

/// Reshapes `gat.lock` (if `cfg.lock.shard_levels()` calls for a different
/// on-disk shape than currently exists) and captures the resulting
/// canonical [`DesiredRevision`] inside one [`gat_io::RepoLock`]
/// critical section, rather than the
/// release-then-reacquire pattern of reshaping under one lock acquisition
/// and separately reading the current revision under a second, later one.
///
/// A reshape is the calling operation's own legitimate desired-state
/// mutation, not an external race, so `gat sync` (the only production
/// caller) replaces its operation's captured snapshot with the revision
/// this returns rather than tripping the mutation gate's stale-revision
/// rejection. Capturing that revision under the same lock the reshape
/// itself ran under closes the window where a real external write landing
/// between an unlocked reshape and a separately-locked revision read could
/// otherwise be silently folded into the "post-reshape" revision as if it
/// were part of the reshape itself. Revision capture here reads `gat.lock`
/// directly -- it never calls
/// `desired_index::refresh` or reads the materialized mirror, so the
/// returned revision describes exactly the newly published Git-visible
/// lock set the reshape itself produced.
///
/// After a reshape, every shard file it wrote is, by construction, brand
/// new to the desired mirror's catalog (a reshape's shard boundaries
/// never line up with the prior ones), but that requires no
/// special seeding step: [`current_desired_revision`]'s own
/// catalog read just below simply treats every touched shard as a proof
/// miss and performs one coherent read/hash to establish its identity,
/// exactly as it would for any other freshly written shard.
///
/// Returns the new `shard_levels` and desired revision if a reshape actually
/// happened, or `None` after the I/O-owned no-op path. `on_reshape` runs only
/// after the mismatch has been confirmed while holding the repository lock.
pub(crate) fn reshape_and_capture_revision(
    repo: &Repo,
    cfg: &gat_core::config::Config,
    on_reshape: impl FnOnce(),
) -> Result<Option<(gat_core::lock::LockShardLevels, DesiredRevision)>> {
    let target = cfg.lock.shard_levels();
    let Some(reshape) = LockStore::begin_repository_reshape(repo.layout(), target)? else {
        return Ok(None);
    };
    on_reshape();
    let completed = reshape.apply()?;
    let revision = repo.current_desired_revision()?;
    drop(completed);
    Ok(Some((target, revision)))
}

#[cfg(test)]
mod contract_tests {
    use crate::{Repository as Repo, current_desired_revision};
    use gat_core::config::ConfigScope;
    use gat_core::lock::{Entry, Lock, LockShardLevels};
    use gat_core::{lexical_path::GatPath, oid::Oid};
    use gat_io::{LockStore, StateStore};

    fn entry(path: &str, byte: u8) -> Entry {
        Entry {
            path: GatPath::parse_canonical(path).unwrap(),
            oid: Oid::from_hex(&format!("{byte:02x}").repeat(32)).unwrap(),
        }
    }

    fn tracked_repo() -> (crate::test_harness::TestRepo, Repo) {
        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_lock(&Lock {
            entries: vec![entry("a.bin", 1)],
        })
        .unwrap();
        (tmp, repo)
    }

    #[test]
    fn repository_revalidation_accepts_a_config_only_change() {
        let (_tmp, repo) = tracked_repo();
        let expected = current_desired_revision(&repo).unwrap();
        let mut config = repo.load_config_scoped(ConfigScope::Project).unwrap();
        config.sync.trust_state = Some(true);
        repo.save_config_scoped(&config, ConfigScope::Project)
            .unwrap();

        repo.revalidate_desired_revision(&expected).unwrap();
    }

    #[test]
    fn repository_revalidation_rejects_a_desired_state_change() {
        let (_tmp, repo) = tracked_repo();
        let expected = current_desired_revision(&repo).unwrap();
        repo.save_lock(&Lock {
            entries: vec![entry("a.bin", 1), entry("b.bin", 2)],
        })
        .unwrap();

        let error = repo.revalidate_desired_revision(&expected).unwrap_err();
        assert_eq!(
            error.to_string(),
            "desired state (gat.lock) changed during this operation; \
             re-run the command to observe the current state before mutating anything"
        );
    }

    #[test]
    fn repository_revalidation_rejects_a_combined_change_on_the_desired_state_half() {
        let (_tmp, repo) = tracked_repo();
        let expected = current_desired_revision(&repo).unwrap();
        let mut config = repo.load_config_scoped(ConfigScope::Project).unwrap();
        config.sync.trust_state = Some(true);
        repo.save_config_scoped(&config, ConfigScope::Project)
            .unwrap();
        repo.save_lock(&Lock {
            entries: vec![entry("a.bin", 1), entry("b.bin", 2)],
        })
        .unwrap();

        assert!(matches!(
            repo.revalidate_desired_revision(&expected),
            Err(crate::DesiredRevisionError::Stale(_))
        ));
    }

    #[test]
    fn repository_revalidation_accepts_an_unchanged_revision() {
        let (_tmp, repo) = tracked_repo();
        let expected = current_desired_revision(&repo).unwrap();
        repo.revalidate_desired_revision(&expected).unwrap();
    }

    #[test]
    fn shard_layout_only_reshape_changes_the_desired_revision() {
        let (_tmp, repo) = tracked_repo();
        let before = current_desired_revision(&repo).unwrap();
        let _completed =
            LockStore::begin_repository_reshape(repo.layout(), LockShardLevels::new(2).unwrap())
                .unwrap()
                .unwrap()
                .apply()
                .unwrap();
        let lock = LockStore::load_repository(repo.layout()).unwrap();
        assert_eq!(LockStore::load_repository(repo.layout()).unwrap(), lock);

        assert_ne!(current_desired_revision(&repo).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn current_desired_revision_reuses_a_safe_stat_cache_hit_without_reading_shard_content() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, repo) = tracked_repo();
        let mut store = StateStore::open(repo.layout()).unwrap();
        crate::workspace::sync::refresh_desired_index(&repo, &mut store).unwrap();
        let expected = current_desired_revision(&repo).unwrap();
        let shard_path = tmp.path().join("gat.lock");
        let original_mode = std::fs::metadata(&shard_path).unwrap().permissions().mode();
        std::fs::set_permissions(&shard_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let actual = current_desired_revision(&repo);

        std::fs::set_permissions(&shard_path, std::fs::Permissions::from_mode(original_mode))
            .unwrap();
        assert_eq!(actual.unwrap(), expected);
    }

    #[test]
    fn refresh_and_current_desired_revision_agree_on_the_same_prior_and_bytes() {
        let (tmp, repo) = tracked_repo();
        let mut store = StateStore::open(repo.layout()).unwrap();
        let established = crate::workspace::sync::refresh_desired_index(&repo, &mut store).unwrap();
        let refreshed = crate::workspace::sync::refresh_desired_index(&repo, &mut store).unwrap();
        assert_eq!(
            refreshed.desired_identity,
            current_desired_revision(&repo).unwrap().identity()
        );
        assert_eq!(established.desired_identity, refreshed.desired_identity);

        repo.save_lock(&Lock {
            entries: vec![entry("a.bin", 1), entry("b.bin", 2)],
        })
        .unwrap();
        let changed = crate::workspace::sync::refresh_desired_index(&repo, &mut store).unwrap();
        let changed_revision = current_desired_revision(&repo).unwrap();
        assert_eq!(changed.desired_identity, changed_revision.identity());
        assert_ne!(changed.desired_identity, refreshed.desired_identity);

        drop(store);
        let database_path = tmp.path().join(".gat/state/state.sqlite3");
        std::fs::remove_file(&database_path).unwrap();
        for suffix in ["-wal", "-shm"] {
            let sidecar = database_path.with_file_name(format!(
                "{}{suffix}",
                database_path.file_name().unwrap().to_string_lossy()
            ));
            let _ = std::fs::remove_file(sidecar);
        }
        assert_eq!(current_desired_revision(&repo).unwrap(), changed_revision);
    }
}
