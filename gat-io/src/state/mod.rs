//! SQLite-backed persistence for materialized state and reconciliation
//! (`.gat/state/state.sqlite3`). `StateStore` is the only thing in
//! the crate that knows SQL; every other module still deals in
//! [`Entry`]/[`Lock`], the same semantic model `gat.lock` uses -- this
//! module only changes how that data is stored and mutated, never what it
//! means.
//!
//! ## Schema and versioning
//!
//! ```sql
//! CREATE TABLE state (
//!     path              TEXT PRIMARY KEY,
//!
//!     desired_oid       BLOB,
//!     desired_shard_id  TEXT,
//!
//!     materialized_oid   BLOB,
//!     materialized_proof BLOB,
//!
//!     dirty INTEGER GENERATED ALWAYS AS (
//!         CASE
//!           WHEN desired_oid IS NULL AND materialized_oid IS NULL THEN 0
//!           WHEN desired_oid IS NULL OR materialized_oid IS NULL THEN 1
//!           WHEN desired_oid != materialized_oid THEN 1
//!           ELSE 0
//!         END
//!     ) STORED
//! ) WITHOUT ROWID;
//! ```
//!
//! One row per path, not three independently synchronized tables: a
//! path's desired half (mirrored from `gat.lock` by
//! [`StateStore::refresh_desired_identity`]) and materialized half (the
//! ownership ledger `gat add`/`mv`/`rm --cached`/sync all write through)
//! live in the same row, and `dirty` is a `GENERATED ALWAYS ... STORED`
//! column recomputed by `SQLite` itself whenever either half changes.
//! There is no separate `dirty` table that anything could forget to
//! update or have to rebuild from scratch -- `SELECT ... WHERE dirty = 1`
//! (backed by the partial index below) is correct by construction, not by
//! a periodic full re-derivation. A row whose desired and materialized
//! halves are *both* `NULL` carries no information and is deleted rather
//! than kept around as a permanent tombstone.
//!
//! ```sql
//! CREATE INDEX state_dirty_path ON state(path) WHERE dirty = 1;
//! CREATE INDEX state_by_desired_shard ON state(desired_shard_id);
//! ```
//!
//! `WITHOUT ROWID` avoids maintaining a second, redundant B-tree purely to
//! alias `path`, which is already the natural, unique key every lookup
//! and range scan here uses. OIDs are stored as their raw 32-byte blake3
//! digest (see [`gat_core::oid::Oid`]) rather than 64-character hex
//! text, halving the row's dominant column. `PRAGMA user_version` records
//! `SCHEMA_VERSION`; an unrecognised (larger) version fails closed with
//! an actionable error instead of silently trusting or resetting
//! whatever a newer `gat` wrote there.
//!
//! ## Query/mutation shapes
//!
//! Every mutation is one short, explicit transaction with chunked,
//! set-based SQL (`INSERT ... VALUES (...), (...) ON CONFLICT DO UPDATE`,
//! `DELETE ... WHERE path IN (...)`), not a per-row loop of prepared
//! statement calls. Prefix operations (`remove_prefix`, `move_prefix`)
//! match a tracked path or anything nested under it using an explicit
//! lexical byte-range (`path >= 'dir/' AND path < 'dir0'`) rather than
//! `LIKE`/`GLOB`, so `%`, `_`, backslashes, and case are never
//! reinterpreted -- `SQLite`'s default `BINARY` collation on `TEXT` already
//! compares raw bytes the same way Rust's `str::starts_with` does, which
//! is exactly the lexical, case-sensitive scope matching
//! [`gat_core::lock::path_matches_scope`] implements in memory.
//!
//! Because desired/materialized state share one row, every materialized
//! mutation (`upsert_many`/`remove_exact`/`remove_prefix`/`move_prefix`)
//! and every desired-state mutation (`apply_shard_refresh`) updates
//! `dirty` as a side effect of the very same `UPDATE`/`INSERT` that
//! changes the half it touches, inside the very same transaction --
//! there is no separate "now go recompute dirty" pass or second commit
//! following it.

use crate::file_state::{StatProof, decode_stat_proof, encode_stat_proof};
#[cfg(test)]
use crate::lock::LockShardLevels;
use crate::lock::{LockShardId, shard_id_for_path};
use crate::repository_layout::RepositoryLayout;
use gat_core::lexical_path::GatPath;
use gat_core::lock::{CanonicalDesiredIdentity, Entry, Lock};
use gat_core::oid::Oid;
use rusqlite::{Connection, OpenFlags, params_from_iter};
use std::path::Path;

mod desired;
mod error;
mod maintenance;
mod materialized;
mod query;
mod reconciliation;
mod repository;
mod schema;

pub use self::desired::DesiredRemoval;
use self::error::{Result, StateResultExt};
pub use self::error::{StateSqlError, StateSqlErrorKind, StateStoreError};
pub use self::maintenance::{
    StateDatabaseHealth, StateDatabaseUnreadable, StateMaintenanceError, count_stale_sidecars,
    inspect_database, rebuild_atomically, remove_stale_sidecars,
};
#[cfg(any(test, feature = "test-support"))]
pub use self::maintenance::{
    create_stale_sidecar_for_test, current_schema_version_for_test, repair_temp_count_for_test,
    reset_database_for_test, set_schema_version_for_test,
};
use self::schema::{SCHEMA_VERSION, check_schema_version, configure};

#[cfg(any(test, feature = "test-support"))]
pub use self::query::test_support;
pub(crate) use self::query::{CandidateBound, DesiredRows};
pub use self::query::{DesiredPathExclusions, DesiredQuery};
#[cfg(test)]
use self::reconciliation::{ChangedShard, ShardIdentity};
pub use self::reconciliation::{DesiredRefresh, DesiredRefreshError, DirtyRow};
pub(crate) use self::reconciliation::{ExcludeRecord, RemovedShard, StoredShard};
pub use self::repository::{
    AddCandidateDiscoveryError, DesiredCandidateScope, DesiredMutationOpenError,
    DesiredMutationSession, DesiredPublicationError, DesiredStateOpenError, DesiredStateSession,
    MaterializationPreparationError, MountMutationSession, MountReplayResult,
    PreparedMaterialization,
};
#[cfg(any(test, feature = "test-support"))]
pub use self::repository::{load_materialized_for_test, record_materialized_for_test};

#[cfg(any(test, feature = "test-support"))]
pub fn shard_observation_for_test(
    store: &StateStore,
    shard_id: LockShardId,
) -> Result<Option<(crate::lock::ShardContentIdentity, bool)>> {
    Ok(store
        .shard_identity(shard_id)?
        .map(|stored| (stored.identity, stored.proof.is_some())))
}

#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub const fn materialized_row_has_proof_for_test(row: &MaterializedRow) -> bool {
    row.proof.is_some()
}

#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn materialized_row_proof_matches_path_for_test(row: &MaterializedRow, path: &Path) -> bool {
    let Some(proof) = row.proof.as_ref() else {
        return false;
    };
    crate::file_state::observe_regular_file_no_follow(path)
        .is_some_and(|fresh| fresh.matches(proof))
}

#[cfg(any(test, feature = "test-support"))]
pub fn shard_ids_for_test(store: &StateStore) -> Result<Vec<LockShardId>> {
    let mut ids = store
        .all_shard_identities()?
        .into_keys()
        .collect::<Vec<_>>();
    ids.sort_unstable();
    Ok(ids)
}

#[cfg(any(test, feature = "test-support"))]
pub fn record_materialized_unlocked_for_test(
    layout: &crate::repository_layout::RepositoryLayout,
    entries: &[Entry],
) -> Result<()> {
    let rows = entries
        .iter()
        .cloned()
        .map(|entry| {
            let proof = crate::worktree::observe_regular_file(layout.root_path(), &entry.path);
            MaterializedRow::from_entry(entry, proof)
        })
        .collect::<Vec<_>>();
    StateStore::open(layout)?.upsert_rows(&rows)
}

/// Total bound-variable budget a single chunked statement should stay
/// under -- comfortably below `SQLite`'s default
/// `SQLITE_LIMIT_VARIABLE_NUMBER` (32766 as of `SQLite` 3.32), with margin
/// for any statement-level params (e.g. an optional `shard_id` filter)
/// added on top of the per-row binds. This is purely a transport/bind
/// budget, not an algorithmic crossover threshold -- code that decides
/// between a sparse and a bulk *plan* (e.g.
/// `SPARSE_DIRECTORY_CONFLICT_PATHS`) must pick its own threshold rather
/// than reuse this value.
const SQL_BIND_BUDGET: usize = 30_000;

/// Row-chunk size for a chunked multi-row statement that binds
/// `binds_per_row` variables per row, derived from [`SQL_BIND_BUDGET`]
/// rather than fixed: a single-column `IN (...)` chunk (1 bind/row) can
/// batch far more rows per statement than a 3-column upsert (3
/// binds/row) before hitting the same bind-variable ceiling.
const fn sql_chunk_size(binds_per_row: usize) -> usize {
    SQL_BIND_BUDGET / binds_per_row
}

/// Row-chunk size for a chunked multi-row statement that binds
/// `binds_per_row` variables per row *plus* `shared_binds` variables
/// bound once for the whole statement (e.g. a single shard ID reused
/// via one numbered parameter across every row) -- the shared binds are
/// subtracted from the budget once, up front, rather than counted
/// `chunk.len()` times the way [`sql_chunk_size`] would if the shared
/// value were still bound per row.
const fn sql_chunk_size_with_shared_binds(binds_per_row: usize, shared_binds: usize) -> usize {
    (SQL_BIND_BUDGET - shared_binds) / binds_per_row
}

/// State-local stack buffer for binding canonical shard IDs to `SQLite`.
///
/// The representation is deliberately owned by this I/O boundary rather than
/// exposed by `gat-core`; callers only need `LockShardId::write_canonical`.
struct CanonicalShardIdBuf {
    bytes: [u8; 32],
    len: usize,
}

impl CanonicalShardIdBuf {
    fn encode(shard_id: LockShardId) -> Self {
        let mut buffer = Self {
            bytes: [0; 32],
            len: 0,
        };
        shard_id
            .write_canonical(&mut buffer)
            .expect("canonical shard id always fits in the state buffer");
        buffer
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).expect("canonical shard id is always ASCII")
    }
}

