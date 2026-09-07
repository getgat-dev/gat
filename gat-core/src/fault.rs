//! One-shot, thread-local crash-injection registry shared across command
//! modules (test builds only). Shared by mount transaction and replay
//! fault-injection tests so mount replay can
//! arm/hit the boundary between a filesystem shard/flat `gat.lock` publish
//! and the `SQLite` desired-state transaction containing it committing,
//! where a fault must be injectable without also undoing the already-durable
//! filesystem write (faithfully modeling a process that died there).
//!
//! Thread-local rather than process-global because the test runner executes
//! tests concurrently on separate threads: `cargo test` runs
//! tests concurrently on separate threads, so a shared global armed-fault
//! flag could let one test arm a fault that an unrelated, concurrently
//! running test unexpectedly hits (or silently disarms). Scoping the
//! armed state to the calling thread keeps each test's fault injection
//! isolated no matter what else the suite is doing in parallel.
#[cfg(any(test, feature = "test-support"))]
use std::cell::RefCell;

/// A deterministic crash-injection point (test builds only), shared across
/// command modules via [`hit`]. When a test arms the matching
/// label with [`arm`], the next call at that boundary returns an injected
/// error *without* running any code after it, faithfully modeling a
/// process that died there: the durable on-disk artifacts written up to
/// that point remain exactly as they were, and no cleanup/rollback that
/// follows the boundary runs. In non-test builds this is an inlined no-op
/// the optimizer removes entirely.
/// The single injected failure `hit` can raise: a caller-
/// armed fault fired at exactly the boundary the test named, deliberately
/// carrying only that label -- not a stand-in for any other real failure
/// mode.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, thiserror::Error)]
#[error("injected fault at `{label}`")]
pub struct InjectedFault {
    label: String,
}

#[cfg(any(test, feature = "test-support"))]
type Result<T> = std::result::Result<T, InjectedFault>;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static ARMED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Arm the injection point `label`: the next [`hit`] with this label fails
/// once, then disarms itself (so recovery re-running the same boundary
/// proceeds).
#[cfg(any(test, feature = "test-support"))]
pub fn arm(label: &str) {
    ARMED.with(|a| *a.borrow_mut() = Some(label.to_string()));
}

/// Clear any armed injection point.
#[cfg(any(test, feature = "test-support"))]
pub fn disarm() {
    ARMED.with(|a| *a.borrow_mut() = None);
}

/// Fail (once) if `label` is currently armed; otherwise a no-op.
#[cfg(any(test, feature = "test-support"))]
pub fn hit(label: &str) -> Result<()> {
    let armed = ARMED.with(|a| a.borrow().as_deref() == Some(label));
    if armed {
        ARMED.with(|a| *a.borrow_mut() = None);
        return Err(InjectedFault {
            label: label.to_string(),
        });
    }
    Ok(())
}

/// RAII guard returned by [`armed`]: disarms the fault when dropped,
/// including during a panic/unwind. Prefer this over calling [`arm`] and
/// [`disarm`] directly around a fallible body: a test's own assertion
/// failure (or the operation under test panicking instead of returning
/// an `Err`) would otherwise skip a manual `disarm()` call, leaving the
/// label armed in this thread's `ARMED` slot for the rest of the test
/// binary's run -- and since `cargo test`'s default runner reuses worker
/// threads across many test functions over time, a later, wholly
/// unrelated test that happens to land on the same thread and reach the
/// same injection point could then spuriously trip the still-armed
/// fault.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub struct ArmedGuard(());

#[cfg(any(test, feature = "test-support"))]
impl Drop for ArmedGuard {
    fn drop(&mut self) {
        disarm();
    }
}

/// Arms `label` for the duration of the returned guard's scope, disarming
/// it again on drop -- panic-safe, unlike a manual `arm`/`disarm` pair.
#[cfg(any(test, feature = "test-support"))]
pub fn armed(label: &str) -> ArmedGuard {
    arm(label);
    ArmedGuard(())
}
