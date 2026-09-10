use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct Responses {
    calls: Arc<AtomicUsize>,
    mode: Mode,
}

#[derive(Clone, Copy)]
enum Mode {
    Pending,
    PendingBody,
    SlowAfterReady,
    Denied,
    Empty,
    Retry,
}

impl opendal::HttpTransport for Responses {
    async fn fetch(
        &self,
        request: http::Request<opendal::Buffer>,
    ) -> opendal::Result<http::Response<opendal::HttpBody>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            assert!(request.uri().query().unwrap().contains("max-keys=1"));
        }
        if matches!(self.mode, Mode::SlowAfterReady) && call > 0 {
            tokio::time::sleep(Duration::from_secs(11)).await;
        }
        match self.mode {
            Mode::Pending => std::future::pending().await,
            Mode::Denied => Err(opendal::Error::new(
                ErrorKind::PermissionDenied,
                "SYNTHETIC-SECRET",
            )),
            Mode::Retry => {
                Err(opendal::Error::new(ErrorKind::Unexpected, "SYNTHETIC-SECRET").set_temporary())
            }
            Mode::PendingBody => Ok(http::Response::new(opendal::HttpBody::new(
                futures::stream::pending(),
                None,
            ))),
            Mode::Empty | Mode::SlowAfterReady => Ok(http::Response::new(opendal::HttpBody::new(
                futures::stream::iter([Ok(opendal::Buffer::from(
                    "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>",
                ))]),
                None,
            ))),
        }
    }
}

fn client(mode: Mode) -> (RemoteClient, Arc<AtomicUsize>, RemoteRequestBudget) {
    let calls = Arc::new(AtomicUsize::new(0));
    let budget =
        RemoteRequestBudget::new(NonZeroUsize::new(1).unwrap(), NonZeroUsize::new(1).unwrap());
    let operator = Operator::new(
        opendal::services::S3::default()
            .bucket("fixture")
            .region("fixture")
            .skip_signature(),
    )
    .unwrap()
    .with_context(opendal::OperationContext::new().with_http_transport(
        opendal::HttpTransporter::new(Responses {
            calls: calls.clone(),
            mode,
        }),
    ))
    .layer(opendal::layers::RetryLayer::new().with_max_times(3))
    .layer(budget.layer.clone());
    (
        RemoteClient {
            io_timeout: gat_core::settings::NetworkOptions::default()
                .io_timeout
                .duration(),
            operator: Arc::new(operator),
            runtime: tokio::runtime::Handle::current(),
        },
        calls,
        budget,
    )
}

#[tokio::test(start_paused = true)]
async fn readiness_deadline_cancels_request_and_releases_permits() {
    let (client, calls, requests) = client(Mode::Pending);
    let budget = Duration::from_secs(2);
    let started = tokio::time::Instant::now();
    assert!(
        matches!(client.check(budget).await, Err(RemoteError::ReadinessTimedOut { budget: actual }) if actual == budget)
    );
    assert_eq!(started.elapsed(), budget);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(requests.http_semaphore.semaphore.available_permits(), 1);
    // A second check reaches the transport only if the operation permit was released too.
    assert!(matches!(
        client.check(budget).await,
        Err(RemoteError::ReadinessTimedOut { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn readiness_deadline_includes_request_admission() {
    let (client, calls, requests) = client(Mode::Empty);
    let _permit = requests.http_semaphore.semaphore.acquire(1).await;
    assert!(matches!(
        client.check(Duration::from_secs(2)).await,
        Err(RemoteError::ReadinessTimedOut { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn readiness_permission_denied_is_immediate_and_opaque() {
    let (client, calls, _) = client(Mode::Denied);
    let started = tokio::time::Instant::now();
    let error = client.check(Duration::from_secs(10)).await.unwrap_err();
    assert!(matches!(error, RemoteError::PermissionDenied { .. }));
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!format!("{error:?}").contains("SYNTHETIC-SECRET"));
}

#[tokio::test(start_paused = true)]
async fn readiness_accepts_empty_listing_on_the_transfer_operator() {
    let (client, calls, _) = client(Mode::Empty);
    let operator = client.operator.clone();
    client.check(Duration::from_secs(10)).await.unwrap();
    assert!(Arc::ptr_eq(&operator, &client.operator));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn retries_do_not_restart_readiness_deadline() {
    let (client, calls, _) = client(Mode::Retry);
    let budget = Duration::from_millis(1);
    let started = tokio::time::Instant::now();
    assert!(matches!(
        client.check(budget).await,
        Err(RemoteError::ReadinessTimedOut { .. })
    ));
    assert_eq!(started.elapsed(), budget);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn cancelling_internal_lister_releases_http_body_permit() {
    let (client, calls, requests) = client(Mode::PendingBody);
    assert!(matches!(
        client.check(Duration::from_secs(2)).await,
        Err(RemoteError::ReadinessTimedOut { .. })
    ));
    assert_eq!(requests.http_semaphore.semaphore.available_permits(), 1);
    assert!(matches!(
        client.check(Duration::from_secs(2)).await,
        Err(RemoteError::ReadinessTimedOut { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn readiness_budget_does_not_apply_to_subsequent_io() {
    let (mut client, calls, _) = client(Mode::SlowAfterReady);
    client.operator = Arc::new(with_gat_defaults(
        (*client.operator).clone(),
        "s3",
        None,
        gat_core::settings::NetworkOptions::default(),
    ));
    client.check(Duration::from_secs(10)).await.unwrap();
    let started = tokio::time::Instant::now();
    let mut objects = client.enumerate_objects().await.unwrap();
    assert!(objects.next().await.is_none());
    assert_eq!(started.elapsed(), Duration::from_secs(11));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
