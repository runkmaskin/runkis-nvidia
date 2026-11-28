//! Write-combined title memory in the card's own memory
//! (`VICTORY_WC_VRAM=1`, off by default; docs/shared-memory.md).
//!
//! The title says which memory its CPU only writes: protection bit 0x400
//! (write-combined) in `XMemVirtualAlloc`. Its 1,277 MiB graphics pool on
//! Series S is such memory (protection 0x2404, one committed allocation at an
//! address of our choice). On the console the CPU writes it through its
//! write-combining buffers and the GPU reads and writes it in the same memory.
//! Here the layer keeps host copies of it, watches what the title writes,
//! sends that up, and fetches back and merges what shaders wrote: in a game
//! log of 2026-11-27 two copies in this pool alone had ~20 MB fetched back a
//! frame.
//!
//! With the switch the pool is card memory the CPU maps instead
//! (`D3D12_HEAP_TYPE_GPU_UPLOAD`, which needs a resizable BAR): one heap, one
//! buffer over all of it mapped for good, and that mapping IS the title's
//! allocation. The title's CPU writes go straight into the card's memory; title
//! buffers and windows in it are buffers placed in the heap
//! ([`crate::shadow::Shadow`]'s in-place form): no host copy, no write watch,
//! no refresh, no compare and no landing. What shaders write there is in the
//! title's memory when the card is done.
//!
//! **The address.** The mapping is placed by the system, not by us, and lands
//! above 2^40, where translated shaders cannot reach it (`upload.rs`). The
//! system puts it in the first free stretch big enough, searching upwards from
//! a place of its own and then from the bottom. So every other free stretch
//! that could hold it is reserved by us for the moment the heap is made
//! ([`steered`]): the only room left is the address `xmem` chose. Measured on
//! the 7800X3D / RTX 4070 SUPER: it lands there every time; it takes about
//! 8 s, once (the system's search through the reserved space).
//!
//! **What the CPU must not do**: read it. A read of this mapping is uncached
//! and crosses the bus (`bench_card_memory_the_title_writes`). Our own steps
//! that read title memory on the CPU (fills, buffer copies, textures) are
//! counted on the frame-rate line, so that a game run shows whether any read
//! it. The title promised not to read it much.
//!
//! **Order.** What a shader writes is visible to the CPU once the card is done;
//! the title learns that from its fences, which are set after the flight's
//! landing, as before. What the title's CPU writes must leave the CPU's
//! write-combining buffers before the card reads it. A write-combining buffer
//! is emptied by `sfence`, `mfence`, any locked instruction, a serializing
//! instruction, an access to uncached memory and an interrupt (Intel SDM vol.
//! 3, 11.3.1; AMD APM vol. 2, 7.7). The submitting thread empties its own in
//! the runtime's locked operations of `ExecuteCommandLists`. Another title
//! thread that wrote the data and told the submitting thread with a plain
//! store (a release store is a plain `mov` on x86) may still hold it. On the
//! console that would be the title's own race too, but here the title's
//! submission also reaches the card later, after our work; so
//! [`title_submits`] calls `FlushProcessWriteBuffers` when the title submits:
//! an interrupt on every processor that runs a thread of this process, which
//! empties every write-combining buffer of ours. A thread that is not running
//! emptied its own when it was switched out (an interrupt or system call).
//!
//! **What shaders write is declared** as for any copy (`Shadow::gpu_writes`),
//! so that a step of ours that reads it on the CPU waits for the flights that
//! write it, as it waited for their landing before. Their landing then has
//! nothing to merge.
//!
//! shortcut: the pool is never given back: the title keeps its pools for the
//! whole run. Its own `VirtualFree` of a part (not seen) would be refused by
//! the system for this mapping.
//! shortcut: another host copy of the same bytes (a texture whose memory lies
//! in the pool) is not told what shaders wrote there, as with
//! `VICTORY_INPLACE`; the frame-rate line counts textures staged from the pool.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows_sys::Win32::System::Memory as mem;

use crate::inplace::Placed;

const K64: usize = 64 << 10;
/// The least write-combined memory put in the card's memory.
const SMALLEST: usize = 16 << 20;
/// The cells windows of title memory are cut on.
const WINDOW: usize = 4 << 20;
/// The top of the process's address space that allocations may use.
const TOP: usize = 0x7fff_fffe_0000;

