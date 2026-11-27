//! Descriptor heaps: zeroed memory, with handles that are plain addresses
//! (CPU and GPU alike, as on the console).
//!
//! Descriptor sizes: views 32 bytes and samplers 16, the size of RDNA 2's
//! image and sampler descriptors, so a title that copies descriptors itself
//! moves whole ones. Render target and depth views are ours alone; 128 bytes
//! leaves room for whatever stride a title assumes.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{self, DESCRIPTOR_HEAP, E_INVALIDARG, HRESULT, Iid, Object, Table, This, guid};
use crate::resource::{Memory, PAGEABLE_IIDS};

#[cfg(windows)]
pub(crate) mod host;
#[cfg(windows)]
mod rounds;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x8efb471d,
        0x616c,
        0x4f49,
        [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    kind: u32,
    count: u32,
    flags: u32,
    nodes: u32,
}

/// Newer titles (the title) pass the description followed by
/// memory of their own for the descriptors: an address and its size in
/// bytes, or a null address to have the heap allocate (inferred from the
/// title's call sites; older titles pass the plain description).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Placement {
    address: u64,
    size: u64,
}

#[repr(C)]
pub(crate) struct Heap {
    object: Object,
    desc: HeapDesc,
    /// The first descriptor: in `memory`, or in the title's own memory.
    pub(crate) start: u64,
    memory: Option<Memory>,
    /// Shader-visible view and sampler heaps get a host heap with the same
    /// descriptor order when a draw first uses them (decision 0007, step
    /// 3); slots written before wait in `pending`.
    #[cfg(windows)]
    host: std::sync::OnceLock<Option<host::HostHeap>>,
    #[cfg(windows)]
    pending: std::sync::Mutex<Vec<u32>>,
    /// Changed pages of a placed shader-visible heap. Some titles copy
    /// descriptor records into it as bytes, without a descriptor API call.
    #[cfg(windows)]
    written: Option<crate::watch::Tracked>,
}

pub(crate) fn heap<'a>(this: This) -> &'a Heap {
    // SAFETY: called from descriptor heap methods only.
    unsafe { &*this.cast::<Heap>() }
}

impl Heap {
    /// Whether this is a sampler heap.
    pub(crate) fn samplers(&self) -> bool {
        self.desc.kind == SAMPLERS
    }

    /// The host heap, made on first use from what the title wrote so far.
    /// Made late because a host view heap the size of the title's
    /// costs over 100 MB of commit, which its boot needs for its page pool.
    #[cfg(windows)]
    pub(crate) fn host(&self) -> Option<&host::HostHeap> {
        if let Some(h) = self.host.get() {
            return h.as_ref();
        }
        if self.desc.flags & SHADER_VISIBLE == 0 || !matches!(self.desc.kind, VIEWS_KIND | SAMPLERS)
        {
            return None;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // A recorder can take the fast path as soon as the OnceLock is
        // initialized. Publish only after replaying the queued writes.
        self.host
            .get_or_init(|| {
                let step = u64::from(increment(self.desc.kind)?);
                let h = host::HostHeap::new(self.samplers(), self.desc.count)?;
                for i in pending.drain(..) {
                    h.mirror(i, self.start + u64::from(i) * step, false);
                }
                Some(h)
            })
            .as_ref()
    }

    /// The handle of descriptor `index` of this heap.
    #[cfg(windows)]
    pub(crate) fn handle(&self, index: u32) -> Option<u64> {
        let step = u64::from(increment(self.desc.kind)?);
        (index < self.desc.count).then(|| self.start + u64::from(index) * step)
    }

    /// The descriptor index of `handle` in this heap, if it lies in it.
    pub(crate) fn index(&self, handle: u64) -> Option<u32> {
        let step = u64::from(increment(self.desc.kind)?);
        let at = handle.checked_sub(self.start)?;
        let index = at / step;
        (index < u64::from(self.desc.count)).then_some(index as u32)
    }
}

const VIEWS_KIND: u32 = 0;
const SAMPLERS: u32 = 1;
const SHADER_VISIBLE: u32 = 1;

/// Shader-visible heaps by address range, for mirroring descriptor writes:
/// start, end, heap.
#[cfg(windows)]
///
/// Held shared while a heap found here is used (by every thread that writes
/// descriptors, and by [`sync_heaps`]), alone only where heaps come and go.
static SHADER_HEAPS: std::sync::RwLock<Vec<(u64, u64, usize)>> = std::sync::RwLock::new(Vec::new());

/// Makes the host descriptor for what the title wrote at `handle`, if it
/// is in a shader-visible heap. Keeps `SHADER_HEAPS` locked for the duration
/// so `Heap::drop` cannot free the heap concurrently.
#[cfg(windows)]
fn mirror(handle: u64) {
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    let Some(&(_, _, at)) = heaps
        .iter()
        .find(|&&(start, end, _)| (start..end).contains(&handle))
    else {
        return;
    };
    // SAFETY: registered heaps stay alive until `Heap::drop`, which takes
    // the `SHADER_HEAPS` lock we hold here.
    let heap = unsafe { &*(at as *const Heap) };
    let Some(index) = heap.index(handle) else {
        return;
    };
    // Renderer port #2: its record is looked at again with the title's own.
    crate::port_descriptors::mark(handle);
    let mut pending = heap.pending.lock().unwrap_or_else(|e| e.into_inner());
    match heap.host.get() {
        None => pending.push(index),
        Some(host) => {
            drop(pending);
            if let Some(host) = host {
                host.mirror(index, handle, false);
            }
        }
    }
}

/// Whether [`sync_heaps`] is done (`VICTORY_HEAP_SYNC=0`: not).
#[cfg(windows)]
fn sync_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_SYNC").is_none_or(|v| v != "0"))
}

/// Whether a submission's [`sync_heaps`] is done by a thread of its own
/// beside the rest of the preparing (`VICTORY_HEAP_SYNC_ASIDE=0`: in front of
/// it, as before).
///
/// It took 13 ms a frame of the submitting thread (session play-1125-0203:
/// 230 changed heap pages and 470,000 slots in 30 submissions). What it makes
/// are the host heap's descriptors, which the card reads: it has to be done
/// before the lists go to the queue, and nothing of the preparing looks at
/// them (what a draw reads through a table it made itself, when recorded).
#[cfg(windows)]
fn sync_aside() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| sync_on() && std::env::var_os("VICTORY_HEAP_SYNC_ASIDE").is_none_or(|v| v != "0"))
}

/// How long that thread waits for a submission before it makes a round of its
/// own (`VICTORY_HEAP_SYNC_EARLY=<ms>`, 4 if not given; `0`: none). The title
/// copies its records while it records, and the first submission of a frame
/// found all of them: 8 ms waited for (run cpu-2). A round made meanwhile
/// leaves it those written since.
#[cfg(windows)]
fn sync_early() -> Option<std::time::Duration> {
    static EVERY: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        let ms = std::env::var("VICTORY_HEAP_SYNC_EARLY").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(4u64);
        (ms > 0).then(|| std::time::Duration::from_millis(ms))
    })
}

/// The thread's work: rounds asked for, rounds done.
#[cfg(windows)]
static SYNC_ROUNDS: (std::sync::Mutex<(u64, u64)>, std::sync::Condvar) = (std::sync::Mutex::new((0, 0)), std::sync::Condvar::new());

/// LOCAL: whether the card, not the submitting thread, waits for a submission's round
/// (`VICTORY_HEAP_SYNC_GPU=1`; off). The submitting thread stood 10-12 ms a frame at the end
/// of its preparing for the round (RTX 2070 SUPER / Ryzen 5 5500X3D, 2026-11-27). With the
/// switch the drawing queue waits for a fence the round signals from the CPU
/// ([`SyncedHeaps::defer`]), and the thread goes on with the next submission. A round that is
/// not done 400 ms after its submission went is signalled anyway (a hitch, not a hang).
#[cfg(windows)]
pub(crate) fn sync_on_card() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| sync_aside() && std::env::var_os("VICTORY_HEAP_SYNC_GPU").is_some_and(|v| v == "1"))
}

#[cfg(windows)]
struct SyncFence(windows::Win32::Graphics::Direct3D12::ID3D12Fence);
// SAFETY: D3D12 fences are free-threaded.
#[cfg(windows)]
unsafe impl Send for SyncFence {}
#[cfg(windows)]
unsafe impl Sync for SyncFence {}

/// The fence that holds the rounds done ([`sync_on_card`]).
#[cfg(windows)]
fn sync_fence() -> Option<&'static windows::Win32::Graphics::Direct3D12::ID3D12Fence> {
    static FENCE: std::sync::OnceLock<Option<SyncFence>> = std::sync::OnceLock::new();
    FENCE
        .get_or_init(|| {
            let host = crate::host::host()?;
            // SAFETY: plain object creation.
            unsafe {
                host.device
                    .CreateFence::<windows::Win32::Graphics::Direct3D12::ID3D12Fence>(
                        0,
                        windows::Win32::Graphics::Direct3D12::D3D12_FENCE_FLAG_NONE,
                    )
                    .ok()
                    .map(SyncFence)
            }
        })
        .as_ref()
        .map(|f| &f.0)
}

/// Rounds signalled to the fence ahead of their end by the watch, since the start.
#[cfg(windows)]
pub(crate) static SYNC_FORCED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Submissions whose round the card waited for, since the start.
#[cfg(windows)]
pub(crate) static SYNC_DEFERRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The fence is at least `value` (never lowered).
#[cfg(windows)]
fn tell_fence(value: u64) {
    static TELLING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let Some(fence) = sync_fence() else {
        return;
    };
    let _one = TELLING.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: our fence.
    unsafe {
        if fence.GetCompletedValue() < value {
            let _ = fence.Signal(value);
        }
    }
}

/// Signals round `mine` 400 ms after its submission went if it is not done by then.
#[cfg(windows)]
fn watch_round(mine: u64) {
    static WATCH: std::sync::OnceLock<Option<std::sync::Mutex<std::sync::mpsc::Sender<(u64, std::time::Instant)>>>> =
        std::sync::OnceLock::new();
    let send = WATCH.get_or_init(|| {
        let (send, rounds) = std::sync::mpsc::channel::<(u64, std::time::Instant)>();
        crate::named::spawn("victory-heap-sync-watch", move || {
            for (mine, when) in rounds {
                if !round_done_by(&SYNC_ROUNDS, mine, when + std::time::Duration::from_millis(400)) {
                    tell_fence(mine);
                    if SYNC_FORCED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                        tracing::warn!(round = mine, "the heaps' records took over 400 ms: the card was let go on without them");
                    }
                }
            }
        })
        .ok()
        .map(|_| std::sync::Mutex::new(send))
    });
    match send {
        Some(send) => {
            if send.lock().unwrap_or_else(|e| e.into_inner()).send((mine, std::time::Instant::now())).is_err() {
                tell_fence(mine);
            }
        }
        // No watch: the card is not held at all.
        None => tell_fence(mine),
    }
}

/// Whether round `mine` of `rounds` (asked, done) is done by `due`: waits on
/// the condition the round's end tells (the watch slept 5 ms at a time and
/// looked, before). The same answer, without the polls.
#[cfg(windows)]
fn round_done_by(rounds: &(std::sync::Mutex<(u64, u64)>, std::sync::Condvar), mine: u64, due: std::time::Instant) -> bool {
    let mut r = rounds.0.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if r.1 >= mine {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= due {
            return false;
        }
        r = rounds.1.wait_timeout(r, due - now).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// The priority the heaps' records thread is given, if any: above normal with
/// `VICTORY_POOL_PRIORITY=1`, as the submitting thread that waits for it at
/// the end of each submission (10.2 ms a frame, at most 25.5, in the profile
/// of 2026-11-27, where it ran at normal beside the title's threads); off,
/// none: normal, as before.
#[cfg(windows)]
fn heap_sync_priority(raised: bool) -> Option<i32> {
    raised.then_some(windows_sys::Win32::System::Threading::THREAD_PRIORITY_ABOVE_NORMAL)
}

/// [`sync_heaps`], begun; done when it is dropped (with [`sync_aside`]), or
/// when this returns.
#[cfg(windows)]
pub(crate) struct SyncedHeaps(Option<u64>);

/// LOCAL: whether rounds are also asked at the top of a submission, and a
/// submission whose round has not begun when it is needed runs it itself
/// (`VICTORY_HEAP_ROUNDS_EARLY=1`; off, as before: [`rounds`]). The submitting
/// thread waited 10.7-13.3 ms a frame for 17 rounds of 0.5 ms, each begun
/// 0.3 ms after it was asked for (measured in game, 8d8db49). Not with
/// `VICTORY_HEAP_SYNC_GPU=1`, whose card waits on the old rounds' fence.
#[cfg(windows)]
pub(crate) fn rounds_early() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        sync_aside() && !sync_on_card() && std::env::var_os("VICTORY_HEAP_ROUNDS_EARLY").is_some_and(|v| v == "1")
    })
}

