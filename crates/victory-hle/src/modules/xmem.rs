//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_FREE,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    virtual_alloc(base, size, kind, flags, protect, 0)
}

/// [`XMemVirtualAlloc`], at an address aligned to `align` as well (0, or a
/// power of two: what the host's allocation granularity gives anyway is
/// not asked for).
fn virtual_alloc(base: *mut c_void, size: usize, kind: u32, flags: u64, protect: u32, align: usize) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let pages = if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M { 2 * MIB } else { 0 };
    let align = pages.max(align);
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if align > PAGE { align } else { 0 },
    };
    let asked = protect;
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (the title's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if at.is_null() {
        let error = std::io::Error::last_os_error();
        if kind & MEM_COMMIT != 0 {
            commit_refused(size, "virtual memory", error);
        } else {
            tracing::warn!(
                size,
                kind = format_args!("{kind:#x}"),
                flags = format_args!("{flags:#x}"),
                %error,
                "XMemVirtualAlloc refused"
            );
        }
    }
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    if !at.is_null() && !placeholder {
        victory_gpu::title_memory_allocated(at as usize, size);
    }
    // (Grand Theft Auto VI asks for its 32-bit heap with kind 0x42000,
    // without the 64 KiB page bits: not `placeholder` above, though the host
    // makes a placeholder of it all the same.)
    if !at.is_null() && constrained && kind & MEM_COMMIT == 0 && flags & ADDRESS_32BIT != 0 {
        LOW_RESERVATIONS.lock().unwrap_or_else(|e| e.into_inner()).push((at as usize, size));
    }
    note_protection(asked, kind, flags, at as usize, size);
    at
}

/// What the title asks its memory to be (the console's protection, with the
/// bits that say how each processor reaches it: 0x200 uncached, 0x400
/// write-combined, 0x800 and on the GPU's): said for the 1st, 2nd, 4th.. time of
/// each, with how much so far. (For placing it: memory the CPU reaches
/// uncached or write-combined is memory it does not read.)
fn note_protection(protect: u32, kind: u32, flags: u64, at: usize, size: usize) {
    type Seen = std::collections::HashMap<(u32, u32, u64), (u64, usize)>;
    static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
    let (n, bytes) = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let e = seen.get_or_insert_with(Default::default).entry((protect, kind, flags)).or_default();
        *e = (e.0 + 1, e.1 + size);
        *e
    };
    if n.is_power_of_two() {
        tracing::info!(
            "title memory asked for as protection {protect:#x}, kind {kind:#x}, flags {flags:#x}: {n} times, {} MB, last {at:#x}+{size:#x}",
            bytes >> 20
        );
    }
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (the title's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (the title does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        // Whole allocations: 64 KiB aligned by the host; an alignment above
        // that is asked for with the address range.
        return virtual_alloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
            1 << (attributes >> ALIGNMENT_SHIFT & 0x1f),
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (the title uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        tracing::warn!(size, "XMemAllocDefault: the host heap is out of memory");
        return block.cast();
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            VirtualFree(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

struct Section {
    handle: HANDLE,
    pages: usize,
    /// A view of the whole section, kept ([`Section::window`]); 0 if the
    /// host gave none.
    window: std::sync::OnceLock<usize>,
    /// A bit per page: committed, as far as [`Section::window`] asked.
    committed: Box<[std::sync::atomic::AtomicU64]>,
    /// (`VICTORY_POOL_DISCARD`) A bit per page: written through
    /// [`Section::window`] and not discarded since (resident in our view).
    resident: Box<[std::sync::atomic::AtomicU64]>,
    /// (`VICTORY_POOL_DISCARD`) A bit per page: discarded and not written since.
    discarded: Box<[std::sync::atomic::AtomicU64]>,
}

impl Section {
    /// Where bytes of `n` pages from page `index` on can be written,
    /// whether or not the pages are mapped anywhere, committed.
    ///
    /// Every DirectStorage write mapped a view of its pages, committed it,
    /// faulted each of its 4 KiB pages in and unmapped it again, all of it
    /// under the process's one address-space lock, with 24 workers, the
    /// title's own mapping calls (about 100,000 views of one page) and our
    /// guards asking for the same lock. One view of the section, kept,
    /// costs that once per page.
    /// `VICTORY_SECTION_WINDOW=0`: a view for each write, as before.
    fn window(&self, index: usize, n: usize) -> Option<usize> {
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        static ON: LazyLock<bool> =
            LazyLock::new(|| std::env::var_os("VICTORY_SECTION_WINDOW").is_none_or(|v| v != "0"));
        if !*ON || index + n > self.pages {
            return None;
        }
        let base = *self.window.get_or_init(|| {
            // SAFETY: the whole of a section we own.
            unsafe { MapViewOfFile(self.handle, FILE_MAP_WRITE, 0, 0, self.pages * PAGE) }.Value as usize
        });
        if base == 0 {
            return None;
        }
        let missing = |page: usize| self.committed[page / 64].load(Ordering::Acquire) >> (page % 64) & 1 == 0;
        let mut page = index;
        while page < index + n {
            if !missing(page) {
                page += 1;
                continue;
            }
            let run = (page..index + n).take_while(|&p| missing(p)).count();
            if run == 0 {
                continue; // another worker committed it meanwhile
            }
            if !commit(base + page * PAGE, run * PAGE) {
                return None;
            }
            for p in page..page + run {
                self.committed[p / 64].fetch_or(1 << (p % 64), Ordering::AcqRel);
            }
            page += run;
        }
        pool_discard::written(self, index, n);
        Some(base + index * PAGE)
    }
}

impl Section {
    /// Commits the pages from `index` on that a view at `base` shows, unless
    /// all of them are ([`lean`]): committed for one view is for all.
    fn commit_shown(&self, index: usize, base: usize, len: usize) -> bool {
        use std::sync::atomic::Ordering;
        let pages = index..(index + len / PAGE).min(self.pages);
        let bit = |p: usize| (&self.committed[p / 64], 1 << (p % 64));
        let done = || {
            pages.clone().all(|p| {
                let (word, bit) = bit(p);
                word.load(Ordering::Acquire) & bit != 0
            })
        };
        if lean() && done() {
            return true;
        }
        if !counted(Call::Commit, || commit(base, len)) {
            return false;
        }
        for p in pages {
            let (word, bit) = bit(p);
            word.fetch_or(bit, Ordering::AcqRel);
        }
        true
    }
}

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        if let Some(&base) = self.window.get()
            && base != 0
        {
            // SAFETY: our view; whoever writes through it holds the section.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: base as *mut c_void,
                })
            };
        }
        victory_gpu::title_memory_section_gone(self.handle as usize);
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

#[derive(Default)]
struct Physical {
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Placeholders that are some view's own, left by its unmapping: address
    /// and length. The title may have joined one with others since.
    holes: HashMap<usize, usize>,
}

impl Physical {
    /// What the page at `at` shows.
    fn slice_at(&self, at: usize) -> Option<Slice> {
        self.views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
    }
}

/// The title's pages by number.
struct Pages {
    numbers: Numbers,
    pages: HashMap<usize, Slice>,
}

/// Page numbers: handed out from `next` up, and given back by
/// [`XMemFreePhysicalPages`] to be handed out again. The title keeps a
/// number in 21 bits ([`FRAMES`]), 128 GiB of pages: a long session that
/// allocates and frees its pages over and over would run past that if
/// numbers were never used again.
struct Numbers {
    next: usize,
    /// Numbers given back and not handed out since: first and how many,
    /// neighbours joined, none of them ending at `next`.
    free: BTreeMap<usize, usize>,
}

impl Numbers {
    const fn new(first: usize) -> Numbers {
        Numbers { next: first, free: BTreeMap::new() }
    }

    /// `len` consecutive numbers: the first stretch given back that is long
    /// enough, or new ones.
    fn take(&mut self, len: usize) -> usize {
        if let Some((&start, &have)) = self.free.iter().find(|&(_, &have)| have >= len) {
            self.free.remove(&start);
            if have > len {
                self.free.insert(start + len, have - len);
            }
            return start;
        }
        let first = self.next;
        self.next += len;
        first
    }

    /// Gives `[start, start + len)` back; each number must have been handed
    /// out and not given back since.
    fn give(&mut self, mut start: usize, mut len: usize) {
        if let Some((&before, &n)) = self.free.range(..start).next_back()
            && before + n == start
        {
            self.free.remove(&before);
            start = before;
            len += n;
        }
        if let Some(n) = self.free.remove(&(start + len)) {
            len += n;
        }
        if start + len == self.next {
            self.next = start;
        } else {
            self.free.insert(start, len);
        }
    }
}

/// Where the title sees its pages: [`Physical::views`] the other way.
/// Locked apart from them, and only for a moment ([`PAGES`]).
#[derive(Default)]
struct Shown {
    /// The addresses that show a page, by the page's section and index.
    /// Views of more than [`SHORT`] pages are in `long`.
    pages: HashMap<(usize, usize), Vec<usize>>,
    /// Address, bytes, section, first page.
    long: Vec<(usize, usize, usize, usize)>,
}

/// Pages of a view noted page by page in [`Shown::pages`].
const SHORT: usize = 64;

/// Taken after [`PHYSICAL`] where both are.
static SHOWN: LazyLock<RwLock<Shown>> = LazyLock::new(Default::default);

fn shown_mut() -> RwLockWriteGuard<'static, Shown> {
    SHOWN.write().unwrap_or_else(|e| e.into_inner())
}

impl Shown {
    fn note(&mut self, base: usize, len: usize, slice: &Slice) {
        let section = Arc::as_ptr(&slice.section) as usize;
        if len / PAGE > SHORT {
            self.long.push((base, len, section, slice.index));
            return;
        }
        for k in 0..len / PAGE {
            self.pages.entry((section, slice.index + k)).or_default().push(base + k * PAGE);
        }
    }

    fn forget(&mut self, base: usize, len: usize, slice: &Slice) {
        if len / PAGE > SHORT {
            self.long.retain(|view| view.0 != base);
            return;
        }
        let section = Arc::as_ptr(&slice.section) as usize;
        for k in 0..len / PAGE {
            let key = (section, slice.index + k);
            if let Some(at) = self.pages.get_mut(&key) {
                at.retain(|&at| at != base + k * PAGE);
                if at.is_empty() {
                    self.pages.remove(&key);
                }
            }
        }
    }

    /// Where the title sees the page at `slice`: addresses and bytes.
    fn at(&self, slice: &Slice) -> Vec<(usize, usize)> {
        let section = Arc::as_ptr(&slice.section) as usize;
        let mut out: Vec<(usize, usize)> =
            self.pages.get(&(section, slice.index)).into_iter().flatten().map(|&at| (at, PAGE)).collect();
        for &(base, len, of, index) in &self.long {
            if of == section && (index..index + len / PAGE).contains(&slice.index) {
                out.push((base + (slice.index - index) * PAGE, PAGE));
            }
        }
        out
    }
}

/// See [`pages_written`].
pub(crate) fn stream_notice() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("VICTORY_STREAM_NOTICE").is_none_or(|v| v != "0"));
    *ON
}

/// The views, changed by the title's mapping calls; held for as long as
/// those take (the host maps, commits and unmaps under it).
static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(Default::default);

fn physical() -> MutexGuard<'static, Physical> {
    let mut physical = PHYSICAL.lock().unwrap_or_else(|e| e.into_inner());
    // Placeholders the title released are no view's own any more.
    if ANY_FREED.swap(false, Ordering::Acquire) {
        let freed = std::mem::take(&mut *FREED.lock().unwrap_or_else(|e| e.into_inner()));
        for (at, len) in freed {
            physical.holes.retain(|&hole, &mut bytes| hole + bytes <= at || at + len <= hole);
        }
    }
    physical
}

/// The title's pages. Apart from the views: DirectStorage looks pages up at
/// every request, from 24 workers, and used to wait there for the title's
/// mapping calls (12,000 waits of 1 to 170 ms in a load of 90 s). Written
/// only where the title allocates or frees pages. Taken after [`PHYSICAL`]
/// where both are.
static PAGES: LazyLock<RwLock<Pages>> = LazyLock::new(|| {
    RwLock::new(Pages {
        // Page numbers are opaque to titles; 0 stays unused.
        numbers: Numbers::new(0x10_0000),
        pages: HashMap::new(),
    })
});

fn page_table() -> RwLockReadGuard<'static, Pages> {
    PAGES.read().unwrap_or_else(|e| e.into_inner())
}

