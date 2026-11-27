//! Diagnostics: how long what the title is told (fence values, bottom-of-pipe
//! writes) waits behind the relay ([`relay`]), from the moment the card ran
//! the flight ahead of it to the moment it is told, cut where the landing
//! that tells it moves on: the card done, the landing begun, the fetch of
//! the card copies' pieces in, the merge done, told.
//!
//! Why: in stage 2 a fence is set only after its flight's landing
//! ([`late_signal`]). A title thread that spins on a fence (a profile found
//! one spending half its time so) waits for the whole of it. The relay line
//! says how long the relay works; this says how long each tell waits, and
//! which part of the landing it waits for.
//!
//! What it costs: a thread that wakes once per value the card reaches on the
//! host's fence, a clock read per landing step and per tell, and a store at
//! the first time the title asks a fence for its value or its address after
//! it was set (`crate::fence`). Only in builds with the diagnostics feature.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::*;

/// Microseconds since the first call (0 is kept for "never").
pub(crate) fn now_us() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64 + 1
}

/// Values the card reached on the host's fence, and when: every value up to
/// the first was reached by the second. In order.
static REACHED: Mutex<VecDeque<(u64, u64)>> = Mutex::new(VecDeque::new());

/// The most kept (some hundred flights a second; the relay looks them up).
const KEPT: usize = 4096;

/// When the card reached `value`, of the times [`REACHED`] holds: the first
/// that covers it. Values below it are dropped (the relay lands in order).
fn reached(all: &mut VecDeque<(u64, u64)>, value: u64) -> Option<u64> {
    while all.front().is_some_and(|r| r.0 < value) {
        all.pop_front();
    }
    all.front().map(|r| r.1)
}

/// The card reached `value` at `us`.
fn note_reached(all: &mut VecDeque<(u64, u64)>, value: u64, us: u64) {
    if all.back().is_some_and(|r| r.0 >= value) {
        return;
    }
    if all.len() >= KEPT {
        all.pop_front();
    }
    all.push_back((value, us));
}

/// The thread that notes when the card reaches each value of `fence` (the
/// host's): one wait per value, so a flight's end is known within a wake-up
/// whatever the relay is doing then.
pub(super) fn watch(fence: ID3D12Fence) {
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        return;
    };
    // SAFETY: a plain query.
    let mut seen = unsafe { fence.GetCompletedValue() };
    loop {
        // SAFETY: the host's fence and this thread's event.
        let set = unsafe { fence.SetEventOnCompletion(seen + 1, event) };
        if set.is_err() {
            return;
        }
        // SAFETY: as above.
        unsafe { WaitForSingleObject(event, INFINITE) };
        // SAFETY: a plain query.
        let now = unsafe { fence.GetCompletedValue() };
        if now == u64::MAX {
            // The device is gone.
            return;
        }
        let us = now_us();
        note_reached(&mut REACHED.lock().unwrap_or_else(|e| e.into_inner()), now, us);
        seen = now.max(seen + 1);
    }
}

/// Starts [`watch`], once, with [`relay`].
pub(super) fn start(host: &Host) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let fence = host.fence.clone();
        if let Err(e) = crate::named::spawn("victory-tell-watch", move || watch(fence)) {
            tracing::warn!(error = %e, "no thread to note when the card reaches its values: tells are timed from their landing");
        }
    });
}

/// The times of one part of a landing (a landing, or with [`merge_spread`]
/// one of its segments), on the relay's thread.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Times {
    pub(super) begun: u64,
    pub(super) fetched: u64,
    pub(super) merged: u64,
}

thread_local! {
    static TIMES: std::cell::Cell<Times> = const { std::cell::Cell::new(Times { begun: 0, fetched: 0, merged: 0 }) };
}

/// A landing begins.
pub(super) fn begun() {
    let now = now_us();
    TIMES.with(|t| t.set(Times { begun: now, fetched: now, merged: now }));
}

