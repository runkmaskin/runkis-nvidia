//! The title's Series S pools made larger (`VICTORY_TITLE_POOL`; off unless asked for).
//!
//! At start-up the 1.0.2.1 executable sizes two memory pools by device type, not from the memory
//! the layer reports: its write-combined graphics pool (1277 MiB on Series S, a read-only table
//! entry per console) and its count of physical 64 KiB pages (297 MiB, an immediate). With a
//! Series S scene above 768 lines both run out: the title calls its out-of-memory handler and
//! stops at an `int3`. The switch makes both larger, after checking the build's hash and the bytes
//! at each place: a factor (up to 16) or the graphics pool's size in MiB (above 16); the page pool
//! grows by the same factor. Series X's sizes are left alone, so on that profile it changes nothing.
//!
//! Ported from the runkis-nvidia fork (its commits 558f936 and d63e24a, GPL-3.0-or-later), where
//! 810, 900, 960 and 1080 lines on Series S started with factor 2 (1080 measured into gameplay).

/// The first hex digits of the 1.0.2.1 executable's BLAKE3 hash (as `RETAIL_BUILD` in apps/victory).
const BUILD_1_0_2_1: &str = "b99f876b74d4a77f";

/// The environment variable that turns the patch on.
pub const TITLE_POOL_ENV: &str = "VICTORY_TITLE_POOL";

/// The Series S row's graphics pool entry (RVA). The title reads it once, before it reserves the
/// pool.
const SERIES_S_GRAPHICS_POOL: usize = 0x6186268;
/// The size the Series S row asks for (Series X asks for 2199 MiB).
const SERIES_S_GRAPHICS_POOL_BYTES: u64 = 0x4fd0_0000;
/// The count of physical 64 KiB pages the title allocates at start-up: `mov eax, 4752` (297 MiB;
/// Series X 11360). What it maps grows with the render height (432 MiB at 1080 lines).
const SERIES_S_PAGE_POOL: usize = 0x1d488ac;
/// Series S's count of 64 KiB pages.
const SERIES_S_PAGE_POOL_PAGES: u32 = 0x1290;
/// The title turns the count into bytes with a 32-bit shift, so it stays below 2^16 pages.
const PAGE_POOL_MOST: u32 = 0xffff;
/// Graphics pool sizes are rounded up to this: the title hands the pool out in 128 KiB blocks,
/// and 2 MiB keeps large pages possible.
const POOL_ROUNDING: u64 = 2 << 20;
/// No more than 16 GiB: the pools share the space below 2^40 with the title's other reservations.
const POOL_MOST: u64 = 16 << 30;

/// One place of the image: what is there and what goes there instead.
struct Patch {
    what: &'static str,
    rva: usize,
    expect: Vec<u8>,
    new: Vec<u8>,
}

/// The graphics pool size `VICTORY_TITLE_POOL` asks for, in bytes, rounded up to
/// [`POOL_ROUNDING`]. A plain number up to 16 is a factor of the Series S size, a larger one is
/// MiB. Shrinking is refused: the title would only run out sooner.
fn parse_pool(text: &str) -> Option<u64> {
    let v: f64 = text.trim().parse().ok()?;
    if !v.is_finite() {
        return None;
    }
    let bytes = if v <= 16.0 { v * SERIES_S_GRAPHICS_POOL_BYTES as f64 } else { v * (1u64 << 20) as f64 };
    if !(SERIES_S_GRAPHICS_POOL_BYTES as f64..=POOL_MOST as f64).contains(&bytes) {
        return None;
    }
    // POOL_MOST is a multiple of the rounding, so rounding up stays within it.
    Some((bytes as u64).div_ceil(POOL_ROUNDING) * POOL_ROUNDING)
}

/// The page count that grows the page pool as the graphics pool grew to `bytes`, rounded up.
fn page_pool_pages(bytes: u64) -> u32 {
    let pages = (u64::from(SERIES_S_PAGE_POOL_PAGES) * bytes).div_ceil(SERIES_S_GRAPHICS_POOL_BYTES);
    pages.min(u64::from(PAGE_POOL_MOST)) as u32
}

