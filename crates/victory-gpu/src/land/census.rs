//! The copy-back census (`VICTORY_BACK_CENSUS=1`, off by default): of the
//! bytes a landing puts into title memory, who touches them first.
//!
//! Copy-back exists so that the title's CPU sees what shaders wrote. Bytes
//! that only later GPU work reads, or that nobody reads before the card
//! writes them again, could stay on the card. Nothing else in the layer sees
//! the title's *reads*: the write watch and the guards see writes only. So
//! this samples: a 4 KiB page of private title memory that a landing put new
//! bytes into gets a one-shot guard (`PAGE_GUARD`). The first access of any
//! thread, read or write, raises a guard-page exception; the system has taken
//! the guard off by then, our handler notes the thread and the frame and lets
//! the access run again. Pages nobody touched within the window are given
//! their protection back.
//!
//! Only private read-write memory is sampled. View memory (the title's
//! physical pages) is guarded by `crate::watch`, whose protections a second
//! writer would race; its pages are counted, not sampled.
//!
//! Measurement only. What it changes while on: a guard-page exception per
//! sampled page touched, and two protection calls per sampled page. A system
//! call that is handed a guarded page fails once (the kernel takes the guard
//! off and returns an error); the title rarely passes shader output to one,
//! but a census run is not a run to judge the picture by.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

const PAGE: usize = 4096;
/// Slots of the table the handler looks in (a power of two).
const SLOTS: usize = 4096;
/// How far a page's slot may be from its home slot.
const PROBE: usize = 32;
/// A slot let go (lookups go on past it).
const GONE: usize = usize::MAX;

/// See the file.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BACK_CENSUS").is_some_and(|v| v == "1"))
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// One page in this many is a candidate (`VICTORY_BACK_CENSUS_EVERY`, 16).
fn every() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| env("VICTORY_BACK_CENSUS_EVERY", 16).max(1))
}

/// At most this many pages are guarded a frame (`VICTORY_BACK_CENSUS_PAGES`, 64).
fn most() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| env("VICTORY_BACK_CENSUS_PAGES", 64).min(SLOTS as u64 / 4))
}

/// Frames a page waits for a touch (`VICTORY_BACK_CENSUS_FRAMES`, 60).
fn window() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| env("VICTORY_BACK_CENSUS_FRAMES", 60).max(1))
}

/// Page numbers by slot (0: free; [`GONE`]: let go).
static KEYS: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
/// The first touch of a slot's page: (frame + 1) << 33 | write << 32 | thread
/// id; 0 while none came.
static TOUCH: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// Whether a later landing put new bytes into the page before any touch.
static AGAIN: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// Counts since the last line.
mod c {
    /// Landed: bytes put, pages with a new byte.
    pub(super) const BYTES: usize = 0;
    pub(super) const PAGES: usize = 1;
    /// Candidates looked at: private read-write, views, other; their bytes.
    pub(super) const PRIVATE: usize = 2;
    pub(super) const VIEW: usize = 3;
    pub(super) const OTHER: usize = 4;
    pub(super) const PRIVATE_BYTES: usize = 5;
    pub(super) const VIEW_BYTES: usize = 6;
    pub(super) const OTHER_BYTES: usize = 7;
    /// Guarded; not, for want of room or the budget; the protection refused.
    pub(super) const ARMED: usize = 8;
    pub(super) const NO_ROOM: usize = 9;
    pub(super) const REFUSED: usize = 10;
    /// Landed in again while guarded and untouched.
    pub(super) const AGAIN: usize = 11;
    pub(super) const COUNT: usize = 12;
}

static COUNTS: [AtomicU64; c::COUNT] = [const { AtomicU64::new(0) }; c::COUNT];

fn count(which: usize, n: u64) {
    COUNTS[which].fetch_add(n, Ordering::Relaxed);
}

/// Who touched a page first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Who {
    /// A thread of the title's (or a console call of ours on one).
    Title,
    /// The threads that land (the next flight's landing, read-backs).
    Landing,
    /// Another thread of the layer (refreshes, texture uploads, ...).
    Layer,
}

/// The class of a thread by its name.
pub(crate) fn who(name: &str) -> Who {
    if name == "victory-relay" || name.starts_with("victory-land") || name == "victory-late-wait" {
        return Who::Landing;
    }
    const OURS: [&str; 6] = ["texture read-backs", "uploads ahead", "slot check", "card accounting", "pop watch", "victory"];
    if OURS.iter().any(|o| name.starts_with(o)) {
        return Who::Layer;
    }
    Who::Title
}

struct Armed {
    slot: usize,
    page: usize,
    frame: u64,
    bytes: u64,
}

