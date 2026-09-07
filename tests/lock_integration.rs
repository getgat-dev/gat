//! Integration tests for the repository advisory lock, exercising real
//! cross-process mutual exclusion and OS-level crash recovery.
//!
//! These tests spawn this integration-test executable in a dedicated ignored
//! helper-test mode so that the OS file-description semantics are exercised
//! exactly as they would be in production, rather than approximated by
//! same-process file handles or threads. Keeping the helper here avoids adding
//! a test-only binary or dependency to the root crate's production targets.

use std::io::{BufRead as _, Read as _, Write as _};
use std::time::Duration;
use test_support::ChildGuard;

const LOCK_PATH_ENV: &str = "GAT_LOCK_INTEGRATION_HELPER_PATH";
const LOCKED_MARKER: &str = "gat-lock-integration-helper-locked";

/// How long a readiness handshake may take before a test fails with a
/// clear timeout message rather than hanging forever (see
/// `test_support::read_line_within`). Only used for the pre-exit
/// "lock holder has acquired the lock" handshake in [`spawn_holder`] --
/// the post-exit lock-release assertion in
/// [`assert_lock_released_after_exit`] is a single immediate check, not
/// a poll, and doesn't use this constant: polling must never determine
/// turn a post-exit immediate-release regression green.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

fn open_write(path: &std::path::Path) -> std::fs::File {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

/// Spawn this test executable's ignored lock-holder case as a guarded child
/// process and wait with a bounded timeout until it prints [`LOCKED_MARKER`],
/// confirming that it has acquired the advisory lock.
/// The child's stdin is piped so the test can trigger a graceful exit by
/// dropping (closing) the stdin handle. Wrapped in a [`ChildGuard`] so a
/// failed assertion later in the test still kills/reaps the child rather
/// than leaking it.
fn spawn_holder(path: &std::path::Path) -> ChildGuard {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--ignored", "--exact", "lock_holder_process", "--nocapture"])
        .env(LOCK_PATH_ENV, path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut guard = ChildGuard::spawn(cmd);

    let stdout = guard.take_stdout().expect("lock-holder stdout was piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut readiness = Some(tx);
        for line in std::io::BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line.contains(LOCKED_MARKER) => {
                    if let Some(tx) = readiness.take() {
                        let _ = tx.send(Ok(()));
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    if let Some(tx) = readiness.take() {
                        let _ = tx.send(Err(err));
                    }
                    return;
                }
            }
        }
        if let Some(tx) = readiness {
            let _ = tx.send(Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "lock-holder exited before signaling readiness",
            )));
        }
    });
    match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("waiting for lock-holder readiness: {err}"),
        Err(err) => {
            panic!("failed after {HANDSHAKE_TIMEOUT:?} waiting for lock-holder readiness: {err}")
        }
    }
    guard
}

#[test]
#[ignore = "spawned by the cross-process advisory-lock tests"]
fn lock_holder_process() {
    use fs2::FileExt as _;

    let path = std::env::var_os(LOCK_PATH_ENV)
        .expect("lock-holder test requires its lock-path environment variable");
    let file = open_write(std::path::Path::new(&path));
    file.try_lock_exclusive()
        .unwrap_or_else(|err| panic!("lock {}: {err}", std::path::Path::new(&path).display()));

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{LOCKED_MARKER}").expect("write lock-holder readiness marker");
    stdout.flush().expect("flush lock-holder readiness marker");

    let mut input = Vec::new();
    std::io::stdin()
        .read_to_end(&mut input)
        .expect("wait for lock-holder shutdown");
}

/// Verifies the primary contract under test: process exit must release
/// the advisory lock automatically. POSIX flock/fcntl semantics release
/// these locks synchronously as part of the same fd-closing exit that
/// `holder.wait()` (already called by every caller before this) has
/// already reaped -- so on every currently supported platform (Linux,
/// macOS, Windows) the very first non-blocking exclusive-lock attempt
/// after `wait()` must succeed immediately. There is no legitimate
/// in-between window to poll through, so this fails immediately (with
/// the observed OS error) rather than retrying -- a poll loop here would
/// only ever mask a real regression to eventual-rather-than-immediate
/// release semantics as a passing test. If a concrete supported platform
/// is ever observed to need a bounded eventual-release fallback instead,
/// add it back deliberately, scoped to that platform and documented with
/// the root cause, rather than reintroducing a generic timeout.
fn assert_lock_released_after_exit(file_b: &std::fs::File) {
    use fs2::FileExt as _;

    match file_b.try_lock_exclusive() {
        Ok(()) => {
            file_b
                .unlock()
                .expect("unlocking after the immediate post-exit acquisition");
        }
        Err(err) => panic!(
            "lock must be released immediately after holder process exit \
             (already reaped by wait()); the immediate try_lock_exclusive \
             attempt failed with: {err}"
        ),
    }
}

// -------------------------------------------------------------------------
// Test: inter-process mutual exclusion with graceful exit
// -------------------------------------------------------------------------

/// While process A holds the advisory lock, process B cannot acquire it.
/// After A exits gracefully (stdin closed), B can acquire the lock.
#[test]
fn advisory_lock_mutual_exclusion_across_processes() {
    use fs2::FileExt as _;

    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("sync.lock");

    // Process A: acquire the lock.
    let mut holder = spawn_holder(&lock_path);

    // Process B (this process): the non-blocking attempt must fail.
    let file_b = open_write(&lock_path);
    let contended = file_b.try_lock_exclusive();
    assert!(
        contended
            .as_ref()
            .is_err_and(|e| e.kind() == fs2::lock_contended_error().kind()),
        "expected lock contention while holder process is alive; got {contended:?}"
    );

    // Let A exit gracefully by closing its stdin; the OS releases the lock.
    drop(holder.take_stdin());
    holder.wait().unwrap();

    // The process exit above must have already released the lock; see
    // this function's doc comment for exactly what property this proves
    // and why the assertion is a single immediate check, not a poll.
    assert_lock_released_after_exit(&file_b);
}

// -------------------------------------------------------------------------
// Test: crash recovery — OS releases lock on unexpected process exit
// -------------------------------------------------------------------------

/// Process A holds the advisory lock and is killed without any application-
/// level cleanup (simulating a crash).  Process B can subsequently acquire
/// the lock because the OS released it when A's file descriptors closed.
#[test]
fn advisory_lock_released_after_holder_process_is_killed() {
    use fs2::FileExt as _;

    let tmp = tempfile::tempdir().unwrap();
    let lock_path = tmp.path().join("sync.lock");

    // Process A: acquire the lock.
    let mut holder = spawn_holder(&lock_path);

    // Verify B cannot acquire while A is alive.
    let file_b = open_write(&lock_path);
    assert!(
        file_b
            .try_lock_exclusive()
            .is_err_and(|e| e.kind() == fs2::lock_contended_error().kind()),
        "lock should be held by the holder process"
    );

    // Kill A without any graceful shutdown — simulates a crash.
    holder.kill().unwrap();
    holder.wait().unwrap();

    // The process exit above (even via kill, not a graceful shutdown)
    // must have already released the lock; see this function's doc
    // comment for exactly what property this proves and why the
    // assertion is a single immediate check, not a poll.
    assert_lock_released_after_exit(&file_b);
}
