//! Pure use-case orchestration for `gat`.
//!
//! `gat-command` accepts semantic requests from the root application shell,
//! coordinates repository services from `gat-engine`, and returns typed
//! outcomes. CLI parsing, physical persistence, failure mapping, and
//! presentation remain outside this crate.

mod add;
mod config;
mod diff;
mod fetch;
mod gc;
mod init;
mod merge_driver;
mod mount;
#[path = "move.rs"]
mod move_cmd;
mod ownership;
mod parallel;
mod push;
mod remote;
mod remote_status;
mod remove;
mod repair;
mod resource;
mod route;
mod saved_selection;
mod selection;
mod status;
mod sync;
mod system;

pub use add::{AddError, AddOutcome, AddRequest, AddedRow, add, add_with_lifecycle_observer};
pub use config::{
    ConfigAction, ConfigError, ConfigOutcome, ConfigRequest, ConfigScalarValue, ConfigSource,
    config, config_with_lifecycle_observer,
};
pub use diff::{DiffError, DiffOutcome, DiffRequest, DiffRow, DiffTarget, diff};
pub use fetch::{
    FetchError, FetchOutcome, FetchRequest, FetchSource, fetch, fetch_with_desired_operation,
};
pub use gc::{
    GcEngineError, GcError, GcFailure, GcFailureKind, GcOutcome, GcRepositoryFailureKind,
    GcRepositoryIssue, GcRequest, gc, gc_with_lifecycle_observer,
};
pub use init::{
    InitConfigOutcome, InitError, InitGitIntegrationOutcome, InitHooksOutcome, InitOutcome,
    InitRequest, init,
};
pub use merge_driver::{MergeDriverError, MergeDriverOutcome, MergeDriverRequest, merge_driver};
pub use mount::{
    MatchedRoute, MountDetails, MountError, MountOutcome, MountRecord, MountRequest,
    MountRouteBootstrap, mount,
};
pub use move_cmd::{MoveError, MoveOutcome, MoveRequest, move_path, move_with_progress};
pub use ownership::OwnershipError;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use push::test_support as push_test_support;
pub use push::{
    PushError, PushOutcome, PushRequest, PushSkip, PushSkipReason, PushSource, push,
    push_with_desired_operation,
};
pub use remote::{RemoteDefault, RemoteError, RemoteOutcome, RemoteRecord, RemoteRequest, remote};
pub use remote_status::{
    MissingRemoteConfigError, MissingRemoteObject, RemoteStatusError, RemoteStatusOutcome,
    RemoteStatusRequest, remote_status, remote_status_with_desired_operation,
};
pub use remove::{RemoveError, RemoveOutcome, RemoveRequest, remove, remove_with_progress};
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use repair::test_support as repair_test_support;
pub use repair::{RepairError, RepairFailure, RepairOutcome, RepairRequest, repair_with_operation};
pub use resource::{ResourceKind, ResourceScopeError};
pub use route::{
    DefaultRemoteRoute, RouteDetails, RouteError, RouteOutcome, RouteRecord, RouteRequest, route,
};
pub use saved_selection::{
    SavedSelectionError, SelectionDefault, SelectionOutcome, SelectionRecord, SelectionRequest,
    named_selection, saved_selection,
};
pub use selection::SelectionScope;
pub use status::{
    CachePresence, LsFilesError, LsFilesOutcome, LsFilesRequest, StatusError, StatusOutcome,
    StatusRequest, StatusRow, ls_files, status,
};
pub use sync::{
    HookRequest, PullRequest, SyncCompletionStatus, SyncError, SyncIncompleteError, SyncOutcome,
    SyncRequest, hook, pull, pull_with_desired_operation, recover_incomplete, sync,
    sync_with_operation,
};
pub use system::{
    CacheClean, CacheDbState, CacheFact, CacheInspect, CacheRepair, CandidateInvalidReason,
    CandidateOutcome, DbUnreadableReason, DomainFact, GitClean, GitFact, GitInspect, GitRepair,
    LiveLockInvalidReason, LiveLockState, LockClean, LockFact, LockRepair, LockState,
    PreparedTxnStatus, RecoveryChoice, RecoverySelectionFailure, StateClean, StateDbState,
    StateFact, StateInspect, StateRepair, SystemError, SystemOutcome, SystemRequest, SystemScope,
    SystemVerb, TemporaryCleanOutcome, TransactionKind, TransactionMalformedReason,
    TransactionState, system, system_with_lifecycle_observer,
};

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use add::add_with_window_for_test;