#[derive(Default)]
struct State {
    armed: Vec<Armed>,
    /// The frame the budget is of, and what of it is used.
    frame: u64,
    used: u64,
    /// Resolved since the last line: by [`Who`] and age (same frame, the
    /// next, 2-3, 4 and more), pages and bytes; untouched pages and bytes,
    /// and of them those landed in again.
    touched: [[[u64; 2]; 4]; 3],
    untouched: [u64; 2],
    untouched_again: u64,
    writes_first: u64,
    names: std::collections::HashMap<String, u64>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn home(page: usize) -> usize {
    (page.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 20) & (SLOTS - 1)
}

/// The slot holding `page`, if one does.
fn find(page: usize) -> Option<usize> {
    let h = home(page);
    for i in 0..PROBE {
        let slot = (h + i) & (SLOTS - 1);
        match KEYS[slot].load(Ordering::Acquire) {
            0 => return None,
            k if k == page => return Some(slot),
            _ => {}
        }
    }
    None
}

/// A free slot taken for `page`.
fn take(page: usize) -> Option<usize> {
    let h = home(page);
    for i in 0..PROBE {
        let slot = (h + i) & (SLOTS - 1);
        let k = KEYS[slot].load(Ordering::Acquire);
        if k != 0 && k != GONE {
            continue;
        }
        TOUCH[slot].store(0, Ordering::Relaxed);
        AGAIN[slot].store(0, Ordering::Relaxed);
        if KEYS[slot].compare_exchange(k, page, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            return Some(slot);
        }
    }
    None
}

/// Takes guard-page exceptions of sampled pages; registered ahead of the
/// guards' handler (which takes any other exception in our safe copies for
/// a failed copy).
fn install() {
    use windows_sys::Win32::Foundation::{NTSTATUS, STATUS_GUARD_PAGE_VIOLATION};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_guard(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        if r.ExceptionCode != STATUS_GUARD_PAGE_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let Some(slot) = find(r.ExceptionInformation[1] / PAGE) else {
            return EXCEPTION_CONTINUE_SEARCH;
        };
        // SAFETY: a plain query.
        let thread = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        let write = u64::from(r.ExceptionInformation[0] == 1);
        let touch = (crate::frame::current() + 1) << 33 | write << 32 | u64::from(thread);
        let _ = TOUCH[slot].compare_exchange(0, touch, Ordering::AcqRel, Ordering::Relaxed);
        // The guard is off: the access runs again and goes through.
        EXCEPTION_CONTINUE_EXECUTION
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler only reads and writes the tables above.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(on_guard));
    });
}

/// A landing put `bytes` new bytes into the page that holds `at`.
pub(crate) fn landed(at: usize, bytes: u64) {
    if !on() {
        return;
    }
    landed_as(at, bytes, every(), most());
}

fn landed_as(at: usize, bytes: u64, every: u64, most: u64) {
    count(c::BYTES, bytes);
    count(c::PAGES, 1);
    let page = at / PAGE;
    if let Some(slot) = find(page) {
        if TOUCH[slot].load(Ordering::Acquire) == 0 && AGAIN[slot].swap(1, Ordering::AcqRel) == 0 {
            count(c::AGAIN, 1);
        }
        return;
    }
    let frame = crate::frame::current();
    // The set of candidates moves every 64 frames.
    if !(page as u64 ^ (frame / 64).wrapping_mul(0x2545_f491)).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17).is_multiple_of(every) {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = state.get_or_insert_with(State::default);
    if state.frame != frame {
        (state.frame, state.used) = (frame, 0);
    }
    if state.used >= most {
        count(c::NO_ROOM, 1);
        return;
    }
    state.used += 1;
    arm(state, page, frame, bytes);
}

fn arm(state: &mut State, page: usize, frame: u64, bytes: u64) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE, VirtualProtect, VirtualQuery,
    };
    let at = page * PAGE;
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    let private = n != 0 && info.State == MEM_COMMIT && info.Type == MEM_PRIVATE && info.Protect == PAGE_READWRITE;
    if !private {
        let view = n != 0 && info.Type == MEM_MAPPED;
        count(if view { c::VIEW } else { c::OTHER }, 1);
        count(if view { c::VIEW_BYTES } else { c::OTHER_BYTES }, bytes);
        return;
    }
    count(c::PRIVATE, 1);
    count(c::PRIVATE_BYTES, bytes);
    install();
    let Some(slot) = take(page) else {
        count(c::NO_ROOM, 1);
        return;
    };
    let mut old = 0;
    // SAFETY: one committed page of private memory, its protection kept
    // and a one-shot guard added.
    if unsafe { VirtualProtect(at as *const _, PAGE, PAGE_READWRITE | PAGE_GUARD, &mut old) } == 0 {
        KEYS[slot].store(GONE, Ordering::Release);
        count(c::REFUSED, 1);
        return;
    }
    count(c::ARMED, 1);
    state.armed.push(Armed { slot, page, frame, bytes });
}

