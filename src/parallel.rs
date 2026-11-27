use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

/// Run `worker` across `items` on a bounded thread pool (`<= max_workers`) while preserving order.
pub(crate) fn run_bounded<T: Sync, R: Send>(
    items: &[T],
    max_workers: usize,
    worker: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    if items.is_empty() {
        return Vec::new();
    }
    let worker_cnt = items.len().min(max_workers.max(1));
    let next_idx = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..worker_cnt {
            let tx = tx.clone();
            let next_idx = &next_idx;
            let worker = &worker;
            s.spawn(move || loop {
                let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                if idx >= items.len() {
                    break;
                }
                let _ = tx.send((idx, worker(&items[idx])));
            });
        }
    });
    drop(tx);
    let mut slots: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    for (idx, res) in rx {
        slots[idx] = Some(res);
    }
    slots
        .into_iter()
        .map(|s| s.expect("worker slot populated"))
        .collect()
}
