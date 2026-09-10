//! Repository-bound garbage collection.
//!
//! The engine owns reachability coordination and fail-closed policy while
//! physical Git, cache, and remote operations stay in
//! `gat-io`.

use std::collections::HashSet;
use std::num::NonZeroUsize;

use gat_core::git_location::GitLocationSpec;
use gat_core::history::{HistoryRoot, HistorySelection, HistoryTraversal};
use gat_core::name::RemoteName;
use gat_core::oid::Oid;
use gat_core::progress::{
    ProgressActivity, ProgressHandle, ProgressOperation, ProgressReporter, ProgressSpec,
    ProgressUnit, with_progress_typed,
};
use gat_io::CacheSweepDecision;
use rayon::prelude::*;

use crate::history::HistoryError;
use crate::limits::GcLimits;
use crate::remote_catalog::RemoteCatalog;
use crate::remote_session::RemoteSession;
use crate::repository::Repository;

type BoxedSource = Box<dyn std::error::Error + Send + Sync + 'static>;
type Result<T> = std::result::Result<T, GcError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcFailureKind {
    Repository,
    Cache,
    Remote,
    KeepSet,
}

#[derive(Debug, thiserror::Error)]
#[error("garbage collection failed while accessing {kind:?}")]
pub struct GcFailure {
    kind: GcFailureKind,
    confirmed_deletions: Option<usize>,
    #[source]
    source: BoxedSource,
}

