//! Diagnostics: a census of the title's stores into memory the card reads
//! (`VICTORY_WRITE_CENSUS=<file>`; builds with diagnostics; off without it).
//! Which instructions write the title buffers that host copies are kept of,
//! how many pages each makes dirty a frame (so: how much each makes the
//! refreshes copy), how many bytes a store, into which buffers.
//!
//! The write watch says which pages were written, not by what. So for
//! `VICTORY_WRITE_CENSUS_FRAMES` frames (30) from each of the times
//! `VICTORY_WRITE_CENSUS_AT` gives (seconds after the start; 120,180,240),
//! every write-watched page a refresh has just copied ([`refreshed`]) is
//! made read-only. The first store into it faults here: the instruction is
//! noted (with the title's caller when it is the runtime's `memcpy` or a
//! function of ours), the page is given its protection back and the store
//! goes on. The write watch sees that store as before. The next refresh
//! copies the page and makes it read-only again. With
//! `VICTORY_WRITE_CENSUS_STORES=<n>` (1 to 64; 1 unless given) the first `n`
//! stores into a page between two refreshes are counted: the store runs one
//! instruction with the trap flag set and the page is read-only again
//! behind it.
//!
//! Guarded view memory (`crate::watch`) faults at its first store into a
//! 64 KiB block already: those faults are noted as they are taken
//! ([`guard_fault`]), nothing more is done to that memory.
//!
//! What it changes: the frames of a sample are slower (a fault and a
//! protection change for each page written, some 10 us). The title's
//! bytes, the write watch's answers and what the card reads are the same.
//! Writes of the kernel into title memory do not fault and would fail on a
//! read-only page: the runtime says before each of them
//! (`crate::watch::writing`, `kernel_writing`), and that memory is opened
//! and left alone for the rest of the sample ([`disarm`]); memory the title
//! frees or maps anew is forgotten ([`forget`]). Only committed private
//! pages that are plainly read-write are made read-only.
//!
//! Written at the end of each sample as `<file>.<n>.txt`: the sites by pages
//! they made dirty per frame, the host copies by bytes refreshed per frame,
//! and the title's own sites as `image offset` lines (the form
//! `VICTORY_WRITE_SITES` reads).
#![cfg(feature = "diagnostics")]

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, OnceLock};
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE,
    VirtualAlloc, VirtualProtect, VirtualQuery,
};

const PAGE: usize = 4096;
const BLOCK: usize = 64 << 10;
/// Console addresses lie below 1 TiB: one bit per 4 KiB page of it.
const PAGES: usize = 1 << 28;
/// The trap flag.
const TRAP: u32 = 0x100;
/// Ranges the kernel wrote, left alone for the rest of a sample; past this
/// many nothing more is made read-only in it.
const MOST_KERNEL_RANGES: usize = 1 << 16;

/// What a host copy is refreshed from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// Write-watched memory: its pages are made read-only for the census.
    Watched = 0,
    /// Guarded view memory: its own guards' faults are noted.
    Guarded = 1,
    /// Neither: copied whole.
    Whole = 2,
}

/// How a store came to title memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
enum Via {
    /// An instruction of the title's (or code of ours that runs one in its place).
    Title = 0,
    /// The runtime's code (`memcpy`), called by the title.
    Runtime = 1,
    /// A function of ours the title called.
    Ours = 2,
    /// The runtime's code, with no title caller found.
    RuntimeAlone = 3,
}

/// Whether a sample is being taken.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The bitmap of the pages made read-only (0: not made yet).
static BITS: AtomicUsize = AtomicUsize::new(0);

fn bits() -> Option<&'static [AtomicU64]> {
    let at = BITS.load(Relaxed);
    // SAFETY: once made, the bitmap is never let go; atomics of zeroed pages.
    (at != 0).then(|| unsafe { std::slice::from_raw_parts(at as *const AtomicU64, PAGES / 64) })
}

