//! `OpenDAL` operator construction for remote URLs.
//!
//! After template expansion, `Operator::from_uri` selects the backend by
//! scheme and parses its options. `OpenDAL` owns provider-specific settings
//! and credential lookup.
//!
//! `remotes.<name>.url` retains the configured `${VAR}` template. Callers
//! expand it before validation or use; read-only display does not resolve
//! environment variables. [`build_remote`] receives the expanded URL.

use mea::semaphore::{OwnedSemaphorePermit, Semaphore};
use opendal::layers::{ConcurrentLimitLayer, RetryLayer, TimeoutLayer};
use opendal::{ErrorKind, Operator};
use std::num::NonZeroUsize;
#[cfg(any(test, feature = "test-support"))]
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

mod file;
mod file_gc;
pub use file_gc::{FILE_GC_BATCH_SIZE, FileDeleteBatch, FileDeleteOutcome, FileObjectScan};
mod interpolate;
mod transfer;
mod tuning;
pub use transfer::{
    AsyncRemoteWriter, DOWNLOAD_BUFFER_BYTES, PreparedRemoteWrite, RemoteRead, TRANSFER_CHUNK_SIZE,
};

pub use file::{
    FileObjectWriter, FilePublication, FileReceiveError, FileUploadError, FileWriteError,
    FileWritePhase, PreparedFilePresence, PreparedFileRead, PreparedFileWrite,
};
pub use interpolate::InterpolateError;
pub use tuning::{STREAM_BUFFER_SIZE, with_stream_buffer};

/// Shared operation-scoped remote request budget for network remotes.
///
/// `OpenDAL`'s per-reader/per-writer concurrency controls remain responsible
/// for intra-object parallelism. The operation limit bounds `OpenDAL` operations,
/// while the HTTP limit bounds aggregate physical requests shared by every
/// remote operator opened through one operation.
#[derive(Clone)]
pub struct RemoteRequestBudget {
    layer: ConcurrentLimitLayer<ObservedSemaphore>,
    #[cfg(test)]
    http_semaphore: ObservedSemaphore,
}

impl std::fmt::Debug for RemoteRequestBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteRequestBudget")
            .finish_non_exhaustive()
    }
}

impl RemoteRequestBudget {
    #[must_use]
    pub fn new(operation_limit: NonZeroUsize, http_limit: NonZeroUsize) -> Self {
        let operation_semaphore = ObservedSemaphore::new(operation_limit);
        let http_semaphore = ObservedSemaphore::new(http_limit);
        #[cfg(test)]
        let observed_http_semaphore = http_semaphore.clone();
        Self {
            layer: ConcurrentLimitLayer::with_semaphore(operation_semaphore)
                .with_http_semaphore(http_semaphore),
            #[cfg(test)]
            http_semaphore: observed_http_semaphore,
        }
    }

    #[cfg(test)]
    fn with_test_observer(
        operation_limit: NonZeroUsize,
        http_limit: NonZeroUsize,
    ) -> (Self, Arc<RequestOccupancy>) {
        let operation_semaphore = ObservedSemaphore::new(operation_limit);
        let occupancy = Arc::new(RequestOccupancy {
            active: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
        });
        let http_semaphore = ObservedSemaphore {
            semaphore: Arc::new(Semaphore::new(http_limit.get())),
            occupancy: Some(Arc::clone(&occupancy)),
        };
        let layer = ConcurrentLimitLayer::with_semaphore(operation_semaphore)
            .with_http_semaphore(http_semaphore.clone());
        (
            Self {
                layer,
                http_semaphore,
            },
            occupancy,
        )
    }
}

#[derive(Clone)]
struct ObservedSemaphore {
    semaphore: Arc<Semaphore>,
    #[cfg(test)]
    occupancy: Option<Arc<RequestOccupancy>>,
}

impl ObservedSemaphore {
    fn new(limit: NonZeroUsize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(limit.get())),
            #[cfg(test)]
            occupancy: None,
        }
    }
}

struct ObservedPermit {
    _permit: OwnedSemaphorePermit,
    #[cfg(test)]
    _occupancy: Option<Arc<RequestOccupancyGuard>>,
}

impl opendal::layers::ConcurrentLimitSemaphore for ObservedSemaphore {
    type Permit = ObservedPermit;

    async fn acquire(&self) -> Self::Permit {
        let permit = self.semaphore.clone().acquire_owned(1).await;
        #[cfg(test)]
        let occupancy = self.occupancy.as_ref().map(|occupancy| {
            occupancy.acquire();
            Arc::new(RequestOccupancyGuard {
                occupancy: Arc::clone(occupancy),
            })
        });
        ObservedPermit {
            _permit: permit,
            #[cfg(test)]
            _occupancy: occupancy,
        }
    }
}

#[cfg(test)]
struct RequestOccupancy {
    active: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl RequestOccupancy {
    fn acquire(&self) {
        use std::sync::atomic::Ordering;

        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
    }
}

#[cfg(test)]
struct RequestOccupancyGuard {
    occupancy: Arc<RequestOccupancy>,
}

#[cfg(test)]
impl Drop for RequestOccupancyGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        self.occupancy.active.fetch_sub(1, Ordering::AcqRel);
    }
}

