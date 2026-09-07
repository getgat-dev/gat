//! Typed, independently-tunable execution-resource limits.
//!
//! Every bound an operation-scoped command enforces on its own transient
//! work -- how many transfer obligations/repair oids/sync actions it keeps
//! in memory at once, and how much remote-I/O concurrency it allows
//! globally and per remote -- is a distinct resource. Retuning one must
//! never implicitly retune another, and none of these are the same knob as the
//! storage layer's own `gat-io` verification window /
//! proof transaction chunk, which protect SQL/proof-DB
//! batching, not operation execution memory, and stay owned by that layer.
//!
//! Every field is a [`NonZeroUsize`]: each one
//! is used somewhere as a `.chunks(n)`/`Semaphore::new(n)` divisor or
//! capacity where a `0` would either panic or silently mean "unbounded" to
//! one consumer and "nothing runs" to another. Making zero unrepresentable
//! at the type level in the public fields removes
//! the need for scattered `.max(1)` normalization at every call site that
//! would otherwise have to defend against a caller-supplied zero.
//!
//! Fields are grouped by resource domain:
//! [`TransferLimits`], [`SyncLimits`], [`RemoteLimits`]. With several
//! identically-typed `NonZeroUsize` fields, a flat struct/constructor
//! invites accidentally swapping two positional arguments of the same
//! type (e.g. passing `remote_per_remote` where `remote_global` was
//! meant); grouping by domain both makes such a swap a type error across
//! groups and lets each consumer (the transfer/repair windowing code, the
//! sync reconciliation loop, the remote executor) depend on only the
//! narrow sub-struct it actually uses, while `Session` still keeps one
//! top-level immutable `ExecutionLimits` value.
use std::num::NonZeroUsize;

/// How many transfer obligations (push upload / fetch download / repair)
/// or repair oids an operation windows through the object cache / remote
/// executor at once, before flushing that window and moving on to the
/// next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferLimits {
    /// Shared bound for transfer and repair windows. Both operations use the
    /// same limit and remain independently bounded from other resources.
    pub window: NonZeroUsize,
}

/// Independently-tunable sync/reconciliation resource bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncLimits {
    /// How many dirty/pending rows a bounded, re-runnable mutating
    /// `Validation::TrustState` sync action batch processes at once.
    /// Independent of [`Self::merge_window`] below; retuning one must never
    /// implicitly retune the other.
    pub dirty_window: NonZeroUsize,
    /// How many buffered desired/materialized merge rows (resolved or
    /// still cache-pending) reconciliation planning's
    /// `MergeBuffer` accumulates before draining them to the sink,
    /// independent of [`Self::dirty_window`] and the cache verification
    /// window owned by `gat-io`.
    pub merge_window: NonZeroUsize,
}

/// One global and per-remote concurrency budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteConcurrency {
    pub global: NonZeroUsize,
    pub per_remote: NonZeroUsize,
}

impl RemoteConcurrency {
    fn new(global: usize, per_remote: usize, name: &str) -> Self {
        Self {
            global: NonZeroUsize::new(global)
                .unwrap_or_else(|| panic!("{name}_global must be positive")),
            per_remote: NonZeroUsize::new(per_remote)
                .unwrap_or_else(|| panic!("{name}_per_remote must be positive")),
        }
    }
}

/// Independently-tunable remote-I/O concurrency bounds by work class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteLimits {
    /// Lightweight object-presence probes.
    pub presence: RemoteConcurrency,
    /// Object reads, writes, and repairs.
    pub transfer: RemoteConcurrency,
    /// Aggregate physical HTTP requests shared by all network remotes in
    /// one operation, beneath logical object-job concurrency.
    pub physical_requests: NonZeroUsize,
}

/// Work-admission bounds shared by local and remote garbage collection.
/// Reachability and remote candidates use memory proportional to unique OIDs;
/// these limits do not impose a total memory bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcLimits {
    /// Number of explicit peer repositories inspected concurrently during
    /// garbage collection.
    pub repository_concurrency: NonZeroUsize,
    /// Limits specific to remote garbage collection.
    pub remote: RemoteGcLimits,
}

/// Bounds for remote garbage-collection work that does not run through an
/// operation's desired-state session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteGcLimits {
    /// Aggregate physical HTTP requests for the remote-GC client.
    pub physical_requests: NonZeroUsize,
}