fn make_bits() -> bool {
    if BITS.load(Relaxed) != 0 {
        return true;
    }
    // SAFETY: fresh zeroed memory of ours, 32 MiB of commit (pages are only
    // taken where they are touched).
    let at = unsafe { VirtualAlloc(std::ptr::null(), PAGES / 8, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
    !at.is_null() && BITS.compare_exchange(0, at as usize, Relaxed, Relaxed).is_ok()
}

fn armed(page: usize) -> bool {
    page < PAGES && bits().is_some_and(|b| b[page / 64].load(Relaxed) >> (page % 64) & 1 != 0)
}

fn mark(page: usize, on: bool) {
    if let Some(b) = bits() {
        if on {
            b[page / 64].fetch_or(1 << (page % 64), Relaxed);
        } else {
            b[page / 64].fetch_and(!(1 << (page % 64)), Relaxed);
        }
    }
}

/// What the switches ask.
struct Settings {
    file: String,
    at: Vec<u64>,
    frames: u64,
    stores: u32,
}

fn settings() -> Option<&'static Settings> {
    static SETTINGS: OnceLock<Option<Settings>> = OnceLock::new();
    SETTINGS
        .get_or_init(|| {
            let file = std::env::var("VICTORY_WRITE_CENSUS").ok().filter(|f| !f.is_empty())?;
            let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
            Some(Settings {
                file,
                at: times(&var("VICTORY_WRITE_CENSUS_AT").unwrap_or_else(|| "120,180,240".into())),
                frames: var("VICTORY_WRITE_CENSUS_FRAMES").and_then(|v| v.trim().parse().ok()).unwrap_or(30).max(1),
                stores: var("VICTORY_WRITE_CENSUS_STORES").and_then(|v| v.trim().parse().ok()).unwrap_or(1).clamp(1, 64),
            })
        })
        .as_ref()
}

/// Seconds after the start, in order.
fn times(text: &str) -> Vec<u64> {
    let mut at: Vec<u64> = text.split(',').filter_map(|t| t.trim().parse().ok()).collect();
    at.sort_unstable();
    at
}

#[derive(Clone, Default)]
struct Site {
    /// Faults that were the first store into a page since it was copied
    /// (or, of view memory, into a block since it was guarded).
    first: u64,
    /// All faults counted (with `VICTORY_WRITE_CENSUS_STORES` above 1, more than `first`).
    stores: u64,
    /// Bytes of those stores, of the ones whose size is known.
    bytes: u64,
    /// Stores whose size is not known (`rep movs`, and others not decoded).
    unknown: u64,
    /// Bit `n`: a store of `2^n` bytes was seen.
    sizes: u32,
    /// Bit 0: into write-watched memory; bit 1: into guarded view memory.
    kinds: u8,
}

#[derive(Clone)]
struct Region {
    start: usize,
    len: usize,
    kind: Kind,
    refreshes: u64,
    copied: u64,
    first: u64,
    stores: u64,
}

#[derive(Clone, Default)]
struct Totals {
    armed_pages: u64,
    arm_us: u64,
    refused: u64,
    first: u64,
    stores: u64,
    by_via: [u64; 4],
    ours: u64,
    ours_pages: u64,
    guard_faults: u64,
    outside: u64,
    overflow: u64,
    kernel_ranges: u64,
    forgotten_pages: u64,
}

/// Ranges of addresses, joined where they meet or touch: start, end.
#[derive(Default)]
struct Ranges(BTreeMap<usize, usize>);

impl Ranges {
    fn meets(&self, from: usize, to: usize) -> bool {
        self.0.range(..to).next_back().is_some_and(|(_, &e)| e > from)
    }

    fn add(&mut self, mut from: usize, mut to: usize) {
        if self.full() || from >= to {
            return;
        }
        // Every range that meets or touches this one is taken into it.
        while let Some((&a, &e)) = self.0.range(..=to).next_back().filter(|(_, e)| **e >= from) {
            self.0.remove(&a);
            from = from.min(a);
            to = to.max(e);
        }
        self.0.insert(from, to);
    }

    fn full(&self) -> bool {
        self.0.len() >= MOST_KERNEL_RANGES
    }
}

/// Everything of a sample.
#[derive(Default)]
struct State {
    /// Bumped whenever pages are opened for good (a kernel write, an end):
    /// a store's trap from before that does not make its page read-only.
    generation: u64,
    stores: u32,
    frames: u64,
    began: Option<std::time::Instant>,
    after: u64,
    /// Our own image, and the title's images (base addresses).
    own: usize,
    titles: Vec<usize>,
    /// The protection each 64 KiB block had where it was made read-only.
    blocks: HashMap<usize, u32>,
    /// Ranges the kernel wrote: not made read-only again.
    kernel: Ranges,
    /// Host copies refreshed: by start and length, and by start alone.
    ids: HashMap<(usize, usize), usize>,
    by_start: BTreeMap<usize, Vec<usize>>,
    regions: Vec<Region>,
    sites: HashMap<(usize, usize, Via), Site>,
    site_regions: HashMap<(usize, usize, Via, usize), u64>,
    /// Faults per page since it was last copied.
    traps: HashMap<usize, u32>,
    totals: Totals,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn state() -> MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

const SITES: usize = 1 << 14;
const SITE_REGIONS: usize = 1 << 16;
const TRAPS: usize = 1 << 18;

thread_local! {
    /// Pages this thread's store made writable that are to be read-only again
    /// after it (`VICTORY_WRITE_CENSUS_STORES` above 1), with the generation.
    static PENDING: Cell<([(usize, u64); 4], usize)> = const { Cell::new(([(0, 0); 4], 0)) };
}

#[cfg(test)]
thread_local! {
    /// This thread's code counts as the title's.
    static AS_TITLE: Cell<bool> = const { Cell::new(false) };
}

/// The base of the image whose code `rip` is (0: none known, such as code
/// made at run time).
fn image_of(rip: u64) -> usize {
    use windows_sys::Win32::System::Diagnostics::Debug::RtlLookupFunctionEntry;
    let mut base = 0u64;
    // SAFETY: a lookup in the system's tables, with an out pointer.
    let entry = unsafe { RtlLookupFunctionEntry(rip, &mut base, std::ptr::null_mut()) };
    if entry.is_null() { 0 } else { base as usize }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Whose {
    Title,
    Ours,
    Runtime,
}

impl State {
    fn whose(&self, rip: u64) -> Whose {
        #[cfg(test)]
        if AS_TITLE.get() {
            return Whose::Title;
        }
        match image_of(rip) {
            // Code without unwind data is code of ours that runs the title's
            // instructions for it (`victory_loader::write_sites`).
            0 => Whose::Title,
            base if base == self.own => Whose::Ours,
            base if self.titles.contains(&base) => Whose::Title,
            _ => Whose::Runtime,
        }
    }

    /// The store's instruction, the title's caller when the instruction is
    /// not the title's, and how it came.
    fn attribute(&self, context: &CONTEXT) -> (usize, usize, Via) {
        use windows_sys::Win32::System::Diagnostics::Debug::{RtlLookupFunctionEntry, RtlVirtualUnwind};
        let rip = context.Rip as usize;
        let whose = self.whose(context.Rip);
        if whose == Whose::Title {
            return (rip, 0, Via::Title);
        }
        let mut c = *context;
        for _ in 0..8 {
            let mut base = 0u64;
            // SAFETY: a lookup, then an unwind of a copy of the faulting
            // thread's registers over its own stack.
            unsafe {
                let entry = RtlLookupFunctionEntry(c.Rip, &mut base, std::ptr::null_mut());
                if entry.is_null() {
                    // A leaf: the return address is on top of the stack.
                    let mut back = [0u8; 8];
                    if !crate::host::read_into(c.Rsp as *const u8, &mut back) {
                        break;
                    }
                    c.Rip = u64::from_le_bytes(back);
                    c.Rsp += 8;
                } else {
                    let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
                    RtlVirtualUnwind(0, base, c.Rip, entry, &mut c, &mut data, &mut frame, std::ptr::null_mut());
                }
            }
            if c.Rip == 0 {
                break;
            }
            if self.whose(c.Rip) == Whose::Title && image_of(c.Rip) != 0 {
                let via = if whose == Whose::Ours { Via::Ours } else { Via::Runtime };
                return (rip, c.Rip as usize, via);
            }
        }
        (rip, 0, if whose == Whose::Ours { Via::Ours } else { Via::RuntimeAlone })
    }

    /// The host copy `[at]` is in (the latest of those that hold it).
    fn region_of(&self, at: usize) -> Option<usize> {
        for (_, ids) in self.by_start.range(..=at).rev().take(16) {
            if let Some(&id) = ids.iter().find(|&&id| at < self.regions[id].start + self.regions[id].len) {
                return Some(id);
            }
        }
        None
    }

    fn region(&mut self, start: usize, len: usize, kind: Kind) -> usize {
        if let Some(&id) = self.ids.get(&(start, len)) {
            return id;
        }
        let id = self.regions.len();
        self.regions.push(Region { start, len, kind, refreshes: 0, copied: 0, first: 0, stores: 0 });
        self.ids.insert((start, len), id);
        self.by_start.entry(start).or_default().push(id);
        id
    }

    /// Notes a store into `at` (`first`: the first since the page or block
    /// was copied).
    fn note(&mut self, context: &CONTEXT, (rip, caller, via): (usize, usize, Via), at: usize, first: bool, kind: Kind) {
        let size = crate::titlewrite::store_size(context);
        let region = self.region_of(at);
        let t = &mut self.totals;
        t.stores += 1;
        t.first += u64::from(first);
        t.by_via[via as usize] += 1;
        if region.is_none() {
            t.outside += 1;
        }
        let key = (rip, caller, via);
        // (No allocation here, under a fault: past their room, only counted.)
        if self.sites.contains_key(&key) || self.sites.len() < SITES {
            let site = self.sites.entry(key).or_default();
            site.first += u64::from(first);
            site.stores += 1;
            match size {
                Some(n) => {
                    site.bytes += n as u64;
                    site.sizes |= 1 << n.trailing_zeros().min(31);
                }
                None => site.unknown += 1,
            }
            site.kinds |= 1 << (kind as u8);
        } else {
            self.totals.overflow += 1;
        }
        if let Some(id) = region {
            let r = &mut self.regions[id];
            r.first += u64::from(first);
            r.stores += 1;
            let key = (rip, caller, via, id);
            if self.site_regions.contains_key(&key) || self.site_regions.len() < SITE_REGIONS {
                *self.site_regions.entry(key).or_default() += 1;
            }
        }
    }
}

/// The protection a page of `block` is given back.
fn protection(s: &State, block: usize) -> u32 {
    s.blocks.get(&block).copied().unwrap_or(PAGE_READWRITE)
}

/// The read-only form of `protection` (its other bits kept).
fn read_only(protection: u32) -> u32 {
    protection & !0xff | PAGE_READONLY
}

fn protect(at: usize, len: usize, to: u32) -> bool {
    let mut old = 0;
    // SAFETY: committed title memory (asked about first) or pages this
    // module made read-only; the protection is theirs or its read-only form.
    unsafe { VirtualProtect(at as *const _, len, to, &mut old) != 0 }
}

fn query(at: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    (n != 0).then_some(info)
}

/// Gives the pages `[first, last)` that are read-only for the census their
/// protection back, in runs; how many.
fn open_pages(s: &State, first: usize, last: usize) -> u64 {
    let mut opened = 0;
    let mut page = next(first, last, true);
    while page < last {
        let block = page * PAGE / BLOCK;
        // (A run within one block: one protection.)
        let end = next(page, last.min((block + 1) * BLOCK / PAGE), false);
        for p in page..end {
            mark(p, false);
        }
        protect(page * PAGE, (end - page) * PAGE, protection(s, block));
        opened += (end - page) as u64;
        page = end;
    }
    opened
}

/// Makes the write-watched pages of `[from, to)` that are not read-only yet
/// read-only (committed private read-write pages only).
fn arm(s: &mut State, from: usize, to: usize) {
    let started = std::time::Instant::now();
    let (first, last) = (from / PAGE, to.div_ceil(PAGE).min(PAGES));
    let mut page = next(first, last, false);
    while page < last {
        let end = next(page, last, true);
        arm_run(s, page * PAGE, end * PAGE);
        page = next(end, last, false);
    }
    s.totals.arm_us += started.elapsed().as_micros() as u64;
}

/// The first page from `page` on, below `last`, whose bit is `set` (`last`
/// if none): 64 pages at a time where their word says all of them.
fn next(mut page: usize, last: usize, set: bool) -> usize {
    let Some(b) = bits() else {
        return if set { last } else { page.min(last) };
    };
    while page < last {
        let word = b[page / 64].load(Relaxed);
        let word = (if set { word } else { !word }) >> (page % 64);
        if word != 0 {
            return (page + word.trailing_zeros() as usize).min(last);
        }
        page = (page / 64 + 1) * 64;
    }
    last
}

fn arm_run(s: &mut State, from: usize, to: usize) {
    if s.kernel.full() || s.kernel.meets(from, to) {
        return;
    }
    let mut at = from;
    while at < to {
        let Some(info) = query(at) else {
            return;
        };
        let end = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(to);
        if end <= at {
            return;
        }
        let plain = info.State == MEM_COMMIT
            && info.Type == MEM_PRIVATE
            && info.Protect & 0xff == PAGE_READWRITE
            && info.Protect & PAGE_GUARD == 0;
        // One protection per 64 KiB block, as it is given back.
        let same = (at / BLOCK..end.div_ceil(BLOCK)).all(|b| s.blocks.get(&b).is_none_or(|&p| p == info.Protect));
        if plain && same {
            if protect(at, end - at, read_only(info.Protect)) {
                for b in at / BLOCK..end.div_ceil(BLOCK) {
                    s.blocks.insert(b, info.Protect);
                }
                for p in at / PAGE..end / PAGE {
                    mark(p, true);
                    s.traps.remove(&p);
                }
                s.totals.armed_pages += ((end - at) / PAGE) as u64;
            } else {
                s.totals.refused += 1;
            }
        }
        at = end;
    }
}

/// A refresh of the host copy of the title buffer `[start, start + len)`
/// copied `copied` bytes of `[from, to)` (`crate::shadow`); the pages of
/// write-watched memory there are made read-only during a sample.
pub(crate) fn refreshed(start: usize, len: usize, from: usize, to: usize, copied: usize, kind: Kind) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    // (Other tests' refreshes, on threads of their own, are left alone.)
    #[cfg(test)]
    if !AS_TITLE.get() {
        return;
    }
    let mut guard = state();
    let Some(s) = guard.as_mut().filter(|_| ACTIVE.load(Relaxed)) else {
        return;
    };
    let id = s.region(start, len, kind);
    s.regions[id].refreshes += 1;
    s.regions[id].copied += copied as u64;
    if kind == Kind::Watched && from < to {
        arm(s, from, to);
    }
}

/// The kernel will write `[at, at + len)` (a file read, a copy of ours
/// through it): opened, and left so for the rest of the sample.
pub(crate) fn disarm(at: usize, len: usize) {
    if bits().is_none() || len == 0 {
        return;
    }
    #[cfg(test)]
    if !AS_TITLE.get() {
        return;
    }
    let mut guard = state();
    let Some(s) = guard.as_mut() else {
        return;
    };
    let end = at.saturating_add(len);
    s.generation += 1;
    s.totals.kernel_ranges += 1;
    s.kernel.add(at, end);
    open_pages(s, at / PAGE, end.div_ceil(PAGE).min(PAGES));
}

/// The title frees `[at, at + len)` or maps other memory there: what this
/// module knew of it goes (its protection goes with the memory).
pub(crate) fn forget(at: usize, len: usize) {
    if bits().is_none() || len == 0 {
        return;
    }
    #[cfg(test)]
    if !AS_TITLE.get() {
        return;
    }
    let mut guard = state();
    let Some(s) = guard.as_mut() else {
        return;
    };
    s.generation += 1;
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE).min(PAGES));
    for page in first..last {
        if armed(page) {
            mark(page, false);
            s.totals.forgotten_pages += 1;
        }
    }
    // Blocks wholly inside: their protection is the next allocation's.
    for block in at.div_ceil(BLOCK)..at.saturating_add(len) / BLOCK {
        s.blocks.remove(&block);
    }
}

