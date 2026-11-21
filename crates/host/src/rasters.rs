//! Renders drawn outside the core lock, at most [`RASTER_PERMITS`] pages' worth at once host-wide (68i).
//!
//! A render that is not cached (`render.get`, `render.pixels`) draws without the core lock (`designllm_core::RenderJob`). Each
//! may hold a page raster of up to the renderer's limit and its encoding, which the core lock used to take one at a time. The
//! permits keep that bounded for every role (gui, bridge, cli, job-bound), without making a render wait for the core lock: a
//! connection's render waits here only for other renders. A wait ends early when its connection closes or the host stops; a
//! job's cancel does not end it (the job's processes and PDF reads stop; a render is short). A render that draws nothing takes
//! no permit (`conn::draw`). A render of more than [`HEAVY_PIXELS`] takes every permit, so two of them never draw at once. A
//! waiting heavy render is not passed by newer small ones: they wait behind it, so it cannot starve.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::shared::lock;

/// Permits host-wide: the most pixels drawn at once, in renders of at most [`HEAVY_PIXELS`] each.
pub(crate) const RASTER_PERMITS: usize = 2;
/// A render drawing more pixels than this takes every permit (two such rasters are up to 2 x 1.6 GB).
pub(crate) const HEAVY_PIXELS: u64 = 100_000_000;
/// How often a waiting render checks whether it should stop waiting.
const WAIT_POLL: Duration = Duration::from_millis(50);

/// The permits a render drawing `pixels` pixels takes.
pub(crate) fn permits_for(pixels: u64) -> usize {
    if pixels > HEAVY_PIXELS {
        RASTER_PERMITS
    } else {
        1
    }
}

#[derive(Default)]
struct Books {
    /// Permits taken.
    running: usize,
    /// Renders waiting for every permit (the limit). Smaller renders wait behind them.
    wide_waiting: usize,
}

pub(crate) struct RasterPermits {
    books: Mutex<Books>,
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
        Self { books: Mutex::new(Books::default()), freed: Condvar::new(), limit: limit.max(1) }
    }

    /// Waits for `weight` permits (at most the limit), or returns None as soon as `stop` is true (checked every [`WAIT_POLL`]).
    pub(crate) fn acquire(&self, weight: usize, stop: impl Fn() -> bool) -> Option<RasterPermit<'_>> {
        let weight = weight.clamp(1, self.limit);
        let wide = weight == self.limit;
        let mut books = lock(&self.books);
        let mut waiting = false;
        loop {
            if stop() {
                if waiting {
                    books.wide_waiting -= 1;
                    self.freed.notify_all();
                }
                return None;
            }
            // A narrow render does not pass a wide one that waits for every permit.
            if wide || books.wide_waiting == 0 {
                if let Some(permit) = self.take(&mut books.running, weight) {
                    if waiting {
                        books.wide_waiting -= 1;
                    }
                    return Some(permit);
                }
            }
            if wide && !waiting {
                books.wide_waiting += 1;
                waiting = true;
            }
            books = self.freed.wait_timeout(books, WAIT_POLL).map(|(guard, _)| guard).unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    fn take(&self, running: &mut usize, weight: usize) -> Option<RasterPermit<'_>> {
        if *running + weight > self.limit {
            return None;
        }
        *running += weight;
        Some(RasterPermit { permits: self, weight })
    }

    /// A permit now, or None: for tests that hold the permits by hand.
    #[cfg(test)]
    pub(crate) fn try_acquire(&self, weight: usize) -> Option<RasterPermit<'_>> {
        let mut books = lock(&self.books);
        self.take(&mut books.running, weight.clamp(1, self.limit))
    }

    #[cfg(test)]
    fn running(&self) -> usize {
        lock(&self.books).running
    }
}

/// One render's permits, given back when dropped (also by a panic of the render).
pub(crate) struct RasterPermit<'a> {
    permits: &'a RasterPermits,
    weight: usize,
}