/// The rounds of [`rounds_early`].
#[cfg(windows)]
static EARLY: rounds::Rounds = rounds::Rounds::new();

/// Since the last frame line, with [`rounds_early`]: rounds asked at the top of
/// a submission; submissions' rounds run on the submitting thread, their
/// microseconds, and the microseconds those waited first for a round under way.
#[cfg(windows)]
static EARLY_STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// [`rounds_early`]: the round's thread, started once. Without it the
/// submissions run every round themselves.
#[cfg(windows)]
fn start_early_rounds() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = crate::named::spawn("victory-heap-sync", || {
            if let Some(priority) = heap_sync_priority(crate::land::raised()) {
                use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
                // SAFETY: this thread's own priority.
                unsafe { SetThreadPriority(GetCurrentThread(), priority) };
            }
            crate::cores::keep_on_fast_cores("victory-heap-sync");
            EARLY.serve(sync_early(), |asked| {
                let _round =
                    crate::timeline::span(if asked { "the heaps' records, for a submission" } else { "the heaps' records, early" });
                let _ = std::panic::catch_unwind(|| sync_round(Some(asked)));
            });
        });
    });
}

/// [`rounds_early`]: asks for a round at the top of a submission, so that it
/// runs beside the card steps and the wait for flights. The submission still
/// waits for the round asked in [`begin_sync_heaps`], which finds only what was
/// written since. Nothing without the switch.
#[cfg(windows)]
pub(crate) fn ask_sync_heaps_ahead() {
    if !rounds_early() {
        return;
    }
    start_early_rounds();
    EARLY.ask();
    ROUND_ASKED_US.store(crate::host::now_us(), std::sync::atomic::Ordering::Relaxed);
    EARLY_STATS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(windows)]
