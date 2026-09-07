use super::{BUSY_TIMEOUT_MS, Connection, Path, Result, StateResultExt, StateStoreError};

/// The schema version this build of `gat` writes and requires.
/// `PRAGMA user_version` is stamped with this value only once the schema
/// has been created (see [`check_schema_version`]), so a reader never
/// sees a version claiming more than what's actually there.
///
/// A version-`0` database is initialized to this schema. Any other
/// unsupported version fails closed instead of being migrated or discarded
/// and rebuilt; see [`check_schema_version`].
pub const SCHEMA_VERSION: i64 = 1;

/// Per-connection pragmas, applied on every open since (aside from
/// `journal_mode`) none of these persist in the database file itself.
///
/// - `synchronous = FULL` alongside `journal_mode = WAL`: fsyncs at every
///   commit, not just at WAL checkpoints, preserving the required
///   durability contract:
///   the most recent commit always survives an OS crash or power loss,
///   not only an application crash. `WAL + synchronous = NORMAL` trades
///   that guarantee away for extra throughput and is deliberately *not*
///   used here without a separate, explicit decision to
///   accept weaker power-loss durability.
/// - `busy_timeout`: bounded wait for another process's write lock,
///   matching [`BUSY_TIMEOUT_MS`].
/// - `temp_store = MEMORY`: keeps any transient sort/temp b-trees (e.g.
///   sorts that cannot use an index) off disk.
/// - `foreign_keys = ON` / `trusted_schema = OFF`: no foreign keys are
///   declared here, but both are cheap, conservative hardening defaults
///   for an embedded database this crate doesn't need to relax.
/// - `cache_size`/`mmap_size`: a 4,000 KiB page-cache target and a 32 MiB
///   memory-mapping limit per connection.
/// - `wal_autocheckpoint` requests a checkpoint after 1,000 WAL pages.
///   `journal_size_limit` limits retained WAL space after a reset, not
///   active WAL growth; long-lived readers can prevent checkpoints from
///   completing and allow the WAL to grow beyond this limit.
pub(super) fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};
         PRAGMA synchronous = FULL;
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;
         PRAGMA trusted_schema = OFF;
         PRAGMA cache_size = -4000;
         PRAGMA mmap_size = 33554432;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA journal_size_limit = 4194304;"
    ))
    .state_context("configuring SQLite connection")
}

