use super::object::TEMP_PREFIX;
use super::proof::{CacheState, SCHEMA_VERSION};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

const CACHE_DB_FILENAME: &str = "cache.sqlite3";

#[derive(Debug)]
pub enum CacheDatabaseHealth {
    Absent,
    Healthy,
    UnsupportedVersion(i64),
    Unreadable(CacheDatabaseUnreadable),
}

#[derive(Debug)]
pub enum CacheDatabaseUnreadable {
    NotARegularFile,
    OpenFailed(Box<dyn std::error::Error + Send + Sync>),
    SchemaVersionUnreadable(Box<dyn std::error::Error + Send + Sync>),
    IntegrityCheckFailed(String),
    IntegrityCheckUnrunnable(Box<dyn std::error::Error + Send + Sync>),
    NotInitialized { version: i64 },
}

#[derive(Debug, thiserror::Error)]
pub enum CacheMaintenanceError {
    #[error("could not access `{}`", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Proof(#[from] crate::cache::proof::CacheProofError),
}

pub struct CacheMaintenance<'cache> {
    objects_dir: &'cache Path,
}

impl<'cache> CacheMaintenance<'cache> {
    pub(crate) const fn new(objects_dir: &'cache Path) -> Self {
        Self { objects_dir }
    }

    pub fn inspect_database(&self) -> Result<CacheDatabaseHealth, CacheMaintenanceError> {
        inspect_database(self.objects_dir)
    }

    pub fn rebuild_database(&self) -> Result<(), CacheMaintenanceError> {
        rebuild_database(self.objects_dir)
    }

    pub fn count_temporary_objects(&self) -> Result<usize, CacheMaintenanceError> {
        count_temporary_objects(self.objects_dir)
    }

    pub fn purge_temporary_objects(&self) -> Result<usize, CacheMaintenanceError> {
        purge_temporary_objects(self.objects_dir)
    }

    pub fn purge_objects(&self) -> Result<usize, CacheMaintenanceError> {
        purge_objects(self.objects_dir)
    }

    pub fn sweep<E>(
        &self,
        dry_run: bool,
        decide: impl FnMut(gat_core::oid::Oid) -> Result<super::enumeration::CacheSweepDecision, E>,
    ) -> Result<
        Result<super::enumeration::CacheSweepStats, E>,
        super::enumeration::CacheEnumerationError,
    > {
        super::enumeration::sweep_objects(self.objects_dir, dry_run, decide)
    }
}

impl CacheMaintenanceError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub fn inspect_database(objects_dir: &Path) -> Result<CacheDatabaseHealth, CacheMaintenanceError> {
    let path = objects_dir.join(CACHE_DB_FILENAME);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CacheDatabaseHealth::Absent);
        }
        Err(source) => return Err(CacheMaintenanceError::io(path, source)),
    };
    if !meta.is_file() {
        return Ok(CacheDatabaseHealth::Unreadable(
            CacheDatabaseUnreadable::NotARegularFile,
        ));
    }
    let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(source) => {
            return Ok(CacheDatabaseHealth::Unreadable(
                CacheDatabaseUnreadable::OpenFailed(Box::new(source)),
            ));
        }
    };
    let version: i64 = match conn.query_row("PRAGMA user_version", [], |row| row.get(0)) {
        Ok(version) => version,
        Err(source) => {
            return Ok(CacheDatabaseHealth::Unreadable(
                CacheDatabaseUnreadable::SchemaVersionUnreadable(Box::new(source)),
            ));
        }
    };
    if version != 0 && version != SCHEMA_VERSION {
        return Ok(CacheDatabaseHealth::UnsupportedVersion(version));
    }
    if version != SCHEMA_VERSION {
        return Ok(CacheDatabaseHealth::Unreadable(
            CacheDatabaseUnreadable::NotInitialized { version },
        ));
    }
    Ok(
        match conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)) {
            Ok(result) if result == "ok" => CacheDatabaseHealth::Healthy,
            Ok(result) => CacheDatabaseHealth::Unreadable(
                CacheDatabaseUnreadable::IntegrityCheckFailed(result),
            ),
            Err(source) => CacheDatabaseHealth::Unreadable(
                CacheDatabaseUnreadable::IntegrityCheckUnrunnable(Box::new(source)),
            ),
        },
    )
}

