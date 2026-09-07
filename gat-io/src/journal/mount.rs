//! Durable storage for crash-recoverable mount transactions.

use gat_core::config::{Config, ConfigScope};
use gat_core::lexical_path::GatPath;
use gat_core::name::MountName;
use gat_core::oid::Oid;
use gat_core::selection::Selection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::lock::{LockError, LockStore};
use crate::{PreparedGitWorktree, RepositoryLayout};

pub const MOUNT_TXN_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountTxnOp {
    Add,
    Update,
    Remove,
}

#[derive(Debug, Clone)]
pub struct MountTxnRecord {
    pub version: u32,
    pub op: MountTxnOp,
    pub scope: ConfigScope,
    pub name: MountName,
    pub old_target: Option<GatPath>,
    pub new_target: Option<GatPath>,
    pub post_config: Config,
    pub pre_config: Config,
    pub row_windows: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedRow {
    pub path: GatPath,
    pub oid: Oid,
}

/// A parseable mount journal whose semantic operation shape is unsafe to
/// interpret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MountJournalValidationError {
    #[error("an add transaction unexpectedly names an old target")]
    AddHasOldTarget,
    #[error("an add transaction does not name a new target")]
    AddMissingNewTarget,
    #[error("an update transaction does not name an old target")]
    UpdateMissingOldTarget,
    #[error("an update transaction does not name a new target")]
    UpdateMissingNewTarget,
    #[error("a remove transaction does not name an old target")]
    RemoveMissingOldTarget,
    #[error("a remove transaction unexpectedly names a new target")]
    RemoveHasNewTarget,
    #[error("a remove transaction unexpectedly names staged row windows")]
    RemoveHasStagedWindows,
    #[error("staged row window size is zero")]
    ZeroWindowSize,
    #[error("staged row window {window} is missing")]
    MissingStagedWindow { window: usize },
    #[error("staged-row storage contains an undeclared entry")]
    UnexpectedStagedEntry,
    #[error("staged row window {window} is empty")]
    EmptyStagedWindow { window: usize },
    #[error("staged row window {window} contains a row outside the recorded target")]
    StagedRowOutsideTarget { window: usize },
}

#[derive(Serialize)]
struct EncodedMountTxnRecord<'a> {
    version: u32,
    op: MountTxnOp,
    scope: &'static str,
    name: &'a MountName,
    old_target: &'a Option<GatPath>,
    new_target: &'a Option<GatPath>,
    post_config: &'a Config,
    pre_config: &'a Config,
    row_windows: usize,
}

#[derive(Deserialize)]
struct DecodedMountTxnRecord {
    version: u32,
    op: MountTxnOp,
    scope: String,
    name: MountName,
    old_target: Option<GatPath>,
    new_target: Option<GatPath>,
    post_config: Config,
    pre_config: Config,
    row_windows: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MountJournalError {
    #[error("could not {operation} the mount transaction journal at `{}`", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not {operation} the mount transaction journal at `{}`", path.display())]
    Serde {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "mount transaction journal at {} names unknown config scope `{tag}`; \
         it may have been written by an incompatible gat version",
        path.display()
    )]
    UnknownConfigScope { path: PathBuf, tag: String },
    #[error(
        "mount transaction journal at {} has an unsupported version ({found}); an interrupted \
         mutation for mount `{name}` cannot be safely recovered by this build",
        path.display()
    )]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        name: String,
    },
    #[error("mount transaction journal at {} is semantically invalid", path.display())]
    InvalidRecord {
        path: PathBuf,
        #[source]
        reason: MountJournalValidationError,
    },
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Atomic(#[from] crate::atomic::AtomicError),
}

impl MountJournalError {
    fn io(operation: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    fn serde(operation: &'static str, path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        Self::Serde {
            operation,
            path: path.into(),
            source,
        }
    }

    fn invalid(path: impl Into<PathBuf>, reason: MountJournalValidationError) -> Self {
        Self::InvalidRecord {
            path: path.into(),
            reason,
        }
    }
}

pub type Result<T> = std::result::Result<T, MountJournalError>;

#[derive(Debug, Clone)]
pub struct MountJournal {
    cache_root: PathBuf,
}

impl MountJournal {
    #[must_use]
    pub fn open(repository: &RepositoryLayout) -> Self {
        Self::new_in(repository.cache_root_path())
    }

    fn new_in(cache_root: &Path) -> Self {
        Self {
            cache_root: cache_root.to_path_buf(),
        }
    }

    fn path(&self) -> PathBuf {
        self.cache_root.join("mount-transaction.json")
    }