/// `VICTORY_WC_VRAM`: `1` puts write-combined title memory that the title
/// commits whole at an address of our choice into card memory the CPU maps;
/// unset or anything else: off, everything as before.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_WC_VRAM").is_ok_and(|v| v.trim() == "1"))
}

/// The most bytes of card memory given to it (`VICTORY_WC_VRAM_MB`, 2048).
fn most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| std::env::var("VICTORY_WC_VRAM_MB").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(2048) << 20)
}

/// A stretch of title memory that is card memory.
pub(crate) struct Pool {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) heap: ID3D12Heap,
    /// The buffer over all of it, mapped for good: the mapping is the
    /// title's memory.
    _whole: ID3D12Resource,
}

// SAFETY: D3D12 heaps and resources are free-threaded.
unsafe impl Send for Pool {}

static POOLS: Mutex<Vec<Pool>> = Mutex::new(Vec::new());

/// The lowest start and highest end of the pools (both 0: none), for the
/// quick looks on paths that run thousands of times a frame.
static BOUNDS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

/// Since the start: pools made and their bytes; refused (the card, the
/// address, the most); buffers and windows placed and their bytes. Then,
/// since the last line: fills read out of the pools on the CPU and their
/// bytes, buffer copies touching them and their bytes, textures staged from
/// them and their bytes, title submissions flushed and their nanoseconds,
/// constant buffers shown where they are and those copied as before (not on
/// 256 bytes), title buffer copies done on the card instead of the CPU.
static STATS: [AtomicU64; 19] = [const { AtomicU64::new(0) }; 19];
const S_POOLS: usize = 0;
const S_POOL_BYTES: usize = 1;
const S_NO_CARD: usize = 2;
const S_NO_ADDRESS: usize = 3;
const S_OVER: usize = 4;
const S_BUFFERS: usize = 5;
const S_WINDOWS: usize = 6;
const S_PLACED_BYTES: usize = 7;
const S_FILLS: usize = 8;
const S_FILL_BYTES: usize = 9;
const S_COPIES: usize = 10;
const S_COPY_BYTES: usize = 11;
const S_TEXTURES: usize = 12;
const S_TEXTURE_BYTES: usize = 13;
const S_FLUSHES: usize = 14;
const S_FLUSH_NS: usize = 15;
const S_CONSTANTS: usize = 16;
const S_CONSTANTS_UNALIGNED: usize = 17;
const S_CARD_COPIES: usize = 18;

fn add(i: usize, n: u64) {
    STATS[i].fetch_add(n, Ordering::Relaxed);
}

/// Whether `[at, at + len)` touches a pool (a quick look, no lock).
pub(crate) fn touches(at: usize, len: usize) -> bool {
    let hi = BOUNDS[1].load(Ordering::Relaxed);
    hi != 0 && at < hi && at.saturating_add(len) > BOUNDS[0].load(Ordering::Relaxed)
}

/// Whether all of `[at, at + len)` lies in one pool.
pub fn title_memory_in_card(at: usize, len: usize) -> bool {
    touches(at, len) && {
        let pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
        pools.iter().any(|p| p.start <= at && at.saturating_add(len) <= p.end)
    }
}

/// Free stretches of the address space, as `(start, end)`, 64 KiB aligned
/// inwards (an allocation starts on a 64 KiB boundary).
fn free_stretches() -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut at = K64;
    while at < TOP {
        // SAFETY: plain data filled in by the query.
        let mut m: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { mem::VirtualQuery(at as *const c_void, &mut m, size_of_val(&m)) } == 0 {
            break;
        }
        let (start, end) = (m.BaseAddress as usize, (m.BaseAddress as usize).saturating_add(m.RegionSize));
        if m.State == mem::MEM_FREE {
            let (s, e) = (start.next_multiple_of(K64), end.min(TOP) & !(K64 - 1));
            if s < e {
                out.push((s, e));
            }
        }
        if end <= at {
            break;
        }
        at = end;
    }
    out
}

/// Where a reservation of `size` bytes in `[lowest, highest]` aligned to
/// `align` (a power of two, or 0) would go: reserved and let go at once.
fn free_place(lowest: usize, highest: usize, align: usize, size: usize) -> Option<usize> {
    let mut requirements = mem::MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if align > K64 { align } else { 0 },
    };
    let mut parameter = mem::MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = mem::MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // SAFETY: a reservation of our own; the parameter outlives the call.
    let at = unsafe {
        mem::VirtualAlloc2(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            std::ptr::null(),
            size,
            mem::MEM_RESERVE,
            mem::PAGE_NOACCESS,
            &mut parameter,
            1,
        )
    } as usize;
    if at == 0 {
        return None;
    }
    // SAFETY: our reservation, released whole.
    unsafe { mem::VirtualFree(at as *mut c_void, 0, mem::MEM_RELEASE) };
    Some(at)
}

