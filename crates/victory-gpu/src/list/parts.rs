//! Where a draw's or dispatch's time goes on the threads that translate
//! list calls (`VICTORY_DRAW_PARTS=1`; off by default). Each part's time is
//! from the mark before it to its own mark, on the thread's clock; a line
//! of the log gives microseconds per call, by part, every few seconds.
//! Measures only: nothing recorded changes. Off, a mark is one load of a
//! flag.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// The parts, in the order a draw passes them.
#[derive(Clone, Copy)]
pub(crate) enum Part {
    /// The host pipeline found, the draw's checks.
    Pipeline,
    /// Indirect: the arguments' copies found and refreshed, the clamp recorded.
    Clamp,
    /// Textures picked by index (`State::early_textures`).
    Picked,
    /// Heaps, root signature, pipeline set.
    Binds,
    /// Descriptor tables: host slots made or mirrored, views made readable.
    Tables,
    /// Title memory by address (`State::memory_ready`).
    Memory,
    /// User data worked out and set.
    UserData,
    /// Root views: host copies found and refreshed, views or tables set.
    RootViews,
    /// Vertex and index buffers: host copies found and refreshed, set.
    Buffers,
    /// Render target and depth states, depth twins.
    Targets,
    /// The draw or dispatch call and what is noted after it.
    Call,
}

const PARTS: usize = 11;
const NAMES: [&str; PARTS] = [
    "pipeline",
    "indirect clamp",
    "picked textures",
    "heaps/root signature/pipeline",
    "tables",
    "memory by address",
    "user data",
    "root views",
    "vertex/index buffers",
    "target states",
    "the call",
];

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DRAW_PARTS").is_some_and(|v| v == "1"))
}

thread_local! {
    /// The last mark of the call under way (none: no call timed).
    static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
    /// This thread's nanoseconds by part, and calls, not yet added up.
    static MINE: Cell<([u64; PARTS], u64)> = const { Cell::new(([0; PARTS], 0)) };
}

/// Added up from all threads since the last line: nanoseconds by part, calls.
static ALL: [AtomicU64; PARTS + 1] = [const { AtomicU64::new(0) }; PARTS + 1];

/// A draw or dispatch begins.
#[inline]
pub(crate) fn begin() {
    if on() {
        LAST.set(Some(Instant::now()));
    }
}

/// The call under way has done `part` (since the mark before).
#[inline]
pub(crate) fn mark(part: Part) {
    if !on() {
        return;
    }
    let Some(last) = LAST.get() else { return };
    let now = Instant::now();
    let (mut ns, calls) = MINE.get();
    ns[part as usize] += (now - last).as_nanos() as u64;
    MINE.set((ns, calls));
    LAST.set(Some(now));
}

/// The call under way ends with `part`.
#[inline]
pub(crate) fn end(part: Part) {
    if !on() || LAST.get().is_none() {
        return;
    }
    mark(part);
    LAST.set(None);
    let (ns, calls) = MINE.get();
    let calls = calls + 1;
    // A few hundred at a time: no shared counter per call.
    if calls < 256 {
        MINE.set((ns, calls));
        return;
    }
    for (all, n) in ALL.iter().zip(ns) {
        all.fetch_add(n, Relaxed);
    }
    ALL[PARTS].fetch_add(calls, Relaxed);
    MINE.set(([0; PARTS], 0));
}

/// A draw or dispatch timed from here until this goes (its last part is
/// [`Part::Call`]).
pub(crate) struct Timed;

impl Timed {
    #[inline]
    pub(crate) fn new() -> Timed {
        begin();
        Timed
    }
}

impl Drop for Timed {
    #[inline]
    fn drop(&mut self) {
        end(Part::Call);
    }
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let ns: Vec<u64> = ALL.iter().map(|a| a.swap(0, Relaxed)).collect();
    let calls = ns[PARTS].max(1) as f64;
    let total: u64 = ns[..PARTS].iter().sum();
    let parts: Vec<String> =
        NAMES.iter().zip(&ns).map(|(name, &n)| format!("{name} {:.2}", n as f64 / calls / 1e3)).collect();
    tracing::info!(
        "draw and dispatch parts (VICTORY_DRAW_PARTS): {:.0} calls a frame, {:.2} us a call: {}",
        ns[PARTS] as f64 / frames,
        total as f64 / calls / 1e3,
        parts.join(", ")
    );
}

#[cfg(test)]
mod tests {
    /// Marks without a call begun change nothing (and off, nothing at all).
    #[test]
    fn marks_outside_a_call_are_not_counted() {
        super::mark(super::Part::Tables);
        super::end(super::Part::Call);
        assert!(super::MINE.get().0.iter().all(|&n| n == 0));
    }
}
