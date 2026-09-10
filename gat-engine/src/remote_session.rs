//! Coordinator-owned, operation-scoped remote lifecycle.
//!
//! A [`RemoteSession`] is a lazily populated, operation-scoped cache of opaque
//! remote clients, keyed by resolved remote name. Constructing one opens no
//! client; each remote a command actually uses initializes at most one
//! client for the whole operation, reused across every push/fetch/pull/
//! repair obligation routed to it. Network clients are returned only after a
//! bounded readiness check succeeds; failures are shared and cached too.
//!
//! Operator initialization is coordinator-owned: a window's coordinator
//! (the single thread driving
//! `transfer`/`repair`/`status` for one bounded window) always calls
//! [`RemoteSession::open`] (directly, or via [`RemoteSession::open_handle`])
//! for every distinct remote id that window's jobs will need, *before*
//! dispatching any of those jobs to the [`super::remote_executor::RemoteExecutor`].
//! Worker closures only ever receive an already-open opaque client and
//! perform data-plane I/O with it -- they never call [`RemoteSession::open`]
//! themselves and so never race to initialize a remote. This coordinator-owns-init
//! contract is enforced by the type system: every opening method takes
//! `&mut self`, so only
//! the coordinator holding `&mut RemoteSession`/`&mut Session`/`&mut
//! Operation` can call them at all -- a worker closure that only ever
//! receives `&RemoteSession` (or, in practice, an already-built
//! `RemoteHandle`) has no way to construct or open one. The operator pool
//! itself is a plain, uncontended `HashMap` rather than a `Mutex<HashMap<..>>`:
//! there is no concurrent access to synchronize against because opening
//! genuinely requires exclusive coordinator access.

use super::remote_catalog::{RemoteCatalog, RemoteId};
use gat_core::progress::{ProgressActivity, ProgressHandle};
use std::collections::HashMap;
use std::sync::Arc;

/// Everything opening or reusing a remote operator can fail with, without
/// exposing the physical remote implementation above the engine boundary.
#[derive(Clone, Debug)]
pub struct RemoteSessionError {
    source: Arc<crate::remote_open::RemoteOpenError>,
}

impl RemoteSessionError {
    /// Original endpoint template, retained without environment expansion.
    #[must_use]
    pub fn template(&self) -> &gat_core::endpoint::RemoteUrlTemplate {
        self.source.template()
    }

    #[must_use]
    pub fn kind(&self) -> &crate::remote_open::RemoteOpenFailureKind {
        self.source.kind()
    }
}

impl std::fmt::Display for RemoteSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not open the operation's remote")
    }
}

impl std::error::Error for RemoteSessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

type Result<T> = std::result::Result<T, RemoteSessionError>;

/// An already-open remote, ready for worker-side data-plane I/O. It
/// consolidates the operator and id. Always produced by
/// [`RemoteSession::open_handle`] on
/// the coordinator thread, before any job referencing it is dispatched to
/// the [`super::remote_executor::RemoteExecutor`] -- workers only ever
/// clone/read a `RemoteHandle`, never open one themselves.
///
/// Structural guarantee: worker code cannot
/// initialize remotes. `RemoteHandle` has no method that opens/builds an
/// operator -- only [`RemoteSession::open`]/[`RemoteSession::open_handle`]
/// do, and both require `&mut RemoteSession`. A worker closure passed to
/// [`super::remote_executor::RemoteExecutor::run_window`] receives only
/// `&RemoteHandle` (see that function's signature), never `&mut
/// RemoteSession`, so there is no path from worker code to remote
/// initialization -- a compile error, not a runtime check, if attempted.
#[derive(Clone, Debug)]
pub(crate) struct RemoteHandle {
    id: RemoteId,
    client: gat_io::RemoteClient,
}

