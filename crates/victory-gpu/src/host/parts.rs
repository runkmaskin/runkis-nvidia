//! Where the card's time goes, by part, in every build (`VICTORY_CARD_PARTS=1`;
//! off by default).
//!
//! The F3 panel's "GPU BUSY" is the frame's time less the stretches in which
//! nothing of ours was queued (`card_clock_line`): a submission waiting for
//! its turn behind work of another queue, or the system's time to start it
//! and to report it done, counts as busy there. Here the card's own
//! timestamps stand between the lists of every submission, so each list's
//! time on the card is known, and summed by what it is:
//!
//! - the title's lists;
//! - the preludes (`VICTORY_PRELUDE_UPLOADS`: uploads ahead of a list);
//! - our list of steps done on the card (`VICTORY_CARD_STEPS`);
//! - the compare passes after a submission's lists, over the copies in the
//!   card's memory that its shaders may write (`CARD_COMPARE`);
//! - the list that sends what changed up to those copies and hashes the
//!   pieces it sent again, run in front of a submission;
//! - other lists of ours (the diagnostics' own stamps).
//!
//! A list's time is from the stamp in front of it to the one behind: the
//! card may begin a list before the one ahead of it is done, so for lists
//! that follow one another closely the split is a share, while the sum over
//! a submission is the time from its first list's start to its last list's
//! end, whatever overlapped. Stamps are lists of their own, made once: one
//! timestamp and its way into memory the PC reads. They are read when they
//! are used again, some ten frames later.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::Host;

/// Stamps made, used in turn (a frame uses some 300 to 400).
const STAMPS: usize = 4096;

/// What a list in a submission is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(super) enum Kind {
    Title = 0,
    Prelude = 1,
    Steps = 2,
    Compare = 3,
    Sync = 4,
    Other = 5,
}

const KINDS: usize = 6;

/// A stamp with nothing behind it in its submission: the last one.
const END: u8 = u8::MAX;
/// A stamp not in use.
const UNUSED: u8 = u8::MAX - 1;

pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_CARD_PARTS").is_some_and(|v| v == "1");
        if on {
            tracing::info!("the card's time is stamped between the lists of every submission (VICTORY_CARD_PARTS=1)");
        }
        on
    })
}

/// What the stamps read so far add up to, in card ticks.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Sums {
    ticks: [u64; KINDS],
    lists: [u64; KINDS],
    longest: [u64; KINDS],
    /// Submissions read, and the ticks from the first stamp of each to its
    /// last.
    submissions: u64,
    spanned: u64,
}

/// Which stamps hold what, apart from the card's objects. A submission of
/// `n` lists takes `n + 1` stamps in a row; they are given out in order
/// round the ring, so the old submissions in front of `next` are read, the
/// oldest first, before their stamps are used again.
struct Ring {
    /// Per stamp, what the list behind it is (`Kind`), [`END`] or [`UNUSED`].
    behind: Vec<u8>,
    next: usize,
    /// The first stamp of the oldest submission not read yet (or an unused one).
    oldest: usize,
    sums: Sums,
}

impl Ring {
    fn new() -> Ring {
        Ring {
            behind: vec![UNUSED; STAMPS],
            next: 0,
            oldest: 0,
            sums: Sums::default(),
        }
    }

    /// Reads the old submission at [`Ring::oldest`] (or passes over a stamp
    /// not in use) and frees its stamps. `tick` reads a stamp's ticks and
    /// clears them.
    fn read_oldest(&mut self, tick: &mut dyn FnMut(usize) -> u64) {
        let start = self.oldest;
        if self.behind[start] == UNUSED {
            self.oldest = start + 1;
            return;
        }
        let mut end = start;
        while end + 1 < STAMPS && self.behind[end] != END {
            end += 1;
        }
        let ticks: Vec<u64> = (start..=end).map(&mut *tick).collect();
        let whole = self.behind[end] == END && ticks.iter().all(|&t| t != 0);
        if whole {
            for (i, pair) in ticks.windows(2).enumerate() {
                let k = usize::from(self.behind[start + i]);
                if k < KINDS && pair[1] >= pair[0] {
                    let took = pair[1] - pair[0];
                    self.sums.ticks[k] += took;
                    self.sums.lists[k] += 1;
                    self.sums.longest[k] = self.sums.longest[k].max(took);
                }
            }
            if ticks[ticks.len() - 1] >= ticks[0] {
                self.sums.submissions += 1;
                self.sums.spanned += ticks[ticks.len() - 1] - ticks[0];
            }
        }
        self.behind[start..=end].fill(UNUSED);
        self.oldest = end + 1;
    }

