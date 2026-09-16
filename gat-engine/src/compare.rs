//! The one row-comparison engine `gat status` and `gat diff` share:
//! given two path-ordered row streams, classify each path as added,
//! removed, modified, or unchanged.
//!
//! The module merge-walks ordered rows with bounded comparison state so a
//! narrowly scoped comparison does not materialize both whole states:
//!
//! - **Same shape** (both sides flat, or both sharded at the same depth):
//!   compare logical shard by logical shard. Rows inside a shard are
//!   path-ordered, and a path is deterministically placed, so a shard's
//!   rows on one side can only ever pair with the same shard's rows on the
//!   other. Two Git-persisted shards with the *same blob id* are
//!   byte-identical and are skipped without parsing when the caller does
//!   not need unchanged rows.
//! - **Cross shape** (flat vs sharded, or different depths): a path's
//!   shard on one side says nothing about its shard on the other, so this
//!   falls back to one explicit full-logical comparison. That fallback is
//!   deliberately kept out of the same-shape path so the ordinary case
//!   pays nothing for it.
//!
//! Only the *output* is retained and sorted: `gat diff` keeps its `K`
//! changed rows, `gat status` keeps the `N` rows it prints by definition.
//! Neither duplicates its inputs to get there.

use crate::repository::Repository;
use gat_core::git::GitRevisionSpec;
use gat_core::lexical_path::GatPath;
use gat_core::lock::{Entry, LockShardId, LockShardLevels};
use gat_core::oid::Oid;
use gat_core::selection::Selection;
use gat_io::{
    DesiredQuery, DesiredRow, GitReader, LockSnapshot, LockSnapshotError, LockSnapshotErrorKind,
    SnapshotShard, StateStore, StateStoreError,
};
use std::collections::BTreeMap;

/// Semantic stage at which a repository comparison failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompareErrorKind {
    Acquisition(crate::RepoSnapshotErrorKind),
    Repository,
    GitIndex,
    Revision(GitRevisionSpec),
    GitObject,
    InvalidSnapshot { label: String },
    State(crate::SyncStateFailureKind),
    Lock(crate::SyncLockFailureKind),
    DesiredState(crate::SyncErrorKind),
}

/// The row-comparison engine's application-facing error surface.
///
/// Physical Git, lock, and state-store errors remain available through the
/// technical source chain without exposing their paths or implementation
/// variants to command orchestration or root presentation.
#[derive(Debug)]
pub struct CompareError {
    kind: CompareErrorKind,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl CompareError {
    fn new(kind: CompareErrorKind, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &CompareErrorKind {
        &self.kind
    }
}

impl std::fmt::Display for CompareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stage = match self.kind {
            CompareErrorKind::Acquisition(_) => "capture coherent local state",
            CompareErrorKind::Repository => "open the Git repository",
            CompareErrorKind::GitIndex => "read the Git index",
            CompareErrorKind::Revision(_) => "resolve a Git revision",
            CompareErrorKind::GitObject => "read a persisted gat.lock from Git",
            CompareErrorKind::InvalidSnapshot { .. } => "validate a persisted gat.lock",
            CompareErrorKind::State(_) => "read Gat's local state",
            CompareErrorKind::Lock(_) => "read gat.lock",
            CompareErrorKind::DesiredState(_) => "refresh Gat's desired state",
        };
        write!(formatter, "could not {stage}")
    }
}

impl std::error::Error for CompareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

impl From<LockSnapshotError> for CompareError {
    fn from(source: LockSnapshotError) -> Self {
        let kind = match source.kind() {
            LockSnapshotErrorKind::OpenRepository => CompareErrorKind::Repository,
            LockSnapshotErrorKind::IndexLookup => CompareErrorKind::GitIndex,
            LockSnapshotErrorKind::RevisionResolution => {
                CompareErrorKind::Revision(GitRevisionSpec::from(source.operation()))
            }
            LockSnapshotErrorKind::ObjectRead => CompareErrorKind::GitObject,
            LockSnapshotErrorKind::InvalidSnapshot => CompareErrorKind::InvalidSnapshot {
                label: source.label().to_string(),
            },
        };
        Self::new(kind, source)
    }
}

impl From<StateStoreError> for CompareError {
    fn from(source: StateStoreError) -> Self {
        Self::new(
            CompareErrorKind::State(crate::repository_access::classify_state(&source)),
            source,
        )
    }
}

impl From<gat_io::LockError> for CompareError {
    fn from(source: gat_io::LockError) -> Self {
        Self::new(
            CompareErrorKind::Lock(crate::repository_access::classify_lock_kind(&source)),
            source,
        )
    }
}

impl From<crate::workspace::sync::SyncError> for CompareError {
    fn from(source: crate::workspace::sync::SyncError) -> Self {
        use crate::workspace::sync::SyncErrorKind;

        let kind = match source.kind() {
            SyncErrorKind::Lock(kind) => CompareErrorKind::Lock(*kind),
            SyncErrorKind::State(kind) => CompareErrorKind::State(*kind),
            kind => CompareErrorKind::DesiredState(kind.clone()),
        };
        Self {
            kind,
            source: Box::new(source),
        }
    }
}

pub type Result<T> = std::result::Result<T, CompareError>;

/// What happened to one path between the `from` and `to` states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowChange {
    /// Only in `to`.
    Added { oid: Oid },
    /// Only in `from`.
    Removed,
    /// In both, with different OIDs (content identity is OID-only).
    Modified { oid: Oid },
    /// In both with the same OID; only retained when the caller asks for
    /// unchanged rows (`gat status`).
    Unchanged { oid: Oid },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedRow {
    pub path: GatPath,
    pub change: RowChange,
}

impl ChangedRow {
    #[cfg(test)]
    const fn is_change(&self) -> bool {
        !matches!(self.change, RowChange::Unchanged { .. })
    }
}

/// Whether a comparison retains unchanged paths (`gat status` prints every
/// tracked path) or only changed ones (`gat diff`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unchanged {
    Keep,
    Drop,
}

impl Unchanged {
    const fn keeps(self) -> bool {
        matches!(self, Self::Keep)
    }
}

