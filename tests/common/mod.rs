//! Shared helpers for external integration-test binaries. Each test file is
//! compiled as its own crate and cannot reach the library's crate-private
//! unit-test fixtures, so this module owns reusable integration-only process
//! isolation and neutral progress recording without exposing production APIs.
//!
//! [`gat_bin`] is also the single place that resolves *which* `gat`
//! executable gets spawned: ordinary test runs use Cargo's own
//! `CARGO_BIN_EXE_gat`, while release validation can point every helper
//! in this module at an explicit packaged candidate binary via
//! `GAT_TEST_BIN`.
//!
//! Not every helper here is used by every test binary that imports this
//! module, hence `#![allow(dead_code)]`.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use gat_core::progress::{
    ActivityBackend, ProgressActivity, ProgressOperation, ProgressReporter, ProgressSpec,
    ProgressTask, ProgressUnit,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedTask {
    pub id: TaskId,
    pub operation: ProgressOperation,
    pub unit: Option<ProgressUnit>,
    pub initial_total: Option<u64>,
    pub total: Option<u64>,
    pub position: u64,
    pub activities: Vec<ProgressActivity>,
    pub finished: bool,
}

#[derive(Default)]
struct RecordedTaskState {
    operation: Option<ProgressOperation>,
    unit: Option<ProgressUnit>,
    initial_total: Option<u64>,
    total: Option<u64>,
    position: u64,
    activities: Vec<ProgressActivity>,
    finished: bool,
}

struct RecordingBackend {
    state: Arc<Mutex<RecordedTaskState>>,
    finished: AtomicBool,
    finishes: Arc<AtomicU64>,
    active_tasks: Arc<AtomicUsize>,
}

impl ActivityBackend for RecordingBackend {
    fn inc(&self, delta: u64) {
        let mut state = self.state.lock().unwrap();
        debug_assert!(
            state.unit.is_some(),
            "progress positions require a declared unit"
        );
        state.position += delta;
    }

    fn set_activity(&self, activity: &ProgressActivity) {
        self.state.lock().unwrap().activities.push(activity.clone());
    }

    fn finish(&self) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        self.state.lock().unwrap().finished = true;
        self.finishes.fetch_add(1, Ordering::SeqCst);
        self.active_tasks.fetch_sub(1, Ordering::SeqCst);
    }
}

type TaskRegistry = Arc<Mutex<Vec<(TaskId, Arc<Mutex<RecordedTaskState>>)>>>;

#[derive(Clone, Default)]
pub struct RecordingProgress {
    tasks: TaskRegistry,
    next_task_id: Arc<AtomicU64>,
    finishes: Arc<AtomicU64>,
    active_tasks: Arc<AtomicUsize>,
    max_active_tasks: Arc<AtomicUsize>,
}

impl RecordingProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tasks(&self) -> Vec<RecordedTask> {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .map(|(id, state)| {
                let state = state.lock().unwrap();
                RecordedTask {
                    id: *id,
                    operation: state
                        .operation
                        .expect("operation is set when the task begins"),
                    unit: state.unit,
                    initial_total: state.initial_total,
                    total: state.total,
                    position: state.position,
                    activities: state.activities.clone(),
                    finished: state.finished,
                }
            })
            .collect()
    }

    pub fn operations(&self) -> Vec<ProgressOperation> {
        self.tasks()
            .into_iter()
            .map(|task| task.operation)
            .collect()
    }

    pub fn finish_count(&self) -> usize {
        usize::try_from(self.finishes.load(Ordering::SeqCst))
            .expect("finish count must fit in usize")
    }

    pub fn is_active(&self, operation: ProgressOperation) -> bool {
        self.tasks.lock().unwrap().iter().any(|(_, state)| {
            let state = state.lock().unwrap();
            state.operation == Some(operation) && !state.finished
        })
    }

    pub fn count_of(&self, operation: ProgressOperation) -> usize {
        self.tasks()
            .into_iter()
            .filter(|task| task.operation == operation)
            .count()
    }

    pub fn only(&self, operation: ProgressOperation) -> RecordedTask {
        let mut matches: Vec<_> = self
            .tasks()
            .into_iter()
            .filter(|task| task.operation == operation)
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one {operation:?} task, found {}",
            matches.len()
        );
        matches.remove(0)
    }

    pub fn max_active_tasks(&self) -> usize {
        self.max_active_tasks.load(Ordering::SeqCst)
    }

    pub fn active_tasks(&self) -> usize {
        self.active_tasks.load(Ordering::SeqCst)
    }
}

