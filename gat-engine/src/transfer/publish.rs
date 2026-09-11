use super::presence::{PresenceProbeError, RemotePresenceError, RemotePresenceObligation};
use super::upload::{
    ExecutedUpload, PreparedUpload, UploadError, UploadObject, cache_error_kind, execute_upload,
    worker_error,
};
use crate::cache_session::CacheVerificationError;
use crate::operation::Operation;
use crate::path_policy::ResolvedRemote;
use crate::remote_catalog::RemoteId;
use crate::remote_executor::RemoteExecutor;
use crate::remote_session::RemoteHandle;
use futures::stream::{BoxStream, SelectAll};
use futures::{FutureExt, StreamExt};
use gat_core::lexical_path::GatPath;
use gat_core::oid::Oid;
use gat_core::progress::{ProgressActivity, ProgressHandle};
use gat_io::{CacheError, CacheObject, CompletedCacheVerification, ObjectVerification};
use std::collections::{BTreeMap, VecDeque};

const VERIFICATION_BATCH_SIZE: usize = 128;

/// One route-resolved object that should be published to a remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishObject {
    pub oid: Oid,
    pub representative_path: GatPath,
    pub remote: ResolvedRemote,
}

impl PublishObject {
    #[must_use]
    pub const fn new(oid: Oid, representative_path: GatPath, remote: ResolvedRemote) -> Self {
        Self {
            oid,
            representative_path,
            remote,
        }
    }
}

impl RemotePresenceObligation for PublishObject {
    fn oid(&self) -> Oid {
        self.oid
    }

    fn resolved_remote(&self) -> &ResolvedRemote {
        &self.remote
    }

    fn representative_path(&self) -> &GatPath {
        &self.representative_path
    }
}

/// The terminal result for one publication obligation, aligned with the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishStatus {
    AlreadyPresent,
    Uploaded,
    CacheMissing,
    CacheCorrupt,
}

/// Publication results in the same order as the input objects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublishOutcome {
    pub statuses: Vec<PublishStatus>,
}

/// Everything one bounded publication window can fail with.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error(transparent)]
    Presence(#[from] RemotePresenceError),
    #[error(transparent)]
    Upload(#[from] UploadError),
}

/// A verification always has at least one interested obligation. Most OIDs
/// have only one, so keep that index inline without allocating a vector.
struct WaitingObligations {
    first: usize,
    additional: Vec<usize>,
}

impl WaitingObligations {
    const fn new(first: usize) -> Self {
        Self {
            first,
            additional: Vec::new(),
        }
    }
    fn push(&mut self, index: usize) {
        self.additional.push(index);
    }
    fn iter(&self) -> impl Iterator<Item = &usize> {
        std::iter::once(&self.first).chain(self.additional.iter())
    }
    fn minimum(&self) -> usize {
        self.additional.iter().copied().fold(self.first, usize::min)
    }
}

#[derive(Clone, Copy)]
enum CacheRejection {
    Missing,
    Corrupt,
}

impl CacheRejection {
    const fn status(self) -> PublishStatus {
        match self {
            Self::Missing => PublishStatus::CacheMissing,
            Self::Corrupt => PublishStatus::CacheCorrupt,
        }
    }
}

enum VerificationState {
    Running { waiting: WaitingObligations },
    Valid { source: CacheObject },
    Invalid { rejection: CacheRejection },
}

/// The next action after a remote reports an object missing.
enum VerificationAction {
    Wait,
    Upload(CacheObject),
    Reject(CacheRejection),
}

/// An admitted job owns both its queued payload and its lifetime-bound permit.
#[allow(
    clippy::large_enum_variant,
    reason = "One transient admitted job is consumed immediately; boxing would allocate per upload"
)]
enum AdmittedWork {
    Presence {
        remote: RemoteId,
        index: usize,
        lease: crate::remote_executor::PresenceLease,
    },
    Upload {
        upload: PreparedUpload,
        lease: crate::remote_executor::TransferLease,
    },
}

enum RemoteCompletion {
    Presence {
        index: usize,
        result: Result<bool, PresenceProbeError>,
    },
    Upload {
        upload: ExecutedUpload,
    },
    Verification {
        oids: Vec<Oid>,
        result: Result<CompletedCacheVerification, CacheVerificationError>,
    },
}