    /// `n + 1` stamps in a row, for a submission of `n` lists of these
    /// kinds: where they begin.
    fn take(&mut self, kinds: &[Kind], tick: &mut dyn FnMut(usize) -> u64) -> Option<usize> {
        let want = kinds.len() + 1;
        if want > STAMPS / 4 {
            return None;
        }
        if self.next + want > STAMPS {
            // What is left at the ring's end is older than what is at its
            // start: read now, and the start is next.
            while self.oldest < STAMPS {
                self.read_oldest(tick);
            }
            self.next = 0;
            self.oldest = 0;
        }
        let at = self.next;
        while self.oldest < at + want {
            self.read_oldest(tick);
        }
        for (i, kind) in kinds.iter().enumerate() {
            self.behind[at + i] = *kind as u8;
        }
        self.behind[at + kinds.len()] = END;
        self.next = at + want;
        Some(at)
    }
}

struct Parts {
    _allocator: ID3D12CommandAllocator,
    _heap: ID3D12QueryHeap,
    _back: ID3D12Resource,
    /// The mapped results, a tick count per stamp.
    data: usize,
    lists: Vec<ID3D12CommandList>,
    ticks_per_ms: f64,
    ring: Ring,
}

// SAFETY: free-threaded objects and a mapping only read behind a lock.
unsafe impl Send for Parts {}

static PARTS: Mutex<Option<Option<Parts>>> = Mutex::new(None);

impl Host {
    fn make_parts(&self) -> windows::core::Result<Parts> {
        let device = &self.device;
        // SAFETY: plain object creation; lists of one allocator recorded one
        // after the other.
        unsafe {
            let mut heap: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: STAMPS as u32,
                    NodeMask: 0,
                },
                &mut heap,
            )?;
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            let back = self.read_back_buffer(8 * STAMPS as u64).ok_or_else(windows::core::Error::empty)?;
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data))?;
            std::ptr::write_bytes(data.cast::<u8>(), 0, 8 * STAMPS);
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let mut lists = Vec::with_capacity(STAMPS);
            for i in 0..STAMPS as u32 {
                let list: ID3D12GraphicsCommandList =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i);
                list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i, 1, &back, u64::from(i) * 8);
                list.Close()?;
                lists.push(list.cast()?);
            }
            Ok(Parts {
                _allocator: allocator,
                _heap: heap,
                _back: back,
                data: data as usize,
                lists,
                ticks_per_ms: self.queue.GetTimestampFrequency()? as f64 / 1000.0,
                ring: Ring::new(),
            })
        }
    }

    /// `lists` (of the kinds `kinds`, one each) with a stamp in front of each
    /// and one behind the last; `firsts`, indices into `lists`, moved with
    /// them (each onto the stamp in front of its list). Unchanged when the
    /// stamps are off or none are free.
    pub(super) fn stamp_parts(
        &self,
        lists: Vec<Option<ID3D12CommandList>>,
        kinds: &[Kind],
        firsts: &mut [usize],
    ) -> Vec<Option<ID3D12CommandList>> {
        if !on() || lists.is_empty() || kinds.len() != lists.len() {
            return lists;
        }
        let mut made = PARTS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(parts) = made
            .get_or_insert_with(|| {
                self.make_parts()
                    .inspect_err(|e| tracing::warn!(error = %e, "no stamps between lists (VICTORY_CARD_PARTS)"))
                    .ok()
            })
            .as_mut()
        else {
            return lists;
        };
        let data = parts.data as *mut u64;
        // SAFETY: inside the mapping (`STAMPS` words), of stamps the card
        // wrote some ten frames ago; written by it in whole words.
        let mut tick = |i: usize| unsafe {
            let at = data.add(i);
            let t = at.read_volatile();
            at.write_volatile(0);
            t
        };
        let Some(at) = parts.ring.take(kinds, &mut tick) else {
            return lists;
        };
        let mut out = Vec::with_capacity(2 * lists.len() + 1);
        for (i, list) in lists.into_iter().enumerate() {
            out.push(Some(parts.lists[at + i].clone()));
            out.push(list);
        }
        out.push(Some(parts.lists[at + kinds.len()].clone()));
        for first in firsts {
            *first *= 2;
        }
        out
    }
}

