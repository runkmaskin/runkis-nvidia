//! Host memory for the textures the title fills, and for their staging
//! buffers, cut from blocks made once.
//!
//! Each such texture had an allocation of its own and its staging buffer
//! another. On an RX 7900 XTX an allocation takes about 310 microseconds
//! whatever its size, on one thread (`texture::allocation_costs`), and a
//! texture cut from a block 6: the pass that makes the textures shaders
//! pick by index (`draw.rs` `heap_textures`) got through 350-500 of the
//! 24,000 queued when the world loads in each of its 250 ms turns, some
//! forty seconds of a world without its textures.
//! `VICTORY_TEXTURE_POOL=0` makes every one an allocation of its own again.
//!
//! A piece given back is used again only two seconds later (nothing the card
//! was told before that is still running: Windows resets a card that takes
//! that long over one submission), and by then no list names it (the owners' rule, as for allocations of their own): a
//! texture's piece comes back when the last reference to the host texture
//! goes (an object of ours the texture holds says so), a staging buffer's
//! when the last list that recorded its copy is reset.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

/// Bytes of a block; pieces of more than half of one are not pooled (with
/// blocks made ahead, [`ahead`]: of more than one).
const BLOCK: u64 = 64 << 20;
/// Blocks with nothing in them that are kept.
const SPARE: usize = 2;
/// The same while the card has room ([`room`]): 4 GiB of them.
const SPARE_KEPT: usize = 64;
/// The same of blocks of staging buffers, which are the PC's memory: run
/// crash-1 held 5.2 GB of them.
const STAGING_KEPT: usize = 8;
/// The same of blocks of staging buffers while the PC is short of memory
/// (`crate::host::ram_short`, `VICTORY_RAM_TRIM=1`): one, for the next
/// texture.
const STAGING_SHORT: usize = 1;
/// How many are kept now.
static SPARES: AtomicUsize = AtomicUsize::new(SPARE);

/// Sets how many empty blocks stay, by the card's memory (budget and use):
/// [`SPARE_KEPT`] below 85 % ([`crate::host::keep_freed`]), else [`SPARE`].
/// Asked once a second.
pub(crate) fn room(memory: Option<[u64; 4]>) {
    SPARES.store(spares(memory, crate::host::keep_freed()), Ordering::Relaxed);
    if settle_each_second() {
        let now = now();
        settle(&TEXTURES, now);
        settle(&STAGING, now);
    } else if crate::host::ram_short() {
        // The staging blocks go without a take too while the PC is short
        // of memory: streaming may have stopped. (Texture blocks are the
        // card's memory, and stay as they were.)
        settle(&STAGING, now());
    }
    if compact() {
        lock(&STAGING).plan(crate::host::ram_short());
    }
}

/// `VICTORY_STAGING_COMPACT=1` (off): while the PC is short of memory
/// (`VICTORY_RAM_TRIM=1`), the staging pieces textures keep move out of
/// blocks that are at most half full into fuller ones, so that the emptied
/// blocks can go. In a 768p run the staging blocks held 448 MB for 43 MB
/// while short: 103 pieces kept by textures sampled every frame lay in six
/// blocks, and only empty blocks go. A piece moves only while no list holds
/// its staging buffer (`texture::let_go_idle`), with its bytes copied, so
/// every upload after it reads what it would have read before. Off: no
/// block is ever emptied, and takes are as before.
pub(crate) fn compact() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_STAGING_COMPACT"))
}

/// Bytes of staging moved out of blocks being emptied since the start (the
/// memory line's `staging_moved`).
pub(crate) static STAGING_MOVED: AtomicU64 = AtomicU64::new(0);

impl StagingPiece {
    /// Whether this piece lies in a block being emptied ([`compact`]).
    pub(crate) fn to_move(&self) -> bool {
        lock(&STAGING).in_emptying(&self.0)
    }
}

/// For a staging piece that moves ([`compact`]): `bytes` of upload memory in
/// a block that is not being emptied, never a new block. Its bytes are
/// whatever was there; the caller writes all of them. `None`: no room.
pub(crate) fn staging_for_move(bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    let bytes = bytes.next_multiple_of(512);
    let (piece, memory) = lock(&STAGING).take_for_move(bytes, 512, now())?;
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    STAGING_MOVED.fetch_add(bytes, Ordering::Relaxed);
    Some((StagingPiece(piece), memory.0, piece.start, data))
}

/// `VICTORY_POOL_SETTLE=1`: [`settle`] once a second as well. Off by default:
/// freeing empty blocks while nothing is taken means making them again at the
/// next streaming burst, mid-frame; a first game run of builds with it on had
/// more frames over 100 ms. Until a run shows which way it goes, blocks go only
/// at a take, as before.
fn settle_each_second() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_POOL_SETTLE"))
}

