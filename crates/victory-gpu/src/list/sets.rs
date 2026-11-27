//! What a host list was last given, so that a draw sets again only what
//! changed (`VICTORY_SET_CACHE=1`; off by default: everything at every
//! draw, as before).
//!
//! Every draw set its vertex buffers, its index buffer, its user data (one
//! `Set*Root32BitConstants` per stage) and its root views on the host list,
//! also when the draw before had set the same ones. Within a D3D12 list
//! those stay as set until set again, so such a set is a call of the driver
//! for nothing. Measured on an RTX 4070 SUPER (`bench_redundant_sets`) it
//! is a small one: 7 to 20 ns a call, 6,500 draws recorded 9 % faster with
//! 58 % fewer calls, about 0.07 ms of a frame's 230 ms of list calls. So it
//! stays off until a game run says it is worth having.
//!
//! Left out is only a set whose value equals what the list holds in that
//! slot, as far as this knows: nothing is known at a list's start, after a
//! cut (a new host list) or for a side whose root signature was set (that
//! clears every root argument of the side). Anything else that records on
//! the list sets the root signature again before the next draw
//! (`State::bind` is told by `host_roots`), or tells this
//! ([`HostSets::forget_root`], [`HostSets::vertex_buffer_at`]).
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::rec::Rec;

/// Whether redundant sets are left out (`VICTORY_SET_CACHE=1`; off: every set
/// is made, byte for byte the calls of before).
pub(crate) fn on() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_SET_CACHE") {
        // (Asked once per recording: HostSets::on.)
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SET_CACHE").is_some_and(|v| v == "1"))
}

/// The kinds of set counted: vertex buffers, index buffer, root constants,
/// root views and one-view tables.
const KINDS: usize = 4;

/// Since the last frame-rate line, by kind: sets made, sets left out.
static STATS: [[AtomicU64; KINDS]; 2] = [const { [const { AtomicU64::new(0) }; KINDS] }; 2];

/// A vertex buffer view as compared: address, size, stride.
type Vertex = (u64, u32, u32);

fn vertex(v: &D3D12_VERTEX_BUFFER_VIEW) -> Vertex {
    (v.BufferLocation, v.SizeInBytes, v.StrideInBytes)
}

/// A root argument the list holds: a root view (kind 0 constant buffer, 1
/// shader resource, 2 unordered access), a table (3) or constants (4), and
/// its address, handle or words. Known only while `round` is its side's.
#[derive(Debug, Clone, Default)]
struct Held {
    round: u32,
    kind: u8,
    value: u64,
    /// (Kept over rounds: no vector is made again for each.)
    words: Vec<u32>,
}

const CONSTANTS: u8 = 4;
const TABLE: u8 = 3;

/// A root view's kind, for [`HostSets::view`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootView {
    Constants,
    Shader,
    Unordered,
}

/// What one host list holds of what draws set ([`on`]). Part of a title
/// list's draw state: it goes with it and is made anew with it.
pub(crate) struct HostSets {
    /// [`on`] when this was made: one recording goes by one answer.
    on: bool,
    vertex: [Option<Vertex>; 32],
    index: Option<(u64, u32, i32)>,
    /// Graphics, compute: by root parameter.
    roots: [Vec<Held>; 2],
    /// Graphics, compute: what an argument's `round` must be to be known
    /// (0 is never one: an argument set elsewhere is given 0).
    rounds: [u32; 2],
    /// Made, left out, by kind (added to [`STATS`] when this goes).
    counts: [[u32; KINDS]; 2],
}

impl Default for HostSets {
    fn default() -> Self {
        HostSets {
            on: on(),
            vertex: [None; 32],
            index: None,
            roots: Default::default(),
            rounds: [1; 2],
            counts: [[0; KINDS]; 2],
        }
    }
}

#[cfg(test)]
impl HostSets {
    /// One that leaves redundant sets out, whatever `VICTORY_SET_CACHE` says.
    pub(crate) fn enabled() -> Self {
        let mut sets = HostSets::default();
        sets.on = true;
        sets
    }
}

impl Drop for HostSets {
    fn drop(&mut self) {
        for (made, counts) in STATS.iter().zip(&self.counts) {
            for (s, &n) in made.iter().zip(counts) {
                if n != 0 {
                    s.fetch_add(u64::from(n), Relaxed);
                }
            }
        }
    }
}