/// The fetch (or the part of it the next tells wait for) is in.
pub(super) fn fetched() {
    let now = now_us();
    TIMES.with(|t| t.set(Times { fetched: now, ..t.get() }));
}

/// What the next tells wait for is merged (and its ranges done with).
pub(super) fn merged() {
    let now = now_us();
    TIMES.with(|t| {
        let was = t.get();
        // No fetch since the landing began: the fetch took no time.
        let fetched = if was.fetched < was.begun { was.begun } else { was.fetched };
        t.set(Times { fetched, merged: now, ..was });
    });
}

/// One thing told, for [`Tally`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Told {
    /// When the card ran the flight ahead of it (0: not known).
    pub(super) card: u64,
    pub(super) times: Times,
    pub(super) told: u64,
    /// The fence (its address), or 0 for a bottom-of-pipe write.
    pub(super) fence: usize,
    /// When the title first asked that fence since it was last set (0:
    /// it did not).
    pub(super) asked: u64,
    /// The flight ahead of it, counted from 1 in its landing, and the
    /// landing's flights.
    pub(super) behind: u32,
    pub(super) flights: u32,
}

/// Sums since the last line.
#[derive(Default, Debug, PartialEq)]
pub(super) struct Tally {
    pub(super) writes: u64,
    pub(super) signals: u64,
    /// Fence signals the title had asked for, and of those the sums: card
    /// to told, card to begun, begun to fetched, fetched to merged, merged
    /// to told, the title's own wait (asked to told), the most of card to
    /// told; then the flight behind and the landing's flights.
    pub(super) asked: u64,
    pub(super) sums: [u64; 7],
    pub(super) behind: u64,
    pub(super) flights: u64,
    /// The same five parts (no title wait, no most) over every tell.
    pub(super) all: [u64; 5],
    /// Per fence asked for: tells, card to told, the flights behind.
    pub(super) fences: std::collections::HashMap<usize, (u64, u64, u64)>,
}

impl Tally {
    pub(super) fn add(&mut self, t: Told) {
        let begun = t.times.begun;
        // The card's time, if known and not after the landing began (the
        // watching thread may wake after the relay).
        let card = if t.card == 0 || t.card > begun { begun } else { t.card };
        let fetched = t.times.fetched.clamp(begun, t.told);
        let merged = t.times.merged.clamp(fetched, t.told);
        let parts = [t.told - card, begun - card, fetched - begun, merged - fetched, t.told - merged];
        for (sum, part) in self.all.iter_mut().zip(parts) {
            *sum += part;
        }
        if t.fence == 0 {
            self.writes += 1;
            return;
        }
        self.signals += 1;
        if t.asked == 0 {
            return;
        }
        self.asked += 1;
        for (sum, part) in self.sums.iter_mut().zip(parts) {
            *sum += part;
        }
        self.sums[5] += t.told.saturating_sub(t.asked);
        self.sums[6] = self.sums[6].max(parts[0]);
        self.behind += u64::from(t.behind);
        self.flights += u64::from(t.flights);
        let fence = self.fences.entry(t.fence).or_default();
        *fence = (fence.0 + 1, fence.1 + parts[0], fence.2 + u64::from(t.behind));
    }
}

static TALLY: Mutex<Option<Tally>> = Mutex::new(None);