/// The guard taken off a page nobody touched, if it is still on.
fn disarm(page: usize) {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE, VirtualProtect, VirtualQuery};
    let at = page * PAGE;
    // SAFETY: as in `arm`.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: as in `arm`.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    if n != 0 && info.Protect == PAGE_READWRITE | PAGE_GUARD {
        let mut old = 0;
        // SAFETY: the page's own protection back, without the guard.
        unsafe { VirtualProtect(at as *const _, PAGE, PAGE_READWRITE, &mut old) };
    }
}

/// The name of thread `id` ("" if it has none or is gone).
fn thread_name(id: u32) -> String {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::System::Threading::{GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
    let mut text = String::new();
    // SAFETY: a handle opened here and closed; the name is the system's,
    // freed as asked.
    unsafe {
        let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, id);
        if thread.is_null() {
            return text;
        }
        let mut name = std::ptr::null_mut();
        if GetThreadDescription(thread, &mut name) >= 0 && !name.is_null() {
            let len = (0..).take_while(|&i| *name.add(i) != 0).count();
            text = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
            LocalFree(name.cast());
        }
        CloseHandle(thread);
    }
    text
}

/// Pages touched, or past the window at frame `now`, are counted and let go.
fn resolve(state: &mut State, now: u64, window: u64) {
    let mut names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    let mut i = 0;
    while i < state.armed.len() {
        let a = &state.armed[i];
        let mut touch = TOUCH[a.slot].load(Ordering::Acquire);
        if touch == 0 {
            if now.saturating_sub(a.frame) < window {
                i += 1;
                continue;
            }
            disarm(a.page);
            // (Touched between the look and the protection.)
            touch = TOUCH[a.slot].load(Ordering::Acquire);
        }
        let age = (touch >> 33).saturating_sub(1).saturating_sub(a.frame);
        if touch == 0 || age >= window {
            state.untouched[0] += 1;
            state.untouched[1] += a.bytes;
            state.untouched_again += AGAIN[a.slot].load(Ordering::Acquire);
        } else {
            let id = touch as u32;
            let name = names.entry(id).or_insert_with(|| thread_name(id)).clone();
            let w = who(&name) as usize;
            let bin = match age {
                0 => 0,
                1 => 1,
                2..=3 => 2,
                _ => 3,
            };
            state.touched[w][bin][0] += 1;
            state.touched[w][bin][1] += a.bytes;
            state.writes_first += (touch >> 32) & 1;
            if w == Who::Title as usize {
                *state.names.entry(if name.is_empty() { "(no name)".into() } else { name }).or_default() += 1;
            }
        }
        KEYS[a.slot].store(GONE, Ordering::Release);
        state.armed.swap_remove(i);
    }
}