pub(crate) fn begin_sync_heaps() -> SyncedHeaps {
    if !sync_aside() {
        sync_heaps();
        return SyncedHeaps(None);
    }
    if rounds_early() {
        start_early_rounds();
        let mine = EARLY.ask();
        ROUND_ASKED_US.store(crate::host::now_us(), std::sync::atomic::Ordering::Relaxed);
        return SyncedHeaps(Some(mine));
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    static THREAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    STARTED.call_once(|| {
        let made = crate::named::spawn("victory-heap-sync", || {
            if let Some(priority) = heap_sync_priority(crate::land::raised()) {
                use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
                // SAFETY: this thread's own priority.
                unsafe { SetThreadPriority(GetCurrentThread(), priority) };
            }
            crate::cores::keep_on_fast_cores("victory-heap-sync");
            let rounds = || SYNC_ROUNDS.0.lock().unwrap_or_else(|e| e.into_inner());
            /// A round is over, however it ends.
            struct Over;
            impl Drop for Over {
                fn drop(&mut self) {
                    let mut rounds = SYNC_ROUNDS.0.lock().unwrap_or_else(|e| e.into_inner());
                    rounds.1 += 1;
                    let done = rounds.1;
                    drop(rounds);
                    if sync_on_card() {
                        tell_fence(done);
                    }
                    SYNC_ROUNDS.1.notify_all();
                }
            }
            // Between the rounds asked for, while submissions come.
            let mut last = std::time::Instant::now();
            loop {
                let mut r = rounds();
                let mut asked = r.0 != r.1;
                while !asked {
                    let Some(every) = sync_early().filter(|_| last.elapsed().as_millis() < 500) else {
                        r = SYNC_ROUNDS.1.wait(r).unwrap_or_else(|e| e.into_inner());
                        asked = r.0 != r.1;
                        continue;
                    };
                    let (back, over) = SYNC_ROUNDS.1.wait_timeout(r, every).unwrap_or_else(|e| e.into_inner());
                    r = back;
                    asked = r.0 != r.1;
                    if over.timed_out() {
                        break;
                    }
                }
                drop(r);
                let _over = asked.then(|| {
                    last = std::time::Instant::now();
                    Over
                });
                let _round =
                    crate::timeline::span(if asked { "the heaps' records, for a submission" } else { "the heaps' records, early" });
                let _ = std::panic::catch_unwind(sync_heaps);
            }
        });
        THREAD.store(made.is_ok(), std::sync::atomic::Ordering::Release);
    });
    if !THREAD.load(std::sync::atomic::Ordering::Acquire) {
        sync_heaps();
        return SyncedHeaps(None);
    }
    let mut rounds = SYNC_ROUNDS.0.lock().unwrap_or_else(|e| e.into_inner());
    rounds.0 += 1;
    let mine = rounds.0;
    ROUND_ASKED_US.store(crate::host::now_us(), std::sync::atomic::Ordering::Relaxed);
    drop(rounds);
    SYNC_ROUNDS.1.notify_all();
    SyncedHeaps(Some(mine))
}

#[cfg(windows)]
impl SyncedHeaps {
    /// [`sync_on_card`]: `queue` waits for this round instead of the caller; `false` if the
    /// caller is to wait (the switch is off, or the queue cannot wait).
    pub(crate) fn defer(&mut self, queue: &windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue) -> bool {
        let Some(mine) = self.0 else {
            return false;
        };
        if !sync_on_card() {
            return false;
        }
        let Some(fence) = sync_fence() else {
            return false;
        };
        if SYNC_ROUNDS.0.lock().unwrap_or_else(|e| e.into_inner()).1 >= mine {
            self.0 = None;
            return true;
        }
        // SAFETY: our fence and the host's queue.
        if unsafe { queue.Wait(fence, mine) }.is_err() {
            return false;
        }
        self.0 = None;
        SYNC_DEFERRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        watch_round(mine);
        true
    }
}

#[cfg(windows)]
impl Drop for SyncedHeaps {
    fn drop(&mut self) {
        let Some(mine) = self.0 else {
            return;
        };
        let began = std::time::Instant::now();
        if rounds_early() {
            use std::sync::atomic::Ordering::Relaxed;
            // Not begun yet: run here rather than wait for the thread to wake.
            let had = EARLY.wait_or_run(mine, || {
                let _round = crate::timeline::span("the heaps' records, for a submission, on its thread");
                let _ = std::panic::catch_unwind(|| sync_round(Some(true)));
            });
            if had.ran {
                EARLY_STATS[1].fetch_add(1, Relaxed);
                EARLY_STATS[2].fetch_add(had.round_us, Relaxed);
                EARLY_STATS[3].fetch_add(had.behind_us, Relaxed);
            }
            SYNCED[6].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
            return;
        }
        let mut rounds = SYNC_ROUNDS.0.lock().unwrap_or_else(|e| e.into_inner());
        while rounds.1 < mine {
            rounds = SYNC_ROUNDS.1.wait(rounds).unwrap_or_else(|e| e.into_inner());
        }
        drop(rounds);
        SYNCED[6].fetch_add(began.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Mirrors records copied directly into placed shader-visible heaps. Calls
/// to `Create*View` are already mirrored by [`mirror`]; raw CPU copies leave
/// only write-watch pages to reveal which slots changed.
///
/// Upstream VIctory 86a5ed6 does the same and counts 1,300 to 14,400 such
/// slots a frame in the world of the title this fork works on, against a
/// few hundred views made by calls; without it those host slots stayed
/// null, or went on showing what they showed before.
/// `VICTORY_HEAP_SYNC=0`: not done, as before. [`SYNCED`] counts.
#[cfg(windows)]
pub(crate) fn sync_heaps() {
    sync_round(None);
}

/// [`sync_heaps`]; `asked`: whether a submission asked for it, if the caller
/// knows ([`rounds_early`]).
#[cfg(windows)]
fn sync_round(asked: Option<bool>) {
    use std::sync::atomic::Ordering::Relaxed;
    if !sync_on() {
        return;
    }
    let started = std::time::Instant::now();
    // For a submission that waits (asked and not done), or early.
    let for_submission = match asked {
        Some(asked) => asked,
        None => {
            sync_aside() && {
                let rounds = SYNC_ROUNDS.0.lock().unwrap_or_else(|e| e.into_inner());
                rounds.0 != rounds.1
            }
        }
    };
    let began_us = crate::host::now_us();
    let (mut pages, mut looked, mut mirrored, mut tabled, mut later) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut views, mut bytes) = (0u64, 0u64);
    let mut spent = [0u64; 3];
    let since = |at: std::time::Instant| at.elapsed().as_nanos() as u64;
    let mut kinds = [0u64; 5];
    let mut job_ns = [0u64; 3];
    let mut short = 0u64;
    let mut wait: Vec<u32> = Vec::new();
    let frame = crate::frame::current();
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    for &(start, end, at) in heaps.iter() {
        // SAFETY: registered heaps are live (removed when dropped).
        let heap = unsafe { &*(at as *const Heap) };
        let (Some(Some(host)), Some(written), Some(step)) =
            (heap.host.get(), &heap.written, increment(heap.desc.kind))
        else {
            continue;
        };
        let step = u64::from(step);
        // Capped host heaps have no counterparts for the title slots reserved
        // for per-draw tables; raw writes there must not be mirrored.
        let mirrored_count = heap.desc.count.min(host.count());
        // The records of some frames ago that no draw read through a table
        // since: a shader may pick them by index.
        let before = host.copied_before(frame);
        let of_tables = std::sync::atomic::AtomicU64::new(0);
        let (pages_a_job, records_a_job) = sync_jobs();
        let phase = crate::timeline::span("the heaps' records: those that waited");
        crate::land::each_view(true, before.len().div_ceil(records_a_job), &|n| {
            for &index in before.chunks(records_a_job).nth(n).unwrap_or_default() {
                if host.copied_tabled(index, frame) {
                    of_tables.fetch_add(1, Relaxed);
                } else {
                    host.mirror(index, start + u64::from(index) * step, false);
                }
            }
        });
        phase.end();
        let of_tables = of_tables.into_inner();
        tabled += of_tables;
        mirrored += before.len() as u64 - of_tables;
        kinds[4] += before.len() as u64 - of_tables;
        // Those of the changed pages, made together at the end.
        let mut make: Vec<u32> = Vec::new();
        // Ranges of the heap to look at, and whether a range is one the
        // fallback of renderer port #2 looks at whole.
        let mut parts: Vec<(usize, usize, bool)> = Vec::new();
        let port = crate::port_descriptors::adopted(start);
        // With `VICTORY_PORT_DESCRIPTORS_APART`: the fallback's pages cut into
        // pieces, and pages, for the count of pages looked at.
        let mut recount: Option<(u64, u64)> = None;
        if port {
            // Renderer port #2: the slots the title's patched stores and our
            // calls marked, in place of the changed pages; and a few of the
            // pages the watch says were written, looked at whole.
            let phase = crate::timeline::span("the heaps' records: the marks taken (renderer port 2)");
            for (first, n) in crate::port_descriptors::take(start).unwrap_or_default() {
                let from = start as usize + first as usize * step as usize;
                parts.push((from, from + n as usize * step as usize, false));
            }
            phase.end();
            if crate::port_descriptors::ask_due(frame) {
                let began = std::time::Instant::now();
                let phase = crate::timeline::span("the heaps' records: asking (renderer port 2's fallback)");
                let taken = written.take(start as usize, end as usize);
                phase.end();
                spent[0] += since(began);
                crate::port_descriptors::asked(&taken);
            }
            if crate::port_descriptors::apart() {
                // Without the slots just taken: each slot in one part
                // (`port_descriptors::apart`).
                let pages: Vec<(usize, usize)> = crate::port_descriptors::to_check(frame)
                    .into_iter()
                    .map(|page| (page.max(start as usize), (page + crate::watch::PAGE).min(end as usize)))
                    .collect();
                let runs: Vec<(usize, usize)> = parts.iter().map(|&(from, to, _)| (from, to)).collect();
                let pieces = crate::port_descriptors::outside(&runs, &pages);
                recount = Some((pieces.len() as u64, pages.len() as u64));
                parts.extend(pieces.into_iter().map(|(from, to)| (from, to, true)));
            } else {
                for page in crate::port_descriptors::to_check(frame) {
                    parts.push((page.max(start as usize), (page + crate::watch::PAGE).min(end as usize), true));
                }
            }
        } else {
            let began = std::time::Instant::now();
            let phase = crate::timeline::span("the heaps' records: asking");
            let taken = written.take(start as usize, end as usize);
            phase.end();
            spent[0] += since(began);
            for (range_from, range_end) in taken {
                let mut page = range_from & !(crate::watch::PAGE - 1);
                while page < range_end {
                    parts.push((page.max(range_from), (page + crate::watch::PAGE).min(range_end), false));
                    page += crate::watch::PAGE;
                }
            }
        }
        // The marks' runs are short: more of them a job.
        let pages_a_job = if port { pages_a_job.max(parts.len() / 48) } else { pages_a_job };
        // A few pages a thread at a time: read, compared with what was last
        // seen there and sorted out, beside one another (14 ms a frame of one
        // thread at 160 pages, run faults-6).
        let began = std::time::Instant::now();
        let copied = host.copied_begin(step as usize);
        let samplers = heap.samplers();
        let in_place = sync_in_place() && step == 32 && crate::safecopy::on();
        let found: Vec<std::sync::Mutex<SyncPart>> = parts.chunks(pages_a_job).map(|_| Default::default()).collect();
        // A part taken that could not be read, or not all of it: looked at
        // again at the next sync (its page marked changed again, or its
        // slots marked), else its slots kept the records seen before until
        // the title wrote the page once more.
        // (`lead`: the first record whose first byte is in the part.)
        let again = |lead: usize, from: usize, to: usize| {
            if port {
                let mut handle = lead;
                while handle < to {
                    crate::port_descriptors::mark(handle as u64);
                    handle += step as usize;
                }
            } else {
                written.again(from);
            }
        };
        let phase = crate::timeline::span("the heaps' records: pages read and compared");
        crate::land::each_view(true, found.len(), &|n| {
            let mut out = SyncPart::default();
            let mut page_bytes: Vec<u8> = Vec::new();
            let mut changed: Vec<u32> = Vec::new();
            let mut bits: Vec<u64> = Vec::new();
            for &(from, to, check) in parts.chunks(pages_a_job).nth(n).unwrap_or_default() {
                let checking = check.then(std::time::Instant::now);
                let slots = changed_slots(from, to, start as usize, step, mirrored_count);
                // A record that straddles the range's ends is left
                // to the range that holds its first byte.
                let first = ((from - start as usize) as u64).div_ceil(step).max(slots.start);
                let lead = (start + first * step) as usize;
                let records: &[u8] = if in_place {
                    // Read where they are, each record once, and compared
                    // as it is read (`host::Copied::changes_in_place`).
                    let at = std::time::Instant::now();
                    let count = (to.saturating_sub(lead) as u64 / step).min(slots.end.saturating_sub(first));
                    changed.clear();
                    // SAFETY: as below; title memory is read by a copy that
                    // fails where a page is not there.
                    let whole = unsafe { copied.changes_in_place(first as u32, lead as *const u8, count as usize, &mut bits, &mut changed) };
                    out.ns[1] += since(at);
                    // (A run of marked slots is no page; nor does it ask the guards.)
                    if !port || check {
                        out.pages += 1;
                        out.views += u64::from(crate::watch::guarded(from, to));
                    }
                    out.bytes += (to - from) as u64;
                    out.looked += count;
                    out.short += u64::from(!whole);
                    if !whole {
                        again(lead, from, to);
                    }
                    // What was read of the changed records is in the records
                    // last seen now.
                    // SAFETY: this part's records alone, which nothing else
                    // writes meanwhile.
                    unsafe { copied.seen(first as u32, count as usize) }
                } else {
                    // The page's records are read through the kernel, into a
                    // copy: a page the title unmaps meanwhile fails the read
                    // where 2,048 reads in place would fault.
                    let at = std::time::Instant::now();
                    page_bytes.resize(to - from, 0);
                    if !crate::host::read_into(from as *const u8, &mut page_bytes) {
                        again(lead, from, to);
                        continue;
                    }
                    out.ns[0] += since(at);
                    out.pages += 1;
                    out.views += u64::from(crate::watch::guarded(from, to));
                    out.bytes += (to - from) as u64;
                    let Some(records) = page_bytes.get(lead.wrapping_sub(from)..) else {
                        continue;
                    };
                    let count = (records.len() as u64 / step).min(slots.end.saturating_sub(first));
                    out.looked += count;
                    // Only the records that are not the ones last seen
                    // there (`host::Copied::changes`).
                    changed.clear();
                    let at = std::time::Instant::now();
                    // SAFETY: the parts are ranges of the heap apart from one
                    // another, and only records that lie whole in one are its.
                    unsafe { copied.changes(first as u32, &records[..(count * step) as usize], &mut changed) };
                    out.ns[1] += since(at);
                    records
                };
                let at = std::time::Instant::now();
                for &index in &changed {
                    let at = (u64::from(index) - first) as usize * step as usize;
                    let word = |n: usize| {
                        records
                            .get(at + 4 * n..at + 4 * n + 4)
                            .and_then(|b| b.try_into().ok())
                            .map_or(0, u32::from_le_bytes)
                    };
                    if check {
                        // Renderer port #2's fallback: what no mark told of.
                        crate::port_descriptors::missed(index, start + u64::from(index) * step, !samplers && word(0) == VIEW_TAG, host.copied_tabled(index, frame));
                    }
                    if !samplers && word(0) == VIEW_TAG {
                        // A draw's own table: the draw makes what it
                        // reads of it (`draw.rs` `tables`).
                        if host.copied_tabled(index, frame) {
                            out.tabled += 1;
                            continue;
                        }
                        // Buffer views and placed textures get no host
                        // slot from this anyway, they wait for a draw
                        // or a pass: noted some frames on, unless a
                        // draw's table turns out to hold them.
                        if matches!(word(2) >> KIND_SHIFT, CONSTANT | PLACED) && host.copied_waits() {
                            out.wait.push(index);
                            continue;
                        }
                    }
                    // No view, in a slot that shows none: nothing to make or
                    // unmake (32,000 of 41,000 a frame on the water, run
                    // b2-water).
                    if word(0) != VIEW_TAG && host.shows_none(index) {
                        out.nothing += 1;
                        continue;
                    }
                    out.make.push(index);
                    out.kinds[if samplers {
                        3
                    } else if word(0) != VIEW_TAG {
                        0
                    } else if word(2) >> KIND_SHIFT == 0 {
                        1
                    } else {
                        2
                    }] += 1;
                }
                out.ns[2] += since(at);
                if let Some(began) = checking {
                    crate::port_descriptors::CHECKED[4].fetch_add(since(began), Relaxed);
                }
            }
            *found[n].lock().unwrap_or_else(|e| e.into_inner()) = out;
        });
        phase.end();
        drop(copied);
        for part in found {
            let mut part = part.into_inner().unwrap_or_else(|e| e.into_inner());
            pages += part.pages;
            views += part.views;
            bytes += part.bytes;
            looked += part.looked;
            short += part.short;
            tabled += part.tabled;
            kinds[0] += part.nothing;
            later += part.wait.len() as u64;
            mirrored += part.make.len() as u64;
            wait.append(&mut part.wait);
            make.append(&mut part.make);
            for (kind, n) in kinds.iter_mut().zip(part.kinds) {
                *kind += n;
            }
            for (ns, n) in job_ns.iter_mut().zip(part.ns) {
                *ns += n;
            }
        }
        if let Some((pieces, whole)) = recount {
            // A piece of a page counted as a page: the pages are counted.
            pages = pages.saturating_sub(pieces) + whole;
        }
        spent[1] += since(began);
        let began = std::time::Instant::now();
        let phase = crate::timeline::span("the heaps' records: made");
        crate::land::each_view(true, make.len().div_ceil(records_a_job), &|n| {
            for &index in make.chunks(records_a_job).nth(n).unwrap_or_default() {
                host.mirror(index, start + u64::from(index) * step, false);
            }
        });
        phase.end();
        spent[2] += since(began);
        if !wait.is_empty() {
            host.copied_wait(frame, std::mem::take(&mut wait));
        }
    }
    drop(heaps);
    let micros = started.elapsed().as_micros() as u64;
    for (count, n) in SYNCED.iter().zip([pages, looked, mirrored, micros, tabled, later]) {
        count.fetch_add(n, Relaxed);
    }
    let [taking, reading, making] = spent;
    for (count, n) in SYNC_SPLIT.iter().zip([views, bytes, taking, reading, making]) {
        count.fetch_add(n, Relaxed);
    }
    for (count, n) in SYNCED_KINDS.iter().zip(kinds) {
        count.fetch_add(n, Relaxed);
    }
    for (count, n) in SYNC_JOBS_NS.iter().zip(job_ns) {
        count.fetch_add(n, Relaxed);
    }
    SYNC_SHORT.fetch_add(short, Relaxed);
    if sync_aside() {
        if for_submission {
            ROUNDS[0].fetch_add(1, Relaxed);
            ROUNDS[1].fetch_add(micros, Relaxed);
            ROUNDS[2].fetch_add(began_us.saturating_sub(ROUND_ASKED_US.load(Relaxed)), Relaxed);
        } else {
            ROUNDS[3].fetch_add(1, Relaxed);
            ROUNDS[4].fetch_add(micros, Relaxed);
        }
    }
}

/// When the last round was asked for ([`crate::host::now_us`]).
#[cfg(windows)]
static ROUND_ASKED_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Since the last frame line, [`sync_heaps`]'s rounds on its thread: for
/// submissions (begun while one waited), their microseconds, and the
/// microseconds from the last ask to their beginning (an early round in
/// front of them, or the thread not running); early rounds and their
/// microseconds. Then the microseconds of [`make_waiting`].
#[cfg(windows)]
static ROUNDS: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// With the frame line: where the submitting thread's wait at its end goes.
#[cfg(windows)]
fn rounds_line(frames: f64, waited_us: u64) {
    let [asked, asked_us, late_us, early, early_us, waiting_us] =
        [0, 1, 2, 3, 4, 5].map(|i| ROUNDS[i].swap(0, std::sync::atomic::Ordering::Relaxed) as f64);
    if rounds_early() {
        let [ahead, ran, ran_us, behind_us] = [0, 1, 2, 3].map(|i| EARLY_STATS[i].swap(0, std::sync::atomic::Ordering::Relaxed) as f64);
        let per = |us: f64, n: f64| if n > 0.0 { us / n / 1e3 } else { 0.0 };
        tracing::info!(
            "the heaps' records' rounds asked early (VICTORY_HEAP_ROUNDS_EARLY=1) per frame: {:.1} asked at the top of a submission; \
             {:.1} submissions ran their round themselves, of {:.2} ms each after {:.2} ms behind a round under way; \
             the submitting thread waited {:.2} ms in all for them (off: the same figure of the next line)",
            ahead / frames,
            ran / frames,
            per(ran_us, ran),
            per(behind_us, ran),
            waited_us as f64 / 1e3 / frames,
        );
    }
    if asked + early == 0.0 {
        return;
    }
    let per = |us: f64, n: f64| if n > 0.0 { us / n / 1e3 } else { 0.0 };
    tracing::info!(
        "the heaps' records' rounds per frame: {:.1} for submissions of {:.2} ms each, begun {:.2} ms after they were asked for; \
         {:.1} early of {:.2} ms each; the submitting thread waited {:.2} ms for them and {:.2} ms making slots that waited for a \
         texture (the two are its \"at the end waiting for the heaps' records\")",
        asked / frames,
        per(asked_us, asked),
        per(late_us, asked),
        early / frames,
        per(early_us, early),
        waited_us as f64 / 1e3 / frames,
        waiting_us / 1e3 / frames,
    );
}

/// Pages [`sync_heaps`] read in place of which a part could not be read
/// ([`sync_in_place`]), since the last frame line.
#[cfg(windows)]
static SYNC_SHORT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether [`sync_heaps`] reads a changed page's records where they are, each
/// once, and compares each with the one last seen as it reads it, 32 bytes at
/// a time (`VICTORY_SYNC_IN_PLACE=0`: into a copy of the page first, then
/// compared, as before).
///
/// The copy and the compare were each a pass over all of the page: 7.4 and
/// 5.3 ms a frame of the six threads for 35 MB (run heapsync-ab1, in the
/// street). A page the title unmaps meanwhile fails where the copy failed;
/// the records before the place are taken as they were read.
///
/// On by default since run heapsync-ab2 (A/B in the street: the pass 12.7 ms
/// a frame against 15.7-16.2, the copy and compare 7.4 ms of its threads
/// against 13.3, the submitting thread's wait for it 2.0 ms against 2.2-2.6;
/// the picture the same).
#[cfg(windows)]
fn sync_in_place() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_SYNC_IN_PLACE") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SYNC_IN_PLACE").is_none_or(|v| v != "0"))
}

