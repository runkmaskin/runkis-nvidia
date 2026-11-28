//! A submission of joined parts cut ahead of its wait for landings
//! (`VICTORY_SUBMIT_PIPELINE=1`; off by default).
//!
//! With the relay's second stage a submission whose CPU steps read title
//! memory that flights in the air may write, and whose steps the card cannot
//! take ([`card_steps`]), waits until those flights have landed. Measured in
//! the city (RTX 4070 SUPER, Series S 1440p, 2026-11-28): 0.2 such waits a
//! frame, refused for [`card_steps`]' "a copy at its place in a list writes
//! what it reads", 4 to 14 ms of each frame of the submitting thread. When
//! the flights it waits for are the last ones queued, the card runs dry
//! during their landing and the preparing behind it ("the card idle behind
//! us", 5 to 8 ms a frame).
//!
//! Such a submission is often parts the line joined (`VICTORY_JOIN_PARTS`).
//! With the switch, its front parts that read nothing flights write go to the
//! card at once, as a submission of their own, and only the rest waits. The
//! card works on the front while the thread waits.
//!
//! Why the bytes are the same as without joining (`VICTORY_JOIN_PARTS=1`):
//! the cut is at a place where one of the title's submissions began, so each
//! side is what the line would have sent without joining them. The front
//! reads nothing in the air writes, so it reads the same bytes now as after
//! the wait. The rest is joinable behind the front ([`joinable`], checked
//! again here): none of its CPU steps reads what the front's shaders may
//! write, write back or write when done. So it reads the same bytes after
//! the same landings. A front with a copy at its place ([`place`]) is not
//! cut: that copy's destination is not among what joinable looks at.
//!
//! What differs from the joined submission: two submissions for one, so the
//! write watch is asked twice and the rest refreshes its own ranges, as
//! unjoined parts do.

use super::*;

/// Whether it is on (needs the relay's second stage, whose waits it cuts).
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| relay_exact() && crate::switch::on("VICTORY_SUBMIT_PIPELINE"))
}

/// Since the last frame-rate line: submissions cut, lists sent ahead of a
/// wait, waits of submissions of parts that could not be cut, waits of
/// submissions of one part.
static STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Of `parts` (places in `runs` where a part begins, ascending), the last
/// one ahead of which nothing waits for flights in the air, if the rest can
/// go behind it. Under the submission lock.
pub(super) fn cut(runs: &[&HostList], parts: &[usize]) -> Option<usize> {
    if parts.is_empty() {
        return None;
    }
    let (written, copies) = {
        let line = line();
        (line.written.clone(), card_steps::copies(&line))
    };
    let at = last_free(parts, runs.len(), |p| {
        let front = &runs[..p];
        !placed_in(front) && blocked_by(&written, front, false).is_none() && !card_steps::copied_read(&copies, front)
    })?;
    joinable(runs[..at].iter().copied(), &runs[at..]).then_some(at)
}

/// Whether one of `lists` has a copy at its place.
fn placed_in(lists: &[&HostList]) -> bool {
    lists.iter().any(|l| l.deferred().copies.iter().any(|s| matches!(s, Step::Placed(_))))
}

/// The last of `parts` (ascending, inside `1..len`) for which `free` holds
/// of everything ahead of it, asked in order until it does not: a longer
/// front holds everything a shorter one does.
fn last_free(parts: &[usize], len: usize, mut free: impl FnMut(usize) -> bool) -> Option<usize> {
    let mut found = None;
    for &p in parts {
        if p == 0 || p >= len {
            continue;
        }
        if !free(p) {
            break;
        }
        found = Some(p);
    }
    found
}

/// A cut was made and sent `lists` ahead.
pub(super) fn cut_made(lists: usize) {
    STATS[0].fetch_add(1, Ordering::Relaxed);
    STATS[1].fetch_add(lists as u64, Ordering::Relaxed);
}

/// A submission waits all the same.
pub(super) fn not_cut(parts: &[usize]) {
    STATS[if parts.is_empty() { 3 } else { 2 }].fetch_add(1, Ordering::Relaxed);
}

pub(super) fn log(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "submissions cut ahead of their wait for landings per frame (VICTORY_SUBMIT_PIPELINE=1): {:.2}, {:.1} lists sent \
         ahead; waited all the same: {:.2} of joined parts (the first part reads what is in the air), {:.2} of one part",
        s[0],
        s[1],
        s[2],
        s[3],
    );
}

#[cfg(test)]
mod tests {
    use super::last_free;

    #[test]
    fn the_last_free_part_is_taken_and_the_look_stops_at_the_first_that_is_not() {
        let mut asked = Vec::new();
        let got = last_free(&[2, 4, 6, 8], 10, |p| {
            asked.push(p);
            p < 6
        });
        assert_eq!(got, Some(4));
        assert_eq!(asked, [2, 4, 6]);
        // Nothing free; places out of range are passed over.
        assert_eq!(last_free(&[2, 4], 10, |_| false), None);
        assert_eq!(last_free(&[0, 10, 12], 10, |_| true), None);
        assert_eq!(last_free(&[], 10, |_| true), None);
        assert_eq!(last_free(&[3], 10, |_| true), Some(3));
    }