/// Merge two path-ordered sources, retaining at most one pending row per side.
/// Both persisted and current rows carry native OIDs; conversion from a
/// desired-state row moves its fields without allocation.
fn merge_ordered(
    mut left: impl FnMut() -> Result<Option<Entry>>,
    mut right: impl FnMut() -> Result<Option<Entry>>,
    unchanged: Unchanged,
    out: &mut Vec<ChangedRow>,
) -> Result<()> {
    let mut l = left()?;
    let mut r = right()?;
    while let Some(pair) = take_ordered(&mut l, &mut r, |from, to| from.path.cmp(&to.path)) {
        match pair {
            OrderedPair::Left(from) => {
                out.push(ChangedRow {
                    path: from.path,
                    change: RowChange::Removed,
                });
                l = left()?;
            }
            OrderedPair::Right(to) => {
                let Entry { path, oid } = to;
                out.push(ChangedRow {
                    path,
                    change: RowChange::Added { oid },
                });
                r = right()?;
            }
            OrderedPair::Both(from, to) => {
                let same = to.oid == from.oid;
                if !same || unchanged.keeps() {
                    let oid = to.oid;
                    out.push(ChangedRow {
                        path: from.path,
                        change: if same {
                            RowChange::Unchanged { oid }
                        } else {
                            RowChange::Modified { oid }
                        },
                    });
                }
                l = left()?;
                r = right()?;
            }
        }
    }
    Ok(())
}

/// A merge step owns exactly the rows it consumes. The unconsumed row stays
/// in its cursor; callers never reconstruct presence from parallel booleans.
enum OrderedPair<L, R> {
    Left(L),
    Right(R),
    Both(L, R),
}

fn take_ordered<L, R>(
    left: &mut Option<L>,
    right: &mut Option<R>,
    compare: impl FnOnce(&L, &R) -> std::cmp::Ordering,
) -> Option<OrderedPair<L, R>> {
    match (left.take(), right.take()) {
        (None, None) => None,
        (Some(l), None) => Some(OrderedPair::Left(l)),
        (None, Some(r)) => Some(OrderedPair::Right(r)),
        (Some(l), Some(r)) => match compare(&l, &r) {
            std::cmp::Ordering::Less => {
                *right = Some(r);
                Some(OrderedPair::Left(l))
            }
            std::cmp::Ordering::Greater => {
                *left = Some(l);
                Some(OrderedPair::Right(r))
            }
            std::cmp::Ordering::Equal => Some(OrderedPair::Both(l, r)),
        },
    }
}

fn entry_source(entries: impl IntoIterator<Item = Entry>) -> impl FnMut() -> Result<Option<Entry>> {
    let mut iter = entries.into_iter();
    move || Ok(iter.next())
}

/// Pair shards by logical ID, retaining which side supplies each shard.
/// Both inputs must already be sorted by id (as [`LockSnapshot::shards`] returns) -- this walks each vector
/// once with two cursors, yielding one pair at a time instead of
/// collecting every pair into a `Vec` first, so pairing every id present
/// on either side costs `O(Q)` total time and `O(1)` extra space, not
/// `O(Q^2)` time or an `O(Q)` intermediate allocation.
struct ShardIdMerge<'a> {
    left: &'a [SnapshotShard],
    right: &'a [SnapshotShard],
    li: usize,
    ri: usize,
}

impl<'a> Iterator for ShardIdMerge<'a> {
    type Item = OrderedPair<&'a SnapshotShard, &'a SnapshotShard>;

    fn next(&mut self) -> Option<Self::Item> {
        match take_ordered(
            &mut self.left.get(self.li),
            &mut self.right.get(self.ri),
            |l, r| l.id.cmp(&r.id),
        )? {
            OrderedPair::Left(l) => {
                self.li += 1;
                Some(OrderedPair::Left(l))
            }
            OrderedPair::Right(r) => {
                self.ri += 1;
                Some(OrderedPair::Right(r))
            }
            OrderedPair::Both(l, r) => {
                self.li += 1;
                self.ri += 1;
                Some(OrderedPair::Both(l, r))
            }
        }
    }
}

const fn merge_shards_by_id<'a>(
    left: &'a [SnapshotShard],
    right: &'a [SnapshotShard],
) -> ShardIdMerge<'a> {
    ShardIdMerge {
        left,
        right,
        li: 0,
        ri: 0,
    }
}

/// Merge a logical shard pair while streaming at least one side.
fn stream_shards(
    from: &LockSnapshot,
    to: &LockSnapshot,
    pair: OrderedPair<&SnapshotShard, &SnapshotShard>,
    selection: &Selection,
    unchanged: Unchanged,
    out: &mut Vec<ChangedRow>,
) -> Result<()> {
    match pair {
        OrderedPair::Left(l) => from.with_shard_rows_pull(l, selection, |left| {
            merge_ordered(left, || Ok(None), unchanged, out)
        }),
        OrderedPair::Right(r) => to.with_shard_rows_pull(r, selection, |right| {
            merge_ordered(|| Ok(None), right, unchanged, out)
        }),
        OrderedPair::Both(l, r)
            if selection.scope_path().is_some() || !from.shard_levels().is_flat() =>
        {
            // Release the left blob and validation index before opening the
            // right. Scoped rows or one shard stay bounded without retaining
            // two full backing blobs for a potentially tiny selection.
            let left = from.shard_rows_selected(l, selection)?;
            to.with_shard_rows_pull(r, selection, |right| {
                merge_ordered(entry_source(left), right, unchanged, out)
            })
        }
        OrderedPair::Both(l, r) => from.with_shard_rows_pull(l, selection, |left| {
            to.with_shard_rows_pull(r, selection, |right| {
                merge_ordered(left, right, unchanged, out)
            })
        }),
    }
}

