//! The title's fence wait sleeps (`VICTORY_FENCE_SLEEP=1`; off unless asked for).
//!
//! At the end of each frame the 1.0.2.1 executable waits for the frame before it to finish on the
//! GPU: it reads the fence's value at the fence's address and loops until it is reached, with
//! `monitorx`/`mwaitx` and `Sleep(0)` at every turn. The wait is long (the GPU is the layer, and
//! the layer is the bottleneck), so the loop keeps one processor busy: in a CPU profile at 768
//! lines (run P1) the waiting thread was 88 % of a core, 42 % of it in `Sleep(0)` and the kernel.
//! `Sleep(0)` yields only to threads of the same priority or higher, which the layer's are not.
//!
//! Here the loop's `Sleep(0)` call becomes a call to [`wait`], which blocks on the value's address
//! (`WaitOnAddress`; the layer's fences wake it when they move, see `victory-gpu`'s `fence::set`)
//! until the title's own condition holds. The title's code tests that condition again when [`wait`]
//! returns, exactly as after `Sleep(0)`: the loop leaves at the same value as before, only later
//! than the write by a thread wake-up instead of a spin. [`wait`] also returns after [`MOST`], so
//! a value moved by something that does not wake it is seen as late as that.
//!
//! One place of the executable changes, only if it and the loop around it hold the expected bytes:
//! the `call` of the `Sleep(0)` helper, which becomes a call to a stub of ours. The stub calls
//! [`wait`] only when the loop is the plain form (no callback object, the title's default compare);
//! otherwise it goes on to `Sleep(0)` as before. Only those few bytes of the title are kept here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The environment variable that turns the patch on.
pub(crate) const FENCE_SLEEP_ENV: &str = "VICTORY_FENCE_SLEEP";

/// The `call` of the `Sleep(0)` helper in the loop (RVA), and the bytes from `CHECK_FROM` up to
/// and with it: the value's load from `[rdi]`, the "no callback object" and "default compare"
/// tests (`r14`, `r12` against `rbp`), the inline compare of the value minus the target (`rsi`)
/// against bit 55, and the call.
const CALL: usize = 0x4fb0c22;
const CHECK_FROM: usize = 0x4fb0c03;
const EXPECT: [u8; 36] = [
    0x48, 0x8b, 0x17, // mov rdx, [rdi]
    0x48, 0x83, 0xfa, 0xff, // cmp rdx, -1
    0x74, 0x58, // je (the device-removed path)
    0x4d, 0x85, 0xf6, // test r14, r14
    0x75, 0x69, // jne (callback object)
    0x49, 0x39, 0xec, // cmp r12, rbp
    0x75, 0x76, // jne (another compare)
    0x48, 0x29, 0xf2, // sub rdx, rsi
    0x4c, 0x85, 0xea, // test rdx, r13 (bit 55)
    0x0f, 0x84, 0x19, 0xff, 0xff, 0xff, // je (reached: leave)
    0xe8, 0x69, 0x02, 0x00, 0x00, // call the Sleep(0) helper
];
/// Where that call goes: `xor ecx, ecx; jmp [Sleep]`.
const SLEEP0: usize = 0x4fb0e90;

/// The longest one [`wait`] blocks before it lets the title look again.
const MOST: Duration = Duration::from_millis(50);
/// One `WaitOnAddress` at most this long (ms): the layer wakes it sooner.
const SLICE_MS: u32 = 4;

/// Waits and the microseconds spent in them, for the log.
static WAITS: AtomicU64 = AtomicU64::new(0);
static WAITED_US: AtomicU64 = AtomicU64::new(0);

/// The title's condition: the fence's value has reached `target` (56-bit values compared by the
/// sign of their difference, bit 55), or it is all ones (the device is gone; the title's loop
/// takes its own path for that).
pub(crate) fn reached(value: u64, target: u64) -> bool {
    value == u64::MAX || value.wrapping_sub(target) & (1 << 55) == 0
}

