//! The register of copies in the card's memory (`crate::shadow::card_dirty`
//! and its neighbours), apart from `Shadow` so that it can be tested alone.
//!
//! Before, the register was a `Vec<Weak<Shadow>>` walked whole at every
//! submission: each copy upgraded, its "touched" flag swapped, the dead ones
//! dropped (3.6 % of the submitting thread in the profile of 2026-11-27, the
//! closure of that `retain`). Most copies were not touched since the last
//! submission. Now a copy whose flag goes up says so once ([`Register::mark`]),
//! and the submission visits only those, in the register's order: the same
//! copies, in the same order, as the walk visited and found touched. The dead
//! are dropped when the register has grown to twice what it held after the
//! last such sweep, not at every submission.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// One copy: its number in the register (in the order added), its title
/// range, and the copy.
struct Entry<T> {
    number: u64,
    start: usize,
    end: usize,
    weak: Weak<T>,
}

/// The register's entries, in the order added (so in the order of their
/// numbers), with the next number and the length after the last sweep.
struct Entries<T> {
    all: Vec<Entry<T>>,
    next: u64,
    swept: usize,
}

/// What a copy keeps for the register: its number there (0 until it is
/// added) and whether something changed in it since it was last taken.
#[derive(Default)]
pub(crate) struct Mark {
    number: AtomicU64,
    touched: AtomicBool,
}

impl Mark {
    /// Takes the flag down: whether it was up. As the walk's swap did.
    pub(crate) fn take(&self) -> bool {
        self.touched.swap(false, Ordering::AcqRel)
    }
}

pub(crate) struct Register<T> {
    entries: Mutex<Entries<T>>,
    /// Numbers of the entries whose flag went up since the last
    /// [`Register::take_touched`]. Taken last of the locks, alone.
    touched: Mutex<Vec<u64>>,
}

/// The register sweeps the dead at twice this many entries at least.
const LEAST_SWEEP: usize = 64;

