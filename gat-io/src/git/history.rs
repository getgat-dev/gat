//! Semantic Git-history traversal and historical `gat.lock` reads.
//!
//! Repository, ref, walk, commit, tree, blob, and object-ID values remain
//! private. Callers provide core-owned selection values and receive only
//! semantic commit IDs, lock entries, and traversal statistics.

use std::collections::{BTreeSet, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use gat_core::git::{GitCommitId, GitRevisionSpec, GitTimestamp};
use gat_core::history::{HistoryRoot, HistorySelection, HistoryTraversal, ParentMode, TimeWindow};
use gat_core::lock::Entry;

use super::BoxedSource;

/// Semantic stage at which Git history access failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitHistoryErrorKind {
    OpenRepository,
    InvalidDate,
    RevisionResolution,
    Traversal,
    InvalidLockSnapshot,
    UnsupportedHashKind,
}

/// Failure to parse or traverse Git history without exposing Gix errors.
#[derive(Debug)]
pub struct GitHistoryError {
    kind: GitHistoryErrorKind,
    root: Option<PathBuf>,
    subject: String,
    detail: Option<String>,
    source: Option<BoxedSource>,
}

impl GitHistoryError {
    /// The semantic stage that failed.
    #[must_use]
    pub const fn kind(&self) -> GitHistoryErrorKind {
        self.kind
    }

    /// Repository root involved in the failure, when applicable.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Revision, date input, operation, or snapshot label involved.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Safe structural detail for an invalid historical lock snapshot.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl std::fmt::Display for GitHistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            GitHistoryErrorKind::OpenRepository => write!(
                f,
                "could not open the git repository at `{}`",
                self.root
                    .as_deref()
                    .expect("open errors carry a root")
                    .display()
            ),
            GitHistoryErrorKind::InvalidDate => write!(f, "invalid date `{}`", self.subject),
            GitHistoryErrorKind::RevisionResolution => {
                write!(f, "could not resolve revision `{}`", self.subject)
            }
            GitHistoryErrorKind::Traversal => write!(f, "{} failed", self.subject),
            GitHistoryErrorKind::InvalidLockSnapshot => write!(
                f,
                "{} is not a valid gat lock file: {}",
                self.subject,
                self.detail.as_deref().unwrap_or("invalid snapshot")
            ),
            GitHistoryErrorKind::UnsupportedHashKind => {
                f.write_str("a selected commit uses an unsupported hash kind")
            }
        }
    }
}

impl std::error::Error for GitHistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

impl From<super::GitOpenError> for GitHistoryError {
    fn from(source: super::GitOpenError) -> Self {
        Self {
            kind: GitHistoryErrorKind::OpenRepository,
            root: Some(source.path().to_path_buf()),
            subject: "opening repository".to_string(),
            detail: None,
            source: Some(Box::new(source)),
        }
    }
}

fn with_source(
    kind: GitHistoryErrorKind,
    root: Option<&Path>,
    subject: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> GitHistoryError {
    GitHistoryError {
        kind,
        root: root.map(Path::to_path_buf),
        subject: subject.into(),
        detail: None,
        source: Some(Box::new(source)),
    }
}

fn traversal(
    operation: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> GitHistoryError {
    with_source(GitHistoryErrorKind::Traversal, None, operation, source)
}

fn traversal_boxed(operation: impl Into<String>, source: BoxedSource) -> GitHistoryError {
    GitHistoryError {
        kind: GitHistoryErrorKind::Traversal,
        root: None,
        subject: operation.into(),
        detail: None,
        source: Some(source),
    }
}

fn invalid_snapshot(
    label: impl Into<String>,
    detail: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> GitHistoryError {
    GitHistoryError {
        kind: GitHistoryErrorKind::InvalidLockSnapshot,
        root: None,
        subject: label.into(),
        detail: Some(detail.into()),
        source: Some(Box::new(source)),
    }
}

/// Outcome of visiting a resolved history selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryStats {
    /// Number of distinct commits visited after deduplication across roots.
    pub visited: usize,
    /// Whether history may be incomplete because the repository is shallow.
    pub shallow: bool,
}

struct ResolvedHistory<'repo> {
    repo: &'repo gix::Repository,
    roots: Vec<gix::ObjectId>,
    hidden: Vec<gix::ObjectId>,
    traversal: HistoryTraversal,
    time: TimeWindow,
    parents: ParentMode,
    shallow: bool,
}

impl ResolvedHistory<'_> {
    fn visit_object_ids<E>(
        &self,
        mut visit: impl FnMut(gix::ObjectId) -> Result<(), E>,
    ) -> Result<HistoryStats, E>
    where
        E: From<GitHistoryError>,
    {
        let mut selected = BTreeSet::new();
        if matches!(
            self.traversal,
            HistoryTraversal::Ancestors { per_root: None }
        ) {
            selected.extend(
                self.walk_ancestors(self.roots.iter().copied(), None)
                    .map_err(E::from)?,
            );
        } else {
            for &root in &self.roots {
                selected.extend(self.select_from_root(root).map_err(E::from)?);
            }
        }
        for &id in &selected {
            visit(id)?;
        }
        Ok(HistoryStats {
            visited: selected.len(),
            shallow: self.shallow,
        })
    }

    fn select_from_root(&self, tip: gix::ObjectId) -> Result<Vec<gix::ObjectId>, GitHistoryError> {
        match self.traversal {
            HistoryTraversal::Tips => {
                if self.time.is_unbounded() || self.commit_time_in_window(tip)? {
                    Ok(vec![tip])
                } else {
                    Ok(Vec::new())
                }
            }
            HistoryTraversal::Ancestors { per_root } => self.walk_ancestors([tip], per_root),
        }
    }

    fn commit_time_in_window(&self, id: gix::ObjectId) -> Result<bool, GitHistoryError> {
        let commit = self
            .repo
            .find_commit(id)
            .map_err(|source| traversal("reading history selection root", source))?;
        let time = commit
            .time()
            .map_err(|source| traversal("reading commit time", source))?;
        Ok(self.time.contains(GitTimestamp::from(time.seconds)))
    }

    fn walk_ancestors(
        &self,
        tips: impl IntoIterator<Item = gix::ObjectId>,
        per_root: Option<NonZeroUsize>,
    ) -> Result<Vec<gix::ObjectId>, GitHistoryError> {
        let mut platform = self.repo.rev_walk(tips);
        if !self.hidden.is_empty() {
            platform = platform.with_hidden(self.hidden.clone());
        }
        if matches!(self.parents, ParentMode::First) {
            platform = platform.first_parent_only();
        }
        let walk = platform
            .all()
            .map_err(|source| traversal("walking commit history", source))?;

        let mut selected = Vec::new();
        let mut traversed = 0usize;
        for info in walk {
            let info =
                info.map_err(|source| traversal("reading commit during history walk", source))?;
            traversed += 1;
            if self.time.is_unbounded() {
                selected.push(info.id);
            } else {
                let commit = info
                    .object()
                    .map_err(|source| traversal("reading commit during history walk", source))?;
                let time = commit
                    .time()
                    .map_err(|source| traversal("reading commit time", source))?;
                if self.time.contains(GitTimestamp::from(time.seconds)) {
                    selected.push(info.id);
                }
            }
            if let Some(limit) = per_root
                && traversed >= limit.get()
            {
                break;
            }
        }
        Ok(selected)
    }
}

#[cfg(test)]
fn open(root: &Path) -> Result<gix::Repository, GitHistoryError> {
    gix::open(root).map_err(|source| {
        with_source(
            GitHistoryErrorKind::OpenRepository,
            Some(root),
            "opening repository",
            source,
        )
    })
}

