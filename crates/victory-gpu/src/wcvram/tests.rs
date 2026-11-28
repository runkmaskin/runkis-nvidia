use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::System::Memory as mem;

use super::*;

const MIB: usize = 1 << 20;
/// The size the tests steer: allocations of other tests running meanwhile
/// are all smaller.
const STEERED: usize = 1 << 30;

/// What `xmem` asks for the title's graphics pool: from 4 GiB to 2^40,
/// 2 MiB pages.
const LOWEST: usize = 1 << 32;
const HIGHEST: usize = (1 << 40) - 1;
const ALIGN: usize = 2 * MIB;

fn device() -> Option<windows::Win32::Graphics::Direct3D12::ID3D12Device> {
    use windows::Win32::Graphics::Direct3D12::*;
    let (device, _) = crate::zerocopy::tests::device()?;
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
    // SAFETY: writes `o` of the given size.
    let ok = unsafe { device.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS16, (&raw mut o).cast(), size_of_val(&o) as u32).is_ok() };
    if !(ok && o.GPUUploadHeapSupported.as_bool()) {
        eprintln!("no card memory the CPU maps (resizable BAR): skipped");
        return None;
    }
    Some(device)
}

/// Fixed-seed bytes.
fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// With every other stretch that could hold it reserved, an allocation the
/// system places can only go to the target: the steering, shown with a
/// plain reservation at no address of ours (no card needed).
#[test]
fn a_steered_allocation_lands_at_the_target() {
    // As big as the steered tests make: while it is steered, allocations of
    // this size or more in this process fail (other tests run beside it).
    let size = STEERED;
    // Another test's thread may take the place between the choice and the
    // allocation, as `card_memory_at` allows for: three tries.
    let mut landed = None;
    for _ in 0..3 {
        let (at, target, all) = steered(LOWEST, HIGHEST, ALIGN, size, || {
            // SAFETY: a reservation of our own, released below.
            unsafe { mem::VirtualAlloc(std::ptr::null(), size, mem::MEM_RESERVE, mem::PAGE_NOACCESS) as usize }
        })
        .expect("a place");
        assert!((LOWEST..=HIGHEST - size).contains(&target) && target.is_multiple_of(ALIGN), "{target:#x}");
        assert!(all, "every other stretch reserved");
        if at == target {
            landed = Some((at, target));
            break;
        }
        // SAFETY: our reservation.
        unsafe { mem::VirtualFree(at as *mut c_void, 0, mem::MEM_RELEASE) };
    }
    let (at, target) = landed.expect("the system placed it where we wanted, in one of three tries");
    // Everything we held for it is free again: the same size goes elsewhere
    // (above, where the system looks first) and smaller ones as before.
    // SAFETY: our reservations.
    unsafe {
        let other = mem::VirtualAlloc(std::ptr::null(), size, mem::MEM_RESERVE, mem::PAGE_NOACCESS);
        assert!(!other.is_null());
        mem::VirtualFree(other, 0, mem::MEM_RELEASE);
        mem::VirtualFree(at as *mut c_void, 0, mem::MEM_RELEASE);
    }
    assert!(!touches(target, size), "no pool here: nothing is placed");
}

