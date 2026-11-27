//! The heap pass's bookkeeping ([`super::KeptOf::pass`] and
//! [`super::KeptOf::spans`]) against the code it replaced, kept here as the
//! reference: every slot forgotten or set as before, and the spans made
//! from all slots at every pass.

use super::{HEAP_ROUND, HeapRanges, KeptOf, Pass};
use std::sync::Arc;

/// A fixed-seed source of numbers (xorshift64*).
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

type HostCopy = Arc<u64>;
type Range = (HostCopy, u64, u64);

fn key(copy: &HostCopy) -> usize {
    Arc::as_ptr(copy) as usize
}

/// The pass as it was: the part forgotten, every slot looked at set, the
/// spans made from all slots. The spans in the order the pass gives them.
fn old_pass(
    ranges: &mut HeapRanges<HostCopy>,
    passes: &mut u64,
    spread: bool,
    title: &[Option<Range>],
    changed: Vec<u32>,
) -> (bool, usize, Vec<(usize, u64, u64)>, u32) {
    let buffers = || (0..title.len() as u32).filter(|&i| title[i as usize].is_some());
    let all = *passes == 0 || (passes.is_multiple_of(HEAP_ROUND) && !spread);
    let slots: Vec<u32> = if all {
        ranges.0.clear();
        buffers().collect()
    } else if spread {
        let (part, parts) = ((*passes % HEAP_ROUND) as u32, HEAP_ROUND as u32);
        ranges.0.iter_mut().skip(part as usize).step_by(parts as usize).for_each(|r| *r = None);
        let mut slots: Vec<u32> = buffers().filter(|slot| slot % parts == part).collect();
        slots.extend(changed);
        slots
    } else {
        changed
    };
    *passes += 1;
    for &index in &slots {
        ranges.set(index, title[index as usize].clone());
    }
    let mut spans: crate::host::ByCopy<Range> = Default::default();
    let mut views = 0u32;
    for (copy, from, to) in ranges.ranges() {
        views += 1;
        let span = spans.entry(key(copy)).or_insert_with(|| (copy.clone(), *from, *to));
        span.1 = span.1.min(*from);
        span.2 = span.2.max(*to);
    }
    (all, slots.len(), spans.values().map(|(c, f, t)| (key(c), *f, *t)).collect(), views)
}

/// The pass as it is now; and whether its spans were those kept.
fn new_pass(kept: &mut KeptOf<HostCopy>, spread: bool, title: &[Option<Range>], changed: Vec<u32>) -> (bool, usize, Vec<(usize, u64, u64)>, u32, bool) {
    let buffers = || (0..title.len() as u32).filter(|&i| title[i as usize].is_some());
    let Pass { all, slots, moved, forgotten } = kept.pass(
        spread,
        || buffers().collect(),
        |part, parts| buffers().filter(|slot| slot % parts == part).collect(),
        changed,
        |slots| slots.iter().map(|&i| (i, title[i as usize].clone())).collect(),
        Arc::ptr_eq,
    );
    drop(forgotten);
    let (spans, views) = kept.spans(moved, key);
    (all, slots, spans.values().map(|(c, f, t)| (key(c), *f, *t)).collect(), views, !moved)
}

/// The title's buffer slots between passes: some written and told of
/// (`changed`), some changed untold (a view of a released buffer goes
/// stale without a write), some no buffer any more, copies made anew.
fn step(rng: &mut Seeded, title: &mut [Option<Range>], copies: &mut [HostCopy], quiet: bool) -> Vec<u32> {
    let mut changed = Vec::new();
    if quiet {
        return changed;
    }
    let slots = title.len() as u64;
    let range = |rng: &mut Seeded, copies: &[HostCopy]| {
        let copy = copies[rng.below(copies.len() as u64) as usize].clone();
        let from = rng.below(1 << 20) & !15;
        (copy, from, from + 16 + (rng.below(64) << 4))
    };
    for _ in 0..rng.below(40) {
        let slot = rng.below(slots) as u32;
        title[slot as usize] = if rng.below(5) == 0 { None } else { Some(range(rng, copies)) };
        changed.push(slot);
        // Told twice now and then.
        if rng.below(7) == 0 {
            changed.push(slot);
        }
    }
    for _ in 0..rng.below(4) {
        let slot = rng.below(slots) as usize;
        if title[slot].is_some() {
            title[slot] = if rng.below(2) == 0 { None } else { Some(range(rng, copies)) };
        }
    }
    // A copy let go and another made: its slots show the new one.
    if rng.below(10) == 0 {
        let at = rng.below(copies.len() as u64) as usize;
        let old = copies[at].clone();
        copies[at] = Arc::new(rng.next());
        for slot in title.iter_mut().flatten() {
            if Arc::ptr_eq(&slot.0, &old) {
                slot.0 = copies[at].clone();
            }
        }
    }
    changed
}

