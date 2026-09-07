//! Shared remote-to-cache byte movement; coordinators own failure policy.

use crate::remote_executor::RemoteExecutor;
use gat_core::oid::Oid;
use gat_io::{
    CacheError, CacheIngest, CachePublication, CacheWriter, ExpectedIngest, RemoteClient,
    RemoteError,
};

#[derive(Debug)]
pub(super) enum ReceiveError {
    Cancelled,
    Remote(RemoteError),
    Cache(CacheError),
    HashMismatch { actual: Oid },
    Task(tokio::task::JoinError),
}

impl From<crate::remote_executor::LocalTransferError> for ReceiveError {
    fn from(error: crate::remote_executor::LocalTransferError) -> Self {
        match error {
            crate::remote_executor::LocalTransferError::Cancelled => Self::Cancelled,
            crate::remote_executor::LocalTransferError::Task(source) => Self::Task(source),
        }
    }
}

pub(super) async fn receive(
    executor: &RemoteExecutor,
    client: RemoteClient,
    cache: CacheWriter,
    oid: Oid,
) -> Result<Option<CachePublication>, ReceiveError> {
    if let Some(prepared) = client.prepare_file_read(oid) {
        let cancellation = executor.cancellation();
        let result = executor
            .local_transfer(move || prepared.receive(cache, || cancellation.is_cancelled()))
            .await
            .map_err(ReceiveError::from)?
            .map_err(|error| match error {
                gat_io::FileReceiveError::Cancelled => ReceiveError::Cancelled,
                gat_io::FileReceiveError::Remote(source) => ReceiveError::Remote(source),
                gat_io::FileReceiveError::Cache(source) => ReceiveError::Cache(source),
            })?;
        return received_result(result);
    }
    // Box the network state machine once per object, rather than its overlap
    // future once per chunk. The file fast path needs neither allocation.
    Box::pin(receive_network(executor, client, cache, oid)).await
}

#[allow(
    clippy::large_futures,
    reason = "The entire network state machine is boxed once; inline chunk futures avoid repeated allocations"
)]
async fn receive_network(
    executor: &RemoteExecutor,
    client: RemoteClient,
    cache: CacheWriter,
    oid: Oid,
) -> Result<Option<CachePublication>, ReceiveError> {
    let mut reader = executor
        .cancellable(client.open_read(&oid))
        .await
        .map_err(|()| ReceiveError::Cancelled)?
        .map_err(ReceiveError::Remote)?;
    // Select the tiny path from actual bytes, without metadata discovery.
    let mut tiny = Vec::new();
    let mut ingest = None;
    let mut next = executor
        .cancellable(reader.next())
        .await
        .map_err(|()| ReceiveError::Cancelled)?
        .map_err(ReceiveError::Remote)?;
    while let Some(bytes) = next {
        if ingest.is_none() && tiny.len() + bytes.len() <= gat_io::TRANSFER_CHUNK_SIZE {
            if tiny.is_empty() {
                tiny = bytes;
            } else {
                tiny.extend_from_slice(&bytes);
            }
            next = executor
                .cancellable(reader.next())
                .await
                .map_err(|()| ReceiveError::Cancelled)?
                .map_err(ReceiveError::Remote)?;
            continue;
        }
        let writer = cache.clone();
        let prefix = std::mem::take(&mut tiny);
        let (read, appended) = RemoteExecutor::overlap_transfer(
            async {
                executor
                    .cancellable(reader.next())
                    .await
                    .map_err(|()| ReceiveError::Cancelled)?
                    .map_err(ReceiveError::Remote)
            },
            async {
                executor
                    .local_transfer(move || {
                        let mut ingest = match ingest {
                            Some(ingest) => ingest,
                            None => writer.begin_ingest()?,
                        };
                        ingest.append(&prefix)?;
                        ingest.append(&bytes)?;
                        Ok::<_, CacheError>(ingest)
                    })
                    .await
                    .map_err(ReceiveError::from)?
                    .map_err(ReceiveError::Cache)
            },
        )
        .await?;
        ingest = Some(appended);
        next = read;
    }
    drop(reader);
    publish_received(executor, cache, oid, tiny, ingest).await
}

// Keep the completed body owned through cancellation-aware local admission and
// durability work. No remote response remains live at this boundary.
async fn publish_received(
    executor: &RemoteExecutor,
    cache: CacheWriter,
    oid: Oid,
    tiny: Vec<u8>,
    ingest: Option<CacheIngest>,
) -> Result<Option<CachePublication>, ReceiveError> {
    let result = executor
        .local_transfer(move || {
            match ingest {
                Some(ingest) => ingest.finish(oid),
                None => cache.ingest_expected(oid, std::io::Cursor::new(tiny)),
            }
            .map_err(ReceiveError::Cache)
        })
        .await
        .map_err(ReceiveError::from)??;
    received_result(result)
}