    fn staged_rows_dir(&self) -> PathBuf {
        self.cache_root.join("mount-transaction-rows")
    }

    fn staged_window_path(&self, window: usize) -> PathBuf {
        self.staged_rows_dir()
            .join(format!("window-{window:05}.json"))
    }

    pub fn read(&self) -> Result<Option<MountTxnRecord>> {
        let path = self.path();
        let body = match std::fs::read_to_string(&path) {
            Ok(body) => body,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(MountJournalError::io("reading", path, source)),
        };
        let decoded: DecodedMountTxnRecord = serde_json::from_str(&body)
            .map_err(|source| MountJournalError::serde("parsing", &path, source))?;
        if decoded.version != MOUNT_TXN_VERSION {
            return Err(MountJournalError::UnsupportedVersion {
                path,
                found: decoded.version,
                name: decoded.name.to_string(),
            });
        }
        let scope =
            decode_scope(&decoded.scope).ok_or_else(|| MountJournalError::UnknownConfigScope {
                path: path.clone(),
                tag: decoded.scope.clone(),
            })?;
        let record = MountTxnRecord {
            version: decoded.version,
            op: decoded.op,
            scope,
            name: decoded.name,
            old_target: decoded.old_target,
            new_target: decoded.new_target,
            post_config: decoded.post_config,
            pre_config: decoded.pre_config,
            row_windows: decoded.row_windows,
        };
        validate_record_shape(&path, &record)?;
        self.validate_staged_files(record.row_windows)?;
        for window in self.validated_staged_windows(&record) {
            window?;
        }
        Ok(Some(record))
    }

    pub fn write(&self, record: &MountTxnRecord) -> Result<()> {
        let path = self.path();
        validate_record_shape(&path, record)?;
        let encoded = EncodedMountTxnRecord {
            version: record.version,
            op: record.op,
            scope: encode_scope(record.scope),
            name: &record.name,
            old_target: &record.old_target,
            new_target: &record.new_target,
            post_config: &record.post_config,
            pre_config: &record.pre_config,
            row_windows: record.row_windows,
        };
        let body = serde_json::to_string_pretty(&encoded)
            .map_err(|source| MountJournalError::serde("serializing", &path, source))?;
        crate::atomic::write_atomic(&path, &body)?;
        Ok(())
    }

    pub fn reset_staged_rows(&self) -> Result<()> {
        let rows_dir = self.staged_rows_dir();
        remove_dir_if_exists(&rows_dir, "clearing")?;
        std::fs::create_dir_all(&rows_dir)
            .map_err(|source| MountJournalError::io("creating", rows_dir, source))
    }

    pub fn write_staged_window(&self, window: usize, rows: &[StagedRow]) -> Result<()> {
        let path = self.staged_window_path(window);
        let body = serde_json::to_string(rows)
            .map_err(|source| MountJournalError::serde("serializing", &path, source))?;
        crate::atomic::write_atomic(&path, &body)?;
        Ok(())
    }

    pub fn read_staged_window(&self, window: usize) -> Result<Vec<StagedRow>> {
        let path = self.staged_window_path(window);
        let body = std::fs::read_to_string(&path)
            .map_err(|source| MountJournalError::io("reading", &path, source))?;
        serde_json::from_str(&body)
            .map_err(|source| MountJournalError::serde("parsing", path, source))
    }

