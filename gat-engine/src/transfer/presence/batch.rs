//! Bounded physical grouping underneath per-object semantic admission/results.

use super::PresenceProbeError;
use crate::remote_executor::{LocalTransferError, PresenceLease, RemoteExecutor};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::BoxStream};
use gat_core::oid::Oid;
use std::{collections::VecDeque, sync::Arc};

pub(super) type ProbeResult = (usize, Result<bool, PresenceProbeError>);
pub(crate) type AdmittedPresence = (usize, Oid, PresenceLease);

/// Callers acquire one logical presence lease per entry before grouping. Only
/// metadata capabilities and bounded typed sends run on the admitted worker.
pub fn presence_stream(
    executor: &RemoteExecutor,
    client: gat_io::RemoteClient,
    entries: Vec<AdmittedPresence>,
) -> BoxStream<'_, ProbeResult> {
    assert!(!entries.is_empty() && entries.len() <= client.presence_batch_limit());
    if client.presence_batch_limit() == 1 {
        let (index, oid, lease) = entries.into_iter().next().unwrap();
        return async move {
            let _lease = lease;
            (
                index,
                match executor.cancellable(client.contains_object(&oid)).await {
                    Ok(result) => result.map_err(PresenceProbeError::Remote),
                    Err(()) => Err(PresenceProbeError::Cancelled),
                },
            )
        }
        .into_stream()
        .boxed();
    }
    let indices = entries.iter().map(|entry| entry.0).collect();
    let prepared: Vec<_> = entries
        .into_iter()
        .map(|(_, oid, lease)| {
            (
                client
                    .prepare_file_presence(&oid)
                    .expect("file batch capability"),
                lease,
            )
        })
        .collect();
    file_stream(
        executor,
        indices,
        prepared,
        gat_io::PreparedFilePresence::check,
    )
}

fn file_stream(
    executor: &RemoteExecutor,
    indices: VecDeque<usize>,
    prepared: Vec<(gat_io::PreparedFilePresence, PresenceLease)>,
    mut probe: impl FnMut(gat_io::PreparedFilePresence) -> std::io::Result<bool> + Send + 'static,
) -> BoxStream<'_, ProbeResult> {
    // At most one send per admitted entry: try_send never waits for the reader,
    // including when no results have been consumed yet.
    let (sender, receiver) = tokio::sync::mpsc::channel(prepared.len());
    let (checks, leases): (Vec<_>, Vec<_>) = prepared.into_iter().unzip();
    // Both owners retain admission: dropping the stream cannot release a
    // running worker's leases, and worker completion cannot admit fragmented
    // replacement batches before the final result is delivered.
    let leases = Arc::new(leases);
    let worker_leases = leases.clone();
    let cancellation = executor.cancellation();
    let work = async move {
        executor
            .local_transfer(move || {
                let _leases = worker_leases;
                for check in checks {
                    let result = if cancellation.is_cancelled() {
                        Err(PresenceProbeError::Cancelled)
                    } else {
                        probe(check).map_err(PresenceProbeError::File)
                    };
                    if sender.try_send(result).is_err() {
                        break;
                    }
                }
            })
            .await
    }
    .boxed();
    FileBatch {
        indices,
        receiver,
        work: Some(work),
        failure: None,
        leases: Some(leases),
    }
    .into_stream()
}

enum Failure {
    Cancelled,
    Task(Arc<tokio::task::JoinError>),
}

struct FileBatch<'a> {
    indices: VecDeque<usize>,
    receiver: tokio::sync::mpsc::Receiver<Result<bool, PresenceProbeError>>,
    work: Option<BoxFuture<'a, Result<(), LocalTransferError>>>,
    failure: Option<Failure>,
    leases: Option<Arc<Vec<PresenceLease>>>,
}

impl<'a> FileBatch<'a> {
    fn completed(&mut self, result: Result<(), LocalTransferError>) {
        self.work = None;
        self.failure = match result {
            Ok(()) => None,
            Err(LocalTransferError::Cancelled) => Some(Failure::Cancelled),
            Err(LocalTransferError::Task(source)) => Some(Failure::Task(Arc::new(source))),
        };
    }

