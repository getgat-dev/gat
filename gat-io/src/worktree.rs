//! Repository worktree path resolution, inspection, and physical mutation.

use crate::cache::{self, CacheError};
use crate::file_state::{
    IdentityCheck, StatProof, check_known_oid, observe_regular_file_no_follow,
};
use gat_core::config::MaterializationStrategy;
use gat_core::lexical_path::GatPath;
use gat_core::lock::Entry;
use gat_core::oid::Oid;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum WorktreePathError {
    #[error("path `{path}` must be relative")]
    NotRelative { path: String },
    #[error("path `{path}` escapes the worktree (contains `..`)")]
    ParentTraversal { path: String },
    #[error("path `{path}` cannot be materialized on this host")]
    NotMaterializable { path: String },
    #[error("path `{path}` contains non-UTF-8 characters")]
    NonUtf8Component { path: String },
    #[error("path `{path}` escapes the worktree after joining with the repository root")]
    EscapesWorktree { path: String },
    #[error(
        "path `{path}` traverses symlinked ancestor `{ancestor}`; refusing to {verb} outside the repository"
    )]
    SymlinkAncestor {
        path: String,
        ancestor: String,
        verb: &'static str,
    },
    #[error("`{path}` is gat/git infrastructure and can never be added")]
    ForbiddenInfrastructurePath { path: String },
    #[error("`{path}` is a symlink; gat does not track symlinks")]
    UnsupportedLeafSymlink { path: String },
    #[error("{operation} `{}` failed", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("internal error: {detail}")]
    Internal { detail: String },
}

type PathResult<T> = std::result::Result<T, WorktreePathError>;

/// Repository-bound access to working-tree inspection and mutation.
///
/// The capability borrows the repository root, so creating it performs no
/// allocation and physical paths never leave `gat-io`.
#[derive(Clone, Copy)]
pub struct WorktreeClient<'root> {
    root: &'root Path,
}

impl<'root> WorktreeClient<'root> {
    pub(crate) const fn new(root: &'root Path) -> Self {
        Self { root }
    }

    pub fn inspect(&self, path: &GatPath) -> PathResult<EntryKind> {
        inspect_read_path(self.root, path).map(|(_, kind)| kind)
    }

    pub fn validate_mutation(&self, path: &GatPath) -> PathResult<()> {
        confine_mutation(self.root, path).map(|_| ())
    }

    pub fn reject_infrastructure(path: &GatPath) -> PathResult<()> {
        reject_infrastructure_path(path.as_str())
    }

    pub fn inspect_destination(&self, path: &GatPath) -> PathResult<DestinationKind> {
        inspect_move_destination(self.root, path)
    }

    pub fn move_path(&self, src: &GatPath, dst: &GatPath) -> Result<(), MovePathError> {
        move_path(self.root, src, dst)
    }

    pub fn rollback_move(&self, src: &GatPath, dst: &GatPath) -> Result<(), RollbackMoveError> {
        rollback_move(self.root, src, dst)
    }

    pub fn remove_and_prune(&self, paths: &[GatPath]) -> Result<(), RemovePathError> {
        remove_and_prune(self.root, paths)
    }

    pub fn file_status(
        &self,
        cache: &crate::CacheClient,
        prior: &crate::MaterializedRow,
        trust_state: bool,
    ) -> Result<WorktreeFileStatus, WorktreeMutationError> {
        file_status(
            self.root,
            cache.objects_dir(),
            prior.path(),
            &prior.oid(),
            prior.proof(),
            trust_state,
        )
    }

    pub fn file_status_without_prior(
        &self,
        cache: &crate::CacheClient,
        path: &GatPath,
        expected: &Oid,
        trust_state: bool,
    ) -> Result<WorktreeFileStatus, WorktreeMutationError> {
        file_status(
            self.root,
            cache.objects_dir(),
            path,
            expected,
            None,
            trust_state,
        )
    }

