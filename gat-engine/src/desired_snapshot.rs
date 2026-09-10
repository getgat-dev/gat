//! Explicit desired-state snapshot type.
//!
//! Commands that need to read current desired rows (a route/ownership
//! planner like `push`/`fetch`) should hold a [`DesiredSnapshot`] rather than
//! an `Option<StateStore>` buried inside the general operation
//! context: a command that genuinely has no desired-state store (`gat
//! sync`'s own dry-run-safe acquisition path)
//! should simply not hold a `DesiredSnapshot` value at all, rather than
//! holding a context whose desired-state accessor can be called and panic.
//!
//! `DesiredSnapshot` is captured from the same repository generation as the
//! [`crate::snapshot::Snapshot`] built alongside it: both are
//! read inside one [`gat_io::RepoLock`] acquisition, so they always
//! describe the same `gat.lock` generation.
//!
//! Constructs no `Selection` or glob errors of its own:
//! [`DesiredView::visit_entries`]/[`DesiredSnapshot::with_store`] are
//! generic closure plumbing over [`StateStore`]'s already-typed
//! `SQLite` errors, independent of whichever
//! `Selection`/`GlobFilter` the caller applied to narrow `query` before
//! calling in. [`DesiredSnapshot::with_store`] returns the caller's
//! closure result typed directly as `StateStoreError` (it is only used
//! from test code, which never needs to widen it into a command's own
//! error enum); production callers should prefer [`DesiredSnapshot::view`]
//! instead.

use crate::repository::Repository as Repo;
use gat_core::lock::Entry;
use gat_core::selection::Selection;
use gat_io::{DesiredQuery, StateStore, StateStoreError};

/// The desired-state (`SQLite`) mirror captured for one operation, refreshed
/// against the current on-disk `gat.lock` in the same repository-
/// synchronization barrier that captured the paired
/// [`crate::snapshot::Snapshot`].
pub struct DesiredSnapshot {
    store: StateStore,
}

impl DesiredSnapshot {
    /// Wrap an already-opened, already-refreshed desired-state store. Side-
    /// effect free: this does not itself open or refresh anything.
    #[doc(hidden)]
    pub(crate) const fn new(store: StateStore) -> Self {
        Self { store }
    }

    /// A narrow, read-only [`DesiredView`] over this snapshot's store:
    /// selection/reconciliation callers that only
    /// legitimately need to visit current desired rows should go through
    /// this rather than [`Self::with_store`], which still hands out the
    /// full `StateStore` (including its materialized-ledger/write
    /// API) to any caller that happens to close over it.
    pub(crate) const fn view(&self) -> DesiredView<'_> {
        DesiredView { store: &self.store }
    }

    #[cfg(test)]
    pub(crate) fn desired_fingerprint(&self) -> std::result::Result<[u8; 32], StateStoreError> {
        self.store.desired_fingerprint()
    }
}

/// A read-only view over one operation's desired-state mirror, exposing only
/// the query operations selection and
/// route/ownership planning legitimately need: visiting desired rows for a
/// selection/scope, and reading the desired-generation fingerprint for
/// diagnostics. Keeps [`StateStore`]'s materialized-ledger reads and
/// every desired/materialized *write* API (`upsert_entries`, `apply_batch`,
/// `move_prefix`, ...) unreachable through this type -- a caller that holds
/// only a `DesiredView` cannot mutate or read unrelated state-store state,
/// even though the underlying connection could.
#[derive(Clone, Copy)]
pub struct DesiredView<'a> {
    store: &'a StateStore,
}

impl<'a> DesiredView<'a> {
    /// Wrap an already-open store's desired-state reads. Exposed for the
    /// few call sites (tests, `gat status`) that build a view directly
    /// from a freshly refreshed store rather than through a
    /// [`DesiredSnapshot`].
    const fn new(store: &'a StateStore) -> Self {
        Self { store }
    }

    /// Streams semantic desired entries matching `selection` into `visit`
    /// in path order without exposing the state store's query or cursor
    /// row types.
    pub fn visit_entries<E>(
        &self,
        selection: &Selection,
        mut visit: impl FnMut(Entry) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E>
    where
        E: From<crate::RepositoryStateError>,
    {
        enum Bridge<E> {
            State(StateStoreError),
            Callback(E),
        }

        impl<E> From<StateStoreError> for Bridge<E> {
            fn from(error: StateStoreError) -> Self {
                Self::State(error)
            }
        }

        self.store
            .with_desired_rows(DesiredQuery::for_selection(selection), |mut rows| {
                while let Some(row) = rows.next()? {
                    visit(Entry {
                        path: row.path,
                        oid: row.oid,
                    })
                    .map_err(Bridge::Callback)?;
                }
                Ok(())
            })
            .map_err(|error| match error {
                Bridge::State(error) => E::from(crate::RepositoryStateError::read(error)),
                Bridge::Callback(error) => error,
            })
    }
}

/// Streams semantic entries from one freshly refreshed desired-state mirror
/// through an engine-owned error surface.
pub(crate) fn visit_current_desired_entries(
    repo: &Repo,
    selection: &Selection,
    mut visit: impl FnMut(Entry),
) -> Result<(), crate::RepositoryStateError> {
    let mut store = StateStore::open(repo.layout()).map_err(crate::RepositoryStateError::open)?;
    crate::workspace::sync::refresh_desired_index(repo, &mut store)
        .map_err(crate::RepositoryStateError::refresh)?;
    DesiredView::new(&store).visit_entries::<crate::RepositoryStateError>(selection, |entry| {
        visit(entry);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`DesiredView::visit_rows`] must
    /// observe the same rows a direct [`StateStore::with_desired_rows`]
    /// call would, proving the narrower view is not silently missing rows.
    #[test]
    fn view_visits_the_same_rows_as_the_underlying_store() {
        use gat_core::lock::Entry;

        let tmp = crate::test_harness::git_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut store = StateStore::open(repo.layout()).expect("open store");
        store
            .upsert_desired_for_test(
                &[Entry {
                    path: gat_core::lexical_path::GatPath::parse_canonical("a.bin").unwrap(),
                    oid: gat_core::oid::Oid::from_hex(&"0".repeat(64)).unwrap(),
                }],
                gat_core::lock::LockShardLevels::FLAT,
            )
            .unwrap();

        let snapshot = DesiredSnapshot::new(store);
        let mut seen = Vec::new();
        snapshot
            .view()
            .visit_entries(&Selection::default(), |row| {
                seen.push(row.path);
                Ok::<(), crate::RepositoryStateError>(())
            })
            .unwrap();
        assert_eq!(seen, vec!["a.bin".to_string()]);
    }
}