/// Of [`sync_heaps`]'s page jobs since the last frame line, on all threads:
/// nanoseconds reading pages, comparing them with what was last seen, and
/// sorting out the records that changed.
#[cfg(windows)]
static SYNC_JOBS_NS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Pages and records [`sync_heaps`] gives a thread at a time
/// (`VICTORY_SYNC_FINE=1`: [`SYNC_PAGES_FINE`] and [`SYNC_AT_A_TIME_FINE`];
/// otherwise [`SYNC_PAGES`] and [`SYNC_AT_A_TIME`], as before).
///
/// A round for a submission finds a dozen pages or so: by eight a job that
/// was two jobs, on two of the six threads, while the submission waited
/// for the round (2 to 3.4 ms at a time, run city4).
#[cfg(windows)]
fn sync_jobs() -> (usize, usize) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let asked = || *ON.get_or_init(|| std::env::var_os("VICTORY_SYNC_FINE").is_some_and(|v| v != "0"));
    #[cfg(feature = "diagnostics")]
    let fine = crate::ab::pick("VICTORY_SYNC_FINE").unwrap_or_else(asked);
    #[cfg(not(feature = "diagnostics"))]
    let fine = asked();
    if fine { (SYNC_PAGES_FINE, SYNC_AT_A_TIME_FINE) } else { (SYNC_PAGES, SYNC_AT_A_TIME) }
}

#[cfg(windows)]
const SYNC_PAGES_FINE: usize = 2;

#[cfg(windows)]
const SYNC_AT_A_TIME_FINE: usize = 64;

/// Of those mirrored, since the last frame line: no view, a view of an object,
/// another, a sampler; waited and no draw came.
#[cfg(windows)]
static SYNCED_KINDS: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// What a thread found on its pages of a heap, for [`sync_heaps`].
#[cfg(windows)]
#[derive(Default)]
struct SyncPart {
    pages: u64,
    views: u64,
    bytes: u64,
    looked: u64,
    tabled: u64,
    nothing: u64,
    make: Vec<u32>,
    wait: Vec<u32>,
    kinds: [u64; 5],
    /// Nanoseconds reading, comparing, sorting out ([`SYNC_JOBS_NS`]).
    ns: [u64; 3],
    /// Pages read in place not all there ([`SYNC_SHORT`]).
    short: u64,
}

/// Pages [`sync_heaps`] gives a thread at a time.
#[cfg(windows)]
const SYNC_PAGES: usize = 8;

/// Records [`sync_heaps`] gives a thread at a time.
#[cfg(windows)]
const SYNC_AT_A_TIME: usize = 256;

/// Since the last frame line, of [`sync_heaps`]: changed pages of placed
/// shader-visible heaps looked at, slots on them, slots whose record was
/// not the one last seen and was mirrored, the microseconds it took; then
/// new records left to the draws that read them through a table, and new
/// records that waited for one ([`host::HostHeap::copied_wait`]); and the
/// microseconds submissions waited for it ([`sync_aside`]).
#[cfg(windows)]
pub(crate) static SYNCED: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];

/// The same, of [`SYNCED`]'s pages: those in view memory that was guarded;
/// bytes read; nanoseconds asking what was written, reading it and comparing
/// it with what was last seen, making views.
#[cfg(windows)]
static SYNC_SPLIT: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// With the frame line: what [`sync_heaps`] did per frame over `frames`.
/// Nothing while no heap page changed.
#[cfg(windows)]
pub(crate) fn sync_line(frames: f64) {
    crate::port_descriptors::line(frames);
    let [twins, moved, lost] = [0, 1, 2].map(|i| host::TWINS[i].swap(0, std::sync::atomic::Ordering::Relaxed));
    if twins > 0 {
        tracing::info!(
            "raw twins of buffer views made per frame (VICTORY_RAW_VIEWS=0: none): {:.0}, of them {:.1} that do not begin on 16 bytes ({:.1} null)",
            twins as f64 / frames,
            moved as f64 / frames,
            lost as f64 / frames
        );
    }
    let [pages, looked, mirrored, micros, tabled, later, waited] = [0, 1, 2, 3, 4, 5, 6].map(|i| SYNCED[i].swap(0, std::sync::atomic::Ordering::Relaxed));
    let split = [0, 1, 2, 3, 4].map(|i| SYNC_SPLIT[i].swap(0, std::sync::atomic::Ordering::Relaxed) as f64);
    let kinds = [0, 1, 2, 3, 4].map(|i| SYNCED_KINDS[i].swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / frames);
    let jobs = [0, 1, 2].map(|i| SYNC_JOBS_NS[i].swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / 1e6 / frames);
    let quick = [0, 1].map(|i| host::SPANS_QUICK[i].swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / frames);
    let short = SYNC_SHORT.swap(0, std::sync::atomic::Ordering::Relaxed);
    rounds_line(frames, waited);
    if pages == 0 {
        return;
    }
    tracing::info!(
        "the heaps' records' jobs per frame (VICTORY_SYNC_FINE={}, VICTORY_SYNC_IN_PLACE={}): on all threads {:.2} ms reading pages, \
         {:.2} comparing (in place: reading too), {:.2} sorting out; writes of slots {:.0} leaving the spans alone (VICTORY_SPANS_QUICK), \
         {:.0} under their lock; {} pages read in place not all there",
        u8::from(sync_jobs().0 == SYNC_PAGES_FINE),
        u8::from(sync_in_place()),
        jobs[0],
        jobs[1],
        jobs[2],
        quick[0],
        quick[1],
        short
    );
    tracing::info!(
        "descriptor records the title copied into its heap as bytes (VICTORY_HEAP_SYNC=0: not looked for): per frame \
         {:.1} changed heap pages, {:.0} slots looked at, {:.1} whose record was new and was mirrored, {:.1} new ones of \
         draws' tables left to the draws, {:.1} that waited for a draw first, {:.2} ms ({:.2} ms waited for); of the mirrored \
         {:.0} no views (with those of slots that show none), {:.0} of objects, {:.0} others, {:.0} samplers, {:.0} that waited and no draw came; \
         {:.1} of the pages guarded views, {:.1} MB read; {:.2} ms asking, {:.2} reading and comparing, {:.2} making",
        pages as f64 / frames,
        looked as f64 / frames,
        mirrored as f64 / frames,
        tabled as f64 / frames,
        later as f64 / frames,
        micros as f64 / 1000.0 / frames,
        waited as f64 / 1000.0 / frames,
        kinds[0],
        kinds[1],
        kinds[2],
        kinds[3],
        kinds[4],
        split[0] / frames,
        split[1] / (1 << 20) as f64 / frames,
        split[2] / 1e6 / frames,
        split[3] / 1e6 / frames,
        split[4] / 1e6 / frames
    );
    if sync_on_card() {
        tracing::info!(
            "the heaps' records waited for by the card (VICTORY_HEAP_SYNC_GPU=1), since the start: {} submissions, {} let go after 400 ms",
            SYNC_DEFERRED.load(std::sync::atomic::Ordering::Relaxed),
            SYNC_FORCED.load(std::sync::atomic::Ordering::Relaxed)
        );
    }
}

/// Title descriptor slots in a changed byte range that have host counterparts.
/// `range_end` is exclusive.
#[cfg(windows)]
fn changed_slots(
    range_from: usize,
    range_end: usize,
    heap_start: usize,
    step: u64,
    mirrored_count: u32,
) -> std::ops::Range<u64> {
    let from = (range_from - heap_start) as u64;
    let to = (range_end - heap_start) as u64;
    let first = from / step;
    let last = to.div_ceil(step).min(u64::from(mirrored_count));
    first.min(last)..last
}

/// The title's view heap that shaders of a list without one get their views
/// from (`draw.rs`, `bind`): the title's structure builder binds root views
/// only, and the host shows those, and memory by address, through heap slots.
/// The biggest there is. `VICTORY_LENT_HEAP=0`: none, such a dispatch is
/// dropped, as before.
#[cfg(windows)]
pub(crate) fn lent_heap() -> Option<usize> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_LENT_HEAP").is_none_or(|v| v != "0")) {
        return None;
    }
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    heaps
        .iter()
        // SAFETY: registered heaps are live (removed when dropped).
        .map(|&(_, _, h)| (h, unsafe { &*(h as *const Heap) }))
        .filter(|(_, heap)| !heap.samplers() && matches!(heap.host.get(), Some(Some(_))))
        .max_by_key(|(_, heap)| heap.desc.count)
        .map(|(h, _)| h)
}

/// `f` of the title heap at `heap`, if that heap still exists; it stays
/// until `f` is done.
#[cfg(windows)]
pub(crate) fn with_heap<T>(heap: usize, f: impl FnOnce(&Heap) -> T) -> Option<T> {
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    // SAFETY: registered heaps are live (removed when dropped).
    heaps.iter().any(|&(_, _, h)| h == heap).then(|| f(unsafe { &*(heap as *const Heap) }))
}

/// Gives back dynamic slots of the host heap behind the title heap at
/// `heap`, if that heap still exists and its host heap is the one numbered
/// `serial` ([`host::HostHeap::serial`]) that handed them out: a heap made
/// where a released one was would otherwise free slots other lists hold.
#[cfg(windows)]
pub(crate) fn release_dynamic(heap: usize, serial: u64, first: u32, n: u32) {
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    if heaps.iter().any(|&(_, _, h)| h == heap)
        // SAFETY: registered heaps are live (removed when dropped).
        && let Some(Some(host)) = unsafe { &*(heap as *const Heap) }.host.get()
        && host.serial() == serial
    {
        host.release(first, n);
    }
}

/// Runs `f` with the host heap of the title's largest view heap, if there is
/// one (nothing is waited for).
#[cfg(windows)]
pub(crate) fn with_view_heap<T>(f: impl FnOnce(&host::HostHeap) -> T) -> Option<T> {
    let heaps = SHADER_HEAPS.try_read().ok()?;
    let host = heaps
        .iter()
        // SAFETY: registered heaps are live (removed when dropped).
        .map(|&(_, _, at)| unsafe { &*(at as *const Heap) })
        .filter(|heap| !heap.samplers())
        .filter_map(|heap| heap.host.get()?.as_ref())
        .max_by_key(|host| host.count())?;
    Some(f(host))
}

