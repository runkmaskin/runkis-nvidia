//! What asking about written memory costs on this PC, the ways the layer
//! could ask (`cargo test --release -p victory-gpu --lib -- --ignored --nocapture
//! watch_bench`). The benchmark is ignored by default: it takes 256 MiB and
//! installs a vectored handler for the guard pages' part; the test of the
//! gap between ranges asked together is not.
//!
//! The region is write-watched memory as the title's allocator gives
//! (`MEM_WRITE_WATCH`), written as a frame writes it: 7,000 pages scattered
//! over it, as the title writes about 7,000 a frame, and some dense runs.
//! Every way of asking must find the same pages.

use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::System::Memory::{
    GetWriteWatch, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READONLY, PAGE_READWRITE, ResetWriteWatch,
    VirtualAlloc, VirtualFree, VirtualProtect,
};

const PAGE: usize = 4096;
const SIZE: usize = 256 << 20;
const PAGES: usize = SIZE / PAGE;
const WRITE_WATCH_FLAG_RESET: u32 = 1;
/// (Not in the bindings.)
const MEM_WRITE_WATCH: u32 = 0x0020_0000;

/// The pages a frame writes: scattered ones and dense runs, fixed seed.
fn pattern() -> Vec<usize> {
    let mut seed = 0x77a1_c0de_u64;
    let mut next = |n: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize % n
    };
    let mut pages: Vec<usize> = (0..6000).map(|_| next(PAGES)).collect();
    for _ in 0..10 {
        let at = next(PAGES - 100);
        pages.extend(at..at + 100);
    }
    pages.sort_unstable();
    pages.dedup();
    pages
}

fn write(base: *mut u8, pages: &[usize]) {
    for &p in pages {
        // SAFETY: inside the committed region.
        unsafe { base.add(p * PAGE).write_volatile(p as u8) };
    }
}

/// Written pages of `[first, last)` by one call, with or without the reset
/// flag, into `found`.
fn ask(base: *mut u8, first: usize, last: usize, flags: u32, found: &mut Vec<usize>, out: &mut Vec<usize>) {
    let mut count = last - first;
    if found.len() < count {
        found.resize(count, 0);
    }
    let mut granularity = 0;
    // SAFETY: the answer buffer holds `count` entries; the range is ours.
    let failed = unsafe {
        GetWriteWatch(flags, base.add(first * PAGE).cast(), (last - first) * PAGE, found.as_mut_ptr().cast(), &mut count, &mut granularity)
    } != 0;
    assert!(!failed);
    out.extend(found[..count].iter().map(|&a| (a - base as usize) / PAGE));
}

fn best(rounds: usize, mut f: impl FnMut() -> std::time::Duration) -> f64 {
    (0..rounds).map(|_| f().as_nanos() as f64 / 1e3).fold(f64::MAX, f64::min)
}

static REGION: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static FAULTS: AtomicUsize = AtomicUsize::new(0);

/// Guard pages as the layer's guards have them: read-only, the first write
/// faults, the page is opened and noted written.
unsafe extern "system" fn opened(info: *mut windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system's record of the fault.
    let record = unsafe { &*(*info).ExceptionRecord };
    let (lo, hi) = (REGION[0].load(Ordering::Relaxed), REGION[1].load(Ordering::Relaxed));
    let at = record.ExceptionInformation[1];
    if record.ExceptionCode != windows_sys::Win32::Foundation::STATUS_ACCESS_VIOLATION || at < lo || at >= hi {
        return 0; // EXCEPTION_CONTINUE_SEARCH
    }
    let mut old = 0;
    // SAFETY: a page of our region.
    unsafe { VirtualProtect((at & !(PAGE - 1)) as *const _, PAGE, PAGE_READWRITE, &mut old) };
    FAULTS.fetch_add(1, Ordering::Relaxed);
    -1 // EXCEPTION_CONTINUE_EXECUTION
}