fn resolve_selection<'repo>(
    repo: &'repo gix::Repository,
    selection: &HistorySelection,
) -> Result<ResolvedHistory<'repo>, GitHistoryError> {
    let mut seen = BTreeSet::new();
    let mut roots = Vec::new();
    for root in &selection.roots {
        for id in resolve_root(repo, root)? {
            if seen.insert(id) {
                roots.push(id);
            }
        }
    }
    let hidden = selection
        .excluded
        .iter()
        .map(|revision| resolve_commit_strict(repo, revision))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ResolvedHistory {
        repo,
        roots,
        hidden,
        traversal: selection.traversal,
        time: selection.time,
        parents: selection.parents,
        shallow: repo.is_shallow(),
    })
}

fn resolve_commit_strict(
    repo: &gix::Repository,
    revision: &GitRevisionSpec,
) -> Result<gix::ObjectId, GitHistoryError> {
    let text = revision.as_str();
    let object = repo
        .rev_parse_single(text)
        .map_err(|source| with_source(GitHistoryErrorKind::RevisionResolution, None, text, source))?
        .object()
        .map_err(|source| {
            with_source(GitHistoryErrorKind::RevisionResolution, None, text, source)
        })?;
    object
        .peel_to_commit()
        .map(|commit| commit.id)
        .map_err(|source| with_source(GitHistoryErrorKind::RevisionResolution, None, text, source))
}

fn resolve_root(
    repo: &gix::Repository,
    root: &HistoryRoot,
) -> Result<Vec<gix::ObjectId>, GitHistoryError> {
    match root {
        HistoryRoot::Head => Ok(vec![resolve_commit_strict(
            repo,
            &GitRevisionSpec::from("HEAD"),
        )?]),
        HistoryRoot::Revision(revision) => Ok(vec![resolve_commit_strict(repo, revision)?]),
        HistoryRoot::Branches => {
            let mut out = Vec::new();
            let refs = repo
                .references()
                .map_err(|source| traversal("listing refs for --branches", source))?;
            for reference in refs
                .local_branches()
                .map_err(|source| traversal("listing local branches for --branches", source))?
            {
                push_peeled_ref(
                    reference.map_err(|source| {
                        traversal_boxed("reading a local branch ref for --branches", source)
                    })?,
                    &mut out,
                )?;
            }
            for reference in refs.remote_branches().map_err(|source| {
                traversal("listing remote-tracking branches for --branches", source)
            })? {
                push_peeled_ref(
                    reference.map_err(|source| {
                        traversal_boxed(
                            "reading a remote-tracking branch ref for --branches",
                            source,
                        )
                    })?,
                    &mut out,
                )?;
            }
            Ok(out)
        }
        HistoryRoot::Tags => {
            let mut out = Vec::new();
            let refs = repo
                .references()
                .map_err(|source| traversal("listing refs for --tags", source))?;
            for reference in refs
                .tags()
                .map_err(|source| traversal("listing tags for --tags", source))?
            {
                push_peeled_ref(
                    reference.map_err(|source| {
                        traversal_boxed("reading a tag ref for --tags", source)
                    })?,
                    &mut out,
                )?;
            }
            Ok(out)
        }
        HistoryRoot::AllRefs => {
            let mut out = Vec::new();
            let refs = repo
                .references()
                .map_err(|source| traversal("listing refs for --all-history", source))?;
            for reference in refs
                .all()
                .map_err(|source| traversal("listing all refs for --all-history", source))?
            {
                push_peeled_ref(
                    reference.map_err(|source| {
                        traversal_boxed("reading a ref for --all-history", source)
                    })?,
                    &mut out,
                )?;
            }
            Ok(out)
        }
    }
}

fn push_peeled_ref(
    mut reference: gix::Reference<'_>,
    out: &mut Vec<gix::ObjectId>,
) -> Result<(), GitHistoryError> {
    let id = reference.peel_to_id().map_err(|source| {
        traversal(
            format!("peeling ref `{}`", reference.name().as_bstr()),
            source,
        )
    })?;
    let object = id.object().map_err(|source| {
        traversal(
            format!("reading ref `{}` target", reference.name().as_bstr()),
            source,
        )
    })?;
    if object.kind == gix::object::Kind::Commit {
        out.push(id.detach());
    }
    Ok(())
}

fn semantic_commit_id(id: gix::ObjectId) -> Result<GitCommitId, GitHistoryError> {
    match id {
        gix::ObjectId::Sha1(bytes) => Ok(GitCommitId::Sha1(bytes)),
        gix::ObjectId::Sha256(bytes) => Ok(GitCommitId::Sha256(bytes)),
        #[allow(unreachable_patterns)]
        _ => Err(GitHistoryError {
            kind: GitHistoryErrorKind::UnsupportedHashKind,
            root: None,
            subject: "selected commit".to_string(),
            detail: None,
            source: None,
        }),
    }
}

/// Visits each selected commit exactly once in deterministic ID order.
#[cfg(test)]
fn visit_history_commits<E>(
    root: &Path,
    selection: &HistorySelection,
    visit: impl FnMut(GitCommitId) -> Result<(), E>,
) -> Result<HistoryStats, E>
where
    E: From<GitHistoryError>,
{
    let repo = open(root).map_err(E::from)?;
    visit_history_commits_with(&repo, selection, visit)
}

pub(super) fn visit_history_commits_with<E>(
    repo: &gix::Repository,
    selection: &HistorySelection,
    mut visit: impl FnMut(GitCommitId) -> Result<(), E>,
) -> Result<HistoryStats, E>
where
    E: From<GitHistoryError>,
{
    let resolved = resolve_selection(repo, selection).map_err(E::from)?;
    resolved.visit_object_ids(|id| visit(semantic_commit_id(id).map_err(E::from)?))
}

/// Visits selected entries from every distinct historical `gat.lock`.
///
/// The lock shape is discovered from each commit tree. The current
/// on-disk lock is intentionally not included.
#[cfg(test)]
fn visit_history_lock_entries<E>(
    root: &Path,
    selection: &HistorySelection,
    keep: impl Fn(&str) -> bool,
    visit: impl FnMut(&Entry) -> Result<(), E>,
) -> Result<HistoryStats, E>
where
    E: From<GitHistoryError> + From<gat_core::lock::LockError>,
{
    let repo = open(root).map_err(E::from)?;
    visit_history_lock_entries_with(&repo, selection, keep, visit)
}

pub(super) fn visit_history_lock_entries_with<E>(
    repo: &gix::Repository,
    selection: &HistorySelection,
    keep: impl Fn(&str) -> bool,
    mut visit: impl FnMut(&Entry) -> Result<(), E>,
) -> Result<HistoryStats, E>
where
    E: From<GitHistoryError> + From<gat_core::lock::LockError>,
{
    let resolved = resolve_selection(repo, selection).map_err(E::from)?;
    let mut seen_lock_content = HashSet::new();
    resolved.visit_object_ids(|commit_id| {
        let commit = repo
            .find_commit(commit_id)
            .map_err(|source| E::from(traversal("reading commit during history walk", source)))?;
        let tree = commit
            .tree()
            .map_err(|source| E::from(traversal("reading commit tree", source)))?;
        let Some(entry) = tree.find_entry("gat.lock") else {
            return Ok(());
        };
        let entry_oid = entry.object_id();
        if !seen_lock_content.insert(entry_oid) {
            return Ok(());
        }
        if entry.mode().is_blob() {
            visit_lock_blob(repo, entry_oid, &keep, &mut visit)?;
        } else if entry.mode().is_tree() {
            let shard_tree = repo
                .find_tree(entry_oid)
                .map_err(|source| E::from(traversal("reading sharded gat.lock tree", source)))?;
            for file in shard_tree
                .traverse()
                .breadthfirst
                .files()
                .map_err(|source| E::from(traversal("traversing sharded gat.lock tree", source)))?
            {
                if file.mode.is_blob() && seen_lock_content.insert(file.oid) {
                    visit_lock_blob(repo, file.oid, &keep, &mut visit)?;
                }
            }
        }
        Ok(())
    })
}

