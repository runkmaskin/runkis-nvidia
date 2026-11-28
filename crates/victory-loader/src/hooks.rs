//! Opt-in compatibility hooks for known title builds.
//!
//! LOCAL: copied from vitutu1/victory-perf crates/victory-loader/src/hooks.rs, which ported it from
//! ZigZagBoi1/ScarlettPort (`runtime/victory`, GPL-3.0-or-later).
//! Only per-build offsets and short byte signatures live here (asset policy:
//! nothing of the title itself). Every hook checks the build's hash and the
//! exact bytes it replaces, and does nothing otherwise.

use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// First 16 hex digits of the BLAKE3 of the Series X 1.0.2.1 executable.
pub(crate) const BUILD_1_0_2_1: &str = "b99f876b74d4a77f";

/// Environment variable that turns the watchdog hook on.
pub(crate) const NO_WATCHDOG_ENV: &str = "VICTORY_NO_WATCHDOG";

/// One code patch: `rva` must hold exactly `expect` before `new` is written.
struct Patch<'a> {
    what: &'static str,
    rva: usize,
    expect: &'a [u8],
    new: &'a [u8],
}

/// The title's watchdog thread ends the process on purpose when its main
/// thread has not advanced for about 30 one-second ticks. Two places do it:
/// the handler it calls (`mov dword ptr [0], imm32`) and, when the handler
/// returns or is unset, a fallback right after the call (`mov dword ptr [0],
/// 0; int3`). Jumping over the whole block returns the loop to its normal
/// path; the first patch makes the handler harmless if something else calls it.
const WATCHDOG: &[Patch<'static>] = &[
    Patch {
        what: "watchdog: skip the call and the fallback fault",
        rva: 0x2215c27,
        // mov rax, [rip + disp32] (the handler pointer)
        expect: &[0x48, 0x8B, 0x05, 0x1A, 0x07, 0xFA, 0x04],
        // jmp +0x18 (to the loop's normal continuation), then NOPs
        new: &[0xEB, 0x18, 0x90, 0x90, 0x90, 0x90, 0x90],
    },
    Patch {
        what: "watchdog: handler's deliberate fault",
        rva: 0x23cf683,
        expect: &[0xC7, 0x04, 0x25, 0, 0, 0, 0, 0x21, 0xD4, 0x8B, 0x4D],
        // an eleven-byte NOP
        new: &[0x66, 0x66, 0x2E, 0x0F, 0x1F, 0x84, 0, 0, 0, 0, 0],
    },
];

/// Applies the watchdog patches whose bytes match. Returns how many applied.
///
/// # Safety
/// `base` is the mapped image whose original BLAKE3 is `hash`, and no title
/// code is running yet.
pub(crate) unsafe fn disarm_watchdog(base: usize, hash: &blake3::Hash) -> usize {
    if !hash.to_hex().starts_with(BUILD_1_0_2_1) {
        return 0;
    }
    WATCHDOG
        .iter()
        // SAFETY: forwarded from this function's contract.
        .filter(|p| unsafe { apply(base, p) })
        .count()
}

/// Environment variable that pins the title's internal render scale
/// (`0.2..=1.0` of the output size). Testing lever, off by default.
pub(crate) const RENDER_SCALE_ENV: &str = "VICTORY_RENDER_SCALE";
/// Environment variable that makes the title use its own temporal upscaler
/// on every console profile (Series S otherwise picks FSR).
pub(crate) const OWN_UPSCALER_ENV: &str = "VICTORY_OWN_UPSCALER";

/// Environment variable (with [`OWN_UPSCALER_ENV`] and [`RENDER_SCALE_ENV`])
/// that makes the own upscaler's maximum scale 1 instead of the minimum: the
/// title then sizes its scene's target (post-processing included) at the
/// output size and runs its temporal upscaler, `TAA2U:CS_TAA2_Resolve`, from
/// the render size to it, instead of TAA at the render size and a spatial
/// upscale of the finished picture. Off by default (docs/dlss.md).
pub(crate) const OWN_TAAU_ENV: &str = "VICTORY_OWN_TAAU";

