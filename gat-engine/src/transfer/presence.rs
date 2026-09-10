#![allow(
    clippy::future_not_send,
    reason = "The coordinator polls these futures locally with block_on; only spawned work requires Send"
)]
use crate::operation::Operation;

use crate::path_policy::{EffectivePathPolicy, ResolvedRemote};
use crate::remote_catalog::{RemoteCatalog, RemoteId};
use crate::remote_executor::RemoteExecutor;
#[cfg(test)]
use crate::remote_executor::RemoteJob;
use crate::remote_session::{RemoteHandle, RemoteSessionError};
#[cfg(test)]
use futures::stream::FuturesUnordered;
use futures::stream::{BoxStream, SelectAll};
use futures::{FutureExt, StreamExt};
use gat_core::lexical_path::GatPath;
use gat_core::name::RouteName;
use gat_core::oid::Oid;
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
#[cfg(test)]
use std::future::Future;
use std::sync::Arc;

mod batch;
pub(crate) use batch::presence_stream;

#[derive(Debug, thiserror::Error)]
pub(crate) enum PresenceProbeError {
    #[error("presence check cancelled")]
    Cancelled,
    #[error("presence task failed")]
    Task(#[source] Arc<tokio::task::JoinError>),
    #[error("presence batch omitted an admitted result")]
    Incomplete,
    #[error("file presence check failed")]
    File(#[source] std::io::Error),
    #[error("remote presence check failed")]
    Remote(#[source] gat_io::RemoteError),
}

/// One bounded-window item whose object presence should be checked against
/// its already-resolved remote.
pub trait RemotePresenceObligation {
    fn oid(&self) -> Oid;
    fn resolved_remote(&self) -> &ResolvedRemote;
    fn representative_path(&self) -> &GatPath;
}

/// One presence result, addressed back into the caller's bounded input
/// window without cloning caller-owned obligation metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemotePresenceResult {
    pub request_index: usize,
    pub present: bool,
}

/// Everything a bounded remote-presence probe can fail with.
#[derive(Debug, thiserror::Error)]
pub enum RemotePresenceError {
    #[error("could not open remote `{remote_name}` for `{path}`")]
    RemoteOpen {
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        #[source]
        source: Box<RemoteSessionError>,
    },
    #[error("could not check remote `{remote_name}` for `{path}`")]
    PresenceCheck {
        remote_name: Arc<str>,
        route_name: Option<RouteName>,
        route: Option<GatPath>,
        path: GatPath,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

impl RemotePresenceError {
    pub(crate) fn remote_open<T: RemotePresenceObligation>(
        catalog: &RemoteCatalog,
        policy: &EffectivePathPolicy,
        obligation: &T,
        source: RemoteSessionError,
    ) -> Self {
        let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, obligation);
        Self::RemoteOpen {
            remote_name,
            route_name,
            route,
            path: obligation.representative_path().clone(),
            source: Box::new(source),
        }
    }

    pub(crate) fn presence_check<T: RemotePresenceObligation>(
        catalog: &RemoteCatalog,
        policy: &EffectivePathPolicy,
        obligation: &T,
        source: Box<dyn Error + Send + Sync>,
    ) -> Self {
        let (remote_name, route_name, route) = diagnostic_remote(catalog, policy, obligation);
        Self::PresenceCheck {
            remote_name,
            route_name,
            route,
            path: obligation.representative_path().clone(),
            source,
        }
    }
}

fn diagnostic_remote<T: RemotePresenceObligation>(
    catalog: &RemoteCatalog,
    policy: &EffectivePathPolicy,
    obligation: &T,
) -> (Arc<str>, Option<RouteName>, Option<GatPath>) {
    let remote = obligation.resolved_remote();
    let remote_name = catalog.name(remote.id());
    let route = remote.route().map(|id| policy.route_descriptor(id));
    (
        remote_name,
        route.map(|descriptor| descriptor.name.clone()),
        route.map(|descriptor| descriptor.path.clone()),
    )
}

/// Bounded, completion-order streaming presence check: calls `on_result` for
/// every obligation as soon as its own presence check completes, instead of
/// forcing the caller to wait for the entire window.
///
/// This never materializes the whole `obligations` window as futures up
/// front. Instead it keeps an explicit pending queue (round-robined across
/// remotes for fairness -- see [`fair_request_order`]) and admits only as
/// many requests as `ExecutionLimits.remote.presence.global`/`per_remote`
/// guidance allows. Ready file checks share bounded worker batches; network
/// checks remain independent futures. The underlying [`RemoteExecutor`]
/// semaphores remain the sole
/// concurrency authority; this admission bound only avoids parking
/// thousands of unnecessary futures in the executor's semaphore wait queues
/// at once.
///
/// Remote handles are still opened eagerly on the coordinator, before any
/// async work begins, exactly like the pre-streaming implementation.
///
/// Errors are not surfaced as soon as the first one completes: once any
/// request fails, no further request is admitted from the pending queue,
/// but every request already admitted is drained to completion first (safe
/// cleanup -- no request is abandoned mid-flight). The single terminal
/// error ultimately returned is always the one with the lowest original
/// `request_index` among every error observed, independent of completion
/// order, so which input is reported on error stays stable regardless of
/// scheduling.
pub(crate) fn check_remote_presence_streaming<T: RemotePresenceObligation>(
    operation: &mut Operation<'_>,
    obligations: &[T],
    on_result: impl FnMut(RemotePresenceResult),
    progress: &gat_core::progress::ProgressHandle,
) -> Result<(), RemotePresenceError> {
    if obligations.is_empty() {
        return Ok(());
    }

    #[cfg(any(test, feature = "test-support"))]
    test_support::record_remote_check(obligations.len());

    let mut by_remote: BTreeMap<RemoteId, Vec<usize>> = BTreeMap::new();
    for (index, obligation) in obligations.iter().enumerate() {
        by_remote
            .entry(obligation.resolved_remote().id())
            .or_default()
            .push(index);
    }

    let mut handles: BTreeMap<RemoteId, RemoteHandle> = BTreeMap::new();
    let services = operation.window_services();
    for obligation in obligations {
        let remote_id = obligation.resolved_remote().id();
        if handles.contains_key(&remote_id) {
            continue;
        }
        let handle = services
            .remotes
            .open_handle(services.remotes_catalog, remote_id, Some(progress))
            .map_err(|source| {
                RemotePresenceError::remote_open(
                    services.remotes_catalog,
                    services.policy,
                    obligation,
                    source,
                )
            })?;
        handles.insert(remote_id, handle);
    }

    let pending = fair_request_order(&by_remote);
    // Admission capacity uses both remote-executor bounds as guidance: never
    // admit more than the global concurrency limit at once, and never admit
    // more than every distinct remote's own per-remote limit could possibly
    // run concurrently (beyond that, extra admitted futures would just sit
    // in the executor's per-remote semaphore wait queue for no benefit).
    let capacity = presence_scheduler_capacity(
        services.limits.remote.presence.global,
        services.limits.remote.presence.per_remote,
        by_remote.len(),
        pending.len(),
    );

    let outcome = tokio::runtime::Handle::current().block_on(drive_presence_groups(
        services.remote_executor,
        &handles,
        obligations,
        pending,
        capacity,
        |handle, entries| {
            presence_stream(services.remote_executor, handle.client().clone(), entries)
        },
        on_result,
    ));

    outcome.map_err(|(request_index, source)| {
        RemotePresenceError::presence_check(
            services.remotes_catalog,
            services.policy,
            &obligations[request_index],
            source,
        )
    })
}

pub(crate) fn presence_scheduler_capacity(
    global: std::num::NonZeroUsize,
    per_remote: std::num::NonZeroUsize,
    remote_count: usize,
    pending_count: usize,
) -> usize {
    global
        .get()
        .min(per_remote.get().saturating_mul(remote_count))
        .min(pending_count)
}

/// Interleaves each remote's request indices round-robin (first index of
/// every remote, then second index of every remote, ...) instead of
/// grouping every one remote's indices before the next remote's, so a
/// scheduler admitting requests in this order never admits one remote's
/// whole backlog ahead of another's.
pub(crate) fn fair_request_order(by_remote: &BTreeMap<RemoteId, Vec<usize>>) -> Vec<usize> {
    let mut columns: Vec<std::slice::Iter<'_, usize>> =
        by_remote.values().map(|indices| indices.iter()).collect();
    let total = columns.iter().map(std::iter::ExactSizeIterator::len).sum();
    let mut ordered = Vec::with_capacity(total);
    let mut advanced = true;
    while advanced {
        advanced = false;
        for column in &mut columns {
            if let Some(&index) = column.next() {
                ordered.push(index);
                advanced = true;
            }
        }
    }
    ordered
}

/// Drives `pending` through `executor`, keeping at most `capacity` requests
/// admitted at once, calling `on_result` for every completed request in
/// completion order. `launch` returns a stream of per-entry outcomes for each
/// admitted group; partial groups dispatch immediately. See
/// [`check_remote_presence_streaming`] for the error and admission-stopping
/// contract.
async fn drive_presence_groups<'a, T, F, E>(
    executor: &'a RemoteExecutor,
    handles: &BTreeMap<RemoteId, RemoteHandle>,
    obligations: &[T],
    pending: Vec<usize>,
    capacity: usize,
    launch: F,
    mut on_result: impl FnMut(RemotePresenceResult),
) -> Result<(), (usize, Box<dyn Error + Send + Sync>)>
where
    T: RemotePresenceObligation,
    F: Fn(&RemoteHandle, Vec<batch::AdmittedPresence>) -> BoxStream<'a, (usize, Result<bool, E>)>,
    E: Error + Send + Sync + 'static,
{
    let mut pending: VecDeque<_> = pending.into();
    let mut active = SelectAll::new();
    let mut active_entries = 0;
    let mut errors: Vec<(usize, Box<dyn Error + Send + Sync>)> = Vec::new();
    while !pending.is_empty() || active_entries > 0 {
        if executor.is_cancelled()
            && let Some(index) = pending.iter().min().copied()
        {
            errors.push((index, Box::new(PresenceProbeError::Cancelled)));
            pending.clear();
        }
        if errors.is_empty() {
            let mut groups = BTreeMap::<RemoteId, Vec<batch::AdmittedPresence>>::new();
            // Preserve fair request order when reserving logical entries. Group
            // only what fits now; no waits to fill a batch, no tasks for pending OIDs.
            for _ in 0..pending.len() {
                if active_entries == capacity {
                    break;
                }
                let index = pending.pop_front().unwrap();
                let id = obligations[index].resolved_remote().id();
                if let Some(lease) = executor.try_presence(id) {
                    active_entries += 1;
                    let entries = groups.entry(id).or_default();
                    entries.push((index, obligations[index].oid(), lease));
                    if entries.len() == handles[&id].client().presence_batch_limit() {
                        active.push(launch(&handles[&id], groups.remove(&id).unwrap()));
                    }
                } else {
                    pending.push_back(index);
                }
            }
            for (id, entries) in groups {
                active.push(launch(&handles[&id], entries));
            }
            // Another window may own all available permits. Await one logical
            // slot instead of spinning when this window has no active work.
            if active_entries == 0
                && let Some(index) = pending.pop_front()
            {
                let id = obligations[index].resolved_remote().id();
                match executor.acquire_presence(id).await {
                    Ok(lease) => {
                        active_entries = 1;
                        active.push(launch(
                            &handles[&id],
                            vec![(index, obligations[index].oid(), lease)],
                        ));
                    }
                    Err(()) => errors.push((index, Box::new(PresenceProbeError::Cancelled))),
                }
            }
        } else {
            pending.clear();
        }
        if active_entries == 0 {
            break;
        }
        let (request_index, result) = active
            .next()
            .await
            .expect("admitted presence produces an outcome");
        let mut completed = Some((request_index, result));
        while let Some((request_index, result)) = completed {
            active_entries -= 1;
            match result {
                Ok(present) => on_result(RemotePresenceResult {
                    request_index,
                    present,
                }),
                Err(source) => errors.push((request_index, Box::new(source))),
            }
            // Drain ready notifications before refilling. A completed batch
            // must not become a run of singleton refills while its results sit
            // in the channel. Never wait for a slow entry to fill a batch.
            completed = active.next().now_or_never().flatten();
        }
    }
    // Batch streams drain their worker before emitting their final result.
    match errors.into_iter().min_by_key(|(index, _)| *index) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
async fn drive_presence_scheduler<T, F, Fut, E>(
    executor: &RemoteExecutor,
    handles: &BTreeMap<RemoteId, RemoteHandle>,
    obligations: &[T],
    pending: Vec<usize>,
    capacity: usize,
    check: F,
    on_result: impl FnMut(RemotePresenceResult),
) -> Result<(), (usize, Box<dyn Error + Send + Sync>)>
where
    T: RemotePresenceObligation,
    F: Fn(&RemoteHandle, &T) -> Fut,
    Fut: Future<Output = Result<bool, E>> + Send,
    E: Error + Send + Sync + 'static,
{
    drive_presence_groups(
        executor,
        handles,
        obligations,
        pending,
        capacity,
        |handle, entries| {
            entries
                .into_iter()
                .map(|(index, _, lease)| {
                    let result = check(handle, &obligations[index]);
                    async move {
                        let _lease = lease;
                        (index, result.await)
                    }
                })
                .collect::<FuturesUnordered<_>>()
                .boxed()
        },
        on_result,
    )
    .await
}

/// One admitted presence request: clones the resolved remote's handle,
/// builds its [`RemoteJob`], and runs it through
/// [`RemoteExecutor::run_presence_one`] -- the exact same per-remote-then-global
/// semaphore acquisition every other executor caller uses, so a streaming
/// scheduler competes for the identical presence budget across windows in
/// the same operation.
#[cfg(test)]
fn presence_job<'a, T, F, Fut, E>(
    executor: &'a RemoteExecutor,
    handles: &BTreeMap<RemoteId, RemoteHandle>,
    obligations: &'a [T],
    check: &'a F,
    index: usize,
) -> impl Future<Output = (usize, Result<bool, E>)> + 'a
where
    T: RemotePresenceObligation,
    F: Fn(&RemoteHandle, &T) -> Fut,
    Fut: Future<Output = Result<bool, E>> + 'a,
{
    let remote_id = obligations[index].resolved_remote().id();
    let job = RemoteJob::new(handles[&remote_id].clone(), index);
    async move {
        let result = executor
            .run_presence_one(&job, |handle, idx| check(handle, &obligations[*idx]))
            .await;
        (job.payload, result)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support {
    use std::cell::RefCell;

    thread_local! {
        static REMOTE_CHECK_WINDOW_SIZES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn record_remote_check(size: usize) {
        REMOTE_CHECK_WINDOW_SIZES.with(|sizes| sizes.borrow_mut().push(size));
    }

    #[must_use]
    pub fn remote_check_window_sizes() -> Vec<usize> {
        REMOTE_CHECK_WINDOW_SIZES.with(|sizes| sizes.borrow().clone())
    }

    pub fn reset_remote_check_window_sizes() {
        REMOTE_CHECK_WINDOW_SIZES.with(|sizes| sizes.borrow_mut().clear());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn production_presence_groups_full_batches_and_dispatches_the_partial_tail() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let executor = executor(256, 128);
        let (_remote, obligations, handles) = single_remote_fixture(257);
        let sizes = std::cell::RefCell::new(Vec::new());
        let mut seen = Vec::new();
        runtime
            .block_on(drive_presence_groups(
                &executor,
                &handles,
                &obligations,
                (0..257).collect(),
                128,
                |handle, entries| {
                    sizes.borrow_mut().push(entries.len());
                    presence_stream(&executor, handle.client().clone(), entries)
                },
                |result| seen.push(result.request_index),
            ))
            .unwrap();
        assert_eq!(*sizes.borrow(), [128, 128, 1]);
        assert_eq!(executor.local_submissions(), 3);
        assert_eq!(seen, (0..257).collect::<Vec<_>>());
    }

    #[test]
    fn production_batch_error_preserves_successes_and_stops_the_pending_tail() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let executor = executor(256, 128);
        let (remote, obligations, handles) = single_remote_fixture(129);
        std::fs::create_dir_all(
            remote
                .path()
                .join(gat_io::object_key_oid(&obligations[3].oid)),
        )
        .unwrap();
        let mut seen = Vec::new();
        let (index, _) = runtime
            .block_on(drive_presence_groups(
                &executor,
                &handles,
                &obligations,
                (0..129).collect(),
                128,
                |handle, entries| presence_stream(&executor, handle.client().clone(), entries),
                |result| seen.push(result.request_index),
            ))
            .unwrap_err();
        assert_eq!(index, 3);
        assert_eq!(seen.len(), 127);
        assert_eq!(&seen[..3], &[0, 1, 2]);
        assert!(!seen.contains(&128));
        assert_eq!(executor.local_submissions(), 1);
    }

    fn executor(global: usize, per_remote: usize) -> RemoteExecutor {
        let concurrency = crate::limits::RemoteConcurrency {
            global: nz(global),
            per_remote: nz(per_remote),
        };
        RemoteExecutor::new(crate::limits::RemoteLimits {
            presence: concurrency,
            transfer: concurrency,
            physical_requests: nz(global),
        })
    }

    /// One fake obligation: a fixed `oid`/`resolved_remote` pair, real
    /// enough (a real, catalog-opened [`RemoteHandle`]'s [`RemoteId`]) to
    /// exercise remote grouping/fairness, without needing a real
    /// `Operation`/`Session` -- `drive_presence_scheduler`/`presence_job`
    /// never touch `Operation`, only `RemoteExecutor` and `RemoteHandle`.
    struct FakeObligation {
        oid: Oid,
        remote: ResolvedRemote,
        path: GatPath,
    }

    impl RemotePresenceObligation for FakeObligation {
        fn oid(&self) -> Oid {
            self.oid
        }
        fn resolved_remote(&self) -> &ResolvedRemote {
            &self.remote
        }
        fn representative_path(&self) -> &GatPath {
            &self.path
        }
    }

    fn obligation(remote_id: RemoteId, tag: u8) -> FakeObligation {
        let mut bytes = [0u8; 32];
        bytes[0] = tag;
        FakeObligation {
            oid: Oid::from_bytes(bytes),
            remote: ResolvedRemote::for_test(remote_id),
            path: GatPath::parse_canonical("f").unwrap(),
        }
    }

    /// Opens `count` real handles all against the same throwaway remote
    /// (mirrors `remote_executor`'s own `remotes(&["a"])` fixture) and
    /// returns one `FakeObligation` per handle plus the `RemoteId ->
    /// RemoteHandle` map `presence_job` expects.
    fn single_remote_fixture(
        count: usize,
    ) -> (
        tempfile::TempDir,
        Vec<FakeObligation>,
        BTreeMap<RemoteId, RemoteHandle>,
    ) {
        let (dir, handles) = crate::remote_session::test_support::open_handles(&["a"]);
        let handle = handles.into_iter().next().unwrap();
        let remote_id = handle.id();
        let obligations = (0..count)
            .map(|i| obligation(remote_id, (i).to_le_bytes()[0]))
            .collect();
        let mut map = BTreeMap::new();
        map.insert(remote_id, handle);
        (dir, obligations, map)
    }

    /// Two remotes, `per_remote` obligations against each, interleaved so
    /// index `2*i` is remote `a` and `2*i + 1` is remote `b`.
    fn two_remote_fixture(
        per_remote: usize,
    ) -> (
        tempfile::TempDir,
        Vec<FakeObligation>,
        BTreeMap<RemoteId, RemoteHandle>,
    ) {
        let (dir, handles) = crate::remote_session::test_support::open_handles(&["a", "b"]);
        let mut map = BTreeMap::new();
        let mut ids = Vec::new();
        for handle in handles {
            ids.push(handle.id());
            map.insert(handle.id(), handle);
        }
        let mut obligations = Vec::new();
        for i in 0..per_remote {
            obligations.push(obligation(ids[0], (i).to_le_bytes()[0]));
            obligations.push(obligation(ids[1], (i).to_le_bytes()[0]));
        }
        (dir, obligations, map)
    }

    fn with_runtime<T>(f: impl FnOnce() -> T) -> T {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        f()
    }

    fn by_remote(obligations: &[FakeObligation]) -> BTreeMap<RemoteId, Vec<usize>> {
        let mut map: BTreeMap<RemoteId, Vec<usize>> = BTreeMap::new();
        for (index, obligation) in obligations.iter().enumerate() {
            map.entry(obligation.resolved_remote().id())
                .or_default()
                .push(index);
        }
        map
    }

    #[test]
    fn fair_request_order_round_robins_instead_of_grouping_by_remote() {
        with_runtime(|| {
            let (_dir, obligations, _handles) = two_remote_fixture(3);
            // Indices 0,2,4 are remote `a`; 1,3,5 are remote `b`. A
            // remote-grouped order would be [0,2,4,1,3,5] (or the reverse);
            // round-robin must interleave them as encountered instead.
            let ordered = fair_request_order(&by_remote(&obligations));
            assert_eq!(ordered, vec![0, 1, 2, 3, 4, 5]);
        });
    }

    #[test]
    fn drive_presence_scheduler_never_admits_more_than_capacity_concurrently() {
        with_runtime(|| {
            let executor = executor(8, 8);
            let (_dir, obligations, handles) = single_remote_fixture(10);
            let pending: Vec<usize> = (0..obligations.len()).collect();
            let capacity = 3;
            let active = Arc::new(AtomicUsize::new(0));
            let max_active = Arc::new(AtomicUsize::new(0));

            let active_check = active;
            let max_active_check = max_active.clone();
            let check = move |_handle: &RemoteHandle, _obligation: &FakeObligation| {
                let active = active_check.clone();
                let max_active = max_active_check.clone();
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok::<bool, std::io::Error>(true)
                }
            };

            let mut seen = 0usize;
            let result = tokio::runtime::Handle::current().block_on(drive_presence_scheduler(
                &executor,
                &handles,
                &obligations,
                pending,
                capacity,
                check,
                |_result| seen += 1,
            ));

            assert!(result.is_ok());
            assert_eq!(seen, obligations.len());
            assert!(
                max_active.load(Ordering::SeqCst) <= capacity,
                "observed {} concurrently active requests, admission capacity was {capacity}",
                max_active.load(Ordering::SeqCst)
            );
        });
    }

    #[test]
    fn drive_presence_scheduler_calls_on_result_in_completion_order_not_input_order() {
        with_runtime(|| {
            let executor = executor(8, 8);
            let (_dir, obligations, handles) = single_remote_fixture(4);
            let total = obligations.len();
            let pending: Vec<usize> = (0..total).collect();
            let releases: Vec<_> = (0..total)
                .map(|_| Arc::new(tokio::sync::Notify::new()))
                .collect();
            let check = |_handle: &RemoteHandle, obligation: &FakeObligation| {
                let release = Arc::clone(&releases[usize::from(obligation.oid.as_bytes()[0])]);
                async move {
                    release.notified().await;
                    Ok::<bool, std::io::Error>(true)
                }
            };

            let order = std::cell::RefCell::new(Vec::new());
            let mut scheduler = std::pin::pin!(drive_presence_scheduler(
                &executor,
                &handles,
                &obligations,
                pending,
                total,
                check,
                |result| order.borrow_mut().push(result.request_index),
            ));

            // Admit all checks while their gates are closed. Then make exactly
            // one result ready per poll, in reverse input order. Manual polling
            // makes a scheduler that buffers results fail without a timeout.
            assert!(scheduler.as_mut().now_or_never().is_none());
            assert!(order.borrow().is_empty());
            for index in (0..total).rev() {
                releases[index].notify_one();
                let result = scheduler.as_mut().now_or_never();
                assert_eq!(
                    *order.borrow(),
                    (index..total).rev().collect::<Vec<_>>(),
                    "on_result must fire in completion order, not input order"
                );
                if index == 0 {
                    assert!(result.expect("all released checks must complete").is_ok());
                } else {
                    assert!(result.is_none(), "unreleased checks must remain pending");
                }
            }
        });
    }

    /// The command layer relies on `on_result` firing as each request
    /// finishes so it can advance progress without waiting for the whole
    /// admitted batch. The slow job cannot finish until the fast job's
    /// callback releases it, making that ordering deterministic.
    #[test]
    fn drive_presence_scheduler_reports_a_fast_completion_immediately_not_after_the_whole_batch() {
        with_runtime(|| {
            let executor = executor(8, 8);
            let (_dir, obligations, handles) = single_remote_fixture(2);
            let pending: Vec<usize> = (0..obligations.len()).collect();
            let release_slow = Arc::new(tokio::sync::Notify::new());
            let release_slow_check = Arc::clone(&release_slow);
            let check = move |_handle: &RemoteHandle, obligation: &FakeObligation| {
                let tag = obligation.oid.as_bytes()[0];
                let release_slow = Arc::clone(&release_slow_check);
                async move {
                    if tag == 1 {
                        release_slow.notified().await;
                    }
                    Ok::<bool, std::io::Error>(true)
                }
            };

            let mut order = Vec::new();
            let result = tokio::runtime::Handle::current().block_on(tokio::time::timeout(
                Duration::from_secs(1),
                drive_presence_scheduler(
                    &executor,
                    &handles,
                    &obligations,
                    pending,
                    2,
                    check,
                    |result| {
                        order.push(result.request_index);
                        if result.request_index == 0 {
                            release_slow.notify_one();
                        }
                    },
                ),
            ));

            assert!(
                result
                    .expect("scheduler deadlocked before reporting the fast job")
                    .is_ok()
            );
            assert_eq!(order, vec![0, 1]);
        });
    }

    #[test]
    fn presence_scheduler_capacity_saturates_instead_of_overflowing() {
        assert_eq!(
            presence_scheduler_capacity(nz(8), NonZeroUsize::MAX, 2, 3),
            3
        );
    }

    #[test]
    fn drive_presence_scheduler_selects_lowest_request_index_error_under_reversed_completion() {
        with_runtime(|| {
            let executor = executor(8, 8);
            let (_dir, obligations, handles) = single_remote_fixture(3);
            let pending: Vec<usize> = (0..obligations.len()).collect();
            // Complete in reverse order; the lowest-index error must still win.
            let (completed, _) = tokio::sync::watch::channel(3);
            let check = move |_handle: &RemoteHandle, obligation: &FakeObligation| {
                let tag = u64::from(obligation.oid.as_bytes()[0]);
                let completed = completed.clone();
                let mut turn = completed.subscribe();
                async move {
                    turn.wait_for(|next| *next == tag + 1).await.unwrap();
                    completed.send_replace(tag);
                    if tag == 0 || tag == 2 {
                        Err(std::io::Error::other(format!("job {tag} failed")))
                    } else {
                        Ok(true)
                    }
                }
            };

            let result = tokio::runtime::Handle::current().block_on(drive_presence_scheduler(
                &executor,
                &handles,
                &obligations,
                pending,
                3,
                check,
                |_result| {},
            ));

            match result {
                Err((request_index, _)) => assert_eq!(request_index, 0),
                Ok(()) => panic!("expected a terminal error"),
            }
        });
    }

    #[test]
    fn drive_presence_scheduler_stops_admitting_new_work_once_an_error_is_known() {
        with_runtime(|| {
            let executor = executor(8, 8);
            let (_dir, obligations, handles) = single_remote_fixture(3);
            let pending: Vec<usize> = (0..obligations.len()).collect();
            let admitted = Arc::new(AtomicUsize::new(0));
            let admitted_check = admitted.clone();
            // Capacity 1: job 0 admitted first and fails immediately, so
            // job 1 must never be admitted at all.
            let check = move |_handle: &RemoteHandle, obligation: &FakeObligation| {
                let admitted = admitted_check.clone();
                let tag = obligation.oid.as_bytes()[0];
                async move {
                    admitted.fetch_add(1, Ordering::SeqCst);
                    if tag == 0 {
                        Err(std::io::Error::other("job 0 failed"))
                    } else {
                        Ok(true)
                    }
                }
            };

            let result = tokio::runtime::Handle::current().block_on(drive_presence_scheduler(
                &executor,
                &handles,
                &obligations,
                pending,
                1,
                check,
                |_result| {},
            ));

            assert!(result.is_err());
            assert_eq!(
                admitted.load(Ordering::SeqCst),
                1,
                "no further request should have been admitted once job 0's error was known"
            );
        });
    }

    #[test]
    fn presence_job_uses_the_check_body_supplied_by_the_caller() {
        with_runtime(|| {
            let executor = executor(4, 4);
            let (_dir, obligations, handles) = single_remote_fixture(1);
            let check = |_handle: &RemoteHandle, _obligation: &FakeObligation| async {
                Ok::<bool, std::io::Error>(true)
            };
            let (index, result) = tokio::runtime::Handle::current().block_on(presence_job(
                &executor,
                &handles,
                &obligations,
                &check,
                0,
            ));
            assert_eq!(index, 0);
            assert!(result.unwrap());
        });
    }
}
