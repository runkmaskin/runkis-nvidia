//! The fetch inside the flight (`VICTORY_FETCH_IN_FLIGHT=1`; off by default:
//! the relay fetches what the compare flagged with a list of its own once
//! the card has run the flight, as before).
//!
//! With the relay a flight's landing waits for a fetch: a list of copies of
//! the flagged pieces into memory the PC reads, sent once the card has run
//! the flight, and a wait for its fence. On the 7800X3D (2026-11-27, measured
//! in game) that wait was 7.2 ms of the relay's 18.8 ms landing a frame, and
//! the title's fences wait for the landing.
//!
//! What changes: right behind the compare, in the same list of the flight,
//! a pass of ours copies every piece the compare flagged into a ring of
//! memory the PC reads ([`SNAP`]): one group per piece, the first thread
//! takes a place in the flight's room of the ring with an atomic counter
//! and notes it beside the compare's result (`where`), all 64 copy the piece.
//! When the flight's fence is reached the pieces are in that memory: the
//! landing merges them from there, with no list, no fence and no wait.
//!
//! What the bytes are: the card copy's bytes right after the flight's
//! compare hashed them, the same bytes a fetch sent then would bring, so the
//! merge leaves out the title's bytes sent up after the flight's own sends
//! (the plan's `sent_as`), as the fetch on the copy queue does.
//!
//! When the old fetch is used instead, for a whole landing (never a mix in
//! one landing, so the order of merges stays the old one):
//! - a piece is wanted that the last plan of the landing that wants it has
//!   no place for (its room was full, or none was given, or its range was
//!   not compared: the results area was full);
//! - an old fetch since may have brought a later flight's bytes of the
//!   piece ([`Newer`]): merging the older snapshot would put them back.
//!
//! Each flight's room is sized from what flights of the same shape flagged
//! last time ([`Snap::plan`]); a room too small costs the old fetch, never a
//! byte. Not with the check of torn pieces (`VICTORY_FETCH_CHECK`): a piece
//! copied inside its flight cannot be torn.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use super::*;

/// See the file.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_FETCH_IN_FLIGHT").is_some_and(|v| v == "1") && relay() && crate::land::on() && !torn::on()
    })
}

/// The ring's bytes (`VICTORY_FETCH_IN_FLIGHT_MB`, 64): memory the PC reads,
/// some 1,000 pieces. A frame lands some 340 (22 MB in 6,700 pages).
fn ring_bytes() -> u64 {
    static MB: OnceLock<u64> = OnceLock::new();
    let mb = *MB.get_or_init(|| {
        std::env::var("VICTORY_FETCH_IN_FLIGHT_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(64u64).clamp(4, 2048)
    });
    mb << 20
}

/// A place in the ring: a piece of 64 KiB, widened to 16 bytes on both ends
/// as the compare widens it, and kept a multiple of 256.
pub(crate) const SLOT: u64 = (64 << 10) + 256;

/// Counters of rooms in the air (a flight's room has one).
const COUNTERS: u64 = 4096;

/// The pass after the compare: for each piece of a declared range, the
/// compare's flag, and if set a place in the room (`base`, `room` places, of
/// `slots` in the ring) for the piece's bytes, which are copied there.
/// `where` gets, at the compare's result's place, the place in the room plus
/// one, or 0. `mode` 0 sets the room's counter to 0 (one thread).
pub(crate) const SNAP: &str = r#"
#define RS "RootConstants(num32BitConstants=10, b0), SRV(t0), UAV(u0), UAV(u1), UAV(u2), UAV(u3)"
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes (a multiple of 64 KiB)
    uint pieces;
    uint out_at;   // the first piece's dword in `differs` and `places`, in bytes
    uint base;     // the room's first place in the ring
    uint room;     // the room's places
    uint counter;  // the room's counter, in bytes
    uint slots;    // the ring's places
    uint mode;     // 0: the counter is set to 0; else the pieces are copied
};
ByteAddressBuffer now : register(t0);
RWByteAddressBuffer differs : register(u0);
RWByteAddressBuffer places : register(u1);
RWByteAddressBuffer ring : register(u2);
RWByteAddressBuffer counters : register(u3);
groupshared uint taken;
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_GroupID, uint thread : SV_GroupIndex) {
    if (mode == 0u) {
        if (id.x == 0u && thread == 0u) counters.Store(counter, 0u);
        return;
    }
    uint piece = first + id.x;
    uint lo = piece * 65536u;
    lo = lo > skew ? lo - skew : 0u;
    uint hi = min((piece + 1u) * 65536u - skew, size);
    lo = lo & ~15u;
    hi = (hi + 15u) & ~15u;
    if (thread == 0u) {
        uint place = 0u;
        if ((differs.Load(out_at + id.x * 4u) & 1u) != 0u) {
            uint n;
            counters.InterlockedAdd(counter, 1u, n);
            place = n < room ? n + 1u : 0u;
        }
        places.Store(out_at + id.x * 4u, place);
        taken = place;
    }
    GroupMemoryBarrierWithGroupSync();
    if (taken == 0u) return;
    uint to = ((base + taken - 1u) % slots) * 65792u;
    [loop]
    for (uint at = lo + thread * 16u; at < hi; at += 1024u) {
        ring.Store4(to + (at - lo), now.Load4(at));
    }
}
"#;