use futures::StreamExt;
use interpolate::interpolate_env;

/// Retains the backend error for developer inspection without exposing its
/// potentially secret-bearing formatting or concrete `OpenDAL` type.
pub struct RemoteBackendError(opendal::Error);

impl std::fmt::Debug for RemoteBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteBackendError")
    }
}

impl std::fmt::Display for RemoteBackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("remote backend operation failed")
    }
}

impl std::error::Error for RemoteBackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Bounds a single control-plane call (`stat`, `create_dir`, `delete`,
/// `presign`, ...). A minute is long enough
/// that a merely slow-but-alive backend never trips it, short enough that
/// a backend which accepted a TCP connection but then never responds
/// fails instead of hanging the whole command forever.
const REMOTE_CONTROL_TIMEOUT: Duration = Duration::from_mins(1);

/// Bounds every already-open IO body call (one `read`/`write` chunk, one
/// `list` page, ...). A minute avoids an aggressive bound: a healthy but
/// merely slow large-object
/// transfer (a big multipart chunk over a modest connection) must not be
/// mistaken for a stalled one and needlessly retried -- only a call that
/// stays stuck for a full minute (e.g. a credential provider silently
/// falling through to an unreachable EC2 instance-metadata endpoint at
/// 169.254.169.254, a TCP connection stuck "Busy ESTAB", or a firewalled
/// endpoint that drops packets instead of resetting the connection)
/// should trip this and hand off to gat's bounded [`RetryLayer`].
const REMOTE_IO_TIMEOUT: Duration = Duration::from_mins(1);

/// How many times [`RetryLayer`] retries an operation opendal itself
/// classifies as temporary (`opendal::Error::is_temporary`, e.g. rate
/// limiting, a dropped connection, or one of the [`TimeoutLayer`]
/// timeouts above) before giving up and surfacing the error. Kept small
/// and bounded (never unlimited) so a persistently unreachable remote
/// still fails in a predictable, bounded amount of time rather than
/// retrying forever.
const REMOTE_RETRY_MAX_TIMES: usize = 3;

/// Floor and ceiling for [`RetryLayer`]'s exponential backoff between
/// attempts. `min_delay` avoids hammering a remote that's merely
/// momentarily rate-limiting; `max_delay` keeps the bounded number of
/// retries above from still adding up to an unreasonably long total wait.
const REMOTE_RETRY_MIN_DELAY: Duration = Duration::from_millis(200);
const REMOTE_RETRY_MAX_DELAY: Duration = Duration::from_secs(5);

// Compile-time guards against a typo silently turning "bounded retry with
// a sane backoff window" into "retry forever" or an inverted/empty
// backoff window -- both would only surface as a hang against a real
// flaky/unreachable remote, which unit tests in this module deliberately
// never exercise (see the module doc above).
const _: () = assert!(REMOTE_RETRY_MAX_TIMES > 0, "retries must not be disabled");
const _: () = assert!(
    REMOTE_RETRY_MAX_TIMES <= 10,
    "retries must stay small and bounded, not unlimited"
);
const _: () = assert!(
    REMOTE_RETRY_MIN_DELAY.as_nanos() <= REMOTE_RETRY_MAX_DELAY.as_nanos(),
    "backoff window must not be inverted"
);
const _: () = assert!(
    !REMOTE_CONTROL_TIMEOUT.is_zero() && !REMOTE_IO_TIMEOUT.is_zero(),
    "a zero timeout would fail every call instantly"
);

/// Wraps every remote [`Operator`] this module builds with the same resilience
/// layers and, for operation-scoped clients, a shared physical-request layer.
/// The resilience layers are applied in the one order opendal's own docs require
/// (see [`TimeoutLayer`]'s and [`RetryLayer`]'s "Cancellation Safety"/
/// "Stateful operation bodies" notes): [`TimeoutLayer`] first (innermost,
/// closest to the backend) so each individual retry attempt gets its own
/// fresh deadline, then [`RetryLayer`] (outermost) so only opendal's own
/// `is_temporary` errors -- never permission/not-found/malformed-config
/// failures -- are ever retried. Applying `TimeoutLayer` on the outside
/// instead would let a timeout abort an attempt mid-retry and corrupt the
/// retried body's internal state.
///
/// `RetryLayer` itself is only added for network schemes -- `file://`
/// local I/O errors are never the transient, retry-worthy kind this
/// policy targets, so it's left with only the timeout layer. Retries are
/// otherwise transparent: opendal's own default (`log::warn!`-based)
/// notification fires on each attempt, and once
/// [`REMOTE_RETRY_MAX_TIMES`] is exhausted the final `opendal::Error`
/// simply flows through gat's ordinary `classify_opendal_error`
/// mapping, unchanged -- there is no gat-side reclassification of "this
/// was a retried error" versus any other temporary failure.
///
/// The optional physical-request layer is applied outermost so its shared
/// HTTP semaphore is held through the response body lifetime, including
/// multipart/range requests created by `OpenDAL`.
///
/// Gat deliberately does not install a custom HTTP client/transport here:
/// opendal's own default reqwest-based transport already pools
/// connections per operator, so a gat-owned client would only add
/// dependency weight for settings (a separate connect timeout, DNS
/// caching, a forced HTTP version) with no demonstrated benefit over
/// opendal's defaults plus [`REMOTE_CONTROL_TIMEOUT`]/
/// [`REMOTE_IO_TIMEOUT`] above.
fn with_gat_defaults(
    op: Operator,
    scheme: &str,
    request_budget: Option<&RemoteRequestBudget>,
) -> Operator {
    let op = op.layer(
        TimeoutLayer::new()
            .with_timeout(REMOTE_CONTROL_TIMEOUT)
            .with_io_timeout(REMOTE_IO_TIMEOUT),
    );
    let op = if scheme == "file" {
        op
    } else {
        op.layer(
            RetryLayer::new()
                .with_jitter()
                .with_min_delay(REMOTE_RETRY_MIN_DELAY)
                .with_max_delay(REMOTE_RETRY_MAX_DELAY)
                .with_max_times(REMOTE_RETRY_MAX_TIMES),
        )
    };
    if scheme == "file" {
        return op;
    }
    if let Some(budget) = request_budget {
        op.layer(budget.layer.clone())
    } else {
        op
    }
}

/// Typed remote/operator-construction failures, without URL display strings.
/// The engine supplies original template or remote identity context. The
/// `opendal::Error` itself is retained as a hidden `#[source]` -- never
/// rendered directly (its `Display`/`Debug` text isn't proven not to
/// echo back parts of the input, e.g. a decoded token or normalized query
/// parameter). Only `opendal::ErrorKind` -- a small, structured,
/// allowlisted enum opendal publishes for exactly this purpose -- is ever
/// used for the safe, user-facing summary via `classify_opendal_error`.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The readiness deadline override is not a usable positive whole-second duration.
    #[error("invalid remote connection timeout override")]
    InvalidConnectTimeout,

    /// The remote URL failed opendal's own scheme/config validation (bad
    /// host syntax, invalid port, root not specified, an unsupported
    /// query parameter for the resolved backend, ...).
    #[error("invalid remote url")]
    MalformedUrl {
        #[source]
        source: RemoteBackendError,
    },

    /// The URL's scheme selected a backend opendal has no service
    /// registered for -- either genuinely unsupported, or this build's
    /// Cargo features didn't enable it.
    #[error("unsupported remote scheme")]
    UnsupportedScheme {
        #[source]
        source: RemoteBackendError,
    },

    /// A `file://` remote URL is syntactically well-formed but not the
    /// three-slash absolute-path form gat/opendal require. Gat's own
    /// validation (not an opendal error), so there is no opendal source
    /// to retain.
    #[error("invalid file remote path")]
    InvalidFileRemotePath { hint: &'static str },

    /// The URL's scheme is not one of gat's explicitly supported remote
    /// schemes (`file`, `s3`, `azblob`, `gcs`, `oss`) -- checked *before*
    /// the URL ever reaches opendal, so enabling an opendal Cargo feature
    /// alone can never implicitly make a scheme a supported gat remote.
    /// Gat's own validation (not an opendal error), so there is no opendal
    /// source to retain.
    #[error("unsupported remote scheme")]
    DisallowedScheme,

    /// The remote's *effective* capability (after any layers/config) is
    /// missing one or more operations gat's minimum remote contract
    /// requires (see `check_capability_contract`). Gat's own validation
    /// against opendal's structured `Capability` flags -- not an opendal
    /// error -- so there is no opendal source to retain.
    #[error("remote is missing required capabilities: {missing}")]
    UnsupportedCapability { missing: String },

    /// The remote rejected the request for lack of/invalid credentials,
    /// or (opendal's `PermissionDenied` kind does not distinguish the
    /// two) rejected it despite valid credentials due to insufficient
    /// authorization. Both surface through this one variant because
    /// opendal itself only publishes a single `PermissionDenied`
    /// `ErrorKind` for both HTTP 401 and 403-class backend responses --
    /// there is no reliable, backend-agnostic signal (short of parsing
    /// each backend's own error text, which this architecture
    /// deliberately never does) to split them further.
    #[error("remote permission denied")]
    PermissionDenied {
        #[source]
        source: RemoteBackendError,
    },

    /// The requested object/path does not exist on the remote.
    #[error("remote object not found")]
    NotFound {
        #[source]
        source: RemoteBackendError,
    },

    /// The remote could not be reached, or was reached but is rate-
    /// limiting/temporarily failing; retrying later may succeed (see
    /// `opendal::Error::is_temporary`).
    #[error("remote is unavailable")]
    Unavailable {
        #[source]
        source: RemoteBackendError,
    },

    /// The complete writer-abort deadline elapsed, including request admission
    /// and retries. Provider cleanup may not have completed.
    #[error("remote writer cleanup timed out")]
    CleanupTimedOut,

    /// The complete startup check exceeded its total deadline.
    #[error("remote readiness check timed out")]
    ReadinessTimedOut { budget: std::time::Duration },

    /// A prepared transfer or incoming payload exceeds its bounded envelope.
    /// This is a local resource decision, not a provider failure.
    #[error("remote transfer requires {required_bytes} payload bytes, limit is {limit_bytes}")]
    PayloadLimitExceeded {
        required_bytes: usize,
        limit_bytes: usize,
    },

    /// A backend operation failed for a reason not covered by a more
    /// specific variant above.
    #[error("remote operation failed")]
    OperationFailed {
        #[source]
        source: RemoteBackendError,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum OpenRemoteError {
    #[error(transparent)]
    Interpolate(#[from] InterpolateError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
}

/// Classifies an `opendal::Error` into a [`RemoteError`] using only its
/// structured [`opendal::ErrorKind`]/[`opendal::Error::is_temporary`] --
/// never its `Display`/`Debug` text -- so the classification itself can
/// never depend on (or leak) backend-specific message content.
pub(crate) fn classify_opendal_error(source: opendal::Error) -> RemoteError {
    let kind = source.kind();
    let temporary = source.is_temporary();
    let source = RemoteBackendError(source);
    match kind {
        ErrorKind::Unsupported => RemoteError::UnsupportedScheme { source },
        ErrorKind::ConfigInvalid => RemoteError::MalformedUrl { source },
        ErrorKind::NotFound => RemoteError::NotFound { source },
        ErrorKind::PermissionDenied => RemoteError::PermissionDenied { source },
        ErrorKind::RateLimited => RemoteError::Unavailable { source },
        _ if temporary => RemoteError::Unavailable { source },
        _ => RemoteError::OperationFailed { source },
    }
}

/// The exactly five remote schemes gat itself supports -- checked before
/// opendal's own scheme registry, so enabling an opendal Cargo feature
/// (`services-s3`, ...) never implicitly makes a scheme a supported gat
/// remote; removing/adding a supported provider is a change to this list
/// (and the matching `Cargo.toml` features), not just to opendal's build.
const SUPPORTED_SCHEMES: &[&str] = &["file", "s3", "azblob", "gcs", "oss"];

/// Extracts the scheme (the part before the first `:`) from a remote URL,
/// without fully parsing it -- used only for the allowlist check below, so
/// a URL that's too malformed to even have a scheme separator is left to
/// opendal's own parser to reject with a more specific [`RemoteError`].
fn scheme_of(url: &str) -> Option<&str> {
    url.split_once(':').map(|(scheme, _)| scheme)
}

/// Rewrites a Windows drive-letter `file:///C:/...` URL into the query-root
/// form `OpenDAL` accepts without prepending an invalid leading slash.
fn windows_file_root_uri(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file:///")?;
    // A root query already overrides the URI path. Do not encode it into a
    // drive-path filename while applying the no-query Windows workaround.
    if rest.contains('?') {
        return None;
    }
    let [drive, b':', ..] = rest.as_bytes() else {
        return None;
    };
    if !drive.is_ascii_alphabetic() {
        return None;
    }

    let root = rest.replace('\\', "/");
    Some(format!(
        "file:///?root={}",
        url::form_urlencoded::byte_serialize(root.as_bytes()).collect::<String>()
    ))
}

/// Resolve a remote URL into an opendal `Operator` via opendal's own scheme
/// registry (only services whose Cargo feature is enabled get registered),
/// so gat never has to know a given backend's config fields.
fn build_remote(url: &str) -> std::result::Result<Operator, RemoteError> {
    build_remote_with_request_budget(url, None)
}

fn build_remote_with_request_budget(
    url: &str,
    request_budget: Option<&RemoteRequestBudget>,
) -> std::result::Result<Operator, RemoteError> {
    // Object uploads must stage beside the destination. A second staging root
    // can select another filesystem and cannot preserve that contract. Reject
    // unsupported options explicitly instead of silently changing their meaning.
    if url.starts_with("file:")
        && let Ok(parsed) = url::Url::parse(url)
        && parsed.query_pairs().any(|(key, _)| key != "root")
    {
        return Err(RemoteError::InvalidFileRemotePath {
            hint: "file remote URLs support only the root query option; object uploads stage beside their destination.",
        });
    }

    // Gat's own allowlist, checked before the url ever reaches opendal:
    // an opendal Cargo feature being enabled (e.g. for a service another
    // opendal consumer needs) must never silently make that scheme a
    // supported gat remote.
    if let Some(scheme) = scheme_of(url)
        && !SUPPORTED_SCHEMES.contains(&scheme)
    {
        return Err(RemoteError::DisallowedScheme);
    }

    // `file://path` (two slashes) parses as host `path` with an empty root,
    // which opendal rejects with an opaque "root is not specified" — the
    // three-slash form (`file:///abs/path`) is what's actually needed, so
    // steer people there instead of making them decode opendal's message.
    // The suggestion is a static example rather than `file:///{rest}`,
    // since `rest` (taken from the untrusted input) could itself carry a
    // query string or other unsanitized text.
    if let Some(rest) = url.strip_prefix("file:")
        && !rest.starts_with("///")
    {
        return Err(RemoteError::InvalidFileRemotePath {
            hint: "file remotes need three slashes and an absolute path, e.g. \
                   `file:///absolute/path`.",
        });
    }
    // A Windows drive-letter path in the three-slash form (`file:///C:/...`)
    // parses fine, but opendal always re-prepends a leading `/` to whatever
    // root it extracts from the URL *path*, turning `C:/...` into the
    // uncanonicalizable `/C:/...`. Rewrite to a `?root=` query param, which
    // opendal passes through untouched, instead of making every Windows
    // user hit that error.
    let rewritten = windows_file_root_uri(url);
    let operator_url = rewritten.as_deref().unwrap_or(url);
    let scheme = scheme_of(url).unwrap_or("");
    Operator::from_uri(operator_url)
        .map(|op| with_gat_defaults(op, scheme, request_budget))
        .map_err(classify_opendal_error)
        .and_then(|op| check_capability_contract(op, scheme))
}

/// Gat's minimum remote capability contract, verified against the
/// effective (already layer-adjusted) capability of a just-built
/// `Operator` -- catching a backend/config combination that can't satisfy
/// what every gat remote operation needs at construction time, rather
/// than failing confusingly deep inside a later `push`/`fetch`/`sync`.
/// Every supported remote must be able to `stat`, `read`, `write`,
/// `list`, and `delete`; network remotes (anything but `file://`)
/// additionally must support multipart/multi writes (`write_can_multi`),
/// since gat's size-aware upload path (see `transfer_tuning`) relies on
/// it for large objects -- `file://` is exempt since local writes are
/// never chunked.
fn check_capability_contract(
    op: Operator,
    scheme: &str,
) -> std::result::Result<Operator, RemoteError> {
    let cap = op.info().capability();
    let mut missing = Vec::new();
    if !cap.stat {
        missing.push("stat");
    }
    if !cap.read {
        missing.push("read");
    }
    if !cap.write {
        missing.push("write");
    }
    if !cap.list {
        missing.push("list");
    }
    if !cap.delete {
        missing.push("delete");
    }
    if scheme != "file" && !cap.write_can_multi {
        missing.push("write_can_multi (required for network remotes)");
    }
    if missing.is_empty() {
        Ok(op)
    } else {
        Err(RemoteError::UnsupportedCapability {
            missing: missing.join(", "),
        })
    }
}

/// Cloneable remote capability. `OpenDAL` and backend-specific values never
/// cross this boundary.
#[derive(Clone)]
pub struct RemoteClient {
    operator: Arc<Operator>,
    #[cfg(any(test, feature = "test-support"))]
    runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteClient").finish_non_exhaustive()
    }
}

impl RemoteClient {
    pub fn validate(template: &str) -> Result<(), OpenRemoteError> {
        let url = interpolate_env(template)?;
        build_remote(&url)?;
        Ok(())
    }

    pub fn open(template: &str) -> Result<Self, OpenRemoteError> {
        Self::open_with_request_budget(template, None)
    }

    pub fn open_with_request_budget(
        template: &str,
        request_budget: Option<&RemoteRequestBudget>,
    ) -> Result<Self, OpenRemoteError> {
        let url = interpolate_env(template)?;
        let operator = Arc::new(build_remote_with_request_budget(&url, request_budget)?);
        #[cfg(any(test, feature = "test-support"))]
        let runtime = tokio::runtime::Handle::current();
        Ok(Self {
            operator,
            #[cfg(any(test, feature = "test-support"))]
            runtime,
        })
    }

    /// Whether this client needs network readiness before use.
    #[must_use]
    pub fn requires_readiness(&self) -> bool {
        self.operator.info().scheme() != "fs"
    }

    /// Runs `OpenDAL`'s root listing check within one total budget, including
    /// request admission and retries. Cancellation discards the entire lister.
    pub async fn check(&self, budget: std::time::Duration) -> Result<(), RemoteError> {
        match tokio::time::timeout(budget, self.operator.check()).await {
            Ok(result) => result.map_err(classify_opendal_error),
            Err(_) => Err(RemoteError::ReadinessTimedOut { budget }),
        }
    }

    pub async fn contains_object(&self, oid: &gat_core::oid::Oid) -> Result<bool, RemoteError> {
        self.operator
            .exists(&crate::cache::object_key_oid(oid))
            .await
            .map_err(classify_opendal_error)
    }

    pub async fn enumerate_objects(&self) -> Result<RemoteObjectLister, RemoteError> {
        let lister = self
            .operator
            .lister_options(
                &format!("{}/", crate::cache::OBJECT_HASH_NAMESPACE),
                opendal::options::ListOptions {
                    recursive: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(classify_opendal_error)?;
        Ok(RemoteObjectLister { lister })
    }

    /// Delete one already-validated semantic window using native batching.
    /// Calls `confirmed` only after closing each native-sized batch. A failed
    /// batch may be partly deleted but is never counted; prior confirmations
    /// remain valid. Backends without batch support confirm one object at a time.
    pub async fn delete_objects(
        &self,
        oids: &[gat_core::oid::Oid],
        mut confirmed: impl FnMut(usize),
    ) -> Result<(), RemoteError> {
        let batch_size = self
            .operator
            .info()
            .capability()
            .delete_max_size
            .unwrap_or(1)
            .max(1);
        for batch in oids.chunks(batch_size) {
            let mut deleter = self
                .operator
                .deleter()
                .await
                .map_err(classify_opendal_error)?;
            deleter
                .delete_iter(batch.iter().map(crate::cache::object_key_oid))
                .await
                .map_err(classify_opendal_error)?;
            deleter.close().await.map_err(classify_opendal_error)?;
            confirmed(batch.len());
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn write(&self, key: &str, bytes: Vec<u8>) -> Result<(), RemoteError> {
        self.runtime
            .block_on(self.operator.write(key, bytes))
            .map(|_| ())
            .map_err(classify_opendal_error)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn exists(&self, key: &str) -> Result<bool, RemoteError> {
        self.runtime
            .block_on(self.operator.exists(key))
            .map_err(classify_opendal_error)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{OpenRemoteError, classify_opendal_error};
    use opendal::ErrorKind;

    #[must_use]
    pub fn backend_open_error(message: &'static str) -> OpenRemoteError {
        classify_opendal_error(opendal::Error::new(ErrorKind::ConfigInvalid, message)).into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteObject {
    oid: gat_core::oid::Oid,
}

impl RemoteObject {
    #[must_use]
    pub const fn oid(self) -> gat_core::oid::Oid {
        self.oid
    }
}

pub struct RemoteObjectLister {
    lister: opendal::Lister,
}

impl RemoteObjectLister {
    pub async fn next(&mut self) -> Option<Result<RemoteObject, RemoteError>> {
        loop {
            let entry = self.lister.next().await?;
            match entry {
                Ok(entry) => {
                    if let Some(oid) = crate::cache::parse_object_key(entry.path()) {
                        return Some(Ok(RemoteObject { oid }));
                    }
                }
                Err(source) => {
                    return Some(Err(classify_opendal_error(source)));
                }
            }
        }
    }
}

pub fn initialize_backends() {
    opendal::init_default_registry();
}

/// Build a `file://` URL for `path`. Test-only helper shared by every test
/// across the crate that needs a `file://` remote (not specific to a
/// single test); delegates the Windows-drive-letter handling to
/// `build_remote` by just producing an ordinary three-slash URL.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn file_url(path: &Path) -> String {
    let path = path.display().to_string().replace('\\', "/");
    if let Some(stripped) = path.strip_prefix('/') {
        format!("file:///{stripped}")
    } else {
        format!("file:///{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::layers::ConcurrentLimitSemaphore;

    // Unit tests here only exercise the `file` scheme (real, hermetic, no
    // ambient env/network dependency). S3/Azblob construction has
    // backend-specific env/endpoint requirements that only a real (or
    // emulated, e.g. MinIO/Azurite) endpoint can validate meaningfully —
    // cannot be validated by unit tests that mutate process environment and
    // guess at ambient credentials.

    #[test]
    fn build_remote_error_never_contains_query_secret() {
        // an invalid scheme still produces an opendal error; the message
        // must not repeat the raw url or any query value.
        let err = build_remote("ftp://host/path?token=super-secret-value")
            .unwrap_err()
            .to_string();
        assert!(!err.contains("super-secret-value"));
        assert!(!err.contains("token=super-secret-value"));
    }

    /// Walks a `std::error::Error`'s full `source()` chain, collecting
    /// every level's `Display` text -- the `RemoteError`/typed-error
    /// equivalent of `anyhow::Error::chain()`, which is not
    /// available now that `build_remote` returns a typed `RemoteError`
    /// instead of an `anyhow::Error`.
    fn full_chain_text(err: &(dyn std::error::Error + 'static)) -> String {
        let mut full = err.to_string();
        let mut cause = err.source();
        while let Some(source) = cause {
            full.push_str(&source.to_string());
            cause = source.source();
        }
        full
    }

    #[test]
    fn build_remote_error_source_chain_never_contains_query_secret() {
        let err = build_remote("ftp://host/path?token=super-secret-value").unwrap_err();
        let full = full_chain_text(&err);
        assert!(!full.contains("super-secret-value"));
    }

    #[test]
    fn build_remote_error_never_leaks_secret_from_malformed_url() {
        // A malformed URL (bad port breaks `url::Url::parse`) that still
        // carries a credential-shaped query value must not have that value
        // survive into the error, its `Display`, or its full source chain
        // merely because structured parsing failed.
        // hygiene-ok: intentionally malformed URL used only to exercise redaction on a parse failure; never dialed.
        let malformed = format!("https://host:notaport/path?token={}", "supersecretvalue");
        let err = build_remote(&malformed).unwrap_err();
        let full = full_chain_text(&err);
        assert!(!full.contains("supersecretvalue"));
    }

    #[test]
    fn build_remote_relative_file_hint_is_static_and_has_no_query_leak() {
        let err = build_remote("file://remote?token=hunter2").unwrap_err();
        let text = err.to_string();
        assert!(!text.contains("hunter2"));
        assert!(matches!(err, RemoteError::InvalidFileRemotePath { .. }));
    }

    #[test]
    fn build_remote_file_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let url = file_url(tmp.path());
        assert!(build_remote(&url).is_ok());
    }

    #[test]
    fn request_budget_observes_shared_http_occupancy() {
        use std::sync::atomic::Ordering;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (budget, occupancy) = RemoteRequestBudget::with_test_observer(
                NonZeroUsize::new(4).unwrap(),
                NonZeroUsize::new(2).unwrap(),
            );
            let clone = budget.clone();
            let first = budget.http_semaphore.acquire().await;
            let second = clone.http_semaphore.acquire().await;

            assert_eq!(occupancy.active.load(Ordering::Acquire), 2);
            assert_eq!(occupancy.peak.load(Ordering::Acquire), 2);

            drop(second);
            drop(first);
            assert_eq!(occupancy.active.load(Ordering::Acquire), 0);
        });
    }

    #[test]
    fn build_remote_file_scheme_round_trips_through_the_resilience_layers() {
        // A normal, always-succeeding local operation must complete
        // normally now that every `build_remote` operator is
        // wrapped with `TimeoutLayer`/`RetryLayer`: neither layer should
        // ever be visible to a healthy backend, only to one that's slow
        // or transiently failing.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        opendal::init_default_registry();
        let tmp = tempfile::tempdir().unwrap();
        let url = file_url(tmp.path());
        let op = build_remote(&url).unwrap();
        rt.block_on(async {
            op.write("resilience-roundtrip.txt", b"ok".to_vec())
                .await
                .unwrap();
            assert_eq!(
                op.read("resilience-roundtrip.txt").await.unwrap().to_vec(),
                b"ok"
            );
            assert!(op.stat("resilience-roundtrip.txt").await.is_ok());
        });
    }

    #[test]
    fn build_remote_rejects_missing_scheme() {
        assert!(build_remote("just-a-path").is_err());
    }

    #[test]
    fn build_remote_rejects_relative_file_path_with_helpful_hint() {
        // `file://remote` (two slashes) parses `remote` as a host with an
        // empty root -- opendal's own error ("root is not specified") gives
        // no clue that three slashes are needed, so gat adds that hint. The
        // suggestion is a static example rather than echoing back the
        // supplied `remote` part, since that part could itself carry a
        // query string or other unsanitized text.
        let err = build_remote("file://remote").unwrap_err();
        assert!(
            matches!(err, RemoteError::InvalidFileRemotePath { .. }),
            "{err}"
        );
        assert!(matches!(
            err,
            RemoteError::InvalidFileRemotePath {
                hint: "file remotes need three slashes and an absolute path, e.g. `file:///absolute/path`.",
                ..
            }
        ));
    }

    #[test]
    fn capability_contract_rejects_missing_required_capability() {
        let dir = tempfile::tempdir().unwrap();
        let base =
            Operator::new(opendal::services::Fs::default().root(dir.path().to_str().unwrap()))
                .unwrap();
        let op = base.layer(opendal::layers::CapabilityOverrideLayer::new(|mut cap| {
            cap.stat = false;
            cap
        }));
        let err = check_capability_contract(op, "file").unwrap_err();
        match err {
            RemoteError::UnsupportedCapability { missing, .. } => {
                assert!(missing.contains("stat"), "{missing}");
            }
            other => panic!("expected UnsupportedCapability, got {other}"),
        }
    }

    #[test]
    fn capability_contract_exempts_file_scheme_from_write_can_multi() {
        let dir = tempfile::tempdir().unwrap();
        let base =
            Operator::new(opendal::services::Fs::default().root(dir.path().to_str().unwrap()))
                .unwrap();
        let op = base.layer(opendal::layers::CapabilityOverrideLayer::new(|mut cap| {
            cap.write_can_multi = false;
            cap
        }));
        assert!(
            check_capability_contract(op, "file").is_ok(),
            "file:// must not require write_can_multi"
        );
    }

    #[test]
    fn capability_contract_requires_write_can_multi_for_network_schemes() {
        let dir = tempfile::tempdir().unwrap();
        let base =
            Operator::new(opendal::services::Fs::default().root(dir.path().to_str().unwrap()))
                .unwrap();
        let op = base.layer(opendal::layers::CapabilityOverrideLayer::new(|mut cap| {
            cap.write_can_multi = false;
            cap
        }));
        let err = check_capability_contract(op, "s3").unwrap_err();
        match err {
            RemoteError::UnsupportedCapability { missing, .. } => {
                assert!(missing.contains("write_can_multi"), "{missing}");
            }
            other => panic!("expected UnsupportedCapability, got {other}"),
        }
    }

    #[test]
    fn build_remote_rejects_unsupported_scheme() {
        // The scheme allowlist rejects this before opendal is ever
        // consulted, so gat's own `DisallowedScheme` fires, not opendal's
        // `UnsupportedScheme`.
        let err = build_remote("ftp://host/path").unwrap_err();
        assert!(matches!(err, RemoteError::DisallowedScheme), "{err}");
    }

    #[test]
    fn build_remote_rejects_dropped_provider_scheme() {
        // The allowlist rejects unsupported provider schemes such as
        // `gdrive`/`onedrive`/`dropbox`/`azdls`/`lakefs`/`aliyun-drive`
        // the same way
        // as any other unknown scheme, without depending on whether the
        // matching opendal Cargo feature happens to be compiled in.
        for scheme in [
            "gdrive",
            "onedrive",
            "dropbox",
            "azdls",
            "lakefs",
            "aliyun-drive",
        ] {
            let url = format!("{scheme}://default/prefix");
            let err = build_remote(&url).unwrap_err();
            assert!(
                matches!(err, RemoteError::DisallowedScheme),
                "{scheme}: {err}"
            );
        }
    }

    #[test]
    fn build_remote_accepts_windows_drive_letter_file_url() {
        // This is pure string handling on every host; real filesystem
        // operator construction is covered by the TempDir-backed tests above.
        // hygiene-ok: pure Windows file-URL rewrite input; never opened or written.
        let url = "file:///C:/Users/example/cache";
        assert_eq!(
            windows_file_root_uri(url).as_deref(),
            Some("file:///?root=C%3A%2FUsers%2Fexample%2Fcache")
        );
    }

    // `classify_opendal_error` tests: exercise every major `ErrorKind`
    // mapping deterministically by constructing synthetic `opendal::Error`
    // values directly (no real backend/network dependency needed).

    #[test]
    fn classify_maps_unsupported_to_unsupported_scheme() {
        let err = opendal::Error::new(ErrorKind::Unsupported, "boom");
        assert!(matches!(
            classify_opendal_error(err),
            RemoteError::UnsupportedScheme { .. }
        ));
    }

    #[test]
    fn classify_maps_config_invalid_to_malformed_url() {
        let err = opendal::Error::new(ErrorKind::ConfigInvalid, "boom");
        assert!(matches!(
            classify_opendal_error(err),
            RemoteError::MalformedUrl { .. }
        ));
    }

    #[test]
    fn classify_maps_not_found_to_not_found() {
        let err = opendal::Error::new(ErrorKind::NotFound, "boom");
        assert!(matches!(
            classify_opendal_error(err),
            RemoteError::NotFound { .. }
        ));
    }

    #[test]
    fn classify_maps_permission_denied_to_permission_denied() {
        let err = opendal::Error::new(ErrorKind::PermissionDenied, "boom");
        assert!(matches!(
            classify_opendal_error(err),
            RemoteError::PermissionDenied { .. }
        ));
    }

    #[test]
    fn classify_maps_rate_limited_to_unavailable() {
        let err = opendal::Error::new(ErrorKind::RateLimited, "boom");
        assert!(matches!(
            classify_opendal_error(err),
            RemoteError::Unavailable { .. }
        ));
    }

    #[test]
    fn classify_maps_an_unclassified_kind_to_operation_failed() {
        let err = opendal::Error::new(ErrorKind::AlreadyExists, "boom");
        assert!(matches!(
            classify_opendal_error(err),
            RemoteError::OperationFailed { .. }
        ));
    }

    #[test]
    fn backend_error_exposes_opendal_only_through_its_source() {
        use std::error::Error as _;

        const SECRET: &str = "SYNTHETIC-BACKEND-SECRET";
        let backend = RemoteBackendError(opendal::Error::new(ErrorKind::ConfigInvalid, SECRET));

        assert_eq!(backend.to_string(), "remote backend operation failed");
        assert_eq!(format!("{backend:?}"), "RemoteBackendError");
        assert!(
            backend
                .source()
                .and_then(|source| source.downcast_ref::<opendal::Error>())
                .is_some()
        );
    }

    #[test]
    fn classified_error_retains_the_backend_source_without_rendering_it() {
        use std::error::Error as _;

        const SECRET: &str = "SYNTHETIC-BACKEND-SECRET";
        let remote = classify_opendal_error(opendal::Error::new(ErrorKind::ConfigInvalid, SECRET));
        let backend = remote
            .source()
            .and_then(|source| source.downcast_ref::<RemoteBackendError>())
            .expect("remote error must retain the redacted backend wrapper");

        assert!(
            backend
                .source()
                .and_then(|source| source.downcast_ref::<opendal::Error>())
                .is_some()
        );
        assert!(!remote.to_string().contains(SECRET));
        assert!(!format!("{remote:?}").contains(SECRET));
    }
}

#[cfg(test)]
mod readiness_tests;