/// The title picks `TAA2U` over `TAA2` each frame when the scene's render size
/// differs from its target size (`0x13f4023`: fields `+0x44/+0x48` against
/// `+0x4c/+0x50` of the TAA object; measured 960x540 against 960x540 with the
/// maximum made the minimum). The target is the output size times the own
/// upscaler's maximum scale, which the title takes as the larger of its minimum
/// and `0x1fb4300` (1 on one console profile, 2/3 on the other). This makes
/// it 1: `vmaxss xmm0, xmm0, [rsi+0x90]` becomes `vmovss xmm0, [rip+disp]`,
/// a load of this build's read-only 1.0 at `0x5fc6ed8`.
fn own_taau_patch() -> OwnedPatch {
    const AT: usize = 0x28f1e87;
    const ONE: usize = 0x5fc6ed8;
    let disp = (ONE as i64 - (AT as i64 + 8)) as i32;
    let mut new = vec![0xC5, 0xFA, 0x10, 0x05];
    new.extend_from_slice(&disp.to_le_bytes());
    OwnedPatch {
        what: "render scale: own upscaler maximum = 1 (temporal upscaler to the output size)",
        rva: AT,
        expect: vec![0xC5, 0xFA, 0x5F, 0x86, 0x90, 0, 0, 0],
        new,
    }
}

/// A patch whose bytes are computed at load time.
struct OwnedPatch {
    what: &'static str,
    rva: usize,
    expect: Vec<u8>,
    new: Vec<u8>,
}

/// The upscaler choice: `mov eax, 6` (own upscaler) then `cmovne eax, ecx`
/// (FSR when the profile asks for it). NOPing the cmovne keeps 6.
const OWN_UPSCALER: Patch<'static> = Patch {
    what: "upscaler: always the title's own",
    rva: 0x7e9426,
    expect: &[0x0F, 0x45, 0xC1],
    new: &[0x0F, 0x1F, 0x00],
};

/// Read-only float constants of this build usable as the FSR scale:
/// value, RVA.
const FSR_SCALES: [(f32, usize); 4] = [
    (0.5, 0x5fc6edc),
    (0.4, 0x5fc6f90),
    (1.0 / 3.0, 0x615a240),
    (0.75, 0x6159b04),
];

/// The render-scale patches for `scale`, with the image at `base`:
/// - the own upscaler's minimum scale (an immediate, 2/3 by default) set to
///   `scale` and its maximum made equal to the minimum (`vmaxss` → `vmovss`),
///   so the scale is fixed;
/// - FSR's scale (a load of 2/3 on Series S) pointed at a constant equal to
///   `scale`, when this build has one, and its maximum size made the scaled
///   size (vtable slot 11 → slot 10's function).
fn render_scale_patches(base: usize, scale: f32, fsr: bool) -> Vec<OwnedPatch> {
    let mut patches = vec![
        OwnedPatch {
            what: "render scale: own upscaler minimum",
            rva: 0x28f1e7e,
            expect: vec![0xAB, 0xAA, 0x2A, 0x3F],
            new: scale.to_le_bytes().to_vec(),
        },
        OwnedPatch {
            what: "render scale: own upscaler maximum = minimum",
            rva: 0x28f1e87,
            expect: vec![0xC5, 0xFA, 0x5F, 0x86, 0x90, 0, 0, 0],
            new: vec![0xC5, 0xFA, 0x10, 0x86, 0x90, 0, 0, 0],
        },
    ];
    if !fsr {
        return patches;
    }
    match FSR_SCALES.iter().find(|(v, _)| (v - scale).abs() < 1e-3) {
        Some(&(_, constant)) => {
            // vmovss xmm0, [rip + disp32]; the next instruction is at +8.
            const LOAD: usize = 0x28ef6b2;
            let disp = (constant as i64 - (LOAD as i64 + 8)) as i32;
            let mut new = vec![0xC5, 0xFA, 0x10, 0x05];
            new.extend_from_slice(&disp.to_le_bytes());
            patches.push(OwnedPatch {
                what: "render scale: FSR scale",
                rva: LOAD,
                expect: vec![0xC5, 0xFA, 0x10, 0x05, 0xA2, 0xE2, 0x86, 0x03],
                new,
            });
            patches.push(OwnedPatch {
                what: "render scale: FSR maximum = scaled size",
                rva: 0x6300ba8,
                expect: ((base + 0x28ef6f0) as u64).to_le_bytes().to_vec(),
                new: ((base + 0x28ef680) as u64).to_le_bytes().to_vec(),
            });
        }
        None => tracing::warn!(
            scale,
            "no FSR constant for this scale (0.5, 0.4, 1/3, 0.75); FSR keeps its own (use {OWN_UPSCALER_ENV}=1)"
        ),
    }
    patches
}