/// Where [`SNAP`] put the bytes of piece `piece` of a copy whose piece 0
/// begins `skew` bytes before it: the offset in the copy its place begins at.
pub(crate) fn place_begins(piece: u32, skew: u32) -> usize {
    let lo = u64::from(piece) * (64 << 10);
    let lo = if lo > u64::from(skew) { lo - u64::from(skew) } else { 0 };
    (lo & !15) as usize
}

/// A flight's room in the ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Room {
    pub(super) id: u64,
    /// The first place (counted since the start, the ring's place modulo
    /// its places), and the places.
    pub(super) base: u64,
    pub(super) room: u64,
    /// What flights of its shape flagged ([`Snap::plan`]).
    shape: u64,
    /// The first results' places of the ranges the pass was recorded for
    /// ([`Snap::record`]): the places of no other range are its.
    pub(super) ranges: Vec<u32>,
}

/// The ring's places, given out in rooms in order and taken back in any
/// order: a room's places are given again only when it and every room given
/// before it are back.
#[derive(Debug)]
pub(super) struct Rooms {
    slots: u64,
    /// Places given out so far, and taken back (all below `tail`).
    head: u64,
    tail: u64,
    /// Rooms out: id, base, places, back.
    out: VecDeque<(u64, u64, u64, bool)>,
    next: u64,
}

impl Rooms {
    pub(super) fn new(slots: u64) -> Rooms {
        Rooms { slots, head: 0, tail: 0, out: VecDeque::new(), next: 0 }
    }

    /// A room of `want` places, or of what is free if less (at least
    /// `least`); none if fewer are free or too many rooms are out.
    pub(super) fn take(&mut self, want: u64, least: u64) -> Option<Room> {
        let free = self.slots - (self.head - self.tail);
        let room = want.min(free);
        if room < least.max(1) || self.out.len() as u64 >= COUNTERS {
            return None;
        }
        let id = self.next;
        self.next += 1;
        let base = self.head;
        self.head += room;
        self.out.push_back((id, base, room, false));
        Some(Room { id, base, room, shape: 0, ranges: Vec::new() })
    }

    /// Room `id` is done with.
    pub(super) fn give_back(&mut self, id: u64) {
        if let Some(r) = self.out.iter_mut().find(|r| r.0 == id) {
            r.3 = true;
        }
        while self.out.front().is_some_and(|r| r.3) {
            self.out.pop_front();
        }
        self.tail = self.out.front().map_or(self.head, |r| r.1);
    }
}

/// What the pass needs, kept with the compare's (`CardWork`).
pub(super) struct Snap {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// Beside the compare's results, a dword each: the place plus one.
    places: (ID3D12Resource, usize),
    /// The ring and its mapping.
    ring: (ID3D12Resource, usize),
    counters: ID3D12Resource,
    pub(super) rooms: Rooms,
    /// By a flight's shape (its compared ranges), the pieces the last such
    /// flight flagged.
    shapes: HashMap<u64, u64>,
}

