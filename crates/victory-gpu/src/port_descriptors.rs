//! Renderer port #2: the title's writes of descriptor records into its
//! shader-visible heap say which slots they wrote, and the per-frame look at
//! every changed heap page goes (`VICTORY_PORT_DESCRIPTORS=1`, off by
//! default; read in [`on`]).
//!
//! What the title does (game_scarlett_master.exe; `victory-loader`'s
//! `descriptor_sites` has the code): its placed shader-visible view heap is
//! the memory of its frame allocator (image +0x4d9f400), which hands out the
//! draws' and dispatches' descriptor tables and much else (constants, list
//! nodes). The tables are filled one 32-byte record at a time by six stores
//! in two functions (+0x4d9efd0, +0x4eec8e0); views made by our calls
//! (`Create*View`, `CopyDescriptors*`) are mirrored where they are made
//! (`descriptor::mirror`). What the layer did before: the write watch's
//! changed 64 KiB pages of the heap, every slot on them read and compared
//! with the one last seen (~290 pages, ~600,000 slots, 15-16 ms a frame of
//! the heap-sync thread, 5-7 of it asking the watch, run port1-3).
//!
//! What is done here: the six stores are patched (`descriptor_sites`) to set
//! a byte per slot and per 128 slots in maps of ours ([`MARKS`]), and our own
//! mirroring marks too; `descriptor::sync_heaps` takes the marked slots
//! ([`take`]) in place of the changed pages, and compares and makes only
//! those, as before.
//!
//! Correctness first, the fallback: once a frame a few of the pages the
//! write watch says were written ([`check_budget`], every
//! [`check_every`] frames asked) are looked at whole, as before; a slot found
//! with a new view record that no mark told of is made (so it is right from
//! then on) and counted, with the place in the title's code that last
//! faulted on its 64 KiB block when the guards saw one ([`faulted`]): a writer
//! the patches miss shows in the log. Records that are not views (the
//! allocator's other data) are made as before and counted apart.
//!
//! Assumption: a slot whose view record the title overwrote with other data
//! (not a view) and no mark told of keeps showing its old view on the host
//! until the fallback looks at its page; nothing reads such a slot as a
//! descriptor (the title wrote no descriptor there).

use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering::{AcqRel, Acquire, Relaxed, Release}};

/// Whether renderer port #2 is on.
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // Read once, here: the switch of renderer port #2.
    *ON.get_or_init(|| std::env::var("VICTORY_PORT_DESCRIPTORS").is_ok_and(|v| v.trim() == "1"))
}

/// What the patched stores read (`victory_loader::descriptor_sites`,
/// `MARKS_LEN` bytes): the heap's first byte, its length (0: none), the map
/// of slots, the map of chunks of [`CHUNK`] slots.
#[repr(C)]
pub struct Marks {
    start: AtomicU64,
    len: AtomicU64,
    slots: AtomicPtr<AtomicU8>,
    chunks: AtomicPtr<AtomicU8>,
}

const _: () = assert!(size_of::<Marks>() == 32);

pub static MARKS: Marks = Marks {
    start: AtomicU64::new(0),
    len: AtomicU64::new(0),
    slots: AtomicPtr::new(std::ptr::null_mut()),
    chunks: AtomicPtr::new(std::ptr::null_mut()),
};

/// Slots a byte of the map of chunks stands for (4 KiB of records).
const CHUNK: usize = 128;

/// Where the stores' cells are, for `descriptor_sites::apply`.
pub fn marks_address() -> usize {
    std::ptr::from_ref(&MARKS) as usize
}

/// The title's image (base, size) and how many of its stores were patched.
static IMAGE: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

/// Told by the launcher: `patched` of the title's record stores in the image
/// at `base` (`size` bytes) mark their slots.
pub fn hooked(base: usize, size: usize, patched: usize) {
    IMAGE[0].store(base, Relaxed);
    IMAGE[1].store(size, Relaxed);
    IMAGE[2].store(patched, Relaxed);
}

/// Whether [`take`] stands for the heap's changed pages: the switch is on and
/// the stores were patched.
fn working() -> bool {
    on() && IMAGE[2].load(Relaxed) > 0
}

/// The heap whose slots are marked: its slots, and the places in the title's
/// code that last faulted on each of its 64 KiB blocks.
struct Adopted {
    start: u64,
    count: usize,
    slots: &'static [AtomicU8],
    chunks: &'static [AtomicU8],
    writers: &'static [AtomicUsize],
}