/// Compare two persisted snapshots (`gat diff <rev1> <rev2>`, and
/// `gat status`'s staged side when compared against another snapshot).
fn compare_snapshots(
    from: &LockSnapshot,
    to: &LockSnapshot,
    selection: &Selection,
    unchanged: Unchanged,
) -> Result<Vec<ChangedRow>> {
    let mut out = Vec::new();
    if from.shard_levels() == to.shard_levels() {
        for pair in merge_shards_by_id(from.shards(), to.shards()) {
            if let OrderedPair::Both(left, right) = &pair
                && left.has_same_blob(right)
                && !unchanged.keeps()
            {
                continue;
            }
            stream_shards(from, to, pair, selection, unchanged, &mut out)?;
        }
        // Rows are ordered within each shard; only sharded output needs sorting.
        if !from.shard_levels().is_flat() {
            out.sort_by(|a, b| a.path.cmp(&b.path));
        }
    } else {
        // Cross-shape fallback (flat vs sharded, or different depths):
        // hash placement gives no shard correspondence, so both sides are
        // compared as one full logical state. Deliberately isolated here
        // so same-shape comparisons above never pay for it.
        merge_ordered(
            entry_source(from.rows_sorted(selection)?),
            entry_source(to.rows_sorted(selection)?),
            unchanged,
            &mut out,
        )?;
    }
    Ok(out)
}

/// Compare a persisted snapshot (`from`) against current desired state
/// (`to`, the refreshed `SQLite` mirror) -- `gat status`'s staged-vs-working
/// comparison and `gat diff`'s revision-vs-working-tree comparison.
///
/// Current-state work scales with the selection: the `SQLite` side is
/// narrowed by `Selection::scope_path()` and, on the same-shape plan, by
/// logical shard, with `Selection::matches` as the residual authority.
/// Merge every persisted shard against a stream of current-state groups
/// already produced in ascending shard-id order, pairing shard ids with
/// two cursors exactly as same-shape persisted-vs-persisted comparison
/// does (`merge_shards_by_id`): `O(Q)` shard-id pairing, not `O(Q^2)`.
/// `next_current_group` is a plain "give me the next current group, or
/// `None`" callback, so this same merge drives both a bounded
/// (`BTreeMap`-backed) scoped current read and an unbounded full
/// traversal that only ever holds one shard's rows in memory
/// (the state capability's grouped pull operation).
fn merge_persisted_shards_with_current_groups(
    from: &LockSnapshot,
    selection: &Selection,
    unchanged: Unchanged,
    mut next_current_group: impl FnMut() -> Result<Option<(LockShardId, Vec<DesiredRow>)>>,
    out: &mut Vec<ChangedRow>,
) -> Result<()> {
    let mut persisted = from.shards().iter();
    let mut left = persisted.next();
    let mut right = next_current_group()?;
    while let Some(pair) = take_ordered(&mut left, &mut right, |shard, (id, _)| shard.id.cmp(id)) {
        match pair {
            OrderedPair::Left(shard) => {
                merge_ordered(
                    entry_source(from.shard_rows_selected(shard, selection)?),
                    entry_source(Vec::new()),
                    unchanged,
                    out,
                )?;
                left = persisted.next();
            }
            OrderedPair::Right((_, rows)) => {
                merge_ordered(
                    entry_source(Vec::new()),
                    entry_source(rows.into_iter().map(DesiredRow::into_entry)),
                    unchanged,
                    out,
                )?;
                right = next_current_group()?;
            }
            OrderedPair::Both(shard, (_, rows)) => {
                merge_ordered(
                    entry_source(from.shard_rows_selected(shard, selection)?),
                    entry_source(rows.into_iter().map(DesiredRow::into_entry)),
                    unchanged,
                    out,
                )?;
                left = persisted.next();
                right = next_current_group()?;
            }
        }
    }
    Ok(())
}

/// Repository-bound semantic comparison service.
pub struct ComparisonService<'repo> {
    repo: &'repo Repository,
}

/// One pinned local generation for selection, ownership, cache location and rows.
pub struct CurrentComparison<'repo> {
    repo: &'repo Repository,
    config: gat_core::config::Config,
    store: StateStore,
    levels: LockShardLevels,
}

impl CurrentComparison<'_> {
    #[must_use]
    pub const fn config(&self) -> &gat_core::config::Config {
        &self.config
    }

    #[must_use]
    pub fn cache_presence(&self) -> crate::CachePresenceSession {
        crate::CachePresenceSession::from_config(self.repo, &self.config)
    }

    pub fn staged_with_current(
        &self,
        selection: &Selection,
        unchanged: Unchanged,
    ) -> Result<Vec<ChangedRow>> {
        let reader = GitReader::open(self.repo.layout())
            .map_err(|source| CompareError::new(CompareErrorKind::Repository, source))?;
        compare_snapshot_with_current(
            &reader.staged_lock_snapshot()?,
            &self.store,
            self.levels,
            selection,
            unchanged,
        )
    }

    pub fn revision_with_current(
        &self,
        from: &GitRevisionSpec,
        selection: &Selection,
        unchanged: Unchanged,
    ) -> Result<Vec<ChangedRow>> {
        let reader = GitReader::open(self.repo.layout())
            .map_err(|source| CompareError::new(CompareErrorKind::Repository, source))?;
        compare_snapshot_with_current(
            &reader.lock_snapshot_at(from)?,
            &self.store,
            self.levels,
            selection,
            unchanged,
        )
    }
}

