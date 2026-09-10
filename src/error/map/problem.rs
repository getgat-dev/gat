//! The only place outside `crate::error` itself permitted to construct a
//! [`super::super::UserProblem`] -- `UserProblem::new`/`with_source` are
//! `pub(in crate::error)`, so output renderers go through the semantic
//! constructors below instead. This
//! mirrors the `Diagnostic`/`Failure` boundary rule for the
//! non-fatal counterpart: presentation text is authored at one reviewable
//! seam, not scattered across every place a non-fatal finding is
//! discovered.
//!
//! Each `*_problem` function below takes a typed reason (or a concrete
//! `#[source]`-bearing error) and authors the summary text itself, rather
//! than accepting an arbitrary caller-chosen string -- this makes it
//! impossible for a call site to smuggle `err.to_string()` (or any other
//! low-level text) into a rendered `UserProblem::summary()`.

use super::super::UserProblem;
use crate::presentation::UserLine;
use gat_command::DbUnreadableReason;
use gat_command::{CandidateInvalidReason, LiveLockInvalidReason, TransactionMalformedReason};
use std::sync::Arc;

/// Authors the CLI-facing summary for [`DbUnreadableReason`], retaining
/// the engine's opaque diagnostic as a private technical source.
#[must_use]
pub fn db_unreadable_problem(reason: DbUnreadableReason) -> UserProblem {
    match reason {
        DbUnreadableReason::NotARegularFile => {
            UserProblem::new("expected a cache metadata file, found a directory")
        }
        DbUnreadableReason::OpenFailed(err) => {
            UserProblem::with_source("could not open the cache metadata file", err)
        }
        DbUnreadableReason::SchemaVersionUnreadable(err) => {
            UserProblem::with_source("could not read the metadata format version", err)
        }
        DbUnreadableReason::IntegrityCheckFailed(source) => {
            UserProblem::with_source("integrity check failed", source)
        }
        DbUnreadableReason::IntegrityCheckUnrunnable(err) => {
            UserProblem::with_source("integrity check could not be run", err)
        }
        DbUnreadableReason::NotInitialized { version } => UserProblem::new(UserLine::compose([
            UserLine::authored("metadata format version "),
            UserLine::number(version),
            UserLine::authored(" is not initialized by this build"),
        ])),
    }
}

/// Authors the CLI-facing summary for [`LiveLockInvalidReason`].
#[must_use]
pub fn live_lock_invalid_problem(reason: LiveLockInvalidReason) -> UserProblem {
    match reason {
        LiveLockInvalidReason::LoadFailed(err) => {
            UserProblem::with_source("could not load the live lock", err)
        }
        LiveLockInvalidReason::NeitherFileNorShardTree => {
            UserProblem::new("live gat.lock path is neither a file nor a shard tree")
        }
    }
}

/// Authors the CLI-facing summary for [`TransactionMalformedReason`].
#[must_use]
pub fn transaction_malformed_problem(reason: TransactionMalformedReason) -> UserProblem {
    match reason {
        TransactionMalformedReason::RecordDecodeFailed(err) => {
            UserProblem::with_source("malformed transaction record", err)
        }
        TransactionMalformedReason::UnrecognizedPhase => {
            UserProblem::new("unrecognized transaction phase")
        }
        TransactionMalformedReason::IdMismatch => {
            UserProblem::new("recorded transaction id does not match directory")
        }
        TransactionMalformedReason::MetadataOutsideScratchLayout => {
            UserProblem::new("transaction metadata points outside its expected scratch layout")
        }
    }
}

/// Authors the CLI-facing summary for [`CandidateInvalidReason`].
#[must_use]
pub fn candidate_invalid_problem(reason: CandidateInvalidReason) -> UserProblem {
    match reason {
        CandidateInvalidReason::Missing => UserProblem::new("missing"),
        CandidateInvalidReason::ShapeMismatch => UserProblem::new("shape mismatch"),
        CandidateInvalidReason::LoadFailed(err) => UserProblem::with_source("invalid", err),
        CandidateInvalidReason::ShapeUnreadable(err) => UserProblem::with_source("unreadable", err),
    }
}

/// Authors a short, safe [`UserProblem`] for a
/// [`gat_command::RepairError`], for
/// `output::render` -- structured per variant, never
/// the typed error's own `Display` (which may echo third-party
/// opendal/rusqlite/io text through unfiltered). Only the identifying
/// oid/path/remote-name fields already carried on the corresponding
/// `RepairFailure` are appended by the caller; this returns just the
/// "what went wrong" clause, with the complete typed `RepairError`
/// retained as the problem's private technical source. Lives in the
/// mapping layer rather than on `RepairError` itself to preserve
/// dependency direction.
#[must_use]
pub fn repair_problem(err: Arc<gat_command::RepairError>) -> UserProblem {
    use gat_command::RepairError;
    let summary = match err.as_ref() {
        RepairError::UnknownRemoteOverride(_) => {
            "the `--remote` override does not name a configured remote"
        }
        RepairError::MissingRemoteConfig(_) => "no remote is configured for this path",
        RepairError::DataPlane(source) => match source {
            gat_engine::RepairError::Cancelled => "repair cancelled",
            gat_engine::RepairError::RemoteOpen { source, .. } => match source.kind() {
                gat_engine::RemoteOpenFailureKind::InvalidConnectTimeout => {
                    "GAT_CONNECT_TIMEOUT must be a positive whole number of seconds"
                }
                gat_engine::RemoteOpenFailureKind::ReadinessTimedOut { .. } => {
                    "the remote readiness check timed out; check the credential provider and network access"
                }
                gat_engine::RemoteOpenFailureKind::PermissionDenied => {
                    "permission denied while checking the remote; credentials must permit listing its root"
                }
                _ => "the remote could not be opened",
            },
            gat_engine::RepairError::RemoteRead { .. } => {
                "the object could not be read from the remote"
            }
            gat_engine::RepairError::Cache { .. } => "the re-fetched object could not be cached",
            gat_engine::RepairError::HashMismatch { .. } => {
                "the re-fetched object's content did not match its expected hash"
            }
            gat_engine::RepairError::TaskFailed { .. } => {
                "the repair task did not complete normally"
            }
        },
    };
    UserProblem::with_source(summary, err)
}