struct PipelineState {
    pending_presence: BTreeMap<RemoteId, VecDeque<usize>>,
    ready_uploads: BTreeMap<RemoteId, VecDeque<PreparedUpload>>,
    remote_order: Vec<RemoteId>,
    next_remote: usize,
    last_admitted_by_remote: BTreeMap<RemoteId, QueueKind>,
    verification: BTreeMap<Oid, VerificationState>,
    pending_verification: VecDeque<Oid>,
    verification_active: bool,
    statuses: Vec<Option<PublishStatus>>,
    error: Option<(usize, PublishError)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueKind {
    Presence,
    Upload,
}

struct RemoteScheduler<'a> {
    executor: &'a RemoteExecutor,
    handles: &'a BTreeMap<RemoteId, RemoteHandle>,
    objects: &'a [PublishObject],
    task: ProgressHandle,
}

impl PipelineState {
    fn new(objects: &[PublishObject]) -> Self {
        let mut pending_presence = BTreeMap::<RemoteId, VecDeque<usize>>::new();
        let mut remote_order = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            let queue = pending_presence.entry(object.remote.id()).or_default();
            if queue.is_empty() {
                remote_order.push(object.remote.id());
            }
            queue.push_back(index);
        }
        Self {
            pending_presence,
            ready_uploads: BTreeMap::new(),
            remote_order,
            next_remote: 0,
            last_admitted_by_remote: BTreeMap::new(),
            verification: BTreeMap::new(),
            pending_verification: VecDeque::new(),
            verification_active: false,
            statuses: vec![None; objects.len()],
            error: None,
        }
    }

    fn register_verification(&mut self, oid: Oid, index: usize) -> VerificationAction {
        match self.verification.entry(oid) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(VerificationState::Running {
                    waiting: WaitingObligations::new(index),
                });
                self.pending_verification.push_back(oid);
                VerificationAction::Wait
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => match entry.get_mut() {
                VerificationState::Running { waiting } => {
                    waiting.push(index);
                    VerificationAction::Wait
                }
                VerificationState::Valid { source } => VerificationAction::Upload(source.clone()),
                VerificationState::Invalid { rejection } => VerificationAction::Reject(*rejection),
            },
        }
    }

    fn record_error(&mut self, index: usize, error: PublishError) {
        if self
            .error
            .as_ref()
            .is_none_or(|(current_index, _)| index < *current_index)
        {
            self.error = Some((index, error));
            // Only a newly earlier failure changes eligibility. Do not rescan
            // the entire window on every successful admission.
            for queue in self.pending_presence.values_mut() {
                queue.retain(|pending| *pending < index);
            }
            for queue in self.ready_uploads.values_mut() {
                queue.retain(|pending| pending.index() < index);
            }
        }
    }

    fn take_verification_batch(&mut self) -> Vec<Oid> {
        if let Some((frontier, _)) = &self.error {
            let frontier = *frontier;
            self.pending_verification.retain(|oid| {
                matches!(self.verification.get(oid), Some(VerificationState::Running { waiting })
                    if waiting.iter().any(|index| *index < frontier))
            });
        }
        self.pending_verification
            .drain(..self.pending_verification.len().min(VERIFICATION_BATCH_SIZE))
            .collect()
    }

    fn verification_error_index(&self, oids: &[Oid]) -> usize {
        oids.iter()
            .filter_map(|oid| match self.verification.get(oid) {
                Some(VerificationState::Running { waiting }) => Some(waiting.minimum()),
                _ => None,
            })
            .min()
            .expect("a verification batch has at least one waiting obligation")
    }

    fn retry_earlier_verification(&mut self, oids: Vec<Oid>, error_index: usize) {
        // A failed batch has no committed results. Its earlier obligations
        // must resolve before the final semantic error can be selected.
        self.pending_verification
            .extend(oids.into_iter().filter(|oid| {
                matches!(self.verification.get(oid), Some(VerificationState::Running { waiting })
                if waiting.iter().any(|index| *index < error_index))
            }));
    }

    fn next_admissible_work(&mut self, scheduler: &RemoteScheduler<'_>) -> Option<AdmittedWork> {
        if scheduler.executor.is_cancelled() {
            return None;
        }
        for offset in 0..self.remote_order.len() {
            let position = (self.next_remote + offset) % self.remote_order.len();
            let id = self.remote_order[position];
            let presence = self
                .pending_presence
                .get(&id)
                .is_some_and(|queue| !queue.is_empty());
            let upload = self
                .ready_uploads
                .get(&id)
                .is_some_and(|queue| !queue.is_empty());
            if !upload {
                scheduler.executor.forget_transfer_waiter(id);
            }
            let preferred = select_queue_kind(
                presence,
                upload,
                self.last_admitted_by_remote.get(&id).copied(),
            );
            let order = match preferred {
                Some(QueueKind::Upload) => [QueueKind::Upload, QueueKind::Presence],
                _ => [QueueKind::Presence, QueueKind::Upload],
            };
            for kind in order {
                let work = match kind {
                    QueueKind::Presence => self.pending_presence.get_mut(&id).and_then(|queue| {
                        let index = *queue.front()?;
                        let lease = scheduler.executor.try_presence(id)?;
                        queue.pop_front();
                        Some(AdmittedWork::Presence {
                            remote: id,
                            index,
                            lease,
                        })
                    }),
                    QueueKind::Upload => self.ready_uploads.get_mut(&id).and_then(|queue| {
                        let bytes = queue.front()?.buffer_bytes();
                        let lease = scheduler.executor.try_transfer(id, bytes)?;
                        let upload = queue.pop_front()?;
                        Some(AdmittedWork::Upload { upload, lease })
                    }),
                };
                if let Some(work) = work {
                    self.next_remote = (position + 1) % self.remote_order.len();
                    self.last_admitted_by_remote.insert(id, kind);
                    return Some(work);
                }
            }
        }
        None
    }
}

