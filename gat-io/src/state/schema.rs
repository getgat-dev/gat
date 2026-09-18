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
             desired_oid       BLOB CHECK (
                 desired_oid IS NULL OR (typeof(desired_oid) = 'blob' AND length(desired_oid) = 32)
             ),
             desired_shard_id  TEXT,
             materialized_oid  BLOB CHECK (
                 materialized_oid IS NULL OR (typeof(materialized_oid) = 'blob' AND length(materialized_oid) = 32)
             ),
             materialized_proof BLOB CHECK (
                 materialized_proof IS NULL OR materialized_oid IS NOT NULL
             ),
             dirty INTEGER GENERATED ALWAYS AS (
                 CASE
                   WHEN desired_oid IS NULL AND materialized_oid IS NULL THEN 0
                   WHEN desired_oid IS NULL OR materialized_oid IS NULL THEN 1
                   WHEN desired_oid != materialized_oid THEN 1
                   ELSE 0
                 END
             ) STORED,
             CHECK ((desired_oid IS NULL) = (desired_shard_id IS NULL))
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
             identity      BLOB NOT NULL CHECK (typeof(identity) = 'blob' AND length(identity) = 32),
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
             desired_fingerprint   BLOB NOT NULL CHECK (typeof(desired_fingerprint) = 'blob' AND length(desired_fingerprint) = 32),
             exclude_fingerprint   BLOB CHECK (
                 exclude_fingerprint IS NULL OR (typeof(exclude_fingerprint) = 'blob' AND length(exclude_fingerprint) = 32)
             ),
             exclude_count         INTEGER NOT NULL DEFAULT 0 CHECK (typeof(exclude_count) = 'integer' AND exclude_count >= 0),
             exclude_block_identity BLOB CHECK (
                 exclude_block_identity IS NULL OR (typeof(exclude_block_identity) = 'blob' AND length(exclude_block_identity) = 32)
             ),
             exclude_proof         BLOB CHECK (exclude_proof IS NULL OR exclude_block_identity IS NOT NULL),
             validation_required   INTEGER NOT NULL DEFAULT 0 CHECK (validation_required IN (0, 1)),
             CHECK ((exclude_fingerprint IS NULL) = (exclude_block_identity IS NULL)),
             CHECK (exclude_fingerprint IS NOT NULL OR exclude_count = 0)
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
        // Publish the entire schema and its version in one durable commit.
        // WAL selection must happen before entering the transaction.
        let tx = conn
            .unchecked_transaction()
            .state_context("beginning materialized-state schema initialization")?;
        create_schema(&tx)?;
        ensure_desired_path_index(&tx)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .state_context("stamping materialized-state schema version")?;
        tx.commit()
            .state_context("committing materialized-state schema initialization")?;
    } else if version != SCHEMA_VERSION {
        return Err(StateStoreError::UnsupportedSchemaVersion {
            path: db_path.to_path_buf(),
            found: version,
            expected: SCHEMA_VERSION,
        });
    } else {
        ensure_desired_path_index(conn)?;
    }
    Ok(())
}

