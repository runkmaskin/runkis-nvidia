//! Who touches a place in the title's data: `VICTORY_DATA_BREAK=<rva>[:<bytes>],...` (hex RVAs of
//! the title image, 1, 2, 4 or 8 bytes, at most four) sets the processor's debug registers of
//! every thread to stop at each read or write there, and logs each instruction that did it once,
//! with how often, as `module+rva` of the instruction after it. `VICTORY_DATA_BREAK_FROM=<seconds>`
//! waits that long first (the title's menus touch the same places), `VICTORY_DATA_BREAK_FOR`
//! stops after that many seconds (10 by default). Diagnostics builds only; it finds where the
//! title's own data goes, e.g. a pad's stick on its way to the camera.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, CONTEXT_DEBUG_REGISTERS_AMD64, EXCEPTION_POINTERS,
    GetThreadContext, SetThreadContext,
};
use windows_sys::Win32::System::Threading::{
    OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_SET_CONTEXT, THREAD_SUSPEND_RESUME,
};

use crate::crash::locate;
use crate::stacks::other_threads;

/// Instructions seen and how often; open addressing on the address, never emptied.
const SLOTS: usize = 1024;
static AT: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static HITS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// The registers at each instruction's first stop, in [`REGISTERS`]' order, then the eight bytes
/// at the stack pointer (the return address, after a leaf function's read).
static FIRST: [[AtomicU64; 17]; SLOTS] = [const { [const { AtomicU64::new(0) }; 17] }; SLOTS];
const REGISTERS: [&str; 17] = [
    "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15", "[rsp]",
];
/// Whether the handler counts (it stays installed after the end).
static ON: AtomicUsize = AtomicUsize::new(0);
/// The debug register slots that stop at instructions (bits as in debug register 6).
static EXEC: AtomicUsize = AtomicUsize::new(0);

/// The places of a `VICTORY_DATA_BREAK` list: offset and length, at most four, each aligned to its
/// length as the processor wants. `x<rva>` is an instruction to stop at (length 0) instead.
fn places(list: &str) -> Vec<(usize, usize)> {
    list.split(',')
        .filter_map(|p| {
            let p = p.trim();
            if let Some(code) = p.strip_prefix('x') {
                return Some((usize::from_str_radix(code.trim_start_matches("0x"), 16).ok()?, 0));
            }
            let (rva, len) = p.split_once(':').unwrap_or((p, "4"));
            let rva = usize::from_str_radix(rva.trim().trim_start_matches("0x"), 16).ok()?;
            let len: usize = len.trim().parse().ok()?;
            (matches!(len, 1 | 2 | 4 | 8) && rva % len == 0).then_some((rva, len))
        })
        .take(4)
        .collect()
}

/// Debug register 7 for `places`: each enabled locally, stopping on reads and writes, or on
/// running an instruction (length 0).
fn control(places: &[(usize, usize)]) -> u64 {
    places.iter().enumerate().fold(0, |dr7, (k, &(_, len))| {
        let bits = match len {
            0 => 0,
            1 => 3,
            2 => 3 | 1 << 2,
            8 => 3 | 2 << 2,
            _ => 3 | 3 << 2,
        };
        dr7 | 1 << (2 * k) | bits << (16 + 4 * k)
    })
}

/// `VICTORY_DATA_BREAK_SPLIT=<register>`: stops at one instruction count apart by that register's
/// low 16 bits too (which value of a loop's, say), with the registers of each.
fn split() -> Option<usize> {
    static AT: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *AT.get_or_init(|| {
        let name = std::env::var("VICTORY_DATA_BREAK_SPLIT").ok()?;
        REGISTERS.iter().position(|r| r.eq_ignore_ascii_case(name.trim()))
    })
}

unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    const SINGLE_STEP: i32 = 0x8000_0004_u32 as i32;
    const CONTINUE_EXECUTION: i32 = -1;
    const CONTINUE_SEARCH: i32 = 0;
    // SAFETY: the system passes a valid record and context.
    let (code, context) = unsafe { ((*(*info).ExceptionRecord).ExceptionCode, &mut *(*info).ContextRecord) };
    if code != SINGLE_STEP || context.Dr6 & 0xf == 0 {
        return CONTINUE_SEARCH;
    }
    let fired = context.Dr6 as usize & 0xf;
    context.Dr6 = 0;
    // An instruction stop comes before the instruction: the resume flag lets it run once.
    context.EFlags |= 0x1_0000;
    if ON.load(Relaxed) == 0 {
        context.Dr7 = 0;
        return CONTINUE_EXECUTION;
    }
    let c = &*context;
    let all = [
        c.Rax, c.Rbx, c.Rcx, c.Rdx, c.Rsi, c.Rdi, c.Rbp, c.Rsp, c.R8, c.R9, c.R10, c.R11, c.R12, c.R13, c.R14, c.R15,
    ];

    let pc = c.Rip as usize;
    // Only instruction stops are told apart: a data stop's registers are anything.
    let exec = fired & EXEC.load(Relaxed) != 0;
    let tag = split().filter(|_| exec).and_then(|r| all.get(r)).map_or(0, |&v| (v as u16 as usize) << 48);
    let key = pc ^ tag;
    let mut slot = (key >> 2).wrapping_mul(0x9e37_79b9) % SLOTS;
    for _ in 0..SLOTS {
        match AT[slot].compare_exchange(0, key, Relaxed, Relaxed) {
            Ok(_) => {
                let top = c.Rsp as *const u64;
                // SAFETY: the stopped thread's own stack pointer, which points at its stack.
                let top = unsafe { top.read_unaligned() };
                let values = [
                    c.Rax, c.Rbx, c.Rcx, c.Rdx, c.Rsi, c.Rdi, c.Rbp, c.Rsp, c.R8, c.R9, c.R10, c.R11, c.R12, c.R13,
                    c.R14, c.R15, top,
                ];
                for (into, value) in FIRST[slot].iter().zip(values) {
                    into.store(value, Relaxed);
                }
                HITS[slot].fetch_add(1, Relaxed);
                break;
            }
            Err(have) if have == key => {
                HITS[slot].fetch_add(1, Relaxed);
                break;
            }
            Err(_) => slot = (slot + 1) % SLOTS,
        }
    }
    CONTINUE_EXECUTION
}

/// Sets every other thread's debug registers to `addresses` and `dr7` (0: off).
fn arm(addresses: &[usize], dr7: u64) {
    #[repr(C, align(16))]
    struct Aligned(CONTEXT);
    for id in other_threads() {
        // SAFETY: a thread of our own process.
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_SET_CONTEXT, 0, id) };
        if thread.is_null() {
            continue;
        }
        // SAFETY: suspended while its debug registers change, then resumed; zeroed is a valid CONTEXT.
        unsafe {
            if SuspendThread(thread) != u32::MAX {
                let mut context = Aligned(std::mem::zeroed());
                context.0.ContextFlags = CONTEXT_DEBUG_REGISTERS_AMD64;
                if GetThreadContext(thread, &mut context.0) != 0 && context.0.Dr7 != dr7 {
                    let at = |k: usize| addresses.get(k).copied().unwrap_or(0) as u64;
                    (context.0.Dr0, context.0.Dr1, context.0.Dr2, context.0.Dr3) = (at(0), at(1), at(2), at(3));
                    context.0.Dr7 = dr7;
                    SetThreadContext(thread, &context.0);
                }
                ResumeThread(thread);
            }
            CloseHandle(thread);
        }
    }
}