impl<'repo> ComparisonService<'repo> {
    pub(crate) const fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Recover and pin a local generation before resolving selection or ownership.
    pub fn current(
        &self,
        progress: &dyn gat_core::progress::ProgressReporter,
    ) -> std::result::Result<CurrentComparison<'repo>, crate::RepoSnapshotError> {
        let (config, store, refreshed) =
            crate::repo_snapshot::recover_and_pin_state(self.repo, progress)?;
        Ok(CurrentComparison {
            repo: self.repo,
            config,
            store,
            levels: refreshed.shard_levels,
        })
    }

    fn current_state(&self) -> Result<(StateStore, LockShardLevels)> {
        let current = self
            .current(&gat_core::progress::NoopProgress)
            .map_err(|source| {
                CompareError::new(CompareErrorKind::Acquisition(source.kind()), source)
            })?;
        Ok((current.store, current.levels))
    }

    /// Compares the staged desired snapshot with current desired state.
    pub fn staged_with_current(
        &self,
        selection: &Selection,
        unchanged: Unchanged,
    ) -> Result<Vec<ChangedRow>> {
        let reader = GitReader::open(self.repo.layout())
            .map_err(|source| CompareError::new(CompareErrorKind::Repository, source))?;
        let staged = reader.staged_lock_snapshot()?;
        let (store, current_levels) = self.current_state()?;
        compare_snapshot_with_current(&staged, &store, current_levels, selection, unchanged)
    }

    /// Compares two persisted desired snapshots.
    pub fn revisions(
        &self,
        from: &GitRevisionSpec,
        to: &GitRevisionSpec,
        selection: &Selection,
        unchanged: Unchanged,
    ) -> Result<Vec<ChangedRow>> {
        let reader = GitReader::open(self.repo.layout())
            .map_err(|source| CompareError::new(CompareErrorKind::Repository, source))?;
        let from = reader.lock_snapshot_at(from)?;
        let to = reader.lock_snapshot_at(to)?;
        compare_snapshots(&from, &to, selection, unchanged)
    }

    /// Compares a persisted desired snapshot with current desired state.
    pub fn revision_with_current(
        &self,
        from: &GitRevisionSpec,
        selection: &Selection,
        unchanged: Unchanged,
    ) -> Result<Vec<ChangedRow>> {
        let reader = GitReader::open(self.repo.layout())
            .map_err(|source| CompareError::new(CompareErrorKind::Repository, source))?;
        let from = reader.lock_snapshot_at(from)?;
        let (store, current_levels) = self.current_state()?;
        compare_snapshot_with_current(&from, &store, current_levels, selection, unchanged)
    }
}