/// Environment variable that enlarges the two memory pools the title sizes
/// for the Series S and which run out above a render height of 768 lines: a
/// factor (`1.5`, up to 16) or the graphics pool's size in MiB (`2200`,
/// above 16); the page pool grows by the same factor. Off by default.
pub(crate) const TITLE_POOL_ENV: &str = "VICTORY_TITLE_POOL";

/// At start-up the title reserves its memory pools from a read-only table
/// of sizes, one row per console (Series S when the device type is 6, else
/// Series X). Its third pool is write-combined graphics memory, which GPU
/// buffers come from. On Series S it is 1277 MiB; above 768 lines the
/// title runs out of it, calls its out-of-memory handler and stops at an
/// int3. This is that entry's RVA in the Series S row: the title reads it
/// once, before it reserves the pool. Nothing in the title derives a size
/// from what the layer reports as title memory; the row goes by device type.
const SERIES_S_GRAPHICS_POOL: usize = 0x6186268;
/// The size the Series S row asks for (Series X asks for 2199 MiB).
const SERIES_S_GRAPHICS_POOL_BYTES: u64 = 0x4fd0_0000;
/// The title also allocates physical 64 KiB pages at start-up
/// (`XMemAllocatePhysicalPages`) and maps them under GPU resources later.
/// The count is an immediate chosen by device type: `mov eax, 4752` (297
/// MiB; Series X 11360). What it maps grows with the render height (432 MiB
/// at 1080 lines), and the title checks that against the count with an
/// int3. This is that `mov`.
const SERIES_S_PAGE_POOL: usize = 0x1d488ac;
/// Series S's count of 64 KiB pages.
const SERIES_S_PAGE_POOL_PAGES: u32 = 0x1290;
/// The title turns the count into bytes with a 32-bit shift, so it stays
/// below 2^16 pages (4 GiB).
const PAGE_POOL_MOST: u32 = 0xffff;
/// Graphics pool sizes are rounded up to this: the title hands the pool out
/// in 128 KiB blocks, and 2 MiB keeps large pages possible.
const POOL_ROUNDING: u64 = 2 << 20;
/// No more than 16 GiB: the pools share the space below 2^40 with the
/// title's other reservations, and the host has to commit it all.
const POOL_MOST: u64 = 16 << 30;

/// The graphics pool size `VICTORY_TITLE_POOL` asks for, in bytes, rounded
/// up to [`POOL_ROUNDING`]. A plain number up to 16 is a factor of the
/// Series S size, a larger one is MiB. Shrinking is refused: the title would
/// only run out sooner.
fn parse_pool(text: &str) -> Option<u64> {
    let v: f64 = text.trim().parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    let bytes = if v <= 16.0 {
        v * SERIES_S_GRAPHICS_POOL_BYTES as f64
    } else {
        v * (1u64 << 20) as f64
    };
    if !(SERIES_S_GRAPHICS_POOL_BYTES as f64..=POOL_MOST as f64).contains(&bytes) {
        return None;
    }
    // POOL_MOST is a multiple of the rounding, so rounding up stays within it.
    Some((bytes as u64).div_ceil(POOL_ROUNDING) * POOL_ROUNDING)
}

/// The page count that grows the page pool as the graphics pool grew to
/// `bytes`, rounded up.
fn page_pool_pages(bytes: u64) -> u32 {
    let pages = (u64::from(SERIES_S_PAGE_POOL_PAGES) * bytes).div_ceil(SERIES_S_GRAPHICS_POOL_BYTES);
    pages.min(u64::from(PAGE_POOL_MOST)) as u32
}