/// Runs `make` while every free stretch of the address space that could hold
/// `size` bytes, except the place a reservation with the requirements would
/// take ([`free_place`]), is reserved by us, so that a mapping the system
/// places while `make` runs can only go there. Stretches too small for `size`
/// stay free: other threads' smaller allocations go on as before (a bigger one
/// fails for that moment). Returns what `make` returned, the place, and
/// whether every other stretch could be reserved; `None` if there is no place.
pub(crate) fn steered<T>(lowest: usize, highest: usize, align: usize, size: usize, make: impl FnOnce() -> T) -> Option<(T, usize, bool)> {
    // One at a time: two would each hold the other's place.
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let target = free_place(lowest, highest, align, size)?;
    let end = target.saturating_add(size);
    let mut held: Vec<usize> = Vec::new();
    let mut all = true;
    for (s, e) in free_stretches() {
        // The part of the stretch before the target and the part after it.
        // Around the target all of it, however small: the system would start
        // the mapping in a free part right before the target and run on into it.
        let around = s <= target && end <= e;
        for (a, b) in [(s, e.min(target)), (s.max(end), e)] {
            if b <= a || (b - a < size && !around) {
                continue;
            }
            // SAFETY: a reservation of free address space, of our own.
            let p = unsafe { mem::VirtualAlloc(a as *const c_void, b - a, mem::MEM_RESERVE, mem::PAGE_NOACCESS) };
            if p.is_null() {
                all = false;
            } else {
                held.push(p as usize);
            }
        }
    }
    let made = make();
    for p in held {
        // SAFETY: our own reservations, released whole.
        unsafe { mem::VirtualFree(p as *mut c_void, 0, mem::MEM_RELEASE) };
    }
    Some((made, target, all))
}

fn buffer_desc(size: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        // Shaders write title buffers (unordered access).
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    }
}

/// A buffer of `size` bytes at `offset` (a multiple of 64 KiB) of `heap`,
/// which shaders may read and write, in the common state.
pub(crate) fn place(device: &ID3D12Device, heap: &ID3D12Heap, offset: u64, size: u64) -> windows::core::Result<ID3D12Resource> {
    let mut placed: Option<ID3D12Resource> = None;
    // SAFETY: plain creation in a live heap.
    unsafe { device.CreatePlacedResource(heap, offset, &buffer_desc(size), D3D12_RESOURCE_STATE_COMMON, None, &mut placed)? };
    placed.ok_or_else(windows::core::Error::empty)
}

/// Card memory the CPU maps, of `size` bytes (a multiple of 64 KiB): the
/// heap, the buffer over all of it and where it is mapped.
pub(crate) fn card_memory(device: &ID3D12Device, size: usize) -> windows::core::Result<(ID3D12Heap, ID3D12Resource, usize)> {
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: plain creation; the heap is zeroed, as committed memory is.
    unsafe {
        device.CreateHeap(
            &D3D12_HEAP_DESC {
                SizeInBytes: size as u64,
                Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_GPU_UPLOAD, ..Default::default() },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
            },
            &mut heap,
        )?
    };
    let heap = heap.ok_or_else(windows::core::Error::empty)?;
    let whole = place(device, &heap, 0, size as u64)?;
    let mut data = std::ptr::null_mut();
    // SAFETY: a CPU-visible buffer, mapped for good.
    unsafe { whole.Map(0, None, Some(&mut data))? };
    Ok((heap, whole, data as usize))
}

/// Makes `size` bytes of card memory the CPU maps at an address in
/// `[lowest, highest]` aligned to `align` (a power of two, or 0): where a
/// reservation with those requirements would go, steered there. `None` if the
/// card has no such memory, the system would not place it there, or another
/// thread took the place meanwhile.
fn card_memory_at(device: &ID3D12Device, lowest: usize, highest: usize, align: usize, size: usize) -> Option<(ID3D12Heap, ID3D12Resource, usize)> {
    // Another thread may take the place between the choice and the mapping
    // (seen with the crate's tests running beside it): tried again.
    (0..3).find_map(|_| card_memory_once(device, lowest, highest, align, size))
}