#[test]
#[ignore]
fn what_asking_costs() {
    let rounds = if cfg!(debug_assertions) { 3 } else { 15 };
    // SAFETY: a fresh region of ours.
    let base = unsafe { VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) }.cast::<u8>();
    assert!(!base.is_null());
    // Every page there (committed and touched), then nothing written.
    // SAFETY: the region.
    unsafe {
        std::ptr::write_bytes(base, 1, SIZE);
        ResetWriteWatch(base.cast(), SIZE);
    }
    let pattern = pattern();
    let mut found = Vec::new();
    let mut out = Vec::new();
    let mut table: Vec<(String, f64)> = Vec::new();

    // A: one call over all of it, reset by the call (what the layer does).
    let a = best(rounds, || {
        write(base, &pattern);
        out.clear();
        let t = std::time::Instant::now();
        ask(base, 0, PAGES, WRITE_WATCH_FLAG_RESET, &mut found, &mut out);
        t.elapsed()
    });
    assert_eq!(out, pattern);
    table.push(("one call, 256 MiB, reset in the call".into(), a));

    // B: the same without the flag, then ResetWriteWatch.
    let b = best(rounds, || {
        write(base, &pattern);
        out.clear();
        let t = std::time::Instant::now();
        ask(base, 0, PAGES, 0, &mut found, &mut out);
        // SAFETY: the region.
        unsafe { ResetWriteWatch(base.cast(), SIZE) };
        t.elapsed()
    });
    assert_eq!(out, pattern);
    table.push(("one call without the flag + ResetWriteWatch".into(), b));

    // C: 64 calls of 4 MiB each.
    let c = best(rounds, || {
        write(base, &pattern);
        out.clear();
        let t = std::time::Instant::now();
        for piece in 0..64 {
            ask(base, piece * PAGES / 64, (piece + 1) * PAGES / 64, WRITE_WATCH_FLAG_RESET, &mut found, &mut out);
        }
        t.elapsed()
    });
    assert_eq!(out, pattern);
    table.push(("64 calls of 4 MiB, reset in each".into(), c));

    // D: 4,096 calls of 64 KiB (a call per small copy, as before ask-once).
    let d = best(rounds, || {
        write(base, &pattern);
        out.clear();
        let t = std::time::Instant::now();
        for piece in 0..4096 {
            ask(base, piece * PAGES / 4096, (piece + 1) * PAGES / 4096, WRITE_WATCH_FLAG_RESET, &mut found, &mut out);
        }
        t.elapsed()
    });
    assert_eq!(out, pattern);
    table.push(("4,096 calls of 64 KiB, reset in each".into(), d));

    // E: only what is needed: a tenth of the region (26 ranges of 1 MiB the
    // submission refreshes), the rest not asked.
    let live: Vec<(usize, usize)> = (0..26).map(|i| (i * PAGES / 26, i * PAGES / 26 + 256)).collect();
    let want: Vec<usize> = pattern.iter().copied().filter(|&p| live.iter().any(|&(a, b)| a <= p && p < b)).collect();
    let e = best(rounds, || {
        write(base, &pattern);
        out.clear();
        let t = std::time::Instant::now();
        for &(a, b) in &live {
            ask(base, a, b, WRITE_WATCH_FLAG_RESET, &mut found, &mut out);
        }
        let took = t.elapsed();
        // (The rest reset for the next round, not timed.)
        // SAFETY: the region.
        unsafe { ResetWriteWatch(base.cast(), SIZE) };
        took
    });
    assert_eq!(out, want);
    table.push(("only 26 MiB that steps need, 26 calls".into(), e));

    // F: one call over all of it with nothing written.
    let f = best(rounds, || {
        out.clear();
        let t = std::time::Instant::now();
        ask(base, 0, PAGES, WRITE_WATCH_FLAG_RESET, &mut found, &mut out);
        t.elapsed()
    });
    assert!(out.is_empty());
    table.push(("one call, 256 MiB, nothing written".into(), f));

    // G: guard pages: the region read-only, each first write a fault that
    // opens its page. The cost is the protecting plus the faults, paid by
    // the writing threads (the title's), and the pages written are those that
    // faulted.
    REGION[0].store(base as usize, Ordering::Relaxed);
    REGION[1].store(base as usize + SIZE, Ordering::Relaxed);
    // SAFETY: a handler that only acts on our region.
    let handler = unsafe { windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler(1, Some(opened)) };
    assert!(!handler.is_null());
    let (mut protect_us, mut fault_us) = (f64::MAX, f64::MAX);
    for _ in 0..rounds {
        let mut old = 0;
        let t = std::time::Instant::now();
        // SAFETY: the region.
        unsafe { VirtualProtect(base.cast(), SIZE, PAGE_READONLY, &mut old) };
        protect_us = protect_us.min(t.elapsed().as_nanos() as f64 / 1e3);
        FAULTS.store(0, Ordering::Relaxed);
        let t = std::time::Instant::now();
        write(base, &pattern);
        fault_us = fault_us.min(t.elapsed().as_nanos() as f64 / 1e3);
        assert_eq!(FAULTS.load(Ordering::Relaxed), pattern.len());
    }
    // SAFETY: ours.
    unsafe { windows_sys::Win32::System::Diagnostics::Debug::RemoveVectoredExceptionHandler(handler) };
    table.push(("guards: 256 MiB made read-only".into(), protect_us));
    table.push((format!("guards: the {} first writes, each a fault", pattern.len()), fault_us));

    println!("{} pages written of {PAGES} (256 MiB); best of {rounds} rounds:", pattern.len());
    for (what, us) in &table {
        println!("  {what:<55} {us:>10.0} us");
    }
    // SAFETY: the region is ours and nothing refers to it any more.
    unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
}

