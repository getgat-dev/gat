//! Shared cadence for coordinator-owned transfer snapshots.
use tokio::time::{Duration, Instant};

pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn deadline<S: Copy + Eq>(last: Option<(S, Instant)>, current: S) -> Option<Instant> {
    last.and_then(|(previous, when)| (previous != current).then(|| when + REFRESH_INTERVAL))
}

pub(crate) fn due<S: Copy + Eq, A: Eq>(
    last: Option<(S, Instant)>,
    current: S,
    active: impl Fn(S) -> A,
    force: bool,
    now: Instant,
) -> bool {
    last.is_none_or(|(previous, when)| {
        previous != current
            && (force
                || active(previous) != active(current)
                || now.duration_since(when) >= REFRESH_INTERVAL)
    })
}