/// Makes dynamic slot `index` of the host heap behind the title heap at
/// `heap`, if that heap still exists, the constant buffer view of `size`
/// bytes of `buffer` at `gpu`.
#[cfg(windows)]
pub(crate) fn constant_slot(
    heap: usize,
    index: u32,
    buffer: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    gpu: u64,
    size: u32,
) {
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    if heaps.iter().any(|&(_, _, h)| h == heap)
        // SAFETY: registered heaps are live (removed when dropped).
        && let Some(Some(host)) = unsafe { &*(heap as *const Heap) }.host.get()
    {
        host.constant_at(index, buffer, gpu, size);
    }
}

#[cfg(windows)]
impl Drop for Heap {
    fn drop(&mut self) {
        let at = std::ptr::from_ref::<Heap>(self) as usize;
        let mut heaps = SHADER_HEAPS.write().unwrap_or_else(|e| e.into_inner());
        heaps.retain(|&(_, _, h)| h != at);
        crate::port_descriptors::let_go(self.start);
        drop(heaps);
        // What the heap pass kept of it holds host copies, which could
        // never be let go (`shadow::let_go_idle`) while it stayed.
        if let Some(Some(host)) = self.host.get() {
            crate::draw::forget_heap(host.serial());
        }
    }
}

/// Bytes per descriptor, by heap type (views, samplers, render targets,
/// depth).
fn increment(kind: u32) -> Option<u32> {
    [32, 16, 128, 128].get(kind as usize).copied()
}

/// Device method.
pub(crate) extern "system" fn increment_size(_: This, kind: u32) -> u32 {
    increment(kind).unwrap_or(0)
}

/// What our render target views hold: the resource and the view
/// description, turned into a host view when a command uses them.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RtvRecord {
    tag: u32,
    described: u32,
    pub(crate) resource: This,
    desc: [u32; 6],
}
const RTV_TAG: u32 = u32::from_le_bytes(*b"vRTV");
const _: () = assert!(size_of::<RtvRecord>() <= 112);

impl RtvRecord {
    /// All the view it records is made from: records with equal keys have
    /// equal views (of the same host texture).
    #[cfg(windows)]
    pub(crate) fn key(&self) -> (usize, [u32; 7]) {
        let mut words = [self.described; 7];
        words[1..].copy_from_slice(&self.desc);
        (self.resource as usize, words)
    }

    /// The view description in the host's type (the same layout).
    #[cfg(windows)]
    pub(crate) fn desc(
        &self,
    ) -> Option<windows::Win32::Graphics::Direct3D12::D3D12_RENDER_TARGET_VIEW_DESC> {
        const TEXTURE_2D: u32 = 4;
        self.host_desc(TEXTURE_2D)
    }

    /// The depth view description in the host's type (the same layout).
    #[cfg(windows)]
    pub(crate) fn depth_desc(
        &self,
    ) -> Option<windows::Win32::Graphics::Direct3D12::D3D12_DEPTH_STENCIL_VIEW_DESC> {
        const TEXTURE_2D: u32 = 3;
        self.host_desc(TEXTURE_2D)
    }

    /// The description words as a host type, the format as the host has it.
    /// Without one, a typeless host copy gets a 2D view (`dimension`) in
    /// the title's format, which the host needs spelled out.
    #[cfg(windows)]
    fn host_desc<T>(&self, dimension: u32) -> Option<T> {
        assert!(size_of::<T>() <= size_of::<[u32; 6]>());
        let mut words = if self.described != 0 {
            self.desc
        } else {
            [
                crate::resource::view_format(self.resource)?,
                dimension,
                0,
                0,
                0,
                0,
            ]
        };
        // A target or depth view: of a target, in its stand-in format.
        words[0] = crate::resource::target_format(crate::resource::pc_format(words[0]));
        // SAFETY: both host view descriptions are 24 bytes in this layout.
        Some(unsafe { (&raw const words).cast::<T>().read_unaligned() })
    }
}

/// Device method: records the view at `handle` (descriptor memory).
pub(crate) extern "system" fn create_rtv(
    _: This,
    resource: This,
    desc: *const [u32; 6],
    handle: u64,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: the title passes a description or null.
    let described = unsafe { desc.as_ref() };
    let record = RtvRecord {
        tag: RTV_TAG,
        described: u32::from(described.is_some()),
        resource,
        desc: described.copied().unwrap_or_default(),
    };
    // SAFETY: the handle is a descriptor slot, at least 112 bytes.
    unsafe { (handle as *mut RtvRecord).write_unaligned(record) };
}

/// Device method: a depth view, recorded as render target views are.
pub(crate) extern "system" fn create_dsv(
    this: This,
    resource: This,
    desc: *const [u32; 6],
    handle: u64,
) {
    create_rtv(this, resource, desc, handle);
    if handle != 0 {
        // SAFETY: just written by `create_rtv`.
        unsafe { (handle as *mut u32).write_unaligned(DSV_TAG) };
    }
}
const DSV_TAG: u32 = u32::from_le_bytes(*b"vDSV");

/// A shader view or sampler as the title described it (descriptions in the
/// host's layout, kept as words), for the host to build its own from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum View {
    /// Constant buffer: GPU address and size.
    Constant(u64, u32),
    /// Shader resource: resource, description (none: the resource's own).
    Shader(usize, Option<[u32; 10]>),
    /// Unordered access: resource, counter resource, description.
    Unordered(usize, usize, Option<[u32; 10]>),
    Sampler([u32; 13]),
    /// The console's own sampler description: flags, filters per axis,
    /// addressing, LOD and anisotropy settings, border color, then
    /// console-only tuning words.
    ConsoleSampler([u32; 27]),
    /// A view of memory without a resource object: whether it is unordered
    /// access, the resource description, then the view's (the plain one
    /// followed by the memory address and console fields).
    Placed(bool, [u32; 14], [u32; 16]),
}

impl View {
    /// Whether the view reads buffer memory (constant buffers, buffer views
    /// of resources or of memory) or memory the title placed a texture in:
    /// such views get host slots only for draws.
    #[cfg(windows)]
    fn is_buffer(&self) -> bool {
        match self {
            View::Constant(..) | View::Placed(..) => true,
            View::Shader(r, _) | View::Unordered(r, _, _) => {
                crate::resource::buffer_bytes(*r as This).is_some()
            }
            View::Sampler(_) | View::ConsoleSampler(_) => false,
        }
    }
}

/// Every distinct view in use; a descriptor holds a tag, an index into it
/// and the entry's generation, which survive titles copying descriptors as
/// bytes. A view of a resource lives as long as the resource: when the
/// title releases it, its entries are freed for reuse under a new
/// generation, so descriptors (and host slots made from them) naming the
/// old one go stale instead of naming whatever is made at the same address
/// next.
///
/// Views without a resource object have nothing that releases them, and a
/// title copies descriptors as bytes, so nothing says when the last
/// descriptor naming an entry is gone: such entries were kept for the run.
/// The title writes some 14,000 constant buffer views a frame,
/// each at the address its constants have that frame: 2,450,139 entries
/// after four minutes of play, 1,047 MB with the index, and 11,000 more
/// every second (run 65). So the two kinds that carry an address are not
/// entries any more: their records name them whole ([`constant_words`],
/// [`placed_words`]), and the table holds only what is bounded: views of
/// resource objects (freed with the resource), samplers (as many as there
/// are sampler states), and the shapes of placed views (their descriptions
/// without the address: as many as the title has kinds of textures and
/// buffers, however often it moves them).
/// `VICTORY_VIEW_INLINE=0`: every view is an entry, as before.
struct Views {
    /// Each entry's view and generation.
    list: Vec<(View, u32)>,
    index: std::collections::BTreeMap<View, u32>,
    /// Entries naming each resource.
    by_resource: std::collections::BTreeMap<usize, Vec<u32>>,
    free: Vec<u32>,
    /// The shapes of views of placed memory, and where each is.
    shapes: Vec<Shape>,
    shape_index: std::collections::BTreeMap<Shape, u32>,
}

/// Read far more often than written: every slot a draw reads through a
/// table is looked up here, on several recording threads at once in the
/// world. Readers share it; only a view not seen before (a new table entry
/// or shape) and a released resource take it alone.
static VIEWS: std::sync::RwLock<Views> = std::sync::RwLock::new(Views {
    list: Vec::new(),
    index: std::collections::BTreeMap::new(),
    by_resource: std::collections::BTreeMap::new(),
    free: Vec::new(),
    shapes: Vec::new(),
    shape_index: std::collections::BTreeMap::new(),
});
pub(crate) const VIEW_TAG: u32 = u32::from_le_bytes(*b"vVIE");

/// A record is the tag and two words, `i` and `generation` (the host keeps
/// them as one 64-bit key per slot). The top two bits of `generation` say
/// what the other 62 bits are: 0, entry `i` of the table at that
/// generation; [`CONSTANT`], a constant buffer view, whole; [`PLACED`], a
/// view of placed memory: its address and the number of its shape.
const KIND_SHIFT: u32 = 30;
/// Table generations stay below the kind bits (and above 0).
const GENERATIONS: u32 = (1 << KIND_SHIFT) - 1;
const CONSTANT: u32 = 1;
const PLACED: u32 = 2;
/// Bits of a constant buffer view's address in its record: any address a
/// program can have. The 15 bits above them are its size.
const CONSTANT_ADDRESS_BITS: u32 = 47;
/// Bits of a placed view's address in its record: placed memory is title
/// memory, which lies below 1 TiB. The 22 bits above them number its shape.
const PLACED_ADDRESS_BITS: u32 = 40;
const SHAPES: usize = 1 << (62 - PLACED_ADDRESS_BITS);

/// A view of placed memory without its address: whether it is unordered
/// access, the resource description, and the view's words but the two that
/// hold the address (10 and 11).
type Shape = (bool, [u32; 14], [u32; 14]);

/// Whether constant buffer views and views of placed memory are named whole
/// in their records ([`Views`]). On by default; `VICTORY_VIEW_INLINE=0`
/// makes every view a table entry, as before.
fn inline() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_INLINE").is_none_or(|v| v != "0"))
}

/// Views named whole in their records since the start: constant buffer
/// views, views of placed memory (for the memory line).
static NAMED_WHOLE: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The two record words of the 62 bits `payload` of kind `kind`.
fn kind_words(kind: u32, payload: u64) -> [u32; 2] {
    [payload as u32, kind << KIND_SHIFT | (payload >> 32) as u32]
}

/// The 62 bits two record words carry beside their kind.
fn payload(i: u32, generation: u32) -> u64 {
    u64::from(generation & GENERATIONS) << 32 | u64::from(i)
}

/// The record words that name the constant buffer view of `size` bytes at
/// `address` whole; `None` if they cannot: an address past 47 bits, or a
/// size of 16 KiB or more that is no multiple of 256 or reaches 4 MiB (the
/// size has 15 bits: bytes below 16 KiB, else units of 256 bytes). Such a
/// view is a table entry.
fn constant_words(address: u64, size: u32) -> Option<[u32; 2]> {
    if address >> CONSTANT_ADDRESS_BITS != 0 {
        return None;
    }
    let code = if size < 1 << 14 {
        u64::from(size)
    } else if size.is_multiple_of(256) && size >> 8 < 1 << 14 {
        1 << 14 | u64::from(size >> 8)
    } else {
        return None;
    };
    Some(kind_words(CONSTANT, code << CONSTANT_ADDRESS_BITS | address))
}

/// The constant buffer view [`constant_words`] named.
fn constant_of(i: u32, generation: u32) -> View {
    let payload = payload(i, generation);
    let code = (payload >> CONSTANT_ADDRESS_BITS) as u32;
    let size = if code >> 14 == 0 { code } else { (code & 0x3fff) << 8 };
    View::Constant(payload & ((1 << CONSTANT_ADDRESS_BITS) - 1), size)
}

/// A placed view's shape and its address.
fn shape_of(unordered: bool, resource: &[u32; 14], desc: &[u32; 16]) -> (Shape, u64) {
    let mut rest = [0u32; 14];
    rest[..10].copy_from_slice(&desc[..10]);
    rest[10..].copy_from_slice(&desc[12..]);
    (
        (unordered, *resource, rest),
        u64::from(desc[10]) | u64::from(desc[11]) << 32,
    )
}

