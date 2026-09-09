//! Operation-scoped, Rayon-independent async remote-I/O execution.
//!
//! Remote `exists`/`reader`/`writer` calls are I/O-bound.
//! [`RemoteExecutor`] drives
//! job futures cooperatively on the process-wide Tokio runtime that
//! `main` already enters for the whole CLI invocation (see
//! [`tokio::runtime::Handle::current`]) via [`tokio::runtime::Handle::block_on`],
//! rather than spawning a runtime -- or an OS thread -- per object, remote,
//! or window.
//!
//! Presence has separate semaphore admission. Transfers reserve object slots
//! and payload bytes atomically, with per-remote FIFO queues and rotation across
//! remotes. Completion-driven dispatch retains input-aligned results and drains
//! started work before returning. Local tasks have their own bounded admission.

#![allow(
    clippy::future_not_send,
    reason = "The coordinator polls these futures locally with block_on; only spawned work requires Send"
)]

use super::limits::{RemoteConcurrency, RemoteLimits};
use super::remote_catalog::RemoteId;
use super::remote_session::RemoteHandle;
use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
mod admission;
mod cancellation;
pub use cancellation::TransferCancellation;

#[derive(Debug, thiserror::Error)]
pub(crate) enum LocalTransferError {
    #[error("local transfer cancelled")]
    Cancelled,
    #[error("local transfer task failed")]
    Task(#[source] tokio::task::JoinError),
}

pub(crate) enum RemoteLease {
    Presence {
        _global: tokio::sync::OwnedSemaphorePermit,
        _remote: tokio::sync::OwnedSemaphorePermit,
    },
    Transfer {
        _lease: admission::TransferLease,
    },
}

/// One remote-I/O job bundled with the exact opened [`RemoteHandle`] whose
/// scheduling budget it must acquire and whose operator it must use. Carrying
/// the handle itself means the
/// dispatcher and the job body always agree on identity/operator by
/// construction.
pub(crate) struct RemoteJob<T> {
    pub(crate) handle: RemoteHandle,
    pub(crate) payload: T,
}

impl<T> RemoteJob<T> {
    pub(crate) const fn new(handle: RemoteHandle, payload: T) -> Self {
        Self { handle, payload }
    }
}

/// Fair, bounded async dispatcher for one operation's remote-I/O jobs.
/// Distinct from the transfer-planning window
/// ([`super::limits::TransferLimits::window`]): this bounds
/// *concurrent remote-I/O execution*, not planning memory.
///
/// Both the global and per-remote concurrency budgets are session-owned
/// state: built once here and reused by every
/// [`Self::run_window`] call across the whole operation, rather than a
/// fresh [`Semaphore`] per call -- so each
/// `ExecutionLimits.remote.{presence,transfer}` budget bounds the aggregate
/// operation across every window instead of independently resetting each
/// time a coordinator dispatches another window's jobs.
pub(crate) struct RemoteExecutor {
    cancellation: TransferCancellation,
    presence: RemoteBudget,
    transfer: Arc<admission::Admission>,
    local: Arc<Semaphore>,
    #[cfg(test)]
    local_submissions: std::sync::atomic::AtomicUsize,
    request_budget: gat_io::RemoteRequestBudget,
}

struct RemoteBudget {
    per_remote_limit: NonZeroUsize,
    global: Arc<Semaphore>,
    per_remote: Mutex<HashMap<RemoteId, Arc<Semaphore>>>,
}

impl RemoteBudget {
    /// `global_limit` bounds how many jobs run concurrently across every
    /// remote combined; `per_remote_limit` additionally bounds how many jobs
    /// run concurrently against any single remote name. Both are
    /// `NonZeroUsize`: a zero concurrency bound is
    /// unrepresentable rather than silently coerced up to `1` by a
    /// fallback -- `ExecutionLimits::remote` already only ever produces
    /// `NonZeroUsize` values, so there is no legitimate caller for a
    /// `usize` that could be zero here.
    fn new(limits: RemoteConcurrency) -> Self {
        Self {
            per_remote_limit: limits.per_remote,
            global: Arc::new(Semaphore::new(limits.global.get())),
            per_remote: Mutex::new(HashMap::new()),
        }
    }

    /// The session-persistent per-remote [`Semaphore`] for `id`,
    /// built the first time this remote identity is seen and reused for
    /// every later job against it -- see the struct doc for why this must
    /// outlive any single [`Self::run_window`] call.
    fn per_remote_semaphore(&self, id: RemoteId) -> Arc<Semaphore> {
        let mut map = self.per_remote.lock().unwrap();
        Arc::clone(
            map.entry(id)
                .or_insert_with(|| Arc::new(Semaphore::new(self.per_remote_limit.get()))),
        )
    }
}

impl RemoteExecutor {
    /// Overlap one remote operation with one owned local task. Local failure
    /// drops the remote wait; remote failure must still drain the local task.
    pub(crate) async fn overlap_transfer<R, L, E>(
        remote: impl Future<Output = Result<R, E>>,
        local: impl Future<Output = Result<L, E>>,
    ) -> Result<(R, L), E> {
        tokio::pin!(remote, local);
        tokio::select! {
            biased;
            result = &mut remote => {
                let local = local.await;
                Ok((result?, local?))
            }
            result = &mut local => {
                let local = result?;
                Ok((remote.await?, local))
            }
        }
    }

    /// Largest complete payload reservation accepted by transfer admission.
    pub(crate) const fn transfer_buffer_limit() -> usize {
        admission::REMOTE_BYTES
    }

    pub(crate) fn cancellation(&self) -> TransferCancellation {
        self.cancellation.clone()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(crate) async fn cancellable<F: Future>(&self, future: F) -> Result<F::Output, ()> {
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(()),
            result = future => Ok(result),
        }
    }
    pub(crate) fn try_presence(&self, id: RemoteId) -> Option<RemoteLease> {
        let remote = self
            .presence
            .per_remote_semaphore(id)
            .try_acquire_owned()
            .ok()?;
        let global = self.presence.global.clone().try_acquire_owned().ok()?;
        Some(RemoteLease::Presence {
            _global: global,
            _remote: remote,
        })
    }

    pub(crate) async fn acquire_presence(&self, id: RemoteId) -> Result<RemoteLease, ()> {
        let remote = self
            .cancellable(self.presence.per_remote_semaphore(id).acquire_owned())
            .await?
            .expect("presence budget remains open");
        let global = self
            .cancellable(self.presence.global.clone().acquire_owned())
            .await?
            .expect("presence budget remains open");
        Ok(RemoteLease::Presence {
            _global: global,
            _remote: remote,
        })
    }