// SAFETY: free-threaded objects and mappings, under `CARD_WORK`'s lock.
unsafe impl Send for Snap {}

impl Snap {
    /// Made with the compare's work (`Host::make_card_work`), `results` bytes
    /// of results beside the compare's.
    pub(super) fn make(host: &Host, results: u64) -> Option<Snap> {
        let made = (|| -> windows::core::Result<Snap> {
            use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
            use windows::Win32::Graphics::Direct3D::ID3DBlob;
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: the source and names live through the call.
            unsafe {
                D3DCompile(
                    SNAP.as_ptr().cast(),
                    SNAP.len(),
                    windows::core::s!("card snapshot"),
                    None,
                    None,
                    windows::core::s!("main"),
                    windows::core::s!("cs_5_1"),
                    0,
                    0,
                    &mut code,
                    None,
                )?;
            }
            let code = code.ok_or_else(windows::core::Error::empty)?;
            let missing = windows::core::Error::empty;
            let (places, places_data) = host.shared_buffer(results).ok_or_else(missing)?;
            let slots = ring_bytes() / SLOT;
            let (ring, ring_data) = host.shared_buffer(slots * SLOT).ok_or_else(missing)?;
            let counters = host.uav_buffer(COUNTERS * 4).ok_or_else(missing)?;
            name(&places, || "card snapshot places".into());
            name(&ring, || "card snapshot ring".into());
            name(&counters, || "card snapshot counters".into());
            // SAFETY: the blob's own bytes; the root signature is in it.
            unsafe {
                let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
                let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes)?;
                let pso: ID3D12PipelineState = host.device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                    ..Default::default()
                })?;
                Ok(Snap {
                    root,
                    pso,
                    places: (places, places_data as usize),
                    ring: (ring, ring_data as usize),
                    counters,
                    rooms: Rooms::new(slots),
                    shapes: HashMap::new(),
                })
            }
        })();
        match made {
            Ok(snap) => {
                tracing::info!(
                    mb = ring_bytes() >> 20,
                    "pieces the compare flags are copied inside their flight (VICTORY_FETCH_IN_FLIGHT=1)"
                );
                Some(snap)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no copy of flagged pieces inside the flight: they are fetched as before");
                None
            }
        }
    }

    /// A room for a flight whose compared ranges are `ranges` (copy, first
    /// piece, pieces): twice what the last flight of that shape flagged and
    /// some, at most its pieces and an eighth of the ring.
    pub(super) fn plan(&mut self, ranges: impl Iterator<Item = (u64, u32, u32)>) -> Option<Room> {
        let (mut shape, mut pieces) = (0xcbf2_9ce4_8422_2325u64, 0u64);
        for (copy, first, n) in ranges {
            for v in [copy, u64::from(first), u64::from(n)] {
                shape = (shape ^ v).wrapping_mul(0x0100_0000_01b3);
            }
            pieces += u64::from(n);
        }
        if pieces == 0 {
            return None;
        }
        let want = match self.shapes.get(&shape) {
            Some(&flagged) => flagged * 2 + 8,
            None => 64,
        };
        let want = want.min(pieces).min(self.rooms.slots / 8).max(1);
        let room = self.rooms.take(want, want.min(8));
        STATS[if room.is_some() { 0 } else { 1 }].fetch_add(1, Ordering::Relaxed);
        room.map(|r| {
            STATS[2].fetch_add(r.room, Ordering::Relaxed);
            Room { shape, ..r }
        })
    }

    /// A flight of `room`'s shape flagged `flagged` pieces.
    pub(super) fn flagged(&mut self, room: &Room, flagged: u64) {
        if self.shapes.len() > 1 << 14 {
            self.shapes.clear();
        }
        self.shapes.insert(room.shape, flagged);
    }

    /// The mapping of the places beside the compare's results.
    pub(super) fn places(&self) -> usize {
        self.places.1
    }

    /// Where the bytes at place `place` (1 on) of `room` are.
    pub(super) fn bytes_at(&self, room: &Room, place: u32) -> Option<usize> {
        let place = u64::from(place).checked_sub(1).filter(|&p| p < room.room)?;
        Some(self.ring.1 + (((room.base + place) % self.rooms.slots) * SLOT) as usize)
    }

    /// Records the pass, behind the compare, in `list` (open, the compare's
    /// passes recorded): for each of `ranges` (the copy's buffer, its size,
    /// the span, its first result's place).
    ///
    /// # Safety
    /// As the compare's: live buffers, the pieces lie in them.
    pub(super) unsafe fn record(
        &self,
        list: &ID3D12GraphicsCommandList,
        differs: &ID3D12Resource,
        room: &Room,
        ranges: &[(&ID3D12Resource, u32, &crate::shadow::CardSpan, u32)],
    ) {
        const MOST: u32 = D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION;
        let all = [D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
            },
        }];
        let counter = ((room.id % COUNTERS) * 4) as u32;
        let constants = |first: u32, skew: u32, size: u32, pieces: u32, at: u32, mode: u32| {
            [first, skew, size, pieces, at * 4, (room.base % self.rooms.slots) as u32, room.room as u32, counter, self.rooms.slots as u32, mode]
        };
        // SAFETY: the caller's.
        unsafe {
            // The compare's results first.
            list.ResourceBarrier(&all);
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            list.SetComputeRootUnorderedAccessView(2, differs.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(3, self.places.0.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(4, self.ring.0.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(5, self.counters.GetGPUVirtualAddress());
            let zero = constants(0, 0, 0, 0, 0, 0);
            list.SetComputeRoot32BitConstants(0, 10, zero.as_ptr().cast(), 0);
            if let Some((buffer, ..)) = ranges.first() {
                list.SetComputeRootShaderResourceView(1, buffer.GetGPUVirtualAddress());
            }
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(&all);
            for &(buffer, size, span, at) in ranges {
                list.SetComputeRootShaderResourceView(1, buffer.GetGPUVirtualAddress());
                let mut done = 0;
                while done < span.pieces {
                    let n = (span.pieces - done).min(MOST);
                    let c = constants(span.first + done, span.skew, size, n, at + done, 1);
                    list.SetComputeRoot32BitConstants(0, 10, c.as_ptr().cast(), 0);
                    list.Dispatch(n, 1, 1);
                    done += n;
                }
            }
        }
    }
}

/// What old fetches brought while in-flight copies were on: per fetch, the
/// value signalled behind the last flight sent before it, and the pieces
/// (copy, piece) it brought. A fetch sent after flight N ran behind it and
/// may hold its bytes; a snapshot of an earlier flight's piece is older.
#[derive(Default)]
pub(super) struct Newer(Vec<(u64, HashSet<(u64, u32)>)>);

impl Newer {
    /// The landing's first flight is behind `fence`: fetches that ran behind
    /// no later flight say nothing any more.
    pub(super) fn landing(&mut self, fence: u64) {
        self.0.retain(|f| f.0 >= fence);
    }

    pub(super) fn fetched(&mut self, ahead: u64, pieces: impl Iterator<Item = (u64, u32)>) {
        self.0.push((ahead, pieces.collect()));
    }

    /// Whether a fetch may have brought a later flight's bytes of `piece`
    /// than that behind `fence`.
    pub(super) fn newer(&self, fence: u64, piece: (u64, u32)) -> bool {
        self.0.iter().any(|f| f.0 > fence && f.1.contains(&piece))
    }
}

pub(super) static NEWER: Mutex<Newer> = Mutex::new(Newer(Vec::new()));

/// An old fetch, done and merged, brought `pieces`. On the copy queue it ran
/// beside the drawing queue, so it may hold bytes of any flight sent before
/// it was done: the value behind the last flight sent by now bounds them.
pub(super) fn fetched_old(pieces: &[(std::sync::Arc<crate::shadow::Shadow>, usize, usize, u64)]) {
    if !on() {
        return;
    }
    let ahead = line().target;
    let brought = pieces.iter().map(|(shadow, lo, ..)| (shadow.id(), shadow.card_piece_of(*lo)));
    NEWER.lock().unwrap_or_else(|e| e.into_inner()).fetched(ahead, brought);
}

/// Since the last frame-rate line: rooms given, flights without one, places
/// given; landings merged from the ring, landings fetched as before (no
/// place for a piece, a piece newer in title memory), pieces and bytes
/// merged from the ring, microseconds merging them.
pub(super) static STATS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// Why a landing was fetched as before.
pub(super) const NO_PLACE: usize = 4;
pub(super) const NEWER_THERE: usize = 5;

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "pieces copied inside their flight per frame (VICTORY_FETCH_IN_FLIGHT=1): rooms for {:.1} flights ({:.1} places), {:.1} \
         flights without; {:.1} landings merged from the ring ({:.1} pieces, {:.1} MB, {:.2} ms merging), {:.1} fetched as before \
         ({:.1} for a piece without a place, {:.1} for a piece an old fetch brought newer)",
        s[0],
        s[2],
        s[1],
        s[3],
        s[6],
        s[7] / 1e6,
        s[8] / 1000.0,
        s[NO_PLACE] + s[NEWER_THERE],
        s[NO_PLACE],
        s[NEWER_THERE],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rooms out never share a place, a room's places come back only when
    /// every room given before it is back, and taking stops when the ring is
    /// full. Fixed seeds, rooms given back in any order.
    #[test]
    fn rooms_never_share_a_place() {
        let mut seed = 0x6c8e_9cf5u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for slots in [1u64, 7, 64, 1000] {
            let mut rooms = Rooms::new(slots);
            let mut out: Vec<Room> = Vec::new();
            for _ in 0..5000 {
                if next(3) > 0 {
                    let want = 1 + next(slots.min(100));
                    let least = 1 + next(want);
                    let free = slots - out.iter().map(|r| r.room).sum::<u64>();
                    match rooms.take(want, least) {
                        Some(room) => {
                            assert!(room.room >= least && room.room <= want, "{room:?}");
                            out.push(room);
                        }
                        // Out of order, rooms back may still hold their
                        // places; never less is refused than is free.
                        None => assert!(free < least || out.len() as u64 >= COUNTERS || rooms.head - rooms.tail > slots - least),
                    }
                } else if !out.is_empty() {
                    let room = out.swap_remove(next(out.len() as u64) as usize);
                    rooms.give_back(room.id);
                }
                // No place of the ring is in two rooms out.
                let mut held = vec![false; slots as usize];
                for r in &out {
                    for p in r.base..r.base + r.room {
                        let p = (p % slots) as usize;
                        assert!(!held[p], "place {p} twice: {out:?}");
                        held[p] = true;
                    }
                }
                assert!(rooms.head - rooms.tail <= slots);
            }
        }
    }

    /// [`place_begins`] is where the pass begins a piece, as the compare
    /// widens it, and a piece's bytes fit in its place.
    #[test]
    fn a_piece_fits_its_place() {
        for skew in [0u32, 16, 100, 65_535] {
            for piece in 0..5u32 {
                let lo = place_begins(piece, skew);
                let hi = (((u64::from(piece) + 1) * 65_536 - u64::from(skew) + 15) & !15) as usize;
                assert_eq!(lo % 16, 0);
                assert!(hi - lo <= SLOT as usize, "{piece} {skew}");
                // The piece's own bytes begin at or after its place.
                let own = (u64::from(piece) * 65_536).saturating_sub(u64::from(skew)) as usize;
                assert!(lo <= own && own - lo < 16);
            }
        }
    }

    /// A snapshot is refused exactly when a fetch behind a later flight
    /// brought the piece.
    #[test]
    fn a_newer_fetch_refuses_the_snapshot() {
        let mut newer = Newer::default();
        newer.fetched(9, [(1, 4), (2, 0)].into_iter());
        assert!(newer.newer(8, (1, 4)));
        assert!(!newer.newer(9, (1, 4)));
        assert!(!newer.newer(8, (1, 5)));
        newer.landing(10);
        assert!(!newer.newer(1, (1, 4)));
    }
}