/// The record words that name the view of placed memory at `address` with
/// shape number `shape`.
fn placed_words(shape: u32, address: u64) -> [u32; 2] {
    kind_words(PLACED, u64::from(shape) << PLACED_ADDRESS_BITS | address)
}

impl Views {
    /// The number of `shape`, a new one if it is not there yet; `None` when
    /// the shapes are full (the view is then a table entry).
    fn shape(&mut self, shape: Shape) -> Option<u32> {
        if let Some(&at) = self.shape_index.get(&shape) {
            return Some(at);
        }
        if self.shapes.len() >= SHAPES {
            return None;
        }
        let at = self.shapes.len() as u32;
        self.shapes.push(shape);
        self.shape_index.insert(shape, at);
        Some(at)
    }

    /// The view of placed memory [`placed_words`] named.
    fn placed_of(&self, i: u32, generation: u32) -> Option<View> {
        let payload = payload(i, generation);
        let address = payload & ((1 << PLACED_ADDRESS_BITS) - 1);
        let &(unordered, resource, rest) = self.shapes.get((payload >> PLACED_ADDRESS_BITS) as usize)?;
        let mut desc = [0u32; 16];
        desc[..10].copy_from_slice(&rest[..10]);
        (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
        desc[12..].copy_from_slice(&rest[10..]);
        Some(View::Placed(unordered, resource, desc))
    }

    /// The record words of `view` as a table entry, if it has one.
    fn entry_of(&self, view: &View) -> Option<[u32; 2]> {
        let &i = self.index.get(view)?;
        Some([i, self.list[i as usize].1])
    }

    /// The record words of `view` as a table entry: its entry (a new or a
    /// freed one if the view is not there yet) and that entry's generation.
    fn entry(&mut self, view: View) -> [u32; 2] {
        let i = match self.index.get(&view) {
            Some(&i) => i,
            None => {
                let i = match self.free.pop() {
                    Some(i) => {
                        self.list[i as usize].0 = view.clone();
                        i
                    }
                    None => {
                        self.list.push((view.clone(), 1));
                        self.list.len() as u32 - 1
                    }
                };
                for r in view.resources().into_iter().filter(|&r| r != 0) {
                    self.by_resource.entry(r).or_default().push(i);
                }
                self.index.insert(view, i);
                i
            }
        };
        [i, self.list[i as usize].1]
    }
}

/// The two words that name `view` in a record: the view itself where its
/// kind allows ([`Views`]), else its table entry.
fn words_for(view: View) -> [u32; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    if inline() {
        match &view {
            View::Constant(address, size) => {
                if let Some(words) = constant_words(*address, *size) {
                    NAMED_WHOLE[0].fetch_add(1, Relaxed);
                    return words;
                }
            }
            View::Placed(unordered, resource, desc) => {
                let (shape, address) = shape_of(*unordered, resource, desc);
                if address >> PLACED_ADDRESS_BITS == 0 {
                    // Nearly every placed view has a shape seen before.
                    let known = views_read().shape_index.get(&shape).copied();
                    let shape = known.or_else(|| views_write().shape(shape));
                    if let Some(shape) = shape {
                        NAMED_WHOLE[1].fetch_add(1, Relaxed);
                        return placed_words(shape, address);
                    }
                }
            }
            _ => {}
        }
    }
    // A view written before keeps its entry and generation.
    if let Some(words) = views_read().entry_of(&view) {
        return words;
    }
    views_write().entry(view)
}

fn views_read() -> std::sync::RwLockReadGuard<'static, Views> {
    VIEWS.read().unwrap_or_else(|e| e.into_inner())
}

fn views_write() -> std::sync::RwLockWriteGuard<'static, Views> {
    VIEWS.write().unwrap_or_else(|e| e.into_inner())
}

/// For the memory line: the distinct views kept ([`Views`]), those of them
/// free for reuse, and the bytes the table and its index hold for them (an
/// estimate: an entry of the table, and one of the index with its share of
/// a tree node; the shapes likewise); then the entries that are constant
/// buffer views and views of placed memory (those that did not fit a
/// record, or all of them with `VICTORY_VIEW_INLINE=0`), the shapes of
/// placed views kept, and the views named whole in their records since the
/// start: constant buffer views, views of placed memory.
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn views_kept() -> [u64; 8] {
    use std::sync::atomic::Ordering::Relaxed;
    let views = views_read();
    let each = size_of::<(View, u32)>() as u64;
    let shape = size_of::<Shape>() as u64;
    let (mut constants, mut placed) = (0, 0);
    for (view, _) in &views.list {
        match view {
            View::Constant(..) => constants += 1,
            View::Placed(..) => placed += 1,
            _ => {}
        }
    }
    [
        views.list.len() as u64,
        views.free.len() as u64,
        views.list.capacity() as u64 * each
            + views.index.len() as u64 * each * 3 / 2
            + views.shapes.capacity() as u64 * shape
            + views.shape_index.len() as u64 * shape * 3 / 2,
        constants,
        placed,
        views.shapes.len() as u64,
        NAMED_WHOLE[0].load(Relaxed),
        NAMED_WHOLE[1].load(Relaxed),
    ]
}

impl View {
    /// The resource objects the view names.
    fn resources(&self) -> [usize; 2] {
        match *self {
            View::Shader(r, _) => [r, 0],
            View::Unordered(r, counter, _) => [r, counter],
            _ => [0, 0],
        }
    }

    /// The texture object the view is of, if that texture has no host copy
    /// yet (`crate::resource::host_waits`): a slot written with such a
    /// view is left null until the texture is first used.
    #[cfg(windows)]
    fn waiting_texture(&self) -> Option<usize> {
        match *self {
            View::Shader(r, _) | View::Unordered(r, _, _)
                if crate::resource::host_waits(r as This) =>
            {
                Some(r)
            }
            _ => None,
        }
    }
}

/// Title slots (their handles) written with a view of a texture object
/// that had no host copy then, by texture. A title texture gets its host
/// copy on first use (`resource.rs` `Hosted`), and the title
/// writes each texture's views into its view heap when it creates the
/// texture: making those host slots would make every host copy at once.
/// They are made when the texture is: translated shaders index the heap
/// directly, so a slot is not always made again by the draw that reads it.
#[cfg(windows)]
static WAITING: std::sync::Mutex<std::collections::BTreeMap<usize, Vec<u64>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Textures given their host copy since [`make_waiting`] last ran.
#[cfg(windows)]
static MADE_LATE: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Title texture `resource` was given its host copy (called under that
/// texture's own lock: nothing else is locked here).
#[cfg(windows)]
pub(crate) fn texture_made(resource: usize) {
    MADE_LATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(resource);
}

/// The slot at `handle` was written with a view of texture `resource`,
/// which has no host copy: it waits for one.
#[cfg(windows)]
fn wait_for(resource: usize, handle: u64) {
    /// A texture with more slots waiting than this is in use for all we
    /// know (its views are being copied about): its host copy is made.
    const MOST: usize = 64;
    let many = {
        let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
        let slots = waiting.entry(resource).or_default();
        if !slots.contains(&handle) {
            slots.push(handle);
        }
        slots.len() > MOST
    };
    if many {
        // Makes it, and notes it made.
        let _ = crate::resource::host_copy(resource as This);
    } else if !crate::resource::host_waits(resource as This) {
        // Made by another thread since the slot was looked at: that making
        // may have been taken by `make_waiting` before this slot was noted.
        texture_made(resource);
    }
}

/// Makes the host slots that waited for a texture's host copy, for the
/// textures made since the last call. Called before a submission runs, by
/// the thread that submits: a slot is read when a list runs.
#[cfg(windows)]
pub(crate) fn make_waiting() {
    let began = std::time::Instant::now();
    make_waiting_now();
    ROUNDS[5].fetch_add(began.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(windows)]
fn make_waiting_now() {
    let made = std::mem::take(&mut *MADE_LATE.lock().unwrap_or_else(|e| e.into_inner()));
    for resource in made {
        let handles = WAITING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&resource);
        for handle in handles.into_iter().flatten() {
            // Only if the slot still holds a view of that texture: the
            // title may have written it since (and a view of another
            // texture without a host copy must go on waiting).
            // (Looked up first: the handle is read only while its heap is
            // there.)
            // (The heap stays while the lock is held: `mirror`.)
            let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
            let Some(&(_, _, at)) = heaps.iter().find(|&&(start, end, _)| (start..end).contains(&handle)) else {
                continue;
            };
            // SAFETY: a registered heap, live while the lock is held.
            let heap = unsafe { &*(at as *const Heap) };
            let Some(index) = heap.index(handle) else {
                continue;
            };
            let same = match view(handle) {
                Some(View::Shader(r, _) | View::Unordered(r, _, _)) => r == resource,
                _ => false,
            };
            if same && let Some(Some(host)) = heap.host.get() {
                host.mirror(index, handle, true);
            }
        }
    }
}

fn record(handle: u64, view: View) {
    if handle == 0 {
        return;
    }
    let [i, generation] = words_for(view);
    // SAFETY: the handle is a descriptor slot, at least 16 bytes.
    unsafe { (handle as *mut [u32; 3]).write_unaligned([VIEW_TAG, i, generation]) };
    #[cfg(windows)]
    mirror(handle);
}

/// The title released `resource`: the views naming it go stale.
pub(crate) fn forget(resource: usize) {
    #[cfg(windows)]
    WAITING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&resource);
    let mut views = views_write();
    let Some(entries) = views.by_resource.remove(&resource) else {
        return;
    };
    for i in entries {
        let (view, generation) = views.list[i as usize].clone();
        // Entries freed through a view's other resource may hold another
        // view by now.
        if views.index.get(&view) != Some(&i) || !view.resources().contains(&resource) {
            continue;
        }
        views.index.remove(&view);
        // (Below the bits that say a record is no table entry.)
        views.list[i as usize].1 = ((generation + 1) & GENERATIONS).max(1);
        // The view's other resource (a counter) keeps a stale entry, which
        // `forget` of that resource skips.
        views.free.push(i);
    }
}

/// The view the record words `i` and `generation` name: the one they hold
/// whole, or the view of entry `i` if `generation` is still its own.
fn view_at(i: u32, generation: u32) -> Option<View> {
    match generation >> KIND_SHIFT {
        0 => {
            let views = views_read();
            let (view, current) = views.list.get(i as usize)?;
            (*current == generation).then(|| view.clone())
        }
        CONSTANT => Some(constant_of(i, generation)),
        PLACED => views_read().placed_of(i, generation),
        _ => None,
    }
}

/// The words of the record at `handle`, if they are a view's.
pub(crate) fn record_at(handle: u64) -> Option<[u32; 3]> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let words = unsafe { (handle as *const [u32; 3]).read_unaligned() };
    (words[0] == VIEW_TAG).then_some(words)
}

/// The view recorded at `handle`, if one is (and its resource is alive).
pub(crate) fn view(handle: u64) -> Option<View> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
    if tag != VIEW_TAG {
        return None;
    }
    view_at(i, generation)
}

/// A description of `N` words, if the title passes one.
fn words<const N: usize>(desc: *const [u32; N]) -> Option<[u32; N]> {
    // SAFETY: the title passes a description or null.
    (!desc.is_null()).then(|| unsafe { desc.read_unaligned() })
}

/// Device method.
pub(crate) extern "system" fn create_cbv(_: This, desc: *const [u32; 4], handle: u64) {
    if let Some([lo, hi, size, _]) = words(desc) {
        let address = u64::from(lo) | u64::from(hi) << 32;
        record(handle, View::Constant(address, size));
    }
}

/// Device method.
pub(crate) extern "system" fn create_srv(
    _: This,
    resource: This,
    desc: *const [u32; 10],
    handle: u64,
) {
    record(handle, View::Shader(resource as usize, words(desc)));
}

/// Device method.
pub(crate) extern "system" fn create_uav(
    _: This,
    resource: This,
    counter: This,
    desc: *const [u32; 10],
    handle: u64,
) {
    let view = View::Unordered(resource as usize, counter as usize, words(desc));
    record(handle, view);
}

