//! Local content-addressed object cache and shared proof persistence.

pub(crate) mod enumeration;
pub(crate) mod layout;
pub(crate) mod maintenance;
pub(crate) mod object;
pub(crate) mod proof;
pub(crate) mod root;

pub use root::CacheRoot;

pub use enumeration::{CacheEnumerationError, CacheSweepDecision, CacheSweepStats};
pub use layout::{OBJECT_HASH_NAMESPACE, object_key_oid, parse_object_key};
pub use maintenance::{
    CacheDatabaseHealth, CacheDatabaseUnreadable, CacheMaintenance, CacheMaintenanceError,
};
pub use object::{
    CacheClient, CacheError, CacheIngest, CacheObject, CacheObjectOpenError, CacheObjectOpenStage,
    CacheObjectReader, CachePresence, CacheVerificationFailure, CacheWriter,
    CompletedCacheVerification, DEFAULT_INGEST_STRATEGY, ExpectedIngest, IngestStrategy, Ingested,
    PreparedCacheVerification, Result, VERIFY_WINDOW,
};
pub use proof::{CacheProofError, CacheProofErrorKind, CachePublication, ObjectVerification};

#[cfg(any(test, feature = "test-support"))]
pub use object::{
    hash_file_call_count, race_test_hooks, reset_sync_all_call_count, sync_all_call_count,
    test_support as object_test_support, with_exclusive_hash_file_call_count,
};

#[cfg(any(test, feature = "test-support"))]
pub use proof::test_support as proof_test_support;

#[cfg(any(test, feature = "test-support"))]
pub use enumeration::test_support as enumeration_test_support;

#[cfg(test)]
pub(crate) fn seed_cache_proof_for_test(
    objects_dir: &std::path::Path,
    oid: &gat_core::oid::Oid,
    proof: &crate::file_state::StatProof,
) {
    let state = proof::CacheState::open_for_test(objects_dir);
    state.upsert(oid, proof).expect("cache proof is seeded");
}

#[cfg(test)]
pub(crate) fn cache_has_proof_for_test(
    objects_dir: &std::path::Path,
    oid: &gat_core::oid::Oid,
) -> bool {
    let state = proof::CacheState::open_for_test(objects_dir);
    state
        .lookup(oid)
        .expect("cache proof lookup succeeds")
        .is_some()
}