impl ProgressReporter for RecordingProgress {
    fn begin(&self, spec: ProgressSpec) -> ProgressTask {
        let id = TaskId(self.next_task_id.fetch_add(1, Ordering::SeqCst));
        let state = Arc::new(Mutex::new(RecordedTaskState {
            operation: Some(spec.operation()),
            unit: spec.unit(),
            initial_total: spec.total(),
            total: spec.total(),
            ..Default::default()
        }));
        self.tasks.lock().unwrap().push((id, Arc::clone(&state)));
        let active = self.active_tasks.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active_tasks.fetch_max(active, Ordering::SeqCst);
        ProgressTask::from_backend(Arc::new(RecordingBackend {
            state,
            finished: AtomicBool::new(false),
            finishes: Arc::clone(&self.finishes),
            active_tasks: Arc::clone(&self.active_tasks),
        }))
    }
}

/// Runtime override for the `gat` executable under test. Ordinary `cargo
/// test`/nextest runs leave this unset and exercise Cargo's own compiled
/// binary (`CARGO_BIN_EXE_gat`); release validation sets `GAT_TEST_BIN` to
/// the executable extracted from a just-packaged candidate release
/// archive, so the exact packaged bytes -- not merely another `cargo
/// build` output -- are what gets spawned.
const GAT_TEST_BIN_ENV: &str = "GAT_TEST_BIN";

/// The environment variable a spawned `gat` reads to find its home
/// directory: `USERPROFILE` on Windows, `HOME` everywhere else. Shared by
/// every helper below that needs to isolate a spawned `gat` from the
/// developer/CI machine's real home directory.
#[cfg(windows)]
pub const HOME_ENV_VAR: &str = "USERPROFILE";
#[cfg(not(windows))]
pub const HOME_ENV_VAR: &str = "HOME";

/// An empty fake home directory shared by every spawned-`gat` helper in
/// this module, owned for the lifetime of the integration test process
/// (a `static` `TempDir`, lazily created once and reused by every test
/// thread rather than one per call). Only ever *read* by spawned `gat`
/// processes looking for `~/.gat/gat.yaml` -- never written to by any
/// test in this crate -- so sharing it across parallel test threads is
/// sound: every test sees the same, permanently empty global-config
/// directory, exactly as if the developer/CI machine's real home simply
/// had no `~/.gat/gat.yaml` at all.
///
/// This is the fixture behind [`gat_with_env`]'s default isolation. Without
/// it, a spawned `gat` would fall back to the test
/// process's own real, ambient `$HOME`/`%USERPROFILE%`, making test
/// behavior depend on whatever global Gat configuration happens to exist
/// on the machine running the suite.
fn fake_home() -> &'static Path {
    static FAKE_HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    FAKE_HOME
        .get_or_init(|| tempfile::tempdir().expect("creating shared fake-home tempdir"))
        .path()
}

/// Path to the `gat` binary under test: [`GAT_TEST_BIN_ENV`] if set,
/// otherwise the binary Cargo built for this test run
/// (`CARGO_BIN_EXE_gat`). This is the single place that resolves which
/// `gat` executable every helper in this module spawns.
pub fn gat_bin() -> PathBuf {
    match std::env::var_os(GAT_TEST_BIN_ENV) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(env!("CARGO_BIN_EXE_gat")),
    }
}

/// `PATH` with the built `gat` binary's directory prepended, so shell
/// dispatcher lines (`gat hook post-checkout "$@"`) installed by `gat
/// init` resolve to this build rather than whatever (if anything) is on
/// the ambient `PATH`. Built with
/// [`std::env::join_paths`]/`split_paths` rather than a literal `:`
/// separator, so this works unmodified on Windows (`;`-separated `PATH`).
pub fn path_with_gat() -> OsString {
    let gat_dir = gat_bin()
        .parent()
        .expect("gat binary path has a parent directory")
        .to_path_buf();
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![gat_dir];
    dirs.extend(std::env::split_paths(&existing));
    std::env::join_paths(dirs).expect("PATH components must not contain the path separator")
}

