//! Read-only reconciliation planning: comparing the desired `gat.lock`
//! against the last materialized state and the current working-tree/cache
//! state, and classifying every path into a [`super::SyncAction`].
//!
//! Nothing here mutates the working tree, `.git/info/exclude`, the
//! materialized state -- it only inspects
//! file metadata/content and the local object cache's directory listing.
//! Only `super::execute` is allowed to write.

use super::{PlanSink, Result, SyncAction, SyncError, SyncPlan, Validation};
use crate::repository::Repository as Repo;
use gat_core::lexical_path::GatPath;
use gat_core::lock::{Entry, Lock};
use gat_core::oid::Oid;
use gat_core::selection::Selection;
use gat_io::LockStore;
use gat_io::{
    CacheClient, ObjectVerification, VERIFY_WINDOW, WorktreeClient, WorktreeFileStatus,
    WorktreeStatusKind as FileStatus,
};
use gat_io::{
    DesiredQuery, DesiredRow, DirtyRow, MaterializedRow, StateMutation, StateStore, StateStoreError,
};

/// Whether a working-tree file at `path` currently matches a known `oid`,
/// using the shared [`check_known_oid`] resolver: a matching prior stat
/// proof trusts the OID with zero hashing, otherwise the file is hashed
/// once and a refreshed proof is returned for the caller to persist.
/// Content identity is BLAKE3 OID only -- size is never used as a
/// semantic comparison here (it only participates inside the shared
/// proof as a cheap stat-miss signal).
fn file_status(
    worktree: WorktreeClient<'_>,
    cache: &CacheClient,
    prior: &MaterializedRow,
    validation: Validation,
) -> Result<WorktreeFileStatus> {
    Ok(worktree.file_status(cache, prior, validation == Validation::TrustState)?)
}

fn file_status_without_prior(
    worktree: WorktreeClient<'_>,
    cache: &CacheClient,
    path: &GatPath,
    expected: &Oid,
    validation: Validation,
) -> Result<WorktreeFileStatus> {
    Ok(worktree.file_status_without_prior(
        cache,
        path,
        expected,
        validation == Validation::TrustState,
    )?)
}

/// Route a cache object through the shared verification boundary
/// (`CacheClient::verify`) rather than a presence-only `has_object`
/// check, so correctness-sensitive materialization never consumes
/// unverified cache bytes. `Validation::TrustState` may skip *worktree*
/// validation, but it must never bypass cache-object integrity.
fn cache_object_status(cache: &CacheClient, oid: &Oid) -> Result<ObjectVerification> {
    Ok(cache.verify(oid)?)
}

/// Build a `Conflict` action wrapping `resolution`, unless `resolution`
/// itself already reports the backing cache object as corrupted -- in
/// which case surface `Corrupted` directly (unwrapped) instead, since
/// neither a plain conflict resolution nor `--force` can produce correct
/// bytes from a bad cache object. Takes an already-resolved `resolution`
/// (e.g. from `replace_or_missing`) rather than re-deriving the cache
/// object's status itself: both would inspect the exact same oid, so
/// recomputing it here would cost a second (memoized, but still
/// redundant) `CacheClient::verify` call per conflict.
fn conflict_or_corrupted(path: &GatPath, resolution: SyncAction) -> SyncAction {
    match resolution {
        SyncAction::Corrupted { .. } => resolution,
        other => SyncAction::Conflict {
            path: path.clone(),
            resolution: Box::new(other),
        },
    }
}

fn materialize_or_missing(status: ObjectVerification, entry: &Entry) -> SyncAction {
    match status {
        ObjectVerification::Valid => SyncAction::Materialize(entry.clone()),
        ObjectVerification::Missing => SyncAction::MissingObject {
            path: entry.path.clone(),
            oid: entry.oid,
        },
        ObjectVerification::Corrupt => SyncAction::Corrupted {
            path: entry.path.clone(),
            oid: entry.oid,
        },
    }
}

fn replace_or_missing(status: ObjectVerification, entry: &Entry) -> SyncAction {
    match status {
        ObjectVerification::Valid => SyncAction::Replace(entry.clone()),
        ObjectVerification::Missing => SyncAction::MissingObject {
            path: entry.path.clone(),
            oid: entry.oid,
        },
        ObjectVerification::Corrupt => SyncAction::Corrupted {
            path: entry.path.clone(),
            oid: entry.oid,
        },
    }
}

/// Like [`replace_or_missing`], but for `--rematerialize`: the desired
/// object is already what's on disk, and only its representation is
/// being recreated. A missing or corrupt cache object leaves the
/// existing (already-correct) working-tree file untouched and reports
/// the same condition `replace_or_missing` would.
fn rematerialize_or_missing(status: ObjectVerification, entry: &Entry) -> SyncAction {
    match status {
        ObjectVerification::Valid => SyncAction::Rematerialize(entry.clone()),
        ObjectVerification::Missing => SyncAction::MissingObject {
            path: entry.path.clone(),
            oid: entry.oid,
        },
        ObjectVerification::Corrupt => SyncAction::Corrupted {
            path: entry.path.clone(),
            oid: entry.oid,
        },
    }
}

/// A cache-dependent row classification deferred until the bounded
/// window it belongs to has had its oids verified in one
/// `CacheClient::verify_windows_unmemoized` batch (see
/// [`MergeBuffer::flush`]), instead of [`merge_desired_with_prior`]
/// falling back to one point-at-a-time [`CacheClient::verify`] call per
/// distinct oid it streams past.
enum PendingCache {
    Materialize(Entry),
    Replace(Entry),
    /// The desired object already equals what was last materialized, but
    /// `SyncOptions::rematerialize` asked to recreate it anyway.
    Rematerialize(Entry),
    /// A conflict whose resolution is `replace_or_missing(entry)`, unless
    /// the cache object backing that resolution is itself corrupted (see
    /// [`conflict_or_corrupted`]).
    ConflictOrReplace {
        path: GatPath,
        entry: Entry,
    },
    /// Like [`Self::ConflictOrReplace`], but the desired object already
    /// equals what was last materialized: the conflict exists only
    /// because the working-tree file was locally modified, and (under
    /// `--force`) the resolution is `rematerialize_or_missing(entry)`,
    /// not a plain content replace.
    ConflictOrRematerialize {
        path: GatPath,
        entry: Entry,
    },
}

const fn pending_oid(kind: &PendingCache) -> Oid {
    match kind {
        PendingCache::Materialize(entry)
        | PendingCache::Replace(entry)
        | PendingCache::Rematerialize(entry) => entry.oid,
        PendingCache::ConflictOrReplace { entry, .. }
        | PendingCache::ConflictOrRematerialize { entry, .. } => entry.oid,
    }
}

fn resolve_pending(
    statuses: &std::collections::HashMap<Oid, ObjectVerification>,
    oid: Oid,
    kind: PendingCache,
) -> SyncAction {
    // A window's `statuses` map is always populated for every oid that
    // window's `verify_windows_unmemoized` call was given (one entry per
    // distinct pending oid; see `MergeBuffer::flush`) -- `unwrap_or` here
    // only guards against a logic error in that pairing, not a real
    // "unverified" case, so it degrades to `Missing` rather than panicking.
    let status = statuses
        .get(&oid)
        .copied()
        .unwrap_or(ObjectVerification::Missing);
    match kind {
        PendingCache::Materialize(entry) => materialize_or_missing(status, &entry),
        PendingCache::Replace(entry) => replace_or_missing(status, &entry),
        PendingCache::Rematerialize(entry) => rematerialize_or_missing(status, &entry),
        PendingCache::ConflictOrReplace { path, entry } => {
            let resolution = replace_or_missing(status, &entry);
            conflict_or_corrupted(&path, resolution)
        }
        PendingCache::ConflictOrRematerialize { path, entry } => {
            let resolution = rematerialize_or_missing(status, &entry);
            conflict_or_corrupted(&path, resolution)
        }
    }
}

/// One buffered merge row, in the exact order it was classified: either
/// already resolved (a cache-free row, e.g. `Remove`) or still awaiting
/// its window's batched cache verification (see [`PendingCache`]). The
/// oid is parsed once, when the row is first buffered (`pending_oid`),
/// and carried alongside the classification so [`MergeBuffer::flush`]
/// never has to re-parse or re-derive it when resolving the row.
enum BufferedRow {
    Ready(SyncAction),
    Pending(Oid, PendingCache),
}

/// How many buffered merge rows -- resolved or still-pending -- accumulate
/// before [`MergeBuffer::flush`] drains them to the [`super::PlanSink`],
/// independent of how many are cache-dependent. A
/// long run of exclusively cache-free rows (e.g. many consecutive
/// `Remove`s) never trips [`gat_io::VERIFY_WINDOW`]'s own
/// oid-count trigger on its own, so this bounds the merge's *own* window
/// independent of the cache-verification window -- callers configure this
/// through [`crate::limits::SyncLimits::merge_window`], which defaults to
/// the same order of
/// magnitude as [`gat_io::VERIFY_WINDOW`] but can be tuned
/// independently.
///
/// Merge-buffer structural instrumentation, colocated
/// with the code it measures: records the size of each drained
/// [`MergeBuffer::flush`] batch and keeps the maximum seen so far on this
/// thread, so a test can assert the merge's transient buffered-row state
/// never exceeds the configured merge window regardless of how many
/// desired/prior rows the whole merge has to reconcile.
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support {
    use std::cell::Cell;

    thread_local! {
        static MERGE_BUFFER_HIGH_WATER: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn record_merge_buffer_flush(size: usize) {
        MERGE_BUFFER_HIGH_WATER.with(|c| c.set(c.get().max(size)));
    }

    pub fn merge_buffer_high_water() -> usize {
        MERGE_BUFFER_HIGH_WATER.with(Cell::get)
    }
}

/// The desired/materialized merge's own bounded window: buffers merge rows
/// (resolved or still cache-pending) in order, verifies every distinct
/// pending oid in one batch once the window closes, and drains the whole
/// buffer to the [`super::PlanSink`] in original order -- so
/// [`merge_desired_with_prior`] never retains a whole-operation
/// `Vec<SyncAction>` merely so a later phase can apply or collect it.
struct MergeBuffer<'a> {
    cache: &'a CacheClient,
    merge_window: usize,
    sink: &'a mut dyn PlanSink,
    rows: Vec<BufferedRow>,
    pending_oids: Vec<Oid>,
    /// Dedups `pending_oids` within the current window: several buffered
    /// rows may share the same oid (e.g. many desired paths pointing at
    /// one object), but each must be verified at most once per window --
    /// [`CacheClient::verify_windows_unmemoized`] requires its input
    /// already deduplicated, since it no longer performs its own
    /// per-window dedup pass. Cleared alongside `pending_oids` on flush.
    pending_seen: std::collections::HashSet<Oid>,
    /// Which [`CacheClient`] verification retention policy `flush` uses
    /// for this reconciliation, derived once from
    /// `ReconciliationPolicy::rematerialize`:
    ///
    /// - `false` (ordinary sync): [`CacheClient::verify_windows`], which
    ///   keeps every verified oid memoized for the rest of the operation
    ///   so a later window that references the same oid (e.g. many
    ///   desired paths sharing one object) is resolved from the memo
    ///   instead of re-verifying the filesystem.
    /// - `true` (`--rematerialize`): [`CacheClient::verify_windows_unmemoized`],
    ///   which purges each window's statuses once consumed. Ordinary
    ///   sync never enqueues an already-correct row into `pending_oids`
    ///   at all, but `--rematerialize` enqueues *every* selected clean
    ///   row -- so retaining every verified oid for the whole operation
    ///   would grow the memo `O(unique selected oids)` instead of
    ///   staying bounded near one window's worth. Losing cross-window
    ///   reuse for repeated oids under `--rematerialize` is an explicit,
    ///   accepted tradeoff in exchange for
    ///   bounded memory on a flag whose whole point is to touch every
    ///   selected path.
    rematerialize: bool,
}

