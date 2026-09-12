use std::path::{Path, PathBuf};
use std::rc::Rc;

use gat_core::git::GitRevisionSpec;
use gat_core::lock::Entry;
#[cfg(any(test, feature = "test-support"))]
use gat_core::lock::Lock;
use gat_core::lock::validated::{FilteredRowCursor, visit_filtered_matching};
use gat_core::lock::{LockShardId, LockShardLevels};
use gat_core::selection::Selection;

use super::BoxedSource;
use crate::RepositoryLayout;

/// Semantic stage at which a persisted Git lock snapshot could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockSnapshotErrorKind {
    OpenRepository,
    IndexLookup,
    RevisionResolution,
    ObjectRead,
    InvalidSnapshot,
}

/// Failure to discover or read a staged or committed `gat.lock` snapshot.
#[derive(Debug)]
pub struct LockSnapshotError {
    kind: LockSnapshotErrorKind,
    root: PathBuf,
    label: String,
    operation: String,
    detail: Option<String>,
    source: Option<BoxedSource>,
}

impl LockSnapshotError {
    #[must_use]
    pub const fn kind(&self) -> LockSnapshotErrorKind {
        self.kind
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    fn open(root: &Path, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind: LockSnapshotErrorKind::OpenRepository,
            root: root.to_path_buf(),
            label: "gat.lock".to_string(),
            operation: "opening git repository".to_string(),
            detail: None,
            source: Some(Box::new(source)),
        }
    }

    fn index(
        root: &Path,
        operation: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: LockSnapshotErrorKind::IndexLookup,
            root: root.to_path_buf(),
            label: "gat.lock".to_string(),
            operation: operation.into(),
            detail: None,
            source: Some(Box::new(source)),
        }
    }

    fn revision(
        root: &Path,
        revision: &str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: LockSnapshotErrorKind::RevisionResolution,
            root: root.to_path_buf(),
            label: format!("gat.lock at `{revision}`"),
            operation: revision.to_string(),
            detail: None,
            source: Some(Box::new(source)),
        }
    }

    fn revision_with_boxed_source(root: &Path, revision: &str, source: BoxedSource) -> Self {
        Self {
            kind: LockSnapshotErrorKind::RevisionResolution,
            root: root.to_path_buf(),
            label: format!("gat.lock at `{revision}`"),
            operation: revision.to_string(),
            detail: None,
            source: Some(source),
        }
    }

    fn object(
        root: &Path,
        label: &str,
        operation: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: LockSnapshotErrorKind::ObjectRead,
            root: root.to_path_buf(),
            label: label.to_string(),
            operation: operation.into(),
            detail: None,
            source: Some(Box::new(source)),
        }
    }

    fn invalid(
        root: &Path,
        label: &str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        let detail = source.to_string();
        Self::invalid_with_detail(root, label, detail, source)
    }

    fn invalid_with_detail(
        root: &Path,
        label: &str,
        detail: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: LockSnapshotErrorKind::InvalidSnapshot,
            root: root.to_path_buf(),
            label: label.to_string(),
            operation: "validating persisted lock snapshot".to_string(),
            detail: Some(detail.into()),
            source: Some(Box::new(source)),
        }
    }
}

impl std::fmt::Display for LockSnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            LockSnapshotErrorKind::OpenRepository => {
                write!(
                    f,
                    "could not open the git repository at `{}`",
                    self.root.display()
                )
            }
            LockSnapshotErrorKind::IndexLookup | LockSnapshotErrorKind::ObjectRead => {
                write!(f, "{} failed", self.operation)
            }
            LockSnapshotErrorKind::RevisionResolution => {
                write!(f, "could not resolve revision `{}`", self.operation)
            }
            LockSnapshotErrorKind::InvalidSnapshot => write!(
                f,
                "{} is not a valid gat lock file: {}",
                self.label,
                self.detail.as_deref().unwrap_or("invalid snapshot")
            ),
        }
    }
}

impl std::error::Error for LockSnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

/// One logical shard of a persisted snapshot.
#[derive(Clone, PartialEq, Eq)]
pub struct SnapshotShard {
    pub id: LockShardId,
    blob: gix::ObjectId,
}