/// Applies the single default isolated child environment
/// shared by every helper in this module that spawns a process capable of
/// executing `gat`, whether directly (a spawned `gat`) or indirectly (a
/// spawned `git` that can invoke `gat` through an installed hook or the
/// semantic merge driver): the child's home directory ([`HOME_ENV_VAR`])
/// is pointed at [`fake_home`]'s permanently empty directory, the
/// global/system git config is redirected to [`isolated_gitconfig`], any
/// `GAT_CACHE_DIR` the test *process* happens to have inherited is
/// stripped from the child rather than passed through, and `PATH` is set
/// via [`path_with_gat`] so any hook dispatcher/merge-driver invocation
/// resolves `gat` to the build under test.
///
/// [`gat_with_env`] and [`git`]/[`git_with_env`] all build on this one
/// helper, so a subprocess spawned either way is isolated from ambient
/// Gat/Git state identically -- no ordinary test exercising some other
/// behavior silently depends on (or is broken by) the developer/CI
/// machine's real global Gat/Git config or an inherited cache relocation.
pub fn isolated_child_env(cmd: &mut Command) {
    cmd.env(HOME_ENV_VAR, fake_home())
        .env("GIT_CONFIG_GLOBAL", test_support_git::isolated_gitconfig())
        .env("GIT_CONFIG_SYSTEM", test_support_git::isolated_gitconfig())
        .env("PATH", path_with_gat())
        .env_remove("GAT_CACHE_DIR");
}

/// Runs `git` with `args` in `dir`, returning the raw captured [`Output`]
/// regardless of exit status (callers assert success themselves via
/// [`assert_ok`]). Built on the shared `test-support` crate's
/// [`test_support_git::GitCommand`] primitive -- which supplies the fixed,
/// non-config-dependent git author/committer identity -- layered with
/// this module's own [`isolated_child_env`], the same default isolation
/// [`gat_with_env`] applies, so a `git` subprocess that transitively
/// invokes `gat` through an installed hook or the semantic merge driver
/// is isolated from ambient Gat/Git state exactly like a direct
/// [`gat_with_env`] spawn.
pub fn git(dir: &Path, args: &[&str]) -> Output {
    test_support_git::GitCommand::new(dir, args)
        .with_command(isolated_child_env)
        .output()
}

