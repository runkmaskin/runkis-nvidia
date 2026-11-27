//! Native mouse aiming (`VICTORY_MOUSE_NATIVE=1` with `VICTORY_MOUSE=1`; off unless asked for).
//!
//! The title's input code still has the PC engine's keyboard-and-mouse device. Each frame it
//! reports the mouse's movement as two axis events (sources 0x2000 and 0x2001) next to the keys,
//! but on the console the movement is compiled to zero: the device multiplies 0 by a scale. Here
//! that zero becomes the movement our input layer saw since the last frame ([`add`]), times
//! `VICTORY_MOUSE_NATIVE_SCALE`, so the device posts mouse events as on a PC, past the gamepad's
//! dead zone, response curve and acceleration (those belong to the stick's sources).
//!
//! Measured in game (1.0.2.1): the events are posted (source 0x2000, once a frame while the mouse
//! moves), but the device has no listener, so the camera does not turn from them yet. The input
//! layer keeps feeding the stand-in stick too. What is missing is in docs/changes-2026-11-27.md.
//!
//! Two places of the 1.0.2.1 executable change, each only if its bytes are the expected ones:
//! - `vxorps xmm6, xmm6, xmm6; vmulss xmm0, xmm6, [scale x]` (the zero, then the X value) becomes
//!   a jump to code of ours that leaves the X value in xmm0 and keeps the Y value in xmm6;
//! - `vmulss xmm0, xmm6, [scale y]` (the Y value) becomes `vmovaps xmm0, xmm6`.
//!
//! Nothing of the title's code is kept here but those few instruction bytes, to check the build.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::Relaxed};

/// Mouse counts since the title last took them: right, down. The patch takes both with `xchg`.
static DELTA: [AtomicI32; 2] = [AtomicI32::new(0), AtomicI32::new(0)];
/// What one count is in the title's mouse axes (f32 bits): right, down.
static GAIN: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
/// Whether the patch that takes them is in the title's code.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Adds mouse movement (counts, right and down positive).
pub fn add(dx: i32, dy: i32) {
    DELTA[0].fetch_add(dx, Relaxed);
    DELTA[1].fetch_add(dy, Relaxed);
}

/// The movement since the last call, and none left (what the patch does, for tests).
pub fn take() -> (i32, i32) {
    (DELTA[0].swap(0, Relaxed), DELTA[1].swap(0, Relaxed))
}

/// Whether the title takes the movement: without the patch the mouse stays the stick.
pub fn active() -> bool {
    ACTIVE.load(Relaxed)
}

/// The default value of one count, a guess: nothing consumes the events yet to measure it against.
const DEFAULT_SCALE: f32 = 0.01;

/// `VICTORY_MOUSE_NATIVE_SCALE=<x>[/<y>]`: the value of one count, the second for up and down (the
/// first by default; a negative one turns that way round). A comma is a decimal point.
fn parse_scale(text: Option<&str>) -> [f32; 2] {
    let number = |s: &str| s.trim().replace(',', ".").parse::<f32>().ok().filter(|v| v.is_finite() && *v != 0.0);
    let (x, y) = match text.map(|t| t.split_once('/').unwrap_or((t, ""))) {
        Some((x, y)) => (number(x), number(y)),
        None => (None, None),
    };
    let x = x.unwrap_or(DEFAULT_SCALE);
    [x, y.unwrap_or(x)]
}
/// One place of the title's code: where, and the bytes it must hold.
#[derive(Clone, Copy)]
pub(crate) struct Site<'a> {
    pub rva: usize,
    pub expect: &'a [u8],
}

/// The 1.0.2.1 executable's two places (see the module's comment).
const ZERO_AND_X: Site<'static> = Site {
    rva: 0x54578ed,
    expect: &[0xc5, 0xc8, 0x57, 0xf6, 0xc5, 0xca, 0x59, 0x05, 0x97, 0x0f, 0x59, 0x05],
};
const Y: Site<'static> = Site { rva: 0x545791f, expect: &[0xc5, 0xca, 0x59, 0x05, 0x6d, 0x0f, 0x59, 0x05] };

/// Whether `code` holds what `site` expects: anything else is another build, never changed.
fn verify(code: &[u8], site: &Site<'_>) -> bool {
    code.len() >= site.expect.len() && &code[..site.expect.len()] == site.expect
}