fn card_memory_once(device: &ID3D12Device, lowest: usize, highest: usize, align: usize, size: usize) -> Option<(ID3D12Heap, ID3D12Resource, usize)> {
    let started = std::time::Instant::now();
    let (made, target, all) = steered(lowest, highest, align, size, || card_memory(device, size))?;
    let (heap, whole, at) = match made {
        Ok(made) => made,
        Err(e) => {
            #[cfg(test)]
            eprintln!("no card memory: {e}");
            tracing::warn!(error = %e, size, "write-combined title memory: no card memory the CPU maps");
            return None;
        }
    };
    if at != target {
        #[cfg(test)]
        eprintln!("mapped at {at:#x}, not {target:#x}; every other stretch held: {all}");
        tracing::warn!(
            target = format_args!("{target:#x}"),
            at = format_args!("{at:#x}"),
            all,
            "write-combined title memory: the card memory was mapped elsewhere; the title's memory as before"
        );
        return None;
    }
    tracing::info!(
        at = format_args!("{at:#x}"),
        size,
        ms = started.elapsed().as_millis() as u64,
        "write-combined title memory: card memory the CPU maps, at the title's address"
    );
    Some((heap, whole, at))
}

/// The runtime is asked for `size` bytes of write-combined title memory,
/// committed, at an address of its choice in `[lowest, highest]` aligned to
/// `align`: with [`on`], the address of card memory the CPU maps there,
/// zeroed; `None` leaves the runtime to allocate as before.
pub fn title_memory_write_combined(lowest: usize, highest: usize, align: usize, size: usize) -> Option<usize> {
    // Small ones are left alone: the system finds room for them in the many
    // small free stretches steering leaves free (a 64 KiB one of the title's
    // went elsewhere in a game run), and they are not where the bytes are.
    if !on() || size < SMALLEST {
        return None;
    }
    // On whole windows of title memory (`shadow::loose`, 4 MiB cells): no
    // window lies half in the pool and half outside, which would be a copy
    // read through the bus.
    let size = size.next_multiple_of(WINDOW);
    let align = align.max(WINDOW);
    let held: usize = POOLS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|p| p.end - p.start).sum();
    if held + size > most() {
        add(S_OVER, 1);
        tracing::info!(size, held, "write-combined title memory: over VICTORY_WC_VRAM_MB, the title's memory as before");
        return None;
    }
    let Some(host) = crate::host::host() else {
        add(S_NO_CARD, 1);
        return None;
    };
    if !host.mapped_card_memory() {
        add(S_NO_CARD, 1);
        return None;
    }
    let Some((heap, whole, at)) = card_memory_at(&host.device, lowest, highest, align, size) else {
        add(S_NO_ADDRESS, 1);
        return None;
    };
    let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    pools.push(Pool { start: at, end: at + size, heap, _whole: whole });
    BOUNDS[0].store(pools.iter().map(|p| p.start).min().unwrap_or(0), Ordering::Relaxed);
    BOUNDS[1].store(pools.iter().map(|p| p.end).max().unwrap_or(0), Ordering::Relaxed);
    add(S_POOLS, 1);
    add(S_POOL_BYTES, size as u64);
    Some(at)
}

/// Title memory `[start, start + len)` as a buffer placed in its pool, if it
/// lies in one; `None` leaves the caller to do what it did before.
pub(crate) fn place_for(start: u64, len: u64, window: bool) -> Option<Placed> {
    let (s, n) = (start as usize, len.max(1) as usize);
    if !touches(s, n) {
        return None;
    }
    let host = crate::host::host()?;
    let pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    let pool = pools.iter().find(|p| p.start <= s && s + n <= p.end)?;
    let at = s & !(K64 - 1);
    let end = (s + n).next_multiple_of(K64).min(pool.end);
    let buffer = match place(&host.device, &pool.heap, (at - pool.start) as u64, (end - at) as u64) {
        Ok(buffer) => buffer,
        Err(e) => {
            tracing::warn!(start = format_args!("{start:#x}"), len, error = %e, "write-combined title memory: no buffer placed, a copy as before");
            return None;
        }
    };
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + (s - at) as u64;
    // Counted as `crate::inplace` counts what it places: the copy's drop
    // takes it off again.
    crate::inplace::counted(window, len);
    add(if window { S_WINDOWS } else { S_BUFFERS }, 1);
    add(S_PLACED_BYTES, len);
    Some(Placed { buffer, heap: pool.heap.clone(), gpu, bias: (s - at) as u64 })
}