/// [`git`]'s counterpart to [`gat_with_env`]: runs `git` with `args` in
/// `dir` under the same default isolated child environment, then applies
/// `extra_env` overrides strictly *after* that default isolation is
/// established, so a test can give the `git` child -- and, transitively,
/// any `gat` process it invokes through an installed hook or the semantic
/// merge driver -- a deliberately conflicting environment without ever
/// mutating this test process's own environment. Each entry is `(name,
/// Some(value))` to set/override a variable for the child, or `(name,
/// None)` to explicitly unset one of the defaulted variables above for
/// the child, mirroring [`gat_with_env`]'s `extra_env` semantics exactly.
pub fn git_with_env(dir: &Path, args: &[&str], extra_env: &[(&str, Option<&str>)]) -> Output {
    let mut cmd = test_support_git::GitCommand::new(dir, args).with_command(isolated_child_env);
    for (k, v) in extra_env {
        cmd = match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.output()
}

/// Runs the compiled `gat` binary with `args` in `dir`, plus any
/// `extra_env` overrides applied on top of [`isolated_child_env`]'s
/// default isolation -- see that helper's doc comment for exactly what
/// isolation every spawned `gat` gets by default.
///
/// `extra_env` is applied strictly *after* the default isolation
/// environment is established, so a test that specifically needs to
/// characterize `HOME`/`GAT_CACHE_DIR` handling can still override
/// either for the child process, without ever mutating this test
/// process's own environment (unsound to do concurrently across test
/// threads). Each entry is `(name, Some(value))` to set/override a
/// variable for the child, or `(name, None)` to explicitly *unset* one
/// of the two defaulted variables above for the child -- e.g. to
/// characterize the unset-`GAT_CACHE_DIR` path -- rather than falling
/// back to whatever this test process's own environment happens to
/// contain.
pub fn gat_with_env(dir: &Path, args: &[&str], extra_env: &[(&str, Option<&str>)]) -> Output {
    let mut cmd = Command::new(gat_bin());
    cmd.args(args).current_dir(dir);
    isolated_child_env(&mut cmd);
    for (k, v) in extra_env {
        match v {
            Some(v) => {
                cmd.env(k, v);
            }
            None => {
                cmd.env_remove(k);
            }
        }
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("running gat {args:?}: {e}"))
}

/// Runs the compiled `gat` binary with `args` in `dir`.
pub fn gat(dir: &Path, args: &[&str]) -> Output {
    gat_with_env(dir, args, &[])
}

/// Asserts `out` succeeded, panicking with the captured stdout/stderr
/// (labeled by `what`) otherwise.
pub fn assert_ok(out: &Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Renders `command: gat <args>` / `cwd: <dir>` for the richer assertion
/// helpers below, so a failure message always names exactly which
/// invocation, in which directory, produced it.
fn describe(dir: &Path, args: &[&str]) -> String {
    format!("command: gat {}\ncwd: {}", args.join(" "), dir.display())
}

/// Renders every diagnostic a failing assertion below should print:
/// which command, in which directory, its exit code, and both full
/// output streams -- so a failure is debuggable from the test output
/// alone, without needing to reproduce it locally.
fn full_diagnostics(dir: &Path, args: &[&str], out: &Output) -> String {
    format!(
        "{}\nexit code: {:?}\nstdout:\n{}\nstderr:\n{}",
        describe(dir, args),
        out.status.code(),
        stdout(out),
        stderr(out)
    )
}

/// Richer alternative to [`assert_ok`]: asserts `out`'s process exited
/// successfully, panicking with the command, its working directory,
/// exit code, and both full output streams otherwise. Existing
/// `assert_ok` call sites are migrating to this API gradually,
/// not all at once, so both remain available; prefer
/// this one for new tests.
pub fn assert_success(dir: &Path, args: &[&str], out: &Output) {
    assert!(
        out.status.success(),
        "expected success, got failure\n{}",
        full_diagnostics(dir, args, out)
    );
}

/// Asserts `out`'s process exited with exactly `code`, with the same
/// full diagnostics as [`assert_success`] on mismatch.
pub fn assert_failure_code(dir: &Path, args: &[&str], out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "expected exit code {code}\n{}",
        full_diagnostics(dir, args, out)
    );
}

/// Asserts `out`'s stdout contains `needle`, with full diagnostics on
/// mismatch.
pub fn assert_stdout_contains(dir: &Path, args: &[&str], out: &Output, needle: &str) {
    assert!(
        stdout(out).contains(needle),
        "expected stdout to contain {needle:?}\n{}",
        full_diagnostics(dir, args, out)
    );
}

/// Asserts `out`'s stderr contains `needle`, with full diagnostics on
/// mismatch.
pub fn assert_stderr_contains(dir: &Path, args: &[&str], out: &Output, needle: &str) {
    assert!(
        stderr(out).contains(needle),
        "expected stderr to contain {needle:?}\n{}",
        full_diagnostics(dir, args, out)
    );
}

/// A repo with `git init` + `gat init` (hooks installed), no commits yet.
/// Built on the shared `test-support` crate's `TestRepo::empty_gat_repo`
/// (in-process `gat init` against the real production entry point)
/// rather than spawning `gat init` as a subprocess: repo setup itself
/// isn't the process-boundary behavior most callers of this helper are
/// testing (they go on to spawn `gat`/`git` for the command actually
/// under test), so it doesn't need an extra process.
///
/// The one exception is release-artifact acceptance: a test
/// asserting the *packaged* binary works must have the packaged binary
/// itself perform `gat init`, not this in-process fixture -- see
/// [`init_repo_spawned`].
pub fn init_repo() -> test_support::TestRepo {
    test_support::TestRepo::empty_gat_repo()
}

/// [`init_repo`]'s spawned-binary counterpart: an isolated `git init`
/// (via [`test_support::TestRepo::empty_git_repo`], not `gat`) followed
/// by `gat init` run through the actual binary under test ([`gat_bin`],
/// honoring `GAT_TEST_BIN` for release-artifact acceptance). Every
/// subsequent Gat operation in a test built on this helper
/// -- not merely the commands it explicitly spawns afterward -- is then
/// provably reachable through the same packaged executable, including
/// initialization itself: a broken packaged `gat init` (hook
/// installation or merge-driver/`info/attributes` wiring) cannot be silently masked by falling back to
/// in-process fixture setup, unlike [`init_repo`].
///
/// Reserved for `tests/cli_integration/release_artifact.rs`'s
/// `release_artifact_`-prefixed subset: every
/// other caller should keep using the cheaper in-process [`init_repo`],
/// since ordinary command-behavior tests are not asserting anything
/// about `gat init` itself.
pub fn init_repo_spawned() -> test_support::TestRepo {
    let repo = test_support::TestRepo::empty_git_repo();
    assert_ok(&gat(repo.path(), &["init"]), "gat init");
    repo
}

/// Stages and commits every file currently in the working tree (`git add
/// -A; git commit -q -m msg`).
pub fn commit_all(dir: &Path, msg: &str) {
    assert_ok(&git(dir, &["add", "-A"]), "git add");
    assert_ok(&git(dir, &["commit", "-q", "-m", msg]), "git commit");
}

/// A real git repo (`git init`) with an initial commit, so `gat`'s own
/// repo discovery has something to discover. Unlike [`init_repo`], this
/// does *not* run `gat init` -- used by tests that call `app::run`/library
/// APIs directly rather than spawning the `gat` binary. Built on the
/// shared `test-support` crate's `TestRepo::git_repo_with_initial_commit`
/// rather than its own raw `git init`/commit sequence.
pub fn test_repo() -> test_support::TestRepo {
    test_support::TestRepo::git_repo_with_initial_commit()
}

/// Baseline implementation/backend vocabulary that must never reach a
/// user-facing stream: crate/module identifiers, low-level error-kind
/// names, and raw-panic/backtrace markers. Kept case-sensitive and
/// deliberately narrow (real identifiers, not ordinary English words)
/// so this doesn't false-positive on legitimate product wording.
const FORBIDDEN_LOW_LEVEL_VOCABULARY: &[&str] = &[
    "rusqlite",
    "SqliteFailure",
    "opendal",
    "ErrorKind",
    "gix::",
    "gix_",
    "tokio",
    "JoinError",
    "PRAGMA",
    "os error",
    "raw_os_error",
    "Caused by:",
    "cause:",
    "backtrace",
    "panicked at",
];

/// Strips ANSI SGR/cursor escape sequences (`\x1b[...<letter>`) from `s`,
/// for *semantic* substring comparisons that shouldn't care whether a
/// word is colored/bold. Adversarial leak checks should also inspect the
/// untouched raw bytes separately (terminal injection can hide inside
/// the escape sequences themselves), which is why this is a separate
/// helper rather than baked silently into the assertion below.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Asserts that neither `stdout` nor `stderr` of `out` contain any of
/// `secrets` verbatim (e.g. a configured credential, URL password, API
/// token, or query-string value) -- checked independently from
/// [`assert_no_low_level_error_leak`]'s low-level vocabulary list, since
/// a secret leak is a distinct, higher-severity failure mode from an
/// implementation-detail leak and callers should be able to assert one
/// without the other. Compares against ANSI-stripped text.
pub fn assert_no_secret_leak(out: &Output, secrets: &[&str]) {
    let raw_stdout = stdout(out);
    let raw_stderr = stderr(out);
    let clean_stdout = strip_ansi(&raw_stdout);
    let clean_stderr = strip_ansi(&raw_stderr);

    for secret in secrets {
        assert!(
            !clean_stdout.contains(secret),
            "stdout must never contain the configured secret {secret:?}\nstdout:\n{raw_stdout}\nstderr:\n{raw_stderr}"
        );
        assert!(
            !clean_stderr.contains(secret),
            "stderr must never contain the configured secret {secret:?}\nstdout:\n{raw_stdout}\nstderr:\n{raw_stderr}"
        );
    }
}

/// Asserts that neither `stdout` nor `stderr` of `out` contain any
/// baseline forbidden low-level/implementation vocabulary
/// ([`FORBIDDEN_LOW_LEVEL_VOCABULARY`]) or any of `extra_sentinels`
/// (test-specific unique markers, e.g. a distinctive injected
/// `std::io::Error` message, that must never surface verbatim to the
/// user). Comparisons are done on ANSI-stripped text (so colored output
/// doesn't split a forbidden substring across an escape sequence), but
/// this only covers semantic leakage -- callers exercising raw
/// terminal-injection concerns should additionally inspect
/// `out.stdout`/`out.stderr` byte-for-byte themselves.
///
/// Panics with the full stdout/stderr (undecorated) on any match, so a
/// failure is immediately actionable without needing to reproduce it.
pub fn assert_no_low_level_error_leak(out: &Output, extra_sentinels: &[&str]) {
    let raw_stdout = stdout(out);
    let raw_stderr = stderr(out);
    let clean_stdout = strip_ansi(&raw_stdout);
    let clean_stderr = strip_ansi(&raw_stderr);

    let mut needles: Vec<&str> = FORBIDDEN_LOW_LEVEL_VOCABULARY.to_vec();
    needles.extend_from_slice(extra_sentinels);

    for needle in needles {
        assert!(
            !clean_stdout.contains(needle),
            "stdout must never contain low-level/sentinel text {needle:?}\nstdout:\n{raw_stdout}\nstderr:\n{raw_stderr}"
        );
        assert!(
            !clean_stderr.contains(needle),
            "stderr must never contain low-level/sentinel text {needle:?}\nstdout:\n{raw_stdout}\nstderr:\n{raw_stderr}"
        );
    }
}
