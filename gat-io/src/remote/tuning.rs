//! Shared transfer-tuning constants for size-aware upload/download of
//! remote objects.
//!
//! These are internal constants, not user-facing configuration; transfer
//! performance remains automatic and simple:
//! there is no `gat.yaml` field or CLI flag that changes any of them.
//! They exist so `gat_command::push` and the fetch/pull path share one
//! definition of "small vs. large object" and "how much concurrency to
//! ask opendal for" instead of duplicating magic numbers.
//!
//! The per-object concurrency rule is deliberately fixed rather than
//! computed from how many objects happen to be active: large objects request
//! [`PART_CONCURRENCY`] parts, falling back to sequential execution when the
//! complete retained-buffer envelope cannot fit. The operation-scoped physical
//! request budget in `gat_engine::remote_executor` separately caps aggregate
//! HTTP requests across all operators, while the logical object-job budget
//! caps how many transfer jobs may be active.

/// Size of the local `Read`/`Write` stream buffer used both for the
/// upload path (`gat_command::push`), cache ingest
/// (local file ingestion), and the shared stream-copy helper
/// (the engine transfer copy loops). Amortizes local
/// IO syscall overhead; unrelated to `NETWORK_CHUNK_SIZE`, which
/// governs how opendal chunks a *remote* multipart/range transfer.
pub const STREAM_BUFFER_SIZE: usize = 1024 * 1024;

thread_local! {
    // Worker-local, not shared: each blocking worker thread gets its own
    // scratch buffer, so sequential streaming jobs executed on the same
    // thread (e.g. tokio's blocking pool reusing a thread across
    // `spawn_blocking` calls) reuse one allocation instead of a fresh
    // `vec![0; STREAM_BUFFER_SIZE]` per job.
    static STREAM_BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `f` with a worker-local scratch buffer at least
/// `min(size_hint, STREAM_BUFFER_SIZE)` bytes long (and at least 1 byte,
/// so a zero-size object still gets a usable buffer to detect EOF with).
/// `size_hint` is the object's known size, or `None` when streaming from
/// a source of unknown length (in which case the full
/// [`STREAM_BUFFER_SIZE`] is used, matching prior behavior).
///
/// Shared by every local streaming copy/hash loop (upload copy, cache
/// ingest) instead of each allocating (and zeroing) its own
/// `vec![0; STREAM_BUFFER_SIZE]`. The buffer only ever grows to the
/// largest size a worker thread has needed so far -- it is never shrunk
/// back down for a smaller object, since keeping the larger allocation
/// around is strictly cheaper than reallocating later.
pub fn with_stream_buffer<T>(size_hint: Option<u64>, f: impl FnOnce(&mut [u8]) -> T) -> T {
    let wanted = size_hint
        .map_or(STREAM_BUFFER_SIZE, |size| {
            usize::try_from(size)
                .unwrap_or(usize::MAX)
                .min(STREAM_BUFFER_SIZE)
        })
        .max(1);
    STREAM_BUFFER.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < wanted {
            buf.resize(wanted, 0);
        }
        f(&mut buf[..wanted])
    })
}

/// Multipart upload target, clamped to the backend's effective minimum and
/// maximum. Its effective value also determines the payload reservation.
pub const NETWORK_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Uploads at or above this size use opendal's multipart-concurrent
/// path (`NETWORK_CHUNK_SIZE` chunks, [`PART_CONCURRENCY`] parts);
/// objects below it use a single sequential stream with concurrency 1.
/// `file://` remotes are exempt from this distinction entirely (see
/// the remote capability contract, which does not require
/// `write_can_multi` for `file://`).
pub const LARGE_OBJECT_THRESHOLD: u64 = 64 * 1024 * 1024;

/// The fixed intra-object part concurrency every large object
/// upload requests, subject to the operation's payload-buffer reservation.
/// The aggregate physical HTTP request ceiling is owned separately by the
/// operation-scoped remote request budget.
pub const PART_CONCURRENCY: usize = 4;

/// Clamps a requested chunk size to a backend's effective
/// `write_multi_min_size`/`write_multi_max_size`, so gat never asks a
/// backend for a multipart chunk size its capability doesn't accept.
fn clamp_chunk(requested: usize, min: Option<usize>, max: Option<usize>) -> usize {
    let mut chunk = requested;
    if let Some(min) = min {
        chunk = chunk.max(min);
    }
    if let Some(max) = max {
        chunk = chunk.min(max);
    }
    chunk
}

