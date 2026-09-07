//! Read-only cache-presence inspection without opening the cache proof index.

use gat_core::oid::Oid;
use gat_io::CachePresence;

/// Repository-bound content-presence checks for status-like read paths.
///
/// The physical object directory remains engine-private. Checks use the
/// existing presence-only I/O capability and deliberately do not open
/// `cache.sqlite3` or verify object contents.
pub struct CachePresenceSession {
    cache: CachePresence,
}

impl CachePresenceSession {
    pub(crate) fn new(repo: &crate::Repository) -> Self {
        let cache_root = repo.resolved_cache_root();
        Self {
            cache: cache_root.presence(),
        }
    }

    /// Whether the object path currently exists.
    ///
    /// Invalid physical cache paths are treated as absent, preserving the
    /// advisory behavior of status cache annotation.
    #[must_use]
    pub fn contains(&self, oid: &Oid) -> bool {
        self.cache.contains(oid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_checks_do_not_open_the_cache_proof_database() {
        let tmp = crate::test_harness::test_repo();
        let repo = crate::Repository::at(tmp.path().to_path_buf());
        let before = gat_io::cache_proof_test_support::snapshot().cache_db_opens;

        let session = CachePresenceSession::new(&repo);
        assert!(!session.contains(&Oid::from_hex(&"0".repeat(64)).unwrap()));

        let after = gat_io::cache_proof_test_support::snapshot().cache_db_opens;
        assert_eq!(after, before);
    }
}