/// Blocks until the 64-bit value at `value` meets [`reached`] for `target`, or [`MOST`] passed.
/// Called by the title's code in place of `Sleep(0)`.
extern "system" fn wait(value: *const u64, target: u64) {
    // SAFETY: the title's fence address, an aligned u64 that the layer stores atomically.
    let at = unsafe { AtomicU64::from_ptr(value.cast_mut()) };
    let start = Instant::now();
    loop {
        let now = at.load(Ordering::Acquire);
        if reached(now, target) || start.elapsed() >= MOST {
            break;
        }
        #[cfg(windows)]
        // SAFETY: `value` is valid for 8 bytes; `now` is the value it must differ from.
        unsafe {
            windows_sys::Win32::System::Threading::WaitOnAddress(value.cast(), (&raw const now).cast(), 8, SLICE_MS)
        };
    }
    let us = start.elapsed().as_micros() as u64;
    let n = WAITS.fetch_add(1, Ordering::Relaxed) + 1;
    let total = WAITED_US.fetch_add(us, Ordering::Relaxed) + us;
    if n.is_power_of_two() && n >= 64 {
        tracing::info!(waits = n, ms = total / 1000, "{FENCE_SLEEP_ENV}: the title's frame fence waits slept");
    }
}

/// The stub at `stub`, called in place of the `Sleep(0)` helper with the title's registers: calls
/// `wait(rdi, rsi)` when `r14` is 0 and `r12` is `rbp` (the loop's plain form), else jumps to
/// `sleep0`. Either returns to the title's code.
fn stub(wait: usize, sleep0: usize) -> Vec<u8> {
    let mut code = vec![
        0x4d, 0x85, 0xf6, // test r14, r14
        0x75, 0x17, // jne sleep
        0x49, 0x39, 0xec, // cmp r12, rbp
        0x75, 0x12, // jne sleep
        0x48, 0x89, 0xf9, // mov rcx, rdi
        0x48, 0x89, 0xf2, // mov rdx, rsi
        0x48, 0xb8, // mov rax, wait
    ];
    code.extend((wait as u64).to_le_bytes());
    code.extend([0xff, 0xe0]); // jmp rax
    debug_assert_eq!(code.len(), 28);
    code.extend([0x48, 0xb8]); // sleep: mov rax, sleep0
    code.extend((sleep0 as u64).to_le_bytes());
    code.extend([0xff, 0xe0]); // jmp rax
    code
}

/// `call rel32` at `at` to `to`, if it reaches.
fn call(at: usize, to: usize) -> Option<[u8; 5]> {
    let rel = i32::try_from(to as i64 - (at as i64 + 5)).ok()?;
    let [a, b, c, d] = rel.to_le_bytes();
    Some([0xe8, a, b, c, d])
}

/// Whether `code` (the bytes at [`CHECK_FROM`]) is the expected loop.
fn verify(code: &[u8]) -> bool {
    code.len() >= EXPECT.len() && code[..EXPECT.len()] == EXPECT
}