    async fn next(&mut self) -> Option<ProbeResult> {
        let index = *self.indices.front()?;
        let message = loop {
            let Some(work) = self.work.as_mut() else {
                break self.receiver.recv().await;
            };
            tokio::select! {
                message = self.receiver.recv() => break message,
                result = work => self.completed(result),
            }
        };
        // Earlier completions may be reported while later metadata is blocked.
        // The final completion, or a closed channel, must drain started work.
        if (message.is_none() || self.indices.len() == 1)
            && let Some(work) = self.work.take()
        {
            self.completed(work.await);
        }
        self.indices.pop_front();
        if self.indices.is_empty() {
            self.leases.take();
        }
        Some((
            index,
            message.unwrap_or_else(|| {
                Err(match &self.failure {
                    Some(Failure::Cancelled) => PresenceProbeError::Cancelled,
                    Some(Failure::Task(source)) => PresenceProbeError::Task(source.clone()),
                    None => PresenceProbeError::Incomplete,
                })
            }),
        ))
    }

    fn into_stream(self) -> BoxStream<'a, ProbeResult> {
        futures::stream::unfold(self, |mut batch| async move {
            batch.next().await.map(|result| (result, batch))
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_session::RemoteHandle;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn prepared(
        executor: &RemoteExecutor,
        handle: &RemoteHandle,
        count: usize,
    ) -> (
        VecDeque<usize>,
        Vec<(gat_io::PreparedFilePresence, PresenceLease)>,
    ) {
        (0..count)
            .map(|index| {
                (
                    index * 3 + 1,
                    (
                        handle
                            .client()
                            .prepare_file_presence(&Oid::from_bytes(
                                [u8::try_from(index).unwrap(); 32],
                            ))
                            .unwrap(),
                        executor.try_presence(handle.id()).unwrap(),
                    ),
                )
            })
            .unzip()
    }

    #[test]
    fn full_and_partial_batches_use_one_worker_and_keep_aligned_results() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::default().remote);
        for count in [1, 7, 128] {
            let entries = (0..count)
                .map(|index| {
                    (
                        index * 3 + 1,
                        Oid::from_bytes([u8::try_from(index).unwrap(); 32]),
                        executor.try_presence(handles[0].id()).unwrap(),
                    )
                })
                .collect();
            let before = executor.local_submissions();
            let results = runtime.block_on(
                presence_stream(&executor, handles[0].client().clone(), entries)
                    .collect::<Vec<_>>(),
            );
            assert_eq!(results.len(), count);
            for (index, (actual, result)) in results.into_iter().enumerate() {
                assert_eq!(actual, index * 3 + 1);
                assert!(!result.unwrap());
            }
            assert_eq!(executor.local_submissions() - before, 1);
            assert!(executor.try_presence(handles[0].id()).is_some());
        }
    }