    /// A model of title memory, flights and parts: what every part's CPU
    /// steps read is the same (byte for byte) when the submission is cut as
    /// the switch cuts it, when it is sent joined and waits (off), and when
    /// each part goes on its own (`VICTORY_JOIN_PARTS=1`). Parts are made
    /// joinable as the line joins them: no part reads what one ahead of it
    /// writes. Fixed seeds.
    #[test]
    fn a_cut_reads_what_the_joined_and_the_unjoined_submissions_read() {
        const CELLS: usize = 48;
        type R = (usize, usize);
        fn meet(a: R, b: R) -> bool {
            a.0 < b.1 && b.0 < a.1
        }
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
            fn below(&mut self, n: usize) -> usize {
                (self.next() % n as u64) as usize
            }
            fn range(&mut self) -> R {
                let a = self.below(CELLS);
                (a, (a + 1 + self.below(6)).min(CELLS))
            }
        }
        // A flight: what it writes, and the value it writes there.
        #[derive(Clone)]
        struct Flight(R, u32);
        struct Part {
            reads: Vec<R>,
            writes: R,
        }
        // The line: memory and flights in order; landing lands in order up
        // to the last flight that writes one of `reads`.
        struct World {
            memory: [u32; CELLS],
            air: Vec<Flight>,
            next: u32,
        }
        impl World {
            fn blocked(&self, reads: &[R]) -> bool {
                self.air.iter().any(|f| reads.iter().any(|&r| meet(f.0, r)))
            }
            fn wait_for(&mut self, reads: &[R]) {
                if let Some(last) = self.air.iter().rposition(|f| reads.iter().any(|&r| meet(f.0, r))) {
                    for f in self.air.drain(..=last) {
                        self.memory[f.0.0..f.0.1].iter_mut().for_each(|m| *m = f.1);
                    }
                }
            }
            fn read(&self, reads: &[R]) -> Vec<u32> {
                reads.iter().flat_map(|r| self.memory[r.0..r.1].iter().copied()).collect()
            }
            fn send(&mut self, writes: R) {
                self.next += 1;
                self.air.push(Flight(writes, self.next));
            }
            // A submission of `parts`, waiting as one: reads of all, then sends.
            fn joined(&mut self, parts: &[Part], out: &mut Vec<Vec<u32>>) {
                let all: Vec<R> = parts.iter().flat_map(|p| p.reads.iter().copied()).collect();
                self.wait_for(&all);
                out.extend(parts.iter().map(|p| self.read(&p.reads)));
                for p in parts {
                    self.send(p.writes);
                }
            }
        }
        for seed in 1..=4000u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let flights: Vec<Flight> = (0..rng.below(4)).map(|i| Flight(rng.range(), 1000 + i as u32)).collect();
            let mut parts: Vec<Part> = Vec::new();
            for _ in 0..1 + rng.below(5) {
                // Joinable: reads nothing a part ahead writes.
                let reads: Vec<R> = (0..1 + rng.below(3))
                    .map(|_| rng.range())
                    .filter(|&r| parts.iter().all(|p| !meet(p.writes, r)))
                    .collect();
                parts.push(Part { reads, writes: rng.range() });
            }
            let start = World { memory: std::array::from_fn(|i| i as u32), air: flights.clone(), next: 0 };
            let fresh = || World { memory: start.memory, air: start.air.clone(), next: 0 };
            // Off: joined, waiting as one.
            let mut off = Vec::new();
            fresh().joined(&parts, &mut off);
            // Unjoined: each part on its own.
            let mut each = Vec::new();
            let mut w = fresh();
            for p in &parts {
                w.joined(std::slice::from_ref(p), &mut each);
            }
            // The switch: cut where the front reads nothing in the air, again
            // and again for what is left (as `execute_parts` does).
            let mut cut = Vec::new();
            let mut w = fresh();
            let mut rest: &[Part] = &parts;
            let mut cuts = 0;
            while !rest.is_empty() {
                let places: Vec<usize> = (1..rest.len()).collect();
                let reads = |n: usize| -> Vec<R> { rest[..n].iter().flat_map(|p| p.reads.iter().copied()).collect() };
                let at = if w.blocked(&reads(rest.len())) { last_free(&places, rest.len(), |p| !w.blocked(&reads(p))) } else { None };
                match at {
                    Some(at) => {
                        // The front goes without a wait.
                        let before = w.air.len();
                        w.joined(&rest[..at], &mut cut);
                        assert_eq!(w.air.len(), before + at, "seed {seed}: the front waited");
                        rest = &rest[at..];
                        cuts += 1;
                    }
                    None => {
                        w.joined(rest, &mut cut);
                        rest = &[];
                    }
                }
            }
            assert_eq!(cut, off, "seed {seed}: the cut against off");
            assert_eq!(cut, each, "seed {seed}: the cut against parts on their own");
            assert!(cuts <= parts.len());
        }
    }
}