    pub fn materialize(
        &self,
        cache: &crate::CacheClient,
        entry: &Entry,
        strategy: &MaterializationStrategy,
        kind: MaterializeKind,
    ) -> Result<crate::StateMutation, WorktreeMutationError> {
        let proof = materialize(
            self.root,
            cache.objects_dir(),
            &entry.path,
            &entry.oid,
            strategy,
            kind,
        )?;
        Ok(crate::StateMutation::upsert(
            crate::MaterializedRow::from_entry(entry.clone(), proof),
        ))
    }

    pub fn remove(&self, path: &GatPath) -> Result<Option<RemovalReceipt>, WorktreeMutationError> {
        remove(self.root, path).map(|removed| removed.map(|path| RemovalReceipt { path }))
    }

    pub fn prune(&self, removed: &[RemovalReceipt]) -> Result<(), PruneError> {
        prune_touched_ancestor_dirs(
            self.root,
            removed.iter().map(|receipt| receipt.path.as_path()),
        )
    }
}

/// Evidence that one worktree path was actually removed.
///
/// Its physical path is intentionally private and can only be consumed by
/// [`WorktreeClient::prune`].
pub struct RemovalReceipt {
    path: PathBuf,
}

fn resolve_worktree_path(root: &Path, rel: &GatPath) -> PathResult<PathBuf> {
    use std::path::Component;

    for component in Path::new(rel.as_str()).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(WorktreePathError::ParentTraversal {
                    path: rel.to_string(),
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(WorktreePathError::NotMaterializable {
                    path: rel.to_string(),
                });
            }
        }
    }

    let dest = root.join(rel.as_str());
    if !dest.starts_with(root) {
        return Err(WorktreePathError::EscapesWorktree {
            path: rel.to_string(),
        });
    }
    Ok(dest)
}

fn ensure_no_symlink_ancestors(root: &Path, dest: &Path) -> PathResult<()> {
    ancestor_symlink_check(root, dest, "mutate")
}

fn ancestor_symlink_check(root: &Path, dest: &Path, verb: &'static str) -> PathResult<()> {
    let rel = dest
        .strip_prefix(root)
        .map_err(|_| WorktreePathError::Internal {
            detail: format!("{} is not under {}", dest.display(), root.display()),
        })?;
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        cur.push(comp.as_os_str());
        let Some(parent) = cur.parent() else {
            continue;
        };
        if !parent.starts_with(root) {
            continue;
        }
        match std::fs::symlink_metadata(parent) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(WorktreePathError::SymlinkAncestor {
                    path: rel.display().to_string(),
                    ancestor: parent.display().to_string(),
                    verb,
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(WorktreePathError::Io {
                    operation: "reading",
                    path: parent.to_path_buf(),
                    source,
                });
            }
        }
    }
    Ok(())
}

fn confine_mutation(root: &Path, path: &GatPath) -> PathResult<PathBuf> {
    let dest = resolve_worktree_path(root, path)?;
    ensure_no_symlink_ancestors(root, &dest)?;
    Ok(dest)
}

fn confine_read(root: &Path, path: &GatPath) -> PathResult<PathBuf> {
    let dest = resolve_worktree_path(root, path)?;
    ancestor_symlink_check(root, &dest, "read")?;
    Ok(dest)
}

fn remove_leaf_symlink_if_present(dest: &Path) -> PathResult<()> {
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(dest).map_err(|source| WorktreePathError::Io {
                operation: "removing",
                path: dest.to_path_buf(),
                source,
            })?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(WorktreePathError::Io {
                operation: "reading",
                path: dest.to_path_buf(),
                source,
            });
        }
    }
    Ok(())
}

pub(crate) fn is_infrastructure_path(rel: &str) -> bool {
    matches!(
        rel.split('/').next(),
        Some(".git" | ".gat" | "gat.lock" | "gat.yaml")
    )
}