impl std::fmt::Write for CanonicalShardIdBuf {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let end = self.len + text.len();
        if end > self.bytes.len() {
            return Err(std::fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// How long a connection waits for another process's write lock before
/// giving up, so a busy database surfaces a bounded, actionable timeout
/// consistent with the repository's other persistence capabilities.
const BUSY_TIMEOUT_MS: u32 = 10_000;

/// A short-lived connection to `.gat/state/state.sqlite3`, opened at the
/// boundary of whichever command operation needs materialized state
/// (never held across a whole process's lifetime)
/// with the pragmas this CLI-shaped workload wants applied on every open.
#[derive(Debug)]
pub struct StateStore {
    conn: Connection,
    /// `reconciliation_meta.validation_required`, loaded once when this
    /// store is opened and kept in sync by [`Self::set_validation_required`]
    /// so callers on the normal sync path (`engine::workspace::sync::sync_from_snapshot`)
    /// consult an already-in-memory value instead of an extra query or
    /// filesystem probe every sync -- see [`schema::SCHEMA_VERSION`]'s `8`
    /// bump.
    validation_required: bool,
}

/// One already-succeeded filesystem mutation pending persistence via
/// [`StateStore::apply_batch`]. Kept as an ordered enum (rather
/// than separate upsert/remove lists) so a batch can preserve the exact
/// order actions were applied in -- e.g. a file-to-directory transition
/// that removes an ancestor path before materializing a descendant must
/// not have its removal re-applied after the descendant's upsert.
#[derive(Clone)]
pub struct StateMutation(StateMutationKind);

#[derive(Clone)]
enum StateMutationKind {
    Upsert(MaterializedRow),
    RefreshStat { path: GatPath, proof: StatProof },
    RemoveExact(GatPath),
}

impl StateMutation {
    pub(crate) const fn upsert(row: MaterializedRow) -> Self {
        Self(StateMutationKind::Upsert(row))
    }

    pub(crate) const fn refresh_stat(path: GatPath, proof: StatProof) -> Self {
        Self(StateMutationKind::RefreshStat { path, proof })
    }

    #[must_use]
    pub const fn remove_exact(path: GatPath) -> Self {
        Self(StateMutationKind::RemoveExact(path))
    }
}

impl std::fmt::Debug for StateMutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.0 {
            StateMutationKind::Upsert(_) => "upsert",
            StateMutationKind::RefreshStat { .. } => "refresh-stat",
            StateMutationKind::RemoveExact(_) => "remove-exact",
        };
        formatter
            .debug_struct("StateMutation")
            .field("kind", &kind)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct MaterializedRow {
    path: GatPath,
    oid: Oid,
    /// The optional stat proof (shared [`StatProof`] codec) that lets a
    /// later sync trust this path's materialized OID from a stat alone,
    /// or `None` when no reusable proof was recorded (a row whose file
    /// Gat cannot stat-trust).
    proof: Option<StatProof>,
}

impl MaterializedRow {
    #[must_use]
    pub const fn path(&self) -> &GatPath {
        &self.path
    }

    #[must_use]
    pub const fn oid(&self) -> Oid {
        self.oid
    }

    pub(crate) const fn proof(&self) -> Option<&StatProof> {
        self.proof.as_ref()
    }

    #[must_use]
    pub fn into_entry(self) -> Entry {
        Entry {
            path: self.path,
            oid: self.oid,
        }
    }

    pub(crate) fn from_entry(entry: Entry, proof: Option<StatProof>) -> Self {
        Self {
            path: entry.path,
            oid: entry.oid,
            proof,
        }
    }
}

impl std::fmt::Debug for MaterializedRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MaterializedRow")
            .field("path", &self.path)
            .field("oid", &self.oid)
            .finish_non_exhaustive()
    }
}

/// Decode one materialized-state row, validating the stored path/OID
/// rather than trusting whatever bytes are in the database -- a corrupted
/// or hand-edited row must fail loudly, not silently materialize/delete
/// the wrong content. A malformed or unknown `proof` BLOB
/// decodes as *absent* proof (never a fatal error), so gat simply falls
/// back to re-establishing identity by hash next time -- see
/// [`crate::file_state::decode_stat_proof`].
fn decode_row_raw(path: String, oid: Vec<u8>, proof: Option<Vec<u8>>) -> Result<MaterializedRow> {
    let path = decode_path(path, "materialized-state row")?;
    let oid: [u8; 32] = oid
        .as_slice()
        .try_into()
        .map_err(|_| StateStoreError::InvalidRow {
            detail: format!(
                "materialized-state row {path:?}: invalid oid length {} (expected 32 bytes)",
                oid.len()
            ),
        })?;
    let proof = proof.as_deref().and_then(decode_stat_proof);
    Ok(MaterializedRow {
        path,
        oid: Oid::from_bytes(oid),
        proof,
    })
}

/// Decode one `SQLite` `TEXT` shard-ID column, wrapping the shared
/// [`LockShardId::parse_canonical`] decoder in the state-store's own
/// `InvalidRow` failure mode so every desired/reconciliation/query
/// call site reports a malformed shard ID the same way instead of
/// hand-rolling its own `map_err`. `context` names the column/row kind
/// being decoded (e.g. `"desired_shard_id"`, `"stored shard id"`) for
/// the resulting error detail.
fn decode_shard_id(raw: &str, context: &str) -> Result<LockShardId> {
    LockShardId::parse_canonical(raw).map_err(|source| StateStoreError::InvalidRow {
        detail: format!("decoding {context}: {source}"),
    })
}

/// Decode one `SQLite` `TEXT` path column into a strictly-validated
/// [`GatPath`], wrapping [`GatPath::from_canonical_string`] in the
/// state-store's own `InvalidRow` failure mode so every call site
/// reports a malformed path the same way instead of hand-rolling its
/// own `map_err` (mirrors [`decode_shard_id`]'s role for shard IDs).
/// `context` names the column/row kind being decoded (e.g.
/// `"materialized-state row"`, `"directory-conflict row"`) for the
/// resulting error detail. Takes ownership of the SQLite-provided
/// `String` rather than borrowing, since every caller already holds an
/// owned value fresh out of `rusqlite`.
pub(crate) fn decode_path(raw: String, context: &str) -> Result<GatPath> {
    GatPath::from_canonical_string(raw).map_err(|source| StateStoreError::InvalidRow {
        detail: format!("{context} has invalid path: {source}"),
    })
}

fn decode_desired_row_raw(path: String, oid: Vec<u8>) -> Result<DesiredRow> {
    let path = decode_path(path, "desired-state row")?;
    let oid_arr: [u8; 32] = oid
        .as_slice()
        .try_into()
        .map_err(|_| StateStoreError::InvalidRow {
            detail: format!(
                "desired-state row {path:?}: invalid oid length {} (expected 32 bytes)",
                oid.len()
            ),
        })?;
    let oid = Oid::from_bytes(oid_arr);
    Ok(DesiredRow { path, oid })
}

fn load_materialized_rows<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<MaterializedRow>> {
    let mut stmt = conn
        .prepare(sql)
        .state_context("preparing materialized-state load query")?;
    let rows = stmt
        .query_map(params, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })
        .state_context("loading materialized state")?;
    let mut entries = Vec::new();
    for row in rows {
        let (path, oid, proof) = row.state_context("reading materialized-state row")?;
        entries.push(decode_row_raw(path, oid, proof)?);
    }
    Ok(entries)
}

/// A `path`-ordered cursor over materialized-state rows, produced by
/// [`StateStore::with_rows_in_scope`]. Borrows the
/// `rusqlite::Statement`/`Rows` that back it, so it (and any closure
/// holding it) cannot outlive the call that created it -- the planner
/// consumes rows one at a time through [`Self::next`] instead of requiring
/// a fully materialized `Vec<MaterializedRow>` up front.
pub struct MaterializedRows<'a> {
    rows: rusqlite::Rows<'a>,
}

/// A `path`-ordered cursor over exactly the desired-state `path` column,
/// produced by [`StateStore::with_desired_paths`]. Unlike
/// [`StateStore::load_desired_as_lock`], this never decodes a
/// `desired_oid` into an [`Entry`] -- callers that only
/// need ordered desired paths (e.g. `.git/info/exclude` regeneration in
/// `gat-engine`) stream them one at a time
/// instead of materializing a full `Vec<Entry>`/[`Lock`] just to throw the
/// oid back away.
pub struct DesiredPaths<'a> {
    rows: rusqlite::Rows<'a>,
}

/// One decoded desired-state row: a canonical path and a native [`Oid`].
/// [`Self::into_entry`] moves both fields into the lock interchange type
/// without allocating or converting the OID to hexadecimal text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRow {
    pub path: GatPath,
    pub oid: Oid,
}

impl DesiredRow {
    /// Converts to the [`Entry`] interchange form, only where one is
    /// actually needed (e.g. writing `gat.lock`, comparing against
    /// another `Entry`-shaped source).
    #[must_use]
    pub fn into_entry(self) -> Entry {
        Entry {
            path: self.path,
            oid: self.oid,
        }
    }
}

impl DesiredPaths<'_> {
    /// The next desired path in lexical order, or `None` once exhausted.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<GatPath>> {
        match self
            .rows
            .next()
            .state_context("reading desired-state row")?
        {
            None => Ok(None),
            Some(row) => {
                let raw: String = row.get(0).state_context("reading desired-state row")?;
                let path = decode_path(raw, "desired-state row")?;
                Ok(Some(path))
            }
        }
    }
}

/// One explicit desired-state write transaction, used by sharded `gat rm` and
/// `gat mv` to keep `SQLite`'s desired mirror and the touched on-disk shard files
/// in lockstep: mutate/query the desired rows, publish exactly the touched
/// shard files from that post-mutation view, then commit the mirror/catalog
/// only if the filesystem publish succeeded.
pub(crate) struct DesiredStateWrite<'a> {
    tx: rusqlite::Transaction<'a>,
}

impl MaterializedRows<'_> {
    /// Decode and return the next row in `path` order, or `None` once the
    /// cursor is exhausted. A malformed stored row (bad OID length/size)
    /// fails loudly with the same context [`decode_row_raw`] always gives,
    /// exactly as it would if every row had first been collected into a
    /// `Vec`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<MaterializedRow>> {
        match self
            .rows
            .next()
            .state_context("reading materialized-state row")?
        {
            None => Ok(None),
            Some(row) => {
                let path: String = row.get(0).state_context("reading materialized-state row")?;
                let oid: Vec<u8> = row.get(1).state_context("reading materialized-state row")?;
                let proof: Option<Vec<u8>> =
                    row.get(2).state_context("reading materialized-state row")?;
                Ok(Some(decode_row_raw(path, oid, proof)?))
            }
        }
    }
}

/// The half-open byte range `[path/, path0)` that contains exactly
/// every row nested under `path` (`path/...`), excluding `path` itself,
/// expressed as a lexical range instead of a wildcard pattern so `%`,
/// `_`, backslashes, casing, and Unicode in a real path are never
/// reinterpreted as pattern syntax. `/` (0x2F) is immediately followed
/// by `0` (0x30) in byte order, so `path + "0"` is the exclusive upper
/// bound of everything starting with `path + "/"`.
fn descendant_range(path: &str) -> (String, String) {
    (format!("{path}/"), format!("{path}0"))
}

/// `n` copies of `unit` (e.g. `"?"` or `"(?, ?, ?)"`), comma-joined, for a
/// chunked multi-row `IN (...)`/`VALUES (...), (...)` statement -- shared
/// by every bulk read/write below instead of each repeating
/// `vec![unit; n].join(", ")` inline.
fn sql_placeholders(unit: &str, n: usize) -> String {
    vec![unit; n].join(", ")
}

impl StateStore {
    /// Observe the canonical identity of the complete live desired state,
    /// consulting an existing state database only as a best-effort stat
    /// accelerator. Missing, unreadable, stale, or malformed accelerator
    /// state falls back to authoritative lock bytes and never creates or
    /// mutates the database.
    ///
    /// This is the sole boundary coordinating the state catalog with live
    /// lock observation: callers receive only the semantic identity and
    /// cannot access prior shard rows or stat proofs.
    pub fn observe_canonical_identity(
        repo: &RepositoryLayout,
    ) -> crate::lock::Result<CanonicalDesiredIdentity> {
        let prior = Self::open_if_exists(repo)
            .ok()
            .flatten()
            .and_then(|store| store.all_shard_identities().ok());
        match prior {
            Some(prior) => {
                crate::lock::current_desired_identity_with_prior(repo.root_path(), |shard_id| {
                    let stored = prior.get(&shard_id)?;
                    Some((stored.identity, stored.proof))
                })
            }
            None => crate::lock::current_desired_identity(repo.root_path()),
        }
    }

    /// Open (creating it if it doesn't exist yet) the materialized-state
    /// database for `repo`. Short-lived: callers open one of these at the
    /// start of an operation and let it drop at the end, rather than
    /// holding a connection across a whole process's lifetime.
    pub fn open(repo: &RepositoryLayout) -> Result<Self> {
        Self::open_at(&repo.materialized_db_path())
    }

