//! Frame pacing on the presenter's side: what a present waited for, as the
//! frame log writes it (`VICTORY_FRAME_LOG`), and the swap chain's frame
//! latency (`VICTORY_PRESENT_LATENCY`). `docs/frame-pacing.md` walks the
//! whole path from the title's present call to the host's.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObject;

/// What one presented frame waited for, in microseconds since the present
/// before it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Waits {
    /// In the title's present calls (with the submit line: waiting for the
    /// present before to be sent; without it: settling and the copy).
    pub(super) title_us: u64,
    /// Slept in the title's frame waits (`crate::frame::wait`).
    pub(super) paced_us: u64,
    /// The presenter sending the copy to the back buffer (a wait for the
    /// card included, where that submission waits).
    pub(super) submit_us: u64,
    /// The presenter in `IDXGISwapChain::Present`, and waiting for the
    /// frame-latency object with `VICTORY_PRESENT_LATENCY`.
    pub(super) swap_us: u64,
    /// Threads waiting for pipelines to be made (`VICTORY_PIPELINE_WAIT_MS`).
    pub(super) pipeline_us: u64,
    /// Host pipelines the driver compiled and shaders translated.
    pub(super) compiles: u64,
}

/// The counters behind [`Waits`] that run from the start: the title's
/// present calls, its frame waits, the pipeline waits (microseconds), and
/// the compiles.
pub(super) fn counters() -> [u64; 4] {
    let (paced, calls) = crate::frame::title_waits_us();
    let (pipeline, made) = crate::pipeline::frame_counts();
    let (_, translated) = crate::shader_cache::counts();
    [calls, paced, pipeline, made + translated]
}

/// [`Waits`] from the counters at the last present and now, and the
/// presenter's own two measures of this present.
pub(super) fn waits(before: [u64; 4], now: [u64; 4], submit_us: u64, swap_us: u64) -> Waits {
    let d = |i: usize| now[i].saturating_sub(before[i]);
    Waits { title_us: d(0), paced_us: d(1), submit_us, swap_us, pipeline_us: d(2), compiles: d(3) }
}

/// One line of the frame log: the three columns it always had (seconds
/// since the log began, ms since the last present, draws), then the waits
/// in ms (title's present calls, title's frame waits, the copy's submission,
/// the swap chain, pipeline waits) and the compiles. Readers that know the
/// first three only read those (the launcher's `FrameStats.TryParse`).
pub(super) fn line(at_s: f64, since_ms: f64, draws: u64, w: &Waits) -> String {
    let ms = |us: u64| us as f64 / 1000.0;
    format!(
        "{at_s:.3} {since_ms:.2} {draws} {:.2} {:.2} {:.2} {:.2} {:.2} {}",
        ms(w.title_us),
        ms(w.paced_us),
        ms(w.submit_us),
        ms(w.swap_us),
        ms(w.pipeline_us),
        w.compiles
    )
}

/// `VICTORY_PRESENT_LATENCY=1|2|3`: the swap chain is made with a
/// frame-latency object, at most that many frames are queued for the
/// display, and the presenter waits for the object itself
/// ([`Latency::wait`]) instead of blocking inside `Present`. Off (unset):
/// the swap chain as before, whose `Present` blocks when the system's
/// default three frames are queued or no back buffer is free.
///
/// Why: the presenter is the thread that sends the title's submissions
/// (with the submit line), so time it stands in `Present` is time no
/// submission is prepared. The layer bounds how far the title runs ahead on
/// its own (the line holds one present, `VICTORY_SUBMIT_FRAMES`), so a
/// frame that is already late gains nothing from waiting again for the
/// display. Sync interval and the frame interval the title set stay as
/// they are: nothing is uncapped.
pub(super) fn latency() -> Option<u32> {
    static N: OnceLock<Option<u32>> = OnceLock::new();
    *N.get_or_init(|| {
        let asked = std::env::var("VICTORY_PRESENT_LATENCY").ok()?;
        match asked.trim().parse::<u32>() {
            Ok(n @ 1..=3) => {
                tracing::warn!(frames = n, "the swap chain waits on its frame-latency object (VICTORY_PRESENT_LATENCY)");
                Some(n)
            }
            _ => {
                tracing::warn!(asked, "VICTORY_PRESENT_LATENCY takes 1, 2 or 3: the swap chain stays as it was");
                None
            }
        }
    })
}

/// The swap chain's buffers: `default`, or one more than the latency asked
/// for, if that is more (a frame shown, the others queued).
pub(super) fn buffers(default: u32) -> u32 {
    latency().map_or(default, |n| default.max(n + 1))
}

/// The swap chain's flags: none, or the frame-latency object's.
pub(super) fn flags() -> DXGI_SWAP_CHAIN_FLAG {
    match latency() {
        Some(_) => DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
        None => DXGI_SWAP_CHAIN_FLAG(0),
    }
}

