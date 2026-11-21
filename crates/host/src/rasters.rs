//! Renders drawn outside the core lock, at most [`RASTER_PERMITS`] at once host-wide (68i).
//!
//! A render that is not cached (`render.get`, `render.pixels`) draws without the core lock (`designllm_core::RenderJob`). Each
//! may hold a page raster of up to the renderer's limit and its encoding, which the core lock used to take one at a time. The
//! permits keep that bounded for every role (gui, bridge, cli, job-bound), without making a render wait for the core lock: a
//! connection's render waits here only for other renders. A wait ends early when its connection closes or the host stops.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::shared::lock;

/// Renders drawn at once, host-wide.
pub(crate) const RASTER_PERMITS: usize = 2;
/// How often a waiting render checks whether it should stop waiting.
const WAIT_POLL: Duration = Duration::from_millis(50);

pub(crate) struct RasterPermits {
    running: Mutex<usize>,
    freed: Condvar,
    limit: usize,
}

impl Default for RasterPermits {
    fn default() -> Self {
        Self::with_limit(RASTER_PERMITS)
    }
}

impl RasterPermits {
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self { running: Mutex::new(0), freed: Condvar::new(), limit: limit.max(1) }
    }

    /// Waits for a permit, or returns None as soon as `stop` is true (checked every [`WAIT_POLL`]).
    pub(crate) fn acquire(&self, stop: impl Fn() -> bool) -> Option<RasterPermit<'_>> {
        let mut running = lock(&self.running);
        loop {
            if stop() {
                return None;
            }
            if *running < self.limit {
                *running += 1;
                return Some(RasterPermit(self));
            }
            running = self.freed.wait_timeout(running, WAIT_POLL).map(|(guard, _)| guard).unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    #[cfg(test)]
    fn running(&self) -> usize {
        *lock(&self.running)
    }
}

/// One render's permit, given back when dropped (also by a panic of the render).
pub(crate) struct RasterPermit<'a>(&'a RasterPermits);

impl Drop for RasterPermit<'_> {
    fn drop(&mut self) {
        *lock(&self.0.running) -= 1;
        self.0.freed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    #[test]
    fn no_more_than_the_limit_draw_at_once_and_every_waiting_render_gets_its_turn() {
        let permits = Arc::new(RasterPermits::with_limit(2));
        let drawing = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(6));
        let renders: Vec<_> = (0..6)
            .map(|_| {
                let (permits, drawing, most, start) = (Arc::clone(&permits), Arc::clone(&drawing), Arc::clone(&most), Arc::clone(&start));
                std::thread::spawn(move || {
                    start.wait();
                    let _permit = permits.acquire(|| false).expect("a permit");
                    let now = drawing.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(30));
                    drawing.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for render in renders {
            render.join().expect("the render ends");
        }
        assert_eq!(most.load(Ordering::SeqCst), 2, "two renders at most");
        assert_eq!(permits.running(), 0, "every permit is given back");
    }

    #[test]
    fn a_waiting_render_stops_when_its_connection_closes_and_a_panic_gives_its_permit_back() {
        let permits = Arc::new(RasterPermits::with_limit(1));
        let held = permits.acquire(|| false).expect("the only permit");
        let closed = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (permits, closed) = (Arc::clone(&permits), Arc::clone(&closed));
            std::thread::spawn(move || permits.acquire(|| closed.load(Ordering::SeqCst)).is_some())
        };
        std::thread::sleep(Duration::from_millis(80));
        closed.store(true, Ordering::SeqCst);
        assert!(!waiter.join().expect("the waiter ends"), "it stops waiting without a permit");
        assert!(permits.acquire(|| true).is_none(), "a stopped wait takes no permit");
        drop(held);

        let panicking = {
            let permits = Arc::clone(&permits);
            std::thread::spawn(move || {
                let _permit = permits.acquire(|| false).expect("a permit");
                panic!("the renderer panicked");
            })
        };
        assert!(panicking.join().is_err());
        assert_eq!(permits.running(), 0, "the panicking render gave its permit back");
    }
}