const fn select_queue_kind(
    can_admit_presence: bool,
    can_admit_upload: bool,
    last_admitted: Option<QueueKind>,
) -> Option<QueueKind> {
    match (can_admit_presence, can_admit_upload) {
        (false, false) => None,
        (true, false) => Some(QueueKind::Presence),
        (false, true) => Some(QueueKind::Upload),
        (true, true) => match last_admitted {
            Some(QueueKind::Presence) => Some(QueueKind::Upload),
            Some(QueueKind::Upload) | None => Some(QueueKind::Presence),
        },
    }
}

/// Publishes one already-bounded window through a single async pipeline.
///
/// Presence and upload jobs share one bounded completion stream. Cache
/// verification is deduplicated by OID, and results remain aligned with the
/// original publication obligations.
#[allow(
    clippy::missing_panics_doc,
    reason = "Verification preserves one result per input OID"
)]
pub fn publish_window(
    operation: &mut Operation<'_>,
    objects: Vec<PublishObject>,
    task: &ProgressHandle,
) -> Result<PublishOutcome, PublishError> {
    if objects.is_empty() {
        return Ok(PublishOutcome::default());
    }

    let mut state = PipelineState::new(&objects);

    let services = operation.window_services();
    let mut handles = BTreeMap::<RemoteId, RemoteHandle>::new();
    for (index, object) in objects.iter().enumerate() {
        if handles.contains_key(&object.remote.id()) {
            continue;
        }
        let handle = services
            .remotes
            .open_handle(services.remotes_catalog, object.remote.id(), Some(task))
            .map_err(|source| {
                RemotePresenceError::remote_open(
                    services.remotes_catalog,
                    services.policy,
                    &objects[index],
                    source,
                )
            })?;
        handles.insert(object.remote.id(), handle);
    }

    let task = task.clone();

    tokio::runtime::Handle::current().block_on(async move {
        task.set_activity(ProgressActivity::CheckingRemote);
        let scheduler = RemoteScheduler {
            executor: services.remote_executor,
            handles: &handles,
            objects: &objects,
            task: task.clone(),
        };
        let mut completions = SelectAll::<BoxStream<'_, RemoteCompletion>>::new();
        refill_remote_work(&mut state, &mut completions, &scheduler);
        while let Some(completion) = completions.next().await {
            let mut ready = vec![completion];
            while let Some(Some(completion)) = completions.next().now_or_never() {
                ready.push(completion);
            }

            for completion in ready {
                match completion {
                    RemoteCompletion::Presence { index, result } => match result {
                        Ok(true) => {
                            complete_obligation(
                                &mut state.statuses,
                                index,
                                PublishStatus::AlreadyPresent,
                                &task,
                            );
                        }
                        Ok(false) => {
                            let oid = objects[index].oid;
                            match state.register_verification(oid, index) {
                                VerificationAction::Wait => {}
                                VerificationAction::Upload(source) => {
                                    enqueue_upload(&mut state, &handles, &objects, index, source);
                                }
                                VerificationAction::Reject(rejection) => {
                                    complete_obligation(
                                        &mut state.statuses,
                                        index,
                                        rejection.status(),
                                        &task,
                                    );
                                }
                            }
                        }
                        Err(source) => state.record_error(
                            index,
                            PublishError::Presence(RemotePresenceError::presence_check(
                                services.remotes_catalog,
                                services.policy,
                                &objects[index],
                                source,
                            )),
                        ),
                    },
                    RemoteCompletion::Upload { upload } => {
                        let (object, index, result) = upload.into_parts();
                        match result {
                            Ok(()) => {
                                complete_obligation(
                                    &mut state.statuses,
                                    index,
                                    PublishStatus::Uploaded,
                                    &task,
                                );
                            }
                            Err(source) => state.record_error(
                                index,
                                PublishError::Upload(worker_error(
                                    services.remotes_catalog,
                                    services.policy,
                                    &object,
                                    source,
                                )),
                            ),
                        }
                    }
                    RemoteCompletion::Verification { oids, result } => {
                        state.verification_active = false;
                        let error_index = state.verification_error_index(&oids);
                        match result {
                            Ok(completed) => {
                                let verified = services
                                    .cache_session
                                    .commit_verification(services.cache_root, completed);
                                assert_eq!(
                                    verified.len(),
                                    oids.len(),
                                    "verification results remain aligned with the batch"
                                );
                                for (oid, verification) in oids.into_iter().zip(verified) {
                                    let Some(VerificationState::Running { waiting }) =
                                        state.verification.remove(&oid)
                                    else {
                                        unreachable!(
                                            "completed verification has waiting obligations"
                                        )
                                    };
                                    match verification {
                                        ObjectVerification::Valid => {
                                            let source = services
                                                .cache_session
                                                .object_source(services.cache_root, &oid);
                                            for &index in waiting.iter() {
                                                enqueue_upload(
                                                    &mut state,
                                                    &handles,
                                                    &objects,
                                                    index,
                                                    source.clone(),
                                                );
                                            }
                                            state
                                                .verification
                                                .insert(oid, VerificationState::Valid { source });
                                        }
                                        ObjectVerification::Missing
                                        | ObjectVerification::Corrupt => {
                                            let rejection =
                                                if verification == ObjectVerification::Missing {
                                                    CacheRejection::Missing
                                                } else {
                                                    CacheRejection::Corrupt
                                                };
                                            for &index in waiting.iter() {
                                                complete_obligation(
                                                    &mut state.statuses,
                                                    index,
                                                    rejection.status(),
                                                    &task,
                                                );
                                            }
                                            state.verification.insert(
                                                oid,
                                                VerificationState::Invalid { rejection },
                                            );
                                        }
                                    }
                                }
                            }
                            Err(source) => {
                                let error_index = source.oid().map_or(error_index, |oid| {
                                    state.verification_error_index(&[oid])
                                });
                                state.record_error(
                                    error_index,
                                    PublishError::Upload(cache_verification_error(
                                        objects[error_index].representative_path.clone(),
                                        source,
                                    )),
                                );
                                state.retry_earlier_verification(oids, error_index);
                            }
                        }
                    }
                }
            }

            if !state.verification_active && !services.remote_executor.is_cancelled() {
                let oids = state.take_verification_batch();
                if !oids.is_empty() {
                    let prepared = services
                        .cache_session
                        .prepare_verification(services.cache_root, &oids);
                    state.verification_active = true;
                    completions.push(
                        async move {
                            let result = match services
                                .remote_executor
                                .local(move || prepared.verify())
                                .await
                            {
                                Ok(result) => result.map_err(Into::into),
                                Err(source) => Err(CacheVerificationError::Worker(source)),
                            };
                            RemoteCompletion::Verification { oids, result }
                        }
                        .into_stream()
                        .boxed(),
                    );
                }
            }
            refill_remote_work(&mut state, &mut completions, &scheduler);
        }
        if let Some((_, error)) = state.error {
            return Err(error);
        }
        if services.remote_executor.is_cancelled() {
            return Err(PublishError::Upload(UploadError::Cancelled));
        }

        Ok(PublishOutcome {
            statuses: terminal_statuses(state.statuses),
        })
    })
}