/// A tell of the landing under way: `fence` (0: a bottom-of-pipe write)
/// behind the card's `value` (the flight ahead), the `behind`th flight of
/// `flights`; `asked` as [`Told::asked`].
pub(super) fn told(value: u64, fence: usize, asked: u64, behind: u32, flights: u32) {
    let told = now_us();
    // (No flight ahead in the landing: from the landing's beginning.)
    let card = match value {
        0 => 0,
        _ => reached(&mut REACHED.lock().unwrap_or_else(|e| e.into_inner()), value).unwrap_or(0),
    };
    let times = TIMES.with(std::cell::Cell::get);
    TALLY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .add(Told { card, times, told, fence, asked, behind, flights });
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    let Some(t) = TALLY.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let tells = (t.writes + t.signals).max(1) as f64;
    let ms = |us: u64, n: f64| us as f64 / 1000.0 / n.max(1.0);
    let a = t.asked as f64;
    let mut fences: Vec<(usize, (u64, u64, u64))> = t.fences.into_iter().collect();
    fences.sort_unstable_by_key(|f| std::cmp::Reverse(f.1.0));
    let fences: Vec<String> = fences
        .into_iter()
        .take(3)
        .map(|(at, (n, us, behind))| {
            format!("{at:#x} {:.1}x {:.2} ms behind flight {:.1}", n as f64 / frames, ms(us, n as f64), behind as f64 / n as f64)
        })
        .collect();
    tracing::info!(
        "the title is told per frame: {:.1} fence values and {:.1} bottom-of-pipe writes; {:.1} of the fences were asked for by \
         the title since their last set: card done to told {:.2} ms each (most {:.2}): card done to landing begun {:.2}, to the \
         fetch in {:.2}, to merged {:.2}, to told {:.2}; the title asked {:.2} ms before told; behind flight {:.1} of {:.1} in \
         their landing; every tell: {:.2} ms ({:.2} + {:.2} + {:.2} + {:.2}); fences asked most: {}",
        t.signals as f64 / frames,
        t.writes as f64 / frames,
        a / frames,
        ms(t.sums[0], a),
        t.sums[6] as f64 / 1000.0,
        ms(t.sums[1], a),
        ms(t.sums[2], a),
        ms(t.sums[3], a),
        ms(t.sums[4], a),
        ms(t.sums[5], a),
        t.behind as f64 / a.max(1.0),
        t.flights as f64 / a.max(1.0),
        ms(t.all[0], tells),
        ms(t.all[1], tells),
        ms(t.all[2], tells),
        ms(t.all[3], tells),
        ms(t.all[4], tells),
        if fences.is_empty() { "none".to_string() } else { fences.join(", ") },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value is reached at the first time noted for it or a later one;
    /// looking up drops what is below it.
    #[test]
    fn a_value_is_reached_when_first_covered() {
        let mut all = VecDeque::new();
        note_reached(&mut all, 3, 100);
        note_reached(&mut all, 3, 150);
        note_reached(&mut all, 7, 200);
        assert_eq!(all.len(), 2);
        assert_eq!(reached(&mut all, 1), Some(100));
        assert_eq!(reached(&mut all, 3), Some(100));
        assert_eq!(reached(&mut all, 4), Some(200));
        assert_eq!(all.len(), 1);
        assert_eq!(reached(&mut all, 8), None);
        for v in 0..KEPT as u64 + 10 {
            note_reached(&mut all, v + 10, v);
        }
        assert_eq!(all.len(), KEPT);
    }

    /// The parts add up to the whole, a card time after the landing began
    /// counts from the landing, and only fences asked for count as waited.
    #[test]
    fn the_parts_add_up() {
        let mut t = Tally::default();
        let times = Times { begun: 1000, fetched: 1600, merged: 2500 };
        t.add(Told { card: 400, times, told: 2600, fence: 0x10, asked: 900, behind: 2, flights: 3 });
        assert_eq!(t.sums[..5], [2200, 600, 600, 900, 100]);
        assert_eq!(t.sums[1..5].iter().sum::<u64>(), t.sums[0]);
        assert_eq!((t.sums[5], t.sums[6], t.asked, t.behind, t.flights), (1700, 2200, 1, 2, 3));
        t.add(Told { card: 5000, times, told: 2600, fence: 0x10, asked: 0, behind: 1, flights: 1 });
        t.add(Told { card: 0, times, told: 2700, fence: 0, asked: 0, behind: 1, flights: 1 });
        assert_eq!((t.asked, t.signals, t.writes), (1, 2, 1));
        assert_eq!(t.all[0], 2200 + 1600 + 1700);
        assert_eq!(t.fences[&0x10], (1, 2200, 2));
    }
}