impl RemoteHandle {
    /// The remote's compact [`RemoteId`] within the operation's compiled
    /// [`RemoteCatalog`] this handle was opened against: every successfully
    /// opened handle is
    /// catalog-backed, since the private client builder resolves the
    /// operator's URL through the catalog and fails the whole open if
    /// `name` isn't in it, so every opened handle is catalog-backed. Used by
    /// [`super::remote_executor::RemoteExecutor`] to key each job's
    /// per-remote scheduling budget directly off the handle it carries
    /// instead of a separately
    /// maintained remote-name string.
    pub(crate) const fn id(&self) -> RemoteId {
        self.id
    }

    pub(crate) const fn client(&self) -> &gat_io::RemoteClient {
        &self.client
    }
}

/// Coordinator-owned, operation-scoped cache of remote operators. Carries
/// no remote configuration of its own: every
/// open resolves the remote's URL through the caller-supplied
/// [`RemoteCatalog`] (the operation's one authoritative remote-config
/// source, compiled once onto [`super::snapshot::Snapshot`]) instead of
/// cloning and maintaining an independent `RemotesConfig`. Opening
/// requires `&mut self`: only the coordinator
/// holding exclusive access can initialize an operator, so no synchronization
/// is needed. The common first remote is retained inline; a `HashMap` is
/// allocated only if the operation opens a second distinct remote.
pub(crate) struct RemoteSession {
    first: Option<(RemoteId, Result<gat_io::RemoteClient>)>,
    additional: HashMap<RemoteId, Result<gat_io::RemoteClient>>,
    request_budget: gat_io::RemoteRequestBudget,
    resolver: gat_io::TemplateResolver,
    options: gat_core::settings::NetworkOptions,
}

#[cfg(any(test, feature = "test-support"))]
impl Default for RemoteSession {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteSession {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new() -> Self {
        let options = gat_core::settings::NetworkOptions::default();
        let capacity = options.request_concurrency.capacity();
        Self::with_request_budget(
            gat_io::RemoteRequestBudget::new(capacity, capacity),
            gat_io::InvocationInputs::from_pairs([] as [(&str, &str); 0])
                .unwrap()
                .templates(),
            options,
        )
    }

    pub(crate) fn with_request_budget(
        request_budget: gat_io::RemoteRequestBudget,
        resolver: gat_io::TemplateResolver,
        options: gat_core::settings::NetworkOptions,
    ) -> Self {
        Self {
            first: None,
            additional: HashMap::new(),
            request_budget,
            resolver,
            options,
        }
    }

    /// Opens (or reuses) the operator for the already-resolved remote
    /// `id` (e.g. one a caller already holds after routing through
    /// [`RemoteCatalog::resolve`]). Only callable through `&mut self`, so
    /// only a window's coordinator -- never a worker closure -- can reach
    /// this, before any worker job that uses the remote is dispatched
    /// Takes only `id`, never an independent `name`: the diagnostic name
    /// and endpoint URL are derived from `catalog` inside this method, so
    /// it is impossible to pair remote A's id with remote B's name --
    /// there is exactly one identity a caller supplies, and the catalog
    /// is the sole authority translating it to everything else.
    pub(crate) fn open(
        &mut self,
        catalog: &RemoteCatalog,
        id: RemoteId,
        progress: Option<&ProgressHandle>,
    ) -> Result<gat_io::RemoteClient> {
        let resolver = self.resolver.clone();
        let options = self.options;
        self.open_with(id, |request_budget| {
            Self::build(catalog, id, request_budget, progress, &resolver, options)
        })
    }

    fn open_with(
        &mut self,
        id: RemoteId,
        initialize: impl FnOnce(&gat_io::RemoteRequestBudget) -> Result<gat_io::RemoteClient>,
    ) -> Result<gat_io::RemoteClient> {
        if let Some((first_id, client)) = &self.first {
            if *first_id == id {
                return client.clone();
            }
            if let Some(client) = self.additional.get(&id) {
                return client.clone();
            }
            let built = initialize(&self.request_budget);
            self.additional.insert(id, built.clone());
            return built;
        }

        let built = initialize(&self.request_budget);
        self.first = Some((id, built.clone()));
        built
    }