/// A write fault of the title's on guarded view memory at `at`, taken by
/// `crate::watch` (the first store into its block since it was guarded).
pub(crate) fn guard_fault(context: &CONTEXT, at: usize) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    #[cfg(test)]
    if !AS_TITLE.get() {
        return;
    }
    let mut guard = state();
    if let Some(s) = guard.as_mut().filter(|_| ACTIVE.load(Relaxed)) {
        s.totals.guard_faults += 1;
        let who = s.attribute(context);
        s.note(context, who, at, true, Kind::Guarded);
    }
}

/// A write fault at `at`: if it is a page of the census, noted and opened.
fn written(context: &mut CONTEXT, at: usize) -> bool {
    let page = at / PAGE;
    if !armed(page) {
        return false;
    }
    let mut guard = state();
    let Some(s) = guard.as_mut() else {
        return false;
    };
    if !armed(page) {
        // Opened meanwhile by another thread: the store is made again.
        return query(at).is_some_and(|i| i.Protect & 0xff == PAGE_READWRITE);
    }
    let block = at / BLOCK;
    let traps = s.traps.get(&page).copied().unwrap_or(0);
    let who = s.attribute(context);
    if who.2 == Via::Ours && who.1 == 0 {
        // Ours alone (what shaders wrote, landed): not a store of the
        // title's. The whole block is opened, as such writes come in pieces.
        s.totals.ours += 1;
        let first = block * BLOCK / PAGE;
        s.totals.ours_pages += open_pages(s, first, (first + BLOCK / PAGE).min(PAGES));
        return true;
    }
    s.note(context, who, at, traps == 0, Kind::Watched);
    mark(page, false);
    protect(page * PAGE, PAGE, protection(s, block));
    if s.traps.contains_key(&page) || s.traps.len() < TRAPS {
        *s.traps.entry(page).or_default() += 1;
    }
    if traps + 1 < s.stores && ACTIVE.load(Relaxed) {
        let generation = s.generation;
        PENDING.with(|p| {
            let (mut pages, n) = p.get();
            if n < pages.len() {
                pages[n] = (page, generation);
                p.set((pages, n + 1));
                context.EFlags |= TRAP;
            }
        });
    }
    true
}