impl Drop for RasterPermit<'_> {
    fn drop(&mut self) {
        lock(&self.permits.books).running -= self.weight;
        // Waiters need different counts, so any of them may fit now: all are woken.
        self.permits.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier};

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
                    let _permit = permits.acquire(1, || false).expect("a permit");
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
        let held = permits.acquire(1, || false).expect("the only permit");
        let closed = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (permits, closed) = (Arc::clone(&permits), Arc::clone(&closed));
            std::thread::spawn(move || permits.acquire(1, || closed.load(Ordering::SeqCst)).is_some())
        };
        std::thread::sleep(Duration::from_millis(80));
        closed.store(true, Ordering::SeqCst);
        assert!(!waiter.join().expect("the waiter ends"), "it stops waiting without a permit");
        assert!(permits.acquire(1, || true).is_none(), "a stopped wait takes no permit");
        drop(held);

        let panicking = {
            let permits = Arc::clone(&permits);
            std::thread::spawn(move || {
                let _permit = permits.acquire(1, || false).expect("a permit");
                panic!("the renderer panicked");
            })
        };
        assert!(panicking.join().is_err());
        assert_eq!(permits.running(), 0, "the panicking render gave its permit back");
    }

    /// A `stop` closure that reports its second call: the render has been refused once and has waited a poll period.
    fn reports_its_second_check(tx: mpsc::Sender<()>) -> impl Fn() -> bool {
        let checks = std::cell::Cell::new(0_usize);
        move || {
            let n = checks.get() + 1;
            checks.set(n);
            if n == 2 {
                let _ = tx.send(());
            }
            false
        }
    }

    #[test]
    fn a_render_of_more_than_the_heavy_pixels_takes_every_permit_and_small_ones_still_draw_two_at_once() {
        assert_eq!(permits_for(HEAVY_PIXELS), 1, "exactly the limit is still a small render");
        assert_eq!(permits_for(HEAVY_PIXELS + 1), RASTER_PERMITS);
        let permits = RasterPermits::default();
        let heavy = permits.try_acquire(permits_for(HEAVY_PIXELS + 1)).expect("a heavy render draws on an idle host");
        assert!(permits.try_acquire(permits_for(1)).is_none(), "nothing draws beside a heavy render");
        drop(heavy);
        let small = [permits.try_acquire(permits_for(1)), permits.try_acquire(permits_for(1))];
        assert!(small.iter().all(Option::is_some), "two small renders draw at once, as before");
        assert!(permits.try_acquire(permits_for(1)).is_none(), "a third small render waits");
    }

    #[test]
    fn a_waiting_heavy_render_draws_once_the_small_renders_have_ended() {
        let permits = Arc::new(RasterPermits::default());
        let small_a = permits.try_acquire(permits_for(1)).expect("a small render");
        let small_b = permits.try_acquire(permits_for(1)).expect("a small render");
        let (waiting, is_waiting) = mpsc::channel();
        let (drawn, heavy_drew) = mpsc::channel();
        let heavy = {
            let permits = Arc::clone(&permits);
            std::thread::spawn(move || {
                let _permit = permits.acquire(permits_for(HEAVY_PIXELS + 1), reports_its_second_check(waiting)).expect("a permit");
                drawn.send(()).expect("the test listens");
            })
        };
        // The heavy render has been refused and is waiting; one small render still draws, so it must not have started.
        is_waiting.recv_timeout(Duration::from_secs(10)).expect("the heavy render waits");
        drop(small_a);
        assert!(heavy_drew.try_recv().is_err(), "a heavy render waits for every permit");
        drop(small_b);
        heavy_drew.recv_timeout(Duration::from_secs(10)).expect("the heavy render draws once all permits are free");
        heavy.join().expect("the heavy render ends");
        assert_eq!(permits.running(), 0, "every permit is given back");
    }

    #[test]
    fn a_small_render_does_not_pass_a_heavy_one_that_waits_for_the_permits() {
        let permits = Arc::new(RasterPermits::default());
        let small_a = permits.try_acquire(1).expect("a small render");
        let (waiting, heavy_waits) = mpsc::channel();
        let (heavy_took, heavy_draws) = mpsc::channel();
        let (heavy_release, release) = mpsc::channel::<()>();
        let heavy = {
            let permits = Arc::clone(&permits);
            std::thread::spawn(move || {
                let _permit = permits.acquire(permits_for(HEAVY_PIXELS + 1), reports_its_second_check(waiting)).expect("a permit");
                heavy_took.send(()).expect("the test listens");
                release.recv().expect("the test releases the heavy render");
            })
        };
        heavy_waits.recv_timeout(Duration::from_secs(10)).expect("the heavy render waits");
        // A small render now queues: one permit is free, but the heavy render is waiting for every permit.
        let (small_took, small_draws) = mpsc::channel();
        let (small_waiting, small_is_waiting) = mpsc::channel();
        let small = {
            let permits = Arc::clone(&permits);
            std::thread::spawn(move || {
                let _permit = permits.acquire(1, reports_its_second_check(small_waiting)).expect("a permit");
                small_took.send(()).expect("the test listens");
            })
        };
        small_is_waiting.recv_timeout(Duration::from_secs(10)).expect("the small render waits behind the heavy one");
        drop(small_a);
        heavy_draws.recv_timeout(Duration::from_secs(10)).expect("the heavy render takes every permit first");
        assert!(small_draws.try_recv().is_err(), "the small render does not pass the heavy one");
        heavy_release.send(()).expect("the heavy render listens");
        heavy.join().expect("the heavy render ends");
        small_draws.recv_timeout(Duration::from_secs(10)).expect("the small render draws after the heavy one");
        small.join().expect("the small render ends");
        assert_eq!(permits.running(), 0, "every permit is given back");
    }
}