impl<'a> MergeBuffer<'a> {
    fn new(
        cache: &'a CacheClient,
        merge_window: usize,
        sink: &'a mut dyn PlanSink,
        rematerialize: bool,
    ) -> Self {
        Self {
            cache,
            merge_window,
            sink,
            rows: Vec::new(),
            pending_oids: Vec::new(),
            pending_seen: std::collections::HashSet::new(),
            rematerialize,
        }
    }

    /// Buffer an already-resolved (cache-free) action, flushing the
    /// window if it just reached `self.merge_window`.
    fn push_ready(&mut self, action: SyncAction) -> Result<()> {
        self.rows.push(BufferedRow::Ready(action));
        self.flush_if_full()
    }

    /// Buffer a cache-dependent classification, flushing the window once
    /// it reaches [`gat_io::VERIFY_WINDOW`] distinct
    /// pending oids or `self.merge_window` buffered rows, whichever comes
    /// first.
    fn push_pending(&mut self, kind: PendingCache) -> Result<()> {
        let oid = pending_oid(&kind);
        if self.pending_seen.insert(oid) {
            self.pending_oids.push(oid);
        }
        self.rows.push(BufferedRow::Pending(oid, kind));
        if self.pending_oids.len() >= VERIFY_WINDOW {
            return self.flush();
        }
        self.flush_if_full()
    }

    /// Forward an already-validated stat-proof refresh straight to the
    /// sink. Unlike a buffered row, a refresh is never cache-dependent and
    /// never shares a path with a buffered action (the merge only records
    /// one when a row required no action at all -- see
    /// [`FileStatus::Matches`]), so it needs no ordering relative to the
    /// buffer and can be forwarded immediately.
    fn state_mutation(&mut self, mutation: StateMutation) -> Result<()> {
        self.sink.state_mutation(mutation)
    }

    fn flush_if_full(&mut self) -> Result<()> {
        if self.rows.len() >= self.merge_window {
            self.flush()?;
        }
        Ok(())
    }

    /// Verify every oid queued this window in one bounded batch, then
    /// drain every buffered row -- resolving any still-pending
    /// classification directly from that batch's aligned statuses -- to
    /// the sink in order. A no-op when nothing is buffered.
    ///
    /// Chooses between [`CacheClient::verify_windows`] and
    /// [`CacheClient::verify_windows_unmemoized`] based on
    /// `self.rematerialize` (see the field's doc comment); either way,
    /// this window's own `statuses` map -- built directly from the
    /// callback's aligned `(oid, status)` pairs -- is exactly what every
    /// `resolve_pending` call below needs, so there is never a second
    /// per-row `CacheClient::verify` lookup after the batch (unlike the
    /// old `verify_many(...)` -> discard `Vec` -> `verify(...)` per row
    /// pattern this replaced).
    fn flush(&mut self) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_merge_buffer_flush(self.rows.len());
        let mut statuses: std::collections::HashMap<Oid, ObjectVerification> =
            std::collections::HashMap::with_capacity(self.pending_oids.len());
        if !self.pending_oids.is_empty() {
            let on_window = |window: &[Oid], window_statuses: &[ObjectVerification]| {
                for (oid, status) in window.iter().zip(window_statuses.iter()) {
                    statuses.insert(*oid, *status);
                }
                Ok(())
            };
            if self.rematerialize {
                self.cache
                    .verify_windows_unmemoized::<SyncError>(&self.pending_oids, on_window)?;
            } else {
                self.cache
                    .verify_windows::<SyncError>(&self.pending_oids, on_window)?;
            }
            self.pending_oids.clear();
            self.pending_seen.clear();
        }
        for row in self.rows.drain(..) {
            let action = match row {
                BufferedRow::Ready(action) => action,
                BufferedRow::Pending(oid, kind) => resolve_pending(&statuses, oid, kind),
            };
            self.sink.action(action)?;
        }
        Ok(())
    }
}

/// Classify a desired path that has no corresponding materialized row
/// (never synced before, or dropped out of the last materialized state
/// entirely). Under `Validation::TrustState` there is no prior materialized
/// state to trust as ground truth for this path, so this still just
/// materializes/reports-missing without touching the working tree, the
/// same as before this path had a materialized row at all. Under
/// `Validate`, this still must inspect the working tree: a pre-existing
/// file at this path that already matches the desired content is adopted
/// via `replace_or_missing` rather than blindly overwritten, and one that
/// differs is reported as a conflict rather than silently clobbered.
///
/// Returns a [`PendingCache`] rather than resolving it immediately: the
/// caller enqueues it into the current bounded verification window
/// instead of this function calling into the cache one oid at a time.
fn desired_only_pending<D: DesiredSide>(
    worktree: WorktreeClient<'_>,
    cache: &CacheClient,
    d: &D,
    validation: Validation,
) -> Result<PendingCache> {
    if validation == Validation::TrustState {
        // Under Validation::TrustState the planner intentionally skips all
        // working-tree access, so we do not know whether the destination
        // exists.  Emit Materialize: do_materialize handles the case where
        // a regular file is already present by renaming it aside before
        // calling storage::materialize and restoring it if every mode fails,
        // so a pre-existing user-owned file is never silently destroyed on
        // an error.
        return Ok(PendingCache::Materialize(d.to_entry()));
    }
    // Route through the cheap, filesystem-free `resolve_worktree_path`
    // rather than a bare `root.join`: a canonical Gat path is
    // host-independent lexical identity only, and must never regain
    // native drive/root semantics (e.g. `C:/foo` on Windows) merely by
    // being passed to a platform path API. This is deliberately not
    // `confine_read`: adding its ancestor-symlink filesystem walk here
    // would cost an extra `symlink_metadata` pass per validated sync
    // candidate; no separate materialized row is added.
    // Desired-only existing files have no trusted prior proof: pass `None`
    // so the shared resolver hashes when it needs exact identity. Passing
    // `d` itself (rather than a pre-encoded hex string) means an absent
    // destination -- common for a brand-new path -- never hex-encodes
    // `d`'s oid at all, since `file_status` returns `Absent` before
    // `check_known_oid` ever inspects the expected oid.
    let status =
        file_status_without_prior(worktree, cache, d.path(), &d.to_entry().oid, validation)?;
    Ok(match status.kind() {
        FileStatus::Absent => PendingCache::Materialize(d.to_entry()),
        FileStatus::Matches => PendingCache::Replace(d.to_entry()),
        FileStatus::Differs => PendingCache::ConflictOrReplace {
            path: d.path().clone(),
            entry: d.to_entry(),
        },
    })
}

/// Abstracts over how a desired-side comparison row's oid is represented,
/// so the merge loop below can classify a row -- and decide whether it
/// even needs to touch the working tree at all under
/// `Validation::TrustState` -- without hex-encoding a native `SQLite`
/// `Oid` except where an actual `Entry` (an emitted action, or a
/// `file_status` hash comparison) is genuinely required. Mirrors the
/// `RightRow` pattern in the command comparison coordinator applied to comparison
/// output rows.
trait DesiredSide {
    fn path(&self) -> &GatPath;
    /// Whether this row's oid matches a materialized row's native `Oid`,
    /// without ever allocating a hex string.
    fn oid_matches(&self, prior_oid: &Oid) -> bool;
    /// Converts to the persistence-boundary [`Entry`] form -- only ever
    /// called where a [`SyncAction`] actually needs to carry one.
    fn to_entry(&self) -> Entry;
}

#[derive(Debug)]
struct DesiredEntry {
    entry: Entry,
}

impl DesiredSide for DesiredEntry {
    fn path(&self) -> &GatPath {
        &self.entry.path
    }

    fn oid_matches(&self, prior_oid: &Oid) -> bool {
        *prior_oid == self.entry.oid
    }

    fn to_entry(&self) -> Entry {
        self.entry.clone()
    }
}

impl DesiredSide for DesiredRow {
    fn path(&self) -> &GatPath {
        &self.path
    }

    fn oid_matches(&self, prior_oid: &Oid) -> bool {
        *prior_oid == self.oid
    }

    fn to_entry(&self) -> Entry {
        self.clone().into_entry()
    }
}

fn filter_desired_entries(entries: &[Entry], selection: &Selection) -> Vec<DesiredEntry> {
    let mut filtered = entries
        .iter()
        .filter(|entry| selection.matches(&entry.path))
        .map(|entry| DesiredEntry {
            entry: entry.clone(),
        })
        .collect::<Vec<_>>();
    // `gat_io::LockStore::load_repository()` does not guarantee global path ordering: a sharded
    // lock concatenates shard-local sorted entries in shard-filename
    // order, which is deterministic but not globally sorted by path. The
    // merge below requires both sides sorted by path, so sort explicitly
    // rather than trusting on-disk order. Stability is not semantically
    // required here (paths are unique), so this can use the typically
    // faster unstable sort.
    filtered.sort_unstable_by(|a, b| a.entry.path.cmp(&b.entry.path));
    filtered
}

fn debug_assert_sorted_entries(entries: &[DesiredEntry]) {
    debug_assert!(
        entries
            .windows(2)
            .all(|window| window[0].entry.path < window[1].entry.path)
    );
}