impl HostSets {
    fn count(&mut self, kind: usize, made: bool) {
        let c = &mut self.counts[usize::from(!made)][kind];
        *c = c.wrapping_add(1);
    }

    /// Nothing is known any more (a new host list, one reset).
    pub(crate) fn forget(&mut self) {
        self.vertex = [None; 32];
        self.index = None;
        self.root_signature_set(false);
        self.root_signature_set(true);
    }

    /// The side's root signature was set: it has no root arguments now.
    pub(crate) fn root_signature_set(&mut self, compute: bool) {
        let round = &mut self.rounds[usize::from(compute)];
        *round = round.checked_add(1).unwrap_or_else(|| {
            // (Once in four billion: every argument is unknown again.)
            self.roots[usize::from(compute)].clear();
            1
        });
    }

    /// Something else set root parameter `param` of the side.
    pub(crate) fn forget_root(&mut self, compute: bool, param: u32) {
        if let Some(h) = self.roots[usize::from(compute)].get_mut(param as usize) {
            h.round = 0;
        }
    }

    /// Whether `IASetVertexBuffers(0, views)` must be recorded; the list
    /// holds them from here on.
    pub(crate) fn want_vertex(&mut self, views: &[D3D12_VERTEX_BUFFER_VIEW]) -> bool {
        if !self.on {
            return true;
        }
        let views = &views[..views.len().min(32)];
        let held = views.iter().zip(&self.vertex).all(|(v, h)| *h == Some(vertex(v)));
        if !held {
            for (v, h) in views.iter().zip(&mut self.vertex) {
                *h = Some(vertex(v));
            }
        }
        self.count(0, !held);
        !held
    }

    /// Whether `IASetIndexBuffer(view)` must be recorded.
    pub(crate) fn want_index(&mut self, view: &D3D12_INDEX_BUFFER_VIEW) -> bool {
        if !self.on {
            return true;
        }
        let v = (view.BufferLocation, view.SizeInBytes, view.Format.0);
        let held = self.index == Some(v);
        self.index = Some(v);
        self.count(1, !held);
        !held
    }

    /// The argument the side holds at `param`, made if there is none.
    fn held(&mut self, compute: bool, param: u32) -> (&mut Held, u32) {
        let side = usize::from(compute);
        let roots = &mut self.roots[side];
        let at = param as usize;
        if roots.len() <= at {
            roots.resize(at + 1, Held::default());
        }
        (&mut roots[at], self.rounds[side])
    }

    fn want_value(&mut self, compute: bool, param: u32, kind: u8, value: u64) -> bool {
        if !self.on {
            return true;
        }
        let (h, round) = self.held(compute, param);
        let held = h.round == round && h.kind == kind && h.value == value;
        (h.round, h.kind, h.value) = (round, kind, value);
        self.count(3, !held);
        !held
    }

    /// Whether `Set*Root32BitConstants(param, values, 0)` must be recorded.
    pub(crate) fn want_constants(&mut self, compute: bool, param: u32, values: &[u32]) -> bool {
        if !self.on {
            return true;
        }
        let (h, round) = self.held(compute, param);
        let held = h.round == round && h.kind == CONSTANTS && h.words == values;
        if !held {
            (h.round, h.kind) = (round, CONSTANTS);
            h.words.clear();
            h.words.extend_from_slice(values);
        }
        self.count(2, !held);
        !held
    }
    /// Whether a root view `kind` at `gpu` must be set at `param`.
    pub(crate) fn want_view(&mut self, compute: bool, param: u32, kind: RootView, gpu: u64) -> bool {
        self.want_value(compute, param, kind as u8, gpu)
    }

    /// Whether a descriptor table at `handle` must be set at `param`.
    pub(crate) fn want_table(&mut self, compute: bool, param: u32, handle: u64) -> bool {
        self.want_value(compute, param, TABLE, handle)
    }

    /// `IASetVertexBuffers(0, views)`, unless the list holds them.
    ///
    /// # Safety
    /// As the call's: an open list, views of live buffers.
    pub(crate) unsafe fn vertex_buffers(&mut self, l: &Rec, views: &[D3D12_VERTEX_BUFFER_VIEW]) {
        if !views.is_empty() && self.want_vertex(views) {
            // SAFETY: the caller's.
            unsafe { l.IASetVertexBuffers(0, Some(views)) };
        }
    }