fn page_table_mut() -> RwLockWriteGuard<'static, Pages> {
    PAGES.write().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    let Some(size) = n.checked_mul(PAGE).filter(|&s| s > 0) else {
        return 0;
    };
    // A pagefile-backed section of `size` bytes, its pages committed when
    // first mapped or written (`commit`): the title asks for 6.1
    // GiB at once, which a busy host often cannot commit (error 1450).
    // SAFETY: plain section creation.
    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            // Under Wine a committed section is a sparse file, free until
            // touched, and committing a reserved one page by page costs a
            // wineserver request each that scans every range committed so
            // far: commit it whole there.
            PAGE_READWRITE | if on_wine() { SEC_COMMIT } else { SEC_RESERVE },
            (size as u64 >> 32) as u32,
            size as u32,
            std::ptr::null(),
        )
    };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
        return 0;
    }
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    // For the GPU side's in-place census: only a committed section can be
    // opened as a host heap.
    victory_gpu::title_memory_section(handle as usize, size as u64, on_wine());
    let section = Arc::new(Section {
        handle,
        pages: n,
        window: std::sync::OnceLock::new(),
        committed: (0..n.div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
        resident: (0..n.div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
        discarded: (0..n.div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
    });
    let mut table = page_table_mut();
    let first = table.numbers.take(n * FRAMES);
    for index in 0..n {
        let slice = Slice {
            section: section.clone(),
            index,
        };
        table.pages.insert(first + index * FRAMES, slice);
    }
    // Title memory is touched without our locks: a fault there may wait for
    // a mapping call, which takes them.
    drop(table);
    for index in 0..n {
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(first + index * FRAMES) };
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    // SAFETY: the title passes `n` page numbers.
    let numbers = unsafe { std::slice::from_raw_parts(pages, n) }.to_vec();
    let mut table = page_table_mut();
    // Dropped without the lock: the last page of a section unmaps its window.
    let mut gone: Vec<Slice> = Vec::with_capacity(numbers.len());
    for number in &numbers {
        // Only numbers the title owned: one given twice, or never handed
        // out, must not be handed out twice.
        if let Some(slice) = table.pages.remove(number) {
            table.numbers.give(*number, FRAMES);
            gone.push(slice);
        }
    }
    drop(table);
    drop(gone);
    1
}

/// A view of our own on consecutive slices of one section, committed and
/// writable, unmapped when dropped.
struct View {
    at: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let (offset, len) = ((slice.index * PAGE) as u64, n * PAGE);
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.handle,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            tracing::warn!(error = %std::io::Error::last_os_error(), n, "physical pages not mapped for a write");
            return None;
        }
        let view = View { at, len };
        if !commit(at.Value as usize, len) {
            tracing::warn!(error = %std::io::Error::last_os_error(), n, "physical pages not committed for a write");
            return None;
        }
        Some(view)
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the view holds `len` committed read-write bytes, ours
        // alone while `self` lives (other views of the slices only alias
        // them, as the console's do).
        unsafe { std::slice::from_raw_parts_mut(self.at.Value.cast(), self.len) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::UnmapViewOfFile;
        // SAFETY: our view.
        unsafe { UnmapViewOfFile(self.at) };
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let table = page_table();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let Some(slice) = table.pages.get(page) else {
            tracing::warn!(page, offset, len, "a physical page the title does not own (any more)");
            return None;
        };
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    Some(runs)
}

/// Whether the title owns every page under bytes `[offset, offset + len)`.
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    let table = page_table();
    pages
        .get(offset / PAGE..(offset + len).div_ceil(PAGE))
        .is_some_and(|pages| pages.iter().all(|page| table.pages.contains_key(page)))
}

/// Where the title sees the pages under bytes `[offset, offset + len)` now,
/// in order (for the streaming trace: whether that changed while a request
/// wrote them).
pub(crate) fn pages_shown(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let numbers = pages.get(offset / PAGE..(offset + len).div_ceil(PAGE)).unwrap_or_default();
    let slices: Vec<Slice> = {
        let table = page_table();
        numbers.iter().filter_map(|n| table.pages.get(n)).cloned().collect()
    };
    let shown = SHOWN.read().unwrap_or_else(|e| e.into_inner());
    let mut at: Vec<(usize, usize)> = slices.iter().flat_map(|slice| shown.at(slice)).collect();
    at.sort_unstable();
    at
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len()) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let k = (n * PAGE - within).min(data.len() - done);
        // (`VICTORY_POOL_DISCARD`) Until the bytes are there.
        let _writing = pool_discard::writing();
        if let Some(at) = slice.section.window(slice.index, *n) {
            // SAFETY: committed bytes of the section's view, which `runs`
            // holds; the title's other uses of them race as on the console.
            unsafe { std::ptr::copy_nonoverlapping(data[done..].as_ptr(), (at + within) as *mut u8, k) };
        } else {
            let Some(mut view) = View::map(slice, *n) else {
                return false;
            };
            view.bytes()[within..within + k].copy_from_slice(&data[done..done + k]);
        }
        (within, done) = (0, done + k);
    }
    true
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: Window,
    within: usize,
    len: usize,
    /// (`VICTORY_POOL_DISCARD`) No discard of the pages while the window
    /// is written; drop the window once the bytes are there.
    _writing: Option<std::sync::RwLockReadGuard<'static, ()>>,
}

enum Window {
    /// In the section's own view, which the section holds.
    Kept(usize, #[expect(dead_code, reason = "held for the view")] Arc<Section>),
    Own(View),
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        let (within, len) = (self.within, self.len);
        match &mut self.view {
            // SAFETY: committed bytes of the section's view, ours alone as
            // far as the runtime goes (other views alias them, as the
            // console's do).
            Window::Kept(at, _) => unsafe { std::slice::from_raw_parts_mut((*at + within) as *mut u8, len) },
            Window::Own(view) => &mut view.bytes()[within..within + len],
        }
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    let writing = pool_discard::writing();
    let view = match slice.section.window(slice.index, *n) {
        Some(at) => Window::Kept(at, slice.section.clone()),
        None => Window::Own(View::map(slice, *n)?),
    };
    Some(PageWindow {
        view,
        within: offset % PAGE,
        len,
        _writing: writing,
    })
}

/// The title addresses that show bytes `[offset, offset + len)` of `pages`
/// now, byte for byte, as runs (address, bytes) in address order: a write
/// to the pages through a view of our own ([`page_window`],
/// [`write_physical`]) lands there too, and nothing that watches those
/// addresses sees it. Empty if no view shows them. Byte for byte, not by
/// whole pages: the title packs several small textures into one page and
/// fills each with a request of its own.
pub(crate) fn shown_at(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let Some(end) = offset.checked_add(len) else {
        return Vec::new();
    };
    let first = offset / PAGE;
    let Some(touched) = pages.get(first..end.div_ceil(PAGE)) else {
        return Vec::new();
    };
    let slices: Vec<Option<Slice>> = {
        let table = page_table();
        touched.iter().map(|page| table.pages.get(page).cloned()).collect()
    };
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    {
        let shown = SHOWN.read().unwrap_or_else(|e| e.into_inner());
        for (i, slice) in slices.iter().enumerate() {
            let Some(slice) = slice else { continue };
            // The bytes of this page that the write covers.
            let lo = if i == 0 { offset % PAGE } else { 0 };
            let hi = (end - (first + i) * PAGE).min(PAGE);
            pieces.extend(shown.at(slice).into_iter().map(|(at, _)| (at + lo, hi - lo)));
        }
    }
    pieces.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (at, bytes) in pieces {
        match runs.last_mut() {
            Some((start, n)) if *start + *n == at => *n += bytes,
            _ => runs.push((at, bytes)),
        }
    }
    runs
}

/// DirectStorage has put bytes `[offset, offset + len)` of `pages` into
/// memory, through a view of our own: the GPU side is told where the title
/// sees them, so that what it copied there before they arrived (a texture
/// uploaded when its view was written holds whatever the pages held then)
/// is copied again. Call when the bytes are there.
/// `VICTORY_STREAM_NOTICE=0`: not told; what is then done is
/// `VICTORY_PAGE_WRITES`' to say (`victory_gpu::title_memory_written`).
pub(crate) fn pages_written(pages: &[usize], offset: usize, len: usize) {
    if len == 0 {
        return;
    }
    for (at, bytes) in shown_at(pages, offset, len) {
        if stream_notice() {
            victory_gpu::kernel_will_write(at, bytes);
        } else {
            victory_gpu::title_memory_written(at, bytes, true);
        }
    }
}

/// What the title's mapping calls cost since the last asking, for the
/// streaming log, counted only while that log is on: calls, pages,
/// milliseconds in all, the longest one's and waiting for the lock; the same
/// of the GPU's page-mapping copies ([`map_alias`]; calls and milliseconds);
/// and by system call (unmap, split, map, commit) how many and their
/// milliseconds.
static MAPPING: [std::sync::atomic::AtomicU64; 15] = [const { std::sync::atomic::AtomicU64::new(0) }; 15];

pub(crate) fn map_costs() -> Option<String> {
    let n: [u64; 15] = std::array::from_fn(|i| MAPPING[i].swap(0, Ordering::Relaxed));
    let time = |i: usize| matches!(i, 2 | 3 | 4 | 6 | 8 | 10 | 12 | 14);
    (n[0] + n[5] > 0).then(|| {
        let all: Vec<String> = n
            .iter()
            .enumerate()
            .map(|(i, &n)| if time(i) { n / 1_000_000 } else { n })
            .map(|n| n.to_string())
            .collect();
        all.join("\t")
    })
}

/// The system calls that change a mapping, as [`MAPPING`] counts them.
#[derive(Clone, Copy)]
enum Call {
    Unmap = 7,
    Split = 9,
    Map = 11,
    Commit = 13,
}

/// Runs `f`, a system call, and counts it while the streaming log is on.
fn counted<T>(call: Call, f: impl FnOnce() -> T) -> T {
    if !super::dstorage::stream_log::on() {
        return f();
    }
    let started = std::time::Instant::now();
    let done = f();
    MAPPING[call as usize].fetch_add(1, Ordering::Relaxed);
    MAPPING[call as usize + 1].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    done
}

/// Counts a mapping call when dropped.
struct Costed {
    started: std::time::Instant,
    /// `None`: a copy of mappings.
    pages: Option<usize>,
    waited: u64,
}

impl Costed {
    fn begin(pages: Option<usize>) -> Option<Costed> {
        super::dstorage::stream_log::on().then(|| Costed { started: std::time::Instant::now(), pages, waited: 0 })
    }
}

impl Drop for Costed {
    fn drop(&mut self) {
        let ns = self.started.elapsed().as_nanos() as u64;
        let Some(pages) = self.pages else {
            MAPPING[5].fetch_add(1, Ordering::Relaxed);
            MAPPING[6].fetch_add(ns, Ordering::Relaxed);
            return;
        };
        for (i, n) in [1, pages as u64, ns].into_iter().enumerate() {
            MAPPING[i].fetch_add(n, Ordering::Relaxed);
        }
        MAPPING[3].fetch_max(ns, Ordering::Relaxed);
        MAPPING[4].fetch_add(self.waited, Ordering::Relaxed);
    }
}

/// Whether a mapping change asks the host only for what it needs: no split
/// of a placeholder that is already the view's own, no commit of section
/// pages that are committed (`VICTORY_MAP_LEAN=0`: both every time, as
/// before).
///
/// All of the title's streaming goes through one thread, which maps the
/// pages of a batch and then submits it. Each call waits for the process's
/// one address-space lock, behind the guards' and the driver's: 200 to 400
/// us a page in the world against 10 in a menu, 2 to 4 s of every 5 s of
/// that thread (runs stream-1 and -2; all six stacks taken of it in the
/// opening were in these calls), so that models came as fast as their pages
/// could be mapped. A page took up to four calls (unmap, split, map,
/// commit), twice: the title maps a page into its pool and then has the GPU
/// map it into its tiled heap.
fn lean() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VICTORY_MAP_LEAN").is_none_or(|v| v != "0"));
    *ON
}

/// The title's streaming thread runs below normal priority, as do sixteen
/// workers of ours when pipelines are made; on the console nothing else
/// wants its core. Called where that thread maps and submits: lifts a
/// thread that is below normal to above it (`VICTORY_STREAM_PRIORITY=0`:
/// left as the title set it).
pub(crate) fn lift_thread() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("VICTORY_STREAM_PRIORITY").is_none_or(|v| v != "0"));
    if !*ON {
        return;
    }
    // SAFETY: plain calls on this thread.
    unsafe {
        let thread = GetCurrentThread();
        if matches!(GetThreadPriority(thread), -2..0) {
            SetThreadPriority(thread, THREAD_PRIORITY_ABOVE_NORMAL);
        }
    }
}

/// Renderer port 1 (`VICTORY_PORT_CONSTANTS=1`; the GPU side's half is
/// `victory_gpu`'s `port`): read here for xmem's part, [`arrange`].
fn port_on() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var("VICTORY_PORT_CONSTANTS").is_ok_and(|v| v.trim() == "1"));
    *ON
}