/// Card memory the CPU maps, at an address with the title's requirements:
/// zeroed, the CPU's writes read by the card through a buffer placed at an
/// offset as a title buffer's copy is, and the card's writes read by the CPU
/// at the title's address, with no copy in between. Then the order across
/// threads ([nother_threads_writes_are_seen_by_the_card_after_the_flush]),
/// in the same memory: steering is slow, done once.
#[test]
fn card_memory_at_a_title_address_is_the_titles_memory_both_ways() {
    let Some(device) = device() else { return };
    let size = STEERED;
    let (heap, _whole, at) = card_memory_at(&device, LOWEST, HIGHEST, ALIGN, size).expect("card memory at the title's address");
    assert!((LOWEST..=HIGHEST - size).contains(&at) && at.is_multiple_of(ALIGN), "{at:#x}");
    let title = at as *mut u8;
    // Zeroed, as committed memory is (a few pages: reads are slow).
    for page in [0, 17, 1023] {
        // SAFETY: inside the mapping.
        let bytes = unsafe { std::slice::from_raw_parts(title.add(page * K64), 256) };
        assert!(bytes.iter().all(|&b| b == 0), "page {page} not zeroed");
    }
    let mut rig = crate::inplace::Rig::new(&device).expect("a queue");
    // A title buffer as `shadow::lookup` would place it: not 64 KiB aligned.
    let (start, len) = (at + 3 * K64 + 0x1230, 5 * K64 + 0x99);
    let base = start & !(K64 - 1);
    let end = (start + len).next_multiple_of(K64);
    let buffer = place(&device, &heap, (base - at) as u64, (end - base) as u64).expect("a placed buffer");
    let bias = (start - base) as u64;
    for seed in [1u64, 2, 3] {
        let bytes = pattern(len, seed * 0x9e37_79b9);
        // The title's CPU writes (plain stores into write-combined memory).
        // SAFETY: inside the mapping.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), title.add(start - at), len) };
        let seen = rig.fetch(&buffer, bias, len).expect("the card reads");
        assert!(seen == bytes, "seed {seed}: the card saw other bytes");
        // The card writes; the CPU reads at the title's address.
        let new = pattern(len, seed * 77 + 5);
        rig.store(&buffer, bias, &new).expect("the card writes");
        let mut back = vec![0u8; len];
        // SAFETY: inside the mapping; the card is done (the rig waited).
        unsafe { std::ptr::copy_nonoverlapping(title.add(start - at), back.as_mut_ptr(), len) };
        assert!(back == new, "seed {seed}: the CPU read other bytes than the card wrote");
    }
    // The guards of `crate::watch` (read-only pages) are taken by this
    // memory, and the kernel's write watch is not: writes there are found by
    // the guards, if anything asks.
    let mut old = 0;
    // SAFETY: a page inside the mapping, given its protection back.
    let guarded = unsafe { mem::VirtualProtect(title.add(8 * K64).cast(), K64, mem::PAGE_READONLY, &mut old) } != 0;
    if guarded {
        // SAFETY: as above.
        unsafe { mem::VirtualProtect(title.add(8 * K64).cast(), K64, old, &mut old) };
    }
    let (mut found, mut n, mut granularity) = ([0usize; 4], 4usize, 0u32);
    // SAFETY: a plain query into our arrays.
    let watched = unsafe { mem::GetWriteWatch(0, title.cast(), K64, found.as_mut_ptr().cast(), &mut n, &mut granularity) } == 0;
    eprintln!("card memory at {at:#x}: read-only guard taken {guarded}, write watch {watched}");
    assert!(!watched, "card memory is not write-watched");
    let whole = place(&device, &heap, 0, size as u64).expect("a buffer over it all");
    another_threads_writes_are_seen_by_the_card_after_the_flush(&mut rig, &whole, at);
}

/// The title's other thread writes, tells the submitting thread with a plain
/// store, and the submitting thread empties every write-combining buffer
/// ([`title_submits`]'s call) before the card reads: the card sees every byte,
/// 64 times over. (Hardware cannot show the failure on demand; the model test
/// below does.)
fn another_threads_writes_are_seen_by_the_card_after_the_flush(rig: &mut crate::inplace::Rig, buffer: &windows::Win32::Graphics::Direct3D12::ID3D12Resource, at: usize) {
    let len = 64 << 10;
    let told = std::sync::Arc::new(AtomicU64::new(0));
    for round in 1..=64u64 {
        let bytes = pattern(len, round);
        let expect = bytes.clone();
        let flag = told.clone();
        let writer = std::thread::spawn(move || {
            // SAFETY: inside the mapping; only this thread writes it now.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (at + (round as usize % 32) * len) as *mut u8, len) };
            // A plain store (release is a plain `mov` on x86).
            flag.store(round, Ordering::Release);
            // Kept running, so its buffers are not emptied by a switch.
            let until = std::time::Instant::now() + std::time::Duration::from_millis(5);
            while std::time::Instant::now() < until {
                std::hint::spin_loop();
            }
        });
        while told.load(Ordering::Acquire) != round {
            std::hint::spin_loop();
        }
        // SAFETY: no arguments.
        unsafe { windows_sys::Win32::System::Threading::FlushProcessWriteBuffers() };
        let seen = rig.fetch(buffer, ((round as usize % 32) * len) as u64, len).expect("the card reads");
        assert!(seen == expect, "round {round}: the card saw other bytes");
        writer.join().expect("the writer");
    }
}

/// A model of write-combining buffers and the order the module notes argue
/// for. Each thread's stores to write-combined memory wait in its buffer and
/// may reach memory at any moment, in any order, or not until something
/// empties the buffer: a fence or locked instruction of that thread, or an
/// interrupt (a switch, or `FlushProcessWriteBuffers` for every thread). The
/// card reads memory. Every interleaving of the writer's and the submitter's
/// steps and every choice of what leaves a buffer early is tried.
mod model {
    /// How the writer tells the submitting thread its data is written.
    #[derive(Clone, Copy, PartialEq, Debug)]
    pub(super) enum Tell {
        /// A plain store (a release store on x86).
        Plain,
        /// A locked instruction (a lock, an atomic read-modify-write).
        Locked,
    }