fn ensure_desired_path_index(conn: &Connection) -> Result<()> {
    // An additive query accelerator, not a change to persisted row semantics.
    // Existing version-1 stores build it once; subsequent opens reuse it.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS state_desired_path ON state(path) WHERE desired_oid IS NOT NULL;"
    ).state_context("ensuring desired-path index")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_schema_initialization_rolls_back_and_can_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.sqlite3");
        let conn = Connection::open(&path).unwrap();
        configure(&conn).unwrap();
        // Force a failure at the final index, after the other schema objects
        // and initial metadata row have been created.
        conn.execute_batch("CREATE TABLE state_desired_path (sentinel TEXT);")
            .unwrap();

        assert!(check_schema_version(&conn, &path).is_err());
        assert!(conn.is_autocommit());
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 0);
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(tables, ["state_desired_path"]);

        conn.execute_batch("DROP TABLE state_desired_path;")
            .unwrap();
        check_schema_version(&conn, &path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM reconciliation_meta", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2);
    }

    #[test]
    fn existing_schema_adds_missing_index_without_reinitializing_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.sqlite3");
        let conn = Connection::open(&path).unwrap();
        configure(&conn).unwrap();
        check_schema_version(&conn, &path).unwrap();
        conn.execute_batch(
            "DROP INDEX state_desired_path;
             INSERT INTO state(path, materialized_oid) VALUES('kept.bin', zeroblob(32));",
        )
        .unwrap();

        check_schema_version(&conn, &path).unwrap();
        let paths: Vec<String> = conn
            .prepare("SELECT path FROM state")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(paths, ["kept.bin"]);
        assert!(
            conn.prepare(
                "SELECT 1 FROM sqlite_schema WHERE type = 'index' AND name = 'state_desired_path'"
            )
            .unwrap()
            .exists([])
            .unwrap()
        );
    }

    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn
    }

    fn rejects_check(conn: &Connection, sql: &str) {
        let error = conn.execute_batch(sql).unwrap_err();
        assert!(
            matches!(error, rusqlite::Error::SqliteFailure(code, _)
            if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK),
            "{error}"
        );
    }

    #[test]
    fn state_rejects_invalid_oid_types_widths_and_unpaired_columns() {
        let conn = database();
        for sql in [
            "INSERT INTO state(path, desired_oid, desired_shard_id) VALUES('a', zeroblob(31), 'gat.lock')",
            "INSERT INTO state(path, desired_oid, desired_shard_id) VALUES('a', zeroblob(33), 'gat.lock')",
            "INSERT INTO state(path, desired_oid, desired_shard_id) VALUES('a', '01234567890123456789012345678901', 'gat.lock')",
            "INSERT INTO state(path, materialized_oid) VALUES('a', zeroblob(31))",
            "INSERT INTO state(path, materialized_oid) VALUES('a', zeroblob(33))",
            "INSERT INTO state(path, materialized_oid) VALUES('a', '01234567890123456789012345678901')",
            "INSERT INTO state(path, desired_oid) VALUES('a', zeroblob(32))",
            "INSERT INTO state(path, desired_shard_id) VALUES('a', 'gat.lock')",
            "INSERT INTO state(path, materialized_proof) VALUES('a', zeroblob(25))",
        ] {
            rejects_check(&conn, sql);
        }
    }

    #[test]
    fn state_allows_valid_partial_states_and_atomic_clearing() {
        let conn = database();
        conn.execute_batch("INSERT INTO state(path, desired_oid, desired_shard_id) VALUES('a', zeroblob(32), 'gat.lock');
            INSERT INTO state(path, materialized_oid) VALUES('b', zeroblob(32));
            UPDATE state SET materialized_oid = zeroblob(32), materialized_proof = zeroblob(25) WHERE path = 'a';").unwrap();
        let dirty: i64 = conn
            .query_row("SELECT dirty FROM state WHERE path = 'a'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(dirty, 0);
        rejects_check(
            &conn,
            "UPDATE state SET desired_oid = NULL WHERE path = 'a'",
        );
        rejects_check(
            &conn,
            "UPDATE state SET materialized_oid = NULL WHERE path = 'a'",
        );
        conn.execute_batch("UPDATE state SET desired_oid = NULL, desired_shard_id = NULL, materialized_oid = NULL, materialized_proof = NULL WHERE path = 'a';").unwrap();
    }

    #[test]
    fn metadata_rejects_invalid_hashes_and_incomplete_exclude_records() {
        let conn = database();
        for sql in [
            "INSERT INTO lock_shards(shard_id, identity) VALUES('gat.lock', zeroblob(31))",
            "INSERT INTO lock_shards(shard_id, identity) VALUES('gat.lock', '01234567890123456789012345678901')",
            "UPDATE reconciliation_meta SET desired_fingerprint = zeroblob(31)",
            "UPDATE reconciliation_meta SET desired_fingerprint = '01234567890123456789012345678901'",
            "UPDATE reconciliation_meta SET exclude_fingerprint = zeroblob(32)",
            "UPDATE reconciliation_meta SET exclude_block_identity = zeroblob(32)",
            "UPDATE reconciliation_meta SET exclude_proof = zeroblob(25)",
            "UPDATE reconciliation_meta SET exclude_count = 1",
            "UPDATE reconciliation_meta SET exclude_count = -1",
            "UPDATE reconciliation_meta SET exclude_count = 0.5",
            "UPDATE reconciliation_meta SET validation_required = 2",
            "UPDATE reconciliation_meta SET exclude_fingerprint = zeroblob(31), exclude_block_identity = zeroblob(32)",
            "UPDATE reconciliation_meta SET exclude_fingerprint = zeroblob(32), exclude_block_identity = '01234567890123456789012345678901'",
        ] {
            rejects_check(&conn, sql);
        }
        conn.execute_batch("INSERT INTO lock_shards(shard_id, identity) VALUES('gat.lock', zeroblob(32));
            UPDATE reconciliation_meta SET exclude_fingerprint = zeroblob(32), exclude_block_identity = zeroblob(32), exclude_count = 3;
            UPDATE reconciliation_meta SET exclude_proof = zeroblob(25), validation_required = 1;
            UPDATE reconciliation_meta SET exclude_fingerprint = NULL, exclude_block_identity = NULL, exclude_count = 0, exclude_proof = NULL;").unwrap();
    }
}