impl std::fmt::Debug for SnapshotShard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnapshotShard")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl SnapshotShard {
    /// Whether two logical shards refer to the same immutable Git blob.
    #[must_use]
    pub fn has_same_blob(&self, other: &Self) -> bool {
        self.blob == other.blob
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn for_test(id: LockShardId) -> Self {
        Self {
            id,
            blob: gix::ObjectId::null(gix::hash::Kind::Sha1),
        }
    }
}

/// A staged or committed `gat.lock` snapshot with all Gix state owned privately.
pub struct LockSnapshot {
    repo: Rc<gix::Repository>,
    root: PathBuf,
    shard_levels: LockShardLevels,
    shards: Vec<SnapshotShard>,
    label: String,
}

#[cfg(test)]
mod error_tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn invalid_snapshot_retains_typed_source_and_repository_context() {
        let root = Path::new("/repo");
        let source = LockShardId::parse_canonical("gat.lock/not-a-shard").unwrap_err();
        let error = LockSnapshotError::invalid(root, "gat.lock at `HEAD`", source);

        assert_eq!(error.root(), root);
        assert_eq!(error.label(), "gat.lock at `HEAD`");
        assert!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<gat_core::lock::LockShardIdError>())
                .is_some()
        );
    }
}

impl LockSnapshot {
    pub fn staged(layout: &RepositoryLayout) -> Result<Self, LockSnapshotError> {
        let root = layout.root_path();
        let reader = super::GitReader::open(layout)
            .map_err(|source| LockSnapshotError::open(root, source))?;
        reader.staged_lock_snapshot()
    }

    pub(super) fn staged_with(
        repo: Rc<gix::Repository>,
        root: &Path,
    ) -> Result<Self, LockSnapshotError> {
        let index = repo
            .index_or_empty()
            .map_err(|source| LockSnapshotError::index(root, "opening git index", source))?;
        let mut shards = Vec::new();
        for entry in index.entries() {
            let path = entry.path(&index);
            if path == "gat.lock" || path.starts_with(b"gat.lock/") {
                let text = std::str::from_utf8(path).map_err(|source| {
                    LockSnapshotError::invalid_with_detail(
                        root,
                        "gat.lock",
                        "staged gat.lock path is not valid UTF-8",
                        source,
                    )
                })?;
                let id = LockShardId::parse_canonical(text)
                    .map_err(|source| LockSnapshotError::invalid(root, "gat.lock", source))?;
                shards.push(SnapshotShard { id, blob: entry.id });
            }
        }
        Self::new(repo, root, shards, "gat.lock".to_string())
    }

    pub fn at_rev(
        layout: &RepositoryLayout,
        revision: &GitRevisionSpec,
    ) -> Result<Self, LockSnapshotError> {
        let root = layout.root_path();
        let reader = super::GitReader::open(layout)
            .map_err(|source| LockSnapshotError::open(root, source))?;
        reader.lock_snapshot_at(revision)
    }

    pub(super) fn at_rev_with(
        repo: Rc<gix::Repository>,
        root: &Path,
        revision: &GitRevisionSpec,
    ) -> Result<Self, LockSnapshotError> {
        let revision = revision.as_str();
        let label = format!("gat.lock at `{revision}`");
        let shards = {
            let commit = super::resolve_commit_object(&repo, revision).map_err(|source| {
                LockSnapshotError::revision_with_boxed_source(root, revision, source)
            })?;
            let tree = commit
                .tree()
                .map_err(|source| LockSnapshotError::revision(root, revision, source))?;
            let entry = tree
                .lookup_entry_by_path("gat.lock")
                .map_err(|source| LockSnapshotError::revision(root, revision, source))?;
            let mut shards = Vec::new();
            if let Some(entry) = entry {
                if entry.mode().is_tree() {
                    let shard_tree = entry
                        .object()
                        .map_err(|source| LockSnapshotError::revision(root, revision, source))?
                        .into_tree();
                    let files = shard_tree
                        .traverse()
                        .breadthfirst
                        .files()
                        .map_err(|source| {
                            LockSnapshotError::object(
                                root,
                                &label,
                                format!("traversing gat.lock/ at `{revision}`"),
                                source,
                            )
                        })?;
                    for file in files {
                        if file.mode.is_blob() {
                            let text = format!("gat.lock/{}", file.filepath);
                            let id = LockShardId::parse_canonical(&text).map_err(|source| {
                                LockSnapshotError::invalid(root, &label, source)
                            })?;
                            shards.push(SnapshotShard { id, blob: file.oid });
                        }
                    }
                } else {
                    shards.push(SnapshotShard {
                        id: LockShardId::flat(),
                        blob: entry.object_id(),
                    });
                }
            }
            shards
        };
        Self::new(repo, root, shards, label)
    }

