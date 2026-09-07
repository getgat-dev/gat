//! Async object bodies and their conservative payload-buffer requirements.

use super::{RemoteClient, RemoteError, classify_opendal_error, tuning};
use bytes::Buf;
use futures::StreamExt;
use gat_core::oid::Oid;

pub const TRANSFER_CHUNK_SIZE: usize = 1024 * 1024;
pub const DOWNLOAD_BUFFER_BYTES: usize = 4 * TRANSFER_CHUNK_SIZE;

// Locked OpenDAL 0.58.2 MultipartWriter retains completed task inputs (including
// payloads) until collection, with this prefetch ceiling in ConcurrentTasks.
const COMPLETED_PART_PREFETCH: usize = 8192;

/// Backend options are opaque; the coordinator only sees retained payload cost.
#[derive(Clone)]
pub struct PreparedRemoteWrite {
    options: opendal::options::WriteOptions,
    buffer_bytes: usize,
    whole: bool,
    size: u64,
}

impl PreparedRemoteWrite {
    #[must_use]
    pub const fn buffer_bytes(&self) -> usize {
        self.buffer_bytes
    }
    #[must_use]
    pub const fn whole_object(&self) -> bool {
        self.whole
    }
}

pub struct RemoteRead {
    stream: opendal::BufferStream,
    pending: opendal::Buffer,
}

impl RemoteRead {
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, RemoteError> {
        let mut bytes = Vec::new();
        while bytes.len() < TRANSFER_CHUNK_SIZE {
            if self.pending.is_empty() {
                let Some(buffer) = self.stream.next().await else {
                    break;
                };
                let buffer = buffer.map_err(classify_opendal_error)?;
                if buffer.len() > TRANSFER_CHUNK_SIZE {
                    return Err(buffer_limit(buffer.len(), TRANSFER_CHUNK_SIZE));
                }
                self.pending = buffer;
                if self.pending.is_empty() {
                    continue;
                }
            }
            // Defer allocation until data arrives: EOF needs no output buffer.
            // Copy initialized segments directly instead of zero-filling first.
            if bytes.capacity() == 0 {
                bytes.reserve_exact(TRANSFER_CHUNK_SIZE);
            }
            let length = self
                .pending
                .chunk()
                .len()
                .min(TRANSFER_CHUNK_SIZE - bytes.len());
            bytes.extend_from_slice(&self.pending.chunk()[..length]);
            self.pending.advance(length);
            // Advancing OpenDAL's segmented buffer to zero leaves its backing
            // parts owned. Release them before yielding or awaiting more data.
            if self.pending.is_empty() {
                self.pending = opendal::Buffer::new();
            }
        }
        retain_download_remainder(&mut self.pending);
        Ok((!bytes.is_empty()).then_some(bytes))
    }
}

// A transport slice can hide a much larger allocation. Only its unconsumed
// tail crosses a Gat suspension boundary, so detach that tail before yielding.
// Fully consumed frames are dropped without this additional copy.
fn retain_download_remainder(pending: &mut opendal::Buffer) {
    if !pending.is_empty() {
        *pending = pending.to_vec().into();
    }
}

pub struct AsyncRemoteWriter {
    inner: opendal::Writer,
    conditional: bool,
    remaining: u64,
}

impl AsyncRemoteWriter {
    /// Accept one bounded local chunk, including its retained allocation.
    pub async fn write(&mut self, bytes: Vec<u8>) -> Result<(), RemoteError> {
        check_upload_buffer(bytes.capacity())?;
        // The multipart envelope uses the prepared part count. A source that
        // grows after verification must not create unreserved retained parts.
        if bytes.len() as u64 > self.remaining {
            return Err(buffer_limit(
                bytes.len(),
                usize::try_from(self.remaining).unwrap_or(usize::MAX),
            ));
        }
        self.remaining -= bytes.len() as u64;
        self.inner
            .write(bytes)
            .await
            .map_err(classify_opendal_error)
    }

    pub async fn close(&mut self) -> Result<(), RemoteError> {
        match self.inner.close().await {
            Ok(_) => Ok(()),
            // Another publisher won. The object is available, but our multipart
            // upload still belongs to us until cleanup has completed.
            Err(error) if self.conditional && is_create_conflict(&error) => self.abort().await,
            Err(error) => Err(classify_opendal_error(error)),
        }
    }

    pub async fn abort(&mut self) -> Result<(), RemoteError> {
        abort_with_timeout(self.inner.abort()).await
    }
}

