use super::presence::{RemotePresenceError, RemotePresenceObligation};
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
use std::error::Error;

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

enum VerificationState {
    Running { waiting: Vec<usize> },
    Valid { source: CacheObject },
    Invalid { status: PublishStatus },
}

struct ReadyUpload {
    upload: PreparedUpload,
}

enum RemoteCompletion {
    Presence {
        index: usize,
        result: Result<bool, Box<dyn Error + Send + Sync>>,
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
    ready_uploads: BTreeMap<RemoteId, VecDeque<ReadyUpload>>,
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

    fn record_error(&mut self, index: usize, error: PublishError) {
        if self
            .error
            .as_ref()
            .is_none_or(|(current_index, _)| index < *current_index)
        {
            self.error = Some((index, error));
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
                Some(VerificationState::Running { waiting }) => waiting.iter().min().copied(),
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

    fn next_admissible_work(
        &mut self,
        scheduler: &RemoteScheduler<'_>,
    ) -> Option<(RemoteId, QueueKind, crate::remote_executor::RemoteLease)> {
        if scheduler.executor.is_cancelled() {
            return None;
        }
        let frontier = self.error.as_ref().map_or(usize::MAX, |(index, _)| *index);
        for queue in self.pending_presence.values_mut() {
            queue.retain(|index| *index < frontier);
        }
        for queue in self.ready_uploads.values_mut() {
            queue.retain(|ready| ready.upload.index() < frontier);
        }
        for offset in 0..self.remote_order.len() {
            let position = (self.next_remote + offset) % self.remote_order.len();
            let id = self.remote_order[position];
            let presence = self
                .pending_presence
                .get(&id)
                .is_some_and(|queue| !queue.is_empty());
            let upload = self.ready_uploads.get(&id).and_then(|queue| queue.front());
            if upload.is_none() {
                scheduler.executor.forget_transfer_waiter(id);
            }
            let preferred = select_queue_kind(
                presence,
                upload.is_some(),
                self.last_admitted_by_remote.get(&id).copied(),
            );
            let order = match preferred {
                Some(QueueKind::Upload) => [QueueKind::Upload, QueueKind::Presence],
                _ => [QueueKind::Presence, QueueKind::Upload],
            };
            for kind in order {
                let lease = match kind {
                    QueueKind::Presence if presence => scheduler.executor.try_presence(id),
                    QueueKind::Upload => upload.and_then(|ready| {
                        scheduler
                            .executor
                            .try_transfer(id, ready.upload.buffer_bytes())
                    }),
                    QueueKind::Presence => None,
                };
                if let Some(lease) = lease {
                    self.next_remote = (position + 1) % self.remote_order.len();
                    self.last_admitted_by_remote.insert(id, kind);
                    return Some((id, kind, lease));
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
                            match state.verification.remove(&oid) {
                                None => {
                                    state.verification.insert(
                                        oid,
                                        VerificationState::Running {
                                            waiting: vec![index],
                                        },
                                    );
                                    state.pending_verification.push_back(oid);
                                }
                                Some(VerificationState::Running { mut waiting }) => {
                                    waiting.push(index);
                                    state
                                        .verification
                                        .insert(oid, VerificationState::Running { waiting });
                                }
                                Some(VerificationState::Valid { source }) => {
                                    enqueue_upload(
                                        &mut state,
                                        &handles,
                                        &objects,
                                        index,
                                        source.clone(),
                                    );
                                    state
                                        .verification
                                        .insert(oid, VerificationState::Valid { source });
                                }
                                Some(VerificationState::Invalid { status }) => {
                                    complete_obligation(&mut state.statuses, index, status, &task);
                                    state
                                        .verification
                                        .insert(oid, VerificationState::Invalid { status });
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
                                            for &index in &waiting {
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
                                            let status =
                                                if verification == ObjectVerification::Missing {
                                                    PublishStatus::CacheMissing
                                                } else {
                                                    PublishStatus::CacheCorrupt
                                                };
                                            for &index in &waiting {
                                                complete_obligation(
                                                    &mut state.statuses,
                                                    index,
                                                    status,
                                                    &task,
                                                );
                                            }
                                            state
                                                .verification
                                                .insert(oid, VerificationState::Invalid { status });
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
    let mut presence = BTreeMap::<RemoteId, Vec<_>>::new();
    while let Some((remote_id, kind, lease)) = state.next_admissible_work(scheduler) {
        match kind {
            QueueKind::Upload => {
                let ready = state
                    .ready_uploads
                    .get_mut(&remote_id)
                    .expect("selected remote has an upload queue")
                    .pop_front()
                    .expect("selected remote has a ready upload");
                let task = scheduler.task.clone();
                completions.push(
                    async move {
                        let _lease = lease;
                        let upload = execute_upload(scheduler.executor, ready.upload, task).await;
                        RemoteCompletion::Upload { upload }
                    }
                    .into_stream()
                    .boxed(),
                );
            }
            QueueKind::Presence => {
                let index = state
                    .pending_presence
                    .get_mut(&remote_id)
                    .expect("selected remote has a presence queue")
                    .pop_front()
                    .expect("selected remote has pending presence work");
                let oid = scheduler.objects[index].oid;
                presence
                    .entry(remote_id)
                    .or_default()
                    .push((index, oid, lease));
                if presence[&remote_id].len()
                    == scheduler.handles[&remote_id]
                        .client()
                        .presence_batch_limit()
                {
                    let entries = presence.remove(&remote_id).unwrap();
                    completions.push(
                        super::presence::presence_stream(
                            scheduler.executor,
                            scheduler.handles[&remote_id].client().clone(),
                            entries,
                        )
                        .map(|(index, result)| RemoteCompletion::Presence {
                            index,
                            result: result
                                .map_err(|source| Box::new(source) as Box<dyn Error + Send + Sync>),
                        })
                        .boxed(),
                    );
                }
            }
        }
    }
    // Partial batches start immediately; admission keeps its existing fair
    // remote/work-class order rather than filling one remote ahead of others.
    for (id, entries) in presence {
        completions.push(
            super::presence::presence_stream(
                scheduler.executor,
                scheduler.handles[&id].client().clone(),
                entries,
            )
            .map(|(index, result)| RemoteCompletion::Presence {
                index,
                result: result.map_err(|source| Box::new(source) as Box<dyn Error + Send + Sync>),
            })
            .boxed(),
        );
    }
}

fn enqueue_upload(
    state: &mut PipelineState,
    handles: &BTreeMap<RemoteId, RemoteHandle>,
    objects: &[PublishObject],
    index: usize,
    source: CacheObject,
) {
    let object = &objects[index];
    state
        .ready_uploads
        .entry(object.remote.id())
        .or_default()
        .push_back(ReadyUpload {
            upload: PreparedUpload::new(
                UploadObject::new(
                    object.oid,
                    object.representative_path.clone(),
                    object.remote,
                ),
                handles[&object.remote.id()].clone(),
                source,
                index,
            ),
        });
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
                    waiting: vec![3, 0],
                },
            );
            state.verification.insert(
                objects[1].oid,
                VerificationState::Running { waiting: vec![1] },
            );
            state.verification.insert(
                objects[2].oid,
                VerificationState::Running { waiting: vec![2] },
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
