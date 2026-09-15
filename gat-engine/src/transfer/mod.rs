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

fn cache_io_kind(source: &gat_io::CacheError) -> Option<std::io::ErrorKind> {
    use gat_io::CacheError;
    match source {
        CacheError::DirectoryUnavailable { source, .. }
        | CacheError::TempFileUnavailable { source, .. }
        | CacheError::PathUnreadable { source, .. }
        | CacheError::SourceUnreadable { source }
        | CacheError::EntryUnwritable { source, .. }
        | CacheError::EntryUnreadable { source, .. } => Some(source.kind()),
        CacheError::MaterializationFailed { .. }
        | CacheError::State(_)
        | CacheError::Oid(_)
        | CacheError::Atomic(_) => None,
    }
}

// Keep remote and route metadata consistent across transfer error types.
fn diagnostic_remote(
    catalog: &crate::remote_catalog::RemoteCatalog,
    policy: &crate::path_policy::EffectivePathPolicy,
    remote: &crate::path_policy::ResolvedRemote,
) -> (
    std::sync::Arc<str>,
    Option<gat_core::name::RouteName>,
    Option<gat_core::lexical_path::GatPath>,
) {
    let route = remote.route().map(|id| policy.route_descriptor(id));
    (
        catalog.name(remote.id()),
        route.map(|descriptor| descriptor.name.clone()),
        route.map(|descriptor| descriptor.path.clone()),
    )
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub use super::presence::test_support::{
        remote_check_window_sizes, reset_remote_check_window_sizes,
    };
    pub use super::repair::test_support::{
        attempts as repair_attempts, window_high_water as repair_window_high_water,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProgressUpdates;
    use crate::{RemoteCatalog, RemoteIdentityError, ResolvedRemote};
    use gat_core::config::{RemoteConfig, RemotesConfig};
    use gat_core::lexical_path::GatPath;
    use gat_core::name::RemoteName;
    use gat_core::oid::Oid;
    use gat_core::progress::{NoopProgress, ProgressOperation, ProgressReporter, ProgressSpec};

    #[test]
    fn foreign_obligations_fail_all_public_windows_before_io() {
        let tmp = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(tmp.path().to_path_buf());
        let mut operation =
            crate::acquire_operation_without_desired_state(&repo, &NoopProgress).unwrap();
        let mut config = RemotesConfig::default();
        let name = RemoteName::from("foreign");
        config.by_name.insert(
            name.clone(),
            RemoteConfig::from("unsupported://SYNTHETIC-SECRET".to_owned()),
        );
        let catalog = RemoteCatalog::from_config(&config).unwrap();
        let remote = ResolvedRemote::for_test(catalog.id_of(&name).unwrap());
        let oid = Oid::from_bytes([1; 32]);
        let path = GatPath::parse_canonical("data").unwrap();
        let task = NoopProgress.begin(ProgressSpec::indeterminate(ProgressOperation::Repairing));
        let progress = task.handle();
        let opens = crate::remote_session::test_support::remote_opens();
        let downloads = vec![DownloadObject::new(oid, path.clone(), remote)];
        assert!(matches!(
            download_window(
                &mut operation,
                downloads,
                &mut ProgressUpdates::new(progress.clone())
            ),
            Err(DownloadError::Identity(RemoteIdentityError::ForeignOwner))
        ));
        let objects = vec![PublishObject::new(oid, path.clone(), remote)];
        assert!(matches!(
            operation.check_remote_presence_streaming(
                &objects,
                |_| panic!("foreign object reported"),
                &mut ProgressUpdates::new(progress.clone())
            ),
            Err(RemotePresenceError::Identity(
                RemoteIdentityError::ForeignOwner
            ))
        ));
        assert!(matches!(
            publish_window(
                &mut operation,
                objects,
                &mut ProgressUpdates::new(progress.clone())
            ),
            Err(PublishError::Presence(RemotePresenceError::Identity(
                RemoteIdentityError::ForeignOwner
            )))
        ));
        let repairs = vec![RepairObject::new(oid, path, remote, 1)];
        let outcome = repair_window(&mut operation, repairs, &mut ProgressUpdates::new(progress));
        assert!(matches!(
            outcome.results.as_slice(),
            [Err(RepairError::Identity(
                RemoteIdentityError::ForeignOwner
            ))]
        ));
        assert_eq!(crate::remote_session::test_support::remote_opens(), opens);
    }
}