static ADOPTED: std::sync::RwLock<Option<Adopted>> = std::sync::RwLock::new(None);

/// A placed shader-visible view heap of `count` records at `start`: the
/// first such is the one whose slots are marked. Whether it is.
pub(crate) fn adopt(start: u64, count: u32) -> bool {
    working() && adopt_now(start, count)
}

fn adopt_now(start: u64, count: u32) -> bool {
    let mut adopted = ADOPTED.write().unwrap_or_else(|e| e.into_inner());
    if adopted.is_some() {
        tracing::info!(start = format_args!("{start:#x}"), count, "renderer port 2: another placed view heap, looked at by its pages as before");
        return false;
    }
    let count = count as usize;
    let leak = |n: usize| -> &'static [AtomicU8] { Box::leak((0..n).map(|_| AtomicU8::new(0)).collect()) };
    // (Never freed: a store of the title's may be marking while the heap
    // goes; a few MB at most.)
    let slots = leak(count);
    let chunks = leak(count.div_ceil(CHUNK));
    let writers: &'static [AtomicUsize] = Box::leak((0..(count * 32).div_ceil(1 << 16)).map(|_| AtomicUsize::new(0)).collect());
    MARKS.slots.store(slots.as_ptr().cast_mut(), Release);
    MARKS.chunks.store(chunks.as_ptr().cast_mut(), Release);
    MARKS.start.store(start, Release);
    // Last: a store that sees a length sees the rest.
    MARKS.len.store(count as u64 * 32, Release);
    *adopted = Some(Adopted { start, count, slots, chunks, writers });
    tracing::info!(
        start = format_args!("{start:#x}"),
        count,
        stores = IMAGE[2].load(Relaxed),
        "renderer port 2: the title's view heap's slots are told by its record stores and our calls, not looked for on its pages (VICTORY_PORT_DESCRIPTORS)"
    );
    true
}

/// The heap at `start` goes: nothing more is marked for it.
pub(crate) fn let_go(start: u64) {
    let mut adopted = ADOPTED.write().unwrap_or_else(|e| e.into_inner());
    if adopted.as_ref().is_some_and(|a| a.start == start) {
        MARKS.len.store(0, Release);
        *adopted = None;
        tracing::info!(start = format_args!("{start:#x}"), "renderer port 2: the marked view heap went");
    }
}

/// Whether the heap at `start` is the one whose slots are marked.
pub(crate) fn adopted(start: u64) -> bool {
    ADOPTED.read().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|a| a.start == start)
}

/// Marks the slot at `handle` (a record our own calls wrote), if it is in the
/// marked heap.
#[inline]
pub(crate) fn mark(handle: u64) {
    let len = MARKS.len.load(Acquire);
    if len == 0 {
        return;
    }
    let at = handle.wrapping_sub(MARKS.start.load(Relaxed));
    if at >= len {
        return;
    }
    let slot = (at / 32) as usize;
    // SAFETY: the maps are leaked, as long as the length says.
    unsafe {
        (*MARKS.slots.load(Relaxed).add(slot)).store(1, Release);
        (*MARKS.chunks.load(Relaxed).add(slot / CHUNK)).store(1, Release);
    }
    MARKED_BY_US.fetch_add(1, Relaxed);
}

/// Whether slot `index`'s mark is set (told, not yet taken).
fn marked(a: &Adopted, index: u32) -> bool {
    a.slots.get(index as usize).is_some_and(|s| s.load(Acquire) != 0)
}

/// The marked slots of the heap at `start`, each mark cleared, as runs of
/// slots `(first, count)`; `None` if it is not the marked heap.
pub(crate) fn take(start: u64) -> Option<Vec<(u32, u32)>> {
    let began = std::time::Instant::now();
    let adopted = ADOPTED.read().unwrap_or_else(|e| e.into_inner());
    let a = adopted.as_ref().filter(|a| a.start == start)?;
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut n = 0u64;
    for (c, chunk) in a.chunks.iter().enumerate() {
        if chunk.load(Relaxed) == 0 {
            continue;
        }
        // Cleared before its slots are read: a mark set meanwhile sets it again.
        chunk.swap(0, AcqRel);
        let first = c * CHUNK;
        for (i, slot) in a.slots[first..(first + CHUNK).min(a.count)].iter().enumerate() {
            if slot.load(Relaxed) == 0 || slot.swap(0, AcqRel) == 0 {
                continue;
            }
            let index = (first + i) as u32;
            n += 1;
            match runs.last_mut() {
                Some((at, len)) if *at + *len == index => *len += 1,
                _ => runs.push((index, 1)),
            }
        }
    }
    TAKEN[0].fetch_add(n, Relaxed);
    TAKEN[1].fetch_add(1, Relaxed);
    TAKEN[2].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
    Some(runs)
}