/// The patches that give the Series S graphics pool `bytes` and its page pool as many more pages.
fn patches(bytes: u64) -> [Patch; 2] {
    let mut mov = vec![0xB8];
    mov.extend_from_slice(&page_pool_pages(bytes).to_le_bytes());
    [
        Patch {
            what: "Series S graphics pool size",
            rva: SERIES_S_GRAPHICS_POOL,
            expect: SERIES_S_GRAPHICS_POOL_BYTES.to_le_bytes().to_vec(),
            new: bytes.to_le_bytes().to_vec(),
        },
        Patch {
            what: "Series S physical page count",
            rva: SERIES_S_PAGE_POOL,
            // mov eax, 0x1290
            expect: vec![0xB8, 0x90, 0x12, 0x00, 0x00],
            new: mov,
        },
    ]
}

/// Writes `p` at `base + p.rva` if the bytes there are `p.expect`: whether it did.
///
/// # Safety
/// `base + p.rva` is mapped for `p.expect.len()` bytes, and no thread runs or reads them.
#[cfg(windows)]
unsafe fn apply(base: usize, p: &Patch) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let at = base.wrapping_add(p.rva);
    // SAFETY: mapped, by the caller.
    let there = unsafe { std::slice::from_raw_parts(at as *const u8, p.expect.len()) };
    if there != p.expect.as_slice() || p.new.len() != p.expect.len() {
        tracing::warn!(what = p.what, "{TITLE_POOL_ENV}: not the expected bytes; left alone");
        return false;
    }
    let mut old = 0;
    // SAFETY: those bytes, made writable for the copy and put back.
    unsafe {
        if VirtualProtect(at as *const _, p.new.len(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        std::ptr::copy_nonoverlapping(p.new.as_ptr(), at as *mut u8, p.new.len());
        VirtualProtect(at as *const _, p.new.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as *const _, p.new.len());
    }
    true
}

/// Applies the patches when `VICTORY_TITLE_POOL` asks for them, to the 1.0.2.1 executable only:
/// how many applied.
///
/// # Safety
/// `base` is the mapped image whose original BLAKE3 is `hash`, and no title code has run.
#[cfg(windows)]
pub unsafe fn apply_asked(base: usize, hash: &blake3::Hash) -> usize {
    let Some(text) = std::env::var(TITLE_POOL_ENV).ok().filter(|v| !v.trim().is_empty() && v.trim() != "0") else {
        return 0;
    };
    if !hash.to_hex().starts_with(BUILD_1_0_2_1) {
        tracing::warn!("{TITLE_POOL_ENV}: not the 1.0.2.1 executable; nothing patched");
        return 0;
    }
    let Some(bytes) = parse_pool(&text) else {
        tracing::warn!(text, "{TITLE_POOL_ENV} wants a factor (1..=16) or MiB (1277..=16384) of the Series S graphics pool");
        return 0;
    };
    // SAFETY: by the caller; the table entry is read-only data, which `apply` makes writable.
    let applied = patches(bytes).iter().map(|p| usize::from(unsafe { apply(base, p) })).sum();
    tracing::info!(applied, graphics_mib = bytes >> 20, page_mib = u64::from(page_pool_pages(bytes)) >> 4, "{TITLE_POOL_ENV}: Series S pools");
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `apply` on a synthetic copy of `p`'s site: written when the bytes are the expected ones,
    /// left alone otherwise. Returns what the site holds.
    #[cfg(windows)]
    fn patch_site(p: &Patch, site: &[u8]) -> (bool, Vec<u8>) {
        let mut site = site.to_vec();
        // A fake image base that puts the RVA on the buffer.
        let base = (site.as_mut_ptr() as usize).wrapping_sub(p.rva);
        // SAFETY: base + rva is `site`, as long as the expected bytes.
        let done = unsafe { apply(base, p) };
        (done, site)
    }

    #[cfg(windows)]
    #[test]
    fn pool_patches_check_their_sites_before_writing() {
        let [size, count] = patches(2 * SERIES_S_GRAPHICS_POOL_BYTES);
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
}