/// Bound the whole cleanup attempt, including shared request admission and
/// backend retries. Ordinary transfer cancellation does not interrupt cleanup.
async fn abort_with_timeout(
    abort: impl std::future::Future<Output = opendal::Result<()>>,
) -> Result<(), RemoteError> {
    tokio::time::timeout(super::REMOTE_IO_TIMEOUT, abort)
        .await
        .map_err(|_| RemoteError::CleanupTimedOut)?
        .map_err(classify_opendal_error)
}

fn is_create_conflict(error: &opendal::Error) -> bool {
    matches!(
        error.kind(),
        opendal::ErrorKind::AlreadyExists | opendal::ErrorKind::ConditionNotMatch
    )
}

const fn buffer_limit(required_bytes: usize, limit_bytes: usize) -> RemoteError {
    RemoteError::PayloadLimitExceeded {
        required_bytes,
        limit_bytes,
    }
}

const fn check_upload_buffer(capacity: usize) -> Result<(), RemoteError> {
    // OpenDAL takes ownership of the Vec. A short payload can still retain a
    // large allocation, so checking length alone does not protect the envelope.
    if capacity > TRANSFER_CHUNK_SIZE {
        return Err(buffer_limit(capacity, TRANSFER_CHUNK_SIZE));
    }
    Ok(())
}

impl RemoteClient {
    fn require_async_object_io(&self) -> Result<(), RemoteError> {
        if self.operator.info().scheme() == "fs" {
            return Err(RemoteError::UnsupportedCapability {
                missing: "async object I/O; use admitted file transfers".to_owned(),
            });
        }
        Ok(())
    }

    pub fn prepare_write(
        &self,
        size: u64,
        buffer_limit_bytes: usize,
    ) -> Result<PreparedRemoteWrite, RemoteError> {
        self.require_async_object_io()?;
        let info = self.operator.info();
        let cap = info.capability();
        let whole = size <= TRANSFER_CHUNK_SIZE as u64;
        let mut options = tuning::upload_write_options(size, cap);
        options.if_not_exists = cap.write_with_if_not_exists;
        if !whole {
            options.chunk = Some(
                options
                    .chunk
                    .unwrap_or(tuning::NETWORK_CHUNK_SIZE)
                    .max(cap.write_multi_min_size.unwrap_or(1))
                    .min(cap.write_multi_max_size.unwrap_or(usize::MAX)),
            );
        }
        let envelope = |options: &opendal::options::WriteOptions| {
            if whole {
                TRANSFER_CHUNK_SIZE
            } else {
                let chunk = options.chunk.unwrap_or(TRANSFER_CHUNK_SIZE);
                let retained_parts = if options.concurrent > 1 {
                    usize::try_from(size.div_ceil(chunk as u64))
                        .unwrap_or(usize::MAX)
                        .min(options.concurrent.saturating_add(COMPLETED_PART_PREFETCH))
                } else {
                    options.concurrent
                };
                chunk
                    .saturating_mul(retained_parts.saturating_add(2))
                    .saturating_add(2 * TRANSFER_CHUNK_SIZE)
            }
        };
        if envelope(&options) > buffer_limit_bytes {
            options.concurrent = 1;
        }
        let buffer_bytes = envelope(&options);
        if buffer_bytes > buffer_limit_bytes {
            return Err(buffer_limit(buffer_bytes, buffer_limit_bytes));
        }
        Ok(PreparedRemoteWrite {
            options,
            buffer_bytes,
            whole,
            size,
        })
    }

    pub async fn open_read(&self, oid: &Oid) -> Result<RemoteRead, RemoteError> {
        self.require_async_object_io()?;
        let reader = self
            .operator
            .reader_options(
                &crate::cache::object_key_oid(oid),
                opendal::options::ReaderOptions {
                    concurrent: 1,
                    prefetch: 0,
                    ..Default::default()
                },
            )
            .await
            .map_err(classify_opendal_error)?;
        let stream = reader
            .into_stream(..)
            .await
            .map_err(classify_opendal_error)?;
        Ok(RemoteRead {
            stream,
            pending: opendal::Buffer::new(),
        })
    }