fn reject_infrastructure_path(rel: &str) -> PathResult<()> {
    if is_infrastructure_path(rel) {
        return Err(WorktreePathError::ForbiddenInfrastructurePath {
            path: rel.to_string(),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Missing,
    File,
    Directory,
    Symlink,
    Other,
}

pub(crate) fn inspect_read_path(root: &Path, path: &GatPath) -> PathResult<(PathBuf, EntryKind)> {
    match inspect_literal_read_path(root, path) {
        // Literal names take precedence over glob selection. Windows cannot
        // contain * or ?, and reports ERROR_INVALID_NAME rather than NotFound.
        // Preserve other errors, including confinement and symlink failures.
        #[cfg(windows)]
        Err(WorktreePathError::Io { source, .. })
            if source.raw_os_error() == Some(123) && path.as_str().contains(['*', '?']) =>
        {
            Ok((resolve_worktree_path(root, path)?, EntryKind::Missing))
        }
        result => result,
    }
}

fn inspect_literal_read_path(root: &Path, path: &GatPath) -> PathResult<(PathBuf, EntryKind)> {
    let full = confine_read(root, path)?;
    let kind = match std::fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.file_type().is_symlink() => EntryKind::Symlink,
        Ok(metadata) if metadata.is_file() => EntryKind::File,
        Ok(metadata) if metadata.is_dir() => EntryKind::Directory,
        Ok(_) => EntryKind::Other,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EntryKind::Missing,
        Err(source) => {
            return Err(WorktreePathError::Io {
                operation: "reading",
                path: full,
                source,
            });
        }
    };
    Ok((full, kind))
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn observe_regular_file(root: &Path, path: &GatPath) -> Option<StatProof> {
    let full = resolve_worktree_path(root, path).ok()?;
    observe_regular_file_no_follow(&full)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationKind {
    Missing,
    Directory,
    Other,
}

fn inspect_move_destination(root: &Path, path: &GatPath) -> PathResult<DestinationKind> {
    let full = confine_mutation(root, path)?;
    match std::fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.is_dir() => Ok(DestinationKind::Directory),
        Ok(_) => Ok(DestinationKind::Other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DestinationKind::Missing),
        Err(source) => Err(WorktreePathError::Io {
            operation: "checking",
            path: full,
            source,
        }),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MovePathError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error("creating parent directory for {path}")]
    CreateParent {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("moving {src} to {dst}")]
    Rename {
        src: String,
        dst: String,
        #[source]
        source: std::io::Error,
    },
}

fn move_path(root: &Path, src: &GatPath, dst: &GatPath) -> Result<(), MovePathError> {
    let full_src = confine_mutation(root, src)?;
    let full_dst = confine_mutation(root, dst)?;
    if let Some(parent) = full_dst.parent() {
        std::fs::create_dir_all(parent).map_err(|source| MovePathError::CreateParent {
            path: dst.to_string(),
            source,
        })?;
    }
    std::fs::rename(full_src, full_dst).map_err(|source| MovePathError::Rename {
        src: src.to_string(),
        dst: dst.to_string(),
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum RollbackMoveError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error("moving {dst} back to {src}")]
    Rename {
        src: String,
        dst: String,
        #[source]
        source: std::io::Error,
    },
}

fn rollback_move(root: &Path, src: &GatPath, dst: &GatPath) -> Result<(), RollbackMoveError> {
    let full_src = resolve_worktree_path(root, src)?;
    let full_dst = resolve_worktree_path(root, dst)?;
    std::fs::rename(full_dst, full_src).map_err(|source| RollbackMoveError::Rename {
        src: src.to_string(),
        dst: dst.to_string(),
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum RemovePathError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error("could not remove `{path}` from the working tree")]
    Delete {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Prune(#[from] PruneError),
}

fn remove_and_prune(root: &Path, paths: &[GatPath]) -> Result<(), RemovePathError> {
    let mut deleted = Vec::new();
    for path in paths {
        let full = confine_mutation(root, path)?;
        match std::fs::remove_file(&full) {
            Ok(()) => deleted.push(full),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(RemovePathError::Delete {
                    path: path.to_string(),
                    source,
                });
            }
        }
    }
    prune_touched_ancestor_dirs(root, deleted.iter().map(PathBuf::as_path))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("could not remove directory `{path}`")]
pub struct PruneError {
    pub path: PathBuf,
    #[source]
    pub source: std::io::Error,
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        static REMOVE_DIR_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn record_remove_dir_attempt() {
        REMOVE_DIR_ATTEMPTS.with(|counter| counter.set(counter.get() + 1));
    }

    pub fn remove_dir_attempts() -> usize {
        REMOVE_DIR_ATTEMPTS.with(Cell::get)
    }

    pub fn reset_remove_dir_attempts() {
        REMOVE_DIR_ATTEMPTS.with(|counter| counter.set(0));
    }
}

fn prune_touched_ancestor_dirs<'path>(
    root: &Path,
    deleted: impl IntoIterator<Item = &'path Path>,
) -> Result<(), PruneError> {
    let mut candidates = HashSet::new();
    for path in deleted {
        let mut current = path;
        while let Some(parent) = current.parent() {
            if parent == root || !parent.starts_with(root) {
                break;
            }
            if !candidates.insert(parent.to_path_buf()) {
                break;
            }
            current = parent;
        }
    }
    let mut ordered: Vec<_> = candidates.into_iter().collect();
    ordered.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));

    let mut blocked = HashSet::new();
    for dir in &ordered {
        if blocked.contains(dir) {
            block_parent(root, dir, &mut blocked);
            continue;
        }
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_remove_dir_attempt();
        match std::fs::remove_dir(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotADirectory
                ) =>
            {
                block_parent(root, dir, &mut blocked);
            }
            Err(source) => {
                return Err(PruneError {
                    path: dir.clone(),
                    source,
                });
            }
        }
    }
    Ok(())
}

fn block_parent(root: &Path, dir: &Path, blocked: &mut HashSet<PathBuf>) {
    if let Some(parent) = dir.parent()
        && parent != root
        && parent.starts_with(root)
    {
        blocked.insert(parent.to_path_buf());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeStatusKind {
    Absent,
    Matches,
    Differs,
}

pub struct WorktreeFileStatus {
    kind: WorktreeStatusKind,
    proof_refresh: Option<StatProof>,
}

impl std::fmt::Debug for WorktreeFileStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorktreeFileStatus")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl WorktreeFileStatus {
    #[must_use]
    pub const fn kind(&self) -> WorktreeStatusKind {
        self.kind
    }

    #[must_use]
    pub fn into_state_refresh(self, path: GatPath) -> Option<crate::StateMutation> {
        self.proof_refresh
            .map(|proof| crate::StateMutation::refresh_stat(path, proof))
    }

    const fn absent() -> Self {
        Self {
            kind: WorktreeStatusKind::Absent,
            proof_refresh: None,
        }
    }

    const fn matches(proof_refresh: Option<StatProof>) -> Self {
        Self {
            kind: WorktreeStatusKind::Matches,
            proof_refresh,
        }
    }

    const fn differs() -> Self {
        Self {
            kind: WorktreeStatusKind::Differs,
            proof_refresh: None,
        }
    }
}

fn file_status(
    root: &Path,
    objects_dir: &Path,
    path: &GatPath,
    expected: &Oid,
    prior_proof: Option<&StatProof>,
    trust_state: bool,
) -> Result<WorktreeFileStatus, WorktreeMutationError> {
    if trust_state {
        return Ok(WorktreeFileStatus::matches(None));
    }

    let full = resolve_worktree_path(root, path)?;
    let metadata = match std::fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorktreeFileStatus::absent());
        }
        Err(source) => {
            return Err(WorktreeMutationError::Io {
                operation: "reading",
                path: path.to_string(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&full).map_err(|source| WorktreeMutationError::Io {
            operation: "reading symlink",
            path: path.to_string(),
            source,
        })?;
        return Ok(
            if target == cache::object::cache_path_oid(objects_dir, expected) {
                WorktreeFileStatus::matches(None)
            } else {
                WorktreeFileStatus::differs()
            },
        );
    }
    if !metadata.is_file() {
        return Ok(WorktreeFileStatus::differs());
    }
    match check_known_oid::<WorktreeMutationError>(&full, *expected, prior_proof, |candidate| {
        cache::object::hash_file_oid(candidate).map_err(WorktreeMutationError::from)
    })? {
        IdentityCheck::Proven => Ok(WorktreeFileStatus::matches(None)),
        IdentityCheck::Hashed {
            matches: true,
            proof,
            ..
        } => Ok(WorktreeFileStatus::matches(proof)),
        IdentityCheck::Hashed { matches: false, .. } => Ok(WorktreeFileStatus::differs()),
    }
}

pub(crate) fn check_regular_file(
    root: &Path,
    path: &GatPath,
    expected: &Oid,
    prior_proof: Option<&StatProof>,
) -> Result<Option<IdentityCheck>, WorktreeMutationError> {
    let full = confine_read(root, path)?;
    let metadata = match std::fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(WorktreeMutationError::Io {
                operation: "reading",
                path: path.to_string(),
                source,
            });
        }
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    Ok(Some(check_known_oid::<WorktreeMutationError>(
        &full,
        *expected,
        prior_proof,
        |candidate| cache::object::hash_file_oid(candidate).map_err(WorktreeMutationError::from),
    )?))
}

pub(crate) struct WorktreeIngested {
    pub(crate) ingested: cache::Ingested,
    pub(crate) proof: StatProof,
    pub(crate) receipt: Option<cache::CachePublication>,
}

pub(crate) fn ingest_file(
    root: &Path,
    objects_dir: &Path,
    path: &GatPath,
    strategy: cache::IngestStrategy,
    large_file_threshold: u64,
    on_progress: impl Fn(u64, u64) + Sync,
) -> Result<WorktreeIngested, WorktreeMutationError> {
    let full = confine_read(root, path)?;
    let observation = crate::file_state::coherent_observation(&full, || {
        let size = std::fs::metadata(&full)
            .map(|metadata| metadata.len())
            .map_err(|source| CacheError::PathUnreadable {
                path: full.clone(),
                source,
            })
            .map_err(WorktreeMutationError::from)?;
        if size > large_file_threshold {
            cache::object::ingest_file_delta(objects_dir, &full, strategy, |read| {
                on_progress(read, size);
            })
            .map_err(WorktreeMutationError::from)
        } else {
            let file = std::fs::File::open(&full).map_err(|source| CacheError::PathUnreadable {
                path: full.clone(),
                source,
            })?;
            cache::object::ingest_delta(objects_dir, file).map_err(WorktreeMutationError::from)
        }
    })?;
    let (ingested, receipt) = observation.value;
    Ok(WorktreeIngested {
        ingested,
        proof: observation.proof,
        receipt,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterializeKind {
    Create,
    Replace,
    Rematerialize,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeMutationError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error(transparent)]
    FileState(#[from] crate::file_state::FileStateError),
    #[error("cannot rematerialize `{path}`: no file name")]
    NoFileName { path: String },
    #[error("could not {operation} `{path}`")]
    Io {
        operation: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Prune(#[from] PruneError),
}

fn materialize(
    root: &Path,
    objects_dir: &Path,
    path: &GatPath,
    oid: &Oid,
    strategy: &MaterializationStrategy,
    kind: MaterializeKind,
) -> Result<Option<StatProof>, WorktreeMutationError> {
    match kind {
        MaterializeKind::Create => {
            let dest = confine_mutation(root, path)?;
            materialize_create(objects_dir, path, oid, strategy, &dest)?;
        }
        MaterializeKind::Replace => {
            let dest = confine_mutation(root, path)?;
            if std::fs::symlink_metadata(&dest).is_ok_and(|metadata| metadata.is_dir()) {
                std::fs::remove_file(&dest).map_err(|source| WorktreeMutationError::Io {
                    operation: "removing",
                    path: path.to_string(),
                    source,
                })?;
            }
            materialize_create(objects_dir, path, oid, strategy, &dest)?;
        }
        MaterializeKind::Rematerialize => {
            let object = cache::object::cache_path_oid(objects_dir, oid);
            let dest = confine_mutation(root, path)?;
            let parent = dest.parent().unwrap_or_else(|| Path::new("."));
            let tmp_dir = tempfile::Builder::new()
                .prefix(".gat-rematerialize-")
                .tempdir_in(parent)
                .map_err(|source| WorktreeMutationError::Io {
                    operation: "creating a temp directory in",
                    path: path.to_string(),
                    source,
                })?;
            let file_name = dest
                .file_name()
                .ok_or_else(|| WorktreeMutationError::NoFileName {
                    path: path.to_string(),
                })?;
            let tmp = tmp_dir.path().join(file_name);
            cache::object::materialize(&object, &tmp, strategy)?;
            std::fs::rename(tmp, &dest).map_err(|source| WorktreeMutationError::Io {
                operation: "swapping in",
                path: path.to_string(),
                source,
            })?;
        }
    }
    Ok(resolve_worktree_path(root, path)
        .ok()
        .and_then(|full| observe_regular_file_no_follow(&full)))
}

fn materialize_create(
    objects_dir: &Path,
    path: &GatPath,
    oid: &Oid,
    strategy: &MaterializationStrategy,
    dest: &Path,
) -> Result<(), WorktreeMutationError> {
    let object = cache::object::cache_path_oid(objects_dir, oid);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|source| WorktreeMutationError::Io {
            operation: "creating",
            path: path.to_string(),
            source,
        })?;
    }
    remove_leaf_symlink_if_present(dest)?;
    let backup = if dest.exists() {
        let backup = dest.with_extension(dest.extension().map_or_else(
            || std::ffi::OsString::from("gat-tmp"),
            |extension| {
                let mut extension = extension.to_os_string();
                extension.push(".gat-tmp");
                extension
            },
        ));
        std::fs::rename(dest, &backup).map_err(|source| WorktreeMutationError::Io {
            operation: "backing up",
            path: path.to_string(),
            source,
        })?;
        Some(backup)
    } else {
        None
    };
    match cache::object::materialize(&object, dest, strategy) {
        Ok(()) => {
            if let Some(backup) = backup {
                let _ = std::fs::remove_file(backup);
            }
            Ok(())
        }
        Err(error) => {
            if let Some(backup) = backup {
                let _ = std::fs::rename(backup, dest);
            }
            Err(error.into())
        }
    }
}

fn remove(root: &Path, path: &GatPath) -> Result<Option<PathBuf>, WorktreeMutationError> {
    let dest = confine_mutation(root, path)?;
    match std::fs::remove_file(&dest) {
        Ok(()) => Ok(Some(dest)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(WorktreeMutationError::Io {
            operation: "removing",
            path: path.to_string(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> GatPath {
        GatPath::parse_canonical(value).unwrap()
    }

    fn cached_object(objects_dir: &Path, bytes: &[u8]) -> Oid {
        let oid = Oid::from_bytes(*blake3::hash(bytes).as_bytes());
        let object = cache::object::cache_path_oid(objects_dir, &oid);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(object, bytes).unwrap();
        oid
    }

    #[test]
    fn resolves_normal_relative_path() {
        assert_eq!(
            resolve_worktree_path(Path::new("/repo"), &path("data/file.bin")).unwrap(),
            PathBuf::from("/repo/data/file.bin")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_read_ancestor() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("outside")).unwrap();

        assert!(matches!(
            confine_read(root.path(), &path("outside/file")),
            Err(WorktreePathError::SymlinkAncestor { verb: "read", .. })
        ));
    }

    #[test]
    fn infrastructure_check_only_matches_top_level_components() {
        assert!(is_infrastructure_path(".git/info/exclude"));
        assert!(is_infrastructure_path(".gat/state/state.sqlite3"));
        assert!(!is_infrastructure_path("vendor/.git/config"));
    }

    #[test]
    fn capability_classifies_paths_and_consumes_removal_receipts() {
        let root = tempfile::tempdir().unwrap();
        let client = WorktreeClient::new(root.path());
        std::fs::create_dir_all(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/file.bin"), b"payload").unwrap();

        assert_eq!(
            client.inspect(&path("nested/file.bin")).unwrap(),
            EntryKind::File
        );
        assert_eq!(
            client.inspect(&path("missing.bin")).unwrap(),
            EntryKind::Missing
        );

        let removed = client.remove(&path("nested/file.bin")).unwrap().unwrap();
        client.prune(&[removed]).unwrap();
        assert!(!root.path().join("nested").exists());
    }

    #[test]
    fn materialize_create_publishes_content_and_returns_a_stat_proof() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let oid = cached_object(cache.path(), b"payload");

        let proof = materialize(
            root.path(),
            cache.path(),
            &path("nested/file.bin"),
            &oid,
            &"copy".parse().unwrap(),
            MaterializeKind::Create,
        )
        .unwrap();

        assert_eq!(
            std::fs::read(root.path().join("nested/file.bin")).unwrap(),
            b"payload"
        );
        assert!(proof.is_some());
    }

    #[test]
    fn failed_replace_restores_the_existing_file_and_cleans_its_backup() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let destination = root.path().join("file.bin");
        std::fs::write(&destination, b"local edit").unwrap();
        let missing_oid = Oid::from_bytes(*blake3::hash(b"missing").as_bytes());

        let error = materialize(
            root.path(),
            cache.path(),
            &path("file.bin"),
            &missing_oid,
            &"copy".parse().unwrap(),
            MaterializeKind::Replace,
        )
        .unwrap_err();

        assert!(matches!(error, WorktreeMutationError::Cache(_)));
        assert_eq!(std::fs::read(destination).unwrap(), b"local edit");
        assert!(!root.path().join("file.bin.gat-tmp").exists());
    }

    #[test]
    fn replace_publishes_new_content_and_cleans_the_previous_file() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let destination = root.path().join("file.bin");
        std::fs::write(&destination, b"old").unwrap();
        let oid = cached_object(cache.path(), b"new");

        materialize(
            root.path(),
            cache.path(),
            &path("file.bin"),
            &oid,
            &"copy".parse().unwrap(),
            MaterializeKind::Replace,
        )
        .unwrap();

        assert_eq!(std::fs::read(destination).unwrap(), b"new");
        assert!(!root.path().join("file.bin.gat-tmp").exists());
    }

    #[test]
    fn failed_rematerialize_preserves_the_existing_file_and_cleans_temp_state() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let destination = root.path().join("file.bin");
        std::fs::write(&destination, b"local edit").unwrap();
        let missing_oid = Oid::from_bytes(*blake3::hash(b"missing").as_bytes());

        let error = materialize(
            root.path(),
            cache.path(),
            &path("file.bin"),
            &missing_oid,
            &"copy".parse().unwrap(),
            MaterializeKind::Rematerialize,
        )
        .unwrap_err();

        assert!(matches!(error, WorktreeMutationError::Cache(_)));
        assert_eq!(std::fs::read(destination).unwrap(), b"local edit");
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".gat-rematerialize-")
        }));
    }

    #[test]
    fn status_debug_output_does_not_expose_the_private_stat_proof() {
        let status = WorktreeFileStatus::matches(None);
        let output = format!("{status:?}");

        assert!(output.contains("Matches"));
        assert!(!output.contains("proof"));
    }
}
