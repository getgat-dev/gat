const VALIDATION_WINDOW: usize = 4096;

pub(crate) fn map_ordered<T, R, E, F>(items: &[T], f: F) -> Result<Vec<R>, E>
where
    T: Sync,
    R: Send,
    E: Send,
    F: Fn(&T) -> Result<R, E> + Sync + Send,
{
    map_ordered_observed(items, VALIDATION_WINDOW, f, |_, _| {})
}

fn map_ordered_observed<T, R, E, F, O>(
    items: &[T],
    window_size: usize,
    f: F,
    observe: O,
) -> Result<Vec<R>, E>
where
    T: Sync,
    R: Send,
    E: Send,
    F: Fn(&T) -> Result<R, E> + Sync + Send,
    O: Fn(usize, &Result<R, E>) + Sync + Send,
{
    use rayon::prelude::*;

    let mut output = Vec::with_capacity(items.len());
    let mut results = Vec::new();
    for (window_index, window) in items.chunks(window_size).enumerate() {
        window
            .par_iter()
            .enumerate()
            .map(|(index, item)| {
                let result = f(item);
                observe(window_index * window_size + index, &result);
                result
            })
            .collect_into_vec(&mut results);
        // Retain the allocation for the next window.
        #[allow(clippy::iter_with_drain)]
        for result in results.drain(..) {
            output.push(result?);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_input_order_across_windows_under_a_multi_threaded_pool() {
        let items = (0..64).collect::<Vec<_>>();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();

        let output = pool
            .install(|| map_ordered_observed(&items, 7, |item| Ok::<_, &str>(item * 2), |_, _| {}))
            .unwrap();

        assert_eq!(
            output,
            items.iter().map(|item| item * 2).collect::<Vec<_>>()
        );
    }

    #[test]
    fn selects_input_order_error_after_forced_reversed_completion() {
        let items = (0..8).collect::<Vec<_>>();
        let (send, receive) = std::sync::mpsc::channel();
        let receive = std::sync::Mutex::new(receive);
        let map = move |item: &i32| -> Result<i32, String> {
            match *item {
                2 => {
                    receive.lock().unwrap().recv().unwrap();
                    Err(format!("failed at {item}"))
                }
                5 => Err(format!("failed at {item}")),
                value => Ok(value),
            }
        };
        let observe = move |index: usize, _: &Result<i32, String>| {
            if index == 5 {
                send.send(()).unwrap();
            }
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();

        let error = pool
            .install(|| map_ordered_observed(&items, VALIDATION_WINDOW, map, observe))
            .unwrap_err();

        assert_eq!(error, "failed at 2");
    }

    #[test]
    fn stops_after_the_first_failing_window_and_preserves_error_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for threads in [1, 4] {
            let visited = AtomicUsize::new(0);
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let error = pool
                .install(|| {
                    map_ordered_observed(
                        &(0..13).collect::<Vec<_>>(),
                        4,
                        |value| {
                            if *value == 5 || *value == 6 {
                                Err(*value)
                            } else {
                                Ok(())
                            }
                        },
                        |_, _| {
                            visited.fetch_add(1, Ordering::Relaxed);
                        },
                    )
                })
                .unwrap_err();
            assert_eq!(error, 5);
            assert_eq!(visited.load(Ordering::Relaxed), 8);
        }
    }

    #[test]
    fn selects_the_same_error_in_a_single_threaded_pool() {
        let items = (0..8).collect::<Vec<_>>();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();

        let error = pool
            .install(|| {
                map_ordered(&items, |item| match *item {
                    2 | 5 => Err(format!("failed at {item}")),
                    value => Ok(value),
                })
            })
            .unwrap_err();

        assert_eq!(error, "failed at 2");
    }
}