impl Default for GcLimits {
    fn default() -> Self {
        Self {
            repository_concurrency: NonZeroUsize::new(4)
                .expect("remote repository concurrency must be positive"),
            remote: RemoteGcLimits::default(),
        }
    }
}

impl Default for RemoteGcLimits {
    fn default() -> Self {
        Self {
            physical_requests: NonZeroUsize::new(256)
                .expect("remote physical request limit must be positive"),
        }
    }
}

/// Execution bounds with named, nonzero fields for each resource domain.
/// Customize fields on [`Self::default`] to retain unrelated production bounds.
/// Raw positional construction is reserved for test fixtures.
///
/// ```compile_fail
/// use gat_engine::ExecutionLimits;
/// let _ = ExecutionLimits::with_remote_limits(0, 1, 1, ExecutionLimits::default().remote);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionLimits {
    pub transfer: TransferLimits,
    pub sync: SyncLimits,
    pub remote: RemoteLimits,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self::with_remote_limits(
            1024,
            10_000,
            4096,
            RemoteLimits {
                presence: RemoteConcurrency::new(256, 128, "remote_presence"),
                transfer: RemoteConcurrency::new(256, 128, "remote_transfer"),
                physical_requests: NonZeroUsize::new(256)
                    .expect("physical request limit must be positive"),
            },
        )
    }
}