/// The code at `stub` for the first place (at `at`, `len` bytes): takes the movement at `delta`,
/// leaves `x * gain[0]` in xmm0 and `y * gain[1]` in xmm6, and jumps back behind the place. It uses
/// rax, rcx and rdx, which the title's code sets anew right after the place.
fn stub(at: usize, len: usize, stub: usize, delta: usize, gain: usize) -> Option<Vec<u8>> {
    let mut code = vec![0x48, 0xb8]; // mov rax, delta
    code.extend((delta as u64).to_le_bytes());
    code.extend([0x31, 0xc9, 0x87, 0x08]); // xor ecx, ecx ; xchg [rax], ecx
    code.extend([0x31, 0xd2, 0x87, 0x50, 0x04]); // xor edx, edx ; xchg [rax+4], edx
    code.extend([0xc5, 0xfa, 0x2a, 0xc1]); // vcvtsi2ss xmm0, xmm0, ecx
    code.extend([0xc5, 0xca, 0x2a, 0xf2]); // vcvtsi2ss xmm6, xmm6, edx
    code.extend([0x48, 0xb8]); // mov rax, gain
    code.extend((gain as u64).to_le_bytes());
    code.extend([0xc5, 0xfa, 0x59, 0x00]); // vmulss xmm0, xmm0, [rax]
    code.extend([0xc5, 0xca, 0x59, 0x70, 0x04]); // vmulss xmm6, xmm6, [rax+4]
    code.extend(crate::console_insn::jump(stub + code.len(), at + len)?);
    Some(code)
}

/// What replaces the second place: `vmovaps xmm0, xmm6`, then a 4-byte no-op.
const Y_NEW: [u8; 8] = [0xc5, 0xf8, 0x28, 0xc6, 0x0f, 0x1f, 0x40, 0x00];

/// Changes the two places in the image at `base`: whether it did. Neither changes unless both hold
/// the expected bytes.
///
/// # Safety
/// The image is mapped and no thread runs the places' code.
pub(crate) unsafe fn patch(base: usize, first: Site<'_>, second: Site<'_>) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let (a, b) = (base + first.rva, base + second.rva);
    // SAFETY: inside the mapped image, by the caller.
    let (code_a, code_b) = unsafe {
        (
            std::slice::from_raw_parts(a as *const u8, first.expect.len()),
            std::slice::from_raw_parts(b as *const u8, second.expect.len()),
        )
    };
    if !verify(code_a, &first) || !verify(code_b, &second) || first.expect.len() < 5 || second.expect.len() != Y_NEW.len()
    {
        tracing::warn!("VICTORY_MOUSE_NATIVE: the title's code is not the expected build's; nothing changed, the mouse stays the stick");
        return false;
    }
    let Some(place) = crate::console_insn::near(a) else {
        return false;
    };
    let (delta, gain) = (DELTA.as_ptr() as usize, GAIN.as_ptr() as usize);
    let (Some(body), Some(jump)) = (stub(a, first.expect.len(), place, delta, gain), crate::console_insn::jump(a, place)) else {
        return false;
    };
    let mut old = 0;
    // SAFETY: our own fresh code memory; then the two places, made writable for it and put back.
    unsafe {
        std::ptr::copy_nonoverlapping(body.as_ptr(), place as *mut u8, body.len());
        for (at, len) in [(a, first.expect.len()), (b, Y_NEW.len())] {
            if VirtualProtect(at as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                return false;
            }
            if at == a {
                std::ptr::write_bytes(at as *mut u8, 0x90, len);
                std::ptr::copy_nonoverlapping(jump.as_ptr(), at as *mut u8, jump.len());
            } else {
                std::ptr::copy_nonoverlapping(Y_NEW.as_ptr(), at as *mut u8, len);
            }
            VirtualProtect(at as *const _, len, old, &mut old);
            FlushInstructionCache(GetCurrentProcess(), at as *const _, len);
        }
    }
    ACTIVE.store(true, Relaxed);
    true
}

/// `VICTORY_MOUSE_TEST_DELTA=<from>-<to>:<dx>,<dy>` (diagnostics): adds that movement every 8 ms
/// in that window (seconds after the patch), to see the camera turn without a hand on the mouse.
fn test_delta(text: &str) -> Option<(f64, f64, i32, i32)> {
    let (window, delta) = text.split_once(':')?;
    let (from, to) = window.split_once('-')?;
    let (dx, dy) = delta.split_once(',')?;
    Some((from.trim().parse().ok()?, to.trim().parse().ok()?, dx.trim().parse().ok()?, dy.trim().parse().ok()?))
}

