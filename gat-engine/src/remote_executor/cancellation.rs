//! Invocation-owned cooperative cancellation shared by transfers and Git.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Cloneable cancellation capability. Cancelling stops new remote work and
/// lets coordinators drain local tasks and abort owned writers before returning.
/// Git uses the same flag at its own safe interruption checkpoints.
#[derive(Clone, Debug)]
pub struct TransferCancellation {
    state: Arc<CancellationState>,
}

#[derive(Debug)]
struct CancellationState {
    interrupt: AtomicBool,
    // Notification only: the atomic is the single source of cancellation state.
    changed: tokio::sync::watch::Sender<()>,
}

impl Default for TransferCancellation {
    fn default() -> Self {
        Self {
            state: Arc::new(CancellationState {
                interrupt: AtomicBool::new(false),
                changed: tokio::sync::watch::channel(()).0,
            }),
        }
    }
}

impl TransferCancellation {
    pub fn cancel(&self) {
        self.state.interrupt.store(true, Ordering::Release);
        self.state.changed.send_replace(());
    }
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.interrupt.load(Ordering::Acquire)
    }

    pub(crate) fn git_interrupt(&self) -> &AtomicBool {
        &self.state.interrupt
    }

    /// Waits until this capability is cancelled, including cancellation before subscription.
    pub async fn cancelled(&self) {
        // Subscribe before reading the flag so cancellation between the check
        // and the await cannot be missed. Keeping self alive retains the sender.
        let mut receiver = self.state.changed.subscribe();
        if !self.is_cancelled() {
            let _ = receiver.changed().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};

    #[derive(Default)]
    struct WakeFlag(AtomicBool);

    impl Wake for WakeFlag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn cancellation_wakes_all_waiters_and_remains_sticky_across_clones() {
        let cancellation = TransferCancellation::default();
        let clone = cancellation.clone();
        let mut first = std::pin::pin!(cancellation.cancelled());
        let mut second = std::pin::pin!(clone.cancelled());
        let first_wake = Arc::new(WakeFlag::default());
        let second_wake = Arc::new(WakeFlag::default());
        let first_waker = Waker::from(first_wake.clone());
        let second_waker = Waker::from(second_wake.clone());
        let mut first_context = Context::from_waker(&first_waker);
        let mut second_context = Context::from_waker(&second_waker);
        assert_eq!(first.as_mut().poll(&mut first_context), Poll::Pending);
        assert_eq!(second.as_mut().poll(&mut second_context), Poll::Pending);

        clone.cancel();
        assert!(cancellation.git_interrupt().load(Ordering::Acquire));
        assert!(first_wake.0.load(Ordering::Acquire));
        assert!(second_wake.0.load(Ordering::Acquire));
        assert_eq!(first.as_mut().poll(&mut first_context), Poll::Ready(()));
        assert_eq!(second.as_mut().poll(&mut second_context), Poll::Ready(()));

        cancellation.cancel();
        assert!(clone.is_cancelled());
        let mut late = std::pin::pin!(clone.cancelled());
        assert_eq!(late.as_mut().poll(&mut first_context), Poll::Ready(()));
    }
}