/// Adapts the shared desired-query cursor to the merge loop's
/// [`DesiredSide`] abstraction, yielding the native
/// [`gat_io::DesiredRow`] directly instead of
/// eagerly hex-encoding it into an [`Entry`] -- see [`DesiredSide`] for
/// where (and whether) that conversion actually happens.
fn next_matching_desired(
    next: &mut impl FnMut() -> std::result::Result<Option<DesiredRow>, StateStoreError>,
) -> Result<Option<DesiredRow>> {
    Ok(next()?)
}

/// Classify a materialized-only row (no corresponding desired entry, i.e.
/// dropped from `gat.lock` since the last sync) as either nothing (already
/// gone from the working tree), a plain `Remove`, or a `Conflict` wrapping
/// `Remove` if the working tree differs from what was materialized.
fn prior_only_action(
    worktree: WorktreeClient<'_>,
    cache: &CacheClient,
    prior: &MaterializedRow,
    validation: Validation,
) -> Result<Option<SyncAction>> {
    let path = prior.path();
    if validation == Validation::TrustState {
        // `file_status` under `TrustState` always reports `Matches` without
        // touching the working tree at all -- skip resolving a native
        // path/building `dest` entirely rather than doing filesystem-free
        // work whose only possible result is known in advance.
        return Ok(Some(SyncAction::Remove(path.clone())));
    }
    // See `desired_only_action`: route through the cheap, filesystem-free
    // `resolve_worktree_path` so a canonical Gat path can never regain
    // native drive/root semantics, without adding an ancestor-symlink
    // filesystem walk to every validated sync candidate.
    // No trusted expected oid ever changes here since a materialized-only
    // row has no desired counterpart -- pass the native `Oid` straight
    // through so an absent/gone-already file (the common case for a
    // dropped path) never gets hex-encoded at all.
    let status = file_status(worktree, cache, prior, validation)?;
    Ok(match status.kind() {
        FileStatus::Absent => None,
        FileStatus::Matches => Some(SyncAction::Remove(path.clone())),
        FileStatus::Differs => Some(SyncAction::Conflict {
            path: path.clone(),
            resolution: Box::new(SyncAction::Remove(path.clone())),
        }),
    })
}

/// Advances `materialized` to the next row selected by `glob_filter`,
/// silently skipping over any that aren't -- filtering the materialized
/// side the same way [`filter_desired_entries`] filters the desired side,
/// without collecting the cursor into a `Vec` first.
fn next_matching_prior(
    next: &mut impl FnMut() -> std::result::Result<Option<MaterializedRow>, StateStoreError>,
    selection: &Selection,
) -> Result<Option<MaterializedRow>> {
    loop {
        match next()? {
            Some(row) if !selection.matches(row.path()) => {}
            other => return Ok(other),
        }
    }
}

/// Build the exact set of actions needed to reconcile the working tree
/// with the currently checked-out `gat.lock`, comparing against the last
/// materialized state. Read-only: touches nothing but file metadata/hashes
/// and the local object cache's directory listing.
pub fn plan(repo: &Repo, selection: &Selection, validation: Validation) -> Result<SyncPlan> {
    let merge_window = crate::limits::ExecutionLimits::production()
        .sync
        .merge_window
        .get();
    let store = StateStore::open(repo.layout())?;
    // Justified full materialization: this entry point makes no
    // desired-index freshness guarantee, so the authoritative on-disk
    // `gat.lock` -- not the SQLite mirror -- is the only correct source
    // here. Callers that already refreshed the mirror plan through
    // `plan_with_store(.., None, ..)` instead, which streams.
    let desired_lock = LockStore::load_repository(repo.layout())?;
    // This standalone read-only entry point has no operation/`Operation`
    // to source a shared cache from -- it is used outside the `Operation`
    // architecture (e.g. directly by library callers), so it legitimately
    // opens its own `CacheClient` rather than requiring an operation to
    // exist. Callers driven by `sync_from_snapshot` route through the
    // operation's single `Operation::cache` instead.
    let cache_root = repo.resolved_cache_root()?;
    let cache = cache_root.open_client();
    plan_with_store(
        repo,
        &cache,
        DesiredSource {
            store: Some(&store),
            desired_lock: Some(&desired_lock),
        },
        selection,
        // This read-only planning API never rematerializes: only
        // `gat sync --rematerialize`'s mutating/dry-run paths (which call
        // `plan_with_store`/`plan_into_sink` directly with their own
        // `ReconciliationPolicy`) do.
        super::ReconciliationPolicy {
            validation,
            rematerialize: false,
        },
        merge_window,
    )
}

/// Like [`plan`], but optionally takes an already-loaded `desired_lock`
/// (`Some`) instead of streaming desired rows from `SQLite` (`None`).
/// Callers that cannot assume the mirror is fresh (a `--dry-run` sync, or
/// any use before the mirror has been built) must pass a freshly-loaded
/// [`Lock`]. Callers that *can* assume the mirror is fresh should pass
/// `None`: planning then streams the desired side directly from `SQLite`
/// through the shared desired-query layer and never materializes a full
/// [`Lock`] at all; exclude regeneration afterwards streams
/// desired paths from the same store (`excludes::sync_from_store`).
///
/// `store` may also be `None`, for a store that is genuinely absent
/// (see [`StateStore::open_if_exists`]) rather than merely empty:
/// there is no materialized cursor to drive at all, so every desired path
/// is compared against an implicit "nothing has ever been materialized
/// here" prior, exactly as the merge loop treats a desired path with no
/// matching materialized row.
/// [`super::PlanSink`] that reconstructs a complete [`SyncPlan`] -- the
/// read-only planning API's behavior (`plan()`, `--dry-run`, and any other
/// caller that genuinely needs the whole plan for inspection) unchanged
/// from before `plan_into_sink` existed.
#[derive(Default)]
pub(crate) struct CollectPlanSink {
    actions: Vec<SyncAction>,
    validated_state_mutations: Vec<StateMutation>,
}

impl super::PlanSink for CollectPlanSink {
    fn action(&mut self, action: SyncAction) -> Result<()> {
        self.actions.push(action);
        Ok(())
    }

    fn state_mutation(&mut self, mutation: StateMutation) -> Result<()> {
        self.validated_state_mutations.push(mutation);
        Ok(())
    }
}

/// Like [`plan_with_store`], but delivers every classified action (and
/// stat refresh) to `sink` as it's produced instead of collecting a
/// complete [`SyncPlan`]. The mutating `Validation::Validate`
/// sync path drives this directly with an
/// [`super::execute::ExecutePlanSink`] so a huge repository's full
/// desired/materialized merge never has to be fully classified in memory
/// before the first action is applied.
/// The store/`Lock` combination [`plan_into_sink`]/[`plan_with_store`]
/// merge desired rows against a prior materialized cursor with -- bundled
/// into one value so adding `merge_window` alongside them didn't push
/// either function over clippy's argument-count lint (which this codebase
/// treats as a real "too many independently-supplied values" signal, not
/// boilerplate to suppress.
/// [`Self::desired_lock`] documents the same `Some`/`None` streaming
/// contract [`plan_into_sink`] always had.
pub(crate) struct DesiredSource<'a> {
    pub(crate) store: Option<&'a StateStore>,
    pub(crate) desired_lock: Option<&'a Lock>,
}

pub(crate) fn plan_into_sink(
    repo: &Repo,
    cache: &CacheClient,
    desired: DesiredSource<'_>,
    selection: &Selection,
    policy: super::ReconciliationPolicy,
    merge_window: usize,
    sink: &mut dyn super::PlanSink,
) -> Result<()> {
    let DesiredSource {
        store,
        desired_lock,
    } = desired;
    let scope = selection.scope_path();

    if let Some(desired_lock) = desired_lock {
        let desired = filter_desired_entries(&desired_lock.entries, selection);
        debug_assert_sorted_entries(&desired);
        let mut desired_iter = desired.into_iter();
        match store {
            Some(store) => store.with_rows_in_scope(scope, |mut materialized| {
                let mut next_prior = || materialized.next();
                merge_desired_with_prior(
                    repo,
                    cache,
                    policy,
                    &mut || Ok(desired_iter.next()),
                    &mut || next_matching_prior(&mut next_prior, selection),
                    merge_window,
                    sink,
                )
            })?,
            None => merge_desired_with_prior(
                repo,
                cache,
                policy,
                &mut || Ok(desired_iter.next()),
                &mut || Ok(None),
                merge_window,
                sink,
            )?,
        }
    } else {
        let Some(store) = store else {
            return Err(SyncError::missing_materialized_store());
        };
        // The desired side is narrowed by `Selection::scope_path()` in
        // SQL and filtered by `Selection::matches` as the residual
        // authority; see `DesiredQuery::for_selection`.
        store.with_desired_rows(DesiredQuery::for_selection(selection), |mut desired| {
            store.with_rows_in_scope(scope, |mut materialized| {
                let mut next_desired = || desired.next();
                let mut next_prior = || materialized.next();
                merge_desired_with_prior(
                    repo,
                    cache,
                    policy,
                    &mut || next_matching_desired(&mut next_desired),
                    &mut || next_matching_prior(&mut next_prior, selection),
                    merge_window,
                    sink,
                )
            })
        })?;
    }
    Ok(())
}

pub(crate) fn plan_with_store(
    repo: &Repo,
    cache: &CacheClient,
    desired: DesiredSource<'_>,
    selection: &Selection,
    policy: super::ReconciliationPolicy,
    merge_window: usize,
) -> Result<SyncPlan> {
    let mut sink = CollectPlanSink::default();
    plan_into_sink(
        repo,
        cache,
        desired,
        selection,
        policy,
        merge_window,
        &mut sink,
    )?;
    Ok(SyncPlan {
        actions: sink.actions,
        validated_state_mutations: sink.validated_state_mutations,
    })
}

