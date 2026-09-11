//! Shared, cross-repo local proof cache (`<objects_dir>/cache.sqlite3`):
//! reusable stat-proof persistence and the one correctness-sensitive
//! verification boundary for a cached object.
//!
//! ## Cache DB
//!
//! ```sql
//! CREATE TABLE objects (
//!     oid    BLOB PRIMARY KEY,
//!     proof  BLOB NOT NULL
//! ) WITHOUT ROWID;
//! ```
//!
//! `oid` is the raw 32-byte blake3 digest (see [`gat_core::oid::Oid`]);
//! `proof` is a [`crate::file_state::StatProof`] encoded through
//! [`crate::file_state::encode_stat_proof`]/[`crate::file_state::decode_stat_proof`],
//! the same versioned wire codec this cache format uses. `SQLite` itself
//! never learns anything about proof internals, and never sees size,
//! mtime, a path, repo identity, remote presence, GC
//! reachability, or a secondary hash directly -- those all stay opaque
//! bytes inside `proof`, decoded/encoded only in this module. [`CacheState`]
//! is the only thing here that knows this SQL; every other caller only
//! ever sees [`gat_core::oid::Oid`]/[`crate::file_state::StatProof`].
//!
//! This database is purely a derived accelerator over content that is
//! already durably present and content-addressed on disk (the local
//! object cache, see [`crate::cache::object`]): losing, deleting, or
//! corrupting it can only ever cost performance (an extra hash), never
//! object correctness or availability. It is never a GC root -- nothing
//! here ever walks `objects` to decide what a `gc` may reclaim.
//!
//! ## Failure and concurrency behavior
//!
//! `cache.sqlite3` is shared across every repo and process that points
//! at the same object root, so it must degrade gracefully rather than
//! ever fail an otherwise-valid access:
//!
//! - a missing database, or one with no row for a given oid, is
//!   equivalent to "no proof yet" -- the caller just has to hash;
//! - a malformed/unrecognised database (not a `SQLite` file, corrupt
//!   pages, a `PRAGMA user_version` newer than this build understands,
//!   WAL unsupported on this filesystem, ...) makes [`CacheState::open_prepared`]
//!   return a *disabled* [`CacheState`] -- every method on it then
//!   becomes a harmless no-op -- rather than ever downgrading, deleting,
//!   or rewriting a database this build doesn't fully understand, and
//!   rather than ever failing the invocation that asked for it;
//! - a malformed/unknown proof BLOB decodes as "no proof", exactly like
//!   a missing row -- see [`crate::file_state::decode_stat_proof`];
//! - [`crate::CacheClient::verify`] never lets a [`CacheState`] read/write
//!   failure turn into a failure to access an object whose bytes are
//!   otherwise verified: every store call it makes on the caller's
//!   behalf is best-effort.
//!
//! Same-oid concurrent [`CacheState::upsert`] calls are benign: every
//! proof a caller ever persists must first come from a verified,
//! coherent observation (see [`crate::file_state::coherent_observation`]),
//! so two processes racing to write the same oid's proof either agree or
//! (worst case) leave behind the other's equally-valid proof -- there is
//! no unsafe interleaving to guard against beyond `SQLite`'s own row-level
//! atomicity.
//!
//! [`CacheState::lookup`]/[`CacheState::upsert`]/[`CacheState::remove`]
//! are wired into [`crate::cache::object::finalize_tmp`]:
//! verified publication seeds a proof without a
//! second content hash, and a destination with no reusable proof is
//! replaced by an already-verified temp rather than being re-hashed.
//! [`ObjectVerification`] is consumed through the operation-scoped
//! [`crate::CacheClient`] capability.
use crate::cache::object::{cache_path_oid, hash_file_oid};
use crate::file_state::{
    StatProof, coherent_observation, decode_stat_proof, encode_stat_proof,
    observe_regular_file_no_follow,
};
use gat_core::oid::Oid;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, CacheStateError>;

/// Failures opening/using the shared `cache.sqlite3` proof database --
/// distinct from [`crate::cache::object::CacheError`] (the local object
/// cache's own filesystem-level failures).
///
/// Almost every one of these is caught and
/// degraded to a disabled, always-successful no-op cache by
/// [`CacheState::open_prepared`]/[`crate::CacheClient::verify`] rather than ever
/// reaching a caller -- this type exists so that degrade-gracefully
/// decision, and the few paths that *do* propagate (`open_strict`, an
/// explicit [`CacheState::apply_many`] flush), have a real typed
/// error to work with instead of an opaque `anyhow` string.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CacheStateError {
    /// The database file could not be opened at all.
    #[error("could not open the local cache database `{}`", path.display())]
    ConnectionFailed {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    /// The database's `PRAGMA user_version` names a schema newer or
    /// older than this build knows how to read -- left completely
    /// untouched, rather than ever being
    /// dropped/rebuilt/downgraded.
    #[error(
        "the local cache database `{}` has schema version {found} (this build supports {supported})",
        path.display()
    )]
    UnsupportedSchemaVersion {
        path: PathBuf,
        found: i64,
        supported: i64,
    },
    /// `PRAGMA integrity_check` reported real corruption on a database
    /// that otherwise opened and matched this build's schema version.
    #[error(
        "the local cache database `{}` failed its integrity check ({result})",
        path.display()
    )]
    IntegrityCheckFailed { path: PathBuf, result: String },
    /// A specific SQL statement/transaction step failed against an
    /// already-open connection (e.g. a mid-operation disk I/O error).
    /// `operation` names the step (e.g. "looking up cache proof") for a
    /// precise message without needing a distinct variant per statement.
    #[error("{operation} in the local cache database failed")]
    QueryFailed {
        operation: &'static str,
        #[source]
        source: rusqlite::Error,
    },
    /// An oid failed hex validation while computing its cache path.
    /// Virtually unreachable in practice -- see
    /// [`gat_core::oid::OidFormatError`]'s own docs -- since every
    /// real caller here already holds an oid validated on ingest.
    #[error("could not compute a local cache path for an object identifier")]
    InvalidObjectIdentifier {
        #[source]
        source: gat_core::oid::OidFormatError,
    },
    /// A filesystem-level (not `SQLite`) failure verifying an
    /// already-published cache entry -- reuses
    /// [`crate::cache::object::CacheError`]'s own classification
    /// directly rather than duplicating it.
    #[error(transparent)]
    Cache(#[from] crate::cache::object::CacheError),

    /// A coherent hash of an object's cache path detected the file
    /// changed out from under it, or vanished/became irregular between
    /// the pre- and post-hash stat.
    #[error(transparent)]
    FileState(#[from] crate::file_state::FileStateError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheProofErrorKind {
    Unavailable,
    Incompatible,
    Corrupt,
    InvalidObjectIdentifier,
}

#[derive(Debug)]
pub struct CacheProofError {
    kind: CacheProofErrorKind,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl CacheProofError {
    #[must_use]
    pub const fn kind(&self) -> CacheProofErrorKind {
        self.kind
    }
}

impl From<CacheStateError> for CacheProofError {
    fn from(source: CacheStateError) -> Self {
        let kind = match source {
            CacheStateError::UnsupportedSchemaVersion { .. } => CacheProofErrorKind::Incompatible,
            CacheStateError::IntegrityCheckFailed { .. } => CacheProofErrorKind::Corrupt,
            CacheStateError::InvalidObjectIdentifier { .. } => {
                CacheProofErrorKind::InvalidObjectIdentifier
            }
            CacheStateError::ConnectionFailed { .. }
            | CacheStateError::QueryFailed { .. }
            | CacheStateError::Cache(_)
            | CacheStateError::FileState(_) => CacheProofErrorKind::Unavailable,
        };
        Self {
            kind,
            source: Box::new(source),
        }
    }
}

impl std::fmt::Display for CacheProofError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "local cache proof store is {:?}", self.kind)
    }
}