    /// Writer: `n` stores to write-combined memory, then the telling.
    /// Submitter: waits to be told, optionally flushes every buffer, then the
    /// card reads. Returns whether some run lets the card miss a store.
    pub(super) fn card_can_miss(n: usize, tell: Tell, flush: bool) -> bool {
        // State: the writer's program counter, its buffer (stores not in
        // memory), memory (stores in it), whether it has told.
        fn go(n: usize, tell: Tell, flush: bool, pc: usize, buffer: u32, memory: u32, told: bool) -> bool {
            // A buffered store may reach memory at any moment.
            for i in 0..n {
                if buffer & 1 << i != 0 && go(n, tell, flush, pc, buffer & !(1 << i), memory | 1 << i, told) {
                    return true;
                }
            }
            if pc < n {
                // The next store goes into the buffer.
                return go(n, tell, flush, pc + 1, buffer | 1 << pc, memory, told);
            }
            if !told {
                // A locked instruction empties the writer's buffer.
                let (buffer, memory) = if tell == Tell::Locked { (0, memory | buffer) } else { (buffer, memory) };
                return go(n, tell, flush, pc, buffer, memory, true);
            }
            // Told: the submitter goes on, the card reads.
            let seen = if flush { memory | buffer } else { memory };
            seen != (1 << n) - 1
        }
        go(n, tell, flush, 0, 0, 0, false)
    }
}

#[test]
fn model_the_card_sees_every_store_once_the_buffers_are_flushed() {
    use model::{Tell, card_can_miss};
    for n in 1..=6 {
        // The race the flush is for: told with a plain store, nothing empties
        // the writer's buffer.
        assert!(card_can_miss(n, Tell::Plain, false), "n {n}: the model must show the race");
        // A locked telling (the title's locks, our channels) empties it.
        assert!(!card_can_miss(n, Tell::Locked, false), "n {n}: a locked telling");
        // `FlushProcessWriteBuffers` at the title's submission empties it
        // whatever the telling.
        assert!(!card_can_miss(n, Tell::Plain, true), "n {n}: with the flush");
        assert!(!card_can_miss(n, Tell::Locked, true), "n {n}: both");
    }
}