/// Fixed seeds, both ways of looking again (spread and all at once), passes
/// with no writes between them among those with: every pass gives the same
/// spans in the same order, the same views and slots, and the slots hold
/// the same ranges after it.
#[test]
fn the_kept_spans_are_the_spans_made_again() {
    let mut reused = 0;
    for seed in [1u64, 7, 0x5eed, 0xdead_beef, 42] {
        for spread in [true, false] {
            let mut rng = Seeded(seed);
            let mut copies: Vec<HostCopy> = (0..37).map(|n| Arc::new(n as u64)).collect();
            let slots = 600 + rng.below(400) as usize;
            let mut title: Vec<Option<Range>> = (0..slots)
                .map(|_| {
                    (rng.below(3) != 0).then(|| {
                        let from = rng.below(1 << 20) & !15;
                        (copies[rng.below(37) as usize].clone(), from, from + 64)
                    })
                })
                .collect();
            let (mut old, mut passes) = (HeapRanges(Vec::new()), 0u64);
            let mut new = KeptOf::default();
            for pass in 0..200 {
                let changed = step(&mut rng, &mut title, &mut copies, pass % 3 != 0);
                let was = old_pass(&mut old, &mut passes, spread, &title, changed.clone());
                let (all, n, spans, views, kept) = new_pass(&mut new, spread, &title, changed);
                assert_eq!((all, n, &spans, views), (was.0, was.1, &was.2, was.3), "seed {seed} spread {spread} pass {pass}");
                reused += usize::from(kept);
                let held = |r: &HeapRanges<HostCopy>| r.0.iter().map(|s| s.as_ref().map(|(c, f, t)| (key(c), *f, *t))).collect::<Vec<_>>();
                let (a, b) = (held(&old), held(&new.ranges));
                let len = a.len().max(b.len());
                let pad = |mut v: Vec<_>| {
                    v.resize(len, None);
                    v
                };
                assert_eq!(pad(a), pad(b), "seed {seed} spread {spread} pass {pass}: slots");
            }
        }
    }
    assert!(reused > 100, "the kept spans were taken {reused} times");
}

/// Offline: a pass's bookkeeping over 72,000 slots of 4,400 copies, as the
/// pass made it and as it makes it now: a spread pass of no write (the
/// most common) and one of 40 writes. Run with
/// `cargo test --release -p victory-gpu --lib -- --ignored --nocapture bench_heap_pass_spans`.
#[test]
#[ignore]
fn bench_heap_pass_spans() {
    const SLOTS: usize = 72_000;
    let copies: Vec<HostCopy> = (0..4400).map(Arc::new).collect();
    let title: Vec<Option<Range>> =
        (0..SLOTS).map(|slot| Some((copies[slot * 7919 % 4400].clone(), slot as u64 * 64, slot as u64 * 64 + 64))).collect();
    let mut old = (HeapRanges(Vec::new()), 0u64);
    let mut new = KeptOf::default();
    let _ = old_pass(&mut old.0, &mut old.1, true, &title, Vec::new());
    let _ = new_pass(&mut new, true, &title, Vec::new());
    let best = |f: &mut dyn FnMut()| {
        (0..40)
            .map(|_| {
                let t = std::time::Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e3
            })
            .fold(f64::MAX, f64::min)
    };
    for writes in [0usize, 40] {
        let changed: Vec<u32> = (0..writes).map(|n| (n * 1777 % SLOTS) as u32).collect();
        let was = best(&mut || {
            let _ = old_pass(&mut old.0, &mut old.1, true, &title, changed.clone());
        });
        let now = best(&mut || {
            let _ = new_pass(&mut new, true, &title, changed.clone());
        });
        println!("a spread pass with {writes} writes told that leave the ranges as they were: {was:.3} ms as it was, {now:.3} ms now");
    }
}