    #[must_use]
    pub const fn staged_windows(&self, count: usize) -> StagedWindows<'_> {
        StagedWindows {
            journal: self,
            next: 0,
            count,
            target: None,
        }
    }

    /// Read exactly the windows declared by a validated transaction and
    /// reject rows outside its recorded destination target.
    #[must_use]
    pub const fn validated_staged_windows<'a>(
        &'a self,
        record: &'a MountTxnRecord,
    ) -> StagedWindows<'a> {
        StagedWindows {
            journal: self,
            next: 0,
            count: record.row_windows,
            target: record.new_target.as_ref(),
        }
    }

    /// Stream the selected source lock into bounded, durable staging
    /// windows. The source layout and persisted row DTO never escape this
    /// I/O capability.
    pub fn stage_selected(
        &self,
        source: &PreparedGitWorktree,
        selection: &Selection,
        target: &GatPath,
        window_size: usize,
    ) -> Result<usize> {
        self.stage_selected_root(source.root(), selection, target, window_size)
    }

    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn stage_selected_layout(
        &self,
        source: &RepositoryLayout,
        selection: &Selection,
        target: &GatPath,
        window_size: usize,
    ) -> Result<usize> {
        self.stage_selected_root(source.root_path(), selection, target, window_size)
    }

    fn stage_selected_root(
        &self,
        source_root: &Path,
        selection: &Selection,
        target: &GatPath,
        window_size: usize,
    ) -> Result<usize> {
        if window_size == 0 {
            return Err(MountJournalError::invalid(
                self.staged_rows_dir(),
                MountJournalValidationError::ZeroWindowSize,
            ));
        }
        self.reset_staged_rows()?;
        let exact_source_path = selection
            .has_no_glob_filter()
            .then(|| selection.scope_path().cloned())
            .flatten();
        let mut rows = Vec::with_capacity(window_size);
        let mut windows = 0usize;
        let mut flush_error = None;
        let outcome = LockStore::visit_selected(
            source_root,
            exact_source_path.as_ref(),
            |path| selection.matches_str(path),
            |entry| {
                let Some(relative) = selection.reparent_relative(&entry.path) else {
                    return Ok(());
                };
                rows.push(StagedRow {
                    path: if relative.is_empty() {
                        target.clone()
                    } else {
                        target.join_rel(relative)
                    },
                    oid: entry.oid,
                });
                if rows.len() == window_size {
                    if let Err(error) = self.write_staged_window(windows, &rows) {
                        flush_error = Some(error);
                        return Err(LockError::Persistence(
                            crate::lock::PersistenceError::CallbackFailed,
                        ));
                    }
                    windows += 1;
                    rows.clear();
                }
                Ok(())
            },
        );
        if let Some(error) = flush_error {
            return Err(error);
        }
        outcome?;
        if !rows.is_empty() {
            self.write_staged_window(windows, &rows)?;
            windows += 1;
        }
        Ok(windows)
    }

    pub fn remove_journal(&self) -> Result<()> {
        remove_file_if_exists(&self.path(), "removing")
    }

    pub fn remove_staged_rows(&self) -> Result<()> {
        remove_dir_if_exists(&self.staged_rows_dir(), "removing")
    }

    fn validate_staged_files(&self, count: usize) -> Result<()> {
        let directory = self.staged_rows_dir();
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound && count == 0 => {
                return Ok(());
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(MountJournalError::invalid(
                    directory,
                    MountJournalValidationError::MissingStagedWindow { window: 0 },
                ));
            }
            Err(source) => {
                return Err(MountJournalError::io("validating", directory, source));
            }
        };
        let mut found = BTreeSet::new();
        for entry in entries {
            let entry =
                entry.map_err(|source| MountJournalError::io("validating", &directory, source))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                return Err(MountJournalError::invalid(
                    &directory,
                    MountJournalValidationError::UnexpectedStagedEntry,
                ));
            };
            if !found.insert(name) {
                return Err(MountJournalError::invalid(
                    &directory,
                    MountJournalValidationError::UnexpectedStagedEntry,
                ));
            }
        }
        for window in 0..count {
            if !found.remove(&format!("window-{window:05}.json")) {
                return Err(MountJournalError::invalid(
                    &directory,
                    MountJournalValidationError::MissingStagedWindow { window },
                ));
            }
        }
        if !found.is_empty() {
            return Err(MountJournalError::invalid(
                directory,
                MountJournalValidationError::UnexpectedStagedEntry,
            ));
        }
        Ok(())
    }
}

pub struct StagedWindows<'a> {
    journal: &'a MountJournal,
    next: usize,
    count: usize,
    target: Option<&'a GatPath>,
}

impl Iterator for StagedWindows<'_> {
    type Item = Result<Vec<StagedRow>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.count {
            return None;
        }
        let window = self.next;
        self.next += 1;
        Some(self.journal.read_staged_window(window).and_then(|rows| {
            if self.target.is_some() && rows.is_empty() {
                return Err(MountJournalError::invalid(
                    self.journal.staged_window_path(window),
                    MountJournalValidationError::EmptyStagedWindow { window },
                ));
            }
            if let Some(target) = self.target
                && rows.iter().any(|row| !row.path.is_or_under(target))
            {
                return Err(MountJournalError::invalid(
                    self.journal.staged_window_path(window),
                    MountJournalValidationError::StagedRowOutsideTarget { window },
                ));
            }
            Ok(rows)
        }))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count - self.next;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for StagedWindows<'_> {}