/// A write fault of the title's at `at` (from its instruction at `rip`),
/// if in the marked heap: who wrote its block last, for [`missed`].
pub(crate) fn faulted(at: usize, rip: usize) {
    if MARKS.len.load(Relaxed) == 0 {
        return;
    }
    let Ok(adopted) = ADOPTED.try_read() else { return };
    if let Some(a) = adopted.as_ref() {
        let off = (at as u64).wrapping_sub(a.start);
        if let Some(w) = a.writers.get((off >> 16) as usize) {
            w.store(rip, Relaxed);
        }
    }
}

/// Frames between askings of the write watch for the fallback
/// (`VICTORY_PORT_DESCRIPTORS_EVERY=<frames>`, 8 if not given).
fn check_every() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_PORT_DESCRIPTORS_EVERY").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8u64).max(1))
}

/// Pages the fallback looks at a frame (`VICTORY_PORT_DESCRIPTORS_CHECK=<pages>`,
/// 16 if not given; 0: none).
fn check_budget() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("VICTORY_PORT_DESCRIPTORS_CHECK").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(16usize))
}

/// The fallback's pages: those the watch said were written, not looked at yet;
/// the frame it last asked in, and the frame it last looked in.
struct Fallback {
    pages: std::collections::BTreeSet<usize>,
    asked: u64,
    looked: u64,
    /// The page it goes on from (round the heap).
    next: usize,
}

static FALLBACK: std::sync::Mutex<Fallback> = std::sync::Mutex::new(Fallback { pages: std::collections::BTreeSet::new(), asked: 0, looked: 0, next: 0 });

/// Whether to ask the write watch of the marked heap this frame.
pub(crate) fn ask_due(frame: u64) -> bool {
    let mut f = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    if check_budget() == 0 || f.asked != 0 && frame < f.asked + check_every() {
        return false;
    }
    f.asked = frame.max(1);
    true
}

/// What the watch said was written (byte ranges), for later.
pub(crate) fn asked(ranges: &[(usize, usize)]) {
    let mut f = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    for &(from, to) in ranges {
        let mut page = from & !(crate::watch::PAGE - 1);
        while page < to {
            f.pages.insert(page);
            page += crate::watch::PAGE;
        }
    }
}

/// Whether the fallback's pages leave out the slots the marks told of in the
/// same round (`VICTORY_PORT_DESCRIPTORS_APART=1`; off: the pages are looked
/// at whole, as before). Read once.
///
/// Why: the fallback's pages are pages the watch said were written, which is
/// where the marks are too. `descriptor::sync_heaps` hands the marked runs
/// and the whole pages to its threads as parts, and a marked slot on such a
/// page was in two parts: compared twice, and the record last seen of it
/// written by two threads at once (the parts are meant to be apart). When
/// the page's part came first, the slot was found new there, its mark
/// already taken, and counted and said as a view record no store told of.
/// With the switch each slot is in one part. What is made is the same.
pub(crate) fn apart() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PORT_DESCRIPTORS_APART").is_ok_and(|v| v.trim() == "1"))
}

/// The byte ranges of `pages` without those of `runs` (sorted, apart from
/// one another, as [`take`] gives them once made byte ranges): the pieces
/// left, in the order of `pages`, empty ones left out. A piece begins and ends
/// where a page or a run does, so it holds whole records when they do.
pub(crate) fn outside(runs: &[(usize, usize)], pages: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(pages.len());
    for &(from, to) in pages {
        let mut at = from;
        // The first run that ends after the page begins.
        let first = runs.partition_point(|&(_, end)| end <= from);
        for &(run_from, run_to) in &runs[first..] {
            if run_from >= to {
                break;
            }
            if run_from > at {
                out.push((at, run_from));
            }
            at = at.max(run_to);
        }
        if at < to {
            out.push((at, to));
        }
    }
    let kept: usize = out.iter().map(|&(a, b)| b - a).sum();
    let whole: usize = pages.iter().map(|&(a, b)| b.saturating_sub(a)).sum();
    APART[0].fetch_add(whole.saturating_sub(kept) as u64, Relaxed);
    APART[1].fetch_add(out.len() as u64, Relaxed);
    out
}