impl ExecutionLimits {
    /// Test fixture convenience for window and concurrency bounds. Production
    /// callers customize the named, nonzero fields instead. The global budget
    /// independently caps aggregate work, including when a per-remote limit
    /// is larger than the global limit.
    ///
    /// # Panics
    /// Panics if any window or concurrency limit is zero.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        transfer_window: usize,
        sync_dirty_window: usize,
        sync_merge_window: usize,
        remote_global: usize,
        remote_per_remote: usize,
    ) -> Self {
        Self {
            transfer: TransferLimits {
                window: NonZeroUsize::new(transfer_window)
                    .expect("transfer_window must be positive"),
            },
            sync: SyncLimits {
                dirty_window: NonZeroUsize::new(sync_dirty_window)
                    .expect("sync_dirty_window must be positive"),
                merge_window: NonZeroUsize::new(sync_merge_window)
                    .expect("sync_merge_window must be positive"),
            },
            remote: RemoteLimits {
                presence: RemoteConcurrency::new(
                    remote_global,
                    remote_per_remote,
                    "remote_presence",
                ),
                transfer: RemoteConcurrency::new(
                    remote_global,
                    remote_per_remote,
                    "remote_transfer",
                ),
                physical_requests: NonZeroUsize::new(128)
                    .expect("physical request limit must be positive"),
            },
        }
    }

    /// # Panics
    /// Panics if any window size is zero.
    #[must_use]
    const fn with_remote_limits(
        transfer_window: usize,
        sync_dirty_window: usize,
        sync_merge_window: usize,
        remote: RemoteLimits,
    ) -> Self {
        Self {
            transfer: TransferLimits {
                window: NonZeroUsize::new(transfer_window)
                    .expect("transfer_window must be positive"),
            },
            sync: SyncLimits {
                dirty_window: NonZeroUsize::new(sync_dirty_window)
                    .expect("sync_dirty_window must be positive"),
                merge_window: NonZeroUsize::new(sync_merge_window)
                    .expect("sync_merge_window must be positive"),
            },
            remote,
        }
    }

    /// Production defaults for each resource limit.
    #[must_use]
    pub fn production() -> Self {
        Self::default()
    }

    /// Deliberately tiny limits for structural tests that need to force
    /// multiple windows / concurrency bounds without huge fixtures.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    #[allow(
        clippy::missing_panics_doc,
        reason = "All test limits are positive constants"
    )]
    pub fn tiny() -> Self {
        Self::with_remote_limits(
            2,
            2,
            2,
            RemoteLimits {
                presence: RemoteConcurrency::new(2, 1, "remote_presence"),
                transfer: RemoteConcurrency::new(2, 1, "remote_transfer"),
                physical_requests: NonZeroUsize::new(2)
                    .expect("physical request limit must be positive"),
            },
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_defaults_match_the_configured_defaults() {
        let limits = ExecutionLimits::production();
        assert_eq!(limits.transfer.window.get(), 1024);
        assert_eq!(limits.sync.dirty_window.get(), 10_000);
        assert_eq!(limits.sync.merge_window.get(), 4096);
        assert_eq!(limits.remote.presence.global.get(), 256);
        assert_eq!(limits.remote.presence.per_remote.get(), 128);
        assert_eq!(limits.remote.transfer.global.get(), 256);
        assert_eq!(limits.remote.transfer.per_remote.get(), 128);
        assert_eq!(limits.remote.physical_requests.get(), 256);
    }

    #[test]
    fn tiny_limits_are_small_and_distinct_per_field() {
        let limits = ExecutionLimits::tiny();
        assert!(limits.transfer.window < ExecutionLimits::production().transfer.window);
        assert!(limits.remote.presence.global.get() >= 1);
        assert!(limits.remote.presence.per_remote.get() >= 1);
        assert!(limits.remote.transfer.global.get() >= 1);
        assert!(limits.remote.transfer.per_remote.get() >= 1);
        assert!(
            limits.remote.physical_requests
                < ExecutionLimits::production().remote.physical_requests
        );
        assert!(GcLimits::default().repository_concurrency.get() >= 1);
    }

    #[test]
    #[should_panic(expected = "transfer_window must be positive")]
    fn fixture_rejects_a_zero_transfer_window_instead_of_silently_normalizing_it() {
        let _ = ExecutionLimits::for_test(0, 2, 2, 2, 1);
    }

    #[test]
    #[should_panic(expected = "remote_presence_per_remote must be positive")]
    fn fixture_rejects_a_zero_remote_per_remote_limit_instead_of_silently_normalizing_it() {
        let _ = ExecutionLimits::for_test(2, 2, 2, 2, 0);
    }

    /// Every
    /// non-test call site of `ExecutionLimits::production()` in `src/` must
    /// be either the one place a top-level operation session is actually
    /// constructed ([`super::session::Session::new`]) or
    /// a standalone, session-free entry point that has no
    /// `Session`/`Operation` to source limits from at all
    /// (`engine::workspace::sync::plan::plan`). A production call site that constructs
    /// its own `ExecutionLimits::production()` *after* a session already
    /// exists for the same operation would silently duplicate/diverge from
    /// the session's own resolved limits -- this test greps the whole
    /// source tree for `ExecutionLimits::production()` call sites and
    /// fails if a new, unreviewed one appears outside the known-allowed
    /// list, forcing a human to explicitly extend the allowlist (and think
    /// about why) rather than silently growing a second source of limits.
    #[test]
    fn production_limits_are_constructed_only_at_known_session_free_or_session_construction_sites()
    {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("gat-engine is a workspace member");
        let allowed_non_test_sites: &[&str] = &[
            // The one production session-construction site.
            "gat-engine/src/session.rs",
            // Session-free standalone entry points/diagnostics that have no
            // Session to source limits from.
            "gat-engine/src/workspace/sync/plan.rs",
        ];
        let mut offending_files: Vec<String> = Vec::new();
        for source_dir in [
            workspace_root.join("src"),
            workspace_root.join("gat-engine/src"),
        ] {
            for entry in walk_rs_files(&source_dir) {
                let relative = entry
                    .strip_prefix(workspace_root)
                    .unwrap_or(&entry)
                    .to_string_lossy()
                    .replace('\\', "/");
                let relative = relative.trim_start_matches('/');
                let contents = std::fs::read_to_string(&entry).unwrap_or_default();
                if !contents.contains("ExecutionLimits::production()") {
                    continue;
                }
                if relative == "gat-engine/src/limits.rs" {
                    // This module's own definition/tests.
                    continue;
                }
                if allowed_non_test_sites.contains(&relative) {
                    continue;
                }
                offending_files.push(relative.to_string());
            }
        }
        assert!(
            offending_files.is_empty(),
            "unexpected ExecutionLimits::production() call site(s) outside the \
             known session-construction/session-free allowlist: {offending_files:?} \
             -- either route this call site through an existing Session/ \
             Operation instead, or add it to `allowed_non_test_sites` with a \
             comment explaining why it has no session to read limits from"
        );
    }

    #[cfg(test)]
    fn walk_rs_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_rs_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
        out
    }
}