/// `ask_ahead`'s runs as its loop made them before they had a function.
fn old_runs(pieces: &[(usize, usize)]) -> Vec<std::ops::Range<usize>> {
    let (gap, most) = (crate::watch::AHEAD_GAP, (1usize << 30) / PAGE);
    let mut runs = Vec::new();
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len() && pieces[k].0 <= last + gap && last.max(pieces[k].1) - first <= most {
            last = last.max(pieces[k].1);
            k += 1;
        }
        runs.push(i..k);
        i = k;
    }
    runs
}

/// Fixed-seed pieces: the default gap makes the runs the loop made; and on a
/// write-watched region, whatever the gap (`VICTORY_AHEAD_GAP_KIB`), asking
/// the runs finds every written page of every piece, the same set.
#[test]
fn a_smaller_gap_misses_no_write() {
    const SMALL: usize = 64 << 20;
    let mut seed = 0x9a95_u64;
    let mut next = |n: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as usize % n
    };
    // SAFETY: a fresh region of ours.
    let base = unsafe { VirtualAlloc(std::ptr::null(), SMALL, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) }.cast::<u8>();
    assert!(!base.is_null());
    let mut found = Vec::new();
    for round in 0..40 {
        let mut pieces: Vec<(usize, usize)> = (0..1 + next(60))
            .map(|_| {
                let a = next(SMALL / PAGE - 600);
                (a, a + 1 + next(512))
            })
            .collect();
        pieces.sort_unstable();
        assert_eq!(crate::watch::runs_of(&pieces, crate::watch::AHEAD_GAP), old_runs(&pieces), "round {round}");
        let written: Vec<usize> = (0..2000).map(|_| next(SMALL / PAGE)).collect();
        let mut want: Vec<usize> = written.iter().copied().filter(|&p| pieces.iter().any(|&(a, b)| a <= p && p < b)).collect();
        want.sort_unstable();
        want.dedup();
        for gap_kib in [0usize, 256, 2048, 8192] {
            // SAFETY: the region.
            unsafe { ResetWriteWatch(base.cast(), SMALL) };
            write(base, &written);
            let mut out = Vec::new();
            for run in crate::watch::runs_of(&pieces, gap_kib * 1024 / PAGE) {
                let first = pieces[run.clone()].iter().map(|p| p.0).min().unwrap();
                let last = pieces[run.clone()].iter().map(|p| p.1).max().unwrap();
                let mut got = Vec::new();
                ask(base, first, last, WRITE_WATCH_FLAG_RESET, &mut found, &mut got);
                out.extend(got.into_iter().filter(|&p| pieces[run.clone()].iter().any(|&(a, b)| a <= p && p < b)));
            }
            out.sort_unstable();
            out.dedup();
            assert_eq!(out, want, "round {round}, gap {gap_kib} KiB");
        }
    }
    // SAFETY: ours, nothing refers to it.
    unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) };
}