fn compare_snapshot_with_current(
    from: &LockSnapshot,
    store: &StateStore,
    current_levels: LockShardLevels,
    selection: &Selection,
    unchanged: Unchanged,
) -> Result<Vec<ChangedRow>> {
    let mut out = Vec::new();
    if from.shard_levels().is_flat() {
        store.with_desired_rows(DesiredQuery::for_selection(selection), |mut rows| {
            let right = || Ok(rows.next()?.map(DesiredRow::into_entry));
            match from.shards().first() {
                Some(shard) => from.with_shard_rows_pull(shard, selection, |left| {
                    merge_ordered(left, right, unchanged, &mut out)
                }),
                None => merge_ordered(|| Ok(None), right, unchanged, &mut out),
            }
        })?;
        return Ok(out);
    }
    if from.shard_levels() == current_levels && !from.is_empty() {
        if selection.scope_path().is_none() {
            // Unscoped, sharded: SQLite has no lexical range to narrow
            // with, so storage would scan every current row regardless.
            // Rather than collecting that whole scan into a `BTreeMap`,
            // stream it ordered by each row's own stored shard id and
            // merge one shard-id group at a time -- bounded by one
            // shard, never by the whole current state.
            let residual = if selection.is_unrestricted() {
                None
            } else {
                Some(selection)
            };
            store.with_current_shard_groups(residual, |mut groups| {
                merge_persisted_shards_with_current_groups(
                    from,
                    selection,
                    unchanged,
                    || Ok(groups.next_group()?),
                    &mut out,
                )
            })?;
        } else {
            // Scoped: one indexed query already narrows to the candidate
            // current rows in scope, grouped here by the same shard id
            // its writer placed it under (`shard_id_for_path`) -- not one
            // query per persisted shard id, and never an unscoped
            // enumeration of every shard SQLite currently holds. The
            // result is bounded by the scope, so collecting it is fine.
            let mut current_by_shard: BTreeMap<LockShardId, Vec<DesiredRow>> = BTreeMap::new();
            store.with_desired_rows(
                DesiredQuery::for_selection(selection),
                |mut rows| -> Result<()> {
                    while let Some(row) = rows.next()? {
                        let shard_id = LockShardId::for_path(&row.path, current_levels);
                        current_by_shard.entry(shard_id).or_default().push(row);
                    }
                    Ok(())
                },
            )?;
            let mut current_iter = current_by_shard.into_iter();
            merge_persisted_shards_with_current_groups(
                from,
                selection,
                unchanged,
                || Ok(current_iter.next()),
                &mut out,
            )?;
        }
    } else {
        // Shapes differ (or nothing is persisted yet): the persisted side
        // is materialized as one ordered logical state -- the explicit
        // fallback -- while the current side still streams from SQLite
        // rather than being materialized alongside it.
        let left_rows = from.rows_sorted(selection)?;
        store.with_desired_rows(DesiredQuery::for_selection(selection), |mut rows| {
            merge_ordered(
                entry_source(left_rows),
                || Ok(rows.next()?.map(DesiredRow::into_entry)),
                unchanged,
                &mut out,
            )
        })?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::path_scope::normalize_path_scope;
    use std::error::Error as _;

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path).unwrap()
    }

    fn layout(root: &std::path::Path) -> gat_io::RepositoryLayout {
        gat_io::RepositoryLayout::at(root.to_path_buf())
    }

    fn scoped_selection(path: &std::path::Path) -> Selection {
        Selection::from_scope_patterns(normalize_path_scope(path).unwrap(), Vec::new(), Vec::new())
    }

    /// Derives a valid, distinct [`Oid`](gat_core::oid::Oid) from an
    /// arbitrary test token via BLAKE3, rather than requiring `token` to
    /// already be (a length-64-dividing repetition of) valid hex -- so
    /// tests can name fixture OIDs with short, readable, non-hex tokens
    /// (e.g. `"c-new"`) instead of hand-crafted hex strings.
    fn oid(token: &str) -> gat_core::oid::Oid {
        gat_core::oid::Oid::from_hex(blake3::hash(token.as_bytes()).to_hex().as_str()).unwrap()
    }

    fn entry(path: &str, token: &str) -> Entry {
        Entry {
            path: gp(path),
            oid: oid(token),
        }
    }

    fn track_paths(
        root: &std::path::Path,
        repo: &crate::repository::Repository,
        paths: &[std::path::PathBuf],
    ) {
        let mut lock = gat_io::LockStore::load_repository(repo.layout()).unwrap();
        for path in paths {
            let content = std::fs::read(root.join(path)).unwrap();
            let (ingested, _) = repo
                .resolved_cache_root()
                .unwrap()
                .writer()
                .ingest(content.as_slice())
                .unwrap();
            lock.upsert(gp(path.to_str().unwrap()), ingested.oid);
        }
        repo.save_lock(&lock).unwrap();
    }

    fn refreshed_desired_store(
        repo: &crate::repository::Repository,
    ) -> std::result::Result<StateStore, crate::workspace::sync::SyncError> {
        let mut store = StateStore::open(repo.layout())?;
        crate::workspace::sync::refresh_desired_index(repo, &mut store)?;
        Ok(store)
    }

    fn merge(left: Vec<Entry>, right: Vec<Entry>, unchanged: Unchanged) -> Vec<ChangedRow> {
        let mut out = Vec::new();
        merge_ordered(entry_source(left), entry_source(right), unchanged, &mut out).unwrap();
        out
    }

    fn shard(id: &str) -> gat_io::SnapshotShard {
        gat_io::SnapshotShard::for_test(LockShardId::parse_canonical(id).unwrap())
    }

    #[test]
    fn revision_failure_exposes_only_semantic_context_and_retains_technical_source() {
        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let revision = GitRevisionSpec::from("revision-that-does-not-exist");

        let err = repo
            .comparisons()
            .revisions(
                &revision,
                &GitRevisionSpec::from("HEAD"),
                &Selection::root(),
                Unchanged::Drop,
            )
            .unwrap_err();

        assert_eq!(
            err.kind(),
            &CompareErrorKind::Revision(revision),
            "callers should receive the unresolved semantic revision, not Git paths or errors"
        );
        assert!(
            err.source().is_some(),
            "the low-level Git failure must remain in the technical source chain"
        );
    }

    #[test]
    fn two_revision_comparison_opens_git_once() {
        use crate::test_harness::{commit_all, test_repo};
        use gat_core::lock::{Lock, LockShardLevels};

        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(root.clone());
        let mut lock = Lock::default();
        lock.upsert(gp("file.bin"), Oid::from_hex(&"1".repeat(64)).unwrap());
        gat_io::LockStore::publish_repository(repo.layout(), &lock, LockShardLevels::FLAT).unwrap();
        commit_all(&root, "first lock");
        lock.upsert(gp("file.bin"), Oid::from_hex(&"2".repeat(64)).unwrap());
        gat_io::LockStore::publish_repository(repo.layout(), &lock, LockShardLevels::FLAT).unwrap();
        commit_all(&root, "second lock");

        let before = gat_io::git_test_support::repository_opens();
        let rows = repo
            .comparisons()
            .revisions(
                &GitRevisionSpec::from("HEAD~1"),
                &GitRevisionSpec::from("HEAD"),
                &Selection::root(),
                Unchanged::Drop,
            )
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(gat_io::git_test_support::repository_opens() - before, 1);
    }

    /// Pairs every shard id present on either side, in the same order
    /// `.iter().find()` would have produced, but via two cursors over
    /// already-sorted input instead of a linear scan per id -- the fix for
    /// the reviewer-flagged `O(Q^2)` shard lookup in `compare_snapshots`.
    #[test]
    fn merge_shards_by_id_pairs_shared_ids_and_reports_one_sided_ids() {
        let left = vec![
            shard("gat.lock/aa.tsv"),
            shard("gat.lock/bb.tsv"),
            shard("gat.lock/dd.tsv"),
        ];
        let right = vec![
            shard("gat.lock/bb.tsv"),
            shard("gat.lock/cc.tsv"),
            shard("gat.lock/dd.tsv"),
        ];
        let ids: Vec<(Option<String>, Option<String>)> = merge_shards_by_id(&left, &right)
            .map(|pair| match pair {
                OrderedPair::Left(l) => (Some(l.id.to_canonical_string()), None),
                OrderedPair::Right(r) => (None, Some(r.id.to_canonical_string())),
                OrderedPair::Both(l, r) => (
                    Some(l.id.to_canonical_string()),
                    Some(r.id.to_canonical_string()),
                ),
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                (Some("gat.lock/aa.tsv".to_string()), None),
                (
                    Some("gat.lock/bb.tsv".to_string()),
                    Some("gat.lock/bb.tsv".to_string())
                ),
                (None, Some("gat.lock/cc.tsv".to_string())),
                (
                    Some("gat.lock/dd.tsv".to_string()),
                    Some("gat.lock/dd.tsv".to_string())
                ),
            ]
        );
    }

    #[test]
    fn merge_shards_by_id_handles_disjoint_and_empty_sides() {
        assert_eq!(merge_shards_by_id(&[], &[]).count(), 0);
        let left = vec![shard("gat.lock/aa.tsv"), shard("gat.lock/bb.tsv")];
        assert_eq!(merge_shards_by_id(&left, &[]).count(), 2);
        assert_eq!(merge_shards_by_id(&[], &left).count(), 2);
    }

    #[test]
    fn merge_classifies_added_removed_modified_and_unchanged() {
        let left = vec![
            entry("gone.bin", "a"),
            entry("same.bin", "b"),
            entry("changed.bin", "c"),
        ];
        let mut left_sorted = left;
        left_sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let right = vec![
            entry("changed.bin", "c2"),
            entry("new.bin", "d"),
            entry("same.bin", "b"),
        ];

        let changed = merge(left_sorted.clone(), right.clone(), Unchanged::Drop);
        assert_eq!(
            changed,
            vec![
                ChangedRow {
                    path: gp("changed.bin"),
                    change: RowChange::Modified { oid: oid("c2") }
                },
                ChangedRow {
                    path: gp("gone.bin"),
                    change: RowChange::Removed
                },
                ChangedRow {
                    path: gp("new.bin"),
                    change: RowChange::Added { oid: oid("d") }
                },
            ]
        );

        let all = merge(left_sorted, right, Unchanged::Keep);
        assert_eq!(all.len(), 4);
        assert!(all.iter().any(|row| row
            == &ChangedRow {
                path: gp("same.bin"),
                change: RowChange::Unchanged { oid: oid("b") }
            }));
        assert_eq!(all.iter().filter(|row| row.is_change()).count(), 3);
    }

    /// Ported from the previous `changed_lock_entries`/`all_lock_entries`
    /// tests: the shared engine must classify the same four cases the
    /// index-based traversal did, and emit changed paths in lexical
    /// order.
    #[test]
    fn changed_and_all_traversals_match_the_previous_classification() {
        let mut from = vec![
            entry("keep.bin", "a"),
            entry("removed.bin", "b"),
            entry("modified.bin", "c"),
        ];
        from.sort_by(|a, b| a.path.cmp(&b.path));
        let mut to = vec![
            entry("keep.bin", "a"),
            entry("modified.bin", "c-new"),
            entry("added.bin", "d"),
        ];
        to.sort_by(|a, b| a.path.cmp(&b.path));

        let changed = merge(from.clone(), to.clone(), Unchanged::Drop);
        assert_eq!(
            changed.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["added.bin", "modified.bin", "removed.bin"]
        );
        assert!(changed.iter().all(super::ChangedRow::is_change));

        let all = merge(from, to, Unchanged::Keep);
        assert_eq!(
            all.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec!["added.bin", "keep.bin", "modified.bin", "removed.bin"]
        );
        let by_path: std::collections::HashMap<&str, &RowChange> =
            all.iter().map(|r| (r.path.as_str(), &r.change)).collect();
        assert!(matches!(by_path["added.bin"], RowChange::Added { .. }));
        assert!(matches!(by_path["keep.bin"], RowChange::Unchanged { .. }));
        assert!(matches!(
            by_path["modified.bin"],
            RowChange::Modified { .. }
        ));
        assert!(matches!(by_path["removed.bin"], RowChange::Removed));
    }

    #[test]
    fn merged_output_is_lexically_ordered() {
        let from = vec![entry("z.bin", "a")];
        let mut to = vec![
            entry("b.bin", "b"),
            entry("a.bin", "c"),
            entry("m.bin", "d"),
        ];
        to.sort_by(|a, b| a.path.cmp(&b.path));
        let changed = merge(from, to, Unchanged::Drop);
        let paths: Vec<&str> = changed.iter().map(|r| r.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted);
        assert_eq!(paths, vec!["a.bin", "b.bin", "m.bin", "z.bin"]);
    }

    #[test]
    fn ordered_merge_preserves_failure_and_does_not_read_ahead() {
        let mut calls = 0;
        let mut right_calls = 0;
        let mut output = Vec::new();
        let error = merge_ordered(
            || {
                calls += 1;
                if calls == 1 {
                    Ok(Some(entry("a", "a")))
                } else {
                    Err(CompareError::new(
                        CompareErrorKind::GitObject,
                        std::io::Error::other("cursor failure"),
                    ))
                }
            },
            || {
                right_calls += 1;
                Ok(Some(entry("z", "z")))
            },
            Unchanged::Keep,
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error.kind(), &CompareErrorKind::GitObject);
        assert_eq!(calls, 2);
        assert_eq!(right_calls, 1);
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].path.as_str(), "a");
        assert_eq!(output[0].change, RowChange::Removed);
    }

    #[test]
    fn merge_handles_empty_sides() {
        assert!(merge(Vec::new(), Vec::new(), Unchanged::Keep).is_empty());
        assert_eq!(
            merge(vec![entry("a", "1")], Vec::new(), Unchanged::Drop),
            vec![ChangedRow {
                path: gp("a"),
                change: RowChange::Removed
            }]
        );
        assert_eq!(
            merge(Vec::new(), vec![entry("a", "1")], Unchanged::Drop),
            vec![ChangedRow {
                path: gp("a"),
                change: RowChange::Added { oid: oid("1") }
            }]
        );
    }

    #[test]
    fn unchanged_scoped_snapshots_skip_blobs_and_keep_mode_streams_selected_rows() {
        let tmp = crate::test_harness::test_repo();
        let layout = layout(tmp.path());
        let lock = gat_core::lock::Lock {
            entries: vec![
                entry("data/a", "a"),
                entry("data/b", "b"),
                entry("other", "c"),
            ],
        };
        for levels in [LockShardLevels::FLAT, LockShardLevels::new(1).unwrap()] {
            gat_io::LockStore::publish_repository(&layout, &lock, levels).unwrap();
            crate::test_harness::git(tmp.path(), &["add", "-A"]);
            let snapshot = LockSnapshot::staged(&layout).unwrap();
            for scope in ["data", "data/a", "absent"] {
                let selection = scoped_selection(std::path::Path::new(scope));
                let before = gat_io::git_lock_snapshot_test_support::shard_blob_reads();
                assert!(
                    compare_snapshots(&snapshot, &snapshot, &selection, Unchanged::Drop)
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(
                    gat_io::git_lock_snapshot_test_support::shard_blob_reads(),
                    before
                );
                let expected: Vec<_> = lock
                    .entries
                    .iter()
                    .filter(|entry| selection.matches(&entry.path))
                    .map(|entry| ChangedRow {
                        path: entry.path.clone(),
                        change: RowChange::Unchanged { oid: entry.oid },
                    })
                    .collect();
                assert_eq!(
                    compare_snapshots(&snapshot, &snapshot, &selection, Unchanged::Keep).unwrap(),
                    expected
                );
            }
        }
    }

    /// Structural coverage for current-vs-persisted access plans, not just
    /// output equivalence: an unscoped ("full")
    /// comparison must stream the current side through exactly one
    /// shard-grouped cursor, never through the bounded scoped query path
    /// (which would mean collecting the whole current state into a
    /// `BTreeMap` first), while a scoped comparison must do the opposite.
    #[test]
    fn full_and_scoped_current_comparison_use_disjoint_access_plans() {
        use crate::test_harness::{commit_all, git, test_repo};
        use std::path::PathBuf;

        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(root.clone());
        let mut config = repo
            .load_config_scoped(gat_core::config::ConfigScope::Project)
            .unwrap();
        config.lock.shard_levels = Some(gat_core::lock::LockShardLevels::new(2).unwrap());
        repo.save_config_scoped(&config, gat_core::config::ConfigScope::Project)
            .unwrap();
        let paths: Vec<PathBuf> = (0..12)
            .map(|i| {
                let rel = PathBuf::from(format!("data/f{i:03}.bin"));
                std::fs::create_dir_all(root.join("data")).unwrap();
                std::fs::write(root.join(&rel), format!("content-{i}")).unwrap();
                rel
            })
            .collect();
        track_paths(&root, &repo, &paths);
        commit_all(&root, "sharded baseline");
        git(&root, &["add", "-A"]);

        let gix_repo = root;
        let staged = LockSnapshot::staged(&layout(&gix_repo)).unwrap();
        assert!(!staged.shard_levels().is_flat());
        let store = refreshed_desired_store(&repo).unwrap();

        // Unscoped: must go through the shard-grouped streaming cursor
        // exactly once, never through the (bounded, scoped) desired-rows
        // query path.
        let before = gat_io::state_test_support::snapshot();
        compare_snapshot_with_current(
            &staged,
            &store,
            staged.shard_levels(),
            &Selection::root(),
            Unchanged::Keep,
        )
        .unwrap();
        let (_, desired_rows_calls, shard_group_calls, _, _) =
            gat_io::state_test_support::snapshot();
        assert_eq!(
            desired_rows_calls - before.1,
            0,
            "an unscoped comparison must never collect current rows through the bounded scoped path"
        );
        assert_eq!(
            shard_group_calls - before.2,
            1,
            "an unscoped comparison must open exactly one shard-grouped current cursor"
        );

        // Scoped: must go through the bounded desired-rows query path
        // exactly once, never through the full shard-grouped scan.
        let selection = scoped_selection(std::path::Path::new("data/f000.bin"));
        let before = gat_io::state_test_support::snapshot();
        compare_snapshot_with_current(
            &staged,
            &store,
            staged.shard_levels(),
            &selection,
            Unchanged::Keep,
        )
        .unwrap();
        let (_, desired_rows_calls, shard_group_calls, _, _) =
            gat_io::state_test_support::snapshot();
        assert_eq!(
            shard_group_calls - before.2,
            0,
            "a scoped comparison must never fall back to a full shard-grouped scan"
        );
        assert!(
            desired_rows_calls - before.1 >= 1,
            "a scoped comparison must read the current side through the bounded desired-rows path"
        );
    }

    /// A full (unscoped) comparison against a **flat** persisted snapshot
    /// must not take the sharded full-scan plan
    /// (`with_current_shard_groups`/`merge_persisted_shards_with_current_groups`)
    /// at all: the one persisted shard is the whole repository, so
    /// grouping by shard id buys nothing there, and treating it as a
    /// generic "shard" would still mean parsing/buffering the entire
    /// shard as a `Vec<Entry>` in one go. Assert both structural
    /// invariants for this case: exactly one shard
    /// blob is ever parsed, and the current side goes through the same
    /// `with_desired_rows` cursor a scoped read uses, never through
    /// `with_current_shard_groups`.
    #[test]
    fn full_flat_comparison_streams_instead_of_buffering_the_whole_shard() {
        use crate::test_harness::{commit_all, git, test_repo};
        use std::path::PathBuf;

        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(root.clone());
        let paths: Vec<PathBuf> = (0..12)
            .map(|i| {
                let rel = PathBuf::from(format!("f{i:03}.bin"));
                std::fs::write(root.join(&rel), format!("content-{i}")).unwrap();
                rel
            })
            .collect();
        track_paths(&root, &repo, &paths);
        commit_all(&root, "flat baseline");
        git(&root, &["add", "-A"]);

        let gix_repo = root;
        let staged = LockSnapshot::staged(&layout(&gix_repo)).unwrap();
        assert!(staged.shard_levels().is_flat());
        let store = refreshed_desired_store(&repo).unwrap();

        let shards_before = gat_io::git_lock_snapshot_test_support::shard_blob_reads();
        let store_before = gat_io::state_test_support::snapshot();
        let rows = compare_snapshot_with_current(
            &staged,
            &store,
            staged.shard_levels(),
            &Selection::root(),
            Unchanged::Keep,
        )
        .unwrap();
        assert_eq!(rows.len(), 12);
        assert_eq!(
            gat_io::git_lock_snapshot_test_support::shard_blob_reads() - shards_before,
            1,
            "a full flat comparison must parse the single persisted shard exactly once"
        );
        let (_, desired_rows_calls, shard_group_calls, _, _) =
            gat_io::state_test_support::snapshot();
        assert_eq!(
            shard_group_calls - store_before.2,
            0,
            "a full flat comparison must never open a shard-grouped current cursor"
        );
        assert!(
            desired_rows_calls - store_before.1 >= 1,
            "a full flat comparison must read the current side through the ordinary desired-rows cursor"
        );
    }

    /// The persisted-vs-persisted counterpart of the test above: a full
    /// (unscoped) comparison between two **flat** persisted snapshots
    /// must merge-walk both single shards directly
    /// ([`stream_shards`]) rather than collecting either whole
    /// shard into a `Vec<Entry>` first. Assert exactly one
    /// shard blob is parsed per side (two total), not more.
    #[test]
    fn full_flat_persisted_comparison_streams_both_shards_instead_of_buffering_them() {
        use crate::test_harness::{commit_all, test_repo};
        use std::path::PathBuf;

        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(root.clone());
        let paths: Vec<PathBuf> = (0..12)
            .map(|i| {
                let rel = PathBuf::from(format!("f{i:03}.bin"));
                std::fs::write(root.join(&rel), format!("content-{i}")).unwrap();
                rel
            })
            .collect();
        track_paths(&root, &repo, &paths);
        commit_all(&root, "flat rev1");

        std::fs::write(root.join("f000.bin"), "content-0-changed").unwrap();
        track_paths(&root, &repo, &[PathBuf::from("f000.bin")]);
        commit_all(&root, "flat rev2");

        let gix_repo = root;
        let rev1 = LockSnapshot::at_rev(
            &layout(&gix_repo),
            &gat_core::git::GitRevisionSpec::from("HEAD~1"),
        )
        .unwrap();
        let rev2 = LockSnapshot::at_rev(
            &layout(&gix_repo),
            &gat_core::git::GitRevisionSpec::from("HEAD"),
        )
        .unwrap();
        assert!(rev1.shard_levels().is_flat());
        assert!(rev2.shard_levels().is_flat());

        let shards_before = gat_io::git_lock_snapshot_test_support::shard_blob_reads();
        let rows = compare_snapshots(&rev1, &rev2, &Selection::root(), Unchanged::Drop).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "only the one changed path should be reported"
        );
        assert_eq!(
            gat_io::git_lock_snapshot_test_support::shard_blob_reads() - shards_before,
            2,
            "a full flat persisted comparison must parse exactly one shard blob per side"
        );
    }

    /// A `--path`-scoped comparison between a flat snapshot and a sharded
    /// one (cross-shape) must parse the flat side's single shard exactly
    /// once, without an eager read that is discarded before the cross-shape plan.
    #[test]
    fn cross_shape_scoped_comparison_parses_the_flat_side_only_once() {
        use crate::test_harness::{git, test_repo};
        use gat_core::lock::Lock;

        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let layout = gat_io::RepositoryLayout::at(root.clone());

        let mut flat_lock = Lock::default();
        flat_lock.upsert_many([
            Entry {
                path: gp("foo"),
                oid: Oid::from_hex(&format!("{:064x}", 1)).unwrap(),
            },
            Entry {
                path: gp("foo-sibling.bin"),
                oid: Oid::from_hex(&format!("{:064x}", 2)).unwrap(),
            },
        ]);
        gat_io::LockStore::publish_repository(
            &layout,
            &flat_lock,
            gat_core::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        git(&root, &["add", "-A"]);
        let flat = LockSnapshot::staged(&layout).unwrap();
        assert!(flat.shard_levels().is_flat());

        std::fs::remove_file(root.join("gat.lock")).unwrap();
        let mut sharded_lock = Lock::default();
        sharded_lock.upsert(gp("foo"), Oid::from_hex(&format!("{:064x}", 3)).unwrap());
        gat_io::LockStore::publish_repository(
            &layout,
            &sharded_lock,
            gat_core::lock::LockShardLevels::new(1).unwrap(),
        )
        .unwrap();
        git(&root, &["add", "-A"]);
        let sharded = LockSnapshot::staged(&layout).unwrap();
        assert_eq!(
            sharded.shard_levels(),
            gat_core::lock::LockShardLevels::new(1).unwrap()
        );
        assert_eq!(
            sharded.shards().len(),
            1,
            "the sharded fixture must have exactly one populated shard, so \
             the assertion below isolates the flat side's read count"
        );

        let selection = scoped_selection(std::path::Path::new("foo"));
        let shards_before = gat_io::git_lock_snapshot_test_support::shard_blob_reads();
        let rows = compare_snapshots(&flat, &sharded, &selection, Unchanged::Drop).unwrap();
        assert_eq!(rows.len(), 1, "foo's oid differs on each side");
        assert_eq!(
            gat_io::git_lock_snapshot_test_support::shard_blob_reads() - shards_before,
            2,
            "the flat side's single shard must be parsed exactly once (plus \
             the sharded side's one populated shard), not once for the \
             discarded exact-scope attempt and again in the cross-shape fallback"
        );
    }

    /// Both persisted comparisons and current-state refresh enforce path order.
    #[test]
    fn unsorted_flat_lock_is_rejected_by_comparison_and_refresh() {
        use crate::test_harness::{commit_all, test_repo};
        use gat_core::lock::VERSION;

        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(root.clone());

        // Deliberately out of order: "z.bin" before "a.bin".
        std::fs::write(
            root.join("gat.lock"),
            format!(
                "{VERSION}\n{0}\tz.bin\n{1}\ta.bin\n",
                "1".repeat(64),
                "2".repeat(64)
            ),
        )
        .unwrap();
        commit_all(&root, "unsorted flat lock");

        let gix_repo = root.clone();
        let committed = LockSnapshot::at_rev(
            &layout(&gix_repo),
            &gat_core::git::GitRevisionSpec::from("HEAD"),
        )
        .unwrap();
        assert!(committed.shard_levels().is_flat());

        // Persisted-vs-persisted: another (also unsorted) revision that
        // changes "z.bin"'s oid and leaves "a.bin" alone.
        std::fs::write(
            root.join("gat.lock"),
            format!(
                "{VERSION}\n{0}\tz.bin\n{1}\ta.bin\n",
                "3".repeat(64),
                "2".repeat(64)
            ),
        )
        .unwrap();
        commit_all(&root, "unsorted flat lock, z.bin changed");
        let changed = LockSnapshot::at_rev(
            &layout(&gix_repo),
            &gat_core::git::GitRevisionSpec::from("HEAD"),
        )
        .unwrap();

        assert!(
            compare_snapshots(&committed, &changed, &Selection::root(), Unchanged::Drop).is_err()
        );
        assert!(refreshed_desired_store(&repo).is_err());
    }
}