/// For the frame-rate line.
pub(crate) fn log(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = COUNTS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64).collect();
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = state.get_or_insert_with(State::default);
    resolve(state, crate::frame::current(), window());
    let mb = |v: f64| v / (1 << 20) as f64;
    let sum = |w: usize, k: usize| state.touched[w].iter().map(|b| b[k]).sum::<u64>();
    let resolved_bytes: u64 = (0..3).map(|w| sum(w, 1)).sum::<u64>() + state.untouched[1];
    let share = |b: u64| if resolved_bytes == 0 { 0.0 } else { 100.0 * b as f64 / resolved_bytes as f64 };
    let mut names: Vec<(&String, &u64)> = state.names.iter().collect();
    names.sort_by(|a, b| b.1.cmp(a.1));
    let names: Vec<String> = names.iter().take(4).map(|(n, k)| format!("{n} {k}")).collect();
    let bins = |w: usize| format!("{}/{}/{}/{}", state.touched[w][0][0], state.touched[w][1][0], state.touched[w][2][0], state.touched[w][3][0]);
    tracing::info!(
        "copy-back census (VICTORY_BACK_CENSUS=1, 1 page in {}, at most {} a frame, a window of {} frames): landed per frame {:.2} MB \
         in {:.0} pages with a new byte; pages looked at {:.0} private ({:.2} MB), {:.0} views ({:.2} MB, not sampled), {:.0} other; \
         guarded {:.0}, no room {:.0}, refused {:.0}; of the guarded pages resolved since the last line (by bytes): the title's CPU \
         first {:.1} % (pages in the same frame/the next/2-3/4+ frames {}), our landing first {:.1} % ({}), our other threads first \
         {:.1} % ({}), untouched {:.1} % ({} pages, {} of them landed in again); {} first touches were writes; title threads first: [{}]; \
         {} pages still guarded",
        every(),
        most(),
        window(),
        mb(s[c::BYTES] / frames),
        s[c::PAGES] / frames,
        s[c::PRIVATE],
        mb(s[c::PRIVATE_BYTES]),
        s[c::VIEW],
        mb(s[c::VIEW_BYTES]),
        s[c::OTHER],
        s[c::ARMED],
        s[c::NO_ROOM],
        s[c::REFUSED],
        share(sum(Who::Title as usize, 1)),
        bins(Who::Title as usize),
        share(sum(Who::Landing as usize, 1)),
        bins(Who::Landing as usize),
        share(sum(Who::Layer as usize, 1)),
        bins(Who::Layer as usize),
        share(state.untouched[1]),
        state.untouched[0],
        state.untouched_again,
        state.writes_first,
        names.join(", "),
        state.armed.len(),
    );
    let armed = std::mem::take(&mut state.armed);
    *state = State { armed, frame: state.frame, used: state.used, ..State::default() };
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

    #[test]
    fn threads_are_told_apart_by_name() {
        assert_eq!(who("victory-relay"), Who::Landing);
        assert_eq!(who("victory-land-2"), Who::Landing);
        assert_eq!(who("victory-submit"), Who::Layer);
        assert_eq!(who("victory-wide-3"), Who::Layer);
        assert_eq!(who("uploads ahead 1"), Who::Layer);
        assert_eq!(who("title-main"), Who::Title);
        assert_eq!(who(""), Who::Title);
        assert_eq!(who("dstorage 3"), Who::Title);
    }

    /// Guarded pages: a read of this thread is caught once and goes through
    /// with the right bytes; an untouched page gets its protection back and
    /// is counted untouched; a page landed in again is counted so.
    #[test]
    fn guarded_pages_note_their_first_touch_and_are_let_go() {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: memory of our own, released at the end.
        let base = unsafe { VirtualAlloc(std::ptr::null(), 4 * PAGE, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(base, 0);
        for p in 0..4 {
            // SAFETY: inside the allocation.
            unsafe { ((base + p * PAGE) as *mut u32).write_volatile(0x1000 + p as u32) };
        }
        let protect = |at: usize| {
            // SAFETY: as in `arm`.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: as in `arm`.
            unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
            info.Protect
        };
        let mut state = State::default();
        let frame = 1000;
        for p in 0..4 {
            arm(&mut state, base / PAGE + p, frame, 100 * (p as u64 + 1));
        }
        assert_eq!(state.armed.len(), 4);
        assert_ne!(protect(base), PAGE_READWRITE, "guarded");
        // Page 0: read here, the bytes as they were.
        // SAFETY: inside the allocation.
        assert_eq!(unsafe { (base as *const u32).read_volatile() }, 0x1000);
        assert_eq!(protect(base), PAGE_READWRITE, "the guard went with the touch");
        // Page 1: written here.
        // SAFETY: inside the allocation.
        unsafe { ((base + PAGE) as *mut u32).write_volatile(7) };
        // SAFETY: inside the allocation.
        assert_eq!(unsafe { ((base + PAGE) as *const u32).read_volatile() }, 7);
        // Page 2: landed in again (not sampled, as `landed` sees it).
        landed_as(base + 2 * PAGE, 4, u64::MAX, 0);
        // Within the window: pages 2 and 3 wait.
        resolve(&mut state, frame + 1, 8);
        assert_eq!(state.armed.len(), 2);
        let me = who(&std::thread::current().name().map(String::from).unwrap_or_default()) as usize;
        assert_eq!(state.touched[me][0][0], 2, "two first touches in the frame they were guarded");
        assert_eq!(state.touched[me][0][1], 300);
        assert_eq!(state.writes_first, 1);
        // Past it: let go, untouched, protection back.
        resolve(&mut state, frame + 8, 8);
        assert!(state.armed.is_empty());
        assert_eq!(state.untouched, [2, 700]);
        assert_eq!(state.untouched_again, 1);
        assert_eq!(protect(base + 2 * PAGE), PAGE_READWRITE);
        assert_eq!(protect(base + 3 * PAGE), PAGE_READWRITE);
        // SAFETY: inside the allocation, no guard any more.
        assert_eq!(unsafe { ((base + 3 * PAGE) as *const u32).read_volatile() }, 0x1003);
        assert!(find(base / PAGE + 3).is_none());
        // SAFETY: ours.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }

    /// Memory that is not private read-write is never guarded.
    #[test]
    fn only_private_read_write_pages_are_guarded() {
        use windows_sys::Win32::System::Memory::PAGE_READONLY;
        // SAFETY: memory of our own, released at the end.
        let base = unsafe { VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE | MEM_COMMIT, PAGE_READONLY) } as usize;
        let mut state = State::default();
        arm(&mut state, base / PAGE, 5, 1);
        assert!(state.armed.is_empty());
        assert!(find(base / PAGE).is_none());
        // SAFETY: ours.
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }
}
