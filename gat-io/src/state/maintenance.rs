use super::{StateStore, StateStoreError};
use crate::RepositoryLayout;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum StateDatabaseHealth {
    Absent,
    Healthy,
    Outdated(i64),
    NewerVersion(i64),
    Unreadable(StateDatabaseUnreadable),
}

#[derive(Debug)]
pub enum StateDatabaseUnreadable {
    NotARegularFile,
    OpenFailed(Box<dyn std::error::Error + Send + Sync>),
    SchemaVersionUnreadable(Box<dyn std::error::Error + Send + Sync>),
    IntegrityCheckFailed(String),
    IntegrityCheckUnrunnable(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, thiserror::Error)]
pub enum StateMaintenanceError {
    #[error("could not access `{}`", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not publish `{}`", path.display())]
    PersistFailed {
        path: PathBuf,
        #[source]
        source: tempfile::PersistError,
    },
    #[error(transparent)]
    State(#[from] StateStoreError),
    #[error(transparent)]
    Lock(#[from] crate::lock::LockError),
}

impl StateMaintenanceError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub fn inspect_database(
    repository: &RepositoryLayout,
) -> Result<StateDatabaseHealth, StateMaintenanceError> {
    let db_path = repository.materialized_db_path();
    let path = db_path.as_path();
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StateDatabaseHealth::Absent);
        }
        Err(source) => return Err(StateMaintenanceError::io(path, source)),
    };
    if !meta.is_file() {
        return Ok(StateDatabaseHealth::Unreadable(
            StateDatabaseUnreadable::NotARegularFile,
        ));
    }
    // A read-only connection may still create WAL coordination files.
    repository
        .local_directory()
        .ensure()
        .map_err(|error| StateMaintenanceError::io(error.path, error.source))?;
    let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(err) => {
            return Ok(StateDatabaseHealth::Unreadable(
                StateDatabaseUnreadable::OpenFailed(Box::new(err)),
            ));
        }
    };
    let version: i64 = match conn.query_row("PRAGMA user_version", [], |row| row.get(0)) {
        Ok(version) => version,
        Err(err) => {
            return Ok(StateDatabaseHealth::Unreadable(
                StateDatabaseUnreadable::SchemaVersionUnreadable(Box::new(err)),
            ));
        }
    };
    if version > super::schema::SCHEMA_VERSION {
        return Ok(StateDatabaseHealth::NewerVersion(version));
    }
    if version < super::schema::SCHEMA_VERSION {
        return Ok(StateDatabaseHealth::Outdated(version));
    }
    Ok(
        match conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
            Ok(result) if result == "ok" => StateDatabaseHealth::Healthy,
            Ok(result) => StateDatabaseHealth::Unreadable(
                StateDatabaseUnreadable::IntegrityCheckFailed(result),
            ),
            Err(err) => StateDatabaseHealth::Unreadable(
                StateDatabaseUnreadable::IntegrityCheckUnrunnable(Box::new(err)),
            ),
        },
    )
}

pub fn count_stale_sidecars(repository: &RepositoryLayout) -> Result<usize, StateMaintenanceError> {
    Ok(stale_sidecars(&repository.materialized_db_path())?.len())
}

pub fn remove_stale_sidecars(
    repository: &RepositoryLayout,
) -> Result<usize, StateMaintenanceError> {
    let sidecars = stale_sidecars(&repository.materialized_db_path())?;
    for path in &sidecars {
        remove_file_if_exists(path)?;
    }
    Ok(sidecars.len())
}