/// The trap behind a store [`written`] let through: its pages are
/// read-only again. `false` if this thread had none.
fn stepped(context: &mut CONTEXT) -> bool {
    let (pages, n) = PENDING.get();
    if n == 0 {
        return false;
    }
    PENDING.set(([(0, 0); 4], 0));
    context.EFlags &= !TRAP;
    let mut guard = state();
    let Some(s) = guard.as_mut() else {
        return true;
    };
    for &(page, generation) in &pages[..n] {
        if !ACTIVE.load(Relaxed) || generation != s.generation || armed(page) {
            continue;
        }
        let at = page * PAGE;
        if s.kernel.meets(at, at + PAGE) {
            continue;
        }
        if protect(at, PAGE, read_only(protection(s, at / BLOCK))) {
            mark(page, true);
        }
    }
    true
}

fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, EXCEPTION_SINGLE_STEP};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let (r, context) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
        let ours = if r.ExceptionCode == EXCEPTION_SINGLE_STEP {
            stepped(context)
        } else {
            r.ExceptionCode == EXCEPTION_ACCESS_VIOLATION
                && r.NumberParameters >= 2
                && r.ExceptionInformation[0] == 1
                && written(context, r.ExceptionInformation[1])
        };
        if ours { EXCEPTION_CONTINUE_EXECUTION } else { EXCEPTION_CONTINUE_SEARCH }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler touches only this module's pages and state. Put
    // first, ahead of `crate::watch`'s: it passes on all that is not its own.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_fault));
    });
}