/// CPU and card costs of card memory the CPU maps at a title address:
/// writes as the title makes them, reads (the uncached reads the module notes
/// warn of), and the flush at each submission. `cargo test --release -p
/// victory-gpu --lib -- --ignored --nocapture bench_card_memory_the_title_writes`.
#[test]
#[ignore = "benchmark on the PC's card"]
fn bench_card_memory_the_title_writes() {
    let Some(device) = device() else { return };
    let size = 256 * MIB;
    let (_heap, _whole, at) = card_memory_at(&device, LOWEST, HIGHEST, ALIGN, size).expect("card memory");
    let card = at as *mut u8;
    // The title's memory today: private, write-back, write-watched.
    // SAFETY: an allocation of our own.
    let host = unsafe { mem::VirtualAlloc(std::ptr::null(), size, mem::MEM_RESERVE | mem::MEM_COMMIT | 0x20_0000, mem::PAGE_READWRITE) }.cast::<u8>();
    assert!(!host.is_null());
    let source = pattern(64 * MIB, 9);
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let time = |f: &mut dyn FnMut()| {
        let mut v = Vec::new();
        for _ in 0..9 {
            let t = std::time::Instant::now();
            f();
            v.push(t.elapsed().as_secs_f64());
        }
        median(v)
    };
    eprintln!("| what | write-back (today) | card memory the CPU maps |");
    eprintln!("|---|---|---|");
    for &len in &[64 << 10, MIB, 16 * MIB, 64 * MIB] {
        let from = source.as_ptr();
        // SAFETY: both ranges are ours.
        let ta = time(&mut || unsafe { std::ptr::copy_nonoverlapping(from, host, len) });
        // SAFETY: as above.
        let tb = time(&mut || unsafe { std::ptr::copy_nonoverlapping(from, card, len) });
        let gb = |t: f64| len as f64 / t / 1e9;
        eprintln!("| CPU writes {} KiB (memcpy) | {:.1} GB/s | {:.1} GB/s |", len >> 10, gb(ta), gb(tb));
    }
    // Small scattered stores, as a title fills constants and vertices: 8
    // bytes every 64, over 16 MiB.
    for (name, to) in [("write-back", host), ("card", card)] {
        let t = time(&mut || {
            for i in (0..16 * MIB).step_by(64) {
                // SAFETY: inside the allocation.
                unsafe { to.add(i).cast::<u64>().write_volatile(i as u64) };
            }
        });
        eprintln!("| CPU stores 8 bytes every 64 over 16 MiB, {name} | {:.2} ms |", t * 1e3);
    }
    // Reads: bandwidth of a copy out, and the latency of dependent loads.
    let mut sink = vec![0u8; 4 * MIB];
    for (name, from) in [("write-back", host), ("card", card)] {
        let t = time(&mut || {
            // SAFETY: inside the allocation.
            unsafe { std::ptr::copy_nonoverlapping(std::hint::black_box(from), sink.as_mut_ptr(), 4 * MIB) };
            std::hint::black_box(&mut sink);
        });
        eprintln!("| CPU reads 4 MiB (memcpy), {name} | {:.3} GB/s |", 4.0 * MIB as f64 / t / 1e9);
    }
    // A chain of 4,096 cache lines in a random order over 64 MiB.
    let lines = 4096usize;
    let mut order: Vec<usize> = (0..lines).collect();
    let mut x = 7u64;
    for i in (1..lines).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        order.swap(i, x as usize % (i + 1));
    }
    let stride = 64 * MIB / lines;
    for (name, base) in [("write-back", host), ("card", card)] {
        for k in 0..lines {
            let (here, next) = (order[k] * stride, order[(k + 1) % lines] * stride);
            // SAFETY: inside the allocation.
            unsafe { base.add(here).cast::<usize>().write(base as usize + next) };
        }
        let t = time(&mut || {
            let mut p = base as usize + order[0] * stride;
            for _ in 0..lines {
                // SAFETY: each link names a line inside the allocation.
                p = unsafe { (p as *const usize).read_volatile() };
            }
            std::hint::black_box(p);
        });
        eprintln!("| CPU dependent load (latency), {name} | {:.0} ns |", t / lines as f64 * 1e9);
    }
    let mut v = Vec::new();
    for _ in 0..1000 {
        let t = std::time::Instant::now();
        // SAFETY: no arguments.
        unsafe { windows_sys::Win32::System::Threading::FlushProcessWriteBuffers() };
        v.push(t.elapsed().as_secs_f64());
    }
    eprintln!("| FlushProcessWriteBuffers (each title submission) | {:.1} us median |", median(v) * 1e6);
    let started = std::time::Instant::now();
    let again = card_memory_at(&device, LOWEST, HIGHEST, ALIGN, 1277 * MIB);
    eprintln!("| steering 1,277 MiB to a title address | {:.2} s, {} |", started.elapsed().as_secs_f64(), if again.is_some() { "landed" } else { "FAILED" });
    // SAFETY: our allocation.
    unsafe { mem::VirtualFree(host.cast(), 0, mem::MEM_RELEASE) };
}

/// A copy counts as write-combined only inside one such region, and not after
/// the region is let go (part of it or the whole allocation); the floor of
/// `VICTORY_WC_NO_BACK_MIN_KB` keeps small copies' writes coming back. (Ported
/// with `VICTORY_WC_NO_BACK` from trustno1-coder/VIctory-NVIDIA 50c15a1.)
#[test]
fn write_combined_regions() {
    // Addresses of their own: other tests share the registry.
    let base = 0x7f00_0000_0000usize;
    title_memory_is_write_combined(base, 0x100_0000);
    title_memory_is_write_combined(base + 0x1000, 0x1000);
    assert!(is_write_combined(base as u64, 0x1000));
    assert!(is_write_combined(base as u64 + 0xff_f000, 0x1000));
    assert!(!is_write_combined(base as u64 + 0xff_f000, 0x2000), "past its end");
    assert!(!is_write_combined(base as u64 - 0x1000, 0x2000), "before its start");
    write_combined_freed(base + 0x10_0000, 0x1000);
    assert!(!is_write_combined(base as u64, 0x1000), "part of it decommitted");
    let other = base + 0x1000_0000;
    title_memory_is_write_combined(other, 0x10_0000);
    write_combined_freed(other, 0);
    assert!(!is_write_combined(other as u64, 0x1000), "the allocation released");
    // The floor: with the switch read as it is (unset in tests: 0, all).
    let floor = no_back_min();
    assert!(!left_on_card(false, 1 << 30, 64), "not write-combined: comes back");
    assert!(left_on_card(true, floor, 64), "at the floor: left");
    if floor > 0 {
        assert!(!left_on_card(true, floor - 1, 64), "under the floor: comes back");
    }
}