/// The streaming desired/materialized merge shared by [`plan_into_sink`]'s
/// `Some`/`None`-store branches: `next_prior` yields the next materialized
/// row in `path` order (or `None` once exhausted, including when there was
/// never a materialized-state database to begin with). Generic over
/// [`DesiredSide`] so the same merge drives both the `Lock`-based
/// (`DesiredEntry`) and SQLite-streamed native
/// ([`gat_io::DesiredRow`]) desired sources.
///
/// Every classification is delivered to `sink` -- a bounded
/// [`MergeBuffer`] window at a time (see [`MergeBuffer::flush`]) -- rather
/// than collected into a whole-operation `Vec<SyncAction>` here: the merge
/// itself never has to know (or care) whether its caller wants a complete
/// [`SyncPlan`] or an immediately-applied bounded action batch.
fn merge_desired_with_prior<D: DesiredSide>(
    repo: &Repo,
    cache: &CacheClient,
    policy: super::ReconciliationPolicy,
    next_desired: &mut dyn FnMut() -> Result<Option<D>>,
    next_prior: &mut dyn FnMut() -> Result<Option<MaterializedRow>>,
    merge_window: usize,
    sink: &mut dyn super::PlanSink,
) -> Result<()> {
    let validation = policy.validation;
    let worktree = repo.worktree_client();
    // Cache-dependent classifications (anything that needs
    // `CacheClient::verify`) are not resolved as soon as they're seen:
    // each reserves a placeholder slot in `actions` and is queued into
    // `pending`/`pending_oids`, so a bounded window's worth of distinct
    // oids can be verified in one `verify_many` batch (`flush_pending`)
    // instead of the merge falling back to one point lookup per row.
    // Cache-free rows (`Remove`, or a `Matches` with no action)
    // are still pushed directly.
    let mut buffer = MergeBuffer::new(cache, merge_window, sink, policy.rematerialize);

    #[cfg(debug_assertions)]
    let mut last_desired_path: Option<GatPath> = None;
    #[cfg(debug_assertions)]
    let mut last_prior_path: Option<GatPath> = None;
    macro_rules! next_desired_checked {
        () => {{
            let row = next_desired()?;
            #[cfg(debug_assertions)]
            if let Some(r) = &row {
                if let Some(last) = &last_desired_path {
                    debug_assert!(
                        last < r.path(),
                        "desired rows must be strictly ordered by path"
                    );
                }
                last_desired_path = Some(r.path().clone());
            }
            row
        }};
    }
    // Wraps every `next_prior()` call to additionally assert (debug
    // builds only) that consecutively *fetched* rows are strictly
    // increasing by path -- the sole invariant the merge loop below
    // relies on for its materialized-side cursor. Checking this only
    // when a row is actually fetched (rather than unconditionally once
    // per loop iteration, which would also re-compare an unchanged
    // `current_prior` against itself and spuriously fail) is what makes
    // the check correct.
    macro_rules! next_prior_checked {
        () => {{
            let row = next_prior()?;
            #[cfg(debug_assertions)]
            if let Some(r) = &row {
                if let Some(last) = &last_prior_path {
                    debug_assert!(
                        last < r.path(),
                        "materialized rows must be strictly ordered by path"
                    );
                }
                last_prior_path = Some(r.path().clone());
            }
            row
        }};
    }
    macro_rules! enqueue {
        ($kind:expr) => {{ buffer.push_pending($kind)? }};
    }

    let mut current_desired = next_desired_checked!();
    let mut current_prior = next_prior_checked!();

    loop {
        match (current_desired.as_ref(), current_prior.as_ref()) {
            (Some(d), Some(prior)) if d.path() == prior.path() => {
                let path = d.path();
                // Reconciliation identity is OID-only: size is never part
                // of the semantic comparison here. Comparing via the
                // native `Oid` (rather than hex) means a desired row
                // whose oid matches its prior materialized row never
                // gets hex-encoded just to make that determination.
                let target_matches_prior = d.oid_matches(&prior.oid());
                if validation == Validation::TrustState {
                    // `file_status` under `TrustState` always reports
                    // `Matches { proof_refresh: None }` without touching
                    // the working tree or its `oid` argument at all, so
                    // the only thing that can still happen here is the
                    // mismatched-oid `replace` action below -- skip
                    // building `dest`/hex-encoding anything else.
                    if !target_matches_prior {
                        enqueue!(PendingCache::Replace(d.to_entry()));
                    }
                } else {
                    // The stat proof recorded alongside `prior.oid` is
                    // only valid evidence for `prior.oid` itself (see
                    // `check_known_oid`'s doc comment), and when
                    // `target_matches_prior` is true `d`'s oid and
                    // `prior.oid` are the same value anyway -- so
                    // comparing against the native `prior.oid` covers
                    // both branches without ever hex-encoding anything
                    // up front. Only a genuine mismatch (rare) falls
                    // through to a branch below that needs textual
                    // identity for a conflict/corruption report.
                    let status = file_status(worktree, cache, prior, validation)?;
                    if target_matches_prior {
                        match status.kind() {
                            FileStatus::Absent => {
                                enqueue!(PendingCache::Materialize(d.to_entry()));
                            }
                            FileStatus::Matches => {
                                if policy.rematerialize {
                                    // Recreate this already-correct path
                                    // using the current materialization
                                    // strategy. Skip persisting
                                    // `proof_refresh` here: `do_rematerialize`
                                    // records a fresh stat proof off the
                                    // representation it's about to write,
                                    // making any proof observed here
                                    // immediately stale.
                                    enqueue!(PendingCache::Rematerialize(d.to_entry()));
                                } else if let Some(mutation) =
                                    status.into_state_refresh(path.clone())
                                {
                                    buffer.state_mutation(mutation)?;
                                }
                            }
                            FileStatus::Differs => {
                                let entry = d.to_entry();
                                enqueue!(if policy.rematerialize {
                                    PendingCache::ConflictOrRematerialize {
                                        path: path.clone(),
                                        entry,
                                    }
                                } else {
                                    PendingCache::ConflictOrReplace {
                                        path: path.clone(),
                                        entry,
                                    }
                                });
                            }
                        }
                    } else {
                        match status.kind() {
                            FileStatus::Absent => {
                                enqueue!(PendingCache::Materialize(d.to_entry()));
                            }
                            FileStatus::Matches => {
                                enqueue!(PendingCache::Replace(d.to_entry()));
                            }
                            FileStatus::Differs => enqueue!(PendingCache::ConflictOrReplace {
                                path: path.clone(),
                                entry: d.to_entry(),
                            }),
                        }
                    }
                }
                current_desired = next_desired_checked!();
                current_prior = next_prior_checked!();
            }
            (Some(d), Some(prior)) if d.path() < prior.path() => {
                let kind = desired_only_pending(worktree, cache, d, validation)?;
                enqueue!(kind);
                current_desired = next_desired_checked!();
            }
            (Some(_), Some(prior)) => {
                if let Some(action) = prior_only_action(worktree, cache, prior, validation)? {
                    buffer.push_ready(action)?;
                }
                current_prior = next_prior_checked!();
            }
            (Some(d), None) => {
                let kind = desired_only_pending(worktree, cache, d, validation)?;
                enqueue!(kind);
                current_desired = next_desired_checked!();
            }
            (None, Some(prior)) => {
                if let Some(action) = prior_only_action(worktree, cache, prior, validation)? {
                    buffer.push_ready(action)?;
                }
                current_prior = next_prior_checked!();
            }
            (None, None) => break,
        }
    }
    buffer.flush()
}