/// The patches that give the Series S graphics pool `bytes` and its page
/// pool as many more pages.
fn title_pool_patches(bytes: u64) -> [OwnedPatch; 2] {
    let mut mov = vec![0xB8];
    mov.extend_from_slice(&page_pool_pages(bytes).to_le_bytes());
    [
        OwnedPatch {
            what: "title pool: Series S graphics pool size",
            rva: SERIES_S_GRAPHICS_POOL,
            expect: SERIES_S_GRAPHICS_POOL_BYTES.to_le_bytes().to_vec(),
            new: bytes.to_le_bytes().to_vec(),
        },
        OwnedPatch {
            what: "title pool: Series S physical page count",
            rva: SERIES_S_PAGE_POOL,
            // mov eax, 0x1290
            expect: vec![0xB8, 0x90, 0x12, 0x00, 0x00],
            new: mov,
        },
    ]
}
/// Environment variable that sets the worker counts of the title's two large job pools:
/// `<first>[,<second>]`, 1..=16 each (the title's are 8 and 5). Off by default.
pub(crate) const TITLE_WORKERS_ENV: &str = "VICTORY_TITLE_WORKERS";

/// The title starts three job pools at start-up from a read-only table of three rows (one per
/// pool: a kind, the worker count, the workers' priority, a pointer to a processor list that
/// is null in all three). The counts are fixed, 8, 5 and 1, whatever the processor count; the
/// workers float over every processor. These are the first two rows' counts.
const JOB_POOL_COUNTS: [(usize, u32); 2] = [(0x6188d2c, 8), (0x6188d44, 5)];

/// The counts `VICTORY_TITLE_WORKERS` asks for: the first pool's, then the second's (the
/// title's when not given).
fn parse_workers(text: &str) -> Option<[u32; 2]> {
    let mut counts = JOB_POOL_COUNTS.map(|(_, n)| n);
    let mut parts = text.split(',');
    for count in &mut counts {
        if let Some(part) = parts.next() {
            *count = part.trim().parse().ok().filter(|n| (1..=16).contains(n))?;
        }
    }
    parts.next().is_none().then_some(counts)
}

/// The patches that give the two pools `counts` workers.
fn worker_patches(counts: [u32; 2]) -> Vec<OwnedPatch> {
    JOB_POOL_COUNTS
        .iter()
        .zip(counts)
        .filter(|((_, title), n)| title != n)
        .map(|(&(rva, title), n)| OwnedPatch {
            what: "title job pool: worker count",
            rva,
            expect: title.to_le_bytes().to_vec(),
            new: n.to_le_bytes().to_vec(),
        })
        .collect()
}

/// Environment variable (with `VICTORY_FRAME_INTERVAL_US`) that makes the title's timers step by
/// at least one frame of the rate that interval gives, instead of one frame of the title's own
/// 30. Off by default.
pub(crate) const TITLE_STEP_ENV: &str = "VICTORY_TITLE_STEP";

/// LOCAL: ported from trustno1-coder/VIctory-NVIDIA 45f6250 (GPL-3.0-or-later), its `Kind::SimRate`
/// site. The simulation gives each of its timers the real time since the last frame, but at
/// least `1 / rate`, and the rate comes from this function, which reads the title's frame rate
/// setting (30). With our clock at 60 (`VICTORY_FRAME_INTERVAL_US=16667`) every frame shorter
/// than 33.3 ms still moves game time 33.3 ms: game time runs fast (their run fps-1: twice the
/// wall clock in the menus). `cmp dword [rax+210h], 0` becomes `mov eax, rate; jmp` to the
/// function's `vcvtsi2ss`, so it returns `rate`. The bytes from the `cmp` to the `vcvtsi2ss`
/// are compared first.
const SIM_RATE: usize = 0x2241fdb;
const SIM_RATE_EXPECT: [u8; 25] = [
    0x83, 0xb8, 0x10, 0x02, 0x00, 0x00, 0x00, // cmp dword [rax+210h], 0
    0x74, 0x12, // je
    0xe8, 0xc7, 0xa1, 0xe0, 0x02, // call (the settings)
    0x8b, 0x80, 0x10, 0x02, 0x00, 0x00, // mov eax, [rax+210h]
    0xc4, 0xe1, 0xfa, 0x2a, 0xc0, // vcvtsi2ss xmm0, xmm0, rax
];