/// Since the last line, with [`apart`]: bytes of the fallback's pages left
/// out (marked in the same round), pieces the pages were cut into.
static APART: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The pages to look at whole this frame (once a frame; none after).
pub(crate) fn to_check(frame: u64) -> Vec<usize> {
    let mut f = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    if f.looked == frame {
        return Vec::new();
    }
    f.looked = frame;
    let mut out = Vec::new();
    while out.len() < check_budget() {
        let from = f.next;
        let Some(&page) = f.pages.range(from..).next().or_else(|| f.pages.iter().next()) else {
            break;
        };
        f.pages.remove(&page);
        f.next = page + crate::watch::PAGE;
        out.push(page);
    }
    CHECKED[0].fetch_add(out.len() as u64, Relaxed);
    out
}

/// The fallback found slot `index` (at `handle`) with a new record: a view
/// record that no mark told of is counted and said.
pub(crate) fn missed(index: u32, handle: u64, view: bool, tabled: bool) {
    if !view {
        CHECKED[2].fetch_add(1, Relaxed);
        return;
    }
    if tabled {
        // A draw read it through a table lately: the draw makes it anyway.
        CHECKED[5].fetch_add(1, Relaxed);
    }
    let adopted = ADOPTED.read().unwrap_or_else(|e| e.into_inner());
    let Some(a) = adopted.as_ref() else { return };
    if marked(a, index) {
        // Told, not taken yet.
        CHECKED[3].fetch_add(1, Relaxed);
        return;
    }
    let n = CHECKED[1].fetch_add(1, Relaxed);
    MISSED_ALL.fetch_add(1, Relaxed);
    let rip = a.writers.get(((handle - a.start) >> 16) as usize).map_or(0, |w| w.load(Relaxed));
    let (base, size) = (IMAGE[0].load(Relaxed), IMAGE[1].load(Relaxed));
    let writer = if rip >= base && rip < base + size { format!("title+{:#x}", rip - base) } else if rip != 0 { format!("{rip:#x} (not the title's)") } else { "not known (no fault on its block)".into() };
    {
        let mut by = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
        *by.entry(writer.clone()).or_default() += 1;
    }
    if n < 20 || n.is_power_of_two() {
        tracing::warn!(
            slot = index,
            handle = format_args!("{handle:#x}"),
            %writer,
            "renderer port 2: a view record in the title's heap that no store told of (found by the fallback; made now)"
        );
    }
}

/// The runtime's own `memcpy` and `memmove` ([`import`]).
static REAL: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

/// Since the last line: the title's copies into the marked heap, records they marked.
static COPIES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// What the title's import of `name` (bound to `address`) is bound to: with
/// the switch, its `memcpy` and `memmove` mark the view records they copy
/// into the heap (the fallback of run port2-2 found ~250 view records a
/// frame on 16 pages that the six stores had not told of, the last writers
/// of their blocks the runtime's `memcpy` among them).
pub fn import(name: &str, address: usize) -> usize {
    let (which, ours) = match name {
        "memcpy" => (0, copy as *const () as usize),
        "memmove" => (1, shift as *const () as usize),
        _ => return address,
    };
    if !on() {
        return address;
    }
    REAL[which].store(address, Relaxed);
    ours
}

type Copy = unsafe extern "system-unwind" fn(*mut u8, *const u8, usize) -> *mut u8;

unsafe extern "system-unwind" fn copy(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
    // SAFETY: the title's copy, by the function it asked for.
    unsafe { copied(to, from, n, REAL[0].load(Relaxed)) }
}

unsafe extern "system-unwind" fn shift(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
    // SAFETY: as above.
    unsafe { copied(to, from, n, REAL[1].load(Relaxed)) }
}

/// # Safety
/// `real` copies as `memcpy` does; the rest are the title's.
#[inline]
unsafe fn copied(to: *mut u8, from: *const u8, n: usize, real: usize) -> *mut u8 {
    // SAFETY: the host's function, noted by `import` before the title ran.
    let real: Copy = unsafe { std::mem::transmute(real) };
    // SAFETY: by the caller.
    let out = unsafe { real(to, from, n) };
    let len = MARKS.len.load(Acquire);
    if len != 0 && n >= 4 {
        let start = MARKS.start.load(Relaxed);
        let (lo, hi) = ((to as u64).max(start), (to as u64 + n as u64).min(start + len));
        if lo < hi {
            COPIES[0].fetch_add(1, Relaxed);
            // Each slot whose first word now lies in the copy and is a view's.
            let mut slot = (lo - start).div_ceil(32);
            let mut marked = 0;
            while start + slot * 32 + 4 <= hi {
                let at = start + slot * 32;
                // SAFETY: just written by the copy, inside the heap.
                if unsafe { (at as *const u32).read_unaligned() } == crate::descriptor::VIEW_TAG {
                    mark_slot(slot as usize);
                    marked += 1;
                }
                slot += 1;
            }
            COPIES[1].fetch_add(marked, Relaxed);
        }
    }
    out
}

