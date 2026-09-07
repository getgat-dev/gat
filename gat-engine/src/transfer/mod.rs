//! Shared streaming transfer primitives.

mod cache_source;
mod download;
mod planner;
mod presence;
mod publish;
mod receive;
mod repair;
mod selection;
mod upload;

pub use cache_source::TransferCacheSource;
pub use download::{
    DownloadCacheFailureKind, DownloadError, DownloadObject, DownloadOutcome,
    DownloadRemoteFailureKind, download_window,
};
#[doc(hidden)]
pub use planner::{StreamingWindow, WindowBatch};
pub(crate) use presence::check_remote_presence_streaming;
pub use presence::{RemotePresenceError, RemotePresenceObligation, RemotePresenceResult};
pub use publish::{PublishError, PublishObject, PublishOutcome, PublishStatus, publish_window};
pub use repair::{
    RepairCacheFailureKind, RepairError, RepairObject, RepairOutcome, RepairRemoteFailureKind,
    repair_window,
};
#[doc(hidden)]
pub use selection::{SelectedObject, visit_current_state_objects, visit_history_objects};
pub use upload::{
    UploadCacheFailureKind, UploadError, UploadRemoteFailureKind, UploadWriteFailureKind,
};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub use super::presence::test_support::{
        remote_check_window_sizes, reset_remote_check_window_sizes,
    };
    pub use super::repair::test_support::{
        attempts as repair_attempts, window_high_water as repair_window_high_water,
    };
}