fn validate_record_shape(path: &Path, record: &MountTxnRecord) -> Result<()> {
    let invalid = match record.op {
        MountTxnOp::Add if record.old_target.is_some() => {
            Some(MountJournalValidationError::AddHasOldTarget)
        }
        MountTxnOp::Add if record.new_target.is_none() => {
            Some(MountJournalValidationError::AddMissingNewTarget)
        }
        MountTxnOp::Update if record.old_target.is_none() => {
            Some(MountJournalValidationError::UpdateMissingOldTarget)
        }
        MountTxnOp::Update if record.new_target.is_none() => {
            Some(MountJournalValidationError::UpdateMissingNewTarget)
        }
        MountTxnOp::Remove if record.old_target.is_none() => {
            Some(MountJournalValidationError::RemoveMissingOldTarget)
        }
        MountTxnOp::Remove if record.new_target.is_some() => {
            Some(MountJournalValidationError::RemoveHasNewTarget)
        }
        MountTxnOp::Remove if record.row_windows != 0 => {
            Some(MountJournalValidationError::RemoveHasStagedWindows)
        }
        _ => None,
    };
    match invalid {
        Some(reason) => Err(MountJournalError::invalid(path, reason)),
        None => Ok(()),
    }
}

const fn encode_scope(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Global => "global",
        ConfigScope::Project => "project",
        ConfigScope::Local => "local",
    }
}

fn decode_scope(tag: &str) -> Option<ConfigScope> {
    match tag {
        "global" => Some(ConfigScope::Global),
        "project" => Some(ConfigScope::Project),
        "local" => Some(ConfigScope::Local),
        _ => None,
    }
}

fn remove_file_if_exists(path: &Path, operation: &'static str) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MountJournalError::io(operation, path, source)),
    }
}