/// Reads `VICTORY_DATA_BREAK` and, if set, starts the thread that arms, logs and disarms.
pub fn data_breaks_on_schedule(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_DATA_BREAK") else {
        return;
    };
    let seconds = |name: &str, default: f64| {
        let v = std::env::var(name).ok().and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(default);
        Duration::try_from_secs_f64(v.max(0.0)).unwrap_or_default()
    };
    let (from, length) = (seconds("VICTORY_DATA_BREAK_FROM", 0.0), seconds("VICTORY_DATA_BREAK_FOR", 10.0));
    let places: Vec<_> = places(&list).into_iter().filter(|&(rva, len)| rva + len <= size).collect();
    if places.is_empty() {
        tracing::warn!(list, "VICTORY_DATA_BREAK: no place understood");
        return;
    }
    let addresses: Vec<usize> = places.iter().map(|&(rva, _)| base + rva).collect();
    let dr7 = control(&places);
    EXEC.store(places.iter().enumerate().filter(|(_, p)| p.1 == 0).fold(0, |m, (k, _)| m | 1 << k), Relaxed);
    std::thread::spawn(move || {
        std::thread::sleep(from);
        // SAFETY: a handler of ours that only looks at its own exceptions; first in line.
        unsafe { AddVectoredExceptionHandler(1, Some(handler)) };
        ON.store(1, Relaxed);
        eprintln!("VIctory: data breaks armed at {:x?}", places);
        let start = Instant::now();
        while start.elapsed() < length {
            // New threads get them too.
            arm(&addresses, dr7);
            std::thread::sleep(Duration::from_millis(500));
        }
        ON.store(0, Relaxed);
        arm(&addresses, 0);
        let mut seen: Vec<(u64, usize, usize)> = (0..SLOTS)
            .filter_map(|k| Some((HITS[k].load(Relaxed), AT[k].load(Relaxed), k)).filter(|&(_, pc, _)| pc != 0))
            .collect();
        seen.sort_unstable_by(|a, b| b.cmp(a));
        eprintln!("VIctory: data breaks: {} instructions touched {:x?} (image at {base:#x})", seen.len(), places);
        for (hits, pc, k) in seen {
            let registers: Vec<String> = REGISTERS
                .iter()
                .zip(&FIRST[k])
                .map(|(name, v)| format!("{name}={:#x}", v.load(Relaxed)))
                .collect();
            let back = locate(FIRST[k][16].load(Relaxed) as usize);
            let pc = pc & ((1 << 48) - 1);
            eprintln!("VIctory: data break after {} x{hits} (returns to {back}?) {}", locate(pc), registers.join(" "));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_are_aligned_and_at_most_four() {
        assert_eq!(places("40,0x48:8, 13:4,10:2,7:1,99:3"), [(0x40, 4), (0x48, 8), (0x10, 2), (7, 1)]);
        assert_eq!(places("1,2,3,4,8,c,10,14").len(), 4);
    }

    #[test]
    fn control_enables_reads_and_writes_of_each_length() {
        // Slot 0: four bytes (11), reads and writes (11); slot 1: eight bytes (10); slot 2: an
        // instruction (00, 00).
        assert_eq!(control(&[(0, 4), (8, 8), (16, 0)]), 0b10101 | 0xf << 16 | 0xb << 20);
        assert_eq!(places("x1234, 8:8"), [(0x1234, 0), (8, 8)]);
    }

    /// A thread's read of a watched variable is seen, counted, at the instruction after it.
    #[test]
    fn a_watched_read_is_counted() {
        static WATCHED: AtomicU64 = AtomicU64::new(7);
        // SAFETY: as in `data_breaks_on_schedule`.
        unsafe { AddVectoredExceptionHandler(1, Some(handler)) };
        ON.store(1, Relaxed);
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (done, finished) = std::sync::mpsc::channel::<u64>();
        let reader = std::thread::spawn(move || {
            wait.recv().unwrap();
            let mut sum = 0;
            for _ in 0..3 {
                sum += WATCHED.load(Relaxed);
            }
            done.send(sum).unwrap();
        });
        std::thread::sleep(Duration::from_millis(50));
        let at = &WATCHED as *const AtomicU64 as usize;
        arm(&[at], control(&[(at, 8)]));
        go.send(()).unwrap();
        assert_eq!(finished.recv().unwrap(), 21);
        reader.join().unwrap();
        ON.store(0, Relaxed);
        arm(&[at], 0);
        let total: u64 = (0..SLOTS).filter(|&k| AT[k].load(Relaxed) != 0).map(|k| HITS[k].load(Relaxed)).sum();
        assert!(total >= 3, "{total}");
    }
}