/// Device methods 0x230 and 0x238: shader resource and unordered access
/// views of memory the title placed itself, `(resource desc, view desc,
/// handle)`.
pub(crate) extern "system" fn create_placed_srv(
    _: This,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(resource), Some(desc)) = (words(resource), words(desc)) {
        record(handle, View::Placed(false, resource, desc));
    }
}

pub(crate) extern "system" fn create_placed_uav(
    _: This,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(resource), Some(desc)) = (words(resource), words(desc)) {
        record(handle, View::Placed(true, resource, desc));
    }
}

/// Device method.
pub(crate) extern "system" fn create_sampler(_: This, desc: *const [u32; 13], handle: u64) {
    if let Some(desc) = words(desc) {
        record(handle, View::Sampler(desc));
    }
}

/// Device method 0x1e8.
pub(crate) extern "system" fn create_sampler_x(_: This, desc: *const [u32; 27], handle: u64) {
    if let Some(desc) = words(desc) {
        record(handle, View::ConsoleSampler(desc));
    }
}

/// Device method: `count` descriptors of `kind` from `src` to `dst`.
pub(crate) extern "system" fn copy_simple(_: This, count: u32, dst: u64, src: u64, kind: u32) {
    let Some(step) = increment(kind).map(u64::from) else {
        return;
    };
    let len = u64::from(count) * step;
    if dst != 0 && src != 0 {
        // SAFETY: both ranges are descriptor slots the title owns.
        #[cfg(windows)]
        unsafe {
            crate::titlewrite::put(dst as *mut u8, src as *const u8, len as usize)
        };
        // SAFETY: as above.
        #[cfg(not(windows))]
        unsafe {
            std::ptr::copy(src as *const u8, dst as *mut u8, len as usize)
        };
        #[cfg(windows)]
        for i in 0..u64::from(count) {
            mirror(dst + i * step);
        }
    }
}

/// Device method: ranges of descriptors (start and count each) gathered
/// from the sources and scattered to the destinations, in order.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) extern "system" fn copy(
    _: This,
    dst_ranges: u32,
    dst_starts: *const u64,
    dst_sizes: *const u32,
    src_ranges: u32,
    src_starts: *const u64,
    src_sizes: *const u32,
    kind: u32,
) {
    let Some(step) = increment(kind) else {
        return;
    };
    // Each range as (start, count); a null size array means one descriptor
    // per range.
    let ranges = |n: u32, starts: *const u64, sizes: *const u32| -> Vec<(u64, u32)> {
        (0..n as usize)
            .map(|i| {
                // SAFETY: the title passes `n` starts and sizes (or null sizes).
                unsafe {
                    (
                        *starts.add(i),
                        if sizes.is_null() { 1 } else { *sizes.add(i) },
                    )
                }
            })
            .collect()
    };
    if dst_starts.is_null() || src_starts.is_null() {
        return;
    }
    let src = ranges(src_ranges, src_starts, src_sizes);
    let mut dst = ranges(dst_ranges, dst_starts, dst_sizes).into_iter();
    let (mut to, mut room) = dst.next().unwrap_or((0, 0));
    for (mut from, mut left) in src {
        while left > 0 {
            while room == 0 {
                let Some(next) = dst.next() else { return };
                (to, room) = next;
            }
            let n = left.min(room);
            copy_simple(std::ptr::null(), n, to, from, kind);
            let bytes = u64::from(n) * u64::from(step);
            (to, from, room, left) = (to + bytes, from + bytes, room - n, left - n);
        }
    }
}

/// The view recorded at `handle`, if one is.
pub(crate) fn rtv_record(handle: u64) -> Option<RtvRecord> {
    tagged_record(handle, RTV_TAG)
}

/// The depth view recorded at `handle`, if one is.
pub(crate) fn dsv_record(handle: u64) -> Option<RtvRecord> {
    tagged_record(handle, DSV_TAG)
}

fn tagged_record(handle: u64, tag: u32) -> Option<RtvRecord> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let record = unsafe { (handle as *const RtvRecord).read_unaligned() };
    (record.tag == tag).then_some(record)
}

/// Device method.
pub(crate) extern "system" fn create_heap(
    _: This,
    desc_ptr: *const HeapDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc_ptr.as_ref() }) else {
        return E_INVALIDARG;
    };
    let Some(size) = increment(desc.kind).map(|i| u64::from(i) * u64::from(desc.count.max(1)))
    else {
        return E_INVALIDARG;
    };
    // SAFETY: read only as a hint: a title that passes the plain
    // description has its own stack or data after it.
    let placement = unsafe { desc_ptr.add(1).cast::<Placement>().read_unaligned() };
    // shortcut: a placement is recognised by its size matching the count,
    // since the plain and extended descriptions share the slot and the
    // interface version; find the flag that tells them apart if a title
    // trips over this.
    let placed = placement.address != 0 && placement.size == size;
    tracing::debug!(?desc, placed, "descriptor heap");
    let (start, memory) = if placed {
        if placement.address.checked_add(size).is_none() {
            return E_INVALIDARG;
        }
        (placement.address, None)
    } else {
        match Memory::zeroed(size, 64) {
            Ok(memory) => (memory.address(), Some(memory)),
            Err(hr) => return hr,
        }
    };
    let visible = desc.flags & SHADER_VISIBLE != 0 && matches!(desc.kind, VIEWS_KIND | SAMPLERS);
    let heap = Box::new(Heap {
        object: Object::new(&FUNCTIONS),
        desc,
        start,
        memory,
        #[cfg(windows)]
        host: std::sync::OnceLock::new(),
        #[cfg(windows)]
        pending: Default::default(),
        #[cfg(windows)]
        written: (placed && visible)
            .then(|| crate::watch::track(start as usize, size as usize, true))
            .flatten(),
    });
    // Renderer port #2: the title's own stores tell this heap's slots.
    #[cfg(windows)]
    if placed && visible && desc.kind == VIEWS_KIND {
        crate::port_descriptors::adopt(start, desc.count);
    }
    #[cfg(windows)]
    if visible {
        let at = std::ptr::from_ref::<Heap>(&heap) as usize;
        let mut heaps = SHADER_HEAPS.write().unwrap_or_else(|e| e.into_inner());
        heaps.push((start, start + size, at));
    }
    #[cfg(not(windows))]
    let _ = visible;
    object::hand_out(DESCRIPTOR_HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &HEAP_IIDS).then_some(this);
    object::answer(DESCRIPTOR_HEAP, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}
extern "system" fn desc(this: This, out: *mut HeapDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(heap(this).desc) };
    }
}
/// The same address for the CPU and the GPU start.
extern "system" fn start(this: This) -> u64 {
    heap(this).start
}