impl std::error::Error for CacheProofError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

impl From<gat_core::oid::OidFormatError> for CacheStateError {
    fn from(source: gat_core::oid::OidFormatError) -> Self {
        Self::InvalidObjectIdentifier { source }
    }
}

/// The filename the shared proof cache always uses, directly under the
/// content-addressed objects directory it accelerates.
const CACHE_DB_FILENAME: &str = "cache.sqlite3";

/// The schema version this build of `gat` writes and requires. Unlike
/// the repository-local materialized-state database, an
/// unrecognised version here is never dropped/rebuilt: this file is
/// shared by every repo/process pointed at the same object root, so a
/// newer schema this build doesn't understand must be left completely
/// untouched.
pub const SCHEMA_VERSION: i64 = 1;

/// Bounded wait for another process's write lock, matching
/// the materialized-state store's own busy timeout.
const BUSY_TIMEOUT_MS: u32 = 10_000;

/// Total bound-variable budget a single chunked statement should stay
/// under -- comfortably below `SQLite`'s default
/// `SQLITE_LIMIT_VARIABLE_NUMBER` (32766 as of `SQLite` 3.32). Mirrors the
/// repository-local materialized store's budget: a
/// logical set-based proof read/write over N oids maps to
/// `O(N / SQL_CHUNK)` physical statements, never one statement per oid,
/// even when N exceeds `SQLite`'s bind ceiling.
const SQL_BIND_BUDGET: usize = 30_000;

/// Row-chunk size for a chunked multi-row statement that binds
/// `binds_per_row` variables per row -- a single-column `IN (...)`/exact
/// lookup (1 bind/row) can batch far more oids per statement than a
/// two-column proof upsert (2 binds/row) before hitting the same
/// bind-variable ceiling.
const fn sql_chunk_size(binds_per_row: usize) -> usize {
    SQL_BIND_BUDGET / binds_per_row
}

/// Bounds how many deltas a single [`CacheState::apply_many`] mutation
/// transaction covers, independent of `SQL_BIND_BUDGET`/`sql_chunk_size`
/// (which only bound how many *physical statements* one transaction maps
/// onto). Mutation transactions themselves stay bounded and short, not
/// merely the SQL inside them, so `apply_many` must
/// enforce this itself rather than trusting every caller to already pass
/// a small enough `deltas` slice. Matches `cache::VERIFY_WINDOW`, the
/// per-window bound most callers already use, so windowed callers keep
/// committing exactly one transaction per call.
const TRANSACTION_CHUNK: usize = 4096;

/// `n` copies of `unit` (e.g. `"?"` or `"(?, ?)"`), comma-joined, for a
/// chunked multi-row `IN (...)`/`VALUES (...), (...)` statement.
fn sql_placeholders(unit: &str, n: usize) -> String {
    vec![unit; n].join(", ")
}

/// A single pending change to the shared proof index, produced by the
/// pure filesystem verifier (`verify_object_fs`) or a DB-free
/// publication ([`crate::cache::object::publish_tmp`]) and applied later,
/// in bounded set-based batches, through [`CacheState::apply_many`]. This
/// is what lets parallel filesystem/network workers decide *what* a
/// proof row should become without any of them independently touching
/// `SQLite`: they emit deltas, and the single operation
/// thread persists them.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProofMutation {
    /// Persist (insert or replace) a freshly verified, coherently
    /// observed proof for this oid.
    Upsert(Oid, StatProof),
    /// Drop any persisted proof for this oid (a stale/undecodable proof,
    /// or one whose object turned out corrupt).
    Remove(Oid),
}

/// Opaque evidence that a verified cache publication or observation
/// requires one proof-index mutation. Callers may collect and return
/// receipts, but only the cache implementation can inspect their
/// SQLite-facing mutation.
#[derive(Clone, PartialEq, Eq)]
pub struct CachePublication(ProofMutation);

impl std::fmt::Debug for CachePublication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CachePublication(..)")
    }
}

impl CachePublication {
    pub(super) const fn upsert(oid: Oid, proof: StatProof) -> Self {
        Self(ProofMutation::Upsert(oid, proof))
    }

    pub(super) const fn remove(oid: Oid) -> Self {
        Self(ProofMutation::Remove(oid))
    }

    pub(super) const fn oid(&self) -> Oid {
        match self.0 {
            ProofMutation::Upsert(oid, _) | ProofMutation::Remove(oid) => oid,
        }
    }
}

/// A short-lived connection to the shared `cache.sqlite3` proof
/// database, opened once per invocation via [`CacheState::open_prepared`]. May be
/// *disabled* (holding no connection at all) when the database couldn't
/// be safely opened or understood, in which case
/// every method below is a harmless, always-successful no-op, and every
/// caller falls back to actually hashing instead of trusting anything.
pub(crate) struct CacheState {
    conn: RefCell<Option<Connection>>,
}

/// Per-connection pragmas, applied on every open.
fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA trusted_schema = OFF;"
    ))
    .map_err(|source| CacheStateError::QueryFailed {
        operation: "configuring cache database connection",
        source,
    })
}

/// Best-effort switch to WAL journal mode. Unlike
/// the repository-local materialized-state database (which bails
/// if WAL isn't available), this shared cache must never fail an
/// invocation just because one filesystem doesn't support WAL's
/// shared-memory file (e.g. some network mounts): callers fall back to
/// whatever journal mode `SQLite` already defaulted to instead.
fn try_set_wal(conn: &Connection) {
    let _: rusqlite::Result<String> =
        conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0));
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS objects (
             oid    BLOB PRIMARY KEY,
             proof  BLOB NOT NULL
         ) WITHOUT ROWID;",
    )
    .map_err(|source| CacheStateError::QueryFailed {
        operation: "creating cache database schema",
        source,
    })
}