/// Rebuild the database without touching the live path until a complete,
/// checkpointed replacement is ready to publish.
#[allow(
    clippy::missing_panics_doc,
    reason = "Repository database paths are constructed with a parent directory"
)]
pub fn rebuild_atomically(repository: &RepositoryLayout) -> Result<(), StateMaintenanceError> {
    repository
        .local_directory()
        .ensure()
        .map_err(|error| StateMaintenanceError::io(error.path, error.source))?;
    let db_path = repository.materialized_db_path();
    let dir = db_path
        .parent()
        .expect("materialized database path has a parent");
    std::fs::create_dir_all(dir).map_err(|source| StateMaintenanceError::io(dir, source))?;
    let tmp = tempfile::Builder::new()
        .prefix("state-repair-")
        .suffix(".sqlite3")
        .tempfile_in(dir)
        .map_err(|source| StateMaintenanceError::io(dir, source))?;
    let tmp_path = tmp.path().to_path_buf();
    {
        let mut store = StateStore::open_at(&tmp_path)?;
        let evidence = crate::lock::LockStore::observe_full_with_evidence(repository.root_path())?;
        store.apply_full_lock_evidence(evidence)?;
        store.set_validation_required(true)?;
        store.checkpoint_and_truncate_wal()?;
    }
    remove_file_if_exists(&sidecar_path(&tmp_path, "-wal"))?;
    remove_file_if_exists(&sidecar_path(&tmp_path, "-shm"))?;
    crate::atomic::persist_with_retry(tmp, &db_path).map_err(|source| {
        StateMaintenanceError::PersistFailed {
            path: db_path.clone(),
            source,
        }
    })?;
    remove_file_if_exists(&sidecar_path(&db_path, "-wal"))?;
    remove_file_if_exists(&sidecar_path(&db_path, "-shm"))?;
    Ok(())
}

fn stale_sidecars(db_path: &Path) -> Result<Vec<PathBuf>, StateMaintenanceError> {
    if path_exists(db_path)? {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for path in [sidecar_path(db_path, "-wal"), sidecar_path(db_path, "-shm")] {
        if path_exists(&path)? {
            found.push(path);
        }
    }
    Ok(found)
}

fn path_exists(path: &Path) -> Result<bool, StateMaintenanceError> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(StateMaintenanceError::io(path, source)),
    }
}

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn remove_file_if_exists(path: &Path) -> Result<(), StateMaintenanceError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StateMaintenanceError::io(path, source)),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn set_schema_version_for_test(
    layout: &RepositoryLayout,
    version: i64,
) -> Result<(), StateMaintenanceError> {
    let db_path = layout.materialized_db_path();
    let conn = Connection::open(&db_path).map_err(|source| StateStoreError::OpenFailed {
        path: db_path,
        source: super::error::StateSqlError::from_sqlite(source),
    })?;
    conn.pragma_update(None, "user_version", version)
        .map_err(StateStoreError::from)?;
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub const fn current_schema_version_for_test() -> i64 {
    super::schema::SCHEMA_VERSION
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_database_for_test(layout: &RepositoryLayout) -> Result<(), StateMaintenanceError> {
    let db_path = layout.materialized_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|source| StateMaintenanceError::io(parent, source))?;
    }
    for path in [
        db_path.clone(),
        sidecar_path(&db_path, "-wal"),
        sidecar_path(&db_path, "-shm"),
    ] {
        remove_file_if_exists(&path)?;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub fn create_stale_sidecar_for_test(
    layout: &RepositoryLayout,
) -> Result<(), StateMaintenanceError> {
    let db_path = layout.materialized_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|source| StateMaintenanceError::io(parent, source))?;
    }
    let path = sidecar_path(&db_path, "-wal");
    std::fs::write(&path, []).map_err(|source| StateMaintenanceError::io(path, source))
}

#[cfg(any(test, feature = "test-support"))]
#[allow(
    clippy::missing_panics_doc,
    reason = "Repository database paths are constructed with a parent directory"
)]
pub fn repair_temp_count_for_test(
    layout: &RepositoryLayout,
) -> Result<usize, StateMaintenanceError> {
    let db_path = layout.materialized_db_path();
    let dir = db_path
        .parent()
        .expect("materialized database path has a parent");
    let entries =
        std::fs::read_dir(dir).map_err(|source| StateMaintenanceError::io(dir, source))?;
    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|source| StateMaintenanceError::io(dir, source))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("state-repair-")
        {
            count += 1;
        }
    }
    Ok(count)
}
