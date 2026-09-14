//! Shared cadence for coordinator-owned transfer snapshots.
use tokio::time::{Duration, Instant};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn deadline<S: Copy + Eq>(last: Option<(S, Instant)>, current: S) -> Option<Instant> {
    last.and_then(|(previous, when)| (previous != current).then(|| when + REFRESH_INTERVAL))
}

/// A coordinator with a deferred snapshot. The wait policy is shared by push
/// and presence reporting; snapshots and counter semantics remain owned by each
/// operation rather than generalized into an untyped reporting abstraction.
pub(crate) trait Refresh {
    fn deadline(&self) -> Option<Instant>;
    fn flush(&mut self);

    async fn next<S: futures::Stream + Unpin>(
        &mut self,
        completions: &mut S,
        timer: &mut RefreshTimer,
    ) -> Option<S::Item> {
        use futures::StreamExt;
        loop {
            let Some(deadline) = self.deadline() else {
                return completions.next().await;
            };
            tokio::select! {
                biased;
                () = wait_for_refresh(timer, Some(deadline)) => self.flush(),
                completion = completions.next() => return completion,
            }
        }
    }
}

pub(crate) type RefreshTimer = Option<std::pin::Pin<Box<tokio::time::Sleep>>>;

/// Lazily create and reuse one timer. With no deadline this remains pending
/// without touching a Tokio runtime, allocating, or polling a stale timer.
pub(crate) async fn wait_for_refresh(timer: &mut RefreshTimer, deadline: Option<Instant>) {
    let Some(deadline) = deadline else {
        return std::future::pending().await;
    };
    let sleep = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
    if sleep.deadline() != deadline {
        sleep.as_mut().reset(deadline);
    }
    sleep.as_mut().await;
}