impl GcFailure {
    fn new(kind: GcFailureKind, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            confirmed_deletions: None,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> GcFailureKind {
        self.kind
    }

    /// Completed remote deletions before a sweep failure. Additional objects
    /// may have been deleted by an unconfirmed, partially failed native batch.
    #[must_use]
    pub const fn confirmed_deletions(&self) -> Option<usize> {
        self.confirmed_deletions
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcRepositoryFailureKind {
    InvalidLocation,
    CloneFailed,
    ShallowHistory,
    Inspection,
}

#[derive(Debug, thiserror::Error)]
#[error("additional repository history is shallow")]
struct ShallowGcRepository;

#[derive(Debug, thiserror::Error)]
#[error("an additional repository could not be inspected")]
pub struct GcRepositoryIssue {
    location: GitLocationSpec,
    kind: GcRepositoryFailureKind,
    #[source]
    source: BoxedSource,
}

impl GcRepositoryIssue {
    fn new(
        location: GitLocationSpec,
        kind: GcRepositoryFailureKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            location,
            kind,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn location(&self) -> &GitLocationSpec {
        &self.location
    }

    #[must_use]
    pub const fn kind(&self) -> GcRepositoryFailureKind {
        self.kind
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("could not initialize remote for garbage collection")]
    RemoteOpen {
        remote_name: RemoteName,
        #[source]
        source: crate::remote_session::RemoteSessionError,
    },
    #[error("remote deletion requires explicit unsafe acknowledgement")]
    RemoteDeletionRequiresUnsafe,
    #[error(transparent)]
    Failure(#[from] GcFailure),
    #[error("refusing destructive {scope} gc because the keep set is incomplete")]
    IncompleteKeepSet {
        scope: &'static str,
        issues: Vec<GcRepositoryIssue>,
    },
    #[error("refusing destructive {scope} gc because this repository is shallow")]
    ShallowHistory { scope: &'static str },
}

#[derive(Clone, Debug)]
pub struct GcOptions<'a> {
    pub dry_run: bool,
    pub unsafe_override: bool,
    pub remote: Option<&'a RemoteName>,
    pub repositories: &'a [GitLocationSpec],
    /// Applied to every repository; None keeps the working lock and peer HEAD locks.
    pub history: Option<&'a HistorySelection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcReport {
    pub deleted: usize,
    pub uncertain: usize,
    pub incomplete_repositories: usize,
    pub forced_incomplete_keep_set: bool,
}

#[derive(Debug, thiserror::Error)]
enum MarkError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    IoHistory(#[from] gat_io::GitHistoryError),
    #[error(transparent)]
    GitOpen(#[from] gat_io::GitOpenError),
    #[error(transparent)]
    Lock(#[from] gat_core::lock::LockError),
    #[error(transparent)]
    LockStore(#[from] gat_io::LockError),
}

fn failure(kind: GcFailureKind, source: impl std::error::Error + Send + Sync + 'static) -> GcError {
    GcFailure::new(kind, source).into()
}

/// Reuse the larger table and avoid reserving space for already-shared OIDs.
fn merge_keep_set(keep: &mut HashSet<Oid>, mut marked: HashSet<Oid>) {
    if marked.len() > keep.len() {
        std::mem::swap(keep, &mut marked);
    }
    for oid in marked {
        keep.insert(oid);
    }
}

fn mark_repository(
    repo: &Repository,
    selection: Option<&HistorySelection>,
) -> std::result::Result<(HashSet<Oid>, bool), MarkError> {
    let mut keep = HashSet::new();
    let shallow = if let Some(selection) = selection {
        repo.visit_history_lock_entries::<MarkError>(
            selection,
            |_| true,
            |entry| {
                keep.insert(entry.oid);
                Ok(())
            },
        )?
        .shallow
    } else {
        false
    };
    gat_io::LockStore::visit_repository(
        repo.layout(),
        None,
        |_| true,
        |entry| {
            keep.insert(entry.oid);
            Ok(())
        },
    )?;
    Ok((keep, shallow))
}

fn complete_repository_inspection(
    location: GitLocationSpec,
    result: std::result::Result<(HashSet<Oid>, bool), MarkError>,
) -> std::result::Result<HashSet<Oid>, GcRepositoryIssue> {
    let (keep, shallow) = result.map_err(|error| {
        GcRepositoryIssue::new(location.clone(), GcRepositoryFailureKind::Inspection, error)
    })?;
    if shallow {
        return Err(GcRepositoryIssue::new(
            location,
            GcRepositoryFailureKind::ShallowHistory,
            ShallowGcRepository,
        ));
    }
    Ok(keep)
}

fn inspect_additional_repository(
    location: &GitLocationSpec,
    progress: &ProgressHandle,
    selection: &HistorySelection,
) -> std::result::Result<HashSet<Oid>, GcRepositoryIssue> {
    let parsed = gat_io::parse_location(location).map_err(|error| {
        GcRepositoryIssue::new(
            location.clone(),
            GcRepositoryFailureKind::InvalidLocation,
            error,
        )
    })?;
    progress.set_activity(ProgressActivity::CloningSource {
        location: location.clone(),
    });
    let prepared = gat_io::prepare_bare_repository(&parsed).map_err(|error| {
        GcRepositoryIssue::new(
            location.clone(),
            GcRepositoryFailureKind::CloneFailed,
            error,
        )
    })?;
    complete_repository_inspection(location.clone(), mark_bare_repository(&prepared, selection))
}

fn inspect_in_bounded_batches<T, R>(
    candidates: &[T],
    batch_size: NonZeroUsize,
    inspect: impl Fn(&T) -> R + Sync,
    consume: impl FnMut(usize, R) + Send,
) where
    T: Sync,
    R: Send,
{
    // Completed keep sets are merged immediately rather than retained until
    // the slowest inspection in the batch finishes. Only the consumer is
    // serialized; repository scans continue independently.
    let consume = std::sync::Mutex::new(consume);
    for (batch_index, batch) in candidates.chunks(batch_size.get()).enumerate() {
        batch.par_iter().enumerate().for_each(|(index, candidate)| {
            let result = inspect(candidate);
            consume.lock().expect("inspection consumer did not panic")(
                batch_index * batch_size.get() + index,
                result,
            );
        });
    }
}

fn mark_bare_repository(
    repo: &gat_io::PreparedBareGitRepository,
    selection: &HistorySelection,
) -> std::result::Result<(HashSet<Oid>, bool), MarkError> {
    let mut keep = HashSet::new();
    let stats = repo.visit_history_lock_entries::<MarkError>(
        selection,
        |_| true,
        |entry| {
            keep.insert(entry.oid);
            Ok(())
        },
    )?;
    Ok((keep, stats.shallow))
}

fn additional_repositories(
    options: &GcOptions<'_>,
    keep: &mut HashSet<Oid>,
    progress: &ProgressHandle,
    limits: GcLimits,
) -> Vec<GcRepositoryIssue> {
    let mut issues = Vec::new();
    let tips = HistorySelection {
        roots: vec![HistoryRoot::Head],
        traversal: HistoryTraversal::Tips,
        ..HistorySelection::default()
    };
    let selection = options.history.unwrap_or(&tips);
    let mut seen = HashSet::new();
    let candidates: Vec<_> = options
        .repositories
        .iter()
        .filter(|location| seen.insert(location.as_location_str()))
        .collect();
    inspect_in_bounded_batches(
        &candidates,
        limits.repository_concurrency,
        |location| inspect_additional_repository(location, progress, selection),
        |index, result| match result {
            Ok(marked) => merge_keep_set(keep, marked),
            Err(issue) => issues.push((index, issue)),
        },
    );
    issues.sort_unstable_by_key(|(index, _)| *index);
    issues.into_iter().map(|(_, issue)| issue).collect()
}

fn compute_keep_set(
    repo: &Repository,
    options: &GcOptions<'_>,
    progress: &ProgressHandle,
    limits: GcLimits,
    scope: &'static str,
) -> Result<(HashSet<Oid>, bool, usize)> {
    let (mut keep, shallow) = mark_repository(repo, options.history)
        .map_err(|source| failure(GcFailureKind::KeepSet, source))?;
    if shallow && !options.dry_run && !options.unsafe_override {
        return Err(GcError::ShallowHistory { scope });
    }
    let issues = additional_repositories(options, &mut keep, progress, limits);
    let incomplete = issues.len();
    if incomplete != 0 && !options.dry_run && !options.unsafe_override {
        return Err(GcError::IncompleteKeepSet { scope, issues });
    }
    Ok((keep, shallow, incomplete))
}

fn gc_local(
    repo: &Repository,
    config: &gat_core::config::Config,
    options: &GcOptions<'_>,
    progress: &dyn ProgressReporter,
    limits: GcLimits,
) -> Result<GcReport> {
    let cache_root = repo.resolved_cache_root_from(config);
    let cache = cache_root.maintenance();
    let (keep, shallow, incomplete) = with_progress_typed(
        progress,
        ProgressSpec::indeterminate(ProgressOperation::ComputingReachability),
        |task| compute_keep_set(repo, options, &task.handle(), limits, "local cache"),
    )?;
    let uncertain_keep_set = shallow || incomplete != 0;
    let task = progress.begin(ProgressSpec::items(
        ProgressOperation::GarbageCollecting,
        ProgressUnit::Objects,
        None,
    ));
    let stats = cache
        .sweep(options.dry_run, |oid| -> Result<CacheSweepDecision> {
            task.inc(1);
            if keep.contains(&oid) {
                return Ok(CacheSweepDecision::Keep);
            }
            if uncertain_keep_set && !options.unsafe_override {
                return Ok(CacheSweepDecision::Uncertain);
            }
            Ok(CacheSweepDecision::Delete)
        })
        .map_err(|source| failure(GcFailureKind::Cache, source))??;
    Ok(GcReport {
        deleted: stats.deleted,
        uncertain: stats.uncertain,
        incomplete_repositories: incomplete,
        forced_incomplete_keep_set: options.unsafe_override && incomplete != 0,
    })
}

/// Only unreachable OIDs need buffering. The set also deduplicates backend
/// inventory entries before counting or submitting deletion candidates.
#[derive(Default)]
struct RemoteCandidates {
    oids: HashSet<Oid>,
    listed: usize,
}

impl RemoteCandidates {
    fn record(&mut self, keep: &HashSet<Oid>, oid: Oid) {
        self.listed += 1;
        if !keep.contains(&oid) {
            self.oids.insert(oid);
        }
    }
}

fn collect_remote_candidates(
    keep: &HashSet<Oid>,
    list: impl FnOnce(&mut dyn FnMut(Oid)) -> Result<()>,
) -> Result<RemoteCandidates> {
    let mut candidates = RemoteCandidates::default();
    list(&mut |oid| candidates.record(keep, oid))?;
    // A failed or incomplete listing never exposes candidates to the sweep.
    Ok(candidates)
}

fn visit_remote_oids(
    remote: &gat_io::RemoteClient,
    executor: &crate::remote_executor::RemoteExecutor,
    progress: &ProgressHandle,
    record: &mut dyn FnMut(Oid),
) -> Result<()> {
    let runtime = tokio::runtime::Handle::current();
    if let Some(file_gc) = remote.file_gc() {
        let mut scan = file_gc.listing();
        loop {
            let (returned, batch) = runtime
                .block_on(executor.local_transfer(move || {
                    let batch = scan.next_batch();
                    (scan, batch)
                }))
                .map_err(|source| failure(GcFailureKind::Remote, source))?;
            scan = returned;
            let Some(batch) = batch.map_err(|source| failure(GcFailureKind::Remote, source))?
            else {
                break;
            };
            for oid in batch {
                record(oid);
                progress.inc(1);
            }
        }
    } else {
        let mut lister = runtime
            .block_on(remote.enumerate_objects())
            .map_err(|source| failure(GcFailureKind::Remote, source))?;
        while let Some(entry) = runtime.block_on(lister.next()) {
            record(
                entry
                    .map_err(|source| failure(GcFailureKind::Remote, source))?
                    .oid(),
            );
            progress.inc(1);
        }
    }
    Ok(())
}

fn gc_remote_with_limits(
    repo: &Repository,
    config: &gat_core::config::Config,
    options: &GcOptions<'_>,
    progress: &dyn ProgressReporter,
    limits: GcLimits,
) -> Result<GcReport> {
    if !options.dry_run && !options.unsafe_override {
        return Err(GcError::RemoteDeletionRequiresUnsafe);
    }
    let catalog = RemoteCatalog::from_config(&config.remotes)
        .map_err(|source| failure(GcFailureKind::Repository, source))?;
    let remote_id = catalog
        .resolve(options.remote)
        .map_err(|source| failure(GcFailureKind::Repository, source))?;
    let request_budget = gat_io::RemoteRequestBudget::new(
        limits.remote.physical_requests,
        limits.remote.physical_requests,
    );
    let (keep, shallow, incomplete) = with_progress_typed(
        progress,
        ProgressSpec::indeterminate(ProgressOperation::ComputingReachability),
        |task| compute_keep_set(repo, options, &task.handle(), limits, "remote"),
    )?;
    let listing = progress.begin(ProgressSpec::items(
        ProgressOperation::ListingRemoteObjects,
        ProgressUnit::Objects,
        None,
    ));
    let remote = RemoteSession::with_request_budget(
        request_budget,
        repo.inputs.templates(),
        config.network.resolve(),
    )
    .open(&catalog, remote_id, Some(&listing.handle()))
    .map_err(|source| GcError::RemoteOpen {
        remote_name: catalog.remote_name(remote_id),
        source,
    })?;
    let executor = crate::remote_executor::RemoteExecutor::new(
        crate::limits::ExecutionLimits::default().remote,
    );
    let candidates = collect_remote_candidates(&keep, |record| {
        visit_remote_oids(&remote, &executor, &listing.handle(), record)
    })?;
    listing.finish();
    drop(keep);
    let task = progress.begin(ProgressSpec::items(
        ProgressOperation::GarbageCollecting,
        ProgressUnit::Objects,
        Some(candidates.listed as u64),
    ));
    let uncertain_keep_set = shallow || incomplete != 0;
    let mut uncertain = 0;
    let deleted = if uncertain_keep_set && !options.unsafe_override {
        uncertain = candidates.oids.len();
        task.inc(candidates.listed as u64);
        0
    } else if options.dry_run {
        task.inc(candidates.listed as u64);
        candidates.oids.len()
    } else {
        task.inc((candidates.listed - candidates.oids.len()) as u64);
        delete_remote_candidates(
            &remote,
            &executor,
            candidates.oids,
            &task.handle(),
            NonZeroUsize::new(10_000).unwrap(),
        )?
    };
    Ok(GcReport {
        deleted,
        uncertain,
        incomplete_repositories: incomplete,
        forced_incomplete_keep_set: options.unsafe_override && incomplete != 0,
    })
}

/// Delete candidates from a completed inventory in bounded semantic windows.
fn delete_remote_candidates(
    remote: &gat_io::RemoteClient,
    executor: &crate::remote_executor::RemoteExecutor,
    candidates: impl IntoIterator<Item = Oid>,
    progress: &ProgressHandle,
    window_size: NonZeroUsize,
) -> Result<usize> {
    let runtime = tokio::runtime::Handle::current();
    let file_gc = remote.file_gc();
    let mut deleted = 0;
    let result = (|| {
        let mut window = Vec::with_capacity(window_size.get());
        let mut delete = |window: &[Oid]| {
            progress.set_activity(ProgressActivity::DeletingRemoteObjects);
            if let Some(file_gc) = &file_gc {
                for prepared in file_gc.deletion_batches(window) {
                    let outcome = runtime
                        .block_on(executor.local_transfer(move || prepared.delete()))
                        .map_err(|source| failure(GcFailureKind::Remote, source))?;
                    deleted += outcome.confirmed;
                    progress.inc(outcome.confirmed as u64);
                    if let Some(source) = outcome.error {
                        return Err(failure(GcFailureKind::Remote, source));
                    }
                }
                return Ok(());
            }
            runtime
                .block_on(remote.delete_objects(window, |count| {
                    deleted += count;
                    progress.inc(count as u64);
                }))
                .map_err(|source| failure(GcFailureKind::Remote, source))
        };
        for candidate in candidates {
            window.push(candidate);
            if window.len() == window_size.get() {
                delete(&window)?;
                window.clear();
            }
        }
        if !window.is_empty() {
            delete(&window)?;
        }
        Ok(())
    })();
    result.map_err(|mut error| {
        if let GcError::Failure(failure) = &mut error {
            failure.confirmed_deletions = Some(deleted);
        }
        error
    })?;
    Ok(deleted)
}

impl Repository {
    pub fn garbage_collect(
        &self,
        options: &GcOptions<'_>,
        progress: &dyn ProgressReporter,
    ) -> Result<GcReport> {
        let config = self
            .load_config()
            .map_err(|source| failure(GcFailureKind::Repository, source))?;
        let limits = GcLimits {
            remote: crate::limits::RemoteGcLimits {
                physical_requests: config.network.resolve().request_concurrency.capacity(),
            },
            ..GcLimits::default()
        };
        if options.remote.is_some() {
            gc_remote_with_limits(self, &config, options, progress, limits)
        } else {
            gc_local(self, &config, options, progress, limits)
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn garbage_collect_with_limits(
        &self,
        options: &GcOptions<'_>,
        progress: &dyn ProgressReporter,
        limits: GcLimits,
    ) -> Result<GcReport> {
        let config = self
            .load_config()
            .map_err(|source: crate::RepositoryError| failure(GcFailureKind::Repository, source))?;
        if options.remote.is_some() {
            gc_remote_with_limits(self, &config, options, progress, limits)
        } else {
            gc_local(self, &config, options, progress, limits)
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    use super::{
        GcError, GcFailure, GcFailureKind, GcRepositoryFailureKind, GcRepositoryIssue,
        GitLocationSpec,
    };

    pub fn gc_sweep_failure(
        confirmed: usize,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> GcError {
        let mut failure = GcFailure::new(GcFailureKind::Remote, source);
        failure.confirmed_deletions = Some(confirmed);
        failure.into()
    }

    pub fn repository_issue(
        location: GitLocationSpec,
        kind: GcRepositoryFailureKind,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> GcRepositoryIssue {
        GcRepositoryIssue::new(location, kind, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::endpoint::RemoteUrlTemplate;
    use gat_core::name::RemoteName;
    use gat_core::progress::NoopProgress;
    use std::error::Error;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn oid(value: u64) -> Oid {
        Oid::from_hex(&format!("{value:064x}")).unwrap()
    }

    struct DeletionProgress(AtomicUsize);

    impl gat_core::progress::ActivityBackend for DeletionProgress {
        fn inc(&self, count: u64) {
            self.0
                .fetch_add(usize::try_from(count).unwrap(), Ordering::Relaxed);
        }
        fn set_activity(&self, _: &ProgressActivity) {}
        fn finish(&self) {}
    }

    #[test]
    fn remote_candidates_deduplicate_and_filter_unordered_inventory() {
        let mut keep = HashSet::from([oid(1), oid(5)]);
        merge_keep_set(&mut keep, HashSet::from([oid(3), oid(5)]));
        let candidates = collect_remote_candidates(&keep, |record| {
            for value in [5, 4, 1, 4, 2, 3, 1, 2] {
                record(oid(value));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(candidates.oids, HashSet::from([oid(2), oid(4)]));
        assert_eq!(candidates.listed, 8);
        assert_eq!(keep, HashSet::from([oid(1), oid(3), oid(5)]));
    }

    #[test]
    fn failed_listing_discards_candidates_even_after_a_full_deletion_window() {
        let result = collect_remote_candidates(&HashSet::new(), |record| {
            for value in 0..10_001 {
                record(oid(value));
            }
            Err(failure(
                GcFailureKind::Remote,
                std::io::Error::other("listing failed"),
            ))
        });
        let Err(GcError::Failure(error)) = result else {
            panic!("an incomplete inventory must not yield deletion candidates");
        };
        assert_eq!(error.kind(), GcFailureKind::Remote);
        assert_eq!(error.confirmed_deletions(), None);
    }

    #[test]
    fn empty_and_fully_reachable_inventories_retain_no_candidates() {
        for values in [vec![], vec![1, 3, 1]] {
            let keep = HashSet::from([oid(1), oid(3)]);
            let candidates = collect_remote_candidates(&keep, |record| {
                for value in &values {
                    record(oid(*value));
                }
                Ok(())
            })
            .unwrap();
            assert!(candidates.oids.is_empty());
            assert_eq!(candidates.listed, values.len());
        }
    }

    #[test]
    fn file_gc_groups_listing_and_deletion_into_bounded_workers() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (root, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let remote = handles[0].client();
        for index in 0..257 {
            // Seed the file backend directly: this test measures listing and
            // deletion worker batches, not the remote upload implementation.
            let path = root.path().join(gat_io::object_key_oid(&oid(index)));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"orphan").unwrap();
        }
        let executor = crate::remote_executor::RemoteExecutor::new(
            crate::limits::ExecutionLimits::default().remote,
        );
        let progress = Arc::new(DeletionProgress(AtomicUsize::new(0)));
        let task = gat_core::progress::ProgressTask::from_backend(progress.clone());
        let candidates = collect_remote_candidates(&HashSet::new(), |record| {
            visit_remote_oids(remote, &executor, &task.handle(), record)
        })
        .unwrap();
        assert_eq!(candidates.listed, 257);
        assert_eq!(
            executor.local_submissions(),
            4,
            "three pages plus EOF, not a worker per object"
        );
        assert_eq!(candidates.oids, (0..257).map(oid).collect::<HashSet<_>>());
        assert_eq!(progress.0.load(Ordering::Relaxed), 257);
        progress.0.store(0, Ordering::Relaxed);
        let deleted = delete_remote_candidates(
            remote,
            &executor,
            candidates.oids,
            &task.handle(),
            NonZeroUsize::new(10_000).unwrap(),
        )
        .unwrap();
        assert_eq!(deleted, 257);
        assert_eq!(progress.0.load(Ordering::Relaxed), 257);
        assert_eq!(
            executor.local_submissions(),
            7,
            "three bounded deletion workers"
        );
        let mut scan = remote.file_gc().unwrap().listing();
        while let Some(batch) = scan.next_batch().unwrap() {
            assert!(batch.is_empty());
        }
    }

    #[test]
    fn remote_sweep_deletion_failure_reports_only_confirmed_objects() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_root, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let remote = handles[0].client();
        for index in [1, 3] {
            remote
                .write(&gat_io::object_key_oid(&oid(index)), b"orphan".to_vec())
                .unwrap();
        }
        // A nonempty directory at an object key cannot be removed as a file.
        let child = format!("{}/child", gat_io::object_key_oid(&oid(2)));
        remote.write(&child, b"preserve".to_vec()).unwrap();
        let progress = Arc::new(DeletionProgress(AtomicUsize::new(0)));
        let task = gat_core::progress::ProgressTask::from_backend(progress.clone());
        let error = delete_remote_candidates(
            remote,
            &crate::remote_executor::RemoteExecutor::new(
                crate::limits::ExecutionLimits::default().remote,
            ),
            (1..=3).map(oid),
            &task.handle(),
            NonZeroUsize::new(3).unwrap(),
        )
        .unwrap_err();
        let GcError::Failure(error) = error else {
            panic!("expected remote failure")
        };
        assert_eq!(error.kind(), GcFailureKind::Remote);
        assert_eq!(error.confirmed_deletions(), Some(1));
        assert_eq!(progress.0.load(Ordering::Relaxed), 1);
        assert!(!remote.exists(&gat_io::object_key_oid(&oid(1))).unwrap());
        assert!(remote.exists(&child).unwrap());
        assert!(remote.exists(&gat_io::object_key_oid(&oid(3))).unwrap());
    }

    #[test]
    fn completed_inspection_is_consumed_before_its_batch_peer_finishes() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let ready = std::sync::Barrier::new(2);
        let (consumed, receive) = std::sync::mpsc::sync_channel(1);
        let receive = std::sync::Mutex::new(receive);
        let mut results = Vec::new();
        pool.install(|| {
            inspect_in_bounded_batches(
                &[0, 1],
                NonZeroUsize::new(2).unwrap(),
                |candidate| {
                    ready.wait();
                    if *candidate == 1 {
                        receive.lock().unwrap().recv().unwrap();
                    }
                    *candidate
                },
                |index, result| {
                    if index == 0 {
                        consumed.send(()).unwrap();
                    }
                    results.push(result);
                },
            );
        });
        results.sort_unstable();
        assert_eq!(results, [0, 1]);
    }

    #[test]
    fn remote_repository_batches_bound_concurrent_inspections() {
        let batch_size = NonZeroUsize::new(2).unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let consumed = AtomicUsize::new(0);
        let candidates: Vec<_> = (0..8).collect();
        let mut results = Vec::new();
        inspect_in_bounded_batches(
            &candidates,
            batch_size,
            |candidate| {
                assert!(consumed.load(Ordering::Acquire) >= candidate / 2 * 2);
                let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                peak.fetch_max(current, Ordering::AcqRel);
                std::thread::yield_now();
                active.fetch_sub(1, Ordering::AcqRel);
                candidate * 2
            },
            |index, result| {
                results.push((index, result));
                consumed.fetch_add(1, Ordering::Release);
            },
        );

        results.sort_unstable_by_key(|(index, _)| *index);
        let results: Vec<_> = results.into_iter().map(|(_, result)| result).collect();
        assert_eq!(results.len(), candidates.len());
        assert_eq!(
            results,
            candidates
                .iter()
                .map(|candidate| candidate * 2)
                .collect::<Vec<_>>()
        );
        assert!(peak.load(Ordering::Acquire) <= batch_size.get());
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn local_gc_loads_config_and_resolves_cache_location_once() {
        let test = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(test.path().to_path_buf());
        let selection = HistorySelection::conservative_default();
        let config_before = crate::test_support::config_loads();
        let location_before = crate::test_support::cache_location_resolutions();

        repo.garbage_collect(
            &GcOptions {
                repositories: &[],
                dry_run: true,
                unsafe_override: false,
                remote: None,
                history: Some(&selection),
            },
            &NoopProgress,
        )
        .unwrap();

        assert_eq!(crate::test_support::config_loads(), config_before + 1);
        assert_eq!(
            crate::test_support::cache_location_resolutions(),
            location_before + 1
        );
    }

    #[test]
    fn remote_gc_loads_effective_config_once() {
        let test = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(test.path().to_path_buf());
        let remote_dir = tempfile::tempdir().unwrap();
        let remote_name = RemoteName::from_string("origin".to_string());
        let mut config = repo
            .load_config_scoped(gat_core::config::ConfigScope::Project)
            .unwrap();
        config.remotes.default = Some(remote_name.clone());
        config.remotes.by_name.insert(
            remote_name.clone(),
            RemoteUrlTemplate::from_string(test_support_git::file_remote_url(remote_dir.path()))
                .into(),
        );
        repo.save_config_scoped(&config, gat_core::config::ConfigScope::Project)
            .unwrap();
        let selection = HistorySelection::conservative_default();
        let config_before = crate::test_support::config_loads();
        let opens_before = crate::test_support::remote_opens();
        let named_opens_before = crate::test_support::remote_open_count_for("origin");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let _runtime_guard = runtime.enter();

        repo.garbage_collect(
            &GcOptions {
                repositories: &[],
                dry_run: true,
                unsafe_override: false,
                remote: Some(&remote_name),
                history: Some(&selection),
            },
            &NoopProgress,
        )
        .unwrap();

        assert_eq!(crate::test_support::config_loads(), config_before + 1);
        assert_eq!(crate::test_support::remote_opens(), opens_before + 1);
        assert_eq!(
            crate::test_support::remote_open_count_for("origin"),
            named_opens_before + 1
        );
    }

    #[test]
    fn remote_clone_failure_retains_the_complete_io_source_chain() {
        let missing = tempfile::tempdir().unwrap().path().join("missing.git");
        let location = GitLocationSpec::from_string(gat_io::remote_file_url_for_test(&missing));
        let progress = NoopProgress.begin(ProgressSpec::indeterminate(
            ProgressOperation::ComputingReachability,
        ));

        let Err(issue) = inspect_additional_repository(
            &location,
            &progress.handle(),
            &HistorySelection::conservative_default(),
        ) else {
            panic!("missing remote clone should fail")
        };

        assert_eq!(issue.kind(), GcRepositoryFailureKind::CloneFailed);
        let prepare = issue
            .source()
            .and_then(|source| source.downcast_ref::<gat_io::PrepareBareGitRepositoryError>())
            .expect("prepared-clone error was dropped");
        let gat_io::PrepareBareGitRepositoryError::Clone(clone) = prepare else {
            panic!("expected physical clone failure, got {prepare:?}");
        };
        assert!(
            clone.source().is_some(),
            "physical clone source was dropped"
        );
    }

    #[test]
    fn remote_repository_is_inspected_through_the_prepared_bare_owner() {
        let source = crate::test_harness::test_repo();
        let location =
            GitLocationSpec::from_string(gat_io::remote_file_url_for_test(source.path()));
        let progress = NoopProgress.begin(ProgressSpec::indeterminate(
            ProgressOperation::ComputingReachability,
        ));

        let keep = inspect_additional_repository(
            &location,
            &progress.handle(),
            &HistorySelection::conservative_default(),
        )
        .unwrap();

        assert!(keep.is_empty());
    }

    #[test]
    fn shallow_peer_history_is_an_incomplete_repository() {
        let keep = HashSet::from([oid(1)]);
        let Err(issue) = complete_repository_inspection("peer".into(), Ok((keep, true))) else {
            panic!("shallow history should be incomplete")
        };

        assert_eq!(issue.kind(), GcRepositoryFailureKind::ShallowHistory);
    }
}