fn refill_remote_work<'a>(
    state: &mut PipelineState,
    completions: &mut SelectAll<BoxStream<'a, RemoteCompletion>>,
    scheduler: &'a RemoteScheduler<'a>,
) {
    let presence_completions = |id, entries| {
        super::presence::presence_stream(
            scheduler.executor,
            scheduler.handles[&id].client().clone(),
            entries,
        )
        .map(|(index, result)| RemoteCompletion::Presence { index, result })
        .boxed()
    };
    let mut presence = BTreeMap::<RemoteId, Vec<_>>::new();
    while let Some(work) = state.next_admissible_work(scheduler) {
        match work {
            AdmittedWork::Upload { upload, lease } => {
                let task = scheduler.task.clone();
                completions.push(
                    async move {
                        let _lease = lease;
                        let upload = execute_upload(scheduler.executor, upload, task).await;
                        RemoteCompletion::Upload { upload }
                    }
                    .into_stream()
                    .boxed(),
                );
            }
            AdmittedWork::Presence {
                remote: remote_id,
                index,
                lease,
            } => {
                let oid = scheduler.objects[index].oid;
                let limit = scheduler.handles[&remote_id]
                    .client()
                    .presence_batch_limit();
                let complete = match presence.entry(remote_id) {
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        entry.get_mut().push((index, oid, lease));
                        (entry.get().len() == limit).then(|| entry.remove())
                    }
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        let entries = vec![(index, oid, lease)];
                        if limit == 1 {
                            Some(entries)
                        } else {
                            entry.insert(entries);
                            None
                        }
                    }
                };
                if let Some(entries) = complete {
                    completions.push(presence_completions(remote_id, entries));
                }
            }
        }
    }
    // Partial batches start immediately; admission keeps its existing fair
    // remote/work-class order rather than filling one remote ahead of others.
    for (id, entries) in presence {
        completions.push(presence_completions(id, entries));
    }
}