/// The title's reservations of 64 KiB pages for 32-bit graphics memory:
/// base and bytes (one, of 185 MB, in Grand Theft Auto VI).
static LOW_RESERVATIONS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Pages given another place in their section so far by [`arrange`], and
/// the times it found a page that was not fresh.
static ARRANGED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Before `pages` are mapped at `base`: if `base` lies in a 32-bit graphics
/// reservation ([`LOW_RESERVATIONS`]) whose pages are all one section of
/// the same size, the page numbers are given the slices of that section in
/// the reservation's address order (each page at its offset in the
/// reservation), so that the reservation becomes one stretch of the section
/// and the GPU side can open it as one heap.
///
/// Grand Theft Auto VI takes all the pages of its 185 MB heap in one call
/// and maps its five pieces in another order than their addresses (run
/// port1-1: "Scattered"). Only fresh slices are exchanged: never mapped,
/// never written (no commit bit), so every page still reads as zeros and
/// which slice a number names is not observable by the title.
fn arrange(base: usize, pages: &[usize]) {
    let Some((first, bytes)) = LOW_RESERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .find(|&(at, len)| at <= base && base + pages.len() * PAGE <= at + len)
    else {
        return;
    };
    let mut table = page_table_mut();
    let Some(section) = pages.first().and_then(|p| table.pages.get(p)).map(|s| s.section.clone()) else {
        return;
    };
    if section.pages * PAGE < bytes
        || !pages.iter().all(|p| table.pages.get(p).is_some_and(|s| Arc::ptr_eq(&s.section, &section)))
    {
        static SAID: AtomicU64 = AtomicU64::new(0);
        if SAID.fetch_add(1, Ordering::Relaxed) < 4 {
            tracing::warn!(
                at = format_args!("{base:#x}"),
                pages = pages.len(),
                section_pages = section.pages,
                reservation = bytes / PAGE,
                "renderer port 1: pages mapped into the 32-bit heap are not of one section as big as it: left where they are"
            );
        }
        return;
    }
    let fresh = |index: usize| section.committed[index / 64].load(Ordering::Acquire) >> (index % 64) & 1 == 0;
    // Which number has which slice of the section.
    let mut owner: HashMap<usize, usize> = table
        .pages
        .iter()
        .filter(|(_, s)| Arc::ptr_eq(&s.section, &section))
        .map(|(&number, s)| (s.index, number))
        .collect();
    let mut moved = 0u64;
    for (i, number) in pages.iter().enumerate() {
        let want = (base - first) / PAGE + i;
        let Some(have) = table.pages.get(number).map(|s| s.index) else {
            continue;
        };
        if have == want {
            continue;
        }
        if !fresh(have) || !fresh(want) {
            ARRANGED[1].fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if let Some(other) = owner.get(&want).copied()
            && let Some(slice) = table.pages.get_mut(&other)
        {
            slice.index = have;
            owner.insert(have, other);
        }
        if let Some(slice) = table.pages.get_mut(number) {
            slice.index = want;
        }
        owner.insert(want, *number);
        moved += 1;
    }
    if moved > 0 {
        let all = ARRANGED[0].fetch_add(moved, Ordering::Relaxed) + moved;
        tracing::info!(
            at = format_args!("{base:#x}"),
            pages = pages.len(),
            moved,
            all,
            not_fresh = ARRANGED[1].load(Ordering::Relaxed),
            "renderer port 1: the title's 32-bit graphics pages given their section's slices in address order"
        );
    }
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let base = address as usize;
    // Read before any lock is taken: a fault in title memory may wait for
    // the mapping call that holds them.
    // SAFETY: the title passes `n` page numbers, or null.
    let pages = (!pages.is_null()).then(|| unsafe { std::slice::from_raw_parts(pages, n) }.to_vec());
    if port_on()
        && let Some(pages) = &pages
    {
        arrange(base, pages);
    }
    let mut costed = Costed::begin(Some(n));
    let _called = victory_gpu::timeline::span("the title's XMemMapPhysicalPages");
    let _busy = Busy::begin();
    let mut physical = physical();
    if let Some(costed) = &mut costed {
        costed.waited = costed.started.elapsed().as_nanos() as u64;
    }
    // Every page is looked up before anything is unmapped: a call with a
    // page the title does not own fails and leaves what is mapped there as
    // it was, instead of taking the old views away and mapping half of the
    // new ones.
    let slices: Option<Vec<Slice>> = match &pages {
        None => None,
        Some(pages) => {
            let table = page_table();
            let mut slices = Vec::with_capacity(pages.len());
            for page in pages {
                let Some(slice) = table.pages.get(page) else {
                    tracing::warn!(page, "XMemMapPhysicalPages: unknown page");
                    return std::ptr::null_mut();
                };
                slices.push(slice.clone());
            }
            Some(slices)
        }
    };
    // Until the pages are mapped again: `unmap` takes whole views away and
    // maps what stays of them again, and a mapped view is a reservation
    // until it is committed. A read of title memory in place must not fall
    // into that gap (`victory_gpu::title_memory_remapping`).
    let _changing = victory_gpu::title_memory_remapping();
    unmap(&mut physical, base, n * PAGE);
    let Some(slices) = slices else {
        return address;
    };
    let mut i = 0;
    while i < n {
        let first = slices[i].clone();
        // One view per run of consecutive slices of one section.
        let run = 1 + slices[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(s, k)| Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k)
            .count();
        if !map(&mut physical, base + i * PAGE, run * PAGE, first) {
            return std::ptr::null_mut();
        }
        i += run;
    }
    address
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    lift_thread();
    // Read before any lock is taken, as in `XMemMapPhysicalPages`.
    let wanted: Vec<(usize, usize)> = (0..n)
        // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
        .map(|i| unsafe { (*addresses.add(i) as usize, if pages.is_null() { 0 } else { *pages.add(i) }) })
        .collect();
    // Every page is looked up before anything is unmapped, as in
    // `XMemMapPhysicalPages`: a call with a page the title does not own
    // fails and leaves every address showing what it showed.
    {
        let table = page_table();
        if let Some(&(_, page)) = wanted.iter().find(|&&(_, page)| page != 0 && !table.pages.contains_key(&page)) {
            tracing::warn!(page, "XMemMapPhysicalPagesScatter: unknown page");
            return 0;
        }
    }
    if side_by_side() > 1 && n > 1 && wanted.iter().all(|w| w.0 % PAGE == 0) {
        return scatter(&wanted).into();
    }
    for &(address, page) in &wanted {
        let page = if page == 0 { std::ptr::null() } else { &raw const page };
        if XMemMapPhysicalPages(address as *mut c_void, 1, page).is_null() {
            return 0;
        }
    }
    1
}

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`):
/// the title maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let _costed = Costed::begin(None);
    alias(to, from, len)
}

/// [`map_alias`], not counted as a call of its own.
fn alias(to: u64, from: u64, len: u64) -> bool {
    alias_as(to, from, len, map_runs())
}

/// [`alias`]; `runs_in_one`: a view per run of consecutive section pages
/// ([`map_runs`]), else a view per page, as before.
fn alias_as(to: u64, from: u64, len: u64, runs_in_one: bool) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let _busy = Busy::begin();
    let mut physical = physical();
    // As in `XMemMapPhysicalPages`: no read in place meets the gap.
    let _changing = victory_gpu::title_memory_remapping();
    unmap(&mut physical, to, len);
    if from == 0 {
        return true;
    }
    // Off, or a range mapped onto itself (shifted): looked up page by page
    // as it changes, as before.
    if !runs_in_one || (to < from + len && from < to + len) {
        for k in (0..len).step_by(PAGE) {
            let Some(slice) = physical.slice_at(from + k) else {
                return false;
            };
            if !map(&mut physical, to + k, PAGE, slice) {
                return false;
            }
        }
        return true;
    }
    // The pages to show, up to the first that is not mapped physical
    // memory (those before it are mapped all the same, as before).
    let mut slices = Vec::with_capacity(len / PAGE);
    let mut found = true;
    for k in (0..len).step_by(PAGE) {
        match physical.slice_at(from + k) {
            Some(slice) => slices.push(slice),
            None => {
                found = false;
                break;
            }
        }
    }
    // One view per run of consecutive section pages, where its addresses are
    // one placeholder or can be joined into one; else a view a page.
    let mut k = 0;
    for run in runs_of(&slices) {
        let base = to + k * PAGE;
        let len = run * PAGE;
        let one = run > 1
            && if one_placeholder(&mut physical, base, len) {
                if !map(&mut physical, base, len, slices[k].clone()) {
                    return false;
                }
                true
            } else {
                // Memory no view of ours left holes in: mostly part of one
                // placeholder the title reserved, which is cut there (else
                // nothing is changed, and it goes page by page).
                (k..k + run).all(|page| !physical.holes.contains_key(&(to + page * PAGE)))
                    && map_as(&mut physical, base, len, slices[k].clone(), true)
            };
        if !one {
            for page in k..k + run {
                if !map(&mut physical, to + page * PAGE, PAGE, slices[page].clone()) {
                    return false;
                }
            }
        }
        k += run;
    }
    found
}

/// Whether `[base, base + len)`, all placeholders after an unmapping, is one
/// placeholder of exactly that length now, so that a view of it all can be
/// mapped there: one already (the hole a view of that length left), or
/// several such holes side by side that cover it exactly, joined into one
/// here (one system call, counted as a split). Anything else (memory never
/// mapped, holes not known, a hole reaching past it) says no: the caller
/// maps page by page.
fn one_placeholder(physical: &mut Physical, base: usize, len: usize) -> bool {
    // (`VirtualFree`'s flag to join placeholders.)
    const MEM_COALESCE_PLACEHOLDERS: u32 = 0x1;
    if physical.holes.get(&base) == Some(&len) {
        return true;
    }
    let mut parts = Vec::new();
    let mut at = base;
    while at < base + len {
        match physical.holes.get(&at) {
            Some(&part) if part > 0 && at + part <= base + len => {
                parts.push(at);
                at += part;
            }
            _ => return false,
        }
    }
    if parts.len() < 2 {
        return false;
    }
    // SAFETY: acts only on placeholders, which our views left there.
    let joined = counted(Call::Split, || unsafe {
        VirtualFree(base as *mut c_void, len, MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS)
    }) != 0;
    if joined {
        for part in parts {
            physical.holes.remove(&part);
        }
        physical.holes.insert(base, len);
    }
    joined
}

/// The lengths of the runs of `slices` that are consecutive pages of one
/// section, in order.
fn runs_of(slices: &[Slice]) -> Vec<usize> {
    let mut runs: Vec<usize> = Vec::new();
    for (k, slice) in slices.iter().enumerate() {
        let continues = k > 0 && {
            let before = &slices[k - 1];
            Arc::ptr_eq(&before.section, &slice.section) && before.index + 1 == slice.index
        };
        match runs.last_mut() {
            Some(run) if continues => *run += 1,
            _ => runs.push(1),
        }
    }
    runs
}

#[cfg(test)]
thread_local! {
    /// [`map_runs`] on for this test thread.
    static MAP_RUNS_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the GPU's page-mapping copies ([`map_alias`]) show a run of
/// consecutive section pages through one view (`VICTORY_MAP_RUNS=1`; off by
/// default: a view a page, as before).
///
/// Why: the title streams by mapping pool pages into its tiled heap with
/// `CopyPageMappingsBatchX`, and each view is a system call to map and one to
/// unmap again, in the process's one address-space lock, behind every guard's
/// protection change. In the cutscenes those calls were 43 % of the frames
/// over 50 ms (a timeline of three 20-frame windows). A run of n pages in one
/// view costs 2 calls where it cost 2n. What it may cost: a later change of a
/// single page inside such a view unmaps all of it and maps the rest again
/// (`unmap`). Ported from trustno1-coder/VIctory-NVIDIA dfe9acc
/// (GPL-3.0-or-later).
fn map_runs() -> bool {
    #[cfg(test)]
    if MAP_RUNS_TEST.with(std::cell::Cell::get) {
        return true;
    }
    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("VICTORY_MAP_RUNS").is_some_and(|v| v == "1"));
    *ON
}
/// [`map_alias`] of many ranges, in order.
pub fn map_aliases(ranges: &[(u64, u64, u64)]) -> bool {
    let _costed = Costed::begin(None);
    lift_thread();
    if side_by_side() <= 1 {
        return ranges.iter().all(|&(to, from, len)| alias(to, from, len));
    }
    // By the page: the address, and the one whose page it is to show.
    let pages: Vec<(usize, Option<usize>)> = ranges
        .iter()
        .flat_map(|&(to, from, len)| {
            (0..len as usize).step_by(PAGE).map(move |k| (to as usize + k, (from != 0).then_some(from as usize + k)))
        })
        .collect();
    if pages.iter().any(|p| p.0 % PAGE != 0 || p.1.is_some_and(|from| from % PAGE != 0)) {
        return ranges.iter().all(|&(to, from, len)| alias(to, from, len));
    }
    let mut ok = true;
    let mut at = 0;
    while at < pages.len() {
        let _busy = Busy::begin();
        let mut physical = physical();
        let mut ops: Vec<Op> = Vec::new();
        let mut touched = std::collections::HashSet::new();
        for &(to, from) in &pages[at..] {
            // A page the group changes already, or one it would be mapped
            // from: after it.
            if ops.len() == GROUP || touched.contains(&to) || from.is_some_and(|from| touched.contains(&from)) {
                break;
            }
            let slice = from.map(|from| physical.slice_at(from));
            ok &= !matches!(slice, Some(None));
            touched.insert(to);
            ops.push(Op::new(to, slice.flatten()));
            at += 1;
        }
        let _changing = victory_gpu::title_memory_remapping();
        ok &= change(&mut physical, ops);
    }
    ok
}

/// Maps (page number not 0) or unmaps each address; false if a page is not
/// the title's or the host refuses one.
fn scatter(wanted: &[(usize, usize)]) -> bool {
    let mut ok = true;
    let mut at = 0;
    while at < wanted.len() {
        let _busy = Busy::begin();
        let mut costed = Costed::begin(None);
        let mut ops: Vec<Op> = Vec::new();
        let mut physical = physical();
        if let Some(costed) = &mut costed {
            costed.waited = costed.started.elapsed().as_nanos() as u64;
        }
        {
            let table = page_table();
            let mut touched = std::collections::HashSet::new();
            for &(to, page) in &wanted[at..] {
                if ops.len() == GROUP || !touched.insert(to) {
                    break;
                }
                let slice = table.pages.get(&page).cloned();
                if page != 0 && slice.is_none() {
                    tracing::warn!(page, "XMemMapPhysicalPagesScatter: unknown page");
                    ok = false;
                }
                ops.push(Op::new(to, slice));
                at += 1;
            }
        }
        if let Some(costed) = &mut costed {
            costed.pages = Some(ops.len());
        }
        let _changing = victory_gpu::title_memory_remapping();
        ok &= change(&mut physical, ops);
    }
    ok
}

/// Pages changed under the locks at once.
const GROUP: usize = 64;

/// How many threads map a group of pages side by side (`VICTORY_MAP_THREADS`;
/// 8 unless set; 1: the caller alone, a page after the other; `auto`: 1 while
/// mapping takes little of the time, [`WIDE`] while the title streams as fast
/// as its pages are mapped, [`Busy`]).
///
/// shortcut: `auto` is not the default. The intro's diner was there in time
/// with it (one run, textures-d3), but in the dorm, where each call takes 0.3
/// ms of something that only one at a time gets (until a step of the mission,
/// after which it takes 0.01), 16 threads map no more pages than one, and the
/// groups hold the mapping lock: 260 ms a frame against 125 (textures-d4, -d2).
///
/// Mapping and unmapping a view wait for a lock that every protection change
/// of a view takes too, and our guards make 100,000 of those a second, from
/// many threads, in the world: 0.2 to 4 ms a page. One thread stands in that
/// queue once at a time. In a bench (60,000 views, five threads changing
/// protections) one thread mapped 1,500 pages a second, eight 30,000.
fn side_by_side() -> usize {
    // Diagnostics: 1 and 8 by turns.
    #[cfg(all(windows, feature = "diagnostics"))]
    if let Some(on) = victory_gpu::switched("VICTORY_MAP_THREADS") {
        return if on { 8 } else { 1 };
    }
    match asked_threads() {
        Some(n) => n,
        None if BUSY.2.load(Ordering::Relaxed) => WIDE,
        None => 1,
    }
}

/// Threads while the title streams fast: they stand in the host's queue side
/// by side, each for a millisecond or two a page in the world's first seconds.
const WIDE: usize = 16;

/// `VICTORY_MAP_THREADS` as a number; `None`: by how much there is to map.
fn asked_threads() -> Option<usize> {
    static ASKED: LazyLock<Option<usize>> = LazyLock::new(|| {
        match std::env::var("VICTORY_MAP_THREADS") {
            Ok(v) if v.trim() == "auto" => None,
            v => Some(v.ok().and_then(|v| v.trim().parse().ok()).unwrap_or(8).clamp(1, 32)),
        }
    });
    *ASKED
}

/// Of the window under way: when it began, the nanoseconds spent mapping in
/// it; and whether the last one was busy.
static BUSY: (Mutex<Option<std::time::Instant>>, AtomicU64, std::sync::atomic::AtomicBool) =
    (Mutex::new(None), AtomicU64::new(0), std::sync::atomic::AtomicBool::new(false));

/// Counts the time a mapping call of the title's takes, as long as it lives.
/// More than a quarter of a window, and the calls are what loading waits for
/// (one thread of the title's makes them); less than a twentieth, they are
/// not any more.
struct Busy(std::time::Instant);

impl Busy {
    fn begin() -> Busy {
        Busy(std::time::Instant::now())
    }
}

/// Whether mapping is busy after a window of `window` with `spent` in it.
fn busy_after(was: bool, spent: std::time::Duration, window: std::time::Duration) -> bool {
    spent * if was { 20 } else { 4 } > window
}

impl Drop for Busy {
    fn drop(&mut self) {
        const WINDOW: std::time::Duration = std::time::Duration::from_millis(250);
        let now = std::time::Instant::now();
        let all = BUSY.1.fetch_add((now - self.0).as_nanos() as u64, Ordering::Relaxed) + (now - self.0).as_nanos() as u64;
        let mut began = BUSY.0.lock().unwrap_or_else(|e| e.into_inner());
        let window = now - *began.get_or_insert(self.0);
        if window < WINDOW {
            return;
        }
        *began = Some(now);
        BUSY.1.store(0, Ordering::Relaxed);
        let was = BUSY.2.load(Ordering::Relaxed);
        let is = busy_after(was, std::time::Duration::from_nanos(all), window);
        if was != is {
            BUSY.2.store(is, Ordering::Relaxed);
            static SAID: AtomicU64 = AtomicU64::new(0);
            if asked_threads().is_none() && SAID.fetch_add(1, Ordering::Relaxed) < 200 {
                tracing::info!(
                    "title pages are mapped by {} (VICTORY_MAP_THREADS): {:.0} ms of the last {:.0} ms went into mapping calls",
                    if is { "several threads" } else { "the caller alone" },
                    all as f64 / 1e6,
                    window.as_secs_f64() * 1e3
                );
            }
        }
    }
}

/// A page of address space to unmap and, with a slice, map anew.
struct Op {
    at: usize,
    slice: Option<Slice>,
    /// A view of this page alone is there, to be unmapped.
    had: bool,
    /// The placeholder is the page's own (or will be, after the unmapping).
    own: bool,
    /// (`VICTORY_POOL_DISCARD`) What the view taken away showed.
    old: Option<Slice>,
}

impl Op {
    fn new(at: usize, slice: Option<Slice>) -> Op {
        Op { at, slice, had: false, own: false, old: None }
    }

    /// The system calls; whether the host did as asked.
    fn run(&self) -> bool {
        if self.had {
            // SAFETY: a view we mapped; it becomes a placeholder again.
            let gone = counted(Call::Unmap, || unsafe {
                UnmapViewOfFile2(
                    GetCurrentProcess(),
                    MEMORY_MAPPED_VIEW_ADDRESS { Value: self.at as *mut c_void },
                    MEM_PRESERVE_PLACEHOLDER,
                )
            });
            if gone == 0 {
                return false;
            }
        }
        let Some(slice) = &self.slice else {
            return true;
        };
        show(self.at, PAGE, slice, self.own)
    }
}

/// Does `ops`, each of another page, side by side: first what is noted of
/// the pages goes, then the system calls, then what is there now is noted.
/// With [`PHYSICAL`] and the change of mapping (`title_memory_remapping`)
/// held.
fn change(physical: &mut Physical, mut ops: Vec<Op>) -> bool {
    for op in &mut ops {
        let there = physical
            .views
            .range(..=op.at)
            .next_back()
            .filter(|(base, view)| op.at < **base + view.0)
            .map(|(base, view)| (*base, view.0));
        match there {
            Some((base, PAGE)) if base == op.at => {
                if let Some((size, slice)) = physical.views.remove(&op.at) {
                    shown_mut().forget(op.at, size, &slice);
                    op.old = Some(slice);
                }
                victory_gpu::title_memory_unmapping(op.at, PAGE);
                physical.holes.remove(&op.at);
                (op.had, op.own) = (true, lean());
            }
            // Part of a larger view.
            Some(_) => {
                unmap(physical, op.at, PAGE);
            }
            None => {}
        }
        if op.had {
            continue;
        }
        op.own = physical.holes.remove(&op.at) == Some(PAGE) && lean();
        if op.slice.is_some() && !op.own {
            // Cut out of a larger placeholder here, not side by side: a
            // thread that maps a piece while others cut the same one can
            // fail.
            split(op.at, PAGE);
            op.own = true;
        }
    }
    let done = pool::all(ops);
    let mut ok = true;
    for (op, done) in done {
        // (`VICTORY_POOL_DISCARD`) The page's old view is gone.
        if op.had
            && done
            && let Some(old) = &op.old
        {
            pool_discard::unshown(physical, old);
        }
        match op.slice {
            Some(slice) if done => {
                // As in `map`.
                shown_mut().note(op.at, PAGE, &slice);
                victory_gpu::title_memory_remapped(op.at, PAGE);
                victory_gpu::title_memory_view(op.at, PAGE, slice.section.handle as usize, (slice.index * PAGE) as u64);
                physical.views.insert(op.at, (PAGE, slice));
            }
            None if op.had && done => {
                physical.holes.insert(op.at, PAGE);
            }
            _ => ok &= done,
        }
    }
    ok
}

/// The threads that do the system calls of [`change`].
mod pool {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};

    use super::{Op, side_by_side};

    struct Batch {
        ops: Vec<(Op, AtomicBool)>,
        next: AtomicUsize,
        /// Ops not done yet.
        left: AtomicUsize,
        over: (Mutex<bool>, Condvar),
    }

    impl Batch {
        /// Does ops until none is left to begin.
        fn work(&self) {
            loop {
                let i = self.next.fetch_add(1, Ordering::Relaxed);
                let Some((op, done)) = self.ops.get(i) else {
                    return;
                };
                done.store(op.run(), Ordering::Release);
                if self.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                    *self.over.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                    self.over.1.notify_all();
                }
            }
        }
    }

    struct Pool {
        batches: Mutex<VecDeque<Arc<Batch>>>,
        more: Condvar,
    }

    fn pool() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        static STARTED: std::sync::Once = std::sync::Once::new();
        let pool = POOL.get_or_init(|| Pool { batches: Mutex::default(), more: Condvar::new() });
        STARTED.call_once(|| {
            let started = (1..super::asked_threads().unwrap_or(super::WIDE).max(side_by_side()))
                .filter(|i| std::thread::Builder::new().name(format!("mapping {i}")).spawn(|| serve(pool)).is_ok())
                .count();
            tracing::info!(started, "threads that map pages side by side (VICTORY_MAP_THREADS=1: none)");
        });
        pool
    }

    fn serve(pool: &Pool) {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        // SAFETY: a plain call on our own thread.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
        let mut batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let Some(batch) = batches.front().cloned() else {
                batches = pool.more.wait(batches).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            if batch.next.load(Ordering::Relaxed) >= batch.ops.len() {
                // All begun: nobody needs to see it any more.
                if batches.front().is_some_and(|b| Arc::ptr_eq(b, &batch)) {
                    batches.pop_front();
                }
                continue;
            }
            drop(batches);
            batch.work();
            batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Runs `ops`; each with whether the host did it.
    pub(super) fn all(ops: Vec<Op>) -> Vec<(Op, bool)> {
        // Waking the others takes longer than a few quick calls.
        if ops.len() < 3 {
            return ops
                .into_iter()
                .map(|op| {
                    let done = op.run();
                    (op, done)
                })
                .collect();
        }
        let n = ops.len();
        let batch = Arc::new(Batch {
            ops: ops.into_iter().map(|op| (op, AtomicBool::new(false))).collect(),
            next: AtomicUsize::new(0),
            left: AtomicUsize::new(n),
            over: Default::default(),
        });
        let pool = pool();
        pool.batches.lock().unwrap_or_else(|e| e.into_inner()).push_back(batch.clone());
        if n > side_by_side() {
            pool.more.notify_all();
        } else {
            (1..n).for_each(|_| pool.more.notify_one());
        }
        batch.work();
        let mut over = batch.over.0.lock().unwrap_or_else(|e| e.into_inner());
        while !*over {
            over = batch.over.1.wait(over).unwrap_or_else(|e| e.into_inner());
        }
        drop(over);
        // No worker changes it any more (one may still hold it for a moment).
        batch
            .ops
            .iter()
            .map(|(op, done)| {
                let op = Op { at: op.at, slice: op.slice.clone(), had: op.had, own: op.own, old: op.old.clone() };
                (op, done.load(Ordering::Acquire))
            })
            .collect()
    }
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    map_as(physical, base, len, slice, false)
}

/// [`map`]; `quiet`: a failure is not said (the caller tries another way).
fn map_as(physical: &mut Physical, base: usize, len: usize, slice: Slice, quiet: bool) -> bool {
    let own = physical.holes.remove(&base) == Some(len) && lean();
    if !show_as(base, len, &slice, own, quiet) {
        return false;
    }
    // Noted before the GPU side is told: bytes DirectStorage put in earlier,
    // without notice at these addresses ([`pages_written`]), are there by
    // the time the copies of them go.
    shown_mut().note(base, len, &slice);
    // New contents, read-write: the GPU side's copies no longer guard it.
    victory_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    victory_gpu::title_memory_view(base, len, slice.section.handle as usize, (slice.index * PAGE) as u64);
    physical.views.insert(base, (len, slice));
    true
}

/// Cuts `[base, base + len)` out of the placeholder it is in (which fails
/// harmlessly when the range is the whole placeholder).
fn split(base: usize, len: usize) {
    // SAFETY: acts only on a placeholder.
    counted(Call::Split, || unsafe {
        VirtualFree(
            base as *mut c_void,
            len,
            MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
        )
    });
}

/// The system calls of [`map`]; `own`: the placeholder at `base` is `len`
/// long already.
fn show(base: usize, len: usize, slice: &Slice, own: bool) -> bool {
    show_as(base, len, slice, own, false)
}

/// [`show`]; `quiet`: a failure to map is not said.
fn show_as(base: usize, len: usize, slice: &Slice, own: bool, quiet: bool) -> bool {
    let map_view = || {
        // SAFETY: maps the section over a placeholder; acts only on one.
        counted(Call::Map, || unsafe {
            MapViewOfFile3(
                slice.section.handle,
                GetCurrentProcess(),
                base as *const c_void,
                (slice.index * PAGE) as u64,
                len,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
        })
    };
    let mut view = if own { map_view() } else { MEMORY_MAPPED_VIEW_ADDRESS { Value: std::ptr::null_mut() } };
    if view.Value as usize != base {
        split(base, len);
        view = map_view();
    }
    if view.Value as usize != base {
        if quiet {
            return false;
        }
        tracing::warn!(
            address = format_args!("{base:#x}"),
            len,
            error = %std::io::Error::last_os_error(),
            "mapping physical pages failed (not a placeholder?)"
        );
        return false;
    }
    if !slice.section.commit_shown(slice.index, base, len) {
        // SAFETY: the view just mapped, back to a placeholder.
        unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
        return false;
    }
    true
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    commit_refused(len, "physical pages", std::io::Error::last_os_error());
    false
}

/// Reports memory the host would not give: at the Windows commit limit to
/// `crashes.log`, since the title usually faults soon after on the null it
/// gets; otherwise as a warning.
fn commit_refused(len: usize, what: &str, error: std::io::Error) {
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        let mut info = PERFORMANCE_INFORMATION {
            cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
        let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
        victory_loader::log_crash(&commit_limit_message(
            len,
            what,
            &error,
            queried.then_some(&info),
        ));
    } else {
        tracing::warn!(len, %error, "committing {what} failed");
    }
}

fn commit_limit_message(
    len: usize,
    what: &str,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of {what}: {error}.\n"
    );
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    // (`VICTORY_POOL_DISCARD`) The pages that lost a view, looked at once
    // the views are as they stay.
    let mut lost: Vec<Slice> = Vec::new();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        shown_mut().forget(at, size, &slice);
        if at < base || at + size > end {
            // Parts of it stay, and are mapped again below.
            victory_gpu::title_memory_splitting(at, size);
        }
        victory_gpu::title_memory_unmapping(at, size);
        // SAFETY: a view we mapped; it becomes a placeholder again.
        let gone = counted(Call::Unmap, || unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        });
        if gone != 0 && at >= base && at + size <= end {
            physical.holes.insert(at, size);
        }
        // (`VICTORY_POOL_DISCARD`) What of the view lay inside the range is
        // really gone (the rest is mapped again below).
        if gone != 0 && pool_discard::on() {
            let (lo, hi) = (at.max(base), (at + size).min(end));
            lost.extend((lo..hi).step_by(PAGE).map(|a| Slice { index: slice.index + (a - at) / PAGE, section: slice.section.clone() }));
        }
        if at < base {
            map(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map(physical, end, at + size - end, Slice { index, ..slice });
        }
    }
    for page in &lost {
        pool_discard::unshown(physical, page);
    }
}

/// `VICTORY_POOL_DISCARD=1` (off by default; ported from
/// Yaekai/victory-scarlett-3070);
/// needs the section's lasting view, `VICTORY_SECTION_WINDOW`, the fox
/// base's form of our `VICTORY_DSTORAGE_LINEAR_VIEW`). The lasting view
/// keeps every page DirectStorage ever wrote through it in this process's
/// working set, also after the title has given the page back (run-49: 24.7
/// GB working set against 19.1). The title's streaming pool is one
/// allocation it shows whole at one address for good (its pool view) and
/// maps page by page into its CPU and GPU streaming views from a free list;
/// a page shown by no view but a whole-section one is on that free list and
/// its bytes are dead. Such a page, still so [`pool_discard::delay`] after it was
/// last unmapped and while free RAM is under `VICTORY_POOL_DISCARD_BELOW_MB`
/// (4096; 0: always), is reset in the lasting view (`MEM_RESET`) and taken
/// out of the working set (`VirtualUnlock`), under the views' lock, so no
/// map of it can come between the check and the reset, and with no write
/// through a lasting view under way (`pool_discard::WRITES`); a page
/// written since its last unmapping is not discarded. Pages of sections
/// without a whole view are never touched. `VICTORY_POOL_DISCARD_DELAY_S`:
/// 0 (default) adaptive 2-10 s, N fixed. Line every 30 s: "pool discard: ...".
mod pool_discard {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, RwLockReadGuard};
    use std::time::{Duration, Instant};

    use super::{PAGE, Physical, Section, Slice};

    pub(super) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            let window = std::env::var_os("VICTORY_SECTION_WINDOW").is_none_or(|v| v != "0");
            let on = window && std::env::var_os("VICTORY_POOL_DISCARD").is_some_and(|v| v == "1");
            tracing::info!(on, "VICTORY_POOL_DISCARD: pool pages the title has given back (shown by no view but its whole pool view) are reset and taken out of the working set in the section's lasting view while free RAM is short");
            on
        })
    }

    /// Pages resident through the lasting views, discarded since the
    /// start, resets refused, rewritten within 5 s of their discard.
    static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
    const RESIDENT: usize = 0;
    const DISCARDED: usize = 1;
    const FAILED: usize = 2;
    const REWRITTEN: usize = 3;

    /// Pages that lost a view, to look at again later.
    static UNSHOWN: LazyLock<Mutex<Unshown>> = LazyLock::new(Default::default);

    #[derive(Default)]
    struct Unshown {
        /// When, and the page, in that order: a page unmapped again is in it
        /// once for each time.
        queue: VecDeque<(Instant, Slice)>,
        /// By page ([`key`]): when it was last unmapped, until it is written
        /// or discarded. Only the queue's entry of that moment counts, so the
        /// delay runs from the last unmapping and a write since cancels it.
        last: HashMap<(usize, usize), Instant>,
    }

    /// Held (shared) by every write through a lasting view from before
    /// [`written`] until the bytes are there, and (alone) by the discard
    /// loop from its choice of pages to their reset: a page is never reset
    /// while a write into it is under way, nor after one it did not see.
    /// Taken before [`super::PHYSICAL`]; nothing that holds that waits for it.
    static WRITES: RwLock<()> = RwLock::new(());

    /// While a write through a lasting view is under way ([`WRITES`]);
    /// nothing while the feature is off.
    pub(super) fn writing() -> Option<RwLockReadGuard<'static, ()>> {
        on().then(|| WRITES.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Takes the entries of `queue` that are at least `quiet` old at `now`,
    /// at most `most`, and keeps of them the pages for which they are the
    /// last unmapping still standing in `last` (which forgets them): older
    /// ones have a later entry of their own, and a page written since has
    /// none.
    pub(super) fn take_due<T>(
        queue: &mut VecDeque<(Instant, T)>,
        last: &mut HashMap<(usize, usize), Instant>,
        key: impl Fn(&T) -> (usize, usize),
        now: Instant,
        quiet: Duration,
        most: usize,
    ) -> Vec<T> {
        let mut due = Vec::new();
        while due.len() < most
            && let Some((at, _)) = queue.front()
            && now.saturating_duration_since(*at) >= quiet
        {
            let Some((at, page)) = queue.pop_front() else { break };
            let k = key(&page);
            if last.get(&k) == Some(&at) {
                last.remove(&k);
                due.push(page);
            }
        }
        due
    }

    /// Drops the oldest entries of `queue` past `most`, and with them what
    /// `last` says of them.
    pub(super) fn trim<T>(
        queue: &mut VecDeque<(Instant, T)>,
        last: &mut HashMap<(usize, usize), Instant>,
        key: impl Fn(&T) -> (usize, usize),
        most: usize,
    ) {
        let over = queue.len().saturating_sub(most);
        for (at, page) in queue.drain(..over) {
            let k = key(&page);
            if last.get(&k) == Some(&at) {
                last.remove(&k);
            }
        }
    }
    /// The most pages kept noted.
    const UNSHOWN_MOST: usize = 1 << 18;
    /// When each page was discarded, until written again (or 5 s on).
    static DISCARDED_AT: LazyLock<Mutex<HashMap<(usize, usize), Instant>>> = LazyLock::new(Default::default);
    /// Discards in the adaptive window, and of them rewritten within 5 s.
    static WINDOW: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    static HELD_BY_RAM: AtomicBool = AtomicBool::new(false);
    static DELAY_MS: AtomicU64 = AtomicU64::new(2000);

    fn key(section: &Section, index: usize) -> (usize, usize) {
        (std::ptr::from_ref(section) as usize, index)
    }

    /// Pages `[index, index + n)` of `section` are being written through its
    /// lasting view (under [`writing`]): resident there from now on, and no
    /// longer due for a discard since an unmapping before.
    pub(super) fn written(section: &Section, index: usize, n: usize) {
        if !on() {
            return;
        }
        {
            let mut unshown = UNSHOWN.lock().unwrap_or_else(|e| e.into_inner());
            if !unshown.last.is_empty() {
                for k in index..(index + n).min(section.pages) {
                    unshown.last.remove(&key(section, k));
                }
            }
        }
        for k in index..(index + n).min(section.pages) {
            let bit = 1u64 << (k % 64);
            if section.resident[k / 64].fetch_or(bit, Relaxed) & bit == 0 {
                COUNTS[RESIDENT].fetch_add(1, Relaxed);
            }
            if section.discarded[k / 64].fetch_and(!bit, Relaxed) & bit != 0 {
                let at = DISCARDED_AT.lock().unwrap_or_else(|e| e.into_inner()).remove(&key(section, k));
                if at.is_some_and(|at| at.elapsed() < Duration::from_secs(5)) {
                    COUNTS[REWRITTEN].fetch_add(1, Relaxed);
                    WINDOW[1].fetch_add(1, Relaxed);
                }
            }
        }
    }

    /// Whether only whole-section views show `page` now, and at least one.
    fn only_whole_views(physical: &Physical, page: &Slice) -> bool {
        let addresses = super::SHOWN.read().unwrap_or_else(|e| e.into_inner()).at(page);
        let size = page.section.pages * PAGE;
        !addresses.is_empty()
            && addresses.iter().all(|&(a, _)| {
                physical
                    .views
                    .range(..=a)
                    .next_back()
                    .is_some_and(|(&at, (len, slice))| at + len > a && *len == size && slice.index == 0)
            })
    }

    /// A page lost a view (with the views' lock held): noted if it is
    /// resident in our lasting view and only the pool view shows it now.
    pub(super) fn unshown(physical: &Physical, page: &Slice) {
        if !on() {
            return;
        }
        let bit = 1u64 << (page.index % 64);
        if page.index >= page.section.pages
            || page.section.resident[page.index / 64].load(Relaxed) & bit == 0
            || !only_whole_views(physical, page)
        {
            return;
        }
        {
            let now = Instant::now();
            let mut unshown = UNSHOWN.lock().unwrap_or_else(|e| e.into_inner());
            unshown.last.insert(key(&page.section, page.index), now);
            unshown.queue.push_back((now, page.clone()));
        }
        static STARTED: std::sync::Once = std::sync::Once::new();
        STARTED.call_once(|| {
            let _ = std::thread::Builder::new().name("victory-pool-discard".into()).spawn(discard_loop);
        });
    }

    fn below() -> u64 {
        static MB: OnceLock<u64> = OnceLock::new();
        let mb = *MB.get_or_init(|| {
            let mb = std::env::var("VICTORY_POOL_DISCARD_BELOW_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(4096);
            tracing::info!(mb, "VICTORY_POOL_DISCARD_BELOW_MB: pool pages are discarded only while free RAM is under this (0: always)");
            mb
        });
        if mb == 0 { u64::MAX } else { mb << 20 }
    }

    fn free_ram() -> Option<u64> {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: plain data the call fills in.
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: a sized structure.
        (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(m.ullAvailPhys)
    }

    fn delay_setting() -> u64 {
        static S: OnceLock<u64> = OnceLock::new();
        *S.get_or_init(|| {
            let s = std::env::var("VICTORY_POOL_DISCARD_DELAY_S").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
            tracing::info!(s, "VICTORY_POOL_DISCARD_DELAY_S: wait after a pool page's last unmapping before it is discarded (0: adaptive 2-10 s by pages rewritten within 5 s)");
            s
        })
    }

    /// The next adaptive delay from the last window's discards `d` and of
    /// them rewritten within 5 s `r`, and free RAM: more than a quarter
    /// rewritten doubles it (up to 10 s), under 5 % halves it (down to 2 s);
    /// under 1 GiB free RAM it is 2 s.
    pub(super) fn next_delay(ms: u64, d: u64, r: u64, free: Option<u64>) -> u64 {
        if free.is_some_and(|f| f < 1 << 30) {
            return 2000;
        }
        if d >= 64 && r * 4 > d {
            (ms * 2).min(10_000)
        } else if d >= 64 && r * 20 < d {
            (ms / 2).max(2000)
        } else {
            ms
        }
    }

    fn delay() -> Duration {
        match delay_setting() {
            0 => Duration::from_millis(DELAY_MS.load(Relaxed)),
            s => Duration::from_secs(s),
        }
    }

    fn discard_loop() {
        use windows_sys::Win32::System::Memory::{MEM_RESET, PAGE_READWRITE, VirtualAlloc, VirtualUnlock};
        let mut window = Instant::now();
        let mut said = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if window.elapsed() >= Duration::from_secs(5) {
                window = Instant::now();
                let (d, r) = (WINDOW[0].swap(0, Relaxed), WINDOW[1].swap(0, Relaxed));
                DELAY_MS.store(next_delay(DELAY_MS.load(Relaxed), d, r, free_ram()), Relaxed);
                DISCARDED_AT.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, at| at.elapsed() < Duration::from_secs(5));
            }
            if said.elapsed() >= Duration::from_secs(30) {
                said = Instant::now();
                tracing::info!("{}", line());
            }
            let quiet = delay();
            if free_ram().is_some_and(|free| free >= below()) {
                HELD_BY_RAM.store(true, Relaxed);
                let unshown = &mut *UNSHOWN.lock().unwrap_or_else(|e| e.into_inner());
                trim(&mut unshown.queue, &mut unshown.last, |s| key(&s.section, s.index), UNSHOWN_MOST);
                continue;
            }
            HELD_BY_RAM.store(false, Relaxed);
            // Pieces of 1,024 pages, the locks let go between them; each
            // chosen and reset with no write into a lasting view under way.
            loop {
                let writes = WRITES.write().unwrap_or_else(|e| e.into_inner());
                let physical = super::physical();
                let piece: Vec<Slice> = {
                    let unshown = &mut *UNSHOWN.lock().unwrap_or_else(|e| e.into_inner());
                    take_due(&mut unshown.queue, &mut unshown.last, |s| key(&s.section, s.index), Instant::now(), quiet, 1024)
                };
                if piece.is_empty() {
                    break;
                }
                // (lasting view base, first index, pages)
                let mut runs: Vec<(usize, usize, usize, Arc<Section>)> = Vec::new();
                for page in &piece {
                    let section = &*page.section;
                    let Some(&base) = section.window.get() else { continue };
                    let bit = 1u64 << (page.index % 64);
                    if base == 0 || section.resident[page.index / 64].load(Relaxed) & bit == 0 || !only_whole_views(&physical, page) {
                        continue;
                    }
                    section.resident[page.index / 64].fetch_and(!bit, Relaxed);
                    section.discarded[page.index / 64].fetch_or(bit, Relaxed);
                    DISCARDED_AT.lock().unwrap_or_else(|e| e.into_inner()).insert(key(section, page.index), Instant::now());
                    WINDOW[0].fetch_add(1, Relaxed);
                    match runs.last_mut() {
                        Some((b, first, n, _)) if *b == base && *first + *n == page.index => *n += 1,
                        _ => runs.push((base, page.index, 1, page.section.clone())),
                    }
                }
                for (base, first, n, _section) in runs {
                    let (at, len) = ((base + first * PAGE) as *mut core::ffi::c_void, n * PAGE);
                    // SAFETY: committed pages of our lasting view whose bytes
                    // are dead (above); the lock keeps maps of them out, and
                    // `_section` keeps the view.
                    let reset = unsafe { VirtualAlloc(at, len, MEM_RESET, PAGE_READWRITE) };
                    if reset.is_null() && COUNTS[FAILED].fetch_add(1, Relaxed) == 0 {
                        tracing::warn!(error = %std::io::Error::last_os_error(), "VICTORY_POOL_DISCARD: MEM_RESET refused (taken out of the working set only)");
                    }
                    // SAFETY: as above; on pages not locked this only takes
                    // them out of the working set.
                    unsafe { VirtualUnlock(at, len) };
                    COUNTS[RESIDENT].fetch_sub(n as u64, Relaxed);
                    COUNTS[DISCARDED].fetch_add(n as u64, Relaxed);
                }
                drop(physical);
                drop(writes);
            }
        }
    }

    /// "pool discard: ..." with the process's working set.
    fn line() -> String {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
        // SAFETY: our own process and a sized structure.
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        let mb = |pages: u64| (pages * PAGE as u64) >> 20;
        let failed = COUNTS[FAILED].load(Relaxed);
        format!(
            "pool discard: pool pages resident {} MB, discarded this run {} MB, rewritten within 5 s of a discard {} MB (delay {} ms), working set {} MB (peak {} MB) (VICTORY_POOL_DISCARD=1{}; only under {} MB free RAM, VICTORY_POOL_DISCARD_BELOW_MB{})",
            mb(COUNTS[RESIDENT].load(Relaxed)),
            mb(COUNTS[DISCARDED].load(Relaxed)),
            mb(COUNTS[REWRITTEN].load(Relaxed)),
            delay().as_millis(),
            c.WorkingSetSize >> 20,
            c.PeakWorkingSetSize >> 20,
            if failed == 0 { String::new() } else { format!(", resets refused {failed}") },
            if below() == u64::MAX { "any".to_owned() } else { (below() >> 20).to_string() },
            if HELD_BY_RAM.load(Relaxed) { "; held now: RAM plentiful" } else { "" },
        )
    }

    #[cfg(test)]
    mod tests {
        use super::{next_delay, take_due, trim};
        use std::collections::{HashMap, VecDeque};
        use std::time::{Duration, Instant};

        type Queue = VecDeque<(Instant, (usize, usize))>;

        /// What `unshown` notes of a page unmapped at `at`.
        fn unmapped(queue: &mut Queue, last: &mut HashMap<(usize, usize), Instant>, page: (usize, usize), at: Instant) {
            last.insert(page, at);
            queue.push_back((at, page));
        }

        /// The delay runs from a page's last unmapping, not its first; a
        /// page written since its unmapping is not due at all.
        #[test]
        fn pages_are_due_a_delay_after_their_last_unmapping() {
            let t0 = Instant::now();
            let s = Duration::from_secs;
            let quiet = s(2);
            let (mut queue, mut last) = (Queue::new(), HashMap::new());
            let (a, b, c) = ((1, 0), (1, 1), (1, 2));
            unmapped(&mut queue, &mut last, a, t0);
            unmapped(&mut queue, &mut last, b, t0);
            unmapped(&mut queue, &mut last, c, t0 + s(1));
            // `a` mapped again and unmapped just before its first delay ran out.
            unmapped(&mut queue, &mut last, a, t0 + Duration::from_millis(1900));
            // `c` written since (`written` forgets it).
            last.remove(&c);
            let key = |p: &(usize, usize)| *p;
            assert_eq!(take_due(&mut queue, &mut last, key, t0 + s(1), quiet, usize::MAX), vec![]);
            assert_eq!(take_due(&mut queue, &mut last, key, t0 + s(2), quiet, usize::MAX), vec![b], "a was unmapped 0.1 s ago");
            assert_eq!(take_due(&mut queue, &mut last, key, t0 + s(3), quiet, usize::MAX), vec![], "c was written");
            assert_eq!(take_due(&mut queue, &mut last, key, t0 + s(4), quiet, usize::MAX), vec![a]);
            assert!(queue.is_empty() && last.is_empty());
        }

        #[test]
        fn due_pages_come_in_pieces_and_trimming_forgets_them() {
            let t0 = Instant::now();
            let (mut queue, mut last) = (Queue::new(), HashMap::new());
            for i in 0..5 {
                unmapped(&mut queue, &mut last, (2, i), t0);
            }
            let key = |p: &(usize, usize)| *p;
            let later = t0 + Duration::from_secs(10);
            let quiet = Duration::from_secs(2);
            assert_eq!(take_due(&mut queue, &mut last, key, later, quiet, 2), vec![(2, 0), (2, 1)]);
            trim(&mut queue, &mut last, key, 1);
            assert_eq!(queue.len(), 1);
            assert_eq!(last.len(), 1);
            assert_eq!(take_due(&mut queue, &mut last, key, later, quiet, 9), vec![(2, 4)]);
        }

        #[test]
        fn the_discard_delay_adapts_to_pages_rewritten_soon() {
            let plenty = Some(8u64 << 30);
            assert_eq!(next_delay(2000, 100, 30, plenty), 4000);
            assert_eq!(next_delay(8000, 100, 30, plenty), 10_000);
            assert_eq!(next_delay(4000, 100, 2, plenty), 2000);
            assert_eq!(next_delay(4000, 100, 10, plenty), 4000);
            assert_eq!(next_delay(4000, 10, 10, plenty), 4000, "too few discards to judge");
            assert_eq!(next_delay(8000, 100, 30, Some(512 << 20)), 2000, "short of RAM");
        }
    }
}

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th: "used" is all the process has committed, the layer's own
    // memory and the card's included (27 GB in run 60's gameplay against a
    // limit of 13.5 GB). A title that budgets its streaming from these two
    // figures believes it is far over its limit.
    // shortcut: the figure is not yet the title's own memory alone (its
    // heaps and the pages it mapped); count those if this line shows the
    // title asking.
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, Ordering::Relaxed);
    if asked.is_multiple_of(1024) {
        tracing::info!(
            asked = asked + 1,
            which = _which,
            limit_mb = stats.title_limit >> 20,
            used_mb = stats.title_used >> 20,
            "the title asks how much memory it uses"
        );
    }
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

/// The title's own `VirtualFree`. What the GPU side noted of the memory goes
/// first: the address may be other memory's the moment it is free.
extern "system" fn title_virtual_free(address: *mut c_void, size: usize, kind: u32) -> BOOL {
    const MEM_COALESCE_PLACEHOLDERS: u32 = 1;
    // Splitting and joining placeholders changes nothing they show.
    if kind & (MEM_PRESERVE_PLACEHOLDER | MEM_COALESCE_PLACEHOLDERS) == 0 {
        victory_gpu::title_memory_freed(address as usize, size);
    }
    // What a release takes away, asked before it is gone: a release of a
    // whole allocation passes no size. (Splitting a placeholder takes
    // nothing away.)
    let at = address as usize;
    let gone = (kind & MEM_RELEASE != 0 && kind & MEM_PRESERVE_PLACEHOLDER == 0)
        .then(|| (at, if size != 0 { size } else { allocation_len(at) }));
    // SAFETY: the title's own arguments.
    let freed = unsafe { VirtualFree(address, size, kind) };
    if freed != 0
        && let Some((at, len)) = gone
    {
        // Joined placeholders are none of the views' own any more either.
        forget_holes(at, len);
        if kind & MEM_COALESCE_PLACEHOLDERS == 0 {
            forget_low_reservations(at, len);
        }
    }
    freed
}

/// The bytes of the allocation that starts at `at` (0 if none does).
fn allocation_len(at: usize) -> usize {
    let mut end = at;
    loop {
        // SAFETY: plain data; zero is a valid value.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a sized out structure of ours.
        let n = unsafe { VirtualQueryEx(GetCurrentProcess(), end as *const c_void, &mut info, size_of_val(&info)) };
        if n == 0 || info.AllocationBase as usize != at || info.State == MEM_FREE {
            return end - at;
        }
        end = info.BaseAddress as usize + info.RegionSize;
    }
}

/// Ranges the title released, whose [`Physical::holes`] go the next time
/// the views are locked: not here, since a mapping call may hold them for
/// a long time, and the title's thread that frees would wait for it.
static FREED: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
static ANY_FREED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn forget_holes(at: usize, len: usize) {
    FREED.lock().unwrap_or_else(|e| e.into_inner()).push((at, len));
    ANY_FREED.store(true, Ordering::Release);
}

/// [`LOW_RESERVATIONS`] that a release reached and that are now free all
/// through (the title may release the pieces it split one by one; the
/// pages of what is left keep their places in the reservation).
fn forget_low_reservations(at: usize, len: usize) {
    let mut reservations = LOW_RESERVATIONS.lock().unwrap_or_else(|e| e.into_inner());
    reservations.retain(|&(base, bytes)| base + bytes <= at || at + len <= base || !all_free(base, bytes));
}

/// Whether nothing of `[at, at + len)` is reserved.
fn all_free(at: usize, len: usize) -> bool {
    let mut next = at;
    while next < at + len {
        // SAFETY: plain data; zero is a valid value.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a sized out structure of ours.
        let n = unsafe { VirtualQueryEx(GetCurrentProcess(), next as *const c_void, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_FREE {
            return false;
        }
        next = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll", "kernel32.dll", "kernelbase.dll", "api-ms-win-core-memory-*"],
        exports: vec![
            crate::HleExport { name: "VirtualFree", address: title_virtual_free as *const () as usize },
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn a_32_bit_alloc_honours_an_alignment_above_64_kib() {
        // 16 MiB: four at once, so none is aligned by chance alone.
        let attributes = ALLOC_32BIT | 0x18 << ALIGNMENT_SHIFT;
        let blocks: Vec<_> = (0..4).map(|_| XMemAllocDefault(100, attributes)).collect();
        for &p in &blocks {
            assert!(!p.is_null() && (p as usize) < 1 << 32, "{p:p}");
            assert_eq!(p as usize % (16 * MIB), 0, "{p:p}");
        }
        for p in blocks {
            XMemFreeDefault(p, attributes);
        }
        // At or below 64 KiB: what the host gives anyway.
        let p = XMemAllocDefault(100, ALLOC_32BIT | 0x10 << ALIGNMENT_SHIFT);
        assert!(!p.is_null() && (p as usize).is_multiple_of(PAGE));
        XMemFreeDefault(p, ALLOC_32BIT);
    }

    #[test]
    fn a_released_32_bit_reservation_is_forgotten_with_its_last_piece() {
        let known = |base: *mut c_void| LOW_RESERVATIONS.lock().unwrap().iter().any(|r| r.0 == base as usize);
        let base = XMemVirtualAlloc(std::ptr::null_mut(), 2 * PAGE, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, GRAPHICS | ADDRESS_32BIT, PAGE_NOACCESS);
        assert!(!base.is_null() && known(base));
        let second = base.wrapping_byte_add(PAGE);
        // Split in two: still the title's.
        assert_ne!(title_virtual_free(base, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER), 0);
        assert!(known(base));
        // One piece released: the other's page keeps its place.
        assert_ne!(title_virtual_free(base, 0, MEM_RELEASE), 0);
        assert!(known(base), "half of it is still reserved");
        assert_ne!(title_virtual_free(second, 0, MEM_RELEASE), 0);
        assert!(!known(base));
    }

    #[test]
    fn a_released_placeholder_leaves_no_hole_behind() {
        let pages = allocate(1);
        let base = XMemVirtualAlloc(std::ptr::null_mut(), 2 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!base.is_null());
        let hole = |at: *mut c_void| physical().holes.contains_key(&(at as usize));
        assert_eq!(XMemMapPhysicalPages(base, 1, pages.as_ptr()), base);
        assert_eq!(XMemMapPhysicalPages(base, 1, std::ptr::null()), base);
        assert!(hole(base), "the view's own placeholder");
        // The title releases both pieces (mapping split them).
        assert_ne!(title_virtual_free(base, 0, MEM_RELEASE), 0);
        assert_ne!(title_virtual_free(base.wrapping_byte_add(PAGE), 0, MEM_RELEASE), 0);
        assert!(!hole(base));
        assert_eq!(XMemFreePhysicalPages(1, pages.as_ptr()), 1);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, "physical pages", &error, Some(&info));
        assert!(report.contains("4194304 bytes of physical pages"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, "virtual memory", &error, None);
        assert!(fallback.contains("4194304 bytes of virtual memory"));
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn the_32_bit_heap_gets_its_slices_in_address_order() {
        // As the title's heap: a 32-bit reservation of 64 KiB pages, all of
        // its pages from one call, mapped piece by piece in another order.
        // The title's own kind: reserve and 0x40000, no 64 KiB page bits.
        let base = XMemVirtualAlloc(std::ptr::null_mut(), 4 * PAGE, MEM_RESERVE | MEM_RESERVE_PLACEHOLDER, GRAPHICS | ADDRESS_32BIT, PAGE_NOACCESS);
        assert!(!base.is_null() && (base as usize) < 1 << 32);
        assert!(LOW_RESERVATIONS.lock().unwrap().iter().any(|r| r.0 == base as usize), "the reservation is known");
        let pages = allocate(4);
        let index = |number: usize| page_table().pages.get(&number).map(|s| s.index);
        let at = |page: usize| base.wrapping_byte_add(page * PAGE);
        // The last two pages' place first, from the array's start.
        arrange(at(2) as usize, &pages[..2]);
        assert_eq!(XMemMapPhysicalPages(at(2), 2, pages.as_ptr()), at(2));
        arrange(base as usize, &pages[2..]);
        assert_eq!(XMemMapPhysicalPages(base, 2, pages[2..].as_ptr()), base);
        assert_eq!([index(pages[0]), index(pages[1]), index(pages[2]), index(pages[3])], [Some(2), Some(3), Some(0), Some(1)]);
        // Mapped already: nothing moves any more.
        arrange(base as usize, &pages[..2]);
        assert_eq!(index(pages[0]), Some(2));
        // SAFETY: mapped read-write.
        unsafe { at(3).cast::<u32>().write(7) };
        assert_eq!(XMemMapPhysicalPages(base, 4, std::ptr::null()), base);
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// `VICTORY_MAP_RUNS`: the GPU's page-mapping copy of a run of pages
    /// shows exactly what it did a view a page, in one view; a page of the
    /// run unmapped or mapped anew later keeps its neighbours as they were.
    #[test]
    fn a_page_mapping_copy_of_a_run_is_one_view_and_shows_the_same() {
        let shown = |runs: bool| -> (Vec<Vec<u8>>, [usize; 2]) {
            MAP_RUNS_TEST.with(|on| on.set(runs));
            let mut pages = [0usize; 4];
            let mut n = 4;
            assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
            let reserve = || XMemVirtualAlloc(std::ptr::null_mut(), 4 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
            let (a, b) = (reserve(), reserve());
            assert!(!a.is_null() && !b.is_null());
            let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
            assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
            for page in 0..4 {
                // SAFETY: mapped read-write.
                unsafe { at(a, page).cast::<u8>().write(10 + page as u8) };
            }
            let views = |p: *mut c_void| {
                let p = p as usize;
                physical().views.range(p..p + 4 * PAGE).count()
            };
            // The whole run at `b`, then its second page unmapped and its
            // third shown from the first page of `a`.
            assert!(map_alias(b as u64, a as u64, 4 * PAGE as u64));
            let after_copy = views(b);
            assert!(map_alias(at(b, 1) as u64, 0, PAGE as u64));
            assert!(map_alias(at(b, 2) as u64, a as u64, PAGE as u64));
            let read: Vec<Vec<u8>> = (0..4)
                .map(|page| {
                    // An unmapped page reads as nothing.
                    if query(at(b, page)).host.State != MEM_COMMIT {
                        return Vec::new();
                    }
                    // SAFETY: committed, mapped read-write.
                    unsafe { vec![at(b, page).cast::<u8>().read(), at(b, page).cast::<u8>().add(1).read()] }
                })
                .collect();
            let views_after = views(b);
            assert!(map_alias(b as u64, 0, 4 * PAGE as u64));
            assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
            assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
            MAP_RUNS_TEST.with(|on| on.set(false));
            (read, [after_copy, views_after])
        };
        let (before, views_before) = shown(false);
        let (runs, views_runs) = shown(true);
        assert_eq!(before, vec![vec![10, 0], vec![], vec![10, 0], vec![13, 0]]);
        assert_eq!(runs, before);
        assert_eq!(views_before, [4, 3]);
        assert_eq!(views_runs[0], 1);
    }

    /// A page-mapping copy of pages that are consecutive pages of one
    /// allocation shows the same bytes through one view as through a view a
    /// page; four views' holes are joined for one; a single page of it
    /// unmapped later leaves the others. (Ported with `VICTORY_MAP_RUNS`
    /// from trustno1-coder/VIctory-NVIDIA dfe9acc.)
    #[test]
    fn a_copy_of_consecutive_pages_shows_them_through_one_view() {
        let mut pages = [0usize; 4];
        let mut n = 4;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let reserve = || XMemVirtualAlloc(std::ptr::null_mut(), 4 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        let (a, b, c) = (reserve(), reserve(), reserve());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        // Pages 0-2 in one call and page 3 in another: still one run of
        // four consecutive section pages.
        let order = [pages[0], pages[1], pages[2]];
        assert_eq!(XMemMapPhysicalPages(a, 3, order.as_ptr()), a);
        assert_eq!(XMemMapPhysicalPages(at(a, 3), 1, pages[3..].as_ptr()), at(a, 3));
        for k in 0..4 {
            // SAFETY: mapped read-write.
            unsafe { at(a, k).cast::<u8>().write(10 + k as u8) };
        }
        let runs = {
            let physical = physical();
            let slices: Vec<Slice> = (0..4).map(|k| physical.slice_at(at(a, k) as usize).unwrap()).collect();
            runs_of(&slices)
        };
        assert_eq!(runs, vec![4]);
        for (to, in_one) in [(b, false), (c, true)] {
            assert!(alias_as(to as u64, a as u64, 4 * PAGE as u64, in_one));
            for k in 0..4 {
                // SAFETY: mapped read-write.
                assert_eq!(unsafe { at(to, k).cast::<u8>().read() }, 10 + k as u8);
            }
        }
        assert_eq!(physical().views.get(&(c as usize)).map(|v| v.0), Some(4 * PAGE));
        assert_eq!(physical().views.get(&(b as usize)).map(|v| v.0), Some(PAGE));
        // Mapped again in one view where four views of a page were: their
        // holes are joined into one placeholder.
        assert!(alias_as(b as u64, a as u64, 4 * PAGE as u64, true));
        assert_eq!(physical().views.get(&(b as usize)).map(|v| v.0), Some(4 * PAGE));
        for k in 0..4 {
            // SAFETY: mapped read-write.
            assert_eq!(unsafe { at(b, k).cast::<u8>().read() }, 10 + k as u8);
        }
        // One page of the run unmapped: the rest stay, with their bytes.
        assert!(map_alias(at(c, 1) as u64, 0, PAGE as u64));
        assert_eq!(query(at(c, 1)).host.State, MEM_RESERVE);
        for k in [0, 2, 3] {
            // SAFETY: still mapped.
            assert_eq!(unsafe { at(c, k).cast::<u8>().read() }, 10 + k as u8);
        }
        // A source page not mapped: the pages before it are mapped all the same.
        assert!(XMemMapPhysicalPages(at(a, 2), 1, std::ptr::null()) == at(a, 2));
        assert!(!alias_as(c as u64, a as u64, 4 * PAGE as u64, true));
        // SAFETY: mapped.
        assert_eq!(unsafe { at(c, 1).cast::<u8>().read() }, 11);
        assert_eq!(query(at(c, 2)).host.State, MEM_RESERVE);
        for p in [a, b, c] {
            XMemMapPhysicalPages(p, 4, std::ptr::null());
        }
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// Page numbers given back are handed out again: a title that allocates
    /// and frees pages forever stays within the numbers it keeps (21 bits of
    /// `number >> 4`), and no number is handed out twice while it is owned.
    #[test]
    fn page_numbers_are_used_again_and_never_twice() {
        let first = 0x10_0000;
        let mut numbers = Numbers::new(first);
        let mut owned: std::collections::BTreeSet<usize> = Default::default();
        let mut live: Vec<(usize, usize)> = Vec::new();
        let mut seed = 12345u64;
        let mut random = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..200_000 {
            if live.len() < 32 && (live.is_empty() || random() % 2 == 0) {
                let pages = 1 + random() % 512;
                let start = numbers.take(pages * FRAMES);
                for k in 0..pages {
                    assert!(owned.insert(start + k * FRAMES), "{:#x} handed out twice", start + k * FRAMES);
                }
                live.push((start, pages));
            } else {
                let (start, pages) = live.swap_remove(random() % live.len());
                // Freed page by page, out of order, as the title may.
                let mut order: Vec<usize> = (0..pages).collect();
                for i in (1..order.len()).rev() {
                    order.swap(i, random() % (i + 1));
                }
                for k in order {
                    assert!(owned.remove(&(start + k * FRAMES)));
                    numbers.give(start + k * FRAMES, FRAMES);
                }
            }
            // At most 32 allocations of 512 pages are live at once.
            assert!(numbers.next - first <= 32 * 512 * FRAMES * 4, "numbers run on: {:#x}", numbers.next);
            assert!(((numbers.next >> 4) & !0x1f_ffff) == 0);
        }
        // All given back: the next number is the first again.
        for (start, pages) in live.drain(..) {
            for k in 0..pages {
                numbers.give(start + k * FRAMES, FRAMES);
            }
        }
        assert_eq!(numbers.next, first);
        assert!(numbers.free.is_empty());
    }

    /// A call with a page the title does not own fails before it changes
    /// anything: the pages mapped there before stay mapped, bytes and all.
    #[test]
    fn a_map_with_an_unknown_page_leaves_the_old_views() {
        let pages = allocate(3);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 3 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!a.is_null());
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe {
            for k in 0..3 {
                at(k).cast::<u8>().write(10 + k as u8);
            }
        }
        // The second page is unknown; the first and third would be other
        // pages than before (reversed).
        let bad = [pages[2], 1, pages[0]];
        assert!(XMemMapPhysicalPages(a, 3, bad.as_ptr()).is_null());
        for k in 0..3 {
            assert_eq!(query(at(k)).host.State, MEM_COMMIT, "page {k} still mapped");
            // SAFETY: still mapped, as checked.
            assert_eq!(unsafe { at(k).cast::<u8>().read() }, 10 + k as u8, "page {k} shows what it did");
        }
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// A thread that reads a mapped page in place, as the GPU side reads a
    /// texture, never meets the moment in which the runtime has taken the
    /// page's view away to map it again: mapping the same pages at the same
    /// address unmaps the view and maps a new one, and without the lock the
    /// reader faults on a page "reserved, never committed" (the crashes of
    /// runs 71 and 74).
    #[test]
    fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(1) };
        let first = a as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                // (Once at least: the mapping thread may be done before this
                // one's first turn.)
                while reads == 0 || !stop.load(Ordering::Relaxed) {
                    let _reading = victory_gpu::title_memory_reading();
                    // SAFETY: the page is mapped whenever no change of
                    // mapping is under way, and none is while `_reading`
                    // is held.
                    assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                    reads += 1;
                }
                reads
            })
        };
        for _ in 0..2000 {
            // The same pages at the same address: the view goes and comes
            // back, with the same bytes.
            assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(reader.join().expect("the reader never faulted") > 0);
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    /// A title thread that reads and writes a page of a view while other
    /// pages of the view are unmapped and mapped again, as the console lets
    /// it: the page was never given up.
    #[test]
    fn a_title_thread_goes_on_using_the_rest_of_a_view() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let pages = allocate(4);
        for _ in 0..200 {
            // A new reservation each time: the pieces of a view cut up stay
            // cut up.
            let a = XMemVirtualAlloc(
                std::ptr::null_mut(),
                4 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            );
            assert!(!a.is_null());
            assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
            let (first, stop) = (a as usize, AtomicBool::new(false));
            // SAFETY: mapped read-write.
            unsafe { (first as *mut u64).write_volatile(0) };
            std::thread::scope(|s| {
                let title = s.spawn(|| {
                    let mut n = 0u64;
                    // SAFETY (the block): the first page is the title's all
                    // the time; a fault would end the process.
                    unsafe {
                        while !stop.load(Ordering::Relaxed) {
                            assert_eq!((first as *const u64).read_volatile(), n);
                            n += 1;
                            (first as *mut u64).write_volatile(n);
                        }
                    }
                    n
                });
                // SAFETY: as above.
                while !title.is_finished() && unsafe { (first as *const u64).read_volatile() } < 100 {}
                for page in [3, 2, 1] {
                    let at = a.wrapping_byte_add(page * PAGE);
                    assert_eq!(XMemMapPhysicalPages(at, 1, std::ptr::null()), at);
                    assert_eq!(XMemMapPhysicalPages(at, 1, pages[page..].as_ptr()), at);
                }
                stop.store(true, Ordering::Relaxed);
                assert!(title.join().expect("the title's thread never faulted") > 0);
            });
            assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
        }
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// Many pages in one call: mapped, moved over each other's addresses,
    /// given twice in one list, copied by the GPU's mappings and unmapped.
    #[test]
    fn a_scatter_of_many_pages_is_what_one_after_the_other_is() {
        const N: usize = 200;
        let pages = allocate(N);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 2 * N * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE) as usize;
        let at = |k: usize| (a + k * PAGE) as *mut c_void;
        let first = |k: usize| at(k).cast::<u64>();
        for round in 0..4_usize {
            // Every address shows another page each round.
            let to: Vec<*mut c_void> = (0..N).map(|i| at((i + 7 * round) % N)).collect();
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), N, pages.as_ptr()), 1);
            for i in 0..N {
                // SAFETY: mapped read-write.
                unsafe {
                    if round > 0 {
                        assert_eq!(first((i + 7 * round) % N).read(), (1000 * (round - 1) + i) as u64);
                    }
                    first((i + 7 * round) % N).write((1000 * round + i) as u64);
                }
            }
        }
        // The GPU's copies of the mappings, in ranges and one by one; a page
        // mapped from one the same call has mapped shows the same.
        let half = (N / 2 * PAGE) as u64;
        let (from, to) = (a as u64, at(N) as u64);
        assert!(map_aliases(&[(to, from, half), (to + half, to, half)]));
        for k in 0..N / 2 {
            // SAFETY: mapped read-write.
            unsafe {
                assert_eq!(first(N + k).read(), first(k).read());
                assert_eq!(first(N + N / 2 + k).read(), first(k).read());
                first(N + k).write(5);
                assert_eq!(first(k).read(), 5);
            }
        }
        // An address twice: the last one counts; page 0 unmaps.
        let to = [at(0), at(1), at(0), at(2), at(3), at(0)];
        let what = [pages[5], pages[6], 0, 0, pages[7], pages[8]];
        assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), 6, what.as_ptr()), 1);
        assert_eq!(query(at(2)).host.State, MEM_RESERVE);
        // SAFETY: mapped read-write; the same page at both.
        unsafe {
            first(0).write(77);
            assert_eq!(XMemMapPhysicalPagesScatter([at(2), at(4), at(6)].as_ptr(), 3, [pages[8]; 3].as_ptr()), 1);
            assert_eq!((first(2).read(), first(4).read(), first(6).read()), (77, 77, 77));
        }
        // A page the title does not own: the call fails and every address
        // still shows what it showed, whole calls and one page alike.
        // SAFETY: mapped read-write, before the calls and after.
        unsafe {
            first(1).write(66);
            assert_eq!(XMemMapPhysicalPagesScatter([at(0), at(1)].as_ptr(), 2, [pages[0], 1].as_ptr()), 0);
            assert_eq!(XMemMapPhysicalPagesScatter([at(0), at(1)].as_ptr(), 2, [1, pages[0]].as_ptr()), 0);
            assert_eq!(XMemMapPhysicalPagesScatter([at(1)].as_ptr(), 1, [1usize].as_ptr()), 0);
            // (Asked first: a read of an unmapped page would fault.)
            let mapped: Vec<bool> = [at(0), at(1)].iter().map(|&a| physical().slice_at(a as usize).is_some()).collect();
            assert_eq!(mapped, [true, true], "still mapped");
            assert_eq!((first(0).read(), first(1).read()), (77, 66));
        }
        // All gone, also parts of a larger view.
        assert_eq!(XMemMapPhysicalPages(at(0), 4, std::ptr::null()), at(0));
        // SAFETY: placeholders of ours, nothing mapped in them.
        unsafe { VirtualFree(at(0), 4 * PAGE, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */) };
        assert_eq!(XMemMapPhysicalPages(at(0), 4, pages.as_ptr()), at(0));
        assert!(map_aliases(&[(a as u64, 0, (2 * N * PAGE) as u64)]));
        assert!((0..2 * N).all(|k| query(at(k)).host.State == MEM_RESERVE));
        assert!(physical().views.range(a..a + 2 * N * PAGE).next().is_none());
        assert_eq!(XMemFreePhysicalPages(N, pages.as_ptr()), 1);
    }

    /// Pages keep what they hold however often they are mapped, moved and
    /// unmapped without a commit or a split of their own.
    #[test]
    fn pages_mapped_again_hold_what_they_held() {
        let pages = allocate(3);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 8 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!a.is_null());
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        for round in 0..6_usize {
            for (k, page) in pages.iter().enumerate() {
                let to = at((round + 2 * k) % 8);
                assert_eq!(XMemMapPhysicalPages(to, 1, page), to);
                let first = to.cast::<u64>();
                // SAFETY: mapped read-write; the far end of the page too.
                unsafe {
                    if round > 0 {
                        assert_eq!(first.read(), (round * 16 + k - 16) as u64);
                        assert_eq!(first.add(PAGE / 8 - 1).read(), k as u64);
                    }
                    first.write((round * 16 + k) as u64);
                    first.add(PAGE / 8 - 1).write(k as u64);
                }
            }
            for k in 0..pages.len() {
                let to = at((round + 2 * k) % 8);
                assert_eq!(XMemMapPhysicalPages(to, 1, std::ptr::null()), to);
                assert_eq!(query(to).host.State, MEM_RESERVE);
            }
            // The title joins what it unmapped: the next view splits it anew.
            if round == 3 {
                // SAFETY: placeholders of ours, nothing mapped in them.
                unsafe {
                    VirtualFree(a, 8 * PAGE, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */)
                };
            }
        }
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// What a title that maps and unmaps pages all the time costs the
    /// workers that write others: 24 threads look pages up, fill one and
    /// tell of it, as a request does, while one maps.
    /// `cargo test --release -p victory-hle --lib map_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "a benchmark"]
    fn map_bench() {
        use std::sync::atomic::AtomicU64;
        use std::time::{Duration, Instant};
        let pages = allocate(1024);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 1024 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE) as usize;
        for mapping in [false, true] {
            let (done, waited, worst, maps) =
                (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
            let until = Instant::now() + Duration::from_secs(3);
            std::thread::scope(|s| {
                if mapping {
                    s.spawn(|| {
                        let at: Vec<*mut c_void> = (0..256).map(|i| (a + i * PAGE) as *mut c_void).collect();
                        while Instant::now() < until {
                            assert_eq!(XMemMapPhysicalPagesScatter(at.as_ptr(), 256, pages.as_ptr()), 1);
                            assert_eq!(XMemMapPhysicalPagesScatter(at.as_ptr(), 256, std::ptr::null()), 1);
                            maps.fetch_add(512, Ordering::Relaxed);
                        }
                    });
                }
                for t in 0..24 {
                    let (pages, done, waited, worst) = (&pages, &done, &waited, &worst);
                    s.spawn(move || {
                        let mut i = 256 + t * 16;
                        while Instant::now() < until {
                            let page = &pages[i..=i];
                            i = if i == 256 + t * 16 + 15 { 256 + t * 16 } else { i + 1 };
                            let began = Instant::now();
                            assert!(pages_known(page, 0, PAGE));
                            let mut window = page_window(page, 0, PAGE).unwrap();
                            let looked = began.elapsed();
                            window.bytes().fill(t as u8);
                            let began = Instant::now();
                            pages_written(page, 0, PAGE);
                            let took = (looked + began.elapsed()).as_nanos() as u64;
                            waited.fetch_add(took, Ordering::Relaxed);
                            worst.fetch_max(took, Ordering::Relaxed);
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                }
            });
            let n = done.load(Ordering::Relaxed).max(1);
            println!(
                "mapping {mapping}: {:.0} requests/s, {:.1} us of each on lookups and notices (worst {:.1} ms); {:.0} maps/s",
                n as f64 / 3.0,
                waited.load(Ordering::Relaxed) as f64 / n as f64 / 1e3,
                worst.load(Ordering::Relaxed) as f64 / 1e6,
                maps.load(Ordering::Relaxed) as f64 / 3.0
            );
        }
        assert_eq!(XMemFreePhysicalPages(1024, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = page_table().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// Pages say where the title has them mapped, byte for byte (after
    /// fork PR #571's test; its runs are whole pages).
    #[test]
    fn pages_say_where_they_are_mapped() {
        let pages = allocate(3);
        let reserve = |n: usize| {
            let at = XMemVirtualAlloc(std::ptr::null_mut(), n * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
            assert!(!at.is_null());
            at
        };
        // `b` keeps a page unmapped at either end, so that no run at `b`
        // ever touches one at `a`, wherever the host puts the two.
        let (a, b) = (reserve(3), reserve(4));
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty(), "mapped nowhere");
        // All three at `a`; the last two at `b`, from its second page on.
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        let second = b.wrapping_byte_add(PAGE);
        assert_eq!(XMemMapPhysicalPages(second, 2, pages[1..].as_ptr()), second);
        let (a, second) = (a as usize, second as usize);
        let sorted = |mut runs: Vec<(usize, usize)>| {
            runs.sort_unstable();
            runs
        };
        assert_eq!(shown_at(&pages, 0, PAGE), vec![(a, PAGE)]);
        // A request for part of a page names those bytes alone.
        assert_eq!(shown_at(&pages, 0x4000, 0x3000), vec![(a + 0x4000, 0x3000)]);
        // Across two pages, shown at two addresses each: one run at each.
        assert_eq!(
            shown_at(&pages, PAGE + 1, PAGE),
            sorted(vec![(a + PAGE + 1, PAGE), (second + 1, PAGE)])
        );
        // A page unmapped in the middle of a view: the rest still shows.
        let middle = (a + PAGE) as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(middle, 1, std::ptr::null()), middle);
        assert_eq!(
            shown_at(&pages, 0, 3 * PAGE),
            sorted(vec![(a, PAGE), (a + 2 * PAGE, PAGE), (second, 2 * PAGE)])
        );
        // Too few pages for the bytes: nothing (the write fails too).
        assert!(shown_at(&pages, 0, 3 * PAGE + 1).is_empty());
        for (at, n) in [(a, 3), (second, 2)] {
            let at = at as *mut c_void;
            assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
        }
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty());
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_is_busy_from_a_quarter_of_the_time_to_a_twentieth() {
        let ms = std::time::Duration::from_millis;
        assert!(!busy_after(false, ms(60), ms(250)) && busy_after(false, ms(70), ms(250)));
        assert!(busy_after(true, ms(15), ms(250)) && !busy_after(true, ms(12), ms(250)));
        // A window without a call, then one short one.
        assert!(!busy_after(true, ms(5), ms(4000)));
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }
}