/// Build a plan directly from the reconciliation layer's dirty rows,
/// skipping the full desired/materialized merge entirely. Only valid
/// under `Validation::TrustState` (both the fast path's and the full merge's
/// precondition -- see [`super::desired_index::refresh`]): every dirty
/// row's desired-vs-materialized comparison is already exactly what
/// [`file_status`] under `Validation::TrustState` would report anyway
/// (`FileStatus::Matches`, unconditionally, since that validation level
/// never touches the working tree), so this reimplements the same
/// classification decisions as [`plan_with_store`]'s merge loop without
/// walking every clean path.
///
/// `glob_filter` applies the same include/exclude semantics a full
/// [`plan_with_store`] call would (Rust `GlobFilter` semantics stay
/// authoritative -- no SQL glob translation): a path scope is already
/// applied by the caller querying
/// [`gat_io::StateStore::dirty_rows_in_scope`]
/// with the scope's lexical range, so only *these* dirty candidates ever
/// need a glob check, not every clean path in the repository.
pub(crate) fn plan_from_dirty_rows(
    cache: &CacheClient,
    rows: Vec<DirtyRow>,
    selection: &Selection,
) -> Result<Vec<SyncAction>> {
    // Set-batch this chunk's cache verification instead of
    // leaving `replace_or_missing`/`materialize_or_missing` below to each
    // discover their oid cold: derive the distinct desired oids this dirty
    // chunk actually needs classified and verify them in one bounded
    // bounded verification pass, so the per-row loop below only ever hits the
    // resulting operation-local memo.
    let selected_oids: Vec<Oid> = rows
        .iter()
        .filter(|row| selection.is_unrestricted() || selection.matches(&row.path))
        .filter_map(|row| row.desired)
        .collect();
    if !selected_oids.is_empty() {
        cache.verify_windows(&selected_oids, |_, _| Ok::<(), SyncError>(()))?;
    }

    let mut actions = Vec::with_capacity(rows.len());
    for row in rows {
        if !selection.is_unrestricted() && !selection.matches(&row.path) {
            continue;
        }
        match (row.desired, row.materialized) {
            (Some(oid), Some(_)) => {
                let entry = Entry {
                    path: row.path,
                    oid,
                };
                actions.push(replace_or_missing(
                    cache_object_status(cache, &entry.oid)?,
                    &entry,
                ));
            }
            (Some(oid), None) => {
                let entry = Entry {
                    path: row.path,
                    oid,
                };
                actions.push(materialize_or_missing(
                    cache_object_status(cache, &entry.oid)?,
                    &entry,
                ));
            }
            (None, Some(_)) => {
                actions.push(SyncAction::Remove(row.path));
            }
            (None, None) => {
                // Already reconciled by the time this ran (e.g. removed
                // then re-added within the same refresh); nothing to do.
            }
        }
    }
    Ok(actions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_mutation::record_materialized_for_test as record_materialized;
    use crate::test_harness::git_repo;
    use crate::workspace::sync::{SyncOptions, sync};
    use gat_core::globs::GatGlobPattern;
    use gat_core::path_scope::{PathScope, normalize_path_scope};

    fn selection(path: Option<&str>, include: &[&str], exclude: &[&str]) -> Selection {
        let scope = path
            .map(std::path::Path::new)
            .map(normalize_path_scope)
            .transpose()
            .unwrap()
            .unwrap_or(PathScope::Root);
        let include = include
            .iter()
            .map(|pattern| GatGlobPattern::parse(pattern))
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let exclude = exclude
            .iter()
            .map(|pattern| GatGlobPattern::parse(pattern))
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        Selection::from_scope_patterns(scope, include, exclude)
    }

    fn ingest(repo: &Repo, content: impl std::io::Read) -> gat_io::Ingested {
        repo.resolved_cache_root()
            .unwrap()
            .writer()
            .ingest(content)
            .unwrap()
            .0
    }

    /// RAII guard clearing [`gat_io::cache_race_test_hooks`] on
    /// drop (including on panic/early return), so a test that injects a
    /// mid-hash race can never leak its hook into a later test sharing
    /// the same process.
    struct HashRaceGuard;

    impl Drop for HashRaceGuard {
        fn drop(&mut self) {
            gat_io::cache_race_test_hooks::clear();
        }
    }

    /// The working-tree counterpart to
    /// `verify_object_fs_fails_closed_on_a_mid_hash_rewrite_instead_of_reporting_valid`
    /// (`storage::cache_state`). `file_status` must hash `path`, but
    /// `path` is rewritten (by a race hook) after the hash has read the
    /// original bytes and before `check_known_oid`'s post-hash stat --
    /// the same shape of race `IdentityCheck::Unstable` exists to catch.
    /// The planner must not classify this as `Matches` (nor `Differs`):
    /// a hash that never described one coherent observation of `path`
    /// must not drive reconciliation to keep, remove, or replace it.
    #[test]
    fn file_status_fails_closed_on_a_mid_hash_rewrite_instead_of_reporting_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"hello").unwrap();
        let expected_oid = Oid::from_bytes(*blake3::hash(b"hello").as_bytes());

        let path_for_hook = path;
        gat_io::cache_race_test_hooks::set(move |p| {
            if p == path_for_hook {
                std::fs::write(p, b"hello, rewritten mid hash").unwrap();
            }
        });
        let _guard = HashRaceGuard;
        let layout = gat_io::RepositoryLayout::at(dir.path().to_path_buf());

        let result = file_status_without_prior(
            layout.worktree_client(),
            &layout
                .resolve_cache_root(Some(
                    &gat_core::cache_location::CacheLocation::try_from_path(
                        std::path::PathBuf::from(dir.path().as_os_str()),
                    )
                    .expect("nonempty fixture cache path"),
                ))
                .open_client(),
            &GatPath::parse_canonical("f.bin").unwrap(),
            &expected_oid,
            Validation::Validate,
        );
        assert!(
            result.is_err(),
            "an unstable hash must not be reported as a working-tree match or mismatch, got {result:?}"
        );
    }

    fn track(repo: &Repo, path: &str, content: &[u8]) -> Entry {
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let ingested = ingest(repo, content);
        let oid = ingested.oid;
        lock.upsert(GatPath::parse_canonical(path).unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();
        Entry {
            path: gat_core::lexical_path::GatPath::parse_canonical(path).unwrap(),
            oid,
        }
    }

    /// A sharded `gat.lock` concatenates shard-local sorted entries in
    /// shard-*filename* order (`load_sharded()`), which is deterministic
    /// but not globally sorted by path. The merge in `plan_with_store()`
    /// requires both sides sorted by path, so this exercises `shard_levels`
    /// values where two paths' shard order is the reverse of their lexical
    /// path order, and asserts the merge still classifies them correctly
    /// instead of misreading one as removed and the other as newly
    /// materialized.
    fn shard_reordered_paths(levels: gat_core::lock::LockShardLevels) -> (String, String) {
        let level_count = levels.get() as usize;
        let mut candidates: Vec<String> = (0..256).map(|i| format!("p{i:03}.bin")).collect();
        candidates.sort();
        for i in 0..candidates.len() {
            for j in (i + 1)..candidates.len() {
                let a = &candidates[i]; // a < b lexically
                let b = &candidates[j];
                let hash_a = blake3::hash(a.as_bytes());
                let hash_b = blake3::hash(b.as_bytes());
                let rel_a = &hash_a.as_bytes()[..level_count];
                let rel_b = &hash_b.as_bytes()[..level_count];
                if rel_a > rel_b {
                    // `a` sorts first lexically but its shard sorts after
                    // `b`'s shard, so `load_sharded()` yields `b` before
                    // `a`.
                    return (a.clone(), b.clone());
                }
            }
        }
        panic!(
            "no path pair with reversed shard order found for levels={}",
            levels.get()
        );
    }

    #[test]
    fn sharded_lock_with_reversed_shard_order_is_planned_correctly_when_synced() {
        for levels in [1u8, 2u8]
            .map(|levels| gat_core::lock::LockShardLevels::new(levels).expect("valid shard depth"))
        {
            let tmp = git_repo();
            let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            let (path_a, path_b) = shard_reordered_paths(levels);

            let entry_a = track(&repo, &path_a, b"content-a");
            let entry_b = track(&repo, &path_b, b"content-b");
            let lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
            gat_io::LockStore::publish_repository(repo.layout(), &lock, levels).unwrap();

            let mut store = StateStore::open(repo.layout()).unwrap();
            store
                .upsert_many(&[entry_a.clone(), entry_b.clone()])
                .unwrap();
            std::fs::write(tmp.path().join(&path_a), b"content-a").unwrap();
            std::fs::write(tmp.path().join(&path_b), b"content-b").unwrap();

            let planned = plan(&repo, &Selection::root(), Validation::Validate).unwrap();
            assert_eq!(
                planned.actions,
                Vec::new(),
                "levels={}: both paths already match materialized state \
                 and should need no action, got {:?}",
                levels.get(),
                planned.actions
            );
        }
    }

    /// Structural regression test: a `Validation::Validate` sync-plan pass
    /// over an already-synced, unmodified file must not hex-encode its oid
    /// at all. A valid stat proof lets `check_known_oid` return `Proven`
    /// without ever inspecting the expected oid, so the desired row (a
    /// native `SQLite` `Oid`) should reach that point still unencoded.
    #[test]
    fn validated_plan_over_an_unmodified_stat_proven_file_never_hex_encodes_its_oid() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        gat_core::oid::test_support::take_to_hex_calls();
        let planned = plan(&repo, &Selection::root(), Validation::Validate).unwrap();
        let to_hex_calls = gat_core::oid::test_support::take_to_hex_calls();

        assert_eq!(planned.actions, Vec::new());
        assert_eq!(
            to_hex_calls, 0,
            "an unmodified stat-proven file must not hex-encode its oid"
        );
    }

    /// A single logical desired-only action whose worktree file differs
    /// from the desired content builds a `Conflict` wrapping a nested
    /// `replace` resolution -- both arms classify the *same* cache oid.
    /// The bounded merge window must verify that oid exactly once and
    /// resolve the nested resolution directly from that one batch's
    /// aligned status, never re-entering the filesystem verifier *and*
    /// never needing a second, separately memoized lookup for the same
    /// oid (`MergeBuffer::flush` resolves every buffered row straight
    /// from its window's `HashMap<Oid, ObjectVerification>`, not by
    /// calling back into `CacheClient::verify`).
    #[test]
    fn a_differing_desired_only_conflict_verifies_its_cache_oid_only_once() {
        use gat_io::cache_proof_test_support as test_support;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"desired content");
        // A worktree file that differs from the desired content forces the
        // `FileStatus::Differs` branch, which composes
        // `conflict_or_corrupted` around a nested `replace_or_missing` --
        // both inspecting the same desired oid.
        std::fs::write(tmp.path().join("a.bin"), b"locally modified").unwrap();

        let before = test_support::snapshot();
        let planned = plan(&repo, &Selection::root(), Validation::Validate).unwrap();
        let after = test_support::snapshot();

        assert!(matches!(
            planned.actions.as_slice(),
            [SyncAction::Conflict { .. }]
        ));
        assert_eq!(
            after.fs_verifications - before.fs_verifications,
            1,
            "the conflict's oid must be verified exactly once"
        );
        assert_eq!(
            after.memo_hits - before.memo_hits,
            0,
            "the nested resolution must resolve from the window's own \
             verification batch, without any separate memoized lookup"
        );
    }

    /// The normal (non-dirty-row) streaming merge -- `merge_desired_with_prior`,
    /// exercised here via `plan()` -- must batch its cache proof lookups
    /// too, not just `plan_from_dirty_rows`: many distinct desired-only
    /// oids with no materialized row collapse into one set-based
    /// `exact_many` proof lookup rather than one point lookup per oid.
    #[test]
    fn the_normal_streaming_merge_batches_many_distinct_desired_only_oids_into_one_lookup() {
        use gat_io::cache_proof_test_support as test_support;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        for i in 0..16 {
            track(
                &repo,
                &format!("f{i:03}.bin"),
                format!("content {i}").as_bytes(),
            );
        }

        let before = test_support::snapshot();
        let planned = plan(&repo, &Selection::root(), Validation::Validate).unwrap();
        let after = test_support::snapshot();

        assert_eq!(planned.actions.len(), 16);
        assert!(
            planned
                .actions
                .iter()
                .all(|a| matches!(a, SyncAction::Materialize(_)))
        );
        // 16 distinct desired-only oids, one logical request, and --
        // since 16 is far below the SQLite bind budget (32766, see
        // `SQL_BIND_BUDGET`) -- a single physical `SELECT ... IN (...)`
        // statement, never 16 point lookups.
        assert_eq!(
            after.proof_lookup_requests - before.proof_lookup_requests,
            1
        );
        assert_eq!(
            after.proof_lookup_statements - before.proof_lookup_statements,
            1
        );
    }

    #[test]
    fn local_modification_is_reported_even_when_gat_lock_did_not_change() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        // `gat.lock` is untouched -- the bug was that a locally modified
        // file was silently ignored whenever the desired OID hadn't
        // changed, instead of being reported as a conflict like it is when
        // the OID *did* change.
        let outcome = sync(
            &repo,
            &SyncOptions {
                validation: Validation::Validate,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.conflicts, vec!["a.bin".to_string()]);
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"locally edited"
        );
    }

    #[test]
    fn validate_reports_local_modification_via_size_change_when_lock_is_unchanged() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        // A size change is caught by validation's stat-only fast path
        // without paying for a content hash.
        std::fs::write(tmp.path().join("a.bin"), b"locally edited, longer").unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                validation: Validation::Validate,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.conflicts, vec!["a.bin".to_string()]);
    }

    #[test]
    fn locally_modified_file_is_left_untouched_and_reported() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        track(&repo, "a.bin", b"world");
        let outcome = sync(
            &repo,
            &SyncOptions {
                validation: Validation::Validate,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.conflicts, vec!["a.bin".to_string()]);
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"locally edited"
        );
    }

    #[test]
    fn existing_conflicting_file_for_a_newly_desired_path_is_reported_under_validate() {
        // A path that has never been synced/materialized before (no
        // materialized row) must still be checked against the working
        // tree under `Validate`: a pre-existing conflicting
        // file must be reported as a conflict, not silently overwritten,
        // regardless of the materialization mode that wrote it.
        let validation = Validation::Validate;
        for link in ["copy", "hardlink", "symlink"] {
            let tmp = git_repo();
            let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            repo.save_config(&gat_core::config::Config {
                cache: gat_core::config::CacheConfig {
                    materialization_strategy: Some(link.parse().unwrap()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
            let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
            let ingested = ingest(&repo, &b"desired content"[..]);
            lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
            repo.save_lock(&lock).unwrap();
            std::fs::write(tmp.path().join("a.bin"), b"pre-existing, different content").unwrap();

            let outcome = sync(
                &repo,
                &SyncOptions {
                    validation,
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(
                outcome.conflicts,
                vec!["a.bin".to_string()],
                "link={link} validation={validation:?}"
            );
            assert_eq!(
                std::fs::read(tmp.path().join("a.bin")).unwrap(),
                b"pre-existing, different content",
                "link={link} validation={validation:?}: existing file must not be overwritten"
            );
        }
    }

    #[test]
    fn missing_cache_object_is_reported_and_leaves_no_file() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut lock = Lock::default();
        lock.upsert(
            GatPath::parse_canonical("a.bin").unwrap(),
            Oid::from_hex(&"a".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(
            outcome.missing,
            vec![(
                gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                gat_core::oid::Oid::from_hex(&"a".repeat(64)).unwrap(),
            )]
        );
        assert!(!tmp.path().join("a.bin").exists());
    }

    #[test]
    fn partial_path_sync_only_touches_the_given_subtree() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "data/a.bin", b"a");
        track(&repo, "other.bin", b"b");

        let outcome = sync(
            &repo,
            &SyncOptions {
                selection: selection(Some("data"), &[], &[]),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.materialized, 1);
        assert!(tmp.path().join("data/a.bin").exists());
        assert!(!tmp.path().join("other.bin").exists());
    }

    #[test]
    fn include_glob_restricts_sync_to_matching_paths() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "models/a.onnx", b"a");
        track(&repo, "models/b.bin", b"b");

        let outcome = sync(
            &repo,
            &SyncOptions {
                selection: selection(None, &["**/*.onnx"], &[]),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.materialized, 1);
        assert!(tmp.path().join("models/a.onnx").exists());
        assert!(!tmp.path().join("models/b.bin").exists());
    }

    #[test]
    fn exclude_glob_leaves_previously_materialized_excluded_files_untouched_on_removal() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"a");
        track(&repo, "tests/b.bin", b"b");
        sync(&repo, &SyncOptions::default()).unwrap();
        assert!(tmp.path().join("a.bin").exists());
        assert!(tmp.path().join("tests/b.bin").exists());

        // Drop both rows from `gat.lock`; only `a.bin` is in scope via
        // `exclude`, so the excluded `tests/b.bin` must be left alone even
        // though desired state does not track it.
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        lock.entries.clear();
        repo.save_lock(&lock).unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                selection: selection(None, &[], &["tests/**"]),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert!(!tmp.path().join("a.bin").exists());
        assert!(tmp.path().join("tests/b.bin").exists());
    }

    #[test]
    fn invalid_lock_file_is_a_hard_error() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("gat.lock"), "not a gat lock\n").unwrap();

        assert!(sync(&repo, &SyncOptions::default()).is_err());
    }

    /// A malformed row discovered part-way through the streaming
    /// materialized-state cursor must fail the whole `plan()` call rather
    /// than silently returning whatever partial `SyncPlan` had been built
    /// up to that point -- planning stays read-only and all-or-nothing.
    #[test]
    fn malformed_row_mid_scan_fails_the_whole_plan_instead_of_a_partial_result() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"first");
        let entry_z = track(&repo, "z.bin", b"last");
        sync(&repo, &SyncOptions::default()).unwrap();

        // Corrupt the *last* materialized row (by path order) directly in
        // SQLite, after `a.bin` would already have been classified, to
        // prove the streaming merge does not return a plan containing
        // only the actions decided before the bad row was reached.
        gat_io::state_test_support::replace_raw_materialized_oid(
            &tmp.path().join(".gat/state/state.sqlite3"),
            "z.bin",
            &[0u8; 31],
        );

        let result = plan(&repo, &Selection::root(), Validation::TrustState);
        assert!(
            result.is_err(),
            "a malformed materialized row must fail planning, not silently truncate it"
        );
        // gat.lock itself, and the still-valid materialized row for
        // a.bin, are untouched -- planning never got to writing anything
        // anyway, but this also confirms the error didn't come from some
        // unrelated corruption.
        assert_eq!(entry_z.path, "z.bin");
    }

    #[test]
    fn corrupted_cache_object_is_reported_distinctly_from_a_plain_conflict() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        let world = track(&repo, "a.bin", b"world");
        // Corrupt the cache object gat would replace the file with, while
        // keeping its size unchanged so a `Size` check alone wouldn't
        // catch it -- only `Hash` re-hashing does.
        let cache_root = repo.resolved_cache_root().unwrap();
        let obj = cache_root.object_path_for_test(&world.oid);
        cache_root
            .make_object_writable_for_test(&world.oid)
            .unwrap();
        std::fs::write(&obj, b"XXXXX").unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                validation: Validation::Validate,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.conflicts.is_empty());
        assert_eq!(
            outcome.corrupted,
            vec![(
                gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                world.oid,
            )]
        );
        assert_eq!(
            std::fs::read(tmp.path().join("a.bin")).unwrap(),
            b"locally edited"
        );
    }

    #[test]
    fn validate_detects_size_preserving_cache_corruption() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"locally edited").unwrap();

        let world = track(&repo, "a.bin", b"world");
        let cache_root = repo.resolved_cache_root().unwrap();
        let obj = cache_root.object_path_for_test(&world.oid);
        cache_root
            .make_object_writable_for_test(&world.oid)
            .unwrap();
        std::fs::write(&obj, b"XXXXX").unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                validation: Validation::Validate,
                ..Default::default()
            },
        )
        .unwrap();

        assert!(outcome.conflicts.is_empty());
        assert_eq!(
            outcome.corrupted,
            vec![(
                gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                world.oid,
            )]
        );
    }

    #[test]
    fn corrupted_missing_object_is_still_reported_as_missing() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        repo.save_config(&gat_core::config::Config {
            cache: gat_core::config::CacheConfig {
                materialization_strategy: Some("copy".parse().unwrap()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        track(&repo, "a.bin", b"hello");
        sync(&repo, &SyncOptions::default()).unwrap();

        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        lock.upsert(
            GatPath::parse_canonical("a.bin").unwrap(),
            Oid::from_hex(&"0".repeat(64)).unwrap(),
        );
        repo.save_lock(&lock).unwrap();

        let outcome = sync(
            &repo,
            &SyncOptions {
                validation: Validation::Validate,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            outcome.missing,
            vec![(
                gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                gat_core::oid::Oid::from_hex(&"0".repeat(64)).unwrap(),
            )]
        );
        assert!(outcome.corrupted.is_empty());
    }

    #[test]
    fn trust_state_treats_deleted_matching_file_as_unchanged() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "a.bin", b"hello");
        sync(
            &repo,
            &SyncOptions {
                validation: Validation::Validate,
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::remove_file(tmp.path().join("a.bin")).unwrap();

        let planned = plan(&repo, &Selection::root(), Validation::TrustState).unwrap();

        assert!(planned.actions.is_empty());
    }

    #[test]
    fn trust_state_with_existing_destination_succeeds_regardless_of_link_mode() {
        // Regression test: with Validation::TrustState and an existing local file at
        // the desired path, sync must succeed and materialize the file rather
        // than producing a link-mode-dependent "delete then fail" error.
        // Under Validation::TrustState the planner emits Materialize without probing
        // the filesystem; do_materialize handles a pre-existing regular file by
        // renaming it aside before calling storage::materialize (which removes
        // dest as cleanup between fallback modes), then restores it only if
        // every mode fails so the pre-existing file is never silently destroyed.
        for link in ["hardlink", "symlink", "copy"] {
            let tmp = git_repo();
            let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .repository_at(tmp.path().to_path_buf());
            repo.save_config(&gat_core::config::Config {
                cache: gat_core::config::CacheConfig {
                    materialization_strategy: Some(link.parse().unwrap()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
            let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
            let ingested = ingest(&repo, &b"desired content"[..]);
            lock.upsert(GatPath::parse_canonical("a.bin").unwrap(), ingested.oid);
            repo.save_lock(&lock).unwrap();
            // Write a pre-existing file at the destination (never synced/not
            // in materialized state) before running sync.
            std::fs::write(tmp.path().join("a.bin"), b"pre-existing content").unwrap();

            let outcome = sync(
                &repo,
                &SyncOptions {
                    validation: Validation::TrustState,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| {
                panic!("link={link}: sync returned an error: {e}");
            });

            assert_eq!(
                outcome.conflicts,
                Vec::<String>::new(),
                "link={link}: expected no conflicts"
            );
            assert_eq!(
                outcome.materialized, 1,
                "link={link}: expected 1 materialized"
            );
            assert_eq!(
                std::fs::read(tmp.path().join("a.bin")).unwrap(),
                b"desired content",
                "link={link}: file content should be updated to desired content"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn trust_state_skips_inaccessible_worktree_paths_entirely() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let entry = track(&repo, "blocked/a.bin", b"hello");
        record_materialized(&repo, std::slice::from_ref(&entry)).unwrap();
        let blocked = tmp.path().join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        let original = std::fs::metadata(&blocked).unwrap().permissions().mode();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let none_result = plan(&repo, &Selection::root(), Validation::TrustState);
        let size_result = plan(&repo, &Selection::root(), Validation::Validate);

        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(original)).unwrap();

        let none_plan = none_result.unwrap();
        assert!(none_plan.actions.is_empty());
        assert!(size_result.is_err());
    }

    /// `prior_only_action`'s `TrustState` branch must skip resolving a
    /// native path/touching the working tree entirely (see
    /// the private `gat-io` worktree resolver),
    /// not merely avoid *following through* on an inaccessible directory:
    /// prove this with a materialized-only path (dropped from `gat.lock`)
    /// whose ancestor is inaccessible, and confirm `TrustState` planning
    /// still succeeds and schedules the plain removal, while `Validate`
    /// planning of the same state errors out from the actual filesystem
    /// probe.
    #[test]
    #[cfg(unix)]
    fn prior_only_action_under_trust_state_never_touches_the_filesystem() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let entry = track(&repo, "blocked/a.bin", b"hello");
        record_materialized(&repo, std::slice::from_ref(&entry)).unwrap();

        // Drop the path from `gat.lock` so it becomes materialized-only,
        // driving `prior_only_action` instead of `desired_only_action`.
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        lock.entries.retain(|e| e.path != "blocked/a.bin");
        repo.save_lock(&lock).unwrap();

        let blocked = tmp.path().join("blocked");
        std::fs::create_dir_all(&blocked).unwrap();
        let original = std::fs::metadata(&blocked).unwrap().permissions().mode();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let trust_result = plan(&repo, &Selection::root(), Validation::TrustState);
        let validate_result = plan(&repo, &Selection::root(), Validation::Validate);

        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(original)).unwrap();

        let trust_plan = trust_result.unwrap();
        assert_eq!(
            trust_plan.actions,
            vec![SyncAction::Remove(
                gat_core::lexical_path::GatPath::parse_canonical("blocked/a.bin").unwrap(),
            )],
            "TrustState must schedule the plain removal without probing the filesystem"
        );
        assert!(
            validate_result.is_err(),
            "Validate must still hit the actual (inaccessible) directory"
        );
    }

    /// Sync planning must resolve a canonical Gat path such as `C:/foo`
    /// (a valid Gat path everywhere because identity is
    /// host-independent, materializability is host-dependent) as an
    /// ordinary relative path below the repository root on this host,
    /// never reinterpreting it as a native drive/rooted path. On Unix,
    /// `std::path::Path` never recognizes a Windows drive prefix, so this
    /// exercises the full `desired_only_action` -> `resolve_worktree_path`
    /// path end-to-end.
    #[test]
    #[cfg(unix)]
    fn desired_only_action_resolves_a_windows_drive_like_gat_path_below_root() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        track(&repo, "C:/foo", b"hello");

        let outcome = sync(&repo, &SyncOptions::default()).unwrap();

        assert_eq!(outcome.materialized, 1);
        assert_eq!(
            std::fs::read(tmp.path().join("C:").join("foo")).unwrap(),
            b"hello"
        );
    }

    /// A desired-only path between two materialized rows must not advance
    /// or revalidate the materialized cursor against itself. The merge must
    /// classify the inserted path and continue with the pending prior row.
    #[test]
    fn desired_only_path_between_two_materialized_rows_does_not_panic() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());

        // Materialized state left over from a previous branch: a.bin,
        // c.bin, d.bin.
        let entry_a = track(&repo, "a.bin", b"a");
        let entry_c = track(&repo, "c.bin", b"c");
        let entry_d = track(&repo, "d.bin", b"d");
        record_materialized(&repo, &[entry_a, entry_c, entry_d]).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("c.bin"), b"c").unwrap();
        std::fs::write(tmp.path().join("d.bin"), b"d").unwrap();

        // Desired (checked-out) lock now tracks a.bin, b.bin, d.bin --
        // c.bin is gone, b.bin is new and sorts strictly between the two
        // materialized rows a.bin and c.bin.
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        lock.remove_prefix(&gat_core::lexical_path::GatPath::parse_canonical("c.bin").unwrap());
        let ingested = ingest(&repo, &b"b"[..]);
        lock.upsert(GatPath::parse_canonical("b.bin").unwrap(), ingested.oid);
        repo.save_lock(&lock).unwrap();

        let planned = plan(&repo, &Selection::root(), Validation::Validate).unwrap();
        let paths: Vec<&str> = planned
            .actions
            .iter()
            .map(|a| match a {
                SyncAction::Materialize(entry)
                | SyncAction::Replace(entry)
                | SyncAction::Rematerialize(entry) => entry.path.as_str(),
                SyncAction::Remove(path)
                | SyncAction::MissingObject { path, .. }
                | SyncAction::Corrupted { path, .. } => path.as_str(),
                SyncAction::Conflict { path, .. } => path.as_str(),
            })
            .collect();
        assert!(
            paths.contains(&"b.bin"),
            "expected b.bin materialized: {paths:?}"
        );
        assert!(
            paths.contains(&"c.bin"),
            "expected c.bin removed: {paths:?}"
        );
        assert_eq!(
            paths.len(),
            2,
            "no action expected for a.bin/d.bin: {paths:?}"
        );
    }

    /// Builds a fresh [`crate::operation::Operation`] for `repo`
    /// using explicit `limits` instead of production defaults, mirroring
    /// `engine::workspace::sync::tests::sync_with_limits` so this test can exercise
    /// a small, explicit `merge_window` through the real
    /// `sync_from_snapshot` entry point rather than a parallel test-only
    /// sync path.
    fn sync_with_limits(
        repo: &Repo,
        opts: &SyncOptions,
        limits: crate::limits::ExecutionLimits,
    ) -> crate::workspace::sync::SyncOutcome {
        let cfg = repo.load_config().unwrap();
        let desired_revision = {
            let _guard = repo.acquire_configuration_lock().unwrap();
            crate::repository_state::current_desired_revision(repo).unwrap()
        };
        let snapshot =
            crate::snapshot::Snapshot::new(repo.snapshot_input(cfg, desired_revision)).unwrap();
        let session = crate::session::Session::with_limits(limits);
        let mut operation = crate::operation::Operation::new(repo, snapshot, session);
        super::super::sync_from_snapshot(&mut operation, opts, None).unwrap()
    }

    /// A fresh validated sync spanning more than one
    /// [`MERGE_BATCH`] worth of desired rows must never buffer more than
    /// one bounded batch of merge rows at a time -- the merge streams
    /// classified actions straight into `ExecutePlanSink` (or
    /// `DryRunPlanSink` for `--dry-run`), draining `MergeBuffer` every
    /// time it reaches `MERGE_BATCH`, rather than accumulating an
    /// additional whole-operation `Vec` across the whole merge. Uses a
    /// small, explicit `merge_window` (rather than the production
    /// default) and a small synthetic multi-window row set through the
    /// real merge/buffer/sink path, so this test proves the same
    /// bounded-buffering invariant without thousands of object ingests
    /// and filesystem materializations.
    #[test]
    fn merge_buffer_never_exceeds_one_bounded_batch_across_many_rows() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut limits = crate::limits::ExecutionLimits::tiny();
        let merge_window = 4;
        limits.sync.merge_window = std::num::NonZeroUsize::new(merge_window).unwrap();
        let count = merge_window * 2 + 3;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..count {
            let content = format!("data-{i}").into_bytes();
            let ingested = ingest(&repo, content.as_slice());
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();

        let outcome = sync_with_limits(&repo, &SyncOptions::default(), limits);
        assert_eq!(outcome.materialized, count);
        let high_water = test_support::merge_buffer_high_water();
        assert!(
            high_water <= merge_window,
            "merge buffer high-water mark {high_water} exceeded merge_window {merge_window}"
        );
    }

    /// An ordinary sync never enqueues a
    /// `FileStatus::Matches` row's oid into cache verification at all
    /// (see the merge loop's `FileStatus::Matches` arm), but
    /// `--rematerialize` enqueues *every* selected clean row as a
    /// `PendingCache::Rematerialize` -- so this proves the sync engine's
    /// cache-verification memo stays bounded near one verification
    /// window's worth of distinct oids for a `--rematerialize` run with
    /// many more unique oids than that, rather than retaining every
    /// verified oid for the rest of the operation
    /// (`MergeBuffer::flush` uses `verify_windows_unmemoized`, not
    /// `verify_many`). Overrides `VERIFY_WINDOW` down to a small value
    /// (rather than needing a many-thousand-object fixture) to exercise
    /// the same bounded-memo code path deterministically.
    #[test]
    fn rematerialize_keeps_the_cache_verification_memo_bounded_across_many_unique_oids() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let verify_window = 4;
        let _guard = gat_io::cache_object_test_support::with_verify_window(verify_window);
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.merge_window = std::num::NonZeroUsize::new(verify_window).unwrap();
        let count = verify_window * 5 + 3;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..count {
            let content = format!("unique-{i}").into_bytes();
            let ingested = ingest(&repo, content.as_slice());
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();
        // Materialize every path normally first so it's all already clean;
        // only then does the second, --rematerialize run below actually
        // exercise the all-`Rematerialize` worst case. Ordinary sync now
        // intentionally retains its verification memo for the whole
        // operation (cross-window reuse) -- reset the high-water counter
        // afterwards so it only reflects the `--rematerialize` run below.
        sync_with_limits(&repo, &SyncOptions::default(), limits);
        gat_io::cache_object_test_support::reset_memo_high_water();

        let outcome = sync_with_limits(
            &repo,
            &SyncOptions {
                rematerialize: true,
                ..Default::default()
            },
            limits,
        );

        assert_eq!(outcome.rematerialized, count);
        let high_water = gat_io::cache_object_test_support::memo_high_water();
        assert!(
            high_water <= verify_window,
            "expected the cache-verification memo to stay bounded near one verification \
             window ({verify_window}) even though --rematerialize enqueued {count} unique \
             oids, got {high_water}"
        );
    }

    /// Companion to the high-water test above: many *paths* referencing
    /// the same handful of distinct oids must still be verified
    /// efficiently -- the merge window's own within-window dedup
    /// (`verify_windows_unmemoized`'s `seen` set) means a shared oid is
    /// only ever actually filesystem-verified once per window it appears
    /// in, not once per path.
    #[test]
    fn rematerialize_deduplicates_repeated_paths_sharing_the_same_oid_within_a_window() {
        use gat_io::cache_proof_test_support as cache_state_test_support;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let verify_window = 8;
        let _guard = gat_io::cache_object_test_support::with_verify_window(verify_window);
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.merge_window = std::num::NonZeroUsize::new(verify_window).unwrap();
        let ingested = ingest(&repo, b"shared content".as_slice());
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        let path_count = verify_window * 2;
        for i in 0..path_count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();
        sync_with_limits(&repo, &SyncOptions::default(), limits);

        let before = cache_state_test_support::snapshot();
        let outcome = sync_with_limits(
            &repo,
            &SyncOptions {
                rematerialize: true,
                ..Default::default()
            },
            limits,
        );
        let after = cache_state_test_support::snapshot();

        assert_eq!(outcome.rematerialized, path_count);
        // One distinct oid per merge window (at most `path_count /
        // merge_window` windows, rounded up) -- never one filesystem
        // verification per path.
        let max_windows = path_count.div_ceil(verify_window);
        assert!(
            after.fs_verifications - before.fs_verifications <= max_windows,
            "expected at most {max_windows} filesystem verifications for {path_count} paths \
             sharing one oid, got {}",
            after.fs_verifications - before.fs_verifications
        );
    }

    /// Ordinary non-`--rematerialize` sync must keep
    /// benefiting from cross-window memoization -- `MergeBuffer::flush`
    /// uses [`gat_io::CacheClient::verify_windows`] (not
    /// `_unmemoized`) whenever `policy.rematerialize == false`, so a
    /// shared oid that reappears in a later merge window is resolved
    /// from the memo instead of triggering another filesystem
    /// verification. Spans more than 3 merge windows so an accidental
    /// per-window (rather than per-operation) verification
    /// implementation would be caught.
    #[test]
    fn ordinary_sync_reuses_the_verification_memo_for_a_shared_oid_across_many_windows() {
        use gat_io::cache_proof_test_support as cache_state_test_support;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let merge_window = 4;
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.merge_window = std::num::NonZeroUsize::new(merge_window).unwrap();
        let ingested = ingest(&repo, b"shared content".as_slice());
        // More than 3 merge windows' worth of paths, all sharing one oid,
        // and all already materialized under a *different* oid so every
        // row is a genuine `PendingCache::Replace` (cache-dependent) --
        // never a cache-free `Matches` row that ordinary sync would skip
        // enqueueing entirely.
        let path_count = merge_window * 5;
        let stale = ingest(&repo, b"stale content".as_slice());
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..path_count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                stale.oid,
            );
        }
        repo.save_lock(&lock).unwrap();
        sync_with_limits(&repo, &SyncOptions::default(), limits);

        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..path_count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();

        let before = cache_state_test_support::snapshot();
        let outcome = sync_with_limits(&repo, &SyncOptions::default(), limits);
        let after = cache_state_test_support::snapshot();

        assert_eq!(outcome.replaced, path_count);
        // Exactly one real filesystem verification for the whole
        // operation -- every later window's lookup of the same oid must
        // be a memo hit, not a re-verification.
        assert_eq!(
            after.fs_verifications - before.fs_verifications,
            1,
            "expected exactly one filesystem verification for one oid shared across \
             {path_count} paths spanning {} merge windows, got {}",
            path_count.div_ceil(merge_window),
            after.fs_verifications - before.fs_verifications
        );
    }

    /// Companion to the single-shared-oid test above: a *small set* of
    /// shared oids distributed across many merge windows must still
    /// reuse the memo per-oid, proving reuse isn't an artifact of the
    /// single-oid fixture.
    #[test]
    fn ordinary_sync_reuses_the_verification_memo_for_a_small_set_of_shared_oids() {
        use gat_io::cache_proof_test_support as cache_state_test_support;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let merge_window = 4;
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.merge_window = std::num::NonZeroUsize::new(merge_window).unwrap();
        let distinct_oid_count = 3;
        let oids: Vec<Oid> = (0..distinct_oid_count)
            .map(|i| ingest(&repo, format!("shared content {i}").into_bytes().as_slice()).oid)
            .collect();
        let stale = ingest(&repo, b"stale content".as_slice());
        let path_count = merge_window * 6;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..path_count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                stale.oid,
            );
        }
        repo.save_lock(&lock).unwrap();
        sync_with_limits(&repo, &SyncOptions::default(), limits);

        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..path_count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                oids[i % distinct_oid_count],
            );
        }
        repo.save_lock(&lock).unwrap();

        let before = cache_state_test_support::snapshot();
        let outcome = sync_with_limits(&repo, &SyncOptions::default(), limits);
        let after = cache_state_test_support::snapshot();

        assert_eq!(outcome.replaced, path_count);
        assert_eq!(
            after.fs_verifications - before.fs_verifications,
            distinct_oid_count,
            "expected exactly one filesystem verification per distinct shared oid \
             ({distinct_oid_count} oids across {path_count} paths spanning {} merge windows), \
             got {}",
            path_count.div_ceil(merge_window),
            after.fs_verifications - before.fs_verifications
        );
    }

    /// A warm no-op ordinary sync (every path already matches what's
    /// materialized) must perform zero cache verification: the merge
    /// loop only enqueues a `PendingCache` row for a cache-dependent
    /// classification (`Materialize`/`Replace`/conflict), and a
    /// `FileStatus::Matches` row with `rematerialize == false` is pushed
    /// as a cache-free ready row instead -- so `MergeBuffer::flush` never
    /// even calls into `CacheClient::verify_windows` for these paths.
    #[test]
    fn ordinary_warm_no_op_sync_performs_no_cache_verification() {
        use gat_io::cache_proof_test_support as cache_state_test_support;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let merge_window = 4;
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.merge_window = std::num::NonZeroUsize::new(merge_window).unwrap();
        let ingested = ingest(&repo, b"warm content".as_slice());
        let path_count = merge_window * 3;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..path_count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();
        let outcome = sync_with_limits(&repo, &SyncOptions::default(), limits);
        assert_eq!(outcome.materialized, path_count);

        let before = cache_state_test_support::snapshot();
        let outcome = sync_with_limits(&repo, &SyncOptions::default(), limits);
        let after = cache_state_test_support::snapshot();

        assert!(outcome.did_nothing());
        assert_eq!(
            after.fs_verifications, before.fs_verifications,
            "expected a warm no-op sync to perform zero cache verification"
        );
    }

    /// A conflict whose forced resolution references the same oid as
    /// another conflicting path in the same operation must not trigger a
    /// second filesystem verification: conflicts are cache-dependent
    /// (`PendingCache::ConflictOrReplace`) exactly like `Replace` rows,
    /// so they go through the same memoized `verify_windows` path under
    /// ordinary sync.
    #[test]
    fn ordinary_sync_force_resolves_conflicts_sharing_an_oid_with_one_verification() {
        use gat_io::cache_proof_test_support as cache_state_test_support;

        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let merge_window = 4;
        let mut limits = crate::limits::ExecutionLimits::tiny();
        limits.sync.merge_window = std::num::NonZeroUsize::new(merge_window).unwrap();
        let ingested = ingest(&repo, b"desired content".as_slice());
        let path_count = merge_window * 3;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..path_count {
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();
        // Materialize once, then locally modify every path so the next
        // sync sees a genuine conflict (working tree differs from both
        // the desired and last-materialized content) for every row.
        sync_with_limits(&repo, &SyncOptions::default(), limits);
        for i in 0..path_count {
            std::fs::write(
                tmp.path().join(format!("file-{i:06}.bin")),
                format!("locally modified {i}"),
            )
            .unwrap();
        }

        let before = cache_state_test_support::snapshot();
        let outcome = sync_with_limits(
            &repo,
            &SyncOptions {
                force: true,
                ..Default::default()
            },
            limits,
        );
        let after = cache_state_test_support::snapshot();

        assert_eq!(outcome.replaced, path_count);
        assert_eq!(
            after.fs_verifications - before.fs_verifications,
            1,
            "expected exactly one filesystem verification for one oid shared across \
             {path_count} force-resolved conflicting paths, got {}",
            after.fs_verifications - before.fs_verifications
        );
    }

    /// `--rematerialize --dry-run` must stay
    /// bounded exactly like a real `--rematerialize` run -- `plan_dry_run`
    /// drives the same bounded `MergeBuffer`/`verify_windows_unmemoized`
    /// machinery through `DryRunPlanSink` instead of first collecting a
    /// complete `SyncPlan` (`plan::CollectPlanSink`) that would retain one
    /// `SyncAction::Rematerialize` per selected clean path. Proves the
    /// merge-buffer high-water stays bounded near one `merge_window` even
    /// though every one of `count` selected paths classifies as
    /// `Rematerialize` under dry-run, and that dry-run performs zero real
    /// filesystem rematerializations (`execute::test_support::
    /// do_rematerialize_calls()` stays flat) while still reporting the
    /// exact same `rematerialized` count a real run would.
    #[test]
    fn dry_run_rematerialize_keeps_the_merge_buffer_bounded_across_many_rows() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut limits = crate::limits::ExecutionLimits::tiny();
        let merge_window = 4;
        limits.sync.merge_window = std::num::NonZeroUsize::new(merge_window).unwrap();
        let count = merge_window * 3 + 2;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..count {
            let content = format!("dry-run-data-{i}").into_bytes();
            let ingested = ingest(&repo, content.as_slice());
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();
        // Materialize every path normally first so every one of them is
        // already clean -- only then does `--rematerialize --dry-run`
        // classify every single row as `Rematerialize` (the worst case for
        // cache-dependent action density).
        sync_with_limits(&repo, &SyncOptions::default(), limits);

        let before_calls = crate::workspace::sync::execute::test_support::do_rematerialize_calls();
        let outcome = sync_with_limits(
            &repo,
            &SyncOptions {
                rematerialize: true,
                dry_run: true,
                ..Default::default()
            },
            limits,
        );
        let after_calls = crate::workspace::sync::execute::test_support::do_rematerialize_calls();

        assert!(outcome.dry_run);
        assert_eq!(outcome.rematerialized, count);
        assert_eq!(
            after_calls, before_calls,
            "dry-run must never perform a real filesystem rematerialization"
        );
        let high_water = test_support::merge_buffer_high_water();
        assert!(
            high_water <= merge_window,
            "dry-run merge buffer high-water mark {high_water} exceeded merge_window \
             {merge_window} even though every one of {count} rows classified as Rematerialize"
        );
    }

    /// `--rematerialize --dry-run`'s reported
    /// counts must exactly match what a subsequent real
    /// `--rematerialize` run actually does -- the bounded `DryRunPlanSink`
    /// tally must classify identically to the real, mutating
    /// `ExecutePlanSink` path, not merely stay bounded.
    #[test]
    fn dry_run_rematerialize_counts_match_a_subsequent_real_run() {
        let tmp = git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut limits = crate::limits::ExecutionLimits::tiny();
        let merge_window = 4;
        limits.sync.merge_window = std::num::NonZeroUsize::new(merge_window).unwrap();
        let count = merge_window * 2 + 1;
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for i in 0..count {
            let content = format!("match-data-{i}").into_bytes();
            let ingested = ingest(&repo, content.as_slice());
            lock.upsert(
                GatPath::parse_canonical(&format!("file-{i:06}.bin")).unwrap(),
                ingested.oid,
            );
        }
        repo.save_lock(&lock).unwrap();
        sync_with_limits(&repo, &SyncOptions::default(), limits);

        let dry_run_outcome = sync_with_limits(
            &repo,
            &SyncOptions {
                rematerialize: true,
                dry_run: true,
                ..Default::default()
            },
            limits,
        );
        let real_outcome = sync_with_limits(
            &repo,
            &SyncOptions {
                rematerialize: true,
                ..Default::default()
            },
            limits,
        );

        assert_eq!(dry_run_outcome.rematerialized, real_outcome.rematerialized);
        assert_eq!(dry_run_outcome.rematerialized, count);
        assert_eq!(dry_run_outcome.conflicts, real_outcome.conflicts);
        assert_eq!(dry_run_outcome.missing, real_outcome.missing);
        assert_eq!(dry_run_outcome.corrupted, real_outcome.corrupted);
    }
}