/// Whether what shaders write in the copy `[start, start + len)` is to be
/// declared although the copy is in place: it is in a pool.
pub(crate) fn declares(start: u64, len: u64) -> bool {
    touches(start as usize, len as usize)
}

/// The title submits: every write-combining buffer of ours is emptied first
/// (see the module notes), if there is a pool.
pub(crate) fn title_submits() {
    if BOUNDS[1].load(Ordering::Relaxed) == 0 {
        return;
    }
    let started = std::time::Instant::now();
    // SAFETY: no arguments; it returns when every processor has run it.
    unsafe { windows_sys::Win32::System::Threading::FlushProcessWriteBuffers() };
    add(S_FLUSHES, 1);
    add(S_FLUSH_NS, started.elapsed().as_nanos() as u64);
}

/// A fill of upload memory read `len` bytes at `src` on the CPU.
pub(crate) fn filled(src: usize, len: usize) {
    if touches(src, len) {
        add(S_FILLS, 1);
        add(S_FILL_BYTES, len as u64);
    }
}

/// A title buffer copy was done on the CPU.
pub(crate) fn copied(dst: usize, src: usize, len: usize) {
    if touches(src, len) || touches(dst, len) {
        add(S_COPIES, 1);
        add(S_COPY_BYTES, len as u64);
    }
}

/// A texture's staging is brought up to date from title memory `[at, at + len)`.
pub(crate) fn textured(at: usize, len: usize) {
    if touches(at, len) {
        add(S_TEXTURES, 1);
        add(S_TEXTURE_BYTES, len as u64);
    }
}

/// Whether a constant buffer in a pool at host GPU address `gpu` is shown
/// where it is: on 256 bytes, as a constant buffer view must be. (Else it is
/// copied into upload memory as before, reading the pool on the CPU.) Like
/// renderer port 5, the card reads it when the draw runs, not as it was at
/// submission: the console's way.
pub(crate) fn constants_in_place(gpu: u64) -> bool {
    let aligned = gpu.is_multiple_of(256);
    add(if aligned { S_CONSTANTS } else { S_CONSTANTS_UNALIGNED }, 1);
    aligned
}

/// A title buffer copy into a pool was recorded as a copy on the card.
pub(crate) fn copied_on_card() {
    add(S_CARD_COPIES, 1);
}

// `VICTORY_WC_NO_BACK`: ported from trustno1-coder/VIctory-NVIDIA (GPL-3.0),
// commits 50c15a1 ("Write-combined title memory: no write-back of shader
// writes") and 2f81d0c (`VICTORY_WC_NO_BACK_MIN_KB`). Their measurement on a
// 4070 SUPER: gameplay 38.3 -> 33.3 ms a frame, cutscene median 47.3 -> 36.3
// ms with a floor of 1,024 KB; without the floor, cutscenes went near black.

/// Title memory the title asked for write-combined, as `[start, end)`.
static WRITE_COMBINED: std::sync::RwLock<Vec<(u64, u64)>> = std::sync::RwLock::new(Vec::new());

/// The title was given `[at, at + len)` write-combined (the console's 0x400
/// protection bit): its CPU writes that memory and does not read it. On the
/// console a read of it is uncached; results the CPU reads go to cacheable
/// memory.
pub fn title_memory_is_write_combined(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let (from, to) = (at as u64, at.saturating_add(len) as u64);
    let mut all = WRITE_COMBINED.write().unwrap_or_else(|e| e.into_inner());
    // Commits inside a reservation already known add nothing.
    if !all.iter().any(|&(a, b)| a <= from && to <= b) {
        all.push((from, to));
    }
}

/// The title let go of `[at, at + len)` (`len` 0: of the allocation at `at`):
/// write-combined regions in it are forgotten.
pub(crate) fn write_combined_freed(at: usize, len: usize) {
    let (at, end) = (at as u64, at.saturating_add(len) as u64);
    let mut all = WRITE_COMBINED.write().unwrap_or_else(|e| e.into_inner());
    if all.is_empty() {
        return;
    }
    all.retain(|&(from, to)| if len == 0 { from != at } else { to <= at || from >= end });
}