/// Set `journal_mode = WAL` once, when the schema is first created on a
/// fresh database. Unlike the per-connection pragmas in [`configure`],
/// `journal_mode` is a persistent property of the database file itself:
/// once WAL is selected, every later connection that opens this file is
/// already in WAL mode without needing to set it again.
fn set_journal_mode_wal(conn: &Connection, db_path: &Path) -> Result<()> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .state_context(format!(
            "enabling WAL journal mode for {}",
            db_path.display()
        ))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StateStoreError::WalUnsupported {
            path: db_path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS state (
             path              TEXT PRIMARY KEY,
             desired_oid       BLOB,
             desired_shard_id  TEXT,
             materialized_oid  BLOB,
             materialized_proof BLOB,
             dirty INTEGER GENERATED ALWAYS AS (
                 CASE
                   WHEN desired_oid IS NULL AND materialized_oid IS NULL THEN 0
                   WHEN desired_oid IS NULL OR materialized_oid IS NULL THEN 1
                   WHEN desired_oid != materialized_oid THEN 1
                   ELSE 0
                 END
             ) STORED
         ) WITHOUT ROWID;

         CREATE INDEX IF NOT EXISTS state_dirty_path ON state(path) WHERE dirty = 1;
         CREATE INDEX IF NOT EXISTS state_by_desired_shard ON state(desired_shard_id);

         -- Content-identity catalog for each shard file currently backing
         -- `gat.lock` (`shard_id` is `\"gat.lock\"` for the flat case, or a
         -- shard's path relative to the repo root when sharded). Used to
         -- decide whether a shard needs reparsing at all: an unchanged
         -- `identity` (confirmed via an unchanged stat `proof`) means the
         -- shard's desired rows in `state` are already current. `identity`
         -- is always exactly 32 bytes --
         -- `crate::lock::ShardContentIdentity`'s fixed-width
         -- `BLAKE3(raw shard file bytes)` digest, one meaning, not a
         -- tagged union of encodings, so there is no `identity_kind`
         -- column to discriminate -- enforced by
         -- the `CHECK` constraint below, not just by convention. `proof`
         -- is one optional encoded `crate::file_state::StatProof` (the
         -- application's one shared filesystem-identity-preservation
         -- accelerator):
         -- `NULL` means the identity above is known but must be
         -- re-established from a coherent read/hash before any stat-only
         -- reuse -- an observed pre/post metadata mismatch during that
         -- read fails closed rather than minting a proof for an
         -- observation that was never coherent in the first place (see
         -- `crate::file_state::coherent_observation`).
         CREATE TABLE IF NOT EXISTS lock_shards (
             shard_id      TEXT PRIMARY KEY,
             identity      BLOB NOT NULL CHECK (length(identity) = 32),
             proof         BLOB
         ) WITHOUT ROWID;

         -- Single-row table (`id` fixed at 1) tracking two fingerprints
         -- so `gat-engine`'s clean-sync fast path never has
         -- to load the full desired lock set just to find out nothing
         -- changed:
         --
         -- `desired_fingerprint` is the current
         -- `crate::lock::CanonicalDesiredIdentity`, a domain-separated
         -- XOR set accumulator over per-shard components, maintained
         -- incrementally in O(k) time for a k-shard
         -- change by `reconciliation::apply_shard_catalog_tx` XORing only
         -- the touched shards' old/new contributions in or out, never by
         -- rereading and re-deriving the whole `lock_shards` catalog.
         -- Seeded to `zeroblob(32)` -- `CanonicalDesiredIdentity::empty()`,
         -- the XOR accumulator's identity element, not an arbitrary
         -- placeholder -- which already agrees with
         -- `current_desired_revision`'s independently-computed identity
         -- for a repository with nothing tracked yet, with no special
         -- empty-catalog case required anywhere to make that true.
         --
         -- `exclude_fingerprint`/`exclude_count` cache the fingerprint
         -- (desired fingerprint plus `git.ignore_patterns`) and path
         -- count that `.git/info/exclude`'s gat-managed block was last
         -- regenerated from. The fingerprint
         -- proves only that the *inputs* to that regeneration are
         -- unchanged; `exclude_block_identity` (BLAKE3 of exactly the
         -- gat-managed block's own bytes, never the whole file -- lines
         -- outside gat's markers are user-owned and are never folded
         -- into gat's semantic output identity) and `exclude_proof` (one
         -- optional encoded `crate::file_state::StatProof`, same shape
         -- as `lock_shards.proof`) additionally
         -- prove the *output* file on disk still matches what was last
         -- written -- so an externally deleted or hand-edited
         -- `.git/info/exclude` is never mistaken for unchanged just
         -- because its generating inputs didn't change. `NULL`
         -- `exclude_proof` means the block identity is known but must be
         -- re-established from bytes before any stat-only reuse.
         -- `validation_required` records whether `gat system repair state`
         -- destructively rebuilt this database's materialized ledger
         -- without being able to prove the freshly reset (empty) ledger
         -- still reflects the working tree. It lives in this table (set in
         -- the same
         -- transaction/publish that rebuilds the ledger) rather than as a
         -- separate marker file, so there is no window where the ledger
         -- is reset but the reduced-trust condition isn't yet durable.
         CREATE TABLE IF NOT EXISTS reconciliation_meta (
             id                    INTEGER PRIMARY KEY CHECK (id = 1),
             desired_fingerprint   BLOB NOT NULL CHECK (length(desired_fingerprint) = 32),
             exclude_fingerprint   BLOB,
             exclude_count         INTEGER NOT NULL DEFAULT 0,
             exclude_block_identity BLOB,
             exclude_proof         BLOB,
             validation_required   INTEGER NOT NULL DEFAULT 0
         );

         INSERT OR IGNORE INTO reconciliation_meta (id, desired_fingerprint)
         VALUES (1, zeroblob(32));",
    )
    .state_context("creating materialized-state schema")?;
    Ok(())
}

/// Read `PRAGMA user_version`. A fresh/empty database file (one `SQLite`
/// itself just created, with no `CREATE TABLE`/`user_version` yet) reads
/// as `0`; any nonzero version other than exactly [`SCHEMA_VERSION`] is
/// rejected outright rather than migrated, discarded, rebuilt, or silently
/// reinterpreted.
pub(super) fn check_schema_version(conn: &Connection, db_path: &Path) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .state_context("reading materialized-state schema version")?;
    if version == 0 {
        // An empty database file with no schema yet (e.g. created but
        // never populated) -- initialize it in place.
        set_journal_mode_wal(conn, db_path)?;
        create_schema(conn)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .state_context("stamping materialized-state schema version")?;
    } else if version != SCHEMA_VERSION {
        return Err(StateStoreError::UnsupportedSchemaVersion {
            path: db_path.to_path_buf(),
            found: version,
            expected: SCHEMA_VERSION,
        });
    }
    // An additive query accelerator, not a change to persisted row semantics.
    // Existing version-1 stores build it once; subsequent opens reuse it.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS state_desired_path ON state(path) WHERE desired_oid IS NOT NULL;"
    ).state_context("ensuring desired-path index")?;
    Ok(())
}