#[cfg(test)]
mod coherent_context_tests {
    use super::*;
    use gat_core::config::Config;
    use gat_core::progress::NoopProgress;

    #[test]
    fn current_context_keeps_configuration_and_rows_and_does_not_reload_for_cache() {
        let directory = crate::test_harness::git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(directory.path().to_path_buf());
        let mut original = Config::default();
        original.sync.auto_fetch = Some(true);
        repo.save_config(&original).unwrap();
        let entry = Entry {
            path: GatPath::parse_canonical("original.bin").unwrap(),
            oid: Oid::from_hex(&"a".repeat(64)).unwrap(),
        };
        repo.save_lock(&gat_core::lock::Lock {
            entries: vec![entry.clone()],
        })
        .unwrap();
        let before = crate::test_support::config_loads();
        let current = repo.comparisons().current(&NoopProgress).unwrap();
        assert_eq!(crate::test_support::config_loads() - before, 1);
        repo.save_config(&Config::default()).unwrap();
        repo.save_lock(&gat_core::lock::Lock::default()).unwrap();
        let mut writer = StateStore::open(repo.layout()).unwrap();
        crate::workspace::sync::refresh_desired_index(&repo, &mut writer).unwrap();
        assert_eq!(current.config(), &original);
        let rows = current
            .staged_with_current(&Selection::root(), Unchanged::Keep)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, entry.path);
        let before = crate::test_support::config_loads();
        assert!(!current.cache_presence().contains(&entry.oid));
        assert_eq!(crate::test_support::config_loads(), before);
    }
}