    pub async fn write_object(
        &self,
        oid: &Oid,
        bytes: Vec<u8>,
        prepared: PreparedRemoteWrite,
    ) -> Result<(), RemoteError> {
        self.require_async_object_io()?;
        check_upload_buffer(bytes.capacity())?;
        let conditional = prepared.options.if_not_exists;
        match self
            .operator
            .write_options(&crate::cache::object_key_oid(oid), bytes, prepared.options)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if conditional && is_create_conflict(&error) => Ok(()),
            Err(error) => Err(classify_opendal_error(error)),
        }
    }

    pub async fn open_writer(
        &self,
        oid: &Oid,
        prepared: PreparedRemoteWrite,
    ) -> Result<AsyncRemoteWriter, RemoteError> {
        self.require_async_object_io()?;
        let conditional = prepared.options.if_not_exists;
        let inner = self
            .operator
            .writer_options(&crate::cache::object_key_oid(oid), prepared.options)
            .await
            .map_err(classify_opendal_error)?;
        Ok(AsyncRemoteWriter {
            remaining: prepared.size,
            inner,
            conditional,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn segmented_download_preserves_bytes_across_output_boundaries() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let payload: Vec<u8> = (0..=255)
            .cycle()
            .take(2 * TRANSFER_CHUNK_SIZE + 13)
            .collect();
        let fixture = HttpFixture::new(payload.clone(), "200 OK");
        let client = fixture.client(&runtime);
        runtime.block_on(async {
            let mut reader = client.open_read(&Oid::from_bytes([1; 32])).await.unwrap();
            reader.pending = vec![
                bytes::Bytes::from_static(b"first"),
                bytes::Bytes::from_static(b"second"),
            ]
            .into();
            let mut actual = Vec::new();
            while let Some(chunk) = reader.next().await.unwrap() {
                assert!(chunk.len() <= TRANSFER_CHUNK_SIZE);
                actual.extend(chunk);
            }
            let mut expected = b"firstsecond".to_vec();
            expected.extend(payload);
            assert_eq!(actual, expected);
            assert!(reader.next().await.unwrap().is_none());
        });
    }

    #[test]
    fn consumed_download_buffers_release_backing_owners_before_returning() {
        struct Payload {
            bytes: Vec<u8>,
            dropped: Arc<AtomicBool>,
        }
        impl AsRef<[u8]> for Payload {
            fn as_ref(&self) -> &[u8] {
                &self.bytes
            }
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::Release);
            }
        }

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(Vec::new(), "200 OK");
        let client = fixture.client(&runtime);
        runtime.block_on(async {
            for segmented in [false, true] {
                let dropped = Arc::new(AtomicBool::new(false));
                let backing = bytes::Bytes::from_owner(Payload {
                    bytes: vec![42; TRANSFER_CHUNK_SIZE],
                    dropped: Arc::clone(&dropped),
                });
                let mut reader = client.open_read(&Oid::from_bytes([1; 32])).await.unwrap();
                reader.pending = if segmented {
                    vec![backing.slice(..17), backing.slice(17..)].into()
                } else {
                    backing.clone().into()
                };
                drop(backing);
                assert!(!dropped.load(Ordering::Acquire));
                let chunk = reader.next().await.unwrap().unwrap();
                assert_eq!(chunk, vec![42; TRANSFER_CHUNK_SIZE]);
                assert!(reader.pending.is_empty());
                assert!(
                    dropped.load(Ordering::Acquire),
                    "a consumed buffer must not retain backing storage until the next read"
                );
            }
        });
    }

    #[test]
    fn download_remainder_does_not_retain_hidden_transport_backing() {
        struct Payload(Arc<Vec<u8>>);
        impl AsRef<[u8]> for Payload {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }
        for segmented in [false, true] {
            let allocation = Arc::new(vec![42; 8 * TRANSFER_CHUNK_SIZE]);
            let owner = Arc::downgrade(&allocation);
            let backing = bytes::Bytes::from_owner(Payload(allocation));
            let mut pending: opendal::Buffer = if segmented {
                vec![backing.slice(..17), backing.slice(17..34)].into()
            } else {
                backing.slice(..34).into()
            };
            drop(backing);
            assert!(owner.upgrade().is_some());
            retain_download_remainder(&mut pending);
            assert!(owner.upgrade().is_none());
            assert_eq!(pending.to_vec(), vec![42; 34]);
        }
    }

    struct HttpFixture {
        address: SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    struct RequestGate {
        matches: fn(&str) -> bool,
        arrived: tokio::sync::oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl HttpFixture {
        fn new(payload: Vec<u8>, status: &'static str) -> Self {
            Self::with_chunked_response(payload, status, false)
        }

        fn with_chunked_response(payload: Vec<u8>, status: &'static str, chunked: bool) -> Self {
            Self::with_responses(payload, status, chunked, 0, None)
        }

        fn with_responses(
            payload: Vec<u8>,
            status: &'static str,
            chunked: bool,
            successful_delete_batches: usize,
            reported_length: Option<usize>,
        ) -> Self {
            Self::serve(
                payload,
                status,
                chunked,
                successful_delete_batches,
                reported_length,
                None,
            )
        }

        fn serve(
            payload: Vec<u8>,
            status: &'static str,
            chunked: bool,
            successful_delete_batches: usize,
            reported_length: Option<usize>,
            mut gate: Option<RequestGate>,
        ) -> Self {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&requests);
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = Arc::clone(&stop);
            let worker = std::thread::spawn(move || {
                let mut deletion_batches = 0;
                while let Ok((mut stream, _)) = listener.accept() {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        if stream.read(&mut byte).unwrap_or(0) == 0 {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8(request).unwrap();
                    let head = request.starts_with("HEAD ");
                    let length = request
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<u64>().unwrap())
                        })
                        .unwrap_or(0);
                    std::io::copy(&mut (&mut stream).take(length), &mut std::io::sink()).unwrap();
                    let initiating = request.starts_with("POST ")
                        && request.lines().next().unwrap().contains("uploads");
                    let deleting = request.starts_with("POST ")
                        && request.lines().next().unwrap().contains("?delete");
                    let completing = request.starts_with("POST ") && !initiating;
                    let response: &[u8] = if deleting {
                        if payload.is_empty() || deletion_batches < successful_delete_batches {
                            b"<DeleteResult/>"
                        } else {
                            &payload
                        }
                    } else if initiating {
                        b"<InitiateMultipartUploadResult><UploadId>fixture</UploadId></InitiateMultipartUploadResult>"
                    } else if completing && status == "200 OK" {
                        b"<CompleteMultipartUploadResult><ETag>fixture</ETag></CompleteMultipartUploadResult>"
                    } else {
                        &payload
                    };
                    deletion_batches += usize::from(deleting);
                    let response_status = if request.starts_with("DELETE ") {
                        "204 No Content"
                    } else if initiating
                        || request.starts_with("PUT ") && request.contains("uploadId=")
                    {
                        "200 OK"
                    } else {
                        status
                    };
                    let gated = gate.as_ref().is_some_and(|gate| (gate.matches)(&request));
                    recorded.lock().unwrap().push(request);
                    if gated {
                        let gate = gate.take().unwrap();
                        let _ = gate.arrived.send(());
                        let _ = gate.release.recv();
                    }
                    let framing = if chunked {
                        "Transfer-Encoding: chunked".to_owned()
                    } else {
                        format!(
                            "Content-Length: {}",
                            reported_length.unwrap_or(response.len())
                        )
                    };
                    let headers = format!(
                        "HTTP/1.1 {response_status}\r\n{framing}\r\nETag: fixture\r\nConnection: close\r\n\r\n"
                    );
                    if stream.write_all(headers.as_bytes()).is_ok() && !head {
                        if chunked {
                            for chunk in response.chunks(64 * 1024) {
                                if write!(stream, "{:x}\r\n", chunk.len())
                                    .and_then(|()| stream.write_all(chunk))
                                    .and_then(|()| stream.write_all(b"\r\n"))
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            let _ = stream.write_all(b"0\r\n\r\n");
                        } else {
                            let _ = stream.write_all(response);
                        }
                    }
                }
            });
            Self {
                address,
                requests,
                stop,
                worker: Some(worker),
            }
        }

        fn client(&self, runtime: &tokio::runtime::Runtime) -> RemoteClient {
            let builder = opendal::services::S3::default()
                .bucket("fixture")
                .region("fixture")
                // hygiene-ok: endpoint belongs to this fixture's ephemeral loopback listener.
                .endpoint(&format!("http://{}", self.address))
                .skip_signature();
            RemoteClient {
                operator: Arc::new(opendal::Operator::new(builder).unwrap()),
                runtime: runtime.handle().clone(),
            }
        }
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.address);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn cancelled_http_read_write_and_close_release_requests_and_abort_owned_writers() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for phase in ["read", "write", "close"] {
            let (arrived, arrival) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            let matches: fn(&str) -> bool = match phase {
                "read" => |request| request.starts_with("GET "),
                "write" => |request| request.starts_with("PUT ") && request.contains("uploadId="),
                _ => |request| request.starts_with("POST ") && request.contains("uploadId="),
            };
            let fixture = HttpFixture::serve(
                Vec::new(),
                "200 OK",
                false,
                0,
                None,
                Some(RequestGate {
                    matches,
                    arrived,
                    release: released,
                }),
            );
            let (budget, occupancy) = crate::remote::RemoteRequestBudget::with_test_observer(
                std::num::NonZeroUsize::new(2).unwrap(),
                std::num::NonZeroUsize::new(1).unwrap(),
            );
            let mut client = fixture.client(&runtime);
            client.operator =
                Arc::new(client.operator.as_ref().clone().layer(budget.layer.clone()));
            // The future owns the release sender so panic/drop also unblocks
            // the fixture before its thread is joined.
            runtime.block_on(async move {
                let oid = Oid::from_bytes([1; 32]);
                if phase == "read" {
                    {
                        let read = async {
                            let mut reader = client.open_read(&oid).await.unwrap();
                            reader.next().await.unwrap()
                        };
                        tokio::pin!(read);
                        tokio::select! {
                            _ = &mut read => panic!("held GET cannot complete"),
                            result = arrival => result.unwrap(),
                        }
                    } // Cooperative cancellation drops the pending network future.
                    assert_eq!(occupancy.active.load(Ordering::Acquire), 0);
                    release.send(()).unwrap();
                } else {
                    let prepared = client
                        .prepare_write(64 * TRANSFER_CHUNK_SIZE as u64, 128 * TRANSFER_CHUNK_SIZE)
                        .unwrap();
                    let mut writer = client.open_writer(&oid, prepared).await.unwrap();
                    if phase == "close" {
                        for _ in 0..10 {
                            writer.write(vec![42; TRANSFER_CHUNK_SIZE]).await.unwrap();
                        }
                    }
                    {
                        let transfer = async {
                            if phase == "write" {
                                for _ in 0..80 {
                                    writer.write(vec![42; TRANSFER_CHUNK_SIZE]).await.unwrap();
                                }
                            } else {
                                writer.close().await.unwrap();
                            }
                        };
                        tokio::pin!(transfer);
                        tokio::select! {
                            () = &mut transfer => panic!("held writer operation cannot complete"),
                            result = arrival => result.unwrap(),
                        }
                    }
                    release.send(()).unwrap();
                    writer.abort().await.unwrap();
                    assert_eq!(occupancy.active.load(Ordering::Acquire), 0);
                }
                assert_eq!(occupancy.peak.load(Ordering::Acquire), 1);
            });
            let requests = fixture.requests.lock().unwrap();
            if phase != "read" {
                assert!(
                    requests
                        .iter()
                        .any(|request| request.starts_with("DELETE ")
                            && request.contains("uploadId="))
                );
            }
        }
    }

    #[test]
    fn misreported_content_length_cannot_expand_download_accumulation() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        // HTTP framing hides bytes beyond a short content length; a long
        // content length must fail at EOF rather than allocate for that length.
        for (actual, reported) in [(2 * TRANSFER_CHUNK_SIZE, 17), (17, 2 * TRANSFER_CHUNK_SIZE)] {
            let fixture =
                HttpFixture::with_responses(vec![42; actual], "200 OK", false, 0, Some(reported));
            let client = fixture.client(&runtime);
            runtime.block_on(async {
                let mut reader = client.open_read(&Oid::from_bytes([1; 32])).await.unwrap();
                let mut received = 0;
                let failed = loop {
                    match reader.next().await {
                        Ok(Some(bytes)) => {
                            assert!(bytes.capacity() <= TRANSFER_CHUNK_SIZE);
                            received += bytes.len();
                        }
                        Ok(None) => break false,
                        Err(_) => break true,
                    }
                };
                assert_eq!(failed, reported > actual);
                assert!(received <= actual.min(reported));
                if !failed {
                    assert_eq!(received, reported);
                }
            });
            let requests = fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with("GET "));
        }
    }

    #[test]
    fn deletion_uses_native_batches_and_flushes_the_final_partial_batch() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(Vec::new(), "200 OK");
        let client = fixture.client(&runtime);
        let oids: Vec<_> = (0u64..2001)
            .map(|index| {
                let mut bytes = [0; 32];
                bytes[..8].copy_from_slice(&index.to_le_bytes());
                Oid::from_bytes(bytes)
            })
            .collect();
        runtime.block_on(async {
            client
                .delete_objects(&[], |_| panic!("empty input cannot confirm deletions"))
                .await
                .unwrap();
            assert!(fixture.requests.lock().unwrap().is_empty());
            let mut confirmed = Vec::new();
            client
                .delete_objects(&oids, |count| {
                    confirmed.push(count);
                    assert_eq!(fixture.requests.lock().unwrap().len(), confirmed.len());
                })
                .await
                .unwrap();
            assert_eq!(confirmed, [1000, 1000, 1]);
        });
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.starts_with("POST ") && r.contains("?delete"))
                .count(),
            2
        );
        assert!(requests[2].starts_with("DELETE "));
        assert!(requests[2].contains(&crate::cache::object_key_oid(&oids[2000])));
    }

    #[test]
    fn partial_native_deletion_failure_does_not_confirm_the_window() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let oids = [Oid::from_bytes([1; 32]), Oid::from_bytes([2; 32])];
        let response = format!(
            "<DeleteResult><Error><Key>{}</Key><Code>AccessDenied</Code><Message>denied</Message></Error></DeleteResult>",
            crate::cache::object_key_oid(&oids[0])
        );
        let fixture = HttpFixture::new(response.into_bytes(), "200 OK");
        let client = fixture.client(&runtime);
        assert!(
            runtime
                .block_on(
                    client.delete_objects(&oids, |_| panic!(
                        "a partially failed batch is unconfirmed"
                    ))
                )
                .is_err()
        );
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST ") && requests[0].contains("?delete"));
    }

    #[test]
    fn later_batch_failure_preserves_prior_confirmations_and_stops_submission() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let oids: Vec<_> = (0u64..2001)
            .map(|index| {
                let mut bytes = [0; 32];
                bytes[..8].copy_from_slice(&index.to_le_bytes());
                Oid::from_bytes(bytes)
            })
            .collect();
        let response = format!(
            "<DeleteResult><Error><Key>{}</Key><Code>AccessDenied</Code><Message>denied</Message></Error></DeleteResult>",
            crate::cache::object_key_oid(&oids[1000])
        );
        let fixture = HttpFixture::with_responses(response.into_bytes(), "200 OK", false, 1, None);
        let client = fixture.client(&runtime);
        let mut confirmed = Vec::new();
        assert!(
            runtime
                .block_on(client.delete_objects(&oids, |count| confirmed.push(count)))
                .is_err()
        );
        assert_eq!(confirmed, [1000]);
        assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn cleanup_deadline_bounds_a_stalled_abort_with_virtual_time() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let attempted = AtomicBool::new(false);
            let abort = std::future::poll_fn(|_| {
                attempted.store(true, Ordering::Release);
                std::task::Poll::<opendal::Result<()>>::Pending
            });
            let cleanup = abort_with_timeout(abort);
            tokio::pin!(cleanup);
            assert!(futures::poll!(&mut cleanup).is_pending());
            assert!(attempted.load(Ordering::Acquire));
            tokio::time::advance(super::super::REMOTE_IO_TIMEOUT).await;
            assert!(matches!(cleanup.await, Err(RemoteError::CleanupTimedOut)));
        });
    }

    #[test]
    fn cleanup_preserves_success_and_provider_errors_before_the_deadline() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            assert!(abort_with_timeout(async { Ok(()) }).await.is_ok());
            assert!(matches!(
                abort_with_timeout(async {
                    Err(opendal::Error::new(
                        opendal::ErrorKind::PermissionDenied,
                        "provider denied abort",
                    ))
                })
                .await,
                Err(RemoteError::PermissionDenied { .. })
            ));
        });
    }

    #[test]
    fn live_response_holds_shared_http_capacity_until_dropped() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let first = HttpFixture::new(vec![42; 3 * TRANSFER_CHUNK_SIZE], "200 OK");
        let second = HttpFixture::new(b"next".to_vec(), "200 OK");
        let (budget, occupancy) = crate::remote::RemoteRequestBudget::with_test_observer(
            std::num::NonZeroUsize::new(2).unwrap(),
            std::num::NonZeroUsize::new(1).unwrap(),
        );
        let mut first_client = first.client(&runtime);
        first_client.operator = Arc::new(
            first_client
                .operator
                .as_ref()
                .clone()
                .layer(budget.layer.clone()),
        );
        let mut second_client = second.client(&runtime);
        second_client.operator =
            Arc::new(second_client.operator.as_ref().clone().layer(budget.layer));
        runtime.block_on(async {
            let oid = Oid::from_bytes([1; 32]);
            let mut first_reader = first_client.open_read(&oid).await.unwrap();
            assert_eq!(
                first_reader.next().await.unwrap().unwrap().len(),
                TRANSFER_CHUNK_SIZE
            );
            assert_eq!(occupancy.active.load(Ordering::Acquire), 1);
            let mut cancelled_reader = second_client.open_read(&oid).await.unwrap();
            {
                let waiting = cancelled_reader.next();
                tokio::pin!(waiting);
                assert!(futures::poll!(&mut waiting).is_pending());
            }
            drop(cancelled_reader);
            assert!(second.requests.lock().unwrap().is_empty());
            let mut second_reader = second_client.open_read(&oid).await.unwrap();
            {
                let next = second_reader.next();
                tokio::pin!(next);
                assert!(futures::poll!(&mut next).is_pending());
                assert!(second.requests.lock().unwrap().is_empty());
                drop(first_reader);
                assert_eq!(next.await.unwrap().unwrap(), b"next");
            }
            assert!(second_reader.next().await.unwrap().is_none());
            assert_eq!(occupancy.active.load(Ordering::Acquire), 0);
            drop(second_reader);
            assert_eq!(occupancy.active.load(Ordering::Acquire), 0);
            assert_eq!(occupancy.peak.load(Ordering::Acquire), 1);
        });
        assert_eq!(first.requests.lock().unwrap().len(), 1);
        assert_eq!(second.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn failed_http_read_releases_shared_request_capacity() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(Vec::new(), "500 Internal Server Error");
        let (budget, occupancy) = crate::remote::RemoteRequestBudget::with_test_observer(
            std::num::NonZeroUsize::new(1).unwrap(),
            std::num::NonZeroUsize::new(1).unwrap(),
        );
        let mut client = fixture.client(&runtime);
        client.operator = Arc::new(client.operator.as_ref().clone().layer(budget.layer));
        runtime.block_on(async {
            for _ in 0..2 {
                let mut reader = client.open_read(&Oid::from_bytes([1; 32])).await.unwrap();
                assert!(reader.next().await.is_err());
                drop(reader);
                assert_eq!(occupancy.active.load(Ordering::Acquire), 0);
            }
            assert_eq!(occupancy.peak.load(Ordering::Acquire), 1);
        });
        assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn tiny_body_uses_one_get_without_head() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(b"payload".to_vec(), "200 OK");
        let client = fixture.client(&runtime);
        runtime.block_on(async {
            let mut reader = client.open_read(&Oid::from_bytes([1; 32])).await.unwrap();
            let mut bytes = Vec::new();
            while let Some(chunk) = reader.next().await.unwrap() {
                bytes.extend(chunk);
            }
            assert_eq!(bytes, b"payload");
        });
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET "));
    }

    #[test]
    fn missing_content_length_streams_empty_tiny_and_large_bodies_without_head() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for size in [0, 7, 3 * TRANSFER_CHUNK_SIZE + 17] {
            let payload = vec![42; size];
            let fixture = HttpFixture::with_chunked_response(payload.clone(), "200 OK", true);
            let client = fixture.client(&runtime);
            runtime.block_on(async {
                let mut reader = client.open_read(&Oid::from_bytes([1; 32])).await.unwrap();
                let mut received = Vec::new();
                while let Some(chunk) = reader.next().await.unwrap() {
                    assert!(chunk.len() <= TRANSFER_CHUNK_SIZE);
                    received.extend(chunk);
                }
                assert_eq!(received, payload);
            });
            let requests = fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with("GET "));
        }
    }

    #[test]
    fn large_body_uses_one_get_with_bounded_chunks() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let payload = vec![42; 3 * TRANSFER_CHUNK_SIZE + 17];
        let fixture = HttpFixture::new(payload.clone(), "200 OK");
        let client = fixture.client(&runtime);
        runtime.block_on(async {
            let mut reader = client.open_read(&Oid::from_bytes([1; 32])).await.unwrap();
            let mut received = Vec::new();
            while let Some(chunk) = reader.next().await.unwrap() {
                assert!(chunk.len() <= TRANSFER_CHUNK_SIZE);
                received.extend(chunk);
            }
            assert_eq!(received, payload);
        });
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET "));
    }

    #[test]
    fn multipart_completion_conflict_aborts_the_losing_upload() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for status in ["200 OK", "412 Precondition Failed"] {
            let fixture = HttpFixture::new(Vec::new(), status);
            let client = fixture.client(&runtime);
            runtime.block_on(async {
                let prepared = client
                    .prepare_write(10 * TRANSFER_CHUNK_SIZE as u64, 128 * TRANSFER_CHUNK_SIZE)
                    .unwrap();
                let mut writer = client
                    .open_writer(&Oid::from_bytes([1; 32]), prepared)
                    .await
                    .unwrap();
                for _ in 0..10 {
                    writer.write(vec![42; TRANSFER_CHUNK_SIZE]).await.unwrap();
                }
                writer.close().await.unwrap();
            });
            let requests = fixture.requests.lock().unwrap();
            assert!(
                requests
                    .iter()
                    .any(|r| r.starts_with("POST ") && r.contains("uploads"))
            );
            assert!(
                requests
                    .iter()
                    .any(|r| r.starts_with("POST ") && r.contains("uploadId="))
            );
            assert_eq!(
                requests.iter().filter(|r| r.starts_with("DELETE ")).count(),
                usize::from(status != "200 OK")
            );
        }
    }

    #[test]
    fn conditional_whole_write_treats_precondition_conflict_as_publication() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(Vec::new(), "412 Precondition Failed");
        let client = fixture.client(&runtime);
        let prepared = client.prepare_write(7, TRANSFER_CHUNK_SIZE).unwrap();
        runtime
            .block_on(client.write_object(&Oid::from_bytes([1; 32]), b"payload".to_vec(), prepared))
            .unwrap();
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("if-none-match: *")
        );
    }

    #[test]
    fn multipart_buffer_requirement_includes_all_parts_and_falls_back_before_admission() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(Vec::new(), "200 OK");
        let client = fixture.client(&runtime);
        let parallel = client
            .prepare_write(64 * 1024 * 1024, 128 * 1024 * 1024)
            .unwrap();
        assert_eq!(parallel.options.concurrent, 4);
        assert_eq!(
            parallel.buffer_bytes(),
            10 * parallel.options.chunk.unwrap() + 2 * TRANSFER_CHUNK_SIZE
        );
        let large = client
            .prepare_write(1024 * 1024 * 1024, 128 * 1024 * 1024)
            .unwrap();
        assert_eq!(
            large.options.concurrent, 1,
            "completed multipart payloads must fit too"
        );
        let sequential = client
            .prepare_write(64 * 1024 * 1024, 32 * 1024 * 1024)
            .unwrap();
        assert_eq!(sequential.options.concurrent, 1);
        assert!(sequential.buffer_bytes() <= 32 * 1024 * 1024);
        assert!(matches!(
            client.prepare_write(64 * 1024 * 1024, 1),
            Err(RemoteError::PayloadLimitExceeded { required_bytes, limit_bytes: 1, .. })
                if required_bytes == sequential.buffer_bytes()
        ));
        assert!(fixture.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn growing_upload_cannot_exceed_its_prepared_part_count() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(Vec::new(), "200 OK");
        let client = fixture.client(&runtime);
        runtime.block_on(async {
            let prepared = client
                .prepare_write(2 * TRANSFER_CHUNK_SIZE as u64, 128 * 1024 * 1024)
                .unwrap();
            let mut writer = client
                .open_writer(&Oid::from_bytes([1; 32]), prepared)
                .await
                .unwrap();
            for _ in 0..2 {
                writer.write(vec![42; TRANSFER_CHUNK_SIZE]).await.unwrap();
            }
            assert!(matches!(
                writer.write(vec![42]).await,
                Err(RemoteError::PayloadLimitExceeded {
                    required_bytes: 1,
                    limit_bytes: 0,
                    ..
                })
            ));
            writer.abort().await.unwrap();
        });
    }

    #[test]
    fn oversized_whole_write_fails_locally_with_a_typed_payload_limit() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(Vec::new(), "200 OK");
        let client = fixture.client(&runtime);
        let prepared = client.prepare_write(7, TRANSFER_CHUNK_SIZE).unwrap();
        let error = runtime
            .block_on(client.write_object(
                &Oid::from_bytes([1; 32]),
                vec![42; TRANSFER_CHUNK_SIZE + 1],
                prepared,
            ))
            .unwrap_err();
        assert!(matches!(error, RemoteError::PayloadLimitExceeded {
            required_bytes, limit_bytes: TRANSFER_CHUNK_SIZE, ..
        } if required_bytes == TRANSFER_CHUNK_SIZE + 1));
        assert!(fixture.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn upload_limits_include_spare_capacity_before_provider_io() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let fixture = HttpFixture::new(Vec::new(), "200 OK");
        let client = fixture.client(&runtime);
        runtime.block_on(async {
            let oid = Oid::from_bytes([1; 32]);
            for streaming in [false, true] {
                for length in [7, TRANSFER_CHUNK_SIZE + 1] {
                    let mut bytes = Vec::with_capacity(TRANSFER_CHUNK_SIZE + 1);
                    bytes.resize(length, 42);
                    let capacity = bytes.capacity();
                    let prepared = client
                        .prepare_write(
                            if streaming {
                                64 * TRANSFER_CHUNK_SIZE as u64
                            } else {
                                7
                            },
                            128 * TRANSFER_CHUNK_SIZE,
                        )
                        .unwrap();
                    let error = if streaming {
                        let mut writer = client.open_writer(&oid, prepared).await.unwrap();
                        let error = writer.write(bytes).await.unwrap_err();
                        writer.abort().await.unwrap();
                        error
                    } else {
                        client
                            .write_object(&oid, bytes, prepared)
                            .await
                            .unwrap_err()
                    };
                    assert!(matches!(error, RemoteError::PayloadLimitExceeded {
                        required_bytes, limit_bytes: TRANSFER_CHUNK_SIZE, ..
                    } if required_bytes == capacity));
                }
            }
        });
        assert!(fixture.requests.lock().unwrap().is_empty());
    }
}