fn visit_lock_blob<E>(
    repo: &gix::Repository,
    blob_oid: gix::ObjectId,
    keep: &impl Fn(&str) -> bool,
    visit: &mut dyn FnMut(&Entry) -> Result<(), E>,
) -> Result<(), E>
where
    E: From<GitHistoryError> + From<gat_core::lock::LockError>,
{
    #[cfg(test)]
    test_support::record_lock_blob_read();
    let blob = repo
        .find_blob(blob_oid)
        .map_err(|source| E::from(traversal("reading gat.lock blob", source)))?;
    let text = std::str::from_utf8(&blob.data).map_err(|source| {
        E::from(invalid_snapshot(
            format!("gat.lock blob {blob_oid}"),
            format!("blob is not valid UTF-8: {source}"),
            source,
        ))
    })?;
    let mut callback_error = None;
    let outcome = gat_core::lock::validated::visit_filtered_matching(
        text,
        |path| keep(path),
        |entry| {
            visit(&entry).map_err(|error| {
                callback_error = Some(error);
                gat_core::lock::LockError::CallbackFailed
            })
        },
    );
    if let Some(error) = callback_error {
        return Err(error);
    }

    outcome.map_err(E::from)
}

#[cfg(test)]
mod test_support {
    use std::cell::Cell;

    thread_local! {
        static LOCK_BLOB_READS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record_lock_blob_read() {
        LOCK_BLOB_READS.with(|counter| counter.set(counter.get() + 1));
    }

    pub(super) fn lock_blob_reads() -> usize {
        LOCK_BLOB_READS.with(Cell::get)
    }
}

/// Parses a CLI date through Git's date grammar into a semantic timestamp.
pub fn parse_cli_date(input: &str) -> Result<GitTimestamp, GitHistoryError> {
    gix::date::parse(input, Some(gix::date::Zoned::now()))
        .map(|time| GitTimestamp::from(time.seconds))
        .map_err(|source| {
            with_source(
                GitHistoryErrorKind::InvalidDate,
                None,
                input,
                source.into_error(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = test_support_git::GitCommand::new(root, args).run();
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "user.name", "Test"]);
        git(
            root.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        root
    }

    fn commit(root: &Path, path: &str, content: &[u8], message: &str) {
        std::fs::write(root.join(path), content).unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", message]);
    }

    #[test]
    fn commit_visitor_returns_semantic_ids_in_deterministic_order() {
        let root = repository();
        commit(root.path(), "a", b"a", "a");
        let first = GitCommitId::parse_hex(&git(root.path(), &["rev-parse", "HEAD"])).unwrap();
        commit(root.path(), "b", b"b", "b");
        let second = GitCommitId::parse_hex(&git(root.path(), &["rev-parse", "HEAD"])).unwrap();
        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            ..Default::default()
        };

        let mut actual = Vec::new();
        let stats = visit_history_commits::<GitHistoryError>(root.path(), &selection, |id| {
            actual.push(id);
            Ok(())
        })
        .unwrap();

        let mut expected = vec![first, second];
        expected.sort();
        assert_eq!(actual, expected);
        assert_eq!(stats.visited, 2);
    }

    #[test]
    fn explicit_non_commit_revision_fails_strictly() {
        let root = repository();
        let blob = git(root.path(), &["hash-object", "-w", "--stdin"]);
        let selection = HistorySelection {
            roots: vec![HistoryRoot::Revision(blob.into())],
            ..Default::default()
        };

        let error = visit_history_commits::<GitHistoryError>(root.path(), &selection, |_| Ok(()))
            .unwrap_err();

        assert_eq!(error.kind(), GitHistoryErrorKind::RevisionResolution);
    }

    #[test]
    fn head_non_commit_revision_fails_strictly() {
        let root = repository();
        let mut repo = gix::open(root.path()).unwrap();
        let head = repo
            .head_name()
            .unwrap()
            .expect("fresh repository has a symbolic HEAD");
        let blob = repo.write_blob(b"not a commit").unwrap().detach();
        repo.refs.write_reflog = gix::refs::store::WriteReflog::Disable;
        repo.reference(head, blob, gix::refs::transaction::PreviousValue::Any, "")
            .unwrap();
        drop(repo);
        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            ..Default::default()
        };

        let error = visit_history_commits::<GitHistoryError>(root.path(), &selection, |_| Ok(()))
            .unwrap_err();

        assert_eq!(error.kind(), GitHistoryErrorKind::RevisionResolution);
    }

    #[test]
    fn aggregate_refs_skip_non_commit_targets() {
        let root = repository();
        commit(root.path(), "a", b"a", "a");
        let blob = git(root.path(), &["hash-object", "a"]);
        git(root.path(), &["update-ref", "refs/custom/blob", &blob]);
        let selection = HistorySelection {
            roots: vec![HistoryRoot::AllRefs],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };

        let mut commits = Vec::new();
        let stats = visit_history_commits::<GitHistoryError>(root.path(), &selection, |id| {
            commits.push(id);
            Ok(())
        })
        .unwrap();

        assert_eq!(stats.visited, 1);
        assert_eq!(commits.len(), 1);
    }

    #[test]
    fn historical_lock_shape_comes_from_each_commit_tree() {
        let root = repository();
        let first = format!("{0}\n{1}\ta.bin\n", gat_core::lock::VERSION, "a".repeat(64));
        commit(root.path(), "gat.lock", first.as_bytes(), "flat");
        std::fs::remove_file(root.path().join("gat.lock")).unwrap();
        std::fs::create_dir(root.path().join("gat.lock")).unwrap();
        let second = format!("{0}\n{1}\tb.bin\n", gat_core::lock::VERSION, "b".repeat(64));
        commit(
            root.path(),
            "gat.lock/00.lock",
            second.as_bytes(),
            "sharded",
        );
        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            ..Default::default()
        };

        let mut paths = BTreeSet::new();
        let stats = visit_history_lock_entries::<HistoryLockTestError>(
            root.path(),
            &selection,
            |_| true,
            |entry| {
                paths.insert(entry.path.as_str().to_string());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(stats.visited, 2);
        assert_eq!(
            paths,
            BTreeSet::from(["a.bin".to_string(), "b.bin".to_string()])
        );
    }

    #[derive(Debug, thiserror::Error)]
    enum HistoryLockTestError {
        #[error(transparent)]
        History(#[from] GitHistoryError),
        #[error(transparent)]
        Lock(#[from] gat_core::lock::LockError),
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use gat_core::lexical_path::GatPath;
    use gat_core::oid::Oid;
    use std::process::Command as GitCommand;

    fn lock_path(path: &str) -> GatPath {
        GatPath::parse_canonical(path).unwrap()
    }

    fn lock_oid(digit: char) -> Oid {
        Oid::from_hex(&digit.to_string().repeat(64)).unwrap()
    }

    fn git_command(dir: &Path, args: &[&str]) -> GitCommand {
        test_support_git::GitCommand::empty(dir)
            .args(["-c", "core.autocrlf=false"])
            .args(args)
            .into_command()
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = git_command(dir, args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["config", "user.name", "Test"]);
        git(
            tmp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git(tmp.path(), &["commit", "-q", "--allow-empty", "-m", "init"]);
        tmp
    }

    fn commit_all(dir: &Path, message: &str) {
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
    }

    fn open_repo(dir: &std::path::Path) -> gix::Repository {
        gix::open(dir).unwrap()
    }

    fn write_commit(dir: &std::path::Path, file: &str, contents: &str, message: &str) {
        std::fs::write(dir.join(file), contents).unwrap();
        commit_all(dir, message);
    }

    /// Like [`write_commit`], but pins the commit's author/committer time
    /// to an exact Unix timestamp instead of "whenever the test happened
    /// to run" -- needed for tests that assert `--depth`/time-window
    /// interaction, where commit history must be deliberately
    /// non-monotonic (an in-range commit, then an out-of-range one, then
    /// back in-range) to distinguish "depth bounds traversal" from "depth
    /// bounds matches".
    fn write_commit_at(
        dir: &std::path::Path,
        file: &str,
        contents: &str,
        message: &str,
        seconds: i64,
    ) {
        std::fs::write(dir.join(file), contents).unwrap();
        git(dir, &["add", "-A"]);
        let date = format!("@{seconds} +0000");
        let out = git_command(dir, &["commit", "-q", "-m", message])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The name `test_repo()`'s initial branch got, whatever the
    /// environment's `init.defaultBranch`/compiled-in default is (tests
    /// must not assume `main`).
    fn current_branch(dir: &std::path::Path) -> String {
        let out = git_command(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn visit_all(history: &ResolvedHistory<'_>) -> (Vec<gix::ObjectId>, HistoryStats) {
        let mut ids = Vec::new();
        let stats = history
            .visit_object_ids(|id| -> Result<(), GitHistoryError> {
                ids.push(id);
                Ok(())
            })
            .unwrap();
        (ids, stats)
    }

    #[test]
    fn linear_history_depth_selects_closest_ancestors_first() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        write_commit(dir, "b.txt", "b", "B");
        write_commit(dir, "c.txt", "c", "C");
        write_commit(dir, "d.txt", "d", "D");
        write_commit(dir, "e.txt", "e", "E");
        let branch = current_branch(dir);
        let repo = open_repo(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Revision(branch.into())],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(3),
            },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert_eq!(stats.visited, 3);

        // E, D, C expected (closest ancestors first); order of `visit` is
        // sorted-by-id so just assert set membership/exclusion.
        let head = repo.head_id().unwrap().detach();
        let head_commit = repo.find_commit(head).unwrap();
        let parent1 = head_commit.parent_ids().next().unwrap().detach();
        let parent1_commit = repo.find_commit(parent1).unwrap();
        let parent2 = parent1_commit.parent_ids().next().unwrap().detach();
        let grandparent2 = repo
            .find_commit(parent2)
            .unwrap()
            .parent_ids()
            .next()
            .unwrap()
            .detach();

        assert!(ids.contains(&head));
        assert!(ids.contains(&parent1));
        assert!(ids.contains(&parent2));
        assert!(!ids.contains(&grandparent2));
    }

    #[test]
    fn linear_history_tips_selects_only_the_root_commit() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        write_commit(dir, "b.txt", "b", "B");
        write_commit(dir, "c.txt", "c", "C");
        write_commit(dir, "d.txt", "d", "D");
        write_commit(dir, "e.txt", "e", "E");
        let branch = current_branch(dir);
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Revision(branch.into())],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert_eq!(stats.visited, 1);
        assert_eq!(ids, vec![head]);
    }

    #[test]
    fn branches_tips_selects_only_distinct_branch_tips() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        git(dir, &["branch", "feature"]);
        write_commit(dir, "b.txt", "b", "B");
        let repo = open_repo(dir);
        let main_tip = repo.head_id().unwrap().detach();
        let feature_tip = repo.rev_parse_single("feature").unwrap().detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Branches],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert_eq!(stats.visited, 2);
        assert!(ids.contains(&main_tip));
        assert!(ids.contains(&feature_tip));
    }

    #[test]
    fn tags_tips_selects_only_tagged_commits() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        git(dir, &["tag", "v1"]);
        write_commit(dir, "b.txt", "b", "B");
        let repo = open_repo(dir);
        let tagged = repo.rev_parse_single("v1").unwrap().detach();
        let head = repo.head_id().unwrap().detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Tags],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert_eq!(stats.visited, 1);
        assert_eq!(ids, vec![tagged]);
        assert!(!ids.contains(&head));
    }

    #[test]
    fn branches_and_tags_roots_together_visit_complete_ancestry() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        git(dir, &["tag", "v1"]);
        write_commit(dir, "b.txt", "b", "B");
        let repo = open_repo(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Branches, HistoryRoot::Tags],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let walked = resolved
            .walk_ancestors(resolved.roots.iter().copied(), None)
            .unwrap();
        assert_eq!(walked.len(), 3, "shared ancestors must be enumerated once");
        let (_ids, stats) = visit_all(&resolved);
        // init + A + B: the full ancestry from the branch tip, plus the
        // tag root contributes nothing new since it's an ancestor of it.
        assert_eq!(stats.visited, 3);
    }

