//! A round of the heaps' records that nobody has begun, run by the submission
//! that waits for it (`VICTORY_HEAP_ROUNDS_EARLY=1`; off by default).
//!
//! Ported in spirit from runkis-nvidia's old base (53d7171). There the
//! submitting thread waited 10.7-13.3 ms a frame for 17 rounds of 0.5 ms,
//! each begun 0.3 ms after it was asked for: the heap-sync thread's wake-up,
//! paid once a round. The old switch also asked for a round at the top of
//! each submission; upstream's `VICTORY_HEAP_SYNC_AT_SUBMIT` (on in `victory
//! play`) asks even earlier, at the title's submit, so only the other half
//! is carried over: at the wait, a round asked for that the thread has not
//! begun is run on the submitting thread instead.
//!
//! Why a submission sees the same records: the rounds are numbered as before
//! (asked, done; a round counts once it is done). Round `k` begins only
//! once `k - 1` is done and `k` was asked for, by whichever thread claims it
//! first ([`claim`]); one round runs at a time ([`RUNNING`], changed only
//! under the rounds' lock). So round `mine` still begins after the ask that
//! made it, and the wait still ends only once it is done.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};

/// Whether the switch is on (read once).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_HEAP_ROUNDS_EARLY"))
}

/// A round is under way (on the heap-sync thread or a submitting thread).
/// Read and written only with the rounds' lock held.
pub(crate) static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether a round may begin now, with rounds `asked` and `done`: one is
/// asked for and none is under way. Takes it if so.
pub(crate) fn claim(rounds: &(u64, u64), running: &AtomicBool) -> bool {
    if rounds.0 != rounds.1 && !running.load(Relaxed) {
        running.store(true, Relaxed);
        return true;
    }
    false
}

/// What [`wait_or_run`] did, for the frame line.
#[derive(Default)]
pub(crate) struct Waited {
    /// Rounds run on the calling thread.
    pub(crate) ran: u64,
    /// Microseconds of those.
    pub(crate) ran_us: u64,
}