/// Open (creating the file, and its schema, if it doesn't exist yet) a
/// connection to `db_path`, failing closed with an `Err` for anything
/// [`CacheState::open_prepared`] should treat as "disable the accelerator for
/// this invocation" -- a corrupt/foreign file, a schema newer than
/// [`SCHEMA_VERSION`], or any other unexpected `SQLite` error.
fn try_open(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).map_err(|source| CacheStateError::ConnectionFailed {
        path: db_path.to_path_buf(),
        source,
    })?;
    configure(&conn)?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|source| CacheStateError::QueryFailed {
            operation: "reading cache database schema version",
            source,
        })?;
    if version == 0 {
        // A fresh/empty database file: initialize it in place.
        try_set_wal(&conn);
        create_schema(&conn)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(|source| CacheStateError::QueryFailed {
                operation: "stamping cache database schema version",
                source,
            })?;
    } else if version != SCHEMA_VERSION {
        // Either newer than this build understands, or an older schema
        // version this build does not support -- either way,
        // never touch it: just refuse to use it for
        // this invocation.
        return Err(CacheStateError::UnsupportedSchemaVersion {
            path: db_path.to_path_buf(),
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(conn)
}

impl CacheState {
    pub(crate) const fn disabled() -> Self {
        Self {
            conn: RefCell::new(None),
        }
    }

    /// Open the shared proof cache using evidence of prepared storage.
    /// Database errors disable the accelerator without failing the operation.
    /// If the object directory is absent, no database can be opened and the
    /// accelerator is likewise disabled.
    ///
    /// Operation clients retain this connection for reuse; preparation evidence
    /// prevents callers from opening a writable database before protecting its
    /// repository-owned storage.
    pub(crate) fn open_prepared(directory: &super::root::PreparedCacheDirectory<'_>) -> Self {
        Self::open_at(directory.path())
    }

    /// Raw-path constructor for isolated proof-store fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_for_test(objects_dir: &Path) -> Self {
        Self::open_at(objects_dir)
    }

    fn open_at(objects_dir: &Path) -> Self {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_cache_db_open();
        let db_path = objects_dir.join(CACHE_DB_FILENAME);
        let conn = try_open(&db_path).ok();
        Self {
            conn: RefCell::new(conn),
        }
    }

    /// Open `objects_dir`'s cache database *strictly*: unlike [`open`]
    /// (which degrades any open/configure/schema problem to a disabled,
    /// always-successful no-op cache), this propagates the underlying
    /// error instead. Also runs `PRAGMA integrity_check` so a rebuilt
    /// database is only reported healthy once it has actually been
    /// verified, not merely opened.
    ///
    /// Intended only for `gat system repair cache`'s post-rebuild
    /// validation, where reporting success on a database that would
    /// silently degrade to "disabled" on the very next real use would be
    /// a false success -- never for the ordinary ingest/lookup path,
    /// which must keep degrading gracefully via [`open`].
    ///
    /// [`open`]: CacheState::open_prepared
    pub(crate) fn open_strict(directory: &super::root::PreparedCacheDirectory<'_>) -> Result<Self> {
        let objects_dir = directory.path();
        let db_path = objects_dir.join(CACHE_DB_FILENAME);
        let conn = try_open(&db_path)?;
        let result: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(|source| CacheStateError::QueryFailed {
                operation: "verifying rebuilt cache database",
                source,
            })?;
        if result != "ok" {
            return Err(CacheStateError::IntegrityCheckFailed {
                path: db_path,
                result,
            });
        }
        Ok(Self {
            conn: RefCell::new(Some(conn)),
        })
    }

    /// Look up the persisted proof for `oid`, if any. A
    /// disabled state, an absent row, or a malformed/unrecognised proof
    /// BLOB (see [`decode_stat_proof`]) are all indistinguishable here:
    /// each simply means "no proof", never an error.
    pub fn lookup(&self, oid: &Oid) -> Result<Option<StatProof>> {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_proof_lookup_request();
        let conn_ref = self.conn.borrow();
        let Some(conn) = conn_ref.as_ref() else {
            return Ok(None);
        };
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_proof_lookup_statement();
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT proof FROM objects WHERE oid = ?1",
                params![oid.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|source| CacheStateError::QueryFailed {
                operation: "looking up cache proof",
                source,
            })?;
        #[cfg(any(test, feature = "test-support"))]
        if bytes.is_some() {
            test_support::record_proof_lookup_row();
        }
        Ok(bytes.and_then(|bytes| decode_stat_proof(&bytes)))
    }

    /// Look up the persisted proofs for exactly `oids` in one logical
    /// request, mapped onto `O(N / SQL_CHUNK)` bounded set-based
    /// `SELECT ... WHERE oid IN (...)` statements rather than one point
    /// query per oid. A disabled state yields an empty map;
    /// oids with no row, or a malformed/unrecognised proof BLOB, are
    /// simply absent from the result, exactly like [`lookup`].
    ///
    /// [`lookup`]: CacheState::lookup
    pub fn exact_many(&self, oids: &[Oid]) -> Result<HashMap<Oid, StatProof>> {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_proof_lookup_request();
        let mut found = HashMap::new();
        if oids.is_empty() {
            return Ok(found);
        }
        let conn_ref = self.conn.borrow();
        let Some(conn) = conn_ref.as_ref() else {
            return Ok(found);
        };
        for chunk in oids.chunks(sql_chunk_size(1)) {
            let placeholders = sql_placeholders("?", chunk.len());
            let sql = format!("SELECT oid, proof FROM objects WHERE oid IN ({placeholders})");
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_proof_lookup_statement();
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|source| CacheStateError::QueryFailed {
                    operation: "preparing set-based cache proof lookup",
                    source,
                })?;
            let params: Vec<Value> = chunk.iter().map(|o| o.as_bytes().to_vec().into()).collect();
            let rows = stmt
                .query_map(params_from_iter(params.iter()), |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|source| CacheStateError::QueryFailed {
                    operation: "querying set-based cache proofs",
                    source,
                })?;
            for row in rows {
                let (oid_bytes, proof_bytes) =
                    row.map_err(|source| CacheStateError::QueryFailed {
                        operation: "reading cache proof row",
                        source,
                    })?;
                #[cfg(any(test, feature = "test-support"))]
                test_support::record_proof_lookup_row();
                let Ok(oid_arr) = <[u8; 32]>::try_from(oid_bytes.as_slice()) else {
                    continue;
                };
                if let Some(proof) = decode_stat_proof(&proof_bytes) {
                    found.insert(Oid::from_bytes(oid_arr), proof);
                }
            }
        }
        Ok(found)
    }

    /// Persist (inserting or replacing) `proof` for `oid`. A disabled
    /// state makes this a no-op success.
    #[cfg(test)]
    pub fn upsert(&self, oid: &Oid, proof: &StatProof) -> Result<()> {
        self.apply_many(&[CachePublication::upsert(*oid, *proof)])
    }

    /// Remove any persisted proof for `oid`. A disabled state, or `oid`
    /// simply having no row, are both a no-op success.
    #[cfg(test)]
    pub fn remove(&self, oid: &Oid) -> Result<()> {
        self.apply_many(&[CachePublication::remove(*oid)])
    }

    /// Remove any persisted proof for each oid in `oids`. A disabled
    /// state, an empty `oids`, or any of them simply having no row, are
    /// all a no-op success.
    pub fn remove_many(&self, oids: &[Oid]) -> Result<()> {
        if oids.is_empty() {
            return Ok(());
        }
        let deltas: Vec<CachePublication> = oids
            .iter()
            .map(|oid| CachePublication::remove(*oid))
            .collect();
        self.apply_many(&deltas)
    }

    /// Apply `deltas` (upserts and removes intermixed) to the shared
    /// proof index in bounded, short transactions (at most
    /// `TRANSACTION_CHUNK` deltas each), mapped onto `O(N / SQL_CHUNK)`
    /// bounded set-based statements per transaction -- multi-row
    /// `INSERT ... VALUES (...) ON CONFLICT` for the upserts and
    /// `DELETE ... WHERE oid IN (...)` for the removes -- rather than one
    /// statement per delta hidden inside a single commit. A
    /// disabled state, or an empty `deltas`, are a no-op success.
    ///
    /// `deltas` is first coalesced to at most one final delta per oid,
    /// keeping each oid's *last* occurrence in input order, before it is
    /// partitioned into bounded transactions. This keeps the "later write
    /// wins" precedence deterministic regardless of where a
    /// `TRANSACTION_CHUNK` boundary falls: an `Upsert(X)` followed later
    /// by a `Remove(X)` (or vice versa) resolves the same way whether
    /// both land in the same bounded transaction or straddle two of
    /// them.
    /// Test-only fault injection: makes every subsequent read/write on
    /// this already-successfully-opened connection fail with a genuine
    /// `SQLite` error (as opposed to `CacheState::open_prepared` itself never having
    /// succeeded), so tests can verify degrade-gracefully behavior for a
    /// DB that opens fine but then fails mid-operation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn break_for_test(&self) {
        let conn_ref = self.conn.borrow();
        if let Some(conn) = conn_ref.as_ref() {
            conn.execute_batch("DROP TABLE objects").unwrap();
        }
    }

    pub fn apply_many(&self, deltas: &[CachePublication]) -> Result<()> {
        if deltas.is_empty() {
            return Ok(());
        }
        let conn_ref = self.conn.borrow();
        let Some(conn) = conn_ref.as_ref() else {
            return Ok(());
        };
        // Coalesce to one final delta per oid -- keeping each oid's last
        // occurrence in input order -- *before* partitioning into bounded
        // transactions, so an oid whose last-write delta happens to fall
        // in an earlier `TRANSACTION_CHUNK` group than an earlier-write
        // delta for the same oid still resolves deterministically to the
        // logically-last write, not to whichever chunk happens to commit
        // last.
        let mut coalesced: HashMap<Oid, CachePublication> = HashMap::with_capacity(deltas.len());
        for delta in deltas {
            coalesced.insert(delta.oid(), delta.clone());
        }
        let deltas: Vec<CachePublication> = coalesced.into_values().collect();
        // Bound each mutation to at most `TRANSACTION_CHUNK` deltas so a
        // single transaction/commit never spans an entire unwindowed
        // `deltas` slice -- a caller that already passes one
        // bounded window (the common case) still commits in exactly one
        // transaction, unchanged from before.
        for group in deltas.chunks(TRANSACTION_CHUNK) {
            let mut removes: Vec<&Oid> = Vec::new();
            let mut upserts: Vec<(&Oid, &StatProof)> = Vec::new();
            for delta in group {
                match &delta.0 {
                    ProofMutation::Remove(oid) => removes.push(oid),
                    ProofMutation::Upsert(oid, proof) => upserts.push((oid, proof)),
                }
            }
            let tx =
                conn.unchecked_transaction()
                    .map_err(|source| CacheStateError::QueryFailed {
                        operation: "beginning cache proof mutation",
                        source,
                    })?;
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_proof_mutation_transaction();
            for chunk in removes.chunks(sql_chunk_size(1)) {
                let placeholders = sql_placeholders("?", chunk.len());
                #[cfg(any(test, feature = "test-support"))]
                test_support::record_proof_mutation_statement();
                let params: Vec<Value> =
                    chunk.iter().map(|o| o.as_bytes().to_vec().into()).collect();
                let affected = tx
                    .execute(
                        &format!("DELETE FROM objects WHERE oid IN ({placeholders})"),
                        params_from_iter(params.iter()),
                    )
                    .map_err(|source| CacheStateError::QueryFailed {
                        operation: "removing cache proofs",
                        source,
                    })?;
                #[cfg(any(test, feature = "test-support"))]
                test_support::record_proof_mutation_rows(affected);
                let _ = affected;
            }
            for chunk in upserts.chunks(sql_chunk_size(2)) {
                let values = sql_placeholders("(?, ?)", chunk.len());
                #[cfg(any(test, feature = "test-support"))]
                test_support::record_proof_mutation_statement();
                let mut params: Vec<Value> = Vec::with_capacity(chunk.len() * 2);
                for (oid, proof) in chunk {
                    params.push(oid.as_bytes().to_vec().into());
                    params.push(encode_stat_proof(proof).to_vec().into());
                }
                let affected = tx
                    .execute(
                        &format!(
                            "INSERT INTO objects (oid, proof) VALUES {values}
                             ON CONFLICT (oid) DO UPDATE SET proof = excluded.proof"
                        ),
                        params_from_iter(params.iter()),
                    )
                    .map_err(|source| CacheStateError::QueryFailed {
                        operation: "upserting cache proofs",
                        source,
                    })?;
                #[cfg(any(test, feature = "test-support"))]
                test_support::record_proof_mutation_rows(affected);
                let _ = affected;
            }
            tx.commit().map_err(|source| CacheStateError::QueryFailed {
                operation: "committing cache proof mutation",
                source,
            })?;
        }
        Ok(())
    }
}