/// Changes the call in the image at `base`, the stub placed at `place`: whether it did.
///
/// # Safety
/// The image is mapped, no thread runs the loop, and `place` is our fresh code memory within
/// reach of a `call rel32` from the site.
#[cfg(windows)]
unsafe fn patch(base: usize, place: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: inside the mapped image, by the caller.
    let code = unsafe { std::slice::from_raw_parts((base + CHECK_FROM) as *const u8, EXPECT.len()) };
    if !verify(code) {
        tracing::warn!("{FENCE_SLEEP_ENV}: the title's fence wait is not the expected build's; nothing changed");
        return false;
    }
    let at = base + CALL;
    let body = stub(wait as *const () as usize, base + SLEEP0);
    let Some(jump) = call(at, place) else {
        return false;
    };
    let mut old = 0;
    // SAFETY: our own code memory, then the five bytes of the call, made writable and put back.
    unsafe {
        std::ptr::copy_nonoverlapping(body.as_ptr(), place as *mut u8, body.len());
        FlushInstructionCache(GetCurrentProcess(), place as *const _, body.len());
        if VirtualProtect(at as *const _, jump.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        std::ptr::copy_nonoverlapping(jump.as_ptr(), at as *mut u8, jump.len());
        VirtualProtect(at as *const _, jump.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as *const _, jump.len());
    }
    true
}

/// Applies the patch when `VICTORY_FENCE_SLEEP=1` asks for it, to the 1.0.2.1 executable only.
///
/// # Safety
/// As [`patch`], with `hash` the original image's.
#[cfg(windows)]
pub(crate) unsafe fn apply_asked(base: usize, hash: &blake3::Hash) {
    if std::env::var_os(FENCE_SLEEP_ENV).is_none_or(|v| v != "1") {
        return;
    }
    if !hash.to_hex().starts_with(crate::hooks::BUILD_1_0_2_1) {
        tracing::debug!("{FENCE_SLEEP_ENV}: not the 1.0.2.1 executable; nothing patched");
        return;
    }
    let Some(place) = crate::console_insn::near(base + CALL) else {
        tracing::warn!("{FENCE_SLEEP_ENV}: no code memory near the title; nothing changed");
        return;
    };
    // SAFETY: by the caller; `place` is fresh and near.
    let done = unsafe { patch(base, place) };
    tracing::info!(done, "the title's frame fence wait sleeps on the fence ({FENCE_SLEEP_ENV})");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The title's inline test, as its instructions do it: `sub rdx, rsi; test rdx, r13` with
    /// r13 = 1 << 55, leaving on zero; and the `cmp rdx, -1` before it.
    fn title_leaves(value: u64, target: u64) -> bool {
        value == u64::MAX || (value.wrapping_sub(target) & 0x0080_0000_0000_0000) == 0
    }

    #[test]
    fn the_condition_is_the_titles() {
        // The title masks targets to 56 bits (bzhi 56) before the loop.
        let mask = (1u64 << 56) - 1;
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let target = x & mask;
            let value = x.rotate_left(17) & mask;
            for v in [value, target, target.wrapping_add(1), target.wrapping_sub(1), u64::MAX] {
                assert_eq!(reached(v, target), title_leaves(v, target), "{v:#x} {target:#x}");
            }
        }
        assert!(reached(5, 5));
        assert!(reached(6, 5));
        assert!(!reached(4, 5));
        assert!(reached(u64::MAX, 5));
    }

    #[test]
    fn the_site_is_checked_before_writing() {
        assert!(verify(&EXPECT));
        // The call is the last five bytes, at its RVA.
        assert_eq!(CHECK_FROM + EXPECT.len() - 5, CALL);
        // It goes to the Sleep(0) helper: rel32 from the next instruction.
        let rel = i32::from_le_bytes(EXPECT[32..36].try_into().unwrap());
        assert_eq!((CALL as i64 + 5 + i64::from(rel)) as usize, SLEEP0);
        for k in 0..EXPECT.len() {
            let mut other = EXPECT;
            other[k] ^= 0x40;
            assert!(!verify(&other), "byte {k}");
        }
        assert!(!verify(&EXPECT[..20]));
    }

    #[test]
    fn the_stub_takes_the_plain_loop_only() {
        let s = stub(0x1122_3344_5566_7788, 0x7ff6_0000_1234);
        assert_eq!(s.len(), 40);
        // Both branches land on the second mov.
        assert_eq!(5 + usize::from(s[4]), 28);
        assert_eq!(10 + usize::from(s[9]), 28);
        assert_eq!(&s[18..26], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&s[30..38], &0x7ff6_0000_1234u64.to_le_bytes());
        assert_eq!(call(0x1000, 0x2000), Some([0xe8, 0xfb, 0x0f, 0, 0]));
        assert_eq!(call(0, 1 << 40), None);
    }

    /// The stub run for real: the plain loop's registers reach [`wait`], which returns once
    /// another thread moves the value to the target, woken by `WakeByAddressAll`.
    #[cfg(windows)]
    #[test]
    fn the_stub_waits_until_the_value_is_reached() {
        use std::sync::atomic::AtomicBool;
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree};
        static VALUE: AtomicU64 = AtomicU64::new(3);
        static SLEPT: AtomicBool = AtomicBool::new(false);
        extern "system" fn sleep0() {
            SLEPT.store(true, Ordering::Relaxed);
        }
        // A caller that sets the loop's registers as the title has them and calls the stub:
        // rdi = value address, rsi = target, r14 = callback, r12 and rbp = the compares.
        // push rbp/rdi/rsi/r12/r14 (5 pushes keep the stack aligned with the call),
        // mov rdi, rcx; mov rsi, rdx; mov r14, r8; mov r12, r9; mov rbp, [rsp+0x50] (5th arg),
        // sub rsp, 0x20; call [stub at r10 = rsp+0x78+... ] -- the stub's address goes in rax.
        let place = unsafe { VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) } as usize;
        assert_ne!(place, 0);
        let body = stub(wait as *const () as usize, sleep0 as *const () as usize);
        let caller_at = place + 256;
        let mut caller = vec![
            0x55, 0x57, 0x56, 0x41, 0x54, 0x41, 0x56, // push rbp, rdi, rsi, r12, r14
            0x48, 0x89, 0xcf, // mov rdi, rcx
            0x48, 0x89, 0xd6, // mov rsi, rdx
            0x4d, 0x89, 0xc6, // mov r14, r8
            0x4d, 0x89, 0xcc, // mov r12, r9
            0x48, 0x8b, 0x6c, 0x24, 0x50, // mov rbp, [rsp+0x50] (the 5th argument)
            0x48, 0x83, 0xec, 0x20, // sub rsp, 0x20
            0x48, 0xb8, // mov rax, stub
        ];
        caller.extend((place as u64).to_le_bytes());
        caller.extend([0xff, 0xd0]); // call rax
        caller.extend([0x48, 0x83, 0xc4, 0x20]); // add rsp, 0x20
        caller.extend([0x41, 0x5e, 0x41, 0x5c, 0x5e, 0x5f, 0x5d, 0xc3]); // pop r14, r12, rsi, rdi, rbp; ret
        // SAFETY: our fresh page.
        unsafe {
            std::ptr::copy_nonoverlapping(body.as_ptr(), place as *mut u8, body.len());
            std::ptr::copy_nonoverlapping(caller.as_ptr(), caller_at as *mut u8, caller.len());
        }
        type Run = extern "system" fn(*const AtomicU64, u64, usize, usize, usize);
        // SAFETY: the code just written, with that signature.
        let run: Run = unsafe { std::mem::transmute(caller_at) };
        // Another form of the loop (a callback object): Sleep(0) as before, no wait.
        run(&VALUE, 9, 1, 0x10, 0x10);
        assert!(SLEPT.swap(false, Ordering::Relaxed));
        run(&VALUE, 9, 0, 0x10, 0x20);
        assert!(SLEPT.swap(false, Ordering::Relaxed));
        // The plain loop: blocks until another thread moves the value to 9.
        let mover = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(20));
            VALUE.store(7, Ordering::Release);
            // SAFETY: wakes the waiters on that address.
            unsafe { windows_sys::Win32::System::Threading::WakeByAddressAll(VALUE.as_ptr().cast()) };
            std::thread::sleep(Duration::from_millis(5));
            VALUE.store(9, Ordering::Release);
            // SAFETY: as above.
            unsafe { windows_sys::Win32::System::Threading::WakeByAddressAll(VALUE.as_ptr().cast()) };
        });
        let start = Instant::now();
        run(&VALUE, 9, 0, 0x10, 0x10);
        let took = start.elapsed();
        assert!(!SLEPT.load(Ordering::Relaxed));
        assert_eq!(VALUE.load(Ordering::Acquire), 9);
        assert!(took >= Duration::from_millis(20) && took < MOST, "{took:?}");
        mover.join().unwrap();
        // Already reached: back at once.
        run(&VALUE, 8, 0, 0x10, 0x10);
        // SAFETY: ours.
        unsafe { VirtualFree(place as *mut _, 0, MEM_RELEASE) };
    }
}