    /// Opens (or reuses) the operator for the already-resolved remote
    /// `id` and bundles it into a [`RemoteHandle`]. Only callable through
    /// `&mut self`, exactly like [`Self::open`] -- workers only ever
    /// receive and use the returned handle. Takes only `id`, not a name
    /// A caller resolving an explicit `--remote`
    /// name first goes through [`RemoteCatalog::resolve`] to get an `id`,
    /// so this never re-resolves a name string itself and cannot be
    /// handed a mismatched id/name pair.
    pub(crate) fn open_handle(
        &mut self,
        catalog: &RemoteCatalog,
        id: RemoteId,
        progress: Option<&ProgressHandle>,
    ) -> Result<RemoteHandle> {
        let client = self.open(catalog, id, progress)?;
        Ok(RemoteHandle { id, client })
    }

    fn build(
        catalog: &RemoteCatalog,
        id: RemoteId,
        request_budget: &gat_io::RemoteRequestBudget,
        progress: Option<&ProgressHandle>,
        resolver: &gat_io::TemplateResolver,
        options: gat_core::settings::NetworkOptions,
    ) -> Result<gat_io::RemoteClient> {
        let result = gat_io::RemoteClient::open_with_request_budget(
            catalog.url(id).as_template_str(),
            Some(request_budget),
            resolver,
            options,
        );
        let client = result.map_err(|source| RemoteSessionError {
            source: Arc::new(crate::remote_open::RemoteOpenError::from_io(
                catalog.url(id),
                source,
            )),
        })?;
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_remote_open(catalog.name(id).as_ref());
        if client.requires_readiness() {
            let budget = options.readiness_timeout.duration();
            if let Some(progress) = progress {
                progress.set_activity(ProgressActivity::Connecting);
            }
            let checked = tokio::runtime::Handle::current().block_on(client.check(budget));
            if let Some(progress) = progress {
                progress.set_activity(ProgressActivity::CheckingRemote);
            }
            checked.map_err(|source| RemoteSessionError {
                source: Arc::new(crate::remote_open::RemoteOpenError::from_io(
                    catalog.url(id),
                    source.into(),
                )),
            })?;
        }
        Ok(client)
    }

    pub(crate) fn validate(
        catalog: &RemoteCatalog,
        id: RemoteId,
        resolver: &gat_io::TemplateResolver,
    ) -> Result<()> {
        gat_io::RemoteClient::validate(catalog.url(id).as_template_str(), resolver).map_err(
            |source| RemoteSessionError {
                source: Arc::new(crate::remote_open::RemoteOpenError::from_io(
                    catalog.url(id),
                    source,
                )),
            },
        )
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    #[cfg(test)]
    use super::*;
    #[cfg(test)]
    use gat_core::config::RemotesConfig;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    thread_local! {
        static REMOTE_OPENS: Cell<usize> = const { Cell::new(0) };
        static REMOTE_OPENS_BY_NAME: RefCell<HashMap<String, usize>> =
            RefCell::new(HashMap::new());
    }

    /// One remote operator actually initialized by [`super::RemoteSession`] (a
    /// pool cache hit does not record one). A well-behaved command observes
    /// at most one per distinct remote name it uses, regardless of how many
    /// obligations/objects route to that remote.
    pub fn record_remote_open(name: &str) {
        REMOTE_OPENS.with(|c| c.set(c.get() + 1));
        REMOTE_OPENS_BY_NAME.with(|m| {
            *m.borrow_mut().entry(name.to_string()).or_insert(0) += 1;
        });
    }

    pub fn remote_opens() -> usize {
        REMOTE_OPENS.with(Cell::get)
    }

    /// The number of times the remote operator named `name` has actually
    /// been initialized (a pool cache hit does not count), independent of
    /// any other remote. Lets a test prove not just an aggregate open
    /// count but that a *specific* effective remote (e.g. one selected by
    /// a route) is the one that was opened -- and that another configured
    /// remote never was.
    #[must_use]
    pub fn remote_open_count_for(name: &str) -> usize {
        REMOTE_OPENS_BY_NAME.with(|m| m.borrow().get(name).copied().unwrap_or(0))
    }

    /// Real, working [`RemoteHandle`] construction for tests outside this
    /// module (e.g. [`super::super::remote_executor`]'s scheduling tests)
    /// that need genuine handles keyed by distinct [`RemoteId`]s but have
    /// no interest in which backend they open -- only that every handle
    /// came from one real `open_handle` call, never a hand-built struct
    /// literal.
    ///
    /// Opens one real [`RemoteHandle`] per distinct name in `names` (all
    /// pointing at the same throwaway `file://` root, since these tests
    /// only care about scheduling identity, not backend content), keeping
    /// the backing [`tempfile::TempDir`] alive alongside the handles so
    /// callers don't need their own fixture setup.
    #[cfg(test)]
    pub(crate) fn open_handles(names: &[&str]) -> (tempfile::TempDir, Vec<RemoteHandle>) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        open_handles_on_current_runtime(names)
    }