/// The process's images: ours (the executable) and the title's (mapped by
/// the loader as `victory-image-*.dll`).
fn images() -> (usize, Vec<usize>) {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
    use windows_sys::Win32::System::ProcessStatus::K32EnumProcessModules;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: questions about this process, with buffers of the sizes given.
    unsafe {
        let own = GetModuleHandleW(std::ptr::null()) as usize;
        let mut modules: Vec<HMODULE> = vec![std::ptr::null_mut(); 1024];
        let mut needed = 0;
        let size = (modules.len() * size_of::<HMODULE>()) as u32;
        if K32EnumProcessModules(GetCurrentProcess(), modules.as_mut_ptr(), size, &mut needed) == 0 {
            return (own, Vec::new());
        }
        modules.truncate(needed as usize / size_of::<HMODULE>());
        let titles = modules
            .into_iter()
            .filter(|&m| {
                let mut name = [0u16; 1024];
                let n = GetModuleFileNameW(m, name.as_mut_ptr(), name.len() as u32) as usize;
                String::from_utf16_lossy(&name[..n]).to_ascii_lowercase().contains("victory-image-")
            })
            .map(|m| m as usize)
            .collect();
        (own, titles)
    }
}

/// Begins a sample: `stores` stores counted per page between refreshes.
fn begin(stores: u32, after: u64) -> bool {
    if !make_bits() {
        return false;
    }
    install();
    let (own, titles) = images();
    let mut s = State {
        stores,
        after,
        own,
        titles,
        began: Some(std::time::Instant::now()),
        ..State::default()
    };
    s.sites.reserve(SITES + 1);
    s.site_regions.reserve(SITE_REGIONS + 1);
    s.traps.reserve(TRAPS + 1);
    *state() = Some(s);
    ACTIVE.store(true, Relaxed);
    true
}