/// What a take does first ([`Pool::rested`]), also while nothing is taken:
/// pieces given back that have rested are free again, and empty blocks past
/// those kept go. Only takes did it: once the world had loaded and few
/// textures were made, a pool kept every block whose pieces had come back,
/// and kept them when the card's room fell ([`room`]).
fn settle<B: Clone>(pool: &Mutex<Pool<B>>, now: u64) {
    pool.lock().unwrap_or_else(|e| e.into_inner()).rested(now);
}

fn spares(memory: Option<[u64; 4]>, keep: bool) -> usize {
    match memory {
        Some([budget, used, ..]) if keep && u128::from(used) * 100 < u128::from(budget) * 85 => SPARE_KEPT,
        _ => SPARE,
    }
}

/// Milliseconds until a piece given back is used again. (It was three
/// frames, a tenth of a second while the title shows a loading screen at 30
/// a second, with up to 18 submissions in the air.)
const REST: u64 = 2500;

fn now() -> u64 {
    crate::host::now_ms()
}

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_POOL").is_none_or(|v| v != "0"))
}

/// How many blocks of textures are made ahead, so that one has been there
/// for a while when its first piece is taken (`VICTORY_RIPE_BLOCKS`, 12;
/// 0: none, and new blocks are used at once, as before).
///
/// A page fault of lists already on the card, in a texture made the same
/// tenth of a second as the first piece of a block made for it, lost the
/// device in runs n08 and n11 (2026-11-25): the title gives a slot that
/// those lists read by index a new texture. Why the card does not have the
/// new block's addresses yet is not known (n11 waited for `MakeResident`;
/// `indirect::tests::new_blocks_under_readers` never loses it). A block that
/// every submission begun before its making has left behind ([`Ripe`]) has
/// no such lists: pieces are cut from those first.
fn ahead() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| std::env::var("VICTORY_RIPE_BLOCKS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(12))
}

/// Submissions begun and finished ([`crate::host::submissions`]), and how
/// many blocks to make ahead (0: every block is ripe at once).
#[derive(Clone, Copy)]
struct Ripe {
    started: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    const ALL: Ripe = Ripe { started: 0, finished: 0, ahead: 0 };

    fn now() -> Ripe {
        let (started, finished) = crate::host::submissions();
        // Not more than are kept ([`room`]): they would go and be made again.
        Ripe { started, finished, ahead: ahead().min(SPARES.load(Ordering::Relaxed)) }
    }

    fn is<B>(self, block: &Block<B>) -> bool {
        self.ahead == 0 || block.made <= self.finished
    }
}

/// Pieces cut from blocks that were not ripe, for want of a ripe one.
static UNRIPE: AtomicU64 = AtomicU64::new(0);

