use gat_command::{AddRequest, ConfigAction, ConfigRequest, RemoveRequest, add, config, remove};
use gat_core::config::ConfigScope;
use gat_core::config_keys::SettingKey;
use gat_core::path_scope::normalize_path_scope;
use gat_core::progress::{
    ActivityBackend, NoopProgress, ProgressActivity, ProgressOperation, ProgressReporter,
    ProgressSpec, ProgressTask, ProgressUnit,
};
use gat_engine::Repository;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub fn repository() -> (tempfile::TempDir, Repository) {
    let temp = tempfile::tempdir().unwrap();
    test_support_git::run_git(temp.path(), &["init", "-q", "-b", "main"]);
    test_support_git::run_git(
        temp.path(),
        &["commit", "-q", "--allow-empty", "-m", "initial"],
    );
    let repo = gat_engine::Invocation::from_pairs([] as [(&str, &str); 0])
        .unwrap()
        .repository_at(temp.path().to_path_buf());
    (temp, repo)
}

pub fn commit_all(root: &std::path::Path, message: &str) {
    test_support_git::commit_all(root, message);
}

#[allow(dead_code)] // Shared test module is compiled independently per integration test.
pub fn git(root: &std::path::Path, args: &[&str]) {
    test_support_git::run_git(root, args);
}

pub fn add_paths(repo: &Repository, paths: &[PathBuf]) {
    add(
        repo,
        AddRequest {
            paths: paths
                .iter()
                .map(normalize_path_scope)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            force: false,
        },
        &NoopProgress,
    )
    .unwrap();
}

#[allow(dead_code)] // Shared test module is compiled independently per integration test.
pub fn remove_paths(repo: &Repository, paths: &[PathBuf]) {
    remove(
        repo,
        RemoveRequest {
            paths: paths
                .iter()
                .map(normalize_path_scope)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            cached: true,
        },
    )
    .unwrap();
}

#[allow(dead_code)] // Shared test module is compiled independently per integration test.
pub fn set_lock_shard_levels(repo: &Repository, levels: u8) {
    config(
        repo,
        ConfigRequest::new(
            SettingKey::LockShardLevels,
            ConfigAction::Set(vec![levels.to_string()]),
            ConfigScope::Project,
        ),
    )
    .unwrap();
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedTask {
    pub operation: ProgressOperation,
    pub unit: Option<ProgressUnit>,
    pub total: Option<u64>,
    pub position: u64,
    pub finished: bool,
}

struct TaskState {
    operation: ProgressOperation,
    unit: Option<ProgressUnit>,
    total: Option<u64>,
    position: AtomicU64,
    finished: AtomicBool,
}

#[derive(Default)]
struct RecordingInner {
    tasks: Mutex<Vec<Arc<TaskState>>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

struct RecordingBackend {
    inner: Arc<RecordingInner>,
    state: Arc<TaskState>,
}

impl ActivityBackend for RecordingBackend {
    fn inc(&self, delta: u64) {
        self.state.position.fetch_add(delta, Ordering::SeqCst);
    }

    fn set_activity(&self, _activity: &ProgressActivity) {}

    fn finish(&self) {
        if !self.state.finished.swap(true, Ordering::SeqCst) {
            self.inner.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Default)]
pub struct RecordingProgress {
    inner: Arc<RecordingInner>,
}

impl RecordingProgress {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)] // Shared test module is compiled independently per integration test.
    pub fn operations(&self) -> Vec<ProgressOperation> {
        self.inner
            .tasks
            .lock()
            .unwrap()
            .iter()
            .map(|task| task.operation)
            .collect()
    }

    pub fn max_active_tasks(&self) -> usize {
        self.inner.max_active.load(Ordering::SeqCst)
    }

    pub fn only(&self, operation: ProgressOperation) -> RecordedTask {
        let matching = self
            .inner
            .tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|task| task.operation == operation)
            .map(|task| RecordedTask {
                operation: task.operation,
                unit: task.unit,
                total: task.total,
                position: task.position.load(Ordering::SeqCst),
                finished: task.finished.load(Ordering::SeqCst),
            })
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "expected exactly one {operation:?} task");
        matching.into_iter().next().unwrap()
    }
}

impl ProgressReporter for RecordingProgress {
    fn begin(&self, spec: ProgressSpec) -> ProgressTask {
        let state = Arc::new(TaskState {
            operation: spec.operation(),
            unit: spec.unit(),
            total: spec.total(),
            position: AtomicU64::new(0),
            finished: AtomicBool::new(false),
        });
        self.inner.tasks.lock().unwrap().push(Arc::clone(&state));
        let active = self.inner.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner.max_active.fetch_max(active, Ordering::SeqCst);
        ProgressTask::from_backend(Arc::new(RecordingBackend {
            inner: Arc::clone(&self.inner),
            state,
        }))
    }
}