/// Ends a sample: every page is given its protection back. What was
/// counted, and the milliseconds the sample took.
fn end() -> Option<(State, f64)> {
    let mut guard = state();
    ACTIVE.store(false, Relaxed);
    let s = guard.as_mut()?;
    s.generation += 1;
    let blocks: Vec<usize> = s.blocks.keys().copied().collect();
    for block in blocks {
        let (first, last) = (block * BLOCK / PAGE, (block + 1) * BLOCK / PAGE);
        let protection = protection(s, block);
        let mut page = first;
        while page < last {
            if !armed(page) {
                page += 1;
                continue;
            }
            // A run read-only by our hand, as far as one protection goes
            // (the title may have changed a protection there since: then
            // it is left as the title made it).
            let info = query(page * PAGE);
            let mut end = page + 1;
            while end < last && armed(end) {
                end += 1;
            }
            if let Some(info) = info {
                let region_end = (info.BaseAddress as usize + info.RegionSize) / PAGE;
                end = end.min(region_end.max(page + 1));
                if info.Protect == read_only(protection) {
                    protect(page * PAGE, (end - page) * PAGE, protection);
                }
            }
            for p in page..end {
                mark(p, false);
            }
            page = end;
        }
    }
    let ms = s.began.map_or(0.0, |b| b.elapsed().as_secs_f64() * 1e3);
    let taken = guard.take()?;
    Some((taken, ms))
}

/// A frame was presented: samples begin and end here.
pub(crate) fn frame() {
    static BEGAN: OnceLock<std::time::Instant> = OnceLock::new();
    static MADE: AtomicU64 = AtomicU64::new(0);
    let began = *BEGAN.get_or_init(std::time::Instant::now);
    let Some(settings) = settings() else {
        return;
    };
    if ACTIVE.load(Relaxed) {
        let frames = {
            let mut guard = state();
            let Some(s) = guard.as_mut() else {
                return;
            };
            s.frames += 1;
            s.frames
        };
        if frames < settings.frames {
            return;
        }
        let Some((s, ms)) = end() else {
            return;
        };
        let file = format!("{}.{}.txt", settings.file, MADE.fetch_add(1, Relaxed));
        std::thread::spawn(move || {
            let text = report(&s, ms, &name);
            match std::fs::write(&file, text) {
                Ok(()) => tracing::info!(file, sites = s.sites.len(), "write census written (VICTORY_WRITE_CENSUS)"),
                Err(e) => tracing::warn!(file, %e, "write census not written"),
            }
        });
    } else if let Some(&at) = settings.at.get(MADE.load(Relaxed) as usize) {
        if began.elapsed().as_secs() >= at && begin(settings.stores, at) {
            tracing::info!(frames = settings.frames, stores = settings.stores, "write census sample begun (VICTORY_WRITE_CENSUS)");
        }
    }
}

/// `image+offset` of a code address.
fn name(address: usize) -> String {
    match crate::titlewrite::place(address) {
        Some((image, offset)) => format!("{image}+{offset:#x}"),
        None => format!("{address:#x}"),
    }
}

fn sizes(mask: u32, unknown: u64) -> String {
    let mut out: Vec<String> = (0..32).filter(|n| mask >> n & 1 != 0).map(|n| (1u64 << n).to_string()).collect();
    if unknown != 0 {
        out.push("?".into());
    }
    if out.is_empty() { "-".into() } else { out.join("/") }
}