    /// `HistoryRoot::AllRefs` (`--all-history`) is a genuinely broader
    /// scope than `Branches`/`Tags` combined: it must also pick up refs
    /// that are neither a branch nor a tag (e.g. a CI/notes-style ref),
    /// which `Branches`/`Tags` roots never see.
    #[test]
    fn all_refs_root_includes_refs_outside_branches_and_tags() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();
        // A ref that's neither `refs/heads/**` nor `refs/tags/**`.
        write_commit(dir, "b.txt", "b", "B");
        let repo = open_repo(dir);
        let custom_tip = repo.head_id().unwrap().detach();
        git(
            dir,
            &["update-ref", "refs/custom/thing", &custom_tip.to_string()],
        );
        // Detach `refs/custom/thing` from the branch tip so it's *only*
        // reachable via `AllRefs`, not via `Branches`: reset the branch
        // back to the first commit.
        git(dir, &["reset", "--hard", &head.to_string()]);
        let repo = open_repo(dir);

        let branches_only = HistorySelection {
            roots: vec![HistoryRoot::Branches],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let (branches_ids, _) = visit_all(&resolve_selection(&repo, &branches_only).unwrap());
        assert!(!branches_ids.contains(&custom_tip));

        let all_refs = HistorySelection {
            roots: vec![HistoryRoot::AllRefs],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let (all_ids, _) = visit_all(&resolve_selection(&repo, &all_refs).unwrap());
        assert!(all_ids.contains(&custom_tip));
        assert!(all_ids.contains(&head));
    }

    #[test]
    fn multiple_roots_under_tips_visit_only_roots_and_dedup_identical_ids() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();
        let branch = current_branch(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Revision(branch.into()), HistoryRoot::Head],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // Both roots resolve to the same commit id, so this must
        // deduplicate to exactly one visited commit, not two.
        assert_eq!(stats.visited, 1);
        assert_eq!(ids, vec![head]);
    }