/// Waits until round `mine` is done. With `here`, a round asked for that
/// nobody has begun is run by the caller (`round`), and counted done, as the
/// heap-sync thread does (ound is given its number); without it this is
/// the old wait.
pub(crate) fn wait_or_run(
    lock: &Mutex<(u64, u64)>,
    told: &Condvar,
    running: &AtomicBool,
    mine: u64,
    here: bool,
    mut round: impl FnMut(u64),
) -> Waited {
    let mut waited = Waited::default();
    let mut rounds = lock.lock().unwrap_or_else(|e| e.into_inner());
    while rounds.1 < mine {
        if here && claim(&rounds, running) {
            let number = rounds.1 + 1;
            drop(rounds);
            let began = std::time::Instant::now();
            /// Done however the round ends.
            struct Over<'a>(&'a Mutex<(u64, u64)>, &'a Condvar, &'a AtomicBool);
            impl Drop for Over<'_> {
                fn drop(&mut self) {
                    let mut r = self.0.lock().unwrap_or_else(|e| e.into_inner());
                    r.1 += 1;
                    self.2.store(false, Relaxed);
                    drop(r);
                    self.1.notify_all();
                }
            }
            {
                let _over = Over(lock, told, running);
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| round(number)));
            }
            waited.ran += 1;
            waited.ran_us += began.elapsed().as_micros() as u64;
            rounds = lock.lock().unwrap_or_else(|e| e.into_inner());
            continue;
        }
        rounds = told.wait(rounds).unwrap_or_else(|e| e.into_inner());
    }
    waited
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// The rounds, as the heap-sync thread and the submissions share them:
    /// a model with real threads. A clock counts events; each ask notes its
    /// time and the round number it was given, each round the time it
    /// began. The heap-sync thread is the old loop (rounds asked for, early
    /// rounds while idle) with the claim; submissions ask and wait, with
    /// and without `here`.
    struct Model {
        lock: Mutex<(u64, u64)>,
        told: Condvar,
        running: AtomicBool,
        clock: AtomicU64,
        /// Time each round (by number) began, and how many ran at once.
        began: Mutex<Vec<(u64, u64)>>,
        inside: AtomicU64,
        stop: AtomicBool,
        /// Rounds the waiters ran.
        here: AtomicU64,
    }

    impl Model {
        fn round(&self, number: Option<u64>) {
            assert_eq!(self.inside.fetch_add(1, Relaxed), 0, "two rounds at once");
            let at = self.clock.fetch_add(1, Relaxed);
            if let Some(n) = number {
                self.began.lock().unwrap().push((n, at));
            }
            std::thread::yield_now();
            self.inside.fetch_sub(1, Relaxed);
        }

        /// The heap-sync thread's loop, as in `begin_sync_heaps`.
        fn serve(&self) {
            loop {
                let mut r = self.lock.lock().unwrap();
                let asked = loop {
                    if self.stop.load(Relaxed) {
                        return;
                    }
                    if claim(&r, &self.running) {
                        break true;
                    }
                    let (back, over) = self.told.wait_timeout(r, std::time::Duration::from_micros(200)).unwrap();
                    r = back;
                    if over.timed_out() && !self.running.load(Relaxed) {
                        self.running.store(true, Relaxed);
                        break false;
                    }
                };
                // The round's number is the one after those done.
                let number = asked.then_some(r.1 + 1);
                drop(r);
                self.round(number);
                let mut r = self.lock.lock().unwrap();
                if asked {
                    r.1 += 1;
                }
                self.running.store(false, Relaxed);
                drop(r);
                self.told.notify_all();
            }
        }
    }

    fn spin(us: u64) {
        let t = std::time::Instant::now();
        while t.elapsed().as_micros() < u128::from(us) {
            std::hint::spin_loop();
        }
    }

    /// What the submitting thread waits a frame (offline, a model; ignored:
    /// `cargo test --release -p victory-gpu --lib -- --ignored --nocapture
    /// bench_rounds_here`): 17 submissions, each asks and then prepares
    /// `prep` us before it waits; the heap-sync thread wakes `wake` us after
    /// an ask (the in-game 0.3 ms), a round takes 500 us.
    #[test]
    #[ignore]
    fn bench_rounds_here() {
        for (wake, prep) in [(300u64, 150u64), (300, 400), (300, 0), (100, 150)] {
            let mut line = Vec::new();
            for here in [false, true] {
                let lock = Mutex::new((0u64, 0u64));
                let told = Condvar::new();
                let running = AtomicBool::new(false);
                let stop = AtomicBool::new(false);
                let waited_us = std::thread::scope(|s| {
                    s.spawn(|| {
                        loop {
                            let mut r = lock.lock().unwrap();
                            while r.0 == r.1 {
                                if stop.load(Relaxed) {
                                    return;
                                }
                                r = told.wait_timeout(r, std::time::Duration::from_millis(2)).unwrap().0;
                            }
                            drop(r);
                            // The thread's wake-up after the ask, then the
                            // round if nobody took it meanwhile.
                            spin(wake);
                            if !claim(&lock.lock().unwrap(), &running) {
                                continue;
                            }
                            spin(500);
                            let mut r = lock.lock().unwrap();
                            r.1 += 1;
                            running.store(false, Relaxed);
                            drop(r);
                            told.notify_all();
                        }
                    });
                    let mut waited = 0u64;
                    for _ in 0..20 {
                        for _ in 0..17 {
                            let mine = {
                                let mut r = lock.lock().unwrap();
                                r.0 += 1;
                                r.0
                            };
                            told.notify_all();
                            spin(prep);
                            let t = std::time::Instant::now();
                            wait_or_run(&lock, &told, &running, mine, here, |_| spin(500));
                            waited += t.elapsed().as_micros() as u64;
                        }
                    }
                    stop.store(true, Relaxed);
                    told.notify_all();
                    waited / 20
                });
                line.push(waited_us as f64 / 1e3);
            }
            println!("wake {wake} us, preparing {prep} us past the ask: waited {:.2} ms a frame before, {:.2} ms with rounds run here", line[0], line[1]);
        }
    }

    #[test]
    fn every_wait_ends_after_a_round_begun_after_its_ask() {
        for here in [false, true] {
            let m = Arc::new(Model {
                lock: Mutex::new((0, 0)),
                told: Condvar::new(),
                running: AtomicBool::new(false),
                clock: AtomicU64::new(0),
                began: Mutex::new(Vec::new()),
                inside: AtomicU64::new(0),
                stop: AtomicBool::new(false),
                here: AtomicU64::new(0),
            });
            let server = {
                let m = m.clone();
                std::thread::spawn(move || m.serve())
            };
            let asks: Vec<(u64, u64)> = std::thread::scope(|s| {
                let workers: Vec<_> = (0..3u64)
                    .map(|t| {
                        let m = m.clone();
                        s.spawn(move || {
                            let mut seed = 0x9e37_79b9_7f4a_7c15u64 ^ t;
                            let mut asks = Vec::new();
                            for _ in 0..300 {
                                seed ^= seed << 13;
                                seed ^= seed >> 7;
                                seed ^= seed << 17;
                                let mut r = m.lock.lock().unwrap();
                                r.0 += 1;
                                let mine = r.0;
                                let at = m.clock.fetch_add(1, Relaxed);
                                drop(r);
                                m.told.notify_all();
                                if seed.is_multiple_of(4) {
                                    std::thread::yield_now();
                                }
                                let had = wait_or_run(&m.lock, &m.told, &m.running, mine, here, |n| m.round(Some(n)));
                                m.here.fetch_add(had.ran, Relaxed);
                                // Done by now: the round of its number.
                                assert!(m.lock.lock().unwrap().1 >= mine);
                                asks.push((mine, at));
                            }
                            asks
                        })
                    })
                    .collect();
                workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
            });
            m.stop.store(true, Relaxed);
            m.told.notify_all();
            server.join().unwrap();
            // Without it nobody but the thread runs a round.
            assert_eq!(m.here.load(Relaxed) > 0, here);
            let began = m.began.lock().unwrap().clone();
            assert_eq!(began.len() as u64, m.lock.lock().unwrap().1, "every round done once");
            // The rounds ran in order, each begun after the ask it answers.
            let mut by_number = began.clone();
            by_number.sort();
            assert!(by_number.iter().enumerate().all(|(i, r)| r.0 == i as u64 + 1), "here {here}");
            for (mine, at) in asks {
                let round = by_number[(mine - 1) as usize];
                assert!(round.1 > at, "here {here}: round {mine} began at {}, the ask at {at}", round.1);
            }
        }
    }
}