    pub(crate) fn try_transfer(&self, id: RemoteId, bytes: usize) -> Option<RemoteLease> {
        self.transfer
            .try_acquire(id, bytes)
            .map(|lease| RemoteLease::Transfer { _lease: lease })
    }

    pub(crate) fn forget_transfer_waiter(&self, id: RemoteId) {
        self.transfer.forget_waiter(id);
    }
    pub(crate) fn new(limits: RemoteLimits) -> Self {
        let operation_limit = std::cmp::max(limits.presence.global, limits.transfer.global);
        Self {
            cancellation: TransferCancellation::default(),
            presence: RemoteBudget::new(limits.presence),
            transfer: admission::Admission::new(limits.transfer),
            local: Arc::new(Semaphore::new(8)),
            #[cfg(test)]
            local_submissions: std::sync::atomic::AtomicUsize::new(0),
            request_budget: gat_io::RemoteRequestBudget::new(
                operation_limit,
                limits.physical_requests,
            ),
        }
    }

    pub(crate) fn request_budget(&self) -> gat_io::RemoteRequestBudget {
        self.request_budget.clone()
    }

    /// Runs one bounded, best-effort window, returning results in input order.
    /// Only admitted jobs become futures; remote queues remain ordinary indices.
    /// The synchronous coordinator drives async work on the existing runtime.
    #[cfg(test)]
    pub(crate) fn run_transfer_window<T, F, Fut, R>(&self, jobs: &[RemoteJob<T>], job: F) -> Vec<R>
    where
        F: Fn(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = R>,
    {
        if jobs.is_empty() {
            return Vec::new();
        }
        tokio::runtime::Handle::current().block_on(self.run_transfer_window_async(jobs, job))
    }

    #[cfg(test)]
    async fn run_transfer_window_async<T, F, Fut, R>(&self, jobs: &[RemoteJob<T>], job: F) -> Vec<R>
    where
        F: Fn(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = R>,
    {
        self.run_transfer_window_until(jobs, job, |_| false, None)
            .await
            .into_iter()
            .map(|result| result.expect("all best-effort jobs complete"))
            .collect()
    }

    pub(crate) fn run_repair_window<T, F, Fut, R, E>(
        &self,
        jobs: &[RemoteJob<T>],
        job: F,
        cancelled: impl Fn() -> E,
    ) -> Vec<Result<R, E>>
    where
        F: Fn(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = Result<R, E>>,
    {
        tokio::runtime::Handle::current()
            .block_on(self.run_transfer_window_until(
                jobs,
                job,
                |_| false,
                Some(&|| Err(cancelled())),
            ))
            .into_iter()
            .map(|result| result.expect("every repair job completes or is cancelled"))
            .collect()
    }

    pub(crate) fn run_download_window<T, F, Fut, R, E>(
        &self,
        jobs: &[RemoteJob<T>],
        job: F,
        cancelled: impl Fn() -> E,
    ) -> Vec<Option<Result<R, E>>>
    where
        F: Fn(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = Result<R, E>>,
    {
        tokio::runtime::Handle::current().block_on(self.run_transfer_window_until(
            jobs,
            job,
            Result::is_err,
            Some(&|| Err(cancelled())),
        ))
    }

    async fn run_transfer_window_until<T, F, Fut, R>(
        &self,
        jobs: &[RemoteJob<T>],
        job: F,
        failed: impl Fn(&R) -> bool,
        cancelled: Option<&dyn Fn() -> R>,
    ) -> Vec<Option<R>>
    where
        F: Fn(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = R>,
    {
        use futures::{StreamExt, stream::FuturesUnordered};
        use std::collections::VecDeque;
        let mut queues = Vec::<(RemoteId, VecDeque<usize>)>::new();
        for (index, job) in jobs.iter().enumerate() {
            if let Some((_, queue)) = queues.iter_mut().find(|(id, _)| *id == job.handle.id()) {
                queue.push_back(index);
            } else {
                queues.push((job.handle.id(), VecDeque::from([index])));
            }
        }
        let mut active = FuturesUnordered::new();
        let mut results: Vec<Option<R>> = (0..jobs.len()).map(|_| None).collect();
        let mut frontier = jobs.len();
        loop {
            let changed = self.transfer.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_cancelled()
                && let Some(cancelled) = cancelled
            {
                for (id, queue) in &mut queues {
                    for index in queue.drain(..) {
                        results[index] = Some(cancelled());
                    }
                    self.transfer.forget_waiter(*id);
                }
            }
            loop {
                let mut admitted = false;
                for (id, queue) in &mut queues {
                    let Some(&index) = queue.front() else {
                        continue;
                    };
                    if index >= frontier {
                        queue.clear();
                        self.transfer.forget_waiter(*id);
                        continue;
                    }
                    let Some(lease) = self
                        .transfer
                        .try_acquire(*id, jobs[index].handle.client().download_buffer_bytes())
                    else {
                        continue;
                    };
                    queue.pop_front();
                    let future = job(&jobs[index].handle, &jobs[index].payload);
                    active.push(async move {
                        let _lease = lease;
                        (index, future.await)
                    });
                    admitted = true;
                }
                if !admitted {
                    break;
                }
            }
            let completed = tokio::select! {
                biased;
                () = self.cancellation.cancelled(), if cancelled.is_some() && !self.is_cancelled() => continue,
                completed = active.next() => completed,
            };
            if let Some((index, result)) = completed {
                if failed(&result) {
                    frontier = frontier.min(index);
                }
                results[index] = Some(result);
            } else if queues.iter().all(|(_, queue)| queue.is_empty()) {
                break;
            } else {
                // Another coordinator can own capacity from this operation.
                tokio::select! {
                    () = changed => {},
                    () = self.cancellation.cancelled(), if cancelled.is_some() => {},
                }
            }
        }
        results
    }

    #[cfg(test)]
    pub(crate) async fn run_presence_one<T, F, Fut, R>(&self, job: &RemoteJob<T>, f: F) -> R
    where
        F: FnOnce(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = R>,
    {
        self.run_with_budget(&self.presence, job, f).await
    }

    #[cfg(test)]
    pub(crate) async fn run_transfer_one<T, F, Fut, R>(&self, job: &RemoteJob<T>, f: F) -> R
    where
        F: FnOnce(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = R>,
    {
        self.run_transfer_buffered(job, 0, f).await
    }

    #[cfg(test)]
    pub(crate) async fn run_transfer_buffered<T, F, Fut, R>(
        &self,
        job: &RemoteJob<T>,
        bytes: usize,
        f: F,
    ) -> R
    where
        F: FnOnce(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = R>,
    {
        let _lease = self.transfer.acquire(job.handle.id(), bytes).await;
        f(&job.handle, &job.payload).await
    }

    /// Cancellation can discard queued work, but never detaches a started task.
    pub(crate) async fn local_transfer<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, LocalTransferError> {
        let permit = self
            .cancellable(self.local.clone().acquire_owned())
            .await
            .map_err(|()| LocalTransferError::Cancelled)?
            .expect("local pool remains open");
        let cancellation = self.cancellation();
        #[cfg(test)]
        self.local_submissions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // The blocking pool can queue an admitted task before executing it.
            if cancellation.is_cancelled() {
                return Err(LocalTransferError::Cancelled);
            }
            Ok(work())
        })
        .await
        .map_err(LocalTransferError::Task)?
    }

    #[cfg(test)]
    pub(crate) fn local_submissions(&self) -> usize {
        self.local_submissions
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Verification work is drained even after cancellation. The permit belongs
    /// to the task, even if its awaiting future is dropped.
    pub(crate) async fn local<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, tokio::task::JoinError> {
        let permit = self
            .local
            .clone()
            .acquire_owned()
            .await
            .expect("local pool remains open");
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
    }

    /// Runs a single remote-I/O job, acquiring `job.handle`'s scheduling
    /// budget (per-remote permit, then global permit -- see the struct doc
    /// for why that order matters) for the duration of `f`'s future.
    ///
    /// This is the primitive [`Self::run_window_async`] itself is built on:
    /// both share the exact same session-persistent
    /// [`Semaphore`]s, so a caller driving jobs one at a time through
    /// `run_one` (e.g. a streaming scheduler admitting jobs incrementally)
    /// competes for the identical global/per-remote budget as every
    /// `run_window`/`run_window_async` caller in the same operation --
    /// there is no separate, independently-sized budget for either path.
    #[cfg(test)]
    async fn run_with_budget<T, F, Fut, R>(
        &self,
        budget: &RemoteBudget,
        job: &RemoteJob<T>,
        f: F,
    ) -> R
    where
        F: FnOnce(&RemoteHandle, &T) -> Fut,
        Fut: Future<Output = R>,
    {
        let global = Arc::clone(&budget.global);
        let per_remote = budget.per_remote_semaphore(job.handle.id());
        // Acquire the per-remote permit first: a job blocked here never
        // holds a global permit, so a saturated remote's backlog cannot
        // starve a different remote's global-permit acquisition.
        let _per_remote_permit = per_remote.acquire().await.expect("semaphore not closed");
        let _global_permit = global.acquire().await.expect("semaphore not closed");
        f(&job.handle, &job.payload).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Test-only shorthand for a nonzero concurrency bound literal --
    /// every call site below passes a small, obviously-nonzero constant.
    fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    fn executor(global: usize, per_remote: usize) -> RemoteExecutor {
        executor_with_limits(global, per_remote, global, per_remote)
    }

    fn executor_with_limits(
        presence_global: usize,
        presence_per_remote: usize,
        transfer_global: usize,
        transfer_per_remote: usize,
    ) -> RemoteExecutor {
        RemoteExecutor::new(RemoteLimits {
            presence: RemoteConcurrency {
                global: nz(presence_global),
                per_remote: nz(presence_per_remote),
            },
            transfer: RemoteConcurrency {
                global: nz(transfer_global),
                per_remote: nz(transfer_per_remote),
            },
            physical_requests: nz(transfer_global),
        })
    }

    /// Builds one real, catalog-backed [`RemoteHandle`] per entry in
    /// `names` (opened through [`crate::remote_session`]'s own
    /// `open_handle`, never a hand-built struct literal) bundled with its
    /// index as payload, keeping the backing tempdir alive for the
    /// caller's whole test.
    fn remotes(names: &[&str]) -> (tempfile::TempDir, Vec<RemoteJob<usize>>) {
        let (dir, handles) = crate::remote_session::test_support::open_handles(names);
        let jobs = handles
            .into_iter()
            .enumerate()
            .map(|(i, handle)| RemoteJob::new(handle, i))
            .collect();
        (dir, jobs)
    }

    fn with_runtime<T>(f: impl FnOnce() -> T) -> T {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        f()
    }

    #[test]
    fn local_failure_stops_pending_network_without_waiting_for_it() {
        with_runtime(|| {
            tokio::runtime::Handle::current().block_on(async {
                let result = RemoteExecutor::overlap_transfer(
                    std::future::pending::<Result<(), &str>>(),
                    async { Err::<(), _>("local failure") },
                );
                tokio::pin!(result);
                assert_eq!(
                    futures::poll!(&mut result),
                    std::task::Poll::Ready(Err("local failure"))
                );
            });
        });
    }

    #[test]
    fn network_failure_drains_local_work_and_preserves_the_primary_error() {
        with_runtime(|| {
            tokio::runtime::Handle::current().block_on(async {
                let (release, released) = tokio::sync::oneshot::channel();
                let result = RemoteExecutor::overlap_transfer(
                    async { Err::<(), _>("network failure") },
                    async {
                        released.await.unwrap();
                        Err::<(), _>("local failure")
                    },
                );
                tokio::pin!(result);
                assert!(futures::poll!(&mut result).is_pending());
                release.send(()).unwrap();
                assert_eq!(result.await, Err("network failure"));
            });
        });
    }

    #[test]
    fn completed_lookahead_does_not_complete_a_pending_network_operation() {
        with_runtime(|| {
            tokio::runtime::Handle::current().block_on(async {
                let (release, released) = tokio::sync::oneshot::channel();
                let result = RemoteExecutor::overlap_transfer(
                    async {
                        released.await.unwrap();
                        Ok::<_, &str>(42)
                    },
                    async { Ok::<_, &str>(7) },
                );
                tokio::pin!(result);
                assert!(futures::poll!(&mut result).is_pending());
                release.send(()).unwrap();
                assert_eq!(result.await, Ok((42, 7)));
            });
        });
    }

    #[test]
    fn streaming_steps_apply_backpressure_without_parking_local_workers_on_network() {
        with_runtime(|| {
            tokio::runtime::Handle::current().block_on(async {
                for slow_local in [false, true] {
                    let executor = executor(1, 1);
                    let network_calls = AtomicUsize::new(0);
                    let local_calls = Arc::new(AtomicUsize::new(0));
                    let (remote_release, remote_released) = tokio::sync::oneshot::channel();
                    let (local_release, local_released) = std::sync::mpsc::channel();
                    let (started, running) = tokio::sync::oneshot::channel();
                    let (finished, mut finished_local) = tokio::sync::oneshot::channel();
                    let mut remote_released = Some(remote_released);
                    let mut local_released = Some(local_released);
                    let mut started = Some(started);
                    let mut finished = Some(finished);
                    let pipeline = async {
                        for step in 0..2 {
                            let local_calls = Arc::clone(&local_calls);
                            let released = local_released.take();
                            let started = started.take();
                            RemoteExecutor::overlap_transfer(
                                async {
                                    network_calls.fetch_add(1, Ordering::SeqCst);
                                    if step == 0 && !slow_local {
                                        remote_released.take().unwrap().await.unwrap();
                                    }
                                    Ok::<_, ()>(())
                                },
                                async {
                                    executor
                                        .local_transfer(move || {
                                            local_calls.fetch_add(1, Ordering::SeqCst);
                                            if let Some(started) = started {
                                                started.send(()).unwrap();
                                                if slow_local {
                                                    released.unwrap().recv().unwrap();
                                                }
                                            }
                                        })
                                        .await
                                        .unwrap();
                                    if let Some(finished) = finished.take() {
                                        finished.send(()).unwrap();
                                    }
                                    Ok::<_, ()>(())
                                },
                            )
                            .await
                            .unwrap();
                        }
                    };
                    tokio::pin!(pipeline);
                    assert!(futures::poll!(&mut pipeline).is_pending());
                    running.await.unwrap();
                    if !slow_local {
                        // Drive the real blocking-task join to completion while
                        // the network operation remains explicitly held.
                        tokio::select! {
                            () = &mut pipeline => panic!("network gate must stop the pipeline"),
                            result = &mut finished_local => result.unwrap(),
                        }
                    }
                    assert!(futures::poll!(&mut pipeline).is_pending());
                    assert_eq!(network_calls.load(Ordering::SeqCst), 1);
                    assert_eq!(local_calls.load(Ordering::SeqCst), 1);
                    assert_eq!(
                        executor.local.available_permits(),
                        if slow_local { 7 } else { 8 }
                    );
                    let _ = local_release.send(());
                    let _ = remote_release.send(());
                    pipeline.await;
                    assert_eq!(network_calls.load(Ordering::SeqCst), 2);
                    assert_eq!(local_calls.load(Ordering::SeqCst), 2);
                    assert_eq!(executor.local.available_permits(), 8);
                }
            });
        });
    }

    #[test]
    fn cancellation_wakes_capacity_waiters_without_starting_queued_jobs() {
        with_runtime(|| {
            let executor = executor(1, 1);
            let (_dir, jobs) = remotes(&["a"]);
            let held = executor.try_transfer(jobs[0].handle.id(), 1).unwrap();
            tokio::runtime::Handle::current().block_on(async {
                let cancelled = || Err::<(), _>("cancelled");
                let window = executor.run_transfer_window_until(
                    &jobs,
                    |_, _| async { panic!("queued work must not start") },
                    Result::is_err,
                    Some(&cancelled),
                );
                tokio::pin!(window);
                assert!(futures::poll!(&mut window).is_pending());
                executor.cancellation().cancel();
                assert_eq!(window.await, vec![Some(Err("cancelled"))]);
            });
            drop(held);
        });
    }

    #[test]
    fn local_admission_is_shared_and_bounds_started_workers() {
        use futures::StreamExt;

        with_runtime(|| {
            let executor = executor(256, 128);
            tokio::runtime::Handle::current().block_on(async {
                let mut active = futures::stream::FuturesUnordered::new();
                let mut releases = Vec::new();
                let mut arrivals = Vec::new();
                for _ in 0..8 {
                    let worker = &executor;
                    let (release, released) = std::sync::mpsc::channel();
                    let (started, arrived) = tokio::sync::oneshot::channel();
                    releases.push(release);
                    arrivals.push(arrived);
                    active.push(async move {
                        worker
                            .local_transfer(move || {
                                started.send(()).unwrap();
                                released.recv().unwrap();
                            })
                            .await
                            .unwrap();
                    });
                }
                assert!(futures::poll!(active.next()).is_pending());
                for arrived in arrivals {
                    arrived.await.unwrap();
                }
                assert_eq!(executor.local.available_permits(), 0);
                let queued = executor.local_transfer(|| 9);
                tokio::pin!(queued);
                assert!(futures::poll!(&mut queued).is_pending());
                for release in releases {
                    release.send(()).unwrap();
                }
                while active.next().await.is_some() {}
                assert_eq!(queued.await.unwrap(), 9);
                assert_eq!(executor.local.available_permits(), 8);
            });
        });
    }

    #[test]
    fn cancellation_drains_active_jobs_and_marks_pending_results_in_order() {
        with_runtime(|| {
            let executor = executor(1, 1);
            let (_dir, jobs) = remotes(&["a", "b"]);
            tokio::runtime::Handle::current().block_on(async {
                let (release, released) = tokio::sync::oneshot::channel();
                let released = Mutex::new(Some(released));
                let cancelled = || Err("cancelled");
                let window = executor.run_transfer_window_until(
                    &jobs,
                    |_, index| {
                        assert_eq!(*index, 0, "no admission after cancellation");
                        let released = released.lock().unwrap().take().unwrap();
                        async {
                            executor.cancellation().cancel();
                            released.await.unwrap();
                            Ok(0)
                        }
                    },
                    Result::is_err,
                    Some(&cancelled),
                );
                tokio::pin!(window);
                assert!(futures::poll!(&mut window).is_pending());
                assert!(executor.is_cancelled());
                assert!(executor.try_transfer(jobs[0].handle.id(), 1).is_none());
                release.send(()).unwrap();
                assert_eq!(window.await, vec![Some(Ok(0)), Some(Err("cancelled"))]);
                assert!(executor.try_transfer(jobs[0].handle.id(), 1).is_some());
            });
        });
    }

    #[test]
    fn failure_frontier_drains_earlier_work_without_admitting_later_jobs() {
        with_runtime(|| {
            let executor = executor(2, 1);
            let (_dir, jobs) = remotes(&["a", "b", "c"]);
            tokio::runtime::Handle::current().block_on(async {
                let (release, released) = tokio::sync::oneshot::channel();
                let released = Mutex::new(Some(released));
                let window = executor.run_transfer_window_until(
                    &jobs,
                    |_, index| {
                        assert!(*index < 2, "jobs above the error frontier stay queued");
                        let gate = (*index == 0).then(|| released.lock().unwrap().take().unwrap());
                        let index = *index;
                        async move {
                            if let Some(gate) = gate {
                                gate.await.unwrap();
                            }
                            Err::<(), _>(index)
                        }
                    },
                    Result::is_err,
                    None,
                );
                tokio::pin!(window);
                assert!(futures::poll!(&mut window).is_pending());
                release.send(()).unwrap();
                assert_eq!(window.await, vec![Some(Err(0)), Some(Err(1)), None]);
            });
        });
    }

    #[test]
    fn cancelled_local_transfer_does_not_wait_for_capacity_or_run_work() {
        with_runtime(|| {
            let executor = executor(1, 1);
            tokio::runtime::Handle::current().block_on(async {
                let held = executor.local.clone().acquire_many_owned(8).await.unwrap();
                let task = executor.local_transfer(|| panic!("cancelled local work must not run"));
                tokio::pin!(task);
                assert!(futures::poll!(&mut task).is_pending());
                executor.cancellation().cancel();
                assert!(matches!(task.await, Err(LocalTransferError::Cancelled)));
                assert_eq!(executor.local.available_permits(), 0);
                drop(held);
                assert_eq!(executor.local.available_permits(), 8);
            });
        });
    }

    #[test]
    fn cancellation_skips_transfer_work_queued_inside_the_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let executor = executor(1, 1);
            let (started, running) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                released.recv().unwrap();
            });
            running.await.unwrap();
            let task = executor.local_transfer(|| panic!("queued payload work must not run"));
            tokio::pin!(task);
            assert!(futures::poll!(&mut task).is_pending());
            assert_eq!(executor.local.available_permits(), 7);
            executor.cancellation().cancel();
            release.send(()).unwrap();
            blocker.await.unwrap();
            assert!(matches!(task.await, Err(LocalTransferError::Cancelled)));
            assert_eq!(executor.local.available_permits(), 8);
        });
    }

    #[test]
    fn cooperative_cancellation_keeps_local_work_owned_until_completion() {
        with_runtime(|| {
            let executor = executor(1, 1);
            tokio::runtime::Handle::current().block_on(async {
                let (started, running) = tokio::sync::oneshot::channel();
                let (release, released) = std::sync::mpsc::channel();
                let task = executor.local_transfer(move || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                    42
                });
                tokio::pin!(task);
                assert!(futures::poll!(&mut task).is_pending());
                running.await.unwrap();
                executor.cancellation().cancel();
                assert!(futures::poll!(&mut task).is_pending());
                assert_eq!(executor.local.available_permits(), 7);
                release.send(()).unwrap();
                assert_eq!(task.await.unwrap(), 42);
                assert_eq!(executor.local.available_permits(), 8);
            });
        });
    }

    /// Structural test: a dispatched job's scheduling
    /// identity can never diverge from the operator its body actually
    /// calls, because both come from the exact same `RemoteHandle` --
    /// `RemoteJob<T>` has only `handle`/`payload` fields (no separate
    /// `remote: String`/id), and `run_window_async` derives both the
    /// per-remote semaphore key (`j.handle.id()`) and the value passed to
    /// the job body (`&j.handle`) from that one field. Build jobs against
    /// two distinct remotes and assert every job body observes the same
    /// `RemoteHandle::id()` (and therefore the same operator) as the
    /// handle stored in that job at construction time -- input, dispatch,
    /// and scheduling identity can never disagree.
    #[test]
    fn run_window_job_body_always_observes_its_own_jobs_handle_identity() {
        with_runtime(|| {
            let executor = executor(4, 4);
            let (_dir, jobs) = remotes(&["a", "b", "a", "b"]);
            let expected_ids: Vec<RemoteId> = jobs.iter().map(|j| j.handle.id()).collect();
            let results = executor.run_transfer_window(&jobs, |handle, i| {
                let observed = handle.id();
                let i = *i;
                async move { (i, observed) }
            });
            for (i, observed) in results {
                assert_eq!(
                    observed, expected_ids[i],
                    "job {i}'s body observed a different RemoteHandle identity than the \
                     handle it was constructed with"
                );
            }
        });
    }

    #[test]
    fn run_ordered_returns_results_in_original_index_order() {
        with_runtime(|| {
            let executor = executor(4, 4);
            let (_dir, remote_of) = remotes(&["a", "b", "a", "b", "a"]);
            let results = executor.run_transfer_window(&remote_of, |_handle, i| {
                let i = *i;
                async move { i * 10 }
            });
            assert_eq!(results, vec![0, 10, 20, 30, 40]);
        });
    }

    /// Deliberately make later-indexed jobs finish
    /// *before* earlier-indexed ones (index 0 sleeps longest, the last
    /// index doesn't sleep at all) and assert the returned result order
    /// still follows semantic input order, not completion order.
    #[test]
    fn run_ordered_preserves_input_order_under_reversed_completion_order() {
        with_runtime(|| {
            let executor = executor(8, 8);
            let (_dir, remote_of) = remotes(&["a", "b", "c", "d", "e"]);
            let total = remote_of.len();
            let results = executor.run_transfer_window(&remote_of, move |_handle, i| {
                let i = *i;
                async move {
                    // Reversed: index 0 sleeps the longest, so completion
                    // order is exactly the reverse of input order.
                    let delay_ms = (total - i) as u64 * 5;
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    i
                }
            });
            assert_eq!(
                results,
                vec![0, 1, 2, 3, 4],
                "result order must follow input index order, not completion order"
            );
        });
    }

    /// Same reversed-completion property as above, but for a run whose
    /// first job is also the one that fails: the first surfaced error
    /// must be the first *input-order* error even though it's the last
    /// job to actually complete.
    #[test]
    fn run_ordered_first_error_follows_input_order_under_reversed_completion() {
        with_runtime(|| {
            let executor = executor(8, 8);
            let (_dir, remote_of) = remotes(&["a", "b", "c"]);
            let results: Vec<Result<usize, String>> =
                executor.run_transfer_window(&remote_of, |_handle, i| {
                    let i = *i;
                    async move {
                        // Index 0 sleeps longest (finishes last) but must
                        // still be reported first if `results` is consumed
                        // in input order.
                        let delay_ms = (3 - i) as u64 * 5;
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        if i == 0 {
                            Err(format!("job {i} failed"))
                        } else {
                            Ok(i)
                        }
                    }
                });
            let first_err = results.iter().find_map(|r| r.as_ref().err());
            assert_eq!(
                first_err,
                Some(&"job 0 failed".to_string()),
                "first surfaced error must be the first input-order error, not completion order"
            );
        });
    }

    #[test]
    fn run_ordered_never_exceeds_global_concurrency() {
        with_runtime(|| {
            let executor = executor(3, 100);
            let (_dir, remote_of) = remotes(&["a", "b", "c", "d", "e", "f", "g", "h"]);
            let current = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let _ = executor.run_transfer_window(&remote_of, |_handle, _| {
                let current = Arc::clone(&current);
                let peak = Arc::clone(&peak);
                async move {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // sleep-ok: elapsed job duration is the actual property
                    // under test here (observed peak concurrency over
                    // overlapping "durations"), not a synchronization delay.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                }
            });
            assert!(
                peak.load(Ordering::SeqCst) <= 3,
                "observed concurrency {} exceeded the configured global limit",
                peak.load(Ordering::SeqCst)
            );
        });
    }

    #[test]
    fn run_ordered_never_exceeds_per_remote_concurrency() {
        with_runtime(|| {
            let executor = executor(8, 2);
            // Eight jobs all targeting the same remote: per-remote limit (2)
            // must still cap concurrency even though the global limit (8)
            // would otherwise allow all eight to run at once.
            let (_dir, remote_of) = remotes(&["only"; 8]);
            let current = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let _ = executor.run_transfer_window(&remote_of, |_handle, _| {
                let current = Arc::clone(&current);
                let peak = Arc::clone(&peak);
                async move {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // sleep-ok: see above.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                }
            });
            assert!(
                peak.load(Ordering::SeqCst) <= 2,
                "observed per-remote concurrency {} exceeded the configured limit",
                peak.load(Ordering::SeqCst)
            );
        });
    }

    /// A slow remote and a fast remote must progress concurrently within
    /// the configured budgets. A remote with many slow jobs must not prevent a
    /// different remote's jobs from completing promptly.
    #[test]
    fn slow_remote_does_not_starve_a_fast_remote() {
        with_runtime(|| {
            let executor = executor(2, 1);
            let names = ["slow", "slow", "slow", "slow", "slow", "slow", "fast"];
            let (_dir, handles) = crate::remote_session::test_support::open_handles(&names);
            let jobs: Vec<RemoteJob<&str>> = handles
                .into_iter()
                .zip(names)
                .map(|(handle, name)| RemoteJob::new(handle, name))
                .collect();
            let completion_order: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let _ = executor.run_transfer_window(&jobs, |_: &RemoteHandle, remote| {
                let completion_order = Arc::clone(&completion_order);
                let remote = remote.to_string();
                async move {
                    if remote == "slow" {
                        // sleep-ok: elapsed job duration is the actual
                        // property under test here (a slow remote's jobs
                        // must not starve a fast remote's), not a
                        // synchronization delay.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    completion_order.lock().unwrap().push(remote);
                }
            });
            let order = completion_order.lock().unwrap();
            let fast_pos = order.iter().position(|r| r == "fast").unwrap();
            assert!(
                fast_pos < order.len() - 1,
                "the fast remote's single job must not be stuck behind every slow-remote job: \
                 completion order was {order:?}"
            );
        });
    }

    #[test]
    fn run_window_with_no_jobs_returns_empty() {
        with_runtime(|| {
            let executor = executor(4, 4);
            let results: Vec<i32> =
                executor.run_transfer_window(&[] as &[RemoteJob<usize>], |_handle, _| async { 0 });
            assert!(results.is_empty());
        });
    }

    /// A blocking, synchronous byte-copy body
    /// dispatched through an owned `spawn_blocking` task (as every real
    /// fetch/push/repair job body is) must genuinely
    /// overlap with the other jobs in its window rather than serializing
    /// behind them, proving `run_window`'s `join_all` dispatch doesn't
    /// accidentally let one job's blocking work stall the whole window on
    /// the executing Tokio worker thread.
    #[test]
    fn run_window_overlaps_blocking_byte_copy_bodies_within_one_window() {
        with_runtime(|| {
            let executor = executor(8, 8);
            let (_dir, remote_of) = remotes(&["a", "b", "c", "d"]);
            let current = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let _: Vec<()> = executor.run_transfer_window(&remote_of, |_handle, _| {
                let current = Arc::clone(&current);
                let peak = Arc::clone(&peak);
                async move {
                    tokio::task::spawn_blocking(move || {
                        let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        // sleep-ok: genuinely blocking (not async) sleep,
                        // simulating a synchronous byte-copy body so
                        // several overlap on the blocking thread pool.
                        std::thread::sleep(Duration::from_millis(20));
                        current.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .unwrap();
                }
            });
            assert!(
                peak.load(Ordering::SeqCst) > 1,
                "blocking byte-copy bodies in the same window must overlap on the \
                 blocking thread pool, not serialize one job at a time; observed peak \
                 concurrent blocking work was {}",
                peak.load(Ordering::SeqCst)
            );
        });
    }

    /// A transfer window much larger than the
    /// configured remote concurrency must still dispatch every job (no
    /// silent truncation/hidden second batching bound) while never letting
    /// observed concurrency exceed the small configured limit -- proving
    /// `run_window`'s "dispatch the whole window via `join_all`, gate only
    /// via the semaphores" design scales the window dimension independently
    /// of the concurrency dimension.
    #[test]
    fn run_window_much_larger_than_remote_concurrency_dispatches_every_job_within_the_limit() {
        with_runtime(|| {
            let executor = executor(3, 3);
            let names: Vec<&str> = (0..64).map(|_| "only").collect();
            let (_dir, remote_of) = remotes(&names);
            let current = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let results = executor.run_transfer_window(&remote_of, |_handle, i| {
                let i = *i;
                let current = Arc::clone(&current);
                let peak = Arc::clone(&peak);
                async move {
                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // sleep-ok: see other concurrency tests above.
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    current.fetch_sub(1, Ordering::SeqCst);
                    i
                }
            });
            assert_eq!(
                results,
                (0..64).collect::<Vec<_>>(),
                "every job in a window far larger than the concurrency limit must still \
                 be dispatched and returned in input order"
            );
            assert!(
                peak.load(Ordering::SeqCst) <= 3,
                "observed concurrency {} exceeded the configured limit even for a window \
                 much larger than that limit",
                peak.load(Ordering::SeqCst)
            );
        });
    }

    /// The global concurrency budget must
    /// be owned by the session (`RemoteExecutor` instance) and persist
    /// across every call, not be reconstructed fresh inside each
    /// `run_window` invocation. Two concurrent top-level calls sharing one
    /// executor must never together exceed the configured global limit --
    /// if semaphores were rebuilt per call (the pre-fix behavior) each
    /// call would get its own full budget and this assertion would fail.
    #[test]
    fn run_window_shares_global_budget_across_two_concurrent_calls() {
        let executor = Arc::new(executor(2, 100));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let spawn_call = |input: (tempfile::TempDir, Vec<RemoteJob<usize>>)| {
            let executor = Arc::clone(&executor);
            let concurrent = Arc::clone(&concurrent);
            let max_seen = Arc::clone(&max_seen);
            std::thread::spawn(move || {
                let (_dir, names) = input;
                let rt = tokio::runtime::Runtime::new().unwrap();
                let _guard = rt.enter();
                let _: Vec<()> = executor.run_transfer_window(&names, |_handle, _| {
                    let concurrent = Arc::clone(&concurrent);
                    let max_seen = Arc::clone(&max_seen);
                    async move {
                        let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        concurrent.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            })
        };

        let t1 = spawn_call(remotes(&["a", "b", "c", "d"]));
        let t2 = spawn_call(remotes(&["e", "f", "g", "h"]));
        t1.join().unwrap();
        t2.join().unwrap();

        assert!(
            max_seen.load(Ordering::SeqCst) <= 2,
            "global budget of 2 must bound both concurrent executor calls combined, saw {}",
            max_seen.load(Ordering::SeqCst)
        );
    }

    /// Drives one `RemoteExecutor` through
    /// many sequential bounded windows -- standing in for the many
    /// windows a single long-running operation dispatches one after
    /// another -- and proves, across the *whole* sequence rather than
    /// just one call:
    ///
    /// - remote concurrency never exceeds the session's configured global
    ///   limit in any window, including under reversed completion order;
    /// - each window's result order still follows semantic input order
    ///   despite later-indexed jobs finishing before earlier ones;
    /// - each `run_window` call's returned `Vec` is exactly that window's
    ///   own job count -- nothing accumulates across windows, i.e. there
    ///   is no operation-sized completion/reorder collection sitting
    ///   underneath the already-bounded per-window calls.
    #[test]
    fn run_window_across_many_sequential_windows_stays_bounded_ordered_and_within_limits() {
        with_runtime(|| {
            let executor = executor(3, 100);
            let current = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));

            // Sequential windows in one session reuse the same remote identities.
            let (_dir, remote_of) = remotes(&["a", "b", "c", "d", "e", "f", "g", "h"]);
            for window_index in 0..20 {
                let total = remote_of.len();
                let current = Arc::clone(&current);
                let peak = Arc::clone(&peak);
                let results = executor.run_transfer_window(&remote_of, move |_handle, i| {
                    let i = *i;
                    let current = Arc::clone(&current);
                    let peak = Arc::clone(&peak);
                    async move {
                        let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        // Reversed completion order within this window:
                        // index 0 sleeps longest, the last index doesn't
                        // sleep at all.
                        let delay_ms = (total - i) as u64 * 3;
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        current.fetch_sub(1, Ordering::SeqCst);
                        i
                    }
                });
                assert_eq!(
                    results,
                    (0..total).collect::<Vec<_>>(),
                    "window {window_index}'s result order must follow input order, \
                     not completion order"
                );
                assert_eq!(
                    results.len(),
                    total,
                    "window {window_index}'s returned results must be exactly that \
                     window's own job count, not accumulated across earlier windows"
                );
            }

            assert!(
                peak.load(Ordering::SeqCst) <= 3,
                "observed concurrency {} exceeded the configured global limit across \
                 the whole multi-window sequence",
                peak.load(Ordering::SeqCst)
            );
        });
    }

    /// `run_window` (via `run_window_async`) and standalone `run_one` calls
    /// on the same executor must draw from the exact same global/per-remote
    /// semaphore state, not two independently sized budgets. Run one
    /// `run_window` call concurrently (on its own OS thread/runtime) with
    /// several bare `run_one` calls against the same executor and assert
    /// the *combined* observed concurrency never exceeds the configured
    /// global limit.
    #[test]
    fn run_window_and_run_one_share_the_same_semaphore_state() {
        let executor = Arc::new(executor(3, 100));
        let (_dir, window_jobs) = remotes(&["a", "b", "c", "d", "e"]);
        let (_dir2, one_jobs) = remotes(&["f", "g", "h"]);
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let body = |current: Arc<AtomicUsize>, peak: Arc<AtomicUsize>| async move {
            let now = current.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            // sleep-ok: elapsed job duration is the actual property
            // under test (combined observed peak concurrency across
            // both entry points), not a synchronization delay.
            tokio::time::sleep(Duration::from_millis(20)).await;
            current.fetch_sub(1, Ordering::SeqCst);
        };

        let window_thread = {
            let executor = Arc::clone(&executor);
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let _guard = rt.enter();
                let _: Vec<()> = executor.run_transfer_window(&window_jobs, |_handle, _| {
                    body(Arc::clone(&current), Arc::clone(&peak))
                });
            })
        };

        let one_threads: Vec<_> = one_jobs
            .into_iter()
            .map(|job| {
                let executor = Arc::clone(&executor);
                let current = Arc::clone(&current);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(executor.run_transfer_one(&job, |_handle, _| body(current, peak)));
                })
            })
            .collect();

        window_thread.join().unwrap();
        for handle in one_threads {
            handle.join().unwrap();
        }

        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "combined observed concurrency {} across run_window and run_one exceeded \
             the shared global limit -- run_one must draw from the same semaphore \
             state as run_window, not an independent budget",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn run_one_does_not_construct_the_job_future_before_acquiring_permits() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (_dir, mut jobs) = remotes(&["only", "only"]);
        let second = jobs.pop().unwrap();
        let first = jobs.pop().unwrap();
        rt.block_on(async {
            let executor = Arc::new(executor(1, 1));
            let first_started = Arc::new(tokio::sync::Notify::new());
            let release_first = Arc::new(tokio::sync::Notify::new());

            let first_task = {
                let executor = Arc::clone(&executor);
                let first_started = Arc::clone(&first_started);
                let release_first = Arc::clone(&release_first);
                tokio::spawn(async move {
                    executor
                        .run_transfer_one(&first, move |_handle, _| async move {
                            first_started.notify_one();
                            release_first.notified().await;
                        })
                        .await;
                })
            };
            first_started.notified().await;

            let constructed = Arc::new(AtomicUsize::new(0));
            let second_future = {
                let constructed = Arc::clone(&constructed);
                executor.run_transfer_one(&second, move |_handle, _| {
                    constructed.fetch_add(1, Ordering::SeqCst);
                    async {}
                })
            };
            tokio::pin!(second_future);

            assert!(futures::poll!(&mut second_future).is_pending());
            assert_eq!(
                constructed.load(Ordering::SeqCst),
                0,
                "the job closure must not run before its concurrency permits are acquired"
            );

            release_first.notify_one();
            first_task.await.unwrap();
            second_future.await;
            assert_eq!(constructed.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn cancelling_run_one_releases_its_permits() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (_dir, mut jobs) = remotes(&["only", "only"]);
        let replacement = jobs.pop().unwrap();
        let cancelled = jobs.pop().unwrap();
        rt.block_on(async {
            let executor = Arc::new(executor(1, 1));
            let started = Arc::new(tokio::sync::Notify::new());

            let task = {
                let executor = Arc::clone(&executor);
                let started = Arc::clone(&started);
                tokio::spawn(async move {
                    executor
                        .run_transfer_one(&cancelled, move |_handle, _| async move {
                            started.notify_one();
                            std::future::pending::<()>().await;
                        })
                        .await;
                })
            };
            started.notified().await;
            task.abort();
            let _ = task.await;

            tokio::time::timeout(
                Duration::from_secs(1),
                executor.run_transfer_one(&replacement, |_handle, _| async {}),
            )
            .await
            .expect("a cancelled job must release both semaphore permits");
        });
    }

    /// Several concurrent bare `run_one` calls (each on its own OS
    /// thread/runtime) against the same remote must still be capped by
    /// that remote's per-remote budget, exactly like `run_window` jobs
    /// targeting the same remote are.
    #[test]
    fn concurrent_run_one_calls_never_exceed_per_remote_concurrency() {
        let executor = Arc::new(executor(8, 2));
        let (_dir, jobs) = remotes(&["only"; 8]);
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let threads: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let executor = Arc::clone(&executor);
                let current = Arc::clone(&current);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(executor.run_transfer_one(&job, |_handle, _| async move {
                        let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        // sleep-ok: see other concurrency tests above.
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        current.fetch_sub(1, Ordering::SeqCst);
                    }));
                })
            })
            .collect();

        for handle in threads {
            handle.join().unwrap();
        }

        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "observed per-remote concurrency {} across concurrent run_one calls \
             exceeded the configured limit",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn presence_and_transfer_use_independent_budgets() {
        with_runtime(|| {
            let one = RemoteConcurrency {
                global: nz(1),
                per_remote: nz(1),
            };
            let executor = RemoteExecutor::new(RemoteLimits {
                presence: one,
                transfer: one,
                physical_requests: nz(1),
            });
            let (_dir, mut jobs) = remotes(&["only", "only"]);
            let transfer = jobs.pop().unwrap();
            let presence = jobs.pop().unwrap();
            let barrier = Arc::new(tokio::sync::Barrier::new(3));

            tokio::runtime::Handle::current().block_on(async {
                let presence_barrier = Arc::clone(&barrier);
                let presence = executor.run_presence_one(&presence, move |_handle, _| async move {
                    presence_barrier.wait().await;
                });
                let transfer_barrier = Arc::clone(&barrier);
                let transfer = executor.run_transfer_one(&transfer, move |_handle, _| async move {
                    transfer_barrier.wait().await;
                });
                let both_started = barrier.wait();

                tokio::time::timeout(Duration::from_secs(1), async {
                    tokio::join!(presence, transfer, both_started)
                })
                .await
                .expect("presence and transfer must overlap under independent budgets");
            });
        });
    }

    #[test]
    fn unequal_presence_and_transfer_limits_are_enforced_independently() {
        with_runtime(|| {
            let executor = executor_with_limits(2, 2, 1, 1);
            let (_dir, jobs) = remotes(&["only"; 4]);
            let presence_current = Arc::new(AtomicUsize::new(0));
            let presence_peak = Arc::new(AtomicUsize::new(0));
            let transfer_current = Arc::new(AtomicUsize::new(0));
            let transfer_peak = Arc::new(AtomicUsize::new(0));

            tokio::runtime::Handle::current().block_on(async {
                let presence = jobs[..2].iter().map(|job| {
                    let current = Arc::clone(&presence_current);
                    let peak = Arc::clone(&presence_peak);
                    executor.run_presence_one(job, move |_handle, _| async move {
                        let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        current.fetch_sub(1, Ordering::SeqCst);
                    })
                });
                let transfer = jobs[2..].iter().map(|job| {
                    let current = Arc::clone(&transfer_current);
                    let peak = Arc::clone(&transfer_peak);
                    executor.run_transfer_one(job, move |_handle, _| async move {
                        let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        current.fetch_sub(1, Ordering::SeqCst);
                    })
                });
                futures::future::join(
                    futures::future::join_all(presence),
                    futures::future::join_all(transfer),
                )
                .await;
            });

            assert_eq!(presence_peak.load(Ordering::SeqCst), 2);
            assert_eq!(transfer_peak.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn cancelling_presence_work_releases_presence_permits() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (_dir, mut jobs) = remotes(&["only", "only"]);
        let replacement = jobs.pop().unwrap();
        let cancelled = jobs.pop().unwrap();
        rt.block_on(async {
            let executor = Arc::new(executor_with_limits(1, 1, 4, 4));
            let started = Arc::new(tokio::sync::Notify::new());
            let task = {
                let executor = Arc::clone(&executor);
                let started = Arc::clone(&started);
                tokio::spawn(async move {
                    executor
                        .run_presence_one(&cancelled, move |_handle, _| async move {
                            started.notify_one();
                            std::future::pending::<()>().await;
                        })
                        .await;
                })
            };
            started.notified().await;
            task.abort();
            let _ = task.await;

            tokio::time::timeout(
                Duration::from_secs(1),
                executor.run_presence_one(&replacement, |_handle, _| async {}),
            )
            .await
            .expect("cancelled presence work must release both presence permits");
        });
    }
}