fn remove_dir_if_exists(path: &Path, operation: &'static str) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(MountJournalError::io(operation, path, source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(root: &Path) -> RepositoryLayout {
        RepositoryLayout::at(root.to_path_buf())
    }

    fn record() -> MountTxnRecord {
        MountTxnRecord {
            version: MOUNT_TXN_VERSION,
            op: MountTxnOp::Add,
            scope: ConfigScope::Project,
            name: MountName::from_string("data".to_string()),
            old_target: None,
            new_target: Some(GatPath::from_canonical_string("data".to_string()).unwrap()),
            post_config: Config::default(),
            pre_config: Config::default(),
            row_windows: 0,
        }
    }

    #[test]
    fn derives_stable_paths_and_window_names() {
        let repository = RepositoryLayout::at(PathBuf::from("/repo"));
        let journal = MountJournal::open(&repository);
        assert_eq!(
            journal.path(),
            PathBuf::from("/repo/.gat/mount-transaction.json")
        );
        assert_eq!(
            journal.staged_rows_dir(),
            PathBuf::from("/repo/.gat/mount-transaction-rows")
        );
        assert_eq!(
            journal.staged_window_path(12),
            PathBuf::from("/repo/.gat/mount-transaction-rows/window-00012.json")
        );
    }

    #[test]
    fn missing_journal_and_idempotent_removal_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = layout(tmp.path());
        let journal = MountJournal::open(&repository);
        assert!(journal.read().unwrap().is_none());
        journal.remove_journal().unwrap();
        journal.remove_staged_rows().unwrap();
    }

    #[test]
    fn journal_round_trip_preserves_typed_payload_and_stable_scope_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = layout(tmp.path());
        let journal = MountJournal::open(&repository);
        journal.write(&record()).unwrap();

        let text = std::fs::read_to_string(journal.path()).unwrap();
        assert!(text.contains("\"scope\": \"project\""));
        let decoded = journal.read().unwrap().unwrap();
        assert_eq!(decoded.version, MOUNT_TXN_VERSION);
        assert_eq!(decoded.op, MountTxnOp::Add);
        assert_eq!(decoded.scope, ConfigScope::Project);
        assert_eq!(decoded.name.as_str(), "data");
        assert_eq!(decoded.new_target.unwrap().as_str(), "data");
        assert_eq!(decoded.row_windows, 0);
    }

    #[test]
    fn unsupported_version_and_unknown_scope_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = layout(tmp.path());
        let journal = MountJournal::open(&repository);

        let mut unsupported = record();
        unsupported.version += 1;
        journal.write(&unsupported).unwrap();
        assert!(matches!(
            journal.read(),
            Err(MountJournalError::UnsupportedVersion { .. })
        ));

        journal.write(&record()).unwrap();
        let path = journal.path();
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"scope\": \"project\"", "\"scope\": \"future\"");
        std::fs::write(path, text).unwrap();
        assert!(matches!(
            journal.read(),
            Err(MountJournalError::UnknownConfigScope { .. })
        ));
    }

    #[test]
    fn staged_windows_round_trip_in_bounded_order() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = layout(tmp.path());
        let journal = MountJournal::open(&repository);
        journal.reset_staged_rows().unwrap();
        let oid = Oid::from_hex(&"0".repeat(64)).unwrap();
        for window in 0..3 {
            journal
                .write_staged_window(
                    window,
                    &[StagedRow {
                        path: GatPath::from_canonical_string(format!("data/file-{window}"))
                            .unwrap(),
                        oid,
                    }],
                )
                .unwrap();
        }
        let paths: Vec<_> = journal
            .staged_windows(3)
            .map(|rows| rows.unwrap().pop().unwrap().path)
            .collect();
        assert_eq!(
            paths,
            ["data/file-0", "data/file-1", "data/file-2"]
                .map(|path| GatPath::from_canonical_string(path.to_string()).unwrap())
        );
    }

    #[test]
    fn operation_target_and_window_combinations_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = layout(tmp.path());
        let journal = MountJournal::open(&repository);
        let target = GatPath::parse_canonical("data").unwrap();

        let mut add_with_old = record();
        add_with_old.old_target = Some(target.clone());
        assert!(matches!(
            journal.write(&add_with_old),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::AddHasOldTarget,
                ..
            })
        ));

        let mut add_without_new = record();
        add_without_new.new_target = None;
        assert!(matches!(
            journal.write(&add_without_new),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::AddMissingNewTarget,
                ..
            })
        ));

        let mut update_without_old = record();
        update_without_old.op = MountTxnOp::Update;
        assert!(matches!(
            journal.write(&update_without_old),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::UpdateMissingOldTarget,
                ..
            })
        ));

        let mut update_without_new = record();
        update_without_new.op = MountTxnOp::Update;
        update_without_new.old_target = Some(target.clone());
        update_without_new.new_target = None;
        assert!(matches!(
            journal.write(&update_without_new),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::UpdateMissingNewTarget,
                ..
            })
        ));

        let mut remove_without_old = record();
        remove_without_old.op = MountTxnOp::Remove;
        remove_without_old.new_target = None;
        assert!(matches!(
            journal.write(&remove_without_old),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::RemoveMissingOldTarget,
                ..
            })
        ));

        let mut remove_with_new = record();
        remove_with_new.op = MountTxnOp::Remove;
        remove_with_new.old_target = Some(target.clone());
        assert!(matches!(
            journal.write(&remove_with_new),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::RemoveHasNewTarget,
                ..
            })
        ));

        let mut remove_with_rows = record();
        remove_with_rows.op = MountTxnOp::Remove;
        remove_with_rows.old_target = Some(target);
        remove_with_rows.new_target = None;
        remove_with_rows.row_windows = 1;
        assert!(matches!(
            journal.write(&remove_with_rows),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::RemoveHasStagedWindows,
                ..
            })
        ));
    }

    #[test]
    fn journal_read_rejects_missing_and_extra_staged_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = layout(tmp.path());
        let journal = MountJournal::open(&repository);
        let mut missing = record();
        missing.row_windows = 1;
        journal.write(&missing).unwrap();
        assert!(matches!(
            journal.read(),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::MissingStagedWindow { window: 0 },
                ..
            })
        ));

        journal.reset_staged_rows().unwrap();
        journal
            .write_staged_window(
                0,
                &[StagedRow {
                    path: GatPath::parse_canonical("data/a.bin").unwrap(),
                    oid: Oid::from_hex(&"a".repeat(64)).unwrap(),
                }],
            )
            .unwrap();
        journal.write(&record()).unwrap();
        assert!(matches!(
            journal.read(),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::UnexpectedStagedEntry,
                ..
            })
        ));
    }

    #[test]
    fn staged_rows_must_stay_within_the_recorded_target() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = layout(tmp.path());
        let journal = MountJournal::open(&repository);
        journal.reset_staged_rows().unwrap();
        journal
            .write_staged_window(
                0,
                &[StagedRow {
                    path: GatPath::parse_canonical("outside/a.bin").unwrap(),
                    oid: Oid::from_hex(&"a".repeat(64)).unwrap(),
                }],
            )
            .unwrap();
        let mut transaction = record();
        transaction.row_windows = 1;
        journal.write(&transaction).unwrap();
        assert!(matches!(
            journal.read(),
            Err(MountJournalError::InvalidRecord {
                reason: MountJournalValidationError::StagedRowOutsideTarget { window: 0 },
                ..
            })
        ));
    }
}