/// Decides the [`opendal::options::WriteOptions`] gat's push path should
/// use for uploading an object of `size` bytes, given the destination
/// operator's effective capability.
///
/// Only network uploads use this policy. Objects below
/// [`LARGE_OBJECT_THRESHOLD`] also get the sequential default
/// (`concurrent: 1`), matching gat's existing object-level parallelism
/// (many small objects, one stream each) rather than paying multipart
/// overhead on tiny uploads. Objects at/above the threshold request
/// `NETWORK_CHUNK_SIZE` (clamped to the operator's
/// `write_multi_min_size`/`write_multi_max_size`) with
/// [`PART_CONCURRENCY`] parts -- falling back to sequential if the
/// operator doesn't support multipart writes at all, rather than
/// attempting unsupported behavior.
pub fn upload_write_options(size: u64, cap: opendal::Capability) -> opendal::options::WriteOptions {
    let mut opts = opendal::options::WriteOptions::default();
    if size < LARGE_OBJECT_THRESHOLD || !cap.write_can_multi {
        opts.concurrent = 1;
        return opts;
    }
    let chunk = clamp_chunk(
        NETWORK_CHUNK_SIZE,
        cap.write_multi_min_size,
        cap.write_multi_max_size,
    );
    opts.chunk = Some(chunk);
    opts.concurrent = PART_CONCURRENCY;
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_buffer_is_sized_to_the_hint_when_smaller_than_the_cap() {
        with_stream_buffer(Some(4096), |buf| {
            assert_eq!(buf.len(), 4096);
        });
    }

    #[test]
    fn stream_buffer_caps_at_stream_buffer_size_for_a_larger_hint() {
        with_stream_buffer(Some(u64::MAX), |buf| {
            assert_eq!(buf.len(), STREAM_BUFFER_SIZE);
        });
    }

    #[test]
    fn stream_buffer_uses_the_full_cap_with_no_hint() {
        with_stream_buffer(None, |buf| {
            assert_eq!(buf.len(), STREAM_BUFFER_SIZE);
        });
    }

    #[test]
    fn stream_buffer_never_shrinks_below_a_prior_larger_call_on_the_same_thread() {
        with_stream_buffer(Some(STREAM_BUFFER_SIZE as u64), |buf| {
            assert_eq!(buf.len(), STREAM_BUFFER_SIZE);
        });
        // Reusing the worker-local buffer for a small object must not
        // reallocate: the returned slice is still only as long as this
        // call asked for, but the underlying allocation stays put.
        with_stream_buffer(Some(64), |buf| {
            assert_eq!(buf.len(), 64);
        });
        let retained_capacity = STREAM_BUFFER.with(|cell| cell.borrow().capacity());
        assert!(retained_capacity >= STREAM_BUFFER_SIZE);
    }

    #[test]
    fn stream_buffer_gives_at_least_one_byte_for_a_zero_size_hint() {
        with_stream_buffer(Some(0), |buf| {
            assert_eq!(buf.len(), 1);
        });
    }

    #[test]
    fn stream_buffer_reuses_capacity_across_repeated_calls_without_exceeding_the_cap() {
        // Repeated calls with a variety of hints (including ones above the
        // cap and unknown-size `None`) must never grow the underlying
        // allocation past `STREAM_BUFFER_SIZE`, and once the buffer has
        // reached the cap it must be reused verbatim (no further
        // reallocation) for every later call on this thread.
        let hints = [
            Some(1u64),
            Some(4096),
            None,
            Some(STREAM_BUFFER_SIZE as u64 * 8),
            Some(64),
            None,
            Some(STREAM_BUFFER_SIZE as u64),
        ];
        for hint in hints {
            with_stream_buffer(hint, |buf| {
                assert!(buf.len() <= STREAM_BUFFER_SIZE);
            });
            let capacity = STREAM_BUFFER.with(|cell| cell.borrow().capacity());
            assert!(
                capacity <= STREAM_BUFFER_SIZE,
                "retained capacity {capacity} exceeded STREAM_BUFFER_SIZE"
            );
        }
        let final_capacity = STREAM_BUFFER.with(|cell| cell.borrow().capacity());
        assert_eq!(
            final_capacity, STREAM_BUFFER_SIZE,
            "the largest hint seen (a full-cap request) must have grown the buffer to \
             exactly STREAM_BUFFER_SIZE and no later smaller call may shrink it back down"
        );
    }

    fn cap_with_multi(min: Option<usize>, max: Option<usize>) -> opendal::Capability {
        opendal::Capability {
            write: true,
            write_can_multi: true,
            write_multi_min_size: min,
            write_multi_max_size: max,
            ..Default::default()
        }
    }

    #[test]
    fn upload_options_use_concurrency_one_below_the_large_object_threshold() {
        let cap = cap_with_multi(None, None);
        let opts = upload_write_options(LARGE_OBJECT_THRESHOLD - 1, cap);
        assert_eq!(opts.concurrent, 1);
        assert_eq!(opts.chunk, None);
    }

    #[test]
    fn upload_options_use_multipart_at_the_large_object_threshold() {
        let cap = cap_with_multi(None, None);
        let opts = upload_write_options(LARGE_OBJECT_THRESHOLD, cap);
        assert_eq!(opts.chunk, Some(NETWORK_CHUNK_SIZE));
        assert_eq!(opts.concurrent, PART_CONCURRENCY);
    }

    #[test]
    fn upload_options_fall_back_to_sequential_without_multipart_support() {
        let cap = opendal::Capability {
            write: true,
            write_can_multi: false,
            ..Default::default()
        };
        let opts = upload_write_options(LARGE_OBJECT_THRESHOLD * 2, cap);
        assert_eq!(
            opts.concurrent, 1,
            "must not attempt multipart writes the operator doesn't support"
        );
    }

    #[test]
    fn upload_options_clamp_chunk_to_operator_capability() {
        // Operator's own max multipart chunk is smaller than gat's
        // requested NETWORK_CHUNK_SIZE.
        let small_max = NETWORK_CHUNK_SIZE / 2;
        let cap = cap_with_multi(None, Some(small_max));
        let opts = upload_write_options(LARGE_OBJECT_THRESHOLD, cap);
        assert_eq!(opts.chunk, Some(small_max));

        // Operator requires a minimum multipart chunk larger than gat's
        // requested NETWORK_CHUNK_SIZE.
        let large_min = NETWORK_CHUNK_SIZE * 2;
        let cap = cap_with_multi(Some(large_min), None);
        let opts = upload_write_options(LARGE_OBJECT_THRESHOLD, cap);
        assert_eq!(opts.chunk, Some(large_min));
    }
}