/// The rate an interval of `us` microseconds gives (16667 -> 60), 1000 at most.
fn step_rate(us: u64) -> Option<u32> {
    (us > 0).then(|| (1e6 / us as f64).round().clamp(1.0, 1000.0) as u32)
}

/// The timers' shortest step patch for `rate`: `mov eax, rate; jmp +13` (to the `vcvtsi2ss`, 20
/// bytes on), the rest of the compared bytes as they are.
fn sim_rate_patch(rate: u32) -> OwnedPatch {
    let mut new = SIM_RATE_EXPECT.to_vec();
    new[0] = 0xb8;
    new[1..5].copy_from_slice(&rate.to_le_bytes());
    new[5] = 0xeb;
    new[6] = 0x0d;
    OwnedPatch {
        what: "timers: the shortest step follows our frame rate",
        rva: SIM_RATE,
        expect: SIM_RATE_EXPECT.to_vec(),
        new,
    }
}

/// Applies [`TITLE_STEP_ENV`] when asked for and `VICTORY_FRAME_INTERVAL_US` gives another rate
/// than the title's 30. Returns how many applied.
///
/// # Safety
/// As `disarm_watchdog`.
pub(crate) unsafe fn timer_step(base: usize, hash: &blake3::Hash) -> usize {
    if std::env::var_os(TITLE_STEP_ENV).is_none_or(|v| v != "1") {
        return 0;
    }
    if !hash.to_hex().starts_with(BUILD_1_0_2_1) {
        return 0;
    }
    let us = std::env::var("VICTORY_FRAME_INTERVAL_US").ok().and_then(|v| v.trim().parse::<u64>().ok());
    let Some(rate) = us.and_then(step_rate).filter(|&r| r != 30) else {
        tracing::info!("{TITLE_STEP_ENV}: the title's own 30 fps; its timers left as they are");
        return 0;
    };
    let p = sim_rate_patch(rate);
    let p = Patch {
        what: p.what,
        rva: p.rva,
        expect: &p.expect,
        new: &p.new,
    };
    // SAFETY: forwarded from this function's contract.
    let done = unsafe { apply(base, &p) };
    tracing::info!(rate, done, "{TITLE_STEP_ENV}: the title's timers step at least 1/{rate} s");
    usize::from(done)
}

/// The scale `VICTORY_RENDER_SCALE` asks for, if valid.
fn parse_scale(text: &str) -> Option<f32> {
    let v: f32 = text.trim().parse().ok()?;
    (0.2..=1.0).contains(&v).then_some(v)
}