    #[cfg(test)]
    pub(crate) fn open_handles_on_current_runtime(
        names: &[&str],
    ) -> (tempfile::TempDir, Vec<RemoteHandle>) {
        let dir = tempfile::tempdir().unwrap();
        let url = gat_io::remote_file_url_for_test(dir.path());
        let by_name = names
            .iter()
            .map(|n| {
                (
                    gat_core::name::RemoteName::from_string(n.to_string()),
                    gat_core::endpoint::RemoteUrlTemplate::from_string(url.clone()).into(),
                )
            })
            .collect();
        let remotes = RemotesConfig {
            by_name,
            default: None,
        };
        let catalog = RemoteCatalog::from_config(&remotes).unwrap();
        let mut session = RemoteSession::new();
        let handles = names
            .iter()
            .map(|name| {
                let id = catalog
                    .id_of(&gat_core::name::RemoteName::from_string(name.to_string()))
                    .expect("name is in the built catalog");
                session.open_handle(&catalog, id, None).unwrap()
            })
            .collect();
        (dir, handles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::config::RemotesConfig;
    use gat_core::name::RemoteName;
    use std::collections::BTreeMap;

    fn rn(name: &str) -> RemoteName {
        RemoteName::from_string(name.to_string())
    }

    fn catalog_and_pool(url: &str) -> (RemoteCatalog, RemoteSession) {
        let remotes = RemotesConfig {
            by_name: BTreeMap::from([
                (
                    RemoteName::from_string("a".to_string()),
                    gat_core::endpoint::RemoteUrlTemplate::from_string(url.to_string()).into(),
                ),
                (
                    RemoteName::from_string("b".to_string()),
                    gat_core::endpoint::RemoteUrlTemplate::from_string(url.to_string()).into(),
                ),
            ]),
            default: Some(RemoteName::from_string("a".to_string())),
        };
        (
            RemoteCatalog::from_config(&remotes).unwrap(),
            RemoteSession::new(),
        )
    }

    /// The whole point of `RemoteSession`: reusing operators by
    /// [`RemoteId`] means a command routing many objects to the same
    /// remote initializes it once, not once per obligation.
    #[test]
    fn initializes_each_distinct_remote_name_at_most_once() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let url = gat_io::remote_file_url_for_test(dir.path());
        let (catalog, mut pool) = catalog_and_pool(&url);

        let a_id = catalog.id_of(&rn("a")).unwrap();
        let b_id = catalog.id_of(&rn("b")).unwrap();
        let before = test_support::remote_opens();
        let _ = pool.open_handle(&catalog, a_id, None).unwrap();
        let _ = pool.open_handle(&catalog, a_id, None).unwrap();
        let default_id = catalog.resolve(None).unwrap();
        let _ = pool.open(&catalog, default_id, None).unwrap();
        assert_eq!(
            pool.additional.capacity(),
            0,
            "the common single-remote operation must not allocate a hash table"
        );
        let after = test_support::remote_opens();
        assert_eq!(
            after - before,
            1,
            "repeated lookups of the same resolved remote name must reuse one operator"
        );

        let _ = pool.open_handle(&catalog, b_id, None).unwrap();
        let after_b = test_support::remote_opens();
        assert_eq!(
            after_b - before,
            2,
            "a distinct remote name must still initialize its own operator"
        );
    }

    /// Sequential coordinator-owned opens (never worker-side) still
    /// produce one shared operator instance per remote, reused across
    /// however many times the coordinator calls `open_handle` for it
    /// across many windows in the same operation.
    #[test]
    fn operator_is_reused_across_many_sequential_windows() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let url = gat_io::remote_file_url_for_test(dir.path());
        let (catalog, mut pool) = catalog_and_pool(&url);

        let a_id = catalog.id_of(&rn("a")).unwrap();
        let first = pool.open_handle(&catalog, a_id, None).unwrap();
        for _ in 0..50 {
            let op = pool.open_handle(&catalog, a_id, None).unwrap();
            assert_eq!(first.id(), op.id());
        }
    }