fn enqueue_upload(
    state: &mut PipelineState,
    handles: &BTreeMap<RemoteId, RemoteHandle>,
    objects: &[PublishObject],
    index: usize,
    source: CacheObject,
) {
    if state
        .error
        .as_ref()
        .is_some_and(|(frontier, _)| index >= *frontier)
    {
        return;
    }
    let object = &objects[index];
    state
        .ready_uploads
        .entry(object.remote.id())
        .or_default()
        .push_back(PreparedUpload::new(
            UploadObject::new(
                object.oid,
                object.representative_path.clone(),
                object.remote,
            ),
            handles[&object.remote.id()].clone(),
            source,
            index,
        ));
}

fn cache_verification_error(path: GatPath, source: CacheVerificationError) -> UploadError {
    match source {
        CacheVerificationError::Verification(source) => cache_error(path, source.into_source()),
        CacheVerificationError::Worker(source) => UploadError::TaskFailed { path, source },
    }
}

fn cache_error(path: GatPath, source: CacheError) -> UploadError {
    UploadError::CacheVerification {
        kind: cache_error_kind(&source),
        path,
        source: super::TransferCacheSource::new(source),
    }
}

fn terminal_statuses(statuses: Vec<Option<PublishStatus>>) -> Vec<PublishStatus> {
    statuses
        .into_iter()
        .map(|status| status.expect("every publish object reaches a terminal status"))
        .collect()
}