    #[test]
    fn merge_graph_tips_selects_only_the_merge_commit() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "base.txt", "base", "base");
        let base_branch = current_branch(dir);
        git(dir, &["checkout", "-b", "topic"]);
        write_commit(dir, "topic.txt", "topic", "topic");
        git(dir, &["checkout", &base_branch]);
        write_commit(dir, "main.txt", "main", "main-side");
        git(dir, &["merge", "--no-ff", "-m", "merge", "topic"]);
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert_eq!(stats.visited, 1);
        assert_eq!(ids, vec![head]);
    }

    #[test]
    fn multiple_roots_apply_depth_independently_before_dedup() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        write_commit(dir, "b.txt", "b", "B");
        git(dir, &["branch", "feature"]);
        write_commit(dir, "c.txt", "c", "C");
        write_commit(dir, "d.txt", "d", "D");
        let branch = current_branch(dir);
        let repo = open_repo(dir);

        let selection = HistorySelection {
            roots: vec![
                HistoryRoot::Revision(branch.into()),
                HistoryRoot::Revision("feature".into()),
            ],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(1),
            },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // Each root contributes its own single tip commit; no shared
        // ancestor is close enough to collide here, so both are distinct.
        assert_eq!(stats.visited, 2);
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn shared_ancestry_is_deduplicated_across_roots() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();
        let branch = current_branch(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Revision(branch.into()), HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // test_repo()'s initial "init" commit + "A".
        assert_eq!(stats.visited, 2);
        assert!(ids.contains(&head));
    }

    #[test]
    fn merge_commit_ancestry_visits_both_parents() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "base.txt", "base", "base");
        let base_branch = current_branch(dir);
        git(dir, &["checkout", "-b", "topic"]);
        write_commit(dir, "topic.txt", "topic", "topic");
        git(dir, &["checkout", &base_branch]);
        write_commit(dir, "main.txt", "main", "main-side");
        git(dir, &["merge", "--no-ff", "-m", "merge", "topic"]);
        let repo = open_repo(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (_ids, stats) = visit_all(&resolved);
        // init + base + topic + main-side + merge
        assert_eq!(stats.visited, 5);
    }

    #[test]
    fn first_parent_only_follows_first_parent_chain() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "base.txt", "base", "base");
        let base_branch = current_branch(dir);
        git(dir, &["checkout", "-b", "topic"]);
        write_commit(dir, "topic.txt", "topic", "topic");
        git(dir, &["checkout", &base_branch]);
        write_commit(dir, "main.txt", "main", "main-side");
        git(dir, &["merge", "--no-ff", "-m", "merge", "topic"]);
        let repo = open_repo(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            parents: ParentMode::First,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (_ids, stats) = visit_all(&resolved);
        // init, merge, main-side, base -- the topic-only commit is excluded.
        assert_eq!(stats.visited, 4);
    }

    #[test]
    fn branches_root_includes_local_and_remote_tracking_tips_deduplicated() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        git(dir, &["branch", "feature"]);
        let repo = open_repo(dir);

        // Fabricate a remote-tracking ref pointing at the same commit as
        // `main`, simulating `git fetch` having populated it, without
        // needing a real remote.
        let head = repo.head_id().unwrap().detach();
        git(
            dir,
            &["update-ref", "refs/remotes/origin/main", &head.to_string()],
        );
        let repo = open_repo(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Branches],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // main, feature, and origin/main all point at the same commit ->
        // one deduplicated root.
        assert_eq!(stats.visited, 1);
        assert_eq!(ids, vec![head]);
        // The semantic contract is the deduplicated selected commit set;
        // private ref provenance is intentionally not exposed.
    }

    /// Runs `git` with `args` in `dir` and returns its trimmed stdout,
    /// panicking on failure -- used by tests that need the value `git`
    /// prints (a hash, a ref name) rather than just its exit status.
    fn git_stdout(dir: &std::path::Path, args: &[&str]) -> String {
        let out = git_command(dir, args)
            .output()
            .unwrap_or_else(|e| panic!("running git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Writes `contents` to a new blob via `git hash-object -w --stdin`
    /// and returns its OID for building refs/tags that point at a
    /// non-commit object.
    fn write_blob(dir: &std::path::Path, contents: &[u8]) -> String {
        use std::io::Write;
        let mut child = git_command(dir, &["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("running git hash-object: {e}"));
        child
            .stdin
            .take()
            .unwrap()
            .write_all(contents)
            .unwrap_or_else(|e| panic!("writing blob contents: {e}"));
        let out = child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("running git hash-object: {e}"));
        assert!(out.status.success(), "git hash-object failed");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Writes a *dangling* ref at `refs/<name>` -- one whose target OID
    /// is syntactically valid but was never written to the object
    /// database. `git update-ref` refuses to create such a ref (it
    /// verifies the target object exists), so this bypasses it by
    /// writing the loose ref file directly, exactly as a corrupted
    /// repository (partial clone, interrupted transfer, disk
    /// corruption) would end up with a ref like this in practice.
    fn write_dangling_ref(dir: &std::path::Path, name: &str) {
        let fake_oid = "0123456789abcdef0123456789abcdef01234567";
        let ref_path = dir.join(".git").join("refs").join(name);
        std::fs::create_dir_all(ref_path.parent().unwrap())
            .unwrap_or_else(|e| panic!("creating parent dir for dangling ref {name}: {e}"));
        std::fs::write(&ref_path, format!("{fake_oid}\n"))
            .unwrap_or_else(|e| panic!("writing dangling ref {name}: {e}"));
    }

    #[test]
    fn tags_root_peels_annotated_and_lightweight_tags_to_commits() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        git(dir, &["tag", "lightweight"]);
        git(dir, &["tag", "-a", "annotated", "-m", "annotated tag"]);
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Tags],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert_eq!(stats.visited, 1);
        assert_eq!(ids, vec![head]);
    }

    /// `--tags` is commit-only: a lightweight tag pointing directly at a
    /// blob, and an annotated tag object pointing at a blob, are both
    /// silently skipped rather than causing an error -- unlike an
    /// explicit `--rev <tag>`, which would fail strictly on the same
    /// tag (see `resolve_commit_strict`).
    #[test]
    fn tags_root_skips_tags_that_do_not_resolve_to_a_commit() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();

        // A lightweight tag pointing directly at a blob.
        let blob_sha = write_blob(dir, b"not a commit");
        git(dir, &["update-ref", "refs/tags/blob-tag", &blob_sha]);
        // An annotated tag object pointing at the same blob.
        git(
            dir,
            &[
                "tag",
                "-a",
                "annotated-blob-tag",
                "-m",
                "points at a blob",
                &blob_sha,
            ],
        );
        // A normal tag pointing at the commit, as a control.
        git(dir, &["tag", "commit-tag"]);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Tags],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // Only `commit-tag` resolves to a commit; the two blob-pointing
        // tags are skipped without error, and don't cause duplicate
        // visits of `head` either.
        assert_eq!(ids, vec![head]);
        assert_eq!(stats.visited, 1);
    }

    /// `--all-history` is likewise commit-only: a custom ref pointing at
    /// a tree, and one pointing at a blob, are skipped without error
    /// while a sibling commit-bearing custom ref is still selected.
    #[test]
    fn all_refs_root_skips_refs_that_do_not_resolve_to_a_commit() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();

        let tree_sha = git_stdout(dir, &["rev-parse", "HEAD^{tree}"]);
        git(dir, &["update-ref", "refs/custom/tree-ref", &tree_sha]);
        let blob_sha = write_blob(dir, b"not a commit either");
        git(dir, &["update-ref", "refs/custom/blob-ref", &blob_sha]);
        git(
            dir,
            &["update-ref", "refs/custom/commit-ref", &head.to_string()],
        );

        let selection = HistorySelection {
            roots: vec![HistoryRoot::AllRefs],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert!(ids.contains(&head));
        assert_eq!(stats.visited, 1);
    }

    /// `HistorySelection::conservative_default()` (used as `gc`'s
    /// no-selection-given safety default, and as `--all-history`'s
    /// underlying root set) is built on `HistoryRoot::AllRefs`, so it
    /// must inherit the same commit-only behavior: a ref pointing at a
    /// non-commit object must not cause it to error, and every
    /// commit-bearing ref -- including ones outside branches/tags --
    /// must still be walked to its full unbounded ancestry.
    #[test]
    fn conservative_default_skips_non_commit_refs_but_walks_every_commit_bearing_ref() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        write_commit(dir, "b.txt", "b", "B");
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();
        let parent = repo
            .find_commit(head)
            .unwrap()
            .parent_ids()
            .next()
            .unwrap()
            .detach();

        let blob_sha = write_blob(dir, b"not a commit");
        git(dir, &["update-ref", "refs/custom/blob-ref", &blob_sha]);
        git(
            dir,
            &["update-ref", "refs/custom/commit-ref", &head.to_string()],
        );

        let selection = HistorySelection::conservative_default();
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, _stats) = visit_all(&resolved);
        // The non-commit ref didn't cause an error, and both the
        // branch-reachable commits are present via full unbounded
        // ancestry from the commit-bearing refs.
        assert!(ids.contains(&head));
        assert!(ids.contains(&parent));
    }

    /// A dangling ref (syntactically valid target OID, but the object
    /// was never written to the database -- e.g. from a corrupted repo,
    /// a partial clone, or an interrupted transfer) is fundamentally
    /// different from a ref that *validly* resolves to a non-commit
    /// object: `gix` cannot even peel it to find out what kind of
    /// object it points at. `--branches` must fail closed on this
    /// rather than silently proceeding as if the ref didn't exist,
    /// because doing so could make `gc` sweep away content that ref
    /// was meant to protect.
    #[test]
    fn branches_root_errors_on_a_dangling_ref() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);

        write_dangling_ref(dir, "heads/dangling");

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Branches],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let Err(err) = resolve_selection(&repo, &selection) else {
            panic!("expected resolve() to fail closed on the dangling ref")
        };
        assert!(
            format!("{err:#}").contains("dangling"),
            "expected the dangling branch ref name in the error, got: {err:#}"
        );
    }

    /// Same fail-closed contract as
    /// `branches_root_errors_on_a_dangling_ref`, but for `--tags`.
    #[test]
    fn tags_root_errors_on_a_dangling_ref() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);

        write_dangling_ref(dir, "tags/dangling");

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Tags],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let Err(err) = resolve_selection(&repo, &selection) else {
            panic!("expected resolve() to fail closed on the dangling ref")
        };
        assert!(
            format!("{err:#}").contains("dangling"),
            "expected the dangling tag ref name in the error, got: {err:#}"
        );
    }

    /// Same fail-closed contract as
    /// `branches_root_errors_on_a_dangling_ref`, but for `--all-history`,
    /// exercised via a custom ref outside `refs/heads`/`refs/tags`.
    #[test]
    fn all_refs_root_errors_on_a_dangling_ref() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);

        write_dangling_ref(dir, "custom/dangling");

        let selection = HistorySelection {
            roots: vec![HistoryRoot::AllRefs],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let Err(err) = resolve_selection(&repo, &selection) else {
            panic!("expected resolve() to fail closed on the dangling ref")
        };
        assert!(
            format!("{err:#}").contains("dangling"),
            "expected the dangling custom ref name in the error, got: {err:#}"
        );
    }

    /// `HistorySelection::conservative_default()` is `gc`'s
    /// no-selection-given safety default. A dangling ref anywhere in the
    /// repository must make it fail closed (return an error) rather
    /// than silently compute an incomplete keep-set -- the whole point
    /// of `conservative_default()` is to never let `gc` sweep away
    /// content it can't fully account for.
    #[test]
    fn conservative_default_errors_on_a_dangling_ref() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);

        write_dangling_ref(dir, "custom/dangling");

        let selection = HistorySelection::conservative_default();
        let Err(err) = resolve_selection(&repo, &selection) else {
            panic!("expected resolve() to fail closed on the dangling ref")
        };
        assert!(
            format!("{err:#}").contains("dangling"),
            "expected the dangling ref name in the error, got: {err:#}"
        );
    }

    #[test]
    fn time_window_filters_out_commits_outside_the_range() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        write_commit(dir, "b.txt", "b", "B");
        write_commit(dir, "c.txt", "c", "C");
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();
        let head_commit = repo.find_commit(head).unwrap();
        let mid_time = head_commit.time().unwrap().seconds;

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            time: TimeWindow {
                since: Some(GitTimestamp::from(mid_time)),
                until: None,
            },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // Only the most recent commit was committed at/after `mid_time`
        // in this fast, same-second test run; assert it's at least
        // present and older commits authored strictly before are absent.
        assert!(ids.contains(&head));
        assert_eq!(stats.visited, ids.len());
    }

    /// `--depth N` bounds how many commits are *traversed*, not how many
    /// end up matching the time window: an in-range commit at the tip, an
    /// out-of-range commit as its parent, and an in-range grandparent
    /// must select only the tip, not `{tip, grandparent}` -- the
    /// out-of-range parent still consumes one unit of the depth-2 budget.
    #[test]
    fn depth_bounds_traversal_before_time_filter_is_applied_since() {
        let tmp = test_repo();
        let dir = tmp.path();
        // A: t=1000 (in range), B: t=500 (out of range, since >= 900),
        // C: t=1500 (in range, but only reachable past the depth-2 cap).
        write_commit_at(dir, "a.txt", "a", "A", 1000);
        write_commit_at(dir, "b.txt", "b", "B", 500);
        write_commit_at(dir, "c.txt", "c", "C", 1500);
        let repo = open_repo(dir);
        let head_c = repo.head_id().unwrap().detach();
        let parent_b = repo
            .find_commit(head_c)
            .unwrap()
            .parent_ids()
            .next()
            .unwrap()
            .detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(2),
            },
            time: TimeWindow {
                since: Some(GitTimestamp::from(900)),
                until: None,
            },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // Traversal visits C (t=1500, in range) then B (t=500, out of
        // range) and stops there -- A (t=1000, in range) is never
        // reached because the depth-2 budget was already spent on B.
        assert_eq!(ids, vec![head_c]);
        assert_eq!(stats.visited, 1);
        assert!(!ids.contains(&parent_b));
    }

    /// The `--until` equivalent of the `--since` case above: an in-range
    /// tip, an out-of-range parent, then an in-range grandparent that
    /// must not be reached because the out-of-range parent already used
    /// up the depth-2 traversal budget.
    #[test]
    fn depth_bounds_traversal_before_time_filter_is_applied_until() {
        let tmp = test_repo();
        let dir = tmp.path();
        // A: t=500 (in range, until <= 900), B: t=1500 (out of range),
        // C: t=800 (in range, but past the depth-2 cap).
        write_commit_at(dir, "a.txt", "a", "A", 500);
        write_commit_at(dir, "b.txt", "b", "B", 1500);
        write_commit_at(dir, "c.txt", "c", "C", 800);
        let repo = open_repo(dir);
        let head_c = repo.head_id().unwrap().detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(2),
            },
            time: TimeWindow {
                since: None,
                until: Some(GitTimestamp::from(900)),
            },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert_eq!(ids, vec![head_c]);
        assert_eq!(stats.visited, 1);
    }

    /// If none of the first `N` traversed commits satisfy the time
    /// window, the result is empty -- traversal must not silently keep
    /// walking past the depth cap looking for a match.
    #[test]
    fn depth_cap_can_yield_an_empty_result_when_no_traversed_commit_matches() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit_at(dir, "a.txt", "a", "A", 100);
        write_commit_at(dir, "b.txt", "b", "B", 200);
        write_commit_at(dir, "c.txt", "c", "C", 300);
        let repo = open_repo(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(2),
            },
            time: TimeWindow {
                since: Some(GitTimestamp::from(10_000)),
                until: None,
            },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert!(ids.is_empty());
        assert_eq!(stats.visited, 0);
    }

    /// Depth-before-time evaluation is independent per root: two
    /// branches each get their own depth-2 traversal budget and their
    /// own time-window evaluation, before the results are unioned.
    #[test]
    fn depth_before_time_is_applied_independently_per_root() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit_at(dir, "base.txt", "base", "base", 1000);
        let base_branch = current_branch(dir);
        git(dir, &["checkout", "-b", "feature"]);
        // On `feature`: in-range tip, out-of-range parent (consumes the
        // depth-2 budget before the in-range base commit is reached).
        write_commit_at(dir, "f1.txt", "f1", "F-out-of-range", 500);
        write_commit_at(dir, "f2.txt", "f2", "F-in-range", 1500);
        git(dir, &["checkout", &base_branch]);
        // On the base branch: two more in-range commits, both within the
        // depth-2 budget.
        write_commit_at(dir, "m1.txt", "m1", "M-in-range-1", 1100);
        write_commit_at(dir, "m2.txt", "m2", "M-in-range-2", 1200);
        let repo = open_repo(dir);
        let base_tip = repo
            .rev_parse_single(base_branch.as_str())
            .unwrap()
            .detach();
        let feature_tip = repo.rev_parse_single("feature").unwrap().detach();

        let selection = HistorySelection {
            roots: vec![
                HistoryRoot::Revision(base_branch.into()),
                HistoryRoot::Revision("feature".into()),
            ],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(2),
            },
            time: TimeWindow {
                since: Some(GitTimestamp::from(900)),
                until: None,
            },
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, _stats) = visit_all(&resolved);
        // Base branch: both traversed commits (tip + parent) are in
        // range, so both are selected.
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&base_tip));
        // Feature branch: only its in-range tip is selected -- its
        // out-of-range parent consumed the depth-2 budget, so the
        // in-range `base` commit beneath it is never reached.
        assert!(ids.contains(&feature_tip));
    }

    /// `--first-parent` still determines which edges are walked *before*
    /// depth is counted: on a merge graph, the depth-2 budget must be
    /// spent on the first-parent chain, not on the merged-in branch, and
    /// the time filter is applied to whatever that first-parent walk
    /// actually traverses.
    #[test]
    fn first_parent_depth_and_time_filter_compose_on_a_merge_graph() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit_at(dir, "base.txt", "base", "base", 1000);
        let base_branch = current_branch(dir);
        git(dir, &["checkout", "-b", "topic"]);
        write_commit_at(dir, "topic.txt", "topic", "topic", 1100);
        git(dir, &["checkout", &base_branch]);
        write_commit_at(dir, "main.txt", "main", "main-side", 500);
        let out = git_command(dir, &["merge", "--no-ff", "-m", "merge", "topic"])
            .env("GIT_AUTHOR_DATE", "@1500 +0000")
            .env("GIT_COMMITTER_DATE", "@1500 +0000")
            .output()
            .unwrap();
        assert!(out.status.success());
        let repo = open_repo(dir);
        let merge = repo.head_id().unwrap().detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(2),
            },
            time: TimeWindow {
                since: Some(GitTimestamp::from(900)),
                until: None,
            },
            parents: ParentMode::First,
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // First-parent walk visits merge (t=1500, in range) then
        // main-side (t=500, out of range) and stops -- `topic` (t=1100,
        // in range) is never visited since it's not on the first-parent
        // chain.
        assert_eq!(ids, vec![merge]);
        assert_eq!(stats.visited, 1);
    }

    /// `--exclude-rev` still hides a commit and its ancestors from the
    /// walk entirely (they are never traversed, so they never consume
    /// depth budget) -- the depth-2 cap and time filter apply only to
    /// whatever remains reachable after exclusion. Uses a merge graph so
    /// the excluded branch is deterministically never a traversal
    /// candidate at all (a linear-chain exclusion would also hide every
    /// commit *beneath* the excluded one, which can't demonstrate freed
    /// depth budget the way a merge's second parent can).
    #[test]
    fn exclude_rev_removes_commits_from_traversal_before_depth_and_time_are_applied() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit_at(dir, "base.txt", "base", "base", 1000);
        let base_branch = current_branch(dir);

        git(dir, &["checkout", "-b", "topic"]);
        write_commit_at(dir, "topic.txt", "topic", "topic", 999);
        let topic_tip = {
            let repo = open_repo(dir);
            repo.head_id().unwrap().detach()
        };

        git(dir, &["checkout", &base_branch]);
        write_commit_at(dir, "m1.txt", "m1", "M1-in-range", 1100);
        let m1 = {
            let repo = open_repo(dir);
            repo.head_id().unwrap().detach()
        };

        let out = git_command(dir, &["merge", "--no-ff", "-m", "merge", "topic"])
            .env("GIT_AUTHOR_DATE", "@1500 +0000")
            .env("GIT_COMMITTER_DATE", "@1500 +0000")
            .output()
            .unwrap();
        assert!(out.status.success());
        let repo = open_repo(dir);
        let merge = repo.head_id().unwrap().detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors {
                per_root: NonZeroUsize::new(2),
            },
            time: TimeWindow {
                since: Some(GitTimestamp::from(900)),
                until: None,
            },
            excluded: vec![topic_tip.to_string().into()],
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        // `topic` is hidden along with its ancestry, so it is never a
        // traversal candidate at all -- the depth-2 budget is spent
        // entirely on the merge commit and its remaining predecessor
        // `m1`, both of which are in range.
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&merge));
        assert!(ids.contains(&m1));
        assert!(!ids.contains(&topic_tip));
        assert_eq!(stats.visited, 2);
    }

    #[test]
    fn exclude_rev_hides_commit_and_its_ancestors() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        write_commit(dir, "b.txt", "b", "B");
        let repo = open_repo(dir);
        let head = repo.head_id().unwrap().detach();
        let parent = repo
            .find_commit(head)
            .unwrap()
            .parent_ids()
            .next()
            .unwrap()
            .detach();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            excluded: vec![parent.to_string().into()],
            ..Default::default()
        };
        let resolved = resolve_selection(&repo, &selection).unwrap();
        let (ids, stats) = visit_all(&resolved);
        assert_eq!(stats.visited, 1);
        assert_eq!(ids, vec![head]);
    }

    #[test]
    fn strict_resolution_rejects_unknown_revision() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Revision("does-not-exist".into())],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let Err(err) = resolve_selection(&repo, &selection) else {
            panic!("expected an error resolving an unknown revision")
        };
        assert_eq!(err.kind(), GitHistoryErrorKind::RevisionResolution);
        assert_eq!(err.subject(), "does-not-exist");
    }

    /// A revision that resolves to *something*, but not a commit (here, a
    /// blob), classifies the same way as an unknown revision above
    /// ([`GitHistoryErrorKind::RevisionResolution`]): `gix` 0.86's rev-spec
    /// parser reports both cases through the same type-erased
    /// `gix_error::Error`, with no stable typed signal left to
    /// distinguish "does not exist" from "resolved to something that
    /// isn't a commit" without parsing `gix`'s own `Display` wording
    /// (which Gat deliberately does not do).
    #[test]
    fn strict_resolution_of_a_non_commit_object_is_a_resolution_failure() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        let repo = open_repo(dir);
        let out = git_command(dir, &["hash-object", "-w", "a.txt"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let blob_id = String::from_utf8(out.stdout).unwrap().trim().to_string();

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Revision(blob_id.clone().into())],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        };
        let Err(err) = resolve_selection(&repo, &selection) else {
            panic!("expected an error resolving a non-commit revision")
        };
        assert_eq!(err.kind(), GitHistoryErrorKind::RevisionResolution);
        assert_eq!(err.subject(), blob_id);
    }

    /// Opening a directory that isn't a git repository at all must report
    /// [`GitHistoryErrorKind::OpenRepository`] without exposing Gix types.
    #[test]
    fn opening_a_non_repository_directory_reports_open_failed_without_gix_wording() {
        let tmp = tempfile::tempdir().unwrap();
        let err = super::open(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), GitHistoryErrorKind::OpenRepository);
        assert_eq!(err.root(), Some(tmp.path()));
        let rendered = err.to_string();
        for banned in ["gix::", "Exn<", "gix_error", "::exn::"] {
            assert!(
                !rendered.contains(banned),
                "rendered diagnostic leaked gix-internal wording {banned:?}: {rendered}"
            );
        }
    }

    #[test]
    fn shallow_repository_is_reported_on_resolved_history() {
        let tmp = test_repo();
        let dir = tmp.path();
        write_commit(dir, "a.txt", "a", "A");
        write_commit(dir, "b.txt", "b", "B");

        let shallow_dir = tempfile::tempdir().unwrap();
        let source_url = format!("file://{}", dir.to_str().unwrap());
        git(
            shallow_dir.path(),
            &["clone", "--depth", "1", &source_url, "."],
        );
        let shallow_repo = open_repo(shallow_dir.path());
        assert!(shallow_repo.is_shallow());

        let selection = HistorySelection::conservative_default();
        let resolved = resolve_selection(&shallow_repo, &selection).unwrap();
        assert!(resolved.shallow);
        let (_ids, stats) = visit_all(&resolved);
        assert!(stats.shallow);
    }

    fn head_only() -> HistorySelection {
        HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Tips,
            ..Default::default()
        }
    }

    #[test]
    fn history_blob_certifies_the_file_once() {
        let tmp = test_repo();
        let mut lock = gat_core::lock::Lock::default();
        lock.upsert(lock_path("a.bin"), lock_oid('a'));
        lock.upsert(lock_path("data/b.bin"), lock_oid('b'));
        lock.upsert(lock_path("data/c.bin"), lock_oid('c'));
        crate::LockStore::publish_repository(
            &crate::RepositoryLayout::at(tmp.path().to_path_buf()),
            &lock,
            gat_core::lock::LockShardLevels::FLAT,
        )
        .unwrap();
        commit_all(tmp.path(), "persist lock");

        let before = crate::lock::test_support::file_validation_parses();
        let mut kept = Vec::new();
        let stats = visit_history_lock_entries::<Box<dyn std::error::Error>>(
            tmp.path(),
            &head_only(),
            |path| path.starts_with("data/"),
            |entry| {
                kept.push(entry.path.clone());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(stats.visited, 1);
        assert_eq!(
            crate::lock::test_support::file_validation_parses() - before,
            1,
            "a historical lock blob must be certified exactly once"
        );
        assert_eq!(kept, vec!["data/b.bin", "data/c.bin"]);
    }

    #[test]
    fn history_lock_validation_errors_are_unchanged() {
        let cases = [
            (
                format!(
                    "{0}\n{1}\tshared.bin\n{2}\tshared.bin\n",
                    gat_core::lock::VERSION,
                    "a".repeat(64),
                    "b".repeat(64)
                ),
                "tracked more than once",
            ),
            (
                format!(
                    "{0}\n{1}\tfoo\n{2}\tfoo/bar\n",
                    gat_core::lock::VERSION,
                    "a".repeat(64),
                    "b".repeat(64)
                ),
                "directory prefix",
            ),
            (
                format!(
                    "{}\n\"bad.bin\"\tsha256:{}\n",
                    gat_core::lock::VERSION,
                    "a".repeat(64)
                ),
                "expected a TAB",
            ),
        ];

        for (index, (text, needle)) in cases.into_iter().enumerate() {
            let tmp = test_repo();
            std::fs::write(tmp.path().join("gat.lock"), text).unwrap();
            commit_all(tmp.path(), &format!("bad lock {index}"));

            let error = visit_history_lock_entries::<Box<dyn std::error::Error>>(
                tmp.path(),
                &head_only(),
                |_| true,
                |_| Ok(()),
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains(needle),
                "expected {needle:?} in {error:#}"
            );
        }
    }

    #[test]
    fn history_lock_blob_with_invalid_utf8_is_reported_not_lossily_repaired() {
        let tmp = test_repo();
        let mut bytes = format!("{}\n", gat_core::lock::VERSION).into_bytes();
        bytes.extend_from_slice(b"\"\xffbad.bin\"\tblake3:");
        bytes.extend_from_slice("a".repeat(64).as_bytes());
        bytes.push(b'\n');
        std::fs::write(tmp.path().join("gat.lock"), bytes).unwrap();
        commit_all(tmp.path(), "invalid utf8 lock");

        let error = visit_history_lock_entries::<Box<dyn std::error::Error>>(
            tmp.path(),
            &head_only(),
            |_| true,
            |_| Ok(()),
        )
        .unwrap_err();

        assert_eq!(
            error
                .downcast_ref::<GitHistoryError>()
                .map(GitHistoryError::kind),
            Some(GitHistoryErrorKind::InvalidLockSnapshot)
        );
        assert!(format!("{error:#}").contains("not valid UTF-8"));
    }

    #[test]
    fn one_reader_open_is_reused_across_commit_and_lock_history_visits() {
        let tmp = test_repo();
        let mut lock = gat_core::lock::Lock::default();
        lock.upsert(lock_path("a.bin"), lock_oid('a'));
        crate::LockStore::publish_repository(
            &crate::RepositoryLayout::at(tmp.path().to_path_buf()),
            &lock,
            gat_core::lock::LockShardLevels::FLAT,
        )
        .unwrap();
        commit_all(tmp.path(), "persist lock");

        let before = crate::git::test_support::repository_opens();
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let reader = crate::GitReader::open(&layout).unwrap();
        reader
            .visit_history_commits::<GitHistoryError>(&head_only(), |_| Ok(()))
            .unwrap();
        reader
            .visit_history_lock_entries::<Box<dyn std::error::Error>>(
                &head_only(),
                |_| true,
                |_| Ok(()),
            )
            .unwrap();

        assert_eq!(crate::git::test_support::repository_opens() - before, 1);
    }

    #[test]
    fn historical_lock_blobs_are_read_once_per_distinct_object_id() {
        let tmp = test_repo();
        let mut lock = gat_core::lock::Lock::default();
        lock.upsert(lock_path("data/a.bin"), lock_oid('a'));
        lock.upsert(lock_path("data/b.bin"), lock_oid('b'));
        let levels = gat_core::lock::LockShardLevels::new(2).unwrap();
        crate::LockStore::publish_repository(
            &crate::RepositoryLayout::at(tmp.path().to_path_buf()),
            &lock,
            levels,
        )
        .unwrap();
        commit_all(tmp.path(), "persist sharded lock");
        let layout = crate::RepositoryLayout::at(tmp.path().to_path_buf());
        let initial_shards = crate::LockSnapshot::at_rev(&layout, &GitRevisionSpec::from("HEAD"))
            .unwrap()
            .shards()
            .len();

        write_commit(tmp.path(), "unrelated.txt", "one", "unchanged lock");
        lock.upsert(lock_path("data/a.bin"), lock_oid('c'));
        crate::LockStore::publish_repository(
            &crate::RepositoryLayout::at(tmp.path().to_path_buf()),
            &lock,
            levels,
        )
        .unwrap();
        commit_all(tmp.path(), "change one lock row");

        let selection = HistorySelection {
            roots: vec![HistoryRoot::Head],
            traversal: HistoryTraversal::Ancestors { per_root: None },
            ..Default::default()
        };
        let before = super::test_support::lock_blob_reads();
        visit_history_lock_entries::<Box<dyn std::error::Error>>(
            tmp.path(),
            &selection,
            |_| true,
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(
            super::test_support::lock_blob_reads() - before,
            initial_shards + 1,
            "unchanged shard blobs must be reused across commits"
        );
    }
}