/// Applies the resolution levers asked for in the environment
/// ([`RENDER_SCALE_ENV`], [`OWN_UPSCALER_ENV`]). Returns how many applied.
///
/// # Safety
/// As `disarm_watchdog`.
pub(crate) unsafe fn resolution_levers(base: usize, hash: &blake3::Hash) -> usize {
    let scale = std::env::var(RENDER_SCALE_ENV).ok();
    let own = std::env::var_os(OWN_UPSCALER_ENV).is_some_and(|v| v != "0");
    let pool = std::env::var(TITLE_POOL_ENV).ok().filter(|v| v.trim() != "0");
    let workers = std::env::var(TITLE_WORKERS_ENV).ok().filter(|v| v.trim() != "0");
    if scale.is_none() && !own && pool.is_none() && workers.is_none() {
        return 0;
    }
    // Every image the loader maps comes here (the title's DLLs too); only
    // the 1.0.2.1 executable has the patched places.
    if !hash.to_hex().starts_with(BUILD_1_0_2_1) {
        tracing::debug!("resolution levers: not the 1.0.2.1 executable; nothing patched");
        return 0;
    }
    let mut applied = 0;
    if own {
        // SAFETY: forwarded from this function's contract.
        applied += usize::from(unsafe { apply(base, &OWN_UPSCALER) });
    }
    if let Some(text) = scale {
        match parse_scale(&text) {
            Some(scale) => {
                // With the own upscaler forced, FSR is left as it is.
                let mut patches = render_scale_patches(base, scale, !own);
                // VICTORY_OWN_TAAU: the maximum is 1, not the minimum.
                if own && std::env::var_os(OWN_TAAU_ENV).is_some_and(|v| v != "0") {
                    patches[1] = own_taau_patch();
                }
                for p in patches {
                    let p = Patch {
                        what: p.what,
                        rva: p.rva,
                        expect: &p.expect,
                        new: &p.new,
                    };
                    // SAFETY: forwarded from this function's contract.
                    applied += usize::from(unsafe { apply(base, &p) });
                }
            }
            None => tracing::warn!(text, "{RENDER_SCALE_ENV} wants a number in 0.2..=1.0"),
        }
    }
    if let Some(text) = pool {
        match parse_pool(&text) {
            Some(bytes) => {
                for p in title_pool_patches(bytes) {
                    let p = Patch {
                        what: p.what,
                        rva: p.rva,
                        expect: &p.expect,
                        new: &p.new,
                    };
                    // SAFETY: forwarded from this function's contract. The
                    // table entry is read-only data; `apply` makes it
                    // writable for the copy.
                    applied += usize::from(unsafe { apply(base, &p) });
                }
                // Series X's sizes are left alone, so on that profile this
                // changes nothing.
                tracing::info!(
                    graphics_mib = bytes >> 20,
                    page_mib = u64::from(page_pool_pages(bytes)) >> 4,
                    "{TITLE_POOL_ENV}: Series S pools"
                );
            }
            None => tracing::warn!(
                text,
                "{TITLE_POOL_ENV} wants a factor (1..=16) or MiB (1277..=16384) of the Series S graphics pool"
            ),
        }
    }
    if let Some(text) = workers {
        match parse_workers(&text) {
            Some(counts) => {
                for p in worker_patches(counts) {
                    let p = Patch {
                        what: p.what,
                        rva: p.rva,
                        expect: &p.expect,
                        new: &p.new,
                    };
                    // SAFETY: forwarded from this function's contract (read-only data, made
                    // writable by `apply` for the copy).
                    applied += usize::from(unsafe { apply(base, &p) });
                }
                tracing::info!(?counts, "{TITLE_WORKERS_ENV}: the title's job pools");
            }
            None => tracing::warn!(text, "{TITLE_WORKERS_ENV} wants <first>[,<second>], 1..=16 each"),
        }
    }
    applied
}