/// The free ranges of a block: start and length, never two that touch.
struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]))
    }

    /// The lowest place for `len` bytes at a multiple of `align`.
    fn take(&mut self, len: u64, align: u64) -> Option<u64> {
        let (start, size, at) = self.0.iter().find_map(|(&start, &size)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })?;
        self.0.remove(&start);
        if at > start {
            self.0.insert(start, at - start);
        }
        if at + len < start + size {
            self.0.insert(at + len, start + size - at - len);
        }
        Some(at)
    }

    fn give(&mut self, mut start: u64, mut len: u64) {
        if let Some((&before, &size)) = self.0.range(..start).next_back()
            && before + size == start
        {
            self.0.remove(&before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.0.remove(&(start + len)) {
            len += size;
        }
        self.0.insert(start, len);
    }

    fn whole(&self, size: u64) -> bool {
        self.0.get(&0) == Some(&size)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    block: usize,
    start: u64,
    len: u64,
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made (zeros).
    used_to: u64,
    /// The newest submission begun when it was made ([`Ripe`]).
    made: u64,
    /// When that was (ms).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    since: u64,
}

struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    blocks: Vec<Option<Block<B>>>,
    /// Pieces given back, and when each may be used again.
    resting: VecDeque<(u64, Piece)>,
    /// The empty blocks it was last told to keep, and the most it keeps.
    spare: usize,
    most: usize,
    /// The most it keeps while `short` says the PC is short of memory.
    most_short: usize,
    short: fn() -> bool,
    /// Blocks whose pieces are to move to fuller ones ([`Pool::plan`]); new
    /// pieces are cut elsewhere first. Empty: none (always for textures).
    emptying: Vec<bool>,
}

impl<B: Clone> Pool<B> {
    const fn new(most: usize) -> Pool<B> {
        Pool::with_short(most, most, never)
    }

    /// A pool that keeps at most `most_short` empty blocks while `short`
    /// says so.
    const fn with_short(most: usize, most_short: usize, short: fn() -> bool) -> Pool<B> {
        Pool { blocks: Vec::new(), resting: VecDeque::new(), spare: 0, most, most_short, short, emptying: Vec::new() }
    }

    /// A piece of `len` bytes and the memory it lies in, and whether that
    /// part was ever used before; `make` makes a block when none has room.
    fn take(&mut self, len: u64, align: u64, now: u64, make: impl FnMut() -> Option<B>) -> Option<(Piece, B, bool)> {
        self.take_ripe(len, align, now, Ripe::ALL, make)
    }

    /// The same from a block that is ripe, if one has room; `make` makes one
    /// ahead too, when few are empty.
    fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        // With blocks made ahead, what would be an allocation of its own (and
        // new when the card first reads it) lies in one of those too.
        if len == 0 || len > if ripe.ahead > 0 { BLOCK } else { BLOCK / 2 } {
            return None;
        }
        self.rested(now);
        let mut found = None;
        // Blocks being emptied ([`Pool::plan`]) last: without any, one pass
        // over the blocks as before.
        let emptying = &self.emptying;
        for last in [false, true] {
            if last && !emptying.contains(&true) {
                break;
            }
            for (i, block) in self.blocks.iter_mut().enumerate() {
                if emptying.get(i).copied().unwrap_or(false) != last {
                    continue;
                }
                if let Some(block) = block
                    && ripe.is(block)
                    && let Some(start) = block.free.take(len, align)
                {
                    found = Some((i, start));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            // The one that has been there the longest.
            let mut unripe: Vec<(u64, usize)> =
                self.blocks.iter().enumerate().filter_map(|(i, b)| Some((b.as_ref()?.made, i))).collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        let (i, start) = match found {
            Some(found) => found,
            None => {
                let i = self.add(make()?, ripe.started);
                (i, self.blocks[i].as_mut()?.free.take(len, align)?)
            }
        };
        if ripe.ahead > 0 {
            if !ripe.is(self.blocks[i].as_ref()?) {
                let n = UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
                }
            }
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count();
            if empty < ripe.ahead
                && let Some(memory) = make()
            {
                self.add(memory, ripe.started);
            }
        }
        let block = self.blocks[i].as_mut()?;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    fn add(&mut self, memory: B, made: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] = Some(Block { memory, free: Ranges::new(BLOCK), used_to: 0, made, since: crate::host::now_ms() });
        i
    }

    fn give(&mut self, piece: Piece, now: u64) {
        self.resting.push_back((now + REST, piece));
    }

    /// Makes the pieces that rested long enough free, and lets go of empty
    /// blocks past [`SPARE`].
    fn rested(&mut self, now: u64) {
        let mut any = false;
        while let Some(&(ready, piece)) = self.resting.front()
            && ready <= now
        {
            self.resting.pop_front();
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                any |= block.free.whole(BLOCK);
            }
        }
        // (Without a piece given back too, once there is less room.)
        let short = (self.short)();
        let most = SPARES.load(Ordering::Relaxed).min(if short { self.most_short } else { self.most });
        if any || most < self.spare {
            self.spare = most;
            let mut spare = 0;
            for block in &mut self.blocks {
                if block.as_ref().is_some_and(|b| b.free.whole(BLOCK)) {
                    spare += 1;
                    if spare > most {
                        *block = None;
                        if short {
                            crate::host::RAM_TRIMS[1].fetch_add(BLOCK, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }

    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }

    /// Bytes of each block that taken pieces hold (not free, not resting).
    fn live(&self) -> Vec<u64> {
        let mut live: Vec<u64> =
            self.blocks.iter().map(|b| b.as_ref().map_or(0, |b| BLOCK - b.free.0.values().sum::<u64>())).collect();
        for (_, piece) in &self.resting {
            if let Some(l) = live.get_mut(piece.block) {
                *l = l.saturating_sub(piece.len);
            }
        }
        live
    }

    /// Picks the blocks whose pieces are to move ([`compact`]): the fullest
    /// blocks that hold what is taken at three quarters full are kept, and
    /// the others that are at most half full are emptied. `on` false: none.
    fn plan(&mut self, on: bool) {
        self.emptying.clear();
        if !on {
            return;
        }
        let live = self.live();
        let total: u64 = live.iter().sum();
        let mut order: Vec<usize> = (0..live.len()).filter(|&i| live[i] > 0).collect();
        order.sort_by_key(|&i| (std::cmp::Reverse(live[i]), i));
        let keep = (total * 4 / 3).div_ceil(BLOCK).max(1) as usize;
        self.emptying = vec![false; self.blocks.len()];
        for &i in order.iter().skip(keep) {
            if live[i] <= BLOCK / 2 {
                self.emptying[i] = true;
            }
        }
    }

    /// Whether `piece` lies in a block being emptied.
    fn in_emptying(&self, piece: &Piece) -> bool {
        self.emptying.get(piece.block).copied().unwrap_or(false)
    }

    /// A piece of `len` bytes for a piece that moves out of a block being
    /// emptied: from a block that is not, never a new block. Its bytes are
    /// whatever they were (the move writes all of them).
    fn take_for_move(&mut self, len: u64, align: u64, now: u64) -> Option<(Piece, B)> {
        self.rested(now);
        let emptying = &self.emptying;
        let (i, start) = self.blocks.iter_mut().enumerate().find_map(|(i, block)| {
            if emptying.get(i).copied().unwrap_or(false) {
                return None;
            }
            Some((i, block.as_mut()?.free.take(len, align)?))
        })?;
        let block = self.blocks[i].as_mut()?;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone()))
    }
}

/// Never short of memory: pools that keep their spares whatever the PC has.
fn never() -> bool {
    false
}

/// A mapped upload buffer's memory.
#[derive(Clone)]
struct Upload(ID3D12Resource, *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

static TEXTURES: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new(SPARE_KEPT));
static STAGING: Mutex<Pool<Upload>> = Mutex::new(Pool::with_short(STAGING_KEPT, STAGING_SHORT, crate::host::ram_short));

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes of the pieces textures hold now.
static HELD: AtomicU64 = AtomicU64::new(0);

/// Bytes of the texture blocks no texture holds: the card's memory in use
/// by us that new textures take first.
pub(crate) fn texture_room() -> u64 {
    (lock(&TEXTURES).blocks() as u64 * BLOCK).saturating_sub(HELD.load(Ordering::Relaxed))
}

/// Bytes of the texture blocks that textures hold now, for the memory line.
pub(crate) fn texture_held() -> u64 {
    HELD.load(Ordering::Relaxed)
}

/// For the memory lines: blocks of textures and of staging buffers, in MiB.
pub(crate) fn memory() -> [u64; 2] {
    [lock(&TEXTURES).blocks() as u64 * (BLOCK >> 20), lock(&STAGING).blocks() as u64 * (BLOCK >> 20)]
}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// `bytes` of upload memory, all zeros: the buffer it lies in, where in
/// that buffer, and its mapping. `None`: not pooled.
pub(crate) fn staging(host: &crate::host::Host, bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !on() {
        return None;
    }
    // Copies from a buffer begin at multiples of 512.
    let bytes = bytes.next_multiple_of(512);
    let (piece, memory, again) = lock(&STAGING).take(bytes, 512, now(), || {
        let (buffer, data) = host.upload_buffer(BLOCK)?;
        crate::host::name(&buffer, || "staging of textures (a block)".into());
        Some(Upload(buffer, data))
    })?;
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (an unmapped texture) uploads zeros,
        // as from a buffer of its own.
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    }
    Some((owned, memory.0, piece.start, data))
}

/// An object the host texture holds and releases when it goes: its piece
/// is free then.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    piece: Piece,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Reclaim>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Reclaim>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Reclaim>());
            HELD.fetch_sub(gone.piece.len, Ordering::Relaxed);
            lock(&TEXTURES).give(gone.piece, now());
        }
        left
    }
}