/// The outcome of [`crate::CacheClient::verify`]: whether the on-disk object
/// at `oid`'s cache path is absent, verified to contain `oid`'s exact
/// bytes, or present but proven *not* to be `oid` (or not even a regular
/// file at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectVerification {
    /// Nothing exists at `oid`'s cache path.
    Missing,
    /// The object exists and its content identity is verified (either
    /// trusted purely from a matching stat proof, or freshly confirmed
    /// by hashing) to be `oid`.
    Valid,
    /// Something exists at `oid`'s cache path but is not usable as that
    /// object: a symlink/directory/special file, or a regular file whose
    /// hashed content doesn't actually match `oid`. Any stale proof row
    /// for `oid` has already been removed.
    Corrupt,
}

/// One filesystem observation, not a guarantee against later external changes.
/// Validity and its required metadata are established together by the verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheObservation {
    Missing,
    Corrupt,
    Valid { size: u64 },
}

impl CacheObservation {
    pub(super) const fn status(self) -> ObjectVerification {
        match self {
            Self::Missing => ObjectVerification::Missing,
            Self::Corrupt => ObjectVerification::Corrupt,
            Self::Valid { .. } => ObjectVerification::Valid,
        }
    }

    pub(super) const fn verified_size(self) -> Option<u64> {
        match self {
            Self::Valid { size } => Some(size),
            Self::Missing | Self::Corrupt => None,
        }
    }
}

/// The pure filesystem/content step of cache-object verification,
/// separated from the shared proof index so object stat/hash work can run
/// in parallel without any worker touching `SQLite`. Given an
/// optional `prior` proof (whatever the operation-scoped proof index
/// already knew for `oid`, looked up once in a bounded set-based batch by
/// the caller), it establishes the object's [`CacheObservation`] and returns
/// the [`CachePublication`] that should later be persisted for it, if any:
///
/// ```text
/// current stat matches prior proof exactly => Valid, zero hash, no delta
/// no/mismatched/undecodable prior proof     => one coherent hash of the object
///      coherent hash == expected OID => Valid; Upsert delta
///      coherent hash != expected OID => Corrupt; Remove delta
///      observed pre/post change      => error (fail closed, no retry)
/// missing                                     => Missing, no delta
/// symlink/directory/special file              => Corrupt; Remove delta
/// ```
///
/// This performs no `SQLite` I/O whatsoever -- it only reads the `prior`
/// value handed to it and reports what the index *should* become, leaving
/// every actual read/write batched and serialized on the operation thread.
pub(super) fn verify_object_fs(
    objects_dir: &Path,
    oid: &Oid,
    prior: Option<&StatProof>,
) -> Result<(CacheObservation, Option<CachePublication>)> {
    let path = cache_path_oid(objects_dir, oid);
    verify_object_path_fs(&path, oid, prior)
}