    fn new(
        repo: Rc<gix::Repository>,
        root: &Path,
        mut shards: Vec<SnapshotShard>,
        label: String,
    ) -> Result<Self, LockSnapshotError> {
        shards.sort_by_key(|shard| shard.id);
        let shard_levels = crate::lock::shard_levels_for_ids(shards.iter().map(|shard| shard.id))
            .map_err(|source| LockSnapshotError::invalid(root, &label, source))?;
        Ok(Self {
            repo,
            root: root.to_path_buf(),
            shard_levels,
            shards,
            label,
        })
    }

    #[must_use]
    pub const fn shard_levels(&self) -> LockShardLevels {
        self.shard_levels
    }

    #[must_use]
    pub fn shards(&self) -> &[SnapshotShard] {
        &self.shards
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    fn invalid(&self, source: impl std::error::Error + Send + Sync + 'static) -> LockSnapshotError {
        LockSnapshotError::invalid(&self.root, &self.label, source)
    }

    fn shard_rows_matching(
        &self,
        shard: &SnapshotShard,
        keep: impl FnMut(&str) -> bool,
    ) -> Result<Vec<Entry>, LockSnapshotError> {
        let mut kept = Vec::new();
        self.visit_shard_rows_matching(shard, keep, |entry| {
            kept.push(entry);
            Ok(())
        })?;
        Ok(kept)
    }

    fn visit_shard_rows_matching(
        &self,
        shard: &SnapshotShard,
        keep: impl FnMut(&str) -> bool,
        mut visit: impl FnMut(Entry) -> Result<(), LockSnapshotError>,
    ) -> Result<(), LockSnapshotError> {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_shard_blob_read();
        let blob = self.repo.find_object(shard.blob).map_err(|source| {
            LockSnapshotError::object(
                &self.root,
                &self.label,
                format!("reading {} blob", self.label),
                source,
            )
        })?;
        let text = std::str::from_utf8(&blob.data).map_err(|source| {
            LockSnapshotError::invalid_with_detail(
                &self.root,
                &self.label,
                format!("shard blob is not valid UTF-8: {source}"),
                source,
            )
        })?;
        let mut visit_err = None;
        let outcome = visit_filtered_matching(text, keep, |entry| {
            visit(entry).map_err(|error| {
                visit_err = Some(error);
                gat_core::lock::LockError::CallbackFailed
            })
        });
        if let Some(error) = visit_err {
            return Err(error);
        }
        outcome.map_err(|error| self.invalid(error))
    }

    pub fn shard_rows_selected(
        &self,
        shard: &SnapshotShard,
        selection: &Selection,
    ) -> Result<Vec<Entry>, LockSnapshotError> {
        self.shard_rows_matching(shard, |path| selection.matches_str(path))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn shard_entries(&self, shard: &SnapshotShard) -> Result<Vec<Entry>, LockSnapshotError> {
        self.shard_rows_matching(shard, |_| true)
    }

    pub fn with_shard_rows_pull<R, E>(
        &self,
        shard: &SnapshotShard,
        selection: &Selection,
        body: impl FnOnce(&mut dyn FnMut() -> Result<Option<Entry>, E>) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<LockSnapshotError>,
    {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_shard_blob_read();
        let blob = self.repo.find_object(shard.blob).map_err(|source| {
            E::from(LockSnapshotError::object(
                &self.root,
                &self.label,
                format!("reading {} blob", self.label),
                source,
            ))
        })?;
        let text = std::str::from_utf8(&blob.data).map_err(|source| {
            E::from(LockSnapshotError::invalid_with_detail(
                &self.root,
                &self.label,
                format!("shard blob is not valid UTF-8: {source}"),
                source,
            ))
        })?;
        let mut cursor = FilteredRowCursor::new(text, |path: &str| selection.matches_str(path))
            .map_err(|error| E::from(self.invalid(error)))?;
        let mut pull = || Ok(cursor.next());
        body(&mut pull)
    }

    pub fn rows_sorted(&self, selection: &Selection) -> Result<Vec<Entry>, LockSnapshotError> {
        let mut entries = Vec::new();
        for shard in &self.shards {
            entries.extend(self.shard_rows_matching(shard, |path| selection.matches_str(path))?);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn entry_for_path(
        &self,
        path: &gat_core::lexical_path::GatPath,
    ) -> Result<Option<Entry>, LockSnapshotError> {
        let Some(shard) = self.shard_for_path(path) else {
            return Ok(None);
        };
        Ok(self
            .shard_rows_matching(shard, |candidate| candidate == path.as_str())?
            .into_iter()
            .next())
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn shard_for_path(&self, path: &gat_core::lexical_path::GatPath) -> Option<&SnapshotShard> {
        if self.shard_levels.is_flat() {
            self.shards.first()
        } else {
            let id = crate::lock::shard_id_for_path(path, self.shard_levels);
            self.shards
                .binary_search_by(|shard| shard.id.cmp(&id))
                .ok()
                .map(|index| &self.shards[index])
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn to_lock(&self) -> Result<Lock, LockSnapshotError> {
        Ok(Lock {
            entries: self.rows_sorted(&Selection::root())?,
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static SHARD_BLOB_READS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn record_shard_blob_read() {
        SHARD_BLOB_READS.with(|counter| counter.set(counter.get() + 1));
    }

    pub fn shard_blob_reads() -> usize {
        SHARD_BLOB_READS.with(Cell::get)
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use gat_core::git::GitRevisionSpec;
    use gat_core::lexical_path::GatPath;
    use gat_core::lock::{Entry, Lock, LockShardLevels};
    use gat_core::oid::Oid;
    use gat_core::path_scope::normalize_path_scope;
    use gat_core::selection::Selection;

    fn layout(root: &Path) -> crate::RepositoryLayout {
        crate::RepositoryLayout::at(root.to_path_buf())
    }

    fn scoped_selection(path: &Path) -> Selection {
        Selection::from_scope_patterns(normalize_path_scope(path).unwrap(), Vec::new(), Vec::new())
    }

    fn git(root: &Path, args: &[&str]) {
        test_support_git::GitCommand::empty(root)
            .args(["-c", "core.autocrlf=false"])
            .args(args)
            .run();
    }

    fn test_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["config", "user.name", "Test"]);
        git(
            tmp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        tmp
    }

    fn commit_all(root: &Path, message: &str) {
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", message]);
    }

    fn gp(path: &str) -> GatPath {
        GatPath::parse_canonical(path).unwrap()
    }

    fn oid(value: String) -> Oid {
        Oid::from_hex(&value).unwrap()
    }

    fn snapshot_repo(
        shard_levels: LockShardLevels,
        paths: &[&str],
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let lock = Lock {
            entries: paths
                .iter()
                .enumerate()
                .map(|(i, path)| Entry {
                    path: gp(path),
                    oid: oid(format!("{:064x}", i + 1)),
                })
                .collect(),
        };
        crate::LockStore::publish_repository(
            &crate::RepositoryLayout::at(root.clone()),
            &lock,
            shard_levels,
        )
        .unwrap();
        git(&root, &["add", "-A"]);
        commit_all(&root, "persist lock");
        (tmp, root)
    }

    #[test]
    fn flat_snapshot_is_one_logical_shard() {
        let (_tmp, gix_repo) = snapshot_repo(LockShardLevels::FLAT, &["a.bin", "data/b.bin"]);
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        assert!(snapshot.shard_levels().is_flat());
        assert_eq!(snapshot.shards().len(), 1);
        assert_eq!(snapshot.shards()[0].id, "gat.lock");
        assert_eq!(
            snapshot
                .to_lock()
                .unwrap()
                .entries
                .into_iter()
                .map(|e| e.path.to_string())
                .collect::<Vec<_>>(),
            vec!["a.bin", "data/b.bin"]
        );
    }

    #[test]
    fn sharded_snapshot_detects_its_own_depth_from_persisted_state() {
        let paths: Vec<String> = (0..40).map(|i| format!("data/f{i:03}.bin")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (_tmp, gix_repo) = snapshot_repo(LockShardLevels::new(2).unwrap(), &refs);
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        assert_eq!(
            snapshot.shard_levels(),
            gat_core::lock::LockShardLevels::new(2).unwrap()
        );
        assert!(snapshot.shards().len() > 1);
        for shard in snapshot.shards() {
            assert!(!shard.id.is_flat(), "{}", shard.id.to_canonical_string());
        }
        let mut got: Vec<String> = snapshot
            .to_lock()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.path.to_string())
            .collect();
        got.sort();
        assert_eq!(got, paths);
    }

    #[test]
    fn committed_snapshot_rejects_a_tree_mixing_the_flat_sentinel_with_sharded_leaves() {
        // A committed `gat.lock` tree that somehow has *both* a flat
        // blob leaf directly under `gat.lock/` (not the flat sentinel,
        // which is a plain `gat.lock` blob -- this is a nested single
        // hex-pair leaf, still depth 1) and a deeper depth-2 leaf is a
        // mixed-topology tree: reject it via
        // `gat_core::lock::shard_levels_for_ids`, never derive a shape
        // from whichever leaf `detect_shape` sees first.
        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let dir = root.join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("00.tsv"),
            format!("{0}\n{1}\tfoo\n", gat_core::lock::VERSION, "a".repeat(64)),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("11")).unwrap();
        std::fs::write(
            dir.join("11").join("22.tsv"),
            format!("{0}\n{1}\tbar\n", gat_core::lock::VERSION, "b".repeat(64)),
        )
        .unwrap();
        git(&root, &["add", "-A"]);
        commit_all(&root, "mixed depth lock");
        let gix_repo = root;

        let err = LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD"))
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains("more than one shard depth"),
            "expected a mixed-shard-topology error, got {err:#}"
        );
    }

    #[test]
    fn staged_snapshot_rejects_a_tree_mixing_the_flat_sentinel_with_sharded_leaves() {
        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let dir = root.join("gat.lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("00.tsv"),
            format!("{0}\n{1}\tfoo\n", gat_core::lock::VERSION, "a".repeat(64)),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("11")).unwrap();
        std::fs::write(
            dir.join("11").join("22.tsv"),
            format!("{0}\n{1}\tbar\n", gat_core::lock::VERSION, "b".repeat(64)),
        )
        .unwrap();
        git(&root, &["add", "-A"]);
        let gix_repo = root;

        let err = LockSnapshot::staged(&layout(&gix_repo)).err().unwrap();
        assert!(
            format!("{err:#}").contains("more than one shard depth"),
            "expected a mixed-shard-topology error, got {err:#}"
        );
    }

    #[test]
    fn shard_rows_selected_returns_only_the_selection_matched_rows() {
        let (_tmp, gix_repo) = snapshot_repo(
            LockShardLevels::FLAT,
            &["a.bin", "data/b.bin", "data/c.bin", "z.bin"],
        );
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        let shard = snapshot.shards()[0].clone();
        let selection = scoped_selection(Path::new("data"));

        let selected = snapshot.shard_rows_selected(&shard, &selection).unwrap();
        let mut selected_paths: Vec<&str> = selected.iter().map(|e| e.path.as_str()).collect();
        selected_paths.sort_unstable();
        assert_eq!(selected_paths, vec!["data/b.bin", "data/c.bin"]);

        // Equivalent to a full read filtered afterward -- this only
        // changes when the filtering happens, never what survives it.
        let mut full = snapshot.shard_entries(&shard).unwrap();
        full.retain(|e| selection.matches(&e.path));
        full.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(selected, full);
    }

    /// Selection consumes one complete certification of the source blob.
    #[test]
    fn scoped_flat_read_certifies_the_file_once() {
        let (_tmp, gix_repo) = snapshot_repo(
            LockShardLevels::FLAT,
            &["a.bin", "data/b.bin", "data/c.bin", "z.bin"],
        );
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        let shard = snapshot.shards()[0].clone();
        let selection = scoped_selection(Path::new("data"));

        let before = crate::lock::test_support::file_validation_parses();
        let selected = snapshot.shard_rows_selected(&shard, &selection).unwrap();
        assert_eq!(
            crate::lock::test_support::file_validation_parses() - before,
            1,
            "a scoped read certifies the complete file exactly once"
        );
        let mut selected_paths: Vec<&str> = selected.iter().map(|e| e.path.as_str()).collect();
        selected_paths.sort_unstable();
        assert_eq!(selected_paths, vec!["data/b.bin", "data/c.bin"]);
    }

    /// Exact-path lookups must resolve through hash placement, i.e. parse
    /// exactly the one shard that can hold the path -- and must agree with
    /// a full traversal for present and absent paths alike.
    #[test]
    fn exact_path_lookup_targets_one_shard() {
        let paths: Vec<String> = (0..40).map(|i| format!("data/f{i:03}.bin")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (_tmp, gix_repo) = snapshot_repo(LockShardLevels::new(2).unwrap(), &refs);
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        let full = snapshot.to_lock().unwrap();

        for path in &paths {
            let expected = full.entries.iter().find(|e| e.path == path).cloned();
            let before = test_support::shard_blob_reads();
            assert_eq!(snapshot.entry_for_path(&gp(path)).unwrap(), expected);
            // Structural, not just output equivalence: a lookup for one
            // path must parse exactly the one shard blob that can hold
            // it, never every shard the snapshot has.
            assert_eq!(
                test_support::shard_blob_reads() - before,
                1,
                "exact-path lookup for {path:?} must read exactly one shard blob"
            );
            let shard = snapshot.shard_for_path(&gp(path)).unwrap();
            assert_eq!(
                shard.id,
                crate::lock::shard_id_for_path(&gp(path), LockShardLevels::new(2).unwrap())
            );
        }
        let before = test_support::shard_blob_reads();
        assert_eq!(
            snapshot.entry_for_path(&gp("data/absent.bin")).unwrap(),
            None
        );
        // The absent path's predicted shard may or may not exist in this
        // snapshot at all: if it doesn't, the lookup correctly reads no
        // blob rather than reading one to find it empty, so at most one
        // read (never more) is the invariant here.
        assert!(test_support::shard_blob_reads() - before <= 1);
    }

    #[test]
    fn flat_exact_path_lookup_scans_its_single_shard() {
        let (_tmp, gix_repo) = snapshot_repo(LockShardLevels::FLAT, &["a.bin", "data/b.bin"]);
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        assert_eq!(
            snapshot
                .entry_for_path(&gp("data/b.bin"))
                .unwrap()
                .unwrap()
                .path,
            "data/b.bin"
        );
        assert_eq!(snapshot.entry_for_path(&gp("nope.bin")).unwrap(), None);
    }

    /// A committed `gat.lock` blob containing invalid UTF-8 must be
    /// reported as a typed invalid-snapshot error, never
    /// silently repaired via lossy replacement-character substitution
    /// (which would corrupt paths/oids instead of failing loudly).
    #[test]
    fn committed_lock_blob_with_invalid_utf8_is_reported_not_lossily_repaired() {
        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        let mut bytes = format!("{}\n", gat_core::lock::VERSION).into_bytes();
        bytes.extend_from_slice(b"\"\xffbad.bin\"\tblake3:");
        bytes.extend_from_slice("a".repeat(64).as_bytes());
        bytes.push(b'\n');
        std::fs::write(root.join("gat.lock"), &bytes).unwrap();
        git(&root, &["add", "-A"]);
        commit_all(&root, "invalid utf8 lock");
        let gix_repo = root;
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        let shard = snapshot.shards()[0].clone();

        let err = snapshot.shard_entries(&shard).unwrap_err();
        assert_eq!(err.kind(), LockSnapshotErrorKind::InvalidSnapshot);
        assert!(format!("{err:#}").contains("not valid UTF-8"));

        // The pull-based path must fail the same way, not just the
        // push-based one.
        let selection = Selection::root();
        let pull_err = snapshot
            .with_shard_rows_pull(
                &shard,
                &selection,
                |pull| -> std::result::Result<(), LockSnapshotError> {
                    loop {
                        pull()?;
                    }
                },
            )
            .unwrap_err();
        assert_eq!(pull_err.kind(), LockSnapshotErrorKind::InvalidSnapshot);
    }

    #[test]
    fn missing_persisted_lock_is_an_empty_snapshot() {
        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        std::fs::write(root.join("readme.txt"), b"hi").unwrap();
        commit_all(&root, "no lock");
        let gix_repo = root;
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        assert!(snapshot.is_empty());
        assert!(snapshot.shard_levels().is_flat());
        assert_eq!(snapshot.to_lock().unwrap(), Lock::default());
        assert_eq!(snapshot.entry_for_path(&gp("a.bin")).unwrap(), None);
    }

    #[test]
    fn staged_snapshot_sees_staged_but_uncommitted_shards() {
        let (tmp, _committed) =
            snapshot_repo(LockShardLevels::new(1).unwrap(), &["a.bin", "data/b.bin"]);
        let root = tmp.path().to_path_buf();

        let mut lock = crate::LockStore::load_all(&root).unwrap();
        lock.upsert(gp("c.bin"), oid(format!("{:064x}", 99)));
        crate::LockStore::publish_repository(
            &crate::RepositoryLayout::at(root.clone()),
            &lock,
            gat_core::lock::LockShardLevels::new(1).unwrap(),
        )
        .unwrap();
        git(&root, &["add", "-A"]);

        let gix_repo = root;
        let staged = LockSnapshot::staged(&layout(&gix_repo)).unwrap();
        assert_eq!(
            staged.shard_levels(),
            gat_core::lock::LockShardLevels::new(1).unwrap()
        );
        let mut paths: Vec<String> = staged
            .to_lock()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.path.to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.bin", "c.bin", "data/b.bin"]);
        assert_eq!(
            staged.entry_for_path(&gp("c.bin")).unwrap().unwrap().oid,
            gat_core::oid::Oid::from_hex(&format!("{:064x}", 99)).unwrap()
        );
    }

    /// A flat snapshot whose committed `gat.lock` blob is itself CRLF
    /// (not merely checkout-converted -- the blob bytes actually stored
    /// in git are `\r\n`-terminated), the same shape a real
    /// `core.autocrlf=true` `git add` would produce. Every persisted-
    /// state read path (exact, scoped, streaming/merge-walk, and a plain
    /// full traversal) must parse it identically to the canonical LF
    /// case.
    fn crlf_snapshot_repo(paths: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let (tmp, _committed) = snapshot_repo(LockShardLevels::FLAT, paths);
        let root = tmp.path().to_path_buf();
        let lf = std::fs::read_to_string(root.join("gat.lock")).unwrap();
        let crlf = lf.replace('\n', "\r\n");
        std::fs::write(root.join("gat.lock"), &crlf).unwrap();
        git(&root, &["add", "-A"]);
        commit_all(&root, "crlf lock");
        (tmp, root)
    }

    #[test]
    fn committed_crlf_snapshot_parses_the_same_as_the_lf_committed_one() {
        let (_tmp, gix_repo) = crlf_snapshot_repo(&["a.bin", "data/b.bin", "data/c.bin", "z.bin"]);
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        let mut paths: Vec<String> = snapshot
            .to_lock()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.path.to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.bin", "data/b.bin", "data/c.bin", "z.bin"]);
    }

    #[test]
    fn staged_crlf_snapshot_parses_the_same_as_the_lf_staged_one() {
        let (tmp, _committed) = crlf_snapshot_repo(&["a.bin", "data/b.bin"]);
        let root = tmp.path().to_path_buf();

        let mut lock = crate::LockStore::load_all(&root).unwrap();
        lock.upsert(gp("c.bin"), oid(format!("{:064x}", 99)));
        crate::LockStore::publish_repository(
            &crate::RepositoryLayout::at(root.clone()),
            &lock,
            gat_core::lock::LockShardLevels::new(0).unwrap(),
        )
        .unwrap();
        let lf = std::fs::read_to_string(root.join("gat.lock")).unwrap();
        std::fs::write(root.join("gat.lock"), lf.replace('\n', "\r\n")).unwrap();
        git(&root, &["add", "-A"]);

        let gix_repo = root;
        let staged = LockSnapshot::staged(&layout(&gix_repo)).unwrap();
        let mut paths: Vec<String> = staged
            .to_lock()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.path.to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a.bin", "c.bin", "data/b.bin"]);
    }

    /// Exact-path lookup (`entry_for_path`, targeting one shard by hash
    /// placement) must resolve correctly against a CRLF-committed shard.
    #[test]
    fn exact_path_lookup_parses_a_crlf_shard_correctly() {
        let (_tmp, gix_repo) = crlf_snapshot_repo(&["a.bin", "data/b.bin", "z.bin"]);
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        assert_eq!(
            snapshot
                .entry_for_path(&gp("data/b.bin"))
                .unwrap()
                .unwrap()
                .path,
            "data/b.bin"
        );
        assert_eq!(snapshot.entry_for_path(&gp("nope.bin")).unwrap(), None);
    }

    /// Scoped selection reads (`shard_rows_selected`) must return the
    /// same rows against a CRLF-committed shard as against the LF one.
    #[test]
    fn scoped_read_parses_a_crlf_shard_correctly() {
        let (_tmp, gix_repo) = crlf_snapshot_repo(&["a.bin", "data/b.bin", "data/c.bin", "z.bin"]);
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        let shard = snapshot.shards()[0].clone();
        let selection = scoped_selection(Path::new("data"));

        let selected = snapshot.shard_rows_selected(&shard, &selection).unwrap();
        let mut selected_paths: Vec<&str> = selected.iter().map(|e| e.path.as_str()).collect();
        selected_paths.sort_unstable();
        assert_eq!(selected_paths, vec!["data/b.bin", "data/c.bin"]);
    }

    /// The streaming/merge-walk pull path (`with_shard_rows_pull`) must
    /// pull the same selected rows, in order, from a CRLF-committed shard
    /// as from the LF one.
    #[test]
    fn streaming_pull_parses_a_crlf_shard_correctly() {
        let (_tmp, gix_repo) = crlf_snapshot_repo(&["a.bin", "data/b.bin", "data/c.bin", "z.bin"]);
        let snapshot =
            LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD")).unwrap();
        let shard = snapshot.shards()[0].clone();
        let selection = scoped_selection(Path::new("data"));

        let pulled = snapshot
            .with_shard_rows_pull(
                &shard,
                &selection,
                |pull| -> std::result::Result<Vec<String>, LockSnapshotError> {
                    let mut rows = Vec::new();
                    while let Some(entry) = pull()? {
                        rows.push(entry.path.to_string());
                    }
                    Ok(rows)
                },
            )
            .unwrap();
        assert_eq!(pulled, vec!["data/b.bin", "data/c.bin"]);
    }

    #[test]
    fn invalid_persisted_shard_reports_the_snapshot_label() {
        let tmp = test_repo();
        let root = tmp.path().to_path_buf();
        std::fs::write(root.join("gat.lock"), "not a lock file\n").unwrap();
        git(&root, &["add", "-A"]);
        commit_all(&root, "bad lock");
        let gix_repo = root;

        let err = LockSnapshot::at_rev(&layout(&gix_repo), &GitRevisionSpec::from("HEAD"))
            .unwrap()
            .to_lock()
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("gat.lock at `HEAD` is not a valid gat lock file"),
            "{err:#}"
        );
        let err = LockSnapshot::staged(&layout(&gix_repo))
            .unwrap()
            .to_lock()
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("gat.lock is not a valid gat lock file"),
            "{err:#}"
        );
    }
}
