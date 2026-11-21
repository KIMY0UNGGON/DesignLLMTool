//! Renders drawn outside the core lock, at most [`RASTER_PERMITS`] pages' worth at once host-wide (68i).
//!
//! A render that is not cached (`render.get`, `render.pixels`) draws without the core lock (`designllm_core::RenderJob`). Each
//! may hold a page raster of up to the renderer's limit and its encoding, which the core lock used to take one at a time. The
//! permits keep that bounded for every role (gui, bridge, cli, job-bound), without making a render wait for the core lock: a
//! connection's render waits here only for other renders. A wait ends early when its connection closes or the host stops; a
//! job's cancel does not end it (the job's processes and PDF reads stop; a render is short). A render that draws nothing takes
//! no permit (`conn::draw`). A render of more than [`HEAVY_PIXELS`] takes every permit, so two of them never draw at once.
//! Renders draw in arrival order: a render waits behind every earlier one, so a heavy render is not passed by small ones and a
//! small one is not held back by heavy ones that come later.

use std::collections::VecDeque;
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
    /// The arrival number of the next render that waits.
    next_ticket: u64,
    /// Renders waiting for permits, oldest first. Only the first one in the queue draws next; none is passed over.
    queue: VecDeque<u64>,
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
    /// Renders take permits in arrival order: a render that fits draws only when every earlier waiter has drawn.
    pub(crate) fn acquire(&self, weight: usize, stop: impl Fn() -> bool) -> Option<RasterPermit<'_>> {
        let weight = weight.clamp(1, self.limit);
        let ticket = {
            let mut books = lock(&self.books);
            let ticket = books.next_ticket;
            books.next_ticket += 1;
            books.queue.push_back(ticket);
            ticket
        };
        // Declared before the guard below, so that its drop (which takes the lock) runs after the guard's.
        let _queued = Queued { permits: self, ticket };
        let mut books = lock(&self.books);
        loop {
            if stop() {
                return None;
            }
            if books.queue.front() == Some(&ticket) && books.running + weight <= self.limit {
                books.queue.pop_front();
                books.running += weight;
                return Some(RasterPermit { permits: self, weight });
            }
            books = self.freed.wait_timeout(books, WAIT_POLL).map(|(guard, _)| guard).unwrap_or_else(|poisoned| poisoned.into_inner().0);
        }
    }

    /// A permit now, unless a render is waiting first, or None: for tests that hold the permits by hand.
    #[cfg(test)]
    pub(crate) fn try_acquire(&self, weight: usize) -> Option<RasterPermit<'_>> {
        let weight = weight.clamp(1, self.limit);
        let mut books = lock(&self.books);
        if !books.queue.is_empty() || books.running + weight > self.limit {
            return None;
        }
        books.running += weight;
        Some(RasterPermit { permits: self, weight })
    }

    #[cfg(test)]
    fn running(&self) -> usize {
        lock(&self.books).running
    }

    #[cfg(test)]
    fn queued(&self) -> usize {
        lock(&self.books).queue.len()
    }
}

/// A render's place in the queue, taken back when its wait ends by any way, a panic of `stop` included. The renders behind it are
/// woken then, so none waits for a render that has gone.
struct Queued<'a> {
    permits: &'a RasterPermits,
    ticket: u64,
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        lock(&self.permits.books).queue.retain(|ticket| *ticket != self.ticket);
        self.permits.freed.notify_all();
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

    #[test]
    fn a_small_render_draws_before_the_heavy_renders_that_queue_after_it() {
        let permits = Arc::new(RasterPermits::default());
        let held = permits.try_acquire(1).expect("a small render");
        let order = Arc::new(Mutex::new(Vec::new()));
        // Each render is started once the one before it has queued, so the queue is in this order.
        let spawn = |name: &'static str, weight: usize| {
            let (queued, is_queued) = mpsc::channel();
            let (permits, order) = (Arc::clone(&permits), Arc::clone(&order));
            let render = std::thread::spawn(move || {
                let _permit = permits.acquire(weight, reports_its_second_check(queued)).expect("a permit");
                order.lock().expect("the order").push(name);
            });
            is_queued.recv_timeout(Duration::from_secs(10)).expect("the render waits in line");
            render
        };
        let heavy_first = spawn("heavy", permits_for(HEAVY_PIXELS + 1));
        let small = spawn("small", permits_for(1));
        let heavy_last = spawn("heavy", permits_for(HEAVY_PIXELS + 1));
        drop(held);
        for render in [heavy_first, small, heavy_last] {
            render.join().expect("the render ends");
        }
        assert_eq!(*order.lock().expect("the order"), ["heavy", "small", "heavy"], "the small render draws after the heavy one ahead of it only");
        assert_eq!(permits.running(), 0, "every permit is given back");
        assert_eq!(permits.queued(), 0, "nobody is left in line");
    }

    #[test]
    fn a_panicking_stop_takes_its_place_out_of_the_queue_so_the_renders_behind_it_draw() {
        let permits = Arc::new(RasterPermits::with_limit(1));
        let held = permits.try_acquire(1).expect("the only permit");
        let waiter = {
            let permits = Arc::clone(&permits);
            std::thread::spawn(move || {
                let checks = std::cell::Cell::new(0_usize);
                permits
                    .acquire(1, move || {
                        let n = checks.get() + 1;
                        checks.set(n);
                        assert!(n < 2, "the stop check panics on its second call");
                        false
                    })
                    .is_some()
            })
        };
        assert!(waiter.join().is_err(), "the panic reaches the waiter");
        assert_eq!(permits.queued(), 0, "the panicking wait left the queue");
        drop(held);
        assert_eq!(permits.running(), 0, "every permit is given back");
        let after = permits.acquire(1, || false).expect("a render after it draws");
        drop(after);
        assert_eq!(permits.queued(), 0, "the render after it left the queue too");
    }
}