/// Whether all of `[start, start + len)` lies in one write-combined region
/// (they are hundreds of MB to GB).
pub(crate) fn is_write_combined(start: u64, len: u64) -> bool {
    let all = WRITE_COMBINED.read().unwrap_or_else(|e| e.into_inner());
    all.iter().any(|&(from, to)| from <= start && start.saturating_add(len) <= to)
}

/// Whether what shaders write into a copy in the card's memory of
/// write-combined title memory stays on the card: not compared, fetched or
/// merged into title memory (`VICTORY_WC_NO_BACK=1`; off by default).
///
/// What it risks: the title's CPU (or a step of ours, or another host copy of
/// the same bytes, such as a texture) reading what shaders wrote there reads
/// older bytes; a page the title writes goes up whole and takes back what
/// shaders wrote in it since. Hence [`no_back_min`]: small results the title
/// does read (exposure) live in small buffers there.
pub(crate) fn no_back() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WC_NO_BACK").is_some_and(|v| v == "1"))
}

/// The smallest copy [`no_back`] applies to, in bytes
/// (`VICTORY_WC_NO_BACK_MIN_KB`; unset: 0, all of them).
pub(crate) fn no_back_min() -> u64 {
    static MIN: OnceLock<u64> = OnceLock::new();
    *MIN.get_or_init(|| std::env::var("VICTORY_WC_NO_BACK_MIN_KB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0) * 1024)
}

/// Bytes declared written in write-combined copies and left on the card
/// ([`no_back`]), since the last frame-rate line.
static LEFT: AtomicU64 = AtomicU64::new(0);

/// [`no_back`]'s choice for a copy of `copy_len` bytes in the card's memory
/// that is write-combined title memory (`wc`, known by the caller): whether a
/// shader write of `len` bytes into it is left on the card (counted).
pub(crate) fn left_on_card(wc: bool, copy_len: u64, len: u64) -> bool {
    let left = wc && copy_len >= no_back_min();
    if left {
        LEFT.fetch_add(len, Ordering::Relaxed);
    }
    left
}

/// For the frame-rate line, while [`on`] or [`no_back`].
pub(crate) fn log(frames: f64) {
    if no_back() {
        tracing::info!(
            "write-combined title memory (VICTORY_WC_NO_BACK=1, VICTORY_WC_NO_BACK_MIN_KB={}): {:.1} MB a frame declared written by \
             shaders and left on the card (not compared, fetched or merged)",
            no_back_min() / 1024,
            LEFT.swap(0, Ordering::Relaxed) as f64 / 1e6 / frames.max(1.0)
        );
    }
    if !on() {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    let per: Vec<f64> = STATS[S_FILLS..].iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames.max(1.0)).collect();
    // This line's figures, by their statistic.
    let at = |i: usize| per[i - S_FILLS];
    let flushes = at(S_FLUSHES);
    tracing::info!(
        "write-combined title memory in the card's memory (VICTORY_WC_VRAM=1): {} pools ({} MB), {} title buffers and {} windows placed in them \
         ({} MB; no host copy, no write watch, nothing sent up or fetched back); per frame, ours reading it on the CPU (uncached): {:.0} fills ({:.1} KB), \
         {:.1} buffer copies ({:.1} KB), {:.1} textures staged ({:.1} KB); not read: {:.0} constant buffers shown where they are ({:.0} copied, not on 256 bytes), {:.1} buffer copies done on the card; {:.1} title submissions flushed the write buffers ({:.1} us each); \
         refused since the start: {} no card memory the CPU maps, {} not at the address, {} over VICTORY_WC_VRAM_MB",
        s[S_POOLS],
        s[S_POOL_BYTES] >> 20,
        s[S_BUFFERS],
        s[S_WINDOWS],
        s[S_PLACED_BYTES] >> 20,
        at(S_FILLS),
        at(S_FILL_BYTES) / 1024.0,
        at(S_COPIES),
        at(S_COPY_BYTES) / 1024.0,
        at(S_TEXTURES),
        at(S_TEXTURE_BYTES) / 1024.0,
        at(S_CONSTANTS),
        at(S_CONSTANTS_UNALIGNED),
        at(S_CARD_COPIES),
        flushes,
        if flushes > 0.0 { at(S_FLUSH_NS) / flushes / 1000.0 } else { 0.0 },
        s[S_NO_CARD],
        s[S_NO_ADDRESS],
        s[S_OVER],
    );
}

#[cfg(test)]
mod tests;