/// IUnknown 3, object 4, device child 1, heap 3.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<DESCRIPTOR_HEAP, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = desc as *const (); // 0x40
    f[9] = start as *const (); // 0x48 CPU
    f[10] = start as *const (); // 0x50 GPU
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};

    /// What newer titles pass; tests use it throughout, so that reading the
    /// placement stays inside the value.
    #[repr(C)]
    struct Extended(HeapDesc, Placement);

    fn create(desc: HeapDesc, placement: Placement, out: &mut This) -> HRESULT {
        let extended = Extended(desc, placement);
        create_heap(std::ptr::null(), &extended.0, &HEAP_IIDS[0], out)
    }

    #[cfg(windows)]
    #[test]
    fn raw_heap_sync_stops_before_dynamic_host_slots() {
        let start = 0x1000usize;
        let step = 32;
        let capped = 5;
        assert_eq!(
            changed_slots(
                start + 3 * step + 4,
                start + 4 * step + 8,
                start,
                step as u64,
                capped
            ),
            3..5
        );
        assert_eq!(
            changed_slots(
                start + 6 * step,
                start + 9 * step,
                start,
                step as u64,
                capped
            ),
            5..5
        );
    }

    /// A table made afresh for a draw holds the descriptors its shader
    /// moves halves through (`HostShader::half_views`), also past the range
    /// starts its used-resource entries give: with one entry for a range
    /// from dword 0 and a 16-bit float buffer view at dword 8, the table
    /// takes two dynamic slots, so the next allocation does not land on the
    /// view's. (The case avt0m4t48 found in review on #927.)
    #[cfg(windows)]
    #[test]
    fn a_fresh_table_holds_the_half_views_past_its_range_starts() {
        use victory_shader::{ResourceType, RootType, UsedResource};
        let Some(device) = crate::host::host() else {
            return; // no host GPU
        };
        let list = device.new_list().expect("a host list");
        let desc = HeapDesc {
            kind: VIEWS_KIND,
            count: 2,
            flags: SHADER_VISIBLE,
            nodes: 1,
        };
        let none = Placement {
            address: 0,
            size: 0,
        };
        let mut h: This = std::ptr::null();
        assert_eq!(create(desc, none, &mut h), S_OK);
        let title = heap(h);
        let host_heap = title.host().expect("a host view heap");
        let (first, second) = (title.handle(0).unwrap(), title.handle(1).unwrap());
        // An R16G16B16A16_FLOAT buffer view of memory in the second slot.
        let memory = [0u64; 64];
        let address = memory.as_ptr() as u64;
        let resource = [1, 0, 0, 0, 512, 0, 1, 0x1_0001, 0, 1, 0, 1, 0, 0];
        let mut words = [0u32; 16];
        words[..10].copy_from_slice(&[10, 1, 5768, 0, 0, 0, 64, 0, 0, 0]);
        words[10] = address as u32;
        words[11] = (address >> 32) as u32;
        create_placed_srv(std::ptr::null(), &resource, &words, second);
        assert!(view(second).as_ref().and_then(host::half_view).is_some());
        let mut state = crate::draw::State::default();
        state.set_heaps(&[h]);
        state.set_table(true, 1, first);
        let range = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 8,
            dwords: 4,
            user_data: 4,
            slot: 1,
        };
        let made = state.tables(&list, &[range], &[], &[(1, 8)], true);
        let [(1, offset)] = made[..] else {
            panic!("one fresh table for root parameter 1, not {:?}", &made[..]);
        };
        assert_ne!(offset, u32::MAX, "the table found no dynamic slots");
        let next = host_heap.dynamic(1).expect("a dynamic slot");
        host_heap.release(next, 1);
        drop(state);
        slot::<extern "system" fn(This) -> u32>(h, 0x10)(h);
        assert_eq!(
            next,
            offset / 32 + 2,
            "the next allocation took the table's second slot"
        );
    }

    /// A shader-visible view heap with a host heap (none without a host GPU).
    #[cfg(windows)]
    fn view_heap() -> Option<This> {
        crate::host::host()?;
        let desc = HeapDesc {
            kind: VIEWS_KIND,
            count: 4,
            flags: SHADER_VISIBLE,
            nodes: 1,
        };
        let none = Placement { address: 0, size: 0 };
        let mut h: This = std::ptr::null();
        assert_eq!(create(desc, none, &mut h), S_OK);
        heap(h).host()?;
        Some(h)
    }

    /// What the heap pass keeps of a heap (host copies among it) goes with
    /// the heap; and a heap made where a released one was is another heap
    /// to it, which starts with a whole pass.
    #[cfg(windows)]
    #[test]
    fn the_heap_pass_forgets_a_released_heap() {
        let Some(h) = view_heap() else {
            return; // no host GPU
        };
        let serial = crate::draw::heap_pass_kept(heap(h), 1).expect("a host heap");
        assert!(crate::draw::heap_ranges_kept(serial));
        slot::<extern "system" fn(This) -> u32>(h, 0x10)(h);
        assert!(!crate::draw::heap_ranges_kept(serial), "kept after the heap was released");
        let Some(again) = view_heap() else {
            return;
        };
        assert_ne!(heap(again).host().unwrap().serial(), serial, "another heap, another number");
        slot::<extern "system" fn(This) -> u32>(again, 0x10)(again);
    }

    /// Dynamic slots handed out by a released heap's host heap are not
    /// given back to a heap made at the same address: other lists hold
    /// them there.
    #[cfg(windows)]
    #[test]
    fn dynamic_slots_go_back_only_to_the_heap_that_gave_them() {
        let Some(h) = view_heap() else {
            return; // no host GPU
        };
        let at = h as usize;
        let host = heap(h).host().unwrap();
        // All of them, so whether any came back shows.
        let mut n = 1u32 << 20;
        let first = loop {
            if let Some(first) = host.dynamic(n) {
                break first;
            }
            n /= 2;
            assert!(n > 0, "no dynamic slots");
        };
        while host.dynamic(1).is_some() {}
        // The slots, as a list that took them from an earlier heap at this
        // address would give them back.
        release_dynamic(at, host.serial() + 1_000_000, first, n);
        assert_eq!(host.dynamic(1), None, "another heap's slots were given back to this one");
        release_dynamic(at, host.serial(), first, n);
        assert_eq!(host.dynamic(1), Some(first));
        slot::<extern "system" fn(This) -> u32>(h, 0x10)(h);
    }

    #[test]
    fn heaps_hold_count_descriptors_from_their_start() {
        let null = std::ptr::null();
        let desc = HeapDesc {
            kind: 0,
            count: 1000,
            flags: 1,
            nodes: 0,
        };
        let none = Placement {
            address: 0,
            size: 0,
        };
        let mut h = null;
        assert_eq!(create(desc, none, &mut h), S_OK);
        let cpu = slot::<extern "system" fn(This) -> u64>(h, 0x48)(h);
        assert_eq!(cpu % 64, 0);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), cpu);
        assert_eq!(increment_size(null, 0), 32);
        // SAFETY: the heap holds 1000 descriptors of 32 bytes.
        unsafe { (cpu as *mut u8).add(999 * 32 + 31).write(1) };
        assert_eq!(increment_size(null, 9), 0);
        let bad = HeapDesc { kind: 9, ..desc };
        assert_eq!(create(bad, none, &mut h), E_INVALIDARG);
    }

    /// A descriptor copied into a placed heap as bytes is mirrored before
    /// the next submission, even though no descriptor method saw the copy.
    #[cfg(windows)]
    #[test]
    fn records_copied_as_bytes_are_made_at_the_next_sync() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let memory = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                crate::watch::PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        };
        assert!(!memory.is_null());
        let desc = HeapDesc {
            kind: SAMPLERS,
            count: 64,
            flags: SHADER_VISIBLE,
            nodes: 1,
        };
        let placement = Placement {
            address: memory as u64,
            size: 64 * 16,
        };
        let mut h: This = std::ptr::null();
        assert_eq!(create(desc, placement, &mut h), S_OK);
        if let Some(host) = heap(h).host() {
            let mut elsewhere = [0u64; 2];
            let source = elsewhere.as_mut_ptr() as u64;
            let sampler = [21, 3, 3, 3, 0, 1, 0, 0, 0, 0, 0, 0x7f7f_ffff, 0];
            create_sampler(std::ptr::null(), &sampler, source);
            // SAFETY: the heap slot and source both hold 16 descriptor bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source as *const u8,
                    memory.cast::<u8>().add(5 * 16),
                    16,
                )
            };
            assert!(!host.made(5));
            sync_heaps();
            assert!(host.made(5));
        }
        slot::<extern "system" fn(This) -> u32>(h, 0x10)(h);
        // SAFETY: this allocation belongs to the test.
        unsafe { VirtualFree(memory, 0, MEM_RELEASE) };
    }

    #[test]
    fn views_survive_copies() {
        let null = std::ptr::null();
        let mut heap = vec![0u64; 4 * 8]; // four 32-byte slots
        let at = |i: usize| heap.as_ptr() as u64 + 32 * i as u64;
        let (s0, s1, s2, s3) = (at(0), at(1), at(2), at(3));
        create_cbv(null, &[0x1000, 1, 256, 0], s0);
        assert_eq!(view(s0), Some(View::Constant(0x1_0000_1000, 256)));
        create_srv(null, null, std::ptr::null(), s1);
        assert_eq!(view(s1), Some(View::Shader(0, None)));
        // The same view twice is one entry.
        create_srv(null, null, std::ptr::null(), s2);
        // SAFETY: both slots are in `heap`.
        assert_eq!(unsafe { *(s1 as *const [u32; 3]) }, unsafe {
            *(s2 as *const [u32; 3])
        });
        // Two ranges of one into one range of two.
        let (dst, dst_n, src, src_n) = ([s2], [2u32], [s0, s1], [1u32, 1]);
        copy(
            null,
            1,
            dst.as_ptr(),
            dst_n.as_ptr(),
            2,
            src.as_ptr(),
            src_n.as_ptr(),
            0,
        );
        assert_eq!(view(s2), view(s0));
        assert_eq!(view(s3), view(s1));
        assert_eq!(view(at(0) + 8), None);
        heap[0] = 0;
    }

    /// Constant buffer views are named whole in their records: any address
    /// a program can have, any size below 16 KiB, multiples of 256 bytes
    /// up to 4 MiB; what does not fit says so.
    #[test]
    fn constant_views_fit_their_records() {
        let top = (1u64 << CONSTANT_ADDRESS_BITS) - 1;
        for address in [0u64, 0x100, 0x2602_0000, 0xd_6b60_0000, 0x7ffd_a34f_0000, top] {
            for size in [0u32, 1, 255, 256, 4000, (1 << 14) - 1, 1 << 14, 65536, (1 << 22) - 256] {
                let [i, generation] = constant_words(address, size).expect("fits");
                assert_eq!(generation >> KIND_SHIFT, CONSTANT);
                assert_eq!(view_at(i, generation), Some(View::Constant(address, size)), "{address:#x} {size}");
            }
        }
        assert_eq!(constant_words(top + 1, 256), None, "an address past 47 bits");
        assert_eq!(constant_words(0x1000, (1 << 14) + 1), None, "16 KiB or more, no multiple of 256");
        assert_eq!(constant_words(0x1000, 1 << 22), None, "4 MiB");
        // Different views never share their words.
        assert_ne!(constant_words(0x1000, 256), constant_words(0x1000, 512));
        assert_ne!(constant_words(0x1000, 256), constant_words(0x1100, 256));
        assert_ne!(constant_words(0x1000, 1 << 14), constant_words(0x1000, 64));
    }

    /// The leak of run 65: a title that writes constant buffer views and
    /// views of placed memory at ever new addresses adds nothing to the
    /// table; one shape stands for all the placed views alike.
    #[test]
    fn views_at_new_addresses_add_no_entries() {
        if !inline() {
            return;
        }
        let null = std::ptr::null();
        let slots = [0u64; 4];
        let at = slots.as_ptr() as u64;
        // A resource description and a view no other test uses.
        let resource: [u32; 14] = std::array::from_fn(|k| 0x5ee0_0000 + k as u32);
        let mut desc: [u32; 16] = std::array::from_fn(|k| 0x7a70_0000 + k as u32);
        let mine = |v: &View| match v {
            View::Constant(address, _) => (0x6_0000_0000..0x6_1000_0000).contains(address),
            View::Placed(_, r, _) => *r == resource,
            _ => false,
        };
        let entries = || VIEWS.read().unwrap().list.iter().filter(|(v, _)| mine(v)).count();
        let shapes = || VIEWS.read().unwrap().shapes.iter().filter(|s| s.1 == resource).count();
        for n in 0..20_000u64 {
            let address = 0x6_0000_0000 + n * 0x100;
            create_cbv(null, &[address as u32, (address >> 32) as u32, 512, 0], at);
            assert_eq!(view(at), Some(View::Constant(address, 512)));
            let placed = 0xd_4940_0000 + n * 0x1_0000;
            (desc[10], desc[11]) = (placed as u32, (placed >> 32) as u32);
            create_placed_srv(null, &resource, &desc, at);
            assert_eq!(view(at), Some(View::Placed(false, resource, desc)));
        }
        assert_eq!(entries(), 0, "no table entries");
        assert_eq!(shapes(), 1, "one shape for 20,000 placed views");
        // The same shape, unordered: another shape.
        create_placed_uav(null, &resource, &desc, at);
        assert_eq!(view(at), Some(View::Placed(true, resource, desc)));
        assert_eq!(shapes(), 2);
        // Placed memory past 1 TiB (not title memory) is a table entry.
        (desc[10], desc[11]) = (0, 0x100);
        create_placed_srv(null, &resource, &desc, at);
        assert_eq!(view(at), Some(View::Placed(false, resource, desc)));
        assert_eq!(entries(), 1);
        // A record's words are the same for the same view (the host knows
        // a slot's view by them), and differ with the address.
        // SAFETY: both handles are slots of 32 bytes above.
        let words = |handle: u64| unsafe { *(handle as *const [u32; 3]) };
        let other = [0u64; 4];
        create_cbv(null, &[0x40, 6, 512, 0], at);
        create_cbv(null, &[0x40, 6, 512, 0], other.as_ptr() as u64);
        assert_eq!(words(at), words(other.as_ptr() as u64));
        create_cbv(null, &[0x140, 6, 512, 0], other.as_ptr() as u64);
        assert_ne!(words(at), words(other.as_ptr() as u64));
    }

    /// A released resource's views go stale; the same view of a new
    /// resource at the same address is a new record.
    #[test]
    fn views_die_with_their_resource() {
        let null = std::ptr::null();
        let slots = [0u64; 2 * 4];
        let (a, b) = (slots.as_ptr() as u64, slots.as_ptr() as u64 + 32);
        let resource = 0x7777_0000usize as This;
        create_srv(null, resource, std::ptr::null(), a);
        assert_eq!(view(a), Some(View::Shader(0x7777_0000, None)));
        forget(0x7777_0000);
        assert_eq!(view(a), None, "stale after release");
        create_srv(null, resource, std::ptr::null(), b);
        assert_eq!(view(b), Some(View::Shader(0x7777_0000, None)));
        // SAFETY: both slots are in `slots`.
        let (old, new) = unsafe { (*(a as *const [u32; 3]), *(b as *const [u32; 3])) };
        assert_ne!(old, new, "a new generation");
    }

    /// The title places its shader-visible heap in its own memory
    /// and checks that the heap starts there.
    #[test]
    fn placed_heaps_start_at_the_title_memory() {
        let mut memory = vec![0u8; 64 * 32];
        let address = memory.as_mut_ptr() as u64;
        let desc = HeapDesc {
            kind: 0,
            count: 64,
            flags: 1,
            nodes: 1,
        };
        let mut h = std::ptr::null();
        assert_eq!(
            create(
                desc,
                Placement {
                    address,
                    size: 64 * 32
                },
                &mut h
            ),
            S_OK
        );
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), address);
        // A size that does not match the count is not a placement.
        assert_eq!(create(desc, Placement { address, size: 7 }, &mut h), S_OK);
        assert_ne!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
    }
}

#[cfg(all(test, windows))]
mod heap_sync_tests {
    use super::{heap_sync_priority, round_done_by};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// Raised only with the switch, and to what the submitting thread has.
    #[test]
    fn the_records_thread_is_raised_only_with_the_pool_switch() {
        use windows_sys::Win32::System::Threading::THREAD_PRIORITY_ABOVE_NORMAL;
        assert_eq!(heap_sync_priority(true), Some(THREAD_PRIORITY_ABOVE_NORMAL));
        assert_eq!(heap_sync_priority(false), None);
    }

    /// The watch's answer is the one its 5 ms polls gave: done if the round
    /// ends before the time, not done if it does not; and it wakes when the
    /// round ends, not at its next poll.
    #[test]
    fn the_watch_waits_for_the_round_or_its_time() {
        let rounds = Arc::new((Mutex::new((3u64, 2u64)), Condvar::new()));
        // Done already.
        assert!(round_done_by(&rounds, 2, Instant::now()));
        // Not done in time.
        let began = Instant::now();
        assert!(!round_done_by(&rounds, 3, began + Duration::from_millis(30)));
        assert!(began.elapsed() >= Duration::from_millis(30));
        // Done meanwhile: told by the round's end.
        let other = rounds.clone();
        let ender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            other.0.lock().unwrap().1 = 3;
            other.1.notify_all();
        });
        let began = Instant::now();
        assert!(round_done_by(&rounds, 3, began + Duration::from_secs(5)));
        assert!(began.elapsed() < Duration::from_secs(4));
        ender.join().unwrap();
    }
}