impl<T> Register<T> {
    pub(crate) const fn new() -> Register<T> {
        Register {
            entries: Mutex::new(Entries {
                all: Vec::new(),
                next: 1,
                swept: 0,
            }),
            touched: Mutex::new(Vec::new()),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, Entries<T>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn touched(&self) -> std::sync::MutexGuard<'_, Vec<u64>> {
        self.touched.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds `copy` of title memory `[start, end)`, whose mark is `mark`.
    pub(crate) fn add(&self, copy: &Arc<T>, start: usize, end: usize, mark: &Mark) {
        let mut entries = self.entries();
        let number = entries.next;
        entries.next += 1;
        if entries.all.len() >= (2 * entries.swept).max(2 * LEAST_SWEEP) {
            entries.all.retain(|e| e.weak.strong_count() > 0);
            entries.swept = entries.all.len();
        }
        entries.all.push(Entry {
            number,
            start,
            end,
            weak: Arc::downgrade(copy),
        });
        // The number first, then the flag: a flag that went up before it was
        // there is told here, one that goes up after it finds the number
        // ([`Register::mark`]). Either sees the other (one order for both).
        mark.number.store(number, Ordering::SeqCst);
        if mark.touched.load(Ordering::SeqCst) {
            self.touched().push(number);
        }
    }

    /// The copy whose mark is `mark` changed: its flag goes up, and if it was
    /// down the register is told. The caller holds what orders the change
    /// with the taking (the copy's list of changes).
    pub(crate) fn mark(&self, mark: &Mark) {
        if mark.touched.swap(true, Ordering::SeqCst) {
            return;
        }
        let number = mark.number.load(Ordering::SeqCst);
        if number != 0 {
            self.touched().push(number);
        }
    }

    /// Each live copy whose flag went up since the last call, in the
    /// register's order, under the register's lock: `visit` takes the flag
    /// down ([`Mark::take`]) and what changed. (A copy told twice, or whose
    /// flag is down again, is visited once and finds it down: as the walk
    /// found the flags of the copies not touched.)
    pub(crate) fn take_touched(&self, mut visit: impl FnMut(Arc<T>)) {
        let entries = self.entries();
        let mut numbers = std::mem::take(&mut *self.touched());
        if numbers.is_empty() {
            return;
        }
        numbers.sort_unstable();
        numbers.dedup();
        let mut from = 0;
        for number in numbers {
            let Ok(at) = entries.all[from..].binary_search_by_key(&number, |e| e.number) else {
                continue;
            };
            from += at;
            if let Some(copy) = entries.all[from].weak.upgrade() {
                visit(copy);
            }
            from += 1;
        }
    }

    /// The live copies, in order.
    pub(crate) fn live(&self) -> Vec<Arc<T>> {
        self.entries().all.iter().filter_map(|e| e.weak.upgrade()).collect()
    }

    /// The live copies whose title range overlaps `[from, to)`, in order:
    /// those of [`Register::live`] that do, without upgrading the others.
    pub(crate) fn live_in(&self, from: usize, to: usize) -> Vec<Arc<T>> {
        self.entries().all.iter().filter(|e| e.start < to && from < e.end).filter_map(|e| e.weak.upgrade()).collect()
    }

    /// How many copies live.
    pub(crate) fn count(&self) -> usize {
        self.entries().all.iter().filter(|e| e.weak.strong_count() > 0).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy as the register sees it, with what changed in it.
    struct Held {
        id: u32,
        start: usize,
        len: usize,
        mark: Mark,
        dirty: Mutex<Vec<u32>>,
    }

    impl Held {
        /// What `Shadow::card_changed_runs` does.
        fn change(&self, register: &Register<Held>, what: u32) {
            let mut dirty = self.dirty.lock().unwrap();
            dirty.push(what);
            register.mark(&self.mark);
        }

        /// What `card_dirty` does with a copy.
        fn taken(&self) -> Option<(u32, Vec<u32>)> {
            self.mark.take().then(|| (self.id, std::mem::take(&mut *self.dirty.lock().unwrap()))).filter(|(_, d)| !d.is_empty())
        }
    }

    /// The register as it was: every copy upgraded and its flag swapped, the
    /// dead dropped, at every taking.
    fn walk(old: &mut Vec<Weak<Held>>) -> Vec<(u32, Vec<u32>)> {
        let mut out = Vec::new();
        old.retain(|weak| {
            let Some(copy) = weak.upgrade() else {
                return false;
            };
            out.extend(copy.taken());
            true
        });
        out
    }

    fn random(seed: &mut u64, n: usize) -> usize {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 33) as usize % n
    }

    /// Fixed-seed runs of copies made (some changed before they are added),
    /// changed, let go and taken: the register gives what the walk gave, copy
    /// for copy and change for change, in the same order; and the live copies,
    /// all and by range, are the walk's.
    #[test]
    fn the_register_takes_what_the_walk_took() {
        for run in 0..40u64 {
            let mut seed = 0x5eed_0000 + run;
            let register = Register::<Held>::new();
            let mut old: Vec<Weak<Held>> = Vec::new();
            // Two sets of the same copies' doubles: one in the register, one
            // in the walk, changed alike.
            let mut held: Vec<(Arc<Held>, Arc<Held>)> = Vec::new();
            let mut id = 0u32;
            for step in 0..3000u32 {
                match random(&mut seed, 10) {
                    0 | 1 => {
                        id += 1;
                        let (start, len) = (random(&mut seed, 1 << 20), 1 + random(&mut seed, 1 << 16));
                        let make = || Held { id, start, len, mark: Mark::default(), dirty: Mutex::default() };
                        let (a, b) = (Arc::new(make()), Arc::new(make()));
                        if random(&mut seed, 4) == 0 {
                            a.change(&register, step);
                            b.dirty.lock().unwrap().push(step);
                            b.mark.touched.store(true, Ordering::SeqCst);
                        }
                        register.add(&a, a.start, a.start + a.len, &a.mark);
                        old.push(Arc::downgrade(&b));
                        held.push((a, b));
                    }
                    2 if !held.is_empty() => {
                        let at = random(&mut seed, held.len());
                        held.swap_remove(at);
                    }
                    3..=6 if !held.is_empty() => {
                        let at = random(&mut seed, held.len());
                        held[at].0.change(&register, step);
                        let b = &held[at].1;
                        b.dirty.lock().unwrap().push(step);
                        b.mark.touched.store(true, Ordering::SeqCst);
                    }
                    7 => {
                        let mut got = Vec::new();
                        register.take_touched(|copy| got.extend(copy.taken()));
                        assert_eq!(got, walk(&mut old), "run {run}, step {step}");
                    }
                    8 => {
                        let ids = |v: Vec<Arc<Held>>| v.iter().map(|c| c.id).collect::<Vec<_>>();
                        let all: Vec<Arc<Held>> = old.iter().filter_map(Weak::upgrade).collect();
                        assert_eq!(ids(register.live()), ids(all.clone()));
                        assert_eq!(register.count(), all.len());
                        let (from, len) = (random(&mut seed, 1 << 20), random(&mut seed, 1 << 18));
                        let overlapping: Vec<Arc<Held>> =
                            all.into_iter().filter(|c| from.max(c.start) < (from + len).min(c.start + c.len)).collect();
                        assert_eq!(ids(register.live_in(from, from + len)), ids(overlapping));
                    }
                    _ => {}
                }
            }
        }
    }

    /// Changes from other threads while the submission takes: nothing
    /// changed is lost, and nothing is taken twice.
    #[test]
    fn a_change_beside_the_taking_is_taken_once() {
        let register = Arc::new(Register::<Held>::new());
        let copies: Vec<Arc<Held>> = (0..64)
            .map(|id| Arc::new(Held { id, start: 0, len: 1, mark: Mark::default(), dirty: Mutex::default() }))
            .collect();
        for c in &copies {
            register.add(c, 0, 1, &c.mark);
        }
        let changers: Vec<_> = (0..4u32)
            .map(|t| {
                let (register, copies) = (register.clone(), copies.clone());
                std::thread::spawn(move || {
                    let mut seed = u64::from(t);
                    for i in 0..20_000u32 {
                        let at = random(&mut seed, copies.len());
                        copies[at].change(&register, t * 1_000_000 + i);
                    }
                })
            })
            .collect();
        let mut taken = Vec::new();
        while changers.iter().any(|c| !c.is_finished()) {
            register.take_touched(|copy| taken.extend(copy.taken().map(|t| t.1).unwrap_or_default()));
        }
        changers.into_iter().for_each(|c| c.join().unwrap());
        register.take_touched(|copy| taken.extend(copy.taken().map(|t| t.1).unwrap_or_default()));
        taken.sort_unstable();
        let all: Vec<u32> = (0..4u32).flat_map(|t| (0..20_000u32).map(move |i| t * 1_000_000 + i)).collect();
        assert_eq!(taken, all);
    }

    /// The micro-benchmark (`cargo test --release -p victory-gpu --lib -- --nocapture
    /// carded::tests::what_taking_costs`): 1,000 copies in the card's memory,
    /// a few touched between takings, as at a submission; the walk against
    /// the register. Results checked equal.
    #[test]
    fn what_taking_costs() {
        let release = !cfg!(debug_assertions);
        let rounds = if release { 20_000 } else { 200 };
        for touched in [0usize, 8, 64] {
            let register = Register::<Held>::new();
            let mut old: Vec<Weak<Held>> = Vec::new();
            let mk = |id| Arc::new(Held { id, start: 0, len: 1, mark: Mark::default(), dirty: Mutex::default() });
            let held: Vec<(Arc<Held>, Arc<Held>)> = (0..1000).map(|id| (mk(id), mk(id))).collect();
            for (a, b) in &held {
                register.add(a, 0, 1, &a.mark);
                old.push(Arc::downgrade(b));
            }
            let mut seed = 7u64;
            let (mut walked, mut registered) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
            for round in 0..rounds {
                for _ in 0..touched {
                    let at = random(&mut seed, held.len());
                    held[at].0.change(&register, round);
                    held[at].1.dirty.lock().unwrap().push(round);
                    held[at].1.mark.touched.store(true, Ordering::SeqCst);
                }
                let began = std::time::Instant::now();
                let want = walk(&mut old);
                walked += began.elapsed();
                let began = std::time::Instant::now();
                let mut got = Vec::new();
                register.take_touched(|copy| got.extend(copy.taken()));
                registered += began.elapsed();
                assert_eq!(got, want);
            }
            println!(
                "1,000 copies in the card's memory, {touched} touched a taking: the walk {:.2} us a taking, the register {:.2} us",
                walked.as_nanos() as f64 / 1e3 / rounds as f64,
                registered.as_nanos() as f64 / 1e3 / rounds as f64
            );
        }
    }
}
