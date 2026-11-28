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
//! What it may cost is bounded: Windows is asked what memory is once per
//! range a sample, making pages read-only takes at most
//! `VICTORY_WRITE_CENSUS_ARM_MS` (8) of a frame, and a thread of its own
//! ends the sample (every page read-write again, what was counted written)
//! when no frame came for `VICTORY_WRITE_CENSUS_STALL_MS` (2000) or after a
//! minute.
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

/// Two bitmaps: the pages read-only now, then the pages ever made so
/// (never cleared: a fault that was raised while a page was read-only may
/// be looked at after it is read-write again).
fn bits() -> Option<&'static [AtomicU64]> {
    let at = BITS.load(Relaxed);
    // SAFETY: once made, the bitmaps are never let go; atomics of zeroed pages.
    (at != 0).then(|| unsafe { std::slice::from_raw_parts(at as *const AtomicU64, 3 * PAGES / 64) })
}

fn make_bits() -> bool {
    if BITS.load(Relaxed) != 0 {
        return true;
    }
    // SAFETY: fresh zeroed memory of ours, 96 MiB of commit (pages are only
    // taken where they are touched).
    let at = unsafe { VirtualAlloc(std::ptr::null(), 3 * PAGES / 8, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
    !at.is_null() && BITS.compare_exchange(0, at as usize, Relaxed, Relaxed).is_ok()
}

fn armed(page: usize) -> bool {
    page < PAGES && bits().is_some_and(|b| b[page / 64].load(Relaxed) >> (page % 64) & 1 != 0)
}

/// The third bitmap: pages read-only again after a counted store (the trap
/// flag's, `VICTORY_WRITE_CENSUS_STORES` above 1), not since a refresh.
fn trapped(page: usize) -> bool {
    page < PAGES && bits().is_some_and(|b| b[2 * PAGES / 64 + page / 64].load(Relaxed) >> (page % 64) & 1 != 0)
}

fn mark_trapped(page: usize, on: bool) {
    if let Some(b) = bits() {
        let word = &b[2 * PAGES / 64 + page / 64];
        if on {
            word.fetch_or(1 << (page % 64), Relaxed);
        } else {
            word.fetch_and(!(1 << (page % 64)), Relaxed);
        }
    }
}

/// A refresh took `[first, last)`: none of its pages is trapped any more
/// (its next store is a first one), 64 at a time.
fn untrap(first: usize, last: usize) {
    let Some(b) = bits() else {
        return;
    };
    let mut page = first;
    while page < last {
        let n = (64 - page % 64).min(last - page);
        let mask = if n == 64 { !0u64 } else { ((1u64 << n) - 1) << (page % 64) };
        b[2 * PAGES / 64 + page / 64].fetch_and(!mask, Relaxed);
        page += n;
    }
}

/// Whether the census ever made the page read-only.
fn ever(page: usize) -> bool {
    page < PAGES && bits().is_some_and(|b| b[PAGES / 64 + page / 64].load(Relaxed) >> (page % 64) & 1 != 0)
}

/// The order that keeps the handler right: a page's bit is set BEFORE it is
/// made read-only and cleared AFTER it is read-write again, so a page that
/// is read-only by our hand always has its bit. (The other order let a
/// store fault on a read-only page whose bit was already or not yet clear,
/// and the fault went on to the title, which died: run census-075138.)
fn mark(page: usize, on: bool) {
    if let Some(b) = bits() {
        if on {
            b[page / 64].fetch_or(1 << (page % 64), Relaxed);
            b[PAGES / 64 + page / 64].fetch_or(1 << (page % 64), Relaxed);
        } else {
            b[page / 64].fetch_and(!(1 << (page % 64)), Relaxed);
        }
    }
}

/// Whether the page at `at` can be written now.
fn writable_now(at: usize) -> bool {
    query(at).is_some_and(|i| i.State == MEM_COMMIT && i.Protect & 0xff == PAGE_READWRITE && i.Protect & PAGE_GUARD == 0)
}

/// What the switches ask.
struct Settings {
    file: String,
    at: Vec<u64>,
    frames: u64,
    stores: u32,
    /// Microseconds a frame the refreshes may spend making pages read-only.
    arm_us: u64,
    /// Milliseconds without a frame after which the sample ends.
    stall_ms: u64,
}

/// How a sample is taken (what [`Settings`] asks of it).
#[derive(Clone, Copy)]
struct Limits {
    stores: u32,
    arm_us: u64,
    stall_ms: u64,
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
                arm_us: var("VICTORY_WRITE_CENSUS_ARM_MS").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(8) * 1000,
                stall_ms: var("VICTORY_WRITE_CENSUS_STALL_MS").and_then(|v| v.trim().parse().ok()).unwrap_or(2000).max(100),
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
    /// Questions to Windows about what memory is (once per range a sample).
    queries: u64,
    /// Refreshes whose pages were left as they were: the frame's time for
    /// making pages read-only was spent.
    over_budget: u64,
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

    /// The parts of `[from, to)` no range holds.
    fn gaps(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut at = from;
        // (The one that starts before `from` and the ones that start inside.)
        let before = self.0.range(..from).next_back().map(|(&a, &e)| (a, e));
        for (a, e) in before.into_iter().chain(self.0.range(from..to).map(|(&a, &e)| (a, e))) {
            if e <= at {
                continue;
            }
            if a > at {
                out.push((at, a.min(to)));
            }
            at = at.max(e);
            if at >= to {
                break;
            }
        }
        if at < to {
            out.push((at, to));
        }
        out
    }

    /// `[from, to)` is held by no range any more.
    fn cut(&mut self, from: usize, to: usize) {
        let met: Vec<(usize, usize)> = self
            .0
            .range(..to)
            .rev()
            .take_while(|(_, e)| **e > from)
            .map(|(&a, &e)| (a, e))
            .collect();
        for (a, e) in met {
            self.0.remove(&a);
            if a < from {
                self.0.insert(a, from);
            }
            if e > to {
                self.0.insert(to, e);
            }
        }
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
    /// Memory asked about this sample (`known`), and of it the committed
    /// private read-write runs (start: end, protection), which alone are
    /// made read-only. Windows is asked once per range: in the title's big
    /// pools one question walks page tables for milliseconds, and asking at
    /// every refresh, under the lock every fault waits for, stopped the
    /// title's frames until its watchdog ended it (run census-051943).
    known: Ranges,
    plain: BTreeMap<usize, (usize, u32)>,
    /// Microseconds spent making pages read-only this frame, and the most.
    frame_arm_us: u64,
    arm_us: u64,
    /// Why the sample ended.
    ended: &'static str,
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

/// The protection the page at `at` had where it was made read-only.
fn protection(s: &State, at: usize) -> u32 {
    s.plain.range(..=at).next_back().filter(|(_, (e, _))| *e > at).map_or(PAGE_READWRITE, |(_, &(_, p))| p)
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
        // (A run within one of the runs it was made read-only in: one protection.)
        let at = page * PAGE;
        let run_end = s.plain.range(..=at).next_back().filter(|(_, (e, _))| *e > at).map_or(page + 1, |(_, &(e, _))| e / PAGE);
        let end = next(page, last.min(run_end), false);
        protect(at, (end - page) * PAGE, protection(s, at));
        for p in page..end {
            mark(p, false);
        }
        opened += (end - page) as u64;
        page = next(end, last, true);
    }
    opened
}

/// Asks Windows what `[from, to)` is where it was not asked this sample,
/// and keeps its committed private read-write runs.
fn learn(s: &mut State, from: usize, to: usize, started: std::time::Instant) {
    for (gap, gap_end) in s.known.gaps(from, to) {
        let mut at = gap;
        while at < gap_end {
            if !ACTIVE.load(Relaxed) || s.frame_arm_us + started.elapsed().as_micros() as u64 >= s.arm_us {
                // (The rest is asked about at a later refresh.)
                s.known.add(gap, at);
                s.totals.over_budget += 1;
                return;
            }
            s.totals.queries += 1;
            let Some(info) = query(at) else {
                break;
            };
            let end = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(gap_end);
            if end <= at {
                break;
            }
            let plain = info.State == MEM_COMMIT
                && info.Type == MEM_PRIVATE
                && info.Protect & 0xff == PAGE_READWRITE
                && info.Protect & PAGE_GUARD == 0;
            if plain {
                s.plain.insert(at, (end, info.Protect));
            }
            at = end;
        }
        s.known.add(gap, gap_end);
    }
}

/// Makes the write-watched pages of `[from, to)` that are not read-only yet
/// read-only (committed private read-write pages only), within the frame's
/// time for it.
fn arm(s: &mut State, from: usize, to: usize) {
    if s.frame_arm_us >= s.arm_us {
        s.totals.over_budget += 1;
        return;
    }
    let started = std::time::Instant::now();
    let (from, to) = (from / PAGE * PAGE, to.div_ceil(PAGE).min(PAGES) * PAGE);
    if from < to && !s.kernel.full() {
        learn(s, from, to, started);
        // The plain runs that meet the range (they do not overlap).
        let runs: Vec<(usize, usize, u32)> = s
            .plain
            .range(..to)
            .rev()
            .take_while(|(_, (e, _))| *e > from)
            .map(|(&a, &(e, p))| (a.max(from), e.min(to), p))
            .collect();
        'runs: for (a, e, p) in runs.into_iter().rev() {
            let (last, mut page) = (e / PAGE, next(a / PAGE, e / PAGE, false));
            while page < last {
                if !ACTIVE.load(Relaxed) || s.frame_arm_us + started.elapsed().as_micros() as u64 >= s.arm_us {
                    s.totals.over_budget += 1;
                    break 'runs;
                }
                let end = next(page, last, true);
                if !s.kernel.meets(page * PAGE, end * PAGE) {
                    for q in page..end {
                        mark(q, true);
                    }
                    if protect(page * PAGE, (end - page) * PAGE, read_only(p)) {
                        s.totals.armed_pages += (end - page) as u64;
                    } else {
                        for q in page..end {
                            mark(q, false);
                        }
                        s.totals.refused += 1;
                    }
                }
                page = next(end, last, false);
            }
        }
    }
    let us = started.elapsed().as_micros() as u64;
    s.frame_arm_us += us;
    s.totals.arm_us += us;
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
        untrap(from / PAGE, to.div_ceil(PAGE).min(PAGES));
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
    let end = at.saturating_add(len);
    let (first, last) = (at / PAGE, end.div_ceil(PAGE).min(PAGES));
    let mut page = next(first, last, true);
    while page < last {
        let run = next(page, last, false);
        for p in page..run {
            mark(p, false);
        }
        s.totals.forgotten_pages += (run - page) as u64;
        page = next(run, last, true);
    }
    // What was known of the memory there is asked again when it is refreshed.
    let met: Vec<(usize, usize)> =
        s.plain.range(..end).rev().take_while(|(_, (e, _))| *e > at).map(|(&a, &(e, _))| (a, e)).collect();
    for &(a, _) in &met {
        s.plain.remove(&a);
    }
    let (from, to) = met.iter().fold((at, end), |(f, t), &(a, e)| (f.min(a), t.max(e)));
    s.known.cut(from, to);
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
    if !ever(page) {
        return false;
    }
    // Every protection the census changes is changed under this lock: only
    // under it are the bit and the protection known to agree. (Looked at
    // before it, a page could be opened and made read-only again between
    // the look and the answer: run census-075138's lost fault.)
    let mut guard = state();
    let Some(s) = guard.as_mut() else {
        // (The sample ended meanwhile: its pages are read-write again.)
        return writable_now(at);
    };
    if !armed(page) {
        // Opened meanwhile by another thread: the store is made again.
        return writable_now(at);
    }
    let block = at / BLOCK;
    // Stores counted since the page was copied: none unless the trap flag
    // made it read-only again since.
    let traps = if trapped(page) { s.traps.get(&page).copied().unwrap_or(0) } else { 0 };
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
    protect(page * PAGE, PAGE, protection(s, page * PAGE));
    mark(page, false);
    if s.traps.contains_key(&page) || s.traps.len() < TRAPS {
        s.traps.insert(page, traps + 1);
    }
    mark_trapped(page, false);
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
    #[cfg(test)]
    STEPS.fetch_add(1, Relaxed);
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
        mark(page, true);
        mark_trapped(page, true);
        if !protect(at, PAGE, read_only(protection(s, at))) {
            mark(page, false);
            mark_trapped(page, false);
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

/// Begins a sample.
fn begin(limits: Limits, after: u64) -> bool {
    if !make_bits() {
        return false;
    }
    install();
    let (own, titles) = images();
    let mut s = State {
        stores: limits.stores,
        arm_us: limits.arm_us,
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
    LAST_FRAME_MS.store(now_ms(), Relaxed);
    ACTIVE.store(true, Relaxed);
    watchdog(limits.stall_ms);
    true
}

/// Milliseconds since the first question.
fn now_ms() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// When the last frame of a sample was presented ([`now_ms`]).
static LAST_FRAME_MS: AtomicU64 = AtomicU64::new(0);
/// Samples ended so far (the next one's number).
static MADE: AtomicU64 = AtomicU64::new(0);
/// The longest a sample may take, whatever its frames.
const MOST_MS: u64 = 60_000;

/// A thread of its own ends the sample when no frame came for `stall_ms`
/// (or the sample has lasted [`MOST_MS`]): every page gets its protection
/// back and what was counted is written. The census must never hold the
/// title's frames up for long; this does not wait for a frame to come.
fn watchdog(stall_ms: u64) {
    let began = now_ms();
    std::thread::spawn(move || {
        while ACTIVE.load(Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let now = now_ms();
            let stalled = now.saturating_sub(LAST_FRAME_MS.load(Relaxed)) >= stall_ms;
            if ACTIVE.load(Relaxed) && (stalled || now - began >= MOST_MS) {
                // (Arming in progress sees this and lets go of the lock.)
                ACTIVE.store(false, Relaxed);
                finish(if stalled { "no frame came for the time allowed (VICTORY_WRITE_CENSUS_STALL_MS)" } else { "it lasted a minute" });
                return;
            }
        }
    });
}

/// Ends a sample and writes what it counted, if there is one.
fn finish(why: &'static str) {
    let Some((mut s, ms)) = end() else {
        return;
    };
    s.ended = why;
    let n = MADE.fetch_add(1, Relaxed);
    let Some(settings) = settings() else {
        #[cfg(test)]
        {
            *ENDED.lock().unwrap_or_else(|e| e.into_inner()) = Some((why, s.totals.armed_pages));
        }
        return;
    };
    let file = format!("{}.{n}.txt", settings.file);
    if why != "its frames were taken" {
        tracing::warn!(why, frames = s.frames, "write census sample ended early (VICTORY_WRITE_CENSUS)");
    }
    std::thread::spawn(move || {
        let text = report(&s, ms, &name);
        match std::fs::write(&file, text) {
            Ok(()) => tracing::info!(file, sites = s.sites.len(), "write census written (VICTORY_WRITE_CENSUS)"),
            Err(e) => tracing::warn!(file, %e, "write census not written"),
        }
    });
}

#[cfg(test)]
static STEPS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static ENDED: Mutex<Option<(&'static str, u64)>> = Mutex::new(None);

/// Ends a sample: every page is given its protection back. What was
/// counted, and the milliseconds the sample took.
fn end() -> Option<(State, f64)> {
    let mut guard = state();
    ACTIVE.store(false, Relaxed);
    let s = guard.as_mut()?;
    s.generation += 1;
    let runs: Vec<(usize, usize)> = s.plain.iter().map(|(&a, &(e, _))| (a, e)).collect();
    for (a, e) in runs {
        open_pages(s, a / PAGE, (e / PAGE).min(PAGES));
    }
    let ms = s.began.map_or(0.0, |b| b.elapsed().as_secs_f64() * 1e3);
    let taken = guard.take()?;
    Some((taken, ms))
}

/// A frame was presented: samples begin and end here.
pub(crate) fn frame() {
    static BEGAN: OnceLock<std::time::Instant> = OnceLock::new();
    let began = *BEGAN.get_or_init(std::time::Instant::now);
    let Some(settings) = settings() else {
        return;
    };
    if ACTIVE.load(Relaxed) {
        LAST_FRAME_MS.store(now_ms(), Relaxed);
        let frames = {
            let mut guard = state();
            let Some(s) = guard.as_mut() else {
                return;
            };
            s.frames += 1;
            s.frame_arm_us = 0;
            s.frames
        };
        if frames >= settings.frames {
            finish("its frames were taken");
        }
    } else if let Some(&at) = settings.at.get(MADE.load(Relaxed) as usize) {
        let limits = Limits { stores: settings.stores, arm_us: settings.arm_us, stall_ms: settings.stall_ms };
        if began.elapsed().as_secs() >= at && begin(limits, at) {
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
         the first {} store(s) into a page between two refreshes counted; ended as {}",
        s.frames,
        s.after,
        ms / frames,
        s.stores,
        s.ended
    );
    let _ = writeln!(
        out,
        "making pages read-only: {:.1} ms a frame allowed (VICTORY_WRITE_CENSUS_ARM_MS), {:.1} refreshes a frame left as they were for want of it; \
         {} questions to Windows about memory in the sample",
        s.arm_us as f64 / 1e3,
        per(t.over_budget),
        t.queries
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

    /// The tests that take samples, one at a time (a sample is the process's).
    static ONE: Mutex<()> = Mutex::new(());

    /// Fresh read-write memory below 1 TiB (where console addresses lie and
    /// the census looks; the host's own allocations lie above).
    fn memory(size: usize) -> Option<usize> {
        static NEXT: AtomicUsize = AtomicUsize::new(0x40_0000_0000);
        for _ in 0..4096 {
            let want = NEXT.fetch_add(1 << 30, Relaxed);
            if want + size >= PAGES * PAGE {
                break;
            }
            // SAFETY: a fresh allocation of the test's, where nothing is.
            let at = unsafe { VirtualAlloc(want as *const _, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) } as usize;
            if at != 0 {
                return Some(at);
            }
        }
        panic!("no memory below 1 TiB for the test");
    }

    /// A thread storing all the time into pages that another makes
    /// read-only, opens and ends samples over: every store is done and none
    /// reaches past the handler (a fault passed on ends the test process, as
    /// it ended the title in run census-075138).
    #[test]
    fn stores_racing_the_protections_all_go_through() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let size = 16 * PAGE;
        let at = memory(size).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Relaxed) {
                    for page in 0..16 {
                        // SAFETY: the test's own memory.
                        unsafe { ((at + page * PAGE + (n as usize % 512) * 8) as *mut u64).write_volatile(n) };
                    }
                    n += 1;
                }
                n
            })
        };
        AS_TITLE.set(true);
        for round in 0..200 {
            assert!(begin(Limits { stores: 1 + round % 3, arm_us: u64::MAX / 4, stall_ms: 60_000 }, 0));
            for _ in 0..20 {
                refreshed(at, size, at, at + size, 0, Kind::Watched);
                let s = state();
                if let Some(s) = s.as_ref() {
                    open_pages(s, at / PAGE, (at + size) / PAGE);
                }
            }
            refreshed(at, size, at, at + size, 0, Kind::Watched);
            end().unwrap();
        }
        AS_TITLE.set(false);
        stop.store(true, Relaxed);
        assert!(writer.join().unwrap() > 0);
        for page in 0..16 {
            assert_eq!(protection_at(at + page * PAGE), PAGE_READWRITE, "page {page}");
        }
        // SAFETY: the test's own memory.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// The handler's answer to a fault that it sees only after the page was
    /// opened again (the window of run census-075138): the store is made
    /// again, not passed on. A page the census never had is passed on.
    #[test]
    fn a_fault_seen_after_its_page_opened_is_made_again() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let size = 4 * PAGE;
        let at = memory(size).unwrap();
        let other = memory(size).unwrap();
        assert!(begin(Limits { stores: 1, arm_us: u64::MAX / 4, stall_ms: 60_000 }, 0));
        AS_TITLE.set(true);
        refreshed(at, size, at, at + size, 0, Kind::Watched);
        assert!(armed(at / PAGE + 1));
        open_pages(state().as_ref().unwrap(), at / PAGE, (at + size) / PAGE);
        assert!(!armed(at / PAGE + 1) && ever(at / PAGE + 1));
        // SAFETY: a zeroed context, as the handler gets one.
        let mut context: CONTEXT = unsafe { std::mem::zeroed() };
        assert!(written(&mut context, at + PAGE + 8), "made again");
        assert!(!written(&mut context, other + 8), "not the census's");
        AS_TITLE.set(false);
        end().unwrap();
        // SAFETY: the test's own memory.
        unsafe {
            VirtualFree(at as *mut _, 0, MEM_RELEASE);
            VirtualFree(other as *mut _, 0, MEM_RELEASE);
        }
    }

    /// With no time for it in the frame, nothing is made read-only.
    #[test]
    fn arming_keeps_to_its_time() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let size = 8 * PAGE;
        let at = memory(size).unwrap();
        assert!(begin(Limits { stores: 1, arm_us: 0, stall_ms: 60_000 }, 0));
        AS_TITLE.set(true);
        refreshed(at, size, at, at + size, 0, Kind::Watched);
        assert_eq!(protection_at(at), PAGE_READWRITE);
        assert!(!armed(at / PAGE));
        AS_TITLE.set(false);
        let (s, _) = end().unwrap();
        assert_eq!((s.totals.armed_pages, s.totals.over_budget), (0, 1));
        // SAFETY: the test's own memory.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A sample whose frames stop is ended by its watchdog: every page is
    /// read-write again, without a frame coming (run census-051943 hung
    /// with its pages read-only).
    #[test]
    fn a_sample_without_frames_ends_by_itself() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let size = 8 * PAGE;
        let at = memory(size).unwrap();
        *ENDED.lock().unwrap() = None;
        assert!(begin(Limits { stores: 1, arm_us: u64::MAX / 4, stall_ms: 200 }, 0));
        AS_TITLE.set(true);
        refreshed(at, size, at, at + size, 0, Kind::Watched);
        AS_TITLE.set(false);
        assert_eq!(protection_at(at) & 0xff, PAGE_READONLY);
        let began = std::time::Instant::now();
        while ACTIVE.load(Relaxed) && began.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!ACTIVE.load(Relaxed), "the watchdog ended it");
        assert!(began.elapsed().as_millis() >= 100, "not before the time allowed: {:?}", began.elapsed());
        while ENDED.lock().unwrap().is_none() && began.elapsed().as_secs() < 5 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (why, armed_pages) = ENDED.lock().unwrap().unwrap();
        assert!(why.starts_with("no frame came"), "{why}");
        assert_eq!(armed_pages, 8);
        for page in 0..8 {
            assert_eq!(protection_at(at + page * PAGE), PAGE_READWRITE, "page {page}");
            assert!(!armed(at / PAGE + page));
        }
        // SAFETY: the test's own memory.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// Stores into pages a refresh took fault once each, are noted and go
    /// on; a refresh takes them again; a kernel write opens its range; the
    /// end gives every page its protection back.
    #[test]
    fn stores_into_copied_pages_are_counted_and_done() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: fresh memory of the test's, written by it alone.
        unsafe {
            let size = 16 * PAGE;
            let at = memory(size).unwrap();
            assert!(begin(Limits { stores: 3, arm_us: u64::MAX / 4, stall_ms: 60_000 }, 0));
            AS_TITLE.set(true);
            refreshed(at, size, at, at + size, 123, Kind::Watched);
            assert_eq!(protection_at(at) & 0xff, PAGE_READONLY);
            assert!(armed(at / PAGE + 2));
            let asked = state().as_ref().unwrap().totals.queries;
            assert!(asked >= 1);
            let p = at as *mut u64;
            // Page 2: one store. Page 5: five stores, three of them counted.
            p.add(2 * PAGE / 8).write_volatile(7);
            for i in 0..5 {
                p.add(5 * PAGE / 8 + i).write_volatile(i as u64);
            }
            assert_eq!(p.add(2 * PAGE / 8).read_volatile(), 7);
            assert_eq!(p.add(5 * PAGE / 8 + 4).read_volatile(), 4);
            // Page 2 is read-only again behind its one store (two more may be
            // counted); page 5 was left open after its third.
            assert_eq!(protection_at(at + 2 * PAGE) & 0xff, PAGE_READONLY);
            assert!(armed(at / PAGE + 2) && trapped(at / PAGE + 2));
            assert_eq!(protection_at(at + 5 * PAGE), PAGE_READWRITE);
            assert!(!armed(at / PAGE + 5) && armed(at / PAGE + 3));
            {
                let guard = state();
                let s = guard.as_ref().unwrap();
                assert_eq!(s.totals.first, 2);
                assert_eq!(s.totals.stores, 4, "steps {}", STEPS.load(Relaxed));
                let site = s.sites.values().map(|x| x.bytes).sum::<u64>();
                assert_eq!(site, 4 * 8);
                assert_eq!(s.regions[0].copied, 123);
            }
            // Taken again: page 2 faults once more as a first store.
            refreshed(at, size, at, at + size, 0, Kind::Watched);
            assert!(armed(at / PAGE + 2));
            // Taking pages again asks Windows nothing more: asking at every
            // refresh, under the lock faults wait for, hung run census-051943.
            assert_eq!(state().as_ref().unwrap().totals.queries, asked);
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
        assert_eq!(r.gaps(0, 500), vec![(0, 50), (450, 500)]);
        assert!(r.gaps(60, 70).is_empty());
        r.cut(100, 200);
        assert_eq!(r.0.iter().collect::<Vec<_>>(), vec![(&50, &100), (&200, &450)]);
        assert_eq!(r.gaps(0, 300), vec![(0, 50), (100, 200)]);
        r.cut(0, 1000);
        assert!(r.0.is_empty());
    }
}
