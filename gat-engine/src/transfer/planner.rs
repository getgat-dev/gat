//! Bounded streaming-window planning shared by remote transfers.

use std::collections::HashSet;
use std::hash::Hash;
use std::num::NonZeroUsize;

/// Access to one transfer window without control over its allocation or size.
/// Borrow items in place or drain them while the planner retains the buffer.
///
/// ```compile_fail
/// use gat_engine::WindowBatch;
/// fn grow(mut batch: WindowBatch<'_, u8>) {
///     batch.push(1);
/// }
/// ```
pub struct WindowBatch<'a, V> {
    items: &'a mut Vec<V>,
}

impl<'a, V> WindowBatch<'a, V> {
    pub const fn as_mut_slice(&mut self) -> &mut [V] {
        self.items.as_mut_slice()
    }

    /// Consume the items without taking ownership of the backing allocation.
    #[must_use]
    pub fn drain(self) -> std::vec::Drain<'a, V> {
        self.items.drain(..)
    }
}

/// Deduplicates a streamed operation globally while retaining at most one
/// bounded window of full per-item metadata.
///
/// The item buffer is preallocated to the window capacity and reused via
/// [`Vec::clear`]. Global deduplication retains every distinct key: the
/// membership set uses O(unique keys) memory and may grow and reallocate.
#[doc(hidden)]
pub struct StreamingWindow<K, V> {
    seen: HashSet<K>,
    window: Vec<V>,
    window_size: NonZeroUsize,
}

impl<K: Eq + Hash, V> StreamingWindow<K, V> {
    #[must_use]
    pub fn new(window_size: NonZeroUsize) -> Self {
        Self {
            seen: HashSet::with_capacity(window_size.get()),
            window: Vec::with_capacity(window_size.get()),
            window_size,
        }
    }

    /// Records the first value for `key`, preserving insertion order in the
    /// window buffer. When the window fills, `on_full` is invoked with the
    /// buffered items and the buffer is cleared in place afterward,
    /// retaining its allocation for the next window.
    pub fn record<E>(
        &mut self,
        key: K,
        make: impl FnOnce() -> V,
        on_full: impl FnOnce(WindowBatch<'_, V>) -> Result<(), E>,
    ) -> Result<(), E> {
        if !self.seen.insert(key) {
            return Ok(());
        }
        self.window.push(make());
        if self.window.len() >= self.window_size.get() {
            on_full(WindowBatch {
                items: &mut self.window,
            })?;
            self.window.clear();
        }
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn unique_count(&self) -> usize {
        self.seen.len()
    }

    /// Drains any items left in the window buffer once streaming ends.
    pub fn finish<E>(
        mut self,
        on_remaining: impl FnOnce(WindowBatch<'_, V>) -> Result<(), E>,
    ) -> Result<(), E> {
        if !self.window.is_empty() {
            on_remaining(WindowBatch {
                items: &mut self.window,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yields_a_window_as_soon_as_it_fills() {
        let mut window: StreamingWindow<usize, usize> =
            StreamingWindow::new(NonZeroUsize::new(2).unwrap());
        let mut windows: Vec<Vec<usize>> = Vec::new();
        window.record(1, || 1, |_| Ok::<_, ()>(())).unwrap();
        window
            .record(
                2,
                || 2,
                |batch| {
                    windows.push(batch.drain().collect());
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
        window.record(3, || 3, |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(windows, vec![vec![1, 2]]);
        assert_eq!(window.unique_count(), 3);
        let mut last = Vec::new();
        window
            .finish(|batch| {
                last = batch.drain().collect();
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(last, vec![3]);
    }

    #[test]
    fn empty_stream_finishes_empty() {
        let window = StreamingWindow::<usize, usize>::new(NonZeroUsize::new(4).unwrap());
        let mut called = false;
        window
            .finish(|_| {
                called = true;
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(!called);
    }

    #[test]
    fn exact_window_finishes_empty() {
        let mut window = StreamingWindow::new(NonZeroUsize::new(1).unwrap());
        let mut windows: Vec<Vec<usize>> = Vec::new();
        window
            .record(
                1,
                || 1,
                |batch| {
                    windows.push(batch.drain().collect());
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
        assert_eq!(windows, vec![vec![1]]);
        let mut called = false;
        window
            .finish(|_| {
                called = true;
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(!called);
    }

    #[test]
    fn duplicate_keys_do_not_build_values_again() {
        let mut window = StreamingWindow::new(NonZeroUsize::new(10).unwrap());
        let mut calls = 0;
        window
            .record(
                "a",
                || {
                    calls += 1;
                    1
                },
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        window
            .record(
                "a",
                || {
                    calls += 1;
                    2
                },
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(window.unique_count(), 1);
    }

    #[test]
    fn window_buffer_retains_capacity_across_windows() {
        // Reused, not replaced: the buffer's allocation stays put across
        // every window instead of being handed off and dropped.
        let mut window: StreamingWindow<usize, usize> =
            StreamingWindow::new(NonZeroUsize::new(4).unwrap());
        let capacity = window.window.capacity();
        let allocation = window.window.as_ptr();
        for key in 0..12usize {
            window
                .record(
                    key,
                    || key,
                    |batch| {
                        assert_eq!(batch.drain().count(), 4);
                        Ok::<_, ()>(())
                    },
                )
                .unwrap();
            assert_eq!(window.window.capacity(), capacity);
            assert_eq!(window.window.as_ptr(), allocation);
        }
    }
}