/// Applies the patch when `VICTORY_MOUSE_NATIVE=1` asks for it, to the 1.0.2.1 executable only.
///
/// # Safety
/// As [`patch`], with `hash` the original image's.
pub(crate) unsafe fn apply_asked(base: usize, hash: &blake3::Hash) {
    if std::env::var_os("VICTORY_MOUSE_NATIVE").is_none_or(|v| v != "1") {
        return;
    }
    if !hash.to_hex().starts_with(crate::hooks::BUILD_1_0_2_1) {
        tracing::debug!("VICTORY_MOUSE_NATIVE: not the 1.0.2.1 executable; nothing patched");
        return;
    }
    let scale = parse_scale(std::env::var("VICTORY_MOUSE_NATIVE_SCALE").ok().as_deref());
    for (into, v) in GAIN.iter().zip(scale) {
        into.store(v.to_bits(), Relaxed);
    }
    // SAFETY: by the caller.
    let done = unsafe { patch(base, ZERO_AND_X, Y) };
    tracing::info!(done, scale = ?scale, "native mouse aiming (VICTORY_MOUSE_NATIVE)");
    if done
        && cfg!(feature = "diagnostics")
        && let Some((from, to, dx, dy)) = std::env::var("VICTORY_MOUSE_TEST_DELTA").ok().as_deref().and_then(test_delta)
    {
        let start = std::time::Instant::now();
        std::thread::spawn(move || {
            loop {
                let now = start.elapsed().as_secs_f64();
                if now >= to {
                    return;
                }
                if now >= from {
                    add(dx, dy);
                }
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_read_one_or_two_numbers() {
        assert_eq!(parse_scale(None), [DEFAULT_SCALE; 2]);
        assert_eq!(parse_scale(Some("0.02")), [0.02, 0.02]);
        assert_eq!(parse_scale(Some("0,02")), [0.02, 0.02]);
        assert_eq!(parse_scale(Some("0.02/-0.03")), [0.02, -0.03]);
        assert_eq!(parse_scale(Some("0,02 / -0,03")), [0.02, -0.03]);
        assert_eq!(parse_scale(Some("nonsense")), [DEFAULT_SCALE; 2]);
        assert_eq!(parse_scale(Some("0")), [DEFAULT_SCALE; 2]);
    }

    #[test]
    fn test_deltas_parse() {
        assert_eq!(test_delta("150-160:20,-5"), Some((150.0, 160.0, 20, -5)));
        assert_eq!(test_delta("150:20,5"), None);
    }

    #[test]
    fn other_bytes_are_never_patched() {
        let site = Site { rva: 0, expect: &[1, 2, 3, 4, 5] };
        assert!(verify(&[1, 2, 3, 4, 5, 9], &site));
        assert!(!verify(&[1, 2, 3, 4, 6], &site));
        assert!(!verify(&[1, 2, 3], &site));
    }

    /// Code of a test's in the shape of the title's (a zero times a scale, stored; that zero times
    /// another scale, stored), patched and run: it stores the movement times the gains, takes it
    /// once, and a place that is not the expected bytes leaves both places as they were.
    #[test]
    fn the_patched_code_stores_the_movement() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc};
        // mov r8, rcx ; vmovaps xmm2, xmm6 (xmm6 is the caller's). At 6: vxorps xmm6, xmm6, xmm6 ;
        // vmulss xmm0, xmm6, [rip+0x100] (a scale of the test's, not the title's displacement).
        // Then: vmovss [r8], xmm0 ; vmulss xmm0, xmm6, [rip+0x100] ; vmovss [r8+4], xmm0 ;
        // vmovaps xmm6, xmm2 ; ret.
        let mut code = vec![0x49, 0x89, 0xc8, 0xc5, 0xf8, 0x28, 0xd6];
        let first = code.len();
        code.extend([0xc5, 0xc8, 0x57, 0xf6, 0xc5, 0xca, 0x59, 0x05, 0x00, 0x01, 0, 0]);
        code.extend([0xc4, 0xc1, 0x7a, 0x11, 0x00]);
        let second = code.len();
        code.extend([0xc5, 0xca, 0x59, 0x05, 0x00, 0x01, 0, 0]);
        code.extend([0xc4, 0xc1, 0x7a, 0x11, 0x40, 0x04, 0xc5, 0xf8, 0x28, 0xf2, 0xc3]);
        let first_site = Site { rva: first, expect: &code[first..first + 12].to_vec().leak()[..] };
        let second_site = Site { rva: second, expect: &code[second..second + 8].to_vec().leak()[..] };
        type F = unsafe extern "system" fn(*mut [f32; 2]);
        // SAFETY: fresh memory for code, which runs only as written here.
        unsafe {
            let place = VirtualAlloc(std::ptr::null(), 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as usize;
            assert_ne!(place, 0);
            std::ptr::copy_nonoverlapping(code.as_ptr(), place as *mut u8, code.len());
            // The scales the plain code multiplies zero by.
            (place as *mut u8).add(first + 12 + 0x100).cast::<f32>().write_unaligned(3.0);
            let f: F = std::mem::transmute::<usize, F>(place);
            let mut out = [9.0f32; 2];
            f(&mut out);
            assert_eq!(out, [0.0, 0.0]);
            // A second place that is not the expected one: nothing changes.
            let wrong = Site { rva: second, expect: &[0xc5, 0xca, 0x59, 0x05, 0x00, 0x02, 0, 0] };
            assert!(!patch(place, first_site, wrong));
            assert_eq!(std::slice::from_raw_parts(place as *const u8, code.len()), &code[..]);
            GAIN[0].store(0.5f32.to_bits(), Relaxed);
            GAIN[1].store((-2.0f32).to_bits(), Relaxed);
            assert!(patch(place, first_site, second_site));
            assert!(active());
            add(7, 3);
            add(1, 0);
            f(&mut out);
            assert_eq!(out, [4.0, -6.0]);
            assert_eq!(take(), (0, 0));
            f(&mut out);
            assert_eq!(out, [0.0, 0.0]);
        }
    }
}