    #[test]
    fn readiness_failure_is_shared_across_windows_and_other_candidates_can_open() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let (catalog, mut pool) = catalog_and_pool(&gat_io::remote_file_url_for_test(dir.path()));
        let a = catalog.id_of(&rn("a")).unwrap();
        let b = catalog.id_of(&rn("b")).unwrap();
        let budget = std::time::Duration::from_millis(5);
        let first = pool
            .open_with(a, |_| {
                Err(RemoteSessionError {
                    source: Arc::new(crate::remote_open::RemoteOpenError::from_io(
                        catalog.url(a),
                        gat_io::RemoteError::ReadinessTimedOut { budget }.into(),
                    )),
                })
            })
            .unwrap_err();
        for _ in 0..50 {
            let error = pool
                .open_with(a, |_| {
                    panic!("failed readiness must not be attempted again")
                })
                .unwrap_err();
            assert!(Arc::ptr_eq(&first.source, &error.source));
            assert_eq!(
                error.kind(),
                &crate::remote_open::RemoteOpenFailureKind::ReadinessTimedOut { budget }
            );
        }
        assert!(pool.open_handle(&catalog, b, None).is_ok());
    }

    #[test]
    fn successful_initialization_is_cached_without_rechecking() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let (catalog, mut pool) = catalog_and_pool(&gat_io::remote_file_url_for_test(dir.path()));
        let id = catalog.id_of(&rn("a")).unwrap();
        pool.open(&catalog, id, None).unwrap();
        for _ in 0..50 {
            pool.open_with(id, |_| panic!("ready clients must be reused"))
                .unwrap();
        }
    }

    #[test]
    fn opening_exposes_semantic_failure_and_retains_the_physical_source() {
        let (catalog, mut pool) =
            catalog_and_pool("unsupported://host/path?token=SYNTHETIC-SECRET");
        let id = catalog.id_of(&rn("a")).unwrap();
        let error = pool.open(&catalog, id, None).unwrap_err();

        assert!(matches!(
            error.kind(),
            crate::remote_open::RemoteOpenFailureKind::DisallowedScheme
        ));
        assert_eq!(error.template(), catalog.url(id));
        assert!(!format!("{error:?}").contains("SYNTHETIC-SECRET"));
        let chain =
            std::iter::successors(std::error::Error::source(&error), |source| source.source())
                .map(ToString::to_string)
                .collect::<String>();
        assert!(!chain.contains("SYNTHETIC-SECRET"), "{chain}");
        assert!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<crate::remote_open::RemoteOpenError>())
                .is_some()
        );
        assert!(
            std::error::Error::source(&error)
                .and_then(std::error::Error::source)
                .and_then(|source| source.downcast_ref::<gat_io::OpenRemoteError>())
                .is_some()
        );
    }
}