/// Presents in a row that found the latency object unsignalled (the display
/// had the most frames queued) before the presenter waits for it in earnest.
const MISSES: u32 = 2;
/// The longest that wait is.
const HELD: Duration = Duration::from_millis(100);

/// How long a present waits for the latency object, `since_last` after the
/// present before, with the title's frame interval `interval`, after
/// `misses` presents in a row whose wait timed out: what is left of the
/// frame interval, nothing once the frame is late (that time is lost
/// already; waiting longer only delays the submissions behind it). Once the
/// object stayed unsignalled [`MISSES`] times the display is that many
/// frames behind, and the wait is up to [`HELD`], so the queue stays bounded.
pub(super) fn latency_timeout(since_last: Duration, interval: Duration, misses: u32) -> Duration {
    if misses >= MISSES { HELD } else { interval.saturating_sub(since_last) }
}

/// The frame-latency object of a swap chain made with [`flags`].
pub(super) struct Latency {
    object: HANDLE,
    misses: u32,
    last: Option<Instant>,
}

// SAFETY: a waitable handle, used under the presenter's lock.
unsafe impl Send for Latency {}

impl Latency {
    /// Sets the swap chain's latency to `frames` and takes its object.
    /// (The object lives as long as the swap chain, which is never let go.)
    pub(super) fn new(swap_chain: &IDXGISwapChain3, frames: u32) -> Option<Latency> {
        // SAFETY: our swap chain, made with the waitable flag.
        unsafe {
            if let Err(e) = swap_chain.SetMaximumFrameLatency(frames) {
                tracing::warn!(error = %e, "the swap chain's frame latency was not set");
                return None;
            }
            let object = swap_chain.GetFrameLatencyWaitableObject();
            if object.is_invalid() {
                tracing::warn!("the swap chain has no frame-latency object");
                return None;
            }
            Some(Latency { object, misses: 0, last: None })
        }
    }

    /// Waits for the object before a present (see [`latency_timeout`]);
    /// the microseconds it took.
    pub(super) fn wait(&mut self, interval: Duration) -> u64 {
        let began = Instant::now();
        let since = self.last.map_or(Duration::MAX, |last| began - last);
        let timeout = latency_timeout(since, interval, self.misses);
        // SAFETY: the swap chain's own waitable object.
        let woke = unsafe { WaitForSingleObject(self.object, timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32) };
        self.misses = if woke == WAIT_OBJECT_0 { 0 } else { self.misses + 1 };
        self.last = Some(Instant::now());
        began.elapsed().as_micros() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_three_columns_are_as_before() {
        let w = Waits { title_us: 31_400, paced_us: 0, submit_us: 620, swap_us: 50, pipeline_us: 12_000, compiles: 3 };
        let text = line(12.345, 55.2, 6012, &w);
        assert_eq!(text, "12.345 55.20 6012 31.40 0.00 0.62 0.05 12.00 3");
        // The old line, exactly, at its start.
        assert!(text.starts_with(&format!("{:.3} {:.2} {}", 12.345, 55.2, 6012)));
        assert_eq!(text.split(' ').count(), 9);
    }

    #[test]
    fn waits_are_counted_since_the_last_present() {
        let w = waits([100, 200, 300, 4], [1_100, 200, 2_300, 6], 7, 8);
        assert_eq!(w, Waits { title_us: 1_000, paced_us: 0, submit_us: 7, swap_us: 8, pipeline_us: 2_000, compiles: 2 });
    }

    /// On a fake clock: the time since the last present decides the wait.
    #[test]
    fn a_late_present_does_not_wait_for_the_display() {
        let interval = Duration::from_micros(16_667);
        let ms = Duration::from_millis;
        // Early: up to what is left of the interval.
        assert_eq!(latency_timeout(ms(10), interval, 0), Duration::from_micros(6_667));
        // Late (the title at 18 fps, 55 ms a frame): not at all.
        assert_eq!(latency_timeout(ms(55), interval, 0), Duration::ZERO);
        assert_eq!(latency_timeout(Duration::MAX, interval, 1), Duration::ZERO);
        // The display stayed behind twice in a row: waited for, but bounded.
        assert_eq!(latency_timeout(ms(55), interval, 2), HELD);
        assert_eq!(latency_timeout(ms(1), interval, 9), HELD);
        // A run of presents on the fake clock: 10 ms frames are held to the
        // interval, 55 ms frames never wait.
        let run = |frame: Duration| {
            let mut waited = Duration::ZERO;
            for _ in 0..10 {
                waited += latency_timeout(frame, interval, 0);
            }
            waited
        };
        assert_eq!(run(ms(10)), Duration::from_micros(66_670));
        assert_eq!(run(ms(55)), Duration::ZERO);
    }

    #[test]
    fn off_the_swap_chain_is_as_before() {
        // (The switch is not set in the tests.)
        assert_eq!(latency(), None);
        assert_eq!(buffers(2), 2);
        assert_eq!(flags(), DXGI_SWAP_CHAIN_FLAG(0));
    }
}
