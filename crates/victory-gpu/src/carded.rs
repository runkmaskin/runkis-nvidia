//! The register of copies in the card's memory (`crate::shadow`'s `CARDED`),
//! apart from `Shadow` so that it can be tested alone.
//!
//! Each entry keeps its copy's title range beside the weak reference, so a
//! title buffer copy done on the CPU (`crate::shadow::send_copied`, once for
//! each of the title's buffer copies) upgrades only the copies it reaches,
//! not all of them (some 3,900 in the first gameplay area). Ported from
//! runkis-nvidia old-u0221 eafd20a; the touched list that `card_dirty` walks
//! was upstream's already.

use std::sync::{Arc, Mutex, Weak};

/// The live entries are kept at least this many before the dead are swept.
const LEAST_SWEEP: usize = 1024;

/// Copies (weak), each with its title range `[start, end)`, in the order
/// added.
pub(crate) struct Register<T> {
    entries: Mutex<(Vec<(usize, usize, Weak<T>)>, usize)>,
}

impl<T> Register<T> {
    pub(crate) const fn new() -> Register<T> {
        Register {
            entries: Mutex::new((Vec::new(), 0)),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, (Vec<(usize, usize, Weak<T>)>, usize)> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Adds `copy` of title memory `[start, end)`. The copies let go are
    /// dropped once the register has doubled since that was last done.
    pub(crate) fn add(&self, copy: &Arc<T>, start: usize, end: usize) {
        let mut entries = self.entries();
        let (all, kept) = &mut *entries;
        all.push((start, end, Arc::downgrade(copy)));
        if all.len() >= 2 * (*kept).max(LEAST_SWEEP) {
            all.retain(|e| e.2.strong_count() > 0);
            *kept = all.len();
        }
    }

    /// The live copies, in order.
    pub(crate) fn live(&self) -> Vec<Arc<T>> {
        self.entries()
            .0
            .iter()
            .filter_map(|e| e.2.upgrade())
            .collect()
    }

    /// The live copies whose title range overlaps `[from, to)`, in order:
    /// those of [`Register::live`] that do, without upgrading the others.
    pub(crate) fn live_in(&self, from: usize, to: usize) -> Vec<Arc<T>> {
        self.entries()
            .0
            .iter()
            .filter(|e| e.0 < to && from < e.1)
            .filter_map(|e| e.2.upgrade())
            .collect()
    }

    /// How many copies live.
    pub(crate) fn count(&self) -> usize {
        self.entries()
            .0
            .iter()
            .filter(|e| e.2.strong_count() > 0)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random(seed: &mut u64, n: usize) -> usize {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 33) as usize % n
    }

    /// The copies `send_copied` did something to before: every live one,
    /// those whose range the copy `[dst, dst + len)` does not reach skipped.
    fn reached_by_walk(all: &[Arc<(usize, usize)>], dst: usize, len: usize) -> Vec<usize> {
        all.iter()
            .filter(|c| dst.max(c.0) < (dst + len).min(c.1))
            .map(|c| Arc::as_ptr(c) as usize)
            .collect()
    }

    /// Fixed-seed runs of copies made, let go, and title copies of every
    /// size (none, inside one, across many): the copies the register gives
    /// for a range and that the copy reaches are the walk's, in its order;
    /// the live copies are the walk's.
    #[test]
    fn the_register_gives_what_the_walk_reached() {
        for run in 0..20u64 {
            let mut seed = 0x5eed_0000 + run;
            let register = Register::<(usize, usize)>::new();
            let mut held: Vec<Arc<(usize, usize)>> = Vec::new();
            for step in 0..6000 {
                match random(&mut seed, 8) {
                    0..=2 => {
                        let (start, len) = (random(&mut seed, 1 << 20), random(&mut seed, 1 << 16));
                        let copy = Arc::new((start, start + len));
                        register.add(&copy, copy.0, copy.1);
                        held.push(copy);
                    }
                    3 if !held.is_empty() => {
                        let at = random(&mut seed, held.len());
                        held.remove(at);
                    }
                    4..=6 => {
                        let (dst, len) = (
                            random(&mut seed, 1 << 20),
                            [0, 1, 4096, 1 << 18][random(&mut seed, 4)],
                        );
                        let got: Vec<usize> = register
                            .live_in(dst, dst.saturating_add(len))
                            .iter()
                            .filter(|c| dst.max(c.0) < (dst + len).min(c.1))
                            .map(|c| Arc::as_ptr(c) as usize)
                            .collect();
                        let all: Vec<Arc<(usize, usize)>> = register.live();
                        assert_eq!(
                            got,
                            reached_by_walk(&all, dst, len),
                            "run {run}, step {step}"
                        );
                    }
                    _ => {
                        let ids = |v: &[Arc<(usize, usize)>]| {
                            v.iter()
                                .map(|c| Arc::as_ptr(c) as usize)
                                .collect::<Vec<_>>()
                        };
                        assert_eq!(ids(&register.live()), ids(&held));
                        assert_eq!(register.count(), held.len());
                    }
                }
            }
        }
    }

    /// The micro-benchmark (`cargo test --release -p victory-gpu --lib -- --ignored
    /// --nocapture carded::tests::what_a_title_copy_costs`): 4,000 copies in the
    /// card's memory, title buffer copies of 4 KiB each reaching one or none;
    /// every copy upgraded and looked at against those in range.
    #[test]
    #[ignore]
    fn what_a_title_copy_costs() {
        let register = Register::<(usize, usize)>::new();
        let held: Vec<Arc<(usize, usize)>> = (0..4000)
            .map(|i| Arc::new((i << 16, (i << 16) + 40_000)))
            .collect();
        for c in &held {
            register.add(c, c.0, c.1);
        }
        let mut seed = 7u64;
        let copies: Vec<usize> = (0..10_000).map(|_| random(&mut seed, 4000 << 16)).collect();
        let (mut walked, mut ranged) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        for &dst in &copies {
            let began = std::time::Instant::now();
            let want: Vec<usize> = register
                .live()
                .iter()
                .filter(|c| dst.max(c.0) < (dst + 4096).min(c.1))
                .map(|c| Arc::as_ptr(c) as usize)
                .collect();
            walked += began.elapsed();
            let began = std::time::Instant::now();
            let got: Vec<usize> = register
                .live_in(dst, dst + 4096)
                .iter()
                .filter(|c| dst.max(c.0) < (dst + 4096).min(c.1))
                .map(|c| Arc::as_ptr(c) as usize)
                .collect();
            ranged += began.elapsed();
            assert_eq!(got, want);
        }
        println!(
            "4,000 copies in the card's memory, a title buffer copy of 4 KiB: every copy upgraded {:.2} us, those in range {:.2} us",
            walked.as_nanos() as f64 / 1e3 / copies.len() as f64,
            ranged.as_nanos() as f64 / 1e3 / copies.len() as f64
        );
    }
}