/// Textures made in the last second ([`fresh`]), and when.
static RECENT: Mutex<VecDeque<(usize, u64)>> = Mutex::new(VecDeque::new());

/// Whether `texture` was made in a block in the last second.
pub(crate) fn fresh(texture: &ID3D12Resource) -> bool {
    let raw = texture.as_raw() as usize;
    let since = now().saturating_sub(1000);
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take_while(|r| r.1 >= since).any(|r| r.0 == raw)
}

fn made_now(texture: &ID3D12Resource) {
    let now = now();
    let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    while recent.front().is_some_and(|r| r.1 + 1000 < now) {
        recent.pop_front();
    }
    recent.push_back((texture.as_raw() as usize, now));
}

/// Makes `texture` give `piece` back when the last reference to it goes.
fn give_back_with(texture: &ID3D12Resource, piece: Piece) {
    const KEY: GUID = GUID::from_u128(0x6f0e2f0c_5a59_4b1e_9a3f_7f4d2c9be713);
    let made = Box::into_raw(Box::new(Reclaim { methods: &RECLAIM.0, references: AtomicU32::new(1), piece }));
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the texture takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if texture.SetPrivateDataInterface(&KEY, &object).is_err() {
            // Never given back: the piece stays taken, nothing is reused
            // under a texture that may be alive.
            std::mem::forget(object);
        }
    }
}