const fn received_result(result: ExpectedIngest) -> Result<Option<CachePublication>, ReceiveError> {
    match result {
        ExpectedIngest::Published { publication } => Ok(publication),
        ExpectedIngest::HashMismatch { actual } => Err(ReceiveError::HashMismatch { actual }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_receive_uses_one_worker_and_a_file_specific_reservation() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let client = handles[0].client().clone();
        let tmp = tempfile::tempdir().unwrap();
        let cache =
            gat_io::RepositoryLayout::at(tmp.path().to_owned()).resolve_cache_root(None, None);
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::tiny().remote);
        assert!(client.download_buffer_bytes() > gat_io::TRANSFER_CHUNK_SIZE);
        assert!(client.download_buffer_bytes() < gat_io::DOWNLOAD_BUFFER_BYTES);
        for size in [
            0,
            7,
            gat_io::TRANSFER_CHUNK_SIZE + 1,
            3 * gat_io::TRANSFER_CHUNK_SIZE + 17,
        ] {
            let bytes = vec![42; size];
            let oid = Oid::from_bytes(*blake3::hash(&bytes).as_bytes());
            client.write(&gat_io::object_key_oid(&oid), bytes).unwrap();
            assert!(
                runtime.block_on(client.open_read(&oid)).is_err(),
                "file reads cannot return to unadmitted backend workers"
            );
            let before = executor.local_submissions();
            let lease = executor
                .try_transfer(handles[0].id(), client.download_buffer_bytes())
                .unwrap();
            runtime
                .block_on(receive(&executor, client.clone(), cache.writer(), oid))
                .unwrap();
            drop(lease);
            assert_eq!(executor.local_submissions() - before, 1);
            assert_eq!(
                cache.open_client().verify(&oid).unwrap(),
                gat_io::ObjectVerification::Valid
            );
        }
    }

    #[test]
    fn cancellation_drains_started_ingest_and_publication_before_releasing_transfer_capacity() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let executor = RemoteExecutor::new(
            crate::limits::ExecutionLimits::for_test(4, 10_000, 4096, 1, 1).remote,
        );
        let tmp = tempfile::tempdir().unwrap();
        let layout = gat_io::RepositoryLayout::at(tmp.path().to_path_buf());
        let cache = layout.resolve_cache_root(None, None);
        let bytes = vec![42; 2 * gat_io::TRANSFER_CHUNK_SIZE];
        let oid = Oid::from_bytes(*blake3::hash(&bytes).as_bytes());
        runtime.block_on(async {
            let lease = executor
                .try_transfer(handles[0].id(), gat_io::DOWNLOAD_BUFFER_BYTES)
                .unwrap();
            let (started, running) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let writer = cache.writer();
            let transfer = async {
                let _lease = lease;
                executor
                    .local_transfer(move || {
                        let mut ingest = writer.begin_ingest().unwrap();
                        ingest
                            .append(&bytes[..gat_io::TRANSFER_CHUNK_SIZE])
                            .unwrap();
                        started.send(()).unwrap();
                        released.recv().unwrap();
                        ingest
                            .append(&bytes[gat_io::TRANSFER_CHUNK_SIZE..])
                            .unwrap();
                        ingest.finish(oid).unwrap()
                    })
                    .await
                    .unwrap()
            };
            tokio::pin!(transfer);
            assert!(futures::poll!(&mut transfer).is_pending());
            running.await.unwrap();
            executor.cancellation().cancel();
            assert!(futures::poll!(&mut transfer).is_pending());
            assert!(
                executor
                    .try_transfer(handles[0].id(), gat_io::DOWNLOAD_BUFFER_BYTES)
                    .is_none()
            );
            release.send(()).unwrap();
            assert!(matches!(transfer.await, ExpectedIngest::Published { .. }));
            assert!(
                executor
                    .try_transfer(handles[0].id(), gat_io::DOWNLOAD_BUFFER_BYTES)
                    .is_some()
            );
        });
        assert_eq!(
            cache.open_client().verify(&oid).unwrap(),
            gat_io::ObjectVerification::Valid
        );
    }

    #[test]
    fn cancelled_publication_discards_tiny_and_streaming_payloads() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            for streaming in [false, true] {
                for queued in [false, true] {
                    let tmp = tempfile::tempdir().unwrap();
                    let layout = gat_io::RepositoryLayout::at(tmp.path().to_path_buf());
                    let cache = layout.resolve_cache_root(None, None);
                    let executor =
                        RemoteExecutor::new(crate::limits::ExecutionLimits::tiny().remote);
                    let bytes = vec![
                        42;
                        if streaming {
                            2 * gat_io::TRANSFER_CHUNK_SIZE
                        } else {
                            17
                        }
                    ];
                    let oid = Oid::from_bytes(*blake3::hash(&bytes).as_bytes());
                    let (tiny, ingest) = if streaming {
                        let mut ingest = cache.writer().begin_ingest().unwrap();
                        ingest.append(&bytes).unwrap();
                        assert_eq!(std::fs::read_dir(cache.display_path()).unwrap().count(), 1);
                        (Vec::new(), Some(ingest))
                    } else {
                        (bytes, None)
                    };
                    let (started, running) = tokio::sync::oneshot::channel();
                    let (release, released) = std::sync::mpsc::channel();
                    let blocker = tokio::task::spawn_blocking(move || {
                        started.send(()).unwrap();
                        released.recv().unwrap();
                    });
                    running.await.unwrap();
                    let publication =
                        publish_received(&executor, cache.writer(), oid, tiny, ingest);
                    tokio::pin!(publication);
                    if queued {
                        assert!(futures::poll!(&mut publication).is_pending());
                    }
                    executor.cancellation().cancel();
                    if queued {
                        // Cancellation must still drain a task that owns the
                        // ingest, even when the blocking pool has not started it.
                        assert!(futures::poll!(&mut publication).is_pending());
                        if streaming {
                            assert_eq!(std::fs::read_dir(cache.display_path()).unwrap().count(), 1);
                        }
                    }
                    release.send(()).unwrap();
                    blocker.await.unwrap();
                    assert!(matches!(publication.await, Err(ReceiveError::Cancelled)));
                    if streaming {
                        assert_eq!(std::fs::read_dir(cache.display_path()).unwrap().count(), 0);
                    } else {
                        assert!(!cache.display_path().exists());
                    }
                    assert_eq!(
                        cache.open_client().verify(&oid).unwrap(),
                        gat_io::ObjectVerification::Missing
                    );
                }
            }
        });
    }

    #[test]
    fn receive_streams_large_file_and_never_publishes_a_mismatch() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let client = handles[0].client().clone();
        let tmp = tempfile::tempdir().unwrap();
        let layout = gat_io::RepositoryLayout::at(tmp.path().to_path_buf());
        let cache = layout.resolve_cache_root(None, None);
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::tiny().remote);
        let bytes = vec![42; 3 * gat_io::TRANSFER_CHUNK_SIZE + 17];
        let oid = Oid::from_bytes(*blake3::hash(&bytes).as_bytes());
        client
            .write(&gat_io::object_key_oid(&oid), bytes.clone())
            .unwrap();
        runtime
            .block_on(receive(&executor, client.clone(), cache.writer(), oid))
            .unwrap();
        let verified = cache.open_client();
        assert_eq!(
            verified.verify(&oid).unwrap(),
            gat_io::ObjectVerification::Valid
        );
        let wrong_oid = Oid::from_bytes(*blake3::hash(b"different").as_bytes());
        client
            .write(&gat_io::object_key_oid(&wrong_oid), bytes)
            .unwrap();
        assert!(
            matches!(runtime.block_on(receive(&executor, client, cache.writer(), wrong_oid)),
            Err(ReceiveError::HashMismatch { actual }) if actual == oid)
        );
        assert_eq!(
            verified.verify(&wrong_oid).unwrap(),
            gat_io::ObjectVerification::Missing
        );
    }

    #[test]
    fn cancellation_before_read_does_not_publish_or_open_the_body() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let tmp = tempfile::tempdir().unwrap();
        let layout = gat_io::RepositoryLayout::at(tmp.path().to_path_buf());
        let cache = layout.resolve_cache_root(None, None);
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::tiny().remote);
        executor.cancellation().cancel();
        // No object exists remotely: opening it would report a remote failure.
        assert!(matches!(
            runtime.block_on(receive(
                &executor,
                handles[0].client().clone(),
                cache.writer(),
                Oid::from_bytes([0; 32])
            )),
            Err(ReceiveError::Cancelled)
        ));
        assert!(!cache.display_path().exists());
    }
}
