//! The heaps' records' rounds, asked ahead and run by whoever gets there first
//! (`VICTORY_HEAP_ROUNDS_EARLY=1`, [`super::rounds_early`]).
//!
//! Measured in game (8d8db49, 5500X3D): 17 rounds a frame of 0.5 ms each,
//! begun 0.3 ms after they were asked for, and the submitting thread waited
//! 10.7-13.3 ms a frame for them. It waited out each round's start and length
//! one after the other. Two things change here:
//! - A round is also asked at the top of a submission, so it runs beside the
//!   card steps and the wait for flights. The round asked where it always was
//!   then finds only what was written since.
//! - A submission whose round has not begun when it needs it runs the round
//!   itself, on its own thread: no wake-up of the round's thread to wait for.
//!
//! What a submission sees is what it saw before. Its lists go to the queue
//! only once a round has ended that began after the place where its round was
//! asked before. More rounds in between are like the early rounds the
//! thread already makes every 4 ms.
//!
//! A round numbers nothing itself. It takes the number of the last ask when it
//! begins, and when it ends every ask up to that number is answered.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Asks, rounds begun and rounds done, by the number of the last ask each
/// covers.
struct State {
    asked: u64,
    begun: u64,
    done: u64,
    /// When the last round was asked for: early rounds are made only while
    /// submissions come.
    last_ask: Option<Instant>,
    /// Tests only: the thread is to end.
    stop: bool,
}

/// How a submission's round was had ([`Rounds::wait_or_run`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Had {
    /// Run on the caller's thread.
    pub(crate) ran: bool,
    /// Microseconds waiting for a round under way first.
    pub(crate) behind_us: u64,
    /// Microseconds of the round run on the caller's thread.
    pub(crate) round_us: u64,
}

pub(crate) struct Rounds {
    state: Mutex<State>,
    moved: Condvar,
    /// One round at a time, whoever runs it.
    running: Mutex<()>,
    /// Tests only: the thread starts a round this many microseconds after it
    /// was woken for it (in game: about 300).
    #[cfg(test)]
    lag_us: std::sync::atomic::AtomicU64,
}

/// A round is over, however it ends: the asks it covers are answered.
struct Ends<'a>(&'a Rounds, u64);

impl Drop for Ends<'_> {
    fn drop(&mut self) {
        let mut s = self.0.state();
        s.done = s.done.max(self.1);
        drop(s);
        self.0.moved.notify_all();
    }
}

