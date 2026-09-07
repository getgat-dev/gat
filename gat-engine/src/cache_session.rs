//! Operation-scoped lazy cache access.
//!
//! The optional proof-index client is coordinator-owned mutable state, reused
//! across all phases of an operation.

use gat_core::lexical_path::GatPath;
use gat_core::oid::Oid;
use gat_io::{
    CacheClient, CacheError, CacheObject, CachePublication, CacheResult, CacheRoot,
    CacheVerificationFailure, CompletedCacheVerification, DesiredStateSession, IngestStrategy,
    MaterializationPreparationError, ObjectVerification, PreparedCacheVerification,
    PreparedMaterialization,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum CacheVerificationError {
    #[error(transparent)]
    Verification(#[from] CacheVerificationFailure),
    #[error("cache verification worker did not complete")]
    Worker(#[from] tokio::task::JoinError),
}

impl CacheVerificationError {
    pub(crate) const fn oid(&self) -> Option<Oid> {
        match self {
            Self::Verification(source) => Some(source.oid()),
            Self::Worker(_) => None,
        }
    }
}

/// One operation's lazily opened,
/// shared [`CacheClient`].
#[derive(Default)]
pub struct CacheSession {
    /// Lazy, coordinator-thread-only [`gat_io::CacheClient`]: the
    /// operation's single proof-index connection, opened at most once
    /// across the whole command on first genuine use, shared by every
    /// phase (push/fetch verification, materialization, repair) that
    /// needs it rather than each phase opening (and threading through
    /// `&mut Option<..>`) its own.
    cache: Option<CacheClient>,
}

impl CacheSession {
    /// Open the proof client lazily and reuse it across all operation phases.
    fn cache(&mut self, cache_root: &CacheRoot) -> &CacheClient {
        self.cache.get_or_insert_with(|| cache_root.open_client())
    }

    /// Reconciliation needs scoped access to the shared proof client while
    /// streaming its plan. Other workflows use the named capabilities below.
    pub(crate) fn sync_scoped_cache<T>(
        &mut self,
        cache_root: &CacheRoot,
        f: impl FnOnce(&CacheClient) -> T,
    ) -> T {
        f(self.cache(cache_root))
    }

    /// Prepare owned cache-verification work on the coordinator.
    pub(crate) fn prepare_verification(
        &mut self,
        cache_root: &CacheRoot,
        oids: &[Oid],
    ) -> PreparedCacheVerification {
        self.cache(cache_root).prepare_verification(oids)
    }

    /// Commit completed cache verification to the operation-scoped memo.
    pub(crate) fn commit_verification(
        &mut self,
        cache_root: &CacheRoot,
        completed: CompletedCacheVerification,
    ) -> Vec<ObjectVerification> {
        self.cache(cache_root).commit_verification(completed)
    }

    /// Create a lazy reader without exposing physical paths.
    pub(crate) fn object_source(&mut self, cache_root: &CacheRoot, oid: &Oid) -> CacheObject {
        self.cache(cache_root).object(oid)
    }

    /// Verifies callers' already globally deduplicated `oids` without
    /// cross-window memoization -- an explicit, named unmemoized
    /// verification operation instead of every such caller reaching for
    /// the generic private cache escape hatch itself. The callback returns
    /// any opaque cache publications produced while handling that window;
    /// this session applies them immediately through the same client
    /// before verification advances to the next window. That keeps proof
    /// persistence coordinator-owned without exposing the client itself.
    pub fn verify_windows_unmemoized<E: From<CacheError>>(
        &mut self,
        cache_root: &CacheRoot,
        oids: &[Oid],
        mut on_window: impl FnMut(
            &[Oid],
            &[ObjectVerification],
        ) -> std::result::Result<Vec<CachePublication>, E>,
    ) -> std::result::Result<(), E> {
        let cache = self.cache(cache_root);
        cache.verify_windows_unmemoized(oids, |oids, status| {
            let publications = on_window(oids, status)?;
            // Best-effort: publications accelerate later verification;
            // the object bytes are already durably present.
            let _ = cache.apply_publications(&publications);
            Ok(())
        })
    }

    /// Apply a batch of coordinator-collected opaque cache publications
    /// through this session's shared
    /// underlying object cache -- an explicit, named publication
    /// operation instead of every applying phase (fetch/repair) reaching
    /// for the generic private cache escape hatch itself. Workers
    /// producing bytes never open the proof DB themselves (see
    /// `gat-io`'s delta-ingest/publication capabilities); they only return deltas for
    /// the coordinator to apply here, in one bounded, set-based
    /// transaction per window, through the operation's single cache
    /// connection.
    pub fn apply_publications(
        &mut self,
        cache_root: &CacheRoot,
        publications: &[CachePublication],
    ) -> CacheResult<()> {
        self.cache(cache_root).apply_publications(publications)
    }

    /// Ingest one bounded add batch through this operation's shared proof
    /// client. Worker-side file ingestion remains database-free; the I/O
    /// session consumes opaque publication receipts on the coordinator.
    pub(crate) fn ingest_materializations(
        &mut self,
        state: &DesiredStateSession<'_>,
        cache_root: &CacheRoot,
        files: &[GatPath],
        strategy: IngestStrategy,
        on_progress: impl Fn(&GatPath, Option<u8>) + Sync,
        on_complete: impl Fn() + Sync,
    ) -> Result<Vec<PreparedMaterialization>, MaterializationPreparationError> {
        state.ingest_materializations(
            self.cache(cache_root),
            files,
            strategy,
            crate::repository_mutation::LARGE_FILE_PROGRESS_THRESHOLD,
            on_progress,
            on_complete,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constructing a session (which owns a default `CacheSession`) must
    /// not create any remote operator.
    #[test]
    fn default_performs_no_side_effects() {
        let remote_opens_before = crate::remote_session::test_support::remote_opens();
        let _session = CacheSession::default();
        let _unrelated = crate::session::Session::new();
        let remote_opens_after = crate::remote_session::test_support::remote_opens();

        assert_eq!(remote_opens_after, remote_opens_before);
    }

    /// Cache access across phases opens the proof database at most once.
    #[test]
    fn cache_opens_the_proof_database_at_most_once_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let objects_dir = tmp.path().join(".gat/cache/objects");
        std::fs::create_dir_all(&objects_dir).unwrap();
        let layout = gat_io::RepositoryLayout::at(tmp.path().to_path_buf());
        let cache_root = layout.resolve_cache_root(Some(objects_dir.as_os_str()), None);

        let mut session = CacheSession::default();
        let before = gat_io::cache_proof_test_support::snapshot().cache_db_opens;

        for _ in 0..5 {
            session.cache(&cache_root);
        }

        let after = gat_io::cache_proof_test_support::snapshot().cache_db_opens;
        assert_eq!(
            after - before,
            1,
            "one session must open cache.sqlite3 exactly once regardless of how many phases use it"
        );
    }
}
