//! Repository worktree path resolution, inspection, and physical mutation.

use crate::cache::{self, CacheError};
use crate::file_state::{
    IdentityCheck, StatProof, check_known_oid, observe_regular_file_no_follow,
};
use gat_core::config::{MaterializationMode, MaterializationStrategy};
use gat_core::lexical_path::GatPath;
use gat_core::lock::Entry;
use gat_core::oid::Oid;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum WorktreePathError {
    #[error("path `{path}` cannot be materialized on this host")]
    NotMaterializable { path: String },
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

    /// Preflight paths in input order, checking shared parents once. This is
    /// not a mutation receipt: each physical mutation still checks its ancestors.
    pub fn validate_mutations(&self, paths: &[GatPath]) -> PathResult<()> {
        let mut parents = HashSet::new();
        for path in paths {
            let dest = resolve_worktree_path(self.root, path)?;
            if let Some(parent) = dest.parent()
                && !parents.contains(parent)
            {
                ensure_no_symlink_ancestors(self.root, &dest)?;
                parents.insert(parent.to_path_buf());
            }
        }
        Ok(())
    }

    pub fn reject_infrastructure(path: &GatPath) -> PathResult<()> {
        reject_infrastructure_path(path.as_str())
    }

    pub fn inspect_destination(&self, path: &GatPath) -> PathResult<DestinationKind> {
        inspect_move_destination(self.root, path)
    }

    pub fn move_path(&self, src: &GatPath, dst: &GatPath) -> Result<PendingMove, MovePathError> {
        move_path(self.root, src, dst)
    }

    /// Remove files concurrently, then prune empty touched ancestors. All file
    /// removals finish before returning the first input-order error, if any;
    /// a failure skips pruning and may leave a partially removed worktree.
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

    /// Publish a privately prepared representation. Failures preserve the destination.
    pub fn materialize(
        &self,
        cache: &crate::CacheClient,
        entry: &Entry,
        strategy: &MaterializationStrategy,
    ) -> Result<crate::StateMutation, WorktreeMutationError> {
        let proof = materialize(
            self.root,
            cache.objects_dir(),
            &entry.path,
            &entry.oid,
            strategy,
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
    // GatPath has already excluded absolute paths, parent traversal and
    // noncanonical separators. Only Windows adds native drive-prefix syntax.
    if cfg!(windows)
        && Path::new(rel.as_str())
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(WorktreePathError::NotMaterializable {
            path: rel.to_string(),
        });
    }
    Ok(root.join(rel.as_str()))
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
    RestoreDestination(#[from] RestoreMoveDestinationError),
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

/// A completed rename whose replaced destination is retained until commit.
/// Dropping an unresolved receipt preserves the backup for manual recovery.
#[must_use = "commit the move after publication or roll it back"]
pub struct PendingMove {
    root: PathBuf,
    src: GatPath,
    dst: GatPath,
    backup: Option<PathBuf>,
}

impl PendingMove {
    /// Finalize publication. Backup cleanup is best effort; failure leaves an
    /// extra recovery copy and never invalidates the published move.
    pub fn commit(self) {
        if let Some(backup) = self.backup {
            let _ = std::fs::remove_file(&backup);
            if let Some(parent) = backup.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }

    pub fn rollback(self) -> Result<(), RollbackMoveError> {
        let full_src = confine_mutation(&self.root, &self.src)?;
        let full_dst = confine_mutation(&self.root, &self.dst)?;
        std::fs::rename(&full_dst, full_src).map_err(|source| RollbackMoveError::Rename {
            src: self.src.to_string(),
            dst: self.dst.to_string(),
            source,
        })?;
        if let Some(backup) = self.backup {
            restore_move_destination(backup, &full_dst)?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("restoring the move destination from {}", backup.display())]
pub struct RestoreMoveDestinationError {
    pub backup: PathBuf,
    #[source]
    pub source: std::io::Error,
}

fn restore_move_destination(
    backup: PathBuf,
    destination: &Path,
) -> Result<(), RestoreMoveDestinationError> {
    std::fs::rename(&backup, destination).map_err(|source| RestoreMoveDestinationError {
        backup: backup.clone(),
        source,
    })?;
    let _ = std::fs::remove_dir(backup.parent().expect("backup directory"));
    Ok(())
}

fn move_path(root: &Path, src: &GatPath, dst: &GatPath) -> Result<PendingMove, MovePathError> {
    let full_src = confine_mutation(root, src)?;
    let full_dst = confine_mutation(root, dst)?;
    let parent = full_dst.parent().expect("repository-relative destination");
    std::fs::create_dir_all(parent).map_err(|source| MovePathError::CreateParent {
        path: dst.to_string(),
        source,
    })?;
    let mut pending = PendingMove {
        root: root.to_path_buf(),
        src: src.clone(),
        dst: dst.clone(),
        backup: None,
    };
    if src == dst {
        std::fs::symlink_metadata(&full_src).map_err(|source| MovePathError::Rename {
            src: src.to_string(),
            dst: dst.to_string(),
            source,
        })?;
        return Ok(pending);
    }
    let backup = match std::fs::symlink_metadata(&full_dst) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(MovePathError::Rename {
                src: src.to_string(),
                dst: dst.to_string(),
                source: std::io::Error::from(std::io::ErrorKind::IsADirectory),
            });
        }
        // Case-only renames on case-insensitive filesystems address the same
        // directory entry. Moving that entry aside would also remove the source.
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && std::fs::symlink_metadata(&full_src)
                    .is_ok_and(|m| !m.file_type().is_symlink())
                && std::fs::canonicalize(&full_src)
                    .ok()
                    .zip(std::fs::canonicalize(&full_dst).ok())
                    .is_some_and(|(src, dst)| src == dst) =>
        {
            None
        }
        Ok(_) => Some(
            tempfile::Builder::new()
                .prefix(".gat-move-")
                .tempdir_in(parent)
                .map_err(|source| WorktreePathError::Io {
                    operation: "preparing move backup",
                    path: full_dst.clone(),
                    source,
                })?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(WorktreePathError::Io {
                operation: "inspecting move destination",
                path: full_dst,
                source,
            }
            .into());
        }
    };
    if let Some(directory) = backup {
        let backup = directory.path().join("destination");
        std::fs::rename(&full_dst, &backup).map_err(|source| WorktreePathError::Io {
            operation: "backing up move destination",
            path: full_dst.clone(),
            source,
        })?;
        // Persist before any subsequent fallible step: unwinding or rollback
        // failure must never delete the only remaining destination copy.
        let _ = directory.keep();
        pending.backup = Some(backup);
    }
    if let Err(source) = std::fs::rename(full_src, &full_dst) {
        if let Some(backup) = pending.backup.take() {
            restore_move_destination(backup, &full_dst)?;
        }
        return Err(MovePathError::Rename {
            src: src.to_string(),
            dst: dst.to_string(),
            source,
        });
    }
    Ok(pending)
}

#[derive(Debug, thiserror::Error)]
pub enum RollbackMoveError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error(transparent)]
    RestoreDestination(#[from] RestoreMoveDestinationError),
    #[error("moving {dst} back to {src}")]
    Rename {
        src: String,
        dst: String,
        #[source]
        source: std::io::Error,
    },
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
    use rayon::prelude::*;

    // Complete physical work before pruning; retain input-order error selection.
    let results: Vec<_> = paths
        .par_iter()
        .map(|path| {
            let full = confine_mutation(root, path)?;
            match std::fs::remove_file(&full) {
                Ok(()) => Ok(Some(full)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(RemovePathError::Delete {
                    path: path.to_string(),
                    source,
                }),
            }
        })
        .collect();
    let mut deleted = Vec::new();
    for result in results {
        deleted.extend(result?);
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
            if candidates.contains(parent) {
                break;
            }
            candidates.insert(parent.to_path_buf());
            current = parent;
        }
    }
    let mut ordered: Vec<_> = candidates.into_iter().collect();
    ordered.sort_by_cached_key(|dir| std::cmp::Reverse(dir.components().count()));

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
        } => Ok(WorktreeFileStatus::matches(Some(proof))),
        IdentityCheck::SizeMismatch | IdentityCheck::Hashed { matches: false, .. } => {
            Ok(WorktreeFileStatus::differs())
        }
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
    pub(crate) receipt: cache::CachePublication,
}

pub(crate) fn ingest_file(
    root: &Path,
    objects_dir: &Path,
    path: &GatPath,
    strategy: cache::IngestStrategy,
    large_file_threshold: u64,
) -> Result<WorktreeIngested, WorktreeMutationError> {
    let full = confine_read(root, path)?;
    let observation = crate::file_state::coherent_observation(&full, |before| {
        let size = before.size;
        if size > large_file_threshold {
            cache::object::ingest_file_delta(objects_dir, &full, strategy)
                .map_err(WorktreeMutationError::from)
        } else {
            let file = std::fs::File::open(&full).map_err(|source| CacheError::PathUnreadable {
                path: full.clone(),
                source,
            })?;
            cache::object::ingest_sized_delta(objects_dir, file, size)
                .map_err(WorktreeMutationError::from)
        }
    })?;
    let (ingested, receipt) = observation.value;
    Ok(WorktreeIngested {
        ingested,
        proof: observation.proof,
        receipt,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeMutationError {
    #[error(transparent)]
    Path(#[from] WorktreePathError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error(transparent)]
    FileState(#[from] crate::file_state::FileStateError),
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

/// Build privately beside the destination, then publish with one rename.
/// Failed strategies can clean up only staged files, never existing user data.
fn materialize(
    root: &Path,
    objects_dir: &Path,
    path: &GatPath,
    oid: &Oid,
    strategy: &MaterializationStrategy,
) -> Result<Option<StatProof>, WorktreeMutationError> {
    let dest = confine_mutation(root, path)?;
    let parent = dest.parent().expect("repository-relative file path");
    std::fs::create_dir_all(parent).map_err(|source| WorktreeMutationError::Io {
        operation: "creating parent directory for",
        path: path.to_string(),
        source,
    })?;
    let preparation_error = |source| WorktreeMutationError::Io {
        operation: "preparing materialization for",
        path: path.to_string(),
        source,
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".gat-materialize-");
    // Copy can fill a reserved file. Link strategies need an absent path, so
    // reserve their namespace with a private directory instead.
    let staging = if strategy.modes() == [MaterializationMode::Copy] {
        None
    } else {
        Some(builder.tempdir_in(parent).map_err(preparation_error)?)
    };
    let staged = match &staging {
        Some(directory) => tempfile::TempPath::try_from_path(directory.path().join("object"))
            .map_err(preparation_error)?,
        None => builder
            .tempfile_in(parent)
            .map_err(preparation_error)?
            .into_temp_path(),
    };
    let object = cache::object::cache_path_oid(objects_dir, oid);
    cache::object::materialize(&object, &staged, strategy)?;
    staged
        .persist(&dest)
        .map_err(|error| WorktreeMutationError::Io {
            operation: "publishing",
            path: path.to_string(),
            source: error.error,
        })?;
    // Publication leaves the private directory empty. Remove it directly rather
    // than recursively inspecting it; retain TempDir's cleanup if that fails.
    if let Some(staging) = staging
        && std::fs::remove_dir(staging.path()).is_ok()
    {
        let _ = staging.keep();
    }
    Ok(observe_regular_file_no_follow(&dest))
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

    fn move_fixture() -> (tempfile::TempDir, GatPath, GatPath) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("source"), b"source bytes").unwrap();
        std::fs::write(temp.path().join("destination"), b"destination bytes").unwrap();
        (
            temp,
            GatPath::parse_canonical("source").unwrap(),
            GatPath::parse_canonical("destination").unwrap(),
        )
    }

    #[test]
    fn move_receipt_restores_both_files_or_discards_backup_on_commit() {
        for commit in [false, true] {
            let (temp, src, dst) = move_fixture();
            let pending = move_path(temp.path(), &src, &dst).unwrap();
            let backup = pending.backup.clone().unwrap();
            assert_eq!(std::fs::read(&backup).unwrap(), b"destination bytes");
            assert_eq!(
                std::fs::read(temp.path().join("destination")).unwrap(),
                b"source bytes"
            );
            if commit {
                pending.commit();
                assert!(!temp.path().join("source").exists());
                assert_eq!(
                    std::fs::read(temp.path().join("destination")).unwrap(),
                    b"source bytes"
                );
            } else {
                pending.rollback().unwrap();
                assert_eq!(
                    std::fs::read(temp.path().join("source")).unwrap(),
                    b"source bytes"
                );
                assert_eq!(
                    std::fs::read(temp.path().join("destination")).unwrap(),
                    b"destination bytes"
                );
            }
            assert!(!backup.parent().unwrap().exists());
        }
    }

    #[test]
    #[cfg(windows)]
    fn case_only_move_does_not_back_up_its_own_source() {
        for commit in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            std::fs::write(temp.path().join("Original"), b"payload").unwrap();
            let src = GatPath::parse_canonical("Original").unwrap();
            let dst = GatPath::parse_canonical("original").unwrap();
            let pending = move_path(temp.path(), &src, &dst).unwrap();
            assert!(pending.backup.is_none());
            if commit {
                pending.commit();
            } else {
                pending.rollback().unwrap();
            }
            let entries = std::fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            assert_eq!(
                entries,
                [std::ffi::OsString::from(if commit {
                    "original"
                } else {
                    "Original"
                })]
            );
            assert_eq!(
                std::fs::read(
                    temp.path()
                        .join(if commit { "original" } else { "Original" })
                )
                .unwrap(),
                b"payload"
            );
        }
    }

    #[test]
    fn failed_move_restores_destination_before_returning() {
        let (temp, src, dst) = move_fixture();
        std::fs::remove_file(temp.path().join("source")).unwrap();
        assert!(matches!(
            move_path(temp.path(), &src, &dst),
            Err(MovePathError::Rename { .. })
        ));
        assert_eq!(
            std::fs::read(temp.path().join("destination")).unwrap(),
            b"destination bytes"
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_rollback_and_unresolved_receipt_preserve_backup() {
        for rollback in [false, true] {
            let (temp, src, dst) = move_fixture();
            let pending = move_path(temp.path(), &src, &dst).unwrap();
            let backup = pending.backup.clone().unwrap();
            if rollback {
                std::fs::create_dir(temp.path().join("source")).unwrap();
                assert!(matches!(
                    pending.rollback(),
                    Err(RollbackMoveError::Rename { .. })
                ));
            } else {
                drop(pending);
            }
            assert_eq!(std::fs::read(&backup).unwrap(), b"destination bytes");
            assert_eq!(
                std::fs::read(temp.path().join("destination")).unwrap(),
                b"source bytes"
            );
        }
    }

    #[test]
    fn failed_destination_restore_reports_the_retained_backup() {
        let temp = tempfile::tempdir().unwrap();
        let backup = temp.path().join("backup");
        let destination = temp.path().join("blocked");
        std::fs::write(&backup, b"original destination").unwrap();
        std::fs::create_dir(&destination).unwrap();
        let error = restore_move_destination(backup.clone(), &destination).unwrap_err();
        assert_eq!(error.backup, backup);
        assert_eq!(std::fs::read(&backup).unwrap(), b"original destination");
    }

    #[test]
    #[cfg(unix)]
    fn move_rollback_restores_dangling_destination_symlink_without_following_it() {
        let (temp, src, dst) = move_fixture();
        std::fs::remove_file(temp.path().join("destination")).unwrap();
        std::os::unix::fs::symlink("missing-target", temp.path().join("destination")).unwrap();
        move_path(temp.path(), &src, &dst)
            .unwrap()
            .rollback()
            .unwrap();
        assert_eq!(
            std::fs::read_link(temp.path().join("destination")).unwrap(),
            PathBuf::from("missing-target")
        );
        assert!(!temp.path().join("missing-target").exists());
        assert_eq!(
            std::fs::read(temp.path().join("source")).unwrap(),
            b"source bytes"
        );
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
        )
        .unwrap();

        assert_eq!(
            std::fs::read(root.path().join("nested/file.bin")).unwrap(),
            b"payload"
        );
        assert!(proof.is_some());
    }

    #[test]
    fn materialization_preserves_user_backup_names_on_success_and_failure() {
        for modes in [
            vec![MaterializationMode::Copy],
            vec![MaterializationMode::Hardlink],
            vec![MaterializationMode::Reflink, MaterializationMode::Copy],
            vec![MaterializationMode::Copy, MaterializationMode::Hardlink],
        ] {
            let strategy = MaterializationStrategy::try_from(modes).unwrap();
            for available in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let cache = tempfile::tempdir().unwrap();
                let destination = root.path().join("file.bin");
                let sentinel = root.path().join("file.bin.gat-tmp");
                std::fs::write(&destination, b"local edit").unwrap();
                std::fs::write(&sentinel, b"unrelated user file").unwrap();
                let oid = if available {
                    cached_object(cache.path(), b"new content")
                } else {
                    Oid::from_bytes(*blake3::hash(b"missing").as_bytes())
                };
                let result = materialize(
                    root.path(),
                    cache.path(),
                    &path("file.bin"),
                    &oid,
                    &strategy,
                );
                if available {
                    assert!(result.unwrap().is_some());
                    assert_eq!(std::fs::read(&destination).unwrap(), b"new content");
                } else {
                    assert!(matches!(result, Err(WorktreeMutationError::Cache(_))));
                    assert_eq!(std::fs::read(&destination).unwrap(), b"local edit");
                }
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"unrelated user file");
                assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn materialization_preserves_destination_symlinks_on_failure_and_never_follows_them() {
        for dangling in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let target = root.path().join("target");
            if !dangling {
                std::fs::write(&target, b"outside content").unwrap();
            }
            let destination = root.path().join("file.bin");
            std::os::unix::fs::symlink(&target, &destination).unwrap();
            let missing = Oid::from_bytes([0; 32]);
            assert!(
                materialize(
                    root.path(),
                    cache.path(),
                    &path("file.bin"),
                    &missing,
                    &"copy".parse().unwrap()
                )
                .is_err()
            );
            assert_eq!(std::fs::read_link(&destination).unwrap(), target);
            let oid = cached_object(cache.path(), b"new content");
            materialize(
                root.path(),
                cache.path(),
                &path("file.bin"),
                &oid,
                &"copy".parse().unwrap(),
            )
            .unwrap();
            assert!(
                !std::fs::symlink_metadata(&destination)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"new content");
            if dangling {
                assert!(!target.exists());
            } else {
                assert_eq!(std::fs::read(&target).unwrap(), b"outside content");
            }
            assert_eq!(
                std::fs::read_dir(root.path()).unwrap().count(),
                if dangling { 1 } else { 2 }
            );
        }
    }

    #[test]
    fn failed_publication_preserves_directories_and_cleans_staged_content() {
        for populated in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let destination = root.path().join("file.bin");
            std::fs::create_dir(&destination).unwrap();
            if populated {
                std::fs::write(destination.join("child"), b"user data").unwrap();
            }
            let oid = cached_object(cache.path(), b"new content");
            assert!(matches!(
                materialize(
                    root.path(),
                    cache.path(),
                    &path("file.bin"),
                    &oid,
                    &"copy".parse().unwrap()
                ),
                Err(WorktreeMutationError::Io { .. })
            ));
            assert!(destination.is_dir());
            if populated {
                assert_eq!(
                    std::fs::read(destination.join("child")).unwrap(),
                    b"user data"
                );
            }
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn status_debug_output_does_not_expose_the_private_stat_proof() {
        let status = WorktreeFileStatus::matches(None);
        let output = format!("{status:?}");

        assert!(output.contains("Matches"));
        assert!(!output.contains("proof"));
    }
}