    /// `IASetVertexBuffers(slot, [view])`, always; the list holds it.
    ///
    /// # Safety
    /// As [`HostSets::vertex_buffers`].
    pub(crate) unsafe fn vertex_buffer_at(&mut self, l: &Rec, slot: u32, view: D3D12_VERTEX_BUFFER_VIEW) {
        self.note_vertex_at(slot, &view);
        // SAFETY: the caller's.
        unsafe { l.IASetVertexBuffers(slot, Some(&[view])) };
    }

    /// `IASetVertexBuffers(slot, [view])` was recorded.
    pub(crate) fn note_vertex_at(&mut self, slot: u32, view: &D3D12_VERTEX_BUFFER_VIEW) {
        if let Some(h) = self.vertex.get_mut(slot as usize) {
            *h = self.on.then(|| vertex(view));
        }
    }

    /// `IASetIndexBuffer(view)`, unless the list holds it.
    ///
    /// # Safety
    /// As the call's.
    pub(crate) unsafe fn index_buffer(&mut self, l: &Rec, view: &D3D12_INDEX_BUFFER_VIEW) {
        if self.want_index(view) {
            // SAFETY: the caller's.
            unsafe { l.IASetIndexBuffer(Some(view)) };
        }
    }

    /// `Set{Graphics,Compute}Root32BitConstants(param, values, 0)`, unless
    /// the list holds them.
    ///
    /// # Safety
    /// As the call's: `values` covers the parameter.
    pub(crate) unsafe fn constants(&mut self, l: &Rec, compute: bool, param: u32, values: &[u32]) {
        if self.want_constants(compute, param, values) {
            // SAFETY: the caller's.
            unsafe {
                if compute {
                    l.SetComputeRoot32BitConstants(param, values.len() as u32, values.as_ptr().cast(), 0);
                } else {
                    l.SetGraphicsRoot32BitConstants(param, values.len() as u32, values.as_ptr().cast(), 0);
                }
            }
        }
    }

    /// A root view, unless the list holds it.
    ///
    /// # Safety
    /// As the call's: the address is in a live buffer.
    pub(crate) unsafe fn view(&mut self, l: &Rec, compute: bool, param: u32, kind: RootView, gpu: u64) {
        if !self.want_view(compute, param, kind, gpu) {
            return;
        }
        // SAFETY: the caller's.
        unsafe {
            match (compute, kind) {
                (false, RootView::Constants) => l.SetGraphicsRootConstantBufferView(param, gpu),
                (false, RootView::Shader) => l.SetGraphicsRootShaderResourceView(param, gpu),
                (false, RootView::Unordered) => l.SetGraphicsRootUnorderedAccessView(param, gpu),
                (true, RootView::Constants) => l.SetComputeRootConstantBufferView(param, gpu),
                (true, RootView::Shader) => l.SetComputeRootShaderResourceView(param, gpu),
                (true, RootView::Unordered) => l.SetComputeRootUnorderedAccessView(param, gpu),
            }
        }
    }

    /// A descriptor table, unless the list holds it.
    ///
    /// # Safety
    /// As the call's: the handle is in the bound heap.
    pub(crate) unsafe fn table(&mut self, l: &Rec, compute: bool, param: u32, handle: D3D12_GPU_DESCRIPTOR_HANDLE) {
        if !self.want_table(compute, param, handle.ptr) {
            return;
        }
        // SAFETY: the caller's.
        unsafe {
            if compute {
                l.SetComputeRootDescriptorTable(param, handle);
            } else {
                l.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
    }
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    let take = |i: usize, k: usize| STATS[i][k].swap(0, Relaxed) as f64 / frames;
    let [made, left]: [[f64; KINDS]; 2] = [0, 1].map(|i| [0, 1, 2, 3].map(|k| take(i, k)));
    if made.iter().chain(&left).all(|&n| n == 0.0) {
        return;
    }
    tracing::info!(
        "host sets per frame (VICTORY_SET_CACHE): vertex buffers {:.0} made, {:.0} left out; index buffers {:.0}, {:.0}; root constants {:.0}, {:.0}; root views and tables {:.0}, {:.0}",
        made[0],
        left[0],
        made[1],
        left[1],
        made[2],
        left[2],
        made[3],
        left[3]
    );
}

#[cfg(test)]
mod tests;