fn complete_obligation(
    statuses: &mut [Option<PublishStatus>],
    index: usize,
    status: PublishStatus,
    task: &ProgressHandle,
) {
    assert!(
        statuses[index].replace(status).is_none(),
        "publication obligation completed more than once"
    );
    task.inc(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gat_core::progress::{ActivityBackend, ProgressActivity, ProgressTask};
    use std::sync::{Arc, Mutex};

    #[test]
    fn push_refill_groups_admitted_file_presence_into_one_worker() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_remote, handles) =
            crate::remote_session::test_support::open_handles_on_current_runtime(&["remote"]);
        let executor = RemoteExecutor::new(crate::limits::ExecutionLimits::default().remote);
        let objects: Vec<_> = (0..128)
            .map(|tag| publish_object(handles[0].id(), tag))
            .collect();
        let handles = BTreeMap::from([(handles[0].id(), handles[0].clone())]);
        let backend = Arc::new(RecordingBackend::default());
        let scheduler = RemoteScheduler {
            executor: &executor,
            handles: &handles,
            objects: &objects,
            task: progress_handle(&backend),
        };
        let mut state = PipelineState::new(&objects);
        let mut completions = SelectAll::new();
        refill_remote_work(&mut state, &mut completions, &scheduler);
        let results = runtime.block_on(completions.collect::<Vec<_>>());
        assert_eq!(results.len(), 128);
        for (expected, result) in results.into_iter().enumerate() {
            assert!(
                matches!(result, RemoteCompletion::Presence { index, result: Ok(false) } if index == expected)
            );
        }
        assert_eq!(executor.local_submissions(), 1);
    }

    #[derive(Default)]
    struct RecordingBackend {
        increments: Mutex<Vec<u64>>,
    }

    impl ActivityBackend for RecordingBackend {
        fn inc(&self, delta: u64) {
            self.increments.lock().unwrap().push(delta);
        }

        fn set_activity(&self, _activity: &ProgressActivity) {}

        fn finish(&self) {}
    }

    fn progress_handle(backend: &Arc<RecordingBackend>) -> ProgressHandle {
        ProgressTask::from_backend(Arc::clone(backend) as Arc<dyn ActivityBackend>).handle()
    }

    fn assert_send_static<T: Send + 'static>() {}

    fn publish_object(remote_id: RemoteId, tag: u8) -> PublishObject {
        let mut bytes = [0; 32];
        bytes[0] = tag;
        PublishObject::new(
            Oid::from_bytes(bytes),
            GatPath::parse_canonical("f").unwrap(),
            ResolvedRemote::for_test(remote_id),
        )
    }

    #[test]
    fn publish_transport_types_are_worker_safe() {
        assert_send_static::<PublishObject>();
        assert_send_static::<PublishOutcome>();
        assert_send_static::<PublishError>();
    }

    #[test]
    fn terminal_statuses_preserve_input_order() {
        let statuses = vec![
            Some(PublishStatus::CacheMissing),
            Some(PublishStatus::AlreadyPresent),
            Some(PublishStatus::Uploaded),
            Some(PublishStatus::CacheCorrupt),
        ];

        assert_eq!(
            terminal_statuses(statuses),
            vec![
                PublishStatus::CacheMissing,
                PublishStatus::AlreadyPresent,
                PublishStatus::Uploaded,
                PublishStatus::CacheCorrupt,
            ]
        );
    }

    #[test]
    fn completing_obligations_increments_each_terminal_result_once() {
        let backend = Arc::new(RecordingBackend::default());
        let task = progress_handle(&backend);
        let mut statuses = vec![None; 4];

        complete_obligation(&mut statuses, 1, PublishStatus::AlreadyPresent, &task);
        complete_obligation(&mut statuses, 3, PublishStatus::Uploaded, &task);
        complete_obligation(&mut statuses, 0, PublishStatus::CacheMissing, &task);
        complete_obligation(&mut statuses, 2, PublishStatus::CacheCorrupt, &task);

        assert_eq!(
            terminal_statuses(statuses),
            vec![
                PublishStatus::CacheMissing,
                PublishStatus::AlreadyPresent,
                PublishStatus::CacheCorrupt,
                PublishStatus::Uploaded,
            ]
        );
        assert_eq!(*backend.increments.lock().unwrap(), vec![1, 1, 1, 1]);
    }

    #[test]
    #[should_panic(expected = "publication obligation completed more than once")]
    fn completing_an_obligation_twice_is_rejected() {
        let backend = Arc::new(RecordingBackend::default());
        let task = progress_handle(&backend);
        let mut statuses = vec![None];

        complete_obligation(&mut statuses, 0, PublishStatus::AlreadyPresent, &task);
        complete_obligation(&mut statuses, 0, PublishStatus::Uploaded, &task);
    }

    #[test]
    fn pipeline_state_groups_presence_in_remote_discovery_order() {
        let (_dir, handles) = crate::remote_session::test_support::open_handles(&["a", "b"]);
        let a = handles[0].id();
        let b = handles[1].id();
        let objects = vec![
            publish_object(b, 0),
            publish_object(a, 1),
            publish_object(b, 2),
            publish_object(a, 3),
        ];

        let state = PipelineState::new(&objects);

        assert_eq!(state.remote_order, vec![b, a]);
        assert_eq!(
            state.pending_presence[&b]
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(
            state.pending_presence[&a]
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn verification_registration_shares_pending_work_and_reuses_rejection() {
        let mut state = PipelineState::new(&[]);
        let oid = Oid::from_bytes([7; 32]);
        assert!(matches!(
            state.register_verification(oid, 3),
            VerificationAction::Wait
        ));
        assert!(matches!(
            state.register_verification(oid, 1),
            VerificationAction::Wait
        ));
        assert_eq!(
            state
                .pending_verification
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [oid]
        );
        let Some(VerificationState::Running { waiting }) = state.verification.get(&oid) else {
            panic!("both obligations must share running verification");
        };
        assert_eq!(waiting.iter().copied().collect::<Vec<_>>(), [3, 1]);
        assert_eq!(waiting.minimum(), 1);
        state.pending_verification.clear();
        state.verification.insert(
            oid,
            VerificationState::Invalid {
                rejection: CacheRejection::Corrupt,
            },
        );
        assert!(matches!(
            state.register_verification(oid, 0),
            VerificationAction::Reject(CacheRejection::Corrupt)
        ));
        assert!(state.pending_verification.is_empty());
        assert!(matches!(
            state.verification.get(&oid),
            Some(VerificationState::Invalid {
                rejection: CacheRejection::Corrupt
            })
        ));
    }

    #[test]
    fn verification_batches_are_bounded_without_waiting_for_a_full_window() {
        let (_dir, handles) = crate::remote_session::test_support::open_handles(&["a"]);
        let remote_id = handles[0].id();
        let objects = (0..(VERIFICATION_BATCH_SIZE + 3))
            .map(|tag| publish_object(remote_id, (tag).to_le_bytes()[0]))
            .collect::<Vec<_>>();
        let mut state = PipelineState::new(&objects);

        for object in &objects {
            state.pending_verification.push_back(object.oid);
        }

        assert_eq!(
            state.take_verification_batch().len(),
            VERIFICATION_BATCH_SIZE
        );
        assert_eq!(state.take_verification_batch().len(), 3);
        assert!(state.take_verification_batch().is_empty());
    }

    #[test]
    fn an_earlier_error_prunes_only_ineligible_queued_presence() {
        let (_dir, handles) = crate::remote_session::test_support::open_handles(&["a", "b"]);
        let objects = (0..6)
            .map(|index| publish_object(handles[index % 2].id(), u8::try_from(index).unwrap()))
            .collect::<Vec<_>>();
        let mut state = PipelineState::new(&objects);
        state.record_error(4, PublishError::Upload(UploadError::Cancelled));
        let queued = |state: &PipelineState| {
            let mut indices: Vec<_> = state.pending_presence.values().flatten().copied().collect();
            indices.sort_unstable();
            indices
        };
        assert_eq!(queued(&state), [0, 1, 2, 3]);
        state.record_error(2, PublishError::Upload(UploadError::Cancelled));
        assert_eq!(queued(&state), [0, 1]);
        state.record_error(5, PublishError::Upload(UploadError::Cancelled));
        assert_eq!(queued(&state), [0, 1]);
    }

    #[test]
    fn late_verification_cannot_enqueue_uploads_beyond_the_error_frontier() {
        let directory = crate::test_harness::test_repo();
        let repo = crate::Invocation::from_pairs([] as [(&str, &str); 0])
            .unwrap()
            .repository_at(directory.path().to_path_buf());
        let cache_root = repo.resolved_cache_root().unwrap();
        let cache = cache_root.open_client();
        let (_remotes, handles) = crate::remote_session::test_support::open_handles(&["a"]);
        let remote = handles[0].id();
        let handles = BTreeMap::from([(remote, handles[0].clone())]);
        let objects = (0..3)
            .map(|tag| {
                let (ingested, _) = cache_root
                    .writer()
                    .ingest(std::io::Cursor::new(vec![tag]))
                    .unwrap();
                cache.verify(&ingested.oid).unwrap();
                let mut object = publish_object(remote, tag);
                object.oid = ingested.oid;
                object
            })
            .collect::<Vec<_>>();
        let mut state = PipelineState::new(&objects);
        state.record_error(1, PublishError::Upload(UploadError::Cancelled));
        // Simulate verified completions arriving after the error was recorded.
        for index in [2, 0, 1] {
            let source = cache.object(&objects[index].oid);
            enqueue_upload(&mut state, &handles, &objects, index, source);
        }
        let ready = &state.ready_uploads[&remote];
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].index(), 0);
    }

    #[test]
    fn deterministic_error_selection_uses_lowest_semantic_input_index() {
        let (_dir, handles) = crate::remote_session::test_support::open_handles(&["a"]);
        let objects = vec![publish_object(handles[0].id(), 0)];
        let mut state = PipelineState::new(&objects);
        let error = |message: &'static str| {
            PublishError::Presence(RemotePresenceError::PresenceCheck {
                remote_name: Arc::from("a"),
                route_name: None,
                route: None,
                path: GatPath::parse_canonical("f").unwrap(),
                source: Box::new(std::io::Error::other(message)),
            })
        };

        state.record_error(3, error("completed first"));
        state.record_error(1, error("completed later"));
        state.record_error(2, error("must not replace the lower index"));

        assert_eq!(state.error.as_ref().map(|(index, _)| *index), Some(1));
    }

    #[test]
    fn failed_verification_resolves_earlier_shared_obligations_in_either_completion_order() {
        let (_dir, handles) = crate::remote_session::test_support::open_handles(&["a"]);
        let objects = (0..4)
            .map(|tag| publish_object(handles[0].id(), tag))
            .collect::<Vec<_>>();
        let error = || PublishError::Upload(UploadError::Cancelled);
        for order in [[3, 2], [2, 3]] {
            let mut state = PipelineState::new(&objects);
            // Presence finished in reverse order; OID 0 is also needed by a
            // later publication, but must survive pruning for obligation 0.
            state.verification.insert(
                objects[0].oid,
                VerificationState::Running {
                    waiting: {
                        let mut waiting = WaitingObligations::new(3);
                        waiting.push(0);
                        waiting
                    },
                },
            );
            state.verification.insert(
                objects[1].oid,
                VerificationState::Running {
                    waiting: WaitingObligations::new(1),
                },
            );
            state.verification.insert(
                objects[2].oid,
                VerificationState::Running {
                    waiting: WaitingObligations::new(2),
                },
            );
            for index in order {
                state.record_error(index, error());
            }
            state.retry_earlier_verification(
                vec![objects[2].oid, objects[1].oid, objects[0].oid],
                2,
            );
            let retry = state.take_verification_batch();
            assert_eq!(retry, [objects[1].oid, objects[0].oid]);
            assert_eq!(state.verification_error_index(&retry), 0);
            // The retried batch identifies obligation 1 as failing. OID 0
            // still needs verification, even though it completed presence last.
            state.record_error(1, error());
            state.retry_earlier_verification(retry, 1);
            assert_eq!(state.take_verification_batch(), [objects[0].oid]);
            state.record_error(0, error());
            assert_eq!(state.error.as_ref().map(|(index, _)| *index), Some(0));
            assert!(state.take_verification_batch().is_empty());
        }
    }

    #[test]
    fn pipeline_scheduler_alternates_ready_work_classes() {
        assert_eq!(
            select_queue_kind(true, true, Some(QueueKind::Presence)),
            Some(QueueKind::Upload)
        );
        assert_eq!(
            select_queue_kind(true, true, Some(QueueKind::Upload)),
            Some(QueueKind::Presence)
        );
    }

    #[test]
    fn pipeline_scheduler_alternates_presence_and_upload() {
        let mut last = None;
        let selected = (0..4)
            .map(|_| {
                let kind = select_queue_kind(true, true, last).unwrap();
                last = Some(kind);
                kind
            })
            .collect::<Vec<_>>();

        assert_eq!(
            selected,
            vec![
                QueueKind::Presence,
                QueueKind::Upload,
                QueueKind::Presence,
                QueueKind::Upload,
            ]
        );
    }

    #[test]
    fn pipeline_scheduler_does_not_starve_presence_behind_upload_backlog() {
        assert_eq!(
            select_queue_kind(true, true, Some(QueueKind::Upload)),
            Some(QueueKind::Presence)
        );
        assert_eq!(
            select_queue_kind(true, true, Some(QueueKind::Presence)),
            Some(QueueKind::Upload)
        );
    }

    #[test]
    fn pipeline_scheduler_uses_all_capacity_when_only_one_kind_is_ready() {
        assert_eq!(
            select_queue_kind(true, false, None),
            Some(QueueKind::Presence)
        );
        assert_eq!(
            select_queue_kind(false, true, None),
            Some(QueueKind::Upload)
        );
        assert_eq!(select_queue_kind(false, false, None), None);
    }
}