    /// Open (creating it if it doesn't exist yet) the materialized-state
    /// database at an explicit path rather than one derived from a
    /// [`Repo`]. Used by `gat system repair state`'s atomic rebuild: it
    /// builds and validates a complete replacement database at a
    /// same-directory sibling temp path -- never touching the live
    /// database until that replacement is fully built -- then publishes it
    /// with `crate::atomic::persist_with_retry`.
    pub(super) fn open_at(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| {
                StateStoreError::DirectoryUnavailable {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
        }
        let conn = Connection::open(db_path).map_err(|source| StateStoreError::OpenFailed {
            path: db_path.to_path_buf(),
            source: StateSqlError::from_sqlite(source),
        })?;
        configure(&conn)?;
        check_schema_version(&conn, db_path)?;
        let validation_required = read_validation_required(&conn)?;
        Ok(Self {
            conn,
            validation_required,
        })
    }

    /// Begin a deferred read transaction on this connection and
    /// immediately touch the `state` table, pinning the WAL snapshot this
    /// connection sees to exactly the commit that existed at the moment
    /// this call returns -- every later read through `self` (including
    /// [`Self::desired_rows`]/[`Self::with_desired_rows`] called much
    /// later) observes that same generation, never a newer one a
    /// concurrent mount mutation commits afterward.
    ///
    /// This prevents a
    /// plain (non-transactional) connection has no snapshot at all --
    /// each individual `SELECT` sees whatever is newest at the moment it
    /// runs, so a `store` captured alongside an older `config` could still
    /// read newer desired rows if a mount mutation committed in between
    /// capture and read. Pinning here, while the caller still holds the
    /// `RepoLock` that guarantees no mutation is concurrently committing
    /// (the engine's coherent snapshot acquisition barrier does this),
    /// makes `config` and every later read from this store provably the
    /// same repository generation for the rest of this store's lifetime.
    ///
    /// Once pinned, `self` must only be used for reads: a write would try
    /// to upgrade this held read transaction, and `SQLite` fails such a
    /// write with `SQLITE_BUSY_SNAPSHOT` as soon as any concurrent commit
    /// has landed since the snapshot was taken. Dropping `self` without an
    /// explicit `COMMIT`/`ROLLBACK` is safe -- `SQLite` implicitly rolls the
    /// open (read-only, so no-op) transaction back when the connection
    /// closes.
    pub(crate) fn release_snapshot(&self) -> Result<()> {
        self.conn
            .execute_batch("ROLLBACK")
            .state_context("releasing desired-state snapshot")
    }

    pub fn pin_snapshot(&self) -> Result<()> {
        self.conn
            .execute_batch("BEGIN DEFERRED")
            .state_context("beginning a pinned desired-state snapshot")?;
        // Any read against the schema pins the snapshot at the WAL frame
        // current right now -- an empty `state` table still pins it, since
        // the pin happens when the b-tree cursor opens, not when a row is
        // actually returned.
        self.conn
            .prepare("SELECT 1 FROM state LIMIT 1")
            .and_then(|mut stmt| stmt.exists([]))
            .state_context("pinning the desired-state snapshot")?;
        Ok(())
    }

    /// Open the materialized-state database for `repo` if it already
    /// exists on disk *and* has current, readable state, returning `None`
    /// rather than creating it (see [`Self::open`]) when it doesn't. For
    /// strictly read-only callers such as `gat sync --dry-run`:
    /// a repo that has never been synced yet -- or whose `.gat/state`
    /// directory was removed -- has no materialized state at all, which is
    /// exactly what `None` here means to callers (an absent database is
    /// empty, not an error, and must never be created as a side effect of
    /// merely planning). A schema version older than `SCHEMA_VERSION` is
    /// also reported as `None`: `check_schema_version` would discard and
    /// rebuild it from scratch on a normal (writable) open anyway, so an
    /// outdated database is likewise equivalent to an empty one here,
    /// without this read-only path needing write access to rebuild it. A
    /// schema newer than this build understands still fails closed, the
    /// same as [`Self::open`]. Opens the connection read-only, so even a
    /// bug that tried to write through it would fail loudly instead of
    /// silently mutating disk state from a read path.
    pub fn open_if_exists(repo: &RepositoryLayout) -> Result<Option<Self>> {
        let db_path = repo.materialized_db_path();
        if !db_path.is_file() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|source| StateStoreError::OpenFailed {
                path: db_path.clone(),
                source: StateSqlError::from_sqlite(source),
            })?;
        configure(&conn)?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .state_context("reading materialized-state schema version")?;
        if version > SCHEMA_VERSION {
            return Err(StateStoreError::UnsupportedSchemaVersion {
                path: db_path,
                found: version,
                expected: SCHEMA_VERSION,
            });
        }
        if version < SCHEMA_VERSION {
            return Ok(None);
        }
        let validation_required = read_validation_required(&conn)?;
        Ok(Some(Self {
            conn,
            validation_required,
        }))
    }

    /// Whether `gat system repair state` destructively rebuilt this
    /// database's materialized ledger without being able to prove the
    /// freshly reset (empty) ledger still reflects the working tree --
    /// see [`Self::set_validation_required`]. Cached at open
    /// time (see the field doc), so reading this never touches `SQLite`.
    pub const fn validation_required(&self) -> bool {
        self.validation_required
    }

    /// Persist (and update the cached copy of) `validation_required`.
    /// `gat system repair state` sets this to `true` in the same publish
    /// as a destructive materialized-ledger rebuild; a full, unrestricted,
    /// clean `Validation::Validate` sync sets it back to `false` once it
    /// has re-established trust (`engine::workspace::sync::sync_from_snapshot`).
    pub fn set_validation_required(&mut self, value: bool) -> Result<()> {
        self.conn
            .execute(
                "UPDATE reconciliation_meta SET validation_required = ?1 WHERE id = 1",
                [i64::from(value)],
            )
            .state_context("updating materialized-state validation-required flag")?;
        self.validation_required = value;
        Ok(())
    }

    /// Fold the WAL back into the main database file and truncate it, so
    /// the temp path a rebuild was staged at is a single, self-contained
    /// file before `crate::atomic::persist_with_retry` publishes it --
    /// the live path must never end up paired with a stale/foreign `-wal`
    /// sidecar left over from the replacement's construction.
    pub fn checkpoint_and_truncate_wal(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .state_context("checkpointing rebuilt materialized-state database")
    }

    /// Test-only: override this connection's `busy_timeout`. Production
    /// code never needs this (the default is always appropriate); tests
    /// that intentionally attempt a checkpoint they *expect* to be
    /// blocked by a pinned reader use this to make `SQLite` fail fast
    /// instead of busy-waiting out the full (production, 10s) default.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_busy_timeout_ms_for_test(&self, ms: u32) -> Result<()> {
        self.conn
            .pragma_update(None, "busy_timeout", ms)
            .state_context("setting test busy_timeout")
    }
}