impl Rounds {
    pub(crate) const fn new() -> Self {
        Rounds {
            state: Mutex::new(State { asked: 0, begun: 0, done: 0, last_ask: None, stop: false }),
            moved: Condvar::new(),
            running: Mutex::new(()),
            #[cfg(test)]
            lag_us: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn running(&self) -> MutexGuard<'_, ()> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Asks for a round that begins after this. Its number, for
    /// [`Rounds::wait_or_run`].
    pub(crate) fn ask(&self) -> u64 {
        let mut s = self.state();
        s.asked += 1;
        s.last_ask = Some(Instant::now());
        let mine = s.asked;
        drop(s);
        self.moved.notify_all();
        mine
    }


    /// The round's thread: a round whenever one is asked for and nobody has
    /// begun it, and with `early`, one of its own that often while asks came
    /// in the last 500 ms. `round` is told whether a submission asked for it.
    /// Returns only once [`Rounds::stop`] was called.
    pub(crate) fn serve(&self, early: Option<Duration>, mut round: impl FnMut(bool)) {
        loop {
            let mut s = self.state();
            #[cfg(test)]
            let mut lagged = false;
            let claim = loop {
                if s.stop {
                    return;
                }
                if s.asked > s.begun {
                    #[cfg(test)]
                    if !lagged {
                        // The time a woken thread takes to get a core.
                        lagged = true;
                        drop(s);
                        spin_us(self.lag_us.load(std::sync::atomic::Ordering::Relaxed));
                        s = self.state();
                        continue;
                    }
                    break Some(s.asked);
                }
                let recent = s.last_ask.is_some_and(|at| at.elapsed() < Duration::from_millis(500));
                match early.filter(|_| recent) {
                    None => s = self.moved.wait(s).unwrap_or_else(|e| e.into_inner()),
                    Some(every) => {
                        let (back, over) = self.moved.wait_timeout(s, every).unwrap_or_else(|e| e.into_inner());
                        s = back;
                        if over.timed_out() && s.asked == s.begun && !s.stop {
                            break None;
                        }
                    }
                }
            };
            if let Some(claim) = claim {
                s.begun = claim;
            }
            drop(s);
            let _one = self.running();
            let _ends = claim.map(|claim| Ends(self, claim));
            // Run by a submission meanwhile, while this waited for the
            // one before: nothing is left of it.
            if claim.is_some_and(|claim| self.state().done >= claim) {
                continue;
            }
            round(claim.is_some());
        }
    }

    /// Returns once round `mine` is done: waits for it if it is under way,
    /// or else runs `round` here, after any round under way.
    pub(crate) fn wait_or_run(&self, mine: u64, round: impl FnOnce()) -> Had {
        let mut s = self.state();
        while s.done < mine && s.begun >= mine {
            s = self.moved.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if s.done >= mine {
            return Had::default();
        }
        // Nobody has begun it: this thread does, and it covers every ask so
        // far.
        let claim = s.asked;
        s.begun = claim;
        drop(s);
        let behind = Instant::now();
        let _one = self.running();
        let behind_us = behind.elapsed().as_micros() as u64;
        let ends = Ends(self, claim);
        let began = Instant::now();
        round();
        let round_us = began.elapsed().as_micros() as u64;
        drop(ends);
        Had { ran: true, behind_us, round_us }
    }

    /// Tests only: [`Rounds::serve`] returns.
    #[cfg(test)]
    pub(crate) fn stop(&self) {
        self.state().stop = true;
        self.moved.notify_all();
    }
}

#[cfg(test)]
fn spin_us(us: u64) {
    let until = Instant::now() + Duration::from_micros(us);
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    //! A model of the title's heap, its host heap and port 2's marks, replayed
    //! through the rounds as they were (`Old`, the code of `begin_sync_heaps`
    //! and its thread, kept here as the reference) and through [`Rounds`].
    use super::{Rounds, spin_us};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// Regions of slots, one per submission in turn: the title writes a
    /// region again only for a later submission, as it waits for the card
    /// before it reuses descriptors.
    const REGIONS: usize = 8;
    const REGION: usize = 64;
    /// Slots past the regions: written all the time by another thread.
    const NOISE: usize = 256;

    struct Heap {
        title: Vec<AtomicU64>,
        host: Vec<AtomicU64>,
        /// Port 2's marks: slots written since the last round took them.
        marks: Mutex<Vec<usize>>,
        /// Microseconds a round costs, and a marked slot in it (nanoseconds).
        base_us: u64,
        per_slot_ns: u64,
    }

    impl Heap {
        fn new(base_us: u64, per_slot_ns: u64) -> Arc<Heap> {
            let n = REGIONS * REGION + NOISE;
            Arc::new(Heap {
                title: (0..n).map(|_| AtomicU64::new(0)).collect(),
                host: (0..n).map(|_| AtomicU64::new(0)).collect(),
                marks: Mutex::new(Vec::new()),
                base_us,
                per_slot_ns,
            })
        }

        fn write(&self, slot: usize, value: u64) {
            self.title[slot].store(value, Relaxed);
            self.marks.lock().unwrap().push(slot);
        }

        /// A round: the marked slots' records read and mirrored.
        fn round(&self) {
            let began = Instant::now();
            let marked = std::mem::take(&mut *self.marks.lock().unwrap());
            for &slot in &marked {
                self.host[slot].store(self.title[slot].load(Relaxed), Relaxed);
            }
            let cost = Duration::from_micros(self.base_us) + Duration::from_nanos(self.per_slot_ns * marked.len() as u64);
            while began.elapsed() < cost {
                std::hint::spin_loop();
            }
        }

        fn region(&self, s: usize, of: &[AtomicU64]) -> Vec<u64> {
            let from = (s % REGIONS) * REGION;
            of[from..from + REGION].iter().map(|v| v.load(Relaxed)).collect()
        }
    }

    /// The old protocol: one round per ask, by the thread alone.
    struct Old {
        rounds: Mutex<(u64, u64)>,
        moved: Condvar,
        stop: AtomicBool,
        lag_us: u64,
    }

    impl Old {
        fn serve(&self, early: Option<Duration>, heap: &Heap) {
            let mut last = Instant::now();
            loop {
                let mut r = self.rounds.lock().unwrap();
                let mut asked = r.0 != r.1;
                let mut lagged = false;
                while !asked || !lagged {
                    if self.stop.load(Relaxed) {
                        return;
                    }
                    if asked {
                        lagged = true;
                        drop(r);
                        spin_us(self.lag_us);
                        r = self.rounds.lock().unwrap();
                        continue;
                    }
                    let Some(every) = early.filter(|_| last.elapsed().as_millis() < 500) else {
                        r = self.moved.wait_timeout(r, Duration::from_millis(5)).unwrap().0;
                        asked = r.0 != r.1;
                        continue;
                    };
                    let (back, over) = self.moved.wait_timeout(r, every).unwrap();
                    r = back;
                    asked = r.0 != r.1;
                    if over.timed_out() && !asked {
                        break;
                    }
                }
                drop(r);
                if asked {
                    last = Instant::now();
                }
                heap.round();
                if asked {
                    self.rounds.lock().unwrap().1 += 1;
                    self.moved.notify_all();
                }
            }
        }

        fn ask(&self) -> u64 {
            let mut r = self.rounds.lock().unwrap();
            r.0 += 1;
            let mine = r.0;
            drop(r);
            self.moved.notify_all();
            mine
        }

        fn wait(&self, mine: u64) {
            let mut r = self.rounds.lock().unwrap();
            while r.1 < mine {
                r = self.moved.wait(r).unwrap();
            }
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Way {
        Old,
        New,
        /// Wrong on purpose: waits for a round asked at the top, before a
        /// landing wrote title memory.
        TopOnly,
    }

    /// One submission of a replay.
    struct Plan {
        /// Slots of its region the title writes before it submits.
        writes: Vec<usize>,
        /// Slots of its region a landing writes before its round was asked
        /// (copy-back writes title memory).
        landed: Vec<usize>,
        /// Microseconds from the top to where its round was asked, and from
        /// there to where it is waited for.
        pre_us: u64,
        prep_us: u64,
    }

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut x = seed;
        move || {
            x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
    }

    fn plans(seed: u64, n: usize, writes: usize, landed: usize, pre_us: u64, prep_us: u64, vary: bool) -> Vec<Plan> {
        let mut next = rng(seed);
        let mut pick = |k: usize| -> Vec<usize> { (0..k).map(|_| (next() % REGION as u64) as usize).collect() };
        let mut plans = Vec::new();
        for _ in 0..n {
            plans.push(Plan { writes: pick(writes), landed: pick(landed), pre_us, prep_us });
        }
        if vary {
            let mut next = rng(seed ^ 0xabcd);
            for plan in &mut plans {
                plan.writes.truncate((next() % (writes as u64 + 1)) as usize);
                plan.landed.truncate((next() % (landed as u64 + 1)) as usize);
                plan.pre_us = next() % (2 * pre_us + 1);
                plan.prep_us = next() % (2 * prep_us + 1);
            }
        }
        plans
    }

    /// What one replay gave: each submission's region of the host heap when
    /// its lists went, and of the title heap then; and the microseconds the
    /// submitting thread waited for rounds.
    struct Replay {
        seen: Vec<Vec<u64>>,
        truth: Vec<Vec<u64>>,
        waited_us: u64,
    }

    fn replay(way: Way, plans: &[Plan], lag_us: u64, early: Option<Duration>, heap: Arc<Heap>, noise: bool) -> Replay {
        let old = Arc::new(Old { rounds: Mutex::new((0, 0)), moved: Condvar::new(), stop: AtomicBool::new(false), lag_us });
        let new = Arc::new(Rounds::new());
        new.lag_us.store(lag_us, Relaxed);
        let thread = {
            let (old, new, heap) = (old.clone(), new.clone(), heap.clone());
            std::thread::spawn(move || match way {
                Way::Old => old.serve(early, &heap),
                Way::New | Way::TopOnly => new.serve(early, |_| heap.round()),
            })
        };
        // The title's other threads, writing descriptors of later frames.
        let stop_noise = Arc::new(AtomicBool::new(false));
        let noisy = noise.then(|| {
            let (heap, stop) = (heap.clone(), stop_noise.clone());
            std::thread::spawn(move || {
                let mut next = rng(7);
                let mut v = 1u64 << 63;
                while !stop.load(Relaxed) {
                    v += 1;
                    heap.write(REGIONS * REGION + (next() % NOISE as u64) as usize, v);
                    spin_us(5);
                }
            })
        });
        let (mut seen, mut truth, mut waited_us) = (Vec::new(), Vec::new(), 0u64);
        let mut value = 0u64;
        for (s, plan) in plans.iter().enumerate() {
            let base = (s % REGIONS) * REGION;
            for &slot in &plan.writes {
                value += 1;
                heap.write(base + slot, ((s as u64) << 32) | value);
            }
            // The top of `Host::submit`.
            let ahead = (way != Way::Old).then(|| new.ask());
            if way == Way::TopOnly
                && let Some(ahead) = ahead
            {
                new.wait_or_run(ahead, || heap.round());
            }
            // The card steps and the wait for flights, which land what
            // shaders wrote.
            spin_us(plan.pre_us);
            for &slot in &plan.landed {
                value += 1;
                heap.write(base + slot, ((s as u64) << 32) | value);
            }
            // Where the round was asked before.
            let mine = match way {
                Way::Old => old.ask(),
                Way::New => new.ask(),
                Way::TopOnly => 0,
            };
            spin_us(plan.prep_us);
            let waiting = Instant::now();
            match way {
                Way::Old => old.wait(mine),
                Way::New => {
                    new.wait_or_run(mine, || heap.round());
                }
                Way::TopOnly => {}
            }
            waited_us += waiting.elapsed().as_micros() as u64;
            // The lists go.
            seen.push(heap.region(s, &heap.host));
            truth.push(heap.region(s, &heap.title));
        }
        stop_noise.store(true, Relaxed);
        if let Some(noisy) = noisy {
            noisy.join().unwrap();
        }
        old.stop.store(true, Relaxed);
        old.moved.notify_all();
        new.stop();
        thread.join().unwrap();
        Replay { seen, truth, waited_us }
    }

    /// Fixed seeds, fixed and varied lengths, with and without early rounds
    /// and other writers: every submission's records are on the host heap
    /// when its lists go, the same with the rounds asked early as with the
    /// rounds as they were.
    #[test]
    fn the_submissions_see_the_same_records() {
        for seed in [1u64, 2, 3, 0x5eed, 0xdead_beef] {
            for vary in [false, true] {
                for early in [None, Some(Duration::from_millis(1))] {
                    let plans = plans(seed, 40, 24, 6, 150, 60, vary);
                    let lag = if vary { seed % 200 } else { 100 };
                    let old = replay(Way::Old, &plans, lag, early, Heap::new(20, 100), true);
                    let new = replay(Way::New, &plans, lag, early, Heap::new(20, 100), true);
                    for (s, ((seen_old, seen_new), (truth_old, truth_new))) in
                        old.seen.iter().zip(&new.seen).zip(old.truth.iter().zip(&new.truth)).enumerate()
                    {
                        assert_eq!(seen_old, truth_old, "seed {seed:#x} submission {s}: the old rounds lost a record");
                        assert_eq!(truth_old, truth_new, "seed {seed:#x} submission {s}: the replays differ");
                        assert_eq!(seen_new, seen_old, "seed {seed:#x} submission {s}: the host heap seen differs");
                    }
                }
            }
        }
    }

    /// The model sees a wrong order: a round that began before the landing
    /// wrote title memory leaves the landed records out.
    #[test]
    fn a_round_begun_before_the_landing_misses_it() {
        let plans = plans(9, 30, 8, 8, 300, 50, false);
        let wrong = replay(Way::TopOnly, &plans, 0, None, Heap::new(5, 0), false);
        assert!(wrong.seen.iter().zip(&wrong.truth).any(|(seen, truth)| seen != truth));
    }

    /// Many asks on one round, rounds taken from the thread, and a round that
    /// panics: each wait returns, and only once a round begun after its ask
    /// ended.
    #[test]
    fn every_ask_is_answered_by_a_round_begun_after_it() {
        let rounds = Arc::new(Rounds::new());
        let began = Arc::new(Mutex::new(Vec::<(u64, Instant)>::new()));
        let stamp = Arc::new(AtomicU64::new(0));
        let thread = {
            let (rounds, began, stamp) = (rounds.clone(), began.clone(), stamp.clone());
            std::thread::spawn(move || {
                rounds.serve(Some(Duration::from_micros(300)), |asked| {
                    if asked {
                        began.lock().unwrap().push((stamp.fetch_add(1, Relaxed), Instant::now()));
                    }
                    spin_us(50);
                })
            })
        };
        let mut next = rng(3);
        for _ in 0..300 {
            let asked_at = Instant::now();
            let mine = rounds.ask();
            if next().is_multiple_of(3) {
                rounds.ask();
            }
            spin_us(next() % 120);
            let panics = next().is_multiple_of(17);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rounds.wait_or_run(mine, || {
                    began.lock().unwrap().push((stamp.fetch_add(1, Relaxed), Instant::now()));
                    if panics {
                        // (Without the panic hook's message.)
                        std::panic::resume_unwind(Box::new("a round that panics"));
                    }
                })
            }));
            assert!(rounds.state().done >= mine);
            assert!(began.lock().unwrap().iter().any(|&(_, at)| at >= asked_at), "no round began after the ask");
        }
        rounds.stop();
        thread.join().unwrap();
    }

    /// A replay with the figures of the game run: 17 submissions a frame, each
    /// round begun 0.3 ms after it was asked for and 0.5 ms long; the
    /// submitting thread's wait for them, the rounds as they were and asked
    /// early. Offline, a model: the lengths of the card steps and of the
    /// preparing past the ask are guesses, given with each figure.
    /// `cargo test --release -p victory-gpu --lib -- --ignored --nocapture bench_heap_rounds_early`.
    #[test]
    #[ignore = "benchmark; run with --release --ignored --nocapture"]
    fn bench_heap_rounds_early() {
        const FRAMES: usize = 20;
        // 0.5 ms a round of 960 records: 20 us and 0.5 us a record.
        let (base_us, per_slot_ns, writes, landed) = (20, 500, 960, 20);
        for (pre_us, prep_us) in [(400, 150), (150, 150), (400, 50), (800, 300)] {
            let plans = plans(0x5eed, 17 * FRAMES, writes, landed, pre_us, prep_us, false);
            let mut figures = Vec::new();
            for way in [Way::Old, Way::New] {
                let run = replay(way, &plans, 300, Some(Duration::from_millis(4)), Heap::new(base_us, per_slot_ns), false);
                assert_eq!(run.seen, run.truth);
                figures.push(run.waited_us as f64 / 1e3 / FRAMES as f64);
            }
            println!(
                "heap rounds, 17 a frame, begun 0.3 ms after the ask, 0.5 ms each; card steps {:.2} ms, preparing past the ask {:.2} ms: \
                 the submitting thread waited {:.2} ms a frame as before, {:.2} ms asked early (offline, model)",
                pre_us as f64 / 1e3,
                prep_us as f64 / 1e3,
                figures[0],
                figures[1]
            );
        }
    }
}