/// For the frame-rate line: the parts read since the last one, per frame
/// (they are of some frames ago).
pub(super) fn line(frames: f64) {
    if !on() {
        return;
    }
    let mut made = PARTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(parts)) = made.as_mut() else {
        return;
    };
    let s = std::mem::take(&mut parts.ring.sums);
    let per = parts.ticks_per_ms;
    let ms = |ticks: u64| ticks as f64 / per / frames;
    let longest = |ticks: u64| ticks as f64 / per;
    let n = |kind: Kind| s.lists[kind as usize] as f64 / frames;
    let t = |kind: Kind| ms(s.ticks[kind as usize]);
    let all: u64 = s.ticks.iter().sum();
    tracing::info!(
        "the card's time by part per frame (VICTORY_CARD_PARTS): the title's lists {:.1} ms ({:.0} lists, the longest {:.1} ms), \
         preludes {:.1} ms ({:.0}), steps on the card {:.1} ms ({:.0}), compares after the lists {:.1} ms ({:.0}, the longest {:.1}), \
         sends and hashes ahead {:.1} ms ({:.0}, the longest {:.1}), other {:.1} ms; all lists {:.1} ms; {:.1} submissions spanning {:.1} ms",
        t(Kind::Title),
        n(Kind::Title),
        longest(s.longest[Kind::Title as usize]),
        t(Kind::Prelude),
        n(Kind::Prelude),
        t(Kind::Steps),
        n(Kind::Steps),
        t(Kind::Compare),
        n(Kind::Compare),
        longest(s.longest[Kind::Compare as usize]),
        t(Kind::Sync),
        n(Kind::Sync),
        longest(s.longest[Kind::Sync as usize]),
        t(Kind::Other),
        ms(all),
        s.submissions as f64 / frames,
        ms(s.spanned),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes stamps for `kinds`, and writes `ticks` where the card would.
    fn submit(ring: &mut Ring, card: &mut [u64], kinds: &[Kind], ticks: &[u64]) -> usize {
        let at = ring.take(kinds, &mut |i| std::mem::take(&mut card[i])).unwrap();
        card[at..at + ticks.len()].copy_from_slice(ticks);
        at
    }

    /// Round the ring with submissions whose stamps the card never wrote.
    fn round(ring: &mut Ring, card: &mut [u64]) {
        let kinds = [Kind::Other; 99];
        let start = ring.next;
        loop {
            let at = ring.take(&kinds, &mut |i| std::mem::take(&mut card[i])).unwrap();
            if at < start {
                break;
            }
        }
    }

    #[test]
    fn a_list_is_timed_from_its_stamp_to_the_next_by_its_kind() {
        let (mut ring, mut card) = (Ring::new(), vec![0u64; STAMPS]);
        let at = submit(&mut ring, &mut card, &[Kind::Sync, Kind::Title, Kind::Compare], &[100, 110, 150, 155]);
        assert_eq!(at, 0);
        assert_eq!(submit(&mut ring, &mut card, &[Kind::Title], &[200, 230]), 4);
        assert_eq!(ring.sums, Sums::default());
        round(&mut ring, &mut card);
        let s = ring.sums;
        assert_eq!(s.ticks[Kind::Sync as usize], 10);
        assert_eq!(s.ticks[Kind::Title as usize], 40 + 30);
        assert_eq!((s.lists[Kind::Title as usize], s.longest[Kind::Title as usize]), (2, 40));
        assert_eq!(s.ticks[Kind::Compare as usize], 5);
        assert_eq!((s.submissions, s.spanned), (2, 55 + 30));
        // The unwritten ones of the round count nothing.
        assert_eq!(s.ticks[Kind::Other as usize], 0);
    }

    #[test]
    fn submissions_at_the_end_of_the_ring_are_read_before_its_start_is_used_again() {
        let (mut ring, mut card) = (Ring::new(), vec![0u64; STAMPS]);
        // Up to just short of the end: 4 x 1000 stamps, then 90.
        for _ in 0..4 {
            ring.take(&[Kind::Other; 999], &mut |i| std::mem::take(&mut card[i])).unwrap();
        }
        ring.take(&[Kind::Other; 89], &mut |i| std::mem::take(&mut card[i])).unwrap();
        let near_end = submit(&mut ring, &mut card, &[Kind::Prelude, Kind::Title], &[1, 3, 8]);
        assert_eq!(near_end, 4090);
        // Does not fit: goes to the start, reading the end first.
        submit(&mut ring, &mut card, &[Kind::Title; 20], &[0; 21]);
        assert_eq!(ring.sums, Sums::default(), "the end was written in this round");
        round(&mut ring, &mut card);
        round(&mut ring, &mut card);
        assert_eq!(ring.sums.ticks[Kind::Prelude as usize], 2);
        assert_eq!(ring.sums.ticks[Kind::Title as usize], 5);
        assert_eq!(ring.sums.submissions, 1);
    }

    #[test]
    fn a_submission_too_big_for_a_quarter_of_the_ring_is_not_stamped() {
        let mut ring = Ring::new();
        assert!(ring.take(&[Kind::Title; STAMPS / 4], &mut |_| 0).is_none());
        assert!(ring.take(&[Kind::Title; 10], &mut |_| 0).is_some());
    }
}