/// A texture of `desc` (no target, depth or unordered access) in a block;
/// `None`: not pooled, or the host refused.
///
/// # Safety
/// `desc` is a valid description.
pub(crate) unsafe fn texture(
    device: &ID3D12Device,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    // Small textures lie at multiples of 4 KiB where the host allows it
    // (the top mip is 64 KiB at most): 64 KiB each otherwise.
    // SAFETY: the caller's description.
    let mut d = unsafe { *desc };
    d.Alignment = u64::from(D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT);
    // SAFETY: plain queries.
    let mut info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    if info.SizeInBytes == u64::MAX || info.Alignment != d.Alignment {
        d.Alignment = 0;
        info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    }
    if info.SizeInBytes == u64::MAX {
        return None;
    }
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (piece, heap, again) = lock(&TEXTURES).take_ripe(info.SizeInBytes, info.Alignment.max(1), now(), Ripe::now(), || {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: BLOCK,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | crate::host::not_resident(),
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation.
        if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
            tracing::warn!(error = %e, "no block of texture memory");
        }
        if let Some(h) = &heap
            && let Err(e) = crate::host::resident(device, h).and_then(|()| crate::host::mapped(device))
        {
            tracing::warn!(error = %e, "a block of texture memory is not resident");
            return None;
        }
        heap
    })?;
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a piece of
    // it is taken.
    let result = unsafe { device.CreatePlacedResource(&heap, piece.start, &d, state, None, &mut made) };
    match (result, made) {
        (Ok(()), Some(made)) => {
            HELD.fetch_add(piece.len, Ordering::Relaxed);
            give_back_with(&made, piece);
            made_now(&made);
            #[cfg(feature = "diagnostics")]
            {
                crate::book::made(&made, || "a texture in a block".into());
                crate::book::block(&heap, &made, piece.start, BLOCK);
                crate::book::note(made.as_raw() as usize, || {
                    let block = lock(&TEXTURES).blocks.get(piece.block).and_then(Option::as_ref).map(|b| (b.made, b.since));
                    let (submission, since) = block.unwrap_or_default();
                    format!(
                        "; piece {:#x}+{:#x} of block {} {:#x} made {:.1} s before it at submission {submission}, used before: {again}, made at submissions {:?}",
                        piece.start,
                        piece.len,
                        piece.block,
                        heap.as_raw() as usize,
                        now().saturating_sub(since) as f64 / 1e3,
                        crate::host::submissions()
                    )
                });
            }
            Some(made)
        }
        _ => {
            lock(&TEXTURES).give(piece, now());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_cut_aligned_and_join_again() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.take(100, 64), Some(0));
        assert_eq!(r.take(100, 64), Some(128));
        assert_eq!(r.take(28, 4), Some(100));
        assert_eq!(r.take(2000, 4), None);
        r.give(128, 100);
        r.give(0, 100);
        assert!(!r.whole(1024));
        r.give(100, 28);
        assert!(r.whole(1024));
    }

    #[test]
    fn empty_blocks_stay_while_the_card_has_room() {
        assert_eq!(spares(Some([100, 84, 0, 0]), true), SPARE_KEPT);
        assert_eq!(spares(Some([100, 85, 0, 0]), true), SPARE);
        assert_eq!(spares(Some([100, 10, 0, 0]), false), SPARE);
        assert_eq!(spares(None, true), SPARE);
    }

    #[test]
    fn pieces_never_overlap_and_rest_before_they_are_used_again() {
        let mut pool: Pool<u32> = Pool::new(SPARE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let mut taken: Vec<Piece> = Vec::new();
        for i in 0..500u64 {
            let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
            assert!(!again);
            assert!(taken.iter().all(|q| q.block != p.block || p.start + p.len <= q.start || q.start + q.len <= p.start));
            taken.push(p);
        }
        let blocks = pool.blocks();
        assert!(blocks >= 2);
        // Given back at 10: not used again before 10 + REST.
        let first = taken[0];
        pool.give(first, 10);
        let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
        assert_ne!((p.block, p.start), (first.block, first.start));
        let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
        assert_eq!((q.block, q.start, again), (first.block, first.start, true));
        // Everything back: the empty blocks past the spare ones go.
        for p in taken.drain(1..).chain([p, q]) {
            pool.give(p, 20);
        }
        pool.rested(20 + REST);
        assert_eq!(pool.blocks(), blocks.min(SPARE));
        // Too big for a block, and a block that cannot be made.
        assert!(pool.take(BLOCK, 65536, 30, make).is_none());
        let mut none: Pool<u32> = Pool::new(SPARE);
        assert!(none.take(65536, 65536, 0, || None).is_none());
    }

    /// Pieces given back free their blocks with the once-a-second look, not
    /// only at the next take; pieces still resting keep theirs.
    #[test]
    fn empty_blocks_go_without_a_take() {
        let pool: Mutex<Pool<u32>> = Mutex::new(Pool::new(0));
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let pieces: Vec<Piece> = {
            let mut p = pool.lock().unwrap();
            (0..3).map(|_| p.take(BLOCK / 2, 65536, 0, make).unwrap().0).collect()
        };
        assert_eq!(pool.lock().unwrap().blocks(), 2);
        for piece in &pieces[..2] {
            pool.lock().unwrap().give(*piece, 10);
        }
        settle(&pool, 10 + REST - 1);
        assert_eq!(pool.lock().unwrap().blocks(), 2, "still resting");
        settle(&pool, 10 + REST);
        assert_eq!(pool.lock().unwrap().blocks(), 1, "the empty one went, the held one stays");
        pool.lock().unwrap().give(pieces[2], 20);
        settle(&pool, 20 + REST);
        assert_eq!(pool.lock().unwrap().blocks(), 0);
        assert_eq!(made.get(), 2);
    }

    #[test]
    fn pieces_come_from_blocks_older_than_the_lists_that_run() {
        let mut pool: Pool<u64> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let at = |started, finished| Ripe { started, finished, ahead: 2 };
        let take = |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap().1;
        // Nothing runs: at once, and two are made ahead, one a time.
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 2);
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 3);
        // 4 is made while submission 5 runs.
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(made.get(), 4);
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        // None is ripe: the oldest; it is ripe once 5 has finished.
        assert_eq!(take(&mut pool, at(7, 4)), 4);
        assert_eq!(take(&mut pool, at(7, 5)), 4);
        // A whole block's worth too.
        assert_eq!(pool.take_ripe(BLOCK, 65536, 0, at(8, 7), make).unwrap().1, 5);
        // Off: the first with room, whatever its age, none ahead.
        let n = made.get();
        assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
        assert_eq!(made.get(), n);
        assert!(pool.take(BLOCK, 65536, 0, make).is_none());
    }

    std::thread_local! {
        /// What [`short_here`] says, for the pools of this thread's test.
        static SHORT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn short_here() -> bool {
        SHORT.with(std::cell::Cell::get)
    }

    /// A fixed-seed xorshift.
    fn next(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Staging as `staging` uses the pool, on memory of the test's own (a
    /// block is a vector of zeros when made, as an upload buffer is), with
    /// the PC short of memory part of the time or never. Returns the bytes
    /// each piece held when its copy was recorded (in order of the takes),
    /// the most empty blocks seen after a settle while short (or in all, if
    /// never short), and the blocks made.
    fn staging_run(seed: u64, short_sometimes: bool) -> (Vec<Vec<u8>>, usize, usize) {
        use std::collections::HashMap;
        // Small blocks' worth of bytes: the pool cuts 64 MiB blocks, but
        // only pieces' own bytes are simulated.
        let mut seed = seed;
        let mut pool: Pool<u64> = Pool::with_short(STAGING_KEPT, STAGING_SHORT, short_here);
        let memory: std::cell::RefCell<HashMap<(u64, u64), Vec<u8>>> = Default::default();
        let made = std::cell::Cell::new(0u64);
        let mut live: Vec<(Piece, u64, u8)> = Vec::new();
        let mut uploaded = Vec::new();
        let mut most_empty_short = 0;
        let mut now = 0u64;
        for step in 0..3000u64 {
            now += next(&mut seed) % 400;
            if short_sometimes {
                // Short for stretches of steps.
                SHORT.with(|s| s.set(step / 250 % 2 == 1));
            }
            // Bursts of takes (a world streaming in), then quiet.
            let burst = step % 500 < 120;
            if burst || next(&mut seed).is_multiple_of(4) {
                let len = (1 + next(&mut seed) % 24) << 20;
                let (piece, block, again) = pool.take(len, 512, now, || {
                    made.set(made.get() + 1);
                    Some(made.get())
                }).unwrap();
                // No live piece overlaps it.
                assert!(live.iter().all(|(q, b, _)| *b != block || piece.start + piece.len <= q.start || q.start + q.len <= piece.start));
                let mut memory = memory.borrow_mut();
                let bytes = memory.entry((block, piece.start)).or_insert_with(|| vec![0; 64]);
                if again {
                    bytes.fill(0);
                }
                // As a block made anew: zeros before the detile writes.
                assert!(bytes.iter().all(|&b| b == 0), "a piece begins as zeros");
                let fill = next(&mut seed) as u8 | 1;
                bytes.fill(fill);
                uploaded.push(bytes.clone());
                live.push((piece, block, fill));
            }
            // Pieces given back (their texture's copy has run).
            if !live.is_empty() && (!burst || next(&mut seed).is_multiple_of(3)) {
                let at = (next(&mut seed) % live.len() as u64) as usize;
                let (piece, block, fill) = live.swap_remove(at);
                let memory = memory.borrow();
                // Nothing else wrote it while it was taken.
                assert!(memory[&(block, piece.start)].iter().all(|&b| b == fill));
                pool.give(piece, now);
            }
            // Once a second: the settle.
            if step % 3 == 0 {
                pool.rested(now);
                let empty = pool.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count();
                // (Without stretches short: the most kept as before.)
                if short_here() || !short_sometimes {
                    most_empty_short = most_empty_short.max(empty);
                }
            }
            // No block that holds a piece taken, or resting, went.
            for (piece, block, _) in &live {
                assert_eq!(pool.blocks[piece.block].as_ref().map(|b| b.memory), Some(*block));
            }
            for (_, piece) in &pool.resting {
                assert!(pool.blocks[piece.block].is_some());
            }
            // A block that went is gone from the simulated memory too.
            let alive: Vec<u64> = pool.blocks.iter().flatten().map(|b| b.memory).collect();
            memory.borrow_mut().retain(|(block, _), _| alive.contains(block));
        }
        SHORT.with(|s| s.set(false));
        (uploaded, most_empty_short, made.get() as usize)
    }

    /// While the PC is short of memory the staging pool keeps one empty
    /// block, and only empty ones go: every piece begins as zeros, keeps
    /// the bytes written into it until it is given back, and the bytes each
    /// copy records are those of the run with spares kept as before
    /// (fixed seeds).
    #[test]
    fn staging_spares_go_while_short_and_bytes_stay_the_same() {
        // As the card's room allows in the game run (8 staging blocks).
        SPARES.store(SPARE_KEPT, Ordering::Relaxed);
        for seed in [0x9E37_79B9_7F4A_7C15u64, 0x1234_5678, 0xDEAD_BEEF_CAFE, 77] {
            let (before, kept_before, made_before) = staging_run(seed, false);
            assert!(kept_before > STAGING_SHORT, "seed {seed:#x}: the run keeps spares as before ({kept_before})");
            let (now, most_empty, made_now) = staging_run(seed, true);
            assert_eq!(before, now, "seed {seed:#x}: the same bytes reach the card");
            assert!(most_empty <= STAGING_SHORT, "seed {seed:#x}: {most_empty} empty blocks kept while short");
            assert!(made_now >= made_before, "seed {seed:#x}");
        }
    }

    /// `VICTORY_STAGING_COMPACT`: after bursts that left a few pieces kept in
    /// many blocks, the pieces of the blocks being emptied move (bytes copied,
    /// as `texture::move_out_of_emptying` does) into fuller blocks, never into
    /// a block being emptied or a new one; the emptied blocks then go, every
    /// piece keeps its bytes, and no two live pieces overlap. Without a plan
    /// the takes are those of before (fixed seeds).
    #[test]
    fn kept_pieces_move_out_and_their_blocks_go() {
        use std::collections::HashMap;
        SPARES.store(SPARE_KEPT, Ordering::Relaxed);
        for seed in [0x9E37_79B9_7F4A_7C15u64, 0x1234_5678, 0xDEAD_BEEF_CAFE, 77] {
            let mut seed = seed;
            let made = std::cell::Cell::new(0u64);
            let make = || {
                made.set(made.get() + 1);
                Some(made.get())
            };
            // Two pools fed the same takes; one plans with nothing to empty.
            let mut pool: Pool<u64> = Pool::with_short(STAGING_KEPT, STAGING_SHORT, || true);
            let mut same: Pool<u64> = Pool::with_short(STAGING_KEPT, STAGING_SHORT, || true);
            let mut bytes: HashMap<(u64, u64), u8> = HashMap::new();
            let mut kept: Vec<(Piece, u64, u8)> = Vec::new();
            let mut now = 0;
            for burst in 0..6 {
                let mut gone = Vec::new();
                for i in 0..40 {
                    let len = (1 + next(&mut seed) % 8) << 20;
                    let (p, b, _) = pool.take(len, 512, now, make).unwrap();
                    let (q, _, _) = same.take(len, 512, now, || Some(0)).unwrap();
                    assert_eq!(p, q, "without a plan, takes are as before");
                    let fill = next(&mut seed) as u8 | 1;
                    bytes.insert((b, p.start), fill);
                    if i % 13 == burst % 13 {
                        kept.push((p, b, fill));
                    } else {
                        gone.push(p);
                    }
                }
                now += 10;
                for p in gone {
                    pool.give(p, now);
                    same.give(p, now);
                }
                now += REST;
                pool.rested(now);
                same.rested(now);
                same.plan(false);
                assert!(same.emptying.is_empty());
            }
            let held: u64 = kept.iter().map(|k| k.0.len).sum();
            let before = pool.blocks();
            pool.plan(true);
            let made_before = made.get();
            // Moves, a few at a time, until none is left to move.
            for _ in 0..100 {
                let mut any = false;
                for k in kept.iter_mut() {
                    if !pool.in_emptying(&k.0) {
                        continue;
                    }
                    let Some((p, b)) = pool.take_for_move(k.0.len, 512, now) else { continue };
                    assert!(!pool.in_emptying(&p), "a piece moves into a block being emptied");
                    assert_eq!(bytes[&(k.1, k.0.start)], k.2);
                    bytes.insert((b, p.start), k.2);
                    pool.give(k.0, now);
                    *k = (p, b, k.2);
                    any = true;
                }
                now += REST;
                pool.rested(now);
                pool.plan(true);
                if !any {
                    break;
                }
            }
            assert_eq!(made.get(), made_before, "seed {seed:#x}: no block made for a move");
            for (i, (p, b, fill)) in kept.iter().enumerate() {
                assert_eq!(bytes[&(*b, p.start)], *fill, "seed {seed:#x}: a piece kept its bytes");
                assert_eq!(pool.blocks[p.block].as_ref().map(|x| x.memory), Some(*b));
                for (q, c, _) in &kept[i + 1..] {
                    assert!(b != c || p.start + p.len <= q.start || q.start + q.len <= p.start, "seed {seed:#x}: overlap");
                }
            }
            let need = (held * 4 / 3).div_ceil(BLOCK) as usize;
            let after = pool.blocks();
            assert!(after <= need + 1 + STAGING_SHORT, "seed {seed:#x}: {before} -> {after} blocks for {} MB", held >> 20);
            assert!(after < before, "seed {seed:#x}: {before} -> {after} blocks");
        }
    }

    /// What the staging blocks hold after a burst of streaming, with spares
    /// kept as before and while the PC is short of memory (ignored: a
    /// measurement on this PC's card; `cargo test --release -p victory-gpu
    /// --lib -- --ignored --nocapture staging_spares_after_a_burst`).
    #[test]
    #[ignore = "measures; prints"]
    fn staging_spares_after_a_burst() {
        let Some(host) = crate::host::host() else { return };
        let shared = || host.video_memory().map_or(0, |m| m[3] >> 20);
        // The card has room, as in the game run: spares as many as allowed.
        SPARES.store(SPARE_KEPT, Ordering::Relaxed);
        for short in [false, true] {
            crate::host::set_ram_short(false);
            let start = shared();
            // A world streaming in: 640 MB of staging, a texture keeping a
            // piece of 1 MiB in every third block's worth.
            let mut kept = Vec::new();
            let mut burst = Vec::new();
            for i in 0..40 {
                burst.push(staging(host, 16 << 20).unwrap().0);
                if i % 12 == 0 {
                    kept.push(staging(host, 1 << 20).unwrap().0);
                }
            }
            let peak = (memory()[1], shared());
            drop(burst);
            crate::host::set_ram_short(short);
            let at = now() + REST + 1;
            settle(&STAGING, at);
            let after = (memory()[1], shared());
            println!(
                "{}: staging blocks {} MB at the burst ({} MB shared with the card, {start} before), {} MB after it rested ({} MB shared), {} MB held by textures",
                if short { "short of memory (VICTORY_RAM_TRIM=1)" } else { "as before" },
                peak.0,
                peak.1,
                after.0,
                after.1,
                kept.len()
            );
            drop(kept);
            crate::host::set_ram_short(true);
            settle(&STAGING, now() + REST + 1);
        }
        crate::host::set_ram_short(false);
    }

    /// A pooled texture's piece comes back when its last reference goes, not
    /// before; its staging piece begins as zeros.
    #[test]
    fn a_texture_gives_its_piece_back_when_it_goes() {
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a valid description.
        let Some(t) = (unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }) else { return };
        assert_eq!(crate::host::references(&t), 1, "what the texture holds is no reference to it");
        let resting = || lock(&TEXTURES).resting.len();
        let before = resting();
        let other = t.clone();
        drop(t);
        assert_eq!(resting(), before);
        drop(other);
        assert_eq!(resting(), before + 1);
        // SAFETY: the piece's mapping, 4096 bytes of it ours.
        unsafe {
            let (piece, _, at, data) = staging(host, 4096).unwrap();
            assert!(at.is_multiple_of(512));
            std::ptr::write_bytes(data, 0xAB, 4096);
            drop(piece);
            lock(&STAGING).rested(u64::MAX);
            let (_piece, _, _, data) = staging(host, 4096).unwrap();
            assert!((0..4096).all(|i| *data.add(i) == 0));
            // SAFETY: plain query.
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }
}
