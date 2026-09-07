//! Operation-owned cooperative transfer cancellation.

/// Cloneable cancellation capability. Cancelling stops new remote work and
/// lets coordinators drain local tasks and abort owned writers before returning.
#[derive(Clone, Debug)]
pub struct TransferCancellation(tokio::sync::watch::Sender<bool>);

impl Default for TransferCancellation {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }
}

impl TransferCancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    pub(super) async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}