/// Marks slot `slot` of the marked heap (in it: checked by the caller).
#[inline]
fn mark_slot(slot: usize) {
    // SAFETY: the maps are leaked, as long as the length says.
    unsafe {
        (*MARKS.slots.load(Relaxed).add(slot)).store(1, Release);
        (*MARKS.chunks.load(Relaxed).add(slot / CHUNK)).store(1, Release);
    }
}

/// Our own calls' marks since the last line.
static MARKED_BY_US: AtomicU64 = AtomicU64::new(0);
/// Since the last line: slots taken, takings, nanoseconds taking.
static TAKEN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Since the last line, of the fallback: pages looked at whole, view records
/// no mark told of, other records changed (not views), view records told and
/// not taken yet; nanoseconds.
/// and of the view records no mark told of, those of draws' tables.
pub(crate) static CHECKED: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
/// Since the start: view records the fallback found that no mark told of.
static MISSED_ALL: AtomicU64 = AtomicU64::new(0);
/// Since the start: the missed records by the place that last faulted on their block.
static WRITERS: std::sync::Mutex<std::collections::BTreeMap<String, u64>> = std::sync::Mutex::new(std::collections::BTreeMap::new());
/// Since the start: slots taken, of them marked by our calls; pages checked.
static ALL: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// With the frame line: what port #2 did per frame over `frames`.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let [taken, takings, ns] = [0, 1, 2].map(|i| TAKEN[i].swap(0, Relaxed));
    let ours = MARKED_BY_US.swap(0, Relaxed);
    let [pages, missed, other, pending, check_ns, tabled] = [0, 1, 2, 3, 4, 5].map(|i| CHECKED[i].swap(0, Relaxed));
    let copies = [0, 1].map(|i| COPIES[i].swap(0, Relaxed));
    ALL[0].fetch_add(taken, Relaxed);
    ALL[1].fetch_add(ours, Relaxed);
    ALL[2].fetch_add(pages, Relaxed);
    if !working() {
        tracing::info!("renderer port 2 (VICTORY_PORT_DESCRIPTORS=1): not working, no store of the title's was patched");
        return;
    }
    let writers = {
        let by = WRITERS.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<(&String, &u64)> = by.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        v.iter().take(6).map(|(w, n)| format!("{w} {n}")).collect::<Vec<_>>().join(", ")
    };
    tracing::info!(
        "renderer port 2 (VICTORY_PORT_DESCRIPTORS=1) per frame: {:.0} slots told (of them {:.0} marked by our calls, the rest by the title's {} patched stores) in {:.1} takings, {:.3} ms taking; \
         of the title's memcpy/memmove calls into the heap {:.0}, {:.0} records they marked; fallback: {:.1} pages looked at whole, {:.2} view records no store told of ({:.2} of them of draws' tables), {:.0} other records changed, {:.1} told and not taken yet, {:.2} ms; \
         since the start: {} slots told ({} by our calls), {} pages looked at, {} view records missed; their writers: {}",
        taken as f64 / frames,
        ours as f64 / frames,
        IMAGE[2].load(Relaxed),
        takings as f64 / frames,
        ns as f64 / 1e6 / frames,
        copies[0] as f64 / frames,
        copies[1] as f64 / frames,
        pages as f64 / frames,
        missed as f64 / frames,
        tabled as f64 / frames,
        other as f64 / frames,
        pending as f64 / frames,
        check_ns as f64 / 1e6 / frames,
        ALL[0].load(Relaxed),
        ALL[1].load(Relaxed),
        ALL[2].load(Relaxed),
        MISSED_ALL.load(Relaxed),
        if writers.is_empty() { "none".into() } else { writers }
    );
    if apart() {
        let [left_out, pieces] = [0, 1].map(|i| APART[i].swap(0, Relaxed) as f64 / frames);
        tracing::info!(
            "renderer port 2's fallback pages without the slots told in the same round (VICTORY_PORT_DESCRIPTORS_APART=1) per frame: {:.0} slots left out, the pages in {:.1} pieces",
            left_out / 32.0,
            pieces
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_layout_is_what_the_stores_read() {
        assert_eq!(size_of::<Marks>(), victory_loader_marks_len());
        assert_eq!(std::mem::offset_of!(Marks, start), 0);
        assert_eq!(std::mem::offset_of!(Marks, len), 8);
        assert_eq!(std::mem::offset_of!(Marks, slots), 16);
        assert_eq!(std::mem::offset_of!(Marks, chunks), 24);
    }

    /// `victory_loader::descriptor_sites::MARKS_LEN` (no dependency on the loader).
    fn victory_loader_marks_len() -> usize {
        32
    }

    /// Slots marked as the stores mark them, and by our calls, come out once
    /// each, as runs; outside the heap nothing is marked.
    #[test]
    fn marked_slots_are_taken_once_as_runs() {
        let start = 0x1000_0000u64;
        assert!(adopt_now(start, 1000));
        assert!(adopted(start) && !adopted(start + 32));
        let store = |slot: usize| {
            let a = ADOPTED.read().unwrap();
            let a = a.as_ref().unwrap();
            a.slots[slot].store(1, Release);
            a.chunks[slot / CHUNK].store(1, Release);
        };
        store(5);
        mark(start + 6 * 32 + 7);
        mark(start + 300 * 32);
        store(999);
        mark(start - 32);
        mark(start + 1000 * 32);
        assert_eq!(take(start).unwrap(), [(5, 2), (300, 1), (999, 1)]);
        assert_eq!(take(start).unwrap(), []);
        assert!(take(start + 32).is_none());
        let_go(start);
        mark(start + 5 * 32);
        assert!(take(start).is_none());
        assert_eq!(MARKS.len.load(Relaxed), 0);
    }

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

    const STEP: usize = 32;
    const PAGE: usize = crate::watch::PAGE;
    const SLOTS_A_PAGE: usize = PAGE / STEP;
    const HEAP: usize = 0x2000_0000;

    /// A round's input: the heap's slots as the title has them now, the runs
    /// taken from the marks, the fallback's pages.
    struct Round {
        title: Vec<[u8; STEP]>,
        runs: Vec<(usize, usize)>,
        pages: Vec<(usize, usize)>,
    }

    /// `rounds` rounds over a heap of `slots` slots: `marked` slots written and
    /// marked a round, in runs of 1 to 32, and on the fallback's `pages`
    /// pages; `unmarked` more written that no mark tells of, on those pages.
    /// With `clustered` the marked runs lie on the pages, as the title's do
    /// (they are the pages the watch said were written).
    fn rounds(seed: u64, slots: usize, rounds: usize, marked: usize, pages: usize, unmarked: usize, clustered: bool) -> Vec<Round> {
        let mut rng = Seeded(seed);
        let mut title = vec![[0u8; STEP]; slots];
        let page_count = slots / SLOTS_A_PAGE;
        (0..rounds)
            .map(|r| {
                let chosen: Vec<usize> = {
                    let mut p: Vec<usize> = Vec::new();
                    while p.len() < pages.min(page_count) {
                        let n = rng.below(page_count as u64) as usize;
                        if !p.contains(&n) {
                            p.push(n);
                        }
                    }
                    p
                };
                let mut is_marked = vec![false; slots];
                let mut left = marked;
                while left > 0 {
                    let len = (1 + rng.below(32) as usize).min(left);
                    let first = if clustered {
                        chosen[rng.below(chosen.len() as u64) as usize] * SLOTS_A_PAGE + rng.below(SLOTS_A_PAGE as u64) as usize
                    } else {
                        rng.below(slots as u64) as usize
                    }
                    .min(slots - len);
                    for s in first..first + len {
                        is_marked[s] = true;
                        let mut record = [0u8; STEP];
                        record[..4].copy_from_slice(&crate::descriptor::VIEW_TAG.to_le_bytes());
                        record[4..12].copy_from_slice(&rng.next().to_le_bytes());
                        record[12] = r as u8;
                        title[s] = record;
                    }
                    left -= len;
                }
                for _ in 0..unmarked {
                    let s = chosen[rng.below(chosen.len() as u64) as usize] * SLOTS_A_PAGE + rng.below(SLOTS_A_PAGE as u64) as usize;
                    if !is_marked[s] {
                        title[s][16..24].copy_from_slice(&rng.next().to_le_bytes());
                    }
                }
                // As `take` gives them: sorted runs of marked slots, as bytes.
                let mut runs: Vec<(usize, usize)> = Vec::new();
                for (s, _) in is_marked.iter().enumerate().filter(|(_, m)| **m) {
                    let at = HEAP + s * STEP;
                    match runs.last_mut() {
                        Some((_, end)) if *end == at => *end += STEP,
                        _ => runs.push((at, at + STEP)),
                    }
                }
                let pages = chosen.iter().map(|&p| (HEAP + p * PAGE, HEAP + (p + 1) * PAGE)).collect();
                Round { title: title.clone(), runs, pages }
            })
            .collect()
    }

    /// What one round of `descriptor::sync_heaps` does with its parts, one part
    /// after another in `order`: the slots whose first byte lies in a part and
    /// that lie whole in it are compared with the record last seen (`seen`),
    /// a new one is made and is seen from then on, and one found new by a
    /// fallback part is counted as no mark's (their marks are taken). Returns
    /// the slots made, and those counted missed.
    fn sync_round(title: &[[u8; STEP]], seen: &mut [[u8; STEP]], parts: &[(usize, usize, bool)], order: &[usize]) -> (Vec<u32>, u64, u64) {
        let (mut made, mut missed, mut looked) = (Vec::new(), 0u64, 0u64);
        for &p in order {
            let (from, to, check) = parts[p];
            // `descriptor::sync_heaps`: `first`, `lead`, and the records' count.
            let first = (from - HEAP).div_ceil(STEP);
            let lead = HEAP + first * STEP;
            let count = (to.saturating_sub(lead) / STEP).min(title.len() - first.min(title.len()));
            looked += count as u64;
            for s in first..first + count {
                if title[s] != seen[s] {
                    seen[s] = title[s];
                    made.push(s as u32);
                    if check {
                        missed += 1;
                    }
                }
            }
        }
        made.sort_unstable();
        made.dedup();
        (made, missed, looked)
    }

    /// The parts of a round as `descriptor::sync_heaps` makes them: the runs,
    /// then the fallback's pages, whole (`apart` off) or without the runs.
    fn parts_of(round: &Round, apart: bool) -> Vec<(usize, usize, bool)> {
        let mut parts: Vec<(usize, usize, bool)> = round.runs.iter().map(|&(a, b)| (a, b, false)).collect();
        if apart {
            parts.extend(outside(&round.runs, &round.pages).into_iter().map(|(a, b)| (a, b, true)));
        } else {
            parts.extend(round.pages.iter().map(|&(a, b)| (a, b, true)));
        }
        parts
    }

    /// The pieces are apart from one another and from the runs, slot-aligned,
    /// inside their pages, and with the runs they hold every byte of the pages.
    #[test]
    fn pages_without_runs_hold_the_rest_of_the_pages() {
        assert_eq!(outside(&[], &[(0, 64)]), [(0, 64)]);
        assert_eq!(outside(&[(0, 64)], &[(0, 64)]), []);
        assert_eq!(outside(&[(32, 96), (128, 160), (300, 320)], &[(64, 192)]), [(96, 128), (160, 192)]);
        assert_eq!(outside(&[(0, 32), (224, 256)], &[(64, 128), (192, 256)]), [(64, 128), (192, 224)]);
        for seed in 1..=8u64 {
            for round in rounds(seed, 64 * SLOTS_A_PAGE, 3, 5_000, 16, 200, seed % 2 == 0) {
                let pieces = outside(&round.runs, &round.pages);
                let mut covered = vec![0u8; 64 * SLOTS_A_PAGE];
                for &(a, b) in pieces.iter().chain(&round.runs) {
                    assert!(a < b && (a - HEAP).is_multiple_of(STEP) && (b - HEAP).is_multiple_of(STEP));
                    for s in (a - HEAP) / STEP..(b - HEAP) / STEP {
                        covered[s] += 1;
                    }
                }
                for &(a, b) in &pieces {
                    assert!(round.pages.iter().any(|&(pa, pb)| pa <= a && b <= pb));
                }
                assert!(covered.iter().all(|&n| n <= 1), "seed {seed}: a slot in two parts");
                for &(a, b) in &round.pages {
                    assert!(covered[(a - HEAP) / STEP..(b - HEAP) / STEP].iter().all(|&n| n == 1), "seed {seed}: a slot of a page in no part");
                }
            }
        }
    }

    /// With the switch the same slots are made, the records last seen come out
    /// the same, and only the true misses are counted; whole pages (the old
    /// way, as reference) count a marked slot as missed when its page's part
    /// is done before its run's, and look at it twice.
    #[test]
    fn pages_without_runs_make_what_whole_pages_make() {
        for (seed, clustered) in (1..=12u64).map(|s| (s, s % 3 != 0)) {
            let slots = 128 * SLOTS_A_PAGE;
            let all = rounds(seed, slots, 6, 8_000, 16, 120, clustered);
            let mut seen = [vec![[0u8; STEP]; slots], vec![[0u8; STEP]; slots], vec![[0u8; STEP]; slots]];
            for round in &all {
                let old = parts_of(round, false);
                let new = parts_of(round, true);
                // Old, runs first (the order of the parts), and pages first.
                let in_order: Vec<usize> = (0..old.len()).collect();
                let pages_first: Vec<usize> = (round.runs.len()..old.len()).chain(0..round.runs.len()).collect();
                let (made_a, missed_a, looked_a) = sync_round(&round.title, &mut seen[0], &old, &in_order);
                let (made_b, missed_b, _) = sync_round(&round.title, &mut seen[1], &old, &pages_first);
                let new_order: Vec<usize> = (round.runs.len()..new.len()).chain(0..round.runs.len()).collect();
                let (made_c, missed_c, looked_c) = sync_round(&round.title, &mut seen[2], &new, &new_order);
                assert_eq!(made_a, made_c, "seed {seed}: other slots made");
                assert_eq!(made_b, made_c, "seed {seed}: other slots made");
                assert!(seen[0] == seen[2] && seen[1] == seen[2], "seed {seed}: other records last seen");
                // Runs first, whole pages count only the true misses too.
                assert_eq!(missed_c, missed_a, "seed {seed}");
                assert!(missed_b >= missed_c);
                assert!(looked_c <= looked_a);
                if clustered {
                    assert!(missed_b > missed_c, "seed {seed}: no false miss in the reference");
                    assert!(looked_c < looked_a);
                }
            }
        }
    }

    /// The cut and the compares it saves, at the figures of `docs/findings.md`:
    /// ~40,000 slots mirrored a frame, 16 fallback pages, a heap of 2^20 slots
    /// (32 MiB). `cargo test --release -p victory-gpu --lib -- --ignored --nocapture bench_pages_without_runs`.
    #[test]
    #[ignore = "benchmark; run with --release --ignored --nocapture"]
    fn bench_pages_without_runs() {
        use std::hint::black_box;
        let slots = 1 << 20;
        for clustered in [false, true] {
            let all = rounds(0x5eed, slots, 4, 40_000, 16, 250, clustered);
            let mut seen = [vec![[0u8; STEP]; slots], vec![[0u8; STEP]; slots]];
            let (mut cut_ns, mut old_ns, mut new_ns, mut looked) = (Vec::new(), Vec::new(), Vec::new(), [0u64; 2]);
            let mut missed = [0u64; 2];
            for round in &all {
                let t = std::time::Instant::now();
                for _ in 0..100 {
                    black_box(outside(black_box(&round.runs), black_box(&round.pages)));
                }
                cut_ns.push(t.elapsed().as_nanos() as f64 / 100.0);
                let old = parts_of(round, false);
                let new = parts_of(round, true);
                let pages_first = |parts: &[(usize, usize, bool)]| -> Vec<usize> { (round.runs.len()..parts.len()).chain(0..round.runs.len()).collect() };
                let t = std::time::Instant::now();
                let (_, m, l) = sync_round(&round.title, &mut seen[0], &old, &pages_first(&old));
                old_ns.push(t.elapsed().as_nanos() as f64);
                missed[0] += m;
                looked[0] += l;
                let t = std::time::Instant::now();
                let (_, m, l) = sync_round(&round.title, &mut seen[1], &new, &pages_first(&new));
                new_ns.push(t.elapsed().as_nanos() as f64);
                missed[1] += m;
                looked[1] += l;
            }
            let median = |v: &mut Vec<f64>| {
                v.sort_by(f64::total_cmp);
                v[v.len() / 2]
            };
            let n = all.len() as f64;
            println!(
                "{}: cut {:.1} us a round; slots looked at {:.0} whole pages, {:.0} without the runs; compares {:.1} us against {:.1} us (one thread); counted missed {:.0} against {:.0} a round",
                if clustered { "runs on the pages" } else { "runs anywhere" },
                median(&mut cut_ns) / 1e3,
                looked[0] as f64 / n,
                looked[1] as f64 / n,
                median(&mut old_ns) / 1e3,
                median(&mut new_ns) / 1e3,
                missed[0] as f64 / n,
                missed[1] as f64 / n,
            );
        }
    }
}