/// The census of a sample as text.
fn report(s: &State, ms: f64, name: &dyn Fn(usize) -> String) -> String {
    use std::fmt::Write;
    let frames = s.frames.max(1) as f64;
    let per = |n: u64| n as f64 / frames;
    let t = &s.totals;
    let mut out = String::new();
    let copied = |kind: Kind| s.regions.iter().filter(|r| r.kind == kind).map(|r| r.copied).sum::<u64>();
    let _ = writeln!(
        out,
        "write census (VICTORY_WRITE_CENSUS): {} frames from {} s after the start, {:.1} ms a frame with the census's faults; \
         the first {} store(s) into a page between two refreshes counted",
        s.frames,
        s.after,
        ms / frames,
        s.stores
    );
    let _ = writeln!(
        out,
        "per frame: {:.1} MB refreshed into host copies ({:.1} write-watched, {:.1} guarded views, {:.1} whole) in {:.0} refreshes of {} host copies; \
         {:.0} pages made read-only ({:.2} ms), {:.1} protections refused",
        per(copied(Kind::Watched) + copied(Kind::Guarded) + copied(Kind::Whole)) / 1e6,
        per(copied(Kind::Watched)) / 1e6,
        per(copied(Kind::Guarded)) / 1e6,
        per(copied(Kind::Whole)) / 1e6,
        per(s.regions.iter().map(|r| r.refreshes).sum()),
        s.regions.len(),
        per(t.armed_pages),
        per(t.arm_us) / 1e3,
        per(t.refused)
    );
    let _ = writeln!(
        out,
        "per frame: {:.0} first stores into pages or blocks since they were copied ({:.0} of them guard faults of view memory), {:.0} stores counted; \
         by the title's code {:.0}, the runtime's for the title {:.0}, ours for the title {:.0}, the runtime's with no title caller {:.0}; \
         {:.0} outside every host copy; our own writes (landing what shaders wrote) {:.1} into {:.0} pages; \
         {:.1} kernel writes opened their range, {:.1} pages forgotten as their memory went; {} not kept for want of room",
        per(t.first),
        per(t.guard_faults),
        per(t.stores),
        per(t.by_via[0]),
        per(t.by_via[1]),
        per(t.by_via[2]),
        per(t.by_via[3]),
        per(t.outside),
        per(t.ours),
        per(t.ours_pages),
        per(t.kernel_ranges),
        per(t.forgotten_pages),
        t.overflow
    );
    let region_name = |id: usize| {
        let r = &s.regions[id];
        format!("{:#x}+{:#x}", r.start, r.len)
    };
    // Per site, its host copies by stores.
    let mut of_site: HashMap<(usize, usize, Via), Vec<(usize, u64)>> = HashMap::new();
    let mut of_region: HashMap<usize, Vec<((usize, usize, Via), u64)>> = HashMap::new();
    for (&(rip, caller, via, id), &n) in &s.site_regions {
        of_site.entry((rip, caller, via)).or_default().push((id, n));
        of_region.entry(id).or_default().push(((rip, caller, via), n));
    }
    let site_name = |&(rip, caller, via): &(usize, usize, Via)| match via {
        Via::Title => name(rip),
        Via::Runtime | Via::Ours => format!("{} <- {}", name(rip), name(caller)),
        Via::RuntimeAlone => format!("{} <- ?", name(rip)),
    };
    let mut sites: Vec<_> = s.sites.iter().collect();
    sites.sort_unstable_by_key(|(_, site)| std::cmp::Reverse((site.first, site.stores)));
    let _ = writeln!(out, "\nsites by pages they made dirty per frame (first stores since a copy took the page; 64 KiB blocks for view memory):");
    let _ = writeln!(out, "  dirty/frame  stores/frame  bytes/frame  sizes  memory  via  site  [host copies by share of its stores]");
    for (key, site) in sites.iter().take(200) {
        let mut regions = of_site.get(*key).cloned().unwrap_or_default();
        regions.sort_unstable_by_key(|r| std::cmp::Reverse(r.1));
        let all: u64 = regions.iter().map(|r| r.1).sum::<u64>().max(1);
        let regions: Vec<String> =
            regions.iter().take(3).map(|&(id, n)| format!("{} {:.0}%", region_name(id), n as f64 * 100.0 / all as f64)).collect();
        let memory = match site.kinds {
            1 => "watched",
            2 => "view",
            _ => "both",
        };
        let via = ["title", "runtime", "ours", "runtime?"][key.2 as usize];
        let _ = writeln!(
            out,
            "  {:9.1}  {:12.1}  {:11.0}  {:5}  {:7}  {:8}  {}  [{}]",
            per(site.first),
            per(site.stores),
            per(site.bytes),
            sizes(site.sizes, site.unknown),
            memory,
            via,
            site_name(*key),
            regions.join(", ")
        );
    }
    let mut regions: Vec<(usize, &Region)> = s.regions.iter().enumerate().collect();
    regions.sort_unstable_by_key(|(_, r)| std::cmp::Reverse((r.copied, r.first)));
    let _ = writeln!(out, "\nhost copies by bytes refreshed per frame:");
    let _ = writeln!(out, "  MB/frame  refreshes/frame  dirty/frame  memory   copy  [sites by share of its stores]");
    for (id, r) in regions.iter().take(60) {
        let mut sites = of_region.get(id).cloned().unwrap_or_default();
        sites.sort_unstable_by_key(|x| std::cmp::Reverse(x.1));
        let all: u64 = sites.iter().map(|x| x.1).sum::<u64>().max(1);
        let sites: Vec<String> =
            sites.iter().take(4).map(|(k, n)| format!("{} {:.0}%", site_name(k), *n as f64 * 100.0 / all as f64)).collect();
        let memory = ["watched", "view", "whole"][r.kind as usize];
        let _ = writeln!(
            out,
            "  {:8.2}  {:15.1}  {:11.1}  {:7}  {}  [{}]",
            per(r.copied) / 1e6,
            per(r.refreshes),
            per(r.first),
            memory,
            region_name(*id),
            sites.join(", ")
        );
    }
    let _ = writeln!(out, "\nthe title's sites, as `image offset` (dirty pages per frame after each):");
    for (key, site) in sites.iter().filter(|(k, _)| k.2 == Via::Title).take(200) {
        if let Some((image, offset)) = crate::titlewrite::place(key.0) {
            let _ = writeln!(out, "{image} {offset:x}  # {:.1}", per(site.first));
        }
    }
    let _ = writeln!(out, "\nthe title's callers of the runtime's and our code, as `image offset`:");
    let mut callers: HashMap<usize, u64> = HashMap::new();
    for (key, site) in &sites {
        if matches!(key.2, Via::Runtime | Via::Ours) {
            *callers.entry(key.1).or_default() += site.first;
        }
    }
    let mut callers: Vec<_> = callers.into_iter().collect();
    callers.sort_unstable_by_key(|c| std::cmp::Reverse(c.1));
    for (caller, first) in callers.iter().take(100) {
        if let Some((image, offset)) = crate::titlewrite::place(*caller) {
            let _ = writeln!(out, "{image} {offset:x}  # {:.1}", per(*first));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};

    fn protection_at(at: usize) -> u32 {
        query(at).unwrap().Protect
    }

    /// Stores into pages a refresh took fault once each, are noted and go
    /// on; a refresh takes them again; a kernel write opens its range; the
    /// end gives every page its protection back.
    #[test]
    fn stores_into_copied_pages_are_counted_and_done() {
        // SAFETY: fresh memory of the test's, written by it alone.
        unsafe {
            let size = 16 * PAGE;
            let at = VirtualAlloc(std::ptr::null(), size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) as usize;
            assert_ne!(at, 0);
            if at / PAGE >= PAGES {
                // (Above 1 TiB: not memory the census looks at.)
                VirtualFree(at as *mut _, 0, MEM_RELEASE);
                return;
            }
            assert!(begin(3, 0));
            AS_TITLE.set(true);
            refreshed(at, size, at, at + size, 123, Kind::Watched);
            assert_eq!(protection_at(at) & 0xff, PAGE_READONLY);
            assert!(armed(at / PAGE + 2));
            let p = at as *mut u64;
            // Page 2: one store. Page 5: five stores, three of them counted.
            p.add(2 * PAGE / 8).write_volatile(7);
            for i in 0..5 {
                p.add(5 * PAGE / 8 + i).write_volatile(i as u64);
            }
            assert_eq!(p.add(2 * PAGE / 8).read_volatile(), 7);
            assert_eq!(p.add(5 * PAGE / 8 + 4).read_volatile(), 4);
            assert_eq!(protection_at(at + 2 * PAGE), PAGE_READWRITE);
            assert!(!armed(at / PAGE + 2) && armed(at / PAGE + 3));
            {
                let guard = state();
                let s = guard.as_ref().unwrap();
                assert_eq!(s.totals.first, 2);
                assert_eq!(s.totals.stores, 4);
                let site = s.sites.values().map(|x| x.bytes).sum::<u64>();
                assert_eq!(site, 4 * 8);
                assert_eq!(s.regions[0].copied, 123);
            }
            // Taken again: page 2 faults once more as a first store.
            refreshed(at, size, at, at + size, 0, Kind::Watched);
            assert!(armed(at / PAGE + 2));
            p.add(2 * PAGE / 8 + 1).write_volatile(8);
            // A kernel write: its pages open and stay so.
            disarm(at + 8 * PAGE, 2 * PAGE);
            assert_eq!(protection_at(at + 8 * PAGE), PAGE_READWRITE);
            refreshed(at, size, at, at + size, 0, Kind::Watched);
            assert!(!armed(at / PAGE + 8) && !armed(at / PAGE + 9));
            AS_TITLE.set(false);
            let (s, ms) = end().unwrap();
            assert_eq!(s.totals.first, 3);
            assert!(ms >= 0.0);
            for page in 0..16 {
                assert_eq!(protection_at(at + page * PAGE), PAGE_READWRITE, "page {page}");
                assert!(!armed(at / PAGE + page));
            }
            let text = report(&s, ms, &|a| format!("{a:#x}"));
            assert!(text.contains("sites by pages"), "{text}");
            assert!(text.contains(&format!("{at:#x}+{size:#x}")), "{text}");
            VirtualFree(at as *mut _, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn times_are_read_in_order() {
        assert_eq!(times("240, 120,x,180"), vec![120, 180, 240]);
        assert!(times("").is_empty());
        assert_eq!(read_only(PAGE_READWRITE | 0x400), PAGE_READONLY | 0x400);
        assert_eq!(sizes(0b1_0100, 1), "4/16/?");
        assert_eq!(sizes(0, 0), "-");
    }

    #[test]
    fn kernel_ranges_join() {
        let mut r = Ranges::default();
        r.add(100, 200);
        r.add(300, 400);
        assert!(r.meets(150, 160) && r.meets(50, 101) && r.meets(399, 500));
        assert!(!r.meets(200, 300) && !r.meets(0, 100) && !r.meets(400, 401));
        r.add(200, 300);
        assert_eq!(r.0.len(), 1);
        assert_eq!(r.0.get(&100), Some(&400));
        r.add(50, 120);
        r.add(390, 450);
        assert_eq!(r.0.iter().collect::<Vec<_>>(), vec![(&50, &450)]);
    }
}