/// Read `reconciliation_meta.validation_required`, defaulting to `false`
/// when the table is empty (a brand-new database whose schema was just
/// created has its single row inserted with this column's `DEFAULT 0`, so
/// this only differs in tests that poke at the schema directly).
fn read_validation_required(conn: &Connection) -> Result<bool> {
    let value: i64 = conn
        .query_row(
            "SELECT validation_required FROM reconciliation_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .state_context("reading materialized-state validation-required flag")?;
    Ok(value != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_layout::RepositoryLayout as Repo;
    use gat_core::lock::path_matches_scope;

    fn git_repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn gp(path: &str) -> gat_core::lexical_path::GatPath {
        gat_core::lexical_path::GatPath::parse_canonical(path.trim_end_matches('/')).unwrap()
    }

    /// Parse a canonical shard-id literal (`"gat.lock"` or
    /// `"gat.lock/xx/.../yy.tsv"`) used throughout these fixtures.
    fn sid(raw: &str) -> LockShardId {
        LockShardId::parse_canonical(raw).unwrap()
    }

    /// A depth-2 [`LockShardId`] derived deterministically from `i`,
    /// distinct for every distinct `i` in `0..65536` -- for fixtures that
    /// just need many unique shard ids (e.g. large-N refresh tests) and
    /// don't care about the id's relationship to any real path.
    fn nsid(i: u32) -> LockShardId {
        debug_assert!(i < 1 << 16, "nsid only covers 0..65536 at depth 2");
        let b = u16::try_from(i).unwrap().to_be_bytes();
        sid(&format!("gat.lock/{:02x}/{:02x}.tsv", b[0], b[1]))
    }

    fn entry(path: &str, byte: u8, _size: u64) -> Entry {
        Entry {
            path: gp(path),
            oid: Oid::from_bytes([byte; 32]),
        }
    }

    fn stat_proof(size: u64, mtime_secs: i64) -> StatProof {
        StatProof {
            size,
            mtime_secs,
            mtime_nanos: 0,
        }
    }

    fn load_scope_paths(store: &StateStore, scope: &str) -> Vec<String> {
        store
            .load_scope(scope)
            .unwrap()
            .into_iter()
            .map(|entry| entry.path.to_string())
            .collect()
    }

    fn seed_desired_shard(
        store: &mut StateStore,
        shard_id: LockShardId,
        byte: u8,
        entries: Vec<Entry>,
    ) {
        let proof = Some(crate::file_state::StatProof {
            size: entries.len() as u64,
            mtime_secs: i64::from(byte),
            mtime_nanos: 0,
        });
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id,
                    prior_identity: None,
                    identity: ShardIdentity::from_array([byte; 32]),
                    proof,
                    entries,
                }],
                &[],
                &[],
            )
            .unwrap();
    }

    fn seed_published_desired(
        repo: &Repo,
        store: &mut StateStore,
        entries: Vec<Entry>,
        levels: LockShardLevels,
    ) {
        let mut lock = Lock::default();
        lock.upsert_many(entries);
        let evidence =
            crate::lock::LockStore::publish_complete_with_evidence(repo.root_path(), &lock, levels)
                .unwrap();
        store.apply_full_lock_evidence(evidence).unwrap();
    }

    #[derive(Debug, thiserror::Error)]
    enum DesiredPublishTestError {
        #[error(transparent)]
        State(#[from] StateStoreError),
        #[error(transparent)]
        Lock(#[from] crate::lock::LockError),
    }

    #[test]
    fn opening_a_fresh_repo_creates_an_empty_database() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let store = StateStore::open(&repo).unwrap();
        assert!(store.load_all().unwrap().entries.is_empty());
        assert!(repo.materialized_db_path().exists());
    }

    #[test]
    fn opening_a_fresh_repo_enables_wal_journal_mode() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let store = StateStore::open(&repo).unwrap();
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn upsert_then_load_round_trips_one_row() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let e = entry("a.bin", 0xab, 5);
        store.upsert_many(std::slice::from_ref(&e)).unwrap();
        assert_eq!(
            store.load_all().unwrap().entries,
            vec![entry("a.bin", 0xab, 0)]
        );
    }

    #[test]
    fn upsert_many_handles_a_large_batch_across_chunk_boundaries() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let entries: Vec<Entry> = (0..(sql_chunk_size(3) * 2 + 7))
            .map(|i| {
                entry(
                    &format!("f{i:06}.bin"),
                    (i % 256).to_le_bytes()[0],
                    i as u64,
                )
            })
            .collect();
        store.upsert_many(&entries).unwrap();
        let loaded = store.load_all().unwrap().entries;
        assert_eq!(loaded.len(), entries.len());
    }

    #[test]
    fn upsert_updates_an_existing_row() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store.upsert_many(&[entry("a.bin", 1, 1)]).unwrap();
        store.upsert_many(&[entry("a.bin", 2, 2)]).unwrap();
        let loaded = store.load_all().unwrap().entries;
        assert_eq!(loaded, vec![entry("a.bin", 2, 0)]);
    }

    #[test]
    fn apply_batch_refresh_stat_updates_large_runs_without_touching_desired_only_rows() {
        for count in [100usize, 1_000, 10_000] {
            let tmp = git_repo();
            let repo = Repo::at(tmp.path().to_path_buf());
            let mut store = StateStore::open(&repo).unwrap();
            let materialized_entries: Vec<Entry> = (0..count)
                .map(|i| {
                    entry(
                        &format!("m{i:06}.bin"),
                        (i % 256).to_le_bytes()[0],
                        i as u64,
                    )
                })
                .collect();
            store.upsert_many(&materialized_entries).unwrap();
            seed_desired_shard(
                &mut store,
                sid("gat.lock"),
                9,
                vec![entry("desired-only.bin", 3, 3)],
            );

            let ops: Vec<StateMutation> = materialized_entries
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    StateMutation::refresh_stat(
                        entry.path.clone(),
                        stat_proof(i as u64 + 10, i as i64 + 100),
                    )
                })
                .collect();
            store.apply_batch(&ops).unwrap();

            let refreshed = store.load_all_raw().unwrap();
            assert_eq!(refreshed.len(), materialized_entries.len());
            for (i, row) in refreshed.iter().enumerate() {
                assert_eq!(row.proof, Some(stat_proof(i as u64 + 10, i as i64 + 100)));
            }
            let desired_only_proof: Option<Vec<u8>> = store
                .conn
                .query_row(
                    "SELECT materialized_proof FROM state WHERE path = 'desired-only.bin'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(desired_only_proof.is_none(), "count={count}");
        }
    }

    #[test]
    fn load_scope_returns_an_exact_file_match_only() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("data.bin", 1, 1),
                entry("data.bin/nested", 2, 2),
                entry("data.bin.bak", 3, 3),
            ])
            .unwrap();

        assert_eq!(
            load_scope_paths(&store, "data.bin"),
            vec!["data.bin".to_string(), "data.bin/nested".to_string()]
        );
    }

    #[test]
    fn load_scope_returns_a_directory_subtree() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("data", 1, 1),
                entry("data/a.bin", 2, 2),
                entry("data/nested/b.bin", 3, 3),
                entry("other.bin", 4, 4),
            ])
            .unwrap();

        assert_eq!(
            load_scope_paths(&store, "data"),
            vec![
                "data".to_string(),
                "data/a.bin".to_string(),
                "data/nested/b.bin".to_string()
            ]
        );
    }

    #[test]
    fn load_scope_excludes_sibling_prefixes() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("data", 1, 1),
                entry("data/a.bin", 2, 2),
                entry("data.bin", 3, 3),
                entry("database.bin", 4, 4),
            ])
            .unwrap();

        assert_eq!(
            load_scope_paths(&store, "data"),
            vec!["data".to_string(), "data/a.bin".to_string()]
        );
    }

    #[test]
    fn load_scope_handles_mixed_case_and_unicode_paths() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("Data/ßeta.bin", 1, 1),
                entry("Data/ßeta/nested.bin", 2, 2),
                entry("data/ßeta.bin", 3, 3),
            ])
            .unwrap();

        assert_eq!(
            load_scope_paths(&store, "Data"),
            vec![
                "Data/ßeta.bin".to_string(),
                "Data/ßeta/nested.bin".to_string()
            ]
        );
    }

    #[test]
    fn load_scope_returns_empty_when_nothing_matches() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store.upsert_many(&[entry("a.bin", 1, 1)]).unwrap();

        assert!(store.load_scope("missing").unwrap().is_empty());
    }

    #[test]
    fn load_scope_accepts_a_trailing_slash_scope() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[entry("data/a.bin", 1, 1), entry("data/b.bin", 2, 2)])
            .unwrap();

        assert_eq!(
            load_scope_paths(&store, "data/"),
            vec!["data/a.bin".to_string(), "data/b.bin".to_string()]
        );
    }

    #[test]
    fn load_scope_matches_filtering_load_all_by_path_matches_scope() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let datasets = vec![
            vec![
                entry("data", 1, 1),
                entry("data/a.bin", 2, 2),
                entry("data.bin", 3, 3),
                entry("other.bin", 4, 4),
            ],
            vec![
                entry("Data/ßeta.bin", 5, 5),
                entry("Data/ßeta/nested.bin", 6, 6),
                entry("data/ßeta.bin", 7, 7),
            ],
        ];

        for dataset in datasets {
            store
                .remove_exact(
                    &store
                        .load_all()
                        .unwrap()
                        .entries
                        .into_iter()
                        .map(|entry| entry.path)
                        .collect::<Vec<_>>(),
                )
                .unwrap();
            store.upsert_many(&dataset).unwrap();
            for scope in ["data", "Data", "Data/ßeta.bin", "missing"] {
                let from_sql = store.load_scope(scope).unwrap();
                let from_filter = store
                    .load_all()
                    .unwrap()
                    .entries
                    .into_iter()
                    .filter(|entry| path_matches_scope(&entry.path, &gp(scope)))
                    .collect::<Vec<_>>();
                assert_eq!(
                    from_sql, from_filter,
                    "scope {scope:?} should match in-memory filtering"
                );
            }
        }
    }

    #[test]
    fn load_all_raw_matches_load_all_decoded() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("a.bin", 1, 1),
                entry("dir/b.bin", 2, 2),
                entry("dir/nested/c.bin", 3, 3),
            ])
            .unwrap();

        let raw: Vec<Entry> = store
            .load_all_raw()
            .unwrap()
            .into_iter()
            .map(MaterializedRow::into_entry)
            .collect();
        assert_eq!(raw, store.load_all().unwrap().entries);
    }

    #[test]
    fn load_scope_raw_matches_load_scope_decoded() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("data", 1, 1),
                entry("data/a.bin", 2, 2),
                entry("data/nested/b.bin", 3, 3),
                entry("data.bin", 4, 4),
            ])
            .unwrap();

        for scope in ["data", "data/", "data.bin", "missing"] {
            let raw: Vec<Entry> = store
                .load_scope_raw(scope)
                .unwrap()
                .into_iter()
                .map(MaterializedRow::into_entry)
                .collect();
            assert_eq!(
                raw,
                store.load_scope(scope).unwrap(),
                "scope {scope:?} raw/decoded mismatch"
            );
        }
    }

    /// Characterization: streaming every row through
    /// [`StateStore::with_rows_in_scope`] (`scope = None`) yields
    /// the exact same rows, in the exact same order, as collecting the
    /// whole table via [`StateStore::load_all_raw`] -- the cursor
    /// path is not a second, divergent read implementation.
    #[test]
    fn with_rows_in_scope_full_scan_matches_load_all_raw() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("a.bin", 1, 1),
                entry("dir/b.bin", 2, 2),
                entry("dir/nested/c.bin", 3, 3),
            ])
            .unwrap();

        let expected = store.load_all_raw().unwrap();
        let streamed = store
            .with_rows_in_scope(None, |mut rows| -> Result<Vec<MaterializedRow>> {
                let mut collected = Vec::new();
                while let Some(row) = rows.next()? {
                    collected.push(row);
                }
                Ok(collected)
            })
            .unwrap();
        assert_eq!(streamed, expected);
    }

    /// Same equivalence as
    /// [`with_rows_in_scope_full_scan_matches_load_all_raw`], but for a
    /// scoped scan against [`StateStore::load_scope_raw`], across
    /// exact-file, subtree, and no-match scopes.
    #[test]
    fn with_rows_in_scope_scoped_scan_matches_load_scope_raw() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("data", 1, 1),
                entry("data/a.bin", 2, 2),
                entry("data/nested/b.bin", 3, 3),
                entry("data.bin", 4, 4),
            ])
            .unwrap();

        for scope in ["data", "data/", "data.bin", "missing"] {
            let expected = store.load_scope_raw(scope).unwrap();
            let streamed = store
                .with_rows_in_scope(
                    Some(&gp(scope)),
                    |mut rows| -> Result<Vec<MaterializedRow>> {
                        let mut collected = Vec::new();
                        while let Some(row) = rows.next()? {
                            collected.push(row);
                        }
                        Ok(collected)
                    },
                )
                .unwrap();
            assert_eq!(streamed, expected, "scope {scope:?} cursor mismatch");
        }
    }

    #[test]
    fn desired_scope_query_matches_exact_path_prefix_and_full_scan_semantics() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        seed_desired_shard(
            &mut store,
            sid("gat.lock/aa.tsv"),
            1,
            vec![
                entry("data", 1, 1),
                entry("data/a.bin", 2, 2),
                entry("data/nested/b.bin", 3, 3),
                entry("data.bin", 4, 4),
            ],
        );

        assert_eq!(
            store
                .desired_rows(DesiredQuery::scope(&gp("data")))
                .unwrap()
                .into_iter()
                .map(|e| e.path)
                .collect::<Vec<_>>(),
            vec!["data", "data/a.bin", "data/nested/b.bin"]
        );
        assert_eq!(
            store
                .desired_rows(DesiredQuery::scope(&gp("data.bin")))
                .unwrap()
                .into_iter()
                .map(|e| e.path)
                .collect::<Vec<_>>(),
            vec!["data.bin"]
        );
        assert_eq!(
            store.desired_rows(DesiredQuery::all()).unwrap(),
            store.load_desired_as_lock().unwrap().entries
        );

        for scope in [Some("data"), Some("data/"), Some("data.bin"), None] {
            let expected = store
                .desired_rows(DesiredQuery::in_scope(scope.map(gp).as_ref()))
                .unwrap();
            let streamed = store
                .with_desired_rows(
                    DesiredQuery::in_scope(scope.map(gp).as_ref()),
                    |mut rows| -> Result<Vec<Entry>> {
                        let mut collected = Vec::new();
                        while let Some(row) = rows.next()? {
                            collected.push(row.into_entry());
                        }
                        Ok(collected)
                    },
                )
                .unwrap();
            assert_eq!(
                streamed, expected,
                "scope {scope:?} desired cursor mismatch"
            );
        }
    }

    /// A malformed row (bad OID length) discovered part-way through a
    /// cursor scan must still fail loudly, exactly like
    /// [`decode_row_rejects_wrong_oid_length`] does for the collected
    /// `Vec` path -- the streaming path is not allowed to silently skip or
    /// truncate at the bad row.
    #[test]
    fn with_rows_in_scope_propagates_a_malformed_row_error() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let store = StateStore::open(&repo).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO state (path, materialized_oid, materialized_proof) VALUES ('a.bin', ?1, NULL)",
                [vec![0u8; 31]],
            )
            .unwrap();

        let result = store.with_rows_in_scope(None, |mut rows| -> Result<usize> {
            let mut seen = 0usize;
            while rows.next()?.is_some() {
                seen += 1;
            }
            Ok(seen)
        });
        assert!(
            result.is_err(),
            "a malformed row must fail the whole scan rather than being skipped"
        );
    }

    #[test]
    fn remove_exact_only_removes_named_paths() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[entry("a.bin", 1, 1), entry("dir/b.bin", 2, 2)])
            .unwrap();
        store.remove_exact(&[gp("a.bin")]).unwrap();
        let loaded = store.load_all().unwrap().entries;
        assert_eq!(loaded, vec![entry("dir/b.bin", 2, 0)]);
    }

    #[test]
    fn apply_batch_remove_exact_preserves_nested_paths() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("a", 1, 1),
                entry("a/b.bin", 2, 2),
                entry("sibling.bin", 3, 3),
            ])
            .unwrap();

        store
            .apply_batch(&[StateMutation::remove_exact(gp("a"))])
            .unwrap();

        let loaded = store.load_all().unwrap().entries;
        assert_eq!(
            loaded,
            vec![entry("a/b.bin", 2, 0), entry("sibling.bin", 3, 0)]
        );
    }

    #[test]
    fn remove_exact_and_prefix_handle_large_batches() {
        for count in [100usize, 1_000, 10_000] {
            let tmp = git_repo();
            let repo = Repo::at(tmp.path().to_path_buf());
            let mut store = StateStore::open(&repo).unwrap();
            let exact_entries: Vec<Entry> = (0..count)
                .map(|i| {
                    entry(
                        &format!("exact/f{i:06}.bin"),
                        (i % 256).to_le_bytes()[0],
                        i as u64,
                    )
                })
                .collect();
            let prefix_entries: Vec<Entry> = (0..count)
                .map(|i| {
                    entry(
                        &format!("prefix/nested/f{i:06}.bin"),
                        (i % 256).to_le_bytes()[0],
                        i as u64,
                    )
                })
                .collect();
            store
                .upsert_many(
                    &exact_entries
                        .iter()
                        .cloned()
                        .chain(prefix_entries.iter().cloned())
                        .chain(std::iter::once(entry("keep.bin", 7, 7)))
                        .collect::<Vec<_>>(),
                )
                .unwrap();

            let exact_paths: Vec<_> = exact_entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect();
            store.remove_exact(&exact_paths).unwrap();
            assert_eq!(
                store.load_scope("exact").unwrap(),
                Vec::<Entry>::new(),
                "count={count}"
            );

            let mut removed = store.remove_prefix(&gp("prefix")).unwrap();
            removed.sort();
            assert_eq!(removed.len(), count, "count={count}");
            assert_eq!(
                store.load_scope("prefix").unwrap(),
                Vec::<Entry>::new(),
                "count={count}"
            );
            assert_eq!(
                store.load_all().unwrap().entries,
                vec![entry("keep.bin", 7, 0)]
            );
        }
    }

    #[test]
    fn remove_prefix_removes_exact_and_nested_but_not_siblings() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("data", 1, 1),
                entry("data/a.bin", 2, 2),
                entry("data/nested/b.bin", 3, 3),
                entry("data.bin", 4, 4),
            ])
            .unwrap();
        let mut removed = store.remove_prefix(&gp("data")).unwrap();
        removed.sort();
        assert_eq!(removed, vec!["data", "data/a.bin", "data/nested/b.bin"]);
        let loaded = store.load_all().unwrap().entries;
        assert_eq!(loaded, vec![entry("data.bin", 4, 0)]);
    }

    /// After clearing every materialized row named by `remove_exact`/
    /// `remove_prefix`, a row with no desired half either must be
    /// actually deleted from `state`, not merely left behind with both
    /// halves `NULL`; empty rows are deleted rather than retained as tombstones.
    #[test]
    fn removing_a_materialized_only_row_deletes_it_entirely() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store.upsert_many(&[entry("a.bin", 1, 1)]).unwrap();
        store.remove_exact(&[gp("a.bin")]).unwrap();

        let count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM state WHERE path = 'a.bin'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 0,
            "an emptied row must be deleted, not left as NULL/NULL"
        );
    }

    #[test]
    fn move_prefix_moves_exact_file() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store.upsert_many(&[entry("a.bin", 1, 1)]).unwrap();
        store.move_prefix(&gp("a.bin"), &gp("b.bin")).unwrap();
        let loaded = store.load_all().unwrap().entries;
        assert_eq!(loaded, vec![entry("b.bin", 1, 0)]);
    }

    #[test]
    fn move_prefix_moves_a_whole_directory() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[entry("data/a.bin", 1, 1), entry("data/nested/b.bin", 2, 2)])
            .unwrap();
        store.move_prefix(&gp("data"), &gp("moved")).unwrap();
        let mut loaded = store.load_all().unwrap().entries;
        loaded.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            loaded,
            vec![
                entry("moved/a.bin", 1, 0),
                entry("moved/nested/b.bin", 2, 0),
            ]
        );
    }

    #[test]
    fn move_prefix_overwrites_a_destination_collision() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[entry("a.bin", 1, 1), entry("b.bin", 9, 9)])
            .unwrap();
        store.move_prefix(&gp("a.bin"), &gp("b.bin")).unwrap();
        let loaded = store.load_all().unwrap().entries;
        assert_eq!(loaded, vec![entry("b.bin", 1, 0)]);
    }

    #[test]
    fn remove_desired_prefix_removes_exact_and_nested_but_not_siblings() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        seed_desired_shard(
            &mut store,
            sid("gat.lock/aa.tsv"),
            1,
            vec![
                entry("data", 1, 1),
                entry("data/a.bin", 2, 2),
                entry("data/nested/b.bin", 3, 3),
                entry("data.bin", 4, 4),
            ],
        );

        let removed = store.remove_desired_prefix(&gp("data")).unwrap();
        assert_eq!(removed, vec!["data", "data/a.bin", "data/nested/b.bin"]);
        assert_eq!(
            store.load_desired_as_lock().unwrap().entries,
            vec![entry("data.bin", 4, 0)]
        );
    }

    #[test]
    fn move_desired_prefix_moves_rows_and_recomputes_their_shard_ids() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        seed_desired_shard(
            &mut store,
            sid("gat.lock/aa.tsv"),
            1,
            vec![entry("data/a.bin", 1, 1), entry("data/nested/b.bin", 2, 2)],
        );

        store
            .move_desired_prefix(
                &gp("data"),
                &gp("moved"),
                crate::lock::LockShardLevels::new(2).unwrap(),
            )
            .unwrap();

        let mut loaded = store.load_desired_as_lock().unwrap().entries;
        loaded.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            loaded,
            vec![
                entry("moved/a.bin", 1, 0),
                entry("moved/nested/b.bin", 2, 0)
            ]
        );
        let moved_paths: Vec<_> = loaded.iter().map(|e| e.path.clone()).collect();
        let shard_ids = store.desired_shard_ids_for_paths(&moved_paths).unwrap();
        let expected = moved_paths
            .iter()
            .map(|p| shard_id_for_path(p, LockShardLevels::new(2).unwrap()))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(shard_ids, expected);
    }

    #[test]
    fn typed_desired_upsert_publishes_the_post_mutation_state() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let levels = LockShardLevels::new(2).unwrap();
        let mut store = StateStore::open(&repo).unwrap();
        seed_published_desired(&repo, &mut store, vec![entry("a.bin", 1, 1)], levels);
        let shape_lock = crate::lock::LockStore::acquire_matching_shape(&repo, levels).unwrap();

        store
            .publish_desired_upsert::<DesiredPublishTestError>(
                repo.root_path(),
                &shape_lock,
                &[entry("a.bin", 2, 2), entry("b.bin", 3, 3)],
            )
            .unwrap();

        let mut published = crate::lock::LockStore::load_all(repo.root_path())
            .unwrap()
            .entries;
        published.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(published, vec![entry("a.bin", 2, 0), entry("b.bin", 3, 0)]);
    }

    #[test]
    fn typed_desired_removal_combines_exact_and_prefix_mutations() {
        for depth in [0, 2] {
            let tmp = git_repo();
            let repo = Repo::at(tmp.path().to_path_buf());
            let levels = LockShardLevels::new(depth).unwrap();
            let mut store = StateStore::open(&repo).unwrap();
            seed_published_desired(
                &repo,
                &mut store,
                vec![
                    entry("exact.bin", 1, 1),
                    entry("tree/a.bin", 2, 2),
                    entry("tree/b.bin", 3, 3),
                    entry("keep.bin", 4, 4),
                ],
                levels,
            );
            let shape_lock = crate::lock::LockStore::acquire_matching_shape(&repo, levels).unwrap();
            let exact = [gp("exact.bin")];
            let prefix = gp("tree");

            crate::lock::flat_publish_test_support::reset_flat_shard_publish_counters();
            store
                .publish_desired_removals::<DesiredPublishTestError>(
                    repo.root_path(),
                    &shape_lock,
                    &[gp("exact.bin"), gp("tree/a.bin"), gp("tree/b.bin")],
                    &[
                        DesiredRemoval::Exact(&exact),
                        DesiredRemoval::Prefix(&prefix),
                    ],
                )
                .unwrap();
            if depth == 0 {
                assert_eq!(
                    crate::lock::flat_publish_test_support::flat_shard_publish_calls(),
                    1
                );
                assert_eq!(
                    crate::lock::flat_publish_test_support::max_retained_flat_publish_rows(),
                    1
                );
            }

            assert_eq!(
                crate::lock::LockStore::load_all(repo.root_path())
                    .unwrap()
                    .entries,
                vec![entry("keep.bin", 4, 0)]
            );
        }
    }

    #[test]
    fn typed_desired_move_publishes_source_destination_and_collision_shards() {
        for depth in [0, 2] {
            let tmp = git_repo();
            let repo = Repo::at(tmp.path().to_path_buf());
            let levels = LockShardLevels::new(depth).unwrap();
            let mut store = StateStore::open(&repo).unwrap();
            seed_published_desired(
                &repo,
                &mut store,
                vec![
                    entry("source/a.bin", 1, 1),
                    entry("source/nested/b.bin", 2, 2),
                    entry("target/stale.bin", 3, 3),
                    entry("keep.bin", 4, 4),
                ],
                levels,
            );
            let shape_lock = crate::lock::LockStore::acquire_matching_shape(&repo, levels).unwrap();

            crate::lock::flat_publish_test_support::reset_flat_shard_publish_counters();
            store
                .publish_desired_move::<DesiredPublishTestError>(
                    repo.root_path(),
                    &shape_lock,
                    &gp("source"),
                    &gp("target"),
                )
                .unwrap();
            if depth == 0 {
                assert_eq!(
                    crate::lock::flat_publish_test_support::flat_shard_publish_calls(),
                    1
                );
                assert_eq!(
                    crate::lock::flat_publish_test_support::max_retained_flat_publish_rows(),
                    1
                );
            }

            let mut published = crate::lock::LockStore::load_all(repo.root_path())
                .unwrap()
                .entries;
            published.sort_by(|a, b| a.path.cmp(&b.path));
            assert_eq!(
                published,
                vec![
                    entry("keep.bin", 4, 0),
                    entry("target/a.bin", 1, 0),
                    entry("target/nested/b.bin", 2, 0),
                ]
            );
        }
    }

    #[test]
    fn desired_move_preserves_captured_rows_when_prefixes_overlap() {
        for depth in [0, 2] {
            for (src, dst, expected) in [
                (
                    "tree",
                    "tree/sub",
                    vec![
                        entry("outside", 4, 0),
                        entry("tree/sub/a", 1, 0),
                        entry("tree/sub/sub/b", 2, 0),
                        entry("tree/sub/sub/deep/c", 3, 0),
                    ],
                ),
                (
                    "tree/sub",
                    "tree",
                    vec![
                        entry("outside", 4, 0),
                        entry("tree/b", 2, 0),
                        entry("tree/deep/c", 3, 0),
                    ],
                ),
                (
                    "tree",
                    "tree",
                    vec![
                        entry("outside", 4, 0),
                        entry("tree/a", 1, 0),
                        entry("tree/sub/b", 2, 0),
                        entry("tree/sub/deep/c", 3, 0),
                    ],
                ),
                (
                    "missing",
                    "tree",
                    vec![
                        entry("outside", 4, 0),
                        entry("tree/a", 1, 0),
                        entry("tree/sub/b", 2, 0),
                        entry("tree/sub/deep/c", 3, 0),
                    ],
                ),
            ] {
                let tmp = git_repo();
                let repo = Repo::at(tmp.path().to_path_buf());
                let levels = LockShardLevels::new(depth).unwrap();
                let mut store = StateStore::open(&repo).unwrap();
                seed_published_desired(
                    &repo,
                    &mut store,
                    vec![
                        entry("tree/a", 1, 0),
                        entry("tree/sub/b", 2, 0),
                        entry("tree/sub/deep/c", 3, 0),
                        entry("outside", 4, 0),
                    ],
                    levels,
                );
                let shape = crate::lock::LockStore::acquire_matching_shape(&repo, levels).unwrap();
                store
                    .publish_desired_move::<DesiredPublishTestError>(
                        repo.root_path(),
                        &shape,
                        &gp(src),
                        &gp(dst),
                    )
                    .unwrap();
                let mut actual = crate::lock::LockStore::load_all(repo.root_path())
                    .unwrap()
                    .entries;
                actual.sort_by(|a, b| a.path.cmp(&b.path));
                assert_eq!(actual, expected, "depth={depth}, {src} -> {dst}");
                assert_eq!(store.load_desired_as_lock().unwrap().entries, actual);
            }
        }
    }

    #[test]
    fn shard_id_lookup_consumes_only_one_sql_batch_before_an_error() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let store = StateStore::open(&repo).unwrap();
        store.conn.execute_batch("DROP TABLE state").unwrap();
        let path = gp("a.bin");
        let consumed = std::cell::Cell::new(0);
        let batch = sql_chunk_size(1);
        let paths = std::iter::repeat_n(&path, batch * 3).inspect(|_| {
            consumed.set(consumed.get() + 1);
        });
        assert!(store.desired_shard_ids_for_paths(paths).is_err());
        assert_eq!(consumed.get(), batch);
    }

    #[test]
    fn shard_id_lookup_merges_ids_across_sql_batches() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let rows = (0..sql_chunk_size(1) * 2 + 3)
            .map(|i| entry(&format!("data/{i:05}.bin"), 1, 0))
            .collect::<Vec<_>>();
        seed_desired_shard(&mut store, sid("gat.lock/aa.tsv"), 1, rows.clone());
        seed_desired_shard(
            &mut store,
            sid("gat.lock/bb.tsv"),
            1,
            vec![entry("z.bin", 2, 0)],
        );
        let z = gp("z.bin");
        let ids = store
            .desired_shard_ids_for_paths(rows.iter().map(|entry| &entry.path).chain([&z]))
            .unwrap();
        assert_eq!(
            ids,
            std::collections::BTreeSet::from([sid("gat.lock/aa.tsv"), sid("gat.lock/bb.tsv"),])
        );
    }

    #[test]
    fn desired_rows_by_shard_ids_only_returns_the_named_current_shards() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        seed_desired_shard(
            &mut store,
            sid("gat.lock/aa.tsv"),
            1,
            vec![entry("a.bin", 1, 1)],
        );
        seed_desired_shard(
            &mut store,
            sid("gat.lock/bb.tsv"),
            2,
            vec![entry("b.bin", 2, 2)],
        );

        let rows = store
            .desired_rows_by_shard_ids(&std::collections::BTreeSet::from([sid("gat.lock/bb.tsv")]))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[&sid("gat.lock/bb.tsv")], vec![entry("b.bin", 2, 0)]);
    }

    #[test]
    fn desired_rows_by_shard_ids_spans_bounded_sql_chunks() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let store = StateStore::open(&repo).unwrap();
        let ids = (0..(u32::try_from(sql_chunk_size(1)).unwrap() + 3))
            .map(nsid)
            .collect::<std::collections::BTreeSet<_>>();

        assert!(store.desired_rows_by_shard_ids(&ids).unwrap().is_empty());
    }

    /// `ORDER BY desired_shard_id, path` groups every row of one shard
    /// contiguously: this exercises a shard with more than one row (so
    /// the parse-once-per-group cache actually gets to reuse a decoded
    /// `LockShardId` across rows) next to a second, later shard (so the
    /// cache also correctly reparses on the group transition).
    #[test]
    fn desired_rows_by_shard_ids_groups_multiple_rows_under_the_same_shard() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        seed_desired_shard(
            &mut store,
            sid("gat.lock/aa.tsv"),
            1,
            vec![
                entry("a1.bin", 1, 1),
                entry("a2.bin", 1, 1),
                entry("a3.bin", 1, 1),
            ],
        );
        seed_desired_shard(
            &mut store,
            sid("gat.lock/bb.tsv"),
            2,
            vec![entry("b1.bin", 2, 2), entry("b2.bin", 2, 2)],
        );

        let rows = store
            .desired_rows_by_shard_ids(&std::collections::BTreeSet::from([
                sid("gat.lock/aa.tsv"),
                sid("gat.lock/bb.tsv"),
            ]))
            .unwrap();
        assert_eq!(rows[&sid("gat.lock/aa.tsv")].len(), 3);
        assert_eq!(rows[&sid("gat.lock/bb.tsv")].len(), 2);
    }

    #[test]
    fn paths_with_unusual_characters_round_trip() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let weird = [
            "tab\t.bin",
            "lf\n.bin",
            "cr\r.bin",
            "has spaces.bin",
            "percent%.bin",
            "under_score.bin",
            "MixedCase.BIN",
            "unicode-héllo-世界.bin",
        ];
        let entries: Vec<Entry> = weird
            .iter()
            .enumerate()
            .map(|(i, p)| entry(p, (i).to_le_bytes()[0], i as u64))
            .collect();
        store.upsert_many(&entries).unwrap();
        let mut loaded = store.load_all().unwrap().entries;
        loaded.sort_by(|a, b| a.path.cmp(&b.path));
        let mut expected: Vec<Entry> = weird
            .iter()
            .enumerate()
            .map(|(i, p)| entry(p, (i).to_le_bytes()[0], 0))
            .collect();
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(loaded, expected);
    }

    #[test]
    fn load_scope_returns_correct_results_for_stored_paths_with_percent_and_underscore() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[
                entry("data%/a.bin", 1, 1),
                entry("data_/b.bin", 2, 2),
                entry("data/d.bin", 4, 4),
            ])
            .unwrap();

        assert_eq!(load_scope_paths(&store, "data%"), vec!["data%/a.bin"]);
        assert_eq!(load_scope_paths(&store, "data_"), vec!["data_/b.bin"]);
        assert_eq!(load_scope_paths(&store, "data"), vec!["data/d.bin"]);
    }

    #[test]
    fn load_scope_with_percent_and_underscore_in_scope_argument() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[entry("a%b.bin", 1, 1), entry("a_b.bin", 2, 2)])
            .unwrap();

        assert!(store.load_scope("a%b").unwrap().is_empty());
        assert!(store.load_scope("a_b").unwrap().is_empty());
        assert_eq!(
            load_scope_paths(&store, "a%b.bin"),
            vec!["a%b.bin".to_string()]
        );
    }

    #[test]
    fn remove_prefix_is_case_sensitive() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store
            .upsert_many(&[entry("Data/a.bin", 1, 1), entry("data/b.bin", 2, 2)])
            .unwrap();
        let removed = store.remove_prefix(&gp("data")).unwrap();
        assert_eq!(removed, vec!["data/b.bin"]);
        let loaded = store.load_all().unwrap().entries;
        assert_eq!(loaded, vec![entry("Data/a.bin", 1, 0)]);
    }

    #[test]
    fn oid_round_trips_all_32_bytes() {
        let bytes: Vec<u8> = (0..32).collect();
        let oid = Oid::from_bytes(bytes.clone().try_into().unwrap());
        assert_eq!(oid.as_bytes().to_vec(), bytes);
    }

    #[test]
    fn decode_row_rejects_wrong_oid_length() {
        assert!(decode_row_raw("a.bin".to_string(), vec![0u8; 31], None).is_err());
    }

    #[test]
    fn decode_row_malformed_proof_decodes_as_absent() {
        // A malformed/unknown proof BLOB must decode as *absent* proof
        // (never a fatal DB error), so gat falls back to re-hashing.
        let row = decode_row_raw(
            "a.bin".to_string(),
            vec![7u8; 32],
            Some(vec![0xFF, 0x00, 0x01]),
        )
        .expect("malformed proof must not be fatal");
        assert!(row.proof.is_none());
    }

    #[test]
    fn decode_row_proof_blob_round_trips() {
        // A well-formed proof BLOB round-trips through the shared codec.
        let proof = crate::file_state::StatProof {
            size: 4242,
            mtime_secs: 1_700_000_000,
            mtime_nanos: 0,
        };
        let blob = crate::file_state::encode_stat_proof(&proof).to_vec();
        let row = decode_row_raw("a.bin".to_string(), vec![9u8; 32], Some(blob)).unwrap();
        assert_eq!(row.proof, Some(proof));
    }

    #[test]
    fn transaction_rolls_back_on_conflicting_type_error() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        store.upsert_many(&[entry("a.bin", 1, 1)]).unwrap();
        // `upsert_many` does not encode `size`, so simulate a mid-transaction
        // SQL failure and verify the whole statement rolls back.
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER reject_b_bin_before_insert
                 BEFORE INSERT ON state
                 WHEN NEW.path = 'b.bin'
                 BEGIN
                     SELECT RAISE(ABORT, 'simulated failure');
                 END;",
            )
            .unwrap();
        assert!(
            store
                .upsert_many(&[entry("c.bin", 3, 3), entry("b.bin", 2, 2)])
                .is_err()
        );
        let loaded = store.load_all().unwrap().entries;
        assert_eq!(loaded, vec![entry("a.bin", 1, 0)]);
    }

    #[test]
    fn unsupported_newer_schema_version_fails_closed() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        // Create and populate a real database, then bump its version.
        {
            let mut store = StateStore::open(&repo).unwrap();
            store.upsert_many(&[entry("a.bin", 1, 1)]).unwrap();
        }
        {
            let conn = Connection::open(repo.materialized_db_path()).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let err = StateStore::open(&repo).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not support"),
            "expected a fail-closed schema-mismatch error, got: {msg}"
        );
    }

    /// Any nonzero version that is not exactly `SCHEMA_VERSION` fails
    /// closed with the same
    /// message, rather than one direction being silently dropped and
    /// rebuilt. `0` remains the one reserved sentinel for "never
    /// initialized" (there is no valid on-disk version strictly below
    /// `SCHEMA_VERSION` to simulate once it's `1`), so this exercises
    /// the mismatch with a version two higher instead.
    #[test]
    fn any_mismatched_nonzero_schema_version_fails_closed() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        {
            let mut store = StateStore::open(&repo).unwrap();
            store.upsert_many(&[entry("a.bin", 1, 1)]).unwrap();
        }
        {
            let conn = Connection::open(repo.materialized_db_path()).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 2)
                .unwrap();
        }

        let err = StateStore::open(&repo).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not support"),
            "expected a fail-closed schema-mismatch error, got: {msg}"
        );
    }

    #[test]
    fn database_state_persists_across_reopen() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        {
            let mut store = StateStore::open(&repo).unwrap();
            store.upsert_many(&[entry("a.bin", 1, 1)]).unwrap();
        }
        let store = StateStore::open(&repo).unwrap();
        assert_eq!(
            store.load_all().unwrap().entries,
            vec![entry("a.bin", 1, 0)]
        );
    }

    #[test]
    fn busy_database_produces_a_bounded_error_rather_than_hanging() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        // Ensure the database exists first.
        StateStore::open(&repo).unwrap();

        // Hold an exclusive write lock from a second connection with a
        // short busy_timeout, then verify a conflicting writer from this
        // store also fails quickly rather than hanging indefinitely.
        let blocker = Connection::open(repo.materialized_db_path()).unwrap();
        blocker.pragma_update(None, "busy_timeout", 50).unwrap();
        blocker
            .execute_batch(
                "BEGIN IMMEDIATE; \
                 INSERT INTO state (path, materialized_oid, materialized_proof) \
                 VALUES ('x', zeroblob(32), NULL);",
            )
            .unwrap();

        let mut store = StateStore::open(&repo).unwrap();
        store.conn.pragma_update(None, "busy_timeout", 50).unwrap();
        let err = store
            .upsert_many(&[entry("a.bin", 1, 1)])
            .expect_err("expected a busy/locked timeout, not a hang");
        assert!(matches!(
            err,
            StateStoreError::QueryFailed {
                source,
                ..
            } if source.kind() == StateSqlErrorKind::Busy
        ));

        blocker.execute_batch("ROLLBACK;").unwrap();
    }

    /// Fault-injection coverage of a *real* invalid-database-bytes
    /// failure: a file at the expected database path that isn't a `SQLite`
    /// database at all (e.g. left over from a corrupted write, or copied
    /// from an unrelated file) must be reported through the same
    /// classification path as a genuine `SQLITE_NOTADB` failure --
    /// `StateCorrupt` -- not panic or silently truncate/recreate the file.
    /// `SQLite` only detects `NotADatabase` on the first real read (here,
    /// inside `configure()`'s `PRAGMA` calls), so this surfaces as a
    /// `StateStoreError::QueryFailed` rather than `OpenFailed`; either way
    /// the typed state error must preserve `SQLite`'s corruption signal.
    #[test]
    fn opening_a_file_that_is_not_a_sqlite_database_reports_state_corrupt() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let db_path = repo.materialized_db_path();
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::write(&db_path, b"not a sqlite database, just garbage bytes").unwrap();

        let err = StateStore::open(&repo).expect_err("expected an open failure");
        assert!(matches!(
            err,
            StateStoreError::QueryFailed {
                source,
                ..
            } if source.kind() == StateSqlErrorKind::Corrupt
        ));
    }

    /// Fault-injection coverage of a *real* filesystem permission failure
    /// (not a constructed [`StateStoreError`]): making the `.gat/state`
    /// parent directory read-only denies the directory creation
    /// `StateStore::open_at` needs before it can even attempt to
    /// open the database, and the resulting error must surface as
    /// [`StateStoreError::DirectoryUnavailable`] classified as
    /// `PermissionDenied` -- mirroring `src/atomic.rs`'s
    /// `write_atomic_reports_permission_denied_for_an_unwritable_parent_directory`
    /// coverage for a genuine OS-reported permission failure.
    #[test]
    #[cfg(unix)]
    fn opening_the_database_under_a_readonly_parent_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let db_path = repo.materialized_db_path();
        let parent = db_path.parent().unwrap();
        // The parent of `.gat/state` (i.e. `.gat`) must exist but not be
        // writable, so `create_dir_all(".gat/state")` itself fails.
        std::fs::create_dir_all(parent.parent().unwrap()).unwrap();
        std::fs::set_permissions(
            parent.parent().unwrap(),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();

        let err = StateStore::open(&repo).expect_err("expected an open failure");
        assert!(
            matches!(err, StateStoreError::DirectoryUnavailable { .. }),
            "expected DirectoryUnavailable, got {err:?}"
        );
        // Restore write permission so the tempdir can clean itself up.
        std::fs::set_permissions(
            parent.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    #[test]
    fn dirty_is_false_for_a_fresh_row_matching_desired_and_materialized() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let e = entry("a.bin", 1, 1);
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id: sid("gat.lock"),
                    prior_identity: None,
                    identity: ShardIdentity::from_array([0u8; 32]),
                    proof: Some(crate::file_state::StatProof {
                        size: 0,
                        mtime_secs: 0,
                        mtime_nanos: 0,
                    }),
                    entries: vec![e.clone()],
                }],
                &[],
                &[],
            )
            .unwrap();
        assert!(store.has_dirty().unwrap());

        store.upsert_many(&[e]).unwrap();
        assert!(!store.has_dirty().unwrap());
    }

    #[test]
    fn dirty_reflects_a_size_or_oid_mismatch_but_not_shard_id_alone() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let e = entry("a.bin", 1, 1);
        let stat = Some(crate::file_state::StatProof {
            size: 0,
            mtime_secs: 0,
            mtime_nanos: 0,
        });
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id: sid("gat.lock/aa.tsv"),
                    prior_identity: None,
                    identity: ShardIdentity::from_array([1u8; 32]),
                    proof: stat,
                    entries: vec![e.clone()],
                }],
                &[],
                &[],
            )
            .unwrap();
        store.upsert_many(std::slice::from_ref(&e)).unwrap();
        assert!(!store.has_dirty().unwrap());

        // Resharding the same content under a different shard id must not
        // flip dirty; resharding is semantic, not physical.
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id: sid("gat.lock/bb.tsv"),
                    prior_identity: None,
                    identity: ShardIdentity::from_array([2u8; 32]),
                    proof: stat,
                    entries: vec![e.clone()],
                }],
                &[],
                &[RemovedShard {
                    shard_id: sid("gat.lock/aa.tsv"),
                    prior_identity: ShardIdentity::from_array([1u8; 32]),
                }],
            )
            .unwrap();
        assert!(!store.has_dirty().unwrap());

        // An actual content change does flip dirty.
        let changed = entry("a.bin", 9, 9);
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id: sid("gat.lock/bb.tsv"),
                    prior_identity: Some(ShardIdentity::from_array([2u8; 32])),
                    identity: ShardIdentity::from_array([3u8; 32]),
                    proof: stat,
                    entries: vec![changed],
                }],
                &[],
                &[],
            )
            .unwrap();
        assert!(store.has_dirty().unwrap());
    }

    #[test]
    fn dirty_rows_in_scope_only_returns_dirty_rows_under_the_given_scope() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let stat = Some(crate::file_state::StatProof {
            size: 0,
            mtime_secs: 0,
            mtime_nanos: 0,
        });
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id: sid("gat.lock"),
                    prior_identity: None,
                    identity: ShardIdentity::from_array([0u8; 32]),
                    proof: stat,
                    entries: vec![
                        entry("data/a.bin", 1, 1),
                        entry("data/b.bin", 2, 2),
                        entry("other.bin", 3, 3),
                    ],
                }],
                &[],
                &[],
            )
            .unwrap();

        // Unscoped: every dirty row.
        let all: Vec<String> = store
            .dirty_rows_in_scope(None)
            .unwrap()
            .into_iter()
            .map(|r| r.path.to_string())
            .collect();
        assert_eq!(all, vec!["data/a.bin", "data/b.bin", "other.bin"]);

        // Scoped to `data`: only rows nested under it, not `other.bin`.
        let scoped: Vec<String> = store
            .dirty_rows_in_scope(Some(&gp("data")))
            .unwrap()
            .into_iter()
            .map(|r| r.path.to_string())
            .collect();
        assert_eq!(scoped, vec!["data/a.bin", "data/b.bin"]);

        // Reconciling `data`'s rows must not affect `other.bin`'s dirty
        // status, and a still-clean scope must come back empty.
        store
            .upsert_many(&[entry("data/a.bin", 1, 1), entry("data/b.bin", 2, 2)])
            .unwrap();
        assert!(
            store
                .dirty_rows_in_scope(Some(&gp("data")))
                .unwrap()
                .is_empty()
        );
        let remaining: Vec<String> = store
            .dirty_rows_in_scope(None)
            .unwrap()
            .into_iter()
            .map(|r| r.path.to_string())
            .collect();
        assert_eq!(remaining, vec!["other.bin"]);
    }

    #[test]
    fn apply_shard_refresh_is_a_no_op_when_nothing_changed() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        // With every list empty, this must never even open a write
        // transaction -- exercised indirectly by asserting it succeeds
        // even while another connection holds an exclusive write lock.
        let blocker = Connection::open(repo.materialized_db_path()).unwrap();
        blocker.pragma_update(None, "busy_timeout", 50).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();

        store.apply_shard_refresh(&[], &[], &[]).unwrap();

        blocker.execute_batch("ROLLBACK;").unwrap();
    }

    #[test]
    fn apply_shard_refresh_handles_many_changed_and_removed_shards() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let stat = Some(crate::file_state::StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 0,
        });
        let initial: Vec<ChangedShard> = (0..1_000usize)
            .map(|i| ChangedShard {
                shard_id: nsid(u32::try_from(i).unwrap()),
                prior_identity: None,
                identity: ShardIdentity::from_array([(i).to_le_bytes()[0]; 32]),
                proof: stat,
                entries: vec![entry(
                    &format!("path-{i:04}.bin"),
                    (i % 256).to_le_bytes()[0],
                    i as u64,
                )],
            })
            .collect();
        store.apply_shard_refresh(&initial, &[], &[]).unwrap();

        let changed: Vec<ChangedShard> = (0..500usize)
            .map(|i| ChangedShard {
                shard_id: nsid(u32::try_from(i).unwrap()),
                prior_identity: Some(ShardIdentity::from_array([(i).to_le_bytes()[0]; 32])),
                identity: ShardIdentity::from_array([((i).to_le_bytes()[0]).wrapping_add(1); 32]),
                proof: stat,
                entries: vec![entry(
                    &format!("path-{i:04}.bin"),
                    ((i + 1) % 256).to_le_bytes()[0],
                    i as u64,
                )],
            })
            .collect();
        let removed: Vec<RemovedShard> = (500..1_000usize)
            .map(|i| RemovedShard {
                shard_id: nsid(u32::try_from(i).unwrap()),
                prior_identity: ShardIdentity::from_array([(i).to_le_bytes()[0]; 32]),
            })
            .collect();
        store.apply_shard_refresh(&changed, &[], &removed).unwrap();

        assert_eq!(store.all_shard_ids().unwrap().len(), 500);
        assert_eq!(store.load_desired_as_lock().unwrap().entries.len(), 500);
    }

    #[test]
    fn desired_fingerprint_starts_at_a_fixed_empty_value_and_changes_with_shard_content() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let empty = store.desired_fingerprint().unwrap();
        let empty_identity = crate::lock::CanonicalDesiredIdentity::empty();
        // Seeded from schema creation, not lazily computed on first
        // refresh: a repo with nothing ever tracked has no
        // shard change/removal to trigger a recompute, so this must
        // already agree with the empty identity element
        // `current_desired_revision` independently computes for the same
        // (empty) state.
        assert_eq!(empty, *empty_identity.as_bytes());

        let stat = Some(crate::file_state::StatProof {
            size: 10,
            mtime_secs: 1,
            mtime_nanos: 0,
        });
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id: sid("gat.lock/aa.tsv"),
                    prior_identity: None,
                    identity: ShardIdentity::from_array([1u8; 32]),
                    proof: stat,
                    entries: vec![entry("a.bin", 1, 1)],
                }],
                &[],
                &[],
            )
            .unwrap();
        let after_first = store.desired_fingerprint().unwrap();
        assert_ne!(after_first, empty);

        // A stat-only refresh (identity unchanged) must not perturb the
        // fingerprint at all.
        store
            .apply_shard_refresh(&[], &[(sid("gat.lock/aa.tsv"), stat)], &[])
            .unwrap();
        assert_eq!(store.desired_fingerprint().unwrap(), after_first);

        // Changing the shard's content identity changes the fingerprint.
        store
            .apply_shard_refresh(
                &[ChangedShard {
                    shard_id: sid("gat.lock/aa.tsv"),
                    prior_identity: Some(ShardIdentity::from_array([1u8; 32])),
                    identity: ShardIdentity::from_array([2u8; 32]),
                    proof: stat,
                    entries: vec![entry("a.bin", 2, 1)],
                }],
                &[],
                &[],
            )
            .unwrap();
        let after_change = store.desired_fingerprint().unwrap();
        assert_ne!(after_change, after_first);

        // Removing the shard restores the fingerprint to the same
        // deterministic empty identity element this test started with.
        store
            .apply_shard_refresh(
                &[],
                &[],
                &[RemovedShard {
                    shard_id: sid("gat.lock/aa.tsv"),
                    prior_identity: ShardIdentity::from_array([2u8; 32]),
                }],
            )
            .unwrap();
        assert_eq!(store.desired_fingerprint().unwrap(), empty);
    }

    /// After a mix of new, changed, unchanged
    /// (stat-only), and removed shards in one refresh, the persisted
    /// whole-lock identity [`StateStore::apply_shard_refresh`]
    /// incrementally maintained must exactly equal an independent
    /// from-scratch recomputation over the final surviving catalog --
    /// proving the incremental XOR toggles never drift from what a full
    /// recompute would say, without ever performing that full recompute
    /// itself.
    #[test]
    fn persisted_identity_after_mixed_refresh_matches_a_from_scratch_recomputation() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let stat = Some(crate::file_state::StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 0,
        });

        // Seed three shards.
        let seed: Vec<ChangedShard> = (0..3u8)
            .map(|i| ChangedShard {
                shard_id: nsid(u32::from(i)),
                prior_identity: None,
                identity: ShardIdentity::from_array([i; 32]),
                proof: stat,
                entries: vec![entry(&format!("path-{i}.bin"), i, u64::from(i))],
            })
            .collect();
        store.apply_shard_refresh(&seed, &[], &[]).unwrap();

        // One mixed refresh: shard-0 changes content, shard-1 gets a
        // stat-only touch (identity unchanged), shard-2 is removed, and
        // shard-3 is brand new.
        let changed_or_new: Vec<ChangedShard> = vec![
            ChangedShard {
                shard_id: nsid(0),
                prior_identity: Some(ShardIdentity::from_array([0u8; 32])),
                identity: ShardIdentity::from_array([9u8; 32]),
                proof: stat,
                entries: vec![entry("path-0.bin", 9, 0)],
            },
            ChangedShard {
                shard_id: nsid(3),
                prior_identity: None,
                identity: ShardIdentity::from_array([3u8; 32]),
                proof: stat,
                entries: vec![entry("path-3.bin", 3, 3)],
            },
        ];
        let stat_only_updates = vec![(nsid(1), stat)];
        let removed: Vec<RemovedShard> = vec![RemovedShard {
            shard_id: nsid(2),
            prior_identity: ShardIdentity::from_array([2u8; 32]),
        }];
        store
            .apply_shard_refresh(&changed_or_new, &stat_only_updates, &removed)
            .unwrap();

        let persisted = store.desired_fingerprint().unwrap();
        let final_catalog = store.all_shard_identities().unwrap();
        let from_scratch = crate::lock::desired_identity_from_shards(
            final_catalog
                .iter()
                .map(|(id, stored)| (*id, &stored.identity)),
        );
        assert_eq!(persisted, *from_scratch.as_bytes());
    }

    /// `record_published_shards` performs no SQL lookup of its own; callers
    /// fetch the touched-shard-scoped prior catalog exactly
    /// once via [`DesiredStateWrite::shard_identities`] and pass it in, so
    /// this proves `record_published_shards` itself prepares zero
    /// statements for a prior-identity lookup (the one scoped fetch is
    /// now visible only at the `shard_identities` call site, exercised
    /// separately below).
    #[test]
    fn record_published_shards_performs_no_internal_prior_identity_query() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();
        let stat = crate::file_state::StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 0,
        };

        // A catalog much larger than the two shards this test actually
        // touches.
        let seed: Vec<ChangedShard> = (0..50u32)
            .map(|i| ChangedShard {
                shard_id: nsid(i),
                prior_identity: None,
                identity: ShardIdentity::from_array([(i % 256).to_le_bytes()[0]; 32]),
                proof: Some(stat),
                entries: vec![],
            })
            .collect();
        store.apply_shard_refresh(&seed, &[], &[]).unwrap();

        let touched: std::collections::BTreeSet<LockShardId> =
            [nsid(0), nsid(1)].into_iter().collect();
        let prior_catalog = store
            .desired_write(|write| write.shard_identities(&touched))
            .unwrap();

        let before = super::query::test_support::snapshot();
        store
            .desired_write(|write| {
                write.record_published_shards(
                    &[crate::lock::flat_publish_test_support::shard_evidence(
                        nsid(0),
                        ShardIdentity::from_array([200u8; 32]),
                        stat,
                    )],
                    &[nsid(1)],
                    &prior_catalog,
                )
            })
            .unwrap();
        let after = super::query::test_support::snapshot();

        // No statements prepared for a prior-identity lookup: the whole
        // scoped fetch happened once, above, via `shard_identities`.
        assert_eq!(
            after.0 - before.0,
            0,
            "record_published_shards must not query for prior identity itself"
        );

        // And the result only reflects the touched shards: shard-0000's
        // identity actually changed, shard-0001 is gone, and every other
        // seeded shard is untouched.
        let catalog = store.all_shard_identities().unwrap();
        assert!(!catalog.contains_key(&nsid(1)));
        assert_eq!(
            catalog.get(&nsid(0)).unwrap().identity,
            ShardIdentity::from_array([200u8; 32])
        );
        assert_eq!(catalog.len(), 49);
    }

    #[test]
    fn exclude_record_round_trips_fingerprint_count_identity_and_proof() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());
        let mut store = StateStore::open(&repo).unwrap();

        // Nothing recorded yet -- every field reads back empty.
        let empty = store.exclude_record().unwrap();
        assert_eq!(empty.fingerprint(), None);
        assert_eq!(empty.count(), 0);
        assert_eq!(empty.block_identity(), None);
        assert!(!empty.has_reusable_proof());

        let proof = crate::file_state::StatProof {
            size: 3,
            mtime_secs: 100,
            mtime_nanos: 0,
        };
        store
            .record_exclude_output(
                [7u8; 32],
                3,
                [1u8; 32],
                crate::git::InfoExcludeUpdate::for_test(Some(proof)),
            )
            .unwrap();
        let record = store.exclude_record().unwrap();
        assert_eq!(record.fingerprint(), Some([7u8; 32]));
        assert_eq!(record.count(), 3);
        assert_eq!(record.block_identity(), Some([1u8; 32]));
        assert!(record.has_reusable_proof());

        // A different fingerprint/count/identity fully replaces the prior
        // record rather than merging with it.
        store
            .record_exclude_output(
                [8u8; 32],
                5,
                [2u8; 32],
                crate::git::InfoExcludeUpdate::for_test(None),
            )
            .unwrap();
        let record = store.exclude_record().unwrap();
        assert_eq!(record.fingerprint(), Some([8u8; 32]));
        assert_eq!(record.count(), 5);
        assert_eq!(record.block_identity(), Some([2u8; 32]));
        assert!(!record.has_reusable_proof());

        // Refreshing a verification only touches the proof column, leaving
        // fingerprint/count/block_identity untouched -- the "confirmed
        // unchanged, mint/clear a proof" step of the exclude coordinator.
        store
            .refresh_exclude_verification(crate::git::InfoExcludeVerification::current_for_test(
                Some(proof),
            ))
            .unwrap();
        let record = store.exclude_record().unwrap();
        assert_eq!(record.fingerprint(), Some([8u8; 32]));
        assert_eq!(record.count(), 5);
        assert_eq!(record.block_identity(), Some([2u8; 32]));
        assert!(record.has_reusable_proof());
    }

    /// Stress test for the pinned desired snapshot's effect on WAL lifetime.
    ///
    /// One `StateStore` pins a desired-state snapshot (as a
    /// long-running fetch/pull operation does before its remote-I/O phase)
    /// and holds that pinned read transaction open for the duration of a
    /// simulated "slow multi-window transfer". Meanwhile a second,
    /// independent connection to the same database performs many desired
    /// row commits (simulating concurrent `gat add`/`gat mv` activity from
    /// another process). We then measure:
    ///
    /// 1. Whether `wal_checkpoint(TRUNCATE)` from the writer connection is
    ///    blocked while the reader's snapshot is pinned (it must be, since
    ///    `SQLite` cannot truncate WAL frames a live reader still needs).
    /// 2. How large the WAL file grows while blocked, proportional to the
    ///    concurrent writer activity -- not to the reader's own work.
    /// 3. That once the pinned reader is dropped (rolling back its
    ///    read-only transaction), a subsequent checkpoint from the writer
    ///    can truncate the WAL back down.
    ///
    /// A pinned reader spanning a slow network operation can hold back WAL
    /// truncation for
    /// as long as remote I/O takes, and WAL growth during that window is
    /// proportional to unrelated concurrent desired-state writes rather
    /// than bounded by the pinned operation itself. The measured behavior
    /// here (checkpoint blocked while pinned, unblocked after drop) is the
    /// reason the engine's desired-state operation provides an explicit
    /// consuming boundary
    /// that releases the pin before a composite operation enters mutating
    /// reconciliation. Read-only operations may retain the pin while
    /// streaming remote I/O and release it when the operation ends.
    #[test]
    fn pinned_desired_snapshot_blocks_wal_checkpoint_until_dropped() {
        let tmp = git_repo();
        let repo = Repo::at(tmp.path().to_path_buf());

        // Establish the database and put a small amount of desired state
        // in it so the pinned reader has something to pin against.
        let mut writer = StateStore::open(&repo).unwrap();
        writer
            .desired_write(|desired| -> Result<()> {
                desired.upsert_entries(
                    &[entry("seed.txt", 1, 1)],
                    crate::lock::LockShardLevels::FLAT,
                )?;
                Ok(())
            })
            .unwrap();

        // Simulate the long-running operation's coherent-snapshot barrier:
        // open a second connection and pin its desired-state snapshot,
        // exactly as the engine's coherent desired-operation acquisition
        // does before selection begins.
        let reader = StateStore::open(&repo).unwrap();
        reader.pin_snapshot().unwrap();

        // While the reader's snapshot is pinned, simulate concurrent
        // desired-state commits from unrelated activity (a second process
        // running `gat add`/`gat mv` while the slow transfer proceeds).
        // A single commit with many distinct rows is enough to grow the
        // WAL by several pages -- what matters for this test is that the
        // WAL cannot be truncated while pinned, not how many separate
        // commits produced that growth.
        let concurrent_entries: Vec<Entry> = (0..200u32)
            .map(|i| entry(&format!("concurrent-{i}.txt"), (i % 250) as u8, 1))
            .collect();
        writer
            .desired_write(|desired| -> Result<()> {
                desired.upsert_entries(
                    &concurrent_entries,
                    crate::lock::LockShardLevels::new(0).unwrap(),
                )?;
                Ok(())
            })
            .unwrap();

        let db_path = repo.materialized_db_path();
        let wal_path = db_path.with_file_name(format!(
            "{}-wal",
            db_path.file_name().unwrap().to_string_lossy()
        ));
        let wal_size_while_pinned = std::fs::metadata(&wal_path).map_or(0, |m| m.len());
        // The WAL must have grown from the 200 concurrent commits -- proving
        // growth while a reader is pinned is proportional to *concurrent
        // writer* activity, not to anything the pinned reader itself did.
        assert!(
            wal_size_while_pinned > 0,
            "expected concurrent commits to grow the WAL while a reader is pinned"
        );

        // A checkpoint attempted by the writer while the pinned reader is
        // still open must not be able to truncate the WAL: SQLite cannot
        // discard frames a live reader may still need. This checkpoint is
        // *expected* to be unable to proceed, so drop the writer's
        // `busy_timeout` to 0 just for this call -- otherwise SQLite would
        // busy-wait for the full (production, 10s) `busy_timeout` before
        // giving up, since it can't get the exclusive access TRUNCATE mode
        // needs while the reader's snapshot is pinned.
        writer.conn.pragma_update(None, "busy_timeout", 0).unwrap();
        writer.checkpoint_and_truncate_wal().unwrap();
        let wal_size_still_pinned = std::fs::metadata(&wal_path).map_or(0, |m| m.len());
        assert!(
            wal_size_still_pinned > 0,
            "checkpoint must not truncate the WAL while a reader's snapshot is pinned"
        );

        // Dropping the pinned reader rolls back its read-only transaction
        // (SQLite implicitly rolls back an open, unmodified transaction on
        // close), releasing the oldest frame the checkpoint needed to keep.
        drop(reader);

        writer.checkpoint_and_truncate_wal().unwrap();
        let wal_size_after_release = std::fs::metadata(&wal_path).map_or(0, |m| m.len());
        assert!(
            wal_size_after_release < wal_size_still_pinned,
            "checkpoint after releasing the pinned reader must shrink the WAL \
             (was {wal_size_still_pinned}, now {wal_size_after_release})"
        );
    }
}