pub fn rebuild_database(objects_dir: &Path) -> Result<(), CacheMaintenanceError> {
    std::fs::create_dir_all(objects_dir)
        .map_err(|source| CacheMaintenanceError::io(objects_dir, source))?;
    remove_database_files(objects_dir)?;
    CacheState::open_strict(objects_dir).map_err(crate::cache::proof::CacheProofError::from)?;
    Ok(())
}

pub fn count_temporary_objects(objects_dir: &Path) -> Result<usize, CacheMaintenanceError> {
    temporary_entries(objects_dir, false)
}

pub fn purge_temporary_objects(objects_dir: &Path) -> Result<usize, CacheMaintenanceError> {
    temporary_entries(objects_dir, true)
}

fn temporary_entries(objects_dir: &Path, remove: bool) -> Result<usize, CacheMaintenanceError> {
    let entries = match std::fs::read_dir(objects_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(CacheMaintenanceError::io(objects_dir, source)),
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|source| CacheMaintenanceError::io(objects_dir, source))?;
        if entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX) {
            count += 1;
            if remove {
                let path = entry.path();
                std::fs::remove_file(&path)
                    .map_err(|source| CacheMaintenanceError::io(path, source))?;
            }
        }
    }
    Ok(count)
}

pub fn purge_objects(objects_dir: &Path) -> Result<usize, CacheMaintenanceError> {
    purge_namespace(&objects_dir.join("blake3"))
}

fn purge_namespace(path: &Path) -> Result<usize, CacheMaintenanceError> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(CacheMaintenanceError::io(path, source)),
    };
    let mut purged = 0;
    for entry in entries {
        let entry = entry.map_err(|source| CacheMaintenanceError::io(path, source))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| CacheMaintenanceError::io(&entry_path, source))?;
        if file_type.is_dir() {
            purged += purge_namespace(&entry_path)?;
        } else {
            std::fs::remove_file(&entry_path)
                .map_err(|source| CacheMaintenanceError::io(&entry_path, source))?;
            purged += 1;
        }
    }
    std::fs::remove_dir(path).map_err(|source| CacheMaintenanceError::io(path, source))?;
    Ok(purged)
}

fn remove_database_files(objects_dir: &Path) -> Result<(), CacheMaintenanceError> {
    for name in [
        CACHE_DB_FILENAME.to_owned(),
        format!("{CACHE_DB_FILENAME}-wal"),
        format!("{CACHE_DB_FILENAME}-shm"),
    ] {
        let path = objects_dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(CacheMaintenanceError::io(path, source)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_objects_removes_only_the_blake3_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let objects = tmp.path();
        std::fs::create_dir_all(objects.join("blake3/aa/bb")).unwrap();
        std::fs::write(objects.join("blake3/aa/object-a"), b"a").unwrap();
        std::fs::write(objects.join("blake3/aa/bb/object-b"), b"b").unwrap();
        std::fs::create_dir_all(objects.join("sha256/aa")).unwrap();
        std::fs::write(objects.join("sha256/aa/unknown"), b"unknown").unwrap();
        // A case variant is a separate namespace only on case-sensitive volumes.
        let case_sensitive = !objects.join("BLAKE3").exists();
        if case_sensitive {
            std::fs::create_dir_all(objects.join("BLAKE3/AA")).unwrap();
            std::fs::write(objects.join("BLAKE3/AA/unknown"), b"unknown").unwrap();
        }
        std::fs::create_dir_all(objects.join("aa")).unwrap();
        std::fs::write(objects.join("aa/legacy"), b"legacy").unwrap();
        std::fs::write(objects.join(format!("{TEMP_PREFIX}active")), b"temporary").unwrap();
        std::fs::write(objects.join("unrecognized"), b"state").unwrap();

        assert_eq!(purge_objects(objects).unwrap(), 2);

        assert!(!objects.join("blake3").exists());
        assert!(objects.join("sha256/aa/unknown").exists());
        if case_sensitive {
            assert!(objects.join("BLAKE3/AA/unknown").exists());
        }
        assert!(objects.join("aa/legacy").exists());
        assert!(objects.join(format!("{TEMP_PREFIX}active")).exists());
        assert!(objects.join("unrecognized").exists());
    }
}
