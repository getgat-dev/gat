//! The operation's one mutable runtime `Session`.
//!
//! [`Session`] owns exactly the mutable/lazy runtime services an operation
//! needs, kept deliberately separate from the immutable
//! [`super::snapshot::Snapshot`]: a lazily opened cache (via [`super::cache_session::CacheSession`]),
//! remote operator reuse ([`super::remote_session::RemoteSession`]), the
//! remote-I/O executor, and typed [`ExecutionLimits`]. It owns no
//! repository, config, or desired-state reference -- those live on
//! [`super::snapshot::Snapshot`] and
//! [`super::operation::Operation`] and the higher-level desired operation
//! instead.
//!
//! Constructing a session performs no side-effecting resource
//! initialization: no `cache.sqlite3` open, no remote operator creation. Both stay lazy.
//!
//! Cache access takes `&mut self`: the lazily opened `CacheClient` is ordinary owned
//! state, not an `AtomicBool`/`RefCell` interior-mutability convention, so
//! the coordinator thread must hold `&mut Session`/`&mut Operation` to use
//! them.

use super::cache_session::CacheSession;
use super::limits::ExecutionLimits;
use super::remote_session::RemoteSession;

/// The operation's one mutable runtime session:
/// remote operator reuse, the remote-I/O executor, typed execution
/// limits, and the operation's [`CacheSession`].
pub(crate) struct Session {
    remotes: RemoteSession,
    remote_executor: super::remote_executor::RemoteExecutor,
    limits: ExecutionLimits,
    cache: CacheSession,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub(crate) fn transfer_cancellation(&self) -> crate::TransferCancellation {
        self.remote_executor.cancellation()
    }
    /// Builds a session using production [`ExecutionLimits`].
    /// Side-effect free until cache or remote services are used. Remote URLs
    /// are resolved through the operation's snapshot catalog.
    pub(crate) fn new() -> Self {
        Self::with_limits(ExecutionLimits::production())
    }

    /// As [`Self::new`], but with an explicit [`ExecutionLimits`] (e.g. a
    /// test constructing deliberately small window/concurrency bounds).
    pub(crate) fn with_limits(limits: ExecutionLimits) -> Self {
        let remote_executor = super::remote_executor::RemoteExecutor::new(limits.remote);
        let request_budget = remote_executor.request_budget();
        Self {
            remotes: RemoteSession::with_request_budget(request_budget),
            remote_executor,
            limits,
            cache: CacheSession::default(),
        }
    }

    /// The operation's typed execution-resource bounds: transfer
    /// window size and remote-I/O concurrency, resolved once alongside the
    /// rest of the session.
    pub(crate) const fn limits(&self) -> &ExecutionLimits {
        &self.limits
    }

    /// The operation's [`CacheSession`]:
    /// lets a caller that only needs cache access (not the other
    /// remote/executor/limits services `split_mut` also exposes) reach it
    /// directly, e.g. sync's reconciliation phases reading/publishing
    /// through the one shared [`gat_io::CacheClient`]
    /// rather than opening a second, independent one.
    pub(crate) const fn cache_session_mut(&mut self) -> &mut CacheSession {
        &mut self.cache
    }

    /// Splits this session into disjoint read-only remote/executor/limits
    /// borrows alongside a mutable [`CacheSession`] borrow, all at once. A
    /// caller that needs to both call
    /// into the mutable `CacheSession` (registering cache use, opening
    /// the shared `CacheClient`) *and* read the other session services
    /// (to open a remote operator, dispatch through the executor, ...)
    /// from the same nested closure cannot do so through sequential
    /// `&self`/`&mut self` method calls -- each would borrow the whole
    /// `Session`. Implemented here, inside this type's own `impl` block,
    /// where the four fields are visibly disjoint to the borrow checker.
    pub(crate) const fn split_mut(
        &mut self,
    ) -> (
        &mut RemoteSession,
        &super::remote_executor::RemoteExecutor,
        &ExecutionLimits,
        &mut CacheSession,
    ) {
        (
            &mut self.remotes,
            &self.remote_executor,
            &self.limits,
            &mut self.cache,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructing a session must not create any remote operator.
    #[test]
    fn construction_performs_no_remote_or_cache_side_effects() {
        let remote_opens_before = crate::remote_session::test_support::remote_opens();
        let mut session = Session::new();
        let remote_opens_after = crate::remote_session::test_support::remote_opens();

        assert_eq!(remote_opens_after, remote_opens_before);
        // Service splitting remains side-effect free.
        let _ = session.split_mut();
        let _ = session.limits();
    }

    #[test]
    fn cache_opens_the_proof_database_at_most_once_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join(".gat/cache/objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let layout = gat_io::RepositoryLayout::at(tmp.path().to_path_buf());
        let cache_root = layout.resolve_cache_root(Some(objects_dir.as_os_str()), None);

        let mut session = Session::new();
        let before = gat_io::cache_proof_test_support::snapshot().cache_db_opens;

        // Simulate several independent phases of one composite operation
        // (e.g. push verification, then fetch verification, then a
        // repair/sync pass) all reaching for the shared cache.
        for _ in 0..5 {
            session
                .cache
                .sync_scoped_cache(&cache_root, |_cache| -> gat_io::CacheResult<()> { Ok(()) })
                .expect("cache access must succeed");
        }

        let after = gat_io::cache_proof_test_support::snapshot().cache_db_opens;
        assert_eq!(
            after - before,
            1,
            "one session must open cache.sqlite3 exactly once regardless of how many phases use it"
        );
    }
}

#[cfg(test)]
mod structural_tests {
    use super::Session;

    /// Worker closures dispatched onto
    /// `spawn_blocking`/`tokio::spawn` must never receive `Session`
    /// itself -- only the coordinator thread may initialize/use its
    /// mutable runtime services (remote operator construction and the lazy `CacheClient`). `Session` remains `!Sync` (`RemoteSession`'s internal
    /// map is not exposed for concurrent mutation either), so it still
    /// cannot be captured by shared reference into any `Send` closure/
    /// closure. This test pins that invariant.
    fn assert_not_sync<T: ?Sized>() {
        struct Check<T: ?Sized>(std::marker::PhantomData<T>);
        #[allow(dead_code)]
        trait AmbiguousIfSync<A> {
            fn some_item() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for Check<T> {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for Check<T> {}
        let _ = <Check<T> as AmbiguousIfSync<_>>::some_item;
    }

    #[test]
    fn session_is_not_sync_so_worker_closures_cannot_capture_it() {
        assert_not_sync::<Session>();
    }
}