/// authored entirely from a caller-provided static string literal --
/// reserved for genuinely short, unambiguous, non-error facts (test-only
/// scaffolding, etc.) that don't warrant a dedicated reason enum. The
/// `&'static str` signature is a compile-time guard, not merely a
/// convention: `err.to_string()`/`format!(...)` produce an owned
/// `String`, so it is impossible to pass one here -- any call site that
/// needs to render dynamic/error-derived content must go through a typed
/// reason enum and its dedicated `*_problem` function instead.
#[must_use]
pub fn authored(summary: &'static str) -> UserProblem {
    UserProblem::new(summary)
}

/// Test-only escape hatch for `output::rows`'s own `RowDetail` tests,
/// which need a source-bearing `UserProblem` fixture without duplicating
/// a `*_problem` reason enum solely for that purpose.
#[cfg(test)]
pub(crate) fn with_source_for_test(
    summary: impl Into<UserLine>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> UserProblem {
    UserProblem::with_source(summary, source)
}

#[cfg(test)]
mod tests {
    #[test]
    fn integrity_problem_keeps_database_text_out_of_the_summary() {
        let sentinel = "SQLITE_INTERNAL_SENTINEL /private/database/path";
        let source = gat_engine::DbIntegrityFailure::for_test(sentinel.to_owned());
        let problem = super::db_unreadable_problem(
            gat_command::DbUnreadableReason::IntegrityCheckFailed(source),
        );
        assert_eq!(problem.summary(), "integrity check failed");
        let retained = problem
            .technical_source()
            .unwrap()
            .downcast_ref::<gat_engine::DbIntegrityFailure>()
            .expect("retain the engine's opaque diagnostic");
        assert_eq!(retained.to_string(), sentinel);
    }

    use super::*;
    use gat_command::RepairError;

    /// The easiest way to construct a cancelled (never-panicked)
    /// `JoinError` synchronously in a unit test.
    fn cancelled_join_error() -> tokio::task::JoinError {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        rt.block_on(async {
            let handle = tokio::spawn(std::future::pending::<()>());
            handle.abort();
            handle.await.expect_err("aborted task yields a JoinError")
        })
    }

    /// `repair_problem` returns a fixed `&'static str` per variant
    /// regardless of the wrapped source's content -- this is what makes
    /// it leak-proof by construction. This test embeds a sentinel into
    /// the `JoinError`-bearing variant's underlying task and proves the
    /// sentinel never appears in the summary.
    #[test]
    fn task_failed_summary_never_renders_the_underlying_join_error_text() {
        let err = Arc::new(RepairError::DataPlane(
            gat_engine::RepairError::TaskFailed {
                path: gat_core::lexical_path::GatPath::parse_canonical("secret.bin").unwrap(),
                source: cancelled_join_error(),
            },
        ));
        let problem = repair_problem(err);
        let summary = problem.summary();
        assert!(!summary.contains("JoinError"));
        assert!(!summary.contains("cancelled"));
        assert_eq!(summary, "the repair task did not complete normally");
    }

    /// Every `RepairError` variant's mapped summary must be non-empty,
    /// static, authored prose -- guards against a variant adding
    /// a `format!`/`to_string()`-based summary that could leak a
    /// third-party error's text.
    #[test]
    fn hash_mismatch_summary_never_renders_the_raw_expected_actual_hashes() {
        let expected = gat_core::oid::Oid::from_bytes([1; 32]);
        let actual = gat_core::oid::Oid::from_bytes([2; 32]);
        let err = Arc::new(RepairError::DataPlane(
            gat_engine::RepairError::HashMismatch {
                path: gat_core::lexical_path::GatPath::parse_canonical("secret.bin").unwrap(),
                expected,
                actual,
            },
        ));
        let problem = repair_problem(err);
        let summary = problem.summary();
        assert!(!summary.contains(&expected.to_string()));
        assert!(!summary.contains(&actual.to_string()));
    }

    /// The complete typed `RepairError` is retained as the problem's
    /// private technical source, not just discarded -- mirrors the same
    /// source-retention contract `Failure::infrastructure` gives the
    /// fatal path.
    #[test]
    fn repair_problem_retains_the_complete_repair_error_as_source() {
        let err = Arc::new(RepairError::DataPlane(
            gat_engine::RepairError::HashMismatch {
                path: gat_core::lexical_path::GatPath::parse_canonical("file.bin").unwrap(),
                expected: gat_core::oid::Oid::from_bytes([1; 32]),
                actual: gat_core::oid::Oid::from_bytes([2; 32]),
            },
        ));
        let problem = repair_problem(err);
        let retained = problem
            .technical_source()
            .expect("hash mismatch retains a technical source");
        assert!(retained.downcast_ref::<Arc<RepairError>>().is_some());
    }
}