    #[test]
    fn reports_before_a_later_check_finishes_and_cancellation_skips_unstarted_entries() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::default().remote);
        let (indices, checks) = prepared(&executor, &handles[0], 128);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let (started, running) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let (release, released) = std::sync::mpsc::channel();
        let mut stream = file_stream(&executor, indices, checks, move |check| {
            if observed.fetch_add(1, Ordering::Relaxed) == 1 {
                started.take().unwrap().send(()).unwrap();
                let _ = released.recv();
            }
            check.check()
        });
        runtime.block_on(async {
            assert_eq!(stream.next().await.unwrap().0, 1);
            running.await.unwrap();
            assert!(
                executor.try_presence(handles[0].id()).is_none(),
                "batch owns every logical lease until drained"
            );
            executor.cancellation().cancel();
            assert!(
                futures::poll!(stream.next()).is_pending(),
                "started metadata still drains"
            );
            release.send(()).unwrap();
            let rest = stream.collect::<Vec<_>>().await;
            assert_eq!(rest.len(), 127);
            assert!(!rest[0].1.as_ref().unwrap());
            assert!(
                rest[1..]
                    .iter()
                    .all(|(_, result)| matches!(result, Err(PresenceProbeError::Cancelled)))
            );
        });
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert!(executor.try_presence(handles[0].id()).is_some());
    }

    #[test]
    fn worker_failure_preserves_prior_results_and_marks_remaining_indices() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::default().remote);
        let (indices, checks) = prepared(&executor, &handles[0], 4);
        let mut called = 0;
        let results = runtime.block_on(
            file_stream(&executor, indices, checks, move |check| {
                called += 1;
                assert!(called != 3, "fixture-owned task failure");
                check.check()
            })
            .collect::<Vec<_>>(),
        );
        assert_eq!(
            results.iter().map(|entry| entry.0).collect::<Vec<_>>(),
            [1, 4, 7, 10]
        );
        assert!(
            results[..2]
                .iter()
                .all(|(_, result)| matches!(result, Ok(false)))
        );
        assert!(
            results[2..]
                .iter()
                .all(|(_, result)| matches!(result, Err(PresenceProbeError::Task(_))))
        );
        assert!(executor.try_presence(handles[0].id()).is_some());
    }

    #[test]
    fn cancellation_before_admission_reports_each_entry_without_a_worker() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::default().remote);
        let (indices, checks) = prepared(&executor, &handles[0], 7);
        executor.cancellation().cancel();
        let results = runtime.block_on(
            file_stream(&executor, indices, checks, |_| {
                panic!("cancelled check must not start")
            })
            .collect::<Vec<_>>(),
        );
        assert_eq!(results.len(), 7);
        assert!(
            results
                .iter()
                .all(|(_, result)| matches!(result, Err(PresenceProbeError::Cancelled)))
        );
        assert_eq!(executor.local_submissions(), 0);
    }

    #[test]
    fn two_remotes_share_local_capacity_and_cancel_queued_batches_without_work() {
        for cancel in [false, true] {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let _entered = runtime.enter();
            let (_remote, handles) =
                crate::remote_session::test_support::open_handles_on_current_runtime(&[
                    "first", "second",
                ]);
            let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::default().remote);
            runtime.block_on(async {
                // Occupy the same local admission used by transfers and network
                // cache work. Barriers, not elapsed time, establish saturation.
                let mut active = futures::stream::FuturesUnordered::new();
                let mut releases = Vec::new();
                let mut arrivals = Vec::new();
                for _ in 0..8 {
                    let (release, released) = std::sync::mpsc::channel();
                    let (started, arrived) = tokio::sync::oneshot::channel();
                    releases.push(release);
                    arrivals.push(arrived);
                    active.push(executor.local_transfer(move || {
                        started.send(()).unwrap();
                        let _ = released.recv();
                    }));
                }
                assert!(futures::poll!(active.next()).is_pending());
                for arrived in arrivals {
                    arrived.await.unwrap();
                }
                assert_eq!(executor.local_submissions(), 8);

                let calls = Arc::new(AtomicUsize::new(0));
                let mut batches = futures::stream::SelectAll::new();
                for handle in &handles {
                    let (indices, checks) = prepared(&executor, handle, 128);
                    let calls = calls.clone();
                    batches.push(file_stream(&executor, indices, checks, move |check| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        check.check()
                    }));
                }
                assert!(futures::poll!(batches.next()).is_pending());
                assert_eq!(executor.local_submissions(), 8);
                assert_eq!(calls.load(Ordering::Relaxed), 0);
                for handle in &handles {
                    assert!(executor.try_presence(handle.id()).is_none());
                }
                if cancel {
                    executor.cancellation().cancel();
                    let results = batches.by_ref().collect::<Vec<_>>().await;
                    assert_eq!(results.len(), 256);
                    assert!(
                        results.iter().all(|(_, result)| matches!(
                            result,
                            Err(PresenceProbeError::Cancelled)
                        ))
                    );
                    assert_eq!(executor.local_submissions(), 8);
                    assert_eq!(calls.load(Ordering::Relaxed), 0);
                }
                for release in releases {
                    release.send(()).unwrap();
                }
                while let Some(result) = active.next().await {
                    result.unwrap();
                }
                if !cancel {
                    let results = batches.collect::<Vec<_>>().await;
                    assert_eq!(results.len(), 256);
                    assert!(
                        results
                            .iter()
                            .all(|(_, result)| matches!(result, Ok(false)))
                    );
                    assert_eq!(executor.local_submissions(), 10);
                    assert_eq!(calls.load(Ordering::Relaxed), 256);
                }
                for handle in &handles {
                    // Every logical permit, not just one, is reusable.
                    let _entries = prepared(&executor, handle, 128);
                }
            });
        }
    }
}
