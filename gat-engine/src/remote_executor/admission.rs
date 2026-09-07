//! Atomic object-slot and payload reservations. No partial leases are retained.

use crate::limits::RemoteConcurrency;
use crate::remote_catalog::RemoteId;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub(super) const GLOBAL_BYTES: usize = 256 * 1024 * 1024;
pub(super) const REMOTE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Default)]
struct Usage {
    jobs: usize,
    bytes: usize,
}

struct State {
    total: Usage,
    remotes: HashMap<RemoteId, Usage>,
    waiting: VecDeque<(RemoteId, usize)>,
}

pub(super) struct Admission {
    limits: RemoteConcurrency,
    state: Mutex<State>,
    changed: Notify,
}

pub(crate) struct TransferLease {
    admission: Arc<Admission>,
    remote: RemoteId,
    bytes: usize,
}

impl Drop for TransferLease {
    fn drop(&mut self) {
        let mut state = self.admission.state.lock().unwrap();
        state.total.jobs -= 1;
        state.total.bytes -= self.bytes;
        let remote = state.remotes.get_mut(&self.remote).unwrap();
        remote.jobs -= 1;
        remote.bytes -= self.bytes;
        drop(state);
        self.admission.changed.notify_waiters();
    }
}

impl Admission {
    pub(super) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.changed.notified()
    }

    pub(super) fn forget_waiter(&self, remote: RemoteId) {
        self.state
            .lock()
            .unwrap()
            .waiting
            .retain(|(id, _)| *id != remote);
        self.changed.notify_waiters();
    }

    pub(super) fn try_acquire(
        self: &Arc<Self>,
        remote: RemoteId,
        bytes: usize,
    ) -> Option<TransferLease> {
        assert!(
            bytes <= REMOTE_BYTES,
            "prepared transfer must fit per-remote capacity"
        );
        let mut state = self.state.lock().unwrap();
        if let Some((_, cost)) = state.waiting.iter_mut().find(|(id, _)| *id == remote) {
            // An error frontier can replace this remote's pending queue head.
            *cost = bytes;
        } else {
            state.waiting.push_back((remote, bytes));
        }
        // Only a remote with local room can reserve the next global opportunity.
        // Keeping its place while global capacity drains prevents small jobs
        // from continually overtaking a large reservation.
        let eligible = state
            .waiting
            .iter()
            .find(|(id, cost)| {
                state.remotes.get(id).is_none_or(|usage| {
                    usage.jobs < self.limits.per_remote.get() && usage.bytes + cost <= REMOTE_BYTES
                })
            })
            .copied();
        if eligible != Some((remote, bytes))
            || state.total.jobs >= self.limits.global.get()
            || state.total.bytes + bytes > GLOBAL_BYTES
        {
            return None;
        }
        state.waiting.retain(|(id, _)| *id != remote);
        state.total.jobs += 1;
        state.total.bytes += bytes;
        let usage = state.remotes.entry(remote).or_default();
        usage.jobs += 1;
        usage.bytes += bytes;
        drop(state);
        self.changed.notify_waiters();
        Some(TransferLease {
            admission: Arc::clone(self),
            remote,
            bytes,
        })
    }

    pub(super) fn new(limits: RemoteConcurrency) -> Arc<Self> {
        Arc::new(Self {
            limits,
            state: Mutex::new(State {
                total: Usage::default(),
                remotes: HashMap::new(),
                waiting: VecDeque::new(),
            }),
            changed: Notify::new(),
        })
    }

    #[cfg(test)]
    pub(super) async fn acquire(self: &Arc<Self>, remote: RemoteId, bytes: usize) -> TransferLease {
        struct Waiting {
            admission: Arc<Admission>,
            remote: RemoteId,
        }
        impl Drop for Waiting {
            fn drop(&mut self) {
                self.admission.forget_waiter(self.remote);
            }
        }
        let _waiting = Waiting {
            admission: Arc::clone(self),
            remote,
        };
        loop {
            let changed = self.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(lease) = self.try_acquire(remote, bytes) {
                return lease;
            }
            changed.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    #[test]
    fn reservations_bound_bytes_and_preserve_large_job_opportunity() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_dir, handles) = crate::remote_session::test_support::open_handles(&["a", "b", "c"]);
        let [a, b, c] = [handles[0].id(), handles[1].id(), handles[2].id()];
        let admission = Admission::new(RemoteConcurrency {
            global: NonZeroUsize::new(256).unwrap(),
            per_remote: NonZeroUsize::new(128).unwrap(),
        });
        let first = admission.try_acquire(a, REMOTE_BYTES).unwrap();
        let second = admission.try_acquire(b, REMOTE_BYTES).unwrap();
        assert!(admission.try_acquire(c, REMOTE_BYTES).is_none());
        drop(first);
        assert!(
            admission.try_acquire(a, 1).is_none(),
            "a newcomer cannot overtake the queued large reservation"
        );
        let third = admission.try_acquire(c, REMOTE_BYTES).unwrap();
        drop(second);
        drop(third);
        let tiny = admission.try_acquire(a, 1).unwrap();
        assert_eq!(admission.state.lock().unwrap().total.bytes, 1);
        drop(tiny);
        assert_eq!(admission.state.lock().unwrap().total.bytes, 0);
    }

    #[test]
    fn saturated_remote_never_reserves_global_capacity() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let (_dir, handles) = crate::remote_session::test_support::open_handles(&["a", "b"]);
        let admission = Admission::new(RemoteConcurrency {
            global: NonZeroUsize::new(2).unwrap(),
            per_remote: NonZeroUsize::new(1).unwrap(),
        });
        let _a = admission.try_acquire(handles[0].id(), 1).unwrap();
        assert!(admission.try_acquire(handles[0].id(), 1).is_none());
        assert!(admission.try_acquire(handles[1].id(), 1).is_some());
    }
}