pub(super) fn verify_object_path_fs(
    path: &Path,
    oid: &Oid,
    prior: Option<&StatProof>,
) -> Result<(CacheObservation, Option<CachePublication>)> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((CacheObservation::Missing, None));
        }
        Err(e) => {
            return Err(crate::cache::object::CacheError::EntryUnreadable {
                path: path.to_path_buf(),
                source: e,
            }
            .into());
        }
    };
    if !meta.is_file() {
        // A symlink, directory, or other special file at an object's
        // cache path is never trustworthy content, regardless of any
        // proof that might still be on record for it.
        return Ok((
            CacheObservation::Corrupt,
            Some(CachePublication::remove(*oid)),
        ));
    }

    if let (Some(prior), Some(current)) = (prior, observe_regular_file_no_follow(path))
        && current.matches(prior)
    {
        return Ok((CacheObservation::Valid { size: current.size }, None));
    }

    // The stat cache couldn't prove identity on its own: perform exactly
    // one coherent hash of the object bytes. An observed pre/post change
    // fails closed immediately -- there is no retry.
    let observation = coherent_observation(path, || {
        #[cfg(any(test, feature = "test-support"))]
        race_test_hooks::fire_before_hash(path);
        hash_file_oid(path).map_err(CacheStateError::from)
    })?;

    if observation.value == *oid {
        Ok((
            CacheObservation::Valid {
                size: observation.proof.size,
            },
            Some(CachePublication::upsert(*oid, observation.proof)),
        ))
    } else {
        Ok((
            CacheObservation::Corrupt,
            Some(CachePublication::remove(*oid)),
        ))
    }
}

/// Test-only deterministic race injection for `verify_object_fs`'s
/// content hash. It lets tests prove a cache object rewritten mid-hash fails
/// `verify_object_fs` closed instead of reporting `Valid`, without
/// depending on a real, inherently flaky thread-timing race.
#[cfg(any(test, feature = "test-support"))]
mod race_test_hooks {
    use std::path::Path;
    use std::sync::Mutex;

    type Hook = Box<dyn FnMut(&Path) + Send>;

    // Process-wide (not thread-local): `verify_object_fs`'s content hash
    // may run on any Rayon worker thread, not necessarily the test's own
    // thread.
    static BEFORE_HASH: Mutex<Option<Hook>> = Mutex::new(None);

    /// Install a hook that runs immediately before the content hash.
    /// Tests must pair this with [`clear`] (a guard is recommended) so
    /// the hook never leaks into an unrelated test running later in the
    /// same process. Because this hook is process-wide, callers must
    /// filter on the exact path they expect inside their closure.
    #[allow(dead_code)]
    pub(super) fn set(hook: impl FnMut(&Path) + Send + 'static) {
        *BEFORE_HASH.lock().unwrap() = Some(Box::new(hook));
    }

    /// Remove any installed hook.
    #[allow(dead_code)]
    pub(super) fn clear() {
        *BEFORE_HASH.lock().unwrap() = None;
    }

    pub(super) fn fire_before_hash(path: &Path) {
        if let Some(hook) = BEFORE_HASH.lock().unwrap().as_mut() {
            hook(path);
        }
    }
}