/// # Safety
/// See `disarm_watchdog`.
unsafe fn apply(base: usize, p: &Patch<'_>) -> bool {
    let at = (base + p.rva) as *mut u8;
    // SAFETY: the offset is inside the mapped .text of this exact build.
    let now = unsafe { std::slice::from_raw_parts(at, p.expect.len()) };
    if now != p.expect || p.new.len() != p.expect.len() {
        tracing::warn!(what = p.what, "hook skipped: the bytes are not the expected ones");
        return false;
    }
    let mut old = 0;
    // SAFETY: changing the protection of those bytes only, restored below.
    if unsafe { VirtualProtect(at as _, p.new.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        return false;
    }
    // SAFETY: writable now; the bytes are the ones checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(p.new.as_ptr(), at, p.new.len());
        VirtualProtect(at as _, p.new.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as _, p.new.len());
    }
    tracing::info!(what = p.what, "hook applied");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_outside_the_floor_are_refused() {
        assert_eq!(parse_scale("0.5"), Some(0.5));
        assert_eq!(parse_scale(" 1 "), Some(1.0));
        assert_eq!(parse_scale("0.1"), None);
        assert_eq!(parse_scale("1.5"), None);
        assert_eq!(parse_scale("half"), None);
    }

    #[test]
    fn half_scale_patches_both_upscalers() {
        let base = 0x7ff0_0000_0000;
        let p = render_scale_patches(base, 0.5, true);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].new, 0.5f32.to_le_bytes());
        // The FSR load now reads 0.5 at 0x5fc6edc: disp from the next instruction.
        assert_eq!(p[2].new, [0xC5, 0xFA, 0x10, 0x05, 0x22, 0x78, 0x6D, 0x03]);
        assert_eq!(p[3].new, ((base + 0x28ef680) as u64).to_le_bytes());
        assert!(p.iter().all(|p| p.new.len() == p.expect.len()));
    }

    #[test]
    fn pool_sizes_are_factors_or_mib_and_never_shrink() {
        let s = SERIES_S_GRAPHICS_POOL_BYTES;
        assert_eq!(parse_pool("1"), Some(s.div_ceil(POOL_ROUNDING) * POOL_ROUNDING));
        assert_eq!(parse_pool(" 2 "), Some(2 * s));
        assert_eq!(parse_pool("2200"), Some(2200 << 20));
        // 2199 MiB (Series X's size) is rounded up to 2 MiB.
        assert_eq!(parse_pool("2199"), Some(2200 << 20));
        assert_eq!(parse_pool("16384"), Some(16 << 30));
        assert_eq!(parse_pool("0.5"), None);
        assert_eq!(parse_pool("1000"), None);
        assert_eq!(parse_pool("16385"), None);
        assert_eq!(parse_pool("NaN"), None);
        assert_eq!(parse_pool("big"), None);
        assert!(parse_pool("1.7").is_some_and(|b| b % POOL_ROUNDING == 0));
    }

    #[test]
    fn the_page_pool_grows_with_the_graphics_pool() {
        assert_eq!(page_pool_pages(SERIES_S_GRAPHICS_POOL_BYTES), SERIES_S_PAGE_POOL_PAGES);
        assert_eq!(page_pool_pages(2 * SERIES_S_GRAPHICS_POOL_BYTES), 2 * SERIES_S_PAGE_POOL_PAGES);
        // Rounded up: never fewer pages than the factor asks for.
        assert_eq!(page_pool_pages(SERIES_S_GRAPHICS_POOL_BYTES + 1), SERIES_S_PAGE_POOL_PAGES + 1);
        // The largest graphics pool stays under the title's 32-bit shift.
        assert!(page_pool_pages(POOL_MOST) < PAGE_POOL_MOST);
        assert_eq!(page_pool_pages(u64::MAX / 0x2000), PAGE_POOL_MOST);
    }

    /// `apply` on a synthetic copy of `p`'s site: written when the bytes are
    /// the expected ones, left alone otherwise. Returns what the site holds.
    fn patch_site(p: &OwnedPatch, site: &[u8]) -> (bool, Vec<u8>) {
        let patch = Patch {
            what: p.what,
            rva: p.rva,
            expect: &p.expect,
            new: &p.new,
        };
        let mut site = site.to_vec();
        // A fake image base that puts the RVA on the buffer.
        let base = (site.as_mut_ptr() as usize).wrapping_sub(p.rva);
        // SAFETY: base + rva is `site`, as long as the expected bytes.
        let done = unsafe { apply(base, &patch) };
        (done, site)
    }

    #[test]
    fn pool_patches_check_their_sites_before_writing() {
        let [size, count] = title_pool_patches(2 * SERIES_S_GRAPHICS_POOL_BYTES);
        assert_eq!(size.expect, [0x00, 0x00, 0xD0, 0x4F, 0, 0, 0, 0]);
        let (done, now) = patch_site(&size, &size.expect);
        assert!(done);
        assert_eq!(u64::from_le_bytes(now.try_into().unwrap()), 2 * SERIES_S_GRAPHICS_POOL_BYTES);
        // mov eax, 0x2520: twice the pages, the instruction's length kept.
        let (done, now) = patch_site(&count, &count.expect);
        assert!(done);
        assert_eq!(now, [0xB8, 0x20, 0x25, 0x00, 0x00]);
        // Another build, or an entry already changed: untouched.
        for p in [&size, &count] {
            let mut other = p.expect.clone();
            other[1] ^= 1;
            assert_eq!(patch_site(p, &other), (false, other));
        }
    }

    #[test]
    fn the_temporal_upscaler_patch_loads_one_in_place_of_the_maximum() {
        let p = own_taau_patch();
        // The same eight bytes as the `maximum = minimum` patch replaces.
        assert_eq!(p.expect, render_scale_patches(0, 0.5, false)[1].expect);
        assert_eq!(p.rva, render_scale_patches(0, 0.5, false)[1].rva);
        // vmovss xmm0, [rip + disp32]: the next instruction is at 0x28f1e8f.
        let disp = i32::from_le_bytes(p.new[4..8].try_into().unwrap());
        assert_eq!(0x28f1e8f_i64 + i64::from(disp), 0x5fc6ed8);
        assert_eq!(&p.new[..4], &[0xC5, 0xFA, 0x10, 0x05]);
    }

    #[test]
    fn rates_come_from_the_interval() {
        assert_eq!(step_rate(16667), Some(60));
        assert_eq!(step_rate(33333), Some(30));
        assert_eq!(step_rate(8333), Some(120));
        assert_eq!(step_rate(1), Some(1000));
        assert_eq!(step_rate(0), None);
    }

    /// The patched function run for real: it returns the rate instead of the setting.
    #[test]
    fn the_timer_step_returns_the_rate() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree};
        let p = sim_rate_patch(60);
        assert_eq!(&p.new[..7], &[0xb8, 60, 0, 0, 0, 0xeb, 0x0d]);
        // The jump lands on the vcvtsi2ss.
        assert_eq!(7 + usize::from(p.new[6]), 20);
        assert_eq!(&p.new[20..], &SIM_RATE_EXPECT[20..]);
        // A function: rax = settings (+0x210 = 30), the compared bytes with the call made
        // harmless (the je skips nothing here: the setting is not 0), ret.
        let settings = [30u32; 0x100];
        let mut f = vec![0x48, 0xb8];
        f.extend((settings.as_ptr() as u64).to_le_bytes());
        let at = f.len();
        let mut body = SIM_RATE_EXPECT;
        body[9..14].copy_from_slice(&[0x0f, 0x1f, 0x44, 0x00, 0x00]); // the call -> nop
        f.extend(body);
        f.push(0xc3);
        // SAFETY: our fresh page, written, patched and run.
        unsafe {
            let place = VirtualAlloc(std::ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as *mut u8;
            assert!(!place.is_null());
            std::ptr::copy_nonoverlapping(f.as_ptr(), place, f.len());
            let rate: extern "system" fn() -> f32 = std::mem::transmute(place);
            assert_eq!(rate(), 30.0);
            // The real site's bytes (with its call) are what `apply` checks; here the call is
            // a nop, so check and write by hand the same way.
            let site = place.add(at);
            assert_eq!(std::slice::from_raw_parts(site, 9), &SIM_RATE_EXPECT[..9]);
            std::ptr::copy_nonoverlapping(p.new.as_ptr(), site, 7);
            assert_eq!(rate(), 60.0);
            VirtualFree(place.cast(), 0, MEM_RELEASE);
        }
        // `apply` checks all 25 bytes.
        let (done, now) = patch_site(&p, &p.expect);
        assert!(done);
        assert_eq!(now, p.new);
        let mut other = p.expect.clone();
        other[2] = 0x14;
        assert_eq!(patch_site(&p, &other), (false, other));
    }

    #[test]
    fn worker_counts_patch_only_what_changes_and_check_first() {
        assert_eq!(parse_workers("6"), Some([6, 5]));
        assert_eq!(parse_workers(" 6 , 4 "), Some([6, 4]));
        assert_eq!(parse_workers("0"), None);
        assert_eq!(parse_workers("17"), None);
        assert_eq!(parse_workers("6,4,2"), None);
        assert_eq!(parse_workers("many"), None);
        assert!(worker_patches([8, 5]).is_empty());
        let p = worker_patches([6, 5]);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].rva, p[0].expect.as_slice()), (0x6188d2c, &[8, 0, 0, 0][..]));
        let (done, now) = patch_site(&p[0], &p[0].expect);
        assert!(done);
        assert_eq!(now, [6, 0, 0, 0]);
        // A count already changed, or another build: untouched.
        assert_eq!(patch_site(&p[0], &[7, 0, 0, 0]), (false, vec![7, 0, 0, 0]));
        let p = worker_patches([8, 3]);
        assert_eq!((p[0].rva, p[0].new.as_slice()), (0x6188d44, &[3, 0, 0, 0][..]));
    }

    #[test]
    fn odd_scales_patch_only_the_own_upscaler() {
        assert_eq!(render_scale_patches(0, 0.6, true).len(), 2);
        assert_eq!(render_scale_patches(0, 0.5, false).len(), 2);
    }
}