/// Test-only structural instrumentation for the shared proof index's
/// access plan: not timing benchmarks, but counters a test can
/// assert against to catch a regression back into per-object DB opens,
/// per-oid point queries, or per-row mutation statements the way plain
/// input/output equivalence tests cannot. Thread-local because
/// `cargo test` runs tests concurrently on separate threads; a global
/// counter would be racy across tests. Every ordinary proof read/write is
/// serialized on the operation thread (parallel workers only ever run the
/// pure `verify_object_fs` filesystem step), so a same-thread snapshot
/// before/after the call under test sees a faithful count.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static CACHE_DB_OPENS: Cell<usize> = const { Cell::new(0) };
        static PROOF_LOOKUP_REQUESTS: Cell<usize> = const { Cell::new(0) };
        static PROOF_LOOKUP_STATEMENTS: Cell<usize> = const { Cell::new(0) };
        static PROOF_LOOKUP_ROWS: Cell<usize> = const { Cell::new(0) };
        static PROOF_MUTATION_STATEMENTS: Cell<usize> = const { Cell::new(0) };
        static PROOF_MUTATION_TRANSACTIONS: Cell<usize> = const { Cell::new(0) };
        static PROOF_MUTATION_ROWS: Cell<usize> = const { Cell::new(0) };
        static FS_VERIFICATIONS: Cell<usize> = const { Cell::new(0) };
        static MEMO_HITS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn record_cache_db_open() {
        CACHE_DB_OPENS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn record_proof_lookup_request() {
        PROOF_LOOKUP_REQUESTS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn record_proof_lookup_statement() {
        PROOF_LOOKUP_STATEMENTS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn record_proof_lookup_row() {
        PROOF_LOOKUP_ROWS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn record_proof_mutation_statement() {
        PROOF_MUTATION_STATEMENTS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn record_proof_mutation_transaction() {
        PROOF_MUTATION_TRANSACTIONS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn record_proof_mutation_rows(rows: usize) {
        PROOF_MUTATION_ROWS.with(|c| c.set(c.get() + rows));
    }

    /// One dispatched filesystem verification of an oid (the pure
    /// [`super::verify_object_fs`] step), recorded on the operation
    /// thread at dispatch so a deduplicated/memoized batch counts each
    /// unique oid at most once even though the step itself may run on a
    /// parallel worker.
    pub(crate) fn record_fs_verification() {
        FS_VERIFICATIONS.with(|c| c.set(c.get() + 1));
    }

    /// One reuse of an already-verified status for an oid, instead of
    /// re-running the filesystem verifier (see
    /// [`crate::CacheClient`]).
    pub(crate) fn record_memo_hit() {
        MEMO_HITS.with(|c| c.set(c.get() + 1));
    }

    /// A structural snapshot on this thread so far; a test reads it before
    /// and after the call under test and asserts on the delta, rather than
    /// on an absolute count shared across the whole test binary.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Counts {
        pub cache_db_opens: usize,
        pub proof_lookup_requests: usize,
        pub proof_lookup_statements: usize,
        pub proof_lookup_rows: usize,
        pub proof_mutation_statements: usize,
        pub proof_mutation_transactions: usize,
        pub proof_mutation_rows: usize,
        pub fs_verifications: usize,
        pub memo_hits: usize,
    }

    pub fn snapshot() -> Counts {
        Counts {
            cache_db_opens: CACHE_DB_OPENS.with(Cell::get),
            proof_lookup_requests: PROOF_LOOKUP_REQUESTS.with(Cell::get),
            proof_lookup_statements: PROOF_LOOKUP_STATEMENTS.with(Cell::get),
            proof_lookup_rows: PROOF_LOOKUP_ROWS.with(Cell::get),
            proof_mutation_statements: PROOF_MUTATION_STATEMENTS.with(Cell::get),
            proof_mutation_transactions: PROOF_MUTATION_TRANSACTIONS.with(Cell::get),
            proof_mutation_rows: PROOF_MUTATION_ROWS.with(Cell::get),
            fs_verifications: FS_VERIFICATIONS.with(Cell::get),
            memo_hits: MEMO_HITS.with(Cell::get),
        }
    }
}

/// Deterministic cache-object construction that bypasses [`ingest`] so owner
/// tests can begin without accidentally creating a proof.
#[cfg(test)]
pub mod fixture {
    use super::Oid;
    use crate::cache::object::{cache_path_oid, protect};
    use std::path::PathBuf;

    pub(crate) struct CacheFixture {
        objects_dir: PathBuf,
    }

    impl CacheFixture {
        pub(crate) fn new(objects_dir: impl Into<PathBuf>) -> Self {
            Self {
                objects_dir: objects_dir.into(),
            }
        }

        fn path_for(&self, oid: &Oid) -> PathBuf {
            cache_path_oid(&self.objects_dir, oid)
        }

        /// Writes `bytes` directly to their content-addressed cache
        /// location and applies production-equivalent file protection,
        /// without calling [`ingest`](crate::cache::object::ingest) or
        /// opening [`CacheState`] -- guaranteeing this can never
        /// incidentally create a proof as a side effect, regardless of
        /// scheduling.
        pub(crate) fn publish_cold(&self, bytes: &[u8]) -> Oid {
            std::fs::create_dir_all(&self.objects_dir).unwrap();
            let oid = Oid::from_bytes(*blake3::hash(bytes).as_bytes());
            let path = self.path_for(&oid);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            protect(&path).unwrap();
            oid
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::object::{
        CacheClient, cache_path_oid, hash_file_call_count, with_exclusive_hash_file_call_count,
    };
    use std::fs;

    /// RAII guard clearing [`race_test_hooks`] on drop (including on
    /// panic/early return), so a test that injects a mid-hash race can
    /// never leak its hook into a later test sharing the same OS thread.
    struct RaceHookGuard;

    impl Drop for RaceHookGuard {
        fn drop(&mut self) {
            race_test_hooks::clear();
        }
    }

    /// Writes `bytes` straight to their content-addressed cache path,
    /// deliberately *not* going through [`crate::cache::object::ingest`]
    /// -- delegates to the shared [`fixture::CacheFixture::publish_cold`]
    /// rather than duplicating its bypass-`ingest`
    /// rationale here; see that method's doc comment for exactly why.
    fn write_object(objects_dir: &Path, bytes: &[u8]) -> Oid {
        fixture::CacheFixture::new(objects_dir).publish_cold(bytes)
    }

    fn verify_through_client(objects_dir: &Path, oid: &Oid) -> ObjectVerification {
        CacheClient::open(objects_dir.to_path_buf())
            .verify(oid)
            .unwrap()
    }

    #[test]
    fn open_creates_exactly_cache_sqlite3() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let _state = CacheState::open_for_test(&objects_dir);
        // WAL mode legitimately adds `-wal`/`-shm` sidecar files
        // alongside the main database file; the requirement is that
        // exactly one *database* is created, not that no sidecars ever
        // exist.
        let mut entries: Vec<_> = fs::read_dir(&objects_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with("-wal") && !name.ends_with("-shm"))
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["cache.sqlite3".to_string()]);
    }

    #[test]
    fn schema_is_exactly_oid_proof_without_rowid() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let _state = CacheState::open_for_test(&objects_dir);
        let conn = Connection::open(objects_dir.join("cache.sqlite3")).unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'objects'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("oid    BLOB PRIMARY KEY"));
        assert!(sql.contains("proof  BLOB NOT NULL"));
        assert!(sql.contains("WITHOUT ROWID"));
    }

    #[test]
    fn lookup_upsert_remove_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);
        let oid = Oid::from_hex(&"ab".repeat(32)).unwrap();
        assert_eq!(state.lookup(&oid).unwrap(), None);

        let proof = StatProof {
            size: 5,
            mtime_secs: 1234,
            mtime_nanos: 0,
        };
        state.upsert(&oid, &proof).unwrap();
        assert_eq!(state.lookup(&oid).unwrap(), Some(proof));

        state.remove(&oid).unwrap();
        assert_eq!(state.lookup(&oid).unwrap(), None);
    }

    #[test]
    fn remove_many_removes_only_listed_oids() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);
        let oid_a = Oid::from_hex(&"aa".repeat(32)).unwrap();
        let oid_b = Oid::from_hex(&"bb".repeat(32)).unwrap();
        let proof = StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 0,
        };
        state.upsert(&oid_a, &proof).unwrap();
        state.upsert(&oid_b, &proof).unwrap();

        state.remove_many(&[oid_a]).unwrap();
        assert_eq!(state.lookup(&oid_a).unwrap(), None);
        assert_eq!(state.lookup(&oid_b).unwrap(), Some(proof));
    }

    #[test]
    fn missing_object_is_reported_as_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let oid = Oid::from_hex(&"cc".repeat(32)).unwrap();
        assert_eq!(
            verify_through_client(&objects_dir, &oid),
            ObjectVerification::Missing
        );
    }

    #[test]
    fn no_prior_proof_hashes_once_then_reuses_stat_proof_warm() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"hello world");
        let state = CacheState::open_for_test(&objects_dir);

        assert_eq!(state.lookup(&oid).unwrap(), None);
        let status = verify_through_client(&objects_dir, &oid);
        assert_eq!(status, ObjectVerification::Valid);
        // A stable hash must have persisted a reusable proof.
        assert!(state.lookup(&oid).unwrap().is_some());

        // A second verification should be able to trust the stat proof
        // alone -- confirm indirectly by making the exact-object bytes
        // unreadable-as-content-but-still-present, which would only
        // still report Valid if no re-hash happened. We instead assert
        // behaviorally: corrupting the file *without* touching its
        // stat proof would still (incorrectly) report Valid if reuse
        // wasn't happening from a stat match; the strongest simple
        // signal here is just that the proof round-trips and a repeat
        // call remains Valid without erroring.
        let status2 = verify_through_client(&objects_dir, &oid);
        assert_eq!(status2, ObjectVerification::Valid);
    }

    #[test]
    fn reusable_warm_cache_proof_avoids_rehashing() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"hello world");
        let state = CacheState::open_for_test(&objects_dir);

        assert_eq!(
            verify_through_client(&objects_dir, &oid),
            ObjectVerification::Valid
        );
        assert!(state.lookup(&oid).unwrap().is_some());
        drop(state);

        let status =
            with_exclusive_hash_file_call_count(|| verify_through_client(&objects_dir, &oid));
        assert_eq!(hash_file_call_count(), 0);
        assert_eq!(status, ObjectVerification::Valid);
    }

    #[test]
    fn mtime_drift_with_unchanged_bytes_rehashes_and_refreshes_proof() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"stable content");
        let state = CacheState::open_for_test(&objects_dir);

        // Prime a proof, then simulate stat drift (as if another process
        // touched the file's mtime without changing its bytes) by
        // storing a stale proof directly.
        let path = cache_path_oid(&objects_dir, &oid);
        let real = observe_regular_file_no_follow(&path).unwrap();
        let mut stale = real;
        stale.mtime_secs -= 100;
        state.upsert(&oid, &stale).unwrap();

        let status = verify_through_client(&objects_dir, &oid);
        assert_eq!(status, ObjectVerification::Valid);
        let refreshed = state.lookup(&oid).unwrap().unwrap();
        assert_eq!(refreshed, real);
    }

    #[test]
    fn same_size_corruption_with_changed_metadata_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"original!!!!");
        let state = CacheState::open_for_test(&objects_dir);
        // Establish a proof.
        assert_eq!(
            verify_through_client(&objects_dir, &oid),
            ObjectVerification::Valid
        );

        // Corrupt in place with same-length bytes so only mtime/proof
        // mismatch (not size) can possibly reveal the change.
        let path = cache_path_oid(&objects_dir, &oid);
        crate::cache::object::unprotect(&path).ok();
        fs::write(&path, b"CORRUPTED!!!").unwrap();

        let status = verify_through_client(&objects_dir, &oid);
        assert_eq!(status, ObjectVerification::Corrupt);
        assert_eq!(state.lookup(&oid).unwrap(), None);
    }

    /// A pre/post stat mismatch during the hash
    /// means the object mutated while being observed, so
    /// `verify_object_fs` must never report `ObjectVerification::Valid`
    /// from an observation it cannot even prove was metadata-stable, no
    /// matter what the transient hash happened to equal. Forces the race
    /// deterministically via `race_test_hooks` (a real thread-timing race
    /// would make this test flaky): the hook rewrites the object to
    /// different, same-length bytes immediately before the hash attempt,
    /// so `verify_object_fs` observes a pre/post stat mismatch and fails
    /// closed with an error instead of silently reporting `Valid`.
    #[test]
    fn verify_object_fs_fails_closed_on_a_mid_hash_rewrite_instead_of_reporting_valid() {
        let _guard = RaceHookGuard;
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"original!!!!");
        let path = cache_path_oid(&objects_dir, &oid);
        crate::cache::object::unprotect(&path).ok();

        // Rewrite the object to different, same-length bytes the instant
        // before the hash attempt -- landing squarely inside
        // `verify_object_fs`'s own `coherent_observation` window (after
        // its "before" stat, before its "after" stat).
        let target_path = path;
        race_test_hooks::set(move |p: &std::path::Path| {
            if p == target_path {
                let modified = fs::metadata(p).unwrap().modified().unwrap();
                fs::write(p, b"CORRUPTED!!!").unwrap();
                // NTFS may defer last-write updates while the reader is open.
                // Force a distinct timestamp instead of relying on clock resolution.
                fs::OpenOptions::new()
                    .write(true)
                    .open(p)
                    .unwrap()
                    .set_modified(modified + std::time::Duration::from_secs(1))
                    .unwrap();
            }
        });

        let err = verify_object_fs(&objects_dir, &oid, None);
        race_test_hooks::clear();
        assert!(
            err.is_err(),
            "a cache object rewritten mid-hash must fail closed, never report Valid or Corrupt \
             from an observation known to have changed mid-hash"
        );
    }
    #[test]
    fn malformed_proof_bytes_behave_as_absent_and_are_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"payload");
        let state = CacheState::open_for_test(&objects_dir);

        // Write a malformed (garbage) proof BLOB directly, bypassing the
        // encoder.
        {
            let conn_ref = state.conn.borrow();
            let conn = conn_ref.as_ref().unwrap();
            conn.execute(
                "INSERT INTO objects (oid, proof) VALUES (?1, ?2)",
                params![oid.as_bytes().as_slice(), b"not-a-real-proof".as_slice()],
            )
            .unwrap();
        }
        assert_eq!(state.lookup(&oid).unwrap(), None);

        let status = verify_through_client(&objects_dir, &oid);
        assert_eq!(status, ObjectVerification::Valid);
        assert!(state.lookup(&oid).unwrap().is_some());
    }

    #[test]
    fn database_deletion_never_authorizes_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"content");
        let state = CacheState::open_for_test(&objects_dir);
        assert_eq!(
            verify_through_client(&objects_dir, &oid),
            ObjectVerification::Valid
        );

        // Drop the state, then corrupt the db file on disk, then reopen.
        drop(state);
        fs::write(objects_dir.join("cache.sqlite3"), b"not a sqlite file").unwrap();
        // A corrupt db disables the accelerator, but never fails the
        // caller, and object bytes are still verified correctly.
        let status = verify_through_client(&objects_dir, &oid);
        assert_eq!(status, ObjectVerification::Valid);
    }

    #[test]
    fn newer_schema_is_left_untouched_and_disables_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let db_path = objects_dir.join("cache.sqlite3");
        {
            let conn = Connection::open(&db_path).unwrap();
            create_schema(&conn).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
            conn.execute(
                "INSERT INTO objects (oid, proof) VALUES (?1, ?2)",
                params![[0u8; 32].as_slice(), b"future-proof-format".as_slice()],
            )
            .unwrap();
        }

        let state = CacheState::open_for_test(&objects_dir);
        // Disabled: never returns the row a newer build wrote.
        let oid = Oid::from_bytes([0u8; 32]);
        assert_eq!(state.lookup(&oid).unwrap(), None);

        // The on-disk schema version must remain untouched.
        let conn = Connection::open(&db_path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION + 1);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "newer schema's row must not be dropped");
    }

    #[test]
    fn wrong_type_cache_path_is_never_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"real bytes");
        let state = CacheState::open_for_test(&objects_dir);
        assert_eq!(
            verify_through_client(&objects_dir, &oid),
            ObjectVerification::Valid
        );

        // Replace the object's cache path with a directory.
        let path = cache_path_oid(&objects_dir, &oid);
        crate::cache::object::unprotect(&path).ok();
        fs::remove_file(&path).unwrap();
        fs::create_dir_all(&path).unwrap();

        let status = verify_through_client(&objects_dir, &oid);
        assert_eq!(status, ObjectVerification::Corrupt);
        assert_eq!(state.lookup(&oid).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cache_path_is_never_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"symlinked bytes");

        let path = cache_path_oid(&objects_dir, &oid);
        let target = tmp.path().join("elsewhere");
        fs::write(&target, b"symlinked bytes").unwrap();
        crate::cache::object::unprotect(&path).ok();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let status = verify_through_client(&objects_dir, &oid);
        assert_eq!(status, ObjectVerification::Corrupt);
    }

    #[cfg(unix)]
    #[test]
    fn special_file_cache_path_is_never_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        let oid = write_object(&objects_dir, b"fifo bytes");
        let state = CacheState::open_for_test(&objects_dir);
        assert_eq!(
            verify_through_client(&objects_dir, &oid),
            ObjectVerification::Valid
        );

        let path = cache_path_oid(&objects_dir, &oid);
        crate::cache::object::unprotect(&path).ok();
        fs::remove_file(&path).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo failed for {}", path.display());

        let status = verify_through_client(&objects_dir, &oid);
        assert_eq!(status, ObjectVerification::Corrupt);
        assert_eq!(state.lookup(&oid).unwrap(), None);
    }

    #[test]
    fn concurrent_same_oid_upserts_are_benign() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = std::sync::Arc::new(std::sync::Mutex::new(CacheState::open_for_test(
            &objects_dir,
        )));
        let oid = Oid::from_hex(&"11".repeat(32)).unwrap();

        #[allow(
            clippy::needless_collect,
            reason = "Spawn every worker before joining any of them so the test exercises concurrent execution"
        )]
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let state = state.clone();
                std::thread::spawn(move || {
                    let proof = StatProof {
                        size: 100 + i,
                        mtime_secs: 1000 + i as i64,
                        mtime_nanos: 0,
                    };
                    state.lock().unwrap().upsert(&oid, &proof).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Some valid proof must remain -- no corruption/panic from the race.
        assert!(state.lock().unwrap().lookup(&oid).unwrap().is_some());
    }

    #[test]
    fn concurrent_different_oid_upserts_remain_correct() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = std::sync::Arc::new(std::sync::Mutex::new(CacheState::open_for_test(
            &objects_dir,
        )));

        #[allow(
            clippy::needless_collect,
            reason = "Spawn every worker before joining any of them so the test exercises concurrent execution"
        )]
        let handles: Vec<_> = (0..8u8)
            .map(|i| {
                let state = state.clone();
                std::thread::spawn(move || {
                    let oid = Oid::from_hex(&format!("{i:02x}").repeat(32)).unwrap();
                    let proof = StatProof {
                        size: u64::from(i),
                        mtime_secs: i64::from(i),
                        mtime_nanos: 0,
                    };
                    state.lock().unwrap().upsert(&oid, &proof).unwrap();
                    oid
                })
            })
            .collect();
        let oids: Vec<Oid> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let guard = state.lock().unwrap();
        for (i, oid) in oids.iter().enumerate() {
            let proof = guard.lookup(oid).unwrap().unwrap();
            assert_eq!(proof.size, i as u64);
        }
    }

    fn oid_from_index(i: usize) -> Oid {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
        Oid::from_bytes(bytes)
    }

    #[test]
    fn exact_many_reads_are_one_set_based_statement_not_n_point_queries() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);

        let proof = StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 0,
        };
        let oids: Vec<Oid> = (0..100).map(oid_from_index).collect();
        state
            .apply_many(
                &oids
                    .iter()
                    .map(|o| CachePublication::upsert(*o, proof))
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        let before = test_support::snapshot();
        let found = state.exact_many(&oids).unwrap();
        let after = test_support::snapshot();

        assert_eq!(found.len(), 100);
        // One logical request over 100 oids -> a single physical
        // `SELECT ... IN (...)` statement (100 << SQL bind budget), never
        // 100 point queries, and 100 rows returned.
        assert_eq!(
            after.proof_lookup_requests - before.proof_lookup_requests,
            1
        );
        assert_eq!(
            after.proof_lookup_statements - before.proof_lookup_statements,
            1
        );
        assert_eq!(after.proof_lookup_rows - before.proof_lookup_rows, 100);
    }

    #[test]
    fn apply_many_upserts_are_one_transaction_and_one_set_based_statement() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);

        let proof = StatProof {
            size: 7,
            mtime_secs: 7,
            mtime_nanos: 0,
        };
        let deltas: Vec<CachePublication> = (0..200)
            .map(|i| CachePublication::upsert(oid_from_index(i), proof))
            .collect();

        let before = test_support::snapshot();
        state.apply_many(&deltas).unwrap();
        let after = test_support::snapshot();

        // 200 upserts persist as one bounded transaction containing a
        // single multi-row `INSERT ... VALUES (...)` statement -- O(N /
        // SQL_CHUNK) statements, not O(N) row statements hidden in one
        // commit.
        assert_eq!(
            after.proof_mutation_transactions - before.proof_mutation_transactions,
            1
        );
        assert_eq!(
            after.proof_mutation_statements - before.proof_mutation_statements,
            1
        );
        assert_eq!(after.proof_mutation_rows - before.proof_mutation_rows, 200);
    }

    #[test]
    fn apply_many_bounds_transactions_to_transaction_chunk_not_the_whole_input() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);

        let proof = StatProof {
            size: 9,
            mtime_secs: 9,
            mtime_nanos: 0,
        };
        // Larger than one `TRANSACTION_CHUNK` (4096), so `apply_many`
        // must commit more than one bounded transaction rather than
        // holding a single transaction open across the entire input
        // to keep lock duration bounded.
        let count = TRANSACTION_CHUNK * 2 + 1;
        let deltas: Vec<CachePublication> = (0..count)
            .map(|i| CachePublication::upsert(oid_from_index(i), proof))
            .collect();

        let before = test_support::snapshot();
        state.apply_many(&deltas).unwrap();
        let after = test_support::snapshot();

        assert_eq!(
            after.proof_mutation_transactions - before.proof_mutation_transactions,
            3,
            "expected one bounded transaction per TRANSACTION_CHUNK-sized group"
        );
        assert_eq!(
            after.proof_mutation_rows - before.proof_mutation_rows,
            count
        );
    }

    #[test]
    fn apply_many_resolves_repeated_oids_by_last_write_across_transaction_chunk_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);

        let proof = StatProof {
            size: 7,
            mtime_secs: 7,
            mtime_nanos: 0,
        };
        // Fill a first bounded transaction group with an `Upsert` for
        // `oid_from_index(0)`, then push the total past one
        // `TRANSACTION_CHUNK` and finish with a `Remove` for that same
        // oid. If chunking were applied before coalescing, the upsert and
        // the removal would land in different bounded transactions and
        // the removal (running last) would win regardless of logical
        // order. `apply_many` must instead resolve every repeated oid to
        // its last occurrence in `deltas` *before* partitioning into
        // bounded transactions.
        let target = oid_from_index(0);
        let mut deltas: Vec<CachePublication> = vec![CachePublication::upsert(target, proof)];
        deltas.extend(
            (1..TRANSACTION_CHUNK + 5).map(|i| CachePublication::upsert(oid_from_index(i), proof)),
        );
        deltas.push(CachePublication::remove(target));

        state.apply_many(&deltas).unwrap();

        assert!(
            state.exact_many(&[target]).unwrap().is_empty(),
            "the later Remove(target) must win over the earlier Upsert(target), even though \
             they straddle a TRANSACTION_CHUNK boundary"
        );

        // And the reverse order: a later `Upsert` must win over an
        // earlier `Remove` across the same kind of boundary.
        let mut deltas: Vec<CachePublication> = vec![CachePublication::remove(target)];
        deltas.extend(
            (1..TRANSACTION_CHUNK + 5).map(|i| CachePublication::upsert(oid_from_index(i), proof)),
        );
        deltas.push(CachePublication::upsert(target, proof));

        state.apply_many(&deltas).unwrap();

        assert_eq!(
            state.exact_many(&[target]).unwrap().get(&target).copied(),
            Some(proof),
            "the later Upsert(target) must win over the earlier Remove(target), even though \
             they straddle a TRANSACTION_CHUNK boundary"
        );
    }

    #[test]
    fn remove_many_deletes_are_one_transaction_and_one_set_based_statement() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);

        let proof = StatProof {
            size: 3,
            mtime_secs: 3,
            mtime_nanos: 0,
        };
        let oids: Vec<Oid> = (0..150).map(oid_from_index).collect();
        state
            .apply_many(
                &oids
                    .iter()
                    .map(|o| CachePublication::upsert(*o, proof))
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        let before = test_support::snapshot();
        state.remove_many(&oids).unwrap();
        let after = test_support::snapshot();

        // 150 removals -> one transaction, one `DELETE ... WHERE oid IN
        // (...)` statement, not 150 per-oid deletes.
        assert_eq!(
            after.proof_mutation_transactions - before.proof_mutation_transactions,
            1
        );
        assert_eq!(
            after.proof_mutation_statements - before.proof_mutation_statements,
            1
        );
        assert_eq!(after.proof_mutation_rows - before.proof_mutation_rows, 150);
        assert!(state.exact_many(&oids).unwrap().is_empty());
    }

    #[test]
    fn exact_many_surfaces_a_raw_error_once_the_db_breaks_after_a_successful_open() {
        // `CacheState` itself is allowed to propagate a genuine SQLite
        // failure that happens *after* a successful open (as opposed to
        // `open` itself, which never errors) -- degrading that into "no
        // priors" is the caller's job (`CacheClient::verify_many`), not
        // this layer's.
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);
        state.break_for_test();

        let oids: Vec<Oid> = (0..4).map(oid_from_index).collect();
        assert!(state.exact_many(&oids).is_err());
    }

    #[test]
    fn apply_many_surfaces_a_raw_error_once_the_db_breaks_after_a_successful_open() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join("objects");
        fs::create_dir_all(&objects_dir).unwrap();
        let state = CacheState::open_for_test(&objects_dir);
        state.break_for_test();

        let proof = StatProof {
            size: 1,
            mtime_secs: 1,
            mtime_nanos: 0,
        };
        let deltas = vec![CachePublication::upsert(oid_from_index(0), proof)];
        assert!(state.apply_many(&deltas).is_err());
